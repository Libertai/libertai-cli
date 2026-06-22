//! Standalone `libertai agents` TUI — ratatui rewrite.
//!
//! Shows background `libertai code --bg` runs from the on-disk JSONL
//! index, lets you peek at logs, stop runs, and dispatch new ones.
//! Polls pids every second via the event-loop tick.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use serde_json::json;

use crate::commands::code_factory::Mode;
use crate::commands::code_tui::theme;
use crate::commands::code_ui::{
    background_agent_status, load_background_agent_records, read_log_tail,
    retain_running_background_agent_records, rewrite_background_agent_records,
    send_background_agent_kill, start_background_agent, BackgroundAgentLaunch,
    BackgroundAgentRecord, BackgroundAgentStatus,
};

/// Poll interval — doubles as the refresh tick (1s).
const REFRESH_TICK: Duration = Duration::from_millis(1000);

/// Bytes pulled into the peek overlay.
const PEEK_TAIL_BYTES: usize = 64_000;

/// Content rows in the peek box.
const PEEK_CONTENT_ROWS: usize = 20;

// ---------------------------------------------------------------------------
// RAII guard
// ---------------------------------------------------------------------------

struct TerminalGuard {
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
            let _ = terminal.show_cursor();
        }
    }
}

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AgentViewEntry {
    record: BackgroundAgentRecord,
    status: BackgroundAgentStatus,
}

struct ViewConfig {
    model: String,
    provider: String,
    cwd_filter: Option<PathBuf>,
    cwd_label: String,
    mode: Mode,
    agent: Option<String>,
}

