//! Top-level App: state machine, event loop, and channel bridge
//! between the ratatui main thread and the asupersync background
//! runtime that drives `pi::AgentSessionHandle`.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::commands::code_approvals::{ApprovalUi, PromptChoice};
use crate::commands::code_factory::{Mode, ModeFlag};
use crate::commands::code_team::AgentRegistry;
use crate::commands::code_tui::theme;
use crate::commands::code_tui::view;
use crate::config::Config as LibertaiConfig;

/// Events sent from the background thread (pi session) to the main
/// thread (ratatui event loop).
#[derive(Debug, Clone)]
pub enum AgentMsg {
    /// Streaming text delta from the assistant.
    TextDelta(String),
    /// A tool started executing.
    ToolStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// A tool finished.
    ToolEnd {
        tool_call_id: String,
        tool_name: String,
        output: serde_json::Value,
    },
    /// The turn ended.
    TurnEnd {
        elapsed_secs: u64,
    },
    /// An approval is needed. The main thread shows a modal and
    /// sends the choice back via the oneshot channel.
    ApprovalRequest {
        tool_name: String,
        preview: String,
        always_rule: String,
        responder: std::sync::mpsc::Sender<PromptChoice>,
    },
    /// An ask_user is needed.
    AskRequest {
        payload: serde_json::Value,
        responder: std::sync::mpsc::Sender<crate::commands::code_approvals::AskOutcome>,
    },
    /// Usage update for the status bar.
    Usage {
        input_tokens: u64,
        context_window: u32,
        model_label: String,
    },
    /// Error from the background thread.
    Error(String),
}

/// Commands sent from the main thread to the background thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Submit a prompt to the pi session.
    Prompt(String),
    /// Abort the current turn.
    Abort,
    /// Queued message for the next turn.
    Queued(String),
}

/// The top-level App state.
pub struct App {
    /// Current phase of the REPL state machine.
    pub phase: Phase,
    /// Shared mode flag (Normal / AcceptEdits / Plan).
    pub mode: ModeFlag,
    /// The conversation transcript — each entry is a rendered line or
    /// block of text that the scrollback widget displays.
    pub transcript: Vec<TranscriptEntry>,
    /// Scroll position of the transcript (0 = bottom/latest).
    pub scroll: u16,
    /// Spinner frame index.
    pub spinner_idx: usize,
    /// When the current turn started (for elapsed display).
    pub turn_started: Option<Instant>,
    /// Output chars seen this turn (for token estimation).
    pub output_chars: u64,
    /// Spinner label ("thinking…", "writing…", etc.).
    pub spinner_label: &'static str,
    /// Messages queued for the next turn.
    pub queued: Vec<String>,
    /// Text being typed in the input bar.
    pub input_buffer: String,
    /// Input history (persisted).
    pub history: Vec<String>,
    /// History navigation index.
    pub history_idx: Option<usize>,
    /// Stashed live buffer when navigating history.
    pub stashed_live: Option<String>,
    /// Approval modal state (if active).
    pub approval: Option<ApprovalModal>,
    /// Live agent registry.
    pub registry: Arc<AgentRegistry>,
    /// Config.
    pub cfg: Arc<LibertaiConfig>,
    /// Status bar info.
    pub bar: BarStatus,
}

/// REPL phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Idle — input bar active, waiting for user.
    Idle,
    /// Streaming — agent is working, footer + spinner active.
    Streaming,
    /// Approval modal is showing.
    Approval,
}

/// A single entry in the conversation transcript.
#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    /// User prompt (bold `❯` prefix).
    User(String),
    /// Assistant text (bold `●` prefix, markdown).
    Assistant(String),
    /// Tool marker (cyan `●` prefix).
    Tool {
        name: String,
        detail: String,
    },
    /// Auto-allow notice (dim).
    AutoAllowed(String),
    /// System message (dim).
    System(String),
    /// Blank separator line.
    Blank,
}

/// Status bar info shown in the rule line.
#[derive(Debug, Clone, Default)]
pub struct BarStatus {
    pub model_label: String,
    pub input_tokens: u64,
    pub context_window: u32,
    pub estimated_cost: Option<f64>,
    pub status_line_template: String,
    pub status_line_command: String,
}

/// Active approval modal state.
pub struct ApprovalModal {
    pub tool_name: String,
    pub preview: String,
    pub always_rule: String,
    pub responder: mpsc::Sender<PromptChoice>,
}

