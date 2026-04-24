//! `libertai code` interactive REPL.
//!
//! Renders a Claude-Code-style bottom-anchored input bar in raw mode.
//! Between prompts the bar waits for input; during a prompt the agent's
//! streaming output flows in plain terminal text above the bar. We hand
//! the agent renderer the cooked terminal (raw mode off) so normal
//! newlines/flushes behave, then re-enter raw mode to read the next line.
//!
//! v0 non-goals: typing during a running prompt (pi callback fires on the
//! runtime thread; mixing that with a parallel stdin reader is out of
//! scope); persistent history file; multi-line paste; syntax highlighting.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

use pi::model::AssistantMessageEvent;
use pi::sdk::{create_agent_session, AbortHandle, AgentEvent, AgentSessionHandle, SessionOptions};

use crate::commands::code_approvals::ApprovalState;
use crate::commands::code_factory::{LibertaiToolFactory, Mode};

/// ANSI dim/bold helpers for cooked output (agent streaming phase).
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Snapshot of the last completed turn's token usage. Written in
/// `repl_loop` after each successful prompt, read in `repaint()` to
/// render the context-usage strip on the rule line.
#[derive(Default, Clone)]
struct BarStatus {
    model_label: String,
    input_tokens: u64,
    context_window: u32,
}

static BAR_STATUS: Mutex<Option<BarStatus>> = Mutex::new(None);

/// Current in-flight abort handle, populated for the duration of each
/// `handle.prompt_with_abort` call. The Ctrl-C handler (installed once at
/// startup) looks at this slot to decide whether to abort or let the
/// signal fall through to the usual process-exit behaviour.
static CURRENT_ABORT: Mutex<Option<AbortHandle>> = Mutex::new(None);

fn install_ctrlc_handler() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = ctrlc::set_handler(|| {
        if let Ok(guard) = CURRENT_ABORT.lock() {
            if let Some(h) = guard.as_ref() {
                h.abort();
            }
        }
    });
}

fn set_current_abort(h: AbortHandle) {
    if let Ok(mut g) = CURRENT_ABORT.lock() {
        *g = Some(h);
    }
}

fn clear_current_abort() {
    if let Ok(mut g) = CURRENT_ABORT.lock() {
        *g = None;
    }
}

fn set_bar_status(status: BarStatus) {
    if let Ok(mut g) = BAR_STATUS.lock() {
        *g = Some(status);
    }
}

fn rule_chip(cols: usize) -> String {
    let status = BAR_STATUS.lock().ok().and_then(|g| g.clone());
    let inner = match status {
        Some(s) if s.context_window > 0 => {
            let pct = (f64::from(s.input_tokens as u32).min(f64::from(s.context_window))
                / f64::from(s.context_window)
                * 100.0)
                .round() as u32;
            format!(
                " {pct}% · {} / {} · {} ",
                human_tokens(s.input_tokens),
                human_tokens(u64::from(s.context_window)),
                s.model_label
            )
        }
        Some(s) => format!(" {} ", s.model_label),
        None => String::new(),
    };
    // Pad with ─ so the whole line fills the terminal width.
    let chip_len = inner.chars().count();
    if chip_len + 4 >= cols || cols == 0 {
        return "\u{2500}".repeat(cols.max(1));
    }
    let left = (cols - chip_len) / 2;
    let right = cols - chip_len - left;
    format!(
        "{}{}{}",
        "\u{2500}".repeat(left),
        inner,
        "\u{2500}".repeat(right)
    )
}

