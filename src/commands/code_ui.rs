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
use std::sync::Arc;

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

use pi::model::AssistantMessageEvent;
use pi::sdk::{create_agent_session, AgentEvent, AgentSessionHandle, SessionOptions};

use crate::commands::code_approvals::ApprovalState;
use crate::commands::code_factory::{LibertaiToolFactory, Mode};

/// ANSI dim/bold helpers for cooked output (agent streaming phase).
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

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
        let line = match read_line()? {
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

        // Hand off to pi. The callback prints plain text; we're in cooked
        // mode here so \n and flush behave as expected.
        let result = handle.prompt(line, render_event).await;

        // Always end on a newline regardless of the last event kind.
        println!();

        match result {
            Ok(msg) => {
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
                eprintln!("{DIM}  error: {e}{RESET}");
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
    println!("{DIM}  /forget   — clear the session \"always allow\" list{RESET}");
    println!("{DIM}  arrows    — move cursor in the current line{RESET}");
    println!("{DIM}  Ctrl+C    — cancel the line you're typing{RESET}");
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
fn read_line() -> Result<LineResult> {
    let _guard = RawModeGuard::enter()?;

    let mut stdout = io::stdout();
    execute!(stdout, cursor::Show)?;

    let mut buffer: Vec<char> = Vec::new();
    let mut cursor_pos: usize = 0; // index within `buffer`
    // First paint lays down two fresh lines; every subsequent paint moves
    // back up to the rule line and overwrites in place so the bar stays
    // anchored to its starting position instead of marching down.
    let mut painted = false;
    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
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
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                (KeyCode::Delete, _) if cursor_pos < buffer.len() => {
                    buffer.remove(cursor_pos);
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                (KeyCode::Left, _) if cursor_pos > 0 => {
                    cursor_pos -= 1;
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                (KeyCode::Right, _) if cursor_pos < buffer.len() => {
                    cursor_pos += 1;
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                (KeyCode::Home, _) => {
                    cursor_pos = 0;
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                (KeyCode::End, _) => {
                    cursor_pos = buffer.len();
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    buffer.insert(cursor_pos, c);
                    cursor_pos += 1;
                    repaint(&mut stdout, &buffer, cursor_pos, painted)?;
                }
                // Up/Down history is a v0 nice-to-have; skipped for now so
                // we don't have to thread history state into the editor.
                _ => {}
            },
            Event::Resize(_, _) => {
                repaint(&mut stdout, &buffer, cursor_pos, painted)?;
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
    painted_before: bool,
) -> Result<()> {
    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    let rule: String = "\u{2500}".repeat(cols.max(1));
    let text: String = buffer.iter().collect();

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
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print("\u{276f} "),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Print(&text),
    )?;

    let col = 2u16 + (cursor_pos as u16).min(u16::MAX - 2);
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
