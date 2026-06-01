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
use crate::commands::code_sandbox::{
    binary_on_path, detect_strict_profile, format_profile_text, BindKind, StrictProfile,
};
use crate::commands::code_session::{
    build_session_options, list_past_sessions, CodeSessionConfig, SessionPersistence,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::commands::code_term::TerminalApprovalUi;
use crate::config::{mask_key, Config as LibertaiConfig};

/// ANSI dim/bold helpers for cooked output (agent streaming phase).
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

const SHELL_ESCAPE_MAX_DISPLAY_BYTES: usize = 256 * 1024;
const SHELL_ESCAPE_CONTEXT_LIMIT: usize = 5;
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
    Json,
    Show(String),
    ShowJson(String),
    Run(String),
    Cancel(String),
    Clear,
    Add { delay: Duration, prompt: String },
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ScheduleJsonRow {
    id: String,
    prompt: String,
    state: String,
    due_epoch_ms: u64,
    due_in_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ScheduleJsonPayload {
    surface: &'static str,
    command: &'static str,
    query: String,
    aliases: &'static [&'static str],
    supported_actions: &'static [&'static str],
    total: usize,
    due: usize,
    pending: usize,
    runs: Vec<ScheduleJsonRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct AutoJsonPayload {
    surface: &'static str,
    command: &'static str,
    query: String,
    aliases: &'static [&'static str],
    supported_actions: &'static [&'static str],
    active: bool,
    limit: usize,
    completed: usize,
    remaining: usize,
    goal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyCommand {
    Status,
    Json,
    On,
    Off,
    Test,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpCommand {
    Status,
    Json,
    Show(String),
    Probe,
    ProbeSave,
    Reset,
    Open,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimCommand {
    Status,
    Json,
    Enable,
    Disable,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimInputMode {
    Insert,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimNormalAction {
    MoveLeft,
    MoveRight,
    Home,
    End,
    Delete,
    InsertBefore,
    InsertAfter,
    InsertHome,
    InsertEnd,
    Submit,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdeCommand {
    Status,
    Json,
    Open,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BugCommand {
    Template,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyCommand {
    LastAssistant,
    Status,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeysCommand {
    Show,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadCommand {
    Session,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusCommand {
    Session,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorCommand {
    Run,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortCommand {
    Status,
    Json,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopedModelsCommand {
    Status,
    Json,
    Clear,
    Set(Vec<String>),
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ThemeCommand {
    Status,
    Json,
    Requested(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSlashCommand<'a> {
    Status,
    Json,
    JsonList,
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
    Json,
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
    Json,
    Off,
    On { turns: usize, goal: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillsCommand {
    List,
    Json,
    Show(String),
    ShowJson(String),
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
static VIM_INPUT_ENABLED: AtomicBool = AtomicBool::new(false);

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
    let mut pending_shell_context: Vec<String> = Vec::new();

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
        if parse_usage_summary_command(trimmed).is_some() {
            let tool_activity = tool_activity
                .lock()
                .map(|tracker| tracker.summary())
                .unwrap_or_default();
            print_usage_summary(usage_summary(&usage_history), &tool_activity);
            continue;
        }
        if let Some(rest) = notify_command_arg(trimmed) {
            if let Err(e) = handle_notify_command(rest, &mut cfg) {
                eprintln!("{DIM}  /notify: {e:#}{RESET}");
            }
            continue;
        }
        if let Some(rest) = hooks_command_arg(trimmed) {
            print_hooks_command(&cfg, rest, parse_hooks_command(rest));
            continue;
        }
        if let Some(rest) = mcp_command_arg(trimmed) {
            print_mcp_status(rest, parse_mcp_command(rest));
            continue;
        }
        if let Some(rest) = send_command_arg(trimmed) {
            print_send_status(rest);
            continue;
        }
        if let Some(rest) = theme_command_arg(trimmed) {
            print_theme_status(parse_theme_command(rest), rest);
            continue;
        }
        if let Some(rest) = vim_command_arg(trimmed) {
            print_vim_status(parse_vim_command(rest), rest);
            continue;
        }
        if let Some(rest) = ide_command_arg(trimmed) {
            print_ide_status(parse_ide_command(rest), rest);
            continue;
        }
        if let Some(rest) = bug_command_arg(trimmed) {
            print_bug_command(
                parse_bug_command(rest),
                rest,
                &provider,
                &model,
                mode.get(),
                output_style.as_deref(),
            );
            continue;
        }
        if let Some(rest) = copy_command_arg(trimmed) {
            match parse_copy_command(rest) {
                CopyCommand::LastAssistant => copy_last_assistant(&handle).await,
                CopyCommand::Status => print_copy_status(&handle).await,
                CopyCommand::Json => print_copy_json(&handle, rest).await,
                CopyCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", copy_usage_text());
                }
            }
            continue;
        }
        if let Some(rest) = hotkeys_command_arg(trimmed) {
            match parse_hotkeys_command(rest) {
                HotkeysCommand::Show => print_hotkeys(),
                HotkeysCommand::Json => print_hotkeys_json(rest),
                HotkeysCommand::Usage => {
                    println!(
                        "{DIM}  usage:{RESET} {}",
                        hotkeys_usage_text()
                    );
                }
            }
            continue;
        }
        if let Some(rest) = reload_command_arg(trimmed) {
            match parse_reload_command(rest) {
                ReloadCommand::Session => {
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
                            update_bar_status(|status| {
                                status.output_style = output_style.clone()
                            });
                        }
                        Err(e) => eprintln!("{DIM}  /reload: {e:#}{RESET}"),
                    }
                }
                ReloadCommand::Json => {
                    print_reload_preview_json(
                        rest,
                        &provider,
                        &model,
                        mode.get(),
                        output_style.as_deref(),
                        &cfg,
                    );
                }
                ReloadCommand::Usage => {
                    println!(
                        "{DIM}  usage:{RESET} /reload [config|session|now|fresh|json|--json|config --json|session --json|now --json|fresh --json]"
                    );
                }
            }
            continue;
        }
        if let Some(rest) = status_command_arg(trimmed) {
            match parse_status_command(rest) {
                StatusCommand::Session => {
                    print_session_status(
                        &provider,
                        &model,
                        mode.get(),
                        output_style.as_deref(),
                        &cfg,
                        usage_summary(&usage_history),
                    );
                }
                StatusCommand::Json => {
                    print_session_status_json(
                        rest,
                        &provider,
                        &model,
                        mode.get(),
                        output_style.as_deref(),
                        &cfg,
                        usage_summary(&usage_history),
                    );
                }
                StatusCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", status_usage_text());
                }
            }
            continue;
        }
        if let Some(rest) = doctor_command_arg(trimmed) {
            match parse_doctor_command(rest) {
                DoctorCommand::Run => {
                    print_doctor(
                        &handle,
                        &provider,
                        &model,
                        mode.get(),
                        output_style.as_deref(),
                        &cfg,
                        &approvals,
                        &scheduled_runs,
                        usage_summary(&usage_history),
                    )
                    .await;
                }
                DoctorCommand::Json => {
                    print_doctor_json(
                        &handle,
                        &provider,
                        &model,
                        mode.get(),
                        output_style.as_deref(),
                        &cfg,
                        &approvals,
                        &scheduled_runs,
                        usage_summary(&usage_history),
                    )
                    .await;
                }
                DoctorCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", doctor_usage_text());
                }
            }
            continue;
        }
        if let Some(rest) = abort_command_arg(trimmed) {
            match parse_abort_command(rest) {
                AbortCommand::Status => println!("{}", abort_status_message()),
                AbortCommand::Json => print_abort_json(rest),
                AbortCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", abort_usage_text());
                }
            }
            continue;
        }
        if let Some(rest) = help_command_arg(trimmed) {
            match parse_help_command(rest) {
                HelpCommand::Show => print_help(),
                HelpCommand::Json => print_help_json(rest),
                HelpCommand::Usage => println!("{DIM}  usage:{RESET} {}", help_usage_text()),
            }
            continue;
        }
        if let Some(rest) = forget_command_arg(trimmed) {
            match parse_forget_command(rest) {
                ForgetCommand::Status => print_forget_status(&approvals),
                ForgetCommand::Json => print_forget_json(&approvals, rest),
                ForgetCommand::Usage => println!("{DIM}  usage:{RESET} {}", forget_usage_text()),
            }
            continue;
        }
        if let Some((command, rest)) = exit_command_arg(trimmed) {
            match parse_exit_command(rest) {
                ExitCommand::Status => print_exit_status(command),
                ExitCommand::Json => print_exit_json(command, rest),
                ExitCommand::Usage => println!("{DIM}  usage:{RESET} {}", exit_usage_text(command)),
            }
            continue;
        }
        if let Some(rest) = compact_preview_arg(trimmed) {
            match parse_compact_preview_command(rest) {
                CompactPreviewCommand::Status => print_compact_status(&cfg),
                CompactPreviewCommand::Json => print_compact_json(&cfg, rest),
                CompactPreviewCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", compact_usage_text())
                }
            }
            continue;
        }
        if let Some(rest) = resume_preview_arg(trimmed) {
            match parse_resume_preview_command(rest) {
                ResumePreviewCommand::Status => {
                    if let Err(e) = print_resume_status() {
                        eprintln!("{DIM}  /resume status: {e:#}{RESET}");
                    }
                }
                ResumePreviewCommand::Json => {
                    if let Err(e) = print_resume_json(rest) {
                        eprintln!("{DIM}  /resume json: {e:#}{RESET}");
                    }
                }
                ResumePreviewCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", resume_usage_text())
                }
            }
            continue;
        }
        let mut content_override: Option<Vec<ContentBlock>> = None;
        let mut slash_prompt_handled = false;
        match trimmed {
            "/exit" | "/quit" => {
                println!("{DIM}goodbye.{RESET}");
                return Ok(());
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
                print_permissions_status(mode.get(), &approvals);
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
            "/config" | "/settings" => {
                print_config_status(&cfg);
                continue;
            }
            "/mcp" => {
                print_mcp_status("status", McpCommand::Status);
                continue;
            }
            "/statusline" | "/status-line" => {
                print_status_line_status(&cfg);
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
        if let Some((command, rest)) = clear_command_arg(trimmed) {
            match parse_clear_command(rest) {
                ClearCommand::Status => print_clear_status(command, &provider, &model, mode.get()),
                ClearCommand::Json => {
                    print_clear_json(command, &provider, &model, mode.get(), rest)
                }
                ClearCommand::Usage => {
                    println!("{DIM}  usage:{RESET} {}", clear_usage_text(command));
                }
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
            if let Some(limit_input) = history_json_request_arg(rest) {
                match parse_history_limit(&limit_input) {
                    Ok(limit) => print_history_json(&history, limit, rest),
                    Err(e) => eprintln!("{DIM}  /history: {e:#}{RESET}"),
                }
            } else {
                match parse_history_limit(rest) {
                    Ok(limit) => print_history(&history, limit),
                    Err(e) => eprintln!("{DIM}  /history: {e:#}{RESET}"),
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/tree ") {
            if let Some(path_input) = tree_json_request_arg(rest) {
                let path = if path_input.is_empty() {
                    None
                } else {
                    Some(path_input.as_str())
                };
                print_project_tree_json(path, rest);
            } else {
                print_project_tree(Some(rest.trim()));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/changelog ") {
            if let Some(limit_input) = changelog_json_request_arg(rest) {
                match parse_changelog_limit(&limit_input) {
                    Ok(limit) => print_changelog_json(limit, rest),
                    Err(e) => eprintln!("{DIM}  /changelog: {e:#}{RESET}"),
                }
            } else {
                match parse_changelog_limit(rest) {
                    Ok(limit) => print_changelog(limit),
                    Err(e) => eprintln!("{DIM}  /changelog: {e:#}{RESET}"),
                }
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
            if let Some(json_input) = loop_json_request_arg(rest) {
                print_loop_json(json_input);
                continue;
            }
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
                AutoCommand::Json => print_auto_json(auto_run.as_ref(), rest),
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
                ScheduleCommand::Json => print_schedule_json(&scheduled_runs, rest, None),
                ScheduleCommand::Show(id) => print_schedule_details(&scheduled_runs, &id),
                ScheduleCommand::ShowJson(id) => print_schedule_json(&scheduled_runs, rest, Some(&id)),
                ScheduleCommand::Run(id) => {
                    let now = Instant::now();
                    if let Some(run) = scheduled_runs.iter_mut().find(|run| run.id == id) {
                        run.due_at = now;
                        run.due_epoch_ms = now_epoch_ms();
                        scheduled_runs.sort_by_key(|run| run.due_at);
                        if let Err(err) = persist_scheduled_runs_if_configured(
                            schedule_store_path.as_deref(),
                            &scheduled_runs,
                        ) {
                            eprintln!(
                                "{DIM}  /schedule: could not save scheduled prompts: {err}.{RESET}"
                            );
                        }
                        println!("{DIM}  /schedule: queued {id} to run now.{RESET}");
                    } else {
                        println!("{DIM}  /schedule: no scheduled prompt found for {id}.{RESET}");
                    }
                }
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
                    eprintln!(
                        "{DIM}  usage: /schedule in 10m follow up, /schedule list|status|state|json|--json|list --json, /schedule show|inspect <id> [--json], /schedule show-json <id>, /schedule run|now|trigger <id>, /schedule cancel|delete|rm <id>, or /schedule clear|stop (also /cron){RESET}"
                    );
                }
            }
            continue;
        }
        if let Some(rest) = thinking_command_arg(trimmed) {
            if is_thinking_json_arg(rest) {
                print_thinking_json(&handle, rest);
                continue;
            }
            if is_thinking_status_arg(rest) {
                print_thinking_status(&handle);
                continue;
            }
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
        if let Some((command, rest)) = mode_command_arg(trimmed) {
            match parse_permissions_command(rest) {
                PermissionsCommand::Show => print_permissions_status(mode.get(), &approvals),
                PermissionsCommand::Json if command == "/mode" => print_mode_json(mode.get(), rest),
                PermissionsCommand::Json => print_permissions_json(mode.get(), &approvals, rest),
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
        if let Some(rest) = trimmed.strip_prefix("/plan ") {
            match parse_plan_command(rest) {
                PlanCommand::Status => print_plan_status(mode.get()),
                PlanCommand::On => {
                    mode.set(Mode::Plan);
                    announce_mode_change(Mode::Plan);
                }
                PlanCommand::Off => {
                    mode.set(Mode::Normal);
                    announce_mode_change(Mode::Normal);
                }
                PlanCommand::Usage => eprintln!("{DIM}  usage: /plan [on|off|status]{RESET}"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/model ") {
            let model_command = parse_model_slash_command(rest);
            match model_command {
                ModelSlashCommand::Status => {
                    print_model_status(&handle, &cfg, &scoped_model_patterns)
                }
                ModelSlashCommand::Json => {
                    print_model_json(&handle, &cfg, &scoped_model_patterns, rest, false)
                }
                ModelSlashCommand::JsonList => {
                    print_model_json(&handle, &cfg, &scoped_model_patterns, rest, true)
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
            match parse_name_command(rest) {
                NameCommand::Status => {
                    print_name_status(session_name.as_deref());
                    continue;
                }
                NameCommand::Json => {
                    print_name_json(session_name.as_deref(), rest);
                    continue;
                }
                NameCommand::Set => {}
            }
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
            let rest = rest.trim();
            if is_export_json_arg(rest) {
                print_export_json(&handle, rest).await;
            } else {
                export_transcript(&handle, Some(rest)).await;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/share ") {
            let rest = rest.trim();
            if is_share_json_arg(rest) {
                print_share_json(&handle, rest).await;
            } else {
                share_transcript(&handle, Some(rest)).await;
            }
            continue;
        }
        if let Some(rest) = onboarding_command_arg(trimmed) {
            if is_onboarding_json_arg(rest) {
                print_onboarding_json(rest);
            } else if is_onboarding_preview_arg(rest) {
                print_onboarding_preview();
            } else {
                write_onboarding_guide(Some(rest));
            }
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
                        "{DIM}  usage: /agent [--worktree|--same-cwd|--background|--detached] <name> <task>{RESET}"
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
                    let rest = rest.trim();
                    if is_template_json_arg(rest) {
                        print_templates_json(rest);
                        continue;
                    }
                    if is_template_list_arg(rest) {
                        print_templates();
                        continue;
                    }
                    match build_template_slash_prompt(rest, &handle).await {
                        Ok(prompt) => {
                            line = prompt;
                        }
                        Err(e) => {
                            eprintln!("{DIM}  /template: {e:#}{RESET}");
                            continue;
                        }
                    }
                } else if let Some((name, args)) = parse_direct_custom_slash(trimmed) {
                    match build_custom_slash_prompt(name, args, &handle).await {
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
            } else if is_init_json_arg(notes) {
                print_init_project_json(None);
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
            if let Some(note) = remember_json_note_arg(text) {
                let cwd = match std::env::current_dir() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("{DIM}  /remember json: could not resolve cwd: {e}{RESET}");
                        continue;
                    }
                };
                print_remember_json(&cwd, note);
                continue;
            }
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
                    if let Some(context) =
                        run_shell_escape(&command, bash_command_wrapper.as_deref())
                    {
                        pending_shell_context.push(context);
                        if pending_shell_context.len() > SHELL_ESCAPE_CONTEXT_LIMIT {
                            pending_shell_context.remove(0);
                        }
                    }
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
            let agent_line = apply_pending_shell_context(&pending_shell_context, &agent_line);
            match crate::commands::code_hooks::run_user_prompt_submit_hooks(
                cfg.as_ref(),
                &agent_line,
            ) {
                Ok(agent_line) => {
                    pending_shell_context.clear();
                    handle.prompt_with_abort(agent_line, abort_signal, render).await
                }
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum InitFromAgentAction {
    Preview,
    Json,
    PreviewApply(&'static str),
    Append,
    Merge,
    MergeLines,
    Replace,
    PreviewSections(Vec<usize>),
    PreviewApplySections(&'static str, Vec<usize>),
    AppendSections(Vec<usize>),
    MergeSections(Vec<usize>),
    MergeLineSections(Vec<usize>),
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
    let rest = rest.trim();
    if let Some((mode, indexes)) = parse_init_from_agent_sections(rest) {
        return match mode {
            "preview" => Some(InitFromAgentAction::PreviewSections(indexes)),
            "preview-append" => Some(InitFromAgentAction::PreviewApplySections("append", indexes)),
            "preview-merge" => Some(InitFromAgentAction::PreviewApplySections("merge", indexes)),
            "preview-merge-lines" => {
                Some(InitFromAgentAction::PreviewApplySections("merge-lines", indexes))
            }
            "append" => Some(InitFromAgentAction::AppendSections(indexes)),
            "merge" => Some(InitFromAgentAction::MergeSections(indexes)),
            "merge-lines" => Some(InitFromAgentAction::MergeLineSections(indexes)),
            _ => None,
        };
    }
    match rest {
        "" | "preview" | "show" => Some(InitFromAgentAction::Preview),
        "json" | "--json" | "status --json" | "preview --json" | "show --json" => {
            Some(InitFromAgentAction::Json)
        }
        "preview append" | "show append" => Some(InitFromAgentAction::PreviewApply("append")),
        "preview merge" | "show merge" => Some(InitFromAgentAction::PreviewApply("merge")),
        "preview merge-lines" | "preview line-merge" | "preview lines" => {
            Some(InitFromAgentAction::PreviewApply("merge-lines"))
        }
        "show merge-lines" | "show line-merge" | "show lines" => {
            Some(InitFromAgentAction::PreviewApply("merge-lines"))
        }
        "preview replace" | "show replace" => Some(InitFromAgentAction::PreviewApply("replace")),
        "append" => Some(InitFromAgentAction::Append),
        "merge" | "apply" => Some(InitFromAgentAction::Merge),
        "merge-lines" | "line-merge" | "lines" => Some(InitFromAgentAction::MergeLines),
        "replace" => Some(InitFromAgentAction::Replace),
        _ => None,
    }
}

fn parse_init_from_agent_sections(input: &str) -> Option<(&'static str, Vec<usize>)> {
    let (mode, rest) = if let Some(rest) = input.strip_prefix("sections ") {
        ("preview", rest)
    } else if let Some(rest) = input.strip_prefix("preview sections ") {
        ("preview", rest)
    } else if let Some(rest) = input
        .strip_prefix("preview append sections ")
        .or_else(|| input.strip_prefix("show append sections "))
    {
        ("preview-append", rest)
    } else if let Some(rest) = input
        .strip_prefix("preview merge sections ")
        .or_else(|| input.strip_prefix("show merge sections "))
    {
        ("preview-merge", rest)
    } else if let Some(rest) = input
        .strip_prefix("preview merge-lines sections ")
        .or_else(|| input.strip_prefix("preview line-merge sections "))
        .or_else(|| input.strip_prefix("preview lines sections "))
        .or_else(|| input.strip_prefix("show merge-lines sections "))
        .or_else(|| input.strip_prefix("show line-merge sections "))
        .or_else(|| input.strip_prefix("show lines sections "))
    {
        ("preview-merge-lines", rest)
    } else if let Some(rest) = input.strip_prefix("append sections ") {
        ("append", rest)
    } else if let Some(rest) = input.strip_prefix("merge sections ") {
        ("merge", rest)
    } else if let Some(rest) = input
        .strip_prefix("merge-lines sections ")
        .or_else(|| input.strip_prefix("line-merge sections "))
        .or_else(|| input.strip_prefix("lines sections "))
    {
        ("merge-lines", rest)
    } else {
        return None;
    };
    parse_init_section_indexes(rest).map(|indexes| (mode, indexes))
}

fn parse_init_section_indexes(input: &str) -> Option<Vec<usize>> {
    let normalized = input.trim().to_ascii_lowercase();
    if normalized == "all" {
        return Some(vec![0]);
    }
    let mut indexes = Vec::new();
    for part in normalized
        .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if part == "all" {
            return None;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = start.parse::<usize>().ok()?;
            let end = end.parse::<usize>().ok()?;
            if start == 0 || end == 0 || start > end {
                return None;
            }
            for index in start..=end {
                if indexes.contains(&index) {
                    return None;
                }
                indexes.push(index);
            }
        } else {
            let index = part.parse::<usize>().ok()?;
            if index == 0 || indexes.contains(&index) {
                return None;
            }
            indexes.push(index);
        }
    }
    (!indexes.is_empty()).then_some(indexes)
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
    // Git context is injected once by pi (build_git_context); do not duplicate it here.
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
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let sessions = list_past_sessions(Some(&cwd))?;
    resolve_repl_resume_path_from_sessions(raw, &sessions)
}

fn resolve_repl_resume_path_from_sessions(
    raw: &str,
    sessions: &[crate::commands::code_session::SessionMeta],
) -> Result<PathBuf> {
    if raw.is_empty() {
        let recent = sessions
            .first()
            .ok_or_else(|| anyhow::anyhow!("no past sessions for this project"))?;
        return Ok(PathBuf::from(recent.path.clone()));
    }
    let path = PathBuf::from(raw);
    if path.exists() {
        return Ok(path);
    }
    if let Some(session) = sessions.iter().find(|session| {
        session.id == raw
            || session.name.as_deref() == Some(raw)
            || Path::new(&session.path)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(raw)
    }) {
        return Ok(PathBuf::from(&session.path));
    }
    anyhow::bail!(
        "session target not found: {raw} (expected saved session id, name, filename, or path)"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumePreviewCommand {
    Status,
    Json,
    Usage,
}

fn resume_usage_text() -> &'static str {
    "/resume [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json|session|path]"
}

fn resume_preview_arg(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("/resume ")?.trim();
    match normalize_help_command_arg(rest).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" | "json" | "--json"
        | "status --json" | "state --json" | "show --json" | "info --json"
        | "preview --json" | "help" | "usage" => Some(rest),
        _ => None,
    }
}

fn parse_resume_preview_command(input: &str) -> ResumePreviewCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => ResumePreviewCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "preview --json" => ResumePreviewCommand::Json,
        _ => ResumePreviewCommand::Usage,
    }
}

fn resume_session_rows(cwd: &Path) -> Result<Vec<serde_json::Value>> {
    let sessions = list_past_sessions(Some(cwd))?;
    Ok(sessions
        .into_iter()
        .map(|session| {
            json!({
                "id": session.id,
                "name": session.name,
                "path": session.path,
                "cwd": session.cwd,
                "timestamp": session.timestamp,
                "message_count": session.message_count,
                "last_modified_ms": session.last_modified_ms,
                "size_bytes": session.size_bytes,
            })
        })
        .collect())
}

fn resume_json_payload_from_rows(
    cwd: &Path,
    query: &str,
    sessions: Vec<serde_json::Value>,
) -> serde_json::Value {
    let default_target = sessions.first().cloned();
    json!({
        "surface": "terminal",
        "command": "resume",
        "query": query.trim(),
        "cwd": cwd.display().to_string(),
        "available": !sessions.is_empty(),
        "candidate_count": sessions.len(),
        "default_target": default_target,
        "candidates": sessions,
        "will_replace_current_repl_session": true,
        "accepts_path": true,
        "query_argument": "/resume SESSION",
        "path_argument": "/resume PATH",
        "aliases": ["resume"],
        "supported_actions": ["status", "state", "show", "info", "preview", "json", "--json", "status --json", "state --json", "show --json", "info --json", "preview --json", "session", "path"],
    })
}

fn resume_json_payload(query: &str) -> Result<serde_json::Value> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let rows = resume_session_rows(&cwd)?;
    Ok(resume_json_payload_from_rows(&cwd, query, rows))
}

fn print_resume_status() -> Result<()> {
    let payload = resume_json_payload("status")?;
    let count = payload["candidate_count"].as_u64().unwrap_or(0);
    let default_path = payload["default_target"]["path"].as_str().unwrap_or("(none)");
    println!(
        "{DIM}  /resume: {count} saved session(s) for this cwd. Running `/resume` resumes the most recent target: {default_path}.{RESET}"
    );
    Ok(())
}

fn print_resume_json(query: &str) -> Result<()> {
    match serde_json::to_string_pretty(&resume_json_payload(query)?) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /resume json failed: {e}{RESET}"),
    }
    Ok(())
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
    if matches!(input, "/thinking" | "/think" | "/t") {
        return Some("");
    }
    for prefix in ["/thinking ", "/think ", "/t "] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn is_thinking_status_arg(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "" | "status" | "show" | "current" | "info"
    )
}

fn is_thinking_json_arg(input: &str) -> bool {
    matches!(
        normalize_help_command_arg(input).as_str(),
        "json" | "--json" | "status --json" | "show --json" | "current --json"
            | "info --json"
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameCommand {
    Status,
    Json,
    Set,
}

fn parse_name_command(input: &str) -> NameCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "status" | "state" | "show" | "current" | "info" => NameCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "current --json" | "info --json" => NameCommand::Json,
        _ => NameCommand::Set,
    }
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
        anyhow::bail!("usage: /thinking [status|show|current|info|json|--json|status --json|show --json|current --json|info --json|off|minimal|low|medium|high|xhigh]");
    }
    raw.parse::<ThinkingLevel>()
        .map_err(|_| anyhow::anyhow!("unknown thinking level `{raw}`"))
}

fn print_thinking_status(handle: &AgentSessionHandle) {
    let current = handle.thinking_level().unwrap_or_default();
    println!("{BOLD}thinking{RESET}");
    println!("{DIM}  current:{RESET} {current}");
    println!("{DIM}  supported:{RESET} off, minimal, low, medium, high, xhigh");
    println!("{DIM}  usage:{RESET} /thinking [status|show|current|info|json|--json|status --json|show --json|current --json|info --json|level] (also /think or /t)");
}

fn thinking_json_payload(level: ThinkingLevel, query: &str) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": "thinking",
        "query": query.trim(),
        "aliases": ["think", "t"],
        "current": level.to_string(),
        "supported_levels": ["off", "minimal", "low", "medium", "high", "xhigh"],
        "will_change": false,
        "supported_actions": ["status", "show", "current", "info", "json", "--json", "status --json", "show --json", "current --json", "info --json", "off", "minimal", "low", "medium", "high", "xhigh", "set"],
    })
}

fn print_thinking_json(handle: &AgentSessionHandle, query: &str) {
    let current = handle.thinking_level().unwrap_or_default();
    match serde_json::to_string_pretty(&thinking_json_payload(current, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /thinking json failed: {e}{RESET}"),
    }
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
    println!("{DIM}  {}{RESET}", permissions_usage_text());
    println!("{DIM}  {} — alias for /permissions{RESET}", mode_usage_text());
    println!("{DIM}  {}{RESET}", model_usage_text());
    println!("{DIM}  {} — set/show this session's display name (also /rename){RESET}", name_usage_text());
    println!("{DIM}  {} — show current REPL session status{RESET}", status_usage_text());
    println!("{DIM}  {} — run a local session/config diagnostic report{RESET}", doctor_usage_text());
    println!("{DIM}  /abort [status|cancel|stop|interrupt] — show how to interrupt the active CLI turn{RESET}");
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
    println!(
        "{DIM}  /pr_comments drafts submit [approve|comment|request_changes] [body] — publish queued draft threads, optionally with a review event{RESET}"
    );
    println!("{DIM}  {} — inspect the bash sandbox profile{RESET}", sandbox_usage_text());
    println!("{DIM}  {} — show token usage for this REPL session{RESET}", usage_slash_usage_text());
    println!("{DIM}  {} — show recent submitted prompts{RESET}", history_usage_text());
    println!("{DIM}  {} — copy the last assistant response to the terminal clipboard{RESET}", copy_usage_text());
    println!("{DIM}  /config [status|show|current|info|path|open|backends|defaults|agents|skills|hooks|mcp|approvals|appearance|sandbox|advanced|set <key> <value>|unset <key>] — show or update active config{RESET}");
    println!("{DIM}  /hooks    — show configured command hooks (/hook is accepted too){RESET}");
    println!("{DIM}  /mcp      — show terminal MCP support status{RESET}");
    println!("{DIM}  {} — customize the input-bar status line{RESET}", status_line_usage_text());
    println!("{DIM}  {} — show input bar keyboard controls{RESET}", hotkeys_usage_text());
    println!("{DIM}  {} — show a bounded project tree{RESET}", tree_usage_text());
    println!("{DIM}  {} — show recent git commits{RESET}", changelog_usage_text());
    println!("{DIM}  /reload [config|session|now|fresh] — reload config and start a fresh agent session{RESET}");
    println!("{DIM}  /resume [session|path] — resume the latest or specified saved session{RESET}");
    println!("{DIM}  /fork [list|index|id] — fork from a previous user message{RESET}");
    println!("{DIM}  /thinking [off|minimal|low|medium|high|xhigh] — show or set thinking{RESET}");
    println!("{DIM}  {}{RESET}", scoped_models_usage_text());
    println!("{DIM}  /compact — compact older conversation history now{RESET}");
    println!("{DIM}  /loop [turns] [goal]|json [turns] [goal]|--json [turns] [goal]|status --json — run bounded autonomous follow-up turns{RESET}");
    println!("{DIM}  /auto on [turns] [goal] — bounded continuous execution (/auto off|stop|cancel|status|state|json|--json|status --json|state --json; also /autorun, /continuous){RESET}");
    println!("{DIM}  /schedule in <delay> <prompt> — queue a due follow-up prompt (/schedule list|status|state|json|--json|list --json|show|inspect|show-json|run|now|trigger|cancel|delete|rm|clear|stop; also /cron){RESET}");
    println!("{DIM}  /send [target message] — show terminal inter-session send status{RESET}");
    println!("{DIM}  /notify on|enable|enabled|off|disable|disabled|clear|status|state|show|test|ping — turn-complete terminal notifications{RESET}");
    println!("{DIM}  /image <path> [prompt] — attach a local image to the next prompt{RESET}");
    println!("{DIM}  /attach <path> [prompt] — alias for /image{RESET}");
    println!("{DIM}  /mention <path> [prompt] — attach a local text file to the next prompt{RESET}");
    println!("{DIM}  {} — inspect auth or run libertai login{RESET}", login_usage_text());
    println!("{DIM}  {} — run libertai logout or explain provider logout{RESET}", logout_usage_text());
    println!("{DIM}  /memory   — show project memory (/memory open|edit|clear|files|references|import <path>|import-claude|import-claude-all|path){RESET}");
    println!("{DIM}  /skills [list|status|show|json|--json|status --json|list --json|show --json|show <name>|show <name> --json|open|settings|edit|enable|on <name>|disable|off <name>] — manage code-agent skills for new sessions{RESET}");
    println!(
        "{DIM}  /init [--agent|from-agent json|from-agent preview append|preview merge|preview merge-lines|preview replace|preview [append|merge|merge-lines] sections N[,M]|N-M|all|append sections N[,M]|N-M|all|merge sections N[,M]|N-M|all|merge-lines sections N[,M]|N-M|all|append|merge-lines|merge|replace] [notes] — create or merge AGENTS.md guidance{RESET}"
    );
    println!("{DIM}  /onboarding|/onboard [show|preview|save|path|gist|json|--json|status --json|show --json|preview --json] — preview or write a local project onboarding guide{RESET}");
    println!("{DIM}  /onboarding gist [public|secret] [filename.md] — publish the onboarding guide with gh{RESET}");
    println!("{DIM}  /agents [list|status|show <name>|json|--json|list --json|status --json|show --json|show <name> --json|open|settings|edit|background|bg|create [--worktree|--same-cwd] <name>|delete|remove <name>] — list or inspect named sub-agents{RESET}");
    println!("{DIM}  /agents create [--worktree|--same-cwd] <name> [description] — create a project sub-agent{RESET}");
    println!("{DIM}  /agents delete <name> — delete the active named sub-agent definition{RESET}");
    println!(
        "{DIM}  /agent [--worktree|--same-cwd|--background|--detached] <name> <task> — run a named sub-agent task{RESET}"
    );
    println!("{DIM}  /agent --background|--detached <name> <task> — start a detached terminal agent and write a log under ~/.config/libertai/code-background-agents{RESET}");
    println!("{DIM}  /agents background [list|show|log|kill|prune|clear] — inspect, stop, or prune terminal background agents{RESET}");
    println!("{DIM}  /template <name> [args] — expand a prompt template{RESET}");
    println!("{DIM}  /theme [status|show|current|system|dark|light|high-contrast] — show terminal theme status{RESET}");
    println!("{DIM}  /export [path] — write this session transcript as Markdown{RESET}");
    println!("{DIM}  /share [path] — write this session transcript as shareable HTML{RESET}");
    println!("{DIM}  /share gist [public|secret] [filename.html] — publish the HTML transcript with gh{RESET}");
    println!("{DIM}  {}{RESET}", output_style_usage_text());
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpCommand {
    Show,
    Json,
    Usage,
}

fn help_usage_text() -> &'static str {
    "/help [status|show|list|commands|json|--json|status --json|show --json|list --json|commands --json]"
}

fn help_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/help" => Some(""),
        _ => trimmed.strip_prefix("/help ").map(str::trim),
    }
}

fn parse_help_command(input: &str) -> HelpCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "show" | "list" | "commands" => HelpCommand::Show,
        "json" | "--json" | "status --json" | "show --json" | "list --json" | "commands --json" => {
            HelpCommand::Json
        }
        _ => HelpCommand::Usage,
    }
}

fn normalize_help_command_arg(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn help_command_rows() -> &'static [(&'static str, &'static [&'static str], &'static str)] {
    &[
        ("abort", &[], "show or interrupt active-turn controls"),
        ("agent", &[], "run a named sub-agent task"),
        ("agents", &[], "manage named and background sub-agents"),
        ("attach", &[], "attach a local file to the next prompt"),
        ("auto", &["autorun", "continuous"], "run bounded continuous execution"),
        ("bug", &[], "print a bug report diagnostic template"),
        ("changelog", &[], "show recent git commits"),
        ("clear", &["new"], "wipe the screen and start a fresh session"),
        ("compact", &[], "compact older conversation history"),
        ("config", &["settings"], "show or update active config"),
        ("copy", &[], "copy the last assistant response"),
        ("doctor", &[], "run local session/config diagnostics"),
        ("exit", &["quit"], "quit the REPL"),
        ("export", &[], "write the transcript as Markdown"),
        ("forget", &[], "clear saved allow rules"),
        ("fork", &[], "fork from a previous user message"),
        ("help", &[], "show slash commands"),
        ("history", &[], "show recent submitted prompts"),
        ("hooks", &["hook"], "show configured command hooks"),
        ("hotkeys", &[], "show input bar keyboard controls"),
        ("ide", &[], "show IDE integration status"),
        ("image", &[], "attach a local image to the next prompt"),
        ("init", &[], "create or merge AGENTS.md guidance"),
        ("login", &[], "inspect auth or run libertai login"),
        ("logout", &[], "run libertai logout or explain provider logout"),
        ("loop", &["autoloop"], "run bounded autonomous follow-up turns"),
        ("mcp", &[], "show terminal MCP support status"),
        ("memory", &[], "show or update project memory"),
        ("mention", &[], "attach a local text file to the next prompt"),
        ("mode", &[], "show or change permission mode"),
        ("model", &[], "show or change the active model"),
        ("name", &["rename"], "show or set this session's display name"),
        ("notify", &["notifications"], "configure turn-complete notifications"),
        ("onboarding", &["onboard"], "write or publish a project onboarding guide"),
        ("output-style", &[], "show or change output style"),
        ("permissions", &[], "show or change permission mode"),
        ("plan", &[], "toggle plan mode"),
        ("pr_comments", &[], "inspect or update GitHub PR review comments"),
        ("reload", &[], "reload config and start a fresh agent session"),
        ("remember", &[], "append typed project memory"),
        ("resume", &[], "resume a saved session"),
        ("review", &[], "ask the agent to review current code changes"),
        ("sandbox", &[], "inspect the bash sandbox profile"),
        ("schedule", &["cron"], "queue a due follow-up prompt"),
        ("scoped-models", &["scoped"], "filter model list and cycling"),
        ("security-review", &[], "ask for a focused security review"),
        ("send", &["send-message"], "show terminal inter-session send status"),
        ("share", &[], "write or publish shareable HTML transcript"),
        ("skills", &[], "manage code-agent skills"),
        ("status", &[], "show current REPL session status"),
        ("statusline", &["status-line"], "customize the input-bar status line"),
        ("template", &[], "expand a prompt template"),
        ("theme", &[], "show terminal theme status"),
        ("thinking", &["think", "t"], "show or set thinking budget"),
        ("tree", &[], "show a bounded project tree"),
        ("usage", &["cost"], "show token usage for this REPL session"),
        ("vim", &[], "show Vim-input status"),
    ]
}

fn help_json_payload(query: &str) -> serde_json::Value {
    let commands: Vec<serde_json::Value> = help_command_rows()
        .iter()
        .map(|(name, aliases, description)| {
            json!({
                "name": name,
                "aliases": aliases,
                "description": description,
                "arg_hint": help_command_arg_hint(name),
            })
        })
        .collect();
    json!({
        "surface": "terminal",
        "command": "help",
        "aliases": ["help"],
        "query": query.trim(),
        "commands": commands,
        "supported_actions": ["status", "show", "list", "commands", "json", "--json", "status --json", "show --json", "list --json", "commands --json"],
    })
}

fn help_command_arg_hint(command: &str) -> &'static str {
    match command {
        "abort" => "status|state|show|info|json|--json|status --json|state --json|show --json|info --json|cancel|stop|interrupt",
        "agent" => "[--worktree|--same-cwd|--background|--detached] <name> <task>",
        "agents" => "list|status|show <name>|json|--json|list --json|status --json|show --json|show <name> --json|open|settings|edit|background|bg|create [--worktree|--same-cwd] <name>|delete|remove <name>",
        "attach" | "image" => "<path> [prompt]",
        "auto" => "on [turns] [goal]|off|stop|cancel|status|state|json|--json|status --json|state --json|status-json|state-json",
        "bug" => "report|template|status|show|json|--json|status --json|show --json|template --json|report --json",
        "changelog" | "history" => "count|list|recent|latest|status|state|show|json|--json|status --json|state --json|show --json|list --json|recent --json|latest --json",
        "clear" | "exit" | "forget" => "status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json",
        "compact" => "status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json|[notes]",
        "config" => "status|show|current|info|json|--json|status --json|show --json|current --json|info --json|path|open|settings|backends|defaults|agents|skills|hooks|mcp|approvals|appearance|sandbox|advanced|set <key> <value>|unset <key>|reset <key>",
        "copy" => "status|show|info|json|--json|status --json|show --json|info --json|last|latest|response|assistant|assistant-response",
        "doctor" => "status|state|show|info|health|diagnostics|diag|json|--json|status --json|state --json|show --json|info --json|health --json|diagnostics --json|diag --json",
        "export" => "copy|save|path|json|--json|status --json|show --json|preview --json|[path]",
        "fork" => "list|index|id",
        "help" => "status|show|list|commands|json|--json|status --json|show --json|list --json|commands --json",
        "hooks" => "status|list|state|diagnostics|diag|json|--json|status --json|list --json|state --json|diagnostics --json|diag --json|show --json|show|event|inspect <event>|open|settings|edit",
        "mcp" => "status|list|state|show|json|--json|status --json|list --json|state --json|diagnostics --json|diag --json|show --json|server|inspect <server>|probe|probes|probe --save|probe save|probe --write|probe write|refresh|diagnostics|diag|reset|reset-sessions|open|settings|edit",
        "hotkeys" => "status|show|list|help|json|--json|status --json|show --json|list --json",
        "ide" => "status|state|show|json|--json|status --json|state --json|show --json|open|settings|edit",
        "init" => "--agent|json|--json|status --json|show --json|preview --json|from-agent|from-agent json|from-agent status --json|from-agent preview append|from-agent preview merge|from-agent preview merge-lines|from-agent preview replace|from-agent append|from-agent merge|from-agent merge-lines|from-agent replace|from-agent preview sections N[,M]|N-M|all|from-agent preview append sections N[,M]|from-agent preview merge sections N[,M]|from-agent preview merge-lines sections N[,M]|from-agent append sections N[,M]|from-agent merge sections N[,M]|from-agent merge-lines sections N[,M]|[notes]",
        "login" | "logout" => "status|show|info|json|--json|status --json|show --json|info --json|libertai|account|key|api-key|api|provider|show <provider>|show <provider> --json|info <provider>|info <provider> --json|inspect <provider>|inspect <provider> --json|provider <provider>|provider <provider> --json|<provider> --json",
        "loop" => "[turns] [goal]|json [turns] [goal]|--json [turns] [goal]|status --json",
        "memory" => "show|status|edit|open|list|files|file <number|path>|read <number|path>|show-file <number|path>|references|refs|verify|import <path>|import-claude|migrate-claude|claude|import-claude-all|migrate-claude-all|claude-all|clear|path|json|--json|status --json|show --json",
        "mention" => "<path> [prompt]",
        "mode" => "status|show|current|info|json|--json|status --json|show --json|current --json|info --json|default|normal|acceptEdits|accept-edits|accept_edits|plan|readonly|read-only",
        "model" => "status|show|current|json|--json|status --json|show --json|current --json|list|ls|list --json|ls --json|next|cycle|prev|previous|back|model|provider/model",
        "name" => "<name>|status|state|show|current|info|json|--json|status --json|state --json|show --json|current --json|info --json",
        "notify" => "on|enable|enabled|off|disable|disabled|clear|status|state|show|json|--json|status --json|state --json|show --json|test|ping",
        "onboarding" => "show|preview|save|path|gist|json|--json|status --json|show --json|preview --json",
        "output-style" => "style|status|show|current|info|list|json|--json|status --json|show --json|current --json|info --json|list --json",
        "permissions" => "status|show|current|info|json|--json|status --json|show --json|current --json|info --json|default|normal|acceptEdits|accept-edits|accept_edits|plan|readonly|read-only|open|settings|edit|approvals|forget|clear|reset|bypassPermissions|bypass|danger",
        "plan" => "on|off|status",
        "pr_comments" => "scope|send|resolve <thread_id>|unresolve <thread_id>|reopen <thread_id>|viewed <path>|view <path>|viewed --all|unviewed <path>|unview <path>|unviewed --all|thread <path>:<line> <body>|comment <path>:<line> <body>|draft <path>:<line> <body>|drafts|drafts submit|drafts submit comment <body>|drafts submit request_changes <body>|drafts submit approve [body]|drafts clear|reply <thread_id> <body>|edit <comment_id> <body>|review <approve|comment|request_changes> [body]|submit <approve|comment|request_changes> [body]",
        "review" | "security-review" => "[scope]",
        "reload" => "config|session|now|fresh|json|--json|config --json|session --json|now --json|fresh --json",
        "remember" => "project: <text>|user: <text>|feedback: <text>|reference: <text>|json <text>|--json <text>|<text> --json|status --json|show --json|preview --json",
        "resume" => "status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json|session|path",
        "sandbox" => "info|status|state|show|diagnostics|diag|json|--json|status --json|state --json|show --json|info --json|diagnostics --json|diag --json|reload",
        "schedule" => "in <delay> <prompt>|list|status|state|json|--json|list --json|show|inspect|show-json|run|now|trigger|cancel|delete|rm|clear|stop",
        "scoped-models" => "status|show|json|--json|status --json|show --json|patterns|clear|reset|off",
        "send" => "status|targets|list|json|--json|status --json|state --json|show --json|list --json|targets --json|queued|queue --json|queued --json|pending --json|clear <id|target|all>|session message",
        "share" => "copy|save|path|gist|json|--json|status --json|show --json|preview --json|[path]",
        "skills" => "list|status|show|json|--json|status --json|list --json|show --json|show <name>|show <name> --json|open|settings|edit|enable|on <name>|disable|off <name>",
        "status" => "status|state|show|info|current|session|json|--json|status --json|state --json|show --json|info --json|current --json|session --json",
        "statusline" => "status|show|json|--json|status --json|show --json|template --json|info --json|template|command <shell>|command-clear|command reset|command clear|reset|clear",
        "template" => "list|show|json|--json|status --json|list --json|show --json|<name> [args]",
        "theme" => "status|show|current|info|json|--json|status --json|show --json|current --json|info --json|system|dark|light|high-contrast",
        "thinking" => "status|show|current|info|json|--json|status --json|show --json|current --json|info --json|off|minimal|low|medium|high|xhigh",
        "tree" => "path|json|--json|status --json|state --json|show --json|path --json",
        "usage" => "status|show|summary|tools|json|--json|status --json|show --json|summary --json|tools --json|csv|export|export json|export csv",
        "vim" => "status|state|show|current|info|json|--json|status --json|state --json|show --json|current --json|info --json|on|enable|enabled|true|off|disable|disabled|false",
        _ => "",
    }
}

fn print_help_json(query: &str) {
    match serde_json::to_string_pretty(&help_json_payload(query)) {
        Ok(raw) => println!("{raw}"),
        Err(err) => eprintln!("{DIM}  failed to render help JSON: {err}{RESET}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClearCommand {
    Status,
    Json,
    Usage,
}

fn clear_usage_text(command: &str) -> String {
    format!(
        "{command} [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json]"
    )
}

fn clear_command_arg(trimmed: &str) -> Option<(&'static str, &str)> {
    for command in ["/clear", "/new"] {
        if trimmed == command {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix(command) {
            if rest.starts_with(' ') {
                return Some((command, rest.trim()));
            }
        }
    }
    None
}

fn parse_clear_command(input: &str) -> ClearCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => ClearCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "preview --json" => ClearCommand::Json,
        _ => ClearCommand::Usage,
    }
}

fn clear_json_payload(
    command: &str,
    provider: &str,
    model: &str,
    mode: Mode,
    query: &str,
) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": command.trim_start_matches('/'),
        "aliases": ["clear", "new"],
        "query": query.trim(),
        "available": true,
        "active_turn": false,
        "will_clear_screen": true,
        "will_start_fresh_session": true,
        "will_preserve_mode": true,
        "current_provider": provider,
        "current_model": model,
        "current_mode": mode_label(mode),
        "supported_actions": ["status", "state", "show", "info", "preview", "json", "--json", "status --json", "state --json", "show --json", "info --json", "preview --json"],
    })
}

fn print_clear_status(command: &str, provider: &str, model: &str, mode: Mode) {
    println!(
        "{DIM}  {command}: ready. Running `{command}` with no arguments clears the screen and starts a fresh {provider}/{model} session in {} mode.{RESET}",
        mode_label(mode)
    );
}

fn print_clear_json(command: &str, provider: &str, model: &str, mode: Mode, query: &str) {
    match serde_json::to_string_pretty(&clear_json_payload(
        command, provider, model, mode, query,
    )) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  {command} json failed: {e}{RESET}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForgetCommand {
    Status,
    Json,
    Usage,
}

fn forget_usage_text() -> &'static str {
    "/forget [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json]"
}

fn forget_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/forget" => None,
        _ => trimmed.strip_prefix("/forget ").map(str::trim),
    }
}

fn parse_forget_command(input: &str) -> ForgetCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => ForgetCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "preview --json" => ForgetCommand::Json,
        _ => ForgetCommand::Usage,
    }
}

fn forget_json_payload(approvals: &ApprovalState, query: &str) -> serde_json::Value {
    let allow_rules_path = crate::config::allow_rules_path()
        .ok()
        .map(|path| path.display().to_string());
    json!({
        "surface": "terminal",
        "command": "forget",
        "aliases": ["forget"],
        "query": query.trim(),
        "available": true,
        "remembered_approvals": approvals.always_rules().len(),
        "will_clear_saved_allow_rules": true,
        "will_change_permission_mode": false,
        "will_change_read_only_auto_approvals": false,
        "allow_rules_path": allow_rules_path,
        "supported_actions": ["status", "state", "show", "info", "preview", "json", "--json", "status --json", "state --json", "show --json", "info --json", "preview --json"],
    })
}

fn print_forget_status(approvals: &ApprovalState) {
    println!(
        "{DIM}  /forget: ready. Running `/forget` with no arguments clears {} saved allow rule(s); read-only tools stay auto-approved.{RESET}",
        approvals.always_rules().len()
    );
}

fn print_forget_json(approvals: &ApprovalState, query: &str) {
    match serde_json::to_string_pretty(&forget_json_payload(approvals, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /forget json failed: {e}{RESET}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitCommand {
    Status,
    Json,
    Usage,
}

fn exit_usage_text(command: &str) -> String {
    format!(
        "{command} [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json]"
    )
}

fn exit_command_arg(trimmed: &str) -> Option<(&'static str, &str)> {
    for command in ["/exit", "/quit"] {
        if trimmed == command {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix(command) {
            if rest.starts_with(' ') {
                return Some((command, rest.trim()));
            }
        }
    }
    None
}

fn parse_exit_command(input: &str) -> ExitCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => ExitCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "preview --json" => ExitCommand::Json,
        _ => ExitCommand::Usage,
    }
}

fn exit_json_payload(command: &str, query: &str) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": command.trim_start_matches('/'),
        "aliases": ["exit", "quit"],
        "query": query.trim(),
        "available": true,
        "active_turn": false,
        "will_exit_repl": true,
        "will_close_session_tab": false,
        "will_stop_current_process": true,
        "interrupt_alternative": "Ctrl+D",
        "supported_actions": ["status", "state", "show", "info", "preview", "json", "--json", "status --json", "state --json", "show --json", "info --json", "preview --json"],
    })
}

fn print_exit_status(command: &str) {
    println!(
        "{DIM}  {command}: ready. Running `{command}` with no arguments quits this terminal REPL; Ctrl+D is equivalent.{RESET}",
    );
}

fn print_exit_json(command: &str, query: &str) {
    match serde_json::to_string_pretty(&exit_json_payload(command, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  {command} json failed: {e}{RESET}"),
    }
}

fn model_usage_text() -> &'static str {
    "/model [status|show|current|json|--json|status --json|show --json|current --json|list|ls|list --json|ls --json|next|cycle|prev|previous|back|model|provider/model]"
}

fn scoped_models_usage_text() -> &'static str {
    "/scoped-models <status|show|json|--json|status --json|show --json|patterns|clear|reset|off> — filter /model list and /model next|prev"
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

fn hotkeys_json_payload(query: &str) -> serde_json::Value {
    let shortcuts: Vec<serde_json::Value> = hotkey_lines()
        .into_iter()
        .map(|line| {
            let (key, action) = line.split_once(" — ").unwrap_or((line, ""));
            json!({
                "key": key,
                "action": action,
            })
        })
        .collect();
    json!({
        "surface": "terminal",
        "command": "hotkeys",
        "query": query.trim(),
        "aliases": ["hotkeys"],
        "supported_actions": ["status", "show", "list", "help", "json", "--json", "status --json", "show --json", "list --json"],
        "shortcuts": shortcuts,
    })
}

fn print_hotkeys_json(query: &str) {
    match serde_json::to_string_pretty(&hotkeys_json_payload(query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /hotkeys json: {e:#}{RESET}"),
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

fn tree_json_request_arg(input: &str) -> Option<String> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "json" | "--json" | "status --json" | "state --json" | "show --json" => {
            Some(String::new())
        }
        _ if lower.starts_with("json ") => Some(raw[5..].trim().to_string()),
        _ if lower.starts_with("--json ") => Some(raw[7..].trim().to_string()),
        _ if lower.ends_with(" --json") => Some(raw[..raw.len() - 7].trim().to_string()),
        _ => None,
    }
}

fn tree_usage_text() -> &'static str {
    "/tree [path|json|--json|status --json|state --json|show --json|path --json]"
}

fn print_project_tree_json(path: Option<&str>, query: &str) {
    let root = match tree_root(path) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("{DIM}  /tree json: {e:#}{RESET}");
            return;
        }
    };
    match project_tree_json_payload(&root, TREE_MAX_ENTRIES, query) {
        Ok(payload) => match serde_json::to_string_pretty(&payload) {
            Ok(raw) => println!("{raw}"),
            Err(e) => eprintln!("{DIM}  /tree json: {e:#}{RESET}"),
        },
        Err(e) => eprintln!("{DIM}  /tree json: {e:#}{RESET}"),
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

fn project_tree_json_payload(
    root: &Path,
    max_entries: usize,
    query: &str,
) -> Result<serde_json::Value> {
    let meta = std::fs::metadata(root).with_context(|| format!("read {}", root.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }
    let mut rows = Vec::new();
    let mut remaining = max_entries;
    collect_tree_json_entries(root, root, 0, &mut remaining, &mut rows)?;
    Ok(json!({
        "surface": "terminal",
        "command": "tree",
        "query": query.trim(),
        "aliases": ["tree"],
        "root": root.display().to_string(),
        "limit": max_entries,
        "count": rows.len(),
        "truncated": remaining == 0,
        "supported_actions": ["json", "--json", "status --json", "state --json", "show --json", "path --json"],
        "entries": rows,
    }))
}

fn collect_tree_json_entries(
    root: &Path,
    path: &Path,
    depth: usize,
    remaining: &mut usize,
    rows: &mut Vec<serde_json::Value>,
) -> Result<()> {
    if *remaining == 0 {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("read {}", path.display()))?;
    let file_type = meta.file_type();
    let name = if depth == 0 {
        path.file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".")
            .to_string()
    } else {
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    let relative = path
        .strip_prefix(root)
        .ok()
        .and_then(|p| if p.as_os_str().is_empty() { Some(".") } else { p.to_str() })
        .unwrap_or(".");
    rows.push(json!({
        "name": name,
        "path": relative,
        "depth": depth,
        "kind": if file_type.is_dir() { "dir" } else { "file" },
        "symlink": file_type.is_symlink(),
    }));
    *remaining -= 1;
    if file_type.is_dir() && !file_type.is_symlink() {
        let mut entries = tree_entries(path)?;
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        for entry in entries {
            if *remaining == 0 {
                break;
            }
            collect_tree_json_entries(root, &entry.path, depth + 1, remaining, rows)?;
        }
    }
    Ok(())
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
    if value.is_empty() || is_default_list_alias(value) {
        return Ok(CHANGELOG_DEFAULT_LIMIT);
    }
    let limit = value
        .parse::<usize>()
        .with_context(|| format!("usage: {}", changelog_usage_text()))?
        .clamp(1, CHANGELOG_MAX_LIMIT);
    Ok(limit)
}

fn changelog_usage_text() -> &'static str {
    "/changelog [count|list|recent|latest|status|state|show|json|--json|status --json|state --json|show --json|list --json|recent --json|latest --json]"
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

fn changelog_json_request_arg(input: &str) -> Option<String> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "list --json" | "recent --json" | "latest --json" => Some(String::new()),
        _ => lower
            .strip_prefix("json ")
            .or_else(|| lower.strip_prefix("--json "))
            .map(str::trim)
            .map(str::to_string),
    }
}

fn changelog_json_payload(limit: usize, query: &str, lines: Vec<String>) -> serde_json::Value {
    let commits = lines
        .into_iter()
        .map(|line| {
            let mut parts = line.splitn(2, char::is_whitespace);
            let hash = parts.next().unwrap_or("").trim();
            let summary = parts.next().unwrap_or("").trim();
            json!({
                "hash": hash,
                "summary": summary,
                "line": line,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "surface": "terminal",
        "command": "changelog",
        "query": query.trim(),
        "aliases": ["changelog"],
        "limit": limit,
        "count": commits.len(),
        "supported_actions": ["count", "list", "recent", "latest", "status", "state", "show", "json", "--json", "status --json", "state --json", "show --json", "list --json", "recent --json", "latest --json"],
        "commits": commits,
    })
}

fn print_changelog_json(limit: usize, query: &str) {
    match recent_git_commits(limit) {
        Ok(lines) => match serde_json::to_string_pretty(&changelog_json_payload(
            limit, query, lines,
        )) {
            Ok(raw) => println!("{raw}"),
            Err(e) => eprintln!("{DIM}  /changelog json: {e:#}{RESET}"),
        },
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
        SandboxAction::Json => {
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(e) => {
                    eprintln!("{DIM}  /sandbox json: could not resolve cwd: {e}{RESET}");
                    return;
                }
            };
            let profile = detect_strict_profile(&cwd);
            print_sandbox_json(&profile, action);
        }
        SandboxAction::Reload => {
            println!(
                "{DIM}  /sandbox reload: CLI sandbox policy is fixed when `libertai code` starts. Exit and restart with the desired --sandbox mode or policy settings.{RESET}"
            );
        }
        SandboxAction::Unknown(value) => {
            eprintln!("{DIM}  unknown /sandbox action: {value}. try \"info\", \"status\", \"diagnostics\", or \"reload\".{RESET}");
        }
    }
}

fn sandbox_usage_text() -> &'static str {
    "/sandbox [info|status|state|show|diagnostics|diag|json|--json|status --json|state --json|show --json|info --json|diagnostics --json|diag --json|reload]"
}

fn sandbox_json_payload(profile: &StrictProfile, query: &str) -> serde_json::Value {
    let count_kind = |kind| profile.binds.iter().filter(|bind| bind.kind == kind).count();
    json!({
        "command": "sandbox",
        "surface": "terminal",
        "query": query.trim(),
        "aliases": ["sandbox"],
        "cwd": profile.cwd,
        "network_allowed": profile.network_allowed,
        "bwrap_path": binary_on_path("bwrap"),
        "binds": {
            "count": profile.binds.len(),
            "enabled_count": profile.binds.iter().filter(|bind| bind.enabled).count(),
            "present_count": profile.binds.iter().filter(|bind| bind.present).count(),
            "bin_count": count_kind(BindKind::Bin),
            "lib_count": count_kind(BindKind::Lib),
            "config_count": count_kind(BindKind::Config),
            "items": profile.binds,
        },
        "env": profile.env,
        "will_write": false,
        "will_reload": false,
        "supported_actions": ["info", "status", "state", "show", "diagnostics", "diag", "json", "--json", "status --json", "state --json", "show --json", "info --json", "diagnostics --json", "diag --json", "reload"],
    })
}

fn print_sandbox_json(profile: &StrictProfile, query: &str) {
    match serde_json::to_string_pretty(&sandbox_json_payload(profile, query)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /sandbox json failed: {e}{RESET}"),
    }
}

fn abort_status_message() -> String {
    format!(
        "{DIM}  no active turn to abort. Press Ctrl+C while the assistant is streaming to interrupt the running turn.{RESET}"
    )
}

fn abort_json_payload(query: &str) -> serde_json::Value {
    json!({
        "command": "abort",
        "surface": "terminal",
        "query": query.trim(),
        "aliases": ["abort"],
        "active_turn": false,
        "abort_available": false,
        "interrupt_mechanism": "ctrl-c",
        "terminal_guidance": "Press Ctrl+C while the assistant is streaming to interrupt the running turn.",
        "supported_actions": ["status", "state", "show", "info", "json", "--json", "status --json", "state --json", "show --json", "info --json", "cancel", "stop", "interrupt"],
    })
}

fn print_abort_json(query: &str) {
    match serde_json::to_string_pretty(&abort_json_payload(query)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /abort json failed: {e}{RESET}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxAction<'a> {
    Info,
    Json,
    Reload,
    Unknown(&'a str),
}

fn parse_sandbox_action(raw: &str) -> SandboxAction<'_> {
    let value = raw.trim();
    if value.is_empty()
        || value.eq_ignore_ascii_case("info")
        || value.eq_ignore_ascii_case("status")
        || value.eq_ignore_ascii_case("state")
        || value.eq_ignore_ascii_case("show")
        || value.eq_ignore_ascii_case("diagnostics")
        || value.eq_ignore_ascii_case("diag")
    {
        SandboxAction::Info
    } else if matches!(
        value.to_ascii_lowercase().as_str(),
        "json"
            | "--json"
            | "status --json"
            | "state --json"
            | "show --json"
            | "info --json"
            | "diagnostics --json"
            | "diag --json"
    ) {
        SandboxAction::Json
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
    let messages = match copy_messages(handle).await {
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

async fn copy_messages(handle: &AgentSessionHandle) -> Result<Vec<Message>> {
    handle.messages().await.context("reading transcript")
}

async fn print_copy_status(handle: &AgentSessionHandle) {
    let messages = match copy_messages(handle).await {
        Ok(messages) => messages,
        Err(e) => {
            eprintln!("{DIM}  /copy status: could not read transcript: {e:#}{RESET}");
            return;
        }
    };
    let response = last_assistant_text(&messages);
    println!("{BOLD}copy{RESET}");
    match response {
        Some(text) => {
            let too_large = text.len() > OSC52_MAX_TEXT_BYTES;
            println!("{DIM}  latest assistant response:{RESET} available");
            println!("{DIM}  bytes:{RESET} {}", text.len());
            if too_large {
                println!(
                    "{DIM}  terminal clipboard:{RESET} unavailable, response exceeds {} bytes",
                    OSC52_MAX_TEXT_BYTES
                );
            } else {
                println!("{DIM}  terminal clipboard:{RESET} available via OSC52");
            }
        }
        None => println!("{DIM}  latest assistant response:{RESET} unavailable"),
    }
    println!("{DIM}  usage:{RESET} {}", copy_usage_text());
}

fn copy_json_payload(messages: &[Message], query: &str) -> serde_json::Value {
    let response = last_assistant_text(messages);
    let bytes = response.as_ref().map(|text| text.len()).unwrap_or(0);
    json!({
        "command": "copy",
        "surface": "terminal",
        "query": query.trim(),
        "aliases": ["copy"],
        "target": "latest_assistant_response",
        "available": response.is_some(),
        "bytes": bytes,
        "max_terminal_clipboard_bytes": OSC52_MAX_TEXT_BYTES,
        "copy_available": response.is_some() && bytes <= OSC52_MAX_TEXT_BYTES,
        "copy_mechanism": "osc52",
        "supported_actions": ["status", "show", "info", "json", "--json", "status --json", "show --json", "info --json", "last", "latest", "response", "assistant", "assistant-response"],
    })
}

async fn print_copy_json(handle: &AgentSessionHandle, query: &str) {
    match copy_messages(handle).await {
        Ok(messages) => match serde_json::to_string_pretty(&copy_json_payload(&messages, query)) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("{DIM}  /copy json failed: {e}{RESET}"),
        },
        Err(e) => eprintln!("{DIM}  /copy json: could not read transcript: {e:#}{RESET}"),
    }
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
    if value.is_empty() || is_default_list_alias(value) {
        return Ok(HISTORY_DEFAULT_LIMIT);
    }
    let limit = value
        .parse::<usize>()
        .with_context(|| format!("usage: {}", history_usage_text()))?
        .clamp(1, HISTORY_MAX_LIMIT);
    Ok(limit)
}

fn history_usage_text() -> &'static str {
    "/history [count|list|recent|latest|status|state|show|json|--json|status --json|state --json|show --json|list --json|recent --json|latest --json]"
}

fn is_default_list_alias(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "status" | "state" | "show" | "list" | "recent" | "latest"
    )
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

fn history_json_request_arg(input: &str) -> Option<String> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "list --json" | "recent --json" | "latest --json" => Some(String::new()),
        _ => lower
            .strip_prefix("json ")
            .or_else(|| lower.strip_prefix("--json "))
            .map(str::trim)
            .map(str::to_string),
    }
}

fn history_json_payload(
    history: &VecDeque<String>,
    limit: usize,
    query: &str,
) -> serde_json::Value {
    let shown = history.len().min(limit);
    let start = history.len().saturating_sub(shown);
    let prompts = history
        .iter()
        .enumerate()
        .skip(start)
        .map(|(idx, prompt)| {
            json!({
                "index": idx + 1,
                "text": prompt,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "surface": "terminal",
        "command": "history",
        "query": query.trim(),
        "aliases": ["history"],
        "total": history.len(),
        "limit": limit,
        "shown": shown,
        "supported_actions": ["count", "list", "recent", "latest", "status", "state", "show", "json", "--json", "status --json", "state --json", "show --json", "list --json", "recent --json", "latest --json"],
        "prompts": prompts,
    })
}

fn print_history_json(history: &VecDeque<String>, limit: usize, query: &str) {
    let payload = history_json_payload(history, limit, query);
    match serde_json::to_string_pretty(&payload) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /history json: {e:#}{RESET}"),
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
        "" | "show" | "status" | "current" | "info" => PermissionsCommand::Show,
        "json" | "--json" | "status --json" | "show --json" | "current --json"
        | "info --json" => PermissionsCommand::Json,
        "open" | "settings" | "edit" | "approvals" => PermissionsCommand::Open,
        "default" | "normal" => PermissionsCommand::Set(Mode::Normal),
        "acceptedits" | "accept-edits" | "accept_edits" => {
            PermissionsCommand::Set(Mode::AcceptEdits)
        }
        "plan" | "readonly" | "read-only" => PermissionsCommand::Set(Mode::Plan),
        "forget" | "clear" | "reset" => PermissionsCommand::Forget,
        "bypass" | "danger" | "bypasspermissions" | "bypass-permissions" | "bypass_permissions" => {
            PermissionsCommand::UnsupportedBypass
        }
        _ => PermissionsCommand::Show,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanCommand {
    Status,
    On,
    Off,
    Usage,
}

fn parse_plan_command(input: &str) -> PlanCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "show" | "current" | "info" => PlanCommand::Status,
        "on" | "enable" | "enabled" | "true" | "plan" | "readonly" | "read-only" => {
            PlanCommand::On
        }
        "off" | "disable" | "disabled" | "false" | "normal" | "default" => PlanCommand::Off,
        _ => PlanCommand::Usage,
    }
}

fn print_plan_status(mode: Mode) {
    let active = matches!(mode, Mode::Plan);
    println!(
        "{DIM}  plan mode: {} ({}){RESET}",
        if active { "on" } else { "off" },
        mode_label(mode)
    );
    println!("{DIM}  use /plan on, /plan off, or bare /plan to cycle modes.{RESET}");
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

fn print_permissions_status(mode: Mode, approvals: &ApprovalState) {
    println!("{DIM}  permission mode: {}{RESET}", mode_label(mode));
    println!(
        "{DIM}  remembered approvals:{RESET} {} saved rule(s)",
        approvals.always_rules().len()
    );
    println!(
        "{DIM}  supported:{RESET} default/normal, acceptEdits/accept-edits/accept_edits, plan/readonly/read-only"
    );
    println!("{DIM}  native bypassPermissions is intentionally unavailable.{RESET}");
    println!("{DIM}  use /permissions forget to clear saved allow rules.{RESET}");
    println!("{DIM}  use /permissions open to show the approvals settings target and rule path.{RESET}");
    println!("{DIM}  use /permissions bypassPermissions to explain the native safety stance.{RESET}");
}

fn permissions_json_payload(
    mode: Mode,
    approvals: &ApprovalState,
    query: &str,
) -> serde_json::Value {
    let allow_rules_path = crate::config::allow_rules_path()
        .ok()
        .map(|path| path.display().to_string());
    json!({
        "surface": "terminal",
        "command": "permissions",
        "query": query.trim(),
        "mode": mode_label(mode),
        "remembered_approvals": approvals.always_rules().len(),
        "native_bypass_permissions": false,
        "bypass_permissions_note": "native bypassPermissions is intentionally unavailable",
        "allow_rules_path": allow_rules_path,
        "settings_target": "Settings > Approvals",
        "supported_actions": ["status", "show", "current", "info", "json", "--json", "status --json", "show --json", "current --json", "info --json", "default", "normal", "acceptEdits", "accept-edits", "accept_edits", "plan", "readonly", "read-only", "open", "settings", "edit", "approvals", "forget", "clear", "reset", "bypassPermissions", "bypass", "danger"],
        "supported_modes": ["normal", "acceptEdits", "plan"],
        "aliases": {
            "normal": ["default", "normal"],
            "acceptEdits": ["acceptEdits", "accept-edits", "accept_edits"],
            "plan": ["plan", "readonly", "read-only"],
            "forget": ["forget", "clear", "reset"],
            "bypass_stance": ["bypassPermissions", "bypass", "danger"]
        }
    })
}

fn print_permissions_json(mode: Mode, approvals: &ApprovalState, query: &str) {
    match serde_json::to_string_pretty(&permissions_json_payload(mode, approvals, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /permissions json: {e:#}{RESET}"),
    }
}

fn mode_json_payload(mode: Mode, query: &str) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": "mode",
        "query": query.trim(),
        "mode": mode_label(mode),
        "behavior": match mode {
            Mode::Normal => "mutating tools ask before running",
            Mode::AcceptEdits => "write/edit tools auto-allow; bash still asks",
            Mode::Plan => "mutating tools are denied automatically",
        },
        "supported_actions": ["status", "show", "current", "info", "json", "--json", "status --json", "show --json", "current --json", "info --json", "default", "normal", "acceptEdits", "accept-edits", "accept_edits", "plan", "readonly", "read-only"],
        "supported_modes": ["normal", "acceptEdits", "plan"],
        "aliases": {
            "normal": ["default", "normal"],
            "acceptEdits": ["acceptEdits", "accept-edits", "accept_edits"],
            "plan": ["plan", "readonly", "read-only"]
        }
    })
}

fn print_mode_json(mode: Mode, query: &str) {
    match serde_json::to_string_pretty(&mode_json_payload(mode, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /mode json: {e:#}{RESET}"),
    }
}

fn permissions_usage_text() -> &'static str {
    "/permissions [status|show|current|info|json|--json|status --json|show --json|current --json|info --json|default|normal|acceptEdits|accept-edits|accept_edits|plan|readonly|read-only|open|settings|edit|approvals|forget|clear|reset|bypassPermissions|bypass|danger]"
}

fn mode_usage_text() -> &'static str {
    "/mode [status|show|current|info|json|--json|status --json|show --json|current --json|info --json|default|normal|acceptEdits|accept-edits|accept_edits|plan|readonly|read-only]"
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
    StatusJson,
    ProviderStatus(&'a str),
    ProviderStatusJson(&'a str),
    Provider(&'a str),
}

fn login_usage_text() -> &'static str {
    "/login [status|show|info|json|--json|status --json|show --json|info --json|libertai|account|key|api-key|api|provider|show <provider>|show <provider> --json|info <provider>|info <provider> --json|inspect <provider>|inspect <provider> --json|provider <provider>|provider <provider> --json|<provider> --json]"
}

fn logout_usage_text() -> &'static str {
    "/logout [status|show|info|json|--json|status --json|show --json|info --json|libertai|account|key|api-key|api|provider|show <provider>|show <provider> --json|info <provider>|info <provider> --json|inspect <provider>|inspect <provider> --json|provider <provider>|provider <provider> --json|<provider> --json]"
}

fn parse_login_slash_target(query: &str) -> LoginSlashTarget<'_> {
    let (raw, wants_json) = strip_login_json_suffix(query);
    if raw.is_empty() {
        return if wants_json {
            LoginSlashTarget::StatusJson
        } else {
            LoginSlashTarget::Account
        };
    }
    if let Some((head, tail)) = split_first_word(raw) {
        if matches!(
            head.to_ascii_lowercase().as_str(),
            "show" | "info" | "inspect" | "provider"
        ) {
            let provider = tail.trim();
            if !provider.is_empty() && provider.split_whitespace().count() == 1 {
                return if wants_json {
                    LoginSlashTarget::ProviderStatusJson(provider)
                } else {
                    LoginSlashTarget::ProviderStatus(provider)
                };
            }
            return if wants_json {
                LoginSlashTarget::StatusJson
            } else {
                LoginSlashTarget::Status
            };
        }
    }
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "status" | "show" | "info" => {
            if wants_json {
                LoginSlashTarget::StatusJson
            } else {
                LoginSlashTarget::Status
            }
        }
        "libertai" | "account" | "key" | "api-key" | "api" => {
            if wants_json {
                LoginSlashTarget::ProviderStatusJson("libertai")
            } else {
                LoginSlashTarget::Account
            }
        }
        _ => {
            if wants_json {
                LoginSlashTarget::ProviderStatusJson(raw)
            } else {
                LoginSlashTarget::Provider(raw)
            }
        }
    }
}

fn strip_login_json_suffix(query: &str) -> (&str, bool) {
    let raw = query.trim();
    let lower = raw.to_ascii_lowercase();
    if lower == "json" || lower == "--json" {
        return ("", true);
    }
    if let Some(prefix) = lower.strip_suffix(" --json") {
        return (&raw[..prefix.len()], true);
    }
    if let Some(prefix) = lower.strip_suffix(" json") {
        return (&raw[..prefix.len()], true);
    }
    (raw, false)
}

fn handle_login_slash(query: &str, cfg: &LibertaiConfig) {
    match parse_login_slash_target(query) {
        LoginSlashTarget::Status => print_login_status(cfg),
        LoginSlashTarget::StatusJson => print_login_status_json("login", query, cfg),
        LoginSlashTarget::Account => {
            println!("{BOLD}login{RESET}");
            println!("{DIM}  LibertAI API key:{RESET} {}", login_key_state(cfg));
            println!(
                "{DIM}  use /login with no arguments to run the interactive LibertAI login flow.{RESET}"
            );
        }
        LoginSlashTarget::ProviderStatus(provider) => print_provider_login_details(provider, cfg),
        LoginSlashTarget::ProviderStatusJson(provider) => {
            print_provider_login_details_json("login", query, provider, cfg)
        }
        LoginSlashTarget::Provider(provider) => print_provider_login_note(provider, cfg),
    }
}

fn handle_logout_slash(query: &str, cfg: &LibertaiConfig) {
    match parse_login_slash_target(query) {
        LoginSlashTarget::Status => print_login_status(cfg),
        LoginSlashTarget::StatusJson => print_login_status_json("logout", query, cfg),
        LoginSlashTarget::Account => {
            println!("{BOLD}logout{RESET}");
            println!(
                "{DIM}  use /logout with no arguments to back up and remove the LibertAI config.{RESET}"
            );
        }
        LoginSlashTarget::ProviderStatus(provider) => print_provider_logout_details(provider, cfg),
        LoginSlashTarget::ProviderStatusJson(provider) => {
            print_provider_login_details_json("logout", query, provider, cfg)
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

fn login_status_payload(command: &str, query: &str, cfg: &LibertaiConfig) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": command,
        "query": query.trim(),
        "aliases": [command],
        "supported_actions": login_supported_actions(),
        "libertai": {
            "api_key": login_key_state(cfg),
            "logged_in": cfg.auth.api_key.is_some(),
            "wallet": cfg.auth.wallet_address.as_deref().map(mask_key),
            "chain": cfg.auth.chain,
        },
        "provider_credentials": {
            "terminal_stores_provider_keys": false,
            "desktop_settings_target": "Settings > Backends",
        },
    })
}

fn login_supported_actions() -> &'static [&'static str] {
    &[
        "status",
        "show",
        "info",
        "json",
        "--json",
        "status --json",
        "show --json",
        "info --json",
        "libertai",
        "account",
        "key",
        "api-key",
        "api",
        "provider",
        "show provider",
        "show provider --json",
        "info provider",
        "info provider --json",
        "inspect provider",
        "inspect provider --json",
        "provider provider",
        "provider provider --json",
        "provider --json",
    ]
}

fn print_login_status_json(command: &str, query: &str, cfg: &LibertaiConfig) {
    match serde_json::to_string_pretty(&login_status_payload(command, query, cfg)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /{command} json: {e:#}{RESET}"),
    }
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

fn provider_login_payload(
    command: &str,
    query: &str,
    provider: &str,
    cfg: &LibertaiConfig,
) -> serde_json::Value {
    let is_libertai = provider.eq_ignore_ascii_case("libertai");
    json!({
        "surface": "terminal",
        "command": command,
        "query": query.trim(),
        "aliases": [command],
        "supported_actions": login_supported_actions(),
        "provider": provider,
        "terminal_provider_key": if is_libertai { login_key_state(cfg) } else { "not stored".to_string() },
        "managed_by_desktop_settings": !is_libertai,
        "libertai": {
            "api_key": login_key_state(cfg),
            "logged_in": cfg.auth.api_key.is_some(),
            "wallet": cfg.auth.wallet_address.as_deref().map(mask_key),
            "chain": cfg.auth.chain,
        },
    })
}

fn print_provider_login_details_json(
    command: &str,
    query: &str,
    provider: &str,
    cfg: &LibertaiConfig,
) {
    match serde_json::to_string_pretty(&provider_login_payload(command, query, provider, cfg)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /{command} show {provider} --json: {e:#}{RESET}"),
    }
}

fn print_provider_login_details(provider: &str, cfg: &LibertaiConfig) {
    println!("{BOLD}login: {provider}{RESET}");
    if provider.eq_ignore_ascii_case("libertai") {
        println!(
            "{DIM}  terminal LibertAI API key:{RESET} {}",
            login_key_state(cfg)
        );
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
        println!("{DIM}  run /login libertai to inspect the terminal account flow.{RESET}");
        return;
    }
    println!("{DIM}  terminal provider key:{RESET} not stored");
    println!(
        "{DIM}  desktop state:{RESET} use desktop /login show {provider} or Settings > Backends for key/base URL/model-cache details."
    );
    println!("{DIM}  terminal LibertAI API key:{RESET} {}", login_key_state(cfg));
}

fn print_provider_logout_details(provider: &str, cfg: &LibertaiConfig) {
    println!("{BOLD}logout: {provider}{RESET}");
    if provider.eq_ignore_ascii_case("libertai") {
        println!(
            "{DIM}  terminal LibertAI API key:{RESET} {}",
            login_key_state(cfg)
        );
        println!("{DIM}  run /logout libertai to clear terminal LibertAI credentials.{RESET}");
        return;
    }
    println!("{DIM}  terminal provider key:{RESET} not stored");
    println!(
        "{DIM}  desktop action:{RESET} use desktop /logout {provider} to clear a desktop-stored provider API key."
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
        "json" | "--json" | "status --json" | "show --json" | "current --json" => {
            ModelSlashCommand::Json
        }
        "list --json" | "ls --json" => ModelSlashCommand::JsonList,
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
    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "json" | "--json" | "status --json" | "show --json"
    ) {
        return ScopedModelsCommand::Json;
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
        ScopedModelsCommand::Json => print_scoped_model_json(scoped_model_patterns, raw),
        ScopedModelsCommand::Clear => {
            scoped_model_patterns.clear();
            println!("{DIM}  scoped models cleared; /model list shows all discovered models.{RESET}");
        }
        ScopedModelsCommand::Set(patterns) => {
            *scoped_model_patterns = patterns;
            print_scoped_model_status(scoped_model_patterns);
        }
        ScopedModelsCommand::Usage => {
            eprintln!(
                "{DIM}  usage: /scoped-models <status|show|json|--json|status --json|show --json|pattern[,pattern...]|clear|reset|off>{RESET}"
            );
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
        "{DIM}  usage:{RESET} /scoped-models status, /scoped-models status --json, /scoped-models qwen* gemma*, /scoped-models clear|reset|off, /model list, /model next|prev"
    );
    println!();
}

fn scoped_model_json_payload(scoped_model_patterns: &[String], query: &str) -> serde_json::Value {
    json!({
        "command": "scoped-models",
        "surface": "terminal",
        "query": query.trim(),
        "patterns": scoped_model_patterns,
        "is_scoped": !scoped_model_patterns.is_empty(),
        "aliases": ["scoped-models", "scoped"],
        "supported_actions": ["status", "show", "json", "--json", "status --json", "show --json", "clear", "reset", "off"],
    })
}

fn print_scoped_model_json(scoped_model_patterns: &[String], query: &str) {
    match serde_json::to_string_pretty(&scoped_model_json_payload(scoped_model_patterns, query)) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{DIM}  /scoped-models json failed: {e}{RESET}"),
    }
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

fn model_json_payload(
    provider: &str,
    model: &str,
    cfg: &LibertaiConfig,
    scoped_model_patterns: &[String],
    query: &str,
    available_models: Option<Vec<String>>,
) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": "model",
        "query": query.trim(),
        "current": {
            "provider": provider,
            "model": model,
            "id": format!("{provider}/{model}"),
        },
        "default": {
            "provider": cfg.default_code_provider,
            "model": cfg.default_code_model,
            "id": format!("{}/{}", cfg.default_code_provider, cfg.default_code_model),
        },
        "scope": {
            "patterns": scoped_model_patterns,
            "is_scoped": !scoped_model_patterns.is_empty(),
        },
        "available_models": available_models,
        "aliases": ["model"],
        "supported_actions": ["status", "show", "current", "json", "--json", "status --json", "show --json", "current --json", "list", "ls", "list --json", "ls --json", "next", "cycle", "prev", "previous", "back", "set <model>", "set <provider/model>"],
    })
}

fn print_model_json(
    handle: &AgentSessionHandle,
    cfg: &LibertaiConfig,
    scoped_model_patterns: &[String],
    query: &str,
    include_list: bool,
) {
    let (provider, model) = handle.model();
    let available_models = if include_list {
        match crate::client::list_models(cfg) {
            Ok(list) => {
                let ids: Vec<String> = list.data.into_iter().map(|entry| entry.id).collect();
                Some(scoped_model_ids(&provider, &ids, scoped_model_patterns))
            }
            Err(e) => {
                eprintln!("{DIM}  /model list --json: {e:#}{RESET}");
                return;
            }
        }
    } else {
        None
    };
    match serde_json::to_string_pretty(&model_json_payload(
        &provider,
        &model,
        cfg,
        scoped_model_patterns,
        query,
        available_models,
    )) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /model json failed: {e}{RESET}"),
    }
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
    println!("{DIM}  usage:{RESET} {}", name_usage_text());
}

fn name_usage_text() -> &'static str {
    "/name <name> | /name [status|state|show|current|info|json|--json|status --json|state --json|show --json|current --json|info --json]"
}

fn name_json_payload(name: Option<&str>, query: &str) -> serde_json::Value {
    json!({
        "command": "name",
        "surface": "terminal",
        "query": query.trim(),
        "aliases": ["name", "rename"],
        "current": name,
        "is_named": name.is_some(),
        "supported_actions": ["status", "state", "show", "current", "info", "json", "--json", "status --json", "state --json", "show --json", "current --json", "info --json", "set"],
    })
}

fn print_name_json(name: Option<&str>, query: &str) {
    match serde_json::to_string_pretty(&name_json_payload(name, query)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /name json failed: {e}{RESET}"),
    }
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

fn is_export_json_arg(input: &str) -> bool {
    matches!(
        normalize_help_command_arg(input).as_str(),
        "json" | "--json" | "status --json" | "show --json" | "preview --json"
    )
}

async fn print_export_json(handle: &AgentSessionHandle, query: &str) {
    match handle.messages().await {
        Ok(messages) => match serde_json::to_string_pretty(&export_json_payload(query, &messages)) {
            Ok(body) => println!("{body}"),
            Err(e) => eprintln!("{DIM}  /export json failed: {e}{RESET}"),
        },
        Err(e) => eprintln!("{DIM}  /export json: could not read transcript: {e:#}{RESET}"),
    }
}

fn export_json_payload(query: &str, messages: &[Message]) -> serde_json::Value {
    let markdown = render_markdown_transcript(messages);
    json!({
        "surface": "terminal",
        "command": "export",
        "aliases": ["export"],
        "query": normalize_help_command_arg(query),
        "available": !messages.is_empty(),
        "message_count": messages.len(),
        "default_path": "libertai-transcript.md",
        "artifact": {
            "format": "markdown",
            "bytes": markdown.len(),
            "lines": markdown.lines().count(),
        },
        "will_write": false,
        "will_copy": false,
        "supported_actions": ["copy", "save", "path", "json", "--json", "status --json", "show --json", "preview --json"],
    })
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

fn is_share_json_arg(input: &str) -> bool {
    matches!(
        normalize_help_command_arg(input).as_str(),
        "json" | "--json" | "status --json" | "show --json" | "preview --json"
    )
}

async fn print_share_json(handle: &AgentSessionHandle, query: &str) {
    match handle.messages().await {
        Ok(messages) => match serde_json::to_string_pretty(&share_json_payload(query, &messages)) {
            Ok(body) => println!("{body}"),
            Err(e) => eprintln!("{DIM}  /share json failed: {e}{RESET}"),
        },
        Err(e) => eprintln!("{DIM}  /share json: could not read transcript: {e:#}{RESET}"),
    }
}

fn share_json_payload(query: &str, messages: &[Message]) -> serde_json::Value {
    let html = render_html_transcript(messages);
    json!({
        "surface": "terminal",
        "command": "share",
        "aliases": ["share"],
        "query": normalize_help_command_arg(query),
        "available": !messages.is_empty(),
        "message_count": messages.len(),
        "default_path": "libertai-share.html",
        "default_gist_filename": "libertai-share.html",
        "artifact": {
            "format": "html",
            "bytes": html.len(),
            "lines": html.lines().count(),
        },
        "will_write": false,
        "will_publish": false,
        "will_copy": false,
        "supported_actions": ["copy", "save", "path", "gist", "json", "--json", "status --json", "show --json", "preview --json"],
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactPreviewCommand {
    Status,
    Json,
    Usage,
}

fn compact_usage_text() -> &'static str {
    "/compact [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json|notes]"
}

fn compact_preview_arg(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("/compact ")?.trim();
    match normalize_help_command_arg(rest).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" | "json" | "--json"
        | "status --json" | "state --json" | "show --json" | "info --json"
        | "preview --json" | "help" | "usage" => Some(rest),
        _ => None,
    }
}

fn parse_compact_preview_command(input: &str) -> CompactPreviewCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => CompactPreviewCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "preview --json" => CompactPreviewCommand::Json,
        _ => CompactPreviewCommand::Usage,
    }
}

fn compact_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": "compact",
        "query": query.trim(),
        "aliases": ["compact"],
        "available": true,
        "active_turn": false,
        "will_compact_history": true,
        "accepts_notes": true,
        "notes_argument": "/compact NOTES",
        "auto_compaction": {
            "enabled": cfg.code_auto_compaction_enabled,
            "reserve_tokens": cfg.code_compaction_reserve_tokens,
            "keep_recent_tokens": cfg.code_compaction_keep_recent_tokens,
        },
        "supported_actions": ["status", "state", "show", "info", "preview", "json", "--json", "status --json", "state --json", "show --json", "info --json", "preview --json", "notes"],
    })
}

fn print_compact_status(cfg: &LibertaiConfig) {
    println!(
        "{DIM}  /compact: ready. Running `/compact` summarizes older conversation history now; add notes with `/compact <notes>`. Auto compaction is {} (reserve={}, keep_recent={}).{RESET}",
        if cfg.code_auto_compaction_enabled {
            "on"
        } else {
            "off"
        },
        cfg.code_compaction_reserve_tokens,
        cfg.code_compaction_keep_recent_tokens
    );
}

fn print_compact_json(cfg: &LibertaiConfig, query: &str) {
    match serde_json::to_string_pretty(&compact_json_payload(cfg, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /compact json failed: {e}{RESET}"),
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
        "/hooks" | "/hook" => Some(""),
        _ => trimmed
            .strip_prefix("/hooks ")
            .or_else(|| trimmed.strip_prefix("/hook "))
            .map(str::trim),
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

fn is_onboarding_json_arg(input: &str) -> bool {
    matches!(
        normalize_onboarding_arg(input).as_str(),
        "json" | "--json" | "status --json" | "show --json" | "preview --json"
    )
}

fn is_onboarding_preview_arg(input: &str) -> bool {
    matches!(
        normalize_onboarding_arg(input).as_str(),
        "show" | "status" | "preview"
    )
}

fn normalize_onboarding_arg(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
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

fn parse_theme_command(rest: &str) -> ThemeCommand {
    let requested = rest.trim();
    match requested.to_ascii_lowercase().as_str() {
        "" | "status" | "show" | "current" | "info" => ThemeCommand::Status,
        "json" | "--json" | "status --json" | "show --json" | "current --json"
        | "info --json" => ThemeCommand::Json,
        _ => ThemeCommand::Requested(requested.to_string()),
    }
}

fn vim_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/vim" => Some(""),
        _ => trimmed.strip_prefix("/vim ").map(str::trim),
    }
}

fn ide_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/ide" => Some(""),
        _ => trimmed.strip_prefix("/ide ").map(str::trim),
    }
}

fn bug_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/bug" => Some(""),
        _ => trimmed.strip_prefix("/bug ").map(str::trim),
    }
}

fn copy_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/copy" => Some(""),
        _ => trimmed.strip_prefix("/copy ").map(str::trim),
    }
}

fn hotkeys_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/hotkeys" => Some(""),
        _ => trimmed.strip_prefix("/hotkeys ").map(str::trim),
    }
}

fn reload_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/reload" => Some(""),
        _ => trimmed.strip_prefix("/reload ").map(str::trim),
    }
}

fn status_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/status" => Some(""),
        _ => trimmed.strip_prefix("/status ").map(str::trim),
    }
}

fn doctor_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/doctor" => Some(""),
        _ => trimmed.strip_prefix("/doctor ").map(str::trim),
    }
}

fn abort_command_arg(trimmed: &str) -> Option<&str> {
    match trimmed {
        "/abort" => Some(""),
        _ => trimmed.strip_prefix("/abort ").map(str::trim),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HooksCommand {
    Status,
    Json,
    Open,
    Show(String),
    Usage,
}

fn parse_hooks_command(input: &str) -> HooksCommand {
    let raw = input.trim();
    let normalized = raw.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "json"
            | "--json"
            | "status --json"
            | "list --json"
            | "state --json"
            | "diagnostics --json"
            | "diag --json"
            | "show --json"
    ) {
        return HooksCommand::Json;
    }
    if let Some((head, tail)) = split_first_word(raw) {
        if matches!(
            head.to_ascii_lowercase().as_str(),
            "show" | "event" | "inspect"
        ) {
            let event = tail.trim();
            if !event.is_empty() && event.split_whitespace().count() == 1 {
                return HooksCommand::Show(event.to_string());
            }
            return HooksCommand::Usage;
        }
    }
    match normalized.as_str() {
        "" | "status" | "list" | "state" | "diagnostics" | "diag" => HooksCommand::Status,
        "open" | "settings" | "edit" => HooksCommand::Open,
        _ => HooksCommand::Usage,
    }
}

fn parse_mcp_command(input: &str) -> McpCommand {
    let raw = input.trim();
    let normalized = raw.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "json"
            | "--json"
            | "status --json"
            | "list --json"
            | "state --json"
            | "diagnostics --json"
            | "diag --json"
            | "show --json"
    ) {
        return McpCommand::Json;
    }
    if let Some((head, tail)) = split_first_word(raw) {
        if matches!(
            head.to_ascii_lowercase().as_str(),
            "show" | "server" | "inspect"
        ) {
            let name = tail.trim();
            if !name.is_empty() && name.split_whitespace().count() == 1 {
                return McpCommand::Show(name.to_string());
            }
            return McpCommand::Usage;
        }
    }
    match normalized.as_str() {
        "" | "status" | "list" | "state" | "diagnostics" | "diag" | "show" => {
            McpCommand::Status
        }
        "probe" | "probes" => McpCommand::Probe,
        "refresh" | "probe --save" | "probe save" | "probe --write" | "probe write" => {
            McpCommand::ProbeSave
        }
        "reset" | "reset-sessions" => McpCommand::Reset,
        "open" | "settings" | "edit" => McpCommand::Open,
        _ => McpCommand::Usage,
    }
}

fn parse_vim_command(input: &str) -> VimCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" | "current" | "info" => VimCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "current --json" | "info --json" => VimCommand::Json,
        "on" | "enable" | "enabled" | "true" => VimCommand::Enable,
        "off" | "disable" | "disabled" | "false" => VimCommand::Disable,
        _ => VimCommand::Usage,
    }
}

fn parse_ide_command(input: &str) -> IdeCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" => IdeCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" => {
            IdeCommand::Json
        }
        "open" | "settings" | "edit" => IdeCommand::Open,
        _ => IdeCommand::Usage,
    }
}

fn parse_bug_command(input: &str) -> BugCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "report" | "template" | "status" | "show" => BugCommand::Template,
        "json" | "--json" | "status --json" | "show --json" | "template --json"
        | "report --json" => BugCommand::Json,
        _ => BugCommand::Usage,
    }
}

fn parse_copy_command(input: &str) -> CopyCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "last" | "latest" | "response" | "assistant" | "assistant-response" => {
            CopyCommand::LastAssistant
        }
        "status" | "show" | "info" => CopyCommand::Status,
        "json" | "--json" | "status --json" | "show --json" | "info --json" => {
            CopyCommand::Json
        }
        _ => CopyCommand::Usage,
    }
}

fn copy_usage_text() -> &'static str {
    "/copy [status|show|info|json|--json|status --json|show --json|info --json|last|latest|response|assistant|assistant-response]"
}

fn parse_hotkeys_command(input: &str) -> HotkeysCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "show" | "list" | "help" => HotkeysCommand::Show,
        "json" | "--json" | "status --json" | "show --json" | "list --json" => HotkeysCommand::Json,
        _ => HotkeysCommand::Usage,
    }
}

fn hotkeys_usage_text() -> &'static str {
    "/hotkeys [status|show|list|help|json|--json|status --json|show --json|list --json]"
}

fn parse_reload_command(input: &str) -> ReloadCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "config" | "session" | "now" | "fresh" => ReloadCommand::Session,
        "json" | "--json" | "config --json" | "session --json" | "now --json"
        | "fresh --json" => ReloadCommand::Json,
        _ => ReloadCommand::Usage,
    }
}

fn parse_status_command(input: &str) -> StatusCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" | "info" | "current" | "session" => {
            StatusCommand::Session
        }
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "current --json" | "session --json" => StatusCommand::Json,
        _ => StatusCommand::Usage,
    }
}

fn status_usage_text() -> &'static str {
    "/status [status|state|show|info|current|session|json|--json|status --json|state --json|show --json|info --json|current --json|session --json]"
}

fn parse_doctor_command(input: &str) -> DoctorCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" | "info" | "health" | "diagnostics" | "diag" => {
            DoctorCommand::Run
        }
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" | "health --json" | "diagnostics --json" | "diag --json" => {
            DoctorCommand::Json
        }
        _ => DoctorCommand::Usage,
    }
}

fn doctor_usage_text() -> &'static str {
    "/doctor [status|state|show|info|health|diagnostics|diag|json|--json|status --json|state --json|show --json|info --json|health --json|diagnostics --json|diag --json]"
}

fn parse_abort_command(input: &str) -> AbortCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" | "info" | "cancel" | "stop" | "interrupt" => {
            AbortCommand::Status
        }
        "json" | "--json" | "status --json" | "state --json" | "show --json"
        | "info --json" => AbortCommand::Json,
        _ => AbortCommand::Usage,
    }
}

fn abort_usage_text() -> &'static str {
    "/abort [status|state|show|info|json|--json|status --json|state --json|show --json|info --json|cancel|stop|interrupt]"
}

fn parse_notify_command(input: &str) -> NotifyCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" => NotifyCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" => {
            NotifyCommand::Json
        }
        "on" | "enable" | "enabled" => NotifyCommand::On,
        "off" | "disable" | "disabled" | "clear" => NotifyCommand::Off,
        "test" | "ping" => NotifyCommand::Test,
        _ => NotifyCommand::Usage,
    }
}

fn handle_notify_command(raw: &str, cfg: &mut Arc<LibertaiConfig>) -> Result<()> {
    match parse_notify_command(raw) {
        NotifyCommand::Status => print_notify_status(cfg),
        NotifyCommand::Json => print_notify_json(cfg, raw),
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
            eprintln!("{DIM}  usage: {}{RESET}", notify_usage_text());
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
    println!("{DIM}  usage:{RESET} {}", notify_usage_text());
}

fn notify_usage_text() -> &'static str {
    "/notify [on|enable|enabled|off|disable|disabled|clear|status|state|show|json|--json|status --json|state --json|show --json|test|ping]"
}

fn notify_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
    json!({
        "command": "notify",
        "surface": "terminal",
        "query": query.trim(),
        "aliases": ["notify", "notifications"],
        "turn_notifications": cfg.code_turn_notifications,
        "agent_push_notifications": {
            "terminal_bell": true,
            "visible_notification_block": true,
        },
        "permission": "terminal",
        "supported_actions": ["status", "state", "show", "json", "--json", "status --json", "state --json", "show --json", "on", "enable", "enabled", "off", "disable", "disabled", "clear", "test", "ping"],
    })
}

fn print_notify_json(cfg: &LibertaiConfig, query: &str) {
    match serde_json::to_string_pretty(&notify_json_payload(cfg, query)) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{DIM}  /notify json failed: {e}{RESET}"),
    }
}

fn print_send_status(rest: &str) {
    let requested = rest.trim();
    if is_send_json_request(requested) {
        let payload = send_json_payload(requested);
        match serde_json::to_string_pretty(&payload) {
            Ok(raw) => println!("{raw}"),
            Err(err) => eprintln!("{DIM}  /send: could not render JSON: {err}.{RESET}"),
        }
        return;
    }
    println!("{BOLD}send message{RESET}");
    println!(
        "{DIM}  desktop:{RESET} /send <session> <message> can relay prompts into another open idle desktop session."
    );
    println!(
        "{DIM}  terminal:{RESET} this REPL has one active session and no desktop session registry to target."
    );
    if matches!(
        requested.to_ascii_lowercase().as_str(),
        "" | "status" | "state" | "show" | "list" | "targets"
    ) {
        println!(
            "{DIM}  target inspection:{RESET} use desktop /send status, /send targets, or /send list to list open idle/busy sessions."
        );
    } else if !requested.is_empty() {
        println!(
            "{DIM}  ignored target/message:{RESET} {}",
            requested.replace('\n', " ")
        );
    }
    println!(
        "{DIM}  usage:{RESET} /send status|targets|list|json|--json|status --json|state --json|show --json|list --json|targets --json|queued|queue --json|queued --json|pending --json|clear <id|target|all>|<session> <message> (also /send-message)"
    );
    println!(
        "{DIM}  remaining gap:{RESET} pi-level streaming child-agent bus or detached inter-agent scheduler."
    );
}

fn send_json_payload(query: &str) -> serde_json::Value {
    let query = query.trim();
    json!({
        "command": "send",
        "surface": "terminal",
        "query": query,
        "aliases": ["send", "send-message"],
        "active_session_only": true,
        "desktop_registry_available": false,
        "total_targets": 0,
        "queued_total": 0,
        "targets": [],
        "queued": [],
        "supported_actions": [
            "status",
            "targets",
            "list",
            "json",
            "--json",
            "status --json",
            "state --json",
            "show --json",
            "list --json",
            "targets --json",
            "queued",
            "queue --json",
            "queued --json",
            "pending --json",
            "clear <id|target|all>",
            "<session> <message>"
        ],
        "desktop_commands": [
            "/send status",
            "/send targets",
            "/send list",
            "/send json",
            "/send --json",
            "/send status --json",
            "/send state --json",
            "/send show --json",
            "/send list --json",
            "/send targets --json",
            "/send queued",
            "/send queue --json",
            "/send queued --json",
            "/send pending --json",
            "/send clear all",
            "/send-message status",
            "/send-message targets",
            "/send-message list",
            "/send-message json",
            "/send-message --json",
            "/send-message status --json",
            "/send-message state --json",
            "/send-message show --json",
            "/send-message list --json",
            "/send-message targets --json",
            "/send-message queued",
            "/send-message queue --json",
            "/send-message queued --json",
            "/send-message pending --json"
        ],
        "remaining_gap": "pi-level streaming child-agent bus or detached inter-agent scheduler"
    })
}

fn is_send_json_request(rest: &str) -> bool {
    matches!(
        rest.trim().to_ascii_lowercase().as_str(),
        "json"
            | "--json"
            | "status json"
            | "status --json"
            | "state json"
            | "state --json"
            | "show json"
            | "show --json"
            | "list json"
            | "list --json"
            | "targets json"
            | "targets --json"
            | "queue json"
            | "queue --json"
            | "queued json"
            | "queued --json"
            | "pending json"
            | "pending --json"
    )
}

fn print_theme_status(command: ThemeCommand, query: &str) {
    if command == ThemeCommand::Json {
        print_theme_status_json(query);
        return;
    }
    println!("{BOLD}theme{RESET}");
    println!(
        "{DIM}  desktop:{RESET} /theme system|dark|light|high-contrast updates the app appearance."
    );
    println!(
        "{DIM}  terminal:{RESET} colors are controlled by your terminal emulator; libertai code uses ANSI styling only."
    );
    match command {
        ThemeCommand::Status => {
            println!(
                "{DIM}  status aliases:{RESET} /theme status, /theme show, /theme current, /theme info, /theme json"
            );
        }
        ThemeCommand::Requested(requested) => {
            if !requested.is_empty() {
                println!("{DIM}  requested theme:{RESET} {requested}");
            }
        }
        ThemeCommand::Json => {}
    }
}

fn print_theme_status_json(query: &str) {
    match serde_json::to_string_pretty(&theme_json_payload(query)) {
        Ok(raw) => println!("{raw}"),
        Err(err) => eprintln!("{DIM}  /theme json: {err:#}{RESET}"),
    }
}

fn theme_json_payload(query: &str) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": "theme",
        "query": query.trim(),
        "aliases": ["theme"],
        "current": null,
        "resolved": null,
        "supported": ["system", "dark", "light", "high-contrast"],
        "terminal_mutates_theme": false,
        "desktop_settings_target": "Settings > Appearance",
        "supported_actions": [
            "status",
            "show",
            "current",
            "info",
            "json",
            "--json",
            "status --json",
            "show --json",
            "current --json",
            "info --json",
            "system",
            "dark",
            "light",
            "high-contrast"
        ],
        "note": "Terminal colors are controlled by the terminal emulator; desktop /theme changes app appearance."
    })
}

const VIM_USAGE: &str =
    "/vim [status|state|show|current|info|json|--json|status --json|state --json|show --json|current --json|info --json|on|enable|enabled|true|off|disable|disabled|false]";
const IDE_USAGE: &str =
    "/ide [status|state|show|json|--json|status --json|state --json|show --json|open|settings|edit]";
const BUG_USAGE: &str =
    "/bug [report|template|status|show|json|--json|status --json|show --json|template --json|report --json]";

fn print_vim_status(command: VimCommand, query: &str) {
    println!("{BOLD}vim{RESET}");
    match command {
        VimCommand::Status => {
            let enabled = VIM_INPUT_ENABLED.load(Ordering::SeqCst);
            println!(
                "{DIM}  status:{RESET} {}",
                if enabled { "on" } else { "off" }
            );
            println!(
                "{DIM}  terminal:{RESET} Vim input supports insert/normal mode: Esc, i/a/I/A, h/l/0/$, x, and Enter."
            );
        }
        VimCommand::Json => print_vim_json(query),
        VimCommand::Enable => {
            VIM_INPUT_ENABLED.store(true, Ordering::SeqCst);
            println!(
                "{DIM}  /vim on:{RESET} enabled for this terminal session."
            );
            println!(
                "{DIM}  terminal:{RESET} input starts in insert mode; press Esc for normal mode."
            );
        }
        VimCommand::Disable => {
            VIM_INPUT_ENABLED.store(false, Ordering::SeqCst);
            println!("{DIM}  /vim off:{RESET} disabled for this terminal session.");
        }
        VimCommand::Usage => {
            println!("{DIM}  usage:{RESET} {VIM_USAGE}");
        }
    }
}

fn vim_json_payload(query: &str) -> serde_json::Value {
    json!({
        "command": "vim",
        "surface": "terminal",
        "aliases": ["vim"],
        "query": query.trim(),
        "enabled": VIM_INPUT_ENABLED.load(Ordering::SeqCst),
        "mode": "insert",
        "supported_modes": ["insert", "normal"],
        "controls": ["Esc", "i", "a", "I", "A", "h", "l", "0", "$", "x", "Enter"],
        "supported_actions": ["status", "state", "show", "current", "info", "json", "--json", "status --json", "state --json", "show --json", "current --json", "info --json", "on", "enable", "enabled", "true", "off", "disable", "disabled", "false"],
    })
}

fn print_vim_json(query: &str) {
    match serde_json::to_string_pretty(&vim_json_payload(query)) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{DIM}  /vim json failed: {e}{RESET}"),
    }
}

fn vim_normal_key_action(code: KeyCode, modifiers: KeyModifiers) -> VimNormalAction {
    match (code, modifiers) {
        (KeyCode::Enter, _) => VimNormalAction::Submit,
        (KeyCode::Char('h'), KeyModifiers::NONE)
        | (KeyCode::Left, _) => VimNormalAction::MoveLeft,
        (KeyCode::Char('l'), KeyModifiers::NONE)
        | (KeyCode::Right, _) => VimNormalAction::MoveRight,
        (KeyCode::Char('0'), KeyModifiers::NONE)
        | (KeyCode::Home, _) => VimNormalAction::Home,
        (KeyCode::Char('$'), KeyModifiers::NONE | KeyModifiers::SHIFT)
        | (KeyCode::End, _) => VimNormalAction::End,
        (KeyCode::Char('x'), KeyModifiers::NONE)
        | (KeyCode::Delete, _) => VimNormalAction::Delete,
        (KeyCode::Char('i'), KeyModifiers::NONE) => VimNormalAction::InsertBefore,
        (KeyCode::Char('a'), KeyModifiers::NONE) => VimNormalAction::InsertAfter,
        (KeyCode::Char('I'), KeyModifiers::SHIFT)
        | (KeyCode::Char('I'), KeyModifiers::NONE) => VimNormalAction::InsertHome,
        (KeyCode::Char('A'), KeyModifiers::SHIFT)
        | (KeyCode::Char('A'), KeyModifiers::NONE) => VimNormalAction::InsertEnd,
        _ => VimNormalAction::None,
    }
}

fn print_ide_status(command: IdeCommand, query: &str) {
    println!("{BOLD}ide{RESET}");
    match command {
        IdeCommand::Status => {
            println!(
                "{DIM}  status:{RESET} no dedicated VS Code / JetBrains integration is bundled today."
            );
            println!(
                "{DIM}  terminal:{RESET} run libertai code inside your project, or use the desktop workspace for project navigation."
            );
        }
        IdeCommand::Json => print_ide_json(query),
        IdeCommand::Open => {
            println!(
                "{DIM}  /ide open:{RESET} no IDE bridge is available to open from the terminal CLI yet."
            );
            println!(
                "{DIM}  desktop:{RESET} use the desktop app workspace and external editor integration for project files."
            );
        }
        IdeCommand::Usage => {
            println!("{DIM}  usage:{RESET} {IDE_USAGE}");
        }
    }
}

fn ide_json_payload(query: &str) -> serde_json::Value {
    json!({
        "command": "ide",
        "surface": "terminal",
        "aliases": ["ide"],
        "query": query.trim(),
        "dedicated_ide_bridge": false,
        "supported_editors": [],
        "desktop_workspace_available": true,
        "terminal_guidance": "Run libertai code inside your project, or use the desktop workspace for project navigation.",
        "supported_actions": ["status", "state", "show", "json", "--json", "status --json", "state --json", "show --json", "open", "settings", "edit"],
    })
}

fn print_ide_json(query: &str) {
    match serde_json::to_string_pretty(&ide_json_payload(query)) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{DIM}  /ide json failed: {e}{RESET}"),
    }
}

fn print_bug_command(
    command: BugCommand,
    query: &str,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
) {
    match command {
        BugCommand::Template => print_bug_template(provider, model, mode, output_style),
        BugCommand::Json => print_bug_json(provider, model, mode, output_style, query),
        BugCommand::Usage => {
            println!("{BOLD}bug report{RESET}");
            println!("{DIM}  usage:{RESET} {BUG_USAGE}");
        }
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
        "list" | "status" | "state" => {
            if rest.is_empty() {
                ScheduleCommand::Status
            } else if rest == "--json" || rest == "json" {
                ScheduleCommand::Json
            } else {
                ScheduleCommand::Usage
            }
        }
        "json" => {
            if rest.is_empty() {
                ScheduleCommand::Json
            } else {
                ScheduleCommand::Usage
            }
        }
        "clear" | "stop" => ScheduleCommand::Clear,
        "show" | "inspect" => {
            let mut args = rest.split_whitespace();
            let Some(id) = args.next() else {
                return ScheduleCommand::Usage;
            };
            match (args.next(), args.next()) {
                (None, None) => ScheduleCommand::Show(id.to_string()),
                (Some("--json" | "json"), None) => ScheduleCommand::ShowJson(id.to_string()),
                _ => ScheduleCommand::Usage,
            }
        }
        "show-json" | "inspect-json" => {
            if rest.is_empty() || rest.split_whitespace().nth(1).is_some() {
                ScheduleCommand::Usage
            } else {
                ScheduleCommand::ShowJson(rest.to_string())
            }
        }
        "list-json" | "status-json" | "state-json" => {
            if rest.is_empty() {
                ScheduleCommand::Json
            } else {
                ScheduleCommand::Usage
            }
        }
        "run" | "now" | "trigger" => {
            if rest.is_empty() || rest.split_whitespace().nth(1).is_some() {
                ScheduleCommand::Usage
            } else {
                ScheduleCommand::Run(rest.to_string())
            }
        }
        "cancel" | "delete" | "rm" => {
            if rest.is_empty() || rest.split_whitespace().nth(1).is_some() {
                ScheduleCommand::Usage
            } else {
                ScheduleCommand::Cancel(rest.to_string())
            }
        }
        "in" => parse_schedule_add(rest),
        _ => {
            if raw == "--json" {
                ScheduleCommand::Json
            } else {
                parse_schedule_add(raw)
            }
        }
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
    let counts = schedule_status_counts(scheduled_runs, now);
    println!(
        "{DIM}  summary:{RESET} {} scheduled, {} due, {} pending",
        counts.total, counts.due, counts.pending
    );
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

fn print_schedule_details(scheduled_runs: &[ScheduledRun], id: &str) {
    println!("{BOLD}schedule: {id}{RESET}");
    let Some(run) = scheduled_runs.iter().find(|run| run.id == id) else {
        println!("{DIM}  no scheduled prompt found for {id}.{RESET}");
        return;
    };
    let now = Instant::now();
    let remaining = run.due_at.saturating_duration_since(now);
    let state = if run.due_at <= now { "due" } else { "pending" };
    println!("{DIM}  state:{RESET} {state}");
    println!(
        "{DIM}  due in:{RESET} {}",
        format_schedule_delay(remaining)
    );
    println!("{DIM}  due epoch ms:{RESET} {}", run.due_epoch_ms);
    println!("{DIM}  prompt:{RESET} {}", run.prompt.replace('\n', " "));
}

fn schedule_supported_actions() -> &'static [&'static str] {
    &[
        "list",
        "status",
        "state",
        "json",
        "--json",
        "list --json",
        "status --json",
        "state --json",
        "show <id>",
        "show <id> --json",
        "show-json <id>",
        "inspect <id>",
        "inspect <id> --json",
        "inspect-json <id>",
        "run <id>",
        "now <id>",
        "trigger <id>",
        "cancel <id>",
        "delete <id>",
        "rm <id>",
        "clear",
        "stop",
        "in <delay> <prompt>",
    ]
}

fn schedule_json_payload(
    scheduled_runs: &[ScheduledRun],
    now: Instant,
    query: &str,
) -> ScheduleJsonPayload {
    let query = query.trim();
    let counts = schedule_status_counts(scheduled_runs, now);
    let runs = scheduled_runs
        .iter()
        .map(|run| {
            let due_in = run.due_at.saturating_duration_since(now);
            ScheduleJsonRow {
                id: run.id.clone(),
                prompt: run.prompt.clone(),
                state: if run.due_at <= now {
                    "due".to_string()
                } else {
                    "pending".to_string()
                },
                due_epoch_ms: run.due_epoch_ms,
                due_in_ms: duration_millis_u64(due_in),
            }
        })
        .collect();
    ScheduleJsonPayload {
        surface: "terminal",
        command: "schedule",
        query: query.to_string(),
        aliases: &["schedule", "cron"],
        supported_actions: schedule_supported_actions(),
        total: counts.total,
        due: counts.due,
        pending: counts.pending,
        runs,
    }
}

fn print_schedule_json(scheduled_runs: &[ScheduledRun], query: &str, id: Option<&str>) {
    let now = Instant::now();
    let mut payload = schedule_json_payload(scheduled_runs, now, query);
    if let Some(id) = id {
        payload.runs.retain(|run| run.id == id);
        payload.total = payload.runs.len();
        payload.due = payload.runs.iter().filter(|run| run.state == "due").count();
        payload.pending = payload.runs.iter().filter(|run| run.state == "pending").count();
    }
    match serde_json::to_string_pretty(&payload) {
        Ok(raw) => println!("{raw}"),
        Err(err) => eprintln!("{DIM}  /schedule: could not render JSON: {err}.{RESET}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScheduleStatusCounts {
    total: usize,
    due: usize,
    pending: usize,
}

fn schedule_status_counts(scheduled_runs: &[ScheduledRun], now: Instant) -> ScheduleStatusCounts {
    let due = scheduled_runs
        .iter()
        .filter(|run| run.due_at <= now)
        .count();
    ScheduleStatusCounts {
        total: scheduled_runs.len(),
        due,
        pending: scheduled_runs.len().saturating_sub(due),
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
        "status" | "state" if rest == "json" || rest == "--json" => AutoCommand::Json,
        "status" | "state" => AutoCommand::Status,
        "json" | "--json" => AutoCommand::Json,
        "status-json" | "state-json" => AutoCommand::Json,
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

fn auto_supported_actions() -> &'static [&'static str] {
    &[
        "status",
        "state",
        "json",
        "--json",
        "status --json",
        "state --json",
        "status-json",
        "state-json",
        "on [turns] [goal]",
        "start [turns] [goal]",
        "run [turns] [goal]",
        "off",
        "stop",
        "cancel",
    ]
}

fn auto_json_payload(auto_run: Option<&AutoRun>, query: &str) -> AutoJsonPayload {
    let query = query.trim();
    match auto_run {
        Some(run) => AutoJsonPayload {
            surface: "terminal",
            command: "auto",
            query: query.to_string(),
            aliases: &["auto", "autorun", "continuous"],
            supported_actions: auto_supported_actions(),
            active: true,
            limit: run.limit,
            completed: run.completed,
            remaining: run.limit.saturating_sub(run.completed),
            goal: if run.goal.is_empty() {
                None
            } else {
                Some(run.goal.clone())
            },
        },
        None => AutoJsonPayload {
            surface: "terminal",
            command: "auto",
            query: query.to_string(),
            aliases: &["auto", "autorun", "continuous"],
            supported_actions: auto_supported_actions(),
            active: false,
            limit: 0,
            completed: 0,
            remaining: 0,
            goal: None,
        },
    }
}

fn print_auto_json(auto_run: Option<&AutoRun>, query: &str) {
    let payload = auto_json_payload(auto_run, query);
    match serde_json::to_string_pretty(&payload) {
        Ok(raw) => println!("{raw}"),
        Err(err) => eprintln!("{DIM}  /auto: could not render JSON: {err}.{RESET}"),
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

fn loop_json_request_arg(input: &str) -> Option<&str> {
    let raw = input.trim();
    if matches!(raw, "json" | "--json" | "status --json" | "state --json") {
        return Some("");
    }
    raw.strip_prefix("json ")
        .or_else(|| raw.strip_prefix("--json "))
        .or_else(|| raw.strip_prefix("status --json "))
        .or_else(|| raw.strip_prefix("state --json "))
        .map(str::trim)
}

fn loop_json_payload(input: &str) -> serde_json::Value {
    let request = parse_loop_request(input);
    let first_prompt = autonomous_loop_prompt(1, request.turns, &request.goal);
    json!({
        "surface": "terminal",
        "command": "loop",
        "query": input,
        "mode": "foreground",
        "detached": false,
        "default_turns": LOOP_DEFAULT_TURNS,
        "max_turns": LOOP_MAX_TURNS,
        "requested_turns": request.turns,
        "goal": if request.goal.is_empty() { None } else { Some(request.goal.as_str()) },
        "queued_on_run": request.turns,
        "first_prompt": first_prompt,
        "aliases": ["loop", "autoloop"],
        "supported_actions": [
            "json",
            "--json",
            "status --json",
            "state --json",
            "json [turns] [goal]",
            "--json [turns] [goal]",
            "[turns] [goal]"
        ],
    })
}

fn print_loop_json(input: &str) {
    let payload = loop_json_payload(input);
    match serde_json::to_string_pretty(&payload) {
        Ok(raw) => println!("{raw}"),
        Err(err) => eprintln!("{DIM}  /loop: could not render JSON: {err}.{RESET}"),
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

fn is_init_json_arg(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "json" | "--json" | "status --json" | "preview --json" | "show --json"
    )
}

fn print_init_project_json(notes: Option<&str>) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "surface": "cli",
                    "command": "init",
                    "available": false,
                    "error": format!("could not resolve cwd: {e}"),
                    "will_write": false,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            );
            return;
        }
    };
    let path = cwd.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).ok();
    let candidate = match crate::commands::code_init::agents_md_candidate(&cwd, notes) {
        Ok(candidate) => candidate,
        Err(e) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "surface": "cli",
                    "command": "init",
                    "available": false,
                    "error": format!("{e:#}"),
                    "path": path.display().to_string(),
                    "will_write": false,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            );
            return;
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&init_project_json_payload(
            "cli",
            &path,
            existing.as_deref(),
            &candidate,
            notes,
        ))
        .unwrap_or_else(|_| "{}".to_string())
    );
}

fn init_project_json_payload(
    surface: &str,
    path: &Path,
    existing: Option<&str>,
    candidate: &str,
    notes: Option<&str>,
) -> serde_json::Value {
    let sections = init_candidate_section_summaries(existing.unwrap_or_default(), candidate)
        .into_iter()
        .enumerate()
        .map(|(idx, section)| {
            json!({
                "index": idx + 1,
                "title": section.title,
                "impact": section.status,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "surface": surface,
        "command": "init",
        "available": true,
        "path": path.display().to_string(),
        "exists": existing.is_some(),
        "would_create": existing.is_none(),
        "will_write": false,
        "notes_supplied": notes.is_some_and(|notes| !notes.trim().is_empty()),
        "existing": existing.map(|content| json!({
            "bytes": content.len(),
            "lines": content.lines().count(),
        })),
        "candidate": {
            "content": candidate,
            "bytes": candidate.len(),
            "lines": candidate.lines().count(),
        },
        "sections": sections,
        "supported_actions": [
            "preview",
            "json",
            "--json",
            "status --json",
            "show --json",
            "preview --json",
            "project notes",
            "--agent",
            "from-agent",
        ],
    })
}

async fn apply_init_from_agent(handle: &AgentSessionHandle, action: InitFromAgentAction) {
    let json_output = matches!(action, InitFromAgentAction::Json);
    let messages = match handle.messages().await {
        Ok(messages) => messages,
        Err(e) => {
            if json_output {
                print_init_from_agent_json(None, None, None, Some(&format!("could not read transcript: {e:#}")));
            } else {
                eprintln!("{DIM}  /init from-agent: could not read transcript: {e:#}{RESET}");
            }
            return;
        }
    };
    let Some(text) = last_assistant_text(&messages) else {
        if json_output {
            print_init_from_agent_json(None, None, None, Some("no assistant response yet"));
        } else {
            println!("{DIM}  /init from-agent: no assistant response yet.{RESET}");
        }
        return;
    };
    let Some(candidate) = crate::commands::code_init::extract_agents_md_candidate(&text) else {
        if json_output {
            print_init_from_agent_json(
                None,
                None,
                None,
                Some("no fenced AGENTS.md candidate found in the latest assistant response"),
            );
        } else {
            println!(
                "{DIM}  /init from-agent: no fenced AGENTS.md candidate found in the latest assistant response.{RESET}"
            );
        }
        return;
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            if json_output {
                print_init_from_agent_json(None, None, Some(&candidate), Some(&format!("could not resolve cwd: {e}")));
            } else {
                eprintln!("{DIM}  /init from-agent: could not resolve cwd: {e}{RESET}");
            }
            return;
        }
    };
    let path = cwd.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    match &action {
        InitFromAgentAction::Json => {
            print_init_from_agent_json(Some(&path), Some(&existing), Some(&candidate), None);
        }
        InitFromAgentAction::Preview => {
            println!("{BOLD}init from-agent{RESET}");
            print!(
                "{}",
                init_candidate_preview(&path.display().to_string(), &existing, &candidate)
            );
            println!();
        }
        InitFromAgentAction::PreviewApply(mode) => {
            let content = match build_init_apply_content(&existing, &candidate, mode) {
                Ok(content) => content,
                Err(e) => {
                    eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                    return;
                }
            };
            println!("{BOLD}init from-agent {mode} preview{RESET}");
            print!(
                "{}",
                init_candidate_preview(&path.display().to_string(), &existing, &content)
            );
            println!();
        }
        InitFromAgentAction::PreviewSections(indexes) => {
            let selected = match selected_init_candidate_sections(&candidate, indexes) {
                Ok(selected) => selected,
                Err(e) => {
                    eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                    return;
                }
            };
            println!("{BOLD}init from-agent selected-section preview{RESET}");
            print!(
                "{}",
                init_candidate_preview(&path.display().to_string(), &existing, &selected)
            );
            println!();
        }
        InitFromAgentAction::PreviewApplySections(mode, indexes) => {
            let selected = match selected_init_candidate_sections(&candidate, indexes) {
                Ok(selected) => selected,
                Err(e) => {
                    eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                    return;
                }
            };
            let content = match build_init_apply_content(&existing, &selected, mode) {
                Ok(content) => content,
                Err(e) => {
                    eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                    return;
                }
            };
            println!("{BOLD}init from-agent selected-section {mode} preview{RESET}");
            print!(
                "{}",
                init_candidate_preview(&path.display().to_string(), &existing, &content)
            );
            println!();
        }
        InitFromAgentAction::Append
        | InitFromAgentAction::Merge
        | InitFromAgentAction::MergeLines
        | InitFromAgentAction::Replace
        | InitFromAgentAction::AppendSections(_)
        | InitFromAgentAction::MergeSections(_)
        | InitFromAgentAction::MergeLineSections(_) => {
            let mode = match &action {
                InitFromAgentAction::Append => "append",
                InitFromAgentAction::Merge => "merge",
                InitFromAgentAction::MergeLines => "merge-lines",
                InitFromAgentAction::Replace => "replace",
                InitFromAgentAction::AppendSections(_) => "append",
                InitFromAgentAction::MergeSections(_) => "merge",
                InitFromAgentAction::MergeLineSections(_) => "merge-lines",
                InitFromAgentAction::Preview
                | InitFromAgentAction::Json
                | InitFromAgentAction::PreviewApply(_)
                | InitFromAgentAction::PreviewSections(_)
                | InitFromAgentAction::PreviewApplySections(_, _) => unreachable!(),
            };
            let candidate = match &action {
                InitFromAgentAction::AppendSections(indexes)
                | InitFromAgentAction::MergeSections(indexes)
                | InitFromAgentAction::MergeLineSections(indexes) => {
                    match selected_init_candidate_sections(&candidate, indexes) {
                        Ok(selected) => selected,
                        Err(e) => {
                            eprintln!("{DIM}  /init from-agent: {e}{RESET}");
                            return;
                        }
                    }
                }
                _ => candidate.clone(),
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
                match &action {
                    InitFromAgentAction::Append => "appended to",
                    InitFromAgentAction::Merge => "merged into",
                    InitFromAgentAction::MergeLines => "line-merged into",
                    InitFromAgentAction::Replace => "replaced",
                    InitFromAgentAction::AppendSections(_) => "appended selected sections to",
                    InitFromAgentAction::MergeSections(_) => "merged selected sections into",
                    InitFromAgentAction::MergeLineSections(_) => {
                        "line-merged selected sections into"
                    }
                    InitFromAgentAction::Preview
                    | InitFromAgentAction::Json
                    | InitFromAgentAction::PreviewApply(_)
                    | InitFromAgentAction::PreviewSections(_)
                    | InitFromAgentAction::PreviewApplySections(_, _) => unreachable!(),
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

fn selected_init_candidate_sections(candidate: &str, indexes: &[usize]) -> Result<String, String> {
    let sections = split_init_markdown_sections(candidate);
    if sections.is_empty() {
        return Err("assistant candidate has no selectable markdown sections".to_string());
    }
    if indexes.len() == 1 && indexes[0] == 0 {
        return Ok(ensure_trailing_newline(&join_init_markdown_sections(&sections)));
    }
    let mut selected = Vec::new();
    for index in indexes {
        let Some(section) = sections.get(index.saturating_sub(1)) else {
            return Err(format!(
                "section index {index} is out of range; candidate has {} section{}",
                sections.len(),
                if sections.len() == 1 { "" } else { "s" }
            ));
        };
        selected.push(section.clone());
    }
    Ok(ensure_trailing_newline(&join_init_markdown_sections(&selected)))
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

fn print_onboarding_preview() {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /onboarding: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match crate::commands::code_init::onboarding_guide(&cwd) {
        Ok(guide) => println!("{guide}"),
        Err(e) => eprintln!("{DIM}  /onboarding: failed: {e:#}{RESET}"),
    }
}

fn print_onboarding_json(query: &str) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /onboarding: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match onboarding_json_payload(&cwd, query) {
        Ok(payload) => match serde_json::to_string_pretty(&payload) {
            Ok(body) => println!("{body}"),
            Err(e) => eprintln!("{DIM}  /onboarding json: {e:#}{RESET}"),
        },
        Err(e) => eprintln!("{DIM}  /onboarding json: {e:#}{RESET}"),
    }
}

fn onboarding_json_payload(cwd: &Path, query: &str) -> Result<serde_json::Value> {
    let guide = crate::commands::code_init::onboarding_guide(cwd)?;
    let suggested_path = PathBuf::from("libertai-onboarding.md");
    let first_heading = guide
        .lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .unwrap_or("onboarding");
    Ok(json!({
        "surface": "terminal",
        "command": "onboarding",
        "aliases": ["onboard"],
        "query": normalize_onboarding_arg(query),
        "cwd": cwd.display().to_string(),
        "suggested_path": suggested_path.display().to_string(),
        "suggested_gist_filename": "libertai-onboarding.md",
        "guide": {
            "bytes": guide.len(),
            "lines": guide.lines().count(),
            "first_heading": first_heading,
        },
        "will_write": false,
        "will_publish": false,
        "supported_actions": ["show", "preview", "save", "path", "gist", "json", "--json", "status --json", "show --json", "preview --json"],
    }))
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
    let sections = init_candidate_section_summaries(existing, candidate);
    if !sections.is_empty() {
        out.push_str("\n  candidate sections:\n");
        for (idx, section) in sections.iter().enumerate() {
            out.push_str(&format!("  {}. {} — {}\n", idx + 1, section.title, section.status));
        }
    }
    out.push_str("\n  Review the candidate against the existing AGENTS.md and merge only verified repo facts.\n");
    out
}

fn print_init_from_agent_json(
    path: Option<&Path>,
    existing: Option<&str>,
    candidate: Option<&str>,
    error: Option<&str>,
) {
    let sections = existing
        .zip(candidate)
        .map(|(existing, candidate)| {
            init_candidate_section_summaries(existing, candidate)
                .into_iter()
                .enumerate()
                .map(|(idx, section)| {
                    json!({
                        "index": idx + 1,
                        "title": section.title,
                        "impact": section.status,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let candidate_content = candidate.map(str::to_string);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "surface": "cli",
            "command": "init",
            "subcommand": "from-agent",
            "available": candidate.is_some() && error.is_none(),
            "error": error,
            "path": path.map(|path| path.display().to_string()),
            "candidate": candidate_content.as_ref().map(|content| json!({
                "content": content,
                "bytes": content.len(),
                "lines": content.lines().count(),
            })),
            "sections": sections,
            "will_write": false,
            "supported_actions": [
                "preview",
                "json",
                "status --json",
                "preview append",
                "preview merge",
                "preview merge-lines",
                "preview replace",
                "append",
                "merge",
                "merge-lines",
                "replace",
                "preview sections N[,M]",
                "preview sections N-M",
                "preview sections all",
                "append sections N[,M]",
                "append sections N-M",
                "append sections all",
                "merge sections N[,M]",
                "merge sections N-M",
                "merge sections all",
                "merge-lines sections N[,M]",
                "merge-lines sections N-M",
                "merge-lines sections all"
            ],
        }))
        .unwrap_or_else(|_| "{}".to_string())
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitSectionSummary {
    title: String,
    status: String,
}

fn init_candidate_section_summaries(existing: &str, candidate: &str) -> Vec<InitSectionSummary> {
    let existing_sections = split_init_markdown_sections(existing);
    split_init_markdown_sections(candidate)
        .into_iter()
        .filter(|section| !is_init_candidate_preamble(&section.content) || section.title.is_none())
        .map(|section| {
            let title = section
                .title
                .as_deref()
                .unwrap_or("Preamble")
                .trim()
                .to_string();
            let status = init_candidate_section_status(&existing_sections, &section);
            InitSectionSummary { title, status }
        })
        .collect()
}

fn init_candidate_section_status(
    existing_sections: &[InitMarkdownSection],
    candidate: &InitMarkdownSection,
) -> String {
    let Some(title) = candidate.title.as_deref() else {
        return "new preamble".to_string();
    };
    let Some(existing) = existing_sections.iter().find(|section| {
        section
            .title
            .as_deref()
            .is_some_and(|existing_title| existing_title.eq_ignore_ascii_case(title))
    }) else {
        return "new section".to_string();
    };
    let existing_lines = existing
        .content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.trim_start().starts_with("## "))
        .map(normalize_init_line)
        .collect::<std::collections::BTreeSet<_>>();
    let additions = candidate
        .content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.trim_start().starts_with("## "))
        .filter(|line| !existing_lines.contains(&normalize_init_line(line)))
        .count();
    if additions > 0 {
        return format!("adds {additions} line{}", if additions == 1 { "" } else { "s" });
    }
    if normalize_init_line(&existing.content) == normalize_init_line(&candidate.content) {
        "unchanged".to_string()
    } else {
        "reorders or rewrites existing lines".to_string()
    }
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
    if let Some(selector) = memory_file_selector(action) {
        match crate::commands::code_memory::list_memory_files(&cwd)
            .and_then(|files| read_memory_sidecar_selection(&files, selector))
        {
            Ok((file, content)) => print_memory_file(&file, &content),
            Err(e) => eprintln!("{DIM}  /memory file: failed: {e:#}{RESET}"),
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
    if is_memory_json_action(action) {
        print_memory_json(&doc, action);
        return;
    }
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

fn memory_file_selector(action: &str) -> Option<&str> {
    let trimmed = action.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next()?;
    if !matches!(command.to_ascii_lowercase().as_str(), "file" | "read" | "show-file") {
        return None;
    }
    parts.next().map(str::trim).filter(|source| !source.is_empty())
}

fn is_memory_json_action(action: &str) -> bool {
    matches!(
        action.trim().to_ascii_lowercase().as_str(),
        "json" | "--json" | "status --json" | "show --json"
    )
}

fn memory_supported_actions() -> &'static [&'static str] {
    &[
        "show",
        "status",
        "json",
        "status --json",
        "show --json",
        "--json",
        "path",
        "open",
        "edit",
        "editor",
        "files",
        "list",
        "file selector",
        "read selector",
        "show-file selector",
        "references",
        "refs",
        "verify",
        "clear",
        "import path",
        "import-claude",
        "migrate-claude",
        "claude",
        "import-claude-all",
        "migrate-claude-all",
        "claude-all",
    ]
}

fn read_memory_sidecar_selection(
    files: &[crate::commands::code_memory::MemoryFileEntry],
    selector: &str,
) -> Result<(crate::commands::code_memory::MemoryFileEntry, String)> {
    let Some(file) = select_memory_sidecar(files, selector) else {
        anyhow::bail!("no memory sidecar matched `{selector}`; run /memory files for indexes");
    };
    let meta = std::fs::metadata(&file.path)
        .with_context(|| format!("reading {}", file.path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("memory sidecar is not a file: {}", file.path.display());
    }
    if meta.len() > 256 * 1024 {
        anyhow::bail!("memory sidecar is too large; keep entries under 256 KiB");
    }
    let content = std::fs::read_to_string(&file.path)
        .with_context(|| format!("reading {}", file.path.display()))?;
    Ok((file.clone(), content))
}

fn select_memory_sidecar(
    files: &[crate::commands::code_memory::MemoryFileEntry],
    selector: &str,
) -> Option<crate::commands::code_memory::MemoryFileEntry> {
    let selector = selector.trim();
    if let Ok(index) = selector.parse::<usize>() {
        return index.checked_sub(1).and_then(|idx| files.get(idx)).cloned();
    }
    files
        .iter()
        .find(|file| {
            file.path.display().to_string() == selector
                || file.path.file_name().and_then(|name| name.to_str()) == Some(selector)
                || file.title.eq_ignore_ascii_case(selector)
        })
        .cloned()
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
    for (idx, file) in files.iter().enumerate() {
        println!(
            "{DIM}  {}. [{}]{RESET} {} - {}",
            idx + 1,
            file.kind.label(),
            file.path.display(),
            file.title
        );
    }
    println!("{DIM}  use /memory file <number|path> to inspect one entry{RESET}");
    println!();
}

fn print_memory_file(file: &crate::commands::code_memory::MemoryFileEntry, content: &str) {
    println!("{BOLD}memory file{RESET}");
    println!("{DIM}  kind:{RESET} {}", file.kind.label());
    println!("{DIM}  path:{RESET} {}", file.path.display());
    println!("{DIM}  title:{RESET} {}", file.title);
    println!();
    print!("{}", content);
    if !content.ends_with('\n') {
        println!();
    }
    println!();
}

fn memory_entry_counts(content: &str) -> (usize, usize, usize, usize) {
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
    (user, feedback, project, reference)
}

fn memory_json_payload(
    doc: &crate::commands::code_memory::MemoryDocument,
    action: &str,
) -> serde_json::Value {
    let (user, feedback, project, reference) = memory_entry_counts(&doc.content);
    let entry_count = user + feedback + project + reference;
    json!({
        "command": "memory",
        "surface": "terminal",
        "aliases": ["memory"],
        "query": action.trim(),
        "path": doc.path,
        "exists": doc.exists,
        "entry_count": entry_count,
        "entries": {
            "user": user,
            "feedback": feedback,
            "project": project,
            "reference": reference,
        },
        "content_bytes": doc.content.len(),
        "supported_actions": memory_supported_actions(),
    })
}

fn print_memory_json(doc: &crate::commands::code_memory::MemoryDocument, action: &str) {
    let payload = memory_json_payload(doc, action);
    match serde_json::to_string_pretty(&payload) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{DIM}  /memory json failed: {e}{RESET}"),
    }
}

fn print_memory_summary(content: &str) {
    let (user, feedback, project, reference) = memory_entry_counts(content);
    println!(
        "{DIM}  entries: user {user} · feedback {feedback} · project {project} · reference {reference}{RESET}"
    );
}

fn remember_json_note_arg(input: &str) -> Option<&str> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "json" | "--json" | "status --json" | "preview --json" | "show --json" => Some(""),
        _ if lower.starts_with("json ") => Some(raw[5..].trim()),
        _ if lower.starts_with("--json ") => Some(raw[7..].trim()),
        _ if lower.ends_with(" --json") => Some(raw[..raw.len() - 7].trim()),
        _ => None,
    }
}

fn remember_json_payload(cwd: &Path, input: &str) -> serde_json::Value {
    let parsed = crate::commands::code_memory::parse_memory_note(input);
    let memory = crate::commands::code_memory::read_memory(cwd).ok();
    let path = memory.as_ref().map(|doc| doc.path.clone());
    let valid = !parsed.text.trim().is_empty();
    json!({
        "command": "remember",
        "surface": "terminal",
        "input": input,
        "kind": parsed.kind.label(),
        "text": parsed.text,
        "valid": valid,
        "path": path,
        "entry_preview": if valid { json!(format!("[{}] {}", parsed.kind.label(), parsed.text)) } else { serde_json::Value::Null },
        "will_write": false,
        "supported_kinds": ["project", "user", "feedback", "reference"],
        "supported_actions": ["project: <text>", "user: <text>", "feedback: <text>", "reference: <text>", "json <text>", "--json <text>", "<text> --json", "status --json", "show --json", "preview --json"],
    })
}

fn print_remember_json(cwd: &Path, input: &str) {
    match serde_json::to_string_pretty(&remember_json_payload(cwd, input)) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{DIM}  /remember json failed: {e}{RESET}"),
    }
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

fn agents_supported_actions() -> &'static [&'static str] {
    &[
        "list",
        "status",
        "show",
        "json",
        "--json",
        "list --json",
        "status --json",
        "show --json",
        "show <name>",
        "show <name> --json",
        "open",
        "settings",
        "edit",
        "background",
        "bg",
        "background json",
        "background list --json",
        "background show <pid|run-id|latest>",
        "background show <pid|run-id|latest> --json",
        "background show-json <pid|run-id|latest>",
        "background log <pid|run-id|latest>",
        "background kill <pid|run-id|latest>",
        "background stop <pid|run-id|latest>",
        "background prune",
        "background clear",
        "create [--worktree|--same-cwd] <name> [description]",
        "delete <name>",
        "remove <name>",
    ]
}

fn agent_definition_json(
    agent: &crate::commands::code_agents::AgentDefinition,
) -> serde_json::Value {
    json!({
        "name": agent.name,
        "description": if agent.description.trim().is_empty() {
            "Named sub-agent"
        } else {
            agent.description.as_str()
        },
        "model": agent.model.as_deref().unwrap_or("default"),
        "tools": agent.tools.clone().unwrap_or_else(|| {
            vec!["read".to_string(), "grep".to_string(), "find".to_string(), "ls".to_string()]
        }),
        "worktree": agent.worktree,
        "source": agent_source_label(&agent.source),
        "path": agent_definition_path(agent),
        "system_prompt": agent.system_prompt,
    })
}

fn agents_json_payload(
    query: &str,
    cwd: &Path,
    agents: &[crate::commands::code_agents::AgentDefinition],
) -> serde_json::Value {
    let query = query.trim();
    let worktree_count = agents.iter().filter(|agent| agent.worktree).count();
    json!({
        "surface": "terminal",
        "command": "agents",
        "query": query,
        "aliases": ["agents"],
        "cwd": cwd,
        "count": agents.len(),
        "worktree_default_count": worktree_count,
        "same_cwd_count": agents.len().saturating_sub(worktree_count),
        "agents": agents.iter().map(agent_definition_json).collect::<Vec<_>>(),
        "will_write": false,
        "supported_actions": agents_supported_actions(),
    })
}

fn print_agents_json(query: &str) {
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
    match serde_json::to_string_pretty(&agents_json_payload(query, &cwd, &agents)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /agents: could not serialize JSON: {e:#}{RESET}"),
    }
}

const AGENTS_USAGE: &str = "/agents [list|status|show <name>|json|--json|list --json|status --json|show --json|show <name> --json|open|settings|edit|background|bg] | /agents background|bg [list|json|show|inspect|show-json|log|kill|stop [pid|run-id|latest]|prune|clear] | /agents create [--worktree|--same-cwd] <name> [description] | /agents delete|remove <name>";

fn handle_agents_command(input: &str) {
    match parse_agents_command(input) {
        AgentsSlashCommand::List => print_agents(),
        AgentsSlashCommand::ListJson => print_agents_json(input.trim()),
        AgentsSlashCommand::Show(rest) => print_agent_details(rest),
        AgentsSlashCommand::ShowJson(rest) => print_agent_details_json(rest),
        AgentsSlashCommand::Open => print_agents_open_hint(),
        AgentsSlashCommand::Create(rest) => create_agent_from_slash(rest),
        AgentsSlashCommand::Delete(rest) => delete_agent_from_slash(rest),
        AgentsSlashCommand::BackgroundList => print_background_agents(),
        AgentsSlashCommand::BackgroundListJson => print_background_agents_json(input.trim()),
        AgentsSlashCommand::BackgroundShow(rest) => print_background_agent_details(rest),
        AgentsSlashCommand::BackgroundShowJson(rest) => print_background_agent_details_json(rest),
        AgentsSlashCommand::BackgroundLog(rest) => print_background_agent_log(rest),
        AgentsSlashCommand::BackgroundKill(rest) => kill_background_agent(rest),
        AgentsSlashCommand::BackgroundPrune => prune_background_agents(),
        AgentsSlashCommand::Usage => {
            eprintln!("{DIM}  /agents: usage: {AGENTS_USAGE}{RESET}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentsSlashCommand<'a> {
    List,
    ListJson,
    Show(&'a str),
    ShowJson(&'a str),
    Open,
    Create(&'a str),
    Delete(&'a str),
    BackgroundList,
    BackgroundListJson,
    BackgroundShow(&'a str),
    BackgroundShowJson(&'a str),
    BackgroundLog(&'a str),
    BackgroundKill(&'a str),
    BackgroundPrune,
    Usage,
}

fn parse_agents_command(input: &str) -> AgentsSlashCommand<'_> {
    let raw = input.trim();
    if raw.is_empty() || raw == "list" || raw == "show" || raw == "status" {
        return AgentsSlashCommand::List;
    }
    if matches!(
        raw,
        "json" | "--json" | "list --json" | "status --json" | "show --json"
    ) {
        return AgentsSlashCommand::ListJson;
    }
    if let Some(rest) = raw.strip_prefix("show ") {
        if let Some(rest) = strip_trailing_json_flag(rest) {
            return AgentsSlashCommand::ShowJson(rest);
        }
        return AgentsSlashCommand::Show(rest.trim());
    }
    if matches!(raw, "open" | "settings" | "edit") {
        return AgentsSlashCommand::Open;
    }
    if raw == "background" || raw == "background list" || raw == "bg" || raw == "bg list" {
        return AgentsSlashCommand::BackgroundList;
    }
    if matches!(
        raw,
        "background json" | "bg json" | "background list --json" | "bg list --json"
    ) {
        return AgentsSlashCommand::BackgroundListJson;
    }
    if let Some(rest) = raw
        .strip_prefix("background show-json")
        .or_else(|| raw.strip_prefix("bg show-json"))
        .or_else(|| raw.strip_prefix("background inspect-json"))
        .or_else(|| raw.strip_prefix("bg inspect-json"))
    {
        return AgentsSlashCommand::BackgroundShowJson(rest.trim());
    }
    if let Some(rest) = raw
        .strip_prefix("background show")
        .or_else(|| raw.strip_prefix("bg show"))
        .or_else(|| raw.strip_prefix("background inspect"))
        .or_else(|| raw.strip_prefix("bg inspect"))
    {
        if let Some(rest) = strip_trailing_json_flag(rest) {
            return AgentsSlashCommand::BackgroundShowJson(rest);
        }
        return AgentsSlashCommand::BackgroundShow(rest.trim());
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
    if matches!(
        raw,
        "background prune" | "bg prune" | "background clear" | "bg clear"
    ) {
        return AgentsSlashCommand::BackgroundPrune;
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

fn strip_trailing_json_flag(input: &str) -> Option<&str> {
    let rest = input.trim();
    rest.strip_suffix(" --json").map(str::trim)
}

fn print_agent_details(input: &str) {
    let name = input.trim().trim_start_matches('@');
    if name.is_empty() || name.split_whitespace().count() != 1 {
        eprintln!("{DIM}  /agents: usage: /agents show <name>{RESET}");
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /agents: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let agent = match crate::commands::code_agents::find_agent(&cwd, name) {
        Ok(Some(agent)) => agent,
        Ok(None) => {
            print_agent_missing_json(name, &cwd);
            return;
        }
        Err(e) => {
            eprintln!("{DIM}  /agents: failed: {e:#}{RESET}");
            return;
        }
    };
    print!("{}", format_agent_details(&agent));
}

fn print_agent_details_json(input: &str) {
    let name = input.trim().trim_start_matches('@');
    if name.is_empty() || name.split_whitespace().count() != 1 {
        eprintln!("{DIM}  /agents: usage: /agents show <name> --json{RESET}");
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /agents: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let agent = match crate::commands::code_agents::find_agent(&cwd, name) {
        Ok(Some(agent)) => agent,
        Ok(None) => {
            eprintln!("{DIM}  /agents: no named sub-agent found for `{name}`{RESET}");
            return;
        }
        Err(e) => {
            eprintln!("{DIM}  /agents: failed: {e:#}{RESET}");
            return;
        }
    };
    let payload = json!({
        "surface": "terminal",
        "command": "agents",
        "query": format!("show {name} --json"),
        "aliases": ["agents"],
        "cwd": cwd,
        "agent": agent_definition_json(&agent),
        "will_write": false,
        "supported_actions": agents_supported_actions(),
    });
    match serde_json::to_string_pretty(&payload) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /agents: could not serialize JSON: {e:#}{RESET}"),
    }
}

fn agent_missing_json_payload(name: &str, cwd: &Path) -> serde_json::Value {
    json!({
        "surface": "terminal",
        "command": "agents",
        "query": format!("show {name} --json"),
        "aliases": ["agents"],
        "cwd": cwd,
        "error": "not_found",
        "name": name,
        "will_write": false,
        "supported_actions": agents_supported_actions(),
    })
}

fn print_agent_missing_json(name: &str, cwd: &Path) {
    match serde_json::to_string_pretty(&agent_missing_json_payload(name, cwd)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /agents: could not serialize JSON: {e:#}{RESET}"),
    }
}

fn format_agent_details(agent: &crate::commands::code_agents::AgentDefinition) -> String {
    let tools = agent
        .tools
        .as_ref()
        .filter(|tools| !tools.is_empty())
        .map(|tools| tools.join(", "))
        .unwrap_or_else(|| "read, grep, find, ls".to_string());
    let model = agent.model.as_deref().unwrap_or("default");
    let isolation = if agent.worktree {
        "worktree"
    } else {
        "same cwd"
    };
    let path = agent_definition_path(agent);
    format!(
        "{BOLD}agent: {}{RESET}\n  description: {}\n  model: {model}\n  tools: {tools}\n  isolation: {isolation}\n  source: {}\n  path: {}\n\n{BOLD}prompt preview{RESET}\n{}\n\n{DIM}  run /agent {} <task> to dispatch this sub-agent.{RESET}\n\n",
        agent.name,
        if agent.description.trim().is_empty() {
            "Named sub-agent"
        } else {
            agent.description.as_str()
        },
        agent_source_label(&agent.source),
        path.display(),
        agent_prompt_preview(&agent.system_prompt, 12, 900),
        agent.name,
    )
}

fn agent_definition_path(agent: &crate::commands::code_agents::AgentDefinition) -> PathBuf {
    let dir = match &agent.source {
        crate::commands::code_agents::AgentSource::Project(path)
        | crate::commands::code_agents::AgentSource::User(path) => path,
    };
    dir.join(format!("{}.md", agent.name))
}

fn agent_prompt_preview(prompt: &str, max_lines: usize, max_chars: usize) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut lines = 0usize;
    let total_lines = prompt.lines().count();
    for line in prompt.lines().take(max_lines) {
        if chars >= max_chars {
            break;
        }
        if lines > 0 {
            out.push('\n');
            chars += 1;
        }
        let remaining = max_chars.saturating_sub(chars);
        let piece = truncate_chars(line.trim_end(), remaining);
        chars += piece.chars().count();
        out.push_str(&piece);
        lines += 1;
    }
    if total_lines > lines || prompt.chars().count() > chars {
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str("...");
    }
    if out.trim().is_empty() {
        "(empty)".to_string()
    } else {
        out
    }
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
            let counts = background_agent_status_counts(&records, background_agent_status);
            println!(
                "{DIM}  summary:{RESET} {} recorded, {} running, {} exited, {} unknown",
                counts.total, counts.running, counts.exited, counts.unknown
            );
            for record in records.iter().rev().take(20) {
                let status = background_agent_status(record.pid);
                println!(
                    "- {} · pid {} [{}] {} — {}",
                    background_agent_record_id(record),
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
            println!(
                "{DIM}  /agents background log [pid|run-id|latest] shows the saved output.{RESET}"
            );
            println!(
                "{DIM}  /agents background show [pid|run-id|latest] inspects one recorded run.{RESET}"
            );
            println!("{DIM}  /agents background json prints machine-readable status.{RESET}");
            println!("{DIM}  /agents background kill [pid|run-id|latest] stops a running background agent.{RESET}");
            println!("{DIM}  /agents background prune removes exited records from the list.{RESET}");
        }
        Err(e) => eprintln!("{DIM}  /agents: could not read background agents: {e:#}{RESET}"),
    }
}

fn print_background_agents_json(query: &str) {
    match load_background_agent_records() {
        Ok(records) => {
            let counts = background_agent_status_counts(&records, background_agent_status);
            let payload = BackgroundAgentListJson {
                surface: "terminal",
                command: "agents background",
                query: query.trim(),
                aliases: &["agents background", "agents bg"],
                supported_actions: background_agents_supported_actions(),
                counts,
                records: records
                    .iter()
                    .rev()
                    .map(|record| BackgroundAgentRecordJson {
                        record,
                        status: background_agent_status(record.pid).label(),
                    })
                    .collect(),
            };
            match serde_json::to_string_pretty(&payload) {
                Ok(raw) => println!("{raw}"),
                Err(e) => eprintln!(
                    "{DIM}  /agents: could not serialize background agents: {e:#}{RESET}"
                ),
            }
        }
        Err(e) => eprintln!("{DIM}  /agents: could not read background agents: {e:#}{RESET}"),
    }
}

fn print_background_agent_details(input: &str) {
    match resolve_background_agent_record(input.trim()) {
        Ok(Some(record)) => {
            let status = background_agent_status(record.pid);
            println!("{}", format_background_agent_details(&record, status));
        }
        Ok(None) => eprintln!("{DIM}  /agents: no matching background agent found{RESET}"),
        Err(e) => eprintln!("{DIM}  /agents: {e:#}{RESET}"),
    }
}

fn print_background_agent_details_json(input: &str) {
    let query = input.trim();
    let records = match load_background_agent_records() {
        Ok(records) => records,
        Err(e) => {
            eprintln!("{DIM}  /agents: could not read background agents: {e:#}{RESET}");
            return;
        }
    };
    match resolve_background_agent_record_from_records(records.clone(), query) {
        Ok(Some(record)) => {
            let status = background_agent_status(record.pid);
            let payload = BackgroundAgentDetailsJson {
                surface: "terminal",
                command: "agents background show",
                query,
                aliases: &["agents background show", "agents bg show"],
                supported_actions: background_agents_supported_actions(),
                record: &record,
                status: status.label(),
            };
            match serde_json::to_string_pretty(&payload) {
                Ok(raw) => println!("{raw}"),
                Err(e) => eprintln!(
                    "{DIM}  /agents: could not serialize background agent: {e:#}{RESET}"
                ),
            }
        }
        Ok(None) => {
            let payload = BackgroundAgentMissingJson {
                surface: "terminal",
                command: "agents background show",
                query,
                aliases: &["agents background show", "agents bg show"],
                supported_actions: background_agents_supported_actions(),
                error: "not_found",
                counts: background_agent_status_counts(&records, background_agent_status),
            };
            match serde_json::to_string_pretty(&payload) {
                Ok(raw) => println!("{raw}"),
                Err(e) => eprintln!(
                    "{DIM}  /agents: could not serialize background agent miss: {e:#}{RESET}"
                ),
            }
        }
        Err(e) => eprintln!("{DIM}  /agents: {e:#}{RESET}"),
    }
}

fn format_background_agent_details(
    record: &BackgroundAgentRecord,
    status: BackgroundAgentStatus,
) -> String {
    [
        format!("{BOLD}background agent: pid {}{RESET}", record.pid),
        format!("{DIM}  run id:{RESET} {}", background_agent_record_id(record)),
        format!("{DIM}  status:{RESET} {}", status.label()),
        format!("{DIM}  name:{RESET} {}", record.name),
        format!("{DIM}  provider:{RESET} {}", display_or_dash(&record.provider)),
        format!("{DIM}  model:{RESET} {}", display_or_dash(&record.model)),
        format!("{DIM}  mode:{RESET} {}", display_or_dash(&record.mode)),
        format!(
            "{DIM}  started:{RESET} {}",
            format_epoch_ms(record.started_at_ms)
        ),
        format!("{DIM}  cwd:{RESET} {}", record.cwd),
        format!("{DIM}  log:{RESET} {}", record.log_path),
        format!(
            "{DIM}  command:{RESET} {}",
            format_background_agent_command(record)
        ),
        format!("{DIM}  prompt:{RESET} {}", record.prompt_preview),
    ]
    .join("\n")
}

#[derive(Debug, Serialize)]
struct BackgroundAgentListJson<'a> {
    surface: &'static str,
    command: &'static str,
    query: &'a str,
    aliases: &'static [&'static str],
    supported_actions: &'static [&'static str],
    counts: BackgroundAgentStatusCounts,
    records: Vec<BackgroundAgentRecordJson<'a>>,
}

#[derive(Debug, Serialize)]
struct BackgroundAgentRecordJson<'a> {
    #[serde(flatten)]
    record: &'a BackgroundAgentRecord,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct BackgroundAgentDetailsJson<'a> {
    surface: &'static str,
    command: &'static str,
    query: &'a str,
    aliases: &'static [&'static str],
    supported_actions: &'static [&'static str],
    #[serde(flatten)]
    record: &'a BackgroundAgentRecord,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct BackgroundAgentMissingJson<'a> {
    surface: &'static str,
    command: &'static str,
    query: &'a str,
    aliases: &'static [&'static str],
    supported_actions: &'static [&'static str],
    error: &'static str,
    counts: BackgroundAgentStatusCounts,
}

fn background_agents_supported_actions() -> &'static [&'static str] {
    &[
        "list",
        "json",
        "list --json",
        "show <pid|run-id|latest>",
        "show <pid|run-id|latest> --json",
        "show-json <pid|run-id|latest>",
        "inspect <pid|run-id|latest>",
        "inspect <pid|run-id|latest> --json",
        "log <pid|run-id|latest>",
        "kill <pid|run-id|latest>",
        "stop <pid|run-id|latest>",
        "prune",
        "clear",
    ]
}

fn format_background_agent_command(record: &BackgroundAgentRecord) -> String {
    if record.launched_argv.is_empty() {
        return "-".to_string();
    }
    record
        .launched_argv
        .iter()
        .map(|arg| quote_sh_string(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_or_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value
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
    let record = match resolve_background_agent_record(input.trim()) {
        Ok(Some(record)) => record,
        Ok(None) => {
            eprintln!("{DIM}  /agents: no matching background agent found{RESET}");
            return;
        }
        Err(e) => {
            eprintln!("{DIM}  /agents: {e:#}{RESET}");
            return;
        }
    };
    let pid = record.pid;
    match send_background_agent_kill(pid) {
        Ok(()) => println!(
            "{DIM}  sent terminate signal to background agent {} (pid {pid}).{RESET}",
            background_agent_record_id(&record)
        ),
        Err(e) => eprintln!("{DIM}  /agents: could not stop pid {pid}: {e:#}{RESET}"),
    }
}

fn prune_background_agents() {
    match load_background_agent_records() {
        Ok(records) if records.is_empty() => {
            println!("{DIM}  /agents background prune: no terminal background agents recorded.{RESET}");
        }
        Ok(records) => {
            let original = records.len();
            let kept = retain_running_background_agent_records(records, background_agent_status);
            let removed = original.saturating_sub(kept.len());
            match rewrite_background_agent_records(&kept) {
                Ok(()) => {
                    println!(
                        "{DIM}  /agents background prune: removed {removed} non-running record{}; kept {} running record{}.{RESET}",
                        if removed == 1 { "" } else { "s" },
                        kept.len(),
                        if kept.len() == 1 { "" } else { "s" }
                    );
                }
                Err(e) => eprintln!("{DIM}  /agents: could not prune records: {e:#}{RESET}"),
            }
        }
        Err(e) => eprintln!("{DIM}  /agents: could not read background agents: {e:#}{RESET}"),
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
        println!("{DIM}  create .claude/commands/<name>.md or .claude/skills/<name>/SKILL.md.{RESET}");
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
            let slash_name = custom_slash_invocation_name(&t);
            let hint = t
                .arg_hint
                .as_ref()
                .map(|h| format!(" · args: {h}"))
                .unwrap_or_default();
            println!("- /{slash_name}: {}{}", desc, hint);
        }
        println!("{DIM}  run /template <name> [args], or /<name> [args].{RESET}");
    }
    println!();
}

fn is_template_json_arg(input: &str) -> bool {
    matches!(
        normalize_template_arg(input).as_str(),
        "json" | "--json" | "status --json" | "list --json" | "show --json"
    )
}

fn is_template_list_arg(input: &str) -> bool {
    matches!(normalize_template_arg(input).as_str(), "list" | "show")
}

fn normalize_template_arg(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn print_templates_json(query: &str) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /template: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match serde_json::to_string_pretty(&template_json_payload(&cwd, query)) {
        Ok(body) => println!("{body}"),
        Err(e) => eprintln!("{DIM}  /template: could not render JSON: {e}{RESET}"),
    }
}

fn template_json_payload(cwd: &Path, query: &str) -> serde_json::Value {
    let templates = crate::commands::code_slash_registry::discover(cwd);
    let rows: Vec<serde_json::Value> = templates
        .iter()
        .map(|template| {
            let source = match template.source {
                crate::commands::code_slash_registry::CommandSource::Project => "project",
                crate::commands::code_slash_registry::CommandSource::User => "user",
            };
            json!({
                "name": template.name,
                "invocation": custom_slash_invocation_name(template),
                "description": template.description,
                "source": source,
                "namespace": template.namespace,
                "path": template.path.display().to_string(),
                "arg_hint": template.arg_hint,
                "argument_names": template.argument_names,
            })
        })
        .collect();
    json!({
        "surface": "terminal",
        "command": "template",
        "query": query.trim(),
        "aliases": ["template"],
        "cwd": cwd.display().to_string(),
        "count": rows.len(),
        "templates": rows,
        "will_write": false,
        "supported_actions": ["list", "show", "json", "--json", "status --json", "list --json", "show --json", "<name> [args]"],
    })
}

fn handle_skills_slash(query: &str) -> Result<()> {
    match parse_skills_command(query)? {
        SkillsCommand::List => print_code_skills(),
        SkillsCommand::Json => print_code_skills_json(query),
        SkillsCommand::Show(name) => print_code_skill_details(&name),
        SkillsCommand::ShowJson(name) => print_code_skill_details_json(&name),
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
    if matches!(
        normalize_help_command_arg(raw).as_str(),
        "json" | "--json" | "status --json" | "list --json" | "show --json"
    ) {
        return Ok(SkillsCommand::Json);
    }
    if let Some(name) = raw.strip_prefix("show ") {
        let name = name.trim().trim_start_matches('@');
        if let Some(name) = name.strip_suffix(" --json").map(str::trim) {
            if name.is_empty() || name.split_whitespace().count() != 1 {
                anyhow::bail!("usage: /skills show <name> --json");
            }
            return Ok(SkillsCommand::ShowJson(name.to_string()));
        }
        if name.is_empty() || name.split_whitespace().count() != 1 {
            anyhow::bail!("usage: /skills show <name>");
        }
        return Ok(SkillsCommand::Show(name.to_string()));
    }
    if raw.eq_ignore_ascii_case("open")
        || raw.eq_ignore_ascii_case("settings")
        || raw.eq_ignore_ascii_case("edit")
    {
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
        _ => anyhow::bail!(
            "usage: /skills [list|status|show|json|--json|status --json|list --json|show --json|show <name>|show <name> --json|open|settings|edit|enable|on <name>|disable|off <name>]"
        ),
    }
}

fn code_skills_json_payload(
    cwd: &Path,
    query: &str,
    skills: Vec<code_skills::SkillInventoryEntry>,
) -> serde_json::Value {
    let enabled = skills.iter().filter(|skill| skill.enabled).count();
    let rows: Vec<serde_json::Value> = skills
        .into_iter()
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "enabled": skill.enabled,
                "allowed_tools": skill.allowed_tools,
                "source": skill.source,
                "source_kind": skill.source_kind,
                "path": skill.path.map(|path| path.display().to_string()),
                "agent_created": skill.agent_created,
            })
        })
        .collect();
    json!({
        "surface": "terminal",
        "command": "skills",
        "query": query.trim(),
        "aliases": ["skills"],
        "cwd": cwd.display().to_string(),
        "count": rows.len(),
        "enabled_count": enabled,
        "disabled_count": rows.len().saturating_sub(enabled),
        "skills": rows,
        "will_write": false,
        "supported_actions": ["list", "status", "show", "json", "--json", "status --json", "list --json", "show --json", "show <name>", "show <name> --json", "open", "settings", "edit", "enable <name>", "on <name>", "disable <name>", "off <name>"],
    })
}

fn print_code_skills_json(query: &str) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /skills json: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    let skills = match code_skills::skill_inventory(SkillPillar::Code, Some(&cwd)) {
        Ok(skills) => skills,
        Err(e) => {
            eprintln!("{DIM}  /skills json: failed: {e:#}{RESET}");
            return;
        }
    };
    match serde_json::to_string_pretty(&code_skills_json_payload(&cwd, query, skills)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /skills json failed: {e}{RESET}"),
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

fn print_code_skill_details(name: &str) {
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
    let Some(skill) = skills
        .iter()
        .find(|skill| skill.name == name)
        .or_else(|| skills.iter().find(|skill| skill.name.starts_with(name)))
    else {
        eprintln!("{DIM}  /skills: no skill found for `{name}`{RESET}");
        return;
    };
    print!("{}", format_code_skill_details(skill));
}

fn code_skill_detail_json_payload(
    cwd: &Path,
    query_name: &str,
    skill: Option<&code_skills::SkillInventoryEntry>,
) -> serde_json::Value {
    let mut payload = json!({
        "surface": "terminal",
        "command": "skills",
        "query": format!("show {} --json", query_name.trim()),
        "aliases": ["skills"],
        "cwd": cwd.display().to_string(),
        "name": query_name.trim(),
        "will_write": false,
        "supported_actions": ["list", "status", "show", "json", "--json", "status --json", "list --json", "show --json", "show <name>", "show <name> --json", "open", "settings", "edit", "enable <name>", "on <name>", "disable <name>", "off <name>"],
    });
    if let Some(skill) = skill {
        payload["skill"] = json!({
            "name": skill.name,
            "description": skill.description,
            "enabled": skill.enabled,
            "allowed_tools": skill.allowed_tools,
            "source": skill.source,
            "source_kind": skill.source_kind,
            "path": skill.path.as_ref().map(|path| path.display().to_string()),
            "agent_created": skill.agent_created,
            "instruction_preview": skill_prompt_preview(&skill.body, 16, 1200),
        });
    } else {
        payload["error"] = json!("not_found");
    }
    payload
}

fn print_code_skill_details_json(name: &str) {
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
    let skill = skills
        .iter()
        .find(|skill| skill.name == name)
        .or_else(|| skills.iter().find(|skill| skill.name.starts_with(name)));
    match serde_json::to_string_pretty(&code_skill_detail_json_payload(&cwd, name, skill)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /skills json failed: {e}{RESET}"),
    }
}

fn format_code_skill_details(skill: &code_skills::SkillInventoryEntry) -> String {
    let state = if skill.enabled { "on" } else { "off" };
    let tools = skill
        .allowed_tools
        .as_ref()
        .filter(|tools| !tools.trim().is_empty())
        .map(|tools| tools.trim().to_string())
        .unwrap_or_else(|| "not restricted".to_string());
    let path = skill
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(built-in)".to_string());
    let created = if skill.agent_created { "yes" } else { "no" };
    format!(
        "{BOLD}skill: {}{RESET}\n  state: {state}\n  description: {}\n  tools: {tools}\n  source: {}\n  path: {path}\n  agent-created: {created}\n\n{BOLD}instruction preview{RESET}\n{}\n\n{DIM}  changes apply to new sessions; use /skills enable|disable {} to toggle it.{RESET}\n\n",
        skill.name,
        skill.description,
        skill.source,
        skill_prompt_preview(&skill.body, 16, 1200),
        skill.name,
    )
}

fn skill_prompt_preview(body: &str, max_lines: usize, max_chars: usize) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    let mut out = String::new();
    let mut chars = 0usize;
    let mut lines = 0usize;
    let total_lines = trimmed.lines().count();
    for line in trimmed.lines().take(max_lines) {
        if chars >= max_chars {
            break;
        }
        if lines > 0 {
            out.push('\n');
            chars += 1;
        }
        let remaining = max_chars.saturating_sub(chars);
        let piece = truncate_chars(line.trim_end(), remaining);
        chars += piece.chars().count();
        out.push_str(&piece);
        lines += 1;
    }
    if total_lines > lines || trimmed.chars().count() > chars {
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str("...");
    }
    out
}

fn print_code_skills_open_hint() {
    let cwd = std::env::current_dir().ok();
    println!("{BOLD}skills{RESET}");
    println!("{DIM}  desktop: /skills open jumps to Settings > Skills.{RESET}");
    if let Some(cwd) = cwd {
        println!(
            "{DIM}  terminal: project skills are read from {}, {}, and {}{RESET}",
            cwd.join(".claude/skills").display(),
            cwd.join(".libertai/skills").display(),
            cwd.join(".agents/skills").display()
        );
    } else {
        println!("{DIM}  terminal: project skills are read from .claude/skills, .libertai/skills, and .agents/skills.{RESET}");
    }
    println!("{DIM}  user skills live under ~/.claude/skills or ~/.config/libertai/skills.{RESET}");
    println!("{DIM}  use /skills list, /skills enable <name>, or /skills disable <name>.{RESET}");
    println!();
}

async fn build_template_slash_prompt(query: &str, handle: &AgentSessionHandle) -> Result<String> {
    let (name, args) = parse_template_query(query)?;
    let Some(prompt) = build_custom_slash_prompt(name, args, handle).await? else {
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
    let trimmed = input.trim();
    let action = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
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
        "submit" | "publish" => {
            let review = parse_pr_comments_draft_submit_review(trimmed);
            match review {
                Ok(review) => submit_pr_comment_drafts(drafts, review),
                Err(e) => eprintln!("{DIM}  /pr_comments: {e:#}{RESET}"),
            }
        }
        _ => eprintln!(
            "{DIM}  usage: /pr_comments draft <path>:<line> <body>, /pr_comments drafts, /pr_comments drafts submit [approve|comment|request_changes] [body], or /pr_comments drafts clear{RESET}"
        ),
    }
}

fn parse_pr_comments_draft_submit_review(input: &str) -> Result<Option<(&str, &str)>> {
    let trimmed = input.trim();
    let rest = trimmed
        .strip_prefix("submit")
        .or_else(|| trimmed.strip_prefix("publish"))
        .map(str::trim)
        .unwrap_or("");
    if rest.is_empty() {
        return Ok(None);
    }
    let (event, body) = parse_pr_comments_review(rest)?;
    if body.is_empty()
        && !matches!(
            event.trim().to_ascii_lowercase().as_str(),
            "approve" | "approved" | "approval"
        )
    {
        anyhow::bail!(
            "usage: /pr_comments drafts submit <approve|comment|request_changes> [body]"
        );
    }
    Ok(Some((event, body)))
}

fn submit_pr_comment_drafts(drafts: &mut Vec<PrCommentDraft>, review: Option<(&str, &str)>) {
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
    let Some((event, body)) = review else {
        return;
    };
    if succeeded == 0 {
        eprintln!(
            "{DIM}  /pr_comments: skipped review submit because no draft threads were submitted.{RESET}"
        );
        return;
    }
    let capture = crate::commands::code_pr_comments::submit_pull_request_review(
        &cwd, "", event, body,
    );
    if capture.error.is_none() && capture.status == Some(0) {
        println!("{DIM}  /pr_comments drafts: submitted PR review: {event}{RESET}");
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
    eprintln!("{DIM}  /pr_comments: review submit failed after drafts: {detail}{RESET}");
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
    if name.is_empty() || name.split('/').any(str::is_empty) {
        None
    } else {
        Some((name, args))
    }
}

fn custom_slash_invocation_name(cmd: &crate::commands::code_slash_registry::CustomCommand) -> String {
    cmd.namespace
        .as_deref()
        .filter(|namespace| !namespace.trim().is_empty())
        .map(|namespace| format!("{namespace}/{}", cmd.name))
        .unwrap_or_else(|| cmd.name.clone())
}

fn custom_slash_matches(
    cmd: &crate::commands::code_slash_registry::CustomCommand,
    needle: &str,
) -> bool {
    cmd.name == needle || custom_slash_invocation_name(cmd).eq_ignore_ascii_case(needle)
}

fn custom_slash_starts_with(
    cmd: &crate::commands::code_slash_registry::CustomCommand,
    needle: &str,
) -> bool {
    cmd.name.starts_with(needle)
        || custom_slash_invocation_name(cmd)
            .to_ascii_lowercase()
            .starts_with(needle)
}

async fn build_custom_slash_prompt(
    name: &str,
    args: &str,
    handle: &AgentSessionHandle,
) -> Result<Option<String>> {
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let templates = crate::commands::code_slash_registry::discover(&cwd);
    let needle = name.trim().to_lowercase();
    let Some(hit) = templates
        .iter()
        .find(|cmd| custom_slash_matches(cmd, &needle))
        .or_else(|| templates.iter().find(|cmd| custom_slash_starts_with(cmd, &needle)))
    else {
        return Ok(None);
    };
    let context = slash_expansion_context(handle).await;
    Ok(Some(crate::commands::code_slash_registry::expand_with_context(
        hit, args, &context,
    )))
}

async fn slash_expansion_context(
    handle: &AgentSessionHandle,
) -> crate::commands::code_slash_registry::ExpansionContext {
    match handle.state().await {
        Ok(state) => crate::commands::code_slash_registry::ExpansionContext {
            session_id: state.session_id,
            effort: state.thinking_level.map(|level| level.to_string()),
        },
        Err(_) => crate::commands::code_slash_registry::ExpansionContext::default(),
    }
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
    #[serde(default)]
    run_id: String,
    name: String,
    provider: String,
    model: String,
    mode: String,
    prompt_preview: String,
    cwd: String,
    log_path: String,
    started_at_ms: u64,
    #[serde(default)]
    launched_argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundAgentStatus {
    Running,
    Exited,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct BackgroundAgentStatusCounts {
    total: usize,
    running: usize,
    exited: usize,
    unknown: usize,
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

fn background_agent_status_counts(
    records: &[BackgroundAgentRecord],
    status: impl Fn(u32) -> BackgroundAgentStatus,
) -> BackgroundAgentStatusCounts {
    let mut counts = BackgroundAgentStatusCounts {
        total: records.len(),
        running: 0,
        exited: 0,
        unknown: 0,
    };
    for record in records {
        match status(record.pid) {
            BackgroundAgentStatus::Running => counts.running += 1,
            BackgroundAgentStatus::Exited => counts.exited += 1,
            BackgroundAgentStatus::Unknown => counts.unknown += 1,
        }
    }
    counts
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
    persist_background_agent_record(&background_agent_record(launch, &started, &exe))?;
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
    exe: &Path,
) -> BackgroundAgentRecord {
    let mut launched_argv = vec![exe.display().to_string()];
    launched_argv.extend(background_agent_args(exe, launch));
    let started_at_ms = now_epoch_ms();
    BackgroundAgentRecord {
        pid: started.pid,
        run_id: background_agent_run_id(started.pid, started_at_ms),
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
        started_at_ms,
        launched_argv,
    }
}

fn background_agent_run_id(pid: u32, started_at_ms: u64) -> String {
    format!("bg-{started_at_ms}-{pid}")
}

fn background_agent_record_id(record: &BackgroundAgentRecord) -> String {
    if record.run_id.trim().is_empty() {
        background_agent_run_id(record.pid, record.started_at_ms)
    } else {
        record.run_id.clone()
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

fn rewrite_background_agent_records(records: &[BackgroundAgentRecord]) -> Result<()> {
    let path = background_agent_records_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if records.is_empty() {
        if path.exists() {
            fs::write(&path, "").with_context(|| format!("writing {}", path.display()))?;
        }
        return Ok(());
    }
    let mut raw = String::new();
    for record in records {
        raw.push_str(
            &serde_json::to_string(record)
                .with_context(|| format!("serializing background record {}", record.pid))?,
        );
        raw.push('\n');
    }
    fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
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
        let mut record = serde_json::from_str::<BackgroundAgentRecord>(line)
            .with_context(|| format!("parsing {} line {}", path.display(), idx + 1))?;
        if record.run_id.trim().is_empty() {
            record.run_id = background_agent_record_id(&record);
        }
        out.push(record);
    }
    Ok(out)
}

fn resolve_background_agent_record(input: &str) -> Result<Option<BackgroundAgentRecord>> {
    let records = load_background_agent_records()?;
    resolve_background_agent_record_from_records(records, input)
}

fn resolve_background_agent_record_from_records(
    records: Vec<BackgroundAgentRecord>,
    input: &str,
) -> Result<Option<BackgroundAgentRecord>> {
    if records.is_empty() {
        return Ok(None);
    }
    if input.is_empty() || input == "latest" {
        return Ok(records.into_iter().last());
    }
    if let Some(record) = records
        .iter()
        .rev()
        .find(|record| background_agent_record_id(record) == input)
    {
        return Ok(Some(record.clone()));
    }
    if !input.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(None);
    }
    let pid = parse_background_agent_pid(input)?;
    Ok(records.into_iter().rev().find(|record| record.pid == pid))
}

fn retain_running_background_agent_records(
    records: Vec<BackgroundAgentRecord>,
    status: impl Fn(u32) -> BackgroundAgentStatus,
) -> Vec<BackgroundAgentRecord> {
    records
        .into_iter()
        .filter(|record| matches!(status(record.pid), BackgroundAgentStatus::Running))
        .collect()
}

fn parse_background_agent_pid(input: &str) -> Result<u32> {
    let raw = input.trim();
    if raw.is_empty() {
        anyhow::bail!("usage: /agents background kill [pid|run-id|latest]");
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
            anyhow::bail!("usage: /agents create [--worktree|--same-cwd] <name> [description]");
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
        anyhow::bail!("usage: /agents create [--worktree|--same-cwd] <name> [description]");
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
            anyhow::bail!("usage: /agent [--worktree|--same-cwd|--background|--detached] <name> <task>");
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
        anyhow::bail!("usage: /agent [--worktree|--same-cwd|--background|--detached] <name> <task>");
    };
    let name = name.trim();
    let task = task.trim();
    if name.is_empty() || task.is_empty() {
        anyhow::bail!("usage: /agent [--worktree|--same-cwd|--background|--detached] <name> <task>");
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
    quote_sh_string(path.to_string_lossy().as_ref())
}

fn quote_sh_string(raw: &str) -> String {
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

fn run_shell_escape(command: &str, wrapper: Option<&[String]>) -> Option<String> {
    println!("{BOLD}$ {command}{RESET}");
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  shell: could not resolve cwd: {e}{RESET}");
            return None;
        }
    };
    match execute_shell_escape(&cwd, command, wrapper) {
        Ok(result) => {
            print_shell_escape_result(&result);
            Some(shell_escape_prompt_context(command, &result))
        }
        Err(e) => {
            eprintln!("{DIM}  shell: {e:#}{RESET}");
            None
        }
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

fn shell_escape_prompt_context(command: &str, result: &ShellEscapeResult) -> String {
    let mut out = String::new();
    out.push_str("Local shell command run before this prompt:\n");
    out.push_str("$ ");
    out.push_str(command);
    out.push('\n');
    if result.stdout.is_empty() {
        out.push_str("stdout: (empty)\n");
    } else {
        out.push_str("stdout:\n");
        out.push_str(result.stdout.trim_end());
        out.push('\n');
    }
    if result.stderr.is_empty() {
        out.push_str("stderr: (empty)\n");
    } else {
        out.push_str("stderr:\n");
        out.push_str(result.stderr.trim_end());
        out.push('\n');
    }
    match result.exit_code {
        Some(code) => out.push_str(&format!("exit: {code}")),
        None => out.push_str("exit: terminated by signal"),
    }
    out
}

fn apply_pending_shell_context(contexts: &[String], prompt: &str) -> String {
    if contexts.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::new();
    out.push_str("Context from local shell escape commands (`!cmd`) executed in this session:\n\n");
    out.push_str(&contexts.join("\n\n---\n\n"));
    out.push_str("\n\nUser prompt:\n");
    out.push_str(prompt);
    out
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

fn print_session_status_json(
    input: &str,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
    usage: Option<UsageSummary>,
) {
    let payload = session_status_json_payload(input, provider, model, mode, output_style, cfg, usage);
    match serde_json::to_string_pretty(&payload) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /status json: {e:#}{RESET}"),
    }
}

fn session_status_json_payload(
    input: &str,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
    usage: Option<UsageSummary>,
) -> serde_json::Value {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let usage = usage.map(|summary| {
        json!({
            "turns": summary.turns,
            "context_high_water": summary.context_high_water,
            "context_high_water_human": human_tokens(summary.context_high_water),
            "output_total": summary.output_total,
            "output_total_human": human_tokens(summary.output_total),
        })
    });
    json!({
        "surface": "terminal",
        "command": "status",
        "query": input.trim(),
        "provider": provider,
        "model": model,
        "mode": mode_label(mode),
        "output_style": output_style.unwrap_or("default"),
        "cwd": cwd,
        "defaults": {
            "provider": cfg.default_code_provider,
            "code_model": cfg.default_code_model,
        },
        "usage": usage,
        "aliases": ["status"],
        "supported_actions": ["status", "state", "show", "info", "current", "session", "json", "--json", "status --json", "state --json", "show --json", "info --json", "current --json", "session --json"],
    })
}

fn print_reload_preview_json(
    input: &str,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
) {
    let payload = reload_preview_json_payload(input, provider, model, mode, output_style, cfg);
    match serde_json::to_string_pretty(&payload) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /reload json: {e:#}{RESET}"),
    }
}

fn reload_preview_json_payload(
    input: &str,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
) -> serde_json::Value {
    let action = input
        .trim()
        .to_ascii_lowercase()
        .replace(" --json", "")
        .trim()
        .to_string();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    json!({
        "surface": "terminal",
        "command": "reload",
        "query": input.trim(),
        "action": if action.is_empty() || action == "json" || action == "--json" { "session" } else { action.as_str() },
        "will_reload_config": true,
        "will_start_fresh_agent_session": true,
        "will_clear_usage_history": true,
        "current": {
            "provider": provider,
            "model": model,
            "mode": mode_label(mode),
            "output_style": output_style.unwrap_or("default"),
            "cwd": cwd,
        },
        "defaults": {
            "provider": cfg.default_code_provider,
            "code_model": cfg.default_code_model,
        },
        "aliases": ["config", "session", "now", "fresh"],
        "action_aliases": ["config", "session", "now", "fresh"],
        "supported_actions": ["config", "session", "now", "fresh", "json", "--json", "config --json", "session --json", "now --json", "fresh --json"],
    })
}

async fn print_doctor(
    handle: &AgentSessionHandle,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
    approvals: &ApprovalState,
    scheduled_runs: &[ScheduledRun],
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
            !cfg.mcp_servers.is_empty(),
            "mcp registry",
            format_mcp_doctor_summary(cfg)
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
        match load_background_agent_records() {
            Ok(records) => println!(
                "{}",
                doctor_line(
                    true,
                    "background agents",
                    format_background_agent_doctor_summary(&records, background_agent_status)
                )
            ),
            Err(e) => println!("{}", doctor_line(false, "background agents", e.to_string())),
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

    println!(
        "{}",
        doctor_line(
            true,
            "scheduled prompts",
            format_schedule_doctor_summary(scheduled_runs)
        )
    );

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

async fn print_doctor_json(
    handle: &AgentSessionHandle,
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    cfg: &LibertaiConfig,
    approvals: &ApprovalState,
    scheduled_runs: &[ScheduledRun],
    usage: Option<UsageSummary>,
) {
    let cwd = std::env::current_dir();
    let cwd_label = cwd
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let mut checks = Vec::new();

    match handle.state().await {
        Ok(state) => {
            checks.push(json!({
                "status": "ok",
                "ok": true,
                "label": "pi session",
                "detail": state.session_id.unwrap_or_else(|| "not persisted".to_string())
            }));
            checks.push(json!({
                "status": if state.save_enabled { "ok" } else { "warn" },
                "ok": state.save_enabled,
                "label": "session persistence",
                "detail": if state.save_enabled { "enabled" } else { "disabled" }
            }));
            checks.push(json!({
                "status": "ok",
                "ok": true,
                "label": "transcript",
                "detail": format!("{} message(s)", state.message_count)
            }));
        }
        Err(e) => checks.push(json!({
            "status": "warn",
            "ok": false,
            "label": "pi session",
            "detail": e.to_string()
        })),
    }

    checks.push(json!({
        "status": if cfg.auth.api_key.is_some() { "ok" } else { "warn" },
        "ok": cfg.auth.api_key.is_some(),
        "label": "LibertAI auth",
        "detail": cfg.auth.api_key.as_deref().map(mask_key).unwrap_or_else(|| "not logged in".to_string())
    }));
    checks.push(json!({
        "status": "ok",
        "ok": true,
        "label": "defaults",
        "detail": format!("{}/{}", cfg.default_code_provider, cfg.default_code_model)
    }));
    checks.push(json!({
        "status": "ok",
        "ok": true,
        "label": "smart approvals",
        "detail": if cfg.smart_approval_enabled {
            format!("enabled ({})", cfg.smart_approval_model)
        } else {
            "disabled".to_string()
        }
    }));
    checks.push(json!({
        "status": "ok",
        "ok": true,
        "label": "remembered approvals",
        "detail": format!("{} saved rule(s)", approvals.always_rules().len())
    }));
    checks.push(json!({
        "status": "ok",
        "ok": true,
        "label": "hooks",
        "detail": format_hook_event_breakdown(cfg)
    }));
    checks.push(json!({
        "status": if cfg.mcp_servers.is_empty() { "info" } else { "ok" },
        "ok": if cfg.mcp_servers.is_empty() { serde_json::Value::Null } else { serde_json::Value::Bool(true) },
        "label": "mcp registry",
        "detail": format_mcp_doctor_summary(cfg)
    }));
    checks.push(json!({
        "status": "ok",
        "ok": true,
        "label": "scheduled prompts",
        "detail": format_schedule_doctor_summary(scheduled_runs)
    }));
    checks.push(json!({
        "status": "ok",
        "ok": true,
        "label": "usage",
        "detail": usage
            .map(|summary| format!("{} turn(s), {} ctx high-water", summary.turns, human_tokens(summary.context_high_water)))
            .unwrap_or_else(|| "no completed turns yet".to_string())
    }));

    let payload = json!({
        "surface": "terminal",
        "command": "doctor",
        "aliases": ["doctor"],
        "supported_actions": ["status", "state", "show", "info", "health", "diagnostics", "diag", "json", "--json", "status --json", "state --json", "show --json", "info --json", "health --json", "diagnostics --json", "diag --json"],
        "cwd": cwd_label,
        "provider": provider,
        "model": model,
        "mode": mode_label(mode),
        "output_style": output_style.unwrap_or("default"),
        "summary": {
            "total": checks.len(),
            "ok": checks.iter().filter(|check| check.get("status").and_then(|value| value.as_str()) == Some("ok")).count(),
            "warn": checks.iter().filter(|check| check.get("status").and_then(|value| value.as_str()) == Some("warn")).count(),
            "info": checks.iter().filter(|check| check.get("status").and_then(|value| value.as_str()) == Some("info")).count(),
        },
        "checks": checks,
    });
    match serde_json::to_string_pretty(&payload) {
        Ok(raw) => println!("{raw}"),
        Err(err) => eprintln!("{DIM}  /doctor: could not render JSON: {err}.{RESET}"),
    }
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

fn format_background_agent_doctor_summary(
    records: &[BackgroundAgentRecord],
    status: impl Fn(u32) -> BackgroundAgentStatus,
) -> String {
    let counts = background_agent_status_counts(records, status);
    format!(
        "{} recorded ({} running, {} exited, {} unknown)",
        counts.total, counts.running, counts.exited, counts.unknown
    )
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

fn format_schedule_doctor_summary(scheduled_runs: &[ScheduledRun]) -> String {
    let counts = schedule_status_counts(scheduled_runs, Instant::now());
    format!(
        "{} queued ({} due, {} pending)",
        counts.total, counts.due, counts.pending
    )
}

fn format_mcp_doctor_summary(cfg: &LibertaiConfig) -> String {
    let exposure = mcp_exposure_summary(cfg);
    format!(
        "{} configured; mcp_call {}, {} named tool(s), resource reader {}, prompt getter {}, {} subscription candidate(s); stdio/http/sse reuse on",
        cfg.mcp_servers.len(),
        if exposure.mcp_call { "on" } else { "off" },
        exposure.named_tools,
        if exposure.resource_reader { "on" } else { "off" },
        if exposure.prompt_getter { "on" } else { "off" },
        exposure.subscription_candidates
    )
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
        .strip_prefix("/usage")
        .or_else(|| raw.strip_prefix("/cost"))?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim().to_ascii_lowercase().as_str() {
        "json" | "--json" | "status --json" | "show --json" | "summary --json"
        | "tools --json" | "export" | "export json" => Some(UsageExportFormat::Json),
        "csv" | "export csv" => Some(UsageExportFormat::Csv),
        _ => None,
    }
}

fn usage_slash_usage_text() -> &'static str {
    "/usage|/cost [status|show|summary|tools|json|--json|status --json|show --json|summary --json|tools --json|csv|export|export json|export csv]"
}

fn parse_usage_summary_command(input: &str) -> Option<()> {
    let raw = input.trim();
    let rest = raw
        .strip_prefix("/usage")
        .or_else(|| raw.strip_prefix("/cost"))?;
    if rest.is_empty() || !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let action = rest.trim().to_ascii_lowercase();
    match action.as_str() {
        "status" | "show" | "summary" | "tools" => Some(()),
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
        let rates = model_token_rate_details(&summary.model);
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
            "pricing": rates,
        })
    });
    serde_json::to_string_pretty(&json!({
        "kind": "libertai_code_usage_export",
        "version": 1,
        "surface": "terminal",
        "command": "usage",
        "aliases": ["usage", "cost"],
        "supported_actions": ["status", "show", "summary", "tools", "json", "--json", "status --json", "show --json", "summary --json", "tools --json", "csv", "export", "export json", "export csv"],
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
        "category,name,count,input_tokens,output_tokens,estimated_tokens,estimated_cost_usd,duration_ms,pricing_match,input_usd_per_million,output_usd_per_million,pricing_source,provenance\n",
    );
    if let Some(summary) = summary {
        let pricing = model_token_rate_match(&summary.model);
        let (pricing_match, input_rate, output_rate, pricing_source) = pricing
            .map(|(matched, input, output)| {
                (
                    matched.to_string(),
                    format!("{input:.8}"),
                    format!("{output:.8}"),
                    "libertai-cli pricing table".to_string(),
                )
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "usage,{},{},{},{},{},{},{},{},{},{},{},{}\n",
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
            csv_cell(&pricing_match),
            input_rate,
            output_rate,
            csv_cell(&pricing_source),
            csv_cell("input=context high-water; output=sum of completed turns")
        ));
        for row in estimate_tool_attribution(summary, tool_activity) {
            out.push_str(&format!(
                "tool,{},{},{},{},{},{},{},{},{},{},{},{}\n",
                csv_cell(&row.tool_name),
                row.count,
                "",
                "",
                row.estimated_tokens,
                row.estimated_cost
                    .map(|cost| format!("{cost:.8}"))
                    .unwrap_or_default(),
                row.total_duration.as_millis(),
                "",
                "",
                "",
                "",
                csv_cell("estimated duration-weighted attribution")
            ));
        }
    } else {
        for tool in tool_activity {
            out.push_str(&format!(
                "tool,{},{},{},{},{},{},{},{},{},{},{},{}\n",
                csv_cell(&tool.tool_name),
                tool.count,
                "",
                "",
                "",
                "",
                tool.total_duration.as_millis(),
                "",
                "",
                "",
                "",
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
    model_token_rate_match(model).map(|(_, input, output)| (input, output))
}

fn model_token_rate_details(model: &str) -> Option<serde_json::Value> {
    model_token_rate_match(model).map(|(matched, input, output)| {
        json!({
            "matched": matched,
            "inputUsdPerMillion": input,
            "outputUsdPerMillion": output,
            "source": "libertai-cli pricing table",
        })
    })
}

fn model_token_rate_match(model: &str) -> Option<(&'static str, f64, f64)> {
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
            return Some((keys[0], *input, *output));
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

fn config_status_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
    let user_prompt_hooks = count_runnable_hooks(&cfg.hooks.user_prompt_submit);
    let pre_tool_hooks = count_runnable_hooks(&cfg.hooks.pre_tool_use);
    let post_tool_hooks = count_runnable_hooks(&cfg.hooks.post_tool_use);
    let subagent_stop_hooks = count_runnable_hooks(&cfg.hooks.subagent_stop);
    let session_start_hooks = count_runnable_hooks(&cfg.hooks.session_start);
    let stop_hooks = count_runnable_hooks(&cfg.hooks.stop);
    let session_end_hooks = count_runnable_hooks(&cfg.hooks.session_end);
    let notification_hooks = count_runnable_hooks(&cfg.hooks.notification);
    let config_path = crate::config::config_path()
        .ok()
        .map(|path| path.display().to_string());
    json!({
        "surface": "terminal",
        "command": "config",
        "query": query.trim(),
        "aliases": ["config", "settings"],
        "supported_actions": ["status", "show", "current", "info", "json", "--json", "status --json", "show --json", "current --json", "info --json", "path", "open", "settings", "backends", "defaults", "agents", "skills", "hooks", "mcp", "approvals", "appearance", "sandbox", "advanced", "set <key> <value>", "unset <key>", "reset <key>"],
        "api_base": cfg.api_base,
        "account_base": cfg.account_base,
        "config_path": config_path,
        "defaults": {
            "chat_model": cfg.default_chat_model,
            "code_provider": cfg.default_code_provider,
            "code_model": cfg.default_code_model,
            "image_model": cfg.default_image_model,
        },
        "smart_approvals": {
            "enabled": cfg.smart_approval_enabled,
            "model": cfg.smart_approval_model,
        },
        "auto_compaction": {
            "enabled": cfg.code_auto_compaction_enabled,
            "reserve_tokens": cfg.code_compaction_reserve_tokens,
            "keep_recent_tokens": cfg.code_compaction_keep_recent_tokens,
        },
        "turn_notifications": cfg.code_turn_notifications,
        "hooks": {
            "user_prompt_submit": user_prompt_hooks,
            "pre_tool_use": pre_tool_hooks,
            "post_tool_use": post_tool_hooks,
            "subagent_stop": subagent_stop_hooks,
            "session_start": session_start_hooks,
            "stop": stop_hooks,
            "session_end": session_end_hooks,
            "notification": notification_hooks,
        },
        "auth": {
            "logged_in": cfg.auth.api_key.is_some(),
            "api_key": cfg.auth.api_key.as_deref().map(mask_key),
        },
    })
}

fn print_config_status_json(cfg: &LibertaiConfig, query: &str) {
    match serde_json::to_string_pretty(&config_status_payload(cfg, query)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /config json: {e:#}{RESET}"),
    }
}

fn handle_repl_config_command(raw: &str, cfg: &mut Arc<LibertaiConfig>) -> Result<()> {
    let action = raw.trim();
    if is_config_json_alias(action) {
        print_config_status_json(cfg, action);
        return Ok(());
    }
    if is_config_status_alias(action) {
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

fn is_config_status_alias(action: &str) -> bool {
    matches!(
        action.trim().to_ascii_lowercase().as_str(),
        "" | "status" | "show" | "current" | "info"
    )
}

fn is_config_json_alias(action: &str) -> bool {
    matches!(
        action.trim().to_ascii_lowercase().as_str(),
        "json" | "--json" | "status --json" | "show --json" | "current --json" | "info --json"
    )
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

const HOOKS_USAGE: &str =
    "/hooks [status|list|state|diagnostics|diag|json|--json|status --json|list --json|state --json|diagnostics --json|diag --json|show --json|show|event|inspect <event>|open|settings|edit]";
const MCP_USAGE: &str = "/mcp [status|list|state|show|json|--json|status --json|list --json|state --json|diagnostics --json|diag --json|show --json|server|inspect <server>|probe|probes|probe --save|probe save|probe --write|probe write|refresh|diagnostics|diag|reset|reset-sessions|open|settings|edit]";

fn print_hooks_command(cfg: &LibertaiConfig, query: &str, command: HooksCommand) {
    match command {
        HooksCommand::Status => print_hooks_status(cfg),
        HooksCommand::Json => print_hooks_json(cfg, query),
        HooksCommand::Open => print_hooks_open_hint(),
        HooksCommand::Show(event) => print_hook_event_details(cfg, &event),
        HooksCommand::Usage => {
            println!("{BOLD}hooks{RESET}");
            println!("{DIM}  usage:{RESET} {HOOKS_USAGE}");
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
    println!("{DIM}  usage:{RESET} {HOOKS_USAGE}");
    println!();
}

fn hook_event_rows<'a>(
    cfg: &'a LibertaiConfig,
) -> [(&'static str, &'a [crate::config::HookCommandConfig]); 8] {
    [
        ("UserPromptSubmit", &cfg.hooks.user_prompt_submit),
        ("PreToolUse", &cfg.hooks.pre_tool_use),
        ("PostToolUse", &cfg.hooks.post_tool_use),
        ("SubagentStop", &cfg.hooks.subagent_stop),
        ("SessionStart", &cfg.hooks.session_start),
        ("Stop", &cfg.hooks.stop),
        ("SessionEnd", &cfg.hooks.session_end),
        ("Notification", &cfg.hooks.notification),
    ]
}

fn is_configured_hook(hook: &crate::config::HookCommandConfig) -> bool {
    let hook_type = normalized_hook_type(&hook.hook_type);
    if hook_type == "http" {
        !hook.url.trim().is_empty()
    } else if hook_type == "prompt" || hook_type == "agent" {
        !hook.prompt.trim().is_empty()
    } else if hook_type == "mcp_tool" {
        !hook.server.trim().is_empty() && !hook.tool.trim().is_empty()
    } else {
        (hook_type.is_empty() || hook_type == "command") && !hook.command.trim().is_empty()
    }
}

fn hook_json_row(event: &str, index: usize, hook: &crate::config::HookCommandConfig) -> serde_json::Value {
    let hook_type_key = normalized_hook_type(&hook.hook_type);
    let hook_type = if hook.hook_type.trim().is_empty() {
        "command"
    } else {
        hook_type_key.as_str()
    };
    json!({
        "event": event,
        "index": index,
        "enabled": hook.enabled,
        "configured": is_configured_hook(hook),
        "type": hook_type,
        "matcher": if hook.matcher.trim().is_empty() { "*" } else { hook.matcher.trim() },
        "target": hook_target_display(hook),
        "source": if hook.source.trim().is_empty() { serde_json::Value::Null } else { json!(hook.source.trim()) },
        "timeout_seconds": hook.timeout,
        "async": hook.async_hook,
        "async_rewake": hook.async_rewake,
        "once": hook.once,
        "continue_on_block": hook.continue_on_block,
        "status_message": if hook.status_message.trim().is_empty() { serde_json::Value::Null } else { json!(hook.status_message.trim()) },
        "review_policy": if hook.review_policy.trim().is_empty() { serde_json::Value::Null } else { json!(hook.review_policy.trim()) },
        "if": if hook.if_condition.trim().is_empty() { serde_json::Value::Null } else { json!(hook.if_condition.trim()) },
        "headers": hook.headers.len(),
        "allowed_env_vars": hook.allowed_env_vars.len(),
        "has_input": hook.input.is_some(),
        "metadata_keys": hook.extra.keys().map(String::as_str).collect::<Vec<_>>(),
    })
}

fn hooks_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
    let mut rows = Vec::new();
    let mut events = Vec::new();
    let mut enabled_total = 0usize;
    let mut configured_total = 0usize;
    for (event, hooks) in hook_event_rows(cfg) {
        let enabled = hooks.iter().filter(|hook| hook.enabled).count();
        let configured = hooks.iter().filter(|hook| is_configured_hook(hook)).count();
        enabled_total += enabled;
        configured_total += configured;
        events.push(json!({
            "event": event,
            "count": hooks.len(),
            "enabled": enabled,
            "configured": configured,
            "types": hook_type_summary(hooks),
        }));
        for (index, hook) in hooks.iter().enumerate() {
            rows.push(hook_json_row(event, index + 1, hook));
        }
    }
    json!({
        "surface": "terminal",
        "command": "hooks",
        "query": query.trim(),
        "events": events,
        "count": rows.len(),
        "enabled_count": enabled_total,
        "configured_count": configured_total,
        "hooks": rows,
        "will_write": false,
        "aliases": ["hooks", "hook"],
        "supported_actions": ["status", "list", "state", "diagnostics", "diag", "json", "--json", "status --json", "list --json", "state --json", "diagnostics --json", "diag --json", "show --json", "show <event>", "event <event>", "inspect <event>", "open", "settings", "edit"],
    })
}

fn print_hooks_json(cfg: &LibertaiConfig, query: &str) {
    match serde_json::to_string_pretty(&hooks_json_payload(cfg, query)) {
        Ok(raw) => println!("{raw}"),
        Err(e) => eprintln!("{DIM}  /hooks json failed: {e}{RESET}"),
    }
}

fn print_hook_event_details(cfg: &LibertaiConfig, event: &str) {
    match hooks_for_event(cfg, event) {
        Some((canonical, hooks)) => print!("{}", format_hook_event_details(canonical, hooks)),
        None => {
            eprintln!("{DIM}  /hooks: no known hook event `{event}`{RESET}");
            eprintln!(
                "{DIM}  events:{RESET} UserPromptSubmit, PreToolUse, PostToolUse, SubagentStop, SessionStart, Stop, SessionEnd, Notification"
            );
        }
    }
}

fn hooks_for_event<'a>(
    cfg: &'a LibertaiConfig,
    event: &str,
) -> Option<(&'static str, &'a [crate::config::HookCommandConfig])> {
    match normalize_hook_event(event)?.as_str() {
        "userpromptsubmit" => Some(("UserPromptSubmit", &cfg.hooks.user_prompt_submit)),
        "pretooluse" => Some(("PreToolUse", &cfg.hooks.pre_tool_use)),
        "posttooluse" => Some(("PostToolUse", &cfg.hooks.post_tool_use)),
        "subagentstop" => Some(("SubagentStop", &cfg.hooks.subagent_stop)),
        "sessionstart" => Some(("SessionStart", &cfg.hooks.session_start)),
        "stop" => Some(("Stop", &cfg.hooks.stop)),
        "sessionend" => Some(("SessionEnd", &cfg.hooks.session_end)),
        "notification" => Some(("Notification", &cfg.hooks.notification)),
        _ => None,
    }
}

fn normalize_hook_event(event: &str) -> Option<String> {
    let key = event
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let normalized = match key.as_str() {
        "userpromptsubmit" | "promptsubmit" | "prompt" => "userpromptsubmit",
        "pretooluse" | "pretool" | "pre" => "pretooluse",
        "posttooluse" | "posttool" | "post" => "posttooluse",
        "subagentstop" | "subagent" => "subagentstop",
        "sessionstart" | "start" => "sessionstart",
        "stop" => "stop",
        "sessionend" | "end" => "sessionend",
        "notification" | "notify" => "notification",
        _ => return None,
    };
    Some(normalized.to_string())
}

fn print_hooks_open_hint() {
    println!("{BOLD}hooks{RESET}");
    println!("{DIM}  /hooks open:{RESET} open Desktop Settings > Hooks for graphical hook management.");
    println!(
        "{DIM}  terminal:{RESET} edit hook rows in the LibertAI config file; /hooks status shows the active rows."
    );
    println!();
}

fn print_mcp_status(query: &str, command: McpCommand) {
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
                    let exposure = mcp_exposure_summary(&cfg);
                    println!("{DIM}  configured servers:{RESET} {}", cfg.mcp_servers.len());
                    println!(
                        "{DIM}  native exposure:{RESET} mcp_call {}, {} named MCP tool(s), mcp_read_resource {}, mcp_get_prompt {}, {} resource subscription candidate(s)",
                        if exposure.mcp_call { "on" } else { "off" },
                        exposure.named_tools,
                        if exposure.resource_reader { "on" } else { "off" },
                        if exposure.prompt_getter { "on" } else { "off" },
                        exposure.subscription_candidates
                    );
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
            println!("{DIM}  usage:{RESET} {MCP_USAGE}");
        }
        McpCommand::Json => print_mcp_json(query),
        McpCommand::Show(name) => print_mcp_server_details(&name),
        McpCommand::Probe => print_mcp_probe(),
        McpCommand::ProbeSave => print_mcp_probe_save(),
        McpCommand::Reset => {
            let closed = crate::commands::code_hooks::reset_mcp_cli_sessions();
            println!(
                "{DIM}  /mcp reset:{RESET} closed {closed} terminal stdio/HTTP/SSE MCP session{}.",
                if closed == 1 { "" } else { "s" }
            );
            println!("{DIM}  note:{RESET} stdio, Streamable HTTP, and legacy SSE MCP tools/resources/prompts reuse live CLI sessions until reset or process exit.");
        }
        McpCommand::Open => {
            println!(
                "{DIM}  /mcp open:{RESET} open Desktop Settings > MCP for live server management. The terminal CLI has no MCP settings pane."
            );
        }
        McpCommand::Usage => {
            println!("{DIM}  usage:{RESET} {MCP_USAGE}");
        }
    }
    println!();
}

fn mcp_server_json_row(
    name: &str,
    server: &crate::config::McpServerConfig,
) -> serde_json::Value {
    let transport = if server.transport.trim().is_empty() {
        "stdio"
    } else {
        server.transport.trim()
    };
    let enabled_tools = server
        .tools
        .iter()
        .filter(|tool| tool.enabled && !tool.name.trim().is_empty())
        .count();
    let enabled_resources = server
        .resources
        .iter()
        .filter(|resource| resource.enabled && !resource.uri.trim().is_empty())
        .count();
    let enabled_prompts = server
        .prompts
        .iter()
        .filter(|prompt| prompt.enabled && !prompt.name.trim().is_empty())
        .count();
    json!({
        "name": name,
        "transport": transport,
        "target": mcp_server_target(server),
        "env_vars": server.env.len(),
        "headers": server.headers.len(),
        "roots": server.roots.len(),
        "tools": server.tools.len(),
        "resources": server.resources.len(),
        "prompts": server.prompts.len(),
        "enabled_tools": enabled_tools,
        "enabled_resources": enabled_resources,
        "enabled_prompts": enabled_prompts,
    })
}

fn mcp_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
    let exposure = mcp_exposure_summary(cfg);
    let mut servers: Vec<serde_json::Value> = cfg
        .mcp_servers
        .iter()
        .map(|(name, server)| mcp_server_json_row(name, server))
        .collect();
    servers.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["name"].as_str().unwrap_or_default())
    });
    json!({
        "surface": "terminal",
        "command": "mcp",
        "query": query.trim(),
        "configured_servers": cfg.mcp_servers.len(),
        "exposure": {
            "mcp_call": exposure.mcp_call,
            "named_tools": exposure.named_tools,
            "resource_reader": exposure.resource_reader,
            "prompt_getter": exposure.prompt_getter,
            "subscription_candidates": exposure.subscription_candidates,
        },
        "servers": servers,
        "will_write": false,
        "aliases": ["mcp"],
        "supported_actions": ["status", "list", "state", "show", "diagnostics", "diag", "json", "--json", "status --json", "list --json", "state --json", "diagnostics --json", "diag --json", "show --json", "server <name>", "inspect <server>", "probe", "probes", "probe --save", "probe save", "probe --write", "probe write", "refresh", "reset", "reset-sessions", "open", "settings", "edit"],
    })
}

fn print_mcp_json(query: &str) {
    match crate::config::load() {
        Ok(cfg) => match serde_json::to_string_pretty(&mcp_json_payload(&cfg, query)) {
            Ok(raw) => println!("{raw}"),
            Err(e) => eprintln!("{DIM}  /mcp json failed: {e}{RESET}"),
        },
        Err(e) => eprintln!("{DIM}  /mcp json: config load failed: {e:#}{RESET}"),
    }
}

fn print_mcp_server_details(name: &str) {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{DIM}  /mcp: config load failed: {e:#}{RESET}");
            return;
        }
    };
    let Some((server_name, server)) = cfg
        .mcp_servers
        .iter()
        .find(|(server_name, _)| server_name.as_str() == name)
        .or_else(|| {
            cfg.mcp_servers
                .iter()
                .find(|(server_name, _)| server_name.starts_with(name))
        })
    else {
        eprintln!("{DIM}  /mcp: no configured server found for `{name}`{RESET}");
        return;
    };
    print!("{}", format_mcp_server_details(server_name, server));
}

fn format_mcp_server_details(name: &str, server: &crate::config::McpServerConfig) -> String {
    let transport = if server.transport.trim().is_empty() {
        "stdio"
    } else {
        server.transport.trim()
    };
    let target = mcp_server_target(server);
    let enabled_tools = server
        .tools
        .iter()
        .filter(|tool| tool.enabled && !tool.name.trim().is_empty())
        .count();
    let enabled_resources = server
        .resources
        .iter()
        .filter(|resource| resource.enabled && !resource.uri.trim().is_empty())
        .count();
    let enabled_prompts = server
        .prompts
        .iter()
        .filter(|prompt| prompt.enabled && !prompt.name.trim().is_empty())
        .count();
    let mut out = format!(
        "{BOLD}mcp server: {name}{RESET}\n  transport: {transport}\n  target: {target}\n  env vars: {}\n  headers: {}\n  cache: {} tool(s), {} resource(s), {} prompt(s)\n  enabled cache: {enabled_tools}/{} tool(s), {enabled_resources}/{} resource(s), {enabled_prompts}/{} prompt(s)\n\n",
        server.env.len(),
        server.headers.len(),
        server.tools.len(),
        server.resources.len(),
        server.prompts.len(),
        server.tools.len(),
        server.resources.len(),
        server.prompts.len(),
    );
    append_mcp_tool_details(&mut out, &server.tools);
    append_mcp_resource_details(&mut out, &server.resources);
    append_mcp_prompt_details(&mut out, &server.prompts);
    out.push_str(&format!(
        "{DIM}  run /mcp probe --save to refresh cached tools/resources/prompts for `{name}`.{RESET}\n\n"
    ));
    out
}

fn mcp_server_target(server: &crate::config::McpServerConfig) -> String {
    if !server.url.trim().is_empty() {
        return server.url.trim().to_string();
    }
    if server.command.trim().is_empty() {
        return "(no target)".to_string();
    }
    let mut parts = vec![server.command.trim().to_string()];
    parts.extend(server.args.iter().map(|arg| quote_sh_string(arg)));
    parts.join(" ")
}

fn append_mcp_tool_details(out: &mut String, tools: &[crate::config::McpToolConfig]) {
    out.push_str(&format!("{BOLD}tools{RESET}\n"));
    if tools.is_empty() {
        out.push_str("  (none cached)\n\n");
        return;
    }
    for tool in tools.iter().take(12) {
        let marker = if tool.enabled { "on" } else { "off" };
        let desc = if tool.description.trim().is_empty() {
            String::new()
        } else {
            format!(" - {}", truncate_chars(tool.description.trim(), 100))
        };
        out.push_str(&format!("  - [{}] {}{}\n", marker, tool.name, desc));
    }
    if tools.len() > 12 {
        out.push_str(&format!("  - ... {} more tool(s)\n", tools.len() - 12));
    }
    out.push('\n');
}

fn append_mcp_resource_details(out: &mut String, resources: &[crate::config::McpResourceConfig]) {
    out.push_str(&format!("{BOLD}resources{RESET}\n"));
    if resources.is_empty() {
        out.push_str("  (none cached)\n\n");
        return;
    }
    for resource in resources.iter().take(12) {
        let marker = if resource.enabled { "on" } else { "off" };
        let label = if resource.name.trim().is_empty() {
            resource.uri.as_str()
        } else {
            resource.name.as_str()
        };
        let mime = if resource.mime_type.trim().is_empty() {
            String::new()
        } else {
            format!(" ({})", resource.mime_type.trim())
        };
        out.push_str(&format!("  - [{}] {}{} - {}\n", marker, label, mime, resource.uri));
    }
    if resources.len() > 12 {
        out.push_str(&format!("  - ... {} more resource(s)\n", resources.len() - 12));
    }
    out.push('\n');
}

fn append_mcp_prompt_details(out: &mut String, prompts: &[crate::config::McpPromptConfig]) {
    out.push_str(&format!("{BOLD}prompts{RESET}\n"));
    if prompts.is_empty() {
        out.push_str("  (none cached)\n\n");
        return;
    }
    for prompt in prompts.iter().take(12) {
        let marker = if prompt.enabled { "on" } else { "off" };
        let args = prompt
            .arguments
            .iter()
            .filter(|arg| !arg.name.trim().is_empty())
            .map(|arg| {
                if arg.required {
                    format!("{}*", arg.name)
                } else {
                    arg.name.clone()
                }
            })
            .collect::<Vec<_>>();
        let args = if args.is_empty() {
            String::new()
        } else {
            format!(" args: {}", args.join(", "))
        };
        let desc = if prompt.description.trim().is_empty() {
            String::new()
        } else {
            format!(" - {}", truncate_chars(prompt.description.trim(), 100))
        };
        out.push_str(&format!("  - [{}] {}{}{}\n", marker, prompt.name, desc, args));
    }
    if prompts.len() > 12 {
        out.push_str(&format!("  - ... {} more prompt(s)\n", prompts.len() - 12));
    }
    out.push('\n');
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct McpExposureSummary {
    mcp_call: bool,
    named_tools: usize,
    resource_reader: bool,
    prompt_getter: bool,
    subscription_candidates: usize,
}

fn mcp_exposure_summary(cfg: &LibertaiConfig) -> McpExposureSummary {
    let mut named_tools = 0usize;
    let mut enabled_resources = 0usize;
    let mut enabled_prompts = 0usize;
    for server in cfg.mcp_servers.values() {
        named_tools += server
            .tools
            .iter()
            .filter(|tool| tool.enabled && !tool.name.trim().is_empty())
            .count();
        enabled_resources += server
            .resources
            .iter()
            .filter(|resource| resource.enabled && !resource.uri.trim().is_empty())
            .count();
        enabled_prompts += server
            .prompts
            .iter()
            .filter(|prompt| prompt.enabled && !prompt.name.trim().is_empty())
            .count();
    }
    McpExposureSummary {
        mcp_call: !cfg.mcp_servers.is_empty(),
        named_tools,
        resource_reader: enabled_resources > 0,
        prompt_getter: enabled_prompts > 0,
        subscription_candidates: enabled_resources,
    }
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
        .filter(|hook| hook.enabled && is_configured_hook(hook))
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
        let review_policy = if hook.review_policy.trim().is_empty() {
            String::new()
        } else {
            format!(", reviewPolicy={}", hook.review_policy.trim())
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
            "{DIM}  {}. {} [{}] type={} matcher={}{}{}{}{}{}{}{}{}{}{}{}:{RESET} {}",
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
            review_policy,
            metadata,
            if_condition,
            continue_on_block,
            target
        );
    }
}

fn format_hook_event_details(event: &str, hooks: &[crate::config::HookCommandConfig]) -> String {
    let enabled = hooks.iter().filter(|hook| hook.enabled).count();
    let async_count = hooks.iter().filter(|hook| hook.async_hook).count();
    let once_count = hooks.iter().filter(|hook| hook.once).count();
    let continue_count = hooks.iter().filter(|hook| hook.continue_on_block).count();
    let mut out = format!(
        "{BOLD}hooks: {event}{RESET}\n  configured: {} ({} enabled)\n  flags: {} async, {} once, {} continueOnBlock\n",
        hooks.len(),
        enabled,
        async_count,
        once_count,
        continue_count
    );
    let types = hook_type_summary(hooks);
    let types = if types.is_empty() {
        "none"
    } else {
        types.as_str()
    };
    out.push_str(&format!("  types: {types}\n\n"));
    if hooks.is_empty() {
        out.push_str(&format!("{DIM}  no {event} hooks configured{RESET}\n\n"));
        return out;
    }
    for (idx, hook) in hooks.iter().enumerate() {
        let marker = if hook.enabled { "on" } else { "off" };
        let matcher = if hook.matcher.trim().is_empty() {
            "*"
        } else {
            hook.matcher.trim()
        };
        let hook_type_key = normalized_hook_type(&hook.hook_type);
        let hook_type = if hook.hook_type.trim().is_empty() {
            "command"
        } else {
            hook_type_key.as_str()
        };
        let target = hook_target_display(hook);
        out.push_str(&format!(
            "  {}. [{}] type={} matcher={} target={}\n",
            idx + 1,
            marker,
            hook_type,
            matcher,
            target
        ));
        if !hook.if_condition.trim().is_empty() {
            out.push_str(&format!("     if: {}\n", hook.if_condition.trim()));
        }
        if !hook.source.trim().is_empty() {
            out.push_str(&format!("     source: {}\n", hook.source.trim()));
        }
        if let Some(timeout) = hook.timeout {
            out.push_str(&format!("     timeout: {timeout}s\n"));
        }
        if hook.async_hook || hook.async_rewake || hook.once || hook.continue_on_block {
            let mut flags = Vec::new();
            if hook.async_hook {
                flags.push("async");
            }
            if hook.async_rewake {
                flags.push("asyncRewake");
            }
            if hook.once {
                flags.push("once");
            }
            if hook.continue_on_block {
                flags.push("continueOnBlock");
            }
            out.push_str(&format!("     flags: {}\n", flags.join(", ")));
        }
        if !hook.status_message.trim().is_empty() {
            out.push_str(&format!("     statusMessage: {}\n", hook.status_message.trim()));
        }
        if !hook.review_policy.trim().is_empty() {
            out.push_str(&format!("     reviewPolicy: {}\n", hook.review_policy.trim()));
        }
        if hook_type_key == "http" {
            out.push_str(&format!(
                "     http metadata: {} header(s), {} allowed env var(s)\n",
                hook.headers.len(),
                hook.allowed_env_vars.len()
            ));
        }
        if hook_type_key == "mcp_tool" {
            let input = if hook.input.is_some() { "yes" } else { "no" };
            out.push_str(&format!("     mcp input: {input}\n"));
        }
        let metadata = hook_extra_metadata_label(hook);
        if let Some(keys) = metadata.strip_prefix(", metadata=") {
            out.push_str(&format!("     metadata: {keys}\n"));
        }
    }
    out.push('\n');
    out
}

fn hook_type_summary(hooks: &[crate::config::HookCommandConfig]) -> String {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for hook in hooks {
        let hook_type_key = normalized_hook_type(&hook.hook_type);
        let hook_type = if hook.hook_type.trim().is_empty() {
            "command"
        } else {
            hook_type_key.as_str()
        };
        *counts.entry(hook_type.to_string()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(kind, count)| format!("{kind} {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn hook_target_display(hook: &crate::config::HookCommandConfig) -> String {
    let hook_type_key = normalized_hook_type(&hook.hook_type);
    if hook_type_key == "http" {
        if hook.url.trim().is_empty() {
            "(no url)".to_string()
        } else {
            hook.url.trim().to_string()
        }
    } else if hook_type_key == "prompt" || hook_type_key == "agent" {
        if hook.prompt.trim().is_empty() {
            "(no prompt)".to_string()
        } else {
            truncate_chars(hook.prompt.trim(), 120)
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
    println!("{DIM}  usage:{RESET} {}", status_line_usage_text());
    println!();
}

fn status_line_usage_text() -> &'static str {
    "/statusline|/status-line <status|show|json|--json|status --json|show --json|template --json|info --json|template|command <shell>|command-clear|command reset|command clear|reset|clear>"
}

fn is_status_line_json_action(action: &str) -> bool {
    matches!(
        action.trim().to_ascii_lowercase().as_str(),
        "json"
            | "--json"
            | "status --json"
            | "show --json"
            | "template --json"
            | "info --json"
    )
}

fn status_line_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
    let template = cfg.status_line_template.trim();
    let command = cfg.status_line_command.trim();
    json!({
        "surface": "terminal",
        "command": "statusline",
        "query": query.trim(),
        "aliases": ["statusline", "status-line"],
        "template": if template.is_empty() { serde_json::Value::Null } else { json!(template) },
        "effective_template": if template.is_empty() { "default" } else { template },
        "status_command": if command.is_empty() { serde_json::Value::Null } else { json!(command) },
        "tokens": STATUS_LINE_TOKENS,
        "template_max_chars": STATUS_LINE_TEMPLATE_MAX_CHARS,
        "command_max_chars": STATUS_LINE_COMMAND_MAX_CHARS,
        "will_write": false,
        "will_run_command": false,
        "supported_actions": ["status", "show", "json", "--json", "status --json", "show --json", "template --json", "info --json", "template", "command <shell>", "command-clear", "command reset", "command clear", "reset", "clear"],
    })
}

fn print_status_line_json(cfg: &LibertaiConfig, query: &str) {
    match serde_json::to_string_pretty(&status_line_json_payload(cfg, query)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /statusline json failed: {e}{RESET}"),
    }
}

fn handle_status_line_command(raw: &str, cfg: &mut Arc<LibertaiConfig>) -> Result<()> {
    let action = raw.trim();
    if is_status_line_json_action(action) {
        print_status_line_json(cfg, action);
        return Ok(());
    }
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
    if is_output_style_json_request(key) {
        print_output_style_status_json(output_style.as_deref(), key);
        return;
    }
    if is_output_style_status_alias(key) {
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

fn is_output_style_status_alias(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "status" | "show" | "current" | "info" | "list"
    )
}

fn is_output_style_json_request(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "json" | "--json" | "status --json" | "show --json" | "current --json"
            | "info --json" | "list --json"
    )
}

fn output_style_usage_text() -> &'static str {
    "/output-style [default|concise|explanatory|review|status|show|current|info|list|json|--json|status --json|show --json|current --json|info --json|list --json]"
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

fn output_style_status_json_payload(output_style: Option<&str>, query: &str) -> serde_json::Value {
    let cwd = std::env::current_dir().ok();
    let styles = crate::commands::code_output_style::load_styles(cwd.as_deref());
    json!({
        "surface": "terminal",
        "command": "output-style",
        "query": query.trim(),
        "aliases": ["output-style"],
        "current": output_style.unwrap_or("default"),
        "available": styles.into_iter().map(|style| {
            json!({
                "name": style.name,
                "description": style.description,
                "instruction": style.instruction,
            })
        }).collect::<Vec<_>>(),
        "supported_actions": ["default", "concise", "explanatory", "review", "status", "show", "current", "info", "list", "json", "--json", "status --json", "show --json", "current --json", "info --json", "list --json"],
    })
}

fn print_output_style_status_json(output_style: Option<&str>, query: &str) {
    let payload = output_style_status_json_payload(output_style, query);
    match serde_json::to_string_pretty(&payload) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /output-style json: {e:#}{RESET}"),
    }
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

fn bug_json_payload(
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    query: &str,
) -> serde_json::Value {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    json!({
        "command": "bug",
        "surface": "terminal",
        "query": query.trim(),
        "aliases": ["bug"],
        "app": "libertai-cli",
        "branch": "integrated-code",
        "provider": provider,
        "model": model,
        "mode": mode_label(mode),
        "output_style": output_style.unwrap_or("default"),
        "cwd": cwd,
        "template_fields": [
            "what_you_expected",
            "what_happened",
            "last_command_or_prompt",
            "reproduces_in_fresh_libertai_code_session"
        ],
        "supported_actions": ["report", "template", "status", "show", "json", "--json", "status --json", "show --json", "template --json", "report --json"],
    })
}

fn print_bug_json(
    provider: &str,
    model: &str,
    mode: Mode,
    output_style: Option<&str>,
    query: &str,
) {
    match serde_json::to_string_pretty(&bug_json_payload(
        provider,
        model,
        mode,
        output_style,
        query,
    )) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{DIM}  /bug json failed: {e}{RESET}"),
    }
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
    let vim_enabled = VIM_INPUT_ENABLED.load(Ordering::SeqCst);
    let mut vim_input_mode = if vim_enabled {
        Some(VimInputMode::Insert)
    } else {
        None
    };

    // First paint lays down two fresh lines; every subsequent paint moves
    // back up to the rule line and overwrites in place so the bar stays
    // anchored to its starting position instead of marching down.
    let mut painted = false;
    repaint(
        &mut stdout,
        &buffer,
        cursor_pos,
        mode,
        vim_input_mode,
        painted,
    )?;
    painted = true;

    loop {
        let ev = event::read().map_err(|e| anyhow::anyhow!("event::read: {e}"))?;
        match ev {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (code, modifiers) {
                (KeyCode::Esc, _) if vim_enabled => {
                    vim_input_mode = Some(VimInputMode::Normal);
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
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
                (code, modifiers) if vim_input_mode == Some(VimInputMode::Normal) => {
                    match vim_normal_key_action(code, modifiers) {
                        VimNormalAction::Submit => {
                            clear_bar(&mut stdout)?;
                            let line: String = buffer.into_iter().collect();
                            return Ok(LineResult::Submit(line));
                        }
                        VimNormalAction::MoveLeft if cursor_pos > 0 => cursor_pos -= 1,
                        VimNormalAction::MoveRight if cursor_pos < buffer.len() => cursor_pos += 1,
                        VimNormalAction::Home => cursor_pos = 0,
                        VimNormalAction::End => cursor_pos = buffer.len(),
                        VimNormalAction::Delete if cursor_pos < buffer.len() => {
                            buffer.remove(cursor_pos);
                        }
                        VimNormalAction::InsertBefore => {
                            vim_input_mode = Some(VimInputMode::Insert);
                        }
                        VimNormalAction::InsertAfter => {
                            if cursor_pos < buffer.len() {
                                cursor_pos += 1;
                            }
                            vim_input_mode = Some(VimInputMode::Insert);
                        }
                        VimNormalAction::InsertHome => {
                            cursor_pos = 0;
                            vim_input_mode = Some(VimInputMode::Insert);
                        }
                        VimNormalAction::InsertEnd => {
                            cursor_pos = buffer.len();
                            vim_input_mode = Some(VimInputMode::Insert);
                        }
                        _ => {}
                    }
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
                (KeyCode::Backspace, _) if cursor_pos > 0 => {
                    buffer.remove(cursor_pos - 1);
                    cursor_pos -= 1;
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
                (KeyCode::Delete, _) if cursor_pos < buffer.len() => {
                    buffer.remove(cursor_pos);
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
                (KeyCode::Left, _) if cursor_pos > 0 => {
                    cursor_pos -= 1;
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
                (KeyCode::Right, _) if cursor_pos < buffer.len() => {
                    cursor_pos += 1;
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
                (KeyCode::Home, _) => {
                    cursor_pos = 0;
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
                }
                (KeyCode::End, _) => {
                    cursor_pos = buffer.len();
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
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
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
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
                    vim_input_mode = if vim_enabled {
                        Some(VimInputMode::Insert)
                    } else {
                        None
                    };
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
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
                    vim_input_mode = if vim_enabled {
                        Some(VimInputMode::Insert)
                    } else {
                        None
                    };
                    repaint(
                        &mut stdout,
                        &buffer,
                        cursor_pos,
                        mode,
                        vim_input_mode,
                        painted,
                    )?;
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
                repaint(
                    &mut stdout,
                    &buffer,
                    cursor_pos,
                    mode,
                    vim_input_mode,
                    painted,
                )?;
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
    vim_input_mode: Option<VimInputMode>,
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
    let mode_chip = match mode {
        Mode::Normal => "",
        Mode::AcceptEdits => "[accept-edits] ",
        Mode::Plan => "[plan] ",
    };
    let vim_chip = match vim_input_mode {
        Some(VimInputMode::Normal) => "[vim:normal] ",
        Some(VimInputMode::Insert) => "[vim:insert] ",
        None => "",
    };
    let chip_text = format!("{mode_chip}{vim_chip}");
    let chip_colour = match mode {
        Mode::Plan => Color::Yellow,
        Mode::AcceptEdits => Color::Cyan,
        Mode::Normal if vim_input_mode.is_some() => Color::Magenta,
        Mode::Normal => Color::DarkGrey,
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
        Print(&chip_text),
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
    fn output_style_status_aliases_match_desktop_palette() {
        for alias in ["status", "show", "current", "info", "list"] {
            assert!(is_output_style_status_alias(alias), "{alias}");
        }
        assert!(is_output_style_status_alias(" SHOW "));
        assert!(!is_output_style_status_alias("review"));
        assert!(!is_output_style_status_alias("missing"));
        assert!(is_output_style_json_request("json"));
        assert!(is_output_style_json_request("--json"));
        assert!(is_output_style_json_request("status --json"));
        assert!(is_output_style_json_request("show --json"));
        assert!(is_output_style_json_request("current --json"));
        assert!(is_output_style_json_request("info --json"));
        assert!(is_output_style_json_request("list --json"));
        assert!(!is_output_style_json_request("review --json"));
        assert!(output_style_usage_text().contains("status|show|current|info|list"));
        assert!(output_style_usage_text().contains("json|--json|status --json|show --json"));
        assert!(output_style_usage_text().contains("info --json|list --json"));
        assert!(output_style_usage_text().contains("default|concise|explanatory|review"));
        let payload = output_style_status_json_payload(Some("review"), "current --json");
        assert_eq!(payload["command"], "output-style");
        assert_eq!(payload["query"], "current --json");
        assert_eq!(payload["aliases"][0], "output-style");
        assert_eq!(payload["current"], "review");
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("show --json"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("info --json"))
        );
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
            parse_init_from_agent_action("from-agent json"),
            Some(InitFromAgentAction::Json)
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent status --json"),
            Some(InitFromAgentAction::Json)
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
            Some(InitFromAgentAction::PreviewApply("merge-lines"))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview append"),
            Some(InitFromAgentAction::PreviewApply("append"))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview merge"),
            Some(InitFromAgentAction::PreviewApply("merge"))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview replace"),
            Some(InitFromAgentAction::PreviewApply("replace"))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview sections 1,3"),
            Some(InitFromAgentAction::PreviewSections(vec![1, 3]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview sections 1-3"),
            Some(InitFromAgentAction::PreviewSections(vec![1, 2, 3]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview sections all"),
            Some(InitFromAgentAction::PreviewSections(vec![0]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview append sections 1,3"),
            Some(InitFromAgentAction::PreviewApplySections("append", vec![1, 3]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview merge sections 1"),
            Some(InitFromAgentAction::PreviewApplySections("merge", vec![1]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent preview merge-lines sections 2"),
            Some(InitFromAgentAction::PreviewApplySections("merge-lines", vec![2]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent sections 2"),
            Some(InitFromAgentAction::PreviewSections(vec![2]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent append sections 1 2"),
            Some(InitFromAgentAction::AppendSections(vec![1, 2]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent merge sections 2"),
            Some(InitFromAgentAction::MergeSections(vec![2]))
        );
        assert_eq!(
            parse_init_from_agent_action("from-agent merge-lines sections 2"),
            Some(InitFromAgentAction::MergeLineSections(vec![2]))
        );
        assert_eq!(
            parse_init_from_agent_action("apply-agent replace"),
            Some(InitFromAgentAction::Replace)
        );
        assert_eq!(parse_init_from_agent_action("from-agent sections 0"), None);
        assert_eq!(parse_init_from_agent_action("from-agent sections 3-1"), None);
        assert_eq!(parse_init_from_agent_action("from-agent sections all,1"), None);
        assert_eq!(parse_init_from_agent_action("from-agent sections 1 1"), None);
        assert_eq!(parse_init_from_agent_action("from-agent nope"), None);

        let hint = help_command_arg_hint("init");
        assert!(hint.contains("show --json|preview --json"));
        assert!(hint.contains("from-agent preview append|from-agent preview merge|from-agent preview merge-lines|from-agent preview replace"));
        assert!(hint.contains("from-agent append|from-agent merge|from-agent merge-lines|from-agent replace"));
        assert!(hint.contains("from-agent preview sections N[,M]|N-M|all"));
        assert!(hint.contains("from-agent preview append sections N[,M]"));
        assert!(hint.contains("from-agent merge-lines sections N[,M]"));
    }

    #[test]
    fn init_json_arg_accepts_status_aliases() {
        assert!(is_init_json_arg("json"));
        assert!(is_init_json_arg("status --json"));
        assert!(is_init_json_arg("preview --json"));
        assert!(!is_init_json_arg("from-agent json"));
        assert!(!is_init_json_arg("project notes"));
    }

    #[test]
    fn init_project_json_payload_reports_candidate_without_writing() {
        let payload = init_project_json_payload(
            "cli",
            Path::new("/tmp/AGENTS.md"),
            Some("custom\n"),
            "# Demo\n\n## Build & test\n- test: cargo test\n",
            Some("prefer checks"),
        );
        assert_eq!(payload["command"], "init");
        assert_eq!(payload["exists"], true);
        assert_eq!(payload["would_create"], false);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["notes_supplied"], true);
        assert_eq!(payload["sections"][1]["title"], "Build & test");
        assert_eq!(payload["sections"][1]["impact"], "new section");
        assert_eq!(payload["supported_actions"][2], "--json");
        assert_eq!(payload["supported_actions"][5], "preview --json");
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
    fn selected_init_candidate_sections_returns_numbered_sections_only() {
        let candidate = "# Candidate\n\n## Build & test\n- test: cargo test\n\n## Structure\n- src/ - code\n";
        let selected = selected_init_candidate_sections(candidate, &[3]).unwrap();
        assert!(!selected.contains("# Candidate"));
        assert!(!selected.contains("## Build & test"));
        assert!(selected.starts_with("## Structure\n- src/ - code\n"));
        assert!(selected.ends_with('\n'));
        let all = selected_init_candidate_sections(candidate, &[0]).unwrap();
        assert!(all.contains("# Candidate"));
        assert!(all.contains("## Build & test"));
        assert!(all.contains("## Structure"));
        assert!(selected_init_candidate_sections(candidate, &[4])
            .unwrap_err()
            .contains("out of range"));
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
        assert!(preview.contains("1. Preamble — new preamble"));
        assert!(preview.contains("2. Build & test — new section"));
        assert!(preview.contains("merge only verified repo facts"));
    }

    #[test]
    fn init_candidate_section_summaries_label_new_and_added_lines() {
        let existing = "# Demo\n\n## Build\n- cargo test\n\n## Style\n- Use rustfmt\n";
        let candidate = "# Candidate\n\n## Build\n- cargo test\n- cargo clippy\n\n## Style\n- Use rustfmt\n\n## Deploy\n- ship manually\n";
        let summaries = init_candidate_section_summaries(existing, candidate);
        assert_eq!(
            summaries,
            vec![
                InitSectionSummary {
                    title: "Preamble".to_string(),
                    status: "new preamble".to_string(),
                },
                InitSectionSummary {
                    title: "Build".to_string(),
                    status: "adds 1 line".to_string(),
                },
                InitSectionSummary {
                    title: "Style".to_string(),
                    status: "unchanged".to_string(),
                },
                InitSectionSummary {
                    title: "Deploy".to_string(),
                    status: "new section".to_string(),
                },
            ]
        );
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
    fn shell_escape_prompt_context_captures_output_and_exit() {
        let result = ShellEscapeResult {
            stdout: "ok\n".to_string(),
            stderr: "warn\n".to_string(),
            exit_code: Some(7),
        };
        let context = shell_escape_prompt_context("make check", &result);
        assert!(context.contains("Local shell command run before this prompt:"));
        assert!(context.contains("$ make check"));
        assert!(context.contains("stdout:\nok"));
        assert!(context.contains("stderr:\nwarn"));
        assert!(context.contains("exit: 7"));
    }

    #[test]
    fn apply_pending_shell_context_prefixes_next_prompt() {
        let prompt = apply_pending_shell_context(
            &[
                "Local shell command run before this prompt:\n$ git status\nstdout:\nclean\nstderr: (empty)\nexit: 0"
                    .to_string(),
            ],
            "What changed?",
        );
        assert!(prompt.starts_with("Context from local shell escape commands"));
        assert!(prompt.contains("$ git status"));
        assert!(prompt.ends_with("User prompt:\nWhat changed?"));
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
    fn memory_file_selector_parses_read_aliases() {
        assert_eq!(memory_file_selector("file 1"), Some("1"));
        assert_eq!(memory_file_selector("read memory/project/foo.md"), Some("memory/project/foo.md"));
        assert_eq!(memory_file_selector("show-file entry.md"), Some("entry.md"));
        assert_eq!(memory_file_selector("files"), None);
        assert_eq!(memory_file_selector("file"), None);

        let hint = help_command_arg_hint("memory");
        assert!(hint.contains("file <number|path>|read <number|path>|show-file <number|path>"));
        assert!(hint.contains("import-claude|migrate-claude|claude"));
        assert!(hint.contains("import-claude-all|migrate-claude-all|claude-all"));
        assert!(!hint.contains("list --json"));
    }

    #[test]
    fn memory_json_action_and_entry_counts_are_stable() {
        assert!(is_memory_json_action("json"));
        assert!(is_memory_json_action("--json"));
        assert!(is_memory_json_action("status --json"));
        assert!(is_memory_json_action("show --json"));
        assert!(!is_memory_json_action("status"));
        assert_eq!(
            memory_entry_counts(
                "- [user] remember me\n- [feedback] too long\n- [reference] ./README.md\n- project note\n"
            ),
            (1, 1, 1, 1)
        );
        let doc = crate::commands::code_memory::MemoryDocument {
            path: PathBuf::from("/tmp/MEMORY.md"),
            content: "- [user] remember me\n- [reference] ./README.md\n".to_string(),
            exists: true,
        };
        let payload = memory_json_payload(&doc, "status --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "memory");
        assert_eq!(payload["aliases"][0], "memory");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["entries"]["user"], 1);
        assert_eq!(payload["entries"]["reference"], 1);
        assert_eq!(payload["supported_actions"][4], "show --json");
        assert_eq!(payload["supported_actions"][19], "import path");
        assert_eq!(payload["supported_actions"][23], "import-claude-all");
    }

    #[test]
    fn remember_json_arg_and_payload_preview_without_writing() {
        assert_eq!(remember_json_note_arg("json"), Some(""));
        assert_eq!(
            remember_json_note_arg("json feedback: too verbose"),
            Some("feedback: too verbose")
        );
        assert_eq!(
            remember_json_note_arg("reference: docs --json"),
            Some("reference: docs")
        );
        assert_eq!(remember_json_note_arg("project: keep tests"), None);

        let temp = tempfile::tempdir().unwrap();
        let payload = remember_json_payload(temp.path(), "feedback: too verbose");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "remember");
        assert_eq!(payload["kind"], "feedback");
        assert_eq!(payload["text"], "too verbose");
        assert_eq!(payload["valid"], true);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["supported_kinds"][3], "reference");
        assert_eq!(payload["supported_actions"][7], "status --json");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "show --json"));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "preview --json"));
    }

    #[test]
    fn select_memory_sidecar_matches_index_path_name_and_title() {
        let files = vec![crate::commands::code_memory::MemoryFileEntry {
            kind: crate::commands::code_memory::MemoryKind::Project,
            path: PathBuf::from("/tmp/memory/project/entry.md"),
            title: "Important note".to_string(),
        }];
        assert_eq!(select_memory_sidecar(&files, "1").unwrap().title, "Important note");
        assert_eq!(select_memory_sidecar(&files, "/tmp/memory/project/entry.md").unwrap().title, "Important note");
        assert_eq!(select_memory_sidecar(&files, "entry.md").unwrap().title, "Important note");
        assert_eq!(select_memory_sidecar(&files, "important note").unwrap().title, "Important note");
        assert!(select_memory_sidecar(&files, "2").is_none());
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
            parse_usage_export_command("/usage json"),
            Some(UsageExportFormat::Json)
        );
        assert_eq!(
            parse_usage_export_command("/usage --json"),
            Some(UsageExportFormat::Json)
        );
        assert_eq!(
            parse_usage_export_command("/cost status --json"),
            Some(UsageExportFormat::Json)
        );
        assert_eq!(
            parse_usage_export_command("/usage csv"),
            Some(UsageExportFormat::Csv)
        );
        assert_eq!(
            parse_usage_export_command("/cost export csv"),
            Some(UsageExportFormat::Csv)
        );
        assert_eq!(parse_usage_export_command("/cost export xml"), None);
        assert!(usage_slash_usage_text().contains("/usage|/cost"));
        assert!(usage_slash_usage_text().contains("json|--json|status --json"));
        assert!(usage_slash_usage_text().contains("show --json|summary --json|tools --json"));
        assert!(usage_slash_usage_text().contains("export json|export csv"));
    }

    #[test]
    fn parse_usage_summary_accepts_status_show_and_tools_aliases() {
        assert_eq!(parse_usage_summary_command("/usage status"), Some(()));
        assert_eq!(parse_usage_summary_command("/usage show"), Some(()));
        assert_eq!(parse_usage_summary_command("/cost summary"), Some(()));
        assert_eq!(parse_usage_summary_command("/cost tools"), Some(()));
        assert_eq!(parse_usage_summary_command("/cost export"), None);
        assert_eq!(parse_usage_summary_command("/costtools"), None);
        assert_eq!(parse_usage_summary_command("/usage nonsense"), None);
        assert!(usage_slash_usage_text().contains("status|show|summary|tools"));
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
        assert!(report.contains("\"surface\": \"terminal\""));
        assert!(report.contains("\"command\": \"usage\""));
        assert!(report.contains("\"aliases\": ["));
        assert!(report.contains("\"status --json\""));
        assert!(report.contains("\"export csv\""));
        assert!(report.contains("\"pricing\""));
        assert!(report.contains("\"inputUsdPerMillion\": 1.0"));
        assert!(report.contains("\"outputUsdPerMillion\": 3.0"));
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
        assert!(report.contains("pricing_match,input_usd_per_million,output_usd_per_million,pricing_source"));
        assert!(report.contains("\"local,dev/unknown\""));
        assert!(report.contains("estimated duration-weighted attribution"));
        let priced = usage_export_csv(
            Some(&UsageSummary {
                turns: 1,
                last_input: 20,
                last_output: 10,
                output_total: 10,
                context_high_water: 20,
                context_window: 100,
                provider: "libertai".to_string(),
                model: "qwen3-coder-480b".to_string(),
            }),
            &[],
        );
        assert!(priced.contains("qwen3-coder-480b,1.00000000,3.00000000"));
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
        assert!(status_line_usage_text().contains("/statusline|/status-line"));
        assert!(status_line_usage_text().contains("json|--json|status --json|show --json"));
        assert!(status_line_usage_text().contains("template --json|info --json"));
        assert!(status_line_usage_text().contains("command reset|command clear"));
        assert!(status_line_usage_text().contains("reset|clear"));
        assert!(is_status_line_json_action("json"));
        assert!(is_status_line_json_action("--json"));
        assert!(is_status_line_json_action("status --json"));
        assert!(is_status_line_json_action("show --json"));
        assert!(!is_status_line_json_action("status"));
    }

    #[test]
    fn status_line_json_payload_reports_read_only_preview() {
        let cfg = LibertaiConfig {
            status_line_template: "{project} {model}".to_string(),
            status_line_command: "git branch --show-current".to_string(),
            ..Default::default()
        };
        let payload = status_line_json_payload(&cfg, "template --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "statusline");
        assert_eq!(payload["query"], "template --json");
        assert_eq!(payload["aliases"][0], "statusline");
        assert_eq!(payload["aliases"][1], "status-line");
        assert_eq!(payload["template"], "{project} {model}");
        assert_eq!(payload["status_command"], "git branch --show-current");
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["will_run_command"], false);
        assert_eq!(payload["tokens"][0], "project");
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("template --json"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("info --json"))
        );
    }

    #[test]
    fn parse_history_limit_defaults_and_clamps() {
        assert_eq!(parse_history_limit("").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("list").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("recent").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("latest").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("status").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("state").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("show").unwrap(), HISTORY_DEFAULT_LIMIT);
        assert_eq!(parse_history_limit("3").unwrap(), 3);
        assert_eq!(parse_history_limit("0").unwrap(), 1);
        assert_eq!(parse_history_limit("999").unwrap(), HISTORY_MAX_LIMIT);
        assert!(parse_history_limit("open").is_err());
        assert!(history_usage_text().contains("list|recent|latest"));
        assert!(history_usage_text().contains("status|state|show"));
        assert!(history_usage_text().contains("json|--json|status --json"));
        assert!(history_usage_text().contains("state --json|show --json"));
        assert_eq!(history_json_request_arg("json"), Some(String::new()));
        assert_eq!(history_json_request_arg("--json"), Some(String::new()));
        assert_eq!(
            history_json_request_arg("state --json"),
            Some(String::new())
        );
        assert_eq!(history_json_request_arg("show --json"), Some(String::new()));
        assert_eq!(history_json_request_arg("list --json"), Some(String::new()));
        assert_eq!(history_json_request_arg("json 3"), Some("3".to_string()));
        assert_eq!(history_json_request_arg("status"), None);
        let mut history = VecDeque::new();
        history.push_back("first prompt".to_string());
        history.push_back("second prompt".to_string());
        let payload = history_json_payload(&history, 1, "list --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "list --json");
        assert_eq!(payload["aliases"][0], "history");
        assert_eq!(payload["total"], 2);
        assert_eq!(payload["shown"], 1);
        assert_eq!(payload["supported_actions"][9], "status --json");
        assert_eq!(payload["supported_actions"][11], "show --json");
        assert_eq!(payload["supported_actions"][14], "latest --json");
        assert_eq!(payload["prompts"][0]["index"], 2);
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
        assert_eq!(tree_json_request_arg("json"), Some(String::new()));
        assert_eq!(tree_json_request_arg("--json"), Some(String::new()));
        assert_eq!(tree_json_request_arg("status --json"), Some(String::new()));
        assert_eq!(tree_json_request_arg("state --json"), Some(String::new()));
        assert_eq!(tree_json_request_arg("show --json"), Some(String::new()));
        assert_eq!(tree_json_request_arg("json src"), Some("src".to_string()));
        assert_eq!(tree_json_request_arg("src --json"), Some("src".to_string()));
        assert_eq!(tree_json_request_arg("MyDir --json"), Some("MyDir".to_string()));
        assert_eq!(tree_json_request_arg("src"), None);
        assert!(tree_usage_text().contains("json|--json|status --json"));
        assert!(tree_usage_text().contains("state --json|show --json|path --json"));
        let payload = project_tree_json_payload(temp.path(), 20, "src --json").unwrap();
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "tree");
        assert_eq!(payload["query"], "src --json");
        assert_eq!(payload["aliases"][0], "tree");
        assert_eq!(payload["supported_actions"][1], "--json");
        assert_eq!(payload["supported_actions"][2], "status --json");
        assert_eq!(payload["supported_actions"][5], "path --json");
        assert_eq!(payload["entries"][0]["kind"], "dir");
        assert!(
            payload["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "main.rs")
        );
    }

    #[test]
    fn parse_changelog_limit_defaults_and_clamps() {
        assert_eq!(parse_changelog_limit("").unwrap(), CHANGELOG_DEFAULT_LIMIT);
        assert_eq!(parse_changelog_limit("list").unwrap(), CHANGELOG_DEFAULT_LIMIT);
        assert_eq!(
            parse_changelog_limit("recent").unwrap(),
            CHANGELOG_DEFAULT_LIMIT
        );
        assert_eq!(
            parse_changelog_limit("latest").unwrap(),
            CHANGELOG_DEFAULT_LIMIT
        );
        assert_eq!(
            parse_changelog_limit("status").unwrap(),
            CHANGELOG_DEFAULT_LIMIT
        );
        assert_eq!(
            parse_changelog_limit("state").unwrap(),
            CHANGELOG_DEFAULT_LIMIT
        );
        assert_eq!(
            parse_changelog_limit("show").unwrap(),
            CHANGELOG_DEFAULT_LIMIT
        );
        assert_eq!(parse_changelog_limit("3").unwrap(), 3);
        assert_eq!(parse_changelog_limit("0").unwrap(), 1);
        assert_eq!(parse_changelog_limit("999").unwrap(), CHANGELOG_MAX_LIMIT);
        assert!(parse_changelog_limit("open").is_err());
        assert!(changelog_usage_text().contains("list|recent|latest"));
        assert!(changelog_usage_text().contains("status|state|show"));
        assert!(changelog_usage_text().contains("json|--json|status --json"));
        assert!(changelog_usage_text().contains("state --json|show --json"));
        assert!(changelog_usage_text().contains("list --json|recent --json|latest --json"));
        assert_eq!(changelog_json_request_arg("json"), Some(String::new()));
        assert_eq!(changelog_json_request_arg("--json"), Some(String::new()));
        assert_eq!(changelog_json_request_arg("state --json"), Some(String::new()));
        assert_eq!(changelog_json_request_arg("show --json"), Some(String::new()));
        assert_eq!(changelog_json_request_arg("list --json"), Some(String::new()));
        assert_eq!(changelog_json_request_arg("json 3"), Some("3".to_string()));
        assert_eq!(changelog_json_request_arg("status"), None);
        let payload = changelog_json_payload(
            2,
            "latest --json",
            vec![
                "abc1234 first commit".to_string(),
                "def5678 (HEAD -> main) second commit".to_string(),
            ],
        );
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "latest --json");
        assert_eq!(payload["aliases"][0], "changelog");
        assert_eq!(payload["limit"], 2);
        assert_eq!(payload["count"], 2);
        assert_eq!(payload["supported_actions"][9], "status --json");
        assert_eq!(payload["supported_actions"][11], "show --json");
        assert_eq!(payload["supported_actions"][14], "latest --json");
        assert_eq!(payload["commits"][0]["hash"], "abc1234");
        assert_eq!(payload["commits"][1]["summary"], "(HEAD -> main) second commit");
    }

    #[test]
    fn parse_sandbox_action_accepts_info_reload_and_unknown() {
        assert_eq!(parse_sandbox_action(""), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("info"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("STATUS"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("state"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("show"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("diagnostics"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("diag"), SandboxAction::Info);
        assert_eq!(parse_sandbox_action("json"), SandboxAction::Json);
        assert_eq!(parse_sandbox_action("--json"), SandboxAction::Json);
        assert_eq!(parse_sandbox_action("status --json"), SandboxAction::Json);
        assert_eq!(parse_sandbox_action("state --json"), SandboxAction::Json);
        assert_eq!(parse_sandbox_action("show --json"), SandboxAction::Json);
        assert_eq!(parse_sandbox_action("info --json"), SandboxAction::Json);
        assert_eq!(
            parse_sandbox_action("diagnostics --json"),
            SandboxAction::Json
        );
        assert_eq!(parse_sandbox_action("diag --json"), SandboxAction::Json);
        assert_eq!(parse_sandbox_action("reload"), SandboxAction::Reload);
        assert_eq!(parse_sandbox_action("reset"), SandboxAction::Unknown("reset"));
        assert!(sandbox_usage_text().contains("status|state|show"));
        assert!(sandbox_usage_text().contains("diagnostics|diag"));
        assert!(sandbox_usage_text().contains("json|--json|status --json|state --json"));
        assert!(sandbox_usage_text().contains("info --json|diagnostics --json|diag --json"));
        assert!(sandbox_usage_text().contains("reload"));
    }

    #[test]
    fn sandbox_json_payload_reports_profile_counts() {
        let profile = crate::commands::code_sandbox::detect_strict_profile(Path::new(
            env!("CARGO_MANIFEST_DIR"),
        ));
        let payload = sandbox_json_payload(&profile, "diagnostics --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "sandbox");
        assert_eq!(payload["query"], "diagnostics --json");
        assert_eq!(payload["cwd"], env!("CARGO_MANIFEST_DIR"));
        assert_eq!(payload["network_allowed"], false);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["will_reload"], false);
        assert!(payload["binds"]["count"].as_u64().unwrap() > 0);
        assert!(payload["binds"]["bin_count"].as_u64().unwrap() > 0);
        assert_eq!(payload["aliases"][0], "sandbox");
        assert_eq!(payload["supported_actions"][7], "--json");
        assert_eq!(payload["supported_actions"][8], "status --json");
        assert_eq!(payload["supported_actions"][9], "state --json");
        assert_eq!(payload["supported_actions"][13], "diag --json");
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

        let background_records = vec![
            BackgroundAgentRecord {
                pid: 1,
                run_id: "bg-10-1".to_string(),
                name: "running".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "one".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/one.log".to_string(),
                started_at_ms: 10,
                launched_argv: Vec::new(),
            },
            BackgroundAgentRecord {
                pid: 2,
                run_id: "bg-20-2".to_string(),
                name: "done".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "two".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/two.log".to_string(),
                started_at_ms: 20,
                launched_argv: Vec::new(),
            },
            BackgroundAgentRecord {
                pid: 3,
                run_id: "bg-30-3".to_string(),
                name: "unknown".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "three".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/three.log".to_string(),
                started_at_ms: 30,
                launched_argv: Vec::new(),
            },
        ];
        assert_eq!(
            format_background_agent_doctor_summary(&background_records, |pid| match pid {
                1 => BackgroundAgentStatus::Running,
                2 => BackgroundAgentStatus::Exited,
                _ => BackgroundAgentStatus::Unknown,
            }),
            "3 recorded (1 running, 1 exited, 1 unknown)"
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
                body: String::new(),
                source: "builtin".to_string(),
                source_kind: "builtin".to_string(),
                path: None,
                agent_created: false,
                enabled: true,
            },
            crate::commands::code_skills::SkillInventoryEntry {
                name: "project-review".to_string(),
                description: String::new(),
                allowed_tools: None,
                body: String::new(),
                source: "project".to_string(),
                source_kind: "project".to_string(),
                path: Some(PathBuf::from("/tmp/project-review")),
                agent_created: false,
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

    #[test]
    fn resolve_repl_resume_path_accepts_session_identifiers() {
        let sessions = vec![
            crate::commands::code_session::SessionMeta {
                path: "/tmp/project/release.jsonl".to_string(),
                id: "s_release".to_string(),
                cwd: "/tmp/project".to_string(),
                timestamp: "2026-05-31T00:00:00Z".to_string(),
                message_count: 3,
                last_modified_ms: 1,
                size_bytes: 128,
                name: Some("release".to_string()),
            },
            crate::commands::code_session::SessionMeta {
                path: "/tmp/project/other.jsonl".to_string(),
                id: "s_other".to_string(),
                cwd: "/tmp/project".to_string(),
                timestamp: "2026-05-30T00:00:00Z".to_string(),
                message_count: 1,
                last_modified_ms: 0,
                size_bytes: 64,
                name: None,
            },
        ];
        assert_eq!(
            resolve_repl_resume_path_from_sessions("", &sessions).unwrap(),
            PathBuf::from("/tmp/project/release.jsonl")
        );
        assert_eq!(
            resolve_repl_resume_path_from_sessions("s_release", &sessions).unwrap(),
            PathBuf::from("/tmp/project/release.jsonl")
        );
        assert_eq!(
            resolve_repl_resume_path_from_sessions("release", &sessions).unwrap(),
            PathBuf::from("/tmp/project/release.jsonl")
        );
        assert_eq!(
            resolve_repl_resume_path_from_sessions("other.jsonl", &sessions).unwrap(),
            PathBuf::from("/tmp/project/other.jsonl")
        );
        assert!(resolve_repl_resume_path_from_sessions("missing", &sessions).is_err());
    }

    #[test]
    fn resume_preview_arg_preserves_explicit_paths() {
        assert_eq!(resume_preview_arg("/resume"), None);
        assert_eq!(resume_preview_arg("/resume status"), Some("status"));
        assert_eq!(resume_preview_arg("/resume json"), Some("json"));
        assert_eq!(
            resume_preview_arg("/resume status --json"),
            Some("status --json")
        );
        assert_eq!(resume_preview_arg("/resume /tmp/session.jsonl"), None);
        assert_eq!(resume_preview_arg("/resumable status"), None);
        assert_eq!(
            parse_resume_preview_command("status"),
            ResumePreviewCommand::Status
        );
        assert_eq!(
            parse_resume_preview_command("json"),
            ResumePreviewCommand::Json
        );
        assert_eq!(
            parse_resume_preview_command("--json"),
            ResumePreviewCommand::Json
        );
        assert_eq!(
            parse_resume_preview_command("status --json"),
            ResumePreviewCommand::Json
        );
        assert_eq!(
            parse_resume_preview_command("path"),
            ResumePreviewCommand::Usage
        );
        assert!(resume_usage_text().contains("json|--json|status --json|state --json"));
        assert!(resume_usage_text().contains("preview --json|session|path"));
        assert!(help_command_arg_hint("resume").contains("session|path"));

        let cwd = PathBuf::from("/tmp/project");
        let payload = resume_json_payload_from_rows(
            &cwd,
            "status --json",
            vec![json!({
                "id": "s1",
                "name": "release",
                "path": "/tmp/project/session.jsonl",
                "cwd": "/tmp/project",
                "timestamp": "2026-05-31T00:00:00Z",
                "message_count": 3,
                "last_modified_ms": 1,
                "size_bytes": 128,
            })],
        );
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "resume");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["aliases"][0], "resume");
        assert_eq!(payload["available"], true);
        assert_eq!(payload["candidate_count"], 1);
        assert_eq!(payload["default_target"]["id"], "s1");
        assert_eq!(payload["will_replace_current_repl_session"], true);
        assert_eq!(payload["accepts_path"], true);
        assert_eq!(payload["query_argument"], "/resume SESSION");
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("state --json"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("preview --json"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("session"))
        );
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
        assert_eq!(thinking_command_arg("/thinking status"), Some("status"));
        assert_eq!(thinking_command_arg("/think low"), Some("low"));
        assert_eq!(thinking_command_arg("/think show"), Some("show"));
        assert_eq!(thinking_command_arg("/t medium"), Some("medium"));
        assert_eq!(thinking_command_arg("/t current"), Some("current"));
        assert_eq!(thinking_command_arg("/thinking"), Some(""));
        assert_eq!(thinking_command_arg("/think"), Some(""));
        assert_eq!(thinking_command_arg("/t"), Some(""));
        assert_eq!(thinking_command_arg("/theme high"), None);
        assert!(is_thinking_status_arg(""));
        assert!(is_thinking_status_arg("status"));
        assert!(is_thinking_status_arg("show"));
        assert!(is_thinking_status_arg("current"));
        assert!(is_thinking_status_arg("info"));
        assert!(!is_thinking_status_arg("high"));
        assert!(is_thinking_json_arg("json"));
        assert!(is_thinking_json_arg("--json"));
        assert!(is_thinking_json_arg("status --json"));
        assert!(is_thinking_json_arg("show --json"));
        assert!(is_thinking_json_arg("current --json"));
        assert!(is_thinking_json_arg("info --json"));
        assert!(!is_thinking_json_arg("high"));
        assert!(parse_thinking_level("").unwrap_err().to_string().contains("--json"));
        let payload = thinking_json_payload(ThinkingLevel::High, "current --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "thinking");
        assert_eq!(payload["query"], "current --json");
        assert_eq!(payload["current"], "high");
        assert_eq!(payload["will_change"], false);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("--json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("info --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("xhigh")));
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
        assert_eq!(parse_name_command("status"), NameCommand::Status);
        assert_eq!(parse_name_command("state"), NameCommand::Status);
        assert_eq!(parse_name_command("show"), NameCommand::Status);
        assert_eq!(parse_name_command("current"), NameCommand::Status);
        assert_eq!(parse_name_command("info"), NameCommand::Status);
        assert_eq!(parse_name_command("json"), NameCommand::Json);
        assert_eq!(parse_name_command("--json"), NameCommand::Json);
        assert_eq!(parse_name_command("status --json"), NameCommand::Json);
        assert_eq!(parse_name_command("state --json"), NameCommand::Json);
        assert_eq!(parse_name_command("show --json"), NameCommand::Json);
        assert_eq!(parse_name_command("current --json"), NameCommand::Json);
        assert_eq!(parse_name_command("info --json"), NameCommand::Json);
        assert_eq!(parse_name_command("release work"), NameCommand::Set);
        assert!(name_usage_text().contains("status|state|show|current|info"));
        assert!(name_usage_text().contains("json|--json|status --json"));
        assert!(name_usage_text().contains("state --json|show --json"));
        assert!(name_usage_text().contains("current --json|info --json"));
        let payload = name_json_payload(Some("release work"), "current --json");
        assert_eq!(payload["command"], "name");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "current --json");
        assert_eq!(payload["aliases"][1], "rename");
        assert_eq!(payload["current"], "release work");
        assert_eq!(payload["is_named"], true);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("current --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("info --json")));
    }

    #[test]
    fn compact_command_notes_accepts_only_compact_prefix() {
        assert_eq!(compact_command_notes("/compact keep setup"), Some("keep setup"));
        assert_eq!(compact_command_notes("/compact   "), Some(""));
        assert_eq!(compact_command_notes("/compact"), None);
        assert_eq!(compact_command_notes("/compactly keep"), None);
    }

    #[test]
    fn compact_preview_arg_preserves_freeform_notes() {
        assert_eq!(compact_preview_arg("/compact"), None);
        assert_eq!(compact_preview_arg("/compact status"), Some("status"));
        assert_eq!(compact_preview_arg("/compact json"), Some("json"));
        assert_eq!(
            compact_preview_arg("/compact status --json"),
            Some("status --json")
        );
        assert_eq!(compact_preview_arg("/compact keep setup"), None);
        assert_eq!(compact_preview_arg("/compactly status"), None);
        assert_eq!(
            parse_compact_preview_command("status"),
            CompactPreviewCommand::Status
        );
        assert_eq!(
            parse_compact_preview_command("json"),
            CompactPreviewCommand::Json
        );
        assert_eq!(
            parse_compact_preview_command("--json"),
            CompactPreviewCommand::Json
        );
        assert_eq!(
            parse_compact_preview_command("status --json"),
            CompactPreviewCommand::Json
        );
        assert_eq!(
            parse_compact_preview_command("show --json"),
            CompactPreviewCommand::Json
        );
        assert_eq!(
            parse_compact_preview_command("preview --json"),
            CompactPreviewCommand::Json
        );
        assert_eq!(
            parse_compact_preview_command("notes please"),
            CompactPreviewCommand::Usage
        );
        assert!(compact_usage_text().contains("json|--json|status --json"));
        assert!(compact_usage_text().contains("show --json|info --json|preview --json|notes"));
        let cfg = LibertaiConfig::default();
        let payload = compact_json_payload(&cfg, "preview --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "compact");
        assert_eq!(payload["query"], "preview --json");
        assert_eq!(payload["available"], true);
        assert_eq!(payload["active_turn"], false);
        assert_eq!(payload["will_compact_history"], true);
        assert_eq!(payload["accepts_notes"], true);
        assert_eq!(payload["aliases"][0], "compact");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("preview --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("info --json")));
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
    fn loop_json_request_arg_accepts_preview_forms() {
        assert_eq!(loop_json_request_arg("json"), Some(""));
        assert_eq!(loop_json_request_arg("--json"), Some(""));
        assert_eq!(loop_json_request_arg("status --json"), Some(""));
        assert_eq!(
            loop_json_request_arg("json 2 close gaps"),
            Some("2 close gaps")
        );
        assert_eq!(
            loop_json_request_arg("--json close gaps"),
            Some("close gaps")
        );
        assert_eq!(loop_json_request_arg("2 close gaps"), None);
    }

    #[test]
    fn autonomous_loop_prompt_matches_desktop_contract() {
        let prompt = autonomous_loop_prompt(2, 4, "close gaps");
        assert!(prompt.contains("Autonomous loop turn 2/4."));
        assert!(prompt.contains("Goal: close gaps"));
        assert!(prompt.contains("do not invent extra work"));

        let payload = loop_json_payload("2 close gaps");
        assert_eq!(payload["command"], "loop");
        assert_eq!(payload["query"], "2 close gaps");
        assert_eq!(payload["requested_turns"], 2);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("status --json")));
    }

    #[test]
    fn auto_command_arg_accepts_aliases() {
        assert_eq!(auto_command_arg("/auto"), Some(""));
        assert_eq!(auto_command_arg("/auto on 5 ship it"), Some("on 5 ship it"));
        assert_eq!(auto_command_arg("/autorun status"), Some("status"));
        assert_eq!(auto_command_arg("/continuous off"), Some("off"));
        assert_eq!(auto_command_arg("/continuous state"), Some("state"));
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
        assert_eq!(schedule_command_arg("/cron stop"), Some("stop"));
        assert_eq!(schedule_command_arg("/scheduler"), None);
    }

    #[test]
    fn notify_command_arg_and_parser_match_desktop_contract() {
        assert_eq!(notify_command_arg("/notify"), Some(""));
        assert_eq!(notify_command_arg("/notify on"), Some("on"));
        assert_eq!(notify_command_arg("/notifications status"), Some("status"));
        assert_eq!(notify_command_arg("/notifications status --json"), Some("status --json"));
        assert_eq!(notify_command_arg("/notifications clear"), Some("clear"));
        assert_eq!(notify_command_arg("/notifier"), None);
        assert_eq!(parse_notify_command(""), NotifyCommand::Status);
        assert_eq!(parse_notify_command("status"), NotifyCommand::Status);
        assert_eq!(parse_notify_command("state"), NotifyCommand::Status);
        assert_eq!(parse_notify_command("show"), NotifyCommand::Status);
        assert_eq!(parse_notify_command("json"), NotifyCommand::Json);
        assert_eq!(parse_notify_command("--json"), NotifyCommand::Json);
        assert_eq!(parse_notify_command("status --json"), NotifyCommand::Json);
        assert_eq!(parse_notify_command("state --json"), NotifyCommand::Json);
        assert_eq!(parse_notify_command("show --json"), NotifyCommand::Json);
        assert_eq!(parse_notify_command("on"), NotifyCommand::On);
        assert_eq!(parse_notify_command("enable"), NotifyCommand::On);
        assert_eq!(parse_notify_command("enabled"), NotifyCommand::On);
        assert_eq!(parse_notify_command("off"), NotifyCommand::Off);
        assert_eq!(parse_notify_command("disable"), NotifyCommand::Off);
        assert_eq!(parse_notify_command("disabled"), NotifyCommand::Off);
        assert_eq!(parse_notify_command("clear"), NotifyCommand::Off);
        assert_eq!(parse_notify_command("test"), NotifyCommand::Test);
        assert_eq!(parse_notify_command("ping"), NotifyCommand::Test);
        assert_eq!(parse_notify_command("wat"), NotifyCommand::Usage);
        assert!(notify_usage_text().contains("json|--json|status --json"));
        assert!(notify_usage_text().contains("state --json|show --json"));
        assert_eq!(
            help_command_arg_hint("notify"),
            notify_usage_text()
                .trim_start_matches("/notify [")
                .trim_end_matches(']')
        );
        let cfg = LibertaiConfig {
            code_turn_notifications: true,
            ..LibertaiConfig::default()
        };
        let payload = notify_json_payload(&cfg, "status --json");
        assert_eq!(payload["command"], "notify");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["aliases"][1], "notifications");
        assert_eq!(payload["turn_notifications"], true);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("ping")));
    }

    #[test]
    fn config_status_aliases_match_desktop_palette() {
        assert!(is_config_status_alias(""));
        assert!(is_config_status_alias("status"));
        assert!(is_config_status_alias("show"));
        assert!(is_config_status_alias("current"));
        assert!(is_config_status_alias("info"));
        assert!(!is_config_status_alias("path"));
        assert!(!is_config_status_alias("set code_turn_notifications true"));
        assert!(is_config_json_alias("json"));
        assert!(is_config_json_alias("--json"));
        assert!(is_config_json_alias("status --json"));
        assert!(is_config_json_alias("show --json"));
        assert!(is_config_json_alias("current --json"));
        assert!(is_config_json_alias("info --json"));
        assert!(!is_config_json_alias("path --json"));
        let payload = config_status_payload(&LibertaiConfig::default(), "status --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "config");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["aliases"][1], "settings");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "--json"));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "status --json"));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "path"));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "set <key> <value>"));
        let config_help = help_json_payload("commands --json")["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == "config")
            .unwrap()
            .clone();
        assert!(config_help["aliases"]
            .as_array()
            .unwrap()
            .contains(&json!("settings")));
        let hint = help_command_arg_hint("config");
        assert!(hint.contains("backends|defaults|agents|skills"));
        assert!(hint.contains("hooks|mcp|approvals|appearance|sandbox|advanced"));
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
        assert_eq!(hooks_command_arg("/hook"), Some(""));
        assert_eq!(hooks_command_arg("/hook status"), Some("status"));
        assert_eq!(hooks_command_arg("/hook open"), Some("open"));
        assert_eq!(parse_hooks_command(""), HooksCommand::Status);
        assert_eq!(parse_hooks_command("list"), HooksCommand::Status);
        assert_eq!(parse_hooks_command("diagnostics"), HooksCommand::Status);
        assert_eq!(parse_hooks_command("json"), HooksCommand::Json);
        assert_eq!(parse_hooks_command("--json"), HooksCommand::Json);
        assert_eq!(parse_hooks_command("status --json"), HooksCommand::Json);
        assert_eq!(parse_hooks_command("list --json"), HooksCommand::Json);
        assert_eq!(parse_hooks_command("state --json"), HooksCommand::Json);
        assert_eq!(
            parse_hooks_command("diagnostics --json"),
            HooksCommand::Json
        );
        assert_eq!(parse_hooks_command("show --json"), HooksCommand::Json);
        assert_eq!(parse_hooks_command("open"), HooksCommand::Open);
        assert_eq!(parse_hooks_command("settings"), HooksCommand::Open);
        assert_eq!(parse_hooks_command("edit"), HooksCommand::Open);
        assert_eq!(
            parse_hooks_command("show PreToolUse"),
            HooksCommand::Show("PreToolUse".to_string())
        );
        assert_eq!(
            parse_hooks_command("inspect notification"),
            HooksCommand::Show("notification".to_string())
        );
        assert_eq!(parse_hooks_command("show"), HooksCommand::Usage);
        assert_eq!(parse_hooks_command("show pre post"), HooksCommand::Usage);
        assert!(HOOKS_USAGE.contains("diagnostics|diag"));
        assert!(HOOKS_USAGE.contains("json|--json|status --json|list --json"));
        assert!(HOOKS_USAGE.contains("diagnostics --json|diag --json|show --json"));
        assert!(HOOKS_USAGE.contains("show|event|inspect"));
        assert!(HOOKS_USAGE.contains("settings|edit"));
        assert_eq!(
            help_command_arg_hint("hooks"),
            HOOKS_USAGE
                .trim_start_matches("/hooks [")
                .trim_end_matches(']')
        );
    }

    #[test]
    fn hooks_json_payload_reports_counts_without_secret_values() {
        let cfg = LibertaiConfig {
            hooks: crate::config::HooksConfig {
                pre_tool_use: vec![
                    crate::config::HookCommandConfig {
                        enabled: true,
                        hook_type: "http".to_string(),
                        url: "https://hooks.example/pre".to_string(),
                        headers: std::collections::HashMap::from([(
                            "Authorization".to_string(),
                            "Bearer secret-token".to_string(),
                        )]),
                        allowed_env_vars: vec!["TOKEN".to_string()],
                        matcher: "Bash(*)".to_string(),
                        source: "project".to_string(),
                        timeout: Some(7),
                        continue_on_block: true,
                        review_policy: "strict".to_string(),
                        extra: std::collections::BTreeMap::from([(
                            "customFlag".to_string(),
                            serde_json::json!(true),
                        )]),
                        ..Default::default()
                    },
                    crate::config::HookCommandConfig {
                        enabled: false,
                        hook_type: "mcp-tool".to_string(),
                        server: "policy".to_string(),
                        tool: "check".to_string(),
                        input: Some(serde_json::json!({"level": "strict"})),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let payload = hooks_json_payload(&cfg, "diagnostics --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "hooks");
        assert_eq!(payload["query"], "diagnostics --json");
        assert_eq!(payload["count"], 2);
        assert_eq!(payload["enabled_count"], 1);
        assert_eq!(payload["configured_count"], 2);
        assert_eq!(payload["hooks"][0]["event"], "PreToolUse");
        assert_eq!(payload["hooks"][0]["type"], "http");
        assert_eq!(payload["hooks"][0]["headers"], 1);
        assert_eq!(payload["hooks"][0]["allowed_env_vars"], 1);
        assert_eq!(payload["hooks"][0]["metadata_keys"][0], "customFlag");
        assert_eq!(payload["hooks"][1]["type"], "mcp_tool");
        assert_eq!(payload["hooks"][1]["has_input"], true);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["aliases"][0], "hooks");
        assert_eq!(payload["aliases"][1], "hook");
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("show --json"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("inspect <event>"))
        );
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(!raw.contains("secret-token"));
        assert!(!raw.contains("Authorization"));
    }

    #[test]
    fn hook_event_details_expands_one_event_without_secret_values() {
        let hooks = vec![
            crate::config::HookCommandConfig {
                hook_type: "http".to_string(),
                url: "https://hooks.example/pre".to_string(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer secret-token".to_string(),
                )]),
                allowed_env_vars: vec!["TOKEN".to_string()],
                matcher: "Bash(*)".to_string(),
                source: "project".to_string(),
                timeout: Some(7),
                continue_on_block: true,
                review_policy: "strict".to_string(),
                extra: std::collections::BTreeMap::from([(
                    "customFlag".to_string(),
                    serde_json::json!(true),
                )]),
                ..Default::default()
            },
            crate::config::HookCommandConfig {
                enabled: false,
                hook_type: "mcp-tool".to_string(),
                server: "policy".to_string(),
                tool: "check".to_string(),
                input: Some(serde_json::json!({"level": "strict"})),
                ..Default::default()
            },
        ];

        let details = format_hook_event_details("PreToolUse", &hooks);
        assert!(details.contains("hooks: PreToolUse"));
        assert!(details.contains("configured: 2 (1 enabled)"));
        assert!(details.contains("types: http 1, mcp_tool 1"));
        assert!(details.contains("matcher=Bash(*) target=https://hooks.example/pre"));
        assert!(details.contains("http metadata: 1 header(s), 1 allowed env var(s)"));
        assert!(details.contains("reviewPolicy: strict"));
        assert!(details.contains("metadata: customFlag"));
        assert!(!details.contains("metadata: reviewPolicy"));
        assert!(details.contains("target=policy:check"));
        assert!(details.contains("mcp input: yes"));
        assert!(!details.contains("secret-token"));
        assert!(!details.contains("Authorization"));
    }

    #[test]
    fn mcp_command_arg_and_parser_report_terminal_status() {
        assert_eq!(mcp_command_arg("/mcp"), Some(""));
        assert_eq!(mcp_command_arg("/mcp status"), Some("status"));
        assert_eq!(mcp_command_arg("/mcp show docs"), Some("show docs"));
        assert_eq!(mcp_command_arg("/mcp open"), Some("open"));
        assert_eq!(mcp_command_arg("/mc"), None);
        assert_eq!(parse_mcp_command(""), McpCommand::Status);
        assert_eq!(parse_mcp_command("list"), McpCommand::Status);
        assert_eq!(parse_mcp_command("json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("--json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("status --json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("list --json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("state --json"), McpCommand::Json);
        assert_eq!(
            parse_mcp_command("diagnostics --json"),
            McpCommand::Json
        );
        assert_eq!(parse_mcp_command("show --json"), McpCommand::Json);
        assert_eq!(
            parse_mcp_command("show docs"),
            McpCommand::Show("docs".to_string())
        );
        assert_eq!(
            parse_mcp_command("inspect github"),
            McpCommand::Show("github".to_string())
        );
        assert_eq!(parse_mcp_command("diagnostics"), McpCommand::Status);
        assert_eq!(parse_mcp_command("probe"), McpCommand::Probe);
        assert_eq!(parse_mcp_command("probe --save"), McpCommand::ProbeSave);
        assert_eq!(parse_mcp_command("probe --write"), McpCommand::ProbeSave);
        assert_eq!(parse_mcp_command("refresh"), McpCommand::ProbeSave);
        assert_eq!(parse_mcp_command("reset"), McpCommand::Reset);
        assert_eq!(parse_mcp_command("reset-sessions"), McpCommand::Reset);
        assert_eq!(parse_mcp_command("open"), McpCommand::Open);
        assert_eq!(parse_mcp_command("settings"), McpCommand::Open);
        assert_eq!(parse_mcp_command("edit"), McpCommand::Open);
        assert_eq!(parse_mcp_command("remote"), McpCommand::Usage);
        assert!(MCP_USAGE.contains("show|json|--json|status --json|list --json"));
        assert!(MCP_USAGE.contains("diagnostics --json|diag --json|show --json"));
        assert!(MCP_USAGE.contains("probe|probes"));
        assert!(MCP_USAGE.contains("reset|reset-sessions"));
        assert!(MCP_USAGE.contains("settings|edit"));
        assert_eq!(
            help_command_arg_hint("mcp"),
            MCP_USAGE.trim_start_matches("/mcp [").trim_end_matches(']')
        );
    }

    #[test]
    fn mcp_json_payload_reports_exposure_and_servers() {
        let cfg = LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "docs".to_string(),
                crate::config::McpServerConfig {
                    transport: "stdio".to_string(),
                    command: "npx".to_string(),
                    args: vec!["-y".to_string(), "@modelcontextprotocol/server-docs".to_string()],
                    env: std::collections::HashMap::from([(
                        "DOCS_TOKEN".to_string(),
                        "secret".to_string(),
                    )]),
                    headers: std::collections::HashMap::from([(
                        "Authorization".to_string(),
                        "Bearer secret".to_string(),
                    )]),
                    roots: vec!["/tmp/project".to_string()],
                    tools: vec![
                        crate::config::McpToolConfig {
                            name: "search".to_string(),
                            enabled: true,
                            description: "Search docs".to_string(),
                            ..crate::config::McpToolConfig::default()
                        },
                        crate::config::McpToolConfig {
                            name: "admin".to_string(),
                            enabled: false,
                            ..crate::config::McpToolConfig::default()
                        },
                    ],
                    resources: vec![crate::config::McpResourceConfig {
                        uri: "file:///tmp/project/README.md".to_string(),
                        enabled: true,
                        name: "README".to_string(),
                        ..crate::config::McpResourceConfig::default()
                    }],
                    prompts: vec![crate::config::McpPromptConfig {
                        name: "summarize".to_string(),
                        enabled: true,
                        description: "Summarize docs".to_string(),
                        ..crate::config::McpPromptConfig::default()
                    }],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let payload = mcp_json_payload(&cfg, "state --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "mcp");
        assert_eq!(payload["query"], "state --json");
        assert_eq!(payload["configured_servers"], 1);
        assert_eq!(payload["exposure"]["mcp_call"], true);
        assert_eq!(payload["exposure"]["named_tools"], 1);
        assert_eq!(payload["exposure"]["resource_reader"], true);
        assert_eq!(payload["servers"][0]["name"], "docs");
        assert_eq!(payload["servers"][0]["target"], "npx '-y' '@modelcontextprotocol/server-docs'");
        assert_eq!(payload["servers"][0]["env_vars"], 1);
        assert_eq!(payload["servers"][0]["headers"], 1);
        assert_eq!(payload["servers"][0]["enabled_tools"], 1);
        assert_eq!(payload["servers"][0]["enabled_resources"], 1);
        assert_eq!(payload["servers"][0]["enabled_prompts"], 1);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["aliases"][0], "mcp");
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("probe write"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("settings"))
        );
    }

    #[test]
    fn format_mcp_server_details_lists_cache_without_secret_values() {
        let server = crate::config::McpServerConfig {
            transport: "stdio".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-docs".to_string()],
            env: std::collections::HashMap::from([(
                "DOCS_TOKEN".to_string(),
                "secret".to_string(),
            )]),
            headers: std::collections::HashMap::from([(
                "Authorization".to_string(),
                "Bearer secret".to_string(),
            )]),
            tools: vec![
                crate::config::McpToolConfig {
                    name: "search".to_string(),
                    enabled: true,
                    description: "Search docs".to_string(),
                    ..crate::config::McpToolConfig::default()
                },
                crate::config::McpToolConfig {
                    name: "admin".to_string(),
                    enabled: false,
                    ..crate::config::McpToolConfig::default()
                },
            ],
            resources: vec![crate::config::McpResourceConfig {
                uri: "file:///repo/README.md".to_string(),
                enabled: true,
                name: "README".to_string(),
                mime_type: "text/markdown".to_string(),
                ..crate::config::McpResourceConfig::default()
            }],
            prompts: vec![crate::config::McpPromptConfig {
                name: "summarize".to_string(),
                enabled: true,
                description: "Summarize docs".to_string(),
                arguments: vec![crate::config::McpPromptArgumentConfig {
                    name: "topic".to_string(),
                    required: true,
                    ..crate::config::McpPromptArgumentConfig::default()
                }],
            }],
            ..crate::config::McpServerConfig::default()
        };
        let details = format_mcp_server_details("docs", &server);
        assert!(details.contains("mcp server: docs"));
        assert!(details.contains("target: npx '-y' '@modelcontextprotocol/server-docs'"));
        assert!(details.contains("env vars: 1"));
        assert!(details.contains("headers: 1"));
        assert!(details.contains("enabled cache: 1/2 tool(s), 1/1 resource(s), 1/1 prompt(s)"));
        assert!(details.contains("[on] search - Search docs"));
        assert!(details.contains("[off] admin"));
        assert!(details.contains("[on] README (text/markdown) - file:///repo/README.md"));
        assert!(details.contains("[on] summarize - Summarize docs args: topic*"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("Bearer"));
    }

    #[test]
    fn vim_command_arg_and_parser_capture_status_toggles() {
        assert_eq!(vim_command_arg("/vim"), Some(""));
        assert_eq!(vim_command_arg("/vim status"), Some("status"));
        assert_eq!(vim_command_arg("/vim on"), Some("on"));
        assert_eq!(vim_command_arg("/vim off"), Some("off"));
        assert_eq!(vim_command_arg("/vimrc"), None);
        assert_eq!(parse_vim_command(""), VimCommand::Status);
        assert_eq!(parse_vim_command("status"), VimCommand::Status);
        assert_eq!(parse_vim_command("current"), VimCommand::Status);
        assert_eq!(parse_vim_command("info"), VimCommand::Status);
        assert_eq!(parse_vim_command("json"), VimCommand::Json);
        assert_eq!(parse_vim_command("--json"), VimCommand::Json);
        assert_eq!(parse_vim_command("status --json"), VimCommand::Json);
        assert_eq!(parse_vim_command("info --json"), VimCommand::Json);
        assert_eq!(parse_vim_command("on"), VimCommand::Enable);
        assert_eq!(parse_vim_command("enable"), VimCommand::Enable);
        assert_eq!(parse_vim_command("enabled"), VimCommand::Enable);
        assert_eq!(parse_vim_command("true"), VimCommand::Enable);
        assert_eq!(parse_vim_command("off"), VimCommand::Disable);
        assert_eq!(parse_vim_command("disable"), VimCommand::Disable);
        assert_eq!(parse_vim_command("disabled"), VimCommand::Disable);
        assert_eq!(parse_vim_command("false"), VimCommand::Disable);
        assert_eq!(parse_vim_command("toggle"), VimCommand::Usage);
        assert!(VIM_USAGE.contains("current|info"));
        assert!(VIM_USAGE.contains("json|--json|status --json|state --json|show --json"));
        assert!(VIM_USAGE.contains("current --json|info --json"));
        assert!(VIM_USAGE.contains("enable|enabled|true"));
        assert!(VIM_USAGE.contains("disable|disabled|false"));
        assert_eq!(
            help_command_arg_hint("vim"),
            VIM_USAGE.trim_start_matches("/vim [").trim_end_matches(']')
        );
        VIM_INPUT_ENABLED.store(true, Ordering::SeqCst);
        let payload = vim_json_payload("current --json");
        assert_eq!(payload["command"], "vim");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["aliases"][0], "vim");
        assert_eq!(payload["query"], "current --json");
        assert_eq!(payload["enabled"], true);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("info --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("false")));
        VIM_INPUT_ENABLED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn vim_normal_key_action_maps_core_motion_and_insert_keys() {
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('h'), KeyModifiers::NONE),
            VimNormalAction::MoveLeft
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('l'), KeyModifiers::NONE),
            VimNormalAction::MoveRight
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('0'), KeyModifiers::NONE),
            VimNormalAction::Home
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('$'), KeyModifiers::SHIFT),
            VimNormalAction::End
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('x'), KeyModifiers::NONE),
            VimNormalAction::Delete
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('i'), KeyModifiers::NONE),
            VimNormalAction::InsertBefore
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('a'), KeyModifiers::NONE),
            VimNormalAction::InsertAfter
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('I'), KeyModifiers::SHIFT),
            VimNormalAction::InsertHome
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Char('A'), KeyModifiers::SHIFT),
            VimNormalAction::InsertEnd
        );
        assert_eq!(
            vim_normal_key_action(KeyCode::Enter, KeyModifiers::NONE),
            VimNormalAction::Submit
        );
    }

    #[test]
    fn ide_command_arg_and_parser_capture_status_and_open() {
        assert_eq!(ide_command_arg("/ide"), Some(""));
        assert_eq!(ide_command_arg("/ide status"), Some("status"));
        assert_eq!(ide_command_arg("/ide open"), Some("open"));
        assert_eq!(ide_command_arg("/idea"), None);
        assert_eq!(parse_ide_command(""), IdeCommand::Status);
        assert_eq!(parse_ide_command("status"), IdeCommand::Status);
        assert_eq!(parse_ide_command("state"), IdeCommand::Status);
        assert_eq!(parse_ide_command("show"), IdeCommand::Status);
        assert_eq!(parse_ide_command("json"), IdeCommand::Json);
        assert_eq!(parse_ide_command("--json"), IdeCommand::Json);
        assert_eq!(parse_ide_command("status --json"), IdeCommand::Json);
        assert_eq!(parse_ide_command("open"), IdeCommand::Open);
        assert_eq!(parse_ide_command("settings"), IdeCommand::Open);
        assert_eq!(parse_ide_command("edit"), IdeCommand::Open);
        assert_eq!(parse_ide_command("install"), IdeCommand::Usage);
        assert!(IDE_USAGE.contains("state|show"));
        assert!(IDE_USAGE.contains("json|--json|status --json|state --json|show --json"));
        assert!(IDE_USAGE.contains("settings|edit"));
        let payload = ide_json_payload("status --json");
        assert_eq!(payload["command"], "ide");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["dedicated_ide_bridge"], false);
        assert_eq!(payload["desktop_workspace_available"], true);
        assert_eq!(payload["aliases"][0], "ide");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("--json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("edit")));
    }

    #[test]
    fn bug_command_arg_and_parser_capture_template_aliases() {
        assert_eq!(bug_command_arg("/bug"), Some(""));
        assert_eq!(bug_command_arg("/bug report"), Some("report"));
        assert_eq!(bug_command_arg("/bug template"), Some("template"));
        assert_eq!(bug_command_arg("/bugfix"), None);
        assert_eq!(parse_bug_command(""), BugCommand::Template);
        assert_eq!(parse_bug_command("report"), BugCommand::Template);
        assert_eq!(parse_bug_command("template"), BugCommand::Template);
        assert_eq!(parse_bug_command("status"), BugCommand::Template);
        assert_eq!(parse_bug_command("show"), BugCommand::Template);
        assert_eq!(parse_bug_command("json"), BugCommand::Json);
        assert_eq!(parse_bug_command("--json"), BugCommand::Json);
        assert_eq!(parse_bug_command("status --json"), BugCommand::Json);
        assert_eq!(parse_bug_command("show --json"), BugCommand::Json);
        assert_eq!(parse_bug_command("template --json"), BugCommand::Json);
        assert_eq!(parse_bug_command("report --json"), BugCommand::Json);
        assert_eq!(parse_bug_command("open"), BugCommand::Usage);
        assert!(BUG_USAGE.contains("report|template|status|show|json|--json"));
        assert!(BUG_USAGE.contains("show --json|template --json|report --json"));
        let payload = bug_json_payload(
            "libertai",
            "test-model",
            Mode::Plan,
            Some("review"),
            "template --json",
        );
        assert_eq!(payload["command"], "bug");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "template --json");
        assert_eq!(payload["app"], "libertai-cli");
        assert_eq!(payload["mode"], "plan");
        assert_eq!(payload["output_style"], "review");
        assert_eq!(payload["aliases"][0], "bug");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("template --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("report --json")));
    }

    #[test]
    fn copy_hotkeys_reload_command_args_capture_aliases() {
        assert_eq!(copy_command_arg("/copy"), Some(""));
        assert_eq!(copy_command_arg("/copy last"), Some("last"));
        assert_eq!(copy_command_arg("/copy latest"), Some("latest"));
        assert_eq!(copy_command_arg("/copycat"), None);
        assert_eq!(parse_copy_command(""), CopyCommand::LastAssistant);
        assert_eq!(parse_copy_command("last"), CopyCommand::LastAssistant);
        assert_eq!(parse_copy_command("latest"), CopyCommand::LastAssistant);
        assert_eq!(parse_copy_command("response"), CopyCommand::LastAssistant);
        assert_eq!(parse_copy_command("assistant"), CopyCommand::LastAssistant);
        assert_eq!(
            parse_copy_command("assistant-response"),
            CopyCommand::LastAssistant
        );
        assert_eq!(parse_copy_command("status"), CopyCommand::Status);
        assert_eq!(parse_copy_command("show"), CopyCommand::Status);
        assert_eq!(parse_copy_command("info"), CopyCommand::Status);
        assert_eq!(parse_copy_command("json"), CopyCommand::Json);
        assert_eq!(parse_copy_command("--json"), CopyCommand::Json);
        assert_eq!(parse_copy_command("status --json"), CopyCommand::Json);
        assert_eq!(parse_copy_command("show --json"), CopyCommand::Json);
        assert_eq!(parse_copy_command("info --json"), CopyCommand::Json);
        assert_eq!(parse_copy_command("transcript"), CopyCommand::Usage);
        assert!(copy_usage_text().contains("json|--json|status --json"));
        assert!(copy_usage_text().contains("show --json|info --json"));
        let empty_payload = copy_json_payload(&[], "info --json");
        assert_eq!(empty_payload["command"], "copy");
        assert_eq!(empty_payload["surface"], "terminal");
        assert_eq!(empty_payload["query"], "info --json");
        assert_eq!(empty_payload["aliases"][0], "copy");
        assert_eq!(empty_payload["available"], false);
        assert_eq!(empty_payload["copy_mechanism"], "osc52");
        assert_eq!(empty_payload["supported_actions"][5], "status --json");
        assert_eq!(empty_payload["supported_actions"][7], "info --json");
        assert_eq!(
            empty_payload["supported_actions"][12],
            "assistant-response"
        );

        assert_eq!(hotkeys_command_arg("/hotkeys"), Some(""));
        assert_eq!(hotkeys_command_arg("/hotkeys status"), Some("status"));
        assert_eq!(hotkeys_command_arg("/hotkeys show"), Some("show"));
        assert_eq!(hotkeys_command_arg("/hotkey"), None);
        assert_eq!(parse_hotkeys_command(""), HotkeysCommand::Show);
        assert_eq!(parse_hotkeys_command("list"), HotkeysCommand::Show);
        assert_eq!(parse_hotkeys_command("help"), HotkeysCommand::Show);
        assert_eq!(parse_hotkeys_command("json"), HotkeysCommand::Json);
        assert_eq!(parse_hotkeys_command("--json"), HotkeysCommand::Json);
        assert_eq!(parse_hotkeys_command("status --json"), HotkeysCommand::Json);
        assert_eq!(parse_hotkeys_command("show --json"), HotkeysCommand::Json);
        assert_eq!(parse_hotkeys_command("list --json"), HotkeysCommand::Json);
        assert_eq!(parse_hotkeys_command("edit"), HotkeysCommand::Usage);
        assert!(hotkeys_usage_text().contains("json|--json|status --json"));
        assert!(hotkeys_usage_text().contains("show --json|list --json"));
        let payload = hotkeys_json_payload("list --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "hotkeys");
        assert_eq!(payload["query"], "list --json");
        assert_eq!(payload["aliases"][0], "hotkeys");
        assert_eq!(payload["supported_actions"][6], "status --json");
        assert_eq!(payload["supported_actions"][8], "list --json");
        assert!(
            payload["shortcuts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["key"] == "Shift+Tab")
        );

        assert_eq!(reload_command_arg("/reload"), Some(""));
        assert_eq!(reload_command_arg("/reload config"), Some("config"));
        assert_eq!(reload_command_arg("/reload session"), Some("session"));
        assert_eq!(reload_command_arg("/reloader"), None);
        assert_eq!(parse_reload_command(""), ReloadCommand::Session);
        assert_eq!(parse_reload_command("config"), ReloadCommand::Session);
        assert_eq!(parse_reload_command("now"), ReloadCommand::Session);
        assert_eq!(parse_reload_command("fresh"), ReloadCommand::Session);
        assert_eq!(parse_reload_command("json"), ReloadCommand::Json);
        assert_eq!(parse_reload_command("--json"), ReloadCommand::Json);
        assert_eq!(parse_reload_command("config --json"), ReloadCommand::Json);
        assert_eq!(parse_reload_command("session --json"), ReloadCommand::Json);
        assert_eq!(parse_reload_command("now --json"), ReloadCommand::Json);
        assert_eq!(parse_reload_command("fresh --json"), ReloadCommand::Json);
        assert_eq!(parse_reload_command("auth"), ReloadCommand::Usage);
        let payload = reload_preview_json_payload(
            "fresh --json",
            "libertai",
            "qwen",
            Mode::Plan,
            Some("review"),
            &LibertaiConfig::default(),
        );
        assert_eq!(payload["command"], "reload");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "fresh --json");
        assert_eq!(payload["action"], "fresh");
        assert_eq!(payload["action_aliases"][0], "config");
        assert_eq!(payload["supported_actions"][5], "--json");
        assert_eq!(payload["supported_actions"][9], "fresh --json");
    }

    #[test]
    fn status_command_arg_and_parser_capture_session_aliases() {
        assert_eq!(status_command_arg("/status"), Some(""));
        assert_eq!(status_command_arg("/status show"), Some("show"));
        assert_eq!(status_command_arg("/status current"), Some("current"));
        assert_eq!(status_command_arg("/status session"), Some("session"));
        assert_eq!(status_command_arg("/statusline"), None);
        assert_eq!(parse_status_command(""), StatusCommand::Session);
        assert_eq!(parse_status_command("status"), StatusCommand::Session);
        assert_eq!(parse_status_command("state"), StatusCommand::Session);
        assert_eq!(parse_status_command("show"), StatusCommand::Session);
        assert_eq!(parse_status_command("info"), StatusCommand::Session);
        assert_eq!(parse_status_command("current"), StatusCommand::Session);
        assert_eq!(parse_status_command("session"), StatusCommand::Session);
        assert_eq!(parse_status_command("json"), StatusCommand::Json);
        assert_eq!(parse_status_command("--json"), StatusCommand::Json);
        assert_eq!(parse_status_command("status --json"), StatusCommand::Json);
        assert_eq!(parse_status_command("state --json"), StatusCommand::Json);
        assert_eq!(parse_status_command("show --json"), StatusCommand::Json);
        assert_eq!(parse_status_command("info --json"), StatusCommand::Json);
        assert_eq!(parse_status_command("current --json"), StatusCommand::Json);
        assert_eq!(parse_status_command("session --json"), StatusCommand::Json);
        assert_eq!(parse_status_command("open"), StatusCommand::Usage);
        assert!(help_command_arg_hint("status").contains("status|state|show|info"));
        assert!(help_command_arg_hint("status").contains("status --json|state --json"));
        assert!(status_usage_text().contains("status|state|show|info"));
        assert!(status_usage_text().contains("current|session|json|--json"));
        assert!(status_usage_text().contains("status --json"));
        assert!(status_usage_text().contains("state --json|show --json|info --json"));
        assert!(status_usage_text().contains("current --json|session --json"));
    }

    #[test]
    fn status_json_payload_includes_command_metadata() {
        let payload = session_status_json_payload(
            "session --json",
            "libertai",
            "qwen3-coder-480b",
            Mode::Normal,
            Some("review"),
            &LibertaiConfig::default(),
            Some(UsageSummary {
                turns: 2,
                last_input: 100,
                last_output: 25,
                output_total: 40,
                context_high_water: 120,
                context_window: 1000,
                provider: "libertai".to_string(),
                model: "qwen3-coder-480b".to_string(),
            }),
        );
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "status");
        assert_eq!(payload["query"], "session --json");
        assert_eq!(payload["aliases"][0], "status");
        assert_eq!(payload["supported_actions"][7], "--json");
        assert_eq!(payload["supported_actions"][13], "session --json");
        assert_eq!(payload["usage"]["turns"], 2);
        assert_eq!(payload["output_style"], "review");
    }

    #[test]
    fn doctor_command_arg_and_parser_capture_diagnostic_aliases() {
        assert_eq!(doctor_command_arg("/doctor"), Some(""));
        assert_eq!(doctor_command_arg("/doctor status"), Some("status"));
        assert_eq!(doctor_command_arg("/doctor diagnostics"), Some("diagnostics"));
        assert_eq!(doctor_command_arg("/doctors"), None);
        assert_eq!(parse_doctor_command(""), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("status"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("state"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("show"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("info"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("health"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("diagnostics"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("diag"), DoctorCommand::Run);
        assert_eq!(parse_doctor_command("json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("status --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("state --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("show --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("info --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("health --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("diagnostics --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("diag --json"), DoctorCommand::Json);
        assert_eq!(parse_doctor_command("open"), DoctorCommand::Usage);
        assert!(doctor_usage_text().contains("status|state|show|info"));
        assert!(doctor_usage_text().contains("health|diagnostics|diag|json"));
        assert!(doctor_usage_text().contains("json|--json|status --json"));
        assert!(doctor_usage_text().contains("status --json"));
        assert!(doctor_usage_text().contains("state --json|show --json|info --json"));
        assert!(doctor_usage_text().contains("health --json"));
        assert!(doctor_usage_text().contains("diagnostics --json"));
        assert!(doctor_usage_text().contains("diag --json"));
    }

    #[test]
    fn abort_command_arg_and_parser_capture_status_aliases() {
        assert_eq!(abort_command_arg("/abort"), Some(""));
        assert_eq!(abort_command_arg("/abort status"), Some("status"));
        assert_eq!(abort_command_arg("/abort cancel"), Some("cancel"));
        assert_eq!(abort_command_arg("/aborted"), None);
        assert_eq!(parse_abort_command(""), AbortCommand::Status);
        assert_eq!(parse_abort_command("status"), AbortCommand::Status);
        assert_eq!(parse_abort_command("cancel"), AbortCommand::Status);
        assert_eq!(parse_abort_command("stop"), AbortCommand::Status);
        assert_eq!(parse_abort_command("interrupt"), AbortCommand::Status);
        assert_eq!(parse_abort_command("json"), AbortCommand::Json);
        assert_eq!(parse_abort_command("--json"), AbortCommand::Json);
        assert_eq!(parse_abort_command("status --json"), AbortCommand::Json);
        assert_eq!(parse_abort_command("state --json"), AbortCommand::Json);
        assert_eq!(parse_abort_command("show --json"), AbortCommand::Json);
        assert_eq!(parse_abort_command("info --json"), AbortCommand::Json);
        assert_eq!(parse_abort_command("open"), AbortCommand::Usage);
        assert!(abort_usage_text().contains("json|--json|status --json|state --json"));
        assert!(abort_usage_text().contains("show --json|info --json"));
        let payload = abort_json_payload("status --json");
        assert_eq!(payload["command"], "abort");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["active_turn"], false);
        assert_eq!(payload["interrupt_mechanism"], "ctrl-c");
        assert_eq!(payload["aliases"][0], "abort");
        assert_eq!(payload["supported_actions"][6], "status --json");
        assert_eq!(payload["supported_actions"][9], "info --json");
        assert_eq!(payload["supported_actions"][12], "interrupt");
    }

    #[test]
    fn help_command_arg_and_parser_capture_json_aliases() {
        assert_eq!(help_command_arg("/help"), Some(""));
        assert_eq!(help_command_arg("/help status"), Some("status"));
        assert_eq!(help_command_arg("/help json"), Some("json"));
        assert_eq!(help_command_arg("/help status --json"), Some("status --json"));
        assert_eq!(help_command_arg("/helper"), None);
        assert_eq!(parse_help_command(""), HelpCommand::Show);
        assert_eq!(parse_help_command("list"), HelpCommand::Show);
        assert_eq!(parse_help_command("json"), HelpCommand::Json);
        assert_eq!(parse_help_command("--json"), HelpCommand::Json);
        assert_eq!(parse_help_command("status --json"), HelpCommand::Json);
        assert_eq!(parse_help_command("show --json"), HelpCommand::Json);
        assert_eq!(parse_help_command("list --json"), HelpCommand::Json);
        assert_eq!(parse_help_command("commands --json"), HelpCommand::Json);
        assert!(help_usage_text().contains("list|commands|json|--json"));
        assert!(help_usage_text().contains("show --json|list --json|commands --json"));
        let payload = help_json_payload("commands --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "help");
        assert_eq!(payload["aliases"][0], "help");
        assert_eq!(payload["query"], "commands --json");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("commands --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("list --json")));
        assert!(
            payload["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["name"] == "model"
                    && row["description"] == "show or change the active model"
                    && row["arg_hint"]
                        .as_str()
                        .unwrap()
                        .contains("list --json"))
        );
        assert!(
            payload["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["name"] == "remember"
                    && row["arg_hint"]
                        .as_str()
                        .unwrap()
                        .contains("preview --json"))
        );
    }

    #[test]
    fn clear_command_arg_and_parser_capture_preview_aliases() {
        assert_eq!(clear_command_arg("/clear"), None);
        assert_eq!(clear_command_arg("/new"), None);
        assert_eq!(clear_command_arg("/clear status"), Some(("/clear", "status")));
        assert_eq!(clear_command_arg("/new json"), Some(("/new", "json")));
        assert_eq!(
            clear_command_arg("/clear status --json"),
            Some(("/clear", "status --json"))
        );
        assert_eq!(clear_command_arg("/clearer status"), None);
        assert_eq!(parse_clear_command("status"), ClearCommand::Status);
        assert_eq!(parse_clear_command("preview"), ClearCommand::Status);
        assert_eq!(parse_clear_command("json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("--json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("status --json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("state --json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("show --json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("info --json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("preview --json"), ClearCommand::Json);
        assert_eq!(parse_clear_command("run"), ClearCommand::Usage);
        assert!(clear_usage_text("/clear").contains("json|--json|status --json"));
        assert!(clear_usage_text("/clear").contains("show --json|info --json|preview --json"));
        let payload = clear_json_payload("/new", "libertai", "qwen", Mode::Plan, "preview --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "new");
        assert_eq!(payload["query"], "preview --json");
        assert_eq!(payload["available"], true);
        assert_eq!(payload["active_turn"], false);
        assert_eq!(payload["current_mode"], "plan");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("preview --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("info --json")));
    }

    #[test]
    fn forget_command_arg_and_parser_capture_preview_aliases() {
        assert_eq!(forget_command_arg("/forget"), None);
        assert_eq!(forget_command_arg("/forget status"), Some("status"));
        assert_eq!(forget_command_arg("/forget json"), Some("json"));
        assert_eq!(
            forget_command_arg("/forget status --json"),
            Some("status --json")
        );
        assert_eq!(forget_command_arg("/forgettable"), None);
        assert_eq!(parse_forget_command("status"), ForgetCommand::Status);
        assert_eq!(parse_forget_command("preview"), ForgetCommand::Status);
        assert_eq!(parse_forget_command("json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("--json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("status --json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("state --json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("show --json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("info --json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("preview --json"), ForgetCommand::Json);
        assert_eq!(parse_forget_command("run"), ForgetCommand::Usage);
        assert!(forget_usage_text().contains("preview|json|--json|status --json"));
        assert!(forget_usage_text().contains("show --json|info --json|preview --json"));
        let approvals = ApprovalState::new();
        let payload = forget_json_payload(&approvals, "status --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "forget");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["available"], true);
        assert_eq!(payload["remembered_approvals"], 0);
        assert_eq!(payload["will_clear_saved_allow_rules"], true);
        assert_eq!(payload["will_change_permission_mode"], false);
        assert_eq!(payload["aliases"][0], "forget");
        assert_eq!(payload["supported_actions"][7], "status --json");
        assert_eq!(payload["supported_actions"][11], "preview --json");
    }

    #[test]
    fn exit_command_arg_and_parser_capture_preview_aliases() {
        assert_eq!(exit_command_arg("/exit"), None);
        assert_eq!(exit_command_arg("/quit"), None);
        assert_eq!(exit_command_arg("/exit status"), Some(("/exit", "status")));
        assert_eq!(exit_command_arg("/quit json"), Some(("/quit", "json")));
        assert_eq!(
            exit_command_arg("/exit status --json"),
            Some(("/exit", "status --json"))
        );
        assert_eq!(exit_command_arg("/exitcode status"), None);
        assert_eq!(parse_exit_command("status"), ExitCommand::Status);
        assert_eq!(parse_exit_command("preview"), ExitCommand::Status);
        assert_eq!(parse_exit_command("json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("--json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("status --json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("state --json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("show --json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("info --json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("preview --json"), ExitCommand::Json);
        assert_eq!(parse_exit_command("run"), ExitCommand::Usage);
        assert!(exit_usage_text("/exit").contains("json|--json|status --json"));
        assert!(exit_usage_text("/exit").contains("show --json|info --json|preview --json"));
        let payload = exit_json_payload("/quit", "show --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "quit");
        assert_eq!(payload["query"], "show --json");
        assert_eq!(payload["available"], true);
        assert_eq!(payload["active_turn"], false);
        assert_eq!(payload["will_exit_repl"], true);
        assert_eq!(payload["will_close_session_tab"], false);
        assert_eq!(payload["interrupt_alternative"], "Ctrl+D");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("preview --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("info --json")));
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
    fn mcp_exposure_summary_reports_native_cli_tools() {
        let cfg = LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([
                (
                    "docs".to_string(),
                    crate::config::McpServerConfig {
                        tools: vec![
                            crate::config::McpToolConfig {
                                name: "search".to_string(),
                                enabled: true,
                                ..crate::config::McpToolConfig::default()
                            },
                            crate::config::McpToolConfig {
                                name: "disabled".to_string(),
                                enabled: false,
                                ..crate::config::McpToolConfig::default()
                            },
                        ],
                        resources: vec![crate::config::McpResourceConfig {
                            uri: "file:///repo/README.md".to_string(),
                            enabled: true,
                            ..crate::config::McpResourceConfig::default()
                        }],
                        prompts: vec![crate::config::McpPromptConfig {
                            name: "summarize".to_string(),
                            enabled: true,
                            ..crate::config::McpPromptConfig::default()
                        }],
                        ..crate::config::McpServerConfig::default()
                    },
                ),
                (
                    "empty".to_string(),
                    crate::config::McpServerConfig {
                        tools: vec![crate::config::McpToolConfig {
                            name: "   ".to_string(),
                            enabled: true,
                            ..crate::config::McpToolConfig::default()
                        }],
                        resources: vec![crate::config::McpResourceConfig {
                            uri: String::new(),
                            enabled: true,
                            ..crate::config::McpResourceConfig::default()
                        }],
                        ..crate::config::McpServerConfig::default()
                    },
                ),
            ]),
            ..LibertaiConfig::default()
        };
        assert_eq!(
            mcp_exposure_summary(&cfg),
            McpExposureSummary {
                mcp_call: true,
                named_tools: 1,
                resource_reader: true,
                prompt_getter: true,
                subscription_candidates: 1,
            }
        );
        assert_eq!(
            mcp_exposure_summary(&LibertaiConfig::default()),
            McpExposureSummary {
                mcp_call: false,
                named_tools: 0,
                resource_reader: false,
                prompt_getter: false,
                subscription_candidates: 0,
            }
        );
    }

    #[test]
    fn doctor_mcp_summary_reports_cli_exposure() {
        let cfg = LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "docs".to_string(),
                crate::config::McpServerConfig {
                    tools: vec![crate::config::McpToolConfig {
                        name: "search".to_string(),
                        enabled: true,
                        ..crate::config::McpToolConfig::default()
                    }],
                    resources: vec![crate::config::McpResourceConfig {
                        uri: "file:///repo/README.md".to_string(),
                        enabled: true,
                        ..crate::config::McpResourceConfig::default()
                    }],
                    prompts: vec![crate::config::McpPromptConfig {
                        name: "summarize".to_string(),
                        enabled: true,
                        ..crate::config::McpPromptConfig::default()
                    }],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..LibertaiConfig::default()
        };
        assert_eq!(
            format_mcp_doctor_summary(&cfg),
            "1 configured; mcp_call on, 1 named tool(s), resource reader on, prompt getter on, 1 subscription candidate(s); stdio/http/sse reuse on"
        );
        assert_eq!(
            format_mcp_doctor_summary(&LibertaiConfig::default()),
            "0 configured; mcp_call off, 0 named tool(s), resource reader off, prompt getter off, 0 subscription candidate(s); stdio/http/sse reuse on"
        );
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
    fn onboarding_json_arg_accepts_preview_aliases() {
        assert!(is_onboarding_json_arg("json"));
        assert!(is_onboarding_json_arg("--json"));
        assert!(is_onboarding_json_arg("status --json"));
        assert!(is_onboarding_json_arg("show --json"));
        assert!(is_onboarding_json_arg("preview --json"));
        assert!(!is_onboarding_json_arg("save docs/onboarding.md"));
    }

    #[test]
    fn onboarding_preview_arg_accepts_read_only_aliases() {
        assert!(is_onboarding_preview_arg("show"));
        assert!(is_onboarding_preview_arg("status"));
        assert!(is_onboarding_preview_arg("preview"));
        assert!(!is_onboarding_preview_arg("save docs/onboarding.md"));
    }

    #[test]
    fn send_command_arg_accepts_desktop_alias() {
        assert_eq!(send_command_arg("/send"), Some(""));
        assert_eq!(send_command_arg("/send status"), Some("status"));
        assert_eq!(send_command_arg("/send targets"), Some("targets"));
        assert_eq!(send_command_arg("/send list"), Some("list"));
        assert_eq!(send_command_arg("/send json"), Some("json"));
        assert_eq!(send_command_arg("/send show --json"), Some("show --json"));
        assert_eq!(send_command_arg("/send queued --json"), Some("queued --json"));
        assert_eq!(send_command_arg("/send pending --json"), Some("pending --json"));
        assert_eq!(send_command_arg("/send worker finish tests"), Some("worker finish tests"));
        assert_eq!(send_command_arg("/send-message"), Some(""));
        assert_eq!(
            send_command_arg("/send-message worker finish tests"),
            Some("worker finish tests")
        );
        assert_eq!(send_command_arg("/sender worker finish tests"), None);
        assert!(is_send_json_request("json"));
        assert!(is_send_json_request("status --json"));
        assert!(is_send_json_request("state --json"));
        assert!(is_send_json_request("show --json"));
        assert!(is_send_json_request("list --json"));
        assert!(is_send_json_request("targets --json"));
        assert!(is_send_json_request("queue --json"));
        assert!(is_send_json_request("queued --json"));
        assert!(is_send_json_request("pending --json"));
        assert!(!is_send_json_request("worker finish tests"));

        let payload = send_json_payload("  queued --json  ");
        assert_eq!(payload["command"], "send");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "queued --json");
        assert_eq!(payload["aliases"], json!(["send", "send-message"]));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "pending --json"));
        assert!(payload["desktop_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "/send-message pending --json"));
        assert!(help_command_arg_hint("send").contains("show --json"));
        assert!(help_command_arg_hint("send").contains("pending --json"));
    }

    #[test]
    fn theme_command_arg_intercepts_desktop_theme_command() {
        assert_eq!(theme_command_arg("/theme"), Some(""));
        assert_eq!(theme_command_arg("/theme dark"), Some("dark"));
        assert_eq!(theme_command_arg("/theme status"), Some("status"));
        assert_eq!(theme_command_arg("/theme show"), Some("show"));
        assert_eq!(theme_command_arg("/theme current"), Some("current"));
        assert_eq!(
            theme_command_arg("/theme high-contrast"),
            Some("high-contrast")
        );
        assert_eq!(theme_command_arg("/themes dark"), None);
        assert_eq!(parse_theme_command(""), ThemeCommand::Status);
        assert_eq!(parse_theme_command("status"), ThemeCommand::Status);
        assert_eq!(parse_theme_command("show"), ThemeCommand::Status);
        assert_eq!(parse_theme_command("current"), ThemeCommand::Status);
        assert_eq!(parse_theme_command("info"), ThemeCommand::Status);
        assert_eq!(parse_theme_command("json"), ThemeCommand::Json);
        assert_eq!(parse_theme_command("--json"), ThemeCommand::Json);
        assert_eq!(parse_theme_command("status --json"), ThemeCommand::Json);
        assert_eq!(parse_theme_command("current --json"), ThemeCommand::Json);
        assert_eq!(
            parse_theme_command("high-contrast"),
            ThemeCommand::Requested("high-contrast".to_string())
        );
        let payload = theme_json_payload("current --json");
        assert_eq!(payload["command"], "theme");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "current --json");
        assert_eq!(payload["aliases"][0], "theme");
        assert_eq!(payload["terminal_mutates_theme"], false);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "status --json"));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "current --json"));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "high-contrast"));
    }

    #[test]
    fn parse_schedule_command_matches_desktop_contract() {
        assert_eq!(parse_schedule_command(""), ScheduleCommand::Status);
        assert_eq!(parse_schedule_command("list"), ScheduleCommand::Status);
        assert_eq!(parse_schedule_command("json"), ScheduleCommand::Json);
        assert_eq!(parse_schedule_command("--json"), ScheduleCommand::Json);
        assert_eq!(parse_schedule_command("list --json"), ScheduleCommand::Json);
        assert_eq!(
            parse_schedule_command("show sch_2 --json"),
            ScheduleCommand::ShowJson("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("show-json sch_2"),
            ScheduleCommand::ShowJson("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("inspect-json sch_2"),
            ScheduleCommand::ShowJson("sch_2".to_string())
        );
        assert_eq!(parse_schedule_command("status"), ScheduleCommand::Status);
        assert_eq!(parse_schedule_command("state"), ScheduleCommand::Status);
        assert_eq!(
            parse_schedule_command("show sch_2"),
            ScheduleCommand::Show("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("inspect sch_2"),
            ScheduleCommand::Show("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("run sch_2"),
            ScheduleCommand::Run("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("now sch_2"),
            ScheduleCommand::Run("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("trigger sch_2"),
            ScheduleCommand::Run("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("cancel sch_2"),
            ScheduleCommand::Cancel("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("delete sch_2"),
            ScheduleCommand::Cancel("sch_2".to_string())
        );
        assert_eq!(
            parse_schedule_command("rm sch_2"),
            ScheduleCommand::Cancel("sch_2".to_string())
        );
        assert_eq!(parse_schedule_command("clear"), ScheduleCommand::Clear);
        assert_eq!(parse_schedule_command("stop"), ScheduleCommand::Clear);
        assert!(matches!(
            parse_schedule_command("cancel sch_2 extra"),
            ScheduleCommand::Usage
        ));
        assert!(matches!(
            parse_schedule_command("show sch_2 extra"),
            ScheduleCommand::Usage
        ));
        assert!(matches!(
            parse_schedule_command("show sch_2 --json extra"),
            ScheduleCommand::Usage
        ));
        assert!(matches!(
            parse_schedule_command("run sch_2 extra"),
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
        let hint = help_command_arg_hint("schedule");
        assert!(hint.contains("inspect"));
        assert!(hint.contains("show-json"));
        assert!(hint.contains("trigger"));
        assert!(hint.contains("delete"));
        assert!(hint.contains("rm"));
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
    fn schedule_status_counts_split_due_and_pending() {
        let now = Instant::now();
        let runs = vec![
            scheduled_run_for_test("sch_1", "due", now - Duration::from_millis(1)),
            scheduled_run_for_test("sch_2", "also due", now),
            scheduled_run_for_test("sch_3", "later", now + Duration::from_secs(5)),
        ];
        assert_eq!(
            schedule_status_counts(&runs, now),
            ScheduleStatusCounts {
                total: 3,
                due: 2,
                pending: 1,
            }
        );
    }

    #[test]
    fn schedule_json_payload_reports_counts_and_rows() {
        let now = Instant::now();
        let runs = vec![
            scheduled_run_for_test("sch_1", "due", now - Duration::from_millis(1)),
            scheduled_run_for_test("sch_2", "later", now + Duration::from_secs(5)),
        ];
        let payload = schedule_json_payload(&runs, now, "  list --json  ");
        assert_eq!(payload.surface, "terminal");
        assert_eq!(payload.command, "schedule");
        assert_eq!(payload.query, "list --json");
        assert_eq!(payload.aliases, &["schedule", "cron"]);
        assert!(payload.supported_actions.contains(&"trigger <id>"));
        assert!(payload.supported_actions.contains(&"delete <id>"));
        assert!(payload.supported_actions.contains(&"rm <id>"));
        assert_eq!(payload.total, 2);
        assert_eq!(payload.due, 1);
        assert_eq!(payload.pending, 1);
        assert_eq!(payload.runs[0].id, "sch_1");
        assert_eq!(payload.runs[0].state, "due");
        assert_eq!(payload.runs[1].id, "sch_2");
        assert_eq!(payload.runs[1].state, "pending");
        assert!(payload.runs[1].due_in_ms >= 4_000);
    }

    #[test]
    fn doctor_schedule_summary_reports_queued_due_and_pending() {
        let now = Instant::now();
        let runs = vec![
            scheduled_run_for_test("sch_1", "due", now - Duration::from_millis(1)),
            scheduled_run_for_test("sch_2", "later", now + Duration::from_secs(5)),
        ];
        let summary = format_schedule_doctor_summary(&runs);
        assert!(summary.contains("2 queued"));
        assert!(summary.contains("1 due"));
        assert!(summary.contains("1 pending"));
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
        assert_eq!(parse_auto_command("state"), AutoCommand::Status);
        assert_eq!(parse_auto_command("json"), AutoCommand::Json);
        assert_eq!(parse_auto_command("--json"), AutoCommand::Json);
        assert_eq!(parse_auto_command("status --json"), AutoCommand::Json);
        assert_eq!(parse_auto_command("state json"), AutoCommand::Json);
        assert_eq!(parse_auto_command("status-json"), AutoCommand::Json);
        assert_eq!(parse_auto_command("state-json"), AutoCommand::Json);
        assert_eq!(parse_auto_command("off"), AutoCommand::Off);
        assert_eq!(parse_auto_command("stop"), AutoCommand::Off);
        assert_eq!(parse_auto_command("cancel"), AutoCommand::Off);
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
        let hint = help_command_arg_hint("auto");
        assert!(hint.contains("status-json"));
        assert!(hint.contains("state-json"));
    }

    #[test]
    fn auto_json_payload_reports_active_and_inactive_state() {
        assert_eq!(
            auto_json_payload(None, "  status --json  "),
            AutoJsonPayload {
                surface: "terminal",
                command: "auto",
                query: "status --json".to_string(),
                aliases: &["auto", "autorun", "continuous"],
                supported_actions: auto_supported_actions(),
                active: false,
                limit: 0,
                completed: 0,
                remaining: 0,
                goal: None,
            }
        );

        let run = AutoRun {
            limit: 5,
            completed: 2,
            goal: "finish parity".to_string(),
        };
        assert_eq!(
            auto_json_payload(Some(&run), "  json  "),
            AutoJsonPayload {
                surface: "terminal",
                command: "auto",
                query: "json".to_string(),
                aliases: &["auto", "autorun", "continuous"],
                supported_actions: auto_supported_actions(),
                active: true,
                limit: 5,
                completed: 2,
                remaining: 3,
                goal: Some("finish parity".to_string()),
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
        assert_eq!(
            parse_permissions_command("readonly"),
            PermissionsCommand::Set(Mode::Plan)
        );
        assert_eq!(
            parse_permissions_command("read-only"),
            PermissionsCommand::Set(Mode::Plan)
        );
    }

    #[test]
    fn parse_permissions_command_handles_management_actions() {
        assert_eq!(parse_permissions_command(""), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("status"), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("show"), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("current"), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("info"), PermissionsCommand::Show);
        assert_eq!(parse_permissions_command("json"), PermissionsCommand::Json);
        assert_eq!(parse_permissions_command("--json"), PermissionsCommand::Json);
        assert_eq!(
            parse_permissions_command("status --json"),
            PermissionsCommand::Json
        );
        assert_eq!(
            parse_permissions_command("show --json"),
            PermissionsCommand::Json
        );
        assert_eq!(
            parse_permissions_command("current --json"),
            PermissionsCommand::Json
        );
        assert_eq!(
            parse_permissions_command("info --json"),
            PermissionsCommand::Json
        );
        assert_eq!(parse_permissions_command("open"), PermissionsCommand::Open);
        assert_eq!(parse_permissions_command("settings"), PermissionsCommand::Open);
        assert_eq!(parse_permissions_command("edit"), PermissionsCommand::Open);
        assert_eq!(
            parse_permissions_command("approvals"),
            PermissionsCommand::Open
        );
        assert_eq!(parse_permissions_command("forget"), PermissionsCommand::Forget);
        assert_eq!(parse_permissions_command("clear"), PermissionsCommand::Forget);
        assert_eq!(parse_permissions_command("reset"), PermissionsCommand::Forget);
        assert_eq!(
            parse_permissions_command("bypassPermissions"),
            PermissionsCommand::UnsupportedBypass
        );
        assert_eq!(
            parse_permissions_command("danger"),
            PermissionsCommand::UnsupportedBypass
        );
        assert_eq!(parse_permissions_command("wat"), PermissionsCommand::Show);
        assert!(permissions_usage_text().contains("default|normal"));
        assert!(permissions_usage_text().contains("info|json|--json|status --json"));
        assert!(permissions_usage_text().contains("show --json|current --json|info --json"));
        assert!(permissions_usage_text().contains("accept-edits|accept_edits"));
        assert!(permissions_usage_text().contains("readonly|read-only"));
        assert!(permissions_usage_text().contains("settings|edit|approvals"));
        assert!(permissions_usage_text().contains("forget|clear|reset"));
        assert!(permissions_usage_text().contains("bypassPermissions|bypass|danger"));
        assert!(mode_usage_text().contains("info|json|--json|status --json"));
        assert!(mode_usage_text().contains("show --json|current --json|info --json"));
        assert!(mode_usage_text().contains("normal|acceptEdits"));
        assert!(mode_usage_text().contains("readonly|read-only"));
        let approvals = ApprovalState::new();
        let payload = permissions_json_payload(Mode::Plan, &approvals, "status --json");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "permissions");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["mode"], "plan");
        assert_eq!(payload["remembered_approvals"], 0);
        assert_eq!(payload["native_bypass_permissions"], false);
        assert_eq!(payload["supported_actions"][5], "--json");
        let mode_payload = mode_json_payload(Mode::AcceptEdits, "current --json");
        assert_eq!(mode_payload["surface"], "terminal");
        assert_eq!(mode_payload["command"], "mode");
        assert_eq!(mode_payload["query"], "current --json");
        assert_eq!(mode_payload["mode"], "accept-edits");
        assert_eq!(mode_payload["supported_actions"][5], "--json");
    }

    #[test]
    fn parse_login_slash_target_maps_status_account_and_providers() {
        assert_eq!(parse_login_slash_target(""), LoginSlashTarget::Account);
        assert_eq!(parse_login_slash_target("status"), LoginSlashTarget::Status);
        assert_eq!(parse_login_slash_target("show"), LoginSlashTarget::Status);
        assert_eq!(parse_login_slash_target("info"), LoginSlashTarget::Status);
        assert_eq!(parse_login_slash_target("json"), LoginSlashTarget::StatusJson);
        assert_eq!(parse_login_slash_target("--json"), LoginSlashTarget::StatusJson);
        assert_eq!(
            parse_login_slash_target("status --json"),
            LoginSlashTarget::StatusJson
        );
        assert_eq!(
            parse_login_slash_target("show json"),
            LoginSlashTarget::StatusJson
        );
        assert_eq!(parse_login_slash_target("libertai"), LoginSlashTarget::Account);
        assert_eq!(parse_login_slash_target("account"), LoginSlashTarget::Account);
        assert_eq!(parse_login_slash_target("key"), LoginSlashTarget::Account);
        assert_eq!(parse_login_slash_target("api-key"), LoginSlashTarget::Account);
        assert_eq!(parse_login_slash_target("api"), LoginSlashTarget::Account);
        assert_eq!(
            parse_login_slash_target("libertai --json"),
            LoginSlashTarget::ProviderStatusJson("libertai")
        );
        assert_eq!(
            parse_login_slash_target("anthropic"),
            LoginSlashTarget::Provider("anthropic")
        );
        assert_eq!(
            parse_login_slash_target("show anthropic"),
            LoginSlashTarget::ProviderStatus("anthropic")
        );
        assert_eq!(
            parse_login_slash_target("show anthropic --json"),
            LoginSlashTarget::ProviderStatusJson("anthropic")
        );
        assert_eq!(
            parse_login_slash_target("anthropic --json"),
            LoginSlashTarget::ProviderStatusJson("anthropic")
        );
        assert_eq!(
            parse_login_slash_target("inspect libertai"),
            LoginSlashTarget::ProviderStatus("libertai")
        );
        assert!(login_usage_text().contains("account|key|api-key|api"));
        assert!(login_usage_text().contains("json|--json|status --json"));
        assert!(login_usage_text().contains("show --json|info --json"));
        assert!(login_usage_text().contains("status --json"));
        assert!(login_usage_text().contains("show <provider> --json"));
        assert!(login_usage_text().contains("inspect <provider> --json"));
        assert!(login_usage_text().contains("provider <provider> --json"));
        assert!(login_usage_text().contains("show <provider>"));
        assert!(login_usage_text().contains("inspect <provider>"));
        assert!(login_usage_text().contains("provider <provider>"));
        assert!(logout_usage_text().contains("account|key|api-key|api"));
        assert!(logout_usage_text().contains("json|--json|status --json"));
        assert!(logout_usage_text().contains("show --json|info --json"));
        assert!(logout_usage_text().contains("status --json"));
        assert!(logout_usage_text().contains("show <provider> --json"));
        assert!(logout_usage_text().contains("inspect <provider> --json"));
        assert!(logout_usage_text().contains("provider <provider> --json"));
        assert!(logout_usage_text().contains("show <provider>"));
        assert!(logout_usage_text().contains("inspect <provider>"));
        assert!(logout_usage_text().contains("provider <provider>"));
        let cfg = LibertaiConfig::default();
        let payload = login_status_payload("login", "status --json", &cfg);
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "login");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["aliases"][0], "login");
        assert_eq!(payload["supported_actions"][4], "--json");
        assert_eq!(payload["supported_actions"][5], "status --json");
        assert_eq!(payload["supported_actions"][15], "show provider --json");
        let provider_payload = provider_login_payload(
            "logout",
            "show anthropic --json",
            "anthropic",
            &cfg,
        );
        assert_eq!(provider_payload["command"], "logout");
        assert_eq!(provider_payload["query"], "show anthropic --json");
        assert_eq!(provider_payload["provider"], "anthropic");
        assert_eq!(provider_payload["managed_by_desktop_settings"], true);
        assert_eq!(provider_payload["supported_actions"][22], "provider --json");
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
    fn parse_plan_command_accepts_on_off_and_status_aliases() {
        assert_eq!(parse_plan_command(""), PlanCommand::Status);
        assert_eq!(parse_plan_command("status"), PlanCommand::Status);
        assert_eq!(parse_plan_command("show"), PlanCommand::Status);
        assert_eq!(parse_plan_command("on"), PlanCommand::On);
        assert_eq!(parse_plan_command("enable"), PlanCommand::On);
        assert_eq!(parse_plan_command("plan"), PlanCommand::On);
        assert_eq!(parse_plan_command("readonly"), PlanCommand::On);
        assert_eq!(parse_plan_command("off"), PlanCommand::Off);
        assert_eq!(parse_plan_command("disable"), PlanCommand::Off);
        assert_eq!(parse_plan_command("normal"), PlanCommand::Off);
        assert_eq!(parse_plan_command("wat"), PlanCommand::Usage);
        assert_eq!(help_command_arg_hint("plan"), "on|off|status");
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
    fn parse_model_slash_command_accepts_status_and_cycle_aliases() {
        assert_eq!(
            model_usage_text(),
            "/model [status|show|current|json|--json|status --json|show --json|current --json|list|ls|list --json|ls --json|next|cycle|prev|previous|back|model|provider/model]"
        );
        assert!(matches!(parse_model_slash_command(""), ModelSlashCommand::Status));
        assert!(matches!(parse_model_slash_command("status"), ModelSlashCommand::Status));
        assert!(matches!(parse_model_slash_command("show"), ModelSlashCommand::Status));
        assert!(matches!(parse_model_slash_command("current"), ModelSlashCommand::Status));
        assert!(matches!(parse_model_slash_command("json"), ModelSlashCommand::Json));
        assert!(matches!(parse_model_slash_command("--json"), ModelSlashCommand::Json));
        assert!(matches!(parse_model_slash_command("status --json"), ModelSlashCommand::Json));
        assert!(matches!(parse_model_slash_command("show --json"), ModelSlashCommand::Json));
        assert!(matches!(parse_model_slash_command("current --json"), ModelSlashCommand::Json));
        assert!(matches!(parse_model_slash_command("list"), ModelSlashCommand::List));
        assert!(matches!(parse_model_slash_command("ls"), ModelSlashCommand::List));
        assert!(matches!(parse_model_slash_command("list --json"), ModelSlashCommand::JsonList));
        assert!(matches!(parse_model_slash_command("ls --json"), ModelSlashCommand::JsonList));
        assert!(matches!(parse_model_slash_command("next"), ModelSlashCommand::Next));
        assert!(matches!(parse_model_slash_command("cycle"), ModelSlashCommand::Next));
        assert!(matches!(parse_model_slash_command("prev"), ModelSlashCommand::Previous));
        assert!(matches!(parse_model_slash_command("previous"), ModelSlashCommand::Previous));
        assert!(matches!(parse_model_slash_command("back"), ModelSlashCommand::Previous));
        assert!(matches!(parse_model_slash_command("openai/gpt-5"), ModelSlashCommand::Set("openai/gpt-5")));

        let cfg = LibertaiConfig::default();
        let payload = model_json_payload(
            "libertai",
            "qwen3",
            &cfg,
            &["qwen*".to_string()],
            "list --json",
            Some(vec!["qwen3".to_string()]),
        );
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "model");
        assert_eq!(payload["query"], "list --json");
        assert_eq!(payload["current"]["id"], "libertai/qwen3");
        assert_eq!(payload["scope"]["is_scoped"], true);
        assert_eq!(payload["available_models"][0], "qwen3");
        assert_eq!(payload["supported_actions"][4], "--json");
        assert_eq!(payload["supported_actions"][5], "status --json");
        assert_eq!(payload["supported_actions"][10], "list --json");
    }

    #[test]
    fn scoped_models_parse_patterns_and_filter_matches() {
        assert_eq!(
            scoped_models_usage_text(),
            "/scoped-models <status|show|json|--json|status --json|show --json|patterns|clear|reset|off> — filter /model list and /model next|prev"
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
            parse_scoped_models_command("status"),
            ScopedModelsCommand::Status
        );
        assert_eq!(
            parse_scoped_models_command("show"),
            ScopedModelsCommand::Status
        );
        assert_eq!(
            parse_scoped_models_command("json"),
            ScopedModelsCommand::Json
        );
        assert_eq!(
            parse_scoped_models_command("--json"),
            ScopedModelsCommand::Json
        );
        assert_eq!(
            parse_scoped_models_command("status --json"),
            ScopedModelsCommand::Json
        );
        assert_eq!(
            parse_scoped_models_command("show --json"),
            ScopedModelsCommand::Json
        );
        assert_eq!(
            parse_scoped_models_command("reset"),
            ScopedModelsCommand::Clear
        );
        assert_eq!(parse_scoped_models_command("off"), ScopedModelsCommand::Clear);
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

        let payload = scoped_model_json_payload(
            &["qwen*".to_string(), "openai/gpt-*".to_string()],
            "show --json",
        );
        assert_eq!(payload["command"], "scoped-models");
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["query"], "show --json");
        assert_eq!(payload["is_scoped"], true);
        assert_eq!(payload["patterns"][0], "qwen*");
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("show --json"))
        );
    }

    #[test]
    fn model_slash_command_cycles_scoped_models() {
        assert_eq!(
            model_usage_text(),
            "/model [status|show|current|json|--json|status --json|show --json|current --json|list|ls|list --json|ls --json|next|cycle|prev|previous|back|model|provider/model]"
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
    fn export_json_arg_accepts_preview_aliases() {
        assert!(is_export_json_arg("json"));
        assert!(is_export_json_arg("--json"));
        assert!(is_export_json_arg("status --json"));
        assert!(is_export_json_arg("show --json"));
        assert!(is_export_json_arg("preview --json"));
        assert!(!is_export_json_arg("save report.md"));
    }

    #[test]
    fn export_json_payload_reports_non_writing_preview() {
        let messages = vec![Message::User(pi::model::UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: 1,
        })];
        let payload = export_json_payload("status --json", &messages);

        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "export");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["message_count"], 1);
        assert_eq!(payload["default_path"], "libertai-transcript.md");
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["will_copy"], false);
        assert!(payload["artifact"]["bytes"].as_u64().unwrap() > 0);
        assert_eq!(payload["aliases"][0], "export");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("preview --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show --json")));
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
    fn share_json_arg_accepts_preview_aliases() {
        assert!(is_share_json_arg("json"));
        assert!(is_share_json_arg("--json"));
        assert!(is_share_json_arg("status --json"));
        assert!(is_share_json_arg("show --json"));
        assert!(is_share_json_arg("preview --json"));
        assert!(!is_share_json_arg("gist public report.html"));
    }

    #[test]
    fn share_json_payload_reports_non_writing_preview() {
        let messages = vec![Message::User(pi::model::UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: 1,
        })];
        let payload = share_json_payload("status --json", &messages);

        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "share");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["message_count"], 1);
        assert_eq!(payload["default_path"], "libertai-share.html");
        assert_eq!(payload["default_gist_filename"], "libertai-share.html");
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["will_publish"], false);
        assert_eq!(payload["will_copy"], false);
        assert!(payload["artifact"]["bytes"].as_u64().unwrap() > 0);
        assert_eq!(payload["aliases"][0], "share");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("preview --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show --json")));
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
    fn onboarding_json_payload_reports_non_writing_preview() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "# Demo\n\nProject notes.").unwrap();

        let payload = onboarding_json_payload(temp.path(), "status --json").unwrap();

        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "onboarding");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["suggested_path"], "libertai-onboarding.md");
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["will_publish"], false);
        assert!(payload["guide"]["bytes"].as_u64().unwrap() > 0);
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("show --json"))
        );
        assert!(
            payload["supported_actions"]
                .as_array()
                .unwrap()
                .contains(&json!("preview --json"))
        );
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
        let usage = parse_agent_slash_query("reviewer   ")
            .unwrap_err()
            .to_string();
        assert!(usage.contains("--detached"));
        assert!(usage.contains("--same-cwd"));
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
            parse_agents_command("background json"),
            AgentsSlashCommand::BackgroundListJson
        );
        assert_eq!(
            parse_agents_command("bg list --json"),
            AgentsSlashCommand::BackgroundListJson
        );
        assert_eq!(
            parse_agents_command("background show 123"),
            AgentsSlashCommand::BackgroundShow("123")
        );
        assert_eq!(
            parse_agents_command("background show 123 --json"),
            AgentsSlashCommand::BackgroundShowJson("123")
        );
        assert_eq!(
            parse_agents_command("bg show-json latest"),
            AgentsSlashCommand::BackgroundShowJson("latest")
        );
        assert_eq!(
            parse_agents_command("bg inspect latest"),
            AgentsSlashCommand::BackgroundShow("latest")
        );
        assert_eq!(
            parse_agents_command("background log 123"),
            AgentsSlashCommand::BackgroundLog("123")
        );
        assert_eq!(
            parse_agents_command("bg stop 123"),
            AgentsSlashCommand::BackgroundKill("123")
        );
        assert_eq!(
            parse_agents_command("background prune"),
            AgentsSlashCommand::BackgroundPrune
        );
        assert_eq!(
            parse_agents_command("bg clear"),
            AgentsSlashCommand::BackgroundPrune
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
        let record = background_agent_record(&launch, &started, Path::new("/usr/bin/lcode"));
        assert_eq!(record.pid, 4242);
        assert_eq!(record.run_id, format!("bg-{}-4242", record.started_at_ms));
        assert_eq!(record.name, "reviewer");
        assert_eq!(record.mode, "plan");
        assert_eq!(record.cwd, "/tmp/project");
        assert_eq!(record.log_path, "/tmp/reviewer.log");
        assert_eq!(record.prompt_preview, "Run review with details");
        assert_eq!(
            record.launched_argv,
            vec![
                "/usr/bin/lcode".to_string(),
                "--provider".to_string(),
                "libertai".to_string(),
                "--model".to_string(),
                "qwen".to_string(),
                "--plan".to_string(),
                "Run review\nwith details".to_string(),
            ]
        );
        assert!(record.started_at_ms > 0);
    }

    #[test]
    fn background_agent_record_id_backfills_legacy_records() {
        let mut record = BackgroundAgentRecord {
            pid: 4242,
            run_id: String::new(),
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: "normal".to_string(),
            prompt_preview: "Run review".to_string(),
            cwd: "/tmp/project".to_string(),
            log_path: "/tmp/reviewer.log".to_string(),
            started_at_ms: 99,
            launched_argv: Vec::new(),
        };
        assert_eq!(background_agent_record_id(&record), "bg-99-4242");
        record.run_id = "custom-run".to_string();
        assert_eq!(background_agent_record_id(&record), "custom-run");
    }

    #[test]
    fn background_agent_details_include_runtime_metadata() {
        let record = BackgroundAgentRecord {
            pid: 4242,
            run_id: "bg-0-4242".to_string(),
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: "plan".to_string(),
            prompt_preview: "Run review".to_string(),
            cwd: "/tmp/project".to_string(),
            log_path: "/tmp/reviewer.log".to_string(),
            started_at_ms: 0,
            launched_argv: vec![
                "/usr/bin/lcode".to_string(),
                "--provider".to_string(),
                "libertai".to_string(),
                "--model".to_string(),
                "qwen".to_string(),
                "--plan".to_string(),
                "Run review".to_string(),
            ],
        };
        let details = format_background_agent_details(&record, BackgroundAgentStatus::Running);
        assert!(details.contains("background agent: pid 4242"));
        assert!(details.contains("run id:"));
        assert!(details.contains("bg-0-4242"));
        assert!(details.contains("status:"));
        assert!(details.contains("running"));
        assert!(details.contains("name:"));
        assert!(details.contains("reviewer"));
        assert!(details.contains("provider:"));
        assert!(details.contains("libertai"));
        assert!(details.contains("model:"));
        assert!(details.contains("qwen"));
        assert!(details.contains("mode:"));
        assert!(details.contains("plan"));
        assert!(details.contains("/tmp/project"));
        assert!(details.contains("/tmp/reviewer.log"));
        assert!(details.contains("command:"));
        assert!(details.contains(
            "'/usr/bin/lcode' '--provider' 'libertai' '--model' 'qwen' '--plan' 'Run review'"
        ));
        assert!(details.contains("Run review"));
    }

    #[test]
    fn background_agent_json_includes_status_and_run_id() {
        let record = BackgroundAgentRecord {
            pid: 4242,
            run_id: "bg-0-4242".to_string(),
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: "plan".to_string(),
            prompt_preview: "Run review".to_string(),
            cwd: "/tmp/project".to_string(),
            log_path: "/tmp/reviewer.log".to_string(),
            started_at_ms: 0,
            launched_argv: Vec::new(),
        };
        let payload = BackgroundAgentRecordJson {
            record: &record,
            status: BackgroundAgentStatus::Running.label(),
        };
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(raw.contains("\"run_id\":\"bg-0-4242\""));
        assert!(raw.contains("\"status\":\"running\""));
    }

    #[test]
    fn background_agent_details_json_includes_command_metadata() {
        let record = BackgroundAgentRecord {
            pid: 4242,
            run_id: "bg-0-4242".to_string(),
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: "plan".to_string(),
            prompt_preview: "Run review".to_string(),
            cwd: "/tmp/project".to_string(),
            log_path: "/tmp/reviewer.log".to_string(),
            started_at_ms: 0,
            launched_argv: Vec::new(),
        };
        let payload = BackgroundAgentDetailsJson {
            surface: "terminal",
            command: "agents background show",
            query: "latest",
            aliases: &["agents background show", "agents bg show"],
            supported_actions: background_agents_supported_actions(),
            record: &record,
            status: BackgroundAgentStatus::Running.label(),
        };
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(raw.contains("\"surface\":\"terminal\""));
        assert!(raw.contains("\"command\":\"agents background show\""));
        assert!(raw.contains("\"query\":\"latest\""));
        assert!(raw.contains("\"aliases\":[\"agents background show\",\"agents bg show\"]"));
        assert!(raw.contains("\"inspect <pid|run-id|latest> --json\""));
        assert!(raw.contains("\"run_id\":\"bg-0-4242\""));
        assert!(raw.contains("\"status\":\"running\""));
    }

    #[test]
    fn background_agent_missing_json_includes_counts_and_metadata() {
        let payload = BackgroundAgentMissingJson {
            surface: "terminal",
            command: "agents background show",
            query: "missing-run",
            aliases: &["agents background show", "agents bg show"],
            supported_actions: background_agents_supported_actions(),
            error: "not_found",
            counts: BackgroundAgentStatusCounts {
                total: 1,
                running: 0,
                exited: 1,
                unknown: 0,
            },
        };
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(raw.contains("\"surface\":\"terminal\""));
        assert!(raw.contains("\"command\":\"agents background show\""));
        assert!(raw.contains("\"query\":\"missing-run\""));
        assert!(raw.contains("\"error\":\"not_found\""));
        assert!(raw.contains("\"counts\":{\"total\":1,\"running\":0,\"exited\":1,\"unknown\":0}"));
    }

    #[test]
    fn background_agent_list_json_includes_command_metadata() {
        let record = BackgroundAgentRecord {
            pid: 4242,
            run_id: "bg-0-4242".to_string(),
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: "plan".to_string(),
            prompt_preview: "Run review".to_string(),
            cwd: "/tmp/project".to_string(),
            log_path: "/tmp/reviewer.log".to_string(),
            started_at_ms: 0,
            launched_argv: Vec::new(),
        };
        let payload = BackgroundAgentListJson {
            surface: "terminal",
            command: "agents background",
            query: "background list --json",
            aliases: &["agents background", "agents bg"],
            supported_actions: background_agents_supported_actions(),
            counts: BackgroundAgentStatusCounts {
                total: 1,
                running: 1,
                exited: 0,
                unknown: 0,
            },
            records: vec![BackgroundAgentRecordJson {
                record: &record,
                status: BackgroundAgentStatus::Running.label(),
            }],
        };
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(raw.contains("\"surface\":\"terminal\""));
        assert!(raw.contains("\"command\":\"agents background\""));
        assert!(raw.contains("\"query\":\"background list --json\""));
        assert!(raw.contains("\"aliases\":[\"agents background\",\"agents bg\"]"));
        assert!(raw.contains("\"show <pid|run-id|latest> --json\""));
        assert!(raw.contains("\"run_id\":\"bg-0-4242\""));
    }

    #[test]
    fn resolve_background_agent_record_accepts_pid_run_id_and_latest() {
        let records = vec![
            BackgroundAgentRecord {
                pid: 1111,
                run_id: "bg-10-1111".to_string(),
                name: "first".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "one".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/one.log".to_string(),
                started_at_ms: 10,
                launched_argv: Vec::new(),
            },
            BackgroundAgentRecord {
                pid: 2222,
                run_id: "bg-20-2222".to_string(),
                name: "second".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "two".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/two.log".to_string(),
                started_at_ms: 20,
                launched_argv: Vec::new(),
            },
        ];
        assert_eq!(
            resolve_background_agent_record_from_records(records.clone(), "1111")
                .unwrap()
                .unwrap()
                .pid,
            1111
        );
        assert_eq!(
            resolve_background_agent_record_from_records(records.clone(), "bg-20-2222")
                .unwrap()
                .unwrap()
                .pid,
            2222
        );
        assert!(
            resolve_background_agent_record_from_records(records.clone(), "missing-run")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolve_background_agent_record_from_records(records, "latest")
                .unwrap()
                .unwrap()
                .pid,
            2222
        );
    }

    #[test]
    fn retain_running_background_agent_records_prunes_finished_runs() {
        let records = vec![
            BackgroundAgentRecord {
                pid: 1,
                run_id: "bg-10-1".to_string(),
                name: "running".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "one".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/one.log".to_string(),
                started_at_ms: 10,
                launched_argv: Vec::new(),
            },
            BackgroundAgentRecord {
                pid: 2,
                run_id: "bg-20-2".to_string(),
                name: "done".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "two".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/two.log".to_string(),
                started_at_ms: 20,
                launched_argv: Vec::new(),
            },
        ];
        let kept = retain_running_background_agent_records(records, |pid| {
            if pid == 1 {
                BackgroundAgentStatus::Running
            } else {
                BackgroundAgentStatus::Exited
            }
        });
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].pid, 1);
    }

    #[test]
    fn background_agent_status_counts_summarize_records() {
        let records = vec![
            BackgroundAgentRecord {
                pid: 1,
                run_id: "bg-10-1".to_string(),
                name: "running".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "one".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/one.log".to_string(),
                started_at_ms: 10,
                launched_argv: Vec::new(),
            },
            BackgroundAgentRecord {
                pid: 2,
                run_id: "bg-20-2".to_string(),
                name: "done".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "two".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/two.log".to_string(),
                started_at_ms: 20,
                launched_argv: Vec::new(),
            },
            BackgroundAgentRecord {
                pid: 3,
                run_id: "bg-30-3".to_string(),
                name: "unknown".to_string(),
                provider: "libertai".to_string(),
                model: "qwen".to_string(),
                mode: "normal".to_string(),
                prompt_preview: "three".to_string(),
                cwd: "/tmp/project".to_string(),
                log_path: "/tmp/three.log".to_string(),
                started_at_ms: 30,
                launched_argv: Vec::new(),
            },
        ];
        assert_eq!(
            background_agent_status_counts(&records, |pid| match pid {
                1 => BackgroundAgentStatus::Running,
                2 => BackgroundAgentStatus::Exited,
                _ => BackgroundAgentStatus::Unknown,
            }),
            BackgroundAgentStatusCounts {
                total: 3,
                running: 1,
                exited: 1,
                unknown: 1,
            }
        );
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
        let usage = parse_agents_create_query("").unwrap_err().to_string();
        assert!(usage.contains("--same-cwd"));
    }

    #[test]
    fn parse_agents_command_accepts_list_open_and_create() {
        assert_eq!(parse_agents_command(""), AgentsSlashCommand::List);
        assert_eq!(parse_agents_command("list"), AgentsSlashCommand::List);
        assert_eq!(parse_agents_command("show"), AgentsSlashCommand::List);
        assert_eq!(parse_agents_command("json"), AgentsSlashCommand::ListJson);
        assert_eq!(
            parse_agents_command("status --json"),
            AgentsSlashCommand::ListJson
        );
        assert_eq!(
            parse_agents_command("show reviewer"),
            AgentsSlashCommand::Show("reviewer")
        );
        assert_eq!(
            parse_agents_command("show reviewer --json"),
            AgentsSlashCommand::ShowJson("reviewer")
        );
        assert_eq!(parse_agents_command("open"), AgentsSlashCommand::Open);
        assert_eq!(parse_agents_command("settings"), AgentsSlashCommand::Open);
        assert_eq!(parse_agents_command("edit"), AgentsSlashCommand::Open);
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
        assert!(AGENTS_USAGE.contains("settings|edit"));
        assert!(AGENTS_USAGE.contains("background|bg"));
        assert!(AGENTS_USAGE.contains("kill|stop"));
        assert!(AGENTS_USAGE.contains("delete|remove"));
        assert!(AGENTS_USAGE.contains("list --json"));
        assert!(AGENTS_USAGE.contains("show --json"));
        assert!(AGENTS_USAGE.contains("status --json"));
        let hint = help_command_arg_hint("agents");
        assert!(hint.contains("open|settings|edit"));
        assert!(hint.contains("background|bg"));
        assert!(hint.contains("create [--worktree|--same-cwd] <name>"));
        assert!(hint.contains("delete|remove <name>"));
    }

    #[test]
    fn format_agent_details_includes_metadata_and_prompt_preview() {
        let agent = crate::commands::code_agents::AgentDefinition {
            name: "reviewer".to_string(),
            description: "Reviews changes".to_string(),
            tools: Some(vec!["read".to_string(), "grep".to_string()]),
            model: Some("qwen".to_string()),
            worktree: true,
            system_prompt: "Review carefully.\nCite files.".to_string(),
            source: crate::commands::code_agents::AgentSource::Project(PathBuf::from(
                "/tmp/project/.libertai/agents",
            )),
        };
        let details = format_agent_details(&agent);
        assert!(details.contains("agent: reviewer"));
        assert!(details.contains("description: Reviews changes"));
        assert!(details.contains("model: qwen"));
        assert!(details.contains("tools: read, grep"));
        assert!(details.contains("isolation: worktree"));
        assert!(details.contains("/tmp/project/.libertai/agents/reviewer.md"));
        assert!(details.contains("Review carefully.\nCite files."));

        let payload = agents_json_payload("  status --json  ", Path::new("/tmp/project"), &[agent]);
        assert_eq!(payload["command"], "agents");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["worktree_default_count"], 1);
        assert_eq!(payload["agents"][0]["name"], "reviewer");
        assert_eq!(payload["agents"][0]["path"], "/tmp/project/.libertai/agents/reviewer.md");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show <name> --json")));

        let missing = agent_missing_json_payload("missing", Path::new("/tmp/project"));
        assert_eq!(missing["command"], "agents");
        assert_eq!(missing["query"], "show missing --json");
        assert_eq!(missing["error"], "not_found");
        assert_eq!(missing["name"], "missing");
        assert_eq!(missing["will_write"], false);
        assert!(missing["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show <name> --json")));
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
        assert_eq!(
            parse_template_query("team/audit src/lib.rs").unwrap(),
            ("team/audit", "src/lib.rs")
        );
        assert_eq!(parse_template_query("review").unwrap(), ("review", ""));
        assert!(parse_template_query("").is_err());
    }

    #[test]
    fn template_json_arg_accepts_status_aliases() {
        assert!(is_template_json_arg("json"));
        assert!(is_template_json_arg("--json"));
        assert!(is_template_json_arg("status --json"));
        assert!(is_template_json_arg("list --json"));
        assert!(is_template_json_arg("show --json"));
        assert!(!is_template_json_arg("review src/lib.rs"));
    }

    #[test]
    fn template_list_arg_accepts_plain_list_aliases() {
        assert!(is_template_list_arg("list"));
        assert!(is_template_list_arg("show"));
        assert!(is_template_list_arg(" SHOW "));
        assert!(!is_template_list_arg("show --json"));
        assert!(!is_template_list_arg("review src/lib.rs"));
    }

    #[test]
    fn template_json_payload_lists_discovered_templates() {
        let temp = tempfile::tempdir().unwrap();
        let commands_dir = temp.path().join(".claude").join("commands").join("team");
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(
            commands_dir.join("audit.md"),
            "---\ndescription: Team audit\nargument-hint: target\narguments: [target]\n---\nAudit $target",
        )
        .unwrap();

        let payload = template_json_payload(temp.path(), "list --json");
        let row = payload["templates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["invocation"] == "team/audit")
            .unwrap();

        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "template");
        assert_eq!(payload["query"], "list --json");
        assert!(payload["count"].as_u64().unwrap() >= 1);
        assert_eq!(row["name"], "audit");
        assert_eq!(row["description"], "Team audit");
        assert_eq!(row["source"], "project");
        assert_eq!(row["namespace"], "team");
        assert_eq!(row["arg_hint"], "target");
        assert_eq!(row["argument_names"][0], "target");
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["aliases"][0], "template");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("list --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show --json")));
    }

    #[test]
    fn custom_slash_matching_accepts_namespace_qualified_names() {
        let command = crate::commands::code_slash_registry::CustomCommand {
            name: "audit".to_string(),
            namespace: Some("team".to_string()),
            description: None,
            arg_hint: None,
            argument_names: Vec::new(),
            body: "Audit $ARGUMENTS".to_string(),
            source: crate::commands::code_slash_registry::CommandSource::Project,
            path: PathBuf::from(".claude/commands/team/audit.md"),
        };

        assert_eq!(custom_slash_invocation_name(&command), "team/audit");
        assert!(custom_slash_matches(&command, "audit"));
        assert!(custom_slash_matches(&command, "team/audit"));
        assert!(custom_slash_starts_with(&command, "team/aud"));
        assert_eq!(
            parse_direct_custom_slash("/team/audit src"),
            Some(("team/audit", "src"))
        );
    }

    #[test]
    fn parse_skills_command_accepts_list_and_toggles() {
        assert_eq!(parse_skills_command("").unwrap(), SkillsCommand::List);
        assert_eq!(parse_skills_command("status").unwrap(), SkillsCommand::List);
        assert_eq!(parse_skills_command("show").unwrap(), SkillsCommand::List);
        assert_eq!(parse_skills_command("json").unwrap(), SkillsCommand::Json);
        assert_eq!(
            parse_skills_command("status --json").unwrap(),
            SkillsCommand::Json
        );
        assert_eq!(parse_skills_command("--json").unwrap(), SkillsCommand::Json);
        assert_eq!(
            parse_skills_command("list --json").unwrap(),
            SkillsCommand::Json
        );
        assert_eq!(
            parse_skills_command("show --json").unwrap(),
            SkillsCommand::Json
        );
        assert_eq!(
            parse_skills_command("show libertai-harness").unwrap(),
            SkillsCommand::Show("libertai-harness".to_string())
        );
        assert_eq!(
            parse_skills_command("show libertai-harness --json").unwrap(),
            SkillsCommand::ShowJson("libertai-harness".to_string())
        );
        assert_eq!(parse_skills_command("open").unwrap(), SkillsCommand::Open);
        assert_eq!(
            parse_skills_command("settings").unwrap(),
            SkillsCommand::Open
        );
        assert_eq!(parse_skills_command("edit").unwrap(), SkillsCommand::Open);
        assert_eq!(
            parse_skills_command("enable libertai-harness").unwrap(),
            SkillsCommand::Enable("libertai-harness".to_string())
        );
        assert_eq!(
            parse_skills_command("off project-review").unwrap(),
            SkillsCommand::Disable("project-review".to_string())
        );
        assert!(parse_skills_command("enable").is_err());
        let usage = parse_skills_command("remove foo").unwrap_err().to_string();
        assert!(usage.contains("settings"));
        assert!(usage.contains("json"));
        assert!(usage.contains("--json"));
        assert!(usage.contains("show --json"));
        assert!(usage.contains("show <name> --json"));
        assert!(usage.contains("on <name>"));
        assert!(usage.contains("off <name>"));
    }

    #[test]
    fn code_skills_json_payload_reports_counts_and_rows() {
        let payload = code_skills_json_payload(
            Path::new("/tmp/project"),
            "status --json",
            vec![
                code_skills::SkillInventoryEntry {
                    name: "libertai-harness".to_string(),
                    description: "Verification workflow".to_string(),
                    allowed_tools: None,
                    body: "Run checks.".to_string(),
                    source: "builtin".to_string(),
                    source_kind: "builtin".to_string(),
                    path: None,
                    agent_created: false,
                    enabled: true,
                },
                code_skills::SkillInventoryEntry {
                    name: "project-review".to_string(),
                    description: "Project review flow".to_string(),
                    allowed_tools: Some("read, grep".to_string()),
                    body: "Review changes.".to_string(),
                    source: "project:/tmp/project/.libertai/skills/project-review".to_string(),
                    source_kind: "project".to_string(),
                    path: Some(PathBuf::from("/tmp/project/.libertai/skills/project-review")),
                    agent_created: true,
                    enabled: false,
                },
            ],
        );

        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "skills");
        assert_eq!(payload["query"], "status --json");
        assert_eq!(payload["count"], 2);
        assert_eq!(payload["enabled_count"], 1);
        assert_eq!(payload["disabled_count"], 1);
        assert_eq!(payload["skills"][1]["name"], "project-review");
        assert_eq!(payload["skills"][1]["allowed_tools"], "read, grep");
        assert_eq!(payload["skills"][1]["agent_created"], true);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["aliases"][0], "skills");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("list --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("off <name>")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show <name> --json")));
    }

    #[test]
    fn format_code_skill_details_includes_metadata_and_preview() {
        let skill = code_skills::SkillInventoryEntry {
            name: "project-review".to_string(),
            description: "Project review flow".to_string(),
            allowed_tools: Some("read, grep".to_string()),
            body: "Prefer focused findings.\nCite files.".to_string(),
            source: "project:/tmp/.libertai/skills/project-review".to_string(),
            source_kind: "project".to_string(),
            path: Some(PathBuf::from("/tmp/.libertai/skills/project-review")),
            agent_created: true,
            enabled: false,
        };
        let details = format_code_skill_details(&skill);
        assert!(details.contains("skill: project-review"));
        assert!(details.contains("state: off"));
        assert!(details.contains("description: Project review flow"));
        assert!(details.contains("tools: read, grep"));
        assert!(details.contains("/tmp/.libertai/skills/project-review"));
        assert!(details.contains("agent-created: yes"));
        assert!(details.contains("Prefer focused findings.\nCite files."));

        let payload = code_skill_detail_json_payload(Path::new("/tmp/project"), "project-review", Some(&skill));
        assert_eq!(payload["command"], "skills");
        assert_eq!(payload["query"], "show project-review --json");
        assert_eq!(payload["name"], "project-review");
        assert_eq!(payload["skill"]["name"], "project-review");
        assert_eq!(payload["skill"]["instruction_preview"], "Prefer focused findings.\nCite files.");
        assert_eq!(payload["will_write"], false);
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show <name> --json")));

        let missing = code_skill_detail_json_payload(Path::new("/tmp/project"), "missing", None);
        assert_eq!(missing["error"], "not_found");
        assert_eq!(missing["query"], "show missing --json");
        assert_eq!(missing["name"], "missing");
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
        assert_eq!(help_command_arg_hint("review"), "[scope]");
        assert_eq!(help_command_arg_hint("security-review"), "[scope]");
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
        assert_eq!(
            pr_comments_drafts_arg("/pr_comments drafts submit comment Looks good."),
            Some("submit comment Looks good.")
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
        assert_eq!(
            parse_pr_comments_draft_submit_review("submit approve").unwrap(),
            Some(("approve", ""))
        );
        assert_eq!(
            parse_pr_comments_draft_submit_review("submit comment Summary.").unwrap(),
            Some(("comment", "Summary."))
        );
        assert!(parse_pr_comments_draft_submit_review("submit comment").is_err());

        let hint = help_command_arg_hint("pr_comments");
        assert!(hint.contains("resolve <thread_id>|unresolve <thread_id>|reopen <thread_id>"));
        assert!(hint.contains("viewed <path>|view <path>|viewed --all"));
        assert!(hint.contains("thread <path>:<line> <body>|comment <path>:<line> <body>"));
        assert!(hint.contains("drafts submit comment <body>|drafts submit request_changes <body>"));
        assert!(hint.contains("reply <thread_id> <body>|edit <comment_id> <body>"));
        assert!(hint.contains("review <approve|comment|request_changes> [body]"));
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
        assert_eq!(
            parse_direct_custom_slash("/team/review src"),
            Some(("team/review", "src"))
        );
        assert_eq!(parse_direct_custom_slash("/review"), Some(("review", "")));
        assert_eq!(parse_direct_custom_slash("review"), None);
    }
}
