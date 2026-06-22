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
use ratatui::style::{Color, Style};
use ratatui::Terminal;
use tui_textarea::TextArea;

use crate::commands::code_approvals::{ApprovalState, ApprovalUi, PromptChoice};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_hooks::{tool_policy_from_config, run_post_tool_hooks, run_stop_hooks, run_user_prompt_submit_hooks, SessionHookGuard};
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
    /// Result from a slash command executed on the background thread.
    CommandResult(String),
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
    /// Set the model (provider, model_id).
    SetModel(String, String),
    /// Clear the session and start fresh.
    Clear,
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
    /// Detail string for the current tool (e.g. "bash(npm run build)").
    pub current_tool_detail: String,
    /// Messages queued for the next turn.
    pub queued: Vec<String>,
    /// Multi-line input editor (tui-textarea widget).
    pub textarea: TextArea<'static>,
    /// Input history (capped at [`HISTORY_MAX_LIMIT`]).
    pub history: VecDeque<String>,
    /// History navigation index.
    pub history_idx: Option<usize>,
    /// Stashed live buffer when navigating history.
    pub stashed_live: Option<String>,
    /// Approval modal state (if active).
    pub approval: Option<ApprovalModal>,
    /// Ask-user modal state (if active).
    pub ask: Option<AskModal>,
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
    /// Ask-user modal is showing.
    Ask,
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

/// A single question in an ask_user flow.
#[derive(Debug, Clone)]
pub struct AskQuestion {
    pub header: String,
    pub question: String,
    pub multi_select: bool,
    pub options: Vec<AskOption>,
}

/// A single option in an ask_user question.
#[derive(Debug, Clone)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
}

/// Active ask-user modal state.
pub struct AskModal {
    /// Parsed questions from the tool payload.
    pub questions: Vec<AskQuestion>,
    /// Current question index.
    pub current: usize,
    /// List selection state for options.
    pub list_state: ratatui::widgets::ListState,
    /// Multi-select toggles (indices of selected options).
    pub selected: Vec<usize>,
    /// Free-text input (for "Other" or no-options questions).
    pub free_text: String,
    /// Whether we're in free-text mode (no options or "Other" selected).
    pub free_text_mode: bool,
    /// Collected answers so far.
    pub answers: Vec<serde_json::Value>,
    /// Channel to send the result back.
    pub responder: mpsc::Sender<crate::commands::code_approvals::AskOutcome>,
}