#[derive(Debug, Default)]
struct ViewState {
    entries: Vec<AgentViewEntry>,
    selected: usize,
    peek: Option<String>,
    dispatching: bool,
    buffer: String,
    message: Option<String>,
    last_refresh: Option<std::time::Instant>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(
    cwd: Option<String>,
    json: bool,
    model: Option<String>,
    permission_mode: Option<String>,
    agent: Option<String>,
) -> Result<()> {
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
    if !std::io::stdout().is_terminal() {
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

// ---------------------------------------------------------------------------
// JSON / plain output (no TUI)
// ---------------------------------------------------------------------------

fn print_json(cwd_filter: Option<&Path>) -> Result<()> {
    let records = load_background_agent_records()?;
    let records = filter_by_cwd(records, cwd_filter);
    let arr: Vec<serde_json::Value> = records
        .into_iter()
        .map(|r| {
            let status = background_agent_status(r.pid);
            json!({
                "id": r.run_id,
                "pid": r.pid,
                "name": r.name,
                "model": r.model,
                "cwd": r.cwd,
                "state": match status {
                    BackgroundAgentStatus::Running => "working",
                    BackgroundAgentStatus::Exited => "completed",
                    BackgroundAgentStatus::Unknown => "unknown",
                },
                "promptPreview": r.prompt_preview,
                "startedAtMs": r.started_at_ms,
                "logPath": r.log_path,
                "team": r.team,
                "teammateName": r.teammate_name,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr)?);
    Ok(())
}

fn print_plain(cwd_filter: Option<&Path>) -> Result<()> {
    let records = load_background_agent_records()?;
    let records = filter_by_cwd(records, cwd_filter);
    let entries = build_entries(records);
    let (working, completed, unknown) = group_entries(&entries);
    let order = display_order(&entries);

    if order.is_empty() {
        println!("No agent sessions.");
        return Ok(());
    }

    for (label, group) in [
        ("Working", &working),
        ("Completed", &completed),
        ("Unknown", &unknown),
    ] {
        if group.is_empty() {
            continue;
        }
        println!("{label}:");
        for &idx in group {
            let entry = &entries[idx];
            let icon = status_icon(entry.status);
            let time = relative_time(entry.record.started_at_ms, now_epoch_ms());
            println!("  {icon} {}  {}  {time}", entry.record.name, entry.record.prompt_preview);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TUI loop
// ---------------------------------------------------------------------------

fn run_tui(config: &ViewConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;

    let mut guard = TerminalGuard {
        terminal: Some(terminal),
    };
    let terminal = guard.terminal.as_mut().unwrap();

    let mut state = ViewState::default();
    let mut list_state = ListState::default();
    refresh(&mut state, config)?;

    let tick = Duration::from_millis(theme::TICK_RATE_MS);
    let mut since_refresh = Duration::ZERO;

    loop {
        terminal.draw(|frame| draw(frame, &mut state, &mut list_state, config))?;

        if event::poll(tick)? {
            if let Event::Key(key) = event::read()? {
                if handle_key(&mut state, config, key)? {
                    break;
                }
            }
        } else {
            since_refresh += tick;
            if since_refresh >= REFRESH_TICK {
                refresh(&mut state, config)?;
                since_refresh = Duration::ZERO;
            }
        }
    }

    drop(guard);
    Ok(())
}

fn draw(
    frame: &mut ratatui::Frame,
    state: &mut ViewState,
    list_state: &mut ListState,
    config: &ViewConfig,
) {
    let area = frame.area();

    // Consume the one-shot message.
    let message = state.message.take();

    // Layout: header | list | [peek] | footer
    let peek_height = if state.peek.is_some() {
        PEEK_CONTENT_ROWS as u16 + 2
    } else {
        0
    };
    let footer_height = if state.dispatching { 2 } else { 1 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),           // header
            Constraint::Min(1),             // list
            Constraint::Length(peek_height), // peek (0 when hidden)
            Constraint::Length(footer_height), // footer
        ])
        .split(area);

    draw_header(frame, chunks[0], state, config);
    draw_list(frame, chunks[1], state, list_state);
    if state.peek.is_some() {
        draw_peek(frame, chunks[2], state);
    }
    draw_footer(frame, chunks[3], state, &message);
}

fn draw_header(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &ViewState,
    config: &ViewConfig,
) {
    let (working, completed, _) = group_entries(&state.entries);
    let total = state.entries.len();
    let header = format!(
        "LibertAI agents · {} · {} · {} total ({} working, {} completed)",
        config.model, config.cwd_label, total, working.len(), completed.len()
    );
    let line = Line::from(vec![Span::styled(
        header,
        Style::default().add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_list(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &mut ViewState,
    list_state: &mut ListState,
) {
    let order = display_order(&state.entries);

    if order.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::raw("No agent sessions."))),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = order
        .iter()
        .map(|&idx| {
            let entry = &state.entries[idx];
            let icon = status_icon(entry.status);
            let icon_style = match entry.status {
                BackgroundAgentStatus::Running => theme::accent(),
                BackgroundAgentStatus::Exited => theme::muted(),
                BackgroundAgentStatus::Unknown => theme::warning(),
            };
            let name = clip_to(&entry.record.name, 20);
            let time = relative_time(entry.record.started_at_ms, now_epoch_ms());
            let preview = clip_to(&entry.record.prompt_preview, area.width as usize);

            let spans = vec![
                Span::styled(icon.to_string(), icon_style),
                Span::raw(" "),
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(preview, theme::muted()),
                Span::raw("  "),
                Span::styled(time, theme::muted()),
            ];
            ListItem::new(Line::from(spans))
        })
        .collect();

    list_state.select(Some(state.selected));

    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, area, list_state);
}

fn draw_peek(frame: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    let order = display_order(&state.entries);
    let name = order
        .get(state.selected)
        .and_then(|&idx| state.entries.get(idx))
        .map(|e| e.record.name.as_str())
        .unwrap_or("?");

    let title = format!("peek: {name} · Esc to close");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Clear, area);

    let peek = state.peek.as_deref().unwrap_or("");
    let all_lines: Vec<&str> = peek.lines().collect();
    let start = all_lines.len().saturating_sub(PEEK_CONTENT_ROWS);
    let lines: Vec<Line> = all_lines[start..]
        .iter()
        .map(|l| Line::from(Span::styled(l.to_string(), theme::muted())))
        .collect();

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_footer(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &ViewState,
    message: &Option<String>,
) {
    if state.dispatching {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);

        let input_line = Line::from(vec![
            Span::styled("› ", theme::bold_accent()),
            Span::raw(&state.buffer),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);

        let hint = "Enter to dispatch · Esc to cancel";
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme::muted()))),
            chunks[1],
        );
    } else {
        let mut hint = "↑↓ select · Space peek · Ctrl+X stop · / dispatch · q quit".to_string();
        if let Some(msg) = message {
            hint.push_str(&format!(" · {msg}"));
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme::muted()))),
            area,
        );
    }
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

fn handle_key(state: &mut ViewState, config: &ViewConfig, key: KeyEvent) -> Result<bool> {
    if state.dispatching {
        return handle_dispatch_key(state, config, key);
    }

    let order = display_order(&state.entries);

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => return Ok(true),
        (KeyCode::Esc, _) => {
            if state.peek.is_some() {
                state.peek = None;
            } else {
                return Ok(true);
            }
        }
        (KeyCode::Up, _) => {
            state.selected = state.selected.saturating_sub(1);
            state.peek = None;
        }
        (KeyCode::Down, _) => {
            let max = order.len().saturating_sub(1);
            state.selected = (state.selected + 1).min(max);
            state.peek = None;
        }
        (KeyCode::Char(' '), _) => {
            if state.peek.is_some() {
                state.peek = None;
            } else if let Some(&idx) = order.get(state.selected) {
                match read_log_tail(Path::new(&state.entries[idx].record.log_path), PEEK_TAIL_BYTES) {
                    Ok(tail) => state.peek = Some(tail),
                    Err(e) => state.peek = Some(format!("could not read log: {e:#}")),
                }
            }
        }
        (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
            if let Some(&idx) = order.get(state.selected) {
                let pid = state.entries[idx].record.pid;
                let _ = send_background_agent_kill(pid);
                let records = load_background_agent_records()?;
                let running = retain_running_background_agent_records(records, background_agent_status);
                rewrite_background_agent_records(&running)?;
                state.message = Some(format!("stopped pid {pid}"));
                refresh(state, config)?;
            }
        }
        (KeyCode::Char('/'), _) => {
            state.dispatching = true;
            state.buffer.clear();
            state.peek = None;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_dispatch_key(
    state: &mut ViewState,
    config: &ViewConfig,
    key: KeyEvent,
) -> Result<bool> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(true),
        (KeyCode::Esc, _) => {
            state.dispatching = false;
            state.buffer.clear();
        }
        (KeyCode::Enter, _) => {
            let prompt = state.buffer.clone();
            if !prompt.trim().is_empty() {
                match dispatch(config, &prompt) {
                    Ok(started) => {
                        state.message = Some(format!(
                            "dispatched `{}` (pid {})",
                            slug_from_prompt(&prompt),
                            started.pid
                        ));
                    }
                    Err(e) => {
                        state.message = Some(format!("dispatch failed: {e:#}"));
                    }
                }
            }
            state.dispatching = false;
            state.buffer.clear();
            refresh(state, config)?;
        }
        (KeyCode::Backspace, _) => {
            state.buffer.pop();
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) if !c.is_control() => {
            state.buffer.push(c);
        }
        _ => {}
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Helpers (pure, testable)
// ---------------------------------------------------------------------------

fn parse_permission_mode(s: Option<&str>) -> Mode {
    match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("accept-edits") | Some("accept_edits") | Some("accept") => Mode::AcceptEdits,
        Some("plan") | Some("readonly") | Some("read-only") => Mode::Plan,
        _ => Mode::Normal,
    }
}

fn refresh(state: &mut ViewState, config: &ViewConfig) -> Result<()> {
    let records = load_background_agent_records().context("loading background agent records")?;
    let records = filter_by_cwd(records, config.cwd_filter.as_deref());
    state.entries = build_entries(records);
    let order = display_order(&state.entries);
    if order.is_empty() {
        state.selected = 0;
    } else if state.selected >= order.len() {
        state.selected = order.len() - 1;
    }
    state.last_refresh = Some(std::time::Instant::now());
    Ok(())
}

fn build_entries(records: Vec<BackgroundAgentRecord>) -> Vec<AgentViewEntry> {
    records
        .into_iter()
        .map(|r| {
            let status = background_agent_status(r.pid);
            AgentViewEntry { record: r, status }
        })
        .collect()
}

fn group_entries(entries: &[AgentViewEntry]) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let mut working = Vec::new();
    let mut completed = Vec::new();
    let mut unknown = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match e.status {
            BackgroundAgentStatus::Running => working.push(i),
            BackgroundAgentStatus::Exited => completed.push(i),
            BackgroundAgentStatus::Unknown => unknown.push(i),
        }
    }
    sort_newest_first(&mut working, entries);
    sort_newest_first(&mut completed, entries);
    sort_newest_first(&mut unknown, entries);
    (working, completed, unknown)
}

