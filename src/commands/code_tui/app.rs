//! Top-level App: state machine, event loop, and channel bridge
//! between the ratatui main thread and the asupersync background
//! runtime that drives `pi::AgentSessionHandle`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use pi::model::{AssistantMessageEvent, StopReason};
use pi::sdk::{create_agent_session, AbortHandle, AgentEvent, AgentSessionHandle};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::Terminal;
use tui_textarea::TextArea;

use anyhow::Context;

use crate::commands::code_approvals::{ApprovalState, ApprovalUi, PromptChoice};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_slash_registry;
use crate::commands::code_slash_router::{self, BgCommand, CustomResolveResult};
use crate::commands::code_ui::{
    apply_pending_shell_context, context_percent, context_tokens, context_window_for,
    shell_escape_command, start_background_agent, stop_line_text, usage_summary,
    BackgroundAgentLaunch, ShellEscapeAction, UsageRecord,
};
use crate::commands::code_hooks::{tool_policy_from_config, run_post_tool_hooks, run_stop_hooks, run_user_prompt_submit_hooks, SessionHookGuard};
use crate::commands::code_identity_prompt;
use crate::commands::code_mode_prompt;
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, DEFAULT_MAX_TOKENS, SessionPersistence,
};
use crate::commands::code_skills::{prompt_for_pillar, SkillPillar};
use crate::commands::code_team::{
    AgentCapability, AgentId, AgentRegistry, AgentColor, AgentKind, AgentRegistration, AgentStatus,
};
use crate::commands::code_team_spawn::{self, TeamManifest};
use crate::commands::code_tui::approvals::RatatuiApprovalUi;
use crate::commands::code_tui::terminal::TerminalGuard;
use crate::commands::code_tui::theme;
use crate::commands::code_tui::view;
use crate::config::{allow_rules_path, Config as LibertaiConfig};

/// Maximum entries in the input history. Matches the legacy REPL.
const HISTORY_MAX_LIMIT: usize = 64;

/// Shared abort handle — the main thread calls `.abort()` on Ctrl+C
/// to interrupt the background thread's current turn.
type SharedAbort = Arc<Mutex<Option<AbortHandle>>>;

/// Terminal outcome of a subagent (task-tool child session), reported by
/// the background thread in `AgentMsg::SubagentEnd`. Mirrors the strings
/// `code_task.rs` packs into `details.outcome` ("completed"/"failed"; we also
/// accept "stopped"/"aborted" so a future distinction maps cleanly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentOutcome {
    Completed,
    Failed,
    Stopped,
}

