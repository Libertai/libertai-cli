//! Top-level App: state machine, event loop, and channel bridge
//! between the ratatui main thread and the asupersync background
//! runtime that drives `pi::AgentSessionHandle`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use pi::model::AssistantMessageEvent;
use pi::sdk::{create_agent_session, AbortHandle, AgentEvent, AgentSessionHandle};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::commands::code_approvals::{ApprovalState, ApprovalUi, PromptChoice};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_hooks::tool_policy_from_config;
use crate::commands::code_identity_prompt;
use crate::commands::code_mode_prompt;
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, DEFAULT_MAX_TOKENS, SessionPersistence,
};
use crate::commands::code_skills::{prompt_for_pillar, SkillPillar};
use crate::commands::code_team::AgentRegistry;
use crate::commands::code_tui::approvals::RatatuiApprovalUi;
use crate::commands::code_tui::theme;
use crate::commands::code_tui::view;
use crate::config::{allow_rules_path, Config as LibertaiConfig};

/// Maximum entries in the input history. Matches the legacy REPL.
const HISTORY_MAX_LIMIT: usize = 64;

/// Shared abort handle — the main thread calls `.abort()` on Ctrl+C
/// to interrupt the background thread's current turn.
type SharedAbort = Arc<Mutex<Option<AbortHandle>>>;

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
    /// System notice (compaction, retry, etc.) — dim in transcript.
    System(String),
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
    /// Name of the tool currently executing in the main session, if any.
    /// Updated from `AgentMsg::ToolStart`/`ToolEnd`.
    pub current_tool: Option<String>,
    /// Messages queued for the next turn.
    pub queued: Vec<String>,
    /// Text being typed in the input bar.
    pub input_buffer: String,
    /// Input history (capped at [`HISTORY_MAX_LIMIT`]).
    pub history: VecDeque<String>,
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
}

/// Active approval modal state.
pub struct ApprovalModal {
    pub tool_name: String,
    pub preview: String,
    pub always_rule: String,
    pub responder: mpsc::Sender<PromptChoice>,
}

/// RAII guard that restores the terminal on drop — covers early-return
/// and panic paths between `enable_raw_mode` and the end of `run_loop`.
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
// Background thread: asupersync runtime + pi session
// ---------------------------------------------------------------------------

/// Build a pi `AgentSessionHandle` wired with `RatatuiApprovalUi`.
///
/// Mirrors `code_ui::build_handle` but uses the ratatui approval UI
/// instead of `TerminalApprovalUi`.
#[allow(clippy::too_many_arguments)]
async fn build_session(
    provider: &str,
    model: &str,
    mode: ModeFlag,
    cfg: Arc<LibertaiConfig>,
    registry: Arc<AgentRegistry>,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    agent_tx: &mpsc::Sender<AgentMsg>,
) -> anyhow::Result<AgentSessionHandle> {
    let initial_mode = mode.get();
    let approvals = Arc::new(ApprovalState::with_persistent_store(allow_rules_path()?)?);
    let ui: Arc<dyn ApprovalUi> = Arc::new(RatatuiApprovalUi::new(agent_tx.clone()));
    let factory = Arc::new(
        LibertaiToolFactory::new_with_features(
            mode,
            approvals,
            ui,
            FactoryFeatures::cli_defaults(),
            Some(Arc::clone(&cfg)),
        )
        .with_tool_policy(tool_policy_from_config(Arc::clone(&cfg)))
        .with_registry(registry),
    );
    let persistence = match resume_path {
        Some(p) => SessionPersistence::Resume(p),
        None => SessionPersistence::Fresh,
    };
    let max_tokens = Some(DEFAULT_MAX_TOKENS);
    let skill_cwd = std::env::current_dir().ok();
    let append_system_prompt = prompt_for_pillar(SkillPillar::Code, skill_cwd.as_deref())?;
    let append_system_prompt = code_mode_prompt::apply(append_system_prompt, initial_mode);
    let append_system_prompt = code_identity_prompt::apply(append_system_prompt);
    let options = build_session_options(CodeSessionConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        working_directory: None,
        include_cwd_in_prompt: true,
        max_tool_iterations: 50,
        tool_factory: factory,
        persistence,
        enabled_tools: None,
        append_system_prompt,
        max_tokens,
        bash_command_wrapper,
        auto_compaction_enabled: cfg.code_auto_compaction_enabled,
        compaction_reserve_tokens: cfg.code_compaction_reserve_tokens,
        compaction_keep_recent_tokens: cfg.code_compaction_keep_recent_tokens,
    });
    let mut handle = create_agent_session(options)
        .await
        .map_err(|e| anyhow::Error::new(e).context("create_agent_session"))?;
    handle.set_max_tokens(max_tokens);
    Ok(handle)
}