fn sort_newest_first(bucket: &mut [usize], entries: &[AgentViewEntry]) {
    bucket.sort_by_key(|&i| std::cmp::Reverse(entries[i].record.started_at_ms));
}

fn display_order(entries: &[AgentViewEntry]) -> Vec<usize> {
    let (working, completed, unknown) = group_entries(entries);
    working.into_iter().chain(completed).chain(unknown).collect()
}

fn filter_by_cwd(records: Vec<BackgroundAgentRecord>, cwd: Option<&Path>) -> Vec<BackgroundAgentRecord> {
    match cwd {
        Some(cwd) => records
            .into_iter()
            .filter(|r| Path::new(&r.cwd).starts_with(cwd))
            .collect(),
        None => records,
    }
}

fn slug_from_prompt(prompt: &str) -> String {
    let words: Vec<&str> = prompt.split_whitespace().take(4).collect();
    let slug: String = words
        .join(" ")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() { "agent".to_string() } else { slug }
}

fn relative_time(started_at_ms: u64, now_ms: u64) -> String {
    let elapsed = now_ms.saturating_sub(started_at_ms);
    let secs = elapsed / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn clip_to(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let truncated: String = chars[..max.saturating_sub(1)].iter().collect();
    format!("{truncated}…")
}

fn status_icon(status: BackgroundAgentStatus) -> &'static str {
    match status {
        BackgroundAgentStatus::Running => "✽",
        BackgroundAgentStatus::Exited => "✓",
        BackgroundAgentStatus::Unknown => "?",
    }
}

