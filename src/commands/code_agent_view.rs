//! `libertai agents` — one screen for every background coding-agent session.
//!
//! The view lists each background `libertai code --bg` run that the on-disk
//! index (`code-background-agents/runs.jsonl`) knows about, polls its pid
//! to bucket it as working / completed / unknown, and lets you act on it:
//! dispatch a new run from the input line, peek at a run's log tail
//! without attaching, or stop a run. Run without flags on a TTY for the
//! full-screen alt-screen TUI; pass `--json` for a machine-readable array
//! that exits without touching the terminal; run on a pipe and you get the
//! same grouped listing as plain text.
//!
//! The TUI is built on crossterm 0.29. Raw mode and the alt screen are
//! owned by local RAII guards (`RawModeGuard` / `AltScreenGuard`) so a
//! panic mid-loop still restores the user's terminal. The 1 s event-poll
//! timeout doubles as the refresh tick: each iteration that times out
//! without input reloads the records, re-polls every pid, and rebuilds the
//! grouped view — so a run that finishes on its own flips from Working to
//! Completed within a second without user interaction.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde_json::json;

use crate::commands::code_factory::Mode;
use crate::commands::code_ui::{
    background_agent_record_id, background_agent_status, load_background_agent_records,
    preview_text, read_log_tail, retain_running_background_agent_records,
    rewrite_background_agent_records, send_background_agent_kill, start_background_agent,
    BackgroundAgentLaunch, BackgroundAgentRecord, BackgroundAgentStatus, StartedBackgroundAgent,
};

/// How many bytes of a run's log we pull into the peek overlay. Matches
/// the `BACKGROUND_AGENT_LOG_TAIL_BYTES` budget the REPL uses for its
/// `/agents` peek, so the view and the slash command show the same tail.
const PEEK_TAIL_BYTES: usize = 64_000;
/// Lines of the tail we render inside the peek box. The box holds a
/// top border + this many content rows + a bottom border.
const PEEK_CONTENT_ROWS: usize = 20;
/// Total height of the peek box in terminal rows (border + content +
/// border). Pre-computed so the list renderer knows how much vertical
/// room to leave above it.
const PEEK_BOX_ROWS: usize = 1 + PEEK_CONTENT_ROWS + 1;
/// Poll interval for the event loop. Also the refresh tick: every time
/// `event::poll` times out at this duration we reload + re-poll.
const POLL_TICK: Duration = Duration::from_millis(1000);

// ─── RAII terminal guards ──────────────────────────────────────────────
// Local to this module on purpose — the spec forbids depending on
// `code_term::RawModeGuard`. Each guard restores terminal state on drop,
// including the panic-unwind path, so a crash in the render loop can't
// leave the user stuck in raw mode / the alt screen.

/// Enables raw mode on construction and disables it on drop.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().map_err(|e| anyhow::anyhow!("enable_raw_mode: {e}"))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort: we're tearing down, surfacing the error would only
        // mask the real one a caller is already unwinding past.
        let _ = terminal::disable_raw_mode();
    }
}

/// Enters the alternate screen and hides the cursor on construction;
/// leaves the alt screen and shows the cursor on drop.
struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> Result<Self> {
        execute!(io::stdout(), EnterAlternateScreen, Hide)
            .map_err(|e| anyhow::anyhow!("enter alternate screen: {e}"))?;
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

// ─── View model ────────────────────────────────────────────────────────

/// One row in the view: a persisted run record plus its freshly polled
/// process status. The status is snapshotted during `refresh` and stays
/// pinned until the next refresh tick, so rendering never blocks on
/// `kill -0`.
#[derive(Debug, Clone)]
struct AgentViewEntry {
    record: BackgroundAgentRecord,
    status: BackgroundAgentStatus,
}

/// Fixed-for-the-run configuration resolved from the CLI flags + config
/// file. Passed through the loop by reference so the view state stays
/// small and `refresh`/`dispatch` can re-read it without re-parsing.
struct ViewConfig {
    model: String,
    provider: String,
    cwd_filter: Option<PathBuf>,
    /// Pre-formatted label for the header: the filter path's display or
    /// `"all"` when no `--cwd` was passed.
    cwd_label: String,
    mode: Mode,
    agent: Option<String>,
}

/// Mutable TUI state. `selected` is an index into the display order
/// (Working ++ Completed ++ Unknown), not into `entries` directly — the
/// `display_order` helper maps it back to an `entries` index.
#[derive(Debug, Default)]
struct ViewState {
    entries: Vec<AgentViewEntry>,
    selected: usize,
    /// Log tail of the selected run, shown in a boxed overlay. `None`
    /// means no peek open. Set by Space, cleared by Space / Esc / arrow
    /// move / opening dispatch.
    peek: Option<String>,
    /// True while the user is typing a dispatch prompt at the bottom bar.
    dispatching: bool,
    /// In-progress dispatch input buffer.
    buffer: String,
    /// Transient one-shot status line. Rendered once into the footer,
    /// then consumed (taken) so the next frame shows the hints again.
    message: Option<String>,
}

// ─── Pure helpers (unit-tested, no TTY needed) ──────────────────────────

/// Resolve a `--permission-mode` flag to a `Mode`.
///
/// Unknown / mistyped values fall back to `Normal` rather than erroring —
/// the view shouldn't crash on a typo, and `Normal` is the safest mode to
/// default into. Matching is ASCII-case-insensitive so `Accept-Edits` and
/// `Plan` work as well as their lowercase spellings.
fn parse_permission_mode(s: Option<&str>) -> Mode {
    let Some(s) = s else {
        return Mode::Normal;
    };
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "normal" | "default" => Mode::Normal,
        "accept-edits" | "accept_edits" | "accept" => Mode::AcceptEdits,
        "plan" | "readonly" => Mode::Plan,
        // Anything else: don't bail — see the doc comment above.
        _ => Mode::Normal,
    }
}