/// Translate a pi `AgentEvent` into an `AgentMsg` for the main thread.
///
/// Most variants are swallowed (lifecycle noise the TUI doesn't need).
/// The ones that matter: text deltas, tool start/end, compaction/retry
/// status, and errors.
fn translate_event(event: &AgentEvent) -> Option<AgentMsg> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            if delta.is_empty() {
                None
            } else {
                Some(AgentMsg::TextDelta(delta.clone()))
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(AgentMsg::ToolStart {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            args: args.clone(),
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            ..
        } => Some(AgentMsg::ToolEnd {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            output: serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        }),
        AgentEvent::AutoCompactionStart { reason } => {
            Some(AgentMsg::System(format!("compacting: {reason}")))
        }
        AgentEvent::AutoCompactionEnd {
            aborted,
            error_message,
            ..
        } => {
            let text = if *aborted {
                "compaction aborted".to_string()
            } else if let Some(err) = error_message {
                format!("compaction error: {err}")
            } else {
                "compaction complete".to_string()
            };
            Some(AgentMsg::System(text))
        }
        AgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            error_message,
            ..
        } => Some(AgentMsg::System(format!(
            "retry {attempt}/{max_attempts}: {error_message}"
        ))),
        AgentEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => {
            if *success {
                Some(AgentMsg::System(format!("retry {attempt} succeeded")))
            } else {
                final_error
                    .as_ref()
                    .map(|err| AgentMsg::System(format!("retry {attempt} failed: {err}")))
            }
        }
        AgentEvent::ExtensionError { error, .. } => Some(AgentMsg::Error(error.clone())),
        _ => None,
    }
}

