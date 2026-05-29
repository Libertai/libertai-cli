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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
const STATUS_LINE_TEMPLATE_MAX_CHARS: usize = 240;
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
            let text = expand_status_line_template(&s.status_line_template, &s, mode)
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

    // If we resumed, print the rehydrated transcript so the user has
    // visual context before the input bar takes over. Skipped for fresh
    // sessions — there's nothing to show.
    if let Ok(messages) = handle.messages().await {
        if !messages.is_empty() {
            print_rehydrated_transcript(&messages);
        }
    }

    // In-memory input history (no persistence in v0).
    let mut history: VecDeque<String> = VecDeque::with_capacity(64);
    let mut output_style: Option<String> = None;
    let mut usage_history: Vec<UsageRecord> = Vec::new();
    let tool_activity = Arc::new(Mutex::new(ToolActivityTracker::default()));
    let mut session_name: Option<String> = None;
    let mut autonomous_queue: VecDeque<String> = VecDeque::new();
    let mut auto_run: Option<AutoRun> = None;

    loop {
        let autonomous_turn = if let Some(prompt) = autonomous_queue.pop_front() {
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
                if let Some(run) = auto_run.as_ref() {
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
                print_model_status(&handle, &cfg);
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
                        handle = next;
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
                                handle = next;
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
                                handle = next;
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
                                handle = next;
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
            "/init" => {
                print_init_project();
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
            let action = rest.trim();
            if action.eq_ignore_ascii_case("path") {
                match crate::config::config_path() {
                    Ok(path) => println!("{DIM}  config path: {}{RESET}", path.display()),
                    Err(e) => eprintln!("{DIM}  /config path: {e:#}{RESET}"),
                }
            } else {
                print_config_status(&cfg);
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/settings ") {
            let action = rest.trim();
            if action.eq_ignore_ascii_case("path") {
                match crate::config::config_path() {
                    Ok(path) => println!("{DIM}  config path: {}{RESET}", path.display()),
                    Err(e) => eprintln!("{DIM}  /settings path: {e:#}{RESET}"),
                }
            } else {
                print_config_status(&cfg);
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
                            handle = next;
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
        if let Some((_command, rest)) = mode_command_arg(trimmed) {
            match parse_permissions_command(rest) {
                PermissionsCommand::Show => print_permissions_status(mode.get()),
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
            match parse_model_spec(&provider, rest) {
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
                            });
                            println!("{DIM}  → model set to {provider}/{model}{RESET}");
                        }
                        Err(e) => eprintln!("{DIM}  /model: {e:#}{RESET}"),
                    }
                }
                Err(e) => eprintln!("{DIM}  /model: {e:#}{RESET}"),
            }
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
                    println!("{DIM}  usage: /agent [--worktree] <name> <task>{RESET}");
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("/agent ") {
                    match build_agent_slash_prompt(rest.trim()) {
                        Ok(prompt) => {
                            line = prompt;
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
            let command = rest.trim();
            if command.is_empty() {
                println!("{DIM}  usage: !<command> — run a local shell command in this cwd{RESET}");
            } else {
                run_shell_escape(command, bash_command_wrapper.as_deref());
            }
            continue;
        }

        // Remember the submitted line.
        if !is_autonomous && history.back().is_none_or(|last| last != trimmed) {
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
        let render = {
            let tool_activity = Arc::clone(&tool_activity);
            move |event: AgentEvent| {
                if let Ok(mut tracker) = tool_activity.lock() {
                    tracker.observe(&event);
                }
                render_event(event);
            }
        };
        let result = if let Some(content) = content_override {
            handle
                .prompt_with_content_with_abort(content, abort_signal, render)
                .await
        } else {
            let agent_line = apply_output_style(output_style.as_deref(), &line);
            handle.prompt_with_abort(agent_line, abort_signal, render).await
        };
        clear_current_abort();

        // `render_event` already emits a trailing newline on AgentEnd,
        // so we don't need a second one here — emitting one would
        // leave a gap between the response and the usage/status line.
        match result {
            Ok(msg) => {
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
                });
                eprintln!(
                    "{DIM}  {}/{}  stop: {:?}  in={} out={}{RESET}",
                    msg.provider,
                    msg.model,
                    msg.stop_reason,
                    msg.usage.input,
                    msg.usage.output,
                );
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
        Some(cfg),
    ));
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
    println!("{DIM}  /permissions [default|acceptEdits|plan|forget]{RESET}");
    println!("{DIM}  /mode [default|acceptEdits|plan] — alias for /permissions{RESET}");
    println!("{DIM}  /model [model|provider/model]{RESET}");
    println!("{DIM}  /name <name> — set this session's display name (also /rename){RESET}");
    println!("{DIM}  /status   — show current REPL session status{RESET}");
    println!("{DIM}  /doctor   — run a local session/config diagnostic report{RESET}");
    println!("{DIM}  /abort    — show how to interrupt the active CLI turn{RESET}");
    println!("{DIM}  /review [scope] — ask the agent to review current code changes{RESET}");
    println!("{DIM}  /security-review [scope] — ask for a focused security review{RESET}");
    println!("{DIM}  /pr_comments [scope] — ask the agent to inspect PR review comments{RESET}");
    println!("{DIM}  /sandbox [info|reload] — inspect the bash sandbox profile{RESET}");
    println!("{DIM}  /usage    — show token usage for this REPL session (also /cost){RESET}");
    println!("{DIM}  /history [count] — show recent submitted prompts{RESET}");
    println!("{DIM}  /copy     — copy the last assistant response to the terminal clipboard{RESET}");
    println!("{DIM}  /config   — show active configuration summary (/settings is an alias){RESET}");
    println!("{DIM}  /statusline <template|reset> — customize the input-bar status line{RESET}");
    println!("{DIM}  /hotkeys  — show input bar keyboard controls{RESET}");
    println!("{DIM}  /tree [path] — show a bounded project tree{RESET}");
    println!("{DIM}  /changelog [count] — show recent git commits{RESET}");
    println!("{DIM}  /reload   — reload config and start a fresh agent session{RESET}");
    println!("{DIM}  /resume [path] — resume the latest or specified saved session{RESET}");
    println!("{DIM}  /fork [list|index|id] — fork from a previous user message{RESET}");
    println!("{DIM}  /thinking [off|minimal|low|medium|high|xhigh] — show or set thinking{RESET}");
    println!("{DIM}  /compact — compact older conversation history now{RESET}");
    println!("{DIM}  /loop [turns] [goal] — run bounded autonomous follow-up turns{RESET}");
    println!("{DIM}  /auto on [turns] [goal] — bounded continuous execution (/auto off|status){RESET}");
    println!("{DIM}  /image <path> [prompt] — attach a local image to the next prompt{RESET}");
    println!("{DIM}  /attach <path> [prompt] — alias for /image{RESET}");
    println!("{DIM}  /mention <path> [prompt] — attach a local text file to the next prompt{RESET}");
    println!("{DIM}  /login    — run libertai login, then reload this REPL session{RESET}");
    println!("{DIM}  /logout   — run libertai logout, then reload this REPL session{RESET}");
    println!("{DIM}  /memory   — show project memory (/memory edit|clear|files|references|import <path>|import-claude|import-claude-all|path){RESET}");
    println!("{DIM}  /init     — create AGENTS.md for this project if missing{RESET}");
    println!("{DIM}  /agents   — list named sub-agents{RESET}");
    println!("{DIM}  /agent [--worktree] <name> <task> — run a named sub-agent task{RESET}");
    println!("{DIM}  /template <name> [args] — expand a prompt template{RESET}");
    println!("{DIM}  /export [path] — write this session transcript as Markdown{RESET}");
    println!("{DIM}  /share [path] — write this session transcript as shareable HTML{RESET}");
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

fn print_model_status(handle: &AgentSessionHandle, cfg: &LibertaiConfig) {
    let (provider, model) = handle.model();
    println!("{BOLD}model{RESET}");
    println!("{DIM}  current:{RESET} {provider}/{model}");
    println!(
        "{DIM}  default:{RESET} {}/{}",
        cfg.default_code_provider, cfg.default_code_model
    );
    println!("{DIM}  usage:{RESET} /model <model|provider/model>");
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
    let path = match share_path(path) {
        Ok(path) => path,
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
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("{DIM}  /share: could not create {}: {e}{RESET}", parent.display());
            return;
        }
    }
    match std::fs::write(&path, html) {
        Ok(()) => println!("{DIM}  share HTML written: {}{RESET}", path.display()),
        Err(e) => eprintln!("{DIM}  /share: could not write {}: {e}{RESET}", path.display()),
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
    if raw.is_empty() {
        return Ok(PathBuf::from("libertai-transcript.md"));
    }
    Ok(PathBuf::from(raw))
}

fn share_path(path: Option<&str>) -> Result<PathBuf> {
    let raw = path.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(PathBuf::from("libertai-share.html"));
    }
    Ok(PathBuf::from(raw))
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

fn print_init_project() {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{DIM}  /init: could not resolve cwd: {e}{RESET}");
            return;
        }
    };
    match crate::commands::code_init::init_project(&cwd) {
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
            println!();
        }
        Err(e) => eprintln!("{DIM}  /init: failed: {e:#}{RESET}"),
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
    if action.eq_ignore_ascii_case("edit") {
        match crate::commands::code_memory::ensure_memory_file(&cwd) {
            Ok(path) => open_memory_editor(&path),
            Err(e) => eprintln!("{DIM}  /memory edit: failed: {e:#}{RESET}"),
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
    println!();
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
            let desc = t.description.as_deref().unwrap_or(match t.source {
                crate::commands::code_slash_registry::CommandSource::Project => "project template",
                crate::commands::code_slash_registry::CommandSource::User => "user template",
            });
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
    let scope = scope.trim();
    let scope_line = if scope.is_empty() {
        "User-requested PR scope: infer the current branch's pull request.".to_string()
    } else {
        format!("User-requested PR scope: {scope}")
    };
    format!(
        r#"Inspect pull request review comments for this repository and turn them into an actionable response plan.

{scope_line}

Rules:
- Do not modify files or make commits.
- First inspect git state: git status --short, git branch --show-current,
  git remote -v, and git diff --stat.
- Prefer GitHub CLI when available: use gh pr view --json number,url,headRefName,baseRefName,reviewDecision,comments,reviews,files and gh pr checks.
- If the user supplied a PR number or URL, use that exact PR. Otherwise infer the PR for the current branch.
- Summarize unresolved review comments first, grouped by file and reviewer when possible.
- For each actionable comment, cite file:line when available, explain the requested change, and propose the minimal fix.
- Call out comments that appear already addressed by the current diff.
- If PR data cannot be loaded, report the exact command/error and suggest the next concrete command the user can run."#
    )
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

fn build_agent_slash_prompt(query: &str) -> Result<String> {
    let parsed = parse_agent_slash_query(query)?;
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let agents = crate::commands::code_agents::discover_agents(&cwd)?;
    build_agent_prompt_from_defs(&parsed, &agents)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentSlashQuery<'a> {
    name: &'a str,
    task: &'a str,
    isolation: Option<AgentSlashIsolation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentSlashIsolation {
    Worktree,
    SameCwd,
}

fn parse_agent_slash_query(query: &str) -> Result<AgentSlashQuery<'_>> {
    let raw = query.trim();
    let mut isolation = None;
    let mut rest = raw;
    loop {
        let Some((head, tail)) = split_first_word(rest) else {
            anyhow::bail!("usage: /agent [--worktree] <name> <task>");
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
            _ => break,
        }
    }
    let Some((name, task)) = rest.split_once(char::is_whitespace) else {
        anyhow::bail!("usage: /agent [--worktree] <name> <task>");
    };
    let name = name.trim();
    let task = task.trim();
    if name.is_empty() || task.is_empty() {
        anyhow::bail!("usage: /agent [--worktree] <name> <task>");
    }
    Ok(AgentSlashQuery {
        name,
        task,
        isolation,
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
        match crate::commands::code_agents::discover_agents(cwd) {
            Ok(agents) => println!(
                "{}",
                doctor_line(true, "named agents", format!("{} loaded", agents.len()))
            ),
            Err(e) => println!("{}", doctor_line(false, "named agents", e.to_string())),
        }
        let templates = crate::commands::code_slash_registry::discover(cwd);
        println!(
            "{}",
            doctor_line(
                true,
                "custom slash commands",
                format!("{} loaded", templates.len())
            )
        );
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
    match summary {
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
            print_tool_activity(tool_activity);
        }
        None => {
            println!("{DIM}  no usage recorded yet — send a prompt first.{RESET}");
            print_tool_activity(tool_activity);
        }
    }
    println!();
}

fn print_tool_activity(tool_activity: &[ToolActivitySummary]) {
    if tool_activity.is_empty() {
        return;
    }
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
    match cfg.auth.api_key.as_deref() {
        Some(key) => println!("{DIM}  auth:{RESET} {}", mask_key(key)),
        None => println!("{DIM}  auth:{RESET} not logged in"),
    }
    println!(
        "{DIM}  edit:{RESET} libertai config show|path|set|unset, or use the desktop settings UI"
    );
    println!();
}

fn print_status_line_status(cfg: &LibertaiConfig) {
    println!("{BOLD}statusline{RESET}");
    println!(
        "{DIM}  current:{RESET} {}",
        if cfg.status_line_template.trim().is_empty() {
            "(default)"
        } else {
            cfg.status_line_template.as_str()
        }
    );
    println!("{DIM}  {}{RESET}", status_line_help());
    println!(
        "{DIM}  usage:{RESET} /statusline <template>, /statusline reset, /statusline status"
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
    if action.eq_ignore_ascii_case("reset") || action.eq_ignore_ascii_case("clear") {
        next.status_line_template.clear();
    } else {
        next.status_line_template = normalize_status_line_template(action);
    }
    crate::config::save(&next).context("save config")?;
    *cfg = Arc::new(next);
    let template = cfg.status_line_template.clone();
    update_bar_status(|status| status.status_line_template = template.clone());
    if cfg.status_line_template.trim().is_empty() {
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
        };
        assert!(expand_status_line_template("", &status, Mode::Normal).is_none());
        assert_eq!(default_rule_text(&status), "50% · 512 / 1.0k · libertai/qwen");
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
        assert_eq!(parse_permissions_command("forget"), PermissionsCommand::Forget);
        assert_eq!(
            parse_permissions_command("bypassPermissions"),
            PermissionsCommand::UnsupportedBypass
        );
        assert_eq!(parse_permissions_command("wat"), PermissionsCommand::Show);
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
    }

    #[test]
    fn share_path_uses_default_or_custom_path() {
        assert_eq!(
            share_path(None).unwrap(),
            PathBuf::from("libertai-share.html")
        );
        assert_eq!(
            share_path(Some("out/session.html")).unwrap(),
            PathBuf::from("out/session.html")
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
                isolation: None
            }
        );
        assert_eq!(
            parse_agent_slash_query("--worktree reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::Worktree)
            }
        );
        assert_eq!(
            parse_agent_slash_query("--isolation=worktree reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::Worktree)
            }
        );
        assert_eq!(
            parse_agent_slash_query("--worktree --same-cwd reviewer inspect src").unwrap(),
            AgentSlashQuery {
                name: "reviewer",
                task: "inspect src",
                isolation: Some(AgentSlashIsolation::SameCwd)
            }
        );
        assert!(parse_agent_slash_query("reviewer").is_err());
        assert!(parse_agent_slash_query("reviewer   ").is_err());
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
        assert_eq!(parse_direct_custom_slash("/review"), Some(("review", "")));
        assert_eq!(parse_direct_custom_slash("review"), None);
    }
}