/// Split `entries` into `(working, completed, unknown)` index lists,
/// each sorted newest-first by `started_at_ms`. Returns indices into the
/// same `entries` slice so the caller can render without copying records.
fn group_entries(entries: &[AgentViewEntry]) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    // Collect each bucket as (started_at_ms, idx) so we can sort by time
    // without re-scanning the slice. Stable sort keeps insertion order for
    // records that share a start time.
    let mut working = Vec::new();
    let mut completed = Vec::new();
    let mut unknown = Vec::new();
    for (idx, entry) in entries.iter().enumerate() {
        let bucket = match entry.status {
            BackgroundAgentStatus::Running => &mut working,
            BackgroundAgentStatus::Exited => &mut completed,
            BackgroundAgentStatus::Unknown => &mut unknown,
        };
        bucket.push((entry.record.started_at_ms, idx));
    }
    // Newest-first: descending by started_at_ms.
    sort_newest_first(&mut working);
    sort_newest_first(&mut completed);
    sort_newest_first(&mut unknown);
    (
        working.into_iter().map(|(_, i)| i).collect(),
        completed.into_iter().map(|(_, i)| i).collect(),
        unknown.into_iter().map(|(_, i)| i).collect(),
    )
}

/// In-place descending sort by the first tuple element (started_at_ms).
fn sort_newest_first(bucket: &mut [(u64, usize)]) {
    // `sort_by_key` + `Reverse` gives newest-first (descending) without
    // the manual `b.0.cmp(&a.0)` closure clippy flags as `unnecessary_sort_by`.
    bucket.sort_by_key(|&(t, _)| std::cmp::Reverse(t));
}

/// Build a slug for a dispatched run from its prompt: the first four
/// whitespace words, lowercased, every non-alphanumeric char → `-`, runs
/// of `-` collapsed, leading/trailing `-` trimmed. Empty / punctuation-only
/// prompts fall back to `"agent"` so the on-disk log filename is always
/// meaningful.
fn slug_from_prompt(prompt: &str) -> String {
    // Take the first four words; `join(" ")` lets us reuse the same
    // char walk for both word separators and in-word punctuation.
    let head: String = prompt.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
    let mut raw = String::new();
    for ch in head.chars() {
        if ch.is_ascii_alphanumeric() {
            raw.push(ch.to_ascii_lowercase());
        } else {
            raw.push('-');
        }
    }
    // Collapse runs of '-' into one so `Fix!! the login` → `fix-the-login`
    // rather than `fix--the-login`.
    let mut collapsed = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch == '-' {
            if !prev_dash {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(ch);
            prev_dash = false;
        }
    }
    let slug = collapsed.trim_matches('-').to_string();
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug
    }
}

/// Keep only records whose `cwd` starts with `cwd` (component-wise via
/// `Path::starts_with`, which handles trailing slashes correctly and
/// won't match `/foo` against `/foobar`). `None` keeps everything.
fn filter_by_cwd(
    records: Vec<BackgroundAgentRecord>,
    cwd: Option<&Path>,
) -> Vec<BackgroundAgentRecord> {
    let Some(cwd) = cwd else {
        return records;
    };
    records
        .into_iter()
        .filter(|record| Path::new(&record.cwd).starts_with(cwd))
        .collect()
}

/// Display order: Working group first, then Completed, then Unknown, each
/// newest-first. This is the order rows are painted top→bottom, and
/// `state.selected` indexes into this flattened list.
fn display_order(entries: &[AgentViewEntry]) -> Vec<usize> {
    let (working, completed, unknown) = group_entries(entries);
    let mut order = Vec::with_capacity(working.len() + completed.len() + unknown.len());
    order.extend(working);
    order.extend(completed);
    order.extend(unknown);
    order
}

/// Relative age of a run: "just now" / "5m ago" / "3h ago" / "2d ago".
/// Pure so a test can pin the bucket boundaries without a clock.
fn relative_time(started_at_ms: u64, now_ms: u64) -> String {
    let delta_secs = now_ms.saturating_sub(started_at_ms) / 1000;
    if delta_secs < 60 {
        "just now".to_string()
    } else if delta_secs < 3600 {
        format!("{}m ago", delta_secs / 60)
    } else if delta_secs < 86_400 {
        format!("{}h ago", delta_secs / 3600)
    } else {
        format!("{}d ago", delta_secs / 86_400)
    }
}