/// Spawn the background thread that owns the asupersync runtime and
/// the pi `AgentSessionHandle`.
///
/// The thread loops on `cmd_rx.recv()`, calling
/// `handle.prompt_with_abort(...)` for each `Cmd::Prompt`. AgentEvents
/// are translated to `AgentMsg`s and sent via `agent_tx`. Ctrl+C aborts
/// are handled via `shared_abort` (the main thread calls `.abort()`
/// directly — no channel round-trip needed).
#[allow(clippy::too_many_arguments)]
fn spawn_background(
    agent_tx: mpsc::Sender<AgentMsg>,
    cmd_rx: mpsc::Receiver<Cmd>,
    shared_abort: SharedAbort,
    provider: String,
    model: String,
    mode: ModeFlag,
    cfg: Arc<LibertaiConfig>,
    registry: Arc<AgentRegistry>,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let reactor = match asupersync::runtime::reactor::create_reactor() {
            Ok(r) => r,
            Err(e) => {
                let _ = agent_tx.send(AgentMsg::Error(format!("asupersync reactor: {e}")));
                return;
            }
        };
        let runtime = match asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                let _ = agent_tx.send(AgentMsg::Error(format!("asupersync runtime: {e}")));
                return;
            }
        };

        runtime.block_on(async move {
            let mut handle = match build_session(
                &provider,
                &model,
                mode,
                Arc::clone(&cfg),
                Arc::clone(&registry),
                resume_path,
                bash_command_wrapper,
                &agent_tx,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    let _ = agent_tx.send(AgentMsg::Error(format!("{e:#}")));
                    return;
                }
            };

            loop {
                match cmd_rx.recv() {
                    Ok(Cmd::Prompt(prompt)) => {
                        let (abort_handle, abort_signal) = AbortHandle::new();
                        *shared_abort.lock().unwrap() = Some(abort_handle);

                        let tx = agent_tx.clone();
                        let start = Instant::now();
                        let result = handle
                            .prompt_with_abort(
                                prompt,
                                abort_signal,
                                move |event: AgentEvent| {
                                    if let Some(msg) = translate_event(&event) {
                                        let _ = tx.send(msg);
                                    }
                                },
                            )
                            .await;

                        *shared_abort.lock().unwrap() = None;
                        let elapsed = start.elapsed().as_secs();

                        match result {
                            Ok(msg) => {
                                let _ = agent_tx.send(AgentMsg::Usage {
                                    input_tokens: msg.usage.input,
                                    context_window: 0,
                                    model_label: format!("{}/{}", msg.provider, msg.model),
                                });
                                let _ =
                                    agent_tx.send(AgentMsg::TurnEnd { elapsed_secs: elapsed });
                            }
                            Err(e) => {
                                let _ = agent_tx.send(AgentMsg::Error(format!("{e}")));
                                let _ =
                                    agent_tx.send(AgentMsg::TurnEnd { elapsed_secs: elapsed });
                            }
                        }
                    }
                    Ok(Cmd::Queued(_)) => {
                        // TODO: queued messages
                    }
                    Ok(Cmd::Abort) => {
                        // Handled via shared_abort directly from the main thread.
                    }
                    Err(_) => break, // channel closed — main thread exited
                }
            }
        });
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Entry point — replaces `run_interactive` in `code_ui.rs`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    provider: String,
    model: String,
    initial_mode: Mode,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    cfg: Arc<LibertaiConfig>,
    registry: Arc<AgentRegistry>,
) -> anyhow::Result<()> {
    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;

    // RAII guard — if anything below fails, the terminal is restored.
    let mut guard = TerminalGuard {
        terminal: Some(terminal),
    };
    let terminal = guard.terminal.as_mut().unwrap();

    let mode = ModeFlag::new(initial_mode);

    // Channels: bg -> main (AgentMsg), main -> bg (Cmd).
    let (agent_tx, agent_rx) = mpsc::channel::<AgentMsg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

    // Shared abort handle for Ctrl+C to interrupt the current turn.
    let shared_abort: SharedAbort = Arc::new(Mutex::new(None));

    // Spawn the background thread (asupersync runtime + pi session).
    let _bg_thread = spawn_background(
        agent_tx,
        cmd_rx,
        Arc::clone(&shared_abort),
        provider.clone(),
        model.clone(),
        mode.clone(),
        Arc::clone(&cfg),
        Arc::clone(&registry),
        resume_path,
        bash_command_wrapper,
    );

    // Build initial app state.
    let mut app = App {
        phase: Phase::Idle,
        mode,
        transcript: Vec::new(),
        scroll: 0,
        spinner_idx: 0,
        turn_started: None,
        output_chars: 0,
        spinner_label: "thinking…",
        current_tool: None,
        queued: Vec::new(),
        input_buffer: String::new(),
        history: VecDeque::new(),
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
    let result = run_loop(terminal, &mut app, agent_rx, cmd_tx, &shared_abort);

    // Restore terminal (also done by guard on drop, but do it explicitly
    // on the success path so `result` is returned after cleanup).
    drop(guard);
    result
}

/// Main event loop — polls crossterm events + agent messages,
/// updates app state, and draws.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    agent_rx: mpsc::Receiver<AgentMsg>,
    cmd_tx: mpsc::Sender<Cmd>,
    shared_abort: &SharedAbort,
) -> anyhow::Result<()> {
    let tick = Duration::from_millis(theme::TICK_RATE_MS);

    loop {
        // Draw.
        terminal.draw(|frame| view::draw(frame, app))?;

        // Poll for events (keyboard, mouse, resize) with timeout.
        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(action) = handle_key(app, key, &cmd_tx, shared_abort) {
                        match action {
                            Action::Quit => break,
                            Action::Submit(prompt) => {
                                // Echo the user prompt into the transcript.
                                app.transcript.push(TranscriptEntry::User(prompt.clone()));
                                app.transcript.push(TranscriptEntry::Blank);
                                let _ = cmd_tx.send(Cmd::Prompt(prompt));
                                app.phase = Phase::Streaming;
                                app.turn_started = Some(Instant::now());
                                app.output_chars = 0;
                                app.current_tool = None;
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
            handle_agent_msg(app, msg);
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
fn handle_key(
    app: &mut App,
    key: KeyEvent,
    _cmd_tx: &mpsc::Sender<Cmd>,
    shared_abort: &SharedAbort,
) -> Option<Action> {
    // If approval modal is active, keys go to it.
    if app.approval.is_some() {
        return handle_approval_key(app, key);
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.phase == Phase::Streaming {
                // Abort the current turn directly via the shared abort handle.
                if let Some(abort) = shared_abort.lock().unwrap().take() {
                    abort.abort();
                }
                app.phase = Phase::Idle;
                None
            } else {
                Some(Action::Quit)
            }
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL)
            if app.phase == Phase::Idle && app.input_buffer.is_empty() =>
        {
            Some(Action::Quit)
        }
        (KeyCode::Enter, _) if app.phase == Phase::Idle => {
            let prompt = std::mem::take(&mut app.input_buffer);
            if prompt.is_empty() && !app.queued.is_empty() {
                // TODO: drain queue and submit first entry.
                None
            } else if !prompt.is_empty() {
                // Add to history with dedup + cap.
                if app.history.back().is_none_or(|last| last != &prompt) {
                    app.history.push_back(prompt.clone());
                    if app.history.len() > HISTORY_MAX_LIMIT {
                        app.history.pop_front();
                    }
                }
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
    // The turn resumes while the background thread processes the choice.
    app.phase = Phase::Streaming;
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
                name: tool_name.clone(),
                detail,
            });
            app.current_tool = Some(tool_name);
            app.spinner_label = "working…";
        }
        AgentMsg::ToolEnd { .. } => {
            app.current_tool = None;
            app.spinner_label = "thinking…";
        }
        AgentMsg::TurnEnd { elapsed_secs: _ } => {
            app.phase = Phase::Idle;
            app.turn_started = None;
            app.current_tool = None;
            app.transcript.push(TranscriptEntry::Blank);
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
        AgentMsg::System(text) => {
            app.transcript.push(TranscriptEntry::System(text));
        }
        AgentMsg::Error(e) => {
            app.transcript.push(TranscriptEntry::System(format!("error: {e}")));
        }
    }
}

