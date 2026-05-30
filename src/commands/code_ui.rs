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
//! scope); multi-line paste; syntax highlighting.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

use pi::model::{
    AssistantMessageEvent, ContentBlock, ImageContent, Message, TextContent, UserContent,
};
use pi::sdk::{
    create_agent_session, AbortHandle, AgentEvent, AgentSessionHandle, Error as PiError,
    RpcForkMessage, ThinkingLevel,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::commands::code_approvals::ApprovalState;
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_sandbox::{detect_strict_profile, format_profile_text};
use crate::commands::code_session::{
    build_session_options, most_recent_session, CodeSessionConfig, SessionPersistence,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::commands::code_term::TerminalApprovalUi;
use crate::config::{mask_key, Config as LibertaiConfig};

/// ANSI dim/bold helpers for cooked output (agent streaming phase).
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

const SHELL_ESCAPE_MAX_DISPLAY_BYTES: usize = 256 * 1024;
const HISTORY_DEFAULT_LIMIT: usize = 20;
const HISTORY_MAX_LIMIT: usize = 64;
const OSC52_MAX_TEXT_BYTES: usize = 128 * 1024;
const IMAGE_ATTACHMENT_MAX_BYTES: usize = 10 * 1024 * 1024;
const MENTION_ATTACHMENT_MAX_BYTES: usize = 256 * 1024;
const TREE_MAX_ENTRIES: usize = 200;
const CHANGELOG_DEFAULT_LIMIT: usize = 10;
const CHANGELOG_MAX_LIMIT: usize = 50;
const LOOP_DEFAULT_TURNS: usize = 3;
const LOOP_MAX_TURNS: usize = 10;
const AUTO_DEFAULT_TURNS: usize = 10;
const AUTO_MAX_TURNS: usize = 25;
const SCHEDULE_MAX_DELAY: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const STATUS_LINE_TEMPLATE_MAX_CHARS: usize = 240;
const STATUS_LINE_COMMAND_MAX_CHARS: usize = 240;
const STATUS_LINE_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const STATUS_LINE_COMMAND_CACHE_TTL: Duration = Duration::from_secs(5);
const BACKGROUND_AGENT_LOG_TAIL_BYTES: usize = 64 * 1024;
const STATUS_LINE_TOKENS: &[&str] = &[
    "project", "path", "session", "backend", "model", "mode", "style", "tokens", "ctx", "live",
];

/// Snapshot of the last completed turn's token usage. Written in
/// `repl_loop` after each successful prompt, read in `repaint()` to
/// render the context-usage strip on the rule line.
#[derive(Default, Clone)]
struct BarStatus {
    model_label: String,
    input_tokens: u64,
    context_window: u32,
    output_style: Option<String>,
    status_line_template: String,
    status_line_command: String,
}

#[derive(Clone)]
struct StatusLineCommandCache {
    key: String,
    value: String,
    error: String,
    ts: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageRecord {
    provider: String,
    model: String,
    input: u64,
    output: u64,
    context_window: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageSummary {
    turns: usize,
    last_input: u64,
    last_output: u64,
    output_total: u64,
    context_high_water: u64,
    context_window: u32,
    provider: String,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolActivitySummary {
    tool_name: String,
    count: u64,
    total_duration: Duration,
}

#[derive(Debug, Clone, PartialEq)]
struct ToolAttribution {
    tool_name: String,
    count: u64,
    total_duration: Duration,
    estimated_tokens: u64,
    estimated_cost: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageExportFormat {
    Json,
    Csv,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScheduledRun {
    id: String,
    prompt: String,
    due_at: Instant,
    due_epoch_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredScheduledRun {
    id: String,
    prompt: String,
    due_epoch_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrCommentDraft {
    path: String,
    line: u64,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScheduleCommand {
    Status,
    Cancel(String),
    Clear,
    Add { delay: Duration, prompt: String },
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyCommand {
    Status,
    On,
    Off,
    Test,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpCommand {
    Status,
    Probe,
    ProbeSave,
    Reset,
    Open,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopedModelsCommand {
    Status,
    Clear,
    Set(Vec<String>),
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSlashCommand<'a> {
    Status,
    List,
    Next,
    Previous,
    Set(&'a str),
}

#[derive(Debug, Default)]
struct ToolActivityTracker {
    active: HashMap<String, (String, Instant)>,
    totals: BTreeMap<String, ToolActivityTotal>,
}

#[derive(Debug, Default)]
struct ToolActivityTotal {
    count: u64,
    total_duration: Duration,
}

impl ToolActivityTracker {
    fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => {
                self.active
                    .insert(tool_call_id.clone(), (tool_name.clone(), Instant::now()));
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                ..
            } => {
                let elapsed = self
                    .active
                    .remove(tool_call_id)
                    .map(|(_, started_at)| started_at.elapsed())
                    .unwrap_or_default();
                let total = self.totals.entry(tool_name.clone()).or_default();
                total.count += 1;
                total.total_duration += elapsed;
            }
            _ => {}
        }
    }

    fn summary(&self) -> Vec<ToolActivitySummary> {
        self.totals
            .iter()
            .map(|(tool_name, total)| ToolActivitySummary {
                tool_name: tool_name.clone(),
                count: total.count,
                total_duration: total.total_duration,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionsCommand {
    Show,
    Open,
    Set(Mode),
    Forget,
    UnsupportedBypass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoopRequest {
    turns: usize,
    goal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AutoRun {
    limit: usize,
    completed: usize,
    goal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoCommand {
    Status,
    Off,
    On { turns: usize, goal: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillsCommand {
    List,
    Open,
    Enable(String),
    Disable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSettingsTarget {
    Account,
    Backends,
    Defaults,
    Agents,
    Skills,
    Hooks,
    Mcp,
    Approvals,
    Appearance,
    Sandbox,
    Advanced,
}

/// Process-global because the Ctrl-C handler (spawned by the `ctrlc`
/// crate on a separate thread) needs to reach both pieces of state
/// without a reference chain.
///
/// **Caveat for tests / library reuse:** `run_interactive` assumes it
/// is the sole owner of this process's terminal for its lifetime.
/// Calling it twice in the same process (e.g. from an integration
/// test) would share these slots across invocations, and the `ctrlc`
/// handler installed by the first call outlives the function. If we
/// ever need that, add a per-invocation reset step and document the
/// invariant more loudly.
static BAR_STATUS: Mutex<Option<BarStatus>> = Mutex::new(None);
static STATUS_LINE_COMMAND_CACHE: OnceLock<Mutex<Option<StatusLineCommandCache>>> =
    OnceLock::new();

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

fn update_bar_status(mut update: impl FnMut(&mut BarStatus)) {
    if let Ok(mut g) = BAR_STATUS.lock() {
        if let Some(status) = g.as_mut() {
            update(status);
        }
    }
}

fn rule_chip(cols: usize, mode: Mode) -> String {
    let status = BAR_STATUS.lock().ok().and_then(|g| g.clone());
    let inner = match status {
        Some(s) => {
            let text = status_line_command_text(&s.status_line_command)
                .or_else(|| expand_status_line_template(&s.status_line_template, &s, mode))
                .unwrap_or_else(|| default_rule_text(&s));
            format!(" {text} ")
        }
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

fn status_line_command_text(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let cwd = std::env::current_dir().ok();
    let key = format!(
        "{command}\n{}",
        cwd.as_ref()
            .map_or_else(String::new, |p| p.display().to_string())
    );
    let now = Instant::now();
    let cache = STATUS_LINE_COMMAND_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(entry) = guard.as_ref() {
            if entry.key == key
                && now.saturating_duration_since(entry.ts) < STATUS_LINE_COMMAND_CACHE_TTL
            {
                if !entry.value.is_empty() {
                    return Some(entry.value.clone());
                }
                if !entry.error.is_empty() {
                    return Some(format!("status command: {}", entry.error));
                }
            }
        }
    }
    let (value, error) = run_status_line_command(command);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(StatusLineCommandCache {
            key,
            value: value.clone(),
            error: error.clone(),
            ts: now,
        });
    }
    if !value.is_empty() {
        Some(value)
    } else if !error.is_empty() {
        Some(format!("status command: {error}"))
    } else {
        None
    }
}

fn run_status_line_command(command: &str) -> (String, String) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut child = match Command::new(shell)
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return (String::new(), e.to_string()),
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() >= STATUS_LINE_COMMAND_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return (String::new(), "timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return (String::new(), e.to_string()),
        }
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => (
            first_status_line(&String::from_utf8_lossy(&output.stdout)),
            String::new(),
        ),
        Ok(output) => {
            let stderr = first_status_line(&String::from_utf8_lossy(&output.stderr));
            let detail = if stderr.is_empty() {
                format!("exit {}", output.status.code().unwrap_or(1))
            } else {
                stderr
            };
            (String::new(), detail)
        }
        Err(e) => (String::new(), e.to_string()),
    }
}

fn first_status_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(STATUS_LINE_TEMPLATE_MAX_CHARS)
        .collect()
}

fn default_rule_text(status: &BarStatus) -> String {
    if status.context_window > 0 {
        let pct = context_percent(status.input_tokens, status.context_window);
        format!(
            "{pct}% · {} / {} · {}",
            human_tokens(status.input_tokens),
            human_tokens(u64::from(status.context_window)),
            status.model_label
        )
    } else {
        status.model_label.clone()
    }
}

fn context_percent(input_tokens: u64, context_window: u32) -> u32 {
    if context_window == 0 {
        return 0;
    }
    ((input_tokens.min(u64::from(context_window)) as f64 / f64::from(context_window)) * 100.0)
        .round() as u32
}

fn human_tokens(n: u64) -> String {
    if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Default context-window used by the status chip. LibertAI's
/// `/v1/models` doesn't expose this today, so every model we ship
/// defaults for shares the same cap. Kept as a function rather than a
/// constant so we can spec per-model once the endpoint grows a field.
fn context_window_for(_model: &str) -> u32 {
    32_768
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

// RawModeGuard lives in `code_term` so both this module and
// code_approvals can share the same panic-safe raw-mode wrapper.
use crate::commands::code_term::RawModeGuard;

/// Entry point from `code::run` when the command line has no prompt.
///
/// Owns the asupersync runtime, builds one `AgentSessionHandle`, then
/// drives the REPL loop against it.
pub fn run_interactive(
    provider: String,
    model: String,
    mode: Mode,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    cfg: Arc<LibertaiConfig>,
) -> Result<()> {
    print_banner(&provider, &model, mode);

    // Prime the status bar so the rule renders a useful label even
    // before the first turn completes.
    set_bar_status(BarStatus {
        model_label: format!("{provider}/{model}"),
        input_tokens: 0,
        context_window: context_window_for(&model),
        output_style: None,
        status_line_template: cfg.status_line_template.clone(),
        status_line_command: cfg.status_line_command.clone(),
    });

    // Forward Ctrl-C during streaming to pi's AbortHandle.
    install_ctrlc_handler();

    // Shared across prompts AND across mode toggles. The CLI backs this
    // with ~/.config/libertai/allow-rules.toml so "always allow" choices
    // survive future code sessions until /forget clears them.
    let approvals = Arc::new(ApprovalState::with_persistent_store(
        crate::config::allow_rules_path()?,
    )?);

    // Same asupersync setup as the non-interactive path.
    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|e| anyhow::anyhow!("asupersync reactor: {e}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!("asupersync runtime: {e}"))?;

    runtime.block_on(async move {
        repl_loop(
            provider,
            model,
            mode,
            approvals,
            resume_path,
            bash_command_wrapper,
            cfg,
        )
        .await
    })
}

fn print_banner(provider: &str, model: &str, mode: Mode) {
    let mode_tag = match mode {
        Mode::Normal => String::new(),
        Mode::AcceptEdits => format!(" {DIM}[accept-edits]{RESET}"),
        Mode::Plan => format!(" {DIM}[plan]{RESET}"),
    };
    println!(
        "{BOLD}libertai code{RESET} {DIM}— interactive ({provider}/{model}){RESET}{mode_tag}"
    );
    println!("{DIM}  type /help for commands, /exit or Ctrl+D to quit{RESET}");
    println!();
}

async fn repl_loop(
    mut provider: String,
    mut model: String,
    initial_mode: Mode,
    approvals: Arc<ApprovalState>,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    mut cfg: Arc<LibertaiConfig>,
) -> Result<()> {
    // Shared mode flag — flipped by Shift+Tab and `/plan`. The same
    // Arc is held by every ApprovalTool inside the session's
    // ToolRegistry, so toggling here changes behaviour at the next
    // tool call without rebuilding the session (and so without losing
    // message history).
    let mode = ModeFlag::new(initial_mode);
    let mut handle = build_handle(
        &provider,
        &model,
        mode.clone(),
        Arc::clone(&approvals),
        resume_path,
        bash_command_wrapper.clone(),
        Arc::clone(&cfg),
    )
    .await?;
    let mut session_hooks =
        crate::commands::code_hooks::SessionHookGuard::start(Arc::clone(&cfg));

    // If we resumed, print the rehydrated transcript so the user has
    // visual context before the input bar takes over. Skipped for fresh
    // sessions — there's nothing to show.
    if let Ok(messages) = handle.messages().await {
        if !messages.is_empty() {
            print_rehydrated_transcript(&messages);
        }
    }

    let history_store_path = history_store_path().ok();
    let mut history = match history_store_path.as_deref() {
        Some(path) => match load_input_history(path) {
            Ok(history) => history,
            Err(err) => {
                eprintln!("{DIM}  /history: could not load saved prompts: {err}.{RESET}");
                VecDeque::with_capacity(HISTORY_MAX_LIMIT)
            }
        },
        None => VecDeque::with_capacity(HISTORY_MAX_LIMIT),
    };
    let mut output_style: Option<String> = None;
    let mut usage_history: Vec<UsageRecord> = Vec::new();
    let tool_activity = Arc::new(Mutex::new(ToolActivityTracker::default()));
    let mut session_name: Option<String> = None;
    let mut autonomous_queue: VecDeque<String> = VecDeque::new();
    let mut auto_run: Option<AutoRun> = None;
    let mut scoped_model_patterns: Vec<String> = Vec::new();
    let schedule_store_path = schedule_store_path_for_cwd().ok();
    let mut scheduled_runs = match schedule_store_path.as_deref() {
        Some(path) => match load_scheduled_runs(path) {
            Ok(runs) => runs,
            Err(err) => {
                eprintln!("{DIM}  /schedule: could not load saved prompts: {err}.{RESET}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let mut next_scheduled_run_id = next_scheduled_run_id(&scheduled_runs);
    let mut pr_comment_drafts: Vec<PrCommentDraft> = Vec::new();
    let mut last_shell_command: Option<String> = None;

    loop {
        let autonomous_turn = if let Some(prompt) = pop_due_scheduled_prompt(&mut scheduled_runs) {
            if let Err(err) =
                persist_scheduled_runs_if_configured(schedule_store_path.as_deref(), &scheduled_runs)
            {
                eprintln!(
                    "{DIM}  /schedule: could not save scheduled prompts: {err}.{RESET}"
                );
            }
            Some(prompt)
        } else if let Some(prompt) = autonomous_queue.pop_front() {
            Some(prompt)
        } else if let Some(run) = auto_run.as_ref() {
            if run.completed < run.limit {
                Some(auto_loop_prompt(run.completed + 1, run.limit, &run.goal))
            } else {
                None
            }
        } else {
            None
        };
        let is_autonomous = autonomous_turn.is_some();
        let mut line = match autonomous_turn {
            Some(prompt) => {
                if prompt.starts_with("Scheduled follow-up (") {
                    println!("{DIM}  /schedule: running due follow-up.{RESET}");
                } else if let Some(run) = auto_run.as_ref() {
                    println!(
                        "{DIM}  /auto: running turn {}/{}.{RESET}",
                        run.completed + 1,
                        run.limit
                    );
                } else {
                    println!(
                        "{DIM}  /loop: running autonomous turn; {} queued after this.{RESET}",
                        autonomous_queue.len()
                    );
                }
                prompt
            }
            None => match read_line(mode.get(), &history)? {
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
                    let new_mode = flip(mode.get());
                    mode.set(new_mode);
                    announce_mode_change(new_mode);
                    continue;
                }
            }
        };

        let trimmed_owned = line.trim().to_string();
        let trimmed = trimmed_owned.as_str();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(format) = parse_usage_export_command(trimmed) {
            let tool_activity = tool_activity
                .lock()
                .map(|tracker| tracker.summary())
                .unwrap_or_default();
            print_usage_export(usage_summary(&usage_history), &tool_activity, format);
            continue;
        }
        if let Some(rest) = notify_command_arg(trimmed) {
            if let Err(e) = handle_notify_command(rest, &mut cfg) {
                eprintln!("{DIM}  /notify: {e:#}{RESET}");
            }
            continue;
        }
        if let Some(rest) = hooks_command_arg(trimmed) {
            print_hooks_command(&cfg, parse_hooks_command(rest));
            continue;
        }
        if let Some(rest) = mcp_command_arg(trimmed) {
            print_mcp_status(parse_mcp_command(rest));
            continue;
        }
        if let Some(rest) = send_command_arg(trimmed) {
            print_send_status(rest);
            continue;
        }
        if let Some(rest) = theme_command_arg(trimmed) {
            print_theme_status(rest);
            continue;
        }
        let mut content_override: Option<Vec<ContentBlock>> = None;
        let mut slash_prompt_handled = false;
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
                let new_mode = flip(mode.get());
                mode.set(new_mode);
                announce_mode_change(new_mode);
                continue;
            }
            "/forget" => {
                approvals.forget();
                println!("{DIM}  cleared saved \"always allow\" rules.{RESET}");
                continue;
            }
            "/permissions" | "/mode" => {
                print_permissions_status(mode.get());
                continue;
            }
            "/model" => {
                print_model_status(&handle, &cfg, &scoped_model_patterns);
                continue;
            }
            "/scoped-models" | "/scoped" => {
                handle_scoped_models_command("", &mut scoped_model_patterns);
                continue;
            }
            "/name" | "/rename" => {
                print_name_status(session_name.as_deref());
                continue;
            }
            "/status" => {
                print_session_status(
                    &provider,
                    &model,
                    mode.get(),
                    output_style.as_deref(),
                    &cfg,
                    usage_summary(&usage_history),
                );
                continue;
            }
            "/doctor" => {
                print_doctor(
                    &handle,
                    &provider,
                    &model,
                    mode.get(),
                    output_style.as_deref(),
                    &cfg,
                    &approvals,
                    usage_summary(&usage_history),
                )
                .await;
                continue;
            }
            "/abort" => {
                println!("{}", abort_status_message());
                continue;
            }
            "/sandbox" => {
                print_sandbox_status("info");
                continue;
            }
            "/usage" | "/cost" => {
                let tool_activity = tool_activity
                    .lock()
                    .map(|tracker| tracker.summary())
                    .unwrap_or_default();
                print_usage_summary(usage_summary(&usage_history), &tool_activity);
                continue;
            }
            "/notify" | "/notifications" => {
                if let Err(e) = handle_notify_command("", &mut cfg) {
                    eprintln!("{DIM}  /notify: {e:#}{RESET}");
                }
                continue;
            }
            "/history" => {
                print_history(&history, HISTORY_DEFAULT_LIMIT);
                continue;
            }
            "/copy" => {
                copy_last_assistant(&handle).await;
                continue;
            }
            "/config" | "/settings" => {
                print_config_status(&cfg);
                continue;
            }
            "/mcp" => {
                print_mcp_status(McpCommand::Status);
                continue;
            }
            "/statusline" | "/status-line" => {
                print_status_line_status(&cfg);
                continue;
            }
            "/hotkeys" => {
                print_hotkeys();
                continue;
            }
            "/tree" => {
                print_project_tree(None);
                continue;
            }
            "/changelog" => {
                print_changelog(CHANGELOG_DEFAULT_LIMIT);
                continue;
            }
            "/reload" => {
                match reload_repl_session(
                    "reloaded config",
                    &mut provider,
                    &mut model,
                    &mut cfg,
                    mode.clone(),
                    Arc::clone(&approvals),
                    bash_command_wrapper.clone(),
                )
                .await
                {
                    Ok(next) => {
                        drop(session_hooks);
                        handle = next;
                        session_hooks =
                            crate::commands::code_hooks::SessionHookGuard::start(Arc::clone(
                                &cfg,
                            ));
                        usage_history.clear();
                        update_bar_status(|status| status.output_style = output_style.clone());
                    }
                    Err(e) => eprintln!("{DIM}  /reload: {e:#}{RESET}"),
                }
                continue;
            }
            "/login" => {
                match crate::commands::login::run() {
                    Ok(()) => {
                        match reload_repl_session(
                            "logged in",
                            &mut provider,
                            &mut model,
                            &mut cfg,
                            mode.clone(),
                            Arc::clone(&approvals),
                            bash_command_wrapper.clone(),
                        )
                        .await
                        {
                            Ok(next) => {
                                drop(session_hooks);
                                handle = next;
                                session_hooks =
                                    crate::commands::code_hooks::SessionHookGuard::start(
                                        Arc::clone(&cfg),
                                    );
                                usage_history.clear();
                                update_bar_status(|status| {
                                    status.output_style = output_style.clone()
                                });
                            }
                            Err(e) => eprintln!("{DIM}  /login reload: {e:#}{RESET}"),
                        }
                    }
                    Err(e) => eprintln!("{DIM}  /login: {e:#}{RESET}"),
                }
                continue;
            }
            "/logout" => {
                match crate::commands::logout::run() {
                    Ok(()) => {
                        match reload_repl_session(
                            "logged out",
                            &mut provider,
                            &mut model,
                            &mut cfg,
                            mode.clone(),
                            Arc::clone(&approvals),
                            bash_command_wrapper.clone(),
                        )
                        .await
                        {
                            Ok(next) => {
                                drop(session_hooks);
                                handle = next;
                                session_hooks =
                                    crate::commands::code_hooks::SessionHookGuard::start(
                                        Arc::clone(&cfg),
                                    );
                                usage_history.clear();
                                update_bar_status(|status| {
                                    status.output_style = output_style.clone()
                                });
                            }
                            Err(e) => eprintln!("{DIM}  /logout reload: {e:#}{RESET}"),
                        }
                    }
                    Err(e) => eprintln!("{DIM}  /logout: {e:#}{RESET}"),
                }
                continue;
            }
            "/resume" => {
                match resolve_repl_resume_path("") {
                    Ok(path) => {
                        match resume_repl_session(
                            &path,
                            &provider,
                            &model,
                            mode.clone(),
                            Arc::clone(&approvals),
                            bash_command_wrapper.clone(),
                            Arc::clone(&cfg),
                        )
                        .await
                        {
                            Ok(next) => {
                                drop(session_hooks);
                                handle = next;
                                session_hooks =
                                    crate::commands::code_hooks::SessionHookGuard::start(
                                        Arc::clone(&cfg),
                                    );
                                usage_history.clear();
                                update_bar_status(|status| {
                                    status.output_style = output_style.clone()
                                });
                            }
                            Err(e) => eprintln!("{DIM}  /resume: {e:#}{RESET}"),
                        }
                    }
                    Err(e) => eprintln!("{DIM}  /resume: {e:#}{RESET}"),
                }
                continue;
            }
            "/fork" => {
                match handle_fork(&mut handle, "").await {
                    Ok(true) => usage_history.clear(),
                    Ok(false) => {}
                    Err(e) => eprintln!("{DIM}  /fork: {e:#}{RESET}"),
                }
                continue;
            }
            "/thinking" | "/think" | "/t" => {
                print_thinking_status(&handle);
                continue;
            }
            "/compact" => {
                if compact_transcript(&mut handle, None).await {
                    usage_history.clear();
                }
                continue;
            }
            "/memory" => {
                print_memory("show");
                continue;
            }
            "/skills" => {
                if let Err(e) = handle_skills_slash("") {
                    eprintln!("{DIM}  /skills: {e:#}{RESET}");
                }
                continue;
            }
            "/init" => {
                print_init_project(None);
                continue;
            }
            "/onboarding" | "/onboard" => {
                write_onboarding_guide(None);
                continue;
            }
            "/agents" => {
                print_agents();
                continue;
            }
            "/template" => {
                print_templates();
                continue;
            }
            "/export" => {
                export_transcript(&handle, None).await;
                continue;
            }
            "/share" => {
                share_transcript(&handle, None).await;
                continue;
            }
            "/image" | "/attach" => {
                println!("{DIM}  usage: {trimmed} <path> [prompt]{RESET}");
                continue;
            }
            "/mention" => {
                println!("{DIM}  usage: /mention <path> [prompt]{RESET}");
                continue;
            }
            "/vim" => {
                println!(
                    "{DIM}  Vim bindings are not implemented in the CLI REPL yet. The input bar uses native line-editing keys.{RESET}"
                );
                continue;
            }
            "/ide" => {
                println!(
                    "{DIM}  Dedicated VS Code / JetBrains integrations are not part of libertai code today. Run the CLI inside your project or use the desktop app workspace.{RESET}"
                );
                continue;
            }
            "/bug" => {
                print_bug_template(&provider, &model, mode.get(), output_style.as_deref());
                continue;
            }
            "/clear" | "/new" => {
                // Wipe the screen *and* rebuild the session so the
                // agent's message history starts fresh too. (Mode
                // toggles no longer rebuild — they preserve history —
                // so /clear is now the explicit "start over" verb.)
                let _ = std::io::stdout().write_all(b"\x1b[2J\x1b[H");
                let _ = std::io::stdout().flush();
                handle = build_handle(
                    &provider,
                    &model,
                    mode.clone(),
                    Arc::clone(&approvals),
                    None,
                    bash_command_wrapper.clone(),
                    Arc::clone(&cfg),
                )
                .await?;
                history.clear();
                usage_history.clear();
                println!("{DIM}  → fresh session.{RESET}");
                println!();
                continue;
            }
            _ => {}
        }
        // Slash commands that take an argument (handled here, not in
        // the match above, since `match` doesn't pattern-match prefixes).
        if let Some(rest) = trimmed.strip_prefix("/config ") {
            match handle_repl_config_command(rest, &mut cfg) {
                Ok(()) => {}
                Err(e) => eprintln!("{DIM}  /config: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/settings ") {
            match handle_repl_config_command(rest, &mut cfg) {
                Ok(()) => {}
                Err(e) => eprintln!("{DIM}  /settings: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = status_line_command_arg(trimmed) {
            match handle_status_line_command(rest, &mut cfg) {
                Ok(()) => {}
                Err(e) => eprintln!("{DIM}  /statusline: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/history ") {
            match parse_history_limit(rest) {
                Ok(limit) => print_history(&history, limit),
                Err(e) => eprintln!("{DIM}  /history: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/tree ") {
            print_project_tree(Some(rest.trim()));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/changelog ") {
            match parse_changelog_limit(rest) {
                Ok(limit) => print_changelog(limit),
                Err(e) => eprintln!("{DIM}  /changelog: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/resume ") {
            match resolve_repl_resume_path(rest) {
                Ok(path) => {
                    match resume_repl_session(
                        &path,
                        &provider,
                        &model,
                        mode.clone(),
                        Arc::clone(&approvals),
                        bash_command_wrapper.clone(),
                        Arc::clone(&cfg),
                    )
                    .await
                    {
                        Ok(next) => {
                            drop(session_hooks);
                            handle = next;
                            session_hooks =
                                crate::commands::code_hooks::SessionHookGuard::start(Arc::clone(
                                    &cfg,
                                ));
                            usage_history.clear();
                            update_bar_status(|status| status.output_style = output_style.clone());
                        }
                        Err(e) => eprintln!("{DIM}  /resume: {e:#}{RESET}"),
                    }
                }
                Err(e) => eprintln!("{DIM}  /resume: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/fork ") {
            match handle_fork(&mut handle, rest.trim()).await {
                Ok(true) => usage_history.clear(),
                Ok(false) => {}
                Err(e) => eprintln!("{DIM}  /fork: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = compact_command_notes(trimmed) {
            if compact_transcript(&mut handle, Some(rest)).await {
                usage_history.clear();
            }
            continue;
        }
        if let Some(rest) = loop_command_arg(trimmed) {
            let request = parse_loop_request(rest);
            auto_run = None;
            autonomous_queue = autonomous_loop_prompts(&request);
            println!(
                "{DIM}  /loop: queued {} autonomous turn(s){}.{RESET}",
                request.turns,
                if request.goal.is_empty() { "" } else { " with a goal" }
            );
            continue;
        }
        if let Some(rest) = auto_command_arg(trimmed) {
            match parse_auto_command(rest) {
                AutoCommand::Status => print_auto_status(auto_run.as_ref()),
                AutoCommand::Off => {
                    auto_run = None;
                    autonomous_queue.clear();
                    println!("{DIM}  /auto: continuous execution is off.{RESET}");
                }
                AutoCommand::On { turns, goal } => {
                    autonomous_queue.clear();
                    auto_run = Some(AutoRun {
                        limit: turns,
                        completed: 0,
                        goal,
                    });
                    let run = auto_run.as_ref().expect("auto run set");
                    println!(
                        "{DIM}  /auto: continuous execution is on for up to {} turn(s){}. Press Ctrl+C during a turn to stop.{RESET}",
                        run.limit,
                        if run.goal.is_empty() { "" } else { " with a goal" }
                    );
                }
            }
            continue;
        }
        if let Some(rest) = schedule_command_arg(trimmed) {
            match parse_schedule_command(rest) {
                ScheduleCommand::Status => print_schedule_status(&scheduled_runs),
                ScheduleCommand::Cancel(id) => {
                    let before = scheduled_runs.len();
                    scheduled_runs.retain(|run| run.id != id);
                    if scheduled_runs.len() < before {
                        if let Err(err) = persist_scheduled_runs_if_configured(
                            schedule_store_path.as_deref(),
                            &scheduled_runs,
                        ) {
                            eprintln!(
                                "{DIM}  /schedule: could not save scheduled prompts: {err}.{RESET}"
                            );
                        }
                        println!("{DIM}  /schedule: cancelled {id}.{RESET}");
                    } else {
                        println!("{DIM}  /schedule: no scheduled prompt found for {id}.{RESET}");
                    }
                }
                ScheduleCommand::Clear => {
                    let count = scheduled_runs.len();
                    scheduled_runs.clear();
                    if let Err(err) = persist_scheduled_runs_if_configured(
                        schedule_store_path.as_deref(),
                        &scheduled_runs,
                    ) {
                        eprintln!(
                            "{DIM}  /schedule: could not save scheduled prompts: {err}.{RESET}"
                        );
                    }
                    println!(
                        "{DIM}  /schedule: cleared {count} scheduled prompt{}.{RESET}",
                        if count == 1 { "" } else { "s" }
                    );
                }
                ScheduleCommand::Add { delay, prompt } => {
                    let id = format!("sch_{next_scheduled_run_id}");
                    next_scheduled_run_id += 1;
                    scheduled_runs.push(ScheduledRun {
                        id: id.clone(),
                        prompt: prompt.clone(),
                        due_at: Instant::now() + delay,
                        due_epoch_ms: now_epoch_ms().saturating_add(duration_millis_u64(delay)),
                    });
                    scheduled_runs.sort_by_key(|run| run.due_at);
                    if let Err(err) = persist_scheduled_runs_if_configured(
                        schedule_store_path.as_deref(),
                        &scheduled_runs,
                    ) {
                        eprintln!(
                            "{DIM}  /schedule: could not save scheduled prompts: {err}.{RESET}"
                        );
                    }
                    println!(
                        "{DIM}  /schedule: scheduled {id} in {}.{RESET}",
                        format_schedule_delay(delay)
                    );
                }
                ScheduleCommand::Usage => {
                    eprintln!("{DIM}  usage: /schedule in 10m follow up, /schedule list, /schedule cancel <id>, or /schedule clear{RESET}");
                }
            }
            continue;
        }
        if let Some(rest) = thinking_command_arg(trimmed) {
            match parse_thinking_level(rest) {
                Ok(level) => match handle.set_thinking_level(level).await {
                    Ok(()) => println!("{DIM}  → thinking set to {level}{RESET}"),
                    Err(e) => eprintln!("{DIM}  /thinking: {e:#}{RESET}"),
                },
                Err(e) => eprintln!("{DIM}  /thinking: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/memory ") {
            print_memory(rest.trim());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/skills ") {
            if let Err(e) = handle_skills_slash(rest.trim()) {
                eprintln!("{DIM}  /skills: {e:#}{RESET}");
            }
            continue;
        }
        if let Some(rest) = scoped_models_command_arg(trimmed) {
            handle_scoped_models_command(rest, &mut scoped_model_patterns);
            continue;
        }
        if let Some((_command, rest)) = mode_command_arg(trimmed) {
            match parse_permissions_command(rest) {
                PermissionsCommand::Show => print_permissions_status(mode.get()),
                PermissionsCommand::Open => print_permissions_open_hint(),
                PermissionsCommand::Set(new_mode) => {
                    mode.set(new_mode);
                    announce_mode_change(new_mode);
                }
                PermissionsCommand::Forget => {
                    approvals.forget();
                    println!("{DIM}  cleared saved \"always allow\" rules.{RESET}");
                }
                PermissionsCommand::UnsupportedBypass => {
                    println!(
                        "{DIM}  native bypassPermissions is intentionally unavailable. Use default, acceptEdits, or plan.{RESET}"
                    );
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/model ") {
            let model_command = parse_model_slash_command(rest);
            match model_command {
                ModelSlashCommand::Status => {
                    print_model_status(&handle, &cfg, &scoped_model_patterns)
                }
                ModelSlashCommand::List => {
                    print_model_list(&cfg, &provider, &scoped_model_patterns)
                }
                ModelSlashCommand::Next | ModelSlashCommand::Previous => {
                    let direction = if matches!(model_command, ModelSlashCommand::Previous) {
                        -1
                    } else {
                        1
                    };
                    match next_scoped_model(&cfg, &provider, &model, &scoped_model_patterns, direction) {
                        Ok(Some(next_model)) => {
                            match handle.set_model(&provider, &next_model).await {
                                Ok(()) => {
                                    model = next_model;
                                    set_bar_status(BarStatus {
                                        model_label: format!("{provider}/{model}"),
                                        input_tokens: 0,
                                        context_window: context_window_for(&model),
                                        output_style: output_style.clone(),
                                        status_line_template: cfg.status_line_template.clone(),
                                        status_line_command: cfg.status_line_command.clone(),
                                    });
                                    println!("{DIM}  → model set to {provider}/{model}{RESET}");
                                }
                                Err(e) => eprintln!("{DIM}  /model: {e:#}{RESET}"),
                            }
                        }
                        Ok(None) => eprintln!(
                            "{DIM}  /model: no alternative model found for the current scope.{RESET}"
                        ),
                        Err(e) => eprintln!("{DIM}  /model: {e:#}{RESET}"),
                    }
                }
                ModelSlashCommand::Set(spec) => match parse_model_spec(&provider, spec) {
                    Ok((next_provider, next_model)) => {
                        match handle.set_model(&next_provider, &next_model).await {
                            Ok(()) => {
                                provider = next_provider;
                                model = next_model;
                                set_bar_status(BarStatus {
                                    model_label: format!("{provider}/{model}"),
                                    input_tokens: 0,
                                    context_window: context_window_for(&model),
                                    output_style: output_style.clone(),
                                    status_line_template: cfg.status_line_template.clone(),
                                    status_line_command: cfg.status_line_command.clone(),
                                });
                                println!("{DIM}  → model set to {provider}/{model}{RESET}");
                            }
                            Err(e) => eprintln!("{DIM}  /model: {e:#}{RESET}"),
                        }
                    }
                    Err(e) => eprintln!("{DIM}  /model: {e:#}{RESET}"),
                },
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/login ") {
            handle_login_slash(rest.trim(), &cfg);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/logout ") {
            handle_logout_slash(rest.trim(), &cfg);
            continue;
        }
        if let Some((command, rest)) = name_command_arg(trimmed) {
            match parse_session_name(rest) {
                Ok(name) => match handle.set_session_name(name.clone()).await {
                    Ok(()) => {
                        session_name = Some(name.clone());
                        println!("{DIM}  → session name set to {name}{RESET}");
                    }
                    Err(e) => eprintln!("{DIM}  {command}: {e:#}{RESET}"),
                },
                Err(e) => eprintln!("{DIM}  {command}: {e:#}{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/export ") {
            export_transcript(&handle, Some(rest.trim())).await;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/share ") {
            share_transcript(&handle, Some(rest.trim())).await;
            continue;
        }
        if let Some(rest) = onboarding_command_arg(trimmed) {
            write_onboarding_guide(Some(rest));
            continue;
        }
        if let Some(rest) = pr_comments_draft_arg(trimmed) {
            stage_pr_comment_draft(rest, &mut pr_comment_drafts);
            continue;
        }
        if let Some(rest) = pr_comments_drafts_arg(trimmed) {
            handle_pr_comment_drafts(rest, &mut pr_comment_drafts);
            continue;
        }
        if let Some(rest) = pr_comments_reply_arg(trimmed) {
            reply_to_pr_comment_thread(rest);
            continue;
        }
        if let Some(rest) = pr_comments_resolve_arg(trimmed) {
            resolve_pr_comment_thread(rest, true);
            continue;
        }
        if let Some(rest) = pr_comments_unresolve_arg(trimmed) {
            resolve_pr_comment_thread(rest, false);
            continue;
        }
        if let Some(rest) = pr_comments_viewed_arg(trimmed) {
            mark_pr_comment_file(rest, true);
            continue;
        }
        if let Some(rest) = pr_comments_unviewed_arg(trimmed) {
            mark_pr_comment_file(rest, false);
            continue;
        }
        if let Some(rest) = pr_comments_thread_arg(trimmed) {
            create_pr_comment_thread(rest);
            continue;
        }
        if let Some(rest) = pr_comments_edit_arg(trimmed) {
            edit_pr_comment(rest);
            continue;
        }
        if let Some(rest) = pr_comments_review_arg(trimmed) {
            submit_pr_review(rest);
            continue;
        }
        if let Some((command, rest)) = image_command_arg(trimmed) {
            match build_image_prompt_content(rest, output_style.as_deref()) {
                Ok(content) => {
                    content_override = Some(content);
                    slash_prompt_handled = true;
                }
                Err(e) => {
                    eprintln!("{DIM}  {command}: {e:#}{RESET}");
                    continue;
                }
            }
        }
        if let Some(rest) = mention_command_arg(trimmed) {
            match build_mention_prompt(rest, output_style.as_deref()) {
                Ok(prompt) => {
                    line = prompt;
                    slash_prompt_handled = true;
                }
                Err(e) => {
                    eprintln!("{DIM}  /mention: {e:#}{RESET}");
                    continue;
                }
            }
        }
        if let Some(rest) = trimmed.strip_prefix("/sandbox ") {
            print_sandbox_status(rest.trim());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/agents ") {
            handle_agents_command(rest.trim());
            continue;
        }
        if !slash_prompt_handled {
            if let Some((command, scope)) = review_command_parts(trimmed) {
                match build_review_slash_prompt(command, scope) {
                    Ok(prompt) => line = prompt,
                    Err(e) => {
                        eprintln!("{DIM}  {command}: {e:#}{RESET}");
                        continue;
                    }
                }
            } else {
                if trimmed == "/agent" {
                    println!(
                        "{DIM}  usage: /agent [--worktree|--background] <name> <task>{RESET}"
                    );
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("/agent ") {
                    match build_agent_slash_action(rest.trim(), &provider, &model, mode.get()) {
                        Ok(AgentSlashAction::Foreground(prompt)) => {
                            line = prompt;
                        }
                        Ok(AgentSlashAction::Background(launch)) => {
                            match start_background_agent(&launch) {
                                Ok(started) => {
                                    println!(
                                        "{DIM}  /agent: started background agent `{}` pid {}.{RESET}",
                                        launch.name, started.pid
                                    );
                                    println!(
                                        "{DIM}  log: {}{RESET}",
                                        started.log_path.display()
                                    );
                                }
                                Err(e) => {
                                    eprintln!("{DIM}  /agent: {e:#}{RESET}");
                                }
                            }
                            continue;
                        }
                        Err(e) => {
                            eprintln!("{DIM}  /agent: {e:#}{RESET}");
                            continue;
                        }
                    }
                }
                if let Some(rest) = trimmed.strip_prefix("/template ") {
                    match build_template_slash_prompt(rest.trim()) {
                        Ok(prompt) => {
                            line = prompt;
                        }
                        Err(e) => {
                            eprintln!("{DIM}  /template: {e:#}{RESET}");
                            continue;
                        }
                    }
                } else if let Some((name, args)) = parse_direct_custom_slash(trimmed) {
                    match build_custom_slash_prompt(name, args) {
                        Ok(Some(prompt)) => {
                            line = prompt;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("{DIM}  /{name}: {e:#}{RESET}");
                            continue;
                        }
                    }
                }
            }
        }
        if trimmed == "/output-style" {
            handle_output_style("", &mut output_style);
            update_bar_status(|status| status.output_style = output_style.clone());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/output-style ") {
            handle_output_style(rest, &mut output_style);
            update_bar_status(|status| status.output_style = output_style.clone());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/init ") {
            let notes = rest.trim();
            if notes.is_empty() {
                println!("{DIM}  usage: /init [--agent] [project notes]{RESET}");
                continue;
            } else if let Some(action) = parse_init_from_agent_action(notes) {
                apply_init_from_agent(&handle, action).await;
                continue;
            } else if let Some(agent_notes) = parse_init_agent_notes(notes) {
                line = crate::commands::code_init::init_agent_prompt(agent_notes);
            } else {
                print_init_project(Some(notes));
                continue;
            }
        }
        if let Some(rest) = trimmed.strip_prefix("/remember") {
            let text = rest.trim();
            if text.is_empty() {
                println!(
                    "{DIM}  usage: /remember [user:|feedback:|project:|reference:] <text>{RESET}"
                );
                continue;
            }
            let cwd = match std::env::current_dir() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{DIM}  /remember: could not resolve cwd: {e}{RESET}");
                    continue;
                }
            };
            let parsed = crate::commands::code_memory::parse_memory_note(text);
            if parsed.text.is_empty() {
                println!(
                    "{DIM}  usage: /remember [user:|feedback:|project:|reference:] <text>{RESET}"
                );
                continue;
            }
            match crate::commands::code_memory::append_memory_with_kind(
                &cwd,
                parsed.kind,
                &parsed.text,
            ) {
                Ok(path) => {
                    println!(
                        "{DIM}  → remembered [{}] in {} (takes effect next session){RESET}",
                        parsed.kind.label(),
                        path.display(),
                    );
                }
                Err(e) => {
                    eprintln!("{DIM}  /remember: failed: {e:#}{RESET}");
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('!') {
            match shell_escape_command(rest, last_shell_command.as_deref()) {
                ShellEscapeAction::Run(command) => {
                    last_shell_command = Some(command.clone());
                    run_shell_escape(&command, bash_command_wrapper.as_deref());
                }
                ShellEscapeAction::Usage(message) => println!("{DIM}  {message}{RESET}"),
            }
            continue;
        }

        // Remember the submitted line.
        if !is_autonomous && history.back().is_none_or(|last| last != trimmed) {
            if history.len() == HISTORY_MAX_LIMIT {
                history.pop_front();
            }
            history.push_back(trimmed.to_string());
            if let Err(err) =
                persist_input_history_if_configured(history_store_path.as_deref(), &history)
            {
                eprintln!("{DIM}  /history: could not save prompt history: {err}.{RESET}");
            }
        }

        // Echo the submitted user line as a chip above the stream region.
        println!("{BOLD}\u{276f} {}{RESET}", trimmed);

        // Hand off to pi with an abort signal so the Ctrl-C handler can
        // interrupt an in-flight turn without tearing the REPL down.
        let (abort_handle, abort_signal) = AbortHandle::new();
        set_current_abort(abort_handle);
        let render = {
            let tool_activity = Arc::clone(&tool_activity);
            let hook_cfg = Arc::clone(&cfg);
            move |event: AgentEvent| {
                if let Ok(mut tracker) = tool_activity.lock() {
                    tracker.observe(&event);
                }
                crate::commands::code_hooks::run_post_tool_hooks(hook_cfg.as_ref(), &event);
                render_event(event);
            }
        };
        let result = if let Some(content) = content_override {
            handle
                .prompt_with_content_with_abort(content, abort_signal, render)
                .await
        } else {
            let agent_line = apply_output_style(output_style.as_deref(), &line);
            match crate::commands::code_hooks::run_user_prompt_submit_hooks(
                cfg.as_ref(),
                &agent_line,
            ) {
                Ok(agent_line) => handle.prompt_with_abort(agent_line, abort_signal, render).await,
                Err(e) => {
                    clear_current_abort();
                    eprintln!("{DIM}  {e:#}{RESET}");
                    continue;
                }
            }
        };
        clear_current_abort();

        // `render_event` already emits a trailing newline on AgentEnd,
        // so we don't need a second one here — emitting one would
        // leave a gap between the response and the usage/status line.
        match result {
            Ok(msg) => {
                crate::commands::code_hooks::run_stop_hooks(cfg.as_ref());
                if let Some(run) = auto_run.as_mut() {
                    run.completed += 1;
                }
                let context_window = context_window_for(&msg.model);
                usage_history.push(UsageRecord {
                    provider: msg.provider.clone(),
                    model: msg.model.clone(),
                    input: msg.usage.input,
                    output: msg.usage.output,
                    context_window,
                });
                // Update the status bar with this turn's input-token count
                // so the next repaint reflects real context usage.
                set_bar_status(BarStatus {
                    model_label: format!("{}/{}", msg.provider, msg.model),
                    input_tokens: msg.usage.input,
                    context_window,
                    output_style: output_style.clone(),
                    status_line_template: cfg.status_line_template.clone(),
                    status_line_command: cfg.status_line_command.clone(),
                });
                eprintln!(
                    "{DIM}  {}/{}  stop: {:?}  in={} out={}{RESET}",
                    msg.provider,
                    msg.model,
                    msg.stop_reason,
                    msg.usage.input,
                    msg.usage.output,
                );
                if cfg.code_turn_notifications && !is_autonomous {
                    crate::commands::code_term::notify_terminal(
                        "LibertAI Code",
                        "Agent turn complete",
                    );
                }
                if matches!(mode.get(), Mode::Plan) && prompt_plan_exit_handoff()? {
                    mode.set(Mode::Normal);
                    println!(
                        "{DIM}  → plan approved. normal mode is active; mutating tools are back online.{RESET}"
                    );
                }
                if let Some(run) = auto_run.as_ref() {
                    let messages = handle.messages().await.unwrap_or_default();
                    let text = last_assistant_text(&messages).unwrap_or_default();
                    if text.contains("AUTO_DONE")
                        || text.contains("AUTO_BLOCKED")
                        || run.completed >= run.limit
                    {
                        println!(
                            "{DIM}  /auto: stopped after {}/{} turn(s).{RESET}",
                            run.completed, run.limit
                        );
                        auto_run = None;
                    }
                }
            }
            Err(PiError::Aborted) => {
                autonomous_queue.clear();
                auto_run = None;
                println!();
                eprintln!("{DIM}  (interrupted){RESET}");
            }
            Err(e) => {
                autonomous_queue.clear();
                auto_run = None;
                println!();
                eprintln!("{DIM}  error: {e}{RESET}");
            }
        }
        // One blank line between the stats/error footer and the next
        // input bar — anything more was visually noisy.
        println!();
    }
}

fn flip(m: Mode) -> Mode {
    // Shift+Tab cycles Normal → AcceptEdits → Plan → Normal. Most
    // users only toggle Normal ↔ Plan; the middle stop is opt-in
    // via /mode in the REPL or the slash picker in the desktop.
    match m {
        Mode::Normal => Mode::AcceptEdits,
        Mode::AcceptEdits => Mode::Plan,
        Mode::Plan => Mode::Normal,
    }
}

fn announce_mode_change(new_mode: Mode) {
    match new_mode {
        Mode::Normal => {
            println!(
                "{DIM}  → normal mode. mutating tools (bash, edit, write) are back online.{RESET}"
            );
        }
        Mode::AcceptEdits => {
            println!(
                "{DIM}  → accept-edits mode. write/edit/hashline_edit auto-allow; bash still prompts.{RESET}"
            );
        }
        Mode::Plan => {
            println!(
                "{DIM}  → plan mode. mutating tools auto-deny until you toggle back. session history is preserved.{RESET}"
            );
        }
    }
    // Trailing blank line so the next read_line's first paint doesn't
    // overwrite the status message we just emitted.
    println!();
}

fn parse_init_agent_notes(input: &str) -> Option<Option<&str>> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    for marker in ["--agent", "agent", "model"] {
        if lower == marker {
            return Some(None);
        }
        if lower.starts_with(marker)
            && lower
                .as_bytes()
                .get(marker.len())
                .is_some_and(u8::is_ascii_whitespace)
        {
            return Some(Some(trimmed[marker.len()..].trim()));
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitFromAgentAction {
    Preview,
    PreviewMergeLines,
    Append,
    Merge,
    MergeLines,
    Replace,
}

fn parse_init_from_agent_action(input: &str) -> Option<InitFromAgentAction> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("from-agent")
        .or_else(|| lower.strip_prefix("from_agent"))
        .or_else(|| lower.strip_prefix("apply-agent"))
        .or_else(|| lower.strip_prefix("apply_agent"))?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim() {
        "" | "preview" | "show" => Some(InitFromAgentAction::Preview),
        "preview merge-lines" | "preview line-merge" | "preview lines" => {
            Some(InitFromAgentAction::PreviewMergeLines)
        }
        "append" => Some(InitFromAgentAction::Append),
        "merge" | "apply" => Some(InitFromAgentAction::Merge),
        "merge-lines" | "line-merge" | "lines" => Some(InitFromAgentAction::MergeLines),
        "replace" => Some(InitFromAgentAction::Replace),
        _ => None,
    }
}

fn prompt_plan_exit_handoff() -> Result<bool> {
    let mut stderr = io::stderr();
    eprintln!();
    eprintln!("  \x1b[36;1m⎯ plan ready for approval ⎯\x1b[0m");
    eprint!("  \x1b[2m[a]\x1b[0m approve plan and switch to normal mode  ");
    eprint!("\x1b[2m[d]\x1b[0m keep planning: ");
    let _ = stderr.flush();

    let _guard = match RawModeGuard::enter() {
        Ok(g) => g,
        Err(_) => {
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            eprintln!();
            return Ok(parse_plan_exit_choice(&line));
        }
    };
    let approved = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent { code, .. })) => match code {
                KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Enter => break true,
                KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc => break false,
                _ => continue,
            },
            Ok(_) => continue,
            Err(e) => return Err(anyhow::anyhow!("read plan approval: {e}")),
        }
    };
    drop(_guard);
    eprintln!(
        "\x1b[2m{}\x1b[0m",
        if approved { "approved" } else { "kept in plan mode" }
    );
    Ok(approved)
}

fn parse_plan_exit_choice(line: &str) -> bool {
    matches!(line.trim().chars().next(), Some('a') | Some('A') | None)
}

async fn build_handle(
    provider: &str,
    model: &str,
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    cfg: Arc<LibertaiConfig>,
) -> Result<AgentSessionHandle> {
    // Snapshot the mode value before the factory consumes the flag so
    // the prompt addendum (S1-B) can be conditional on the *initial*
    // mode. The flag itself remains Arc-shared with the factory and
    // tracks runtime toggles via Shift+Tab.
    let initial_mode = mode.get();
    let ui = Arc::new(TerminalApprovalUi);
    let factory = Arc::new(LibertaiToolFactory::new_with_features(
        mode,
        approvals,
        ui,
        FactoryFeatures::cli_defaults(),
        Some(Arc::clone(&cfg)),
    )
    .with_tool_policy(crate::commands::code_hooks::tool_policy_from_config(
        Arc::clone(&cfg),
    )));
    let persistence = match resume_path {
        Some(p) => SessionPersistence::Resume(p),
        None => SessionPersistence::Fresh,
    };
    let max_tokens = Some(crate::commands::code_session::DEFAULT_MAX_TOKENS);
    let skill_cwd = std::env::current_dir().ok();
    let append_system_prompt =
        code_skills::prompt_for_pillar(SkillPillar::Code, skill_cwd.as_deref())?;
    let append_system_prompt = crate::commands::code_env_prompt::append_environment_prompt(
        append_system_prompt,
        skill_cwd.as_deref(),
    );
    let append_system_prompt =
        crate::commands::code_mode_prompt::apply(append_system_prompt, initial_mode);
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

async fn reload_repl_session(
    label: &str,
    provider: &mut String,
    model: &mut String,
    cfg: &mut Arc<LibertaiConfig>,
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    bash_command_wrapper: Option<Vec<String>>,
) -> Result<AgentSessionHandle> {
    let next_cfg = crate::config::load().context("reload config")?;
    let (next_provider, next_model) =
        reload_model_selection(cfg.as_ref(), &next_cfg, provider, model);
    *cfg = Arc::new(next_cfg);
    *provider = next_provider;
    *model = next_model;
    let handle = build_handle(
        provider,
        model,
        mode,
        approvals,
        None,
        bash_command_wrapper,
        Arc::clone(cfg),
    )
    .await?;
    set_bar_status(BarStatus {
        model_label: format!("{provider}/{model}"),
        input_tokens: 0,
        context_window: context_window_for(model),
        output_style: None,
        status_line_template: cfg.status_line_template.clone(),
        status_line_command: cfg.status_line_command.clone(),
    });
    println!("{DIM}  → {label}; fresh session using {provider}/{model}.{RESET}");
    Ok(handle)
}

fn reload_model_selection(
    old_cfg: &LibertaiConfig,
    next_cfg: &LibertaiConfig,
    current_provider: &str,
    current_model: &str,
) -> (String, String) {
    if current_provider == old_cfg.default_code_provider
        && current_model == old_cfg.default_code_model
    {
        (
            next_cfg.default_code_provider.clone(),
            next_cfg.default_code_model.clone(),
        )
    } else {
        (current_provider.to_string(), current_model.to_string())
    }
}

async fn resume_repl_session(
    path: &Path,
    provider: &str,
    model: &str,
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    bash_command_wrapper: Option<Vec<String>>,
    cfg: Arc<LibertaiConfig>,
) -> Result<AgentSessionHandle> {
    let handle = build_handle(
        provider,
        model,
        mode,
        approvals,
        Some(path.to_path_buf()),
        bash_command_wrapper,
        cfg,
    )
    .await?;
    println!("{DIM}  → resumed session: {}{RESET}", path.display());
    if let Ok(messages) = handle.messages().await {
        if !messages.is_empty() {
            print_rehydrated_transcript(&messages);
        }
    }
    Ok(handle)
}

fn resolve_repl_resume_path(input: &str) -> Result<PathBuf> {
    let raw = input.trim();
    if raw.is_empty() {
        let cwd = std::env::current_dir().context("resolve current directory")?;
        let recent = most_recent_session(&cwd)?
            .ok_or_else(|| anyhow::anyhow!("no past sessions for {}", cwd.display()))?;
        return Ok(PathBuf::from(recent.path));
    }
    let path = PathBuf::from(raw);
    if !path.exists() {
        anyhow::bail!("session file not found: {}", path.display());
    }
    Ok(path)
}

async fn handle_fork(handle: &mut AgentSessionHandle, query: &str) -> Result<bool> {
    let messages = handle
        .get_fork_messages()
        .await
        .context("list forkable messages")?;
    if messages.is_empty() {
        println!("{DIM}  /fork: no user messages to fork from.{RESET}");
        return Ok(false);
    }
    if matches!(query.trim().to_ascii_lowercase().as_str(), "list" | "ls") {
        print_fork_messages(&messages);
        return Ok(false);
    }
    let selected = select_fork_message(&messages, query)?;
    let forked = handle
        .fork(&selected.entry_id)
        .await
        .context("fork session")?;
    if forked.cancelled {
        println!("{DIM}  /fork: cancelled by session hook.{RESET}");
        return Ok(false);
    }
    println!("{DIM}  → forked from {}{RESET}", selected.entry_id);
    if let Ok(messages) = handle.messages().await {
        if !messages.is_empty() {
            print_rehydrated_transcript(&messages);
        }
    }
    let restored = forked.text.trim();
    if !restored.is_empty() {
        println!("{BOLD}restored prompt{RESET}");
        println!("{restored}");
        println!("{DIM}  edit or resubmit that prompt as your next message.{RESET}");
    }
    Ok(true)
}

fn select_fork_message(messages: &[RpcForkMessage], query: &str) -> Result<RpcForkMessage> {
    let raw = query.trim();
    if raw.is_empty() {
        return messages
            .last()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no user messages to fork from"));
    }
    if raw.chars().all(|c| c.is_ascii_digit()) {
        let idx = raw.parse::<usize>().context("invalid fork index")?;
        if idx == 0 || idx > messages.len() {
            anyhow::bail!("invalid fork index {idx}; expected 1..={}", messages.len());
        }
        return Ok(messages[idx - 1].clone());
    }
    let needle = raw.trim_start_matches('/');
    let mut hits = messages
        .iter()
        .filter(|m| m.entry_id == needle || m.entry_id.starts_with(needle));
    let Some(first) = hits.next() else {
        anyhow::bail!("no user message id matches `{raw}`");
    };
    if hits.next().is_some() {
        anyhow::bail!("ambiguous fork id `{raw}`");
    }
    Ok(first.clone())
}

fn print_fork_messages(messages: &[RpcForkMessage]) {
    println!("{BOLD}forkable user messages{RESET}");
    for (idx, message) in messages.iter().enumerate() {
        println!(
            "{DIM}  {:>2}. {} — {}{RESET}",
            idx + 1,
            message.entry_id,
            fork_message_preview(&message.text)
        );
    }
}

fn fork_message_preview(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(empty)");
    truncate_chars(first, 80)
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn thinking_command_arg(input: &str) -> Option<&str> {
    for prefix in ["/thinking ", "/think ", "/t "] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn mode_command_arg(input: &str) -> Option<(&str, &str)> {
    for command in ["/permissions", "/mode"] {
        if input == command {
            return Some((command, ""));
        }
        if let Some(rest) = input.strip_prefix(&format!("{command} ")) {
            return Some((command, rest.trim()));
        }
    }
    None
}

fn name_command_arg(input: &str) -> Option<(&str, &str)> {
    for command in ["/name", "/rename"] {
        if let Some(rest) = input.strip_prefix(&format!("{command} ")) {
            return Some((command, rest.trim()));
        }
    }
    None
}

fn parse_thinking_level(input: &str) -> Result<ThinkingLevel> {
    let raw = input.trim();
    if raw.is_empty() {
        anyhow::bail!("usage: /thinking <off|minimal|low|medium|high|xhigh>");
    }
    raw.parse::<ThinkingLevel>()
        .map_err(|_| anyhow::anyhow!("unknown thinking level `{raw}`"))
}

fn print_thinking_status(handle: &AgentSessionHandle) {
    let current = handle.thinking_level().unwrap_or_default();
    println!("{BOLD}thinking{RESET}");
    println!("{DIM}  current:{RESET} {current}");
    println!("{DIM}  supported:{RESET} off, minimal, low, medium, high, xhigh");
    println!("{DIM}  usage:{RESET} /thinking <level> (also /think or /t)");
}

/// Render a previously-saved conversation in the same shape the live REPL
/// uses, so a `--resume` user sees their context as static history before
/// the input bar appears. Streamed output during normal operation comes
/// out via the same `print!` path; here we just paint each prior turn at
/// once, with no animation.
fn print_rehydrated_transcript(messages: &[Message]) {
    println!("{DIM}  ── resuming session ──{RESET}");
    for msg in messages {
        match msg {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => println!("{BOLD}you{RESET}  {t}"),
                UserContent::Blocks(blocks) => {
                    let mut buf = String::new();
                    for b in blocks {
                        if let ContentBlock::Text(tc) = b {
                            buf.push_str(&tc.text);
                        }
                    }
                    if !buf.is_empty() {
                        println!("{BOLD}you{RESET}  {buf}");
                    }
                }
            },
            Message::Assistant(a) => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for b in &a.content {
                    match b {
                        ContentBlock::Text(tc) => text.push_str(&tc.text),
                        ContentBlock::ToolCall(tc) => tool_calls.push(tc.name.clone()),
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    println!("{text}");
                }
                for name in tool_calls {
                    if name != "todo" {
                        println!("  {DIM}[tool] {name}{RESET}");
                    }
                }
            }
            // Tool results were already shown inline last time; replaying
            // them verbatim would be noisy. Skip.
            Message::ToolResult(_) | Message::Custom(_) => {}
        }
    }
    println!("{DIM}  ── end of saved transcript ──{RESET}");
    println!();
}

fn print_help() {
    println!("{DIM}  /help     — show this message{RESET}");
    println!("{DIM}  /exit     — quit the REPL (also /quit, Ctrl+D){RESET}");
    println!("{DIM}  /plan     — toggle plan mode (also Shift+Tab){RESET}");
    println!("{DIM}  /permissions [default|acceptEdits|plan|open|forget]{RESET}");
    println!("{DIM}  /mode [default|acceptEdits|plan] — alias for /permissions{RESET}");
    println!("{DIM}  {}{RESET}", model_usage_text());
    println!("{DIM}  /name <name> — set this session's display name (also /rename){RESET}");
    println!("{DIM}  /status   — show current REPL session status{RESET}");
    println!("{DIM}  /doctor   — run a local session/config diagnostic report{RESET}");
    println!("{DIM}  /abort    — show how to interrupt the active CLI turn{RESET}");
    println!("{DIM}  /review [scope] — ask the agent to review current code changes{RESET}");
    println!("{DIM}  /security-review [scope] — ask for a focused security review{RESET}");
    println!("{DIM}  /pr_comments [scope] — ask the agent to inspect PR review comments{RESET}");
    println!(
        "{DIM}  /pr_comments reply <thread_id> <body> — reply to a GitHub PR review thread{RESET}"
    );
    println!(
        "{DIM}  /pr_comments resolve <thread_id> — resolve a GitHub PR review thread{RESET}"
    );
    println!(
        "{DIM}  /pr_comments unresolve <thread_id> — reopen a GitHub PR review thread{RESET}"
    );
    println!(
        "{DIM}  /pr_comments viewed <path> — mark a pull request file as viewed{RESET}"
    );
    println!(
        "{DIM}  /pr_comments unviewed <path> — mark a pull request file as unviewed{RESET}"
    );
    println!(
        "{DIM}  /pr_comments thread <path>:<line> <body> — add a GitHub PR line review thread{RESET}"
    );
    println!(
        "{DIM}  /pr_comments edit <comment_id> <body> — edit a GitHub PR review comment{RESET}"
    );
    println!(
        "{DIM}  /pr_comments review <approve|comment|request_changes> <body> — submit a GitHub PR summary review{RESET}"
    );
    println!("{DIM}  /sandbox [info|reload] — inspect the bash sandbox profile{RESET}");
    println!("{DIM}  /usage    — show token usage for this REPL session (also /cost; /usage export [json|csv]){RESET}");
    println!("{DIM}  /history [count] — show recent submitted prompts{RESET}");
    println!("{DIM}  /copy     — copy the last assistant response to the terminal clipboard{RESET}");
    println!("{DIM}  /config [path|open|advanced|set <key> <value>|unset <key>] — show or update active config{RESET}");
    println!("{DIM}  /hooks    — show configured command hooks (/hooks open shows settings target){RESET}");
    println!("{DIM}  /mcp      — show terminal MCP support status{RESET}");
    println!(
        "{DIM}  /statusline <template|command <shell>|reset> — customize the input-bar status line{RESET}"
    );
    println!("{DIM}  /hotkeys  — show input bar keyboard controls{RESET}");
    println!("{DIM}  /tree [path] — show a bounded project tree{RESET}");
    println!("{DIM}  /changelog [count] — show recent git commits{RESET}");
    println!("{DIM}  /reload   — reload config and start a fresh agent session{RESET}");
    println!("{DIM}  /resume [path] — resume the latest or specified saved session{RESET}");
    println!("{DIM}  /fork [list|index|id] — fork from a previous user message{RESET}");
    println!("{DIM}  /thinking [off|minimal|low|medium|high|xhigh] — show or set thinking{RESET}");
    println!("{DIM}  {}{RESET}", scoped_models_usage_text());
    println!("{DIM}  /compact — compact older conversation history now{RESET}");
    println!("{DIM}  /loop [turns] [goal] — run bounded autonomous follow-up turns{RESET}");
    println!("{DIM}  /auto on [turns] [goal] — bounded continuous execution (/auto off|status){RESET}");
    println!("{DIM}  /schedule in <delay> <prompt> — queue a due follow-up prompt (/schedule list|cancel|clear){RESET}");
    println!("{DIM}  /send [target message] — show terminal inter-session send status{RESET}");
    println!("{DIM}  /notify on|off|status|test — turn-complete terminal notifications{RESET}");
    println!("{DIM}  /image <path> [prompt] — attach a local image to the next prompt{RESET}");
    println!("{DIM}  /attach <path> [prompt] — alias for /image{RESET}");
    println!("{DIM}  /mention <path> [prompt] — attach a local text file to the next prompt{RESET}");
    println!("{DIM}  /login [status|libertai|provider] — inspect auth or run libertai login{RESET}");
    println!("{DIM}  /logout [status|libertai|provider] — run libertai logout or explain provider logout{RESET}");
    println!("{DIM}  /memory   — show project memory (/memory open|edit|clear|files|references|import <path>|import-claude|import-claude-all|path){RESET}");
    println!("{DIM}  /skills [list|open|enable <name>|disable <name>] — manage code-agent skills for new sessions{RESET}");
    println!("{DIM}  /init [--agent|from-agent preview merge-lines|append|merge-lines|merge|replace] [notes] — create or merge AGENTS.md guidance{RESET}");
    println!("{DIM}  /onboarding|/onboard [save|path] — write a local project onboarding guide{RESET}");
    println!("{DIM}  /onboarding gist [public|secret] [filename.md] — publish the onboarding guide with gh{RESET}");
    println!("{DIM}  /agents   — list named sub-agents (/agents open shows agent paths){RESET}");
    println!("{DIM}  /agents create [--worktree] <name> [description] — create a project sub-agent{RESET}");
    println!("{DIM}  /agents delete <name> — delete the active named sub-agent definition{RESET}");
    println!(
        "{DIM}  /agent [--worktree|--background] <name> <task> — run a named sub-agent task{RESET}"
    );
    println!("{DIM}  /agent --background <name> <task> — start a detached terminal agent and write a log under ~/.config/libertai/code-background-agents{RESET}");
    println!("{DIM}  /agents background [list|log|kill] — inspect or stop terminal background agents{RESET}");
    println!("{DIM}  /template <name> [args] — expand a prompt template{RESET}");
    println!("{DIM}  /theme [system|dark|light|high-contrast] — show terminal theme status{RESET}");
    println!("{DIM}  /export [path] — write this session transcript as Markdown{RESET}");
    println!("{DIM}  /share [path] — write this session transcript as shareable HTML{RESET}");
    println!("{DIM}  /share gist [public|secret] [filename.html] — publish the HTML transcript with gh{RESET}");
    println!("{DIM}  /output-style <default|concise|explanatory|review|status>{RESET}");
    println!("{DIM}  /vim      — show Vim-input status{RESET}");
    println!("{DIM}  /ide      — show IDE integration status{RESET}");
    println!("{DIM}  /bug      — print a bug report template{RESET}");
    println!("{DIM}  /clear    — wipe the screen and start a fresh session (also /new){RESET}");
    println!("{DIM}  /forget   — clear saved allow rules{RESET}");
    println!("{DIM}  /remember [kind:] <text> — append typed project memory (user, feedback, project, reference){RESET}");
    println!("{DIM}  !<cmd>    — run a local shell command in this cwd{RESET}");
    println!("{DIM}  ↑ / ↓     — walk through previously submitted prompts{RESET}");
    println!("{DIM}  ← / →     — move cursor in the current line{RESET}");
    println!("{DIM}  Ctrl+C    — cancel the line / interrupt streaming{RESET}");
    println!();
}

fn model_usage_text() -> &'static str {
    "/model [status|list|next|prev|model|provider/model]"
}

fn scoped_models_usage_text() -> &'static str {
    "/scoped-models <patterns|clear> — filter /model list and /model next|prev"
}

fn hotkey_lines() -> &'static [&'static str] {
    &[
        "Shift+Tab — cycle default / acceptEdits / plan modes",
        "Up / Down — walk submitted prompt history",
        "Left / Right — move cursor in the current line",
        "Backspace / Delete — edit the current line",
        "Home / End — jump to start or end of the line",
        "Enter — submit the current line",
        "Ctrl+C — clear the current line or interrupt streaming",
        "Ctrl+D — exit when the line is empty",
    ]
}

fn print_hotkeys() {
    println!("{BOLD}hotkeys{RESET}");
    for line in hotkey_lines() {
        println!("{DIM}  {line}{RESET}");
    }
}

fn print_project_tree(path: Option<&str>) {
    let root = match tree_root(path) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("{DIM}  /tree: {e:#}{RESET}");
            return;
        }
    };
    match render_project_tree(&root, TREE_MAX_ENTRIES) {
        Ok(tree) => print!("{tree}"),
        Err(e) => eprintln!("{DIM}  /tree: {e:#}{RESET}"),
    }
}

fn tree_root(path: Option<&str>) -> Result<PathBuf> {
    let raw = path.unwrap_or("").trim();
    if raw.is_empty() {
        return std::env::current_dir().context("resolve current directory");
    }
    Ok(PathBuf::from(raw))
}

fn render_project_tree(root: &Path, max_entries: usize) -> Result<String> {
    let meta = std::fs::metadata(root).with_context(|| format!("read {}", root.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let mut out = format!("{BOLD}{name}/{RESET}\n");
    let mut remaining = max_entries;
    render_tree_children(root, "", &mut remaining, &mut out)?;
    if remaining == 0 {
        out.push_str(&format!("{DIM}... truncated after {max_entries} entries{RESET}\n"));
    }
    Ok(out)
}

fn render_tree_children(
    dir: &Path,
    prefix: &str,
    remaining: &mut usize,
    out: &mut String,
) -> Result<()> {
    if *remaining == 0 {
        return Ok(());
    }
    let mut entries = tree_entries(dir)?;
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    let len = entries.len();
    for (idx, entry) in entries.into_iter().enumerate() {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        let is_last = idx + 1 == len;
        let connector = if is_last { "`-- " } else { "|-- " };
        let suffix = if entry.is_dir { "/" } else { "" };
        out.push_str(prefix);
        out.push_str(connector);
        out.push_str(&entry.name);
        out.push_str(suffix);
        out.push('\n');
        if entry.is_dir && !entry.is_symlink {
            let child_prefix = if is_last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}|   ")
            };
            render_tree_children(&entry.path, &child_prefix, remaining, out)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    is_symlink: bool,
}

fn tree_entries(dir: &Path) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip_tree_entry(&name) {
            continue;
        }
        let file_type = entry.file_type()?;
        entries.push(TreeEntry {
            name,
            path: entry.path(),
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
        });
    }
    Ok(entries)
}

fn should_skip_tree_entry(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "target"
            | "node_modules"
            | ".next"
            | ".nuxt"
            | "dist"
            | "build"
            | ".venv"
            | "__pycache__"
    )
}

fn parse_changelog_limit(input: &str) -> Result<usize> {
    let value = input.trim();
    if value.is_empty() {
        return Ok(CHANGELOG_DEFAULT_LIMIT);
    }
    let limit = value
        .parse::<usize>()
        .context("usage: /changelog [count]")?
        .clamp(1, CHANGELOG_MAX_LIMIT);
    Ok(limit)
}

fn print_changelog(limit: usize) {
    match recent_git_commits(limit) {
        Ok(lines) if lines.is_empty() => println!("{DIM}  /changelog: no commits found.{RESET}"),
        Ok(lines) => {
            println!("{BOLD}changelog{RESET}");
            for line in lines {
                println!("  {line}");
            }
        }
        Err(e) => eprintln!("{DIM}  /changelog: {e:#}{RESET}"),
    }
}

fn print_sandbox_status(action: &str) {
    match parse_sandbox_action(action) {
        SandboxAction::Info => {
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(e) => {
                    eprintln!("{DIM}  /sandbox: could not resolve cwd: {e}{RESET}");
                    return;
                }
            };
            let profile = detect_strict_profile(&cwd);
            print!("{}", format_profile_text(&profile));
        }
        SandboxAction::Reload => {
            println!(
                "{DIM}  /sandbox reload: CLI sandbox policy is fixed when `libertai code` starts. Exit and restart with the desired --sandbox mode or policy settings.{RESET}"
            );
        }
        SandboxAction::Unknown(value) => {
            eprintln!("{DIM}  unknown /sandbox action: {value}. try \"info\" or \"reload\".{RESET}");
        }
    }
}

fn abort_status_message() -> String {
    format!(
        "{DIM}  no active turn to abort. Press Ctrl+C while the assistant is streaming to interrupt the running turn.{RESET}"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxAction<'a> {
    Info,
    Reload,
    Unknown(&'a str),
}

fn parse_sandbox_action(raw: &str) -> SandboxAction<'_> {
    let value = raw.trim();
    if value.is_empty()
        || value.eq_ignore_ascii_case("info")
        || value.eq_ignore_ascii_case("status")
        || value.eq_ignore_ascii_case("show")
    {
        SandboxAction::Info
    } else if value.eq_ignore_ascii_case("reload") {
        SandboxAction::Reload
    } else {
        SandboxAction::Unknown(value)
    }
}

fn recent_git_commits(limit: usize) -> Result<Vec<String>> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    recent_git_commits_in(&cwd, limit)
}

fn recent_git_commits_in(cwd: &Path, limit: usize) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("log")
        .arg(format!("-n{}", limit.clamp(1, CHANGELOG_MAX_LIMIT)))
        .arg("--oneline")
        .arg("--decorate")
        .output()
        .context("run git log")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("not a git repository");
        }
        anyhow::bail!("{}", message);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn git_status_short_in(cwd: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("status")
        .arg("--short")
        .arg("--branch")
        .output()
        .context("run git status")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("not a git repository");
        }
        anyhow::bail!("{}", message);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

async fn copy_last_assistant(handle: &AgentSessionHandle) {
    let messages = match handle.messages().await {
        Ok(messages) => messages,
        Err(e) => {
            eprintln!("{DIM}  /copy: could not read transcript: {e:#}{RESET}");
            return;
        }
    };
    let Some(text) = last_assistant_text(&messages) else {
        println!("{DIM}  /copy: no assistant response to copy yet.{RESET}");
        return;
    };
    if text.len() > OSC52_MAX_TEXT_BYTES {
        eprintln!(
            "{DIM}  /copy: last response is too large for terminal clipboard copy ({} bytes, max {}).{RESET}",
            text.len(),
            OSC52_MAX_TEXT_BYTES
        );
        return;
    }
    let sequence = osc52_sequence(&text);
    print!("{sequence}");
    let _ = io::stdout().flush();
    println!("{DIM}  copied last assistant response to terminal clipboard.{RESET}");
}

fn last_assistant_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        let Message::Assistant(assistant) = message else {
            return None;
        };
        let mut out = String::new();
        for block in &assistant.content {
            if let ContentBlock::Text(text) = block {
                out.push_str(&text.text);
                if !text.text.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        let text = out.trim_end().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    })
}

fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", BASE64_STANDARD.encode(text.as_bytes()))
}

fn history_store_path() -> Result<PathBuf> {
    Ok(crate::config::libertai_config_dir()?.join("code-history.json"))
}

fn load_input_history(path: &Path) -> Result<VecDeque<String>> {
    if !path.exists() {
        return Ok(VecDeque::with_capacity(HISTORY_MAX_LIMIT));
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading history {}", path.display()))?;
    let items: Vec<String> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing history {}", path.display()))?;
    let mut history = VecDeque::with_capacity(HISTORY_MAX_LIMIT);
    for item in items
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
    {
        if history.back() == Some(&item) {
            continue;
        }
        if history.len() == HISTORY_MAX_LIMIT {
            history.pop_front();
        }
        history.push_back(item);
    }
    Ok(history)
}

fn persist_input_history_if_configured(
    path: Option<&Path>,
    history: &VecDeque<String>,
) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    persist_input_history(path, history)
}

fn persist_input_history(path: &Path, history: &VecDeque<String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating history dir {}", parent.display()))?;
    }
    let items: Vec<&str> = history.iter().map(String::as_str).collect();
    let raw = serde_json::to_string_pretty(&items).context("serializing history")?;
    fs::write(path, format!("{raw}\n"))
        .with_context(|| format!("writing history {}", path.display()))?;
    Ok(())
}

fn parse_history_limit(input: &str) -> Result<usize> {
    let value = input.trim();
    if value.is_empty() {
        return Ok(HISTORY_DEFAULT_LIMIT);
    }
    let limit = value
        .parse::<usize>()
        .context("usage: /history [count]")?
        .clamp(1, HISTORY_MAX_LIMIT);
    Ok(limit)
}

fn print_history(history: &VecDeque<String>, limit: usize) {
    println!("{BOLD}history{RESET}");
    if history.is_empty() {
        println!("{DIM}  no submitted prompts yet.{RESET}");
        return;
    }
    let shown = history.len().min(limit);
    let start = history.len() - shown;
    for (idx, item) in history.iter().enumerate().skip(start) {
        println!("{DIM}  {:>2}.{RESET} {}", idx + 1, item);
    }
}

fn image_command_arg(trimmed: &str) -> Option<(&'static str, &str)> {
    for command in ["/image", "/attach"] {
        if let Some(rest) = trimmed.strip_prefix(command) {
            if rest.starts_with(char::is_whitespace) {
                return Some((command, rest.trim_start()));
            }
        }
    }
    None
}

fn mention_command_arg(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("/mention")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim_start)
}

fn build_mention_prompt(input: &str, output_style: Option<&str>) -> Result<String> {
    let (path, prompt) = parse_mention_prompt(input)?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() > MENTION_ATTACHMENT_MAX_BYTES {
        anyhow::bail!(
            "{} is too large ({} bytes); max is {} bytes",
            path.display(),
            bytes.len(),
            MENTION_ATTACHMENT_MAX_BYTES
        );
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8 text", path.display()))?;
    let prompt = if prompt.is_empty() {
        "Please use this file as context.".to_string()
    } else {
        prompt
    };
    let body = format!(
        "{}\n\nMentioned file: `{}`\n\n```text\n{}\n```",
        prompt,
        path.display(),
        text
    );
    Ok(apply_output_style(output_style, &body))
}

fn parse_mention_prompt(input: &str) -> Result<(PathBuf, String)> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("usage: /mention <path> [prompt]");
    }
    let (path, rest) = parse_path_and_rest(input)?;
    if path.as_os_str().is_empty() {
        anyhow::bail!("usage: /mention <path> [prompt]");
    }
    Ok((path, rest.trim().to_string()))
}

fn build_image_prompt_content(
    input: &str,
    output_style: Option<&str>,
) -> Result<Vec<ContentBlock>> {
    let (path, prompt) = parse_image_prompt(input)?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() > IMAGE_ATTACHMENT_MAX_BYTES {
        anyhow::bail!(
            "{} is too large ({} bytes); max is {} bytes",
            path.display(),
            bytes.len(),
            IMAGE_ATTACHMENT_MAX_BYTES
        );
    }
    let mime_type = detect_supported_image_mime_type(&bytes)
        .ok_or_else(|| anyhow::anyhow!("unsupported image type; use PNG, JPEG, GIF, or WebP"))?;
    let prompt = if prompt.is_empty() {
        "Please analyze this image.".to_string()
    } else {
        prompt
    };
    Ok(vec![
        ContentBlock::Text(TextContent::new(apply_output_style(output_style, &prompt))),
        ContentBlock::Image(ImageContent {
            data: BASE64_STANDARD.encode(bytes),
            mime_type: mime_type.to_string(),
        }),
    ])
}

fn parse_image_prompt(input: &str) -> Result<(PathBuf, String)> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("usage: /image <path> [prompt]");
    }
    let (path, rest) = parse_path_and_rest(input)?;
    if path.as_os_str().is_empty() {
        anyhow::bail!("usage: /image <path> [prompt]");
    }
    Ok((path, rest.trim().to_string()))
}

fn parse_path_and_rest(input: &str) -> Result<(PathBuf, &str)> {
    let Some(first) = input.chars().next() else {
        anyhow::bail!("usage: /image <path> [prompt]");
    };
    if first == '"' || first == '\'' {
        let quote = first;
        let mut path = String::new();
        let mut escaped = false;
        let mut end = None;
        for (idx, ch) in input.char_indices().skip(1) {
            if escaped {
                path.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                end = Some(idx + ch.len_utf8());
                break;
            } else {
                path.push(ch);
            }
        }
        let Some(end) = end else {
            anyhow::bail!("unterminated quoted image path");
        };
        return Ok((PathBuf::from(path), &input[end..]));
    }

    match input.find(char::is_whitespace) {
        Some(idx) => Ok((PathBuf::from(&input[..idx]), &input[idx..])),
        None => Ok((PathBuf::from(input), "")),
    }
}

fn detect_supported_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn parse_permissions_command(input: &str) -> PermissionsCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "show" | "status" => PermissionsCommand::Show,
        "open" | "settings" | "approvals" => PermissionsCommand::Open,
        "default" | "normal" => PermissionsCommand::Set(Mode::Normal),
        "acceptedits" | "accept-edits" | "accept_edits" => {
            PermissionsCommand::Set(Mode::AcceptEdits)
        }
        "plan" => PermissionsCommand::Set(Mode::Plan),
        "forget" | "clear" => PermissionsCommand::Forget,
        "bypass" | "bypasspermissions" | "bypass-permissions" | "bypass_permissions" => {
            PermissionsCommand::UnsupportedBypass
        }
        _ => PermissionsCommand::Show,
    }
}

fn status_line_command_arg(input: &str) -> Option<&str> {
    for command in ["/statusline", "/status-line"] {
        if let Some(rest) = input.strip_prefix(&format!("{command} ")) {
            return Some(rest.trim());
        }
    }
    None
}

fn normalize_status_line_template(value: &str) -> String {
    value
        .trim()
        .chars()
        .take(STATUS_LINE_TEMPLATE_MAX_CHARS)
        .collect()
}

fn normalize_status_line_command(value: &str) -> String {
    value
        .trim()
        .chars()
        .take(STATUS_LINE_COMMAND_MAX_CHARS)
        .collect()
}

fn clear_status_line_command_cache() {
    if let Some(cache) = STATUS_LINE_COMMAND_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

fn status_line_help() -> String {
    format!(
        "tokens: {}",
        STATUS_LINE_TOKENS
            .iter()
            .map(|name| format!("{{{name}}}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn expand_status_line_template(
    template: &str,
    status: &BarStatus,
    mode: Mode,
) -> Option<String> {
    let template = normalize_status_line_template(template);
    if template.is_empty() {
        return None;
    }
    let cwd = std::env::current_dir().ok();
    let path = cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let project = cwd
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("-");
    let backend = status
        .model_label
        .split_once('/')
        .map(|(provider, _)| provider)
        .unwrap_or(status.model_label.as_str());
    let model = status
        .model_label
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(status.model_label.as_str());
    let ctx = if status.context_window > 0 {
        format!("{}%", context_percent(status.input_tokens, status.context_window))
    } else {
        "-".to_string()
    };
    let output_style = status.output_style.as_deref().unwrap_or("default");

    Some(replace_status_line_tokens(&template, |name| match name {
        "project" => Some(project.to_string()),
        "path" => Some(path.clone()),
        "session" => Some("cli".to_string()),
        "backend" => Some(backend.to_string()),
        "model" => Some(model.to_string()),
        "mode" => Some(mode_label(mode).to_string()),
        "style" => Some(output_style.to_string()),
        "tokens" => Some(human_tokens(status.input_tokens)),
        "ctx" => Some(ctx.clone()),
        "live" => Some("idle".to_string()),
        _ => None,
    }))
}

fn replace_status_line_tokens(
    template: &str,
    mut value_for: impl FnMut(&str) -> Option<String>,
) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '{' {
            out.push(ch);
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        while let Some(next) = chars.next() {
            if next == '}' {
                closed = true;
                break;
            }
            name.push(next);
        }
        if closed && is_status_line_token_name(&name) {
            let normalized = name.replace('-', "_");
            match value_for(&normalized) {
                Some(value) if !value.trim().is_empty() => out.push_str(value.trim()),
                Some(_) => out.push('-'),
                None => {
                    out.push('{');
                    out.push_str(&name);
                    out.push('}');
                }
            }
        } else {
            out.push('{');
            out.push_str(&name);
            if closed {
                out.push('}');
            }
        }
    }
    out
}

fn is_status_line_token_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn print_permissions_status(mode: Mode) {
    println!("{DIM}  permission mode: {}{RESET}", mode_label(mode));
    println!("{DIM}  supported: default, acceptEdits, plan{RESET}");
    println!("{DIM}  native bypassPermissions is intentionally unavailable.{RESET}");
    println!("{DIM}  use /permissions forget to clear saved allow rules.{RESET}");
    println!("{DIM}  use /permissions open to show the approvals settings target and rule path.{RESET}");
    println!("{DIM}  use /permissions bypassPermissions to explain the native safety stance.{RESET}");
}

fn print_permissions_open_hint() {
    println!("{BOLD}permissions{RESET}");
    println!("{DIM}  desktop: /permissions open jumps to Settings > Approvals.{RESET}");
    match crate::config::allow_rules_path() {
        Ok(path) => println!(
            "{DIM}  terminal: remembered \"always allow\" rules are stored in {}{RESET}",
            path.display()
        ),
        Err(e) => println!("{DIM}  terminal: could not resolve allow-rules path: {e}{RESET}"),
    }
    println!("{DIM}  use /permissions forget to clear remembered terminal approval rules.{RESET}");
    println!();
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoginSlashTarget<'a> {
    Account,
    Status,
    Provider(&'a str),
}

fn parse_login_slash_target(query: &str) -> LoginSlashTarget<'_> {
    let raw = query.trim();
    if raw.is_empty() {
        return LoginSlashTarget::Account;
    }
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "status" | "show" | "info" => LoginSlashTarget::Status,
        "libertai" | "account" | "key" | "api-key" | "api" => LoginSlashTarget::Account,
        _ => LoginSlashTarget::Provider(raw),
    }
}

fn handle_login_slash(query: &str, cfg: &LibertaiConfig) {
    match parse_login_slash_target(query) {
        LoginSlashTarget::Status => print_login_status(cfg),
        LoginSlashTarget::Account => {
            println!("{BOLD}login{RESET}");
            println!("{DIM}  LibertAI API key:{RESET} {}", login_key_state(cfg));
            println!(
                "{DIM}  use /login with no arguments to run the interactive LibertAI login flow.{RESET}"
            );
        }
        LoginSlashTarget::Provider(provider) => print_provider_login_note(provider, cfg),
    }
}

fn handle_logout_slash(query: &str, cfg: &LibertaiConfig) {
    match parse_login_slash_target(query) {
        LoginSlashTarget::Status => print_login_status(cfg),
        LoginSlashTarget::Account => {
            println!("{BOLD}logout{RESET}");
            println!(
                "{DIM}  use /logout with no arguments to back up and remove the LibertAI config.{RESET}"
            );
        }
        LoginSlashTarget::Provider(provider) => {
            println!("{BOLD}logout{RESET}");
            println!(
                "{DIM}  provider-specific logout for `{provider}` is managed by the desktop backend settings, not the terminal CLI config.{RESET}"
            );
            println!(
                "{DIM}  terminal CLI only stores the LibertAI API key: {}{RESET}",
                login_key_state(cfg)
            );
        }
    }
}

fn print_login_status(cfg: &LibertaiConfig) {
    println!("{BOLD}login{RESET}");
    println!("{DIM}  LibertAI API key:{RESET} {}", login_key_state(cfg));
    println!(
        "{DIM}  wallet:{RESET} {}",
        cfg.auth
            .wallet_address
            .as_deref()
            .map(mask_key)
            .unwrap_or_else(|| "missing".to_string())
    );
    println!(
        "{DIM}  chain:{RESET} {}",
        cfg.auth.chain.as_deref().unwrap_or("missing")
    );
    println!(
        "{DIM}  providers:{RESET} terminal CLI stores only LibertAI credentials; use desktop Settings > Backends for Anthropic, Google, Copilot, GitLab, Vertex, and other provider keys."
    );
}

fn login_key_state(cfg: &LibertaiConfig) -> String {
    cfg.auth
        .api_key
        .as_deref()
        .map(mask_key)
        .unwrap_or_else(|| "missing".to_string())
}

fn print_provider_login_note(provider: &str, cfg: &LibertaiConfig) {
    println!("{BOLD}login{RESET}");
    println!(
        "{DIM}  provider `{provider}` is not stored in the terminal CLI config.{RESET}"
    );
    println!(
        "{DIM}  use the desktop `/login {provider}` flow or Settings > Backends for provider-specific credentials.{RESET}"
    );
    println!("{DIM}  terminal LibertAI API key:{RESET} {}", login_key_state(cfg));
}

fn parse_model_spec(current_provider: &str, input: &str) -> Result<(String, String)> {
    let spec = input.trim();
    if spec.is_empty() {
        anyhow::bail!("usage: /model <model|provider/model>");
    }
    let (provider, model) = match spec.split_once('/') {
        Some((provider, model)) => (provider.trim(), model.trim()),
        None => (current_provider.trim(), spec),
    };
    if provider.is_empty() || model.is_empty() {
        anyhow::bail!("usage: /model <model|provider/model>");
    }
    Ok((provider.to_string(), model.to_string()))
}

fn parse_model_slash_command(input: &str) -> ModelSlashCommand<'_> {
    let raw = input.trim();
    match raw.to_ascii_lowercase().as_str() {
        "" | "status" | "show" | "current" => ModelSlashCommand::Status,
        "list" | "ls" => ModelSlashCommand::List,
        "next" | "cycle" => ModelSlashCommand::Next,
        "prev" | "previous" | "back" => ModelSlashCommand::Previous,
        _ => ModelSlashCommand::Set(raw),
    }
}

fn scoped_models_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/scoped-models" | "/scoped" => Some(""),
        _ => trimmed
            .strip_prefix("/scoped-models ")
            .or_else(|| trimmed.strip_prefix("/scoped "))
            .map(str::trim),
    }
}

fn parse_scoped_models_command(input: &str) -> ScopedModelsCommand {
    let raw = input.trim();
    if raw.is_empty() || matches!(raw.to_ascii_lowercase().as_str(), "status" | "show") {
        return ScopedModelsCommand::Status;
    }
    if matches!(raw.to_ascii_lowercase().as_str(), "clear" | "reset" | "off") {
        return ScopedModelsCommand::Clear;
    }
    let patterns = parse_scoped_model_patterns(raw);
    if patterns.is_empty() {
        ScopedModelsCommand::Usage
    } else {
        ScopedModelsCommand::Set(patterns)
    }
}

fn parse_scoped_model_patterns(input: &str) -> Vec<String> {
    input
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn handle_scoped_models_command(raw: &str, scoped_model_patterns: &mut Vec<String>) {
    match parse_scoped_models_command(raw) {
        ScopedModelsCommand::Status => print_scoped_model_status(scoped_model_patterns),
        ScopedModelsCommand::Clear => {
            scoped_model_patterns.clear();
            println!("{DIM}  scoped models cleared; /model list shows all discovered models.{RESET}");
        }
        ScopedModelsCommand::Set(patterns) => {
            *scoped_model_patterns = patterns;
            print_scoped_model_status(scoped_model_patterns);
        }
        ScopedModelsCommand::Usage => {
            eprintln!("{DIM}  usage: /scoped-models <pattern[,pattern...]|clear>{RESET}");
        }
    }
}

fn print_scoped_model_status(scoped_model_patterns: &[String]) {
    println!("{BOLD}scoped models{RESET}");
    if scoped_model_patterns.is_empty() {
        println!("{DIM}  patterns:{RESET} (all models)");
    } else {
        println!("{DIM}  patterns:{RESET}");
        for pattern in scoped_model_patterns {
            println!("{DIM}  -{RESET} {pattern}");
        }
    }
    println!(
        "{DIM}  usage:{RESET} /scoped-models qwen* gemma*, /scoped-models clear, /model list, /model next|prev"
    );
    println!();
}

fn print_model_status(
    handle: &AgentSessionHandle,
    cfg: &LibertaiConfig,
    scoped_model_patterns: &[String],
) {
    let (provider, model) = handle.model();
    println!("{BOLD}model{RESET}");
    println!("{DIM}  current:{RESET} {provider}/{model}");
    println!(
        "{DIM}  default:{RESET} {}/{}",
        cfg.default_code_provider, cfg.default_code_model
    );
    if scoped_model_patterns.is_empty() {
        println!("{DIM}  scoped models:{RESET} (all models)");
    } else {
        println!("{DIM}  scoped models:{RESET} {}", scoped_model_patterns.join(", "));
    }
    println!("{DIM}  usage:{RESET} {}", model_usage_text());
}

fn print_model_list(cfg: &LibertaiConfig, provider: &str, scoped_model_patterns: &[String]) {
    match crate::client::list_models(cfg) {
        Ok(list) => {
            let ids: Vec<String> = list.data.into_iter().map(|entry| entry.id).collect();
            let scoped = scoped_model_ids(provider, &ids, scoped_model_patterns);
            println!("{BOLD}models{RESET}");
            if !scoped_model_patterns.is_empty() {
                println!("{DIM}  scope:{RESET} {}", scoped_model_patterns.join(", "));
            }
            for id in scoped {
                println!("{DIM}  -{RESET} {provider}/{id}");
            }
            println!();
        }
        Err(e) => eprintln!("{DIM}  /model list: {e:#}{RESET}"),
    }
}

fn next_scoped_model(
    cfg: &LibertaiConfig,
    provider: &str,
    current_model: &str,
    scoped_model_patterns: &[String],
    direction: isize,
) -> Result<Option<String>> {
    let ids: Vec<String> = crate::client::list_models(cfg)?
        .data
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    Ok(cycle_scoped_model(
        provider,
        current_model,
        &ids,
        scoped_model_patterns,
        direction,
    ))
}

fn cycle_scoped_model(
    provider: &str,
    current_model: &str,
    ids: &[String],
    scoped_model_patterns: &[String],
    direction: isize,
) -> Option<String> {
    let scoped = scoped_model_ids(provider, ids, scoped_model_patterns);
    if scoped.len() < 2 {
        return None;
    }
    let current_idx = scoped.iter().position(|candidate| candidate == current_model);
    let base = current_idx.unwrap_or_else(|| if direction < 0 { 0 } else { scoped.len() - 1 });
    let len = scoped.len() as isize;
    let next = (base as isize + direction).rem_euclid(len) as usize;
    scoped.get(next).cloned()
}

fn scoped_model_ids(
    provider: &str,
    ids: &[String],
    scoped_model_patterns: &[String],
) -> Vec<String> {
    if scoped_model_patterns.is_empty() {
        return ids.to_vec();
    }
    let matched: Vec<String> = ids
        .iter()
        .filter(|id| {
            scoped_model_patterns
                .iter()
                .any(|pattern| model_matches_scoped_pattern(provider, id, pattern))
        })
        .cloned()
        .collect();
    if matched.is_empty() {
        ids.to_vec()
    } else {
        matched
    }
}

fn model_matches_scoped_pattern(provider: &str, model_id: &str, pattern: &str) -> bool {
    glob_match_case_insensitive(pattern, model_id)
        || glob_match_case_insensitive(pattern, &format!("{provider}/{model_id}"))
}

fn glob_match_case_insensitive(pattern: &str, value: &str) -> bool {
    glob_match(
        &pattern.to_ascii_lowercase().chars().collect::<Vec<_>>(),
        &value.to_ascii_lowercase().chars().collect::<Vec<_>>(),
    )
}

fn glob_match(pattern: &[char], value: &[char]) -> bool {
    let (mut p, mut v) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_value = 0usize;
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            star_value = v;
        } else if let Some(star_idx) = star {
            p = star_idx + 1;
            star_value += 1;
            v = star_value;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn parse_session_name(input: &str) -> Result<String> {
    let name = input.trim();
    if name.is_empty() {
        anyhow::bail!("usage: /name <name>");
    }
    if name.chars().count() > 120 {
        anyhow::bail!("session name must be 120 characters or fewer");
    }
    Ok(name.to_string())
}

fn print_name_status(name: Option<&str>) {
    println!("{BOLD}name{RESET}");
    match name {
        Some(name) => println!("{DIM}  current:{RESET} {name}"),
        None => println!("{DIM}  current:{RESET} unnamed or not loaded in this REPL"),
    }
    println!("{DIM}  usage:{RESET} /name <name>");
}

async fn export_transcript(handle: &AgentSessionHandle, path: Option<&str>) {
    let path = match export_path(path) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("{DIM}  /export: {e:#}{RESET}");
            return;
        }
    };
    let messages = match handle.messages().await {
        Ok(messages) => messages,
        Err(e) => {
            eprintln!("{DIM}  /export: could not read transcript: {e:#}{RESET}");
            return;
        }
    };
    let markdown = render_markdown_transcript(&messages);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("{DIM}  /export: could not create {}: {e}{RESET}", parent.display());
            return;
        }
    }
    match std::fs::write(&path, markdown) {
        Ok(()) => println!("{DIM}  exported transcript: {}{RESET}", path.display()),
        Err(e) => eprintln!("{DIM}  /export: could not write {}: {e}{RESET}", path.display()),
    }
}

async fn share_transcript(handle: &AgentSessionHandle, path: Option<&str>) {
    let target = match parse_share_target(path) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("{DIM}  /share: {e:#}{RESET}");
            return;
        }
    };
    let messages = match handle.messages().await {
        Ok(messages) => messages,
        Err(e) => {
            eprintln!("{DIM}  /share: could not read transcript: {e:#}{RESET}");
            return;
        }
    };
    let html = render_html_transcript(&messages);
    match target {
        ShareTarget::File(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "{DIM}  /share: could not create {}: {e}{RESET}",
                        parent.display()
                    );
                    return;
                }
            }
            match std::fs::write(&path, html) {
                Ok(()) => println!("{DIM}  share HTML written: {}{RESET}", path.display()),
                Err(e) => eprintln!("{DIM}  /share: could not write {}: {e}{RESET}", path.display()),
            }
        }
        ShareTarget::Gist { public, filename } => {
            match publish_gist(
                &html,
                public,
                &filename,
                "LibertAI Code shared transcript",
            ) {
                Ok(url) => println!("{DIM}  share gist created: {url}{RESET}"),
                Err(e) => eprintln!("{DIM}  /share gist: {e:#}{RESET}"),
            }
        }
    }
}

async fn compact_transcript(handle: &mut AgentSessionHandle, notes: Option<&str>) -> bool {
    let notes = notes.unwrap_or("").trim();
    println!("{DIM}  compacting conversation history...{RESET}");
    let instructions = (!notes.is_empty()).then_some(notes);
    match handle
        .compact_force_with_instructions(instructions, render_event)
        .await
    {
        Ok(()) => {
            println!("{DIM}  compact complete.{RESET}");
            true
        }
        Err(e) => {
            eprintln!("{DIM}  /compact: {e:#}{RESET}");
            false
        }
    }
}

fn export_path(path: Option<&str>) -> Result<PathBuf> {
    let raw = path.unwrap_or("").trim();
    let raw = strip_save_action(raw);
    if raw.is_empty() {
        return Ok(PathBuf::from("libertai-transcript.md"));
    }
    Ok(PathBuf::from(raw))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShareTarget {
    File(PathBuf),
    Gist { public: bool, filename: String },
}

fn parse_share_target(path: Option<&str>) -> Result<ShareTarget> {
    let raw = path.unwrap_or("").trim();
    let raw = strip_save_action(raw);
    if raw.is_empty() {
        return Ok(ShareTarget::File(PathBuf::from("libertai-share.html")));
    }
    let Some(rest) = raw.strip_prefix("gist") else {
        return Ok(ShareTarget::File(PathBuf::from(raw)));
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Ok(ShareTarget::File(PathBuf::from(raw)));
    }
    let mut public = false;
    let mut filename_parts = Vec::new();
    for part in rest.split_whitespace() {
        match part.to_ascii_lowercase().as_str() {
            "public" | "--public" => public = true,
            "secret" | "private" | "--secret" | "--private" => public = false,
            other if other.starts_with('-') => {
                anyhow::bail!("unknown gist option `{part}`; use /share gist [public|secret] [filename.html]");
            }
            _ => filename_parts.push(part),
        }
    }
    let filename = filename_parts.join("-");
    let filename = sanitize_gist_filename(if filename.trim().is_empty() {
        "libertai-share.html"
    } else {
        filename.trim()
    });
    Ok(ShareTarget::Gist { public, filename })
}

fn strip_save_action(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some((head, tail)) = split_first_word(trimmed) else {
        return "";
    };
    if matches!(
        head.to_ascii_lowercase().as_str(),
        "save" | "file" | "download" | "write"
    ) {
        tail.trim_start()
    } else {
        trimmed
    }
}

fn sanitize_gist_filename(raw: &str) -> String {
    let name = Path::new(raw)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("libertai-share.html")
        .trim();
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "libertai-share.html".to_string()
    } else {
        out.chars().take(96).collect()
    }
}

fn publish_gist(content: &str, public: bool, filename: &str, desc: &str) -> Result<String> {
    let mut child = Command::new("gh")
        .args(["gist", "create", "-"])
        .arg("--filename")
        .arg(filename)
        .arg("--desc")
        .arg(desc)
        .arg(if public { "--public" } else { "--secret" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start `gh`; install GitHub CLI and run `gh auth login`")?;
    child
        .stdin
        .as_mut()
        .context("could not open gh stdin")?
        .write_all(content.as_bytes())
        .context("could not send content to gh")?;
    let output = child
        .wait_with_output()
        .context("gh gist create did not finish")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "gh gist create failed{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

fn compact_command_notes(trimmed: &str) -> Option<&str> {
    trimmed.strip_prefix("/compact ").map(str::trim)
}

fn loop_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/loop" | "/autoloop" => Some(""),
        _ => trimmed
            .strip_prefix("/loop ")
            .or_else(|| trimmed.strip_prefix("/autoloop "))
            .map(str::trim),
    }
}

fn auto_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/auto" | "/autorun" | "/continuous" => Some(""),
        _ => trimmed
            .strip_prefix("/auto ")
            .or_else(|| trimmed.strip_prefix("/autorun "))
            .or_else(|| trimmed.strip_prefix("/continuous "))
            .map(str::trim),
    }
}

fn schedule_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/schedule" | "/cron" => Some(""),
        _ => trimmed
            .strip_prefix("/schedule ")
            .or_else(|| trimmed.strip_prefix("/cron "))
            .map(str::trim),
    }
}

fn notify_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/notify" | "/notifications" => Some(""),
        _ => trimmed
            .strip_prefix("/notify ")
            .or_else(|| trimmed.strip_prefix("/notifications "))
            .map(str::trim),
    }
}

fn hooks_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/hooks" => Some(""),
        _ => trimmed.strip_prefix("/hooks ").map(str::trim),
    }
}

fn mcp_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/mcp" => Some(""),
        _ => trimmed.strip_prefix("/mcp ").map(str::trim),
    }
}

fn onboarding_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/onboarding" | "/onboard" => Some(""),
        _ => trimmed
            .strip_prefix("/onboarding ")
            .or_else(|| trimmed.strip_prefix("/onboard "))
            .map(str::trim),
    }
}

fn send_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/send" | "/send-message" => Some(""),
        _ => trimmed
            .strip_prefix("/send ")
            .or_else(|| trimmed.strip_prefix("/send-message "))
            .map(str::trim),
    }
}

fn theme_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/theme" => Some(""),
        _ => trimmed.strip_prefix("/theme ").map(str::trim),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HooksCommand {
    Status,
    Open,
    Usage,
}

fn parse_hooks_command(input: &str) -> HooksCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "list" | "state" | "diagnostics" | "diag" => HooksCommand::Status,
        "open" | "settings" | "edit" => HooksCommand::Open,
        _ => HooksCommand::Usage,
    }
}

fn parse_mcp_command(input: &str) -> McpCommand {
    let normalized = input.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" | "status" | "list" | "state" | "diagnostics" | "diag" => McpCommand::Status,
        "probe" | "probes" => McpCommand::Probe,
        "refresh" | "probe --save" | "probe save" | "probe --write" | "probe write" => {
            McpCommand::ProbeSave
        }
        "reset" | "reset-sessions" => McpCommand::Reset,
        "open" | "settings" | "edit" => McpCommand::Open,
        _ => McpCommand::Usage,
    }
}

fn parse_notify_command(input: &str) -> NotifyCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" => NotifyCommand::Status,
        "on" | "enable" | "enabled" => NotifyCommand::On,
        "off" | "disable" | "disabled" => NotifyCommand::Off,
        "test" | "ping" => NotifyCommand::Test,
        _ => NotifyCommand::Usage,
    }
}

fn handle_notify_command(raw: &str, cfg: &mut Arc<LibertaiConfig>) -> Result<()> {
    match parse_notify_command(raw) {
        NotifyCommand::Status => print_notify_status(cfg),
        NotifyCommand::On => {
            set_turn_notifications(cfg, true)?;
            println!("{DIM}  /notify: turn-complete terminal notifications enabled.{RESET}");
        }
        NotifyCommand::Off => {
            set_turn_notifications(cfg, false)?;
            println!("{DIM}  /notify: turn-complete terminal notifications disabled.{RESET}");
        }
        NotifyCommand::Test => {
            crate::commands::code_term::notify_terminal("LibertAI Code", "Notification test");
        }
        NotifyCommand::Usage => {
            eprintln!("{DIM}  usage: /notify [on|off|status|test]{RESET}");
        }
    }
    Ok(())
}

fn set_turn_notifications(cfg: &mut Arc<LibertaiConfig>, enabled: bool) -> Result<()> {
    let mut next = cfg.as_ref().clone();
    next.code_turn_notifications = enabled;
    crate::config::save(&next).context("save config")?;
    *cfg = Arc::new(next);
    Ok(())
}

fn print_notify_status(cfg: &LibertaiConfig) {
    println!(
        "{DIM}  turn notifications:{RESET} {}",
        if cfg.code_turn_notifications {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "{DIM}  agent push notifications:{RESET} terminal bell + visible notification block"
    );
    println!("{DIM}  usage:{RESET} /notify on, /notify off, /notify status, /notify test");
}

fn print_send_status(rest: &str) {
    let requested = rest.trim();
    println!("{BOLD}send message{RESET}");
    println!(
        "{DIM}  desktop:{RESET} /send <session> <message> can relay prompts into another open idle desktop session."
    );
    println!(
        "{DIM}  terminal:{RESET} this REPL has one active session and no desktop session registry to target."
    );
    if !requested.is_empty() {
        println!(
            "{DIM}  ignored target/message:{RESET} {}",
            requested.replace('\n', " ")
        );
    }
    println!(
        "{DIM}  remaining gap:{RESET} pi-level streaming child-agent bus or detached inter-agent scheduler."
    );
}

fn print_theme_status(rest: &str) {
    let requested = rest.trim();
    println!("{BOLD}theme{RESET}");
    println!(
        "{DIM}  desktop:{RESET} /theme system|dark|light|high-contrast updates the app appearance."
    );
    println!(
        "{DIM}  terminal:{RESET} colors are controlled by your terminal emulator; libertai code uses ANSI styling only."
    );
    if !requested.is_empty() {
        println!("{DIM}  requested theme:{RESET} {requested}");
    }
}

fn parse_schedule_command(input: &str) -> ScheduleCommand {
    let raw = input.trim();
    if raw.is_empty() {
        return ScheduleCommand::Status;
    }
    let mut parts = raw.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match head {
        "list" | "status" | "state" => ScheduleCommand::Status,
        "clear" | "stop" => ScheduleCommand::Clear,
        "cancel" | "delete" | "rm" => {
            if rest.is_empty() || rest.split_whitespace().nth(1).is_some() {
                ScheduleCommand::Usage
            } else {
                ScheduleCommand::Cancel(rest.to_string())
            }
        }
        "in" => parse_schedule_add(rest),
        _ => parse_schedule_add(raw),
    }
}

fn parse_schedule_add(input: &str) -> ScheduleCommand {
    let raw = input.trim();
    let mut parts = raw.splitn(2, char::is_whitespace);
    let Some(delay_token) = parts.next().filter(|value| !value.trim().is_empty()) else {
        return ScheduleCommand::Usage;
    };
    let prompt = parts.next().unwrap_or("").trim();
    let Some(delay) = parse_schedule_delay(delay_token) else {
        return ScheduleCommand::Usage;
    };
    if prompt.is_empty() {
        return ScheduleCommand::Usage;
    }
    ScheduleCommand::Add {
        delay,
        prompt: prompt.to_string(),
    }
}

fn parse_schedule_delay(value: &str) -> Option<Duration> {
    let raw = value.trim().to_ascii_lowercase();
    let split_at = raw
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .filter(|idx| *idx > 0)?;
    let (amount, unit) = raw.split_at(split_at);
    if amount.matches('.').count() > 1 || unit.is_empty() {
        return None;
    }
    let amount = amount.parse::<f64>().ok()?;
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }
    let scale_ms = match unit {
        "ms" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
        "d" | "day" | "days" => 86_400_000.0,
        _ => return None,
    };
    let ms = (amount * scale_ms).round();
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    Some(Duration::from_millis(ms as u64).min(SCHEDULE_MAX_DELAY))
}

fn format_schedule_delay(delay: Duration) -> String {
    let ms = delay.as_millis();
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}s", (ms + 500) / 1_000)
    } else if ms < 3_600_000 {
        format!("{}m", (ms + 30_000) / 60_000)
    } else if ms < 86_400_000 {
        format!("{}h", (ms + 1_800_000) / 3_600_000)
    } else {
        format!("{}d", (ms + 43_200_000) / 86_400_000)
    }
}

fn schedule_store_path_for_cwd() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    schedule_store_path_for_project(&cwd)
}

fn schedule_store_path_for_project(cwd: &Path) -> Result<PathBuf> {
    let project = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let project_key = project.to_string_lossy();
    let mut hasher = DefaultHasher::new();
    project_key.hash(&mut hasher);
    let hash = hasher.finish();
    Ok(crate::config::libertai_config_dir()?
        .join("code-schedules")
        .join(format!("{hash:016x}.json")))
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_millis_u64)
        .unwrap_or(0)
}

fn load_scheduled_runs(path: &Path) -> Result<Vec<ScheduledRun>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading schedule store {}", path.display()))?;
    let stored: Vec<StoredScheduledRun> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing schedule store {}", path.display()))?;
    let now_epoch = now_epoch_ms();
    let now_instant = Instant::now();
    let mut runs: Vec<ScheduledRun> = stored
        .into_iter()
        .filter(|run| !run.id.trim().is_empty() && !run.prompt.trim().is_empty())
        .map(|run| {
            let delay = Duration::from_millis(run.due_epoch_ms.saturating_sub(now_epoch));
            ScheduledRun {
                id: run.id,
                prompt: run.prompt,
                due_at: now_instant + delay,
                due_epoch_ms: run.due_epoch_ms,
            }
        })
        .collect();
    runs.sort_by_key(|run| (run.due_at, run.id.clone()));
    Ok(runs)
}

fn persist_scheduled_runs_if_configured(
    path: Option<&Path>,
    scheduled_runs: &[ScheduledRun],
) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    persist_scheduled_runs(path, scheduled_runs)
}

fn persist_scheduled_runs(path: &Path, scheduled_runs: &[ScheduledRun]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating schedule store dir {}", parent.display()))?;
    }
    let stored: Vec<StoredScheduledRun> = scheduled_runs
        .iter()
        .map(|run| StoredScheduledRun {
            id: run.id.clone(),
            prompt: run.prompt.clone(),
            due_epoch_ms: run.due_epoch_ms,
        })
        .collect();
    let raw = serde_json::to_string_pretty(&stored).context("serializing schedule store")?;
    fs::write(path, format!("{raw}\n"))
        .with_context(|| format!("writing schedule store {}", path.display()))?;
    Ok(())
}

fn next_scheduled_run_id(scheduled_runs: &[ScheduledRun]) -> usize {
    scheduled_runs
        .iter()
        .filter_map(|run| run.id.strip_prefix("sch_")?.parse::<usize>().ok())
        .max()
        .map(|value| value.saturating_add(1))
        .unwrap_or(1)
}

fn print_schedule_status(scheduled_runs: &[ScheduledRun]) {
    if scheduled_runs.is_empty() {
        println!("{DIM}  /schedule: no scheduled prompts.{RESET}");
        return;
    }
    println!("{BOLD}schedule{RESET}");
    let now = Instant::now();
    for run in scheduled_runs {
        let remaining = run.due_at.saturating_duration_since(now);
        println!(
            "{DIM}  - {}: in {} - {}{RESET}",
            run.id,
            format_schedule_delay(remaining),
            run.prompt
        );
    }
}

fn pop_due_scheduled_prompt(scheduled_runs: &mut Vec<ScheduledRun>) -> Option<String> {
    let now = Instant::now();
    let idx = scheduled_runs
        .iter()
        .enumerate()
        .filter(|(_, run)| run.due_at <= now)
        .min_by_key(|(_, run)| run.due_at)
        .map(|(idx, _)| idx)?;
    let run = scheduled_runs.remove(idx);
    Some(format!("Scheduled follow-up ({}).\n\n{}", run.id, run.prompt))
}

#[cfg(test)]
fn scheduled_run_for_test(id: &str, prompt: &str, due_at: Instant) -> ScheduledRun {
    ScheduledRun {
        id: id.to_string(),
        prompt: prompt.to_string(),
        due_at,
        due_epoch_ms: now_epoch_ms(),
    }
}

fn parse_auto_command(input: &str) -> AutoCommand {
    let raw = input.trim();
    if raw.is_empty() {
        return AutoCommand::Status;
    }
    let mut parts = raw.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match head {
        "status" | "state" => AutoCommand::Status,
        "off" | "stop" | "cancel" => AutoCommand::Off,
        "on" | "start" | "run" => {
            let request = parse_auto_request(rest);
            AutoCommand::On {
                turns: request.turns,
                goal: request.goal,
            }
        }
        _ => {
            let request = parse_auto_request(raw);
            AutoCommand::On {
                turns: request.turns,
                goal: request.goal,
            }
        }
    }
}

fn parse_auto_request(input: &str) -> LoopRequest {
    let raw = input.trim();
    if raw.is_empty() {
        return LoopRequest {
            turns: AUTO_DEFAULT_TURNS,
            goal: String::new(),
        };
    }
    let mut parts = raw.splitn(2, char::is_whitespace);
    let Some(first) = parts.next() else {
        return LoopRequest {
            turns: AUTO_DEFAULT_TURNS,
            goal: String::new(),
        };
    };
    match first.parse::<usize>() {
        Ok(turns) => LoopRequest {
            turns: turns.clamp(1, AUTO_MAX_TURNS),
            goal: parts.next().unwrap_or("").trim().to_string(),
        },
        Err(_) => LoopRequest {
            turns: AUTO_DEFAULT_TURNS,
            goal: raw.to_string(),
        },
    }
}

fn print_auto_status(auto_run: Option<&AutoRun>) {
    match auto_run {
        Some(run) => println!(
            "{DIM}  /auto: on, completed {}/{}, remaining {}{}.{RESET}",
            run.completed,
            run.limit,
            run.limit.saturating_sub(run.completed),
            if run.goal.is_empty() { "" } else { ", goal set" }
        ),
        None => println!("{DIM}  /auto: continuous execution is off.{RESET}"),
    }
}

fn parse_loop_request(input: &str) -> LoopRequest {
    let raw = input.trim();
    if raw.is_empty() {
        return LoopRequest {
            turns: LOOP_DEFAULT_TURNS,
            goal: String::new(),
        };
    }
    let mut parts = raw.splitn(2, char::is_whitespace);
    let Some(first) = parts.next() else {
        return LoopRequest {
            turns: LOOP_DEFAULT_TURNS,
            goal: String::new(),
        };
    };
    match first.parse::<usize>() {
        Ok(turns) => LoopRequest {
            turns: turns.clamp(1, LOOP_MAX_TURNS),
            goal: parts.next().unwrap_or("").trim().to_string(),
        },
        Err(_) => LoopRequest {
            turns: LOOP_DEFAULT_TURNS,
            goal: raw.to_string(),
        },
    }
}

fn autonomous_loop_prompts(request: &LoopRequest) -> VecDeque<String> {
    (1..=request.turns)
        .map(|idx| autonomous_loop_prompt(idx, request.turns, &request.goal))
        .collect()
}

fn autonomous_loop_prompt(idx: usize, total: usize, goal: &str) -> String {
    [
        format!("Autonomous loop turn {idx}/{total}."),
        if goal.trim().is_empty() {
            "Continue making concrete progress on the current task.".to_string()
        } else {
            format!("Goal: {}", goal.trim())
        },
        "Use tools as needed. If the task is complete or blocked, report the exact status and do not invent extra work.".to_string(),
    ]
    .join("\n\n")
}

fn auto_loop_prompt(idx: usize, total: usize, goal: &str) -> String {
    [
        format!("Auto mode turn {idx}/{total}."),
        if goal.trim().is_empty() {
            "Continue making concrete progress on the current task.".to_string()
        } else {
            format!("Goal: {}", goal.trim())
        },
        "Use tools as needed. If the task is complete, end your response with AUTO_DONE. If blocked, end your response with AUTO_BLOCKED.".to_string(),
    ]
    .join("\n\n")
}

fn render_markdown_transcript(messages: &[Message]) -> String {
    let mut out = String::from("# LibertAI Code Transcript\n\n");
    for message in messages {
        match message {
            Message::User(user) => {
                out.push_str("## User\n\n");
                out.push_str(&content_text(&user.content));
                out.push_str("\n\n");
            }
            Message::Assistant(assistant) => {
                out.push_str("## Assistant\n\n");
                render_blocks_markdown(&mut out, &assistant.content);
                out.push_str("\n\n");
            }
            Message::ToolResult(result) => {
                out.push_str(&format!("## Tool Result: {}\n\n", result.tool_name));
                if result.is_error {
                    out.push_str("**Error**\n\n");
                }
                render_blocks_markdown(&mut out, &result.content);
                out.push_str("\n\n");
            }
            Message::Custom(custom) => {
                out.push_str(&format!("## {}\n\n", custom.custom_type));
                out.push_str(&custom.content);
                out.push_str("\n\n");
            }
        }
    }
    out
}

fn render_html_transcript(messages: &[Message]) -> String {
    let mut out = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>LibertAI Code Transcript</title><style>\
body{font-family:system-ui,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif;line-height:1.5;margin:2rem auto;max-width:920px;padding:0 1rem;background:#fafafa;color:#161616}\
h1{font-size:1.6rem;margin-bottom:1.5rem}.turn{border:1px solid #ddd;background:white;border-radius:8px;margin:1rem 0;padding:1rem}\
.role{font-weight:700;margin-bottom:.5rem}.user .role{color:#064f8f}.assistant .role{color:#166534}.tool .role{color:#7c2d12}.custom .role{color:#581c87}\
pre{background:#111827;color:#f9fafb;border-radius:6px;overflow:auto;padding:.75rem}code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}.error{color:#b91c1c;font-weight:700}</style></head><body><h1>LibertAI Code Transcript</h1>\n",
    );
    for message in messages {
        match message {
            Message::User(user) => {
                out.push_str("<section class=\"turn user\"><div class=\"role\">User</div>");
                out.push_str(&html_paragraphs(&content_text(&user.content)));
                out.push_str("</section>\n");
            }
            Message::Assistant(assistant) => {
                out.push_str("<section class=\"turn assistant\"><div class=\"role\">Assistant</div>");
                render_blocks_html(&mut out, &assistant.content);
                out.push_str("</section>\n");
            }
            Message::ToolResult(result) => {
                out.push_str("<section class=\"turn tool\"><div class=\"role\">Tool Result: ");
                out.push_str(&escape_html(&result.tool_name));
                out.push_str("</div>");
                if result.is_error {
                    out.push_str("<p class=\"error\">Error</p>");
                }
                render_blocks_html(&mut out, &result.content);
                out.push_str("</section>\n");
            }
            Message::Custom(custom) => {
                out.push_str("<section class=\"turn custom\"><div class=\"role\">");
                out.push_str(&escape_html(&custom.custom_type));
                out.push_str("</div>");
                out.push_str(&html_paragraphs(&custom.content));
                out.push_str("</section>\n");
            }
        }
    }
    out.push_str("</body></html>\n");
    out
}

fn content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks_text(blocks),
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    render_blocks_markdown(&mut out, blocks);
    out.trim_end().to_string()
}

fn render_blocks_markdown(out: &mut String, blocks: &[ContentBlock]) {
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                out.push_str(&text.text);
                out.push('\n');
            }
            ContentBlock::Thinking(thinking) => {
                out.push_str("<details><summary>Thinking</summary>\n\n");
                out.push_str(&thinking.thinking);
                out.push_str("\n\n</details>\n");
            }
            ContentBlock::Image(image) => {
                out.push_str(&format!("![image](data:{};base64,...)\n", image.mime_type));
            }
            ContentBlock::ToolCall(tool) => {
                out.push_str(&format!("### Tool Call: {}\n\n", tool.name));
                out.push_str("```json\n");
                out.push_str(
                    &serde_json::to_string_pretty(&tool.arguments)
                        .unwrap_or_else(|_| tool.arguments.to_string()),
                );
                out.push_str("\n```\n");
            }
        }
    }
}

fn render_blocks_html(out: &mut String, blocks: &[ContentBlock]) {
    for block in blocks {
        match block {
            ContentBlock::Text(text) => out.push_str(&html_paragraphs(&text.text)),
            ContentBlock::Thinking(thinking) => {
                out.push_str("<details><summary>Thinking</summary>");
                out.push_str(&html_paragraphs(&thinking.thinking));
                out.push_str("</details>");
            }
            ContentBlock::Image(image) => {
                out.push_str("<p><em>Image: ");
                out.push_str(&escape_html(&image.mime_type));
                out.push_str("</em></p>");
            }
            ContentBlock::ToolCall(tool) => {
                out.push_str("<h3>Tool Call: ");
                out.push_str(&escape_html(&tool.name));
                out.push_str("</h3><pre><code>");
                let json = serde_json::to_string_pretty(&tool.arguments)
                    .unwrap_or_else(|_| tool.arguments.to_string());
                out.push_str(&escape_html(&json));
                out.push_str("</code></pre>");
            }
        }
    }
}

fn html_paragraphs(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::from("<p><em>(empty)</em></p>");
    }
    let mut out = String::new();
    for chunk in trimmed.split("\n\n") {
        out.push_str("<p>");
        out.push_str(&escape_html(chunk).replace('\n', "<br>"));
        out.push_str("</p>");
    }
    out
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn print_init_project(notes: Option<&str>) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /init: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match crate::commands::code_init::init_project_with_notes(&cwd, notes) {
        Ok(result) if result.created => {
            println!("{BOLD}init{RESET}");
            println!("{DIM}  created: {}{RESET}", result.path.display());
            println!("{DIM}  future sessions in this tree will load it automatically.{RESET}");
            println!();
        }
        Ok(result) => {
            println!("{BOLD}init{RESET}");
            println!("{DIM}  AGENTS.md already exists: {}{RESET}", result.path.display());
            println!("{DIM}  left existing content unchanged.{RESET}");
            match crate::commands::code_init::agents_md_candidate(&cwd, notes) {
                Ok(candidate) => {
                    print!(
                        "{}",
                        init_candidate_preview(
                            &result.path.display().to_string(),
                            &result.content,
                            &candidate
                        )
                    )
                }
                Err(e) => eprintln!("{DIM}  could not build merge candidate: {e:#}{RESET}"),
            }
            println!();
        }
        Err(e) => eprintln!("{DIM}  /init: failed: {e:#}{RESET}"),
    }
}

async fn apply_init_from_agent(handle: &AgentSessionHandle, action: InitFromAgentAction) {
    let messages = match handle.messages().await {
        Ok(messages) => messages,
        Err(e) => {
            eprintln!("{DIM}  /init from-agent: could not read transcript: {e:#}{RESET}");
            return;
        }
    };
    let Some(text) = last_assistant_text(&messages) else {
        println!("{DIM}  /init from-agent: no assistant response yet.{RESET}");
        return;
    };
    let Some(candidate) = crate::commands::code_init::extract_agents_md_candidate(&text) else {
        println!(
            "{DIM}  /init from-agent: no fenced AGENTS.md candidate found in the latest assistant response.{RESET}"
        );
        return;
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /init from-agent: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let path = cwd.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    match action {
        InitFromAgentAction::Preview => {
            println!("{BOLD}init from-agent{RESET}");
            print!(
                "{}",
                init_candidate_preview(&path.display().to_string(), &existing, &candidate)
            );
            println!();
        }
        InitFromAgentAction::PreviewMergeLines => {
            let content = match build_init_apply_content(&existing, &candidate, "merge-lines") {
                Ok(content) => content,
                Err(e) => {
                    eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                    return;
                }
            };
            println!("{BOLD}init from-agent merge-lines preview{RESET}");
            print!(
                "{}",
                init_candidate_preview(&path.display().to_string(), &existing, &content)
            );
            println!();
        }
        InitFromAgentAction::Append
        | InitFromAgentAction::Merge
        | InitFromAgentAction::MergeLines
        | InitFromAgentAction::Replace => {
            let mode = match action {
                InitFromAgentAction::Append => "append",
                InitFromAgentAction::Merge => "merge",
                InitFromAgentAction::MergeLines => "merge-lines",
                InitFromAgentAction::Replace => "replace",
                InitFromAgentAction::Preview | InitFromAgentAction::PreviewMergeLines => {
                    unreachable!()
                }
            };
            let content = match build_init_apply_content(&existing, &candidate, mode) {
                Ok(content) => content,
                Err(e) => {
                    eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                    return;
                }
            };
            let backup = if action == InitFromAgentAction::Replace && path.exists() {
                let backup = path.with_file_name(format!(
                    "AGENTS.md.bak.{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0)
                ));
                if let Err(e) = std::fs::write(&backup, &existing) {
                    eprintln!(
                        "{DIM}  /init from-agent: could not write backup {}: {e}{RESET}",
                        backup.display()
                    );
                    return;
                }
                Some(backup)
            } else {
                None
            };
            if let Err(e) = std::fs::write(&path, content) {
                eprintln!(
                    "{DIM}  /init from-agent: could not write {}: {e}{RESET}",
                    path.display()
                );
                return;
            }
            println!(
                "{DIM}  /init from-agent: {} AGENTS.md from latest assistant candidate: {}{RESET}",
                match action {
                    InitFromAgentAction::Append => "appended to",
                    InitFromAgentAction::Merge => "merged into",
                    InitFromAgentAction::MergeLines => "line-merged into",
                    InitFromAgentAction::Replace => "replaced",
                    InitFromAgentAction::Preview | InitFromAgentAction::PreviewMergeLines => {
                        unreachable!()
                    }
                },
                path.display()
            );
            if let Some(backup) = backup {
                println!("{DIM}  backup: {}{RESET}", backup.display());
            }
        }
    }
}

fn build_init_apply_content(existing: &str, candidate: &str, mode: &str) -> Result<String, String> {
    let candidate = ensure_trailing_newline(candidate.trim());
    match mode {
        "append" => {
            let mut content = existing.trim_end().to_string();
            if !content.trim().is_empty() {
                content.push_str("\n\n");
            }
            content.push_str("## Generated /init candidate\n\n");
            content.push_str(&candidate);
            Ok(content)
        }
        "merge" => Ok(merge_init_candidate_sections(existing, &candidate)),
        "merge-lines" => Ok(merge_init_candidate_lines(existing, &candidate)),
        "replace" => Ok(candidate),
        _ => Err("init apply mode must be append, merge-lines, merge, or replace".to_string()),
    }
}

fn ensure_trailing_newline(value: &str) -> String {
    let mut out = value.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone)]
struct InitMarkdownSection {
    title: Option<String>,
    content: String,
}

fn merge_init_candidate_sections(existing: &str, candidate: &str) -> String {
    let candidate_sections = split_init_markdown_sections(candidate);
    if existing.trim().is_empty() || candidate_sections.is_empty() {
        return ensure_trailing_newline(candidate.trim());
    }
    let mut existing_sections = split_init_markdown_sections(existing);
    let mut append_sections = Vec::new();
    for candidate_section in candidate_sections {
        let Some(candidate_title) = candidate_section.title.as_deref() else {
            append_sections.push(candidate_section);
            continue;
        };
        if let Some(existing_section) = existing_sections.iter_mut().find(|section| {
            section
                .title
                .as_deref()
                .is_some_and(|title| title.eq_ignore_ascii_case(candidate_title))
        }) {
            existing_section.content = candidate_section.content;
        } else {
            append_sections.push(candidate_section);
        }
    }
    let mut merged = join_init_markdown_sections(&existing_sections);
    if !append_sections.is_empty() {
        if !merged.trim().is_empty() {
            merged.push_str("\n\n");
        }
        merged.push_str("## Generated /init candidate\n\n");
        merged.push_str(&join_init_markdown_sections(&append_sections));
    }
    ensure_trailing_newline(merged.trim_end())
}

fn merge_init_candidate_lines(existing: &str, candidate: &str) -> String {
    let candidate_sections = split_init_markdown_sections(candidate);
    if existing.trim().is_empty() || candidate_sections.is_empty() {
        return ensure_trailing_newline(candidate.trim());
    }
    let mut existing_sections = split_init_markdown_sections(existing);
    let mut append_sections = Vec::new();
    for candidate_section in candidate_sections {
        let Some(candidate_title) = candidate_section.title.as_deref() else {
            if !is_init_candidate_preamble(&candidate_section.content) {
                append_sections.push(candidate_section);
            }
            continue;
        };
        if let Some(existing_section) = existing_sections.iter_mut().find(|section| {
            section
                .title
                .as_deref()
                .is_some_and(|title| title.eq_ignore_ascii_case(candidate_title))
        }) {
            existing_section.content =
                merge_init_section_lines(&existing_section.content, &candidate_section.content);
        } else {
            append_sections.push(candidate_section);
        }
    }
    let mut merged = join_init_markdown_sections(&existing_sections);
    if !append_sections.is_empty() {
        if !merged.trim().is_empty() {
            merged.push_str("\n\n");
        }
        merged.push_str("## Generated /init candidate\n\n");
        merged.push_str(&join_init_markdown_sections(&append_sections));
    }
    ensure_trailing_newline(merged.trim_end())
}

fn merge_init_section_lines(existing: &str, candidate: &str) -> String {
    let mut lines = existing
        .trim_end()
        .lines()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut seen = lines
        .iter()
        .map(|line| normalize_init_line(line))
        .collect::<std::collections::BTreeSet<_>>();
    let additions = candidate
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.trim_start().starts_with("## "))
        .filter(|line| seen.insert(normalize_init_line(line)))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    lines.extend(additions);
    lines.join("\n")
}

fn is_init_candidate_preamble(content: &str) -> bool {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .all(|line| line.starts_with("# "))
}

fn normalize_init_line(line: &str) -> String {
    line.trim()
        .trim_start_matches(|ch| ch == '-' || ch == '*')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn split_init_markdown_sections(markdown: &str) -> Vec<InitMarkdownSection> {
    let mut sections = Vec::new();
    let mut current_title = None;
    let mut current = Vec::new();
    let flush = |sections: &mut Vec<InitMarkdownSection>,
                 title: &mut Option<String>,
                 lines: &mut Vec<String>| {
        let content = lines.join("\n").trim().to_string();
        if content.is_empty() {
            lines.clear();
            return;
        }
        sections.push(InitMarkdownSection {
            title: title.take(),
            content,
        });
        lines.clear();
    };
    for line in markdown.replace("\r\n", "\n").lines() {
        if let Some(title) = line.trim().strip_prefix("## ") {
            flush(&mut sections, &mut current_title, &mut current);
            current_title = Some(title.trim().to_string());
            current.push(line.to_string());
        } else {
            current.push(line.to_string());
        }
    }
    flush(&mut sections, &mut current_title, &mut current);
    sections
}

fn join_init_markdown_sections(sections: &[InitMarkdownSection]) -> String {
    sections
        .iter()
        .map(|section| section.content.trim())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn write_onboarding_guide(path: Option<&str>) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /onboarding: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let target = match parse_onboarding_target(path) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("{DIM}  /onboarding: {e:#}{RESET}");
            return;
        }
    };
    let guide = match crate::commands::code_init::onboarding_guide(&cwd) {
        Ok(guide) => guide,
        Err(e) => {
            eprintln!("{DIM}  /onboarding: failed: {e:#}{RESET}");
            return;
        }
    };
    match target {
        OnboardingTarget::File(path) => {
            if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "{DIM}  /onboarding: could not create {}: {e}{RESET}",
                        parent.display()
                    );
                    return;
                }
            }
            match std::fs::write(&path, guide) {
                Ok(()) => println!("{DIM}  onboarding guide written: {}{RESET}", path.display()),
                Err(e) => eprintln!(
                    "{DIM}  /onboarding: could not write {}: {e}{RESET}",
                    path.display()
                ),
            }
        }
        OnboardingTarget::Gist { public, filename } => {
            match publish_gist(&guide, public, &filename, "LibertAI Code onboarding guide") {
                Ok(url) => println!("{DIM}  onboarding gist created: {url}{RESET}"),
                Err(e) => eprintln!("{DIM}  /onboarding gist: {e:#}{RESET}"),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OnboardingTarget {
    File(PathBuf),
    Gist { public: bool, filename: String },
}

fn parse_onboarding_target(path: Option<&str>) -> Result<OnboardingTarget> {
    let raw = path.unwrap_or("").trim();
    let raw = strip_save_action(raw);
    if raw.is_empty() {
        return Ok(OnboardingTarget::File(PathBuf::from("libertai-onboarding.md")));
    }
    let Some(rest) = raw.strip_prefix("gist") else {
        return Ok(OnboardingTarget::File(PathBuf::from(raw)));
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Ok(OnboardingTarget::File(PathBuf::from(raw)));
    }
    let mut public = false;
    let mut filename_parts = Vec::new();
    for part in rest.split_whitespace() {
        match part.to_ascii_lowercase().as_str() {
            "public" | "--public" => public = true,
            "secret" | "private" | "--secret" | "--private" => public = false,
            other if other.starts_with('-') => {
                anyhow::bail!("unknown gist option `{part}`; use /onboarding gist [public|secret] [filename.md]");
            }
            _ => filename_parts.push(part),
        }
    }
    let filename = filename_parts.join("-");
    let filename = sanitize_gist_filename(if filename.trim().is_empty() {
        "libertai-onboarding.md"
    } else {
        filename.trim()
    });
    Ok(OnboardingTarget::Gist { public, filename })
}

fn init_candidate_preview(path: &str, existing: &str, candidate: &str) -> String {
    let mut out = String::new();
    out.push_str("  generated merge candidate (not written):\n\n");
    out.push_str(candidate);
    if !candidate.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n  diff against existing AGENTS.md:\n\n");
    out.push_str(&crate::commands::code_diff::render_line_diff(
        path, existing, candidate,
    ));
    out.push('\n');
    let sections = init_candidate_sections(candidate);
    if !sections.is_empty() {
        out.push_str("\n  candidate sections:\n");
        for (idx, section) in sections.iter().enumerate() {
            out.push_str(&format!("  {}. {section}\n", idx + 1));
        }
    }
    out.push_str("\n  Review the candidate against the existing AGENTS.md and merge only verified repo facts.\n");
    out
}

fn init_candidate_sections(candidate: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut has_preamble = false;
    for line in candidate.replace("\r\n", "\n").lines() {
        if let Some(title) = line.trim().strip_prefix("## ") {
            let title = title.trim();
            if !title.is_empty() {
                sections.push(title.to_string());
            }
        } else if !line.trim().is_empty() && sections.is_empty() && !has_preamble {
            sections.push("Preamble".to_string());
            has_preamble = true;
        }
    }
    sections
}

fn print_memory(action: &str) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /memory: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    if is_memory_edit_action(action) {
        match crate::commands::code_memory::ensure_memory_file(&cwd) {
            Ok(path) => open_memory_editor(&path),
            Err(e) => eprintln!("{DIM}  /memory open: failed: {e:#}{RESET}"),
        }
        return;
    }
    if let Some(source) = memory_import_source(action) {
        match crate::commands::code_memory::import_memory_file(&cwd, Path::new(source)) {
            Ok(result) => {
                println!("{BOLD}memory import{RESET}");
                println!("{DIM}  source:{RESET} {}", result.source_path.display());
                println!("{DIM}  imported:{RESET} {} bytes", result.bytes);
                println!("{DIM}  memory path:{RESET} {}", result.path.display());
                println!("{DIM}  changes take effect in new agent sessions.{RESET}");
            }
            Err(e) => eprintln!("{DIM}  /memory import: failed: {e:#}{RESET}"),
        }
        return;
    }
    if matches!(
        action.to_ascii_lowercase().as_str(),
        "import-claude" | "migrate-claude" | "claude"
    ) {
        match crate::commands::code_memory::import_claude_memory(&cwd) {
            Ok(result) => {
                println!("{BOLD}memory import-claude{RESET}");
                println!("{DIM}  source:{RESET} {}", result.source_dir.display());
                println!(
                    "{DIM}  imported:{RESET} {} files ({} bytes)",
                    result.imported_files, result.imported_bytes
                );
                if result.skipped_files > 0 {
                    println!("{DIM}  skipped:{RESET} {} files", result.skipped_files);
                }
                println!("{DIM}  memory path:{RESET} {}", result.path.display());
                println!("{DIM}  changes take effect in new agent sessions.{RESET}");
            }
            Err(e) => eprintln!("{DIM}  /memory import-claude: failed: {e:#}{RESET}"),
        }
        return;
    }
    if matches!(
        action.to_ascii_lowercase().as_str(),
        "import-claude-all" | "migrate-claude-all" | "claude-all"
    ) {
        match crate::commands::code_memory::import_all_claude_memory() {
            Ok(result) => {
                println!("{BOLD}memory import-claude-all{RESET}");
                println!("{DIM}  projects:{RESET} {}", result.imported_projects);
                println!(
                    "{DIM}  imported:{RESET} {} files ({} bytes)",
                    result.imported_files, result.imported_bytes
                );
                if result.skipped_projects > 0 {
                    println!("{DIM}  skipped projects:{RESET} {}", result.skipped_projects);
                }
                if result.skipped_files > 0 {
                    println!("{DIM}  skipped files:{RESET} {}", result.skipped_files);
                }
                println!("{DIM}  changes take effect in new agent sessions.{RESET}");
            }
            Err(e) => eprintln!("{DIM}  /memory import-claude-all: failed: {e:#}{RESET}"),
        }
        return;
    }
    if action.eq_ignore_ascii_case("clear") {
        match crate::commands::code_memory::clear_memory(&cwd) {
            Ok(result) => {
                if let Some(backup) = result.backup_path {
                    println!(
                        "{DIM}  memory cleared: {} (backup: {}){RESET}",
                        result.path.display(),
                        backup.display()
                    );
                } else {
                    println!(
                        "{DIM}  no MEMORY.md to clear: {}{RESET}",
                        result.path.display()
                    );
                }
            }
            Err(e) => eprintln!("{DIM}  /memory clear: failed: {e:#}{RESET}"),
        }
        return;
    }
    if matches!(action.to_ascii_lowercase().as_str(), "references" | "refs" | "verify") {
        match crate::commands::code_memory::verify_memory_references(&cwd) {
            Ok(refs) => print_memory_references(&refs),
            Err(e) => eprintln!("{DIM}  /memory references: failed: {e:#}{RESET}"),
        }
        return;
    }
    if matches!(action.to_ascii_lowercase().as_str(), "files" | "list") {
        match crate::commands::code_memory::list_memory_files(&cwd) {
            Ok(files) => print_memory_files(&files),
            Err(e) => eprintln!("{DIM}  /memory files: failed: {e:#}{RESET}"),
        }
        return;
    }
    let doc = match crate::commands::code_memory::read_memory(&cwd) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("{DIM}  /memory: failed: {e:#}{RESET}");
            return;
        }
    };
    if action.eq_ignore_ascii_case("path") {
        println!("{DIM}  memory path: {}{RESET}", doc.path.display());
        return;
    }
    println!("{BOLD}memory{RESET}");
    println!("{DIM}  path:{RESET} {}", doc.path.display());
    if !doc.exists {
        println!("{DIM}  no MEMORY.md yet. Use /remember <text> to create one.{RESET}");
    } else if doc.content.trim().is_empty() {
        println!("{DIM}  MEMORY.md is empty. Use /remember <text> to append a note.{RESET}");
    } else {
        print_memory_summary(&doc.content);
        print!("{}", doc.content);
        if !doc.content.ends_with('\n') {
            println!();
        }
    }
    println!();
}

fn is_memory_edit_action(action: &str) -> bool {
    matches!(
        action.trim().to_ascii_lowercase().as_str(),
        "open" | "edit" | "editor"
    )
}

fn memory_import_source(action: &str) -> Option<&str> {
    let trimmed = action.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next()?;
    if !command.eq_ignore_ascii_case("import") {
        return None;
    }
    parts.next().map(str::trim).filter(|source| !source.is_empty())
}

fn print_memory_references(refs: &[crate::commands::code_memory::MemoryReference]) {
    println!("{BOLD}memory references{RESET}");
    if refs.is_empty() {
        println!("{DIM}  no [reference] entries found in project memory{RESET}");
        println!();
        return;
    }
    for reference in refs {
        let target = reference.target.as_deref().unwrap_or("(no target)");
        println!(
            "{DIM}  line {} · {}:{RESET} {} — {}",
            reference.line_number,
            reference.status.label(),
            target,
            reference.detail
        );
    }
    println!();
}

fn print_memory_files(files: &[crate::commands::code_memory::MemoryFileEntry]) {
    println!("{BOLD}memory files{RESET}");
    if files.is_empty() {
        println!("{DIM}  no per-entry memory files found yet{RESET}");
        println!("{DIM}  use /remember [kind:] <text> to create one{RESET}");
        println!();
        return;
    }
    for file in files {
        println!(
            "{DIM}  [{}]{RESET} {} - {}",
            file.kind.label(),
            file.path.display(),
            file.title
        );
    }
    println!();
}

fn print_memory_summary(content: &str) {
    let mut user = 0usize;
    let mut feedback = 0usize;
    let mut project = 0usize;
    let mut reference = 0usize;
    for line in content.lines() {
        if line.contains("[user]") {
            user += 1;
        } else if line.contains("[feedback]") {
            feedback += 1;
        } else if line.contains("[reference]") {
            reference += 1;
        } else if line.contains("[project]") || line.trim_start().starts_with("- ") {
            project += 1;
        }
    }
    println!(
        "{DIM}  entries: user {user} · feedback {feedback} · project {project} · reference {reference}{RESET}"
    );
}

fn print_agents() {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /agents: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let agents = match crate::commands::code_agents::discover_agents(&cwd) {
        Ok(agents) => agents,
        Err(e) => {
            eprintln!("{DIM}  /agents: failed: {e:#}{RESET}");
            return;
        }
    };
    println!("{BOLD}agents{RESET}");
    if agents.is_empty() {
        println!("{DIM}  no named sub-agents found.{RESET}");
        println!("{DIM}  create .claude/agents/<name>.md or .libertai/agents/<name>.md in this project.{RESET}");
    } else {
        for agent in agents {
            let tools = agent
                .tools
                .as_ref()
                .filter(|tools| !tools.is_empty())
                .map(|tools| tools.join(", "))
                .unwrap_or_else(|| "read, grep, find, ls".to_string());
            let model = agent.model.as_deref().unwrap_or("default");
            println!(
                "- {}: {}",
                agent.name,
                if agent.description.trim().is_empty() {
                    "Named sub-agent"
                } else {
                    agent.description.as_str()
                }
            );
            println!(
                "{DIM}  model: {model} · tools: {tools} · {}{RESET}",
                agent_source_label(&agent.source)
            );
        }
        println!("{DIM}  run /agent <name> <task> to dispatch a focused task.{RESET}");
    }
    println!("{DIM}  run /agents create <name> [description] to scaffold a project sub-agent.{RESET}");
    println!();
}

fn handle_agents_command(input: &str) {
    match parse_agents_command(input) {
        AgentsSlashCommand::List => print_agents(),
        AgentsSlashCommand::Open => print_agents_open_hint(),
        AgentsSlashCommand::Create(rest) => create_agent_from_slash(rest),
        AgentsSlashCommand::Delete(rest) => delete_agent_from_slash(rest),
        AgentsSlashCommand::BackgroundList => print_background_agents(),
        AgentsSlashCommand::BackgroundLog(rest) => print_background_agent_log(rest),
        AgentsSlashCommand::BackgroundKill(rest) => kill_background_agent(rest),
        AgentsSlashCommand::Usage => {
            eprintln!("{DIM}  /agents: usage: /agents [list|open|background] | /agents create [--worktree] <name> [description] | /agents delete <name>{RESET}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentsSlashCommand<'a> {
    List,
    Open,
    Create(&'a str),
    Delete(&'a str),
    BackgroundList,
    BackgroundLog(&'a str),
    BackgroundKill(&'a str),
    Usage,
}

fn parse_agents_command(input: &str) -> AgentsSlashCommand<'_> {
    let raw = input.trim();
    if raw.is_empty() || raw == "list" {
        return AgentsSlashCommand::List;
    }
    if raw == "open" {
        return AgentsSlashCommand::Open;
    }
    if raw == "background" || raw == "background list" || raw == "bg" || raw == "bg list" {
        return AgentsSlashCommand::BackgroundList;
    }
    if let Some(rest) = raw
        .strip_prefix("background log")
        .or_else(|| raw.strip_prefix("bg log"))
    {
        return AgentsSlashCommand::BackgroundLog(rest.trim());
    }
    if let Some(rest) = raw
        .strip_prefix("background kill")
        .or_else(|| raw.strip_prefix("bg kill"))
        .or_else(|| raw.strip_prefix("background stop"))
        .or_else(|| raw.strip_prefix("bg stop"))
    {
        return AgentsSlashCommand::BackgroundKill(rest.trim());
    }
    if let Some(rest) = raw.strip_prefix("create ") {
        return AgentsSlashCommand::Create(rest.trim());
    }
    if let Some(rest) = raw
        .strip_prefix("delete ")
        .or_else(|| raw.strip_prefix("remove "))
    {
        return AgentsSlashCommand::Delete(rest.trim());
    }
    AgentsSlashCommand::Usage
}

fn print_agents_open_hint() {
    let cwd = std::env::current_dir().ok();
    println!("{BOLD}agents management{RESET}");
    println!("{DIM}  desktop: /agents open jumps to Settings > Agents.{RESET}");
    if let Some(cwd) = cwd {
        println!(
            "{DIM}  terminal: edit project agents in {} or {}{RESET}",
            cwd.join(".libertai/agents").display(),
            cwd.join(".claude/agents").display()
        );
    } else {
        println!("{DIM}  terminal: edit .libertai/agents or .claude/agents in this project.{RESET}");
    }
    println!("{DIM}  user agents live under ~/.libertai/agents or ~/.claude/agents.{RESET}");
}

fn create_agent_from_slash(input: &str) {
    let parsed = match parse_agents_create_query(input) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{DIM}  /agents: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /agents: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match crate::commands::code_agents::create_project_agent(
        &cwd,
        parsed.name,
        parsed.description,
        parsed.worktree,
    ) {
        Ok(path) => {
            println!("{DIM}  created project sub-agent: {}{RESET}", path.display());
            println!("{DIM}  edit the prompt, then run /agent {} <task>{RESET}", parsed.name);
        }
        Err(e) => eprintln!("{DIM}  /agents: create failed: {e:#}{RESET}"),
    }
}

fn delete_agent_from_slash(input: &str) {
    let Some((name, tail)) = split_first_word(input.trim()) else {
        eprintln!("{DIM}  /agents: usage: /agents delete <name>{RESET}");
        return;
    };
    if !tail.trim().is_empty() {
        eprintln!("{DIM}  /agents: usage: /agents delete <name>{RESET}");
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /agents: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match crate::commands::code_agents::delete_agent(&cwd, name) {
        Ok(path) => println!(
            "{DIM}  deleted sub-agent `{}`: {}{RESET}",
            name.trim().trim_start_matches('@'),
            path.display()
        ),
        Err(e) => eprintln!("{DIM}  /agents: delete failed: {e:#}{RESET}"),
    }
}

fn print_background_agents() {
    match load_background_agent_records() {
        Ok(records) if records.is_empty() => {
            println!("{DIM}  no terminal background agents recorded.{RESET}");
        }
        Ok(records) => {
            println!("{BOLD}background agents{RESET}");
            for record in records.iter().rev().take(20) {
                let status = background_agent_status(record.pid);
                println!(
                    "- pid {} [{}] {} — {}",
                    record.pid,
                    status.label(),
                    record.name,
                    record.prompt_preview
                );
                println!(
                    "{DIM}  started: {} · cwd: {}{RESET}",
                    format_epoch_ms(record.started_at_ms),
                    record.cwd
                );
                println!("{DIM}  log: {}{RESET}", record.log_path);
            }
            println!("{DIM}  /agents background log [pid|latest] shows the saved output.{RESET}");
            println!("{DIM}  /agents background kill <pid> stops a running background agent.{RESET}");
        }
        Err(e) => eprintln!("{DIM}  /agents: could not read background agents: {e:#}{RESET}"),
    }
}

fn print_background_agent_log(input: &str) {
    match resolve_background_agent_record(input.trim()) {
        Ok(Some(record)) => {
            let path = PathBuf::from(&record.log_path);
            match read_log_tail(&path, BACKGROUND_AGENT_LOG_TAIL_BYTES) {
                Ok(text) if text.is_empty() => {
                    println!("{DIM}  log is empty: {}{RESET}", path.display());
                }
                Ok(text) => {
                    println!(
                        "{BOLD}background agent {} log{RESET} {DIM}{}{}",
                        record.pid,
                        path.display(),
                        RESET
                    );
                    print!("{text}");
                    if !text.ends_with('\n') {
                        println!();
                    }
                }
                Err(e) => eprintln!("{DIM}  /agents: could not read log: {e:#}{RESET}"),
            }
        }
        Ok(None) => eprintln!("{DIM}  /agents: no matching background agent found{RESET}"),
        Err(e) => eprintln!("{DIM}  /agents: {e:#}{RESET}"),
    }
}

fn kill_background_agent(input: &str) {
    let pid = match parse_background_agent_pid(input) {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("{DIM}  /agents: {e:#}{RESET}");
            return;
        }
    };
    match send_background_agent_kill(pid) {
        Ok(()) => println!("{DIM}  sent terminate signal to background agent pid {pid}.{RESET}"),
        Err(e) => eprintln!("{DIM}  /agents: could not stop pid {pid}: {e:#}{RESET}"),
    }
}

fn print_templates() {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /template: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let templates = crate::commands::code_slash_registry::discover(&cwd);
    println!("{BOLD}templates{RESET}");
    if templates.is_empty() {
        println!("{DIM}  no prompt templates found.{RESET}");
        println!("{DIM}  create .claude/commands/<name>.md or .libertai/commands/<name>.md.{RESET}");
    } else {
        for t in templates {
            let base_desc = t.description.as_deref().unwrap_or(match t.source {
                crate::commands::code_slash_registry::CommandSource::Project => "project template",
                crate::commands::code_slash_registry::CommandSource::User => "user template",
            });
            let desc = if let Some(ns) = t.namespace.as_deref() {
                format!("{base_desc} ({ns})")
            } else {
                base_desc.to_string()
            };
            let hint = t
                .arg_hint
                .as_ref()
                .map(|h| format!(" · args: {h}"))
                .unwrap_or_default();
            println!("- /{}: {}{}", t.name, desc, hint);
        }
        println!("{DIM}  run /template <name> [args], or /<name> [args].{RESET}");
    }
    println!();
}

fn handle_skills_slash(query: &str) -> Result<()> {
    match parse_skills_command(query)? {
        SkillsCommand::List => print_code_skills(),
        SkillsCommand::Open => print_code_skills_open_hint(),
        SkillsCommand::Enable(name) => {
            code_skills::set_skill_enabled(&name, true)?;
            println!("{DIM}  enabled skill for new sessions: {name}{RESET}");
            print_code_skills();
        }
        SkillsCommand::Disable(name) => {
            code_skills::set_skill_enabled(&name, false)?;
            println!("{DIM}  disabled skill for new sessions: {name}{RESET}");
            print_code_skills();
        }
    }
    Ok(())
}

fn parse_skills_command(query: &str) -> Result<SkillsCommand> {
    let raw = query.trim();
    if raw.is_empty()
        || raw.eq_ignore_ascii_case("list")
        || raw.eq_ignore_ascii_case("status")
        || raw.eq_ignore_ascii_case("show")
    {
        return Ok(SkillsCommand::List);
    }
    if raw.eq_ignore_ascii_case("open") || raw.eq_ignore_ascii_case("settings") {
        return Ok(SkillsCommand::Open);
    }
    let Some((head, tail)) = split_first_word(raw) else {
        return Ok(SkillsCommand::List);
    };
    let name = tail.trim();
    match head.to_ascii_lowercase().as_str() {
        "enable" | "on" => {
            if name.is_empty() {
                anyhow::bail!("usage: /skills enable <name>");
            }
            Ok(SkillsCommand::Enable(name.to_string()))
        }
        "disable" | "off" => {
            if name.is_empty() {
                anyhow::bail!("usage: /skills disable <name>");
            }
            Ok(SkillsCommand::Disable(name.to_string()))
        }
        _ => anyhow::bail!("usage: /skills [list|open|enable <name>|disable <name>]"),
    }
}

fn print_code_skills() {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /skills: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let skills = match code_skills::skill_inventory(SkillPillar::Code, Some(&cwd)) {
        Ok(skills) => skills,
        Err(e) => {
            eprintln!("{DIM}  /skills: failed: {e:#}{RESET}");
            return;
        }
    };
    println!("{BOLD}skills{RESET}");
    if skills.is_empty() {
        println!("{DIM}  no built-in, project, or user skills match the code pillar.{RESET}");
    } else {
        for skill in skills {
            let marker = if skill.enabled { "on" } else { "off" };
            let tools = skill
                .allowed_tools
                .as_ref()
                .filter(|tools| !tools.trim().is_empty())
                .map(|tools| format!(" · tools: {tools}"))
                .unwrap_or_default();
            println!("- [{}] {}: {}", marker, skill.name, skill.description);
            println!("{DIM}  {}{}{RESET}", skill.source, tools);
        }
        println!("{DIM}  changes apply to new sessions; use /reload to start a fresh session now.{RESET}");
        println!("{DIM}  run /skills disable <name> or /skills enable <name>.{RESET}");
    }
    println!();
}

fn print_code_skills_open_hint() {
    let cwd = std::env::current_dir().ok();
    println!("{BOLD}skills{RESET}");
    println!("{DIM}  desktop: /skills open jumps to Settings > Skills.{RESET}");
    if let Some(cwd) = cwd {
        println!(
            "{DIM}  terminal: project skills are read from {} and {}{RESET}",
            cwd.join(".libertai/skills").display(),
            cwd.join(".claude/skills").display()
        );
    } else {
        println!("{DIM}  terminal: project skills are read from .libertai/skills and .claude/skills.{RESET}");
    }
    println!("{DIM}  user skills live under ~/.libertai/skills or ~/.claude/skills.{RESET}");
    println!("{DIM}  use /skills list, /skills enable <name>, or /skills disable <name>.{RESET}");
    println!();
}

fn build_template_slash_prompt(query: &str) -> Result<String> {
    let (name, args) = parse_template_query(query)?;
    let Some(prompt) = build_custom_slash_prompt(name, args)? else {
        anyhow::bail!("template not found: {name}");
    };
    Ok(prompt)
}

fn parse_template_query(query: &str) -> Result<(&str, &str)> {
    let raw = query.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("list") {
        anyhow::bail!("usage: /template <name> [args]");
    }
    let (name, args) = raw
        .split_once(char::is_whitespace)
        .map_or((raw, ""), |(name, args)| (name, args.trim()));
    Ok((name, args))
}

fn review_command_parts(trimmed: &str) -> Option<(&str, &str)> {
    let (command, scope) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, ""), |(command, scope)| (command, scope.trim()));
    match command {
        "/review" | "/security-review" | "/pr_comments" | "/pr-comments" => {
            Some((command, scope))
        }
        _ => None,
    }
}

fn build_review_slash_prompt(command: &str, scope: &str) -> Result<String> {
    match command {
        "/review" => Ok(review_prompt(false, scope)),
        "/security-review" => Ok(review_prompt(true, scope)),
        "/pr_comments" | "/pr-comments" => Ok(pr_comments_prompt(scope)),
        _ => anyhow::bail!("unknown review command: {command}"),
    }
}

fn review_prompt(security: bool, scope: &str) -> String {
    let scope = scope.trim();
    let scope_line = if scope.is_empty() {
        "User-requested scope: current repository changes.".to_string()
    } else {
        format!("User-requested scope: {scope}")
    };
    let title = if security {
        "Run a focused security review"
    } else {
        "Review the current code changes"
    };
    let focus = if security {
        r#"Security focus:
- Injection, command execution, path traversal, filesystem escape, auth
  and secret handling, SSRF/network trust, sandbox bypass, unsafe
  deserialization, dependency or configuration exposure.
- Treat user-controlled input and model/tool output as hostile until
  proven otherwise."#
    } else {
        r#"Review focus:
- Bugs, behavioral regressions, race conditions, missing error handling,
  integration mismatches, data loss, and missing tests.
- Keep style-only comments out unless they hide a real correctness or
  maintenance risk."#
    };
    format!(
        r#"{title} for this repository.

{scope_line}

Rules:
- Do not modify files or make commits.
- Start by inspecting git state: git status --short, git diff --stat,
  git diff, and git diff --staged when relevant.
- If the scope names files, PR notes, or a topic, prioritize that scope
  but still call out high-impact adjacent issues visible in the diff.
- Report findings first, ordered by severity.
- For each finding include a concrete file:line reference when available,
  the risk, and the minimal fix.
- If you find no issues, say that clearly and mention any residual
  test or review gap.

{focus}"#
    )
}

fn pr_comments_prompt(scope: &str) -> String {
    let snapshot = std::env::current_dir()
        .ok()
        .map(|cwd| crate::commands::code_pr_comments::collect_pr_comments_snapshot(&cwd, scope));
    crate::commands::code_pr_comments::build_pr_comments_prompt(scope, snapshot.as_ref())
}

fn pr_comments_reply_arg(trimmed: &str) -> Option<&str> {
    for prefix in ["/pr_comments reply ", "/pr-comments reply "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_edit_arg(trimmed: &str) -> Option<&str> {
    for prefix in ["/pr_comments edit ", "/pr-comments edit "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_review_arg(trimmed: &str) -> Option<&str> {
    for prefix in [
        "/pr_comments review ",
        "/pr-comments review ",
        "/pr_comments submit ",
        "/pr-comments submit ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_resolve_arg(trimmed: &str) -> Option<&str> {
    for prefix in ["/pr_comments resolve ", "/pr-comments resolve "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_unresolve_arg(trimmed: &str) -> Option<&str> {
    for prefix in [
        "/pr_comments unresolve ",
        "/pr-comments unresolve ",
        "/pr_comments reopen ",
        "/pr-comments reopen ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_viewed_arg(trimmed: &str) -> Option<&str> {
    for prefix in [
        "/pr_comments viewed ",
        "/pr-comments viewed ",
        "/pr_comments view ",
        "/pr-comments view ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_unviewed_arg(trimmed: &str) -> Option<&str> {
    for prefix in [
        "/pr_comments unviewed ",
        "/pr-comments unviewed ",
        "/pr_comments unview ",
        "/pr-comments unview ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_thread_arg(trimmed: &str) -> Option<&str> {
    for prefix in [
        "/pr_comments thread ",
        "/pr-comments thread ",
        "/pr_comments comment ",
        "/pr-comments comment ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_draft_arg(trimmed: &str) -> Option<&str> {
    for prefix in ["/pr_comments draft ", "/pr-comments draft "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn pr_comments_drafts_arg(trimmed: &str) -> Option<&str> {
    for prefix in ["/pr_comments drafts", "/pr-comments drafts"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn parse_pr_comments_reply(input: &str) -> Result<(&str, &str)> {
    let raw = input.trim();
    let Some((thread_id, body)) = raw.split_once(char::is_whitespace) else {
        anyhow::bail!("usage: /pr_comments reply <thread_id> <body>");
    };
    let thread_id = thread_id.trim();
    let body = body.trim();
    if thread_id.is_empty() || body.is_empty() {
        anyhow::bail!("usage: /pr_comments reply <thread_id> <body>");
    }
    Ok((thread_id, body))
}

fn parse_pr_comments_edit(input: &str) -> Result<(&str, &str)> {
    let raw = input.trim();
    let Some((comment_id, body)) = raw.split_once(char::is_whitespace) else {
        anyhow::bail!("usage: /pr_comments edit <comment_id> <body>");
    };
    let comment_id = comment_id.trim();
    let body = body.trim();
    if comment_id.is_empty() || body.is_empty() {
        anyhow::bail!("usage: /pr_comments edit <comment_id> <body>");
    }
    Ok((comment_id, body))
}

fn parse_pr_comments_resolve(input: &str) -> Result<&str> {
    let thread_id = input.trim();
    if thread_id.is_empty() || thread_id.split_whitespace().nth(1).is_some() {
        anyhow::bail!("usage: /pr_comments resolve <thread_id>");
    }
    Ok(thread_id)
}

fn parse_pr_comments_review(input: &str) -> Result<(&str, &str)> {
    let raw = input.trim();
    let (event, body) = raw
        .split_once(char::is_whitespace)
        .map_or((raw, ""), |(event, body)| (event, body.trim()));
    if event.trim().is_empty() {
        anyhow::bail!("usage: /pr_comments review <approve|comment|request_changes> <body>");
    }
    Ok((event.trim(), body))
}

fn parse_pr_comments_file_path(input: &str) -> Result<&str> {
    let path = input.trim();
    if path.is_empty() {
        anyhow::bail!("usage: /pr_comments viewed <path>");
    }
    Ok(path)
}

fn parse_pr_comments_all_files(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "--all" | "all")
}

fn parse_pr_comments_thread(input: &str) -> Result<(&str, u64, &str)> {
    let raw = input.trim();
    let Some((target, body)) = raw.split_once(char::is_whitespace) else {
        anyhow::bail!("usage: /pr_comments thread <path>:<line> <body>");
    };
    let target = target.trim();
    let body = body.trim();
    let Some((path, line)) = target.rsplit_once(':') else {
        anyhow::bail!("usage: /pr_comments thread <path>:<line> <body>");
    };
    let path = path.trim();
    let line = line
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|line| *line > 0)
        .ok_or_else(|| anyhow::anyhow!("line must be a positive integer"))?;
    if path.is_empty() || body.is_empty() {
        anyhow::bail!("usage: /pr_comments thread <path>:<line> <body>");
    }
    Ok((path, line, body))
}

fn parse_pr_comment_draft(input: &str) -> Result<PrCommentDraft> {
    let (path, line, body) = parse_pr_comments_thread(input)?;
    Ok(PrCommentDraft {
        path: path.to_string(),
        line,
        body: body.to_string(),
    })
}

fn reply_to_pr_comment_thread(input: &str) {
    let (thread_id, body) = match parse_pr_comments_reply(input) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let capture = crate::commands::code_pr_comments::reply_review_thread(&cwd, thread_id, body);
    if capture.error.is_none() && capture.status == Some(0) {
        println!("{DIM}  replied to review thread: {thread_id}{RESET}");
        return;
    }
    let detail = capture
        .error
        .as_deref()
        .or_else(|| {
            let stderr = capture.stderr.trim();
            (!stderr.is_empty()).then_some(stderr)
        })
        .or_else(|| {
            let stdout = capture.stdout.trim();
            (!stdout.is_empty()).then_some(stdout)
        })
        .unwrap_or("unknown error");
    eprintln!("{DIM}  /pr_comments: reply failed: {detail}{RESET}");
}

fn resolve_pr_comment_thread(input: &str, resolved: bool) {
    let thread_id = match parse_pr_comments_resolve(input) {
        Ok(thread_id) => thread_id,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let capture = if resolved {
        crate::commands::code_pr_comments::resolve_review_thread(&cwd, thread_id)
    } else {
        crate::commands::code_pr_comments::unresolve_review_thread(&cwd, thread_id)
    };
    if capture.error.is_none() && capture.status == Some(0) {
        let label = if resolved { "resolved" } else { "reopened" };
        println!("{DIM}  {label} review thread: {thread_id}{RESET}");
        return;
    }
    let detail = capture
        .error
        .as_deref()
        .or_else(|| {
            let stderr = capture.stderr.trim();
            (!stderr.is_empty()).then_some(stderr)
        })
        .or_else(|| {
            let stdout = capture.stdout.trim();
            (!stdout.is_empty()).then_some(stdout)
        })
        .unwrap_or("unknown error");
    let action = if resolved { "resolve" } else { "reopen" };
    eprintln!("{DIM}  /pr_comments: {action} failed: {detail}{RESET}");
}

fn edit_pr_comment(input: &str) {
    let (comment_id, body) = match parse_pr_comments_edit(input) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let capture = crate::commands::code_pr_comments::edit_review_comment(&cwd, comment_id, body);
    if capture.error.is_none() && capture.status == Some(0) {
        println!("{DIM}  edited review comment: {comment_id}{RESET}");
        return;
    }
    let detail = capture
        .error
        .as_deref()
        .or_else(|| {
            let stderr = capture.stderr.trim();
            (!stderr.is_empty()).then_some(stderr)
        })
        .or_else(|| {
            let stdout = capture.stdout.trim();
            (!stdout.is_empty()).then_some(stdout)
        })
        .unwrap_or("unknown error");
    eprintln!("{DIM}  /pr_comments: edit failed: {detail}{RESET}");
}

fn submit_pr_review(input: &str) {
    let (event, body) = match parse_pr_comments_review(input) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let capture = crate::commands::code_pr_comments::submit_pull_request_review(
        &cwd, "", event, body,
    );
    if capture.error.is_none() && capture.status == Some(0) {
        println!("{DIM}  submitted PR review: {event}{RESET}");
        return;
    }
    let detail = capture
        .error
        .as_deref()
        .or_else(|| {
            let stderr = capture.stderr.trim();
            (!stderr.is_empty()).then_some(stderr)
        })
        .or_else(|| {
            let stdout = capture.stdout.trim();
            (!stdout.is_empty()).then_some(stdout)
        })
        .unwrap_or("unknown error");
    eprintln!("{DIM}  /pr_comments: review submit failed: {detail}{RESET}");
}

fn mark_pr_comment_file(input: &str, viewed: bool) {
    if parse_pr_comments_all_files(input) {
        mark_all_pr_comment_files(viewed);
        return;
    }
    let path = match parse_pr_comments_file_path(input) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let capture = crate::commands::code_pr_comments::mark_file_viewed(&cwd, "", path, viewed);
    if capture.error.is_none() && capture.status == Some(0) {
        let label = if viewed { "viewed" } else { "unviewed" };
        println!("{DIM}  marked file {label}: {path}{RESET}");
        return;
    }
    let detail = capture
        .error
        .as_deref()
        .or_else(|| {
            let stderr = capture.stderr.trim();
            (!stderr.is_empty()).then_some(stderr)
        })
        .or_else(|| {
            let stdout = capture.stdout.trim();
            (!stdout.is_empty()).then_some(stdout)
        })
        .unwrap_or("unknown error");
    let action = if viewed { "mark viewed" } else { "mark unviewed" };
    eprintln!("{DIM}  /pr_comments: {action} failed: {detail}{RESET}");
}

fn mark_all_pr_comment_files(viewed: bool) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let batch = crate::commands::code_pr_comments::mark_all_files_viewed(&cwd, "", viewed);
    let label = if viewed { "viewed" } else { "unviewed" };
    if batch.total == 0 {
        let detail = batch
            .captures
            .first()
            .and_then(|(_, capture)| {
                capture
                    .error
                    .as_deref()
                    .or_else(|| {
                        let stderr = capture.stderr.trim();
                        (!stderr.is_empty()).then_some(stderr)
                    })
                    .or_else(|| {
                        let stdout = capture.stdout.trim();
                        (!stdout.is_empty()).then_some(stdout)
                    })
            })
            .unwrap_or("no changed PR files were returned");
        eprintln!("{DIM}  /pr_comments: mark all {label} failed: {detail}{RESET}");
        return;
    }
    println!(
        "{DIM}  marked {}/{} PR file(s) {label}{RESET}",
        batch.succeeded, batch.total
    );
    if batch.succeeded < batch.total {
        for (path, capture) in batch
            .captures
            .iter()
            .filter(|(_, capture)| capture.error.is_some() || capture.status != Some(0))
        {
            let detail = capture
                .error
                .as_deref()
                .or_else(|| {
                    let stderr = capture.stderr.trim();
                    (!stderr.is_empty()).then_some(stderr)
                })
                .or_else(|| {
                    let stdout = capture.stdout.trim();
                    (!stdout.is_empty()).then_some(stdout)
                })
                .unwrap_or("unknown error");
            eprintln!("{DIM}    {path}: {detail}{RESET}");
        }
    }
}

fn stage_pr_comment_draft(input: &str, drafts: &mut Vec<PrCommentDraft>) {
    match parse_pr_comment_draft(input) {
        Ok(draft) => {
            println!(
                "{DIM}  staged PR review draft {}:{}. Use /pr_comments drafts submit to publish queued thread(s).{RESET}",
                draft.path, draft.line
            );
            drafts.push(draft);
        }
        Err(e) => eprintln!("{DIM}  /pr_comments: {e:#}{RESET}"),
    }
}

fn print_pr_comment_drafts(drafts: &[PrCommentDraft]) {
    if drafts.is_empty() {
        println!("{DIM}  /pr_comments drafts: no queued draft review threads.{RESET}");
        return;
    }
    println!("{DIM}  /pr_comments drafts: {} queued thread(s):{RESET}", drafts.len());
    for (idx, draft) in drafts.iter().enumerate() {
        println!(
            "{DIM}    {}. {}:{} - {}{RESET}",
            idx + 1,
            draft.path,
            draft.line,
            truncate_chars(&draft.body, 80)
        );
    }
}

fn handle_pr_comment_drafts(input: &str, drafts: &mut Vec<PrCommentDraft>) {
    let action = input.trim().to_ascii_lowercase();
    match action.as_str() {
        "" | "list" | "state" | "status" => print_pr_comment_drafts(drafts),
        "clear" => {
            let count = drafts.len();
            drafts.clear();
            println!(
                "{DIM}  /pr_comments drafts: cleared {count} draft thread{}.{RESET}",
                if count == 1 { "" } else { "s" }
            );
        }
        "submit" | "publish" => submit_pr_comment_drafts(drafts),
        _ => eprintln!(
            "{DIM}  usage: /pr_comments draft <path>:<line> <body>, /pr_comments drafts, /pr_comments drafts submit, or /pr_comments drafts clear{RESET}"
        ),
    }
}

fn submit_pr_comment_drafts(drafts: &mut Vec<PrCommentDraft>) {
    if drafts.is_empty() {
        println!("{DIM}  /pr_comments drafts: no queued draft review threads.{RESET}");
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let total = drafts.len();
    let mut failed = Vec::new();
    for draft in drafts.iter() {
        let capture = crate::commands::code_pr_comments::create_review_thread(
            &cwd,
            "",
            &draft.path,
            draft.line,
            &draft.body,
        );
        if capture.error.is_some() || capture.status != Some(0) {
            let detail = capture
                .error
                .as_deref()
                .or_else(|| {
                    let stderr = capture.stderr.trim();
                    (!stderr.is_empty()).then_some(stderr)
                })
                .or_else(|| {
                    let stdout = capture.stdout.trim();
                    (!stdout.is_empty()).then_some(stdout)
                })
                .unwrap_or("unknown error");
            eprintln!(
                "{DIM}    draft failed for {}:{}: {detail}{RESET}",
                draft.path, draft.line
            );
            failed.push(draft.clone());
        }
    }
    let succeeded = total.saturating_sub(failed.len());
    *drafts = failed;
    println!(
        "{DIM}  /pr_comments drafts: submitted {succeeded}/{total} draft review thread{}.{RESET}",
        if total == 1 { "" } else { "s" }
    );
}

fn create_pr_comment_thread(input: &str) {
    let (path, line, body) = match parse_pr_comments_thread(input) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: {e:#}{RESET}");
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /pr_comments: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let capture = crate::commands::code_pr_comments::create_review_thread(
        &cwd, "", path, line, body,
    );
    if capture.error.is_none() && capture.status == Some(0) {
        println!("{DIM}  created review thread: {path}:{line}{RESET}");
        return;
    }
    let detail = capture
        .error
        .as_deref()
        .or_else(|| {
            let stderr = capture.stderr.trim();
            (!stderr.is_empty()).then_some(stderr)
        })
        .or_else(|| {
            let stdout = capture.stdout.trim();
            (!stdout.is_empty()).then_some(stdout)
        })
        .unwrap_or("unknown error");
    eprintln!("{DIM}  /pr_comments: thread create failed: {detail}{RESET}");
}

fn parse_direct_custom_slash(trimmed: &str) -> Option<(&str, &str)> {
    let raw = trimmed.strip_prefix('/')?;
    if raw.is_empty() {
        return None;
    }
    let (name, args) = raw
        .split_once(char::is_whitespace)
        .map_or((raw, ""), |(name, args)| (name, args.trim()));
    if name.is_empty() || name.contains('/') {
        None
    } else {
        Some((name, args))
    }
}

fn build_custom_slash_prompt(name: &str, args: &str) -> Result<Option<String>> {
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let templates = crate::commands::code_slash_registry::discover(&cwd);
    let needle = name.trim().to_lowercase();
    let Some(hit) = templates
        .iter()
        .find(|cmd| cmd.name == needle)
        .or_else(|| templates.iter().find(|cmd| cmd.name.starts_with(&needle)))
    else {
        return Ok(None);
    };
    Ok(Some(crate::commands::code_slash_registry::expand(hit, args)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentSlashAction {
    Foreground(String),
    Background(BackgroundAgentLaunch),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackgroundAgentLaunch {
    name: String,
    provider: String,
    model: String,
    mode: Mode,
    prompt: String,
    cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartedBackgroundAgent {
    pid: u32,
    log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BackgroundAgentRecord {
    pid: u32,
    name: String,
    provider: String,
    model: String,
    mode: String,
    prompt_preview: String,
    cwd: String,
    log_path: String,
    started_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundAgentStatus {
    Running,
    Exited,
    Unknown,
}

impl BackgroundAgentStatus {
    fn label(self) -> &'static str {
        match self {
            BackgroundAgentStatus::Running => "running",
            BackgroundAgentStatus::Exited => "exited",
            BackgroundAgentStatus::Unknown => "unknown",
        }
    }
}

fn build_agent_slash_action(
    query: &str,
    provider: &str,
    model: &str,
    mode: Mode,
) -> Result<AgentSlashAction> {
    let parsed = parse_agent_slash_query(query)?;
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let agents = crate::commands::code_agents::discover_agents(&cwd)?;
    let prompt = build_agent_prompt_from_defs(&parsed, &agents)?;
    if parsed.background {
        Ok(AgentSlashAction::Background(BackgroundAgentLaunch {
            name: parsed.name.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            mode,
            prompt,
            cwd,
        }))
    } else {
        Ok(AgentSlashAction::Foreground(prompt))
    }
}

fn start_background_agent(launch: &BackgroundAgentLaunch) -> Result<StartedBackgroundAgent> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let log_path = background_agent_log_path(&launch.name)?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let err_log = log
        .try_clone()
        .with_context(|| format!("cloning {}", log_path.display()))?;

    let mut command = background_agent_command(&exe, launch);
    command
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));
    detach_background_command(&mut command);
    let child = command
        .spawn()
        .with_context(|| format!("starting background agent `{}`", launch.name))?;
    let started = started_background_agent(child, log_path);
    persist_background_agent_record(&background_agent_record(launch, &started))?;
    Ok(started)
}

fn started_background_agent(child: Child, log_path: PathBuf) -> StartedBackgroundAgent {
    StartedBackgroundAgent {
        pid: child.id(),
        log_path,
    }
}

fn background_agent_command(exe: &Path, launch: &BackgroundAgentLaunch) -> Command {
    let mut command = Command::new(exe);
    for arg in background_agent_args(exe, launch) {
        command.arg(arg);
    }
    command
}

fn background_agent_args(exe: &Path, launch: &BackgroundAgentLaunch) -> Vec<String> {
    let mut args = Vec::new();
    if !is_lcode_executable(exe) {
        args.push("code".to_string());
    }
    if !launch.provider.trim().is_empty() {
        args.push("--provider".to_string());
        args.push(launch.provider.clone());
    }
    if !launch.model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(launch.model.clone());
    }
    if launch.mode == Mode::Plan {
        args.push("--plan".to_string());
    }
    args.push(launch.prompt.clone());
    args
}

fn is_lcode_executable(exe: &Path) -> bool {
    exe.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "lcode")
}

fn background_agent_log_path(name: &str) -> Result<PathBuf> {
    let safe_name: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let safe_name = safe_name.trim_matches('-');
    let safe_name = if safe_name.is_empty() {
        "agent"
    } else {
        safe_name
    };
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_millis_u64)
        .unwrap_or(0);
    Ok(crate::config::libertai_config_dir()?
        .join("code-background-agents")
        .join(format!("{started_at}-{safe_name}.log")))
}

fn detach_background_command(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn background_agent_record(
    launch: &BackgroundAgentLaunch,
    started: &StartedBackgroundAgent,
) -> BackgroundAgentRecord {
    BackgroundAgentRecord {
        pid: started.pid,
        name: launch.name.clone(),
        provider: launch.provider.clone(),
        model: launch.model.clone(),
        mode: match launch.mode {
            Mode::Normal => "normal",
            Mode::AcceptEdits => "accept-edits",
            Mode::Plan => "plan",
        }
        .to_string(),
        prompt_preview: preview_text(&launch.prompt, 160),
        cwd: launch.cwd.display().to_string(),
        log_path: started.log_path.display().to_string(),
        started_at_ms: now_epoch_ms(),
    }
}

fn background_agent_records_path() -> Result<PathBuf> {
    Ok(crate::config::libertai_config_dir()?
        .join("code-background-agents")
        .join("runs.jsonl"))
}

fn persist_background_agent_record(record: &BackgroundAgentRecord) -> Result<()> {
    let path = background_agent_records_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("writing {}", path.display()))?;
    writeln!(file).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn load_background_agent_records() -> Result<Vec<BackgroundAgentRecord>> {
    let path = background_agent_records_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_str::<BackgroundAgentRecord>(line)
            .with_context(|| format!("parsing {} line {}", path.display(), idx + 1))?;
        out.push(record);
    }
    Ok(out)
}

fn resolve_background_agent_record(input: &str) -> Result<Option<BackgroundAgentRecord>> {
    let records = load_background_agent_records()?;
    if records.is_empty() {
        return Ok(None);
    }
    if input.is_empty() || input == "latest" {
        return Ok(records.into_iter().last());
    }
    let pid = parse_background_agent_pid(input)?;
    Ok(records.into_iter().rev().find(|record| record.pid == pid))
}

fn parse_background_agent_pid(input: &str) -> Result<u32> {
    let raw = input.trim();
    if raw.is_empty() {
        anyhow::bail!("usage: /agents background kill <pid>");
    }
    raw.parse::<u32>()
        .with_context(|| format!("invalid background agent pid `{raw}`"))
}

fn read_log_tail(path: &Path, max_bytes: usize) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let start = bytes.len().saturating_sub(max_bytes);
    let mut text = String::from_utf8_lossy(&bytes[start..]).to_string();
    if start > 0 {
        text.insert_str(0, &format!("[truncated to last {max_bytes} bytes]\n"));
    }
    Ok(text)
}

fn background_agent_status(pid: u32) -> BackgroundAgentStatus {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return match status {
            Ok(status) if status.success() => BackgroundAgentStatus::Running,
            Ok(_) => BackgroundAgentStatus::Exited,
            Err(_) => BackgroundAgentStatus::Unknown,
        };
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        BackgroundAgentStatus::Unknown
    }
}

fn send_background_agent_kill(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .status()
            .context("running kill")?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("kill exited with status {status}")
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        anyhow::bail!("stopping background agents is not supported on this platform yet")
    }
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

fn format_epoch_ms(epoch_ms: u64) -> String {
    if epoch_ms == 0 {
        return "unknown".to_string();
    }
    chrono::DateTime::<chrono::Local>::from(
        UNIX_EPOCH + Duration::from_millis(epoch_ms),
    )
    .format("%Y-%m-%d %H:%M:%S")
    .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentSlashQuery<'a> {
    name: &'a str,
    task: &'a str,
    isolation: Option<AgentSlashIsolation>,
    background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentsCreateQuery<'a> {
    name: &'a str,
    description: Option<&'a str>,
    worktree: bool,
}

fn parse_agents_create_query(query: &str) -> Result<AgentsCreateQuery<'_>> {
    let mut rest = query.trim();
    let mut worktree = false;
    loop {
        let Some((head, tail)) = split_first_word(rest) else {
            anyhow::bail!("usage: /agents create [--worktree] <name> [description]");
        };
        match head {
            "--worktree" | "--isolation=worktree" => {
                worktree = true;
                rest = tail.trim_start();
            }
            "--same-cwd" | "--isolation=same-cwd" => {
                worktree = false;
                rest = tail.trim_start();
            }
            _ => break,
        }
    }
    let Some((name, tail)) = split_first_word(rest) else {
        anyhow::bail!("usage: /agents create [--worktree] <name> [description]");
    };
    let description = tail.trim();
    Ok(AgentsCreateQuery {
        name,
        description: (!description.is_empty()).then_some(description),
        worktree,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentSlashIsolation {
    Worktree,
    SameCwd,
}

fn parse_agent_slash_query(query: &str) -> Result<AgentSlashQuery<'_>> {
    let raw = query.trim();
    let mut isolation = None;
    let mut background = false;
    let mut rest = raw;
    loop {
        let Some((head, tail)) = split_first_word(rest) else {
            anyhow::bail!("usage: /agent [--worktree|--background] <name> <task>");
        };
        match head {
            "--worktree" | "--isolation=worktree" => {
                isolation = Some(AgentSlashIsolation::Worktree);
                rest = tail.trim_start();
            }
            "--same-cwd" | "--isolation=same-cwd" => {
                isolation = Some(AgentSlashIsolation::SameCwd);
                rest = tail.trim_start();
            }
            "--background" | "--detached" => {
                background = true;
                rest = tail.trim_start();
            }
            _ => break,
        }
    }
    let Some((name, task)) = rest.split_once(char::is_whitespace) else {
        anyhow::bail!("usage: /agent [--worktree|--background] <name> <task>");
    };
    let name = name.trim();
    let task = task.trim();
    if name.is_empty() || task.is_empty() {
        anyhow::bail!("usage: /agent [--worktree|--background] <name> <task>");
    }
    Ok(AgentSlashQuery {
        name,
        task,
        isolation,
        background,
    })
}

fn split_first_word(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.find(char::is_whitespace) {
        Some(idx) => Some((&trimmed[..idx], &trimmed[idx..])),
        None => Some((trimmed, "")),
    }
}

fn build_agent_prompt_from_defs(
    query: &AgentSlashQuery<'_>,
    agents: &[crate::commands::code_agents::AgentDefinition],
) -> Result<String> {
    let needle = query.name.trim().trim_start_matches('@');
    let Some(agent) = agents
        .iter()
        .find(|agent| agent.name == needle)
        .or_else(|| agents.iter().find(|agent| agent.name.starts_with(needle)))
    else {
        let suffix = if agents.is_empty() {
            "no named sub-agents are configured".to_string()
        } else {
            format!(
                "available sub-agents: {}",
                agents
                    .iter()
                    .map(|agent| agent.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        anyhow::bail!("unknown agent `{}` ({suffix})", query.name);
    };
    let use_worktree = match query.isolation {
        Some(AgentSlashIsolation::Worktree) => true,
        Some(AgentSlashIsolation::SameCwd) => false,
        None => agent.worktree,
    };
    let isolation = if use_worktree {
        " and isolation: \"worktree\""
    } else {
        ""
    };
    Ok(format!(
        "Use the task tool with subagent_type \"{}\"{} for this focused task:\n\n{}\n\nReturn the named sub-agent's findings and cite any files or commands it used.",
        agent.name, isolation, query.task
    ))
}

fn agent_source_label(source: &crate::commands::code_agents::AgentSource) -> String {
    match source {
        crate::commands::code_agents::AgentSource::Project(path) => {
            format!("project: {}", path.display())
        }
        crate::commands::code_agents::AgentSource::User(path) => {
            format!("user: {}", path.display())
        }
    }
}

fn open_memory_editor(path: &Path) {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let cmd = format!("{editor} {}", quote_for_sh(path));
    match Command::new("/bin/sh").arg("-c").arg(&cmd).status() {
        Ok(status) if status.success() => {
            println!("{DIM}  memory saved: {}{RESET}", path.display());
            println!("{DIM}  changes take effect in new agent sessions.{RESET}");
        }
        Ok(status) => {
            eprintln!("{DIM}  editor exited with status {status}{RESET}");
        }
        Err(e) => {
            eprintln!("{DIM}  failed to launch editor `{editor}`: {e}{RESET}");
        }
    }
}

fn quote_for_sh(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellEscapeResult {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellEscapeAction {
    Run(String),
    Usage(&'static str),
}

fn shell_escape_command(rest: &str, last: Option<&str>) -> ShellEscapeAction {
    let command = rest.trim();
    if command.is_empty() {
        return ShellEscapeAction::Usage(
            "usage: !<command> — run a local shell command in this cwd; !! repeats the last shell command",
        );
    }
    if command == "!" {
        return match last {
            Some(last) if !last.trim().is_empty() => ShellEscapeAction::Run(last.to_string()),
            _ => ShellEscapeAction::Usage("no previous shell command to repeat"),
        };
    }
    ShellEscapeAction::Run(command.to_string())
}

fn run_shell_escape(command: &str, wrapper: Option<&[String]>) {
    println!("{BOLD}$ {command}{RESET}");
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  shell: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match execute_shell_escape(&cwd, command, wrapper) {
        Ok(result) => print_shell_escape_result(&result),
        Err(e) => eprintln!("{DIM}  shell: {e:#}{RESET}"),
    }
}

fn execute_shell_escape(
    cwd: &Path,
    command: &str,
    wrapper: Option<&[String]>,
) -> Result<ShellEscapeResult> {
    let argv = shell_escape_argv(wrapper);
    let Some((program, args)) = argv.split_first() else {
        anyhow::bail!("empty shell argv");
    };
    let output = Command::new(program)
        .args(args)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn shell escape via {}", argv.join(" ")))?;
    Ok(ShellEscapeResult {
        stdout: truncate_shell_output(&String::from_utf8_lossy(&output.stdout)),
        stderr: truncate_shell_output(&String::from_utf8_lossy(&output.stderr)),
        exit_code: output.status.code(),
    })
}

fn shell_escape_argv(wrapper: Option<&[String]>) -> Vec<String> {
    match wrapper.filter(|w| !w.is_empty()) {
        Some(wrapper) => {
            let mut argv = wrapper.to_vec();
            argv.push("/bin/sh".to_string());
            argv
        }
        None => vec!["/bin/sh".to_string()],
    }
}

fn truncate_shell_output(raw: &str) -> String {
    if raw.len() <= SHELL_ESCAPE_MAX_DISPLAY_BYTES {
        return raw.to_string();
    }
    let mut end = SHELL_ESCAPE_MAX_DISPLAY_BYTES;
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated after {} bytes]",
        &raw[..end],
        SHELL_ESCAPE_MAX_DISPLAY_BYTES
    )
}

fn print_shell_escape_result(result: &ShellEscapeResult) {
    if result.stdout.is_empty() && result.stderr.is_empty() {
        println!("{DIM}  (no output){RESET}");
    } else {
        if !result.stdout.is_empty() {
            print!("{}", result.stdout);
            if !result.stdout.ends_with('\n') {
                println!();
            }
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
            if !result.stderr.ends_with('\n') {
                eprintln!();
            }
        }
    }
    match result.exit_code {
        Some(0) => println!("{DIM}  exit 0{RESET}"),
        Some(code) => println!("{DIM}  exit {code}{RESET}"),
        None => println!("{DIM}  terminated by signal{RESET}"),
    }
    println!();
}

fn print_session_status(
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
    usage: Option<UsageSummary>,
) {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    println!("{BOLD}status{RESET}");
    println!("{DIM}  provider:{RESET} {provider}");
    println!("{DIM}  model:{RESET} {model}");
    println!("{DIM}  mode:{RESET} {}", mode_label(mode));
    println!("{DIM}  output-style:{RESET} {}", output_style.unwrap_or("default"));
    println!("{DIM}  cwd:{RESET} {cwd}");
    println!("{DIM}  default provider:{RESET} {}", cfg.default_code_provider);
    println!("{DIM}  default code model:{RESET} {}", cfg.default_code_model);
    if let Some(summary) = usage {
        println!(
            "{DIM}  usage:{RESET} {} turn(s), {} ctx high-water, {} output total",
            summary.turns,
            human_tokens(summary.context_high_water),
            human_tokens(summary.output_total)
        );
    } else {
        println!("{DIM}  usage:{RESET} no turns recorded");
    }
    println!();
}

async fn print_doctor(
    handle: &AgentSessionHandle,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
    approvals: &ApprovalState,
    usage: Option<UsageSummary>,
) {
    let cwd = std::env::current_dir();
    let cwd_label = cwd
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));

    println!("{BOLD}doctor{RESET}");
    println!("{DIM}  cwd:{RESET} {cwd_label}");
    println!("{DIM}  provider/model:{RESET} {provider}/{model}");
    println!("{DIM}  mode:{RESET} {}", mode_label(mode));
    println!("{DIM}  output-style:{RESET} {}", output_style.unwrap_or("default"));

    match handle.state().await {
        Ok(state) => {
            println!(
                "{}",
                doctor_line(
                    true,
                    "pi session",
                    state.session_id.as_deref().unwrap_or("not persisted")
                )
            );
            println!(
                "{}",
                doctor_line(
                    state.save_enabled,
                    "session persistence",
                    if state.save_enabled { "enabled" } else { "disabled" }
                )
            );
            println!(
                "{}",
                doctor_line(true, "transcript", format!("{} message(s)", state.message_count))
            );
            if let Some(level) = state.thinking_level {
                println!("{}", doctor_line(true, "thinking", level.to_string()));
            }
        }
        Err(e) => println!("{}", doctor_line(false, "pi session", e.to_string())),
    }

    println!(
        "{}",
        doctor_line(
            cfg.auth.api_key.is_some(),
            "LibertAI auth",
            cfg.auth
                .api_key
                .as_deref()
                .map(mask_key)
                .unwrap_or_else(|| "not logged in".to_string())
        )
    );
    println!(
        "{}",
        doctor_line(
            true,
            "defaults",
            format!("{}/{}", cfg.default_code_provider, cfg.default_code_model)
        )
    );
    println!(
        "{}",
        doctor_line(
            true,
            "smart approvals",
            if cfg.smart_approval_enabled {
                format!("enabled ({})", cfg.smart_approval_model)
            } else {
                "disabled".to_string()
            }
        )
    );
    println!(
        "{}",
        doctor_line(
            true,
            "remembered approvals",
            format!("{} saved rule(s)", approvals.always_rules().len())
        )
    );
    println!(
        "{}",
        doctor_line(
            true,
            "hooks",
            format_hook_event_breakdown(cfg)
        )
    );
    println!(
        "{}",
        doctor_line(
            false,
            "mcp registry",
            "not persisted in CLI config; desktop owns MCP server discovery/cache"
        )
    );
    match crate::config::config_path() {
        Ok(path) => println!("{}", doctor_line(true, "config path", path.display().to_string())),
        Err(e) => println!("{}", doctor_line(false, "config path", e.to_string())),
    }

    if let Ok(cwd) = cwd.as_ref() {
        match crate::commands::code_memory::read_memory(cwd) {
            Ok(doc) => {
                let detail = if doc.exists {
                    doc.path.display().to_string()
                } else {
                    format!("missing ({})", doc.path.display())
                };
                println!("{}", doctor_line(doc.exists, "project memory", detail));
            }
            Err(e) => println!("{}", doctor_line(false, "project memory", e.to_string())),
        }
        match crate::commands::code_memory::list_memory_files(cwd) {
            Ok(files) => println!(
                "{}",
                doctor_line(true, "memory sidecars", format!("{} file(s)", files.len()))
            ),
            Err(e) => println!("{}", doctor_line(false, "memory sidecars", e.to_string())),
        }
        match crate::commands::code_memory::verify_memory_references(cwd) {
            Ok(refs) => println!(
                "{}",
                doctor_line(true, "memory references", format_memory_reference_summary(&refs))
            ),
            Err(e) => println!("{}", doctor_line(false, "memory references", e.to_string())),
        }
        match crate::commands::code_agents::discover_agents(cwd) {
            Ok(agents) => println!(
                "{}",
                doctor_line(true, "named agents", format_agent_doctor_summary(&agents))
            ),
            Err(e) => println!("{}", doctor_line(false, "named agents", e.to_string())),
        }
        let templates = crate::commands::code_slash_registry::discover(cwd);
        println!(
            "{}",
            doctor_line(
                true,
                "custom slash commands",
                format_custom_slash_doctor_summary(&templates)
            )
        );
        match code_skills::skill_inventory(SkillPillar::Code, Some(cwd)) {
            Ok(skills) => println!(
                "{}",
                doctor_line(true, "skills", format_skill_doctor_summary(&skills))
            ),
            Err(e) => println!("{}", doctor_line(false, "skills", e.to_string())),
        }
        match git_status_short_in(cwd) {
            Ok(lines) if lines.len() <= 1 => println!("{}", doctor_line(true, "git status", "clean")),
            Ok(lines) => println!(
                "{}",
                doctor_line(
                    true,
                    "git status",
                    format!("{} changed/untracked line(s)", lines.len().saturating_sub(1))
                )
            ),
            Err(e) => println!("{}", doctor_line(false, "git status", e.to_string())),
        }
    }

    match usage {
        Some(summary) => println!(
            "{}",
            doctor_line(
                true,
                "usage",
                format!(
                    "{} turn(s), {} ctx high-water",
                    summary.turns,
                    human_tokens(summary.context_high_water)
                )
            )
        ),
        None => println!("{}", doctor_line(true, "usage", "no completed turns yet")),
    }
    println!();
}

fn doctor_line(ok: bool, label: &str, detail: impl AsRef<str>) -> String {
    let status = if ok { "ok" } else { "warn" };
    let detail = detail.as_ref();
    if detail.is_empty() {
        format!("{DIM}  [{status}]{RESET} {label}")
    } else {
        format!("{DIM}  [{status}]{RESET} {label}: {detail}")
    }
}

fn format_hook_event_breakdown(cfg: &LibertaiConfig) -> String {
    let rows = [
        ("UserPromptSubmit", count_runnable_hooks(&cfg.hooks.user_prompt_submit)),
        ("PreToolUse", count_runnable_hooks(&cfg.hooks.pre_tool_use)),
        ("PostToolUse", count_runnable_hooks(&cfg.hooks.post_tool_use)),
        ("SubagentStop", count_runnable_hooks(&cfg.hooks.subagent_stop)),
        ("SessionStart", count_runnable_hooks(&cfg.hooks.session_start)),
        ("Stop", count_runnable_hooks(&cfg.hooks.stop)),
        ("SessionEnd", count_runnable_hooks(&cfg.hooks.session_end)),
        ("Notification", count_runnable_hooks(&cfg.hooks.notification)),
    ];
    let total: usize = rows.iter().map(|(_, count)| *count).sum();
    let events = rows
        .iter()
        .map(|(event, count)| format!("{event} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{total} runnable hook(s); {events}")
}

fn format_agent_doctor_summary(
    agents: &[crate::commands::code_agents::AgentDefinition],
) -> String {
    let worktree = agents.iter().filter(|agent| agent.worktree).count();
    format!("{} loaded ({worktree} worktree default)", agents.len())
}

fn format_custom_slash_doctor_summary(
    commands: &[crate::commands::code_slash_registry::CustomCommand],
) -> String {
    let project = commands
        .iter()
        .filter(|cmd| {
            matches!(
                cmd.source,
                crate::commands::code_slash_registry::CommandSource::Project
            )
        })
        .count();
    let user = commands.len().saturating_sub(project);
    format!("{} loaded ({project} project, {user} user)", commands.len())
}

fn format_skill_doctor_summary(
    skills: &[crate::commands::code_skills::SkillInventoryEntry],
) -> String {
    let enabled = skills.iter().filter(|skill| skill.enabled).count();
    format!("{enabled}/{} enabled", skills.len())
}

fn format_memory_reference_summary(
    refs: &[crate::commands::code_memory::MemoryReference],
) -> String {
    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut external = 0usize;
    let mut unparsed = 0usize;
    for reference in refs {
        match reference.status {
            crate::commands::code_memory::MemoryReferenceStatus::Ok => ok += 1,
            crate::commands::code_memory::MemoryReferenceStatus::Missing => missing += 1,
            crate::commands::code_memory::MemoryReferenceStatus::External => external += 1,
            crate::commands::code_memory::MemoryReferenceStatus::Unparsed => unparsed += 1,
        }
    }
    format!(
        "{} total (ok {ok}, missing {missing}, external {external}, unparsed {unparsed})",
        refs.len()
    )
}

fn usage_summary(records: &[UsageRecord]) -> Option<UsageSummary> {
    let last = records.last()?;
    Some(UsageSummary {
        turns: records.len(),
        last_input: last.input,
        last_output: last.output,
        output_total: records.iter().map(|r| r.output).sum(),
        context_high_water: records.iter().map(|r| r.input).max().unwrap_or(0),
        context_window: last.context_window,
        provider: last.provider.clone(),
        model: last.model.clone(),
    })
}

fn print_usage_summary(summary: Option<UsageSummary>, tool_activity: &[ToolActivitySummary]) {
    println!("{BOLD}usage{RESET}");
    match summary.as_ref() {
        Some(summary) => {
            println!("{DIM}  provider/model:{RESET} {}/{}", summary.provider, summary.model);
            println!("{DIM}  turns:{RESET} {}", summary.turns);
            println!(
                "{DIM}  last turn:{RESET} {} in · {} out",
                human_tokens(summary.last_input),
                human_tokens(summary.last_output)
            );
            println!(
                "{DIM}  session output total:{RESET} {}",
                human_tokens(summary.output_total)
            );
            if summary.context_window > 0 {
                let pct = ((summary.context_high_water as f64
                    / f64::from(summary.context_window))
                    * 100.0)
                    .round()
                    .min(100.0) as u32;
                println!(
                    "{DIM}  context high-water:{RESET} {pct}% · {} / {}",
                    human_tokens(summary.context_high_water),
                    human_tokens(u64::from(summary.context_window))
                );
            } else {
                println!(
                    "{DIM}  context high-water:{RESET} {}",
                    human_tokens(summary.context_high_water)
                );
            }
            println!(
                "{DIM}  note:{RESET} input is cumulative context; output is summed across completed turns."
            );
            print_tool_activity(tool_activity, Some(summary));
        }
        None => {
            println!("{DIM}  no usage recorded yet — send a prompt first.{RESET}");
            print_tool_activity(tool_activity, None);
        }
    }
    println!();
}

fn parse_usage_export_command(input: &str) -> Option<UsageExportFormat> {
    let raw = input.trim();
    let rest = raw
        .strip_prefix("/usage export")
        .or_else(|| raw.strip_prefix("/cost export"))?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim().to_ascii_lowercase().as_str() {
        "" | "json" => Some(UsageExportFormat::Json),
        "csv" => Some(UsageExportFormat::Csv),
        _ => None,
    }
}

fn print_usage_export(
    summary: Option<UsageSummary>,
    tool_activity: &[ToolActivitySummary],
    format: UsageExportFormat,
) {
    match format {
        UsageExportFormat::Json => {
            println!("{}", usage_export_json(summary.as_ref(), tool_activity))
        }
        UsageExportFormat::Csv => print!("{}", usage_export_csv(summary.as_ref(), tool_activity)),
    }
}

fn usage_export_json(
    summary: Option<&UsageSummary>,
    tool_activity: &[ToolActivitySummary],
) -> String {
    let tool_rows = summary
        .map(|summary| estimate_tool_attribution(summary, tool_activity))
        .unwrap_or_default();
    let tools = if tool_rows.is_empty() {
        tool_activity
            .iter()
            .map(|tool| {
                json!({
                    "toolName": tool.tool_name,
                    "count": tool.count,
                    "observedDurationMs": tool.total_duration.as_millis() as u64,
                    "estimatedTokens": null,
                    "estimatedCostUsd": null,
                })
            })
            .collect::<Vec<_>>()
    } else {
        tool_rows
            .iter()
            .map(|row| {
                json!({
                    "toolName": row.tool_name,
                    "count": row.count,
                    "observedDurationMs": row.total_duration.as_millis() as u64,
                    "estimatedTokens": row.estimated_tokens,
                    "estimatedCostUsd": row.estimated_cost,
                })
            })
            .collect::<Vec<_>>()
    };
    let usage = summary.map(|summary| {
        json!({
            "provider": summary.provider,
            "model": summary.model,
            "turns": summary.turns,
            "lastInputTokens": summary.last_input,
            "lastOutputTokens": summary.last_output,
            "outputTotalTokens": summary.output_total,
            "contextHighWaterTokens": summary.context_high_water,
            "contextWindow": summary.context_window,
            "estimatedCostUsd": model_token_cost(
                &summary.model,
                summary.context_high_water,
                summary.output_total,
            ),
        })
    });
    serde_json::to_string_pretty(&json!({
        "kind": "libertai_code_usage_export",
        "version": 1,
        "usage": usage,
        "tools": tools,
        "provenance": {
            "usage": "Input is cumulative context high-water; output is summed across completed turns.",
            "toolAttribution": "Estimated by distributing session tokens/cost across observed tool calls, weighted by observed duration when available.",
            "rates": "Static model rates from libertai-cli pricing table; provider-measured per-tool billing is not available."
        }
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

fn usage_export_csv(
    summary: Option<&UsageSummary>,
    tool_activity: &[ToolActivitySummary],
) -> String {
    let mut out = String::from(
        "category,name,count,input_tokens,output_tokens,estimated_tokens,estimated_cost_usd,duration_ms,provenance\n",
    );
    if let Some(summary) = summary {
        out.push_str(&format!(
            "usage,{},{},{},{},{},{},{},{}\n",
            csv_cell(&format!("{}/{}", summary.provider, summary.model)),
            summary.turns,
            summary.context_high_water,
            summary.output_total,
            "",
            summary
                .model_token_cost()
                .map(|cost| format!("{cost:.8}"))
                .unwrap_or_default(),
            "",
            csv_cell("input=context high-water; output=sum of completed turns")
        ));
        for row in estimate_tool_attribution(summary, tool_activity) {
            out.push_str(&format!(
                "tool,{},{},{},{},{},{},{},{}\n",
                csv_cell(&row.tool_name),
                row.count,
                "",
                "",
                row.estimated_tokens,
                row.estimated_cost
                    .map(|cost| format!("{cost:.8}"))
                    .unwrap_or_default(),
                row.total_duration.as_millis(),
                csv_cell("estimated duration-weighted attribution")
            ));
        }
    } else {
        for tool in tool_activity {
            out.push_str(&format!(
                "tool,{},{},{},{},{},{},{},{}\n",
                csv_cell(&tool.tool_name),
                tool.count,
                "",
                "",
                "",
                "",
                tool.total_duration.as_millis(),
                csv_cell("observed tool activity only; no usage recorded")
            ));
        }
    }
    out
}

impl UsageSummary {
    fn model_token_cost(&self) -> Option<f64> {
        model_token_cost(&self.model, self.context_high_water, self.output_total)
    }
}

fn csv_cell(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn print_tool_activity(tool_activity: &[ToolActivitySummary], usage: Option<&UsageSummary>) {
    if tool_activity.is_empty() {
        return;
    }
    let attribution = usage.map(|summary| estimate_tool_attribution(summary, tool_activity));
    if let Some(rows) = attribution.filter(|rows| !rows.is_empty()) {
        println!("{DIM}  tool activity · estimated attribution:{RESET}");
        for row in rows {
            let estimate = match row.estimated_cost {
                Some(cost) if cost > 0.0 => {
                    format!("{} est · ~{}", dollar(cost), human_tokens(row.estimated_tokens))
                }
                _ => format!("~{}", human_tokens(row.estimated_tokens)),
            };
            println!(
                "{DIM}    -{RESET} {}: {} call(s), {} observed, {estimate}",
                row.tool_name,
                row.count,
                format_duration(row.total_duration)
            );
        }
        println!(
            "{DIM}  note:{RESET} tool attribution is estimated from session tokens/cost and weighted by observed tool duration."
        );
    } else {
        println!("{DIM}  tool activity:{RESET}");
        for tool in tool_activity {
            println!(
                "{DIM}    -{RESET} {}: {} call(s), {} observed",
                tool.tool_name,
                tool.count,
                format_duration(tool.total_duration)
            );
        }
    }
}

fn estimate_tool_attribution(
    summary: &UsageSummary,
    tool_activity: &[ToolActivitySummary],
) -> Vec<ToolAttribution> {
    let estimated_tokens = summary.context_high_water.saturating_add(summary.output_total);
    if estimated_tokens == 0 || tool_activity.is_empty() {
        return Vec::new();
    }
    let weights: Vec<f64> = tool_activity
        .iter()
        .map(|tool| {
            let millis = tool.total_duration.as_millis() as f64;
            if millis > 0.0 { millis } else { tool.count.max(1) as f64 }
        })
        .collect();
    let total_weight: f64 = weights.iter().sum();
    if total_weight <= 0.0 {
        return Vec::new();
    }
    let estimated_total_cost = model_token_cost(
        &summary.model,
        summary.context_high_water,
        summary.output_total,
    );
    tool_activity
        .iter()
        .zip(weights)
        .map(|(tool, weight)| {
            let share = weight / total_weight;
            ToolAttribution {
                tool_name: tool.tool_name.clone(),
                count: tool.count,
                total_duration: tool.total_duration,
                estimated_tokens: (estimated_tokens as f64 * share).round() as u64,
                estimated_cost: estimated_total_cost.map(|cost| cost * share),
            }
        })
        .collect()
}

fn model_token_cost(model: &str, input_tokens: u64, output_tokens: u64) -> Option<f64> {
    let (input_per_million, output_per_million) = model_token_rates(model)?;
    Some(
        ((input_tokens as f64) * input_per_million
            + (output_tokens as f64) * output_per_million)
            / 1_000_000.0,
    )
}

fn model_token_rates(model: &str) -> Option<(f64, f64)> {
    let m = model.to_ascii_lowercase();
    const TABLE: &[(&[&str], f64, f64)] = &[
        (&["opus-4.7", "opus 4.7"], 15.00, 75.00),
        (&["opus-4", "opus 4"], 15.00, 75.00),
        (&["sonnet-4.6", "sonnet 4.6"], 3.00, 15.00),
        (&["sonnet-4.5", "sonnet 4.5"], 3.00, 15.00),
        (&["sonnet-4", "sonnet 4"], 3.00, 15.00),
        (&["haiku-4.5", "haiku 4.5"], 1.00, 5.00),
        (&["haiku-4", "haiku 4"], 1.00, 5.00),
        (&["gpt-4o-mini"], 0.15, 0.60),
        (&["gpt-4o"], 2.50, 10.00),
        (&["gpt-4.1-mini"], 0.40, 1.60),
        (&["gpt-4.1"], 2.00, 8.00),
        (&["o1-mini"], 1.10, 4.40),
        (&["o1"], 15.00, 60.00),
        (
            &["qwen3.6-35b-a3b", "qwen3.6 35b a3b", "qwen3-6-35b-a3b"],
            0.15,
            1.00,
        ),
        (&["qwen3.6-27b", "qwen3.6 27b", "qwen3-6-27b"], 0.32, 3.20),
        (
            &[
                "qwen3.5-122b-a10b",
                "qwen3.5 122b a10b",
                "qwen3-5-122b-a10b",
            ],
            0.40,
            2.00,
        ),
        (
            &["qwen3.5-35b-a3b", "qwen3.5 35b a3b", "qwen3-5-35b-a3b"],
            0.25,
            2.00,
        ),
        (&["qwen3-coder-480b"], 1.00, 3.00),
        (&["qwen3-coder"], 0.22, 0.95),
        (&["deepseek-v3"], 0.50, 1.50),
        (&["glm-4.6"], 0.40, 1.20),
        (&["llama-3.3", "llama 3.3"], 0.30, 0.90),
        (&["mixtral"], 0.50, 1.50),
    ];
    for (keys, input, output) in TABLE {
        if keys.iter().any(|key| m.contains(key)) {
            return Some((*input, *output));
        }
    }
    None
}

fn dollar(value: f64) -> String {
    format!("${:.2}", value.max(0.0))
}

fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", millis as f64 / 1_000.0)
    } else {
        let minutes = millis / 60_000;
        let seconds = (millis % 60_000) / 1_000;
        format!("{minutes}m {seconds}s")
    }
}

fn print_config_status(cfg: &LibertaiConfig) {
    println!("{BOLD}config{RESET}");
    println!("{DIM}  api base:{RESET} {}", cfg.api_base);
    if cfg.account_base != cfg.api_base {
        println!("{DIM}  account base:{RESET} {}", cfg.account_base);
    }
    println!("{DIM}  default chat model:{RESET} {}", cfg.default_chat_model);
    println!("{DIM}  default code provider:{RESET} {}", cfg.default_code_provider);
    println!("{DIM}  default code model:{RESET} {}", cfg.default_code_model);
    println!("{DIM}  default image model:{RESET} {}", cfg.default_image_model);
    if cfg.smart_approval_enabled {
        println!(
            "{DIM}  smart approvals:{RESET} enabled ({})",
            cfg.smart_approval_model
        );
    } else {
        println!("{DIM}  smart approvals:{RESET} disabled");
    }
    println!(
        "{DIM}  auto compaction:{RESET} {} (reserve={}, keep_recent={})",
        if cfg.code_auto_compaction_enabled {
            "enabled"
        } else {
            "disabled"
        },
        cfg.code_compaction_reserve_tokens,
        cfg.code_compaction_keep_recent_tokens
    );
    println!(
        "{DIM}  turn notifications:{RESET} {}",
        if cfg.code_turn_notifications {
            "on"
        } else {
            "off"
        }
    );
    let user_prompt_hooks = count_runnable_hooks(&cfg.hooks.user_prompt_submit);
    let pre_tool_hooks = count_runnable_hooks(&cfg.hooks.pre_tool_use);
    let post_tool_hooks = count_runnable_hooks(&cfg.hooks.post_tool_use);
    let subagent_stop_hooks = count_runnable_hooks(&cfg.hooks.subagent_stop);
    let session_start_hooks = count_runnable_hooks(&cfg.hooks.session_start);
    let stop_hooks = count_runnable_hooks(&cfg.hooks.stop);
    let session_end_hooks = count_runnable_hooks(&cfg.hooks.session_end);
    let notification_hooks = count_runnable_hooks(&cfg.hooks.notification);
    println!(
        "{DIM}  hooks:{RESET} {user_prompt_hooks} UserPromptSubmit, \
         {pre_tool_hooks} PreToolUse, {post_tool_hooks} PostToolUse, \
         {subagent_stop_hooks} SubagentStop, \
         {session_start_hooks} SessionStart, {stop_hooks} Stop, \
         {session_end_hooks} SessionEnd, \
         {notification_hooks} Notification hook(s)"
    );
    match cfg.auth.api_key.as_deref() {
        Some(key) => println!("{DIM}  auth:{RESET} {}", mask_key(key)),
        None => println!("{DIM}  auth:{RESET} not logged in"),
    }
    println!(
        "{DIM}  edit:{RESET} /config set <key> <value>, /config unset <key>, or libertai config set|unset"
    );
    println!();
}

fn handle_repl_config_command(raw: &str, cfg: &mut Arc<LibertaiConfig>) -> Result<()> {
    let action = raw.trim();
    if action.is_empty()
        || action.eq_ignore_ascii_case("show")
        || action.eq_ignore_ascii_case("status")
    {
        print_config_status(cfg);
        return Ok(());
    }
    if action.eq_ignore_ascii_case("path") {
        let path = crate::config::config_path()?;
        println!("{DIM}  config path: {}{RESET}", path.display());
        return Ok(());
    }
    if let Some(target) = parse_config_settings_target(action) {
        print_config_settings_target(target)?;
        return Ok(());
    }

    let mut parts = action.splitn(3, char::is_whitespace);
    let verb = parts.next().unwrap_or("");
    match verb {
        "set" => {
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            if key.is_empty() || value.is_empty() {
                anyhow::bail!("usage: /config set <key> <value>");
            }
            set_repl_config_value(cfg, key, value)?;
            println!("{DIM}  config updated: {key} = {value}{RESET}");
        }
        "unset" | "reset" => {
            let key = parts.next().unwrap_or("").trim();
            if key.is_empty() || parts.next().is_some() {
                anyhow::bail!("usage: /config unset <key>");
            }
            unset_repl_config_value(cfg, key)?;
            println!("{DIM}  config reset: {key}{RESET}");
        }
        _ => print_config_status(cfg),
    }
    Ok(())
}

fn parse_config_settings_target(action: &str) -> Option<ConfigSettingsTarget> {
    match action.trim().to_ascii_lowercase().as_str() {
        "account" | "accounts" | "login" | "auth" | "key" | "api" => {
            Some(ConfigSettingsTarget::Account)
        }
        "open" | "settings" | "backends" | "backend" | "provider" | "providers" | "model"
        | "models" => Some(ConfigSettingsTarget::Backends),
        "defaults" | "default" => Some(ConfigSettingsTarget::Defaults),
        "agents" | "agent" | "sub-agents" | "subagents" => Some(ConfigSettingsTarget::Agents),
        "skills" | "skill" => Some(ConfigSettingsTarget::Skills),
        "hooks" | "hook" => Some(ConfigSettingsTarget::Hooks),
        "mcp" | "mcp-server" | "mcp-servers" => Some(ConfigSettingsTarget::Mcp),
        "approvals" | "approval" | "permissions" | "permission" => {
            Some(ConfigSettingsTarget::Approvals)
        }
        "appearance" | "theme" | "themes" => Some(ConfigSettingsTarget::Appearance),
        "sandbox" | "bash-sandbox" | "bash sandbox" => Some(ConfigSettingsTarget::Sandbox),
        "advanced" | "advance" => Some(ConfigSettingsTarget::Advanced),
        _ => None,
    }
}

fn print_config_settings_target(target: ConfigSettingsTarget) -> Result<()> {
    let path = crate::config::config_path().context("resolve config path")?;
    println!("{BOLD}config{RESET}");
    match target {
        ConfigSettingsTarget::Account => {
            println!("{DIM}  desktop: /settings account jumps to Settings > Account.{RESET}");
            println!(
                "{DIM}  terminal: LibertAI account auth lives in {}; use /login status, /login, or /logout for terminal auth.{RESET}",
                path.display()
            );
        }
        ConfigSettingsTarget::Backends => {
            println!(
                "{DIM}  desktop: /config open jumps to Settings > Backends for provider keys.{RESET}"
            );
            println!(
                "{DIM}  terminal: LibertAI account auth lives in {}; provider-specific keys are managed in desktop Settings > Backends.{RESET}",
                path.display()
            );
            println!("{DIM}  use /login status to inspect terminal auth state.{RESET}");
        }
        ConfigSettingsTarget::Defaults => {
            println!("{DIM}  desktop: /settings defaults jumps to Settings > Defaults.{RESET}");
            println!(
                "{DIM}  terminal: default provider/model and scoped-model defaults live in {}; use /model and /scoped-models for session-local changes.{RESET}",
                path.display()
            );
        }
        ConfigSettingsTarget::Agents => {
            println!("{DIM}  desktop: /settings agents jumps to Settings > Sub-agents.{RESET}");
            println!(
                "{DIM}  terminal: use /agents, /agents open, /agent, and /agents background for named sub-agent workflows.{RESET}"
            );
        }
        ConfigSettingsTarget::Skills => {
            println!("{DIM}  desktop: /settings skills jumps to Settings > Skills.{RESET}");
            println!(
                "{DIM}  terminal: use /skills, /skills enable <name>, and /skills disable <name> for future sessions.{RESET}"
            );
        }
        ConfigSettingsTarget::Hooks => {
            println!("{DIM}  desktop: /settings hooks jumps to Settings > Hooks.{RESET}");
            println!(
                "{DIM}  terminal: use /hooks status, /hooks open, and config.toml hook rows for CLI hook management.{RESET}"
            );
        }
        ConfigSettingsTarget::Mcp => {
            println!("{DIM}  desktop: /settings mcp jumps to Settings > MCP servers.{RESET}");
            println!(
                "{DIM}  terminal: use /mcp status, /mcp probe, /mcp probe --save, /mcp refresh, and config.toml mcpServers rows.{RESET}"
            );
        }
        ConfigSettingsTarget::Approvals => {
            println!("{DIM}  desktop: /settings approvals jumps to Settings > Approvals.{RESET}");
            println!(
                "{DIM}  terminal: use /permissions, /permissions open, or /forget to inspect and clear remembered allow rules.{RESET}"
            );
        }
        ConfigSettingsTarget::Appearance => {
            println!("{DIM}  desktop: /settings appearance jumps to Settings > Appearance.{RESET}");
            println!(
                "{DIM}  terminal: use /theme to inspect desktop appearance support; terminal colors are controlled by your emulator.{RESET}"
            );
        }
        ConfigSettingsTarget::Sandbox => {
            println!("{DIM}  desktop: /settings sandbox jumps to Settings > Bash sandbox.{RESET}");
            println!(
                "{DIM}  terminal: use /sandbox info; sandbox policy is fixed for the active REPL and changes require restart.{RESET}"
            );
        }
        ConfigSettingsTarget::Advanced => {
            println!("{DIM}  desktop: /settings advanced jumps to Settings > Advanced.{RESET}");
            println!(
                "{DIM}  terminal: shared advanced config lives in {}; use /config set or /config unset for supported keys.{RESET}",
                path.display()
            );
            println!(
                "{DIM}  supported REPL keys include code_turn_notifications, code_auto_compaction_enabled, smart_approval_enabled, and smart_approval_model.{RESET}"
            );
        }
    }
    println!();
    Ok(())
}

fn set_repl_config_value(cfg: &mut Arc<LibertaiConfig>, key: &str, value: &str) -> Result<()> {
    let mut next = cfg.as_ref().clone();
    match key {
        "code_turn_notifications" => {
            next.code_turn_notifications = value.parse::<bool>().with_context(|| {
                format!("code_turn_notifications must be true or false, got {value}")
            })?;
        }
        "code_auto_compaction_enabled" => {
            next.code_auto_compaction_enabled = value.parse::<bool>().with_context(|| {
                format!("code_auto_compaction_enabled must be true or false, got {value}")
            })?;
        }
        "smart_approval_enabled" => {
            next.smart_approval_enabled = value.parse::<bool>().with_context(|| {
                format!("smart_approval_enabled must be true or false, got {value}")
            })?;
        }
        "smart_approval_model" => {
            if value.trim().is_empty() {
                anyhow::bail!("smart_approval_model must not be empty");
            }
            next.smart_approval_model = value.to_string();
        }
        _ => anyhow::bail!(
            "unsupported REPL config key `{key}`; use `libertai config set {key} <value>` outside the REPL"
        ),
    }
    crate::config::save(&next).context("save config")?;
    *cfg = Arc::new(next);
    Ok(())
}

fn unset_repl_config_value(cfg: &mut Arc<LibertaiConfig>, key: &str) -> Result<()> {
    let mut next = cfg.as_ref().clone();
    match key {
        "code_turn_notifications" => {
            next.code_turn_notifications = crate::config::DEFAULT_CODE_TURN_NOTIFICATIONS;
        }
        "code_auto_compaction_enabled" => {
            next.code_auto_compaction_enabled =
                crate::config::DEFAULT_CODE_AUTO_COMPACTION_ENABLED;
        }
        "smart_approval_enabled" => {
            next.smart_approval_enabled = crate::config::DEFAULT_SMART_APPROVAL_ENABLED;
        }
        "smart_approval_model" => {
            next.smart_approval_model = crate::config::DEFAULT_SMART_APPROVAL_MODEL.to_string();
        }
        _ => anyhow::bail!(
            "unsupported REPL config key `{key}`; use `libertai config unset {key}` outside the REPL"
        ),
    }
    crate::config::save(&next).context("save config")?;
    *cfg = Arc::new(next);
    Ok(())
}

fn print_hooks_command(cfg: &LibertaiConfig, command: HooksCommand) {
    match command {
        HooksCommand::Status => print_hooks_status(cfg),
        HooksCommand::Open => print_hooks_open_hint(),
        HooksCommand::Usage => {
            println!("{BOLD}hooks{RESET}");
            println!("{DIM}  usage:{RESET} /hooks, /hooks status, /hooks open, or /hooks edit");
            println!();
        }
    }
}

fn print_hooks_status(cfg: &LibertaiConfig) {
    println!("{BOLD}hooks{RESET}");
    print_hook_section("UserPromptSubmit", &cfg.hooks.user_prompt_submit);
    print_hook_section("PreToolUse", &cfg.hooks.pre_tool_use);
    print_hook_section("PostToolUse", &cfg.hooks.post_tool_use);
    print_hook_section("SubagentStop", &cfg.hooks.subagent_stop);
    print_hook_section("SessionStart", &cfg.hooks.session_start);
    print_hook_section("Stop", &cfg.hooks.stop);
    print_hook_section("SessionEnd", &cfg.hooks.session_end);
    print_hook_section("Notification", &cfg.hooks.notification);
    println!(
        "{DIM}  UserPromptSubmit hooks run before the prompt reaches the agent and may block it.{RESET}"
    );
    println!(
        "{DIM}  PreToolUse hooks may return permissionDecision allow|ask|defer|deny.{RESET}"
    );
    println!(
        "{DIM}  PostToolUse hooks run after tool execution and cannot alter the result.{RESET}"
    );
    println!("{DIM}  SubagentStop hooks run after task-tool subagents finish.{RESET}");
    println!("{DIM}  Notification hooks run after agent-requested push notifications.{RESET}");
    println!("{DIM}  lifecycle hooks warn on nonzero exit and do not block the session.{RESET}");
    println!("{DIM}  command, HTTP, MCP-tool, prompt, and agent hook handlers are executed natively.{RESET}");
    println!("{DIM}  usage:{RESET} /hooks, /hooks status, /hooks open, /hooks edit");
    println!();
}

fn print_hooks_open_hint() {
    println!("{BOLD}hooks{RESET}");
    println!("{DIM}  /hooks open:{RESET} open Desktop Settings > Hooks for graphical hook management.");
    println!(
        "{DIM}  terminal:{RESET} edit hook rows in the LibertAI config file; /hooks status shows the active rows."
    );
    println!();
}

fn print_mcp_status(command: McpCommand) {
    println!("{BOLD}mcp{RESET}");
    match command {
        McpCommand::Status => {
            println!("{DIM}  terminal registry:{RESET} stdio, Streamable HTTP, and legacy SSE mcpServers from config.toml are available to MCP-tool hooks, mcp_call, and cached named MCP tools");
            println!("{DIM}  native CLI tools:{RESET} generic mcp_call is registered when mcpServers exist; cached tools[] register as mcp__server__tool names, resources[] as mcp_read_resource, and prompts[] as mcp_get_prompt");
            match crate::config::load() {
                Ok(cfg) if cfg.mcp_servers.is_empty() => {
                    println!("{DIM}  configured servers:{RESET} 0");
                }
                Ok(cfg) => {
                    println!("{DIM}  configured servers:{RESET} {}", cfg.mcp_servers.len());
                }
                Err(e) => {
                    println!("{DIM}  configured servers:{RESET} config load failed: {e:#}");
                }
            }
            println!(
                "{DIM}  desktop:{RESET} Settings > MCP owns stdio/HTTP/SSE server discovery, probing, and richer cache management"
            );
            println!(
                "{DIM}  tools:{RESET} CLI executes generic mcp_call, cached named mcp__server__tool entries, mcp_read_resource, mcp_get_prompt, and MCP-tool hook handlers from mcpServers"
            );
            println!("{DIM}  usage:{RESET} /mcp, /mcp status, /mcp probe, /mcp probe --save, /mcp refresh, /mcp reset, /mcp open");
        }
        McpCommand::Probe => print_mcp_probe(),
        McpCommand::ProbeSave => print_mcp_probe_save(),
        McpCommand::Reset => {
            println!(
                "{DIM}  /mcp reset:{RESET} no terminal MCP sessions were reset; CLI MCP calls are short-lived today."
            );
            println!(
                "{DIM}  desktop:{RESET} use Desktop Settings > MCP or desktop /mcp reset to close live stdio/HTTP/SSE clients."
            );
        }
        McpCommand::Open => {
            println!(
                "{DIM}  /mcp open:{RESET} open Desktop Settings > MCP for live server management. The terminal CLI has no MCP settings pane."
            );
        }
        McpCommand::Usage => {
            println!("{DIM}  usage:{RESET} /mcp, /mcp status, /mcp probe, /mcp probe --save, /mcp refresh, /mcp reset, /mcp open, or /mcp edit");
        }
    }
    println!();
}

fn print_mcp_probe() {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{DIM}  /mcp probe: config load failed: {e:#}{RESET}");
            return;
        }
    };
    if cfg.mcp_servers.is_empty() {
        println!("{DIM}  no configured mcpServers in terminal config.{RESET}");
        return;
    }
    println!("{DIM}  probing configured mcpServers...{RESET}");
    let report = crate::commands::code_mcp::probe_configured_servers(&cfg, Duration::from_secs(5));
    for server in report.servers {
        println!(
            "- {} [{}] {} · tools={} resources={} prompts={}",
            server.name,
            server.status.label(),
            server.transport,
            server.tools.len(),
            server.resources.len(),
            server.prompts.len()
        );
        print_mcp_probe_inventory("tools", &server.tools);
        print_mcp_probe_inventory("resources", &server.resources);
        print_mcp_probe_inventory("prompts", &server.prompts);
        for diagnostic in server.diagnostics {
            println!("{DIM}  warning: {diagnostic}{RESET}");
        }
    }
    println!(
        "{DIM}  note:{RESET} /mcp probe is terminal discovery only; generic mcp_call plus cached tool/resource/prompt entries can call configured servers."
    );
}

fn print_mcp_probe_save() {
    let mut cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{DIM}  /mcp probe --save: config load failed: {e:#}{RESET}");
            return;
        }
    };
    if cfg.mcp_servers.is_empty() {
        println!("{DIM}  no configured mcpServers in terminal config.{RESET}");
        return;
    }
    println!("{DIM}  probing configured mcpServers and updating cached inventory...{RESET}");
    let report = crate::commands::code_mcp::probe_configured_servers(&cfg, Duration::from_secs(5));
    let changed = merge_mcp_probe_report_into_config(&mut cfg, &report);
    for server in &report.servers {
        println!(
            "- {} [{}] {} · tools={} resources={} prompts={}",
            server.name,
            server.status.label(),
            server.transport,
            server.tools.len(),
            server.resources.len(),
            server.prompts.len()
        );
        print_mcp_probe_inventory("tools", &server.tools);
        print_mcp_probe_inventory("resources", &server.resources);
        print_mcp_probe_inventory("prompts", &server.prompts);
        for diagnostic in &server.diagnostics {
            println!("{DIM}  warning: {diagnostic}{RESET}");
        }
    }
    if let Err(e) = crate::config::save(&cfg) {
        eprintln!("{DIM}  /mcp probe --save: config save failed: {e:#}{RESET}");
        return;
    }
    println!(
        "{DIM}  saved:{RESET} refreshed cached MCP inventory for {changed} configured server(s). New code sessions will expose updated cached MCP tools/resources/prompts."
    );
}

fn merge_mcp_probe_report_into_config(
    cfg: &mut LibertaiConfig,
    report: &crate::commands::code_mcp::McpProbeReport,
) -> usize {
    let mut changed = 0usize;
    for probed in &report.servers {
        if probed.status == crate::commands::code_mcp::McpProbeStatus::Error {
            continue;
        }
        let Some(server) = cfg.mcp_servers.get_mut(&probed.name) else {
            continue;
        };
        let next_tools = probed
            .tools
            .iter()
            .filter(|name| !name.trim().is_empty())
            .map(|name| {
                server
                    .tools
                    .iter()
                    .find(|tool| tool.name == *name)
                    .cloned()
                    .unwrap_or_else(|| crate::config::McpToolConfig {
                        name: name.clone(),
                        ..crate::config::McpToolConfig::default()
                    })
            })
            .collect::<Vec<_>>();
        let next_resources = probed
            .resources
            .iter()
            .filter(|uri| !uri.trim().is_empty())
            .map(|uri| {
                server
                    .resources
                    .iter()
                    .find(|resource| resource.uri == *uri)
                    .cloned()
                    .unwrap_or_else(|| crate::config::McpResourceConfig {
                        uri: uri.clone(),
                        ..crate::config::McpResourceConfig::default()
                    })
            })
            .collect::<Vec<_>>();
        let next_prompts = probed
            .prompts
            .iter()
            .filter(|name| !name.trim().is_empty())
            .map(|name| {
                server
                    .prompts
                    .iter()
                    .find(|prompt| prompt.name == *name)
                    .cloned()
                    .unwrap_or_else(|| crate::config::McpPromptConfig {
                        name: name.clone(),
                        ..crate::config::McpPromptConfig::default()
                    })
            })
            .collect::<Vec<_>>();
        if server.tools != next_tools
            || server.resources != next_resources
            || server.prompts != next_prompts
        {
            server.tools = next_tools;
            server.resources = next_resources;
            server.prompts = next_prompts;
            changed += 1;
        }
    }
    changed
}

fn print_mcp_probe_inventory(label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let shown = items.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
    let suffix = if items.len() > 6 {
        format!(" +{} more", items.len() - 6)
    } else {
        String::new()
    };
    println!("{DIM}  {label}: {shown}{suffix}{RESET}");
}

fn count_runnable_hooks(hooks: &[crate::config::HookCommandConfig]) -> usize {
    hooks
        .iter()
        .filter(|hook| {
            let hook_type = normalized_hook_type(&hook.hook_type);
            hook.enabled
                && if hook_type == "http" {
                    !hook.url.trim().is_empty()
                } else if hook_type == "prompt" || hook_type == "agent" {
                    !hook.prompt.trim().is_empty()
                } else if hook_type == "mcp_tool" {
                    !hook.server.trim().is_empty() && !hook.tool.trim().is_empty()
                } else {
                    (hook_type.is_empty() || hook_type == "command")
                        && !hook.command.trim().is_empty()
                }
        })
        .count()
}

fn normalized_hook_type(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "mcp-tool" | "mcptool" => "mcp_tool".to_string(),
        other => other.to_string(),
    }
}

fn print_hook_section(event: &str, hooks: &[crate::config::HookCommandConfig]) {
    if hooks.is_empty() {
        println!("{DIM}  no {event} hooks configured{RESET}");
        return;
    }
    for (idx, hook) in hooks.iter().enumerate() {
        let marker = if hook.enabled { "on" } else { "off" };
        let matcher = if hook.matcher.trim().is_empty() {
            "*"
        } else {
            hook.matcher.trim()
        };
        let timeout = hook
            .timeout
            .map(|secs| format!(", timeout={secs}s"))
            .unwrap_or_default();
        let hook_type_key = normalized_hook_type(&hook.hook_type);
        let shell = if hook.shell.trim().is_empty()
            || hook_type_key == "http"
            || hook_type_key == "prompt"
            || hook_type_key == "agent"
            || hook_type_key == "mcp_tool"
        {
            String::new()
        } else {
            format!(", shell={}", hook.shell.trim())
        };
        let async_flag = if hook.async_hook { ", async" } else { "" };
        let once_flag = if hook.once { ", once" } else { "" };
        let async_rewake = if hook.async_rewake {
            ", asyncRewake"
        } else {
            ""
        };
        let source = if hook.source.trim().is_empty() {
            String::new()
        } else {
            format!(", source={}", hook.source.trim())
        };
        let status_message = if hook.status_message.trim().is_empty() {
            String::new()
        } else {
            format!(", statusMessage={}", hook.status_message.trim())
        };
        let metadata = hook_extra_metadata_label(hook);
        let if_condition = if hook.if_condition.trim().is_empty() {
            String::new()
        } else {
            format!(", if={}", hook.if_condition.trim())
        };
        let continue_on_block = if hook.continue_on_block {
            ", continueOnBlock"
        } else {
            ""
        };
        let hook_type = if hook.hook_type.trim().is_empty() {
            "command"
        } else {
            hook_type_key.as_str()
        };
        let target = if hook_type_key == "http" {
            if hook.url.trim().is_empty() {
                "(no url)".to_string()
            } else {
                hook.url.trim().to_string()
            }
        } else if hook_type_key == "prompt" || hook_type_key == "agent" {
            if hook.prompt.trim().is_empty() {
                "(no prompt)".to_string()
            } else {
                hook.prompt.trim().to_string()
            }
        } else if hook_type_key == "mcp_tool" {
            if hook.server.trim().is_empty() || hook.tool.trim().is_empty() {
                "(no mcp tool)".to_string()
            } else {
                format!("{}:{}", hook.server.trim(), hook.tool.trim())
            }
        } else if hook.command.trim().is_empty() {
            "(no command)".to_string()
        } else {
            crate::commands::code_hooks::hook_command_display(hook)
        };
        println!(
            "{DIM}  {}. {} [{}] type={} matcher={}{}{}{}{}{}{}{}{}{}{}:{RESET} {}",
            idx + 1,
            event,
            marker,
            hook_type,
            matcher,
            timeout,
            shell,
            async_flag,
            once_flag,
            async_rewake,
            source,
            status_message,
            metadata,
            if_condition,
            continue_on_block,
            target
        );
    }
}

fn hook_extra_metadata_label(hook: &crate::config::HookCommandConfig) -> String {
    if hook.extra.is_empty() {
        return String::new();
    }
    let keys = hook
        .extra
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("|");
    format!(", metadata={keys}")
}

fn print_status_line_status(cfg: &LibertaiConfig) {
    println!("{BOLD}statusline{RESET}");
    println!(
        "{DIM}  template:{RESET} {}",
        if cfg.status_line_template.trim().is_empty() {
            "(default)"
        } else {
            cfg.status_line_template.as_str()
        }
    );
    println!(
        "{DIM}  command:{RESET} {}",
        if cfg.status_line_command.trim().is_empty() {
            "(none)"
        } else {
            cfg.status_line_command.as_str()
        }
    );
    println!("{DIM}  {}{RESET}", status_line_help());
    println!(
        "{DIM}  usage:{RESET} {}",
        concat!(
            "/statusline <template>, /statusline command <shell>, ",
            "/statusline command-clear, /statusline reset, /statusline status"
        )
    );
    println!();
}

fn handle_status_line_command(raw: &str, cfg: &mut Arc<LibertaiConfig>) -> Result<()> {
    let action = raw.trim();
    if action.is_empty()
        || action.eq_ignore_ascii_case("status")
        || action.eq_ignore_ascii_case("show")
    {
        print_status_line_status(cfg);
        return Ok(());
    }

    let mut next = cfg.as_ref().clone();
    if action.eq_ignore_ascii_case("command-clear")
        || action.eq_ignore_ascii_case("command clear")
        || action.eq_ignore_ascii_case("command reset")
    {
        next.status_line_command.clear();
    } else if let Some(command) = action.strip_prefix("command ") {
        next.status_line_command = normalize_status_line_command(command);
    } else if action.eq_ignore_ascii_case("reset") || action.eq_ignore_ascii_case("clear") {
        next.status_line_template.clear();
        next.status_line_command.clear();
    } else {
        next.status_line_template = normalize_status_line_template(action);
    }
    crate::config::save(&next).context("save config")?;
    *cfg = Arc::new(next);
    let template = cfg.status_line_template.clone();
    let command = cfg.status_line_command.clone();
    update_bar_status(|status| {
        status.status_line_template = template.clone();
        status.status_line_command = command.clone();
    });
    clear_status_line_command_cache();
    if !cfg.status_line_command.trim().is_empty() {
        println!(
            "{DIM}  status command updated: {}{RESET}",
            cfg.status_line_command
        );
    } else if cfg.status_line_template.trim().is_empty() {
        println!("{DIM}  status line reset to the default.{RESET}");
    } else {
        println!(
            "{DIM}  status line updated: {}{RESET}",
            cfg.status_line_template
        );
    }
    Ok(())
}

fn handle_output_style(raw: &str, output_style: &mut Option<String>) {
    let value = raw.trim();
    let key = if value.is_empty() {
        "status"
    } else {
        value
    };
    if key.eq_ignore_ascii_case("status") || key.eq_ignore_ascii_case("list") {
        print_output_style_status(output_style.as_deref(), None);
        return;
    }
    let cwd = std::env::current_dir().ok();
    let Some(style) = crate::commands::code_output_style::find_style(key, cwd.as_deref()) else {
        print_output_style_status(output_style.as_deref(), Some(key));
        return;
    };
    *output_style = if style.name == "default" {
        None
    } else {
        Some(style.name)
    };
    print_output_style_status(output_style.as_deref(), None);
}

fn print_output_style_status(output_style: Option<&str>, unknown: Option<&str>) {
    let cwd = std::env::current_dir().ok();
    let styles = crate::commands::code_output_style::load_styles(cwd.as_deref());
    println!("{BOLD}output-style{RESET}");
    println!("{DIM}  current:{RESET} {}", output_style.unwrap_or("default"));
    println!("{DIM}  available:{RESET}");
    for style in styles {
        println!("{DIM}    {:<12}{RESET} {}", style.name, style.description);
    }
    if let Some(name) = unknown {
        println!("{DIM}  unknown output style: {name}{RESET}");
    }
    println!();
}

fn apply_output_style(output_style: Option<&str>, prompt: &str) -> String {
    let cwd = std::env::current_dir().ok();
    crate::commands::code_output_style::apply_output_style(output_style, prompt, cwd.as_deref())
}

fn print_bug_template(provider: &str, model: &str, mode: Mode, output_style: Option<&str>) {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    println!("{BOLD}bug report{RESET}");
    println!("Include this diagnostic block with the issue:");
    println!();
    println!("- app: libertai-cli");
    println!("- branch: integrated-code");
    println!("- provider: {provider}");
    println!("- model: {model}");
    println!("- mode: {}", mode_label(mode));
    println!("- output-style: {}", output_style.unwrap_or("default"));
    println!("- cwd: {cwd}");
    println!();
    println!("Describe:");
    println!("- What you expected");
    println!("- What happened");
    println!("- The last command or prompt you ran");
    println!("- Whether it reproduces in a fresh `libertai code` session");
    println!();
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::AcceptEdits => "accept-edits",
        Mode::Plan => "plan",
    }
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
        // The todo tool renders its own nicely-formatted output;
        // adding "[tool] todo" above it is just noise.
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } if tool_name != "todo" => {
            let preview = crate::commands::code_tool_preview::tool_preview(&tool_name, &args);
            println!("\n{DIM}  [tool] {preview}{RESET}");
        }
        AgentEvent::AutoCompactionStart { reason } => {
            println!("{DIM}  [compact] {reason}{RESET}");
        }
        AgentEvent::AutoCompactionEnd {
            aborted,
            error_message,
            ..
        } => {
            if aborted {
                println!("{DIM}  [compact] aborted{RESET}");
            } else if let Some(message) = error_message {
                println!("{DIM}  [compact] failed: {message}{RESET}");
            } else {
                println!("{DIM}  [compact] finished{RESET}");
            }
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
                // Don't try to MoveToPreviousLine onto a row that may
                // no longer exist after a shrink — treat as a fresh
                // paint. The previous bar position is left as-is in
                // scrollback rather than risk drifting into row 0.
                painted = false;
                repaint(&mut stdout, &buffer, cursor_pos, mode, painted)?;
                painted = true;
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
    let rule: String = rule_chip(cols, mode);

    // Mode chip printed in-line with the prompt, left of ❯. Dimmed so
    // it's a status cue, not a shout.
    let (chip_text, chip_colour) = match mode {
        Mode::Normal => ("", Color::DarkGrey),
        Mode::AcceptEdits => ("[accept-edits] ", Color::Cyan),
        Mode::Plan => ("[plan] ", Color::Yellow),
    };

    // Prefix width: mode chip + `❯ ` (2 cells). Anything past
    // `cols - prefix_cols - 1` characters of the buffer would wrap the
    // terminal onto a third row — which breaks `clear_bar`'s two-line
    // erase assumption. Slide a window over the buffer so the cursor is
    // always visible and the line never wraps.
    let prefix_cols_usize = chip_text.chars().count() + 2;
    let avail = cols.saturating_sub(prefix_cols_usize).max(1);
    let (display_text, display_cursor) = slide_window(buffer, cursor_pos, avail);

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
        Print(&display_text),
    )?;

    let prefix_cols = u16::try_from(prefix_cols_usize).unwrap_or(u16::MAX);
    let cursor_cell = u16::try_from(display_cursor).unwrap_or(u16::MAX);
    let col = prefix_cols.saturating_add(cursor_cell);
    queue!(stdout, cursor::MoveToColumn(col))?;

    stdout.flush()?;
    Ok(())
}

/// Slide a visible window of `avail` cells over `buffer`. When the
/// buffer fits, the whole thing is returned unchanged. Otherwise the
/// window is anchored so the cursor is visible inside it; when clipped
/// at either end a `…` marker replaces the off-screen cell. Returns
/// `(display_text, cursor_column_within_display)`.
fn slide_window(buffer: &[char], cursor_pos: usize, avail: usize) -> (String, usize) {
    if buffer.len() <= avail {
        return (buffer.iter().collect(), cursor_pos);
    }
    // Keep the cursor at least one cell from each edge so there's
    // always room for a `…` indicator.
    let start = cursor_pos.saturating_sub(avail.saturating_sub(1));
    let end = (start + avail).min(buffer.len());
    let mut out = String::with_capacity(avail);
    if start > 0 {
        out.push('\u{2026}');
        out.extend(buffer[start + 1..end].iter());
    } else {
        out.extend(buffer[start..end].iter());
    }
    if end < buffer.len() {
        // Replace the last visible cell with `…`.
        out.pop();
        out.push('\u{2026}');
    }
    (out, cursor_pos - start)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_style_lookup_is_case_insensitive() {
        assert_eq!(
            crate::commands::code_output_style::find_style("REVIEW", None).map(|style| style.name),
            Some("review".to_string())
        );
        assert!(crate::commands::code_output_style::find_style("missing", None).is_none());
    }

    #[test]
    fn apply_output_style_leaves_default_prompt_unchanged() {
        assert_eq!(apply_output_style(None, "hello"), "hello");
    }

    #[test]
    fn apply_output_style_appends_session_instruction() {
        let prompt = apply_output_style(Some("concise"), "hello");
        assert!(prompt.starts_with("hello\n\n[Session output style: concise."));
        assert!(prompt.contains("Be concise."));
    }

    #[test]
    fn parse_init_agent_notes_accepts_agent_markers() {
        assert_eq!(parse_init_agent_notes("--agent"), Some(None));
        assert_eq!(
            parse_init_agent_notes("--agent prefer pnpm"),
            Some(Some("prefer pnpm"))
        );
        assert_eq!(
            parse_init_agent_notes("model keep CONTRIBUTING guidance"),
            Some(Some("keep CONTRIBUTING guidance"))
        );
        assert_eq!(parse_init_agent_notes("project notes"), None);
    }

    #[test]
    fn parse_init_from_agent_accepts_preview_and_apply_modes() {
        assert_eq!(
            parse_init_from_agent_action("from-agent"),
            Some(InitFromAgentAction::Preview)
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent append"),
            Some(InitFromAgentAction::Append)
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent merge"),
            Some(InitFromAgentAction::Merge)
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent merge-lines"),
            Some(InitFromAgentAction::MergeLines)
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview merge-lines"),
            Some(InitFromAgentAction::PreviewMergeLines)
        );
        assert_eq!(
            parse_init_from_agent_action("apply-agent replace"),
            Some(InitFromAgentAction::Replace)
        );
        assert_eq!(parse_init_from_agent_action("from-agent nope"), None);
    }

    #[test]
    fn build_init_apply_content_appends_or_replaces_candidate() {
        assert_eq!(
            build_init_apply_content("custom guidance\n", "# Demo", "append").unwrap(),
            "custom guidance\n\n## Generated /init candidate\n\n# Demo\n"
        );
        assert_eq!(
            build_init_apply_content("custom guidance\n", "# Demo", "replace").unwrap(),
            "# Demo\n"
        );
    }

    #[test]
    fn build_init_apply_content_merges_matching_sections() {
        let existing = "# Demo\n\n## Build & test\n- test: cargo test\n\n## Conventions\n- keep scoped\n";
        let candidate = "# Candidate\n\n## Build & test\n- test: cargo nextest run\n\n## Structure\n- src/ - code\n";
        let merged = build_init_apply_content(existing, candidate, "merge").unwrap();
        assert!(merged.starts_with("# Demo\n\n## Build & test\n- test: cargo nextest run"));
        assert!(merged.contains("## Conventions\n- keep scoped"));
        assert!(merged.contains("## Generated /init candidate\n\n# Candidate"));
        assert!(merged.contains("## Structure\n- src/ - code"));
    }

    #[test]
    fn build_init_apply_content_line_merges_matching_sections() {
        let existing = "# Demo\n\n## Build & test\n- test: cargo test\n\n## Conventions\n- keep scoped\n";
        let candidate =
            "# Candidate\n\n## Build & test\n- test: cargo test\n- lint: cargo clippy\n\n## Conventions\n- keep scoped\n- prefer small diffs\n";
        let merged = build_init_apply_content(existing, candidate, "merge-lines").unwrap();
        assert!(
            merged.starts_with("# Demo\n\n## Build & test\n- test: cargo test\n- lint: cargo clippy")
        );
        assert!(merged.contains("## Conventions\n- keep scoped\n- prefer small diffs"));
        assert!(!merged.contains("# Candidate"));
    }

    #[test]
    fn init_candidate_preview_marks_candidate_as_not_written() {
        let preview = init_candidate_preview(
            "AGENTS.md",
            "custom guidance\n",
            "# demo\n\n## Build & test\n- test: cargo test\n",
        );
        assert!(preview.contains("generated merge candidate (not written)"));
        assert!(preview.contains("- test: cargo test"));
        assert!(preview.contains("diff against existing AGENTS.md"));
        assert!(preview.contains("--- AGENTS.md"));
        assert!(preview.contains("-custom guidance"));
        assert!(preview.contains("+## Build & test"));
        assert!(preview.contains("candidate sections:"));
        assert!(preview.contains("1. Preamble"));
        assert!(preview.contains("2. Build & test"));
        assert!(preview.contains("merge only verified repo facts"));
    }

    #[test]
    fn shell_escape_argv_uses_plain_shell_without_wrapper() {
        assert_eq!(shell_escape_argv(None), vec!["/bin/sh".to_string()]);
    }

    #[test]
    fn shell_escape_argv_appends_shell_to_wrapper() {
        let wrapper = vec!["/usr/bin/env".to_string(), "FOO=bar".to_string()];
        assert_eq!(
            shell_escape_argv(Some(&wrapper)),
            vec![
                "/usr/bin/env".to_string(),
                "FOO=bar".to_string(),
                "/bin/sh".to_string()
            ]
        );
    }

    #[test]
    fn shell_escape_command_repeats_previous_command() {
        assert_eq!(
            shell_escape_command("!", Some("git status --short")),
            ShellEscapeAction::Run("git status --short".to_string())
        );
    }

    #[test]
    fn shell_escape_command_requires_previous_command_for_repeat() {
        assert_eq!(
            shell_escape_command("!", None),
            ShellEscapeAction::Usage("no previous shell command to repeat")
        );
    }

    #[test]
    fn shell_escape_command_reports_usage_for_empty_command() {
        match shell_escape_command("  ", Some("pwd")) {
            ShellEscapeAction::Usage(message) => {
                assert!(message.contains("!! repeats the last shell command"));
            }
            action => panic!("expected usage, got {action:?}"),
        }
    }

    #[test]
    fn shell_escape_executes_in_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let result = execute_shell_escape(temp.path(), "pwd", None).unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), temp.path().display().to_string());
    }

    #[test]
    fn usage_summary_tracks_context_high_water_and_output_total() {
        let records = vec![
            UsageRecord {
                provider: "libertai".to_string(),
                model: "fast".to_string(),
                input: 100,
                output: 25,
                context_window: 1_000,
            },
            UsageRecord {
                provider: "libertai".to_string(),
                model: "fast".to_string(),
                input: 180,
                output: 40,
                context_window: 1_000,
            },
        ];
        let summary = usage_summary(&records).unwrap();
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.last_input, 180);
        assert_eq!(summary.last_output, 40);
        assert_eq!(summary.output_total, 65);
        assert_eq!(summary.context_high_water, 180);
        assert_eq!(summary.context_window, 1_000);
    }

    #[test]
    fn usage_summary_empty_when_no_turns() {
        assert!(usage_summary(&[]).is_none());
    }

    #[test]
    fn memory_import_source_parses_path_argument() {
        assert_eq!(memory_import_source("import CLAUDE.md"), Some("CLAUDE.md"));
        assert_eq!(
            memory_import_source("IMPORT docs/project notes.md"),
            Some("docs/project notes.md")
        );
        assert_eq!(memory_import_source("files"), None);
        assert_eq!(memory_import_source("import"), None);
    }

    #[test]
    fn memory_edit_action_accepts_open_aliases() {
        assert!(is_memory_edit_action("open"));
        assert!(is_memory_edit_action("edit"));
        assert!(is_memory_edit_action("editor"));
        assert!(is_memory_edit_action(" OPEN "));
        assert!(!is_memory_edit_action(""));
        assert!(!is_memory_edit_action("path"));
    }

    #[test]
    fn tool_activity_tracker_counts_completed_tool_calls() {
        let mut tracker = ToolActivityTracker::default();
        tracker.observe(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/a"}),
        });
        tracker.observe(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            result: empty_tool_output(),
            is_error: false,
        });
        tracker.observe(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-2".to_string(),
            tool_name: "bash".to_string(),
            result: empty_tool_output(),
            is_error: true,
        });

        let summary = tracker.summary();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].tool_name, "bash");
        assert_eq!(summary[0].count, 1);
        assert_eq!(summary[1].tool_name, "read");
        assert_eq!(summary[1].count, 1);
    }

    #[test]
    fn estimate_tool_attribution_weights_by_duration() {
        let usage = UsageSummary {
            turns: 2,
            last_input: 600,
            last_output: 100,
            output_total: 400,
            context_high_water: 600,
            context_window: 10_000,
            provider: "libertai".to_string(),
            model: "qwen3-coder-480b".to_string(),
        };
        let tools = vec![
            ToolActivitySummary {
                tool_name: "bash".to_string(),
                count: 1,
                total_duration: Duration::from_millis(300),
            },
            ToolActivitySummary {
                tool_name: "read".to_string(),
                count: 1,
                total_duration: Duration::from_millis(100),
            },
        ];
        let rows = estimate_tool_attribution(&usage, &tools);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tool_name, "bash");
        assert_eq!(rows[0].estimated_tokens, 750);
        assert_eq!(rows[1].estimated_tokens, 250);
        let total_cost: f64 = rows.iter().filter_map(|row| row.estimated_cost).sum();
        let expected = model_token_cost("qwen3-coder-480b", 600, 400).unwrap();
        assert!((total_cost - expected).abs() < 0.000_001);
    }

    #[test]
    fn estimate_tool_attribution_omits_cost_for_unknown_model() {
        let usage = UsageSummary {
            turns: 1,
            last_input: 10,
            last_output: 5,
            output_total: 5,
            context_high_water: 10,
            context_window: 1_000,
            provider: "custom".to_string(),
            model: "local-model".to_string(),
        };
        let rows = estimate_tool_attribution(
            &usage,
            &[ToolActivitySummary {
                tool_name: "read".to_string(),
                count: 1,
                total_duration: Duration::ZERO,
            }],
        );
        assert_eq!(rows[0].estimated_tokens, 15);
        assert_eq!(rows[0].estimated_cost, None);
    }

    #[test]
    fn model_token_rates_cover_current_libertai_defaults() {
        assert_eq!(
            model_token_rates(crate::config::DEFAULT_CODE_MODEL),
            Some((0.15, 1.00))
        );
        assert_eq!(
            model_token_rates(crate::config::DEFAULT_CHAT_MODEL),
            Some((0.40, 2.00))
        );
        assert_eq!(model_token_rates("qwen3-coder-480b"), Some((1.00, 3.00)));
        assert_eq!(model_token_rates("qwen3-coder"), Some((0.22, 0.95)));
        assert_eq!(model_token_rates("local-model"), None);
    }

    #[test]
    fn parse_usage_export_accepts_cost_and_usage_aliases() {
        assert_eq!(
            parse_usage_export_command("/usage export"),
            Some(UsageExportFormat::Json)
        );
        assert_eq!(
            parse_usage_export_command("/cost export csv"),
            Some(UsageExportFormat::Csv)
        );
        assert_eq!(parse_usage_export_command("/cost export xml"), None);
    }

    #[test]
    fn usage_export_json_includes_provenance_and_tool_estimates() {
        let usage = UsageSummary {
            turns: 2,
            last_input: 600,
            last_output: 200,
            output_total: 300,
            context_high_water: 600,
            context_window: 32768,
            provider: "libertai".to_string(),
            model: "qwen3-coder-480b".to_string(),
        };
        let report = usage_export_json(
            Some(&usage),
            &[ToolActivitySummary {
                tool_name: "bash".to_string(),
                count: 1,
                total_duration: Duration::from_millis(25),
            }],
        );
        assert!(report.contains("\"kind\": \"libertai_code_usage_export\""));
        assert!(report.contains("\"toolName\": \"bash\""));
        assert!(report.contains("provider-measured per-tool billing is not available"));
    }

    #[test]
    fn usage_export_csv_quotes_cells_and_labels_estimates() {
        let usage = UsageSummary {
            turns: 1,
            last_input: 10,
            last_output: 5,
            output_total: 5,
            context_high_water: 10,
            context_window: 100,
            provider: "local,dev".to_string(),
            model: "unknown".to_string(),
        };
        let report = usage_export_csv(
            Some(&usage),
            &[ToolActivitySummary {
                tool_name: "read".to_string(),
                count: 2,
                total_duration: Duration::ZERO,
            }],
        );
        assert!(report.starts_with("category,name,count"));
        assert!(report.contains("\"local,dev/unknown\""));
        assert!(report.contains("estimated duration-weighted attribution"));
    }

    #[test]
    fn format_duration_scales_units() {
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(1_500)), "1.5s");
        assert_eq!(format_duration(Duration::from_millis(61_000)), "1m 1s");
    }

    #[test]
    fn status_line_template_normalizes_and_expands_known_tokens() {
        let status = BarStatus {
            model_label: "libertai/qwen".to_string(),
            input_tokens: 2048,
            context_window: 4096,
            output_style: Some("review".to_string()),
            status_line_template: "{backend}/{model} {mode} {style} {tokens} {ctx} {unknown}"
                .to_string(),
            status_line_command: String::new(),
        };
        let expanded =
            expand_status_line_template(&status.status_line_template, &status, Mode::Plan)
                .unwrap();
        assert_eq!(expanded, "libertai/qwen plan review 2.0k 50% {unknown}");
    }

    fn empty_tool_output() -> pi::sdk::ToolOutput {
        pi::sdk::ToolOutput {
            content: Vec::new(),
            details: None,
            is_error: false,
        }
    }

    #[test]
    fn status_line_template_reset_falls_back_to_default_rule_text() {
        let status = BarStatus {
            model_label: "libertai/qwen".to_string(),
            input_tokens: 512,
            context_window: 1024,
            output_style: None,
            status_line_template: String::new(),
            status_line_command: String::new(),
        };
        assert!(expand_status_line_template("", &status, Mode::Normal).is_none());
        assert_eq!(default_rule_text(&status), "50% · 512 / 1.0k · libertai/qwen");
    }

    #[test]
    fn status_line_command_output_uses_first_nonempty_line() {
        assert_eq!(
            first_status_line(" \n  dynamic branch  \nsecond"),
            "dynamic branch"
        );
    }

    #[test]
    fn status_line_command_normalizes_and_caps_input() {
        let command = format!("  {}  ", "x".repeat(STATUS_LINE_COMMAND_MAX_CHARS + 20));
        let normalized = normalize_status_line_command(&command);
        assert_eq!(normalized.chars().count(), STATUS_LINE_COMMAND_MAX_CHARS);
        assert!(normalized.chars().all(|c| c == 'x'));
    }

    #[test]
    fn status_line_command_runs_shell_and_reads_first_output_line() {
        let (value, error) = run_status_line_command("printf 'ready\\nsecond\\n'");
        assert_eq!(value, "ready");
        assert_eq!(error, "");
    }

    #[test]
    fn status_line_command_arg_accepts_dash_alias() {
        assert_eq!(
            status_line_command_arg("/statusline {model}").unwrap(),
            "{model}"
        );
        assert_eq!(
            status_line_command_arg("/status-line reset").unwrap(),
            "reset"
        );
        assert!(status_line_command_arg("/status").is_none());
    }

    #[test]
    fn parse_history_limit_defaults_and_clamps() {
        assert_eq!(parse_history_limit("").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("3").unwrap(), 3);
        assert_eq!(parse_history_limit("0").unwrap(), 1);
        assert_eq!(parse_history_limit("999").unwrap(), HISTORY_MAX_LIMIT);
        assert!(parse_history_limit("recent").is_err());
    }

    #[test]
    fn input_history_round_trips_and_keeps_recent_unique_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history").join("code-history.json");
        let mut history = VecDeque::new();
        for idx in 0..(HISTORY_MAX_LIMIT + 2) {
            history.push_back(format!("prompt {idx}"));
        }
        persist_input_history(&path, &history).unwrap();

        let loaded = load_input_history(&path).unwrap();
        assert_eq!(loaded.len(), HISTORY_MAX_LIMIT);
        assert_eq!(loaded.front().map(String::as_str), Some("prompt 2"));
        let expected_last = format!("prompt {}", HISTORY_MAX_LIMIT + 1);
        assert_eq!(
            loaded.back().map(String::as_str),
            Some(expected_last.as_str())
        );

        fs::write(&path, "[\" one \", \"one\", \"\", \"two\"]\n").unwrap();
        let loaded = load_input_history(&path).unwrap();
        assert_eq!(loaded.into_iter().collect::<Vec<_>>(), vec!["one", "two"]);
    }

    #[test]
    fn hotkey_lines_include_mode_history_and_interrupt_controls() {
        let joined = hotkey_lines().join("\n");
        assert!(joined.contains("Shift+Tab"));
        assert!(joined.contains("Up / Down"));
        assert!(joined.contains("Ctrl+C"));
        assert!(joined.contains("Ctrl+D"));
    }

    #[test]
    fn tree_skip_rules_cover_noisy_directories() {
        assert!(should_skip_tree_entry(".git"));
        assert!(should_skip_tree_entry("target"));
        assert!(should_skip_tree_entry("node_modules"));
        assert!(!should_skip_tree_entry("src"));
    }

    #[test]
    fn render_project_tree_sorts_dirs_first_and_skips_noise() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::create_dir(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join("README.md"), "readme").unwrap();
        std::fs::write(temp.path().join("src").join("main.rs"), "fn main() {}").unwrap();

        let rendered = render_project_tree(temp.path(), 20).unwrap();
        assert!(rendered.contains("src/"));
        assert!(rendered.contains("main.rs"));
        assert!(rendered.contains("README.md"));
        assert!(!rendered.contains("target/"));
        assert!(
            rendered.find("src/").unwrap() < rendered.find("README.md").unwrap(),
            "directories should be printed before files"
        );
    }

    #[test]
    fn parse_changelog_limit_defaults_and_clamps() {
        assert_eq!(parse_changelog_limit("").unwrap(), CHANGELOG_DEFAULT_LIMIT);
        assert_eq!(parse_changelog_limit("3").unwrap(), 3);
        assert_eq!(parse_changelog_limit("0").unwrap(), 1);
        assert_eq!(parse_changelog_limit("999").unwrap(), CHANGELOG_MAX_LIMIT);
        assert!(parse_changelog_limit("recent").is_err());
    }

    #[test]
    fn parse_sandbox_action_accepts_info_reload_and_unknown() {
        assert_eq!(parse_sandbox_action(""), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("info"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("STATUS"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("reload"), SandboxAction::Reload);
        assert_eq!(parse_sandbox_action("reset"), SandboxAction::Unknown("reset"));
    }

    #[test]
    fn abort_status_message_points_to_ctrl_c() {
        let message = abort_status_message();
        assert!(message.contains("no active turn"));
        assert!(message.contains("Ctrl+C"));
    }

    #[test]
    fn recent_git_commits_reads_repo_history() {
        let lines = recent_git_commits_in(Path::new(env!("CARGO_MANIFEST_DIR")), 1).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .split_whitespace()
                .next()
                .is_some_and(|hash| hash.len() >= 7)
        );
    }

    #[test]
    fn git_status_short_reads_repo_status() {
        let lines = git_status_short_in(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(lines.first().is_some_and(|line| line.starts_with("## ")));
    }

    #[test]
    fn doctor_line_formats_ok_and_warning_states() {
        let ok = doctor_line(true, "git status", "clean");
        assert!(ok.contains("[ok]"));
        assert!(ok.contains("git status: clean"));
        let warn = doctor_line(false, "auth", "");
        assert!(warn.contains("[warn]"));
        assert!(warn.ends_with("auth"));
    }

    #[test]
    fn doctor_hook_breakdown_counts_runnable_events() {
        let mut cfg = LibertaiConfig::default();
        cfg.hooks.user_prompt_submit.push(crate::config::HookCommandConfig {
            command: "scripts/prompt.sh".to_string(),
            ..Default::default()
        });
        cfg.hooks.pre_tool_use.push(crate::config::HookCommandConfig {
            command: "scripts/pre.sh".to_string(),
            ..Default::default()
        });
        cfg.hooks.pre_tool_use.push(crate::config::HookCommandConfig {
            hook_type: "http".to_string(),
            url: "http://127.0.0.1/hook".to_string(),
            ..Default::default()
        });
        cfg.hooks.post_tool_use.push(crate::config::HookCommandConfig {
            hook_type: "prompt".to_string(),
            prompt: "Summarize this hook.".to_string(),
            ..Default::default()
        });
        cfg.hooks.pre_tool_use.push(crate::config::HookCommandConfig {
            enabled: false,
            command: "scripts/pre-disabled.sh".to_string(),
            ..Default::default()
        });
        cfg.hooks.stop.push(crate::config::HookCommandConfig {
            command: "   ".to_string(),
            ..Default::default()
        });
        cfg.hooks.notification.push(crate::config::HookCommandConfig {
            command: "scripts/notify.sh".to_string(),
            ..Default::default()
        });

        let breakdown = format_hook_event_breakdown(&cfg);
        assert!(breakdown.contains("5 runnable hook(s)"));
        assert!(breakdown.contains("UserPromptSubmit 1"));
        assert!(breakdown.contains("PreToolUse 2"));
        assert!(breakdown.contains("PostToolUse 1"));
        assert!(breakdown.contains("SubagentStop 0"));
        assert!(breakdown.contains("Stop 0"));
        assert!(breakdown.contains("Notification 1"));
    }

    #[test]
    fn hook_type_normalization_accepts_claude_mcp_spellings() {
        assert_eq!(normalized_hook_type("mcp-tool"), "mcp_tool");
        assert_eq!(normalized_hook_type("mcptool"), "mcp_tool");
        assert_eq!(normalized_hook_type("MCP_TOOL"), "mcp_tool");
        assert_eq!(normalized_hook_type("Prompt"), "prompt");

        let hooks = vec![crate::config::HookCommandConfig {
            hook_type: "mcp-tool".to_string(),
            server: "policy".to_string(),
            tool: "check".to_string(),
            ..Default::default()
        }];
        assert_eq!(count_runnable_hooks(&hooks), 1);
    }

    #[test]
    fn hook_extra_metadata_label_lists_preserved_keys() {
        let hook = crate::config::HookCommandConfig {
            extra: std::collections::BTreeMap::from([
                ("customFlag".to_string(), serde_json::json!(true)),
                (
                    "metadata".to_string(),
                    serde_json::json!({"owner": "security"}),
                ),
            ]),
            ..Default::default()
        };

        assert_eq!(
            hook_extra_metadata_label(&hook),
            ", metadata=customFlag|metadata"
        );
        assert_eq!(
            hook_extra_metadata_label(&crate::config::HookCommandConfig::default()),
            ""
        );
    }

    #[test]
    fn doctor_summaries_count_local_registries() {
        let agents = vec![
            crate::commands::code_agents::AgentDefinition {
                name: "reviewer".to_string(),
                description: "Reviews changes".to_string(),
                tools: None,
                model: None,
                worktree: true,
                system_prompt: "Review carefully.".to_string(),
                source: crate::commands::code_agents::AgentSource::Project(PathBuf::from(
                    ".claude/agents",
                )),
            },
            crate::commands::code_agents::AgentDefinition {
                name: "tester".to_string(),
                description: "Runs tests".to_string(),
                tools: None,
                model: None,
                worktree: false,
                system_prompt: "Test carefully.".to_string(),
                source: crate::commands::code_agents::AgentSource::User(PathBuf::from(
                    "~/.claude/agents",
                )),
            },
        ];
        assert_eq!(
            format_agent_doctor_summary(&agents),
            "2 loaded (1 worktree default)"
        );

        let commands = vec![
            crate::commands::code_slash_registry::CustomCommand {
                name: "triage".to_string(),
                description: None,
                arg_hint: None,
                argument_names: Vec::new(),
                body: "Triage {{args}}".to_string(),
                source: crate::commands::code_slash_registry::CommandSource::Project,
                namespace: None,
                path: PathBuf::from(".claude/commands/triage.md"),
            },
            crate::commands::code_slash_registry::CustomCommand {
                name: "summarize".to_string(),
                description: None,
                arg_hint: None,
                argument_names: Vec::new(),
                body: "Summarize {{args}}".to_string(),
                source: crate::commands::code_slash_registry::CommandSource::User,
                namespace: None,
                path: PathBuf::from("~/.claude/commands/summarize.md"),
            },
        ];
        assert_eq!(
            format_custom_slash_doctor_summary(&commands),
            "2 loaded (1 project, 1 user)"
        );

        let skills = vec![
            crate::commands::code_skills::SkillInventoryEntry {
                name: "libertai-harness".to_string(),
                description: String::new(),
                allowed_tools: None,
                source: "builtin".to_string(),
                enabled: true,
            },
            crate::commands::code_skills::SkillInventoryEntry {
                name: "project-review".to_string(),
                description: String::new(),
                allowed_tools: None,
                source: "project".to_string(),
                enabled: false,
            },
        ];
        assert_eq!(format_skill_doctor_summary(&skills), "1/2 enabled");
    }

    #[test]
    fn doctor_memory_reference_summary_counts_statuses() {
        let refs = vec![
            crate::commands::code_memory::MemoryReference {
                line_number: 1,
                text: "docs".to_string(),
                target: Some("docs".to_string()),
                status: crate::commands::code_memory::MemoryReferenceStatus::Ok,
                detail: "docs".to_string(),
            },
            crate::commands::code_memory::MemoryReference {
                line_number: 2,
                text: "missing".to_string(),
                target: Some("missing".to_string()),
                status: crate::commands::code_memory::MemoryReferenceStatus::Missing,
                detail: "missing".to_string(),
            },
            crate::commands::code_memory::MemoryReference {
                line_number: 3,
                text: "https://example.com".to_string(),
                target: Some("https://example.com".to_string()),
                status: crate::commands::code_memory::MemoryReferenceStatus::External,
                detail: "external".to_string(),
            },
            crate::commands::code_memory::MemoryReference {
                line_number: 4,
                text: "unknown".to_string(),
                target: None,
                status: crate::commands::code_memory::MemoryReferenceStatus::Unparsed,
                detail: "unparsed".to_string(),
            },
        ];
        assert_eq!(
            format_memory_reference_summary(&refs),
            "4 total (ok 1, missing 1, external 1, unparsed 1)"
        );
    }

    #[test]
    fn reload_model_selection_tracks_changed_defaults_only_when_user_is_on_defaults() {
        let mut old_cfg = LibertaiConfig::default();
        old_cfg.default_code_provider = "libertai".to_string();
        old_cfg.default_code_model = "old-default".to_string();
        let mut next_cfg = old_cfg.clone();
        next_cfg.default_code_model = "new-default".to_string();

        assert_eq!(
            reload_model_selection(&old_cfg, &next_cfg, "libertai", "old-default"),
            ("libertai".to_string(), "new-default".to_string())
        );
        assert_eq!(
            reload_model_selection(&old_cfg, &next_cfg, "libertai", "custom-model"),
            ("libertai".to_string(), "custom-model".to_string())
        );
    }

    #[test]
    fn resolve_repl_resume_path_accepts_existing_explicit_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        assert_eq!(resolve_repl_resume_path(path.to_str().unwrap()).unwrap(), path);
    }

    #[test]
    fn resolve_repl_resume_path_rejects_missing_explicit_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing.jsonl");
        assert!(resolve_repl_resume_path(path.to_str().unwrap()).is_err());
    }

    fn fork_messages_for_tests() -> Vec<RpcForkMessage> {
        vec![
            RpcForkMessage {
                entry_id: "abc111".to_string(),
                text: "first prompt".to_string(),
            },
            RpcForkMessage {
                entry_id: "def222".to_string(),
                text: "second prompt".to_string(),
            },
        ]
    }

    #[test]
    fn select_fork_message_defaults_to_latest_and_accepts_index() {
        let messages = fork_messages_for_tests();
        assert_eq!(
            select_fork_message(&messages, "").unwrap().entry_id,
            "def222"
        );
        assert_eq!(
            select_fork_message(&messages, "1").unwrap().entry_id,
            "abc111"
        );
        assert!(select_fork_message(&messages, "0").is_err());
        assert!(select_fork_message(&messages, "3").is_err());
    }

    #[test]
    fn select_fork_message_accepts_unique_id_prefix() {
        let messages = fork_messages_for_tests();
        assert_eq!(
            select_fork_message(&messages, "def").unwrap().entry_id,
            "def222"
        );
        assert!(select_fork_message(&messages, "missing").is_err());
        let ambiguous = vec![
            RpcForkMessage {
                entry_id: "abc111".to_string(),
                text: String::new(),
            },
            RpcForkMessage {
                entry_id: "abc222".to_string(),
                text: String::new(),
            },
        ];
        assert!(select_fork_message(&ambiguous, "abc").is_err());
    }

    #[test]
    fn fork_message_preview_uses_first_non_empty_line_and_truncates() {
        let preview = fork_message_preview("\n  hello world\nsecond");
        assert_eq!(preview, "hello world");
        assert!(fork_message_preview(&"x".repeat(100)).ends_with("..."));
    }

    #[test]
    fn thinking_command_arg_accepts_aliases() {
        assert_eq!(thinking_command_arg("/thinking high"), Some("high"));
        assert_eq!(thinking_command_arg("/think low"), Some("low"));
        assert_eq!(thinking_command_arg("/t medium"), Some("medium"));
        assert_eq!(thinking_command_arg("/thinking"), None);
        assert_eq!(thinking_command_arg("/theme high"), None);
    }

    #[test]
    fn mode_command_arg_accepts_permissions_and_mode_alias() {
        assert_eq!(mode_command_arg("/permissions"), Some(("/permissions", "")));
        assert_eq!(
            mode_command_arg("/permissions acceptEdits"),
            Some(("/permissions", "acceptEdits"))
        );
        assert_eq!(mode_command_arg("/mode plan"), Some(("/mode", "plan")));
        assert_eq!(mode_command_arg("/model plan"), None);
    }

    #[test]
    fn name_command_arg_accepts_name_and_rename_alias() {
        assert_eq!(name_command_arg("/name release work"), Some(("/name", "release work")));
        assert_eq!(
            name_command_arg("/rename bug bash"),
            Some(("/rename", "bug bash"))
        );
        assert_eq!(name_command_arg("/rename"), None);
        assert_eq!(name_command_arg("/nameplate foo"), None);
    }

    #[test]
    fn compact_command_notes_accepts_only_compact_prefix() {
        assert_eq!(compact_command_notes("/compact keep setup"), Some("keep setup"));
        assert_eq!(compact_command_notes("/compact   "), Some(""));
        assert_eq!(compact_command_notes("/compact"), None);
        assert_eq!(compact_command_notes("/compactly keep"), None);
    }

    #[test]
    fn loop_command_arg_accepts_loop_and_autoloop() {
        assert_eq!(loop_command_arg("/loop"), Some(""));
        assert_eq!(loop_command_arg("/loop 5 ship it"), Some("5 ship it"));
        assert_eq!(loop_command_arg("/autoloop 2"), Some("2"));
        assert_eq!(loop_command_arg("/looper 2"), None);
    }

    #[test]
    fn parse_loop_request_defaults_clamps_and_keeps_goal() {
        assert_eq!(
            parse_loop_request(""),
            LoopRequest {
                turns: LOOP_DEFAULT_TURNS,
                goal: String::new(),
            }
        );
        assert_eq!(
            parse_loop_request("12 finish parity"),
            LoopRequest {
                turns: LOOP_MAX_TURNS,
                goal: "finish parity".to_string(),
            }
        );
        assert_eq!(
            parse_loop_request("0"),
            LoopRequest {
                turns: 1,
                goal: String::new(),
            }
        );
        assert_eq!(
            parse_loop_request("keep going"),
            LoopRequest {
                turns: LOOP_DEFAULT_TURNS,
                goal: "keep going".to_string(),
            }
        );
    }

    #[test]
    fn autonomous_loop_prompt_matches_desktop_contract() {
        let prompt = autonomous_loop_prompt(2, 4, "close gaps");
        assert!(prompt.contains("Autonomous loop turn 2/4."));
        assert!(prompt.contains("Goal: close gaps"));
        assert!(prompt.contains("do not invent extra work"));
    }

    #[test]
    fn auto_command_arg_accepts_aliases() {
        assert_eq!(auto_command_arg("/auto"), Some(""));
        assert_eq!(auto_command_arg("/auto on 5 ship it"), Some("on 5 ship it"));
        assert_eq!(auto_command_arg("/autorun status"), Some("status"));
        assert_eq!(auto_command_arg("/continuous off"), Some("off"));
        assert_eq!(auto_command_arg("/automatic"), None);
    }

    #[test]
    fn schedule_command_arg_accepts_schedule_and_cron() {
        assert_eq!(schedule_command_arg("/schedule"), Some(""));
        assert_eq!(
            schedule_command_arg("/schedule in 10m check tests"),
            Some("in 10m check tests")
        );
        assert_eq!(schedule_command_arg("/cron list"), Some("list"));
        assert_eq!(schedule_command_arg("/cron state"), Some("state"));
        assert_eq!(schedule_command_arg("/scheduler"), None);
    }

    #[test]
    fn notify_command_arg_and_parser_match_desktop_contract() {
        assert_eq!(notify_command_arg("/notify"), Some(""));
        assert_eq!(notify_command_arg("/notify on"), Some("on"));
        assert_eq!(notify_command_arg("/notifications status"), Some("status"));
        assert_eq!(notify_command_arg("/notifier"), None);
        assert_eq!(parse_notify_command(""), NotifyCommand::Status);
        assert_eq!(parse_notify_command("status"), NotifyCommand::Status);
        assert_eq!(parse_notify_command("on"), NotifyCommand::On);
        assert_eq!(parse_notify_command("enable"), NotifyCommand::On);
        assert_eq!(parse_notify_command("off"), NotifyCommand::Off);
        assert_eq!(parse_notify_command("disable"), NotifyCommand::Off);
        assert_eq!(parse_notify_command("test"), NotifyCommand::Test);
        assert_eq!(parse_notify_command("wat"), NotifyCommand::Usage);
    }

    #[test]
    fn parse_config_settings_target_accepts_desktop_settings_aliases() {
        assert_eq!(
            parse_config_settings_target("account"),
            Some(ConfigSettingsTarget::Account)
        );
        assert_eq!(
            parse_config_settings_target("open"),
            Some(ConfigSettingsTarget::Backends)
        );
        assert_eq!(
            parse_config_settings_target("backends"),
            Some(ConfigSettingsTarget::Backends)
        );
        assert_eq!(
            parse_config_settings_target("defaults"),
            Some(ConfigSettingsTarget::Defaults)
        );
        assert_eq!(
            parse_config_settings_target("agents"),
            Some(ConfigSettingsTarget::Agents)
        );
        assert_eq!(
            parse_config_settings_target("skills"),
            Some(ConfigSettingsTarget::Skills)
        );
        assert_eq!(
            parse_config_settings_target("hooks"),
            Some(ConfigSettingsTarget::Hooks)
        );
        assert_eq!(
            parse_config_settings_target("mcp"),
            Some(ConfigSettingsTarget::Mcp)
        );
        assert_eq!(
            parse_config_settings_target("approvals"),
            Some(ConfigSettingsTarget::Approvals)
        );
        assert_eq!(
            parse_config_settings_target("appearance"),
            Some(ConfigSettingsTarget::Appearance)
        );
        assert_eq!(
            parse_config_settings_target("sandbox"),
            Some(ConfigSettingsTarget::Sandbox)
        );
        assert_eq!(
            parse_config_settings_target("advanced"),
            Some(ConfigSettingsTarget::Advanced)
        );
        assert_eq!(parse_config_settings_target("path"), None);
        assert_eq!(parse_config_settings_target("set code_turn_notifications true"), None);
    }

    #[test]
    fn hooks_command_arg_and_parser_report_terminal_targets() {
        assert_eq!(hooks_command_arg("/hooks"), Some(""));
        assert_eq!(hooks_command_arg("/hooks status"), Some("status"));
        assert_eq!(hooks_command_arg("/hooks open"), Some("open"));
        assert_eq!(hooks_command_arg("/hook"), None);
        assert_eq!(parse_hooks_command(""), HooksCommand::Status);
        assert_eq!(parse_hooks_command("list"), HooksCommand::Status);
        assert_eq!(parse_hooks_command("diagnostics"), HooksCommand::Status);
        assert_eq!(parse_hooks_command("open"), HooksCommand::Open);
        assert_eq!(parse_hooks_command("settings"), HooksCommand::Open);
        assert_eq!(parse_hooks_command("edit"), HooksCommand::Open);
    }

    #[test]
    fn mcp_command_arg_and_parser_report_terminal_status() {
        assert_eq!(mcp_command_arg("/mcp"), Some(""));
        assert_eq!(mcp_command_arg("/mcp status"), Some("status"));
        assert_eq!(mcp_command_arg("/mcp open"), Some("open"));
        assert_eq!(mcp_command_arg("/mc"), None);
        assert_eq!(parse_mcp_command(""), McpCommand::Status);
        assert_eq!(parse_mcp_command("list"), McpCommand::Status);
        assert_eq!(parse_mcp_command("diagnostics"), McpCommand::Status);
        assert_eq!(parse_mcp_command("probe"), McpCommand::Probe);
        assert_eq!(parse_mcp_command("probe --save"), McpCommand::ProbeSave);
        assert_eq!(parse_mcp_command("refresh"), McpCommand::ProbeSave);
        assert_eq!(parse_mcp_command("reset"), McpCommand::Reset);
        assert_eq!(parse_mcp_command("reset-sessions"), McpCommand::Reset);
        assert_eq!(parse_mcp_command("open"), McpCommand::Open);
        assert_eq!(parse_mcp_command("settings"), McpCommand::Open);
        assert_eq!(parse_mcp_command("edit"), McpCommand::Open);
        assert_eq!(parse_mcp_command("remote"), McpCommand::Usage);
    }

    #[test]
    fn mcp_probe_cache_merge_preserves_matching_metadata() {
        let mut cfg = LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "docs".to_string(),
                crate::config::McpServerConfig {
                    tools: vec![
                        crate::config::McpToolConfig {
                            name: "search".to_string(),
                            enabled: false,
                            description: "Search docs".to_string(),
                            ..crate::config::McpToolConfig::default()
                        },
                        crate::config::McpToolConfig {
                            name: "stale".to_string(),
                            ..crate::config::McpToolConfig::default()
                        },
                    ],
                    resources: vec![crate::config::McpResourceConfig {
                        uri: "file:///repo/README.md".to_string(),
                        enabled: false,
                        name: "README".to_string(),
                        ..crate::config::McpResourceConfig::default()
                    }],
                    prompts: vec![crate::config::McpPromptConfig {
                        name: "summarize".to_string(),
                        enabled: false,
                        description: "Summarize docs".to_string(),
                        ..crate::config::McpPromptConfig::default()
                    }],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..LibertaiConfig::default()
        };
        let report = crate::commands::code_mcp::McpProbeReport {
            servers: vec![crate::commands::code_mcp::McpServerProbe {
                name: "docs".to_string(),
                transport: "stdio".to_string(),
                status: crate::commands::code_mcp::McpProbeStatus::Ok,
                tools: vec!["search".to_string(), "lookup".to_string()],
                resources: vec!["file:///repo/README.md".to_string()],
                prompts: vec!["summarize".to_string()],
                diagnostics: Vec::new(),
            }],
        };
        assert_eq!(merge_mcp_probe_report_into_config(&mut cfg, &report), 1);
        let server = cfg.mcp_servers.get("docs").unwrap();
        assert_eq!(
            server
                .tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool.enabled, tool.description.as_str()))
                .collect::<Vec<_>>(),
            vec![("search", false, "Search docs"), ("lookup", true, "")]
        );
        assert_eq!(server.resources[0].name, "README");
        assert!(!server.resources[0].enabled);
        assert_eq!(server.prompts[0].description, "Summarize docs");
        assert!(!server.prompts[0].enabled);
    }

    #[test]
    fn onboarding_command_arg_accepts_desktop_alias() {
        assert_eq!(onboarding_command_arg("/onboarding"), Some(""));
        assert_eq!(onboarding_command_arg("/onboarding save"), Some("save"));
        assert_eq!(onboarding_command_arg("/onboard"), Some(""));
        assert_eq!(
            onboarding_command_arg("/onboard save guide.md"),
            Some("save guide.md")
        );
        assert_eq!(onboarding_command_arg("/oneboarding save"), None);
    }

    #[test]
    fn send_command_arg_accepts_desktop_alias() {
        assert_eq!(send_command_arg("/send"), Some(""));
        assert_eq!(send_command_arg("/send worker finish tests"), Some("worker finish tests"));
        assert_eq!(send_command_arg("/send-message"), Some(""));
        assert_eq!(
            send_command_arg("/send-message worker finish tests"),
            Some("worker finish tests")
        );
        assert_eq!(send_command_arg("/sender worker finish tests"), None);
    }

    #[test]
    fn theme_command_arg_intercepts_desktop_theme_command() {
        assert_eq!(theme_command_arg("/theme"), Some(""));
        assert_eq!(theme_command_arg("/theme dark"), Some("dark"));
        assert_eq!(
            theme_command_arg("/theme high-contrast"),
            Some("high-contrast")
        );
        assert_eq!(theme_command_arg("/themes dark"), None);
    }

    #[test]
    fn parse_schedule_command_matches_desktop_contract() {
        assert_eq!(parse_schedule_command(""), ScheduleCommand::Status);
        assert_eq!(parse_schedule_command("list"), ScheduleCommand::Status);
        assert_eq!(parse_schedule_command("state"), ScheduleCommand::Status);
        assert_eq!(
            parse_schedule_command("cancel sch_2"),
            ScheduleCommand::Cancel("sch_2".to_string())
        );
        assert_eq!(parse_schedule_command("clear"), ScheduleCommand::Clear);
        assert!(matches!(
            parse_schedule_command("cancel sch_2 extra"),
            ScheduleCommand::Usage
        ));
        assert_eq!(
            parse_schedule_command("in 1.5s check tests"),
            ScheduleCommand::Add {
                delay: Duration::from_millis(1500),
                prompt: "check tests".to_string()
            }
        );
        assert_eq!(
            parse_schedule_command("10m check tests"),
            ScheduleCommand::Add {
                delay: Duration::from_secs(600),
                prompt: "check tests".to_string()
            }
        );
        assert!(matches!(parse_schedule_command("10m"), ScheduleCommand::Usage));
    }

    #[test]
    fn schedule_delay_formats_and_clamps() {
        assert_eq!(parse_schedule_delay("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_schedule_delay("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(format_schedule_delay(Duration::from_millis(250)), "250ms");
        assert_eq!(format_schedule_delay(Duration::from_secs(90)), "2m");
        assert_eq!(parse_schedule_delay("31d"), Some(SCHEDULE_MAX_DELAY));
        assert_eq!(parse_schedule_delay("0s"), None);
        assert_eq!(parse_schedule_delay("soon"), None);
    }

    #[test]
    fn pop_due_scheduled_prompt_returns_earliest_due() {
        let now = Instant::now();
        let mut runs = vec![
            scheduled_run_for_test("sch_2", "later", now + Duration::from_secs(5)),
            scheduled_run_for_test("sch_1", "now", now - Duration::from_millis(1)),
        ];
        let prompt = pop_due_scheduled_prompt(&mut runs).unwrap();
        assert!(prompt.contains("Scheduled follow-up (sch_1)."));
        assert!(prompt.contains("now"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "sch_2");
        assert!(pop_due_scheduled_prompt(&mut runs).is_none());
    }

    #[test]
    fn scheduled_runs_round_trip_through_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("schedules").join("project.json");
        let now = now_epoch_ms();
        let runs = vec![
            ScheduledRun {
                id: "sch_2".to_string(),
                prompt: "later".to_string(),
                due_at: Instant::now() + Duration::from_secs(5),
                due_epoch_ms: now + 5_000,
            },
            ScheduledRun {
                id: "sch_1".to_string(),
                prompt: "missed".to_string(),
                due_at: Instant::now(),
                due_epoch_ms: now.saturating_sub(1_000),
            },
        ];
        persist_scheduled_runs(&path, &runs).unwrap();

        let loaded = load_scheduled_runs(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "sch_1");
        assert_eq!(loaded[0].prompt, "missed");
        assert!(loaded[0].due_at <= Instant::now() + Duration::from_millis(50));
        assert_eq!(loaded[1].id, "sch_2");
        assert_eq!(loaded[1].due_epoch_ms, now + 5_000);
    }

    #[test]
    fn next_scheduled_run_id_tracks_restored_ids() {
        let runs = vec![
            scheduled_run_for_test("sch_1", "one", Instant::now()),
            scheduled_run_for_test("sch_9", "nine", Instant::now()),
            scheduled_run_for_test("manual", "ignored", Instant::now()),
        ];
        assert_eq!(next_scheduled_run_id(&runs), 10);
        assert_eq!(next_scheduled_run_id(&[]), 1);
    }

    #[test]
    fn schedule_store_path_is_stable_per_project() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();

        let first = schedule_store_path_for_project(&project).unwrap();
        let second = schedule_store_path_for_project(&project).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.extension().and_then(|value| value.to_str()), Some("json"));
        assert!(first.to_string_lossy().contains("code-schedules"));
    }

    #[test]
    fn parse_auto_command_defaults_clamps_and_keeps_goal() {
        assert_eq!(parse_auto_command(""), AutoCommand::Status);
        assert_eq!(parse_auto_command("status"), AutoCommand::Status);
        assert_eq!(parse_auto_command("off"), AutoCommand::Off);
        assert_eq!(
            parse_auto_command("on 30 finish parity"),
            AutoCommand::On {
                turns: AUTO_MAX_TURNS,
                goal: "finish parity".to_string(),
            }
        );
        assert_eq!(
            parse_auto_command("keep going"),
            AutoCommand::On {
                turns: AUTO_DEFAULT_TURNS,
                goal: "keep going".to_string(),
            }
        );
    }

    #[test]
    fn auto_loop_prompt_matches_desktop_contract() {
        let prompt = auto_loop_prompt(2, 4, "close gaps");
        assert!(prompt.contains("Auto mode turn 2/4."));
        assert!(prompt.contains("Goal: close gaps"));
        assert!(prompt.contains("AUTO_DONE"));
        assert!(prompt.contains("AUTO_BLOCKED"));
    }

    #[test]
    fn parse_thinking_level_accepts_supported_levels() {
        assert_eq!(parse_thinking_level("off").unwrap(), ThinkingLevel::Off);
        assert_eq!(parse_thinking_level("minimal").unwrap(), ThinkingLevel::Minimal);
        assert_eq!(parse_thinking_level("low").unwrap(), ThinkingLevel::Low);
        assert_eq!(parse_thinking_level("medium").unwrap(), ThinkingLevel::Medium);
        assert_eq!(parse_thinking_level("high").unwrap(), ThinkingLevel::High);
        assert_eq!(parse_thinking_level("xhigh").unwrap(), ThinkingLevel::XHigh);
        assert!(parse_thinking_level("").is_err());
        assert!(parse_thinking_level("maximum").is_err());
    }

    #[test]
    fn parse_permissions_command_maps_supported_modes() {
        assert_eq!(
            parse_permissions_command("acceptEdits"),
            PermissionsCommand::Set(Mode::AcceptEdits)
        );
        assert_eq!(
            parse_permissions_command("accept-edits"),
            PermissionsCommand::Set(Mode::AcceptEdits)
        );
        assert_eq!(
            parse_permissions_command("default"),
            PermissionsCommand::Set(Mode::Normal)
        );
        assert_eq!(
            parse_permissions_command("plan"),
            PermissionsCommand::Set(Mode::Plan)
        );
    }

    #[test]
    fn parse_permissions_command_handles_management_actions() {
        assert_eq!(parse_permissions_command(""), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("status"), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("open"), PermissionsCommand::Open);
        assert_eq!(
            parse_permissions_command("approvals"),
            PermissionsCommand::Open
        );
        assert_eq!(parse_permissions_command("forget"), PermissionsCommand::Forget);
        assert_eq!(
            parse_permissions_command("bypassPermissions"),
            PermissionsCommand::UnsupportedBypass
        );
        assert_eq!(parse_permissions_command("wat"), PermissionsCommand::Show);
    }

    #[test]
    fn parse_login_slash_target_maps_status_account_and_providers() {
        assert_eq!(parse_login_slash_target(""), LoginSlashTarget::Account);
        assert_eq!(parse_login_slash_target("status"), LoginSlashTarget::Status);
        assert_eq!(parse_login_slash_target("libertai"), LoginSlashTarget::Account);
        assert_eq!(
            parse_login_slash_target("anthropic"),
            LoginSlashTarget::Provider("anthropic")
        );
    }

    #[test]
    fn parse_plan_exit_choice_accepts_approve_and_enter() {
        assert!(parse_plan_exit_choice("a"));
        assert!(parse_plan_exit_choice("A"));
        assert!(parse_plan_exit_choice(""));
        assert!(parse_plan_exit_choice("\n"));
        assert!(!parse_plan_exit_choice("d"));
        assert!(!parse_plan_exit_choice("no"));
    }

    #[test]
    fn parse_model_spec_uses_current_provider_for_bare_model() {
        assert_eq!(
            parse_model_spec("libertai", "qwen3").unwrap(),
            ("libertai".to_string(), "qwen3".to_string())
        );
    }

    #[test]
    fn parse_model_spec_accepts_provider_model_pair() {
        assert_eq!(
            parse_model_spec("libertai", "openai/gpt-5").unwrap(),
            ("openai".to_string(), "gpt-5".to_string())
        );
    }

    #[test]
    fn parse_model_spec_rejects_empty_parts() {
        assert!(parse_model_spec("libertai", "").is_err());
        assert!(parse_model_spec("libertai", "/model").is_err());
        assert!(parse_model_spec("libertai", "provider/").is_err());
    }

    #[test]
    fn scoped_models_parse_patterns_and_filter_matches() {
        assert_eq!(
            scoped_models_usage_text(),
            "/scoped-models <patterns|clear> — filter /model list and /model next|prev"
        );
        assert_eq!(scoped_models_command_arg("/scoped-models"), Some(""));
        assert_eq!(
            scoped_models_command_arg("/scoped qwen* gemma*"),
            Some("qwen* gemma*")
        );
        assert_eq!(scoped_models_command_arg("/scope"), None);
        assert_eq!(
            parse_scoped_model_patterns("qwen*, gemma*  openai/gpt?"),
            vec!["qwen*", "gemma*", "openai/gpt?"]
        );
        assert_eq!(
            parse_scoped_models_command("clear"),
            ScopedModelsCommand::Clear
        );
        assert_eq!(
            parse_scoped_models_command("qwen*"),
            ScopedModelsCommand::Set(vec!["qwen*".to_string()])
        );

        let ids = vec![
            "qwen3.6-35b-a3b".to_string(),
            "gemma-4-31b-it".to_string(),
            "gpt-5".to_string(),
        ];
        assert_eq!(
            scoped_model_ids("libertai", &ids, &["qwen*".to_string()]),
            vec!["qwen3.6-35b-a3b".to_string()]
        );
        assert_eq!(
            scoped_model_ids("openai", &ids, &["openai/gpt-*".to_string()]),
            vec!["gpt-5".to_string()]
        );
        assert_eq!(
            scoped_model_ids("libertai", &ids, &["no-match*".to_string()]),
            ids
        );
    }

    #[test]
    fn model_slash_command_cycles_scoped_models() {
        assert_eq!(
            model_usage_text(),
            "/model [status|list|next|prev|model|provider/model]"
        );
        assert_eq!(parse_model_slash_command(""), ModelSlashCommand::Status);
        assert_eq!(parse_model_slash_command("list"), ModelSlashCommand::List);
        assert_eq!(parse_model_slash_command("next"), ModelSlashCommand::Next);
        assert_eq!(
            parse_model_slash_command("prev"),
            ModelSlashCommand::Previous
        );
        assert_eq!(
            parse_model_slash_command("openai/gpt-5"),
            ModelSlashCommand::Set("openai/gpt-5")
        );

        let ids = vec![
            "qwen-a".to_string(),
            "gemma".to_string(),
            "qwen-b".to_string(),
        ];
        let scope = vec!["qwen*".to_string()];
        assert_eq!(
            cycle_scoped_model("libertai", "qwen-a", &ids, &scope, 1),
            Some("qwen-b".to_string())
        );
        assert_eq!(
            cycle_scoped_model("libertai", "qwen-a", &ids, &scope, -1),
            Some("qwen-b".to_string())
        );
        assert_eq!(
            cycle_scoped_model("libertai", "gemma", &ids, &scope, 1),
            Some("qwen-a".to_string())
        );
    }

    #[test]
    fn parse_session_name_trims_valid_name() {
        assert_eq!(parse_session_name("  triage run  ").unwrap(), "triage run");
    }

    #[test]
    fn parse_session_name_rejects_empty_or_too_long() {
        assert!(parse_session_name("   ").is_err());
        assert!(parse_session_name(&"x".repeat(121)).is_err());
    }

    #[test]
    fn export_path_uses_default_or_custom_path() {
        assert_eq!(
            export_path(None).unwrap(),
            PathBuf::from("libertai-transcript.md")
        );
        assert_eq!(
            export_path(Some("out/session.md")).unwrap(),
            PathBuf::from("out/session.md")
        );
        assert_eq!(
            export_path(Some("save")).unwrap(),
            PathBuf::from("libertai-transcript.md")
        );
        assert_eq!(
            export_path(Some("save report.md")).unwrap(),
            PathBuf::from("report.md")
        );
    }

    #[test]
    fn share_target_uses_default_or_custom_path() {
        assert_eq!(
            parse_share_target(None).unwrap(),
            ShareTarget::File(PathBuf::from("libertai-share.html"))
        );
        assert_eq!(
            parse_share_target(Some("out/session.html")).unwrap(),
            ShareTarget::File(PathBuf::from("out/session.html"))
        );
        assert_eq!(
            parse_share_target(Some("save")).unwrap(),
            ShareTarget::File(PathBuf::from("libertai-share.html"))
        );
        assert_eq!(
            parse_share_target(Some("save report.html")).unwrap(),
            ShareTarget::File(PathBuf::from("report.html"))
        );
        assert_eq!(
            parse_share_target(Some("gist")).unwrap(),
            ShareTarget::Gist {
                public: false,
                filename: "libertai-share.html".to_string(),
            }
        );
        assert_eq!(
            parse_share_target(Some("gist public team notes.html")).unwrap(),
            ShareTarget::Gist {
                public: true,
                filename: "team-notes.html".to_string(),
            }
        );
        assert!(parse_share_target(Some("gist --wat")).is_err());
    }

    #[test]
    fn onboarding_target_uses_default_path_or_gist() {
        assert_eq!(
            parse_onboarding_target(None).unwrap(),
            OnboardingTarget::File(PathBuf::from("libertai-onboarding.md"))
        );
        assert_eq!(
            parse_onboarding_target(Some("docs/team.md")).unwrap(),
            OnboardingTarget::File(PathBuf::from("docs/team.md"))
        );
        assert_eq!(
            parse_onboarding_target(Some("save")).unwrap(),
            OnboardingTarget::File(PathBuf::from("libertai-onboarding.md"))
        );
        assert_eq!(
            parse_onboarding_target(Some("save docs/team.md")).unwrap(),
            OnboardingTarget::File(PathBuf::from("docs/team.md"))
        );
        assert_eq!(
            parse_onboarding_target(Some("gist")).unwrap(),
            OnboardingTarget::Gist {
                public: false,
                filename: "libertai-onboarding.md".to_string(),
            }
        );
        assert_eq!(
            parse_onboarding_target(Some("gist public team guide.md")).unwrap(),
            OnboardingTarget::Gist {
                public: true,
                filename: "team-guide.md".to_string(),
            }
        );
        assert!(parse_onboarding_target(Some("gist --wat")).is_err());
    }

    #[test]
    fn render_markdown_transcript_includes_roles_and_tools() {
        let messages = vec![
            Message::User(pi::model::UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 1,
            }),
            Message::assistant(pi::model::AssistantMessage {
                content: vec![
                    ContentBlock::Text(pi::model::TextContent::new("hi")),
                    ContentBlock::ToolCall(pi::model::ToolCall {
                        id: "tool-1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path":"src/lib.rs"}),
                        thought_signature: None,
                    }),
                ],
                api: "openai".to_string(),
                provider: "libertai".to_string(),
                model: "fast".to_string(),
                usage: pi::model::Usage::default(),
                stop_reason: pi::model::StopReason::Stop,
                error_message: None,
                timestamp: 2,
            }),
            Message::tool_result(pi::model::ToolResultMessage {
                tool_call_id: "tool-1".to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text(pi::model::TextContent::new("contents"))],
                details: None,
                is_error: false,
                paused: None,
                timestamp: 3,
            }),
        ];
        let rendered = render_markdown_transcript(&messages);
        assert!(rendered.contains("## User\n\nhello"));
        assert!(rendered.contains("## Assistant"));
        assert!(rendered.contains("### Tool Call: read"));
        assert!(rendered.contains("\"path\": \"src/lib.rs\""));
        assert!(rendered.contains("## Tool Result: read"));
        assert!(rendered.contains("contents"));
    }

    #[test]
    fn render_html_transcript_escapes_roles_text_and_tool_json() {
        let messages = vec![
            Message::User(pi::model::UserMessage {
                content: UserContent::Text("hello <world>".to_string()),
                timestamp: 1,
            }),
            Message::assistant(pi::model::AssistantMessage {
                content: vec![
                    ContentBlock::Text(pi::model::TextContent::new("hi & bye")),
                    ContentBlock::ToolCall(pi::model::ToolCall {
                        id: "tool-1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path":"src/<lib>.rs"}),
                        thought_signature: None,
                    }),
                ],
                api: "openai".to_string(),
                provider: "libertai".to_string(),
                model: "fast".to_string(),
                usage: pi::model::Usage::default(),
                stop_reason: pi::model::StopReason::Stop,
                error_message: None,
                timestamp: 2,
            }),
        ];
        let rendered = render_html_transcript(&messages);
        assert!(rendered.contains("<!doctype html>"));
        assert!(rendered.contains("User"));
        assert!(rendered.contains("hello &lt;world&gt;"));
        assert!(rendered.contains("hi &amp; bye"));
        assert!(rendered.contains("Tool Call: read"));
        assert!(rendered.contains("src/&lt;lib&gt;.rs"));
    }

    #[test]
    fn last_assistant_text_extracts_latest_text_blocks_only() {
        let messages = vec![
            Message::assistant(pi::model::AssistantMessage {
                content: vec![ContentBlock::Text(pi::model::TextContent::new("old"))],
                api: "openai".to_string(),
                provider: "libertai".to_string(),
                model: "fast".to_string(),
                usage: pi::model::Usage::default(),
                stop_reason: pi::model::StopReason::Stop,
                error_message: None,
                timestamp: 1,
            }),
            Message::User(pi::model::UserMessage {
                content: UserContent::Text("again".to_string()),
                timestamp: 2,
            }),
            Message::assistant(pi::model::AssistantMessage {
                content: vec![
                    ContentBlock::Text(pi::model::TextContent::new("new")),
                    ContentBlock::ToolCall(pi::model::ToolCall {
                        id: "tool-1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path":"src/lib.rs"}),
                        thought_signature: None,
                    }),
                    ContentBlock::Text(pi::model::TextContent::new("reply")),
                ],
                api: "openai".to_string(),
                provider: "libertai".to_string(),
                model: "fast".to_string(),
                usage: pi::model::Usage::default(),
                stop_reason: pi::model::StopReason::Stop,
                error_message: None,
                timestamp: 3,
            }),
        ];
        assert_eq!(last_assistant_text(&messages).unwrap(), "new\nreply");
    }

    #[test]
    fn osc52_sequence_base64_encodes_clipboard_text() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn quote_for_sh_wraps_and_escapes_single_quotes() {
        assert_eq!(
            quote_for_sh(Path::new("/tmp/has ' quote/MEMORY.md")),
            "'/tmp/has '\\'' quote/MEMORY.md'"
        );
    }

    #[test]
    fn parse_agent_slash_query_requires_name_and_task() {
        assert_eq!(
            parse_agent_slash_query("reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: None,
                background: false,
            }
        );
        assert_eq!(
            parse_agent_slash_query("--worktree reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::Worktree),
                background: false,
            }
        );
        assert_eq!(
            parse_agent_slash_query("--isolation=worktree reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::Worktree),
                background: false,
            }
        );
        assert_eq!(
            parse_agent_slash_query("--worktree --same-cwd reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::SameCwd),
                background: false,
            }
        );
        assert_eq!(
            parse_agent_slash_query("--background reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: None,
                background: true
            }
        );
        assert_eq!(
            parse_agent_slash_query("--worktree --detached reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::Worktree),
                background: true
            }
        );
        assert!(parse_agent_slash_query("reviewer").is_err());
        assert!(parse_agent_slash_query("reviewer   ").is_err());
    }

    #[test]
    fn background_agent_args_target_libertai_or_lcode() {
        let launch = BackgroundAgentLaunch {
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: Mode::Plan,
            prompt: "Use the task tool".to_string(),
            cwd: PathBuf::from("/tmp/project"),
        };
        assert_eq!(
            background_agent_args(Path::new("/usr/bin/libertai"), &launch),
            vec![
                "code",
                "--provider",
                "libertai",
                "--model",
                "qwen",
                "--plan",
                "Use the task tool"
            ]
        );
        assert_eq!(
            background_agent_args(Path::new("/usr/bin/lcode"), &launch),
            vec![
                "--provider",
                "libertai",
                "--model",
                "qwen",
                "--plan",
                "Use the task tool"
            ]
        );
    }

    #[test]
    fn background_agent_args_skip_empty_provider_model_and_accept_edits_flag() {
        let launch = BackgroundAgentLaunch {
            name: "reviewer".to_string(),
            provider: String::new(),
            model: String::new(),
            mode: Mode::AcceptEdits,
            prompt: "Run review".to_string(),
            cwd: PathBuf::from("/tmp/project"),
        };
        assert_eq!(
            background_agent_args(Path::new("/usr/bin/libertai"), &launch),
            vec!["code", "Run review"]
        );
    }

    #[test]
    fn parse_agents_command_accepts_background_management() {
        assert_eq!(
            parse_agents_command("background"),
            AgentsSlashCommand::BackgroundList
        );
        assert_eq!(
            parse_agents_command("bg list"),
            AgentsSlashCommand::BackgroundList
        );
        assert_eq!(
            parse_agents_command("background log 123"),
            AgentsSlashCommand::BackgroundLog("123")
        );
        assert_eq!(
            parse_agents_command("bg stop 123"),
            AgentsSlashCommand::BackgroundKill("123")
        );
    }

    #[test]
    fn background_agent_record_captures_launch_metadata() {
        let launch = BackgroundAgentLaunch {
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: Mode::Plan,
            prompt: "Run review\nwith details".to_string(),
            cwd: PathBuf::from("/tmp/project"),
        };
        let started = StartedBackgroundAgent {
            pid: 4242,
            log_path: PathBuf::from("/tmp/reviewer.log"),
        };
        let record = background_agent_record(&launch, &started);
        assert_eq!(record.pid, 4242);
        assert_eq!(record.name, "reviewer");
        assert_eq!(record.mode, "plan");
        assert_eq!(record.cwd, "/tmp/project");
        assert_eq!(record.log_path, "/tmp/reviewer.log");
        assert_eq!(record.prompt_preview, "Run review with details");
        assert!(record.started_at_ms > 0);
    }

    #[test]
    fn parse_agents_create_query_accepts_description_and_worktree() {
        assert_eq!(
            parse_agents_create_query("--worktree reviewer Reviews changes").unwrap(),
            AgentsCreateQuery {
                name: "reviewer",
                description: Some("Reviews changes"),
                worktree: true
            }
        );
        assert_eq!(
            parse_agents_create_query("--worktree --same-cwd qa").unwrap(),
            AgentsCreateQuery {
                name: "qa",
                description: None,
                worktree: false
            }
        );
        assert!(parse_agents_create_query("").is_err());
    }

    #[test]
    fn parse_agents_command_accepts_list_open_and_create() {
        assert_eq!(parse_agents_command(""), AgentsSlashCommand::List);
        assert_eq!(parse_agents_command("list"), AgentsSlashCommand::List);
        assert_eq!(parse_agents_command("open"), AgentsSlashCommand::Open);
        assert_eq!(
            parse_agents_command("create --worktree reviewer Reviews changes"),
            AgentsSlashCommand::Create("--worktree reviewer Reviews changes")
        );
        assert_eq!(
            parse_agents_command("delete reviewer"),
            AgentsSlashCommand::Delete("reviewer")
        );
        assert_eq!(
            parse_agents_command("remove reviewer"),
            AgentsSlashCommand::Delete("reviewer")
        );
    }

    #[test]
    fn image_command_arg_accepts_image_and_attach_only() {
        assert_eq!(
            image_command_arg("/image screenshot.png describe"),
            Some(("/image", "screenshot.png describe"))
        );
        assert_eq!(
            image_command_arg("/attach screenshot.png describe"),
            Some(("/attach", "screenshot.png describe"))
        );
        assert_eq!(image_command_arg("/imagex screenshot.png"), None);
        assert_eq!(image_command_arg("/image"), None);
    }

    #[test]
    fn mention_command_arg_accepts_mention_only() {
        assert_eq!(
            mention_command_arg("/mention src/lib.rs summarize"),
            Some("src/lib.rs summarize")
        );
        assert_eq!(mention_command_arg("/mentions src/lib.rs"), None);
        assert_eq!(mention_command_arg("/mention"), None);
    }

    #[test]
    fn parse_image_prompt_supports_quoted_paths() {
        let (path, prompt) = parse_image_prompt("'has space.png' what is here").unwrap();
        assert_eq!(path, PathBuf::from("has space.png"));
        assert_eq!(prompt, "what is here");

        let (path, prompt) = parse_image_prompt("\"dir/has \\\" quote.png\"").unwrap();
        assert_eq!(path, PathBuf::from("dir/has \" quote.png"));
        assert!(prompt.is_empty());
    }

    #[test]
    fn parse_mention_prompt_reuses_quoted_path_parsing() {
        let (path, prompt) = parse_mention_prompt("\"has space.txt\" explain").unwrap();
        assert_eq!(path, PathBuf::from("has space.txt"));
        assert_eq!(prompt, "explain");
    }

    #[test]
    fn detect_supported_image_mime_type_checks_magic_bytes() {
        assert_eq!(
            detect_supported_image_mime_type(b"\x89PNG\r\n\x1A\nrest"),
            Some("image/png")
        );
        assert_eq!(
            detect_supported_image_mime_type(b"\xFF\xD8\xFF"),
            Some("image/jpeg")
        );
        assert_eq!(detect_supported_image_mime_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(
            detect_supported_image_mime_type(b"RIFFxxxxWEBPrest"),
            Some("image/webp")
        );
        assert_eq!(detect_supported_image_mime_type(b"not an image"), None);
    }

    #[test]
    fn build_image_prompt_content_reads_local_image() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tiny.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1A\npayload").unwrap();

        let content =
            build_image_prompt_content(&format!("{} describe it", path.display()), None).unwrap();
        assert!(matches!(&content[0], ContentBlock::Text(text) if text.text == "describe it"));
        assert!(
            matches!(&content[1], ContentBlock::Image(image) if image.mime_type == "image/png")
        );
    }

    #[test]
    fn build_mention_prompt_reads_local_text_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("note.txt");
        std::fs::write(&path, "alpha\nbeta").unwrap();

        let prompt = build_mention_prompt(&format!("{} summarize", path.display()), None).unwrap();
        assert!(prompt.contains("summarize"));
        assert!(prompt.contains("Mentioned file:"));
        assert!(prompt.contains("alpha\nbeta"));
    }

    #[test]
    fn build_agent_prompt_from_defs_matches_prefix() {
        let agents = vec![crate::commands::code_agents::AgentDefinition {
            name: "reviewer".to_string(),
            description: "Reviews changes".to_string(),
            tools: None,
            model: None,
            worktree: false,
            system_prompt: "Review carefully.".to_string(),
            source: crate::commands::code_agents::AgentSource::Project(PathBuf::from(
                "/tmp/.claude/agents",
            )),
        }];
        let prompt = build_agent_prompt_from_defs(
            &AgentSlashQuery {
                name: "rev",
                task: "check the diff",
                isolation: None,
                background: false,
            },
            &agents,
        )
        .unwrap();
        assert!(prompt.contains("subagent_type \"reviewer\""));
        assert!(prompt.contains("check the diff"));
    }

    #[test]
    fn build_agent_prompt_from_defs_includes_worktree_isolation() {
        let agents = vec![crate::commands::code_agents::AgentDefinition {
            name: "reviewer".to_string(),
            description: "Reviews changes".to_string(),
            tools: None,
            model: None,
            worktree: false,
            system_prompt: "Review carefully.".to_string(),
            source: crate::commands::code_agents::AgentSource::Project(PathBuf::from(
                "/tmp/.claude/agents",
            )),
        }];
        let prompt = build_agent_prompt_from_defs(
            &AgentSlashQuery {
                name: "reviewer",
                task: "check the diff",
                isolation: Some(AgentSlashIsolation::Worktree),
                background: false,
            },
            &agents,
        )
        .unwrap();
        assert!(prompt.contains("subagent_type \"reviewer\""));
        assert!(prompt.contains("isolation: \"worktree\""));
    }

    #[test]
    fn build_agent_prompt_from_defs_uses_worktree_default() {
        let agents = vec![crate::commands::code_agents::AgentDefinition {
            name: "reviewer".to_string(),
            description: "Reviews changes".to_string(),
            tools: None,
            model: None,
            worktree: true,
            system_prompt: "Review carefully.".to_string(),
            source: crate::commands::code_agents::AgentSource::Project(PathBuf::from(
                "/tmp/.claude/agents",
            )),
        }];
        let prompt = build_agent_prompt_from_defs(
            &AgentSlashQuery {
                name: "reviewer",
                task: "check the diff",
                isolation: None,
                background: false,
            },
            &agents,
        )
        .unwrap();
        assert!(prompt.contains("isolation: \"worktree\""));

        let prompt = build_agent_prompt_from_defs(
            &AgentSlashQuery {
                name: "reviewer",
                task: "check the diff",
                isolation: Some(AgentSlashIsolation::SameCwd),
                background: false,
            },
            &agents,
        )
        .unwrap();
        assert!(!prompt.contains("isolation: \"worktree\""));
    }

    #[test]
    fn parse_template_query_splits_name_and_args() {
        assert_eq!(parse_template_query("review src/lib.rs").unwrap(), ("review", "src/lib.rs"));
        assert_eq!(parse_template_query("review").unwrap(), ("review", ""));
        assert!(parse_template_query("").is_err());
    }

    #[test]
    fn parse_skills_command_accepts_list_and_toggles() {
        assert_eq!(parse_skills_command("").unwrap(), SkillsCommand::List);
        assert_eq!(parse_skills_command("status").unwrap(), SkillsCommand::List);
        assert_eq!(parse_skills_command("open").unwrap(), SkillsCommand::Open);
        assert_eq!(
            parse_skills_command("settings").unwrap(),
            SkillsCommand::Open
        );
        assert_eq!(
            parse_skills_command("enable libertai-harness").unwrap(),
            SkillsCommand::Enable("libertai-harness".to_string())
        );
        assert_eq!(
            parse_skills_command("off project-review").unwrap(),
            SkillsCommand::Disable("project-review".to_string())
        );
        assert!(parse_skills_command("enable").is_err());
        assert!(parse_skills_command("remove foo").is_err());
    }

    #[test]
    fn review_command_parts_accepts_review_aliases() {
        assert_eq!(review_command_parts("/review src"), Some(("/review", "src")));
        assert_eq!(
            review_command_parts("/security-review auth"),
            Some(("/security-review", "auth"))
        );
        assert_eq!(
            review_command_parts("/pr-comments 123"),
            Some(("/pr-comments", "123"))
        );
        assert_eq!(review_command_parts("/pr_comments"), Some(("/pr_comments", "")));
        assert_eq!(review_command_parts("/reviewer src"), None);
    }

    #[test]
    fn parse_pr_comments_reply_requires_thread_and_body() {
        assert_eq!(
            pr_comments_reply_arg("/pr_comments reply PRRT_1 Fixed in the next commit."),
            Some("PRRT_1 Fixed in the next commit.")
        );
        assert_eq!(
            parse_pr_comments_reply("PRRT_1 Fixed in the next commit.").unwrap(),
            ("PRRT_1", "Fixed in the next commit.")
        );
        assert!(parse_pr_comments_reply("PRRT_1").is_err());
    }

    #[test]
    fn parse_pr_comments_edit_requires_comment_and_body() {
        assert_eq!(
            pr_comments_edit_arg("/pr_comments edit PRRC_1 Reworded comment."),
            Some("PRRC_1 Reworded comment.")
        );
        assert_eq!(
            parse_pr_comments_edit("PRRC_1 Reworded comment.").unwrap(),
            ("PRRC_1", "Reworded comment.")
        );
        assert!(parse_pr_comments_edit("PRRC_1").is_err());
    }

    #[test]
    fn parse_pr_comments_resolve_requires_single_thread_id() {
        assert_eq!(
            pr_comments_resolve_arg("/pr_comments resolve PRRT_1"),
            Some("PRRT_1")
        );
        assert_eq!(
            pr_comments_unresolve_arg("/pr_comments unresolve PRRT_1"),
            Some("PRRT_1")
        );
        assert_eq!(
            pr_comments_unresolve_arg("/pr_comments reopen PRRT_1"),
            Some("PRRT_1")
        );
        assert_eq!(parse_pr_comments_resolve("PRRT_1").unwrap(), "PRRT_1");
        assert!(parse_pr_comments_resolve("").is_err());
        assert!(parse_pr_comments_resolve("PRRT_1 extra").is_err());
    }

    #[test]
    fn parse_pr_comments_file_viewed_requires_path() {
        assert_eq!(
            pr_comments_viewed_arg("/pr_comments viewed src/lib.rs"),
            Some("src/lib.rs")
        );
        assert_eq!(
            pr_comments_viewed_arg("/pr_comments view js/app.js"),
            Some("js/app.js")
        );
        assert_eq!(
            pr_comments_unviewed_arg("/pr_comments unviewed src/lib.rs"),
            Some("src/lib.rs")
        );
        assert_eq!(
            pr_comments_unviewed_arg("/pr_comments unview js/app.js"),
            Some("js/app.js")
        );
        assert_eq!(parse_pr_comments_file_path("src/lib.rs").unwrap(), "src/lib.rs");
        assert!(parse_pr_comments_file_path("").is_err());
        assert!(parse_pr_comments_all_files("--all"));
        assert!(parse_pr_comments_all_files("all"));
        assert!(!parse_pr_comments_all_files("src/lib.rs"));
    }

    #[test]
    fn parse_pr_comments_thread_requires_target_line_and_body() {
        assert_eq!(
            pr_comments_thread_arg("/pr_comments thread src/lib.rs:42 Needs a test."),
            Some("src/lib.rs:42 Needs a test.")
        );
        assert_eq!(
            pr_comments_thread_arg("/pr_comments comment js/app.js:9 Check this."),
            Some("js/app.js:9 Check this.")
        );
        assert_eq!(
            parse_pr_comments_thread("src/lib.rs:42 Needs a test.").unwrap(),
            ("src/lib.rs", 42, "Needs a test.")
        );
        assert!(parse_pr_comments_thread("src/lib.rs Needs a test.").is_err());
        assert!(parse_pr_comments_thread("src/lib.rs:0 Needs a test.").is_err());
        assert!(parse_pr_comments_thread("src/lib.rs:42").is_err());
    }

    #[test]
    fn parse_pr_comments_drafts_supports_stage_list_and_submit() {
        assert_eq!(
            pr_comments_draft_arg("/pr_comments draft src/lib.rs:42 Needs a test."),
            Some("src/lib.rs:42 Needs a test.")
        );
        assert_eq!(
            pr_comments_drafts_arg("/pr_comments drafts submit"),
            Some("submit")
        );
        assert_eq!(pr_comments_drafts_arg("/pr_comments drafts"), Some(""));

        let draft = parse_pr_comment_draft("src/lib.rs:42 Needs a test.").unwrap();
        assert_eq!(
            draft,
            PrCommentDraft {
                path: "src/lib.rs".to_string(),
                line: 42,
                body: "Needs a test.".to_string(),
            }
        );
    }

    #[test]
    fn parse_pr_comments_review_requires_event() {
        assert_eq!(
            pr_comments_review_arg("/pr_comments review approve Looks good."),
            Some("approve Looks good.")
        );
        assert_eq!(
            pr_comments_review_arg("/pr_comments submit request_changes Needs a test."),
            Some("request_changes Needs a test.")
        );
        assert_eq!(
            parse_pr_comments_review("comment Summary only.").unwrap(),
            ("comment", "Summary only.")
        );
        assert_eq!(parse_pr_comments_review("approve").unwrap(), ("approve", ""));
    }

    #[test]
    fn build_review_slash_prompt_includes_scope_and_rules() {
        let prompt = build_review_slash_prompt("/review", "src/lib.rs").unwrap();
        assert!(prompt.contains("Review the current code changes"));
        assert!(prompt.contains("User-requested scope: src/lib.rs"));
        assert!(prompt.contains("Do not modify files or make commits"));

        let security = build_review_slash_prompt("/security-review", "").unwrap();
        assert!(security.contains("Run a focused security review"));
        assert!(security.contains("Security focus:"));

        let pr = build_review_slash_prompt("/pr_comments", "42").unwrap();
        assert!(pr.contains("pull request review comments"));
        assert!(pr.contains("User-requested PR scope: 42"));
    }

    #[test]
    fn parse_direct_custom_slash_parses_name_and_args() {
        assert_eq!(parse_direct_custom_slash("/review src"), Some(("review", "src")));
        assert_eq!(parse_direct_custom_slash("/team/review src"), None);
        assert_eq!(parse_direct_custom_slash("/review"), Some(("review", "")));
        assert_eq!(parse_direct_custom_slash("review"), None);
    }
}