fn human_tokens(n: u64) -> String {
    if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Best-effort context-window lookup. LibertAI doesn't expose this on
/// `/v1/models` today; hardcode for the models we ship defaults for and
/// fall back to a sensible cap otherwise.
fn context_window_for(model: &str) -> u32 {
    match model {
        "qwen3.6-35b-a3b" => 32_768,
        "qwen3.5-122b-a10b" => 32_768,
        "gemma-4-31b-it" => 32_768,
        _ => 32_768,
    }
}

/// Outcome of reading one input line in raw mode.
enum LineResult {
    /// User pressed Enter with the given text.
    Submit(String),
    /// User sent EOF (Ctrl+D on an empty line) → caller should exit.
    Eof,
    /// User pressed Ctrl+C during input → caller should discard and loop.
    Interrupted,
    /// User pressed Shift+Tab → toggle Normal ↔ Plan mode.
    ToggleMode,
}

/// RAII guard that puts the terminal in raw mode for its lifetime.
///
/// Disables raw mode on drop even on panic / `?`-unwind, so we never
/// leave the user with a broken terminal.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("enable_raw_mode: {e}"))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Entry point from `code::run` when the command line has no prompt.
///
/// Owns the asupersync runtime, builds one `AgentSessionHandle`, then
/// drives the REPL loop against it.
pub fn run_interactive(provider: String, model: String, mode: Mode) -> Result<()> {
    print_banner(&provider, &model, mode);

    // Prime the status bar so the rule renders a useful label even
    // before the first turn completes.
    set_bar_status(BarStatus {
        model_label: format!("{provider}/{model}"),
        input_tokens: 0,
        context_window: context_window_for(&model),
    });

    // Forward Ctrl-C during streaming to pi's AbortHandle.
    install_ctrlc_handler();

    // Shared across prompts AND across mode toggles: the approvals
    // allowlist lives for the whole REPL lifetime, so "always allow bash"
    // sticks across a Shift+Tab trip through Plan mode.
    let approvals = Arc::new(ApprovalState::new());

    // Same asupersync setup as the non-interactive path.
    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|e| anyhow::anyhow!("asupersync reactor: {e}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!("asupersync runtime: {e}"))?;

    runtime.block_on(async move { repl_loop(provider, model, mode, approvals).await })
}

fn print_banner(provider: &str, model: &str, mode: Mode) {
    let mode_tag = match mode {
        Mode::Normal => String::new(),
        Mode::Plan => format!(" {DIM}[plan]{RESET}"),
    };
    println!(
        "{BOLD}libertai code{RESET} {DIM}— interactive ({provider}/{model}){RESET}{mode_tag}"
    );
    println!("{DIM}  type /help for commands, /exit or Ctrl+D to quit{RESET}");
    println!();
}

async fn repl_loop(
    provider: String,
    model: String,
    initial_mode: Mode,
    approvals: Arc<ApprovalState>,
) -> Result<()> {
    let mut mode = initial_mode;
    let mut handle = build_handle(&provider, &model, mode, Arc::clone(&approvals)).await?;

    // In-memory input history (no persistence in v0).
    let mut history: VecDeque<String> = VecDeque::with_capacity(64);

    loop {
        let line = match read_line(mode, &history)? {
            LineResult::Submit(s) => s,
            LineResult::Interrupted => {
                // Ctrl+C in the input bar: discard this line, keep looping.
                // We're now in cooked mode (read_line restored it); emit a
                // visible marker so the user knows the cancel registered.
                println!("{DIM}  (interrupted){RESET}");
                continue;
            }
            LineResult::Eof => {
                println!("{DIM}goodbye.{RESET}");
                return Ok(());
            }
            LineResult::ToggleMode => {
                mode = flip_mode(mode);
                announce_mode_change(mode);
                handle = build_handle(&provider, &model, mode, Arc::clone(&approvals)).await?;
                continue;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed {
            "/exit" | "/quit" => {
                println!("{DIM}goodbye.{RESET}");
                return Ok(());
            }
            "/help" => {
                print_help();
                continue;
            }
            "/plan" => {
                mode = flip_mode(mode);
                announce_mode_change(mode);
                handle = build_handle(&provider, &model, mode, Arc::clone(&approvals)).await?;
                continue;
            }
            "/forget" => {
                approvals.forget();
                println!("{DIM}  cleared session-scoped \"always allow\" list.{RESET}");
                continue;
            }
            "/clear" => {
                // Scroll the screen and rebuild the session — agent
                // history was already lost by pi's ephemeral-Session
                // policy, but wiping the terminal is still useful when
                // output has accumulated.
                let _ = std::io::stdout().write_all(b"\x1b[2J\x1b[H");
                let _ = std::io::stdout().flush();
                handle = build_handle(&provider, &model, mode, Arc::clone(&approvals)).await?;
                println!("{DIM}  → fresh session.{RESET}");
                continue;
            }
            _ => {}
        }

        // Remember the submitted line.
        if history.back().is_none_or(|last| last != trimmed) {
            if history.len() == 64 {
                history.pop_front();
            }
            history.push_back(trimmed.to_string());
        }

        // Echo the submitted user line as a chip above the stream region.
        println!("{BOLD}\u{276f} {}{RESET}", trimmed);

        // Hand off to pi with an abort signal so the Ctrl-C handler can
        // interrupt an in-flight turn without tearing the REPL down.
        let (abort_handle, abort_signal) = AbortHandle::new();
        set_current_abort(abort_handle);
        let result = handle
            .prompt_with_abort(line, abort_signal, render_event)
            .await;
        clear_current_abort();

        // Always end on a newline regardless of the last event kind.
        println!();

        match result {
            Ok(msg) => {
                // Update the status bar with this turn's input-token count
                // so the next repaint reflects real context usage.
                set_bar_status(BarStatus {
                    model_label: format!("{}/{}", msg.provider, msg.model),
                    input_tokens: msg.usage.input,
                    context_window: context_window_for(&msg.model),
                });
                eprintln!(
                    "{DIM}  {}/{}  stop: {:?}  in={} out={}{RESET}",
                    msg.provider,
                    msg.model,
                    msg.stop_reason,
                    msg.usage.input,
                    msg.usage.output,
                );
            }
            Err(e) => {
                // Distinguish user-aborts from real errors so the UI
                // stays calm on Ctrl-C.
                let msg = format!("{e}");
                if msg.contains("abort") || msg.contains("cancel") || msg.contains("Aborted") {
                    eprintln!("{DIM}  (interrupted){RESET}");
                } else {
                    eprintln!("{DIM}  error: {msg}{RESET}");
                }
            }
        }
        println!();
    }
}

fn flip_mode(m: Mode) -> Mode {
    match m {
        Mode::Normal => Mode::Plan,
        Mode::Plan => Mode::Normal,
    }
}

fn announce_mode_change(new_mode: Mode) {
    match new_mode {
        Mode::Normal => {
            println!(
                "{DIM}  → normal mode. mutating tools (bash, edit, write) are back online. session history reset.{RESET}"
            );
        }
        Mode::Plan => {
            println!(
                "{DIM}  → plan mode. only read, grep, find, ls are available — the agent can research but not modify. session history reset.{RESET}"
            );
        }
    }
}

async fn build_handle(
    provider: &str,
    model: &str,
    mode: Mode,
    approvals: Arc<ApprovalState>,
) -> Result<AgentSessionHandle> {
    let factory = Arc::new(LibertaiToolFactory::new(mode, approvals));
    let options = SessionOptions {
        provider: Some(provider.to_string()),
        model: Some(model.to_string()),
        no_session: true,
        max_tool_iterations: 50,
        tool_factory: Some(factory),
        ..SessionOptions::default()
    };
    create_agent_session(options)
        .await
        .map_err(|e| anyhow::anyhow!("create_agent_session: {e}"))
}

fn print_help() {
    println!("{DIM}  /help     — show this message{RESET}");
    println!("{DIM}  /exit     — quit the REPL (also /quit, Ctrl+D){RESET}");
    println!("{DIM}  /plan     — toggle plan mode (also Shift+Tab){RESET}");
    println!("{DIM}  /clear    — wipe the screen and start a fresh session{RESET}");
    println!("{DIM}  /forget   — clear the session \"always allow\" list{RESET}");
    println!("{DIM}  ↑ / ↓     — walk through previously submitted prompts{RESET}");
    println!("{DIM}  ← / →     — move cursor in the current line{RESET}");
    println!("{DIM}  Ctrl+C    — cancel the line / interrupt streaming{RESET}");
    println!();
}

/// Event callback handed to `handle.prompt`. Must be `Fn + Send + Sync +
/// 'static`, so it can't borrow local state — we just write to stdout.
fn render_event(event: AgentEvent) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            print!("{delta}");
            let _ = io::stdout().flush();
        }
        // Only mark turns past the first; turn 0 is the initial user
        // prompt and would just add noise above the first response.
        AgentEvent::TurnStart { turn_index, .. } if turn_index > 0 => {
            println!("\n{DIM}  [turn {turn_index}]{RESET}");
        }
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            println!("\n{DIM}  [tool] {tool_name}{RESET}");
        }
        AgentEvent::AgentEnd { .. } => {
            // Pi doesn't emit a trailing newline after the last text delta;
            // seed one here so the usage stats line lands cleanly below.
            println!();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Input bar — raw-mode line editor
// ---------------------------------------------------------------------------

/// Read a single line with a Claude-Code-style input bar:
///
/// ```text
/// ────────────────────
/// ❯ hello wor_
/// ```
///
/// Stays in raw mode for the duration, redrawing on every keystroke and
/// on `Resize`. Returns `LineResult::Submit` on Enter,
/// `LineResult::Interrupted` on Ctrl+C, `LineResult::Eof` on Ctrl+D of an
/// empty buffer.
fn read_line(mode: Mode, history: &VecDeque<String>) -> Result<LineResult> {
    let _guard = RawModeGuard::enter()?;

    let mut stdout = io::stdout();
    execute!(stdout, cursor::Show)?;

    let mut buffer: Vec<char> = Vec::new();
    let mut cursor_pos: usize = 0; // index within `buffer`
    // History cursor. `None` means "live buffer" (not walking history).
    // A `Some(i)` points at `history[history.len() - 1 - i]` — Up
    // increments, Down decrements, Enter/edit commits the recalled line
    // back to the live buffer.
    let mut hist_idx: Option<usize> = None;
    let mut stashed_live: Option<Vec<char>> = None;

    // First paint lays down two fresh lines; every subsequent paint moves
    // back up to the rule line and overwrites in place so the bar stays
    // anchored to its starting position instead of marching down.
    let mut painted = false;
    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
    painted = true;

    loop {
        let ev = event::read().map_err(|e| anyhow::anyhow!("event::read: {e}"))?;
        match ev {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (code, modifiers) {
                (KeyCode::Enter, _) => {
                    // Erase both bar lines so the caller's printlns flow
                    // naturally where the bar used to be — no stale rule,
                    // no doubled-up prompt text.
                    clear_bar(&mut stdout)?;
                    let line: String = buffer.into_iter().collect();
                    return Ok(LineResult::Submit(line));
                }
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    clear_bar(&mut stdout)?;
                    return Ok(LineResult::Interrupted);
                }
                // Ctrl+D on an empty buffer → EOF. In the middle of a line
                // it's a no-op (matches most readline implementations).
                (KeyCode::Char('d'), KeyModifiers::CONTROL) if buffer.is_empty() => {
                    clear_bar(&mut stdout)?;
                    return Ok(LineResult::Eof);
                }
                // Shift+Tab → toggle Normal ↔ Plan. crossterm surfaces it
                // as BackTab regardless of whether Shift is in modifiers
                // (terminfo handling varies by terminal).
                (KeyCode::BackTab, _) => {
                    clear_bar(&mut stdout)?;
                    return Ok(LineResult::ToggleMode);
                }
                (KeyCode::Backspace, _) if cursor_pos > 0 => {
                    buffer.remove(cursor_pos - 1);
                    cursor_pos -= 1;
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Delete, _) if cursor_pos < buffer.len() => {
                    buffer.remove(cursor_pos);
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Left, _) if cursor_pos > 0 => {
                    cursor_pos -= 1;
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Right, _) if cursor_pos < buffer.len() => {
                    cursor_pos += 1;
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Home, _) => {
                    cursor_pos = 0;
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::End, _) => {
                    cursor_pos = buffer.len();
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    // Any edit to the buffer ends history navigation —
                    // we're back to a live line.
                    if hist_idx.is_some() {
                        hist_idx = None;
                        stashed_live = None;
                    }
                    buffer.insert(cursor_pos, c);
                    cursor_pos += 1;
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Up, _) => {
                    if history.is_empty() {
                        continue;
                    }
                    let next = hist_idx.map_or(0, |i| (i + 1).min(history.len() - 1));
                    if hist_idx.is_none() {
                        stashed_live = Some(std::mem::take(&mut buffer));
                    }
                    hist_idx = Some(next);
                    let recalled = history
                        .get(history.len() - 1 - next)
                        .cloned()
                        .unwrap_or_default();
                    buffer = recalled.chars().collect();
                    cursor_pos = buffer.len();
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                (KeyCode::Down, _) => {
                    match hist_idx {
                        None => continue,
                        Some(0) => {
                            // Back to live buffer.
                            buffer = stashed_live.take().unwrap_or_default();
                            cursor_pos = buffer.len();
                            hist_idx = None;
                        }
                        Some(i) => {
                            let next = i - 1;
                            hist_idx = Some(next);
                            let recalled = history
                                .get(history.len() - 1 - next)
                                .cloned()
                                .unwrap_or_default();
                            buffer = recalled.chars().collect();
                            cursor_pos = buffer.len();
                        }
                    }
                    repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                }
                // Old TODO retired: history nav is wired above.
                _ => {}
            },
            Event::Resize(_, _) => {
                repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
            }
            _ => {}
        }
    }
}

/// Paint the two-line input bar (separator + prompt) in place.
///
/// Layout:
/// ```text
/// ──────────── (dim, terminal-width)
/// ❯ <buffer>
/// ```
///
/// On the first paint of a `read_line` call we paint where the cursor
/// already is (typically column 0 of a fresh line after the banner or
/// prior agent output). On every later paint we step up one line so the
/// rule lands back on its original row — otherwise each keystroke would
/// shove the bar one line further down.
fn repaint(
    stdout: &mut io::Stdout,
    buffer: &[char],
    cursor_pos: usize,
    mode: Mode,
    painted_before: bool,
) -> Result<()> {
    let cols = terminal::size()
        .ok()
        .map(|(c, _)| c as usize)
        .filter(|c| *c > 0)
        .unwrap_or(80);
    let rule: String = rule_chip(cols);
    let text: String = buffer.iter().collect();

    // Mode chip printed in-line with the prompt, left of ❯. Dimmed so
    // it's a status cue, not a shout.
    let (chip_text, chip_colour) = match mode {
        Mode::Normal => ("", Color::DarkGrey),
        Mode::Plan => ("[plan] ", Color::Yellow),
    };

    if painted_before {
        // Jump back to the rule line so we overwrite in place.
        queue!(stdout, cursor::MoveToPreviousLine(1))?;
    }

    queue!(
        stdout,
        Print("\r"),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::DarkGrey),
        SetAttribute(Attribute::Dim),
        Print(&rule),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(chip_colour),
        SetAttribute(Attribute::Dim),
        Print(chip_text),
        ResetColor,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print("\u{276f} "),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Print(&text),
    )?;

    let prefix_cols = chip_text.chars().count() as u16 + 2;
    let col = prefix_cols + (cursor_pos as u16).min(u16::MAX - prefix_cols);
    queue!(stdout, cursor::MoveToColumn(col))?;

    stdout.flush()?;
    Ok(())
}

/// Wipe the two-line bar and leave the cursor at column 0 of where the
/// rule used to be, so the caller's subsequent `println!`s flow naturally
/// from that point.
fn clear_bar(stdout: &mut io::Stdout) -> Result<()> {
    queue!(
        stdout,
        Print("\r"),
        Clear(ClearType::CurrentLine),
        cursor::MoveToPreviousLine(1),
        Clear(ClearType::CurrentLine),
    )?;
    stdout.flush()?;
    Ok(())
}