/// Current wall clock as epoch milliseconds. Used as the `now` for
/// `relative_time` and for nothing else — status polling lives in
/// `refresh` and uses `background_agent_status`, not the clock.
fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Clip a string to `max` visible chars, appending an ellipsis if it was
/// truncated. A 1-char budget yields just `…`; a 0-char budget yields the
/// empty string. Used for header / footer / name clipping where we don't
/// want `preview_text`'s trim + control-replacement behaviour.
fn clip_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    if max > 0 {
        out.push('…');
    }
    out
}

/// Replace control chars with spaces so a stray ESC or bell inside a log
/// line (or a record name) can't reposition the cursor or ring the bell
/// inside the alt screen. Newlines are already gone by the time we call
/// this (we split on them first), so they aren't a concern here.
fn sanitize_for_term(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

// ─── Entry point ───────────────────────────────────────────────────────

/// Run the `libertai agents` view.
///
/// `--json` prints a machine-readable array and returns without entering
/// the TUI. On a non-TTY stdout without `--json`, prints the grouped
/// listing as plain text and returns. Otherwise enters the full-screen
/// interactive view and returns when the user quits.
pub fn run(
    cwd: Option<String>,
    json: bool,
    model: Option<String>,
    permission_mode: Option<String>,
    agent: Option<String>,
) -> Result<()> {
    // Resolve model/provider from config so the dispatch bar can build a
    // `BackgroundAgentLaunch` without re-parsing flags on each Enter.
    let cfg = crate::config::load().context("loading libertai config")?;
    let resolved_model = model.unwrap_or_else(|| cfg.default_code_model.clone());
    let provider = cfg.default_code_provider.clone();
    let mode = parse_permission_mode(permission_mode.as_deref());
    let cwd_filter = cwd.map(PathBuf::from);
    let cwd_label = cwd_filter
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "all".to_string());

    if json {
        return print_json(cwd_filter.as_deref());
    }

    // Don't try to enter raw mode / the alt screen on a pipe — crossterm
    // would happily do it and then produce a screenful of escape codes on
    // the user's scrollback. Fall back to the plain grouped listing.
    if !io::stdout().is_terminal() {
        return print_plain(cwd_filter.as_deref());
    }

    let config = ViewConfig {
        model: resolved_model,
        provider,
        cwd_filter,
        cwd_label,
        mode,
        agent,
    };
    run_tui(&config)
}

// ─── JSON mode ─────────────────────────────────────────────────────────