fn dispatch(config: &ViewConfig, prompt: &str) -> Result<crate::commands::code_ui::StartedBackgroundAgent> {
    let slug = slug_from_prompt(prompt);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let launch = BackgroundAgentLaunch {
        name: slug,
        provider: config.provider.clone(),
        model: config.model.clone(),
        mode: config.mode,
        prompt: prompt.to_string(),
        cwd,
        team: None,
        teammate_name: None,
        agent: config.agent.clone(),
    };
    start_background_agent(&launch)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rec(pid: u32, started: u64, cwd: &str, run_id: &str) -> BackgroundAgentRecord {
        BackgroundAgentRecord {
            pid,
            run_id: run_id.to_string(),
            name: format!("agent-{pid}"),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            mode: "normal".to_string(),
            prompt_preview: "do stuff".to_string(),
            cwd: cwd.to_string(),
            log_path: "/tmp/log".to_string(),
            started_at_ms: started,
            launched_argv: vec![],
            team: None,
            teammate_name: None,
        }
    }

    fn entry(pid: u32, started: u64, status: BackgroundAgentStatus) -> AgentViewEntry {
        AgentViewEntry {
            record: rec(pid, started, "/tmp", &format!("run-{pid}")),
            status,
        }
    }

    #[test]
    fn parse_permission_mode_maps_known_aliases() {
        assert_eq!(parse_permission_mode(Some("normal")), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("accept-edits")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("accept_edits")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("accept")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("plan")), Mode::Plan);
        assert_eq!(parse_permission_mode(Some("readonly")), Mode::Plan);
        assert_eq!(parse_permission_mode(Some("read-only")), Mode::Plan);
    }

    #[test]
    fn parse_permission_mode_is_case_insensitive() {
        assert_eq!(parse_permission_mode(Some("Accept-Edits")), Mode::AcceptEdits);
        assert_eq!(parse_permission_mode(Some("PLAN")), Mode::Plan);
    }

    #[test]
    fn parse_permission_mode_falls_back_on_typo() {
        assert_eq!(parse_permission_mode(Some("garbage")), Mode::Normal);
        assert_eq!(parse_permission_mode(Some("")), Mode::Normal);
        assert_eq!(parse_permission_mode(None), Mode::Normal);
    }

    #[test]
    fn group_entries_splits_by_status_and_sorts_newest_first() {
        let entries = vec![
            entry(1, 100, BackgroundAgentStatus::Exited),
            entry(2, 300, BackgroundAgentStatus::Running),
            entry(3, 200, BackgroundAgentStatus::Running),
            entry(4, 50, BackgroundAgentStatus::Unknown),
        ];
        let (working, completed, unknown) = group_entries(&entries);
        assert_eq!(working, vec![1, 2]); // indices: entry[1] started=300, entry[2] started=200
        assert_eq!(completed, vec![0]);
        assert_eq!(unknown, vec![3]);
    }

    #[test]
    fn display_order_is_working_then_completed_then_unknown() {
        let entries = vec![
            entry(1, 100, BackgroundAgentStatus::Exited),
            entry(2, 300, BackgroundAgentStatus::Running),
            entry(3, 200, BackgroundAgentStatus::Running),
        ];
        let order = display_order(&entries);
        assert_eq!(order, vec![1, 2, 0]); // working (newest first) then completed
    }

    #[test]
    fn slug_from_prompt_joins_first_four_words() {
        assert_eq!(slug_from_prompt("fix the build now please"), "fix-the-build-now");
    }

    #[test]
    fn slug_from_prompt_replaces_punctuation_with_dashes() {
        assert_eq!(slug_from_prompt("hello, world!"), "hello-world");
    }

    #[test]
    fn slug_from_prompt_defaults_when_empty() {
        assert_eq!(slug_from_prompt(""), "agent");
        assert_eq!(slug_from_prompt("!!!"), "agent");
    }

    #[test]
    fn slug_from_prompt_lowercases() {
        assert_eq!(slug_from_prompt("Fix The Build"), "fix-the-build");
    }

    #[test]
    fn relative_time_formats_age_buckets() {
        let now = 100_000_000; // 100s in ms
        assert_eq!(relative_time(99_950_000, now), "just now");     // 50s ago
        assert_eq!(relative_time(99_880_000, now), "2m ago");       // 120s = 2m ago
        assert_eq!(relative_time(92_800_000, now), "2h ago");      // 7200s = 2h ago
        assert_eq!(relative_time(13_600_000, now), "1d ago");      // 86400s = 1d ago
    }

    #[test]
    fn relative_time_is_safe_when_started_is_in_the_future() {
        let now = 100_000_000;
        assert_eq!(relative_time(200_000_000, now), "just now");
    }

    #[test]
    fn clip_to_truncates_with_ellipsis() {
        assert_eq!(clip_to("hello world", 5), "hell…");
        assert_eq!(clip_to("abc", 0), "");
        assert_eq!(clip_to("ab", 1), "…");
        assert_eq!(clip_to("abc", 3), "abc");
    }

    #[test]
    fn filter_by_cwd_none_keeps_everything() {
        let records = vec![rec(1, 0, "/a", "r1"), rec(2, 0, "/b", "r2")];
        let result = filter_by_cwd(records, None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_by_cwd_keeps_only_records_under_the_path() {
        let records = vec![
            rec(1, 0, "/a/b", "r1"),
            rec(2, 0, "/a", "r2"),
            rec(3, 0, "/c", "r3"),
        ];
        let result = filter_by_cwd(records, Some(Path::new("/a")));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_by_cwd_does_not_match_partial_components() {
        let records = vec![rec(1, 0, "/abc", "r1")];
        let result = filter_by_cwd(records, Some(Path::new("/a")));
        assert_eq!(result.len(), 0);
    }
}