/// Entry point — replaces `run_interactive` in `code_ui.rs`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    provider: String,
    model: String,
    initial_mode: Mode,
    _approvals: Arc<dyn ApprovalUi>,
    _resume_path: Option<std::path::PathBuf>,
    _bash_command_wrapper: Option<Vec<String>>,
    cfg: Arc<LibertaiConfig>,
    registry: Arc<AgentRegistry>,
) -> anyhow::Result<()> {
    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mode = ModeFlag::new(initial_mode);

    // Channels: bg -> main (AgentMsg), main -> bg (Cmd).
    let (_agent_tx, agent_rx) = mpsc::channel::<AgentMsg>();
    let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

    // Build initial app state.
    let app = App {
        phase: Phase::Idle,
        mode,
        transcript: Vec::new(),
        scroll: 0,
        spinner_idx: 0,
        turn_started: None,
        output_chars: 0,
        spinner_label: "thinking…",
        queued: Vec::new(),
        input_buffer: String::new(),
        history: Vec::new(),
        history_idx: None,
        stashed_live: None,
        approval: None,
        registry,
        cfg,
        bar: BarStatus {
            model_label: format!("{provider}/{model}"),
            ..Default::default()
        },
    };

    // Run the event loop.
    let result = run_loop(&mut terminal, app, agent_rx, cmd_tx);

    // Restore terminal.
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Main event loop — polls crossterm events + agent messages,
/// updates app state, and draws.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut app: App,
    agent_rx: mpsc::Receiver<AgentMsg>,
    cmd_tx: mpsc::Sender<Cmd>,
) -> anyhow::Result<()> {
    let tick = Duration::from_millis(theme::TICK_RATE_MS);

    loop {
        // Draw.
        terminal.draw(|frame| view::draw(frame, &mut app))?;

        // Poll for events (keyboard, mouse, resize) with timeout.
        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(action) = handle_key(&mut app, key, &cmd_tx) {
                        match action {
                            Action::Quit => break,
                            Action::Submit(prompt) => {
                                cmd_tx.send(Cmd::Prompt(prompt))?;
                                app.phase = Phase::Streaming;
                                app.turn_started = Some(Instant::now());
                                app.output_chars = 0;
                                app.spinner_label = "thinking…";
                            }
                        }
                    }
                }
                Event::Resize(_, _) => {
                    // ratatui handles resize automatically.
                }
                _ => {}
            }
        }

        // Drain agent messages (non-blocking).
        while let Ok(msg) = agent_rx.try_recv() {
            handle_agent_msg(&mut app, msg);
        }

        // Animate spinner.
        if app.phase == Phase::Streaming {
            app.spinner_idx = (app.spinner_idx + 1) % theme::SPINNER_FRAMES.len();
        }
    }

    Ok(())
}

/// Key handling action.
enum Action {
    Quit,
    Submit(String),
}

/// Handle a keyboard event. Returns `Some(Action)` if the loop should
/// do something (quit, submit), `None` otherwise.
fn handle_key(app: &mut App, key: KeyEvent, _cmd_tx: &mpsc::Sender<Cmd>) -> Option<Action> {
    // If approval modal is active, keys go to it.
    if app.approval.is_some() {
        return handle_approval_key(app, key);
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.phase == Phase::Streaming {
                _cmd_tx.send(Cmd::Abort).ok();
                app.phase = Phase::Idle;
                None
            } else {
                Some(Action::Quit)
            }
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) if app.input_buffer.is_empty() => {
            Some(Action::Quit)
        }
        (KeyCode::Enter, _) if app.phase == Phase::Idle => {
            let prompt = std::mem::take(&mut app.input_buffer);
            if prompt.is_empty() && !app.queued.is_empty() {
                // Drain queue.
                None
            } else if !prompt.is_empty() {
                // Add to history.
                app.history.push(prompt.clone());
                app.history_idx = None;
                app.stashed_live = None;
                Some(Action::Submit(prompt))
            } else {
                None
            }
        }
        (KeyCode::Char(c), _) if app.phase == Phase::Idle => {
            app.input_buffer.push(c);
            None
        }
        (KeyCode::Backspace, _) if app.phase == Phase::Idle => {
            app.input_buffer.pop();
            None
        }
        _ => None,
    }
}

/// Handle a key when the approval modal is active.
fn handle_approval_key(app: &mut App, key: KeyEvent) -> Option<Action> {
    let approval = app.approval.take()?;
    use crate::commands::code_approvals::PromptChoice;
    let choice = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => PromptChoice::Allow,
        KeyCode::Char('a') | KeyCode::Char('A') => PromptChoice::AlwaysAllow,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
            PromptChoice::Deny
        }
        _ => {
            // Put it back — didn't handle.
            app.approval = Some(approval);
            return None;
        }
    };
    let _ = approval.responder.send(choice);
    None
}

/// Handle an agent message from the background thread.
fn handle_agent_msg(app: &mut App, msg: AgentMsg) {
    match msg {
        AgentMsg::TextDelta(delta) => {
            app.output_chars += delta.len() as u64;
            // Append to the last assistant entry, or create a new one.
            if let Some(TranscriptEntry::Assistant(text)) = app.transcript.last_mut() {
                text.push_str(&delta);
            } else {
                app.transcript.push(TranscriptEntry::Assistant(delta));
            }
        }
        AgentMsg::ToolStart {
            tool_name,
            args,
            ..
        } => {
            let detail = crate::commands::code_tool_preview::tool_preview(&tool_name, &args);
            let detail = detail
                .strip_prefix(&tool_name)
                .map(str::trim_start)
                .unwrap_or("")
                .to_string();
            app.transcript.push(TranscriptEntry::Tool {
                name: tool_name,
                detail,
            });
            app.spinner_label = "working…";
        }
        AgentMsg::ToolEnd { .. } => {
            app.spinner_label = "thinking…";
        }
        AgentMsg::TurnEnd { elapsed_secs: _ } => {
            app.phase = Phase::Idle;
            app.turn_started = None;
        }
        AgentMsg::ApprovalRequest {
            tool_name,
            preview,
            always_rule,
            responder,
        } => {
            app.approval = Some(ApprovalModal {
                tool_name,
                preview,
                always_rule,
                responder,
            });
            app.phase = Phase::Approval;
        }
        AgentMsg::AskRequest { .. } => {
            // TODO: show ask_user modal
        }
        AgentMsg::Usage {
            input_tokens,
            context_window,
            model_label,
        } => {
            app.bar.input_tokens = input_tokens;
            app.bar.context_window = context_window;
            app.bar.model_label = model_label;
        }
        AgentMsg::Error(e) => {
            app.transcript.push(TranscriptEntry::System(format!("error: {e}")));
        }
    }
}