/// Print one JSON object per record to stdout and return. Maps
/// `BackgroundAgentStatus` to the documented state strings
/// (`working` / `completed` / `unknown`).
fn print_json(cwd_filter: Option<&Path>) -> Result<()> {
    let records = load_background_agent_records().context("loading background agent records")?;
    let records = filter_by_cwd(records, cwd_filter);
    let mut out = Vec::with_capacity(records.len());
    for record in &records {
        let state = match background_agent_status(record.pid) {
            BackgroundAgentStatus::Running => "working",
            BackgroundAgentStatus::Exited => "completed",
            BackgroundAgentStatus::Unknown => "unknown",
        };
        out.push(json!({
            "id": background_agent_record_id(record),
            "pid": record.pid,
            "name": record.name,
            "model": record.model,
            "cwd": record.cwd,
            "state": state,
            "promptPreview": record.prompt_preview,
            "startedAtMs": record.started_at_ms,
            "logPath": record.log_path,
            "team": record.team,
            "teammateName": record.teammate_name,
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&out).context("serializing agent records to JSON")?
    );
    Ok(())
}

// ─── Non-TTY fallback ──────────────────────────────────────────────────

/// Print the grouped listing as plain text (no alt screen) and return.
/// Used when stdout isn't a TTY and `--json` wasn't asked for.
fn print_plain(cwd_filter: Option<&Path>) -> Result<()> {
    let records = load_background_agent_records().context("loading background agent records")?;
    let records = filter_by_cwd(records, cwd_filter);
    let entries = build_entries(records);
    let now_ms = now_epoch_ms();
    let (working, completed, unknown) = group_entries(&entries);
    print_plain_group(&entries, &working, "Working", now_ms);
    print_plain_group(&entries, &completed, "Completed", now_ms);
    print_plain_group(&entries, &unknown, "Unknown", now_ms);
    if entries.is_empty() {
        println!("No agent sessions.");
    }
    Ok(())
}

/// Print one plain-text group header + its rows. Empty groups are skipped
/// so the piped output stays compact.
fn print_plain_group(
    entries: &[AgentViewEntry],
    idxs: &[usize],
    label: &str,
    now_ms: u64,
) {
    if idxs.is_empty() {
        return;
    }
    println!("{label}:");
    for &i in idxs {
        let entry = &entries[i];
        let icon = status_icon(entry.status);
        let time = relative_time(entry.record.started_at_ms, now_ms);
        // `preview_text` trims + clips + swaps control chars for spaces,
        // same helper the REPL uses for its own preview chips.
        let preview = preview_text(&entry.record.prompt_preview, 60);
        println!("  {icon} {}  {preview}  {time}", entry.record.name);
    }
}

// ─── TUI loop ──────────────────────────────────────────────────────────

/// Drive the interactive view until the user quits. Owns the raw-mode and
/// alt-screen guards for the whole loop so a panic unwinds them and
/// restores the terminal.
fn run_tui(config: &ViewConfig) -> Result<()> {
    let _raw = RawModeGuard::enter()?;
    let _alt = AltScreenGuard::enter()?;

    let mut state = ViewState::default();
    // Prime the view before the first paint so the first frame isn't empty.
    refresh(&mut state, config)?;

    loop {
        // `terminal::size` can briefly fail during a resize; fall back to
        // 80x24 rather than crashing the loop.
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        render(&mut io::stdout(), &mut state, config, cols, rows)?;

        if event::poll(POLL_TICK)? {
            // A key arrived — handle it. Non-key events (mouse, resize,
            // paste) are ignored; a resize just means the next frame
            // re-queries `terminal::size`.
            if let Event::Key(key) = event::read()? {
                if handle_key(&mut state, config, key)? {
                    break;
                }
            }
        } else {
            // Poll timed out with no input — treat it as the refresh tick
            // so runs that finish on their own flip to Completed without
            // the user needing to press anything.
            refresh(&mut state, config)?;
        }
    }

    Ok(())
}

/// Reload records, poll every pid, rebuild `entries`, and clamp the
/// selection back into range. Called on entry, on the poll-timeout tick,
/// and after mutating actions (stop / dispatch).
fn refresh(state: &mut ViewState, config: &ViewConfig) -> Result<()> {
    let records = load_background_agent_records().context("loading background agent records")?;
    let records = filter_by_cwd(records, config.cwd_filter.as_deref());
    state.entries = build_entries(records);

    // Clamp the selection into the new display order. If a run we had
    // selected was pruned (stop, or it exited and got reaped elsewhere),
    // we land on the last valid index rather than going out of bounds.
    let order = display_order(&state.entries);
    if order.is_empty() {
        state.selected = 0;
    } else if state.selected >= order.len() {
        state.selected = order.len() - 1;
    }
    Ok(())
}

/// Build `AgentViewEntry` rows from a list of records, polling each pid
/// once. Done in `refresh` so the render path never blocks on `kill -0`.
fn build_entries(records: Vec<BackgroundAgentRecord>) -> Vec<AgentViewEntry> {
    records
        .into_iter()
        .map(|record| {
            let status = background_agent_status(record.pid);
            AgentViewEntry { record, status }
        })
        .collect()
}

/// Handle one keystroke. Returns `true` to quit the loop. Mutating
/// actions (stop / dispatch) refresh the state themselves so the next
/// paint reflects them immediately rather than waiting for the tick.
fn handle_key(state: &mut ViewState, config: &ViewConfig, key: KeyEvent) -> Result<bool> {
    // Dispatch mode is a modal input bar — handle it entirely separately
    // so the global movement/peek/stop bindings don't fire while the user
    // is typing a prompt.
    if state.dispatching {
        return handle_dispatch_key(state, config, key);
    }

    let order = display_order(&state.entries);

    match (key.code, key.modifiers) {
        // Ctrl+C and `q` quit. Ctrl+C arrives as a normal key event under
        // raw mode (the `ctrlc` crate isn't needed here), so we catch it
        // explicitly rather than letting it kill the process.
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => return Ok(true),
        // Esc closes the peek overlay if open; otherwise it quits. This
        // matches the "Esc first closes, then quits" convention from the
        // approval micro-prompt in `code_term`.
        (KeyCode::Esc, _) => {
            if state.peek.is_some() {
                state.peek = None;
            } else {
                return Ok(true);
            }
        }
        // Up/Down move within the flattened display order. Moving closes
        // any open peek so it doesn't float over a different row.
        (KeyCode::Up, _) => {
            state.selected = state.selected.saturating_sub(1);
            state.peek = None;
        }
        (KeyCode::Down, _) => {
            let max = order.len().saturating_sub(1);
            state.selected = (state.selected + 1).min(max);
            state.peek = None;
        }
        // Space toggles the peek overlay for the selected run. On a read
        // error we show the error message in the overlay instead of the
        // tail, so the user sees what went wrong rather than a blank box.
        (KeyCode::Char(' '), _) => {
            if state.peek.is_some() {
                state.peek = None;
            } else if let Some(&idx) = order.get(state.selected) {
                let record = &state.entries[idx].record;
                match read_log_tail(Path::new(&record.log_path), PEEK_TAIL_BYTES) {
                    Ok(tail) => state.peek = Some(tail),
                    Err(e) => state.peek = Some(format!("could not read log: {e:#}")),
                }
            }
        }
        // Ctrl+X stops the selected run. Kill errors are ignored — the
        // process may already have exited between the last poll and now,
        // which `kill` reports as a non-zero exit. We then reload + prune
        // the on-disk index so stopped runs disappear from the view.
        (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
            if let Some(&idx) = order.get(state.selected) {
                let pid = state.entries[idx].record.pid;
                let _ = send_background_agent_kill(pid);
                let records = load_background_agent_records()
                    .context("loading background agent records")?;
                let running = retain_running_background_agent_records(records, |pid| {
                    background_agent_status(pid)
                });
                rewrite_background_agent_records(&running)
                    .context("rewriting background agent records")?;
                state.message = Some(format!("stopped pid {pid}"));
                refresh(state, config)?;
            }
        }
        // `/` enters dispatch mode. We clear the buffer + peek so the
        // dispatch bar starts clean and isn't overlaid on a stale peek.
        (KeyCode::Char('/'), _) => {
            state.dispatching = true;
            state.buffer.clear();
            state.peek = None;
        }
        // Enter when not dispatching is a no-op — opening peek on Enter
        // would surprise users who hit it expecting `q`-style confirm.
        _ => {}
    }
    Ok(false)
}

/// Handle a keystroke while the dispatch input bar is open. Returns
/// `true` to quit (Ctrl+C only; plain Esc cancels dispatch without
/// quitting the whole view).
fn handle_dispatch_key(
    state: &mut ViewState,
    config: &ViewConfig,
    key: KeyEvent,
) -> Result<bool> {
    match (key.code, key.modifiers) {
        // Ctrl+C quits the whole view even mid-dispatch; Esc just cancels
        // the in-progress prompt and drops back to the list.
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(true),
        (KeyCode::Esc, _) => {
            state.dispatching = false;
            state.buffer.clear();
        }
        (KeyCode::Enter, _) => {
            let prompt = state.buffer.clone();
            // An empty/whitespace prompt is a no-op rather than a dispatch
            // of an empty task — `start_background_agent` would happily
            // spawn one, but there's nothing useful it could do.
            if !prompt.trim().is_empty() {
                match dispatch(config, &prompt) {
                    Ok(started) => {
                        state.message =
                            Some(format!("dispatched `{}` (pid {})", slug_from_prompt(&prompt), started.pid));
                    }
                    Err(e) => state.message = Some(format!("dispatch failed: {e:#}")),
                }
            }
            state.dispatching = false;
            state.buffer.clear();
            // Show the new run immediately rather than waiting for the tick.
            refresh(state, config)?;
        }
        (KeyCode::Backspace, _) => {
            state.buffer.pop();
        }
        // Printable chars (no control modifier) append to the buffer.
        // Shift is allowed so upper-case letters type normally; other
        // modifiers (Alt, Control) are ignored to avoid binding clashes.
        // The `!c.is_control()` guard keeps ESC, backspace-as-control,
        // etc. out of the buffer — collapsing it into the match arm keeps
        // clippy's `collapsible_match` quiet and the fall-through `_ => {}`
        // drops control chars on the floor where they belong.
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) if !c.is_control() => {
            state.buffer.push(c);
        }
        _ => {}
    }
    Ok(false)
}

/// Spawn a background run from `prompt`. Builds the launch from the
/// view's resolved model/provider, the parsed permission mode, a slug
/// derived from the prompt, the current directory as the run's cwd, and
/// the optional `--agent` flag.
fn dispatch(config: &ViewConfig, prompt: &str) -> Result<StartedBackgroundAgent> {
    let name = slug_from_prompt(prompt);
    let cwd = std::env::current_dir().context("resolving cwd for dispatch")?;
    let launch = BackgroundAgentLaunch {
        name: name.clone(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        mode: config.mode,
        prompt: prompt.to_string(),
        cwd,
        agent: config.agent.clone(),
        team: None,
        teammate_name: None,
    };
    start_background_agent(&launch).with_context(|| format!("dispatching background agent `{name}`"))
}

// ─── Rendering ─────────────────────────────────────────────────────────

/// Paint one frame to `out`. Clears the screen, draws header / list /
/// peek box / footer, then flushes. Takes `&mut ViewState` only to
/// consume the one-shot `message`; everything else is read-only.
fn render(
    out: &mut impl Write,
    state: &mut ViewState,
    config: &ViewConfig,
    cols: u16,
    rows: u16,
) -> Result<()> {
    // Snapshot the mutable bits up front so the rest of the function
    // borrows `state` immutably without fighting the borrow checker, and
    // so the message is consumed exactly once (one-shot semantics).
    let message = state.message.take();
    let peek = state.peek.clone();
    let dispatching = state.dispatching;
    let buffer = state.buffer.clone();
    let selected = state.selected;
    let entries = &state.entries;

    let cols = if cols == 0 { 80 } else { cols as usize };
    let rows = if rows == 0 { 24 } else { rows as usize };
    let now_ms = now_epoch_ms();

    let order = display_order(entries);
    let (working, completed, _unknown) = group_entries(entries);

    // Header: `LibertAI agents · <model> · <cwd-or-all> · N total (W working, C completed)`.
    let header = clip_to(
        &format!(
            "LibertAI agents · {} · {} · {} total ({} working, {} completed)",
            config.model,
            config.cwd_label,
            entries.len(),
            working.len(),
            completed.len(),
        ),
        cols,
    );

    let footer_height = if dispatching { 2 } else { 1 };
    let peek_some = peek.is_some();
    let peek_height = if peek_some { PEEK_BOX_ROWS } else { 0 };
    // Vertical budget for the list region, leaving room for the header
    // above and the peek box + footer below.
    let list_area = rows.saturating_sub(1 + footer_height + peek_height);

    let list_lines = render_list(entries, &order, selected, cols, list_area, now_ms);

    queue!(out, MoveTo(0, 0), Clear(ClearType::All))
        .map_err(|e| anyhow::anyhow!("clearing screen: {e}"))?;
    queue!(out, MoveTo(0, 0), Print(header.clone()))
        .map_err(|e| anyhow::anyhow!("writing header: {e}"))?;

    for (i, line) in list_lines.iter().enumerate() {
        // Guard against a too-tiny terminal: never write past the last row.
        if 1 + i >= rows {
            break;
        }
        queue!(out, MoveTo(0, (1 + i) as u16), Print(line.clone()))
            .map_err(|e| anyhow::anyhow!("writing list row: {e}"))?;
    }

    // Peek box sits between the list and the footer. We need the selected
    // run's name for the box header; if there's no selection (empty list)
    // we wouldn't have a peek open, so the `unwrap_or` is just defensive.
    if let Some(peek) = peek.as_deref() {
        let name = order
            .get(selected)
            .map(|i| entries[*i].record.name.clone())
            .unwrap_or_default();
        let box_top = (1 + list_area) as u16;
        for (i, line) in render_peek(peek, cols, &name).iter().enumerate() {
            let row = box_top + i as u16;
            if row >= rows as u16 {
                break;
            }
            queue!(out, MoveTo(0, row), Print(line.clone()))
                .map_err(|e| anyhow::anyhow!("writing peek row: {e}"))?;
        }
    }

    // Footer pinned to the bottom of the screen.
    let footer_top = (rows - footer_height) as u16;
    if dispatching {
        let line1 = clip_to(&format!("› {buffer}"), cols);
        let line2 = clip_to("Enter to dispatch · Esc to cancel", cols);
        queue!(out, MoveTo(0, footer_top), Print(line1))
            .map_err(|e| anyhow::anyhow!("writing dispatch bar: {e}"))?;
        queue!(out, MoveTo(0, footer_top + 1), Print(line2))
            .map_err(|e| anyhow::anyhow!("writing dispatch hint: {e}"))?;
    } else {
        let hints = "↑↓ select · Space peek · Ctrl+X stop · / dispatch · q quit";
        let footer = match message {
            Some(msg) => clip_to(&format!("{hints} · {msg}"), cols),
            None => clip_to(hints, cols),
        };
        queue!(out, MoveTo(0, footer_top), Print(footer))
            .map_err(|e| anyhow::anyhow!("writing footer: {e}"))?;
    }

    out.flush().context("flushing stdout")?;
    Ok(())
}

/// Build the list region's lines, one per visible row. Returns at most
/// `max_rows` lines; an empty list yields a single explanatory line.
fn render_list(
    entries: &[AgentViewEntry],
    order: &[usize],
    selected: usize,
    cols: usize,
    max_rows: usize,
    now_ms: u64,
) -> Vec<String> {
    let mut lines = Vec::new();
    if order.is_empty() {
        // Don't push the placeholder if there's literally no room for it.
        if max_rows > 0 {
            lines.push("No agent sessions.".to_string());
        }
        return lines;
    }
    for (display_idx, &entry_idx) in order.iter().enumerate() {
        if lines.len() >= max_rows {
            break;
        }
        let entry = &entries[entry_idx];
        let is_selected = display_idx == selected;
        lines.push(row_line(entry, is_selected, cols, now_ms));
    }
    lines
}

/// Render one list row: `▶ icon name  preview  time`.
///
/// Width budgeting is approximate (char count, not terminal cell width)
/// — wide CJK glyphs will drift, but the alternative (a full wcwidth
/// pass) is out of scope for this view and the drift is cosmetic.
fn row_line(entry: &AgentViewEntry, selected: bool, cols: usize, now_ms: u64) -> String {
    let pointer = if selected { "▶" } else { " " };
    let icon = status_icon(entry.status);
    let name = clip_to(&sanitize_for_term(&entry.record.name), 20);
    let time = relative_time(entry.record.started_at_ms, now_ms);
    // Mail badge: show `✉ N` when this teammate has unread mail.
    let mail_badge = mail_badge_for(&entry.record);
    // Fixed columns: pointer(1) + sp(1) + icon(1) + sp(1) + name(20) + sp(2) + time(8) + sp(2) + badge(0-6).
    let badge_width = mail_badge.chars().count();
    let used = 2 + 2 + 20 + 2 + 8 + 2 + badge_width;
    let preview_budget = cols.saturating_sub(used).max(10);
    let preview = preview_text(&entry.record.prompt_preview, preview_budget);
    format!("{pointer} {icon} {name:<20}  {preview}  {time:>8}{mail_badge}")
}

/// Build a `✉ N` badge (with a leading space) when the record is a
/// teammate with unread mail. Empty string for plain background runs
/// or when the mailbox is empty/missing.
fn mail_badge_for(record: &BackgroundAgentRecord) -> String {
    let (Some(team), Some(teammate)) = (record.team.as_ref(), record.teammate_name.as_ref()) else {
        return String::new();
    };
    let cwd = std::path::Path::new(&record.cwd);
    let team_dir = cwd.join(".libertai").join("teams").join(team);
    let mailbox_dir = team_dir.join("mailbox").join(teammate);
    let unread = crate::commands::code_mailbox::count_unread(&mailbox_dir);
    if unread > 0 {
        format!("  \u{2709} {unread}")
    } else {
        String::new()
    }
}

/// State icon for a row. Working runs get a filled starburst (`✽`),
/// completed runs a bullet (`∙`), unknown runs a question mark (`?`).
fn status_icon(status: BackgroundAgentStatus) -> &'static str {
    match status {
        BackgroundAgentStatus::Running => "✽",
        BackgroundAgentStatus::Exited => "∙",
        BackgroundAgentStatus::Unknown => "?",
    }
}

/// Build the peek box lines: a top border carrying the header, the last
/// `PEEK_CONTENT_ROWS` lines of the tail (each prefixed `│ `), and a
/// bottom border. The box is padded to a stable height so it doesn't
/// flicker as the tail grows.
fn render_peek(peek: &str, cols: usize, name: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(PEEK_BOX_ROWS);

    // Top border with the header embedded: `┌─ peek: <name> · Esc to close ─…─`.
    let header = format!("peek: {} · Esc to close", sanitize_for_term(name));
    let top_prefix = format!("┌─ {header} ");
    let fill = cols.saturating_sub(top_prefix.chars().count());
    let mut top = top_prefix;
    top.push_str(&"─".repeat(fill));
    lines.push(clip_to(&top, cols));

    // Take the last `PEEK_CONTENT_ROWS` lines of the tail so the overlay
    // shows the most recent output regardless of how long the log is.
    let tail_lines: Vec<&str> = peek.lines().collect();
    let start = tail_lines.len().saturating_sub(PEEK_CONTENT_ROWS);
    let budget = cols.saturating_sub(2); // the `│ ` prefix
    for line in &tail_lines[start..] {
        let clipped = clip_to(&sanitize_for_term(line), budget);
        lines.push(format!("│ {clipped}"));
    }
    // Pad with empty content rows so the bottom border stays put.
    while lines.len() < 1 + PEEK_CONTENT_ROWS {
        lines.push("│ ".to_string());
    }

    // Bottom border: `└` + a run of `─` filling the rest of the width.
    let bottom = format!("└{}", "─".repeat(cols.saturating_sub(1)));
    lines.push(clip_to(&bottom, cols));

    lines
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `BackgroundAgentRecord` with the fields the view actually
    /// reads, filling the rest with harmless defaults. Keeps the tests
    /// short and insulated from future record-shape changes.
    fn rec(pid: u32, started: u64, cwd: &str, run_id: &str) -> BackgroundAgentRecord {
        BackgroundAgentRecord {
            pid,
            run_id: run_id.to_string(),
            name: format!("agent-{pid}"),
            provider: "libertai".to_string(),
            model: "test-model".to_string(),
            mode: "normal".to_string(),
            prompt_preview: "do stuff".to_string(),
            cwd: cwd.to_string(),
            log_path: format!("/tmp/agent-{pid}.log"),
            started_at_ms: started,
            launched_argv: Vec::new(),
            team: None,
            teammate_name: None,
        }
    }

    /// Build an `AgentViewEntry` straight from a pid / start time / status.
    fn entry(pid: u32, started: u64, status: BackgroundAgentStatus) -> AgentViewEntry {
        AgentViewEntry {
            record: rec(pid, started, "/", &format!("bg-{started}-{pid}")),
            status,
        }
    }

    // ── parse_permission_mode ──────────────────────────────────────────

    #[test]
    fn parse_permission_mode_maps_known_aliases() {
        assert_eq!(parse_permission_mode(None), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("normal")), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("default")), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("accept-edits")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("accept_edits")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("accept")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("plan")), Mode::Plan);
        assert_eq!(parse_permission_mode(Some("readonly")), Mode::Plan);
    }

    #[test]
    fn parse_permission_mode_is_case_insensitive() {
        assert_eq!(parse_permission_mode(Some("Accept-Edits")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("PLAN")), Mode::Plan);
    }

    #[test]
    fn parse_permission_mode_falls_back_on_typo_without_bailing() {
        // The view must not crash on a bad flag value — Normal is the safe default.
        assert_eq!(parse_permission_mode(Some("garbage")), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("")), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("   ")), Mode::Normal);
    }

    // ── group_entries ───────────────────────────────────────────────────

    #[test]
    fn group_entries_splits_by_status_and_sorts_newest_first() {
        // Indices: 0=exited@1000, 1=running@3000, 2=running@2000, 3=unknown@500.
        let entries = vec![
            entry(1, 1000, BackgroundAgentStatus::Exited),
            entry(2, 3000, BackgroundAgentStatus::Running),
            entry(3, 2000, BackgroundAgentStatus::Running),
            entry(4, 500, BackgroundAgentStatus::Unknown),
        ];
        let (working, completed, unknown) = group_entries(&entries);
        // Working: running pids sorted newest-first → 3000 (idx1), 2000 (idx2).
        assert_eq!(working, vec![1, 2]);
        // Completed: only the exited run.
        assert_eq!(completed, vec![0]);
        // Unknown: only the unknown run.
        assert_eq!(unknown, vec![3]);
    }

    #[test]
    fn group_entries_empty_input_yields_empty_buckets() {
        let entries: Vec<AgentViewEntry> = Vec::new();
        let (w, c, u) = group_entries(&entries);
        assert!(w.is_empty() && c.is_empty() && u.is_empty());
    }

    #[test]
    fn group_entries_preserves_load_order_for_equal_start_times() {
        // Two working runs started at the same ms: load order (idx0 before idx1)
        // is preserved by the stable sort.
        let entries = vec![
            entry(1, 5000, BackgroundAgentStatus::Running),
            entry(2, 5000, BackgroundAgentStatus::Running),
        ];
        let (working, _, _) = group_entries(&entries);
        assert_eq!(working, vec![0, 1]);
    }

    #[test]
    fn display_order_is_working_then_completed_then_unknown() {
        let entries = vec![
            entry(1, 100, BackgroundAgentStatus::Unknown),
            entry(2, 200, BackgroundAgentStatus::Exited),
            entry(3, 300, BackgroundAgentStatus::Running),
        ];
        let order = display_order(&entries);
        // Working (idx2) first, then Completed (idx1), then Unknown (idx0).
        assert_eq!(order, vec![2, 1, 0]);
    }

    // ── slug_from_prompt ────────────────────────────────────────────────

    #[test]
    fn slug_from_prompt_joins_first_four_words() {
        assert_eq!(slug_from_prompt("Fix the login bug now"), "fix-the-login-bug");
        // Only the first four words count even if more are given.
        assert_eq!(
            slug_from_prompt("Fix the login bug please now"),
            "fix-the-login-bug"
        );
    }

    #[test]
    fn slug_from_prompt_replaces_punctuation_with_dashes() {
        assert_eq!(slug_from_prompt("Fix!! the login"), "fix-the-login");
        assert_eq!(slug_from_prompt("Refactor: drop the old API"), "refactor-drop-the-old");
    }

    #[test]
    fn slug_from_prompt_collapses_runs_of_dashes() {
        assert_eq!(slug_from_prompt("a!! b c d"), "a-b-c-d");
    }

    #[test]
    fn slug_from_prompt_defaults_when_empty_or_punctuation_only() {
        assert_eq!(slug_from_prompt(""), "agent");
        assert_eq!(slug_from_prompt("   "), "agent");
        assert_eq!(slug_from_prompt("!!! ???"), "agent");
    }

    #[test]
    fn slug_from_prompt_lowercases_uppercase_input() {
        assert_eq!(slug_from_prompt("Refactor"), "refactor");
        assert_eq!(slug_from_prompt("FIX THE LOGIN BUG"), "fix-the-login-bug");
    }

    #[test]
    fn slug_from_prompt_handles_fewer_than_four_words() {
        assert_eq!(slug_from_prompt("one two"), "one-two");
        assert_eq!(slug_from_prompt("solo"), "solo");
    }

    // ── filter_by_cwd ───────────────────────────────────────────────────

    #[test]
    fn filter_by_cwd_none_keeps_everything() {
        let records = vec![
            rec(1, 1, "/a/b", "r1"),
            rec(2, 2, "/c/d", "r2"),
        ];
        assert_eq!(filter_by_cwd(records, None).len(), 2);
    }

    #[test]
    fn filter_by_cwd_keeps_only_records_under_the_path() {
        let records = vec![
            rec(1, 1, "/a/b", "r1"),
            rec(2, 2, "/c/d", "r2"),
            rec(3, 3, "/a", "r3"),
        ];
        let kept = filter_by_cwd(records, Some(Path::new("/a")));
        // /a/b and /a match; /c/d does not. Order is preserved.
        let pids: Vec<u32> = kept.iter().map(|r| r.pid).collect();
        assert_eq!(pids, vec![1, 3]);
    }

    #[test]
    fn filter_by_cwd_does_not_match_partial_components() {
        // Path::starts_with is component-wise, so `/a` must NOT match `/abc`.
        let records = vec![
            rec(1, 1, "/abc", "r1"),
            rec(2, 2, "/a/x", "r2"),
        ];
        let kept = filter_by_cwd(records, Some(Path::new("/a")));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].pid, 2);
    }

    // ── relative_time ──────────────────────────────────────────────────

    #[test]
    fn relative_time_formats_age_buckets() {
        assert_eq!(relative_time(0, 30_000), "just now");
        assert_eq!(relative_time(0, 120_000), "2m ago");
        assert_eq!(relative_time(0, 7_200_000), "2h ago");
        assert_eq!(relative_time(0, 86_400_000), "1d ago");
    }

    #[test]
    fn relative_time_is_safe_when_started_is_in_the_future() {
        // A clock skew or a record from the future shouldn't panic; saturating
        // subtraction yields "just now".
        assert_eq!(relative_time(1_000_000, 0), "just now");
    }

    // ── clip_to ─────────────────────────────────────────────────────────

    #[test]
    fn clip_to_truncates_with_ellipsis() {
        assert_eq!(clip_to("hello world", 5), "hell…");
        assert_eq!(clip_to("abc", 3), "abc");
        assert_eq!(clip_to("abc", 0), "");
        assert_eq!(clip_to("ab", 1), "…");
    }
}