impl AskModal {
    /// Parse the ask_user tool payload into an `AskModal`.
    pub fn from_payload(
        payload: &serde_json::Value,
        responder: mpsc::Sender<crate::commands::code_approvals::AskOutcome>,
    ) -> Option<Self> {
        let questions = payload.get("questions")?.as_array()?;
        if questions.is_empty() {
            return None;
        }

        let parsed: Vec<AskQuestion> = questions
            .iter()
            .map(|q| AskQuestion {
                header: q
                    .get("header")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                question: q
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                multi_select: q
                    .get("multiSelect")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|o| {
                                let label = o
                                    .get("label")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if label.is_empty() {
                                    return None;
                                }
                                let description = o
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.trim().is_empty())
                                    .map(String::from);
                                Some(AskOption { label, description })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();

        if parsed.is_empty() {
            return None;
        }

        let first_has_options = !parsed[0].options.is_empty();
        let mut list_state = ratatui::widgets::ListState::default();
        if first_has_options {
            list_state.select(Some(0));
        }

        Some(Self {
            questions: parsed,
            current: 0,
            list_state,
            selected: Vec::new(),
            free_text: String::new(),
            free_text_mode: !first_has_options,
            answers: Vec::new(),
            responder,
        })
    }

    /// Current question.
    pub fn current_question(&self) -> &AskQuestion {
        &self.questions[self.current]
    }
}

/// RAII guard that restores the terminal on drop — covers early-return
/// and panic paths between `enable_raw_mode` and the end of `run_loop`.
///
/// Tracks which terminal modifications have been applied so far so
/// that if `enable_raw_mode` succeeds but `Terminal::new` fails, we
/// still undo raw mode and the alternate screen.
struct TerminalGuard {
    raw_mode: bool,
    alt_screen: bool,
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = terminal.show_cursor();
            let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
        } else if self.alt_screen {
            let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
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
                mode.clone(),
                Arc::clone(&cfg),
                Arc::clone(&registry),
                resume_path,
                bash_command_wrapper.clone(),
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

            // Session start hooks (SessionEnd fires on drop).
            let _session_hooks = SessionHookGuard::start(Arc::clone(&cfg));

            // Track the current provider/model so /clear doesn't revert
            // to the original model after /model has changed it.
            let mut current_provider = provider.clone();
            let mut current_model = model.clone();
            let hook_cfg = Arc::clone(&cfg);

            loop {
                match cmd_rx.recv() {
                    Ok(Cmd::Prompt(prompt)) => {
                        let (abort_handle, abort_signal) = AbortHandle::new();
                        *shared_abort.lock().unwrap() = Some(abort_handle);

                        // Apply turn guidance + user-prompt-submit hooks.
                        let prompt = code_mode_prompt::apply_turn_guidance(
                            prompt,
                            mode.get(),
                        );
                        let prompt = match run_user_prompt_submit_hooks(
                            cfg.as_ref(),
                            &prompt,
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = agent_tx.send(AgentMsg::Error(format!("{e:#}")));
                                let _ = agent_tx.send(AgentMsg::TurnEnd {
                                    elapsed_secs: 0,
                                });
                                *shared_abort.lock().unwrap() = None;
                                continue;
                            }
                        };

                        let tx = agent_tx.clone();
                        let hook_cfg = Arc::clone(&hook_cfg);
                        let start = Instant::now();
                        let result = handle
                            .prompt_with_abort(
                                prompt,
                                abort_signal,
                                move |event: AgentEvent| {
                                    run_post_tool_hooks(hook_cfg.as_ref(), &event);
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
                                run_stop_hooks(cfg.as_ref());
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
                    Ok(Cmd::SetModel(provider, model_id)) => {
                        current_provider = provider.clone();
                        current_model = model_id.clone();
                        match handle.set_model(&provider, &model_id).await {
                            Ok(()) => {
                                let _ = agent_tx.send(AgentMsg::CommandResult(
                                    format!("→ model set to {provider}/{model_id}"),
                                ));
                            }
                            Err(e) => {
                                let _ = agent_tx.send(AgentMsg::Error(format!("{e}")));
                            }
                        }
                    }
                    Ok(Cmd::Clear) => {
                        match build_session(
                            &current_provider,
                            &current_model,
                            mode.clone(),
                            Arc::clone(&cfg),
                            Arc::clone(&registry),
                            None, // fresh session
                            bash_command_wrapper.clone(),
                            &agent_tx,
                        )
                        .await
                        {
                            Ok(new_handle) => {
                                handle = new_handle;
                                let _ = agent_tx.send(AgentMsg::CommandResult(
                                    "→ fresh session.".to_string(),
                                ));
                            }
                            Err(e) => {
                                let _ = agent_tx.send(AgentMsg::Error(format!("{e:#}")));
                            }
                        }
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
    // Set up terminal — guard created first so any early-return
    // between enable_raw_mode and the end of run_loop is cleaned up.
    let mut guard = TerminalGuard {
        raw_mode: false,
        alt_screen: false,
        terminal: None,
    };

    enable_raw_mode()?;
    guard.raw_mode = true;

    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    guard.alt_screen = true;

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    guard.terminal = Some(terminal);

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
        current_tool_detail: String::new(),
        queued: Vec::new(),
        textarea: {
            let mut ta = TextArea::default();
            ta.set_cursor_style(Style::default().bg(Color::Cyan));
            ta.set_cursor_line_style(Style::default());
            ta.set_placeholder_text("type your message…");
            ta.set_placeholder_style(Style::default().fg(Color::DarkGray));
            ta
        },
        history: VecDeque::new(),
        history_idx: None,
        stashed_live: None,
            approval: None,
            ask: None,
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
                                app.current_tool_detail = String::new();
                                app.spinner_label = "thinking…";
                            }
                            Action::ClearTranscript => {
                                app.transcript.clear();
                                app.scroll = 0;
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
        loop {
            match agent_rx.try_recv() {
                Ok(msg) => handle_agent_msg(app, msg),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Background thread exited — show error and quit.
                    if app.phase == Phase::Streaming {
                        app.phase = Phase::Idle;
                        app.turn_started = None;
                    }
                    app.transcript
                        .push(TranscriptEntry::System(
                            "session ended — background thread exited.".to_string(),
                        ));
                    terminal.draw(|frame| view::draw(frame, app))?;
                    return Ok(());
                }
            }
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
    /// Clear the transcript (for /clear).
    ClearTranscript,
}

/// Handle a keyboard event. Returns `Some(Action)` if the loop should
/// do something (quit, submit), `None` otherwise.
fn handle_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::Sender<Cmd>,
    shared_abort: &SharedAbort,
) -> Option<Action> {
    // If approval modal is active, keys go to it.
    if app.approval.is_some() {
        return handle_approval_key(app, key, shared_abort);
    }

    // If ask-user modal is active, keys go to it.
    if app.ask.is_some() {
        return handle_ask_key(app, key);
    }

    // Scrollback navigation works in all phases.
    match key.code {
        KeyCode::PageUp => {
            app.scroll = app.scroll.saturating_add(10);
            return None;
        }
        KeyCode::PageDown => {
            app.scroll = app.scroll.saturating_sub(10);
            return None;
        }
        _ => {}
    }

    // Shift+Tab: cycle mode (Normal → AcceptEdits → Plan → Normal).
    if key.code == KeyCode::BackTab {
        let new_mode = match app.mode.get() {
            Mode::Normal => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
            Mode::Plan => Mode::Normal,
        };
        app.mode.set(new_mode);
        let label = match new_mode {
            Mode::Normal => "normal mode",
            Mode::AcceptEdits => "accept-edits mode",
            Mode::Plan => "plan mode",
        };
        app.transcript.push(TranscriptEntry::System(
            format!("→ {label}"),
        ));
        return None;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.phase == Phase::Streaming {
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
            if app.phase == Phase::Idle && app.textarea.is_empty() =>
        {
            Some(Action::Quit)
        }
        (KeyCode::Up, KeyModifiers::NONE) if app.phase == Phase::Idle => {
            // History navigation: go to previous entry.
            // Only intercept when textarea is single-line (cursor on
            // first line). On multi-line, let textarea handle Up.
            let (row, _) = app.textarea.cursor();
            if row > 0 {
                app.textarea.input(key);
                return None;
            }
            if app.history.is_empty() {
                return None;
            }
            if app.history_idx.is_none() {
                let current = app.textarea.lines().join("\n");
                if !current.is_empty() {
                    app.stashed_live = Some(current);
                }
                app.history_idx = Some(app.history.len() - 1);
            } else if let Some(idx) = app.history_idx {
                if idx > 0 {
                    app.history_idx = Some(idx - 1);
                }
            }
            if let Some(idx) = app.history_idx {
                set_textarea_text(&mut app.textarea, &app.history[idx]);
            }
            None
        }
        (KeyCode::Down, KeyModifiers::NONE) if app.phase == Phase::Idle => {
            // History navigation: go to next entry.
            // Only intercept when textarea is single-line (cursor on
            // last line). On multi-line, let textarea handle Down.
            let (row, _) = app.textarea.cursor();
            let last_row = app.textarea.lines().len().saturating_sub(1);
            if row < last_row {
                app.textarea.input(key);
                return None;
            }
            match (app.history_idx, &app.stashed_live) {
                (Some(idx), _) if idx + 1 < app.history.len() => {
                    app.history_idx = Some(idx + 1);
                    set_textarea_text(&mut app.textarea, &app.history[idx + 1]);
                }
                (Some(_), _) => {
                    app.history_idx = None;
                    let stashed = app.stashed_live.take().unwrap_or_default();
                    set_textarea_text(&mut app.textarea, &stashed);
                }
                (None, _) => {
                    app.textarea.input(key);
                }
            }
            None
        }
        (KeyCode::Enter, _) if app.phase == Phase::Idle => {
            // Submit on Enter (no modifier). Multi-line via Alt+Enter
            // or Shift+Enter (textarea handles those).
            if key.modifiers != KeyModifiers::NONE {
                app.textarea.input(key);
                return None;
            }
            let prompt = app.textarea.lines().join("\n");
            if prompt.is_empty() && !app.queued.is_empty() {
                None
            } else if !prompt.is_empty() {
                // Clear the textarea.
                app.textarea = TextArea::default();
                reset_textarea_style(&mut app.textarea);
                // Check for slash commands.
                if prompt.starts_with('/') {
                    handle_slash_command(app, &prompt, cmd_tx)
                } else {
                    if app.history.back().is_none_or(|last| last != &prompt) {
                        app.history.push_back(prompt.clone());
                        if app.history.len() > HISTORY_MAX_LIMIT {
                            app.history.pop_front();
                        }
                    }
                    app.history_idx = None;
                    app.stashed_live = None;
                    Some(Action::Submit(prompt))
                }
            } else {
                None
            }
        }
        _ if app.phase == Phase::Idle => {
            // Pass all other keys to the textarea.
            app.textarea.input(key);
            None
        }
        _ => None,
    }
}

/// Set the textarea content and reset cursor to end.
fn set_textarea_text(ta: &mut TextArea<'static>, text: &str) {
    let lines: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines().map(String::from).collect()
    };
    let last_row = lines.len().saturating_sub(1);
    let last_col = lines.last().map(|l| l.chars().count()).unwrap_or(0);
    ta.set_lines(lines, (last_row, last_col));
}

/// Reapply our styling to a fresh textarea.
fn reset_textarea_style(ta: &mut TextArea<'static>) {
    ta.set_cursor_style(Style::default().bg(Color::Cyan));
    ta.set_cursor_line_style(Style::default());
    ta.set_placeholder_text("type your message…");
    ta.set_placeholder_style(Style::default().fg(Color::DarkGray));
}

/// Handle a slash command. Returns `Some(Action)` for commands that
/// need the main loop to act (Quit, Submit), `None` for commands
/// handled entirely here.
fn handle_slash_command(app: &mut App, input: &str, cmd_tx: &mpsc::Sender<Cmd>) -> Option<Action> {
    let trimmed = input.trim();
    let (cmd, rest) = match trimmed.split_once(' ') {
        Some((c, r)) => (c, r.trim()),
        None => (trimmed, ""),
    };

    match cmd {
        "/exit" | "/quit" => {
            app.transcript.push(TranscriptEntry::System("goodbye.".to_string()));
            Some(Action::Quit)
        }
        "/help" => {
            app.transcript.push(TranscriptEntry::System(
                "Commands: /help /exit /clear /mode /model /status /history".to_string(),
            ));
            app.transcript.push(TranscriptEntry::Blank);
            None
        }
        "/clear" | "/new" => {
            let _ = cmd_tx.send(Cmd::Clear);
            Some(Action::ClearTranscript)
        }
        "/mode" | "/permissions" => {
            if rest.is_empty() || rest == "show" || rest == "status" {
                let mode = app.mode.get();
                let label = match mode {
                    Mode::Normal => "normal",
                    Mode::AcceptEdits => "accept-edits",
                    Mode::Plan => "plan",
                };
                app.transcript.push(TranscriptEntry::System(
                    format!("mode: {label}"),
                ));
            } else {
                let new_mode = match rest {
                    "normal" | "default" => Some(Mode::Normal),
                    "accept-edits" | "accept_edits" | "accept" => Some(Mode::AcceptEdits),
                    "plan" | "readonly" => Some(Mode::Plan),
                    _ => None,
                };
                if let Some(m) = new_mode {
                    app.mode.set(m);
                    let label = match m {
                        Mode::Normal => "normal",
                        Mode::AcceptEdits => "accept-edits",
                        Mode::Plan => "plan",
                    };
                    app.transcript.push(TranscriptEntry::System(
                        format!("→ {label} mode"),
                    ));
                } else {
                    app.transcript.push(TranscriptEntry::System(
                        format!("unknown mode: {rest}"),
                    ));
                }
            }
            None
        }
        "/plan" => {
            let new_mode = match app.mode.get() {
                Mode::Normal | Mode::AcceptEdits => Mode::Plan,
                Mode::Plan => Mode::Normal,
            };
            app.mode.set(new_mode);
            let label = match new_mode {
                Mode::Normal => "normal",
                Mode::AcceptEdits => "accept-edits",
                Mode::Plan => "plan",
            };
            app.transcript.push(TranscriptEntry::System(
                format!("→ {label} mode"),
            ));
            None
        }
        "/model" => {
            if rest.is_empty() || rest == "show" || rest == "status" {
                app.transcript.push(TranscriptEntry::System(
                    format!("model: {}", app.bar.model_label),
                ));
            } else {
                // Parse "provider/model" or just "model".
                if let Some((provider, model_id)) = rest.split_once('/') {
                    let _ = cmd_tx.send(Cmd::SetModel(
                        provider.to_string(),
                        model_id.to_string(),
                    ));
                    app.transcript.push(TranscriptEntry::System(
                        format!("setting model to {rest}…"),
                    ));
                } else {
                    // Just model — keep current provider.
                    let provider = app
                        .bar
                        .model_label
                        .split('/')
                        .next()
                        .unwrap_or("openai")
                        .to_string();
                    let _ = cmd_tx.send(Cmd::SetModel(provider, rest.to_string()));
                    app.transcript.push(TranscriptEntry::System(
                        format!("setting model to {rest}…"),
                    ));
                }
            }
            None
        }
        "/status" => {
            let mode = app.mode.get();
            let mode_label = match mode {
                Mode::Normal => "normal",
                Mode::AcceptEdits => "accept-edits",
                Mode::Plan => "plan",
            };
            app.transcript.push(TranscriptEntry::System(format!(
                "model: {}  ·  mode: {mode_label}  ·  tokens: {}",
                app.bar.model_label, app.bar.input_tokens,
            )));
            None
        }
        "/history" => {
            if app.history.is_empty() {
                app.transcript.push(TranscriptEntry::System(
                    "no history yet.".to_string(),
                ));
            } else {
                app.transcript.push(TranscriptEntry::System("history:".to_string()));
                for (i, item) in app.history.iter().rev().take(20).enumerate() {
                    app.transcript.push(TranscriptEntry::System(format!(
                        "  {}. {item}",
                        i + 1,
                    )));
                }
            }
            None
        }
        _ => {
            app.transcript.push(TranscriptEntry::System(format!(
                "unknown command: {cmd}  (try /help)",
            )));
            None
        }
    }
}

/// Handle a key when the approval modal is active.
fn handle_approval_key(
    app: &mut App,
    key: KeyEvent,
    shared_abort: &SharedAbort,
) -> Option<Action> {
    // Ctrl+C: deny the approval and abort the current turn.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let approval = app.approval.take()?;
        let _ = approval.responder.send(PromptChoice::Deny);
        if let Some(abort) = shared_abort.lock().unwrap().take() {
            abort.abort();
        }
        app.phase = Phase::Idle;
        return None;
    }

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

/// Handle a key when the ask-user modal is active.
fn handle_ask_key(app: &mut App, key: KeyEvent) -> Option<Action> {
    use crate::commands::code_approvals::AskOutcome;

    // Ctrl+C or Esc: cancel the whole ask flow.
    if (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        || key.code == KeyCode::Esc
    {
        let modal = app.ask.take()?;
        let _ = modal.responder.send(AskOutcome::Answer(serde_json::json!({
            "cancelled": true,
            "reason": "USER_DECLINED",
        })));
        app.phase = Phase::Streaming;
        return None;
    }

    let modal = app.ask.as_mut()?;
    let q = modal.current_question().clone();

    if modal.free_text_mode {
        // Free-text input mode.
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(&mut modal.free_text);
                let selected: Vec<String> = modal
                    .selected
                    .iter()
                    .map(|&i| q.options[i].label.clone())
                    .filter(|l| !l.eq_ignore_ascii_case("other"))
                    .collect();
                let answer = serde_json::json!({
                    "header": q.header,
                    "selected": selected,
                    "other": text,
                });
                modal.answers.push(answer);
                advance_question(app);
            }
            KeyCode::Char(c) => {
                modal.free_text.push(c);
            }
            KeyCode::Backspace => {
                modal.free_text.pop();
            }
            _ => {}
        }
        return None;
    }

    // Options list mode.
    match key.code {
        KeyCode::Up => {
            let len = q.options.len();
            if len > 0 {
                let idx = modal.list_state.selected().unwrap_or(0);
                modal.list_state.select(Some((idx + len - 1) % len));
            }
        }
        KeyCode::Down => {
            let len = q.options.len();
            if len > 0 {
                let idx = modal.list_state.selected().unwrap_or(0);
                modal.list_state.select(Some((idx + 1) % len));
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            // Quick-select: 1-9 picks option at that index.
            let num = c.to_digit(10).unwrap() as usize;
            if num >= 1 && num <= q.options.len() {
                let idx = num - 1;
                if q.multi_select {
                    if let Some(pos) = modal.selected.iter().position(|&i| i == idx) {
                        modal.selected.remove(pos);
                    } else {
                        modal.selected.push(idx);
                    }
                } else {
                    modal.selected.clear();
                    modal.selected.push(idx);
                }
            }
        }
        KeyCode::Char(' ') if q.multi_select => {
            // Space toggles selection in multi-select mode.
            if let Some(idx) = modal.list_state.selected() {
                if let Some(pos) = modal.selected.iter().position(|&i| i == idx) {
                    modal.selected.remove(pos);
                } else {
                    modal.selected.push(idx);
                }
            }
        }
        KeyCode::Enter => {
            if q.multi_select {
                // Enter confirms all selected.
                let selected = modal.selected.clone();
                if selected.is_empty() {
                    return None;
                }
                // Check if "Other" is selected → switch to free-text.
                let has_other = selected
                    .iter()
                    .any(|&i| q.options[i].label.eq_ignore_ascii_case("other"));
                if has_other {
                    modal.free_text_mode = true;
                    return None;
                }
                let answer = serde_json::json!({
                    "header": q.header,
                    "selected": selected.iter().map(|&i| q.options[i].label.clone()).collect::<Vec<_>>(),
                    "other": null,
                });
                modal.answers.push(answer);
                advance_question(app);
            } else {
                // Single-select: Enter picks the highlighted option.
                let idx = modal.list_state.selected()?;
                let label = &q.options[idx].label;
                if label.eq_ignore_ascii_case("other") {
                    modal.free_text_mode = true;
                    return None;
                }
                let answer = serde_json::json!({
                    "header": q.header,
                    "selected": [label],
                    "other": null,
                });
                modal.answers.push(answer);
                advance_question(app);
            }
        }
        _ => {}
    }

    None
}

/// Advance to the next question or finalize the ask flow.
fn advance_question(app: &mut App) {
    let modal = match app.ask.as_mut() {
        Some(m) => m,
        None => return,
    };

    modal.current += 1;
    if modal.current >= modal.questions.len() {
        // All questions answered — send result.
        let modal = app.ask.take().unwrap();
        let answers = modal.answers;
        let _ = modal.responder.send(
            crate::commands::code_approvals::AskOutcome::Answer(
                serde_json::json!({ "answers": answers }),
            ),
        );
        app.phase = Phase::Streaming;
    } else {
        // Reset state for the next question.
        let has_options = !modal.questions[modal.current].options.is_empty();
        modal.selected.clear();
        modal.free_text.clear();
        modal.free_text_mode = !has_options;
        if has_options {
            modal.list_state.select(Some(0));
        } else {
            modal.list_state.select(None);
        }
    }
}
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
            app.scroll = 0; // auto-scroll to bottom
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
                detail: detail.clone(),
            });
            app.current_tool = Some(tool_name);
            app.current_tool_detail = detail;
            app.spinner_label = "working…";
            app.scroll = 0; // auto-scroll to bottom
        }
        AgentMsg::ToolEnd { .. } => {
            app.current_tool = None;
            app.current_tool_detail = String::new();
            app.spinner_label = "thinking…";
        }
        AgentMsg::TurnEnd { elapsed_secs: _ } => {
            app.phase = Phase::Idle;
            app.turn_started = None;
            app.current_tool = None;
            app.current_tool_detail = String::new();
            app.transcript.push(TranscriptEntry::Blank);
            app.scroll = 0; // auto-scroll to bottom
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
        AgentMsg::AskRequest { payload, responder } => {
            let resp_clone = responder.clone();
            if let Some(modal) = AskModal::from_payload(&payload, responder) {
                app.ask = Some(modal);
                app.phase = Phase::Ask;
            } else {
                // Invalid payload — cancel immediately.
                let _ = resp_clone.send(crate::commands::code_approvals::AskOutcome::Answer(
                    serde_json::json!({ "cancelled": true, "reason": "USER_DECLINED" }),
                ));
            }
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
            app.scroll = 0; // auto-scroll to bottom
        }
        AgentMsg::CommandResult(text) => {
            app.transcript.push(TranscriptEntry::System(text));
            app.transcript.push(TranscriptEntry::Blank);
            app.scroll = 0; // auto-scroll to bottom
        }
        AgentMsg::Error(e) => {
            app.transcript.push(TranscriptEntry::System(format!("error: {e}")));
            app.scroll = 0; // auto-scroll to bottom
        }
    }
}