/// Map a `details.outcome` string to a [`SubagentOutcome`]. Unknown /
/// missing values default to `Completed`, matching `code_task.rs`'s
/// `error.is_none()` → "completed" reduction.
fn parse_outcome(s: &str) -> SubagentOutcome {
    match s.trim() {
        "failed" => SubagentOutcome::Failed,
        "stopped" | "aborted" => SubagentOutcome::Stopped,
        _ => SubagentOutcome::Completed,
    }
}

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
        /// Context-window occupancy for the turn — `context_tokens(&msg.usage)`
        /// (input + cache_read + cache_write), NOT `msg.usage.input`. This
        /// matches the legacy status bar's single source of truth.
        input_tokens: u64,
        /// `msg.usage.output` — tokens the model produced this turn.
        output_tokens: u64,
        /// `context_window_for(provider, model)` — resolved against pi's
        /// models.json / the catalog, with a 32k fallback.
        context_window: u32,
        /// `"{provider}/{model}"` label for the status chip.
        model_label: String,
        /// `msg.usage.cost.total` — this turn's cost, accumulated into the
        /// session total by the Usage handler.
        cost_total: f64,
        /// The pi `StopReason` for this turn, reused by the TurnEnd stop
        /// line ([`stop_line_text`] takes `&StopReason`). Stored directly
        /// (it's `Copy`) rather than stringifying so the rendered stop line
        /// matches the legacy "● done · …" verb exactly.
        stop_reason: StopReason,
    },
    /// System notice (compaction, retry, etc.) — dim in transcript.
    System(String),
    /// Result from a slash command executed on the background thread.
    CommandResult(String),
    /// Streaming text delta from a subagent (task tool child session).
    SubagentText {
        agent_name: String,
        text: String,
    },
    /// A subagent tool started executing. `args` is the tool's argument
    /// JSON (from `details.args`), kept so the scrollback renderer can
    /// reuse `tool_preview` instead of the TUI re-parsing it.
    SubagentToolStart {
        agent_name: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// A subagent tool finished. `output` is the joined Text content of
    /// the child's `partial_result.content`; `is_error` mirrors
    /// `details.isError`.
    SubagentToolEnd {
        agent_name: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
    /// A subagent finished its turn.
    SubagentEnd {
        agent_name: String,
        outcome: SubagentOutcome,
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
    /// Set the model (provider, model_id).
    SetModel(String, String),
    /// Clear the session and start fresh.
    Clear,
    /// Run a read-only slash command on the background thread (e.g.
    /// `/usage`, `/doctor`) — the ones that need session state only the bg
    /// thread owns. The bg thread builds the result text and sends it back
    /// via `AgentMsg::CommandResult`.
    RunReadOnly(BgCommand),
    /// Stop a specific agent (background agent / teammate / subagent) by
    /// taking its stored abort handle and calling `.abort()`. Handled on
    /// the background thread, where the shared registry lives; the abort
    /// works regardless of which thread issues it because `AbortHandle` is
    /// just an `AtomicBool` + `Notify`. The result rides back as an
    /// `AgentMsg::System` ("stopped {name}") so the main thread can push a
    /// transcript entry (the main thread owns the transcript).
    StopAgent(AgentId),
    /// Send a message to a specific agent. There is no pi primitive to
    /// inject a message into a running child turn, and the TUI has a single
    /// shared session (not per-agent sessions), so this is an honest stub:
    /// the background thread echoes it back as an `AgentMsg::System`
    /// ("reply to {name}: {text} (queued — per-agent reply sessions not
    /// yet supported)") so the user sees the message was received. Reply is
    /// deferred until a per-agent session model exists.
    SendToAgent(AgentId, String),
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
    /// Which pane has keyboard focus.
    pub focus: Focus,
    /// Selected agent index in the agents panel (when focus == Agents).
    pub agent_selection: usize,
    /// Agent output overlay (if active).
    pub agent_overlay: Option<AgentOverlay>,
    /// Live agent registry.
    pub registry: Arc<AgentRegistry>,
    /// Teams we've already fired a completion notification for, so a
    /// finished team only notifies once. Cleared by `/team` respawn.
    pub notified_teams: std::collections::HashSet<String>,
    /// Config.
    pub cfg: Arc<LibertaiConfig>,
    /// Status bar info.
    pub bar: BarStatus,
    /// Last reported turn usage (stop reason + ctx-in + out tokens),
    /// stashed by the `AgentMsg::Usage` handler and consumed by the
    /// `AgentMsg::TurnEnd` handler to render the dim stop line
    /// ("● done · 12.3k in · 1.2k out · 42s"). `take()`n on turn end so a
    /// later turn-end without a preceding Usage (e.g. an error path)
    /// simply omits the line.
    pub last_usage: Option<(StopReason, u64, u64)>,
    /// Last shell command run via the `!`/`!!` escape, so `!!` can repeat
    /// it. Mirrors the legacy REPL's `last_shell_command`.
    pub last_shell_command: Option<String>,
    /// Pending shell-escape output contexts (`!cmd`) waiting to prefix the
    /// next real prompt. Drained + prepended on the next `Action::Submit`.
    pub pending_shell_contexts: Vec<String>,
    /// Optional argv prefix wrapping the shell (e.g. a `--sandbox=strict`
    /// wrapper), honored by both the bg-thread bash tool and the `!`
    /// shell escape. Seeded in `run()` from the same local passed to
    /// `spawn_background`/`build_session`.
    pub bash_command_wrapper: Option<Vec<String>>,
    /// Custom slash commands discovered at startup via
    /// `code_slash_registry::discover`. Tier 3 of `handle_slash_command`
    /// resolves against this cache. A `/reload` could re-discover later
    /// (out of scope for M3a).
    pub custom_commands: Vec<code_slash_registry::CustomCommand>,
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

/// Which pane has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Normal input mode — textarea is active.
    #[default]
    Input,
    /// Browsing the agents panel — Up/Down selects, Enter opens overlay.
    Agents,
}

/// Agent output overlay state.
pub struct AgentOverlay {
    /// Agent name being viewed.
    pub agent_name: String,
    /// Scroll position within the overlay (0 = bottom).
    pub scroll: u16,
    /// Auto-tail: when true, new output resets `scroll` to 0 (stick to
    /// bottom). Flipped to false the moment the user scrolls up, and
    /// re-armed when they scroll back to the bottom.
    pub follow: bool,
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
    /// Tool result (the output a finished tool produced). Rendered as a
    /// dim line below the tool marker. `is_error` controls coloring.
    ToolResult {
        name: String,
        output: String,
        is_error: bool,
    },
    /// Subagent text (colored agent name prefix).
    SubagentText {
        agent_name: String,
        text: String,
    },
    /// Subagent tool marker. `args` is retained so the scrollback
    /// renderer can call `tool_preview` rather than re-parsing here.
    SubagentTool {
        agent_name: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// Subagent finished.
    SubagentEnd {
        agent_name: String,
        outcome: SubagentOutcome,
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
    /// Active output style (None == "default"). Set via `/output-style`.
    pub output_style: Option<String>,
    /// Status-line template string (legacy `{tokens}`/`{ctx}`/… tokens),
    /// expanded by the footer renderer via `expand_status_line_template`.
    pub status_line_template: String,
    /// Status-line shell command whose stdout replaces the rule line.
    pub status_line_command: String,
    /// Current working directory, seeded at startup in `run()`.
    pub cwd: String,
    /// Current git branch, seeded at startup in `run()` (None if detached
    /// or not in a git repo).
    pub git_branch: Option<String>,
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
        AgentEvent::ToolExecutionUpdate {
            partial_result, ..
        } => {
            // Subagent events arrive as ToolExecutionUpdate with a
            // `kind` field in the details JSON.
            let details = match &partial_result.details {
                Some(d) => d,
                None => return None,
            };
            let kind = details.get("kind").and_then(|v| v.as_str())?;
            let agent_name = details
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("subagent")
                .to_string();
            match kind {
                "subagent_text_delta" => {
                    // Text content is in partial_result.content[0].text
                    let text = partial_result
                        .content
                        .first()
                        .and_then(|c| match c {
                            pi::model::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if text.is_empty() {
                        None
                    } else {
                        Some(AgentMsg::SubagentText { agent_name, text })
                    }
                }
                "subagent_tool_start" => {
                    let tool_name = partial_result
                        .content
                        .first()
                        .and_then(|c| match c {
                            pi::model::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    // `code_task.rs` packs the tool args into details.args
                    // (a Value) rather than flattening them into the Text
                    // block. Default Null keeps the entry constructible
                    // when the field is absent.
                    let args = details
                        .get("args")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    Some(AgentMsg::SubagentToolStart {
                        agent_name,
                        tool_name: tool_name.trim().to_string(),
                        args,
                    })
                }
                "subagent_tool_end" => {
                    let tool_name = details
                        .get("tool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    // The tool output lives in partial_result.content's Text
                    // blocks (the child's result content), joined into one
                    // string. `details.isError` flags error results.
                    let output = partial_result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            pi::model::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let is_error = details
                        .get("isError")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    Some(AgentMsg::SubagentToolEnd {
                        agent_name,
                        tool_name,
                        output,
                        is_error,
                    })
                }
                "subagent_end" => {
                    let outcome = details
                        .get("outcome")
                        .and_then(|v| v.as_str())
                        .map(parse_outcome)
                        .unwrap_or(SubagentOutcome::Completed);
                    Some(AgentMsg::SubagentEnd { agent_name, outcome })
                }
                _ => None,
            }
        }
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

            // Per-turn usage records accumulated on this thread, so `/usage`
            // can build its summary from the same source as the legacy REPL
            // (the records live where the handle lives).
            let mut records: Vec<UsageRecord> = Vec::new();

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
                                // Reuse the shared context-occupancy helper so the
                                // status bar, ctx %, and stop line all agree on one
                                // number. NOTE: OpenAI double-counts cached tokens —
                                // context_tokens already folds in cache_read + cache_write,
                                // which is the pre-existing shared behavior; do not
                                // "fix" it here.
                                let input_tokens = context_tokens(&msg.usage);
                                let context_window =
                                    context_window_for(&msg.provider, &msg.model);
                                let _ = agent_tx.send(AgentMsg::Usage {
                                    input_tokens,
                                    output_tokens: msg.usage.output,
                                    context_window,
                                    model_label: format!("{}/{}", msg.provider, msg.model),
                                    cost_total: msg.usage.cost.total,
                                    stop_reason: msg.stop_reason,
                                });
                                // Accumulate this turn's usage so `/usage`
                                // (routed as Cmd::RunReadOnly(BgCommand::Usage))
                                // can summarize it with code_ui::usage_summary.
                                records.push(UsageRecord {
                                    provider: msg.provider.clone(),
                                    model: msg.model.clone(),
                                    input: input_tokens,
                                    output: msg.usage.output,
                                    context_window,
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
                                // Re-resolve the context window for the new
                                // provider/model and push a Usage update so the
                                // status chip reflects the swap immediately. Cost
                                // and token counts are zeroed (no turn happened);
                                // the Usage handler only overwrites the window +
                                // label here, leaving the session-cost accumulator
                                // untouched. This is sent only on the explicit
                                // SetModel path, so it can't clobber a real usage
                                // update mid-turn.
                                let _ = agent_tx.send(AgentMsg::Usage {
                                    input_tokens: 0,
                                    output_tokens: 0,
                                    context_window: context_window_for(&provider, &model_id),
                                    model_label: format!("{provider}/{model_id}"),
                                    cost_total: 0.0,
                                    stop_reason: StopReason::Stop,
                                });
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
                                records.clear();
                                let _ = agent_tx.send(AgentMsg::CommandResult(
                                    "→ fresh session.".to_string(),
                                ));
                            }
                            Err(e) => {
                                let _ = agent_tx.send(AgentMsg::Error(format!("{e:#}")));
                            }
                        }
                    }
                    Ok(Cmd::RunReadOnly(bg)) => {
                        // Dispatch a read-only slash command on this thread,
                        // where the handle + accumulated usage records live.
                        // The result text rides back as `AgentMsg::CommandResult`
                        // (handled by the existing CommandResult arm, which
                        // pushes a System entry + blank separator).
                        let text = match bg {
                            BgCommand::Usage => usage_text(&records),
                            BgCommand::Doctor => {
                                doctor_text(&handle, &current_provider, &current_model, &cfg)
                                    .await
                            }
                            BgCommand::ModelList { scoped_patterns } => {
                                code_slash_router::model_list_text(&cfg, &scoped_patterns)
                            }
                            BgCommand::SkillsList => code_slash_router::skills_list_text(),
                            BgCommand::MemoryShow => code_slash_router::memory_show_text(),
                            // CustomPrompt + ShellEscape are dispatched on the
                            // main thread (Tier 3 / the `!` escape); they are
                            // never sent as RunReadOnly. Bind to keep the match
                            // exhaustive and emit nothing if ever reached.
                            BgCommand::CustomPrompt { .. } | BgCommand::ShellEscape { .. } => {
                                String::new()
                            }
                        };
                        if !text.is_empty() {
                            let _ = agent_tx.send(AgentMsg::CommandResult(text));
                        }
                    }
                    Ok(Cmd::StopAgent(id)) => {
                        // Resolve the handle in the shared registry (keyed
                        // by id). `AbortHandle.abort()` is an AtomicBool +
                        // Notify, so this is safe from the bg thread even
                        // though the handle may have been spawned elsewhere.
                        // Mirrors the main-turn Ctrl+C path: take the abort
                        // slot, abort if Some, then mark the agent Stopped
                        // so the panel/overlay reflect it.
                        let handle = registry
                            .snapshot()
                            .into_iter()
                            .find(|h| h.id == id);
                        if let Some(handle) = handle {
                            let name = handle.name.clone();
                            if let Some(abort) = handle.take_abort() {
                                abort.abort();
                                registry.set_status(id, AgentStatus::Stopped);
                                let _ = agent_tx
                                    .send(AgentMsg::System(format!("stopped {name}")));
                            } else {
                                // No abort handle: the agent already
                                // finished (the spawner clears the slot on
                                // return). Report it rather than silently
                                // doing nothing.
                                let _ = agent_tx.send(AgentMsg::System(format!(
                                    "{name} already finished — nothing to stop"
                                )));
                            }
                        } else {
                            let _ = agent_tx.send(AgentMsg::System(
                                "agent not found — nothing to stop".to_string(),
                            ));
                        }
                    }
                    Ok(Cmd::SendToAgent(id, text)) => {
                        // Honest stub: there is no pi primitive to inject a
                        // message into a running child turn, and the TUI
                        // has a single shared session (not per-agent
                        // sessions). Echo the message back so the user sees
                        // it was received; reply is deferred until a
                        // per-agent session model exists.
                        let name = registry
                            .snapshot()
                            .into_iter()
                            .find(|h| h.id == id)
                            .map(|h| h.name.clone())
                            .unwrap_or_else(|| "<unknown agent>".to_string());
                        let _ = agent_tx.send(AgentMsg::System(format!(
                            "reply to {name}: {text} (queued — per-agent reply sessions not yet supported)"
                        )));
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
    let mut guard = TerminalGuard::new(true);

    enable_raw_mode()?;
    guard.raw_mode = true;

    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
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
    // Clone the wrapper first so the App field can keep a copy for the
    // `!`/`!!` shell escape (which honors `--sandbox=strict` like the
    // bg-thread bash tool).
    let bash_command_wrapper_for_app = bash_command_wrapper.clone();
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
    // Snapshot the status-line strings before `cfg` is moved into the
    // App struct below (the `cfg,` shorthand moves it).
    let status_line_template = cfg.status_line_template.clone();
    let status_line_command = cfg.status_line_command.clone();
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
            focus: Focus::default(),
            agent_selection: 0,
            agent_overlay: None,
        registry,
        notified_teams: std::collections::HashSet::new(),
        cfg,
        bar: BarStatus {
            model_label: format!("{provider}/{model}"),
            status_line_template,
            status_line_command,
            ..Default::default()
        },
        last_usage: None,
        last_shell_command: None,
        pending_shell_contexts: Vec::new(),
        bash_command_wrapper: bash_command_wrapper_for_app,
        custom_commands: Vec::new(),
    };

    // Seed the cwd + git-branch chips for the footer. These never change
    // during a session (we don't follow `cd`), matching the legacy REPL.
    app.bar.cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    app.bar.git_branch = current_git_branch();

    // Discover custom slash commands once at startup (cheap filesystem
    // scan) so Tier 3 of `handle_slash_command` can resolve them without a
    // per-`!command` re-scan. A `/reload` could re-discover later — out of
    // scope for M3a.
    let discover_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    app.custom_commands = code_slash_registry::discover(&discover_cwd);

    // Run the event loop.
    let result = run_loop(terminal, &mut app, agent_rx, cmd_tx, &shared_abort);

    // Restore terminal (also done by guard on drop, but do it explicitly
    // on the success path so `result` is returned after cleanup).
    drop(guard);
    result
}

/// Poll background agent processes to detect completion. For each
/// agent with a `pid`, checks if the process is still alive using
/// `kill(pid, 0)`. If the process has exited, updates the status from
/// `Working`/`Spawning` to `Completed`.
///
/// Returns the set of team names whose teammates *all* transitioned
/// from active to inactive on this poll. The caller uses this to fire
/// a one-shot completion notification per team. (Teammates without a
/// pid — e.g. errored-before-spawn — are treated as inactive and so
/// still count toward "all done".)
fn poll_agent_status(registry: &AgentRegistry) -> Vec<String> {
    let snapshot = registry.snapshot();
    // Active teammates per team *before* this poll reaps any exits.
    let prev_active: std::collections::HashMap<String, bool> = active_team_set(&snapshot);

    let mut completed_teams = Vec::new();
    for handle in &snapshot {
        let Some(pid) = handle.pid else { continue };
        // Only check agents that are still in an active state.
        let status = handle.status();
        if !status.is_active() {
            continue;
        }
        // kill(pid, 0) returns Err(ESRCH) if the process no longer
        // exists. On Unix this is a cheap syscall.
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if !alive {
            handle.set_status(crate::commands::code_team::AgentStatus::Completed);
            handle.set_current_tool(None);
        }
    }

    // A team "completed" if it had active members before and has none now.
    let still_active: std::collections::HashMap<String, bool> =
        active_team_set(&registry.snapshot());
    for team in prev_active.keys() {
        if !still_active.contains_key(team) {
            completed_teams.push(team.clone());
        }
    }
    completed_teams
}

/// Map each team that currently has ≥1 active teammate to `true`.
/// Non-teammate agents are ignored. Pure so it can be unit-tested.
fn active_team_set(
    handles: &[Arc<crate::commands::code_team::AgentHandle>],
) -> std::collections::HashMap<String, bool> {
    let mut map = std::collections::HashMap::new();
    for h in handles {
        if let crate::commands::code_team::AgentKind::Teammate { team } = &h.kind {
            if h.status().is_active() {
                map.insert(team.clone(), true);
            }
        }
    }
    map
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
                Event::Mouse(mouse) => {
                    use crossterm::event::MouseEventKind;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.scroll = app.scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollDown => {
                            app.scroll = app.scroll.saturating_sub(3);
                        }
                        _ => {}
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
                Ok(msg) => handle_agent_msg(app, msg, &cmd_tx),
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

        // Poll background agent process status. Cheap syscall per agent.
        // Fires a one-shot notification when every teammate in a team finishes.
        for team in poll_agent_status(&app.registry) {
            if app.notified_teams.contains(&team) {
                continue;
            }
            app.notified_teams.insert(team.clone());

            let count = app
                .registry
                .snapshot()
                .iter()
                .filter(|h| matches!(&h.kind, crate::commands::code_team::AgentKind::Teammate { team: t } if t == &team))
                .count();
            crate::commands::code_hooks::run_team_complete_hooks(&app.cfg, &team, count);

            let body = format!("Team “{team}” finished · {count} teammate(s) complete");
            crate::commands::code_term::notify_terminal("Team complete", &body);
            app.transcript
                .push(TranscriptEntry::System(format!("› {body}")));
            app.transcript.push(TranscriptEntry::Blank);
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

    // Tab: toggle focus between input and agents panel.
    if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE {
        // Close overlay first if open.
        if app.agent_overlay.is_some() {
            app.agent_overlay = None;
            return None;
        }
        let agents = app.registry.snapshot();
        if agents.is_empty() {
            return None; // no agents to browse
        }
        app.focus = match app.focus {
            Focus::Input => Focus::Agents,
            Focus::Agents => Focus::Input,
        };
        // Clamp selection.
        if app.agent_selection >= agents.len() {
            app.agent_selection = 0;
        }
        return None;
    }

    // Agent overlay keys (takes priority over everything).
    if app.agent_overlay.is_some() {
        return handle_agent_overlay_key(app, key, cmd_tx);
    }

    // Agent panel browse mode.
    if app.focus == Focus::Agents {
        let agents = app.registry.snapshot();
        match key.code {
            KeyCode::Up => {
                if !agents.is_empty() {
                    app.agent_selection =
                        (app.agent_selection + agents.len() - 1) % agents.len();
                }
                return None;
            }
            KeyCode::Down => {
                if !agents.is_empty() {
                    app.agent_selection = (app.agent_selection + 1) % agents.len();
                }
                return None;
            }
            KeyCode::Enter => {
                if let Some(handle) = agents.get(app.agent_selection) {
                    app.agent_overlay = Some(AgentOverlay {
                        agent_name: handle.name.clone(),
                        scroll: 0,
                        follow: true,
                    });
                }
                return None;
            }
            KeyCode::Esc => {
                app.focus = Focus::Input;
                return None;
            }
            _ => {}
        }
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
                // Shell escape (`!`/`!!`) runs on the MAIN thread
                // synchronously before the `/` slash check. The underlying
                // `run_shell_escape_tui` spawns a subprocess that blocks
                // until it exits — acceptable for a quick command the user
                // explicitly invoked (the legacy REPL did the same). A
                // long-running command will block the UI briefly; that
                // matches legacy behavior and is fine for M3a.
                if prompt.starts_with('!') {
                    handle_shell_escape(app, &prompt);
                    return None;
                }
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
                    // Apply pending shell-escape output contexts (`!cmd`)
                    // as a prefix to the next real prompt, mirroring the
                    // legacy REPL, then drain them.
                    let prompt =
                        apply_pending_shell_context(&app.pending_shell_contexts, &prompt);
                    app.pending_shell_contexts.clear();
                    Some(Action::Submit(prompt))
                }
            } else {
                None
            }
        }
        (KeyCode::Enter, _) if app.phase == Phase::Streaming => {
            // Queue a message while the agent is working.
            if key.modifiers != KeyModifiers::NONE {
                app.textarea.input(key);
                return None;
            }
            let prompt = app.textarea.lines().join("\n");
            if !prompt.is_empty() {
                app.textarea = TextArea::default();
                reset_textarea_style(&mut app.textarea);
                app.queued.push(prompt.clone());
                app.transcript
                    .push(TranscriptEntry::System(format!("› queued: {prompt}")));
                app.scroll = 0;
            }
            None
        }
        // Allow textarea input in all phases (Idle + Streaming).
        _ if app.phase == Phase::Idle || app.phase == Phase::Streaming => {
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

/// Handle a `!`/`!!` shell escape on the main thread, synchronously.
///
/// `!` runs the trailing command via `code_slash_router::run_shell_escape_tui`
/// (which spawns a subprocess and blocks until it exits — acceptable for a
/// quick command the user explicitly invoked, matching the legacy REPL).
/// `!!` repeats the last shell command (`app.last_shell_command`). The
/// captured stdout/stderr/exit is rendered as transcript lines and the
/// `prompt_context` is stashed for the next real prompt
/// (`app.pending_shell_contexts`). Honors `app.bash_command_wrapper` so the
/// escape respects `--sandbox=strict` like the bg-thread bash tool.
fn handle_shell_escape(app: &mut App, prompt: &str) {
    let action = shell_escape_command(&prompt[1..], app.last_shell_command.as_deref());
    match action {
        ShellEscapeAction::Usage(msg) => {
            app.transcript.push(TranscriptEntry::System(msg.to_string()));
            app.transcript.push(TranscriptEntry::Blank);
            app.scroll = 0;
        }
        ShellEscapeAction::Run(cmd) => {
            let res = code_slash_router::run_shell_escape_tui(
                &cmd,
                app.bash_command_wrapper.as_deref(),
            );
            // Record the last shell command so `!!` can repeat it.
            app.last_shell_command = Some(cmd.clone());
            // Render the result as transcript: a `$ cmd` header, then stdout
            // and stderr (each trimmed of trailing whitespace), then the exit
            // code when non-zero.
            app.transcript.push(TranscriptEntry::System(format!("$ {cmd}")));
            if !res.stdout.is_empty() {
                app.transcript
                    .push(TranscriptEntry::System(res.stdout.trim_end().to_string()));
            }
            if !res.stderr.is_empty() {
                app.transcript
                    .push(TranscriptEntry::System(res.stderr.trim_end().to_string()));
            }
            if let Some(code) = res.exit_code {
                if code != 0 {
                    app.transcript
                        .push(TranscriptEntry::System(format!("exit {code}")));
                }
            }
            // Stash the prompt context for the next real prompt — only when
            // the command actually ran (exit code present), mirroring the
            // legacy REPL which discards context on spawn failure.
            if res.exit_code.is_some() {
                app.pending_shell_contexts.push(res.prompt_context);
            }
            app.transcript.push(TranscriptEntry::Blank);
            app.scroll = 0;
        }
    }
}

/// Humanize a token count like the legacy `human_tokens` (private in
/// `code_ui`): `>=1k` → `12.3k`, else the bare number. Inlined here so the
/// `/usage` text can reuse the same formatting without re-exporting the
/// private helper.
fn human_tokens(n: u64) -> String {
    if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Build the `/usage` text from the bg thread's accumulated usage records,
/// reusing `code_ui::usage_summary`. Mirrors the key lines of the legacy
/// `print_usage_summary` (which prints to stdout) but returns a plain string
/// for the transcript.
fn usage_text(records: &[UsageRecord]) -> String {
    match usage_summary(records) {
        Some(summary) => {
            let mut out = String::new();
            out.push_str("usage\n");
            out.push_str(&format!("  provider/model: {}/{}\n", summary.provider, summary.model));
            out.push_str(&format!("  turns: {}\n", summary.turns));
            out.push_str(&format!(
                "  last turn: {} in · {} out\n",
                human_tokens(summary.last_input),
                human_tokens(summary.last_output)
            ));
            out.push_str(&format!(
                "  session output total: {}\n",
                human_tokens(summary.output_total)
            ));
            if summary.context_window > 0 {
                let pct = ((summary.context_high_water as f64
                    / f64::from(summary.context_window))
                    * 100.0)
                    .round()
                    .min(100.0) as u32;
                out.push_str(&format!(
                    "  context high-water: {pct}% · {} / {}\n",
                    human_tokens(summary.context_high_water),
                    human_tokens(u64::from(summary.context_window))
                ));
            } else {
                out.push_str(&format!(
                    "  context high-water: {}\n",
                    human_tokens(summary.context_high_water)
                ));
            }
            out
        }
        None => "usage\n  no usage recorded yet — send a prompt first.\n".to_string(),
    }
}

/// Build the `/doctor` text on the bg thread, reusing the live
/// `AgentSessionHandle::state` snapshot + the session config. Mirrors a
/// trimmed subset of the legacy `print_doctor` (which prints to stdout) —
/// enough to surface cwd, provider/model, mode, session id, persistence,
/// transcript size, auth, and config path. Async because `state()` is async.
async fn doctor_text(
    handle: &AgentSessionHandle,
    provider: &str,
    model: &str,
    cfg: &LibertaiConfig,
) -> String {
    let mut out = String::new();
    out.push_str("doctor\n");
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    out.push_str(&format!("  cwd: {cwd}\n"));
    out.push_str(&format!("  provider/model: {provider}/{model}\n"));
    match handle.state().await {
        Ok(state) => {
            out.push_str(&format!(
                "  pi session: {}\n",
                state.session_id.as_deref().unwrap_or("not persisted")
            ));
            out.push_str(&format!(
                "  session persistence: {}\n",
                if state.save_enabled { "enabled" } else { "disabled" }
            ));
            out.push_str(&format!("  transcript: {} message(s)\n", state.message_count));
            if let Some(level) = state.thinking_level {
                out.push_str(&format!("  thinking: {level}\n"));
            }
        }
        Err(e) => out.push_str(&format!("  pi session: {e}\n")),
    }
    out.push_str(&format!(
        "  LibertAI auth: {}\n",
        cfg.auth
            .api_key
            .as_deref()
            .map(|_| "logged in")
            .unwrap_or("not logged in")
    ));
    match crate::config::config_path() {
        Ok(path) => out.push_str(&format!("  config path: {}\n", path.display())),
        Err(e) => out.push_str(&format!("  config path: {e}\n")),
    }
    out
}

// ---------------------------------------------------------------------------
// M3b: agent / team spawn — pure parsing helpers + small app accessors
// ---------------------------------------------------------------------------
//
// These helpers are the testability seam between the slash-command *parsing*
// (pure: no spawn, no println, no registry mutation) and the *real spawn*
// (the thin arm shells in `handle_slash_command`). `build_team_invocation`
// and `build_agent_invocation` return the fully-resolved invocation the arm
// then hands to `code_team_spawn::spawn_team` / `start_background_agent`.

/// A parsed `/team` invocation: the team name plus its resolved
/// `TeamManifest`. Produced by the pure [`build_team_invocation`] helper;
/// the slash arm feeds it to the real spawn.
#[derive(Debug, Clone)]
struct TeamInvocation {
    team_name: String,
    manifest: TeamManifest,
}

/// Split `app.bar.model_label` (`"provider/model"`) into its `(provider,
/// model)` parts. Falls back to the config's defaults when the label can't
/// be split, so the spawn always has a concrete provider/model pair. Reads
/// only from `app` — no spawn, no mutation.
fn app_provider_model(app: &App) -> (String, String) {
    if let Some((p, m)) = app.bar.model_label.split_once('/') {
        if !p.is_empty() && !m.is_empty() {
            return (p.to_string(), m.to_string());
        }
    }
    (
        app.cfg.default_code_provider.clone(),
        app.cfg.default_code_model.clone(),
    )
}

/// Render an [`AgentStatus`] as a short lowercase label for `/agents`.
fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Spawning => "spawning",
        AgentStatus::Working => "working",
        AgentStatus::NeedsInput => "needs-input",
        AgentStatus::Idle => "idle",
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "stopped",
    }
}

/// Parse `/team` args into a [`TeamInvocation`] WITHOUT spawning. Pure:
/// no I/O beyond reading a manifest file (needed to resolve a team name),
/// no spawn, no registry mutation, no printing.
///
/// Supported forms:
/// - `/team <name>` — resolve `<name>` against `.libertai/teams/<name>.toml`.
/// - `/team <name> <manifest-path>` — load the manifest from an explicit path.
/// - `/team <name> <agent> <task>` — quick inline form: a single teammate
///   named `agent-1` running `<agent>` on `<task>`.
///
/// The provider/model/mode are the caller's resolved defaults (the manifest
/// may override them at spawn time).
fn build_team_invocation(
    rest: &str,
    cwd: &Path,
    provider: &str,
    model: &str,
    mode: Mode,
) -> anyhow::Result<TeamInvocation> {
    let _ = (provider, model, mode); // defaults threaded by the arm; unused here.
    let rest = rest.trim();
    if rest.is_empty() {
        anyhow::bail!("usage: /team <name>  |  /team <name> <manifest-path>  |  /team <name> <agent> <task>");
    }
    // Tokenize on whitespace runs so `"a  b"` doesn't yield an empty middle
    // token. We count tokens to pick the form, then recover the raw remainder
    // for the task/manifest path via `split_once` on the first run.
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let team_name = tokens[0].to_string();
    if team_name.is_empty() {
        anyhow::bail!("team name must not be empty");
    }

    // Quick inline form: `<name> <agent> <task...>` — three+ tokens. Build a
    // single-teammate manifest; the task is everything after the agent token.
    if tokens.len() >= 3 {
        let agent = tokens[1].trim();
        // The raw task is the remainder after the first two tokens. Use
        // `split_once` on the first whitespace run to skip `<name>`, then
        // `split_once` again on the next run to skip `<agent>`.
        let after_name = rest
            .split_once(char::is_whitespace)
            .map(|(_, r)| r.trim_start())
            .unwrap_or("");
        let task = after_name
            .split_once(char::is_whitespace)
            .map(|(_, r)| r.trim())
            .unwrap_or("");
        if agent.is_empty() || task.is_empty() {
            anyhow::bail!("usage: /team <name> <agent> <task>");
        }
        let manifest = TeamManifest {
            model: None,
            provider: None,
            mode: None,
            teammates: vec![code_team_spawn::TeammateSpec {
                name: "agent-1".to_string(),
                agent: agent.to_string(),
                task: task.to_string(),
                model: None,
            }],
        };
        return Ok(TeamInvocation { team_name, manifest });
    }

    // Two-token form: `<name> <manifest-path>` — load from an explicit file.
    if tokens.len() == 2 {
        let path_arg = rest
            .split_once(char::is_whitespace)
            .map(|(_, r)| r.trim())
            .unwrap_or("");
        let manifest_path = if Path::new(path_arg).is_absolute() {
            PathBuf::from(path_arg)
        } else {
            cwd.join(path_arg)
        };
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
        let manifest = code_team_spawn::parse_manifest(&raw)?;
        return Ok(TeamInvocation { team_name, manifest });
    }

    // Bare `<name>` — resolve against `.libertai/teams/<name>.toml`.
    let manifest = code_team_spawn::resolve_team(cwd, &team_name)?;
    Ok(TeamInvocation { team_name, manifest })
}

/// Parse `/agent` args (`"<agent> [task...]"`) into a [`BackgroundAgentLaunch`]
/// WITHOUT spawning. Pure: no spawn, no registry mutation, no printing. The
/// caller resolves provider/model/mode/cwd from app state and passes them in.
///
/// Supported form: `/agent <agent> <task...>` (the `<agent>` is a sub-agent
/// name; the remainder is the task prompt). The launch is marked as a plain
/// background run (no team / teammate context).
fn build_agent_invocation(
    rest: &str,
    cwd: &Path,
    provider: &str,
    model: &str,
    mode: Mode,
) -> anyhow::Result<BackgroundAgentLaunch> {
    let rest = rest.trim();
    let Some((name, task)) = rest.split_once(char::is_whitespace) else {
        anyhow::bail!("usage: /agent <agent> <task>");
    };
    let name = name.trim();
    let task = task.trim();
    if name.is_empty() || task.is_empty() {
        anyhow::bail!("usage: /agent <agent> <task>");
    }
    Ok(BackgroundAgentLaunch {
        name: name.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        mode,
        prompt: task.to_string(),
        cwd: cwd.to_path_buf(),
        agent: Some(name.to_string()),
        team: None,
        teammate_name: None,
    })
}

/// Handle a slash command. Returns `Some(Action)` for commands that
/// need the main loop to act (Quit, Submit), `None` for commands
/// handled entirely here.
/// Handle keys when the agent output overlay is open.
fn handle_agent_overlay_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::Sender<Cmd>,
) -> Option<Action> {
    match key.code {
        KeyCode::Esc | KeyCode::Tab => {
            app.agent_overlay = None;
        }
        // Scrolling up leaves the bottom: stop auto-tailing so new output
        // doesn't yank the user back down.
        KeyCode::Up | KeyCode::PageUp => {
            if let Some(overlay) = &mut app.agent_overlay {
                overlay.follow = false;
                overlay.scroll = overlay.scroll.saturating_add(3);
            }
        }
        // Scrolling down decrements the offset; reaching the bottom
        // (scroll == 0) re-arms auto-tail.
        KeyCode::Down | KeyCode::PageDown => {
            if let Some(overlay) = &mut app.agent_overlay {
                overlay.scroll = overlay.scroll.saturating_sub(3);
                if overlay.scroll == 0 {
                    overlay.follow = true;
                }
            }
        }
        // `s` / `x`: stop the viewed agent. This aborts the agent
        // DIRECTLY on the main thread (which owns `app.registry` and is
        // never blocked during a turn), mirroring the main-turn Ctrl-C
        // path that aborts via `shared_abort` from the main thread. This
        // is critical for timing: the bg thread is blocked inside
        // `handle.prompt_with_abort(...)` for the whole turn (subagents
        // run inline inside that), so it cannot drain `cmd_rx` mid-turn —
        // a `Cmd::StopAgent` sent to the bg thread would sit unprocessed
        // until the turn ends, defeating the point (the subagent would
        // already be done). `AbortHandle.abort` is an AtomicBool + Notify,
        // safe to fire cross-thread from here. The overlay stays open so
        // the user can watch the stopped agent.
        KeyCode::Char('s') | KeyCode::Char('x') => {
            if let Some(overlay) = &app.agent_overlay {
                let name = overlay.agent_name.clone();
                if let Some(handle) = app.registry.find_by_name(&name) {
                    if let Some(abort) = handle.take_abort() {
                        abort.abort();
                        handle.set_status(AgentStatus::Stopped);
                        app.transcript.push(TranscriptEntry::System(format!(
                            "stopped {name}"
                        )));
                    } else {
                        app.transcript.push(TranscriptEntry::System(format!(
                            "{name} already finished — nothing to stop"
                        )));
                    }
                } else {
                    app.transcript.push(TranscriptEntry::System(
                        "agent not found — nothing to stop".to_string(),
                    ));
                }
            }
        }
        // `r`: reply to the viewed agent. There is no pi primitive to
        // inject a message into a running child turn (the parent turn
        // owns the child handle and awaits it), and the TUI has a single
        // shared session rather than per-agent sessions. So this is an
        // honest stub: take the textarea content as the reply body,
        // send `Cmd::SendToAgent(id, text)`, and the bg thread echoes it
        // back as a System line ("reply to {name}: … (queued — per-agent
        // reply sessions not yet supported)") so the user sees the
        // message was received. The overlay stays open.
        KeyCode::Char('r') => {
            if let Some(overlay) = &app.agent_overlay {
                let text = app.textarea.lines().join("\n");
                if let Some(handle) = app.registry.find_by_name(&overlay.agent_name) {
                    let _ = cmd_tx.send(Cmd::SendToAgent(handle.id, text));
                }
            }
        }
        _ => {}
    }
    None
}

/// Collect output for a specific agent (by name). For background
/// agents with a `log_path`, reads the log file (each raw line wrapped
/// as a [`TranscriptEntry::System`] so the overlay can render it
/// uniformly). For in-process subagents, scans the transcript for the
/// agent's `SubagentText` / `SubagentTool` / `SubagentEnd` entries plus
/// the per-tool result lines (stored as `ToolResult` with the name
/// prefixed `"{agent} · {tool}"` — see the `SubagentToolEnd` arm of
/// [`handle_agent_msg`]).
///
/// Returns the *typed* entries so the overlay can reuse the scrollback's
/// per-variant styling (agent-colored markers, `↳` result line,
/// `theme::error` on `is_error`) instead of a flat markdown dump that
/// dropped the results and lost the coloring.
pub fn agent_transcript(app: &App, agent_name: &str) -> Vec<TranscriptEntry> {
    // If this agent has a log file, read it — that's the authoritative
    // output for background agents / teammates. Wrap each raw stdout/stderr
    // line as a dim System entry so the overlay renders it uniformly
    // (the log is already the final-formatted text, so dim styling fits).
    if let Some(handle) = app.registry.find_by_name(agent_name) {
        if let Some(log_path) = &handle.log_path {
            return read_agent_log(log_path)
                .into_iter()
                .map(TranscriptEntry::System)
                .collect();
        }
    }

    // Fall back to transcript entries (in-process subagents). The leading
    // "{agent} · " prefix on `ToolResult` (see `SubagentToolEnd` storage)
    // is what binds a per-tool result to this agent.
    let prefix = format!("{agent_name} · ");
    let mut entries = Vec::new();
    for entry in &app.transcript {
        match entry {
            TranscriptEntry::SubagentText {
                agent_name: name,
                text,
            } if name == agent_name => {
                entries.push(TranscriptEntry::SubagentText {
                    agent_name: agent_name.to_string(),
                    text: text.clone(),
                });
            }
            TranscriptEntry::SubagentTool {
                agent_name: name,
                tool_name,
                args,
            } if name == agent_name => {
                entries.push(TranscriptEntry::SubagentTool {
                    agent_name: agent_name.to_string(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                });
            }
            TranscriptEntry::SubagentEnd {
                agent_name: name,
                outcome,
            } if name == agent_name => {
                entries.push(TranscriptEntry::SubagentEnd {
                    agent_name: agent_name.to_string(),
                    outcome: *outcome,
                });
            }
            TranscriptEntry::ToolResult {
                name,
                output,
                is_error,
            } if name.starts_with(&prefix) => {
                // Strip the "{agent} · " prefix so the overlay's result line
                // reads "{tool}" (mirroring how the scrollback shows the
                // tool name, just without the agent repetition here since
                // the whole overlay is already scoped to this agent).
                let tool = name[prefix.len()..].to_string();
                entries.push(TranscriptEntry::ToolResult {
                    name: tool,
                    output: output.clone(),
                    is_error: *is_error,
                });
            }
            _ => {}
        }
    }
    entries
}

/// Read an agent's log file and return its contents as lines. The log
/// file is the combined stdout+stderr of the background `libertai code
/// --print` process. Returns an empty vec if the file can't be read.
fn read_agent_log(log_path: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(log_path) {
        Ok(content) => content.lines().map(String::from).collect(),
        Err(_) => Vec::new(),
    }
}

/// Best-effort current git branch, read directly from `.git/HEAD` (no
/// subprocess). Returns `None` if not in a git repo, detached, or the
/// file can't be read/parsed. Walks up from the cwd looking for the
/// first `.git/HEAD`, matching how a shell prompt would resolve it.
fn current_git_branch() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let head = dir.join(".git").join("HEAD");
        if let Ok(content) = std::fs::read_to_string(&head) {
            let content = content.trim();
            // On a branch: "ref: refs/heads/<branch>".
            if let Some(rest) = content.strip_prefix("ref: refs/heads/") {
                return Some(rest.to_string());
            }
            // Detached HEAD: a bare commit sha — no branch to report.
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

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
                "Commands: /help /exit /clear /mode /model [/model list] /status /statusline /statusline-command /output-style /history /usage /doctor /skills list /memory show /skills /memory /team /agent /agents  !<cmd>  !!  custom templates (e.g. /apply)".to_string(),
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
            // Tier 2 — `/model list [patterns…]`: route to the bg thread,
            // which fetches the catalog via `code_slash_router::model_list_text`
            // (a network call) and sends the listing back as
            // `AgentMsg::CommandResult`.
            if rest == "list" || rest.starts_with("list ") {
                let scoped_patterns: Vec<String> = rest
                    .strip_prefix("list")
                    .map(|s| s.split_whitespace().map(String::from).collect())
                    .unwrap_or_default();
                let _ = cmd_tx.send(Cmd::RunReadOnly(BgCommand::ModelList {
                    scoped_patterns,
                }));
                app.transcript.push(TranscriptEntry::System(
                    "listing models…".to_string(),
                ));
                return None;
            }
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
        "/skills" => {
            // Tier 2 — `/skills` or `/skills list`: list the active code-pillar
            // skills synchronously (pure read-only I/O) via the router adapter.
            if rest.is_empty() || rest == "list" || rest == "show" {
                let text = code_slash_router::skills_list_text();
                app.transcript.push(TranscriptEntry::System(text));
                app.transcript.push(TranscriptEntry::Blank);
            } else {
                app.transcript.push(TranscriptEntry::System(format!(
                    "unknown /skills subcommand: {rest}  (try /skills list)",
                )));
            }
            None
        }
        "/memory" => {
            // Tier 2 — `/memory` or `/memory show`: render the current project
            // memory state synchronously via the router adapter.
            if rest.is_empty() || rest == "show" || rest == "status" {
                let text = code_slash_router::memory_show_text();
                app.transcript.push(TranscriptEntry::System(text));
                app.transcript.push(TranscriptEntry::Blank);
            } else {
                app.transcript.push(TranscriptEntry::System(format!(
                    "unknown /memory subcommand: {rest}  (try /memory show)",
                )));
            }
            None
        }
        "/usage" | "/cost" => {
            // Tier 2 — `/usage` needs the session's accumulated usage records,
            // which live on the bg thread. Route there and let it build the
            // summary text (sent back as `AgentMsg::CommandResult`).
            let _ = cmd_tx.send(Cmd::RunReadOnly(BgCommand::Usage));
            app.transcript
                .push(TranscriptEntry::System("usage…".to_string()));
            None
        }
        "/doctor" => {
            // Tier 2 — `/doctor` needs the live `AgentSessionHandle::state`
            // snapshot, owned by the bg thread. Route there.
            let _ = cmd_tx.send(Cmd::RunReadOnly(BgCommand::Doctor));
            app.transcript
                .push(TranscriptEntry::System("doctor…".to_string()));
            None
        }
        "/status" => {
            let mode = app.mode.get();
            let mode_label = match mode {
                Mode::Normal => "normal",
                Mode::AcceptEdits => "accept-edits",
                Mode::Plan => "plan",
            };
            // Build the status line segment-by-segment, each guarded for
            // missing data so we never show a bare "·  ·" gap. Reuses the
            // shared context_percent helper so the % matches the chip.
            let mut parts: Vec<String> = Vec::new();
            parts.push(format!("model: {}", app.bar.model_label));
            parts.push(format!("mode: {mode_label}"));
            let pct = context_percent(app.bar.input_tokens, app.bar.context_window);
            let window_k = app.bar.context_window / 1000;
            parts.push(format!(
                "ctx: {pct}% ({} / {}k)",
                app.bar.input_tokens, window_k,
            ));
            if let Some(cost) = app.bar.estimated_cost {
                if cost > 0.0 {
                    parts.push(format!("cost: ${cost:.2}"));
                }
            }
            if !app.bar.cwd.is_empty() {
                parts.push(format!("cwd: {}", app.bar.cwd));
            }
            if let Some(branch) = &app.bar.git_branch {
                parts.push(format!("branch: {branch}"));
            }
            let style = app.bar.output_style.clone().unwrap_or_else(|| "default".to_string());
            parts.push(format!("style: {style}"));
            app.transcript
                .push(TranscriptEntry::System(parts.join("  ·  ")));
            None
        }
        "/statusline" => {
            // `/statusline` (no arg) shows the current template.
            // `/statusline <template>` stores it for the footer renderer
            // (which expands it via `expand_status_line_template`).
            // `/statusline-command <cmd>` stores a shell command whose
            // stdout replaces the rule line. We only STORE here; the
            // rendering is the footer's job.
            if rest.is_empty() {
                let shown = if app.bar.status_line_template.is_empty() {
                    "no statusline template set".to_string()
                } else {
                    app.bar.status_line_template.clone()
                };
                app.transcript
                    .push(TranscriptEntry::System(format!("statusline: {shown}")));
            } else {
                app.bar.status_line_template = rest.to_string();
                app.transcript
                    .push(TranscriptEntry::System("statusline set".to_string()));
            }
            None
        }
        "/statusline-command" => {
            if rest.is_empty() {
                let shown = if app.bar.status_line_command.is_empty() {
                    "no statusline command set".to_string()
                } else {
                    app.bar.status_line_command.clone()
                };
                app.transcript
                    .push(TranscriptEntry::System(format!("statusline-command: {shown}")));
            } else {
                app.bar.status_line_command = rest.to_string();
                app.transcript.push(TranscriptEntry::System(
                    "statusline-command set".to_string(),
                ));
            }
            None
        }
        "/output-style" => {
            // `/output-style` (no arg) or `/output-style status` shows the
            // current style. `/output-style <name>` looks the style up via
            // the pure `find_style` helper (we do NOT call the legacy
            // handle_output_style, which prints to stdout). "default"
            // clears the override; any other found name sets it.
            if rest.is_empty() || rest == "status" {
                let shown = app
                    .bar
                    .output_style
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                app.transcript
                    .push(TranscriptEntry::System(format!("output style: {shown}")));
            } else {
                let cwd = std::env::current_dir().ok();
                match crate::commands::code_output_style::find_style(rest, cwd.as_deref()) {
                    Some(style) => {
                        if style.name == "default" {
                            app.bar.output_style = None;
                        } else {
                            app.bar.output_style = Some(style.name.clone());
                        }
                        app.transcript.push(TranscriptEntry::System(format!(
                            "→ output style: {}",
                            app.bar.output_style.clone().unwrap_or_else(|| "default".to_string())
                        )));
                    }
                    None => {
                        app.transcript.push(TranscriptEntry::System(format!(
                            "unknown output style: {rest}",
                        )));
                    }
                }
            }
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
        "/team" => {
            // Tier 1 — needs App state (registry, mode, cwd). Parse the
            // args into a TeamManifest via the pure `build_team_invocation`
            // helper, then run the real spawn (init_team_tasks + spawn_team),
            // which registers each teammate in `app.registry`. spawn_team
            // already registers teammates, so no further registry work is
            // needed for the agents panel / completion notification path.
            let cwd = match std::env::current_dir() {
                Ok(c) => c,
                Err(e) => {
                    app.transcript.push(TranscriptEntry::System(
                        format!("team: could not resolve cwd: {e}"),
                    ));
                    app.transcript.push(TranscriptEntry::Blank);
                    return None;
                }
            };
            let (provider, model) = app_provider_model(app);
            let mode = app.mode.get();
            match build_team_invocation(rest, &cwd, &provider, &model, mode) {
                Ok(inv) => {
                    // Initialize the shared task list BEFORE spawning so
                    // teammates can read it immediately on startup (legacy
                    // order). Errors here are non-fatal — we surface them
                    // but still attempt the spawn.
                    if let Err(e) =
                        code_team_spawn::init_team_tasks(&inv.team_name, &inv.manifest, &cwd)
                    {
                        app.transcript.push(TranscriptEntry::System(format!(
                            "team: failed to init task list: {e:#}"
                        )));
                    }
                    match code_team_spawn::spawn_team(
                        &inv.team_name,
                        &inv.manifest,
                        &cwd,
                        &provider,
                        &model,
                        mode,
                        Some(&app.registry),
                    ) {
                        Ok(spawned) => {
                            // Clear the one-shot completion notification so
                            // a respawn can fire again.
                            app.notified_teams.remove(&inv.team_name);
                            let n = spawned.len();
                            let names: Vec<&str> =
                                spawned.iter().map(|t| t.name.as_str()).collect();
                            app.transcript.push(TranscriptEntry::System(format!(
                                "→ team “{}” spawned · {n} teammate(s): {}",
                                inv.team_name,
                                names.join(", "),
                            )));
                            app.transcript.push(TranscriptEntry::System(
                                "press Tab to browse agents.".to_string(),
                            ));
                            app.transcript.push(TranscriptEntry::Blank);
                        }
                        Err(e) => {
                            app.transcript.push(TranscriptEntry::System(format!(
                                "team: failed to spawn `{}`: {e:#}",
                                inv.team_name
                            )));
                            app.transcript.push(TranscriptEntry::Blank);
                        }
                    }
                }
                Err(e) => {
                    app.transcript.push(TranscriptEntry::System(format!(
                        "team: {e:#}"
                    )));
                    app.transcript.push(TranscriptEntry::Blank);
                }
            }
            None
        }
        "/agent" => {
            // Tier 1 — parse "<agent> [task...]" into a pure
            // BackgroundAgentLaunch via `build_agent_invocation`, then start
            // the detached child and register it in `app.registry` (mirrors
            // spawn_team's AgentRegistration construction).
            let cwd = match std::env::current_dir() {
                Ok(c) => c,
                Err(e) => {
                    app.transcript.push(TranscriptEntry::System(
                        format!("agent: could not resolve cwd: {e}"),
                    ));
                    app.transcript.push(TranscriptEntry::Blank);
                    return None;
                }
            };
            let (provider, model) = app_provider_model(app);
            let mode = app.mode.get();
            match build_agent_invocation(rest, &cwd, &provider, &model, mode) {
                Ok(launch) => {
                    let name = launch.name.clone();
                    let prompt_preview: String =
                        launch.prompt.chars().take(80).collect();
                    match start_background_agent(&launch) {
                        Ok(started) => {
                            let reg = AgentRegistration {
                                name: name.clone(),
                                kind: AgentKind::Background {
                                    pid: started.pid,
                                    run_id: String::new(),
                                },
                                color: AgentColor::color_for_name(&name),
                                capability: AgentCapability::ReadOnly,
                                cwd: cwd.clone(),
                                model: launch.model.clone(),
                                prompt_preview,
                                parent: None,
                                pid: Some(started.pid),
                                log_path: Some(started.log_path.clone()),
                            };
                            let handle = app.registry.register(reg);
                            handle.set_status(AgentStatus::Working);
                            app.transcript.push(TranscriptEntry::System(format!(
                                "→ agent “{name}” spawned · pid {}",
                                started.pid
                            )));
                            app.transcript.push(TranscriptEntry::System(
                                "press Tab to browse agents.".to_string(),
                            ));
                            app.transcript.push(TranscriptEntry::Blank);
                        }
                        Err(e) => {
                            app.transcript.push(TranscriptEntry::System(format!(
                                "agent: failed to spawn `{name}`: {e:#}"
                            )));
                            app.transcript.push(TranscriptEntry::Blank);
                        }
                    }
                }
                Err(e) => {
                    app.transcript.push(TranscriptEntry::System(format!(
                        "agent: {e:#}"
                    )));
                    app.transcript.push(TranscriptEntry::Blank);
                }
            }
            None
        }
        "/agents" => {
            // Tier 1 — render the current registry snapshot directly (do NOT
            // call a separate active_agents_for_footer). One System line per
            // agent: name · status · pid (if any) · team (if any).
            let snapshot = app.registry.snapshot();
            if snapshot.is_empty() {
                app.transcript.push(TranscriptEntry::System("no agents.".to_string()));
            } else {
                app.transcript.push(TranscriptEntry::System(format!(
                    "agents ({}):",
                    snapshot.len()
                )));
                for h in &snapshot {
                    let mut line = format!("  {} · {}", h.name, status_label(h.status()));
                    if let Some(pid) = h.pid {
                        line.push_str(&format!(" · pid {pid}"));
                    }
                    if let AgentKind::Teammate { team } = &h.kind {
                        line.push_str(&format!(" · team {team}"));
                    }
                    app.transcript.push(TranscriptEntry::System(line));
                }
            }
            app.transcript.push(TranscriptEntry::Blank);
            None
        }
        _ => {
            // Tier 3 — custom templates: before the unknown fallback, try
            // `code_slash_router::resolve_custom` against the cached
            // discovered commands. On a unique hit, expand the template
            // (synchronous — uses the pure `code_slash_registry::expand`)
            // and submit it as a prompt via `Cmd::Prompt`. On ambiguity,
            // list the matching invocations. On a miss, fall through to the
            // Tier 4 unknown message so a valid custom template never shows
            // "unknown command".
            match code_slash_router::resolve_custom(&app.custom_commands, cmd) {
                CustomResolveResult::Hit(hit) => {
                    let expanded = code_slash_registry::expand_with_context(
                        hit,
                        rest,
                        &code_slash_registry::ExpansionContext::default(),
                    );
                    // Record the raw invocation in history (so up-arrow
                    // recalls `/apply`, not the expansion), then submit the
                    // expanded prompt. run_loop echoes the expanded prompt
                    // as the User line — i.e. what's actually sent.
                    if app.history.back().is_none_or(|last| last != input) {
                        app.history.push_back(input.to_string());
                        if app.history.len() > HISTORY_MAX_LIMIT {
                            app.history.pop_front();
                        }
                    }
                    app.history_idx = None;
                    app.stashed_live = None;
                    Some(Action::Submit(expanded))
                }
                CustomResolveResult::Ambiguous(names) => {
                    app.transcript.push(TranscriptEntry::System(format!(
                        "ambiguous command: {cmd} — {}",
                        names.join(", "),
                    )));
                    app.transcript.push(TranscriptEntry::Blank);
                    None
                }
                CustomResolveResult::NotFound => {
                    // Tier 4 — only fires after tiers 1-3 all miss.
                    app.transcript.push(TranscriptEntry::System(format!(
                        "unknown command: {cmd}  (try /help)",
                    )));
                    None
                }
            }
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
/// Drain the first queued message, if any, into a new turn.
///
/// Extracted from the `AgentMsg::TurnEnd` handler so the queued-drain
/// logic is unit-testable in isolation. Returns `true` if a queued
/// message was submitted (and the app transitioned back to
/// `Phase::Streaming`), `false` if the queue was empty.
fn drain_queued(app: &mut App, cmd_tx: &mpsc::Sender<Cmd>) -> bool {
    if app.queued.is_empty() {
        return false;
    }
    let next = app.queued.remove(0);
    app.transcript.push(TranscriptEntry::User(next.clone()));
    app.transcript.push(TranscriptEntry::Blank);
    let _ = cmd_tx.send(Cmd::Prompt(next));
    app.phase = Phase::Streaming;
    app.turn_started = Some(Instant::now());
    app.output_chars = 0;
    app.current_tool = None;
    app.current_tool_detail = String::new();
    app.spinner_label = "thinking…";
    true
}

/// Render a pi tool-result `Value` (as packed by `translate_event`'s
/// `ToolExecutionEnd` arm via `serde_json::to_value(result)`) into a short,
/// readable string for the transcript `ToolResult` line. Extracts the text
/// from a Text content block when present; otherwise emits compact JSON so
/// the line stays one-ish line and cheap to scan. Trailing whitespace is
/// trimmed and the result is capped to keep the transcript compact.
fn render_tool_output(value: &serde_json::Value) -> String {
    // pi tool results commonly carry their payload under a `content` array
    // of content blocks (mirroring the assistant message shape). Pull the
    // text out of the first Text block.
    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
        let text: String = content
            .iter()
            .filter_map(|c| {
                (c.get("type").and_then(|t| t.as_str())? == "text").then_some(())?;
                c.get("text").and_then(|t| t.as_str()).map(String::from)
            })
            .collect::<Vec<_>>()
            .join("");
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            return compact(&trimmed);
        }
    }
    // Bare string result.
    if let Some(s) = value.as_str() {
        let trimmed = s.trim().to_string();
        if !trimmed.is_empty() {
            return compact(&trimmed);
        }
    }
    // Fall back to compact JSON for objects/arrays, skipping Null/empty.
    if value.is_null() {
        return String::new();
    }
    compact(&serde_json::to_string(value).unwrap_or_default())
}

/// Best-effort error sniff for a pi tool-result `Value`: looks for an
/// `error`/`is_error` field at the top level or inside a `content` block.
/// Returns false when nothing error-shaped is found (the common success
/// path), so non-error results render as normal dim lines.
fn is_tool_error(value: &serde_json::Value) -> bool {
    if let Some(b) = value.get("is_error").and_then(|v| v.as_bool()) {
        return b;
    }
    if let Some(b) = value.get("isError").and_then(|v| v.as_bool()) {
        return b;
    }
    if value.get("error").is_some() && !value.get("error").unwrap().is_null() {
        return true;
    }
    // Content blocks may carry their own type="error" marker.
    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
        for c in content {
            if c.get("type").and_then(|t| t.as_str()) == Some("error") {
                return true;
            }
            if let Some(b) = c.get("is_error").and_then(|v| v.as_bool()) {
                if b {
                    return true;
                }
            }
        }
    }
    false
}

/// Cap a rendered output string to a compact length, collapsing internal
/// newlines so the transcript line stays scannable. Matches the spirit of
/// `code_tool_preview`'s MAX field lengths.
fn compact(s: &str) -> String {
    const MAX: usize = 200;
    let collapsed: String = s.chars().fold(String::new(), |mut acc, c| {
        if c == '\n' || c == '\r' {
            if !acc.ends_with(' ') {
                acc.push(' ');
            }
        } else {
            acc.push(c);
        }
        acc
    });
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX).collect();
    out.push('…');
    out
}

/// Auto-tail the open agent overlay: if the user is viewing `agent_name`
/// and hasn't scrolled away (i.e. `follow` is true), reset the overlay's
/// scroll to the bottom so new output stays in view. Called from each
/// subagent transcript arm of [`handle_agent_msg`]. No-op when no overlay
/// is open or it's showing a different agent — and no-op when the user
/// scrolled up (follow == false), so we don't yank them back down.
fn overlay_auto_tail(app: &mut App, agent_name: &str) {
    if let Some(overlay) = &mut app.agent_overlay {
        if overlay.agent_name == agent_name && overlay.follow {
            overlay.scroll = 0;
        }
    }
}

fn handle_agent_msg(app: &mut App, msg: AgentMsg, cmd_tx: &mpsc::Sender<Cmd>) {
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
        AgentMsg::ToolEnd {
            tool_name,
            output,
            ..
        } => {
            // Stop dropping tool output: render a dim ToolResult line below
            // the tool marker. `render_tool_output` extracts a readable
            // short form from the pi result Value; `is_tool_error` is a
            // best-effort error sniff.
            let rendered = render_tool_output(&output);
            if !rendered.is_empty() {
                app.transcript.push(TranscriptEntry::ToolResult {
                    name: tool_name.clone(),
                    output: rendered,
                    is_error: is_tool_error(&output),
                });
            }
            app.current_tool = None;
            app.current_tool_detail = String::new();
            app.spinner_label = "thinking…";
        }
        AgentMsg::TurnEnd { elapsed_secs } => {
            // Dim end-of-turn stop line ("● done · 12.3k in · 1.2k out · 42s"),
            // reusing the legacy `stop_line_text` so the verb + figures
            // match the REPL exactly. The stop reason + ctx-in + out are
            // stashed by the Usage handler; `.take()` so a turn-end without
            // a preceding Usage (e.g. an error path) simply omits the line.
            if let Some((reason, ctx_in, out)) = app.last_usage.take() {
                app.transcript.push(TranscriptEntry::System(stop_line_text(
                    &reason,
                    ctx_in,
                    out,
                    elapsed_secs,
                )));
            }

            app.phase = Phase::Idle;
            app.turn_started = None;
            app.current_tool = None;
            app.current_tool_detail = String::new();
            app.transcript.push(TranscriptEntry::Blank);
            app.scroll = 0; // auto-scroll to bottom

            // If there are queued messages, submit the first one.
            drain_queued(app, cmd_tx);
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
            output_tokens,
            context_window,
            model_label,
            cost_total,
            stop_reason,
        } => {
            app.bar.input_tokens = input_tokens;
            app.bar.context_window = context_window;
            app.bar.model_label = model_label;
            // Session-cost accumulator: `estimated_cost` was previously
            // declared but never assigned (the core bug this fixes).
            // Each turn's `cost_total` is added to the running session
            // total. NaN is guarded by clamping the addend to >= 0 — pi's
            // pricing table yields finite values, but a missing entry can
            // surface as 0.0, never NaN, so this is belt-and-suspenders.
            let addend = if cost_total.is_nan() || cost_total < 0.0 {
                0.0
            } else {
                cost_total
            };
            let prev = app.bar.estimated_cost.unwrap_or(0.0);
            app.bar.estimated_cost = Some(prev + addend);
            // Stash for the TurnEnd stop line. `.take()`n there, so a
            // turn-end without a preceding Usage (error path) just omits
            // the line rather than rendering stale numbers.
            app.last_usage = Some((stop_reason, input_tokens, output_tokens));
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
        AgentMsg::SubagentText { agent_name, text } => {
            // Append to last subagent text from same agent, or create new entry.
            if let Some(TranscriptEntry::SubagentText {
                agent_name: name,
                text: existing,
            }) = app.transcript.last_mut()
            {
                if name == &agent_name {
                    existing.push_str(&text);
                    app.scroll = 0;
                    overlay_auto_tail(app, &agent_name);
                    return;
                }
            }
            app.transcript.push(TranscriptEntry::SubagentText {
                agent_name: agent_name.clone(),
                text,
            });
            app.scroll = 0; // auto-scroll to bottom
            overlay_auto_tail(app, &agent_name);
        }
        AgentMsg::SubagentToolStart {
            agent_name,
            tool_name,
            args,
        } => {
            // Store args on the entry; the scrollback renderer calls
            // `tool_preview` (reused, not duplicated) to format the marker.
            app.transcript.push(TranscriptEntry::SubagentTool {
                agent_name: agent_name.clone(),
                tool_name,
                args,
            });
            app.scroll = 0; // auto-scroll to bottom
            overlay_auto_tail(app, &agent_name);
        }
        AgentMsg::SubagentToolEnd {
            agent_name,
            tool_name,
            output,
            is_error,
        } => {
            // Reuse ToolResult for a dim per-tool result line, prefixing the
            // tool name with the agent so the line reads "{agent} · {tool}".
            // Keeps a single result-rendering path (ToolResult) and avoids a
            // near-duplicate SubagentToolResult variant. An empty/whitespace
            // result emits no line, matching the prior implicit-end behavior.
            if output.trim().is_empty() {
                return;
            }
            let name = format!("{agent_name} · {tool_name}");
            let rendered = render_tool_output(&serde_json::Value::String(output));
            app.transcript.push(TranscriptEntry::ToolResult {
                name,
                output: rendered,
                is_error,
            });
            app.scroll = 0; // auto-scroll to bottom
            overlay_auto_tail(app, &agent_name);
        }
        AgentMsg::SubagentEnd { agent_name, outcome } => {
            app.transcript.push(TranscriptEntry::SubagentEnd {
                agent_name: agent_name.clone(),
                outcome,
            });
            app.transcript.push(TranscriptEntry::Blank);
            app.scroll = 0; // auto-scroll to bottom
            overlay_auto_tail(app, &agent_name);
        }
        AgentMsg::Error(e) => {
            app.transcript.push(TranscriptEntry::System(format!("error: {e}")));
            app.scroll = 0; // auto-scroll to bottom
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code_team::{
        AgentCapability, AgentColor, AgentKind, AgentRegistration, AgentStatus,
    };

    fn reg_teammate(team: &str) -> AgentRegistration {
        AgentRegistration {
            name: format!("{team}-agent"),
            kind: AgentKind::Teammate { team: team.to_string() },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        }
    }

    fn register_with_status(
        registry: &AgentRegistry,
        team: &str,
        status: AgentStatus,
    ) -> Arc<crate::commands::code_team::AgentHandle> {
        let h = registry.register(reg_teammate(team));
        h.set_status(status);
        h
    }

    /// Build a minimal `App` for testing pure state transitions. Mirrors
    /// the construction in `run` but trimmed to the fields the tested
    /// helpers actually mutate.
    fn test_app() -> App {
        App {
            phase: Phase::Idle,
            mode: ModeFlag::new(Mode::Normal),
            transcript: Vec::new(),
            scroll: 0,
            spinner_idx: 0,
            turn_started: None,
            output_chars: 0,
            spinner_label: "thinking…",
            current_tool: None,
            current_tool_detail: String::new(),
            queued: Vec::new(),
            textarea: TextArea::default(),
            history: VecDeque::new(),
            history_idx: None,
            stashed_live: None,
            approval: None,
            ask: None,
            focus: Focus::default(),
            agent_selection: 0,
            agent_overlay: None,
            registry: AgentRegistry::new(),
            notified_teams: std::collections::HashSet::new(),
            cfg: Arc::new(LibertaiConfig::default()),
            bar: BarStatus {
                model_label: "openai/gpt-4o".to_string(),
                ..Default::default()
            },
            last_usage: None,
            last_shell_command: None,
            pending_shell_contexts: Vec::new(),
            bash_command_wrapper: None,
            custom_commands: Vec::new(),
        }
    }

    #[test]
    fn active_team_set_lists_only_active_teams() {
        let registry = AgentRegistry::new();
        register_with_status(&registry, "alpha", AgentStatus::Working);
        register_with_status(&registry, "alpha", AgentStatus::Completed);
        register_with_status(&registry, "beta", AgentStatus::Completed);
        let map = active_team_set(&registry.snapshot());
        assert!(map.contains_key("alpha"));
        assert!(!map.contains_key("beta"));
    }

    #[test]
    fn completed_team_is_detected_on_transition() {
        // Simulate the two snapshots poll_agent_status compares: before
        // (team active) and after the last member is reaped (team idle).
        let registry = AgentRegistry::new();
        let a = register_with_status(&registry, "alpha", AgentStatus::Working);
        let prev = active_team_set(&registry.snapshot());
        a.set_status(AgentStatus::Completed);
        let after = active_team_set(&registry.snapshot());
        let completed: Vec<String> =
            prev.keys().filter(|t| !after.contains_key(*t)).cloned().collect();
        assert_eq!(completed, vec!["alpha".to_string()]);
    }

    #[test]
    fn partially_active_team_is_not_completed() {
        let registry = AgentRegistry::new();
        let a = register_with_status(&registry, "alpha", AgentStatus::Working);
        register_with_status(&registry, "alpha", AgentStatus::Working);
        let prev = active_team_set(&registry.snapshot());
        // Only one of two finishes — team still active.
        a.set_status(AgentStatus::Completed);
        let after = active_team_set(&registry.snapshot());
        let completed: Vec<String> =
            prev.keys().filter(|t| !after.contains_key(*t)).cloned().collect();
        assert!(completed.is_empty());
    }

    // --- Category (1): queued drain ----------------------------------------

    #[test]
    fn drain_queued_sends_first_queued_prompt_and_clears_queue() {
        let mut app = test_app();
        app.queued = vec!["hello".to_string(), "world".to_string()];
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        assert!(drain_queued(&mut app, &cmd_tx));

        // One prompt was sent for the first queued message.
        let sent = cmd_rx.try_recv().expect("expected a Cmd::Prompt");
        match sent {
            Cmd::Prompt(p) => assert_eq!(p, "hello"),
            other => panic!("expected Cmd::Prompt, got {other:?}"),
        }
        // No extra commands.
        assert!(cmd_rx.try_recv().is_err(), "no extra command expected");

        // The first queued message was removed.
        assert_eq!(app.queued, vec!["world".to_string()]);
        // App transitioned to a new streaming turn.
        assert_eq!(app.phase, Phase::Streaming);
        assert!(app.turn_started.is_some());
        assert_eq!(app.output_chars, 0);
        assert!(app.current_tool.is_none());
        assert_eq!(app.spinner_label, "thinking…");
        // The echoed prompt + a blank separator were pushed.
        let last_two = &app.transcript[app.transcript.len() - 2..];
        assert!(matches!(last_two[0], TranscriptEntry::User(ref s) if s == "hello"));
        assert!(matches!(last_two[1], TranscriptEntry::Blank));
    }

    #[test]
    fn drain_queued_empty_returns_false_and_is_noop() {
        let mut app = test_app();
        app.phase = Phase::Idle;
        app.queued = Vec::new();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        assert!(!drain_queued(&mut app, &cmd_tx));

        // Nothing sent, nothing appended.
        assert!(cmd_rx.try_recv().is_err());
        assert!(app.transcript.is_empty());
        assert_eq!(app.phase, Phase::Idle);
        assert!(app.turn_started.is_none());
    }

    // --- Category (2): handle_agent_msg state transitions ------------------

    #[test]
    fn handle_agent_msg_textdelta_appends_to_transcript() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(&mut app, AgentMsg::TextDelta("hello".into()), &cmd_tx);
        // First delta creates a new Assistant entry.
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Assistant(ref s)) if s == "hello"
        ));
        assert_eq!(app.output_chars, 5);
        assert_eq!(app.scroll, 0);

        // A second delta appends to the same entry.
        handle_agent_msg(&mut app, AgentMsg::TextDelta(" world".into()), &cmd_tx);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Assistant(ref s)) if s == "hello world"
        ));
        assert_eq!(app.output_chars, 11);
    }

    #[test]
    fn handle_agent_msg_toolstart_sets_current_tool() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(
            &mut app,
            AgentMsg::ToolStart {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({ "command": "echo hi" }),
            },
            &cmd_tx,
        );

        assert_eq!(app.current_tool.as_deref(), Some("bash"));
        assert_eq!(app.spinner_label, "working…");
        assert_eq!(app.scroll, 0);
        // A Tool transcript entry was pushed for the started tool.
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Tool { ref name, .. }) if name == "bash"
        ));
    }

    #[test]
    fn handle_agent_msg_toolend_clears_current_tool() {
        let mut app = test_app();
        app.current_tool = Some("bash".into());
        app.current_tool_detail = "echo hi".into();
        app.spinner_label = "working…";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(
            &mut app,
            AgentMsg::ToolEnd {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                output: serde_json::Value::Null,
            },
            &cmd_tx,
        );

        assert!(app.current_tool.is_none());
        assert!(app.current_tool_detail.is_empty());
        assert_eq!(app.spinner_label, "thinking…");
    }

    #[test]
    fn handle_agent_msg_turnend_idles_when_queue_empty() {
        let mut app = test_app();
        app.phase = Phase::Streaming;
        app.turn_started = Some(Instant::now());
        app.current_tool = Some("bash".into());
        app.queued = Vec::new();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(&mut app, AgentMsg::TurnEnd { elapsed_secs: 5 }, &cmd_tx);

        assert_eq!(app.phase, Phase::Idle);
        assert!(app.turn_started.is_none());
        assert!(app.current_tool.is_none());
        assert!(cmd_rx.try_recv().is_err(), "no queued prompt to send");
        // A trailing Blank separator is pushed on turn end.
        assert!(matches!(app.transcript.last(), Some(TranscriptEntry::Blank)));
    }

    #[test]
    fn handle_agent_msg_turnend_drains_queue_into_next_turn() {
        let mut app = test_app();
        app.phase = Phase::Streaming;
        app.queued = vec!["next-prompt".to_string()];
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(&mut app, AgentMsg::TurnEnd { elapsed_secs: 1 }, &cmd_tx);

        // The queued message is promoted to a new streaming turn.
        assert_eq!(app.phase, Phase::Streaming);
        assert!(app.turn_started.is_some());
        assert!(app.queued.is_empty());
        match cmd_rx.try_recv() {
            Ok(Cmd::Prompt(p)) => assert_eq!(p, "next-prompt"),
            other => panic!("expected Cmd::Prompt, got {other:?}"),
        }
        // The echoed prompt appears in the transcript (after the turn-end
        // Blank separator).
        assert!(app
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::User(s) if s == "next-prompt")));
    }

    // --- Category (3): handle_slash_command dispatch -----------------------

    #[test]
    fn slash_help_pushes_help_text() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/help", &cmd_tx);

        assert!(action.is_none(), "/help does not Quit");
        // Help text mentions the available commands.
        assert!(app
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::System(ref s) if s.contains("Commands:"))));
    }

    #[test]
    fn slash_clear_requests_clear_and_returns_clear_action() {
        let mut app = test_app();
        app.transcript.push(TranscriptEntry::User("old".into()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/clear", &cmd_tx);

        // /clear returns the ClearTranscript action and sends Cmd::Clear.
        assert!(matches!(action, Some(Action::ClearTranscript)));
        match cmd_rx.try_recv() {
            Ok(Cmd::Clear) => {}
            other => panic!("expected Cmd::Clear, got {other:?}"),
        }
        // /clear itself does not mutate the transcript; run_loop clears it.
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(ref s)) if s == "old"
        ));
    }

    #[test]
    fn slash_model_show_reports_current_model() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/model", &cmd_tx);

        assert!(action.is_none());
        assert!(app
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::System(ref s) if s.contains("openai/gpt-4o"))));
    }

    #[test]
    fn slash_model_set_sends_setmodel_command() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/model anthropic/claude-3.5", &cmd_tx);

        assert!(action.is_none());
        match cmd_rx.try_recv() {
            Ok(Cmd::SetModel(provider, model_id)) => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model_id, "claude-3.5");
            }
            other => panic!("expected Cmd::SetModel, got {other:?}"),
        }
        // No further command on the channel.
        assert!(cmd_rx.try_recv().is_err());
        assert!(app
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::System(ref s) if s.contains("setting model"))));
    }

    #[test]
    fn slash_unknown_reports_error() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/nope", &cmd_tx);

        assert!(action.is_none());
        assert!(app
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::System(ref s) if s.contains("unknown command: /nope"))));
    }

    #[test]
    fn slash_quit_returns_quit_action() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/quit", &cmd_tx);

        assert!(matches!(action, Some(Action::Quit)));
    }

    // --- M3a: router dispatch for read-only / background-thread commands ----

    // `/model list` is a Tier 2 read-only command: the catalog fetch runs on
    // the background thread (network), so on the main thread we only push a
    // "listing models…" placeholder System entry and route a
    // `Cmd::RunReadOnly(BgCommand::ModelList)`. Crucially it must NOT send a
    // `Cmd::Prompt` (it's not a prompt submission).
    #[test]
    fn slash_model_list_pushes_placeholder_and_routes_to_bg() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/model list", &cmd_tx);

        assert!(action.is_none(), "/model list does not return an action");
        // A non-empty System entry was pushed (the listing placeholder).
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if !s.is_empty()
        )));
        // A read-only command was routed to the background thread…
        match cmd_rx.try_recv() {
            Ok(Cmd::RunReadOnly(BgCommand::ModelList { scoped_patterns })) => {
                assert!(
                    scoped_patterns.is_empty(),
                    "no patterns expected for bare `/model list`, got {scoped_patterns:?}"
                );
            }
            other => panic!("expected Cmd::RunReadOnly(ModelList), got {other:?}"),
        }
        // …and, importantly, NO Cmd::Prompt was sent.
        assert!(cmd_rx.try_recv().is_err(), "no further command expected");
    }

    // `/skills list` runs the synchronous read-only skills inventory via the
    // router adapter and pushes the rendered text as a (non-empty) System
    // entry. Even in a test cwd with no skills the adapter returns a
    // non-empty "skills: none active…" body, so the entry is never blank.
    #[test]
    fn slash_skills_list_pushes_nonempty_system_entry() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/skills list", &cmd_tx);

        assert!(action.is_none());
        // A non-empty System entry was pushed — either the skills body (which
        // starts with "skills") or, on an inventory error, the adapter's
        // "/skills: <error>" line. Both are non-empty, so the requirement is
        // simply that some System entry landed (and no Cmd::Prompt was sent).
        let skills_entry = app
            .transcript
            .iter()
            .find_map(|e| match e {
                TranscriptEntry::System(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
            .expect("a non-empty /skills System entry was pushed");
        assert!(!skills_entry.is_empty());
        // /skills list is synchronous — it sends no command to the bg thread.
        assert!(cmd_rx.try_recv().is_err(), "/skills list sends no Cmd");
    }

    // --- M3a: pending_shell_contexts prefix the next prompt -----------------
    //
    // `!echo hi` runs a shell escape synchronously: it renders transcript
    // lines and stashes the prompt-context block in
    // `app.pending_shell_contexts`. The next real prompt is then prefixed
    // with that context (via `apply_pending_shell_context`) and the contexts
    // are cleared — exactly the run_loop Submit path. We drive the shell
    // escape through the real `handle_shell_escape` helper, then assert the
    // prefix-apply directly (the pure helper `handle_key` calls).

    #[test]
    fn shell_escape_stashes_context_then_prefixes_next_prompt() {
        let mut app = test_app();

        // `!echo hi` runs the shell escape and stashes the captured context.
        handle_shell_escape(&mut app, "!echo hi");
        assert_eq!(
            app.last_shell_command.as_deref(),
            Some("echo hi"),
            "last shell command should be recorded for `!!`"
        );
        assert!(
            !app.pending_shell_contexts.is_empty(),
            "pending_shell_contexts should be populated after a successful `!cmd`"
        );
        // The stashed context carries the captured stdout.
        let stashed = app.pending_shell_contexts.join("\n");
        assert!(stashed.contains("hi"), "stashed context missing stdout: {stashed}");

        // The next real prompt is prefixed with the stashed context.
        let prefixed = apply_pending_shell_context(&app.pending_shell_contexts, "summarize this");
        assert!(
            prefixed.starts_with("Context from local shell escape commands"),
            "expected the shell-escape context header, got: {prefixed}"
        );
        assert!(
            prefixed.contains("User prompt:\nsummarize this"),
            "expected the user prompt appended after the context, got: {prefixed}"
        );

        // After applying, run_loop clears the contexts — verify the clear is a
        // noop once empty (the next prompt passes through unmodified).
        app.pending_shell_contexts.clear();
        let passthrough = apply_pending_shell_context(&app.pending_shell_contexts, "next prompt");
        assert_eq!(passthrough, "next prompt", "empty contexts should pass the prompt through");
    }

    // `!!` repeats the last shell command: after `!echo hi`, a bare `!!` (rest
    // `!`, last = "echo hi") re-runs it. This exercises the
    // `shell_escape_command` repeat path through `handle_shell_escape`.
    #[test]
    fn shell_escape_repeat_runs_last_command() {
        let mut app = test_app();
        handle_shell_escape(&mut app, "!echo first");
        let first = app.last_shell_command.clone();
        assert_eq!(first.as_deref(), Some("echo first"));

        // `!!` → rest is `!` with the recorded last command.
        handle_shell_escape(&mut app, "!!");
        // The repeated command is what `!!` re-ran (re-recorded as last).
        assert_eq!(app.last_shell_command.as_deref(), Some("echo first"));
        // Two contexts now stashed: one per run.
        assert_eq!(app.pending_shell_contexts.len(), 2);
    }

    // --- M2: cost / context / template / stop-line / statusline / output-style

    // (1) Cost accumulation: two AgentMsg::Usage updates accumulate into
    // app.bar.estimated_cost. Driven through the real handle_agent_msg
    // Usage handler (no extraction needed — the handler is already a
    // standalone free function, so we call it directly).
    #[test]
    fn usage_accumulates_cost_across_turns() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let mk = |cost: f64, stop: StopReason| AgentMsg::Usage {
            input_tokens: 1_000,
            output_tokens: 200,
            context_window: context_window_for("openai", "gpt-4o"),
            model_label: "openai/gpt-4o".to_string(),
            cost_total: cost,
            stop_reason: stop,
        };

        handle_agent_msg(&mut app, mk(0.12, StopReason::Stop), &cmd_tx);
        handle_agent_msg(&mut app, mk(0.34, StopReason::Stop), &cmd_tx);

        assert_eq!(app.bar.estimated_cost, Some(0.46));
        // The latest Usage also refreshes the bar's token/window fields.
        assert_eq!(app.bar.input_tokens, 1_000);
        assert_eq!(app.bar.model_label, "openai/gpt-4o");
    }

    // NaN / negative addends are clamped to 0 by the Usage handler so the
    // session total never goes non-finite or negative.
    #[test]
    fn usage_clamps_nan_and_negative_cost() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(
            &mut app,
            AgentMsg::Usage {
                input_tokens: 0,
                output_tokens: 0,
                context_window: 0,
                model_label: "openai/gpt-4o".to_string(),
                cost_total: f64::NAN,
                stop_reason: StopReason::Stop,
            },
            &cmd_tx,
        );
        assert_eq!(app.bar.estimated_cost, Some(0.0));

        handle_agent_msg(
            &mut app,
            AgentMsg::Usage {
                input_tokens: 0,
                output_tokens: 0,
                context_window: 0,
                model_label: "openai/gpt-4o".to_string(),
                cost_total: -5.0,
                stop_reason: StopReason::Stop,
            },
            &cmd_tx,
        );
        assert_eq!(app.bar.estimated_cost, Some(0.0));
    }

    // (2) context_percent via the pub(crate) helper: a few (input, window)
    // pairs round correctly. window 0 → 0% (the helper's guard).
    #[test]
    fn context_percent_rounds_known_pairs() {
        // 50% of 100k.
        assert_eq!(context_percent(50_000, 100_000), 50);
        // 0% when the window is unknown (guard against divide-by-zero).
        assert_eq!(context_percent(1_000, 0), 0);
        // Clamps to 100% when input exceeds the window.
        assert_eq!(context_percent(200_000, 100_000), 100);
        // Rounding: 33.3% of 100k rounds to 33.
        assert_eq!(context_percent(33_300, 100_000), 33);
    }

    // context_window_for is hermetic under cfg!(test) — always the 32k
    // fallback — so context-% assertions can lean on a known window.
    #[test]
    fn context_window_for_is_hermetic_under_test() {
        let window = context_window_for("openai", "gpt-4o");
        assert_eq!(window, 32_768);
        // ~3% of the hermetic 32k window.
        assert_eq!(context_percent(1_000, window), 3);
    }

    // (3) status-line template expansion: build a legacy BarStatus and
    // call expand_status_line_template so the footer reuse path is
    // guarded. Uses full crate paths because expand_status_line_template
    // is pub(crate) in code_ui and not re-exported by app.rs.
    #[test]
    fn expand_status_line_template_substitutes_tokens() {
        use crate::commands::code_ui::BarStatus as LegacyBarStatus;
        use crate::commands::code_ui::expand_status_line_template;

        let legacy = LegacyBarStatus {
            model_label: "openai/gpt-4o".to_string(),
            input_tokens: 50_000,
            context_window: 100_000,
            output_style: Some("concise".to_string()),
            status_line_template: String::new(),
            status_line_command: String::new(),
            estimated_cost: Some(1.50),
        };

        let rendered =
            expand_status_line_template("{model} {ctx} {cost}", &legacy, Mode::Normal)
                .expect("non-empty template renders");
        // {model} → part after the slash.
        assert!(rendered.contains("gpt-4o"), "model token: {rendered:?}");
        // {ctx} → "50%".
        assert!(rendered.contains("50%"), "ctx token: {rendered:?}");
        // {cost} → "~$1.50" (the legacy expander prefixes ~ and uses dollar()).
        assert!(rendered.contains("~$1.50"), "cost token: {rendered:?}");
    }

    // An empty template yields None (the footer falls back to default chips).
    #[test]
    fn expand_status_line_template_empty_returns_none() {
        use crate::commands::code_ui::BarStatus as LegacyBarStatus;
        use crate::commands::code_ui::expand_status_line_template;

        let legacy = LegacyBarStatus {
            model_label: "openai/gpt-4o".to_string(),
            ..Default::default()
        };
        assert!(expand_status_line_template("", &legacy, Mode::Normal).is_none());
    }

    // (4) stop_line_text formatting: the rendered stop line contains the
    // expected verb + the humanized in/out token strings + the elapsed
    // figure. Reuses the pub(crate) helper (imported at the top of app.rs).
    #[test]
    fn stop_line_text_contains_verb_tokens_and_elapsed() {
        let line = stop_line_text(&StopReason::Stop, 18_324, 272, 41);
        // Verb.
        assert!(line.contains("● done"), "verb: {line:?}");
        // In tokens humanized (>=1k → "18.3k").
        assert!(line.contains("18.3k in"), "in tokens: {line:?}");
        // Out tokens (<1k → plain "272").
        assert!(line.contains("272 out"), "out tokens: {line:?}");
        // Elapsed (<60s → "41s").
        assert!(line.ends_with("41s"), "elapsed: {line:?}");
    }

    #[test]
    fn stop_line_text_handles_minutes_and_length_reason() {
        let line = stop_line_text(&StopReason::Length, 900, 1_200, 128);
        assert!(line.contains("● max tokens"), "verb: {line:?}");
        // Sub-1k in stays plain; >=1k out humanizes.
        assert!(line.contains("900 in"), "in tokens: {line:?}");
        assert!(line.contains("1.2k out"), "out tokens: {line:?}");
        // >=60s renders as m:ss.
        assert!(line.ends_with("2m08s"), "elapsed: {line:?}");
    }

    // (5) /statusline: with an arg it stores the template; with no arg it
    // reports the stored template.
    #[test]
    fn slash_statusline_sets_template() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/statusline {model} {ctx}", &cmd_tx);

        assert!(action.is_none());
        assert_eq!(app.bar.status_line_template, "{model} {ctx}");
        // A confirmation System entry was pushed.
        assert!(app
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::System(ref s) if s == "statusline set")));
    }

    #[test]
    fn slash_statusline_no_arg_reports_template() {
        let mut app = test_app();
        app.bar.status_line_template = "{model}".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/statusline", &cmd_tx);

        assert!(action.is_none());
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "statusline: {model}"
        )));
        // Template is left untouched.
        assert_eq!(app.bar.status_line_template, "{model}");
    }

    #[test]
    fn slash_statusline_no_arg_reports_unset() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/statusline", &cmd_tx);

        assert!(action.is_none());
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "statusline: no statusline template set"
        )));
    }

    // (6) /output-style: a known builtin name sets app.bar.output_style; an
    // unknown name pushes an error System entry and leaves output_style
    // unchanged. "review" is a builtin (always resolves regardless of disk
    // state), so the known-name path is hermetic.
    #[test]
    fn slash_output_style_known_name_sets_style() {
        // Confirm "review" resolves from builtins before relying on it.
        assert!(crate::commands::code_output_style::find_style("review", None).is_some());

        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/output-style review", &cmd_tx);

        assert!(action.is_none());
        assert_eq!(app.bar.output_style.as_deref(), Some("review"));
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "→ output style: review"
        )));
    }

    #[test]
    fn slash_output_style_default_clears_override() {
        let mut app = test_app();
        app.bar.output_style = Some("review".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/output-style default", &cmd_tx);

        assert!(action.is_none());
        assert!(app.bar.output_style.is_none(), "default clears the override");
    }

    #[test]
    fn slash_output_style_unknown_name_pushes_error_and_leaves_style() {
        let mut app = test_app();
        app.bar.output_style = Some("review".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/output-style no-such-style", &cmd_tx);

        assert!(action.is_none());
        // The override is untouched.
        assert_eq!(app.bar.output_style.as_deref(), Some("review"));
        // An error System entry was pushed.
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.contains("unknown output style: no-such-style")
        )));
    }

    #[test]
    fn slash_output_style_no_arg_reports_current() {
        let mut app = test_app();
        app.bar.output_style = Some("concise".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/output-style", &cmd_tx);

        assert!(action.is_none());
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "output style: concise"
        )));
    }

    // /status surfaces the mocked git branch + cost (no real env / git
    // dependency — we set the fields directly). Guards the M2 /status
    // expansion that adds branch + cost chips.
    #[test]
    fn slash_status_includes_mocked_branch_and_cost() {
        let mut app = test_app();
        app.bar.git_branch = Some("main".to_string());
        app.bar.estimated_cost = Some(0.42);
        app.bar.input_tokens = 1_000;
        app.bar.context_window = 32_768;
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        let action = handle_slash_command(&mut app, "/status", &cmd_tx);

        assert!(action.is_none());
        let status_line = app
            .transcript
            .iter()
            .rev()
            .find_map(|e| match e {
                TranscriptEntry::System(s) => Some(s.as_str()),
                _ => None,
            })
            .expect("a /status System entry was pushed");
        assert!(status_line.contains("branch: main"), "branch: {status_line:?}");
        assert!(status_line.contains("cost: $0.42"), "cost: {status_line:?}");
        // ctx % derived from the (known) hermetic window.
        assert!(status_line.contains("ctx: 3%"), "ctx: {status_line:?}");
    }

    // --- M3b: agent / team spawn — pure parsing helpers ----------------------
    //
    // The seam is `build_team_invocation` / `build_agent_invocation`: they
    // produce the fully-resolved invocation WITHOUT spawning, so we can test
    // the parsing hermetically. The real spawn (spawn_team /
    // start_background_agent) is exercised only via the slash arms, which we
    // don't drive here (they spawn OS processes).

    #[test]
    fn app_provider_model_splits_label_on_slash() {
        let mut app = test_app();
        app.bar.model_label = "anthropic/claude-3.5".to_string();
        let (p, m) = app_provider_model(&app);
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-3.5");
    }

    #[test]
    fn app_provider_model_falls_back_to_config_defaults() {
        let mut app = test_app();
        // A label with no slash falls back to config defaults.
        app.bar.model_label = "gpt-4o".to_string();
        app.cfg = Arc::new(LibertaiConfig {
            default_code_provider: "openai".to_string(),
            default_code_model: "gpt-4o".to_string(),
            ..LibertaiConfig::default()
        });
        let (p, m) = app_provider_model(&app);
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
    }

    #[test]
    fn build_team_invocation_empty_is_usage_error() {
        let cwd = PathBuf::from(".");
        let err = build_team_invocation("", &cwd, "openai", "gpt-4o", Mode::Normal)
            .expect_err("empty rest should error");
        assert!(format!("{err:#}").contains("usage:"), "err: {err:#}");
    }

    #[test]
    fn build_team_invocation_quick_form_builds_single_teammate_manifest() {
        let cwd = PathBuf::from(".");
        let inv = build_team_invocation("refactor coder refactor the parser", &cwd, "openai", "gpt-4o", Mode::Normal)
            .expect("quick form parses");
        assert_eq!(inv.team_name, "refactor");
        assert_eq!(inv.manifest.teammates.len(), 1);
        let t = &inv.manifest.teammates[0];
        assert_eq!(t.name, "agent-1");
        assert_eq!(t.agent, "coder");
        assert_eq!(t.task, "refactor the parser");
        assert!(t.model.is_none());
    }

    #[test]
    fn build_team_invocation_quick_form_requires_task() {
        let cwd = PathBuf::from(".");
        // Only two tokens — interpreted as `<name> <manifest-path>`, not the
        // quick form. A nonexistent path → read error (not a usage error).
        let err = build_team_invocation("refactor coder", &cwd, "openai", "gpt-4o", Mode::Normal)
            .expect_err("two-token form with a missing path errors");
        assert!(format!("{err:#}").contains("reading manifest"), "err: {err:#}");
    }

    #[test]
    fn build_team_invocation_manifest_path_form_loads_file() {
        // Write a minimal manifest to a temp file and load it by explicit path.
        let dir = std::env::temp_dir();
        let manifest_path = dir.join(format!(
            "libertai-m3b-team-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &manifest_path,
            r#"
model = "glm-5.2"
[[teammate]]
name = "alice"
agent = "coder"
task = "Do the thing"
"#,
        )
        .expect("write temp manifest");
        let rel = manifest_path.to_string_lossy().to_string();
        let arg = format!("myteam {rel}");
        let inv = build_team_invocation(&arg, &dir, "openai", "gpt-4o", Mode::Normal)
            .expect("manifest-path form parses");
        assert_eq!(inv.team_name, "myteam");
        assert_eq!(inv.manifest.teammates.len(), 1);
        assert_eq!(inv.manifest.teammates[0].name, "alice");
        assert_eq!(inv.manifest.model.as_deref(), Some("glm-5.2"));
        let _ = std::fs::remove_file(&manifest_path);
    }

    #[test]
    fn build_agent_invocation_parses_name_and_task() {
        let cwd = PathBuf::from(".");
        let launch = build_agent_invocation("coder fix the parser", &cwd, "openai", "gpt-4o", Mode::AcceptEdits)
            .expect("agent parses");
        assert_eq!(launch.name, "coder");
        assert_eq!(launch.prompt, "fix the parser");
        assert_eq!(launch.provider, "openai");
        assert_eq!(launch.model, "gpt-4o");
        assert_eq!(launch.mode, Mode::AcceptEdits);
        assert_eq!(launch.cwd, cwd);
        // A plain /agent run carries the agent name + no team context.
        assert_eq!(launch.agent.as_deref(), Some("coder"));
        assert!(launch.team.is_none());
        assert!(launch.teammate_name.is_none());
    }

    #[test]
    fn build_agent_invocation_missing_task_is_usage_error() {
        let cwd = PathBuf::from(".");
        let err = build_agent_invocation("coder", &cwd, "openai", "gpt-4o", Mode::Normal)
            .expect_err("no task should error");
        assert!(format!("{err:#}").contains("usage:"), "err: {err:#}");
    }

    #[test]
    fn build_agent_invocation_empty_is_usage_error() {
        let cwd = PathBuf::from(".");
        let err = build_agent_invocation("   ", &cwd, "openai", "gpt-4o", Mode::Normal)
            .expect_err("empty rest should error");
        assert!(format!("{err:#}").contains("usage:"), "err: {err:#}");
    }

    // /agents renders the registry snapshot directly. Empty → "no agents.";
    // populated → a header + one line per agent. No Cmd is sent.
    #[test]
    fn slash_agents_empty_registry_reports_no_agents() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let action = handle_slash_command(&mut app, "/agents", &cmd_tx);
        assert!(action.is_none());
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "no agents."
        )));
        // /agents sends no command to the bg thread.
        assert!(cmd_rx.try_recv().is_err(), "/agents sends no Cmd");
    }

    #[test]
    fn slash_agents_lists_registered_agents() {
        let mut app = test_app();
        // Register one teammate + one background agent directly in the registry.
        let _ = app.registry.register(AgentRegistration {
            name: "alice".to_string(),
            kind: AgentKind::Teammate { team: "refactor".to_string() },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: "gpt-4o".to_string(),
            prompt_preview: "do work".to_string(),
            parent: None,
            pid: Some(4242),
            log_path: None,
        });
        let bg = app.registry.register(AgentRegistration {
            name: "coder".to_string(),
            kind: AgentKind::Background { pid: 99, run_id: String::new() },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: "gpt-4o".to_string(),
            prompt_preview: "fix parser".to_string(),
            parent: None,
            pid: Some(99),
            log_path: None,
        });
        bg.set_status(AgentStatus::Completed);

        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let action = handle_slash_command(&mut app, "/agents", &cmd_tx);
        assert!(action.is_none());

        // Header reports 2 agents.
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "agents (2):"
        )));
        // The teammate line names the team; the background line carries a pid.
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.contains("alice") && s.contains("team refactor") && s.contains("pid 4242")
        )));
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.contains("coder") && s.contains("pid 99") && s.contains("completed")
        )));
    }

    // /team with empty args pushes a usage error (no spawn attempted).
    #[test]
    fn slash_team_empty_args_pushes_usage_error() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let action = handle_slash_command(&mut app, "/team", &cmd_tx);
        assert!(action.is_none());
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.contains("usage:") && s.contains("team")
        )));
        // No spawn → no command sent.
        assert!(cmd_rx.try_recv().is_err(), "/team empty sends no Cmd");
    }

    // /agent with empty args pushes a usage error (no spawn attempted).
    #[test]
    fn slash_agent_empty_args_pushes_usage_error() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let action = handle_slash_command(&mut app, "/agent", &cmd_tx);
        assert!(action.is_none());
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.contains("usage:") && s.contains("agent")
        )));
        assert!(cmd_rx.try_recv().is_err(), "/agent empty sends no Cmd");
    }

    // /help lists the new agent/team commands.
    #[test]
    fn slash_help_lists_team_agent_agents() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let _ = handle_slash_command(&mut app, "/help", &cmd_tx);
        let help_line = app
            .transcript
            .iter()
            .find_map(|e| match e {
                TranscriptEntry::System(s) if s.starts_with("Commands:") => Some(s.clone()),
                _ => None,
            })
            .expect("a Commands: line was pushed");
        assert!(help_line.contains("/team"), "help: {help_line}");
        assert!(help_line.contains("/agent"), "help: {help_line}");
        assert!(help_line.contains("/agents"), "help: {help_line}");
    }

    // /agents populated via the shared `register_with_status` test helper
    // (the task's recommended seam). A single registered teammate renders
    // as the header + one line naming the agent, its status, and its team.
    #[test]
    fn slash_agents_lists_single_teammate_via_register_with_status() {
        let mut app = test_app();
        register_with_status(&app.registry, "refactor", AgentStatus::Working);

        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let action = handle_slash_command(&mut app, "/agents", &cmd_tx);
        assert!(action.is_none());

        // Header reports exactly one agent.
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s == "agents (1):"
        )));
        // The teammate line names the registered agent (reg_teammate builds
        // "{team}-agent"), carries the status label, and tags its team.
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s)
                if s.contains("refactor-agent") && s.contains("working") && s.contains("team refactor")
        )));
    }

    // /team with malformed args (a two-token form pointing at a missing
    // manifest path) pushes a System error message and does NOT spawn:
    // the registry stays empty and no Cmd is sent.
    #[test]
    fn slash_team_malformed_args_do_not_spawn() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        // Two tokens where the second is a nonexistent path → the parser
        // reaches read_to_string and bails before any spawn.
        let action = handle_slash_command(&mut app, "/team myteam /no/such/manifest.toml", &cmd_tx);
        assert!(action.is_none());
        // A System error was pushed (prefixed with "team:").
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.starts_with("team:") && s.contains("reading manifest")
        )));
        // No spawn: the registry is still empty and no Cmd was sent.
        assert!(app.registry.snapshot().is_empty(), "registry must stay empty on a failed /team");
        assert!(cmd_rx.try_recv().is_err(), "malformed /team sends no Cmd");
    }

    // /agent with malformed args (an agent name but no task) pushes a System
    // error message and does NOT spawn: the registry stays empty and no Cmd
    // is sent.
    #[test]
    fn slash_agent_malformed_args_do_not_spawn() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        // A single token parses as the agent name with an empty task → the
        // parser bails at the usage check before any spawn.
        let action = handle_slash_command(&mut app, "/agent coder", &cmd_tx);
        assert!(action.is_none());
        // A System error was pushed (prefixed with "agent:").
        assert!(app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::System(ref s) if s.starts_with("agent:") && s.contains("usage:")
        )));
        // No spawn: the registry is still empty and no Cmd was sent.
        assert!(app.registry.snapshot().is_empty(), "registry must stay empty on a failed /agent");
        assert!(cmd_rx.try_recv().is_err(), "malformed /agent sends no Cmd");
    }

    // --- M5a: subagent / tool-result transcript-data path ------------------
    //
    // The seam is `translate_event` (pi AgentEvent → AgentMsg) plus the
    // `handle_agent_msg` arms that push the new transcript variants. We
    // fabricate the pi `AgentEvent::ToolExecutionUpdate` exactly the way
    // `code_task.rs`'s `render_child` packs it (details.kind + content
    // blocks), so the assertions exercise the real reduction path.

    use pi::model::{ContentBlock, TextContent};
    use pi::sdk::{AgentEvent, ToolOutput};

    /// Build a `ToolExecutionUpdate` mirroring code_task.rs's `render_child`
    /// packing: `partial_result.content` carries the payload blocks and
    /// `partial_result.details` carries the `kind` + per-kind fields.
    fn child_update(content: Vec<ContentBlock>, details: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: "child-tc-1".to_string(),
            tool_name: "task".to_string(),
            args: serde_json::Value::Null,
            partial_result: ToolOutput {
                content,
                details: Some(details),
                is_error: false,
            },
        }
    }

    // (1a) subagent_tool_start → AgentMsg::SubagentToolStart carrying args.
    #[test]
    fn translate_event_subagent_tool_start_carries_args() {
        let args = serde_json::json!({ "command": "echo hi", "cwd": "." });
        let event = child_update(
            vec![ContentBlock::Text(TextContent::new("bash"))],
            serde_json::json!({
                "kind": "subagent_tool_start",
                "agent": "reviewer",
                "tool": "bash",
                "args": args.clone(),
            }),
        );
        match translate_event(&event).expect("subagent_tool_start translates") {
            AgentMsg::SubagentToolStart {
                agent_name,
                tool_name,
                args: got_args,
            } => {
                assert_eq!(agent_name, "reviewer");
                assert_eq!(tool_name, "bash");
                assert_eq!(got_args, args, "args echoed from details.args");
            }
            other => panic!("expected SubagentToolStart, got {other:?}"),
        }
    }

    // (1b) subagent_tool_end with isError=true → SubagentToolEnd {
    // output, is_error: true }. The joined Text content becomes `output`.
    #[test]
    fn translate_event_subagent_tool_end_error_carries_output() {
        let event = child_update(
            vec![ContentBlock::Text(TextContent::new("command failed: exit 1"))],
            serde_json::json!({
                "kind": "subagent_tool_end",
                "agent": "coder",
                "tool": "bash",
                "toolCallId": "child-tc-1",
                "isError": true,
            }),
        );
        match translate_event(&event).expect("subagent_tool_end translates") {
            AgentMsg::SubagentToolEnd {
                agent_name,
                tool_name,
                output,
                is_error,
            } => {
                assert_eq!(agent_name, "coder");
                assert_eq!(tool_name, "bash");
                assert_eq!(output, "command failed: exit 1");
                assert!(is_error, "isError=true should propagate");
            }
            other => panic!("expected SubagentToolEnd, got {other:?}"),
        }
    }

    // (1c) subagent_end with outcome="failed" → SubagentEnd { outcome: Failed }.
    #[test]
    fn translate_event_subagent_end_failed_maps_to_failed() {
        let event = child_update(
            vec![ContentBlock::Text(TextContent::new("\n[subagent done]\n"))],
            serde_json::json!({
                "kind": "subagent_end",
                "agent": "reviewer",
                "outcome": "failed",
            }),
        );
        match translate_event(&event).expect("subagent_end translates") {
            AgentMsg::SubagentEnd { agent_name, outcome } => {
                assert_eq!(agent_name, "reviewer");
                assert_eq!(outcome, SubagentOutcome::Failed);
            }
            other => panic!("expected SubagentEnd, got {other:?}"),
        }
    }

    // (2) handle_agent_msg on AgentMsg::ToolEnd pushes a TranscriptEntry::ToolResult
    // (not dropped) with is_error reflecting the output. Assert the last entry is a
    // ToolResult. A result with `is_error: true` + non-empty content text is both
    // rendered (non-empty) and flagged as an error.
    #[test]
    fn handle_agent_msg_toolend_pushes_toolresult_reflecting_error() {
        let mut app = test_app();
        app.current_tool = Some("bash".into());
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(
            &mut app,
            AgentMsg::ToolEnd {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                output: serde_json::json!({
                    "is_error": true,
                    "content": [{ "type": "text", "text": "boom: exit 1" }]
                }),
            },
            &cmd_tx,
        );

        // The tool output was NOT dropped — a ToolResult entry landed.
        match app.transcript.last() {
            Some(TranscriptEntry::ToolResult { name, output, is_error }) => {
                assert_eq!(name, "bash");
                assert_eq!(output, "boom: exit 1", "rendered text preserved");
                assert!(*is_error, "is_error should mirror the result");
            }
            other => panic!("expected last entry to be ToolResult, got {other:?}"),
        }
        // current_tool cleared as part of the ToolEnd reset.
        assert!(app.current_tool.is_none());
    }

    // (4) parse_outcome mapping: failed→Failed, completed→Completed,
    // aborted/stopped→Stopped, unknown→Completed.
    #[test]
    fn parse_outcome_maps_known_and_unknown_strings() {
        assert_eq!(parse_outcome("failed"), SubagentOutcome::Failed);
        assert_eq!(parse_outcome("completed"), SubagentOutcome::Completed);
        assert_eq!(parse_outcome("stopped"), SubagentOutcome::Stopped);
        assert_eq!(parse_outcome("aborted"), SubagentOutcome::Stopped);
        // Unknown strings default to Completed (matching code_task.rs's
        // error.is_none() → "completed" reduction).
        assert_eq!(parse_outcome("nope"), SubagentOutcome::Completed);
        assert_eq!(parse_outcome(""), SubagentOutcome::Completed);
        // Whitespace is trimmed before matching.
        assert_eq!(parse_outcome("  failed  "), SubagentOutcome::Failed);
    }

    // (5) handle_agent_msg on SubagentEnd { outcome: Failed } pushes a
    // TranscriptEntry::SubagentEnd { outcome: Failed } (assert the variant).
    #[test]
    fn handle_agent_msg_subagent_end_failed_pushes_subagentend_variant() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();

        handle_agent_msg(
            &mut app,
            AgentMsg::SubagentEnd {
                agent_name: "reviewer".into(),
                outcome: SubagentOutcome::Failed,
            },
            &cmd_tx,
        );

        // The SubagentEnd variant lands; the trailing Blank separator follows it.
        let has_end = app.transcript.iter().any(|e| matches!(
            e,
            TranscriptEntry::SubagentEnd { agent_name, outcome }
                if agent_name == "reviewer" && *outcome == SubagentOutcome::Failed
        ));
        assert!(has_end, "expected a SubagentEnd{{Failed}} entry, got {:?}", app.transcript);
        // A trailing Blank separator is pushed after the end marker.
        assert!(matches!(app.transcript.last(), Some(TranscriptEntry::Blank)));
    }

    // (6) SubagentToolStart pushes a SubagentTool with the args echoed (the
    // renderer tool_previews them; here just assert args is stored).
    #[test]
    fn handle_agent_msg_subagent_tool_start_stores_args() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let args = serde_json::json!({ "command": "rg foo" });

        handle_agent_msg(
            &mut app,
            AgentMsg::SubagentToolStart {
                agent_name: "coder".into(),
                tool_name: "bash".into(),
                args: args.clone(),
            },
            &cmd_tx,
        );

        match app.transcript.last() {
            Some(TranscriptEntry::SubagentTool { agent_name, tool_name, args: got }) => {
                assert_eq!(agent_name, "coder");
                assert_eq!(tool_name, "bash");
                assert_eq!(got.clone(), args, "args stored verbatim for the renderer");
            }
            other => panic!("expected last entry to be SubagentTool, got {other:?}"),
        }
    }

    // --- M5b: agent_transcript + overlay follow/up/down ---------------------

    // (M5b-1) agent_transcript surfaces a per-agent ToolResult (stored with the
    // "{agent} · {tool}" name prefix) for the right agent, with the prefix
    // stripped so the overlay reads just "{tool}". A SubagentText for the same
    // agent is also surfaced; a ToolResult / SubagentText for a *different*
    // agent is excluded. The test registry is empty, so agent_transcript falls
    // through to the transcript scan (no log_path short-circuit).
    #[test]
    fn agent_transcript_surfaces_agent_toolresult_with_prefix_stripped() {
        let mut app = test_app();
        app.transcript.push(TranscriptEntry::SubagentText {
            agent_name: "reviewer".into(),
            text: "looking…".into(),
        });
        app.transcript.push(TranscriptEntry::ToolResult {
            name: "reviewer · bash".into(),
            output: "ok".into(),
            is_error: false,
        });
        // Noise from a different agent — must be filtered out.
        app.transcript.push(TranscriptEntry::SubagentText {
            agent_name: "coder".into(),
            text: "ignore me".into(),
        });
        app.transcript.push(TranscriptEntry::ToolResult {
            name: "coder · bash".into(),
            output: "nope".into(),
            is_error: false,
        });

        let lines = agent_transcript(&app, "reviewer");
        // The agent's own SubagentText is included.
        assert!(
            lines.iter().any(|e| matches!(
                e,
                TranscriptEntry::SubagentText { agent_name, text }
                    if agent_name == "reviewer" && text == "looking…"
            )),
            "reviewer SubagentText should appear, got {lines:?}"
        );
        // The ToolResult line appears with the "{agent} · " prefix stripped.
        let result = lines.iter().find_map(|e| match e {
            TranscriptEntry::ToolResult { name, output, is_error } => {
                Some((name.clone(), output.clone(), *is_error))
            }
            _ => None,
        });
        let (name, output, is_error) =
            result.expect("expected a ToolResult line for reviewer");
        assert_eq!(name, "bash", "prefix should be stripped");
        assert_eq!(output, "ok");
        assert!(!is_error);
        // Nothing from `coder` leaks in.
        assert!(
            !lines.iter().any(|e| matches!(
                e,
                TranscriptEntry::SubagentText { agent_name, .. } if agent_name == "coder"
            )),
            "coder entries must be filtered out, got {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|e| matches!(e, TranscriptEntry::ToolResult { name, .. } if name == "nope")),
            "coder tool result must be filtered out, got {lines:?}"
        );
    }

    // (M5b-2a) Overlay auto-tail: with follow=true, a new SubagentText for the
    // viewed agent keeps scroll at 0 (sticks to the bottom).
    #[test]
    fn overlay_auto_tail_follow_true_resets_scroll_on_new_subagent_text() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_msg(
            &mut app,
            AgentMsg::SubagentText {
                agent_name: "reviewer".into(),
                text: "more".into(),
            },
            &cmd_tx,
        );

        let overlay = app.agent_overlay.as_ref().expect("overlay still open");
        assert_eq!(
            overlay.scroll, 0,
            "follow=true should auto-tail (scroll stays 0)"
        );
        assert!(overlay.follow, "follow flag untouched");
    }

    // (M5b-2b) Overlay auto-tail: with follow=false (user scrolled up), a new
    // SubagentText does NOT yank scroll back to 0.
    #[test]
    fn overlay_auto_tail_follow_false_keeps_scroll_on_new_subagent_text() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 5,
            follow: false,
        });

        handle_agent_msg(
            &mut app,
            AgentMsg::SubagentText {
                agent_name: "reviewer".into(),
                text: "more".into(),
            },
            &cmd_tx,
        );

        let overlay = app.agent_overlay.as_ref().expect("overlay still open");
        assert_ne!(
            overlay.scroll, 0,
            "follow=false must not reset scroll (user scrolled up, not yanked)"
        );
        assert_eq!(overlay.scroll, 5, "scroll preserved");
        assert!(!overlay.follow);
    }

    // (M5b-2c) Overlay auto-tail only tails the *viewed* agent: a SubagentText
    // for a different agent does not touch this overlay's scroll.
    #[test]
    fn overlay_auto_tail_ignores_other_agents() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 7,
            follow: true,
        });

        handle_agent_msg(
            &mut app,
            AgentMsg::SubagentText {
                agent_name: "coder".into(),
                text: "elsewhere".into(),
            },
            &cmd_tx,
        );

        let overlay = app.agent_overlay.as_ref().expect("overlay still open");
        assert_eq!(
            overlay.scroll, 7,
            "a different agent's text must not auto-tail this overlay"
        );
    }

    // (M5b-3a) Up arrow on an overlay with follow=true flips follow to false
    // (so subsequent new output won't yank the user back) and increments scroll.
    #[test]
    fn handle_agent_overlay_key_up_disables_follow_and_increments_scroll() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &cmd_tx);

        let overlay = app.agent_overlay.as_ref().expect("overlay still open");
        assert!(
            !overlay.follow,
            "scrolling up must disable auto-tail (follow=false)"
        );
        assert!(overlay.scroll > 0, "scroll should increment on Up");
    }

    // (M5b-3b) Down arrow reaching the bottom (scroll == 0) re-arms auto-tail
    // (follow=true), so the user re-sticks to the bottom.
    #[test]
    fn handle_agent_overlay_key_down_to_bottom_re_arms_follow() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: false,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &cmd_tx);

        let overlay = app.agent_overlay.as_ref().expect("overlay still open");
        assert_eq!(overlay.scroll, 0, "Down saturating-sub keeps scroll at 0");
        assert!(
            overlay.follow,
            "reaching the bottom must re-arm follow (auto-tail)"
        );
    }

    // (M5b-3c) Down arrow away from the bottom (scroll > 0) does NOT re-arm
    // follow — only reaching the very bottom does.
    #[test]
    fn handle_agent_overlay_key_down_off_bottom_does_not_re_arm_follow() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 4,
            follow: false,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &cmd_tx);

        let overlay = app.agent_overlay.as_ref().expect("overlay still open");
        assert_eq!(overlay.scroll, 1, "Down decrements scroll by 3 (saturating)");
        assert!(
            !overlay.follow,
            "still off-bottom must keep follow=false"
        );
    }

    // (M5b-3d) Esc closes the overlay (Tab too) — the overlay is dropped.
    #[test]
    fn handle_agent_overlay_key_esc_closes_overlay() {
        let mut app = test_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &cmd_tx);

        assert!(app.agent_overlay.is_none(), "Esc should close the overlay");
    }

    // --- M5b-abort: overlay stop / reply keys -------------------------------

    // (M5b-abort-4a) Pressing `s` on an agent overlay aborts the viewed
    // agent DIRECTLY on the main thread (not via a Cmd to the bg thread,
    // which is blocked mid-turn and couldn't drain the channel). It resolves
    // the overlay's agent_name to a registered handle, takes the abort slot,
    // fires `.abort()`, marks the agent Stopped, and pushes a "stopped
    // {name}" System TranscriptEntry. The overlay stays open. Pins the
    // main-thread abort wiring from the key.
    #[test]
    fn overlay_s_aborts_viewed_agent_on_main_thread() {
        let mut app = test_app();
        let h = app.registry.register(AgentRegistration {
            name: "reviewer".to_string(),
            kind: AgentKind::Subagent { depth: 0, parent: None },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        });
        let (abort_handle, abort_signal) = AbortHandle::new();
        h.set_abort(abort_handle);
        assert!(!abort_signal.is_aborted());

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &cmd_tx);

        // No command is sent to the bg thread (the abort is synchronous on
        // the main thread).
        assert!(cmd_rx.try_recv().is_err(), "s must not send a Cmd — it aborts inline");
        // The abort slot was taken and the signal fired cross-thread.
        assert!(abort_signal.is_aborted(), "s must abort the agent's AbortHandle");
        assert!(h.take_abort().is_none(), "s must clear the abort slot (taken)");
        // The agent is marked Stopped and a System notice was pushed.
        assert_eq!(h.status(), AgentStatus::Stopped, "s must mark the agent Stopped");
        let last = app.transcript.last().expect("a transcript entry was pushed");
        let notice = match last { TranscriptEntry::System(s) => s.clone(), _ => String::new() };
        assert_eq!(notice, "stopped reviewer", "s pushes a 'stopped {{name}}' System line");
        // The overlay stays open.
        assert!(app.agent_overlay.is_some(), "s must not close the overlay");
    }

    // (M5b-abort-4b) `x` is the alias for `s` and runs the same main-thread
    // abort path. Pins the shared arm of the key match.
    #[test]
    fn overlay_x_aborts_viewed_agent_on_main_thread() {
        let mut app = test_app();
        let h = app.registry.register(AgentRegistration {
            name: "reviewer".to_string(),
            kind: AgentKind::Subagent { depth: 0, parent: None },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        });
        let (abort_handle, abort_signal) = AbortHandle::new();
        h.set_abort(abort_handle);

        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &cmd_tx);

        assert!(abort_signal.is_aborted(), "x must abort the agent's AbortHandle");
        assert_eq!(h.status(), AgentStatus::Stopped, "x must mark the agent Stopped");
        assert!(app.agent_overlay.is_some(), "x must not close the overlay");
    }

    // (M5b-abort-4c) `s` on an overlay whose agent_name no longer has a
    // registered handle (e.g. an in-process subagent that already returned
    // and was removed) pushes an honest "agent not found" System line and
    // leaves the overlay open — no crash, no silent no-op. Pins the stale
    // overlay path.
    #[test]
    fn overlay_s_for_unregistered_agent_reports_not_found() {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "ghost".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &cmd_tx);

        assert!(cmd_rx.try_recv().is_err(), "no Cmd is sent for an unregistered agent");
        let last = app.transcript.last().expect("a not-found notice was pushed");
        let notice = match last { TranscriptEntry::System(s) => s.clone(), _ => String::new() };
        assert_eq!(notice, "agent not found — nothing to stop", "s reports a not-found System line");
        assert!(app.agent_overlay.is_some(), "overlay must remain open");
    }

    // (M5b-abort-4d) `s` on a registered agent whose abort slot is already
    // empty (the agent finished and the spawner cleared the slot) pushes an
    // honest "already finished" line instead of silently doing nothing.
    #[test]
    fn overlay_s_for_finished_agent_reports_already_finished() {
        let mut app = test_app();
        let h = app.registry.register(AgentRegistration {
            name: "done".to_string(),
            kind: AgentKind::Subagent { depth: 0, parent: None },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        });
        // No set_abort: the slot is None, as it is after a child returns.
        assert!(h.take_abort().is_none());
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "done".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &cmd_tx);

        assert!(cmd_rx.try_recv().is_err(), "no Cmd is sent for a finished agent");
        let last = app.transcript.last().expect("an already-finished notice was pushed");
        let notice = match last { TranscriptEntry::System(s) => s.clone(), _ => String::new() };
        assert_eq!(notice, "done already finished — nothing to stop");
        // Status is NOT changed to Stopped for an already-finished agent.
        assert_ne!(h.status(), AgentStatus::Stopped);
    }

    // (M5b-abort-5) Pressing `r` on an agent overlay takes the textarea
    // content as the reply body and sends `Cmd::SendToAgent(id, text)` to the
    // background thread (an honest stub: the bg thread echoes it back as a
    // System line so the user sees the message was received). Pins the
    // honest-stub reply path the ui-task took.
    #[test]
    fn overlay_r_sends_send_to_agent_command() {
        let mut app = test_app();
        let h = app.registry.register(AgentRegistration {
            name: "reviewer".to_string(),
            kind: AgentKind::Subagent { depth: 0, parent: None },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        });
        // Type a reply into the input editor (the overlay reads the textarea
        // lines for the body).
        app.textarea.insert_str("please re-run grep");
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        app.agent_overlay = Some(AgentOverlay {
            agent_name: "reviewer".into(),
            scroll: 0,
            follow: true,
        });

        handle_agent_overlay_key(&mut app, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &cmd_tx);

        match cmd_rx.try_recv() {
            Ok(Cmd::SendToAgent(id, text)) => {
                assert_eq!(id, h.id, "SendToAgent must target the viewed agent");
                assert_eq!(text, "please re-run grep", "SendToAgent carries the textarea body");
            }
            other => panic!("expected Cmd::SendToAgent, got {other:?}"),
        }
        assert!(cmd_rx.try_recv().is_err(), "no further command expected");
        assert!(app.agent_overlay.is_some(), "r must not close the overlay");
    }

    // (M5b-abort-3) `Cmd::StopAgent(id)` is a real, exhaustively-matchable
    // variant: constructing one and matching it (the way the bg thread does)
    // must not panic, and the matched id round-trips. This also pins the
    // take/abort logic the bg thread runs — replicate it here against a
    // registered handle whose abort slot is `Some`, assert the slot is taken
    // (None) and the signal is aborted afterward.
    #[test]
    fn cmd_stop_agent_is_exhaustive_and_drives_abort() {
        // The variant is constructible and round-trips its id.
        let id = AgentId::new();
        let cmd = Cmd::StopAgent(id);
        match cmd {
            Cmd::StopAgent(matched) => assert_eq!(matched, id),
            _ => panic!("StopAgent must match its own arm"),
        }

        // Replicate the bg thread's `Cmd::StopAgent` dispatch (which lives
        // inline in the spawn closure, so isn't directly callable): resolve
        // the handle by id, take its abort slot, abort, and mark Stopped.
        let registry = AgentRegistry::new();
        let h = registry.register(AgentRegistration {
            name: "reviewer".to_string(),
            kind: AgentKind::Subagent { depth: 0, parent: None },
            color: AgentColor::Dim,
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        });
        let (handle, signal) = AbortHandle::new();
        h.set_abort(handle);
        assert!(!signal.is_aborted());

        let stop_id = h.id;
        // The bg-thread take/abort path, verbatim in effect:
        let found = registry.snapshot().into_iter().find(|h| h.id == stop_id);
        let found = found.expect("registered agent is resolvable by id");
        let aborted = if let Some(abort) = found.take_abort() {
            abort.abort();
            registry.set_status(stop_id, AgentStatus::Stopped);
            true
        } else {
            false
        };
        assert!(aborted, "the Some-branch abort path must run");
        assert!(signal.is_aborted(), "the abort must propagate to the signal");
        assert!(h.abort.lock().unwrap().is_none(), "the slot must be cleared after take");
        assert_eq!(h.status(), AgentStatus::Stopped, "the agent must be marked Stopped");
    }
}

