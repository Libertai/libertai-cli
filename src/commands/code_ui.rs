//! `libertai code` rendering layer.
//!
//! After the ratatui migration, the interactive REPL lives in
//! `code_tui::app`. This module retains the one-shot rendering path
//! (`TurnRenderer`, `Spinner`, `ChromeStream`) used by `code.rs`, the
//! background-agent management functions, and the shared slash-command
//! helpers (parsing + pure formatting) that both the TUI and the one-shot
//! path reach. The legacy `repl_loop` print/parse surface that used to live
//! here has been removed; only genuinely-shared helpers remain.

//! press Esc to stop the turn. Full syntax highlighting remains out of
//! scope.
//!
//! Multi-line input: bracketed paste (`ESC[?2004h`) is enabled while the
//! bar is active, so a pasted stack trace arrives as one `Event::Paste`
//! — newlines included — and lands in the buffer as a single edit
//! instead of submitting line by line. Alt+Enter / Ctrl+J (and
//! Shift+Enter on terminals that report it, e.g. the kitty keyboard
//! protocol) insert a deliberate newline. In the ratatui TUI the input
//! bar is a `tui-textarea` widget whose footer row grows one row per
//! draft line up to `MAX_INPUT_ROWS` (6); past that the textarea's own
//! viewport scrolls to keep the cursor visible. The row sizing lives in
//! `code_tui::view::compute_footer_layout`, which floors the input row at
//! 1 even under extreme height pressure so a multi-line draft is never
//! clipped away.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use crossterm::terminal;

use pi::model::{AssistantMessageEvent, ContentBlock, Message, StopReason, Usage};
use pi::sdk::{AgentEvent, AgentSessionHandle};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::commands::chat_render::MarkdownStream;
use crate::commands::code_approvals::ApprovalState;
use crate::commands::code_factory::{is_path_edit_tool, Mode, ModeFlag};
use crate::config::Config as LibertaiConfig;

/// ANSI dim/bold helpers for cooked output (agent streaming phase).
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
/// Brand accent for the welcome header — matches the cyan `❯` prompt
/// chevron the input bar paints via crossterm.
const CYAN: &str = "\x1b[36m";

const SHELL_ESCAPE_MAX_DISPLAY_BYTES: usize = 256 * 1024;
pub(crate) const OSC52_MAX_TEXT_BYTES: usize = 128 * 1024;
pub(crate) const MENTION_ATTACHMENT_MAX_BYTES: usize = 256 * 1024;
pub(crate) const TREE_MAX_ENTRIES: usize = 200;
pub(crate) const CHANGELOG_DEFAULT_LIMIT: usize = 10;
pub(crate) const CHANGELOG_MAX_LIMIT: usize = 50;
const STATUS_LINE_TEMPLATE_MAX_CHARS: usize = 240;
const STATUS_LINE_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const STATUS_LINE_COMMAND_CACHE_TTL: Duration = Duration::from_secs(5);

/// Snapshot of the last completed turn's token usage. Written in
/// `repl_loop` after each successful prompt, read in `repaint()` to
/// render the context-usage strip on the rule line.
// pub(crate): the TUI (code_tui) builds one to feed expand_status_line_template.
#[derive(Default, Clone)]
pub(crate) struct BarStatus {
    pub(crate) model_label: String,
    pub(crate) input_tokens: u64,
    pub(crate) context_window: u32,
    pub(crate) output_style: Option<String>,
    pub(crate) status_line_template: String,
    pub(crate) status_line_command: String,
    /// Estimated session cost so far (pricing-table lookup over this
    /// session's usage records). `None` before the first turn or when
    /// the model has no pricing entry.
    pub(crate) estimated_cost: Option<f64>,
}

#[derive(Clone)]
struct StatusLineCommandCache {
    key: String,
    value: String,
    error: String,
    ts: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageRecord {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) context_window: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageSummary {
    pub(crate) turns: usize,
    pub(crate) last_input: u64,
    pub(crate) last_output: u64,
    pub(crate) output_total: u64,
    pub(crate) context_high_water: u64,
    pub(crate) context_window: u32,
    pub(crate) provider: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrCommentDraft {
    pub(crate) path: String,
    pub(crate) line: u64,
    pub(crate) body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyCommand {
    Status,
    Json,
    On,
    Off,
    Test,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpCommand {
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
pub(crate) enum VimCommand {
    Status,
    Json,
    Enable,
    Disable,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdeCommand {
    Status,
    Json,
    Open,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BugCommand {
    Template,
    Json,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HotkeysCommand {
    Show,
    Json,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThemeCommand {
    Status,
    Json,
    Requested(String),
}

/// Process-global so the terminal render loop and the vim-mode toggle
/// can reach this state across `run_interactive` invocations without a
/// reference chain.
///
/// **Caveat for tests / library reuse:** `run_interactive` assumes it
/// is the sole owner of this process's terminal for its lifetime.
/// `BAR_STATUS`, `STATUS_LINE_COMMAND_CACHE`, and `VIM_INPUT_ENABLED`
/// are process globals shared across invocations, so calling
/// `run_interactive` twice in the same process (e.g. from an
/// integration test) would carry state from one run into the next.
/// If we ever need that, add a per-invocation reset step and document
/// the invariant more loudly.
static BAR_STATUS: Mutex<Option<BarStatus>> = Mutex::new(None);
static STATUS_LINE_COMMAND_CACHE: OnceLock<Mutex<Option<StatusLineCommandCache>>> = OnceLock::new();
static VIM_INPUT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Read the process-global vim-input flag. Shared with the ratatui TUI input
/// layer, so the `/vim` slash command status reflects the live state without
/// exposing the `AtomicBool` itself.
pub(crate) fn vim_input_enabled() -> bool {
    VIM_INPUT_ENABLED.load(Ordering::SeqCst)
}

/// Store the process-global vim-input flag from the UI thread. Used by the
/// ratatui `/vim on`/`/vim off` slash arms. `Relaxed` mirrors the legacy
/// `print_vim_status` store ordering — the input layer reads it on its own
/// polling cadence and does not require acquire/release synchronization.
pub(crate) fn set_vim_input_enabled(enabled: bool) {
    VIM_INPUT_ENABLED.store(enabled, Ordering::Relaxed);
}

fn rule_chip(cols: usize, mode: Mode) -> String {
    let status = BAR_STATUS.lock().ok().and_then(|g| g.clone());
    let inner = match status {
        Some(s) => {
            let text = status_line_command_text(&s.status_line_command)
                .or_else(|| expand_status_line_template(&s.status_line_template, &s, mode))
                .unwrap_or_else(|| default_rule_text(&s, mode));
            let budget = cols.saturating_sub(4);
            format!(
                " {} ",
                clip_chars(&sanitize_terminal_preview_text(&text), budget)
            )
        }
        None => String::new(),
    };
    // Pad with ─ so the whole line fills the terminal width.
    let chip_len = rich_rust::cells::cell_len(&inner);
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

pub(crate) fn status_line_command_text(command: &str) -> Option<String> {
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

/// Default status-line text when no template/command is configured:
/// `model · mode · context-used% (used/cap)` plus an estimated session
/// cost when the pricing table knows the model. Mode is included so a
/// Shift+Tab toggle is visible in the bar, not just in the prompt chip.
fn default_rule_text(status: &BarStatus, mode: Mode) -> String {
    let mut text = format!("{} · {}", status.model_label, mode_label(mode));
    if status.context_window > 0 {
        let pct = context_percent(status.input_tokens, status.context_window);
        text.push_str(&format!(
            " · {pct}% ctx ({} / {})",
            human_tokens(status.input_tokens),
            human_tokens(u64::from(status.context_window)),
        ));
    }
    if let Some(cost) = status.estimated_cost.filter(|cost| *cost > 0.0) {
        text.push_str(&format!(" · ~{}", dollar(cost)));
    }
    text
}

/// Context-window occupancy for a turn: prompt tokens plus cache
/// reads/writes. Providers that report cached tokens separately (the
/// Anthropic-style usage shape) exclude them from `usage.input`, so
/// `input` alone undercounts what actually sits in the window — that
/// skew is exactly how the status bar once showed a percentage that
/// didn't match its own parenthetical. Every ctx surface (status bar
/// pct, status bar tokens, stop line "in") must use this one number.
pub(crate) fn context_tokens(usage: &Usage) -> u64 {
    usage.input + usage.cache_read + usage.cache_write
}

pub(crate) fn context_percent(input_tokens: u64, context_window: u32) -> u32 {
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

/// Last-resort context-window for the status chip when neither pi's
/// models.json nor the model catalog knows the model.
const FALLBACK_CONTEXT_WINDOW: u32 = 32_768;

/// Context-window used by the status chip, resolved in order:
///
/// 1. `contextWindow` from pi's `<global_dir>/models.json` — this is
///    what pi actually runs with, respects user overrides, and is
///    enriched with real catalog values at startup by
///    `ensure_libertai_registered`. The session's `provider` entry is
///    consulted first (many setups carry the same model id under
///    several providers with stale per-backend values), then every
///    other provider's `models[]`;
/// 2. the LibertAI model catalog (`model_catalog::context_window_for`);
/// 3. the legacy 32k fallback.
///
/// Memoized per provider/model — call sites fire on session start,
/// model swaps, and every turn end, and the memo key makes `/model`
/// switches pick up the new model's window. Unit tests always get the
/// fallback (same hermeticity argument as `catalog_token_rates`: the
/// dev machine's models.json / catalog cache must not leak into
/// assertions).
pub(crate) fn context_window_for(provider: &str, model: &str) -> u32 {
    if cfg!(test) {
        return FALLBACK_CONTEXT_WINDOW;
    }
    static MEMO: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{provider}/{model}");
    if let Ok(map) = memo.lock() {
        if let Some(window) = map.get(&key) {
            return *window;
        }
    }
    let resolved = pi_models_json_context_window(provider, model)
        .or_else(|| crate::commands::model_catalog::context_window_for(model))
        .unwrap_or(FALLBACK_CONTEXT_WINDOW);
    if let Ok(mut map) = memo.lock() {
        map.insert(key, resolved);
    }
    resolved
}

/// `contextWindow` for `provider/model` from pi's models.json, read via
/// pi's own path resolution (honors `$PI_CODING_AGENT_DIR`, defaults to
/// `~/.pi/agent`) — the same file `ensure_libertai_registered` writes.
fn pi_models_json_context_window(provider: &str, model: &str) -> Option<u32> {
    let global_dir = pi::config::Config::global_dir();
    let path = pi::models::default_models_path(&global_dir);
    let raw = std::fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
    models_json_context_window(&root, provider, model)
}

/// Pure lookup over a parsed models.json: find `model`'s positive
/// numeric `contextWindow`, preferring `provider`'s entry, then any
/// other provider carrying the id. Pure so tests can pin the resolution
/// behavior on a fixture without touching `$PI_CODING_AGENT_DIR`.
fn models_json_context_window(
    root: &serde_json::Value,
    provider: &str,
    model: &str,
) -> Option<u32> {
    let providers = root.get("providers")?.as_object()?;
    if let Some(window) = providers
        .get(provider)
        .and_then(|entry| provider_model_context_window(entry, model))
    {
        return Some(window);
    }
    providers
        .iter()
        .filter(|(name, _)| name.as_str() != provider)
        .find_map(|(_, entry)| provider_model_context_window(entry, model))
}

/// Positive `contextWindow` of `model` inside one provider's `models[]`.
fn provider_model_context_window(provider_entry: &serde_json::Value, model: &str) -> Option<u32> {
    let models = provider_entry.get("models")?.as_array()?;
    models
        .iter()
        .filter(|entry| entry.get("id").and_then(|v| v.as_str()) == Some(model))
        .find_map(|entry| {
            entry
                .get("contextWindow")
                .and_then(serde_json::Value::as_u64)
                .filter(|w| *w > 0)
                .map(|w| w.min(u64::from(u32::MAX)) as u32)
        })
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
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

fn normalize_help_command_arg(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForgetCommand {
    Status,
    Json,
    Usage,
}

pub(crate) fn forget_usage_text() -> &'static str {
    "/forget [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json]"
}

pub(crate) fn parse_forget_command(input: &str) -> ForgetCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => ForgetCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" | "info --json"
        | "preview --json" => ForgetCommand::Json,
        _ => ForgetCommand::Usage,
    }
}

pub(crate) fn forget_json_payload(approvals: &ApprovalState, query: &str) -> serde_json::Value {
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

pub(crate) fn hotkey_lines() -> &'static [&'static str] {
    &[
        "Shift+Tab — cycle normal / accept-edits / plan modes",
        "Up / Down — move between draft lines, then walk submitted prompt history",
        "Left / Right / Ctrl+B / Ctrl+F — move cursor in the current line",
        "Ctrl+Left / Ctrl+Right / Alt+B / Alt+F — move by word",
        "Backspace / Delete — edit the current line",
        "Ctrl+W / Alt+Backspace — delete the previous word",
        "Alt+D / Ctrl+Delete — delete the next word",
        "Home / End — jump to start or end of the current line",
        "Enter — submit the current prompt",
        "@ — at a word boundary, autocomplete a file to mention (its content attaches on submit)",
        "Alt+Enter / Ctrl+J — insert a newline (Shift+Enter on terminals that report it)",
        "Paste — bracketed paste inserts text, newlines included, without submitting",
        "Ctrl+C — clear the input line (quit when empty) or interrupt streaming",
        "Esc — stop the running turn from the mid-turn input row",
        "Ctrl+O — open the input in $VISUAL/$EDITOR (vi fallback)",
        "Ctrl+D — exit when the line is empty",
    ]
}

pub(crate) fn hotkeys_json_payload(query: &str) -> serde_json::Value {
    let shortcuts: Vec<serde_json::Value> = hotkey_lines()
        .iter()
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

pub(crate) fn tree_json_request_arg(input: &str) -> Option<String> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "json" | "--json" | "status --json" | "state --json" | "show --json" => Some(String::new()),
        _ if lower.starts_with("json ") => Some(raw[5..].trim().to_string()),
        _ if lower.starts_with("--json ") => Some(raw[7..].trim().to_string()),
        _ if lower.ends_with(" --json") => Some(raw[..raw.len() - 7].trim().to_string()),
        _ => None,
    }
}

pub(crate) fn tree_root(path: Option<&str>) -> Result<PathBuf> {
    let raw = path.unwrap_or("").trim();
    if raw.is_empty() {
        return std::env::current_dir().context("resolve current directory");
    }
    Ok(PathBuf::from(raw))
}

pub(crate) fn render_project_tree(root: &Path, max_entries: usize) -> Result<String> {
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
        out.push_str(&format!(
            "{DIM}... truncated after {max_entries} entries{RESET}\n"
        ));
    }
    Ok(out)
}

pub(crate) fn project_tree_json_payload(
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
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("read {}", path.display()))?;
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
        .and_then(|p| {
            if p.as_os_str().is_empty() {
                Some(".")
            } else {
                p.to_str()
            }
        })
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
                .then_with(|| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                })
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
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
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

pub(crate) fn parse_changelog_limit(input: &str) -> Result<usize> {
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

pub(crate) fn changelog_json_request_arg(input: &str) -> Option<String> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "json" | "--json" | "status --json" | "state --json" | "show --json" | "list --json"
        | "recent --json" | "latest --json" => Some(String::new()),
        _ => lower
            .strip_prefix("json ")
            .or_else(|| lower.strip_prefix("--json "))
            .map(str::trim)
            .map(str::to_string),
    }
}

pub(crate) fn changelog_json_payload(
    limit: usize,
    query: &str,
    lines: Vec<String>,
) -> serde_json::Value {
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

pub(crate) fn recent_git_commits_in(cwd: &Path, limit: usize) -> Result<Vec<String>> {
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

/// Run `git -C <cwd> diff --no-color HEAD [-- <path>]` and return the trimmed
/// stdout. Empty output means the tree is clean (no changes vs HEAD). Used by
/// the TUI `/diff` command (M7b): the bg thread shells out here (blocking) and
/// ships the raw diff string back as `AgentMsg::DiffReady`, where the in-TUI
/// viewer parses it into styled lines. Mirrors `git_status_short_in` /
/// `recent_git_commits_in` (same `git -C` + error-surfacing shape).
pub(crate) fn git_diff_in(cwd: &Path, path: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .arg("diff")
        .arg("--no-color")
        .arg("HEAD");
    if let Some(p) = path {
        cmd.arg("--").arg(p);
    }
    let output = cmd.output().context("run git diff")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("not a git repository");
        }
        anyhow::bail!("{}", message);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) async fn copy_messages(handle: &AgentSessionHandle) -> Result<Vec<Message>> {
    handle.messages().await.context("reading transcript")
}

pub(crate) fn last_assistant_text(messages: &[Message]) -> Option<String> {
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

pub(crate) fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", BASE64_STANDARD.encode(text.as_bytes()))
}

fn is_default_list_alias(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "status" | "state" | "show" | "list" | "recent" | "latest"
    )
}

pub(crate) fn mention_command_arg(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("/mention")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim_start)
}

pub(crate) fn build_mention_prompt(input: &str, output_style: Option<&str>) -> Result<String> {
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

/// Walk cap for [`mention_candidates`]: a huge repo stops enumerating here
/// so opening the @-mention popup stays a bounded one-shot cost.
pub(crate) const MENTION_WALK_MAX: usize = 5000;
/// Max `@path` mentions expanded per prompt by [`expand_at_mentions`] —
/// keeps a mention-heavy prompt from ballooning the context with
/// attachments.
pub(crate) const MENTION_EXPAND_MAX_FILES: usize = 8;

/// Enumerate candidate paths for the @-mention popup: a gitignore-aware
/// walk rooted at `cwd`, relative paths, dotfiles included but `.git`
/// skipped, directories marked with a trailing `/`. Capped at
/// [`MENTION_WALK_MAX`] entries and sorted shallow-first (depth, then
/// alpha) so top-level files rank above deep ones. Called once when the
/// popup opens — not per keystroke.
pub(crate) fn mention_candidates(cwd: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if out.len() >= MENTION_WALK_MAX {
            break;
        }
        let path = entry.path();
        if path == cwd {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let mut s = rel.to_string_lossy().into_owned();
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            s.push('/');
        }
        out.push(s);
    }
    out.sort_by_key(|p| (p.trim_end_matches('/').matches('/').count(), p.clone()));
    out
}

/// Expand inline `@path` mentions into appended file attachments.
///
/// The prompt text itself is left untouched (the transcript and the model
/// both still see the literal `@path` tokens); attachments are appended
/// after it, one fenced block per file, mirroring [`build_mention_prompt`]'s
/// shape. A token expands only when ALL of:
/// - it starts a whitespace-separated word (`@` at start-of-word),
/// - the path is relative, with no `..` components (the popup never inserts
///   either form; an absolute or parent path stays literal — `/mention` is
///   the explicit escape hatch for those),
/// - it resolves to an existing regular file under `cwd`,
/// - the file is UTF-8 and within [`MENTION_ATTACHMENT_MAX_BYTES`].
///
/// A word whose raw form doesn't resolve is retried with trailing
/// punctuation stripped (`@src/main.rs,` → `src/main.rs`). Duplicates
/// attach once; at most [`MENTION_EXPAND_MAX_FILES`] files attach.
pub(crate) fn expand_at_mentions(prompt: &str, cwd: &Path) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut attachments: Vec<String> = Vec::new();
    for word in prompt.split_whitespace() {
        if attachments.len() >= MENTION_EXPAND_MAX_FILES {
            break;
        }
        let Some(raw) = word.strip_prefix('@') else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        // Raw form first; on a miss retry with trailing prose punctuation
        // stripped so "see @src/main.rs." still resolves.
        let trimmed = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '`', '\'', '"']);
        let candidate = [raw, trimmed]
            .into_iter()
            .find(|c| !c.is_empty() && mention_expandable(c, cwd));
        let Some(rel) = candidate else {
            continue;
        };
        if !seen.insert(rel.to_string()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(cwd.join(rel)) else {
            continue;
        };
        if bytes.len() > MENTION_ATTACHMENT_MAX_BYTES {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        attachments.push(format!("Mentioned file: `{rel}`\n\n```text\n{text}\n```"));
    }
    if attachments.is_empty() {
        return prompt.to_string();
    }
    format!("{prompt}\n\n{}", attachments.join("\n\n"))
}

/// A mention token is expandable iff it's a relative path without `..`
/// components that resolves to an existing regular file under `cwd`.
fn mention_expandable(rel: &str, cwd: &Path) -> bool {
    let path = Path::new(rel);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    cwd.join(path).is_file()
}

fn normalize_status_line_template(value: &str) -> String {
    value
        .trim()
        .chars()
        .take(STATUS_LINE_TEMPLATE_MAX_CHARS)
        .collect()
}

pub(crate) fn expand_status_line_template(
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
        format!(
            "{}%",
            context_percent(status.input_tokens, status.context_window)
        )
    } else {
        "-".to_string()
    };
    let output_style = status.output_style.as_deref().unwrap_or("default");
    let cost = status
        .estimated_cost
        .filter(|cost| *cost > 0.0)
        .map(|cost| format!("~{}", dollar(cost)))
        .unwrap_or_else(|| "-".to_string());

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
        "cost" => Some(cost.clone()),
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
        for next in chars.by_ref() {
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

pub(crate) fn model_list_source(cfg: &LibertaiConfig) -> String {
    format!("{}/v1/models", cfg.api_base.trim_end_matches('/'))
}

pub(crate) fn model_list_provider() -> &'static str {
    "libertai"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactPreviewCommand {
    Status,
    Json,
    Usage,
}

pub(crate) fn compact_usage_text() -> &'static str {
    "/compact [status|state|show|info|preview|json|--json|status --json|state --json|show --json|info --json|preview --json|notes]"
}

pub(crate) fn compact_preview_arg(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("/compact ")?.trim();
    match normalize_help_command_arg(rest).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" | "json" | "--json"
        | "status --json" | "state --json" | "show --json" | "info --json" | "preview --json"
        | "help" | "usage" => Some(rest),
        _ => None,
    }
}

pub(crate) fn parse_compact_preview_command(input: &str) -> CompactPreviewCommand {
    match normalize_help_command_arg(input).as_str() {
        "" | "status" | "state" | "show" | "info" | "preview" => CompactPreviewCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" | "info --json"
        | "preview --json" => CompactPreviewCommand::Json,
        _ => CompactPreviewCommand::Usage,
    }
}

pub(crate) fn compact_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
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

pub(crate) fn compact_command_notes(trimmed: &str) -> Option<&str> {
    trimmed.strip_prefix("/compact ").map(str::trim)
}

pub(crate) fn parse_theme_command(rest: &str) -> ThemeCommand {
    let requested = rest.trim();
    match requested.to_ascii_lowercase().as_str() {
        "" | "status" | "show" | "current" | "info" => ThemeCommand::Status,
        "json" | "--json" | "status --json" | "show --json" | "current --json" | "info --json" => {
            ThemeCommand::Json
        }
        _ => ThemeCommand::Requested(requested.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HooksCommand {
    Status,
    Json,
    Open,
    Show(String),
    Usage,
}

pub(crate) fn parse_hooks_command(input: &str) -> HooksCommand {
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

pub(crate) fn parse_mcp_command(input: &str) -> McpCommand {
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
        "" | "status" | "list" | "state" | "diagnostics" | "diag" | "show" => McpCommand::Status,
        "probe" | "probes" => McpCommand::Probe,
        "refresh" | "probe --save" | "probe save" | "probe --write" | "probe write" => {
            McpCommand::ProbeSave
        }
        "reset" | "reset-sessions" => McpCommand::Reset,
        "open" | "settings" | "edit" => McpCommand::Open,
        _ => McpCommand::Usage,
    }
}

pub(crate) fn parse_vim_command(input: &str) -> VimCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" | "current" | "info" => VimCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" | "current --json"
        | "info --json" => VimCommand::Json,
        "on" | "enable" | "enabled" | "true" => VimCommand::Enable,
        "off" | "disable" | "disabled" | "false" => VimCommand::Disable,
        _ => VimCommand::Usage,
    }
}

pub(crate) fn parse_ide_command(input: &str) -> IdeCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" => IdeCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" => IdeCommand::Json,
        "open" | "settings" | "edit" => IdeCommand::Open,
        _ => IdeCommand::Usage,
    }
}

pub(crate) fn parse_bug_command(input: &str) -> BugCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "report" | "template" | "status" | "show" => BugCommand::Template,
        "json" | "--json" | "status --json" | "show --json" | "template --json"
        | "report --json" => BugCommand::Json,
        _ => BugCommand::Usage,
    }
}

pub(crate) fn parse_hotkeys_command(input: &str) -> HotkeysCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "show" | "list" | "help" => HotkeysCommand::Show,
        "json" | "--json" | "status --json" | "show --json" | "list --json" => HotkeysCommand::Json,
        _ => HotkeysCommand::Usage,
    }
}

pub(crate) fn hotkeys_usage_text() -> &'static str {
    "/hotkeys [status|show|list|help|json|--json|status --json|show --json|list --json]"
}

pub(crate) fn parse_notify_command(input: &str) -> NotifyCommand {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "status" | "state" | "show" => NotifyCommand::Status,
        "json" | "--json" | "status --json" | "state --json" | "show --json" => NotifyCommand::Json,
        "on" | "enable" | "enabled" => NotifyCommand::On,
        "off" | "disable" | "disabled" | "clear" => NotifyCommand::Off,
        "test" | "ping" => NotifyCommand::Test,
        _ => NotifyCommand::Usage,
    }
}

pub(crate) fn set_turn_notifications(cfg: &mut Arc<LibertaiConfig>, enabled: bool) -> Result<()> {
    let mut next = cfg.as_ref().clone();
    next.code_turn_notifications = enabled;
    crate::config::save(&next).context("save config")?;
    *cfg = Arc::new(next);
    Ok(())
}

pub(crate) fn notify_usage_text() -> &'static str {
    "/notify [on|enable|enabled|off|disable|disabled|clear|status|state|show|json|--json|status --json|state --json|show --json|test|ping]"
}

pub(crate) fn notify_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
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

pub(crate) fn theme_json_payload(query: &str) -> serde_json::Value {
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

pub(crate) const VIM_USAGE: &str =
    "/vim [status|state|show|current|info|json|--json|status --json|state --json|show --json|current --json|info --json|on|enable|enabled|true|off|disable|disabled|false]";
pub(crate) const IDE_USAGE: &str =
    "/ide [status|state|show|json|--json|status --json|state --json|show --json|open|settings|edit]";
pub(crate) const BUG_USAGE: &str =
    "/bug [report|template|status|show|json|--json|status --json|show --json|template --json|report --json]";

pub(crate) fn vim_json_payload(query: &str) -> serde_json::Value {
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

pub(crate) fn ide_json_payload(query: &str) -> serde_json::Value {
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

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_millis_u64)
        .unwrap_or(0)
}

pub(crate) fn review_prompt(security: bool, scope: &str) -> String {
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

pub(crate) fn stage_pr_comment_draft(
    input: &str,
    drafts: &mut Vec<PrCommentDraft>,
) -> Result<PrCommentDraft> {
    let draft = parse_pr_comment_draft(input)?;
    drafts.push(draft.clone());
    Ok(draft)
}

fn custom_slash_invocation_name(
    cmd: &crate::commands::code_slash_registry::CustomCommand,
) -> String {
    cmd.namespace
        .as_deref()
        .filter(|namespace| !namespace.trim().is_empty())
        .map(|namespace| format!("{namespace}/{}", cmd.name))
        .unwrap_or_else(|| cmd.name.clone())
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CustomSlashResolve<'a> {
    Hit(&'a crate::commands::code_slash_registry::CustomCommand),
    NotFound,
    Ambiguous(Vec<String>),
}

pub(crate) fn resolve_custom_slash<'a>(
    commands: &'a [crate::commands::code_slash_registry::CustomCommand],
    name: &str,
) -> CustomSlashResolve<'a> {
    let needle = name.trim().trim_start_matches('/').to_ascii_lowercase();
    if needle.is_empty() {
        return CustomSlashResolve::NotFound;
    }

    let exact_invocation: Vec<_> = commands
        .iter()
        .filter(|cmd| custom_slash_invocation_name(cmd).eq_ignore_ascii_case(&needle))
        .collect();
    if let Some(hit) = unique_custom_slash_match(exact_invocation) {
        return hit;
    }

    let exact_name: Vec<_> = commands.iter().filter(|cmd| cmd.name == needle).collect();
    if let Some(hit) = unique_custom_slash_match(exact_name) {
        return hit;
    }

    let prefix: Vec<_> = commands
        .iter()
        .filter(|cmd| custom_slash_starts_with(cmd, &needle))
        .collect();
    unique_custom_slash_match(prefix).unwrap_or(CustomSlashResolve::NotFound)
}

fn unique_custom_slash_match<'a>(
    matches: Vec<&'a crate::commands::code_slash_registry::CustomCommand>,
) -> Option<CustomSlashResolve<'a>> {
    match matches.as_slice() {
        [] => None,
        [hit] => Some(CustomSlashResolve::Hit(hit)),
        _ => {
            let mut names: Vec<String> = matches
                .into_iter()
                .map(custom_slash_invocation_name)
                .collect();
            names.sort();
            names.dedup();
            Some(CustomSlashResolve::Ambiguous(names))
        }
    }
}

pub(crate) async fn build_custom_slash_prompt(
    name: &str,
    args: &str,
    handle: &AgentSessionHandle,
) -> Result<Option<String>> {
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let templates = crate::commands::code_slash_registry::discover(&cwd);
    let hit = match resolve_custom_slash(&templates, name) {
        CustomSlashResolve::Hit(hit) => hit,
        CustomSlashResolve::NotFound => return Ok(None),
        CustomSlashResolve::Ambiguous(names) => {
            anyhow::bail!(
                "ambiguous custom slash `{}`; use one of: {}",
                name.trim(),
                names.join(", ")
            );
        }
    };
    let context = slash_expansion_context(handle).await;
    Ok(Some(
        crate::commands::code_slash_registry::expand_with_context(hit, args, &context),
    ))
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
pub(crate) struct BackgroundAgentLaunch {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub mode: Mode,
    pub prompt: String,
    pub cwd: PathBuf,
    /// Optional sub-agent to run the session as. Emitted as
    /// `--agent <name>` on the spawned `libertai code` argv.
    pub agent: Option<String>,
    /// Team name when this run is a teammate in a team. Emitted as
    /// `LIBERTAI_TEAM=<name>` env var on the child process so the
    /// child's factory registers the `team_task` tool.
    pub team: Option<String>,
    /// Teammate name within the team. Emitted as
    /// `LIBERTAI_TEAMMATE=<name>` env var.
    pub teammate_name: Option<String>,
    /// (Issue-1) Path to the parent TUI's approval socket, emitted as
    /// `LIBERTAI_APPROVAL_SOCKET` so the teammate routes its approvals to the
    /// parent instead of auto-denying. `None` for spawns outside a TUI (the
    /// child falls back to `PrintModeApprovalUi` auto-deny).
    pub approval_socket_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartedBackgroundAgent {
    pub pid: u32,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackgroundAgentRecord {
    pub pid: u32,
    #[serde(default)]
    pub run_id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub mode: String,
    pub prompt_preview: String,
    pub cwd: String,
    pub log_path: String,
    pub started_at_ms: u64,
    #[serde(default)]
    pub launched_argv: Vec<String>,
    /// Team name when this run is a teammate. None for plain background
    /// runs. Used by the agent view to show a mail badge.
    #[serde(default)]
    pub team: Option<String>,
    /// Teammate name within the team. None for plain background runs.
    #[serde(default)]
    pub teammate_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundAgentStatus {
    Running,
    Exited,
    Unknown,
}

pub(crate) fn start_background_agent(
    launch: &BackgroundAgentLaunch,
) -> Result<StartedBackgroundAgent> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let log_path = background_agent_log_path(&launch.name)?;
    if let Some(parent) = log_path.parent() {
        crate::config::create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        crate::config::tighten_dir_mode_700(parent)
            .with_context(|| format!("tightening {}", parent.display()))?;
    }
    let log = crate::config::open_append_secure(&log_path)
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
    // Pass team context to the child via env vars so the child's
    // factory registers the `team_task` tool.
    if let Some(team) = launch.team.as_ref() {
        command.env("LIBERTAI_TEAM", team);
    }
    if let Some(teammate) = launch.teammate_name.as_ref() {
        command.env("LIBERTAI_TEAMMATE", teammate);
    }
    // (Issue-1) Pass the parent TUI's approval-socket path so the teammate
    // routes its approvals back here instead of auto-denying. Absent for
    // spawns outside a TUI (the child falls back to PrintModeApprovalUi).
    if let Some(path) = launch.approval_socket_path.as_ref() {
        command.env(
            crate::commands::code_approval_ipc::APPROVAL_SOCKET_ENV,
            path,
        );
    }
    command
}

fn background_agent_args(exe: &Path, launch: &BackgroundAgentLaunch) -> Vec<String> {
    let mut args = Vec::new();
    if !is_lcode_executable(exe) {
        args.push("code".to_string());
    }
    // Background children have no TTY (stdin is null, stdout/stderr are
    // log files). Without --print they'd pick TerminalApprovalUi and
    // hang forever on the first un-approved tool call.
    args.push("--print".to_string());
    if !launch.provider.trim().is_empty() {
        args.push("--provider".to_string());
        args.push(launch.provider.clone());
    }
    if !launch.model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(launch.model.clone());
    }
    if launch.mode == Mode::Bypass {
        // Bypass is gated by a one-time consent sentinel; emit the flag
        // (not `--mode bypass`) so the child re-runs the consent gate,
        // which reads the already-granted sentinel. `--mode bypass` is
        // deliberately NOT accepted by `parse_initial_mode` — that would
        // bypass the gate from any caller.
        args.push("--dangerously-skip-permissions".to_string());
    } else if launch.mode != Mode::Normal {
        args.push("--mode".to_string());
        args.push(mode_label(launch.mode).to_string());
    }
    if let Some(agent) = launch.agent.as_ref() {
        if !agent.trim().is_empty() {
            args.push("--agent".to_string());
            args.push(agent.clone());
        }
    }
    args.push(launch.prompt.clone());
    args
}

fn is_lcode_executable(exe: &Path) -> bool {
    exe.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "lcode")
}

pub(crate) fn background_agent_log_path(name: &str) -> Result<PathBuf> {
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
            Mode::Bypass => "bypass",
        }
        .to_string(),
        prompt_preview: preview_text(&launch.prompt, 160),
        cwd: launch.cwd.display().to_string(),
        log_path: started.log_path.display().to_string(),
        started_at_ms,
        launched_argv,
        team: launch.team.clone(),
        teammate_name: launch.teammate_name.clone(),
    }
}

pub(crate) fn background_agent_run_id(pid: u32, started_at_ms: u64) -> String {
    format!("bg-{started_at_ms}-{pid}")
}

pub(crate) fn background_agent_record_id(record: &BackgroundAgentRecord) -> String {
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
        crate::config::create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        crate::config::tighten_dir_mode_700(parent)
            .with_context(|| format!("tightening {}", parent.display()))?;
    }
    let mut file = crate::config::open_append_secure(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("writing {}", path.display()))?;
    writeln!(file).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub(crate) fn rewrite_background_agent_records(records: &[BackgroundAgentRecord]) -> Result<()> {
    let path = background_agent_records_path()?;
    if let Some(parent) = path.parent() {
        crate::config::create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        crate::config::tighten_dir_mode_700(parent)
            .with_context(|| format!("tightening {}", parent.display()))?;
    }
    if records.is_empty() {
        if path.exists() {
            crate::config::write_file_secure(&path, b"")
                .with_context(|| format!("writing {}", path.display()))?;
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
    crate::config::write_file_secure(&path, raw.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub(crate) fn load_background_agent_records() -> Result<Vec<BackgroundAgentRecord>> {
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

pub(crate) fn retain_running_background_agent_records(
    records: Vec<BackgroundAgentRecord>,
    status: impl Fn(u32) -> BackgroundAgentStatus,
) -> Vec<BackgroundAgentRecord> {
    records
        .into_iter()
        .filter(|record| matches!(status(record.pid), BackgroundAgentStatus::Running))
        .collect()
}

pub(crate) fn read_log_tail(path: &Path, max_bytes: usize) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    // (R4-LOG-2) Seek-based tail read: open the file, stat its length, seek to
    // (len - max_bytes).max(0), and read ONLY the remainder into a bounded
    // buffer. This avoids reading the whole (potentially multi-MB) file on
    // every call — the prior `fs::read(path)` read the entire file into a
    // `Vec<u8>` then sliced the tail, which re-allocated the whole file on
    // every 80ms redraw tick while an agent streamed. The seek read is O(tail)
    // in both bytes read and allocation.
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len() as usize;
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start as u64))
        .with_context(|| format!("seeking {}", path.display()))?;
    let want = len - start;
    let mut bytes = Vec::with_capacity(want.min(1 << 16));
    let mut buf = [0u8; 8192];
    let mut read = 0usize;
    while read < want {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let take = n.min(want - read);
        bytes.extend_from_slice(&buf[..take]);
        read += take;
        if take < n {
            break;
        }
    }

    let mut text = String::from_utf8_lossy(&bytes).to_string();
    // When we started mid-file (file larger than max_bytes), the first line of
    // the tail is a partial line (we landed mid-line). Drop it so the tail
    // begins at a line boundary — cleaner for line-based consumers and matches
    // the task's spec. The "[truncated ...]" prefix below marks the drop.
    if start > 0 {
        if let Some(idx) = text.find('\n') {
            text.drain(..=idx);
        } else {
            // The whole tail is one partial line (tiny max_bytes). Clear it so
            // we don't surface a fragment.
            text.clear();
        }
        text.insert_str(0, &format!("[truncated to last {max_bytes} bytes]\n"));
    }
    Ok(text)
}

pub(crate) fn background_agent_status(pid: u32) -> BackgroundAgentStatus {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => BackgroundAgentStatus::Running,
            Ok(_) => BackgroundAgentStatus::Exited,
            Err(_) => BackgroundAgentStatus::Unknown,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        BackgroundAgentStatus::Unknown
    }
}

pub(crate) fn send_background_agent_kill(pid: u32) -> Result<()> {
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

pub(crate) fn preview_text(text: &str, max_chars: usize) -> String {
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

/// Resolve the user's preferred external editor: `$VISUAL`, then `$EDITOR`,
/// then `vi`. Extracted from `open_memory_editor` so the ratatui TUI's
/// Ctrl+O external-editor flow (`code_tui::app`) can reuse the same
/// resolution order without duplicating the env-var fallback chain.
pub(crate) fn resolve_editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string())
}

/// Quote a filesystem path for safe interpolation into a POSIX `sh -c` command
/// line (single-quote wrapping with `'` → `'\''` escaping). Promoted to
/// `pub(crate)` so the TUI external-editor flow can quote the temp-file path.
pub(crate) fn quote_for_sh(path: &Path) -> String {
    quote_sh_string(path.to_string_lossy().as_ref())
}

/// Quote an arbitrary string for a POSIX `sh -c` command line. Promoted to
/// `pub(crate)` for reuse by the TUI external-editor flow + the server-args
/// path below.
pub(crate) fn quote_sh_string(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellEscapeResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellEscapeAction {
    Run(String),
    Usage(&'static str),
}

pub(crate) fn shell_escape_command(rest: &str, last: Option<&str>) -> ShellEscapeAction {
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

pub(crate) fn execute_shell_escape(
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

pub(crate) fn shell_escape_prompt_context(command: &str, result: &ShellEscapeResult) -> String {
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

pub(crate) fn apply_pending_shell_context(contexts: &[String], prompt: &str) -> String {
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

pub(crate) fn usage_summary(records: &[UsageRecord]) -> Option<UsageSummary> {
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

fn dollar(value: f64) -> String {
    format!("${:.2}", value.max(0.0))
}

pub(crate) const HOOKS_USAGE: &str =
    "/hooks [status|list|state|diagnostics|diag|json|--json|status --json|list --json|state --json|diagnostics --json|diag --json|show --json|show|event|inspect <event>|open|settings|edit]";
pub(crate) const MCP_USAGE: &str = "/mcp [status|list|state|show|json|--json|status --json|list --json|state --json|diagnostics --json|diag --json|show --json|server|inspect <server>|probe|probes|probe --save|probe save|probe --write|probe write|refresh|diagnostics|diag|reset|reset-sessions|open|settings|edit]";

pub(crate) fn hook_event_rows(
    cfg: &LibertaiConfig,
) -> [(&'static str, &[crate::config::HookCommandConfig]); 8] {
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

fn hook_json_row(
    event: &str,
    index: usize,
    hook: &crate::config::HookCommandConfig,
) -> serde_json::Value {
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

pub(crate) fn hooks_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
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

pub(crate) fn hooks_for_event<'a>(
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

fn mcp_server_json_row(name: &str, server: &crate::config::McpServerConfig) -> serde_json::Value {
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

pub(crate) fn mcp_json_payload(cfg: &LibertaiConfig, query: &str) -> serde_json::Value {
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

pub(crate) fn format_mcp_server_details(
    name: &str,
    server: &crate::config::McpServerConfig,
) -> String {
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
        out.push_str(&format!(
            "  - [{}] {}{} - {}\n",
            marker, label, mime, resource.uri
        ));
    }
    if resources.len() > 12 {
        out.push_str(&format!(
            "  - ... {} more resource(s)\n",
            resources.len() - 12
        ));
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
        out.push_str(&format!(
            "  - [{}] {}{}{}\n",
            marker, prompt.name, desc, args
        ));
    }
    if prompts.len() > 12 {
        out.push_str(&format!("  - ... {} more prompt(s)\n", prompts.len() - 12));
    }
    out.push('\n');
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct McpExposureSummary {
    pub(crate) mcp_call: bool,
    pub(crate) named_tools: usize,
    pub(crate) resource_reader: bool,
    pub(crate) prompt_getter: bool,
    pub(crate) subscription_candidates: usize,
}

pub(crate) fn mcp_exposure_summary(cfg: &LibertaiConfig) -> McpExposureSummary {
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

fn normalized_hook_type(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "mcp-tool" | "mcptool" => "mcp_tool".to_string(),
        other => other.to_string(),
    }
}

/// Non-printing twin of [`print_hook_section`]: builds the same per-event
/// hook listing text but returns it instead of printing. Reused by the
/// ratatui `/hooks` status adapter (which renders the string into the
/// transcript).
pub(crate) fn hook_section_text(event: &str, hooks: &[crate::config::HookCommandConfig]) -> String {
    if hooks.is_empty() {
        return format!("  no {event} hooks configured\n");
    }
    let mut out = String::new();
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
        // Build the per-hook line incrementally (the legacy `print_hook_section`
        // formats it in one shot with `{DIM}`/`{RESET}` ANSI wrappers; here we
        // emit plain text, so assemble the same fields in order without ANSI).
        let mut line = String::new();
        line.push_str(&format!(
            "  {}. {} [{}] type={} matcher={}",
            idx + 1,
            event,
            marker,
            hook_type,
            matcher
        ));
        line.push_str(&timeout);
        line.push_str(&shell);
        line.push_str(async_flag);
        line.push_str(once_flag);
        line.push_str(async_rewake);
        line.push_str(&source);
        line.push_str(&status_message);
        line.push_str(&review_policy);
        line.push_str(&metadata);
        line.push_str(&if_condition);
        line.push_str(continue_on_block);
        line.push_str(&format!(": {target}\n"));
        out.push_str(&line);
    }
    out
}

pub(crate) fn format_hook_event_details(
    event: &str,
    hooks: &[crate::config::HookCommandConfig],
) -> String {
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
            out.push_str(&format!(
                "     statusMessage: {}\n",
                hook.status_message.trim()
            ));
        }
        if !hook.review_policy.trim().is_empty() {
            out.push_str(&format!(
                "     reviewPolicy: {}\n",
                hook.review_policy.trim()
            ));
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

fn apply_output_style(output_style: Option<&str>, prompt: &str) -> String {
    let cwd = std::env::current_dir().ok();
    crate::commands::code_output_style::apply_output_style(output_style, prompt, cwd.as_deref())
}

pub(crate) fn bug_json_payload(
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

pub(crate) fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::AcceptEdits => "accept-edits",
        Mode::Plan => "plan",
        Mode::Bypass => "bypass",
    }
}

fn compaction_end_text(aborted: bool, error_message: Option<&str>) -> String {
    if aborted {
        "compaction aborted".to_string()
    } else if let Some(message) = error_message {
        format!("compaction failed: {message}")
    } else {
        "compaction finished".to_string()
    }
}

// ---------------------------------------------------------------------------
// Turn renderer — markdown output, tool chrome, spinner
// ---------------------------------------------------------------------------

/// Max result lines previewed under a tool marker before "… +N lines".
const TOOL_RESULT_PREVIEW_LINES: usize = 4;
/// Spinner repaint cadence. Fast enough that the elapsed-seconds field
/// never looks stuck, slow enough to stay invisible in `top`.
const SPINNER_TICK: Duration = Duration::from_millis(120);
/// Fallback terminal size when the probe fails (e.g. tests, piped
/// stdout). Previously the footer and the input bar had separate
/// fallbacks (100 cols vs 80×24); now both use this single source.
const FALLBACK_TERM_SIZE: (u16, u16) = (100, 24);

/// Current terminal size (cols, rows). Single source of truth for all
/// render paths — the footer, the input bar, and the agent view all
/// ask crossterm the same question and get the same fallback.
fn terminal_size() -> (u16, u16) {
    terminal::size()
        .ok()
        .filter(|(c, r)| *c > 0 && *r > 0)
        .unwrap_or(FALLBACK_TERM_SIZE)
}

/// Current terminal width for footer/preview painting.
fn term_cols() -> usize {
    terminal_size().0 as usize
}

/// Where a [`TurnRenderer`] writes its chrome (markers, previews,
/// spinner). The one-shot `code` path sends chrome to stderr so a
/// piped stdout carries assistant text only; the TUI renders its own
/// chrome and never constructs a [`TurnRenderer`] against this stream.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChromeStream {
    Stderr,
}

impl ChromeStream {
    fn is_tty(self) -> bool {
        use std::io::IsTerminal;
        match self {
            Self::Stderr => io::stderr().is_terminal(),
        }
    }

    fn write_str(self, s: &str) {
        match self {
            Self::Stderr => {
                let mut err = io::stderr();
                let _ = err.write_all(s.as_bytes());
                let _ = err.flush();
            }
        }
    }
}

/// State shared between the event thread, the spinner ticker, and the
/// mid-turn input pump (which feeds the queued-message footer).
struct SpinnerCore {
    label: &'static str,
    started: Instant,
    output_chars: u64,
    /// Ticker may draw. Cleared while content or a tool is printing so
    /// the spinner never interleaves with real output. Also cleared by
    /// [`suspend_active_footer`] during interactive prompts so the
    /// ticker doesn't fight the prompt UI.
    visible: bool,
    /// Rows of the footer block currently painted on screen (spinner
    /// line + queued previews + live typing row). The eraser must clear
    /// exactly this many rows before content prints.
    drawn_rows: u16,
    /// Terminal height when the footer was last drawn, for detecting
    /// resize in sticky mode (requires re-emitting DECSTBM).
    last_term_height: u16,
    stopped: bool,
    stream: ChromeStream,
    styled: bool,
    /// When true (REPL), the footer is pinned to the bottom of the
    /// screen via a DECSTBM scroll region and absolute positioning.
    /// Content scrolls above the footer and the terminal's scrollback
    /// buffer captures it. When false (one-shot `code` command), the
    /// footer uses relative positioning (at the cursor) and never sets
    /// a scroll region.
    sticky: bool,
    /// Shared mode flag so the ticker thread can render the rule line
    /// (mode chip + model + context info) without plumbing the Arc
    /// through every draw call. `None` in the one-shot path.
    mode_flag: Option<ModeFlag>,
    /// Messages queued for the next turn (full texts; previews are
    /// recomputed per draw so a resize re-clips correctly).
    queued_texts: Vec<String>,
    /// The line being typed right now (mid-turn editor buffer).
    typed: String,
}

impl SpinnerCore {
    /// ANSI sequence clearing `rows` footer rows, assuming the cursor
    /// sits at the end of the bottom row. Leaves the cursor on the top
    /// row, column 0 — where content (or the next footer) paints.
    fn erase_seq(rows: u16) -> String {
        let mut seq = String::from("\r\x1b[2K");
        for _ in 1..rows {
            seq.push_str("\x1b[1A\x1b[2K");
        }
        seq
    }

    fn erase(&mut self) {
        if self.drawn_rows > 0 {
            if self.sticky {
                // Sticky: clear footer at absolute position and reset
                // scroll region so read_line / next output uses the
                // full screen.
                let (_, term_height) = terminal_size();
                let footer_top_1 = term_height
                    .saturating_sub(self.drawn_rows)
                    .saturating_add(1)
                    .max(1);
                let mut seq = String::from("\x1b[r\x1b7");
                seq.push_str(&format!("\x1b[{footer_top_1};1H\x1b[2K"));
                for _ in 1..self.drawn_rows {
                    seq.push_str("\x1b[1B\x1b[2K");
                }
                seq.push_str("\x1b8");
                self.stream.write_str(&seq);
            } else {
                self.stream.write_str(&Self::erase_seq(self.drawn_rows));
            }
            self.drawn_rows = 0;
            self.last_term_height = 0;
        }
    }

    /// Repaint the footer block in one terminal write: live agent
    /// panel (one row per active agent), spinner line, dim `› queued:`
    /// previews, and the live typing row.
    ///
    /// In sticky mode (REPL) the footer is pinned to the bottom of the
    /// screen via absolute positioning and a DECSTBM scroll region
    /// confines conversation output to the rows above. The terminal's
    /// scrollback buffer captures scrolled content.
    ///
    /// In non-sticky mode (one-shot `code` command) the footer uses
    /// relative positioning (at the cursor) and never sets a scroll
    /// region.
    fn draw(&mut self) {
        let width = term_cols();
        let (_, term_height) = terminal_size();

        let mut rows: Vec<String> = Vec::new();
        let agents = active_agents_for_footer();

        let max_agent_rows = ((term_height / 3) as usize).max(3);
        let agent_count = agents.len().min(max_agent_rows);

        if agent_count > 0 {
            let header = if agents.len() > agent_count {
                format!(
                    "── agents ({}) +{} more ",
                    agent_count,
                    agents.len() - agent_count
                )
            } else {
                format!("── agents ({}) ", agent_count)
            };
            let pad = width.saturating_sub(header.chars().count());
            let header_line = format!("{}{}", header, "─".repeat(pad));
            rows.push(if self.styled {
                format!("{DIM}{header_line}{RESET}")
            } else {
                header_line
            });
        }

        for handle in agents.iter().take(agent_count) {
            rows.push(agent_footer_line(handle, width));
        }

        let spinner = clip_chars(
            &format!(
                "{}{}",
                spinner_line_text(
                    self.label,
                    self.started.elapsed().as_secs(),
                    self.output_chars,
                ),
                queued_spinner_suffix(self.queued_texts.len()),
            ),
            width,
        );
        rows.push(if self.styled {
            format!("{DIM}{spinner}{RESET}")
        } else {
            spinner
        });
        for line in queued_preview_lines(&self.queued_texts, width) {
            rows.push(if self.styled {
                format!("{DIM}{line}{RESET}")
            } else {
                line
            });
        }
        // Rule line (mode chip + model + context info) — same line
        // read_line shows above ❯, so the status bar is visible
        // during turns too.
        if let Some(mf) = &self.mode_flag {
            let rule = rule_chip(width, mf.get());
            rows.push(if self.styled {
                format!("{DIM}{rule}{RESET}")
            } else {
                rule
            });
        }
        let typed = typed_preview_line(&self.typed, width);
        rows.push(if self.styled {
            format!("{BOLD}{typed}{RESET}")
        } else {
            typed
        });

        let max_rows = term_height.saturating_sub(1).max(1);
        let row_count: u16 = (rows.len() as u16).min(max_rows);
        if row_count == 0 {
            return;
        }
        rows.truncate(row_count as usize);

        if self.sticky {
            self.draw_sticky(rows, row_count, term_height);
        } else {
            self.draw_relative(rows, row_count);
        }
    }

    /// Sticky draw: absolute positioning + DECSTBM scroll region.
    fn draw_sticky(&mut self, rows: Vec<String>, row_count: u16, term_height: u16) {
        let scroll_bottom = term_height.saturating_sub(row_count);
        let footer_top_1 = scroll_bottom.saturating_add(1).min(term_height);

        let need_region_update =
            row_count != self.drawn_rows || term_height != self.last_term_height;
        let need_clear_stale = self.drawn_rows > 0
            && (self.drawn_rows > row_count || term_height != self.last_term_height);

        let mut seq = String::new();

        if need_clear_stale {
            let old_footer_top_1 = self
                .last_term_height
                .saturating_sub(self.drawn_rows)
                .saturating_add(1)
                .max(1);
            seq.push_str("\x1b7");
            seq.push_str(&format!("\x1b[{old_footer_top_1};1H\x1b[J\x1b8"));
        }

        if need_region_update {
            if scroll_bottom > 0 {
                seq.push_str(&format!("\x1b[1;{scroll_bottom}r"));
            } else {
                seq.push_str("\x1b[r");
            }
        }

        seq.push_str("\x1b7");
        seq.push_str(&format!("\x1b[{footer_top_1};1H\x1b[2K"));
        if let Some(first) = rows.first() {
            seq.push_str(first);
        }
        for line in rows.iter().skip(1) {
            seq.push_str(&format!("\x1b[1B\r\x1b[2K{line}"));
        }
        seq.push_str("\x1b8");

        self.stream.write_str(&seq);
        self.drawn_rows = row_count;
        self.last_term_height = term_height;
    }

    /// Non-sticky draw: relative-row (at the cursor), no scroll region.
    fn draw_relative(&mut self, rows: Vec<String>, row_count: u16) {
        let mut block = String::new();
        for (i, line) in rows.into_iter().enumerate() {
            if i == 0 {
                block.push_str(&line);
            } else {
                block.push_str(&format!("\r\n\x1b[2K{line}"));
            }
        }
        let seq = if self.drawn_rows > 0 {
            format!("{}{}", Self::erase_seq(self.drawn_rows), block)
        } else {
            format!("\r\x1b[2K{block}")
        };
        self.stream.write_str(&seq);
        self.drawn_rows = row_count;
    }
}

/// `✳ thinking… 12s` → `✳ writing… 23s · 1.2k tokens`. The token count
/// is the streamed output estimated at ~4 chars/token — pi only reports
/// exact usage at end of turn.
fn spinner_line_text(label: &str, elapsed_secs: u64, output_chars: u64) -> String {
    let mut line = format!("✳ {label} {}", human_elapsed(elapsed_secs));
    let tokens = output_chars / 4;
    if tokens > 0 {
        line.push_str(&format!(" · {} tokens", human_tokens(tokens)));
    }
    line
}

/// The spinner footer that currently owns the bottom of the terminal,
/// if any. Registered by [`Spinner::start`] and cleared by
/// [`Spinner::stop`] so an interactive prompt running on the runtime
/// thread (the approval micro-prompt and the `ask_user` chooser in
/// `code_term`) can erase and pause it before painting. Without this,
/// the ticker thread kept repainting over the menu and the agent looked
/// hung while it was really blocked waiting for an invisible keystroke.
static ACTIVE_SPINNER: Mutex<Option<Arc<Mutex<SpinnerCore>>>> = Mutex::new(None);

fn set_active_spinner(core: Option<Arc<Mutex<SpinnerCore>>>) {
    if let Ok(mut g) = ACTIVE_SPINNER.lock() {
        *g = core;
    }
}

/// Shared live-agent registry for the current REPL session. Set by
/// `repl_loop` at startup so the spinner footer (a background ticker
/// thread) can read the active-agent count and per-agent state without
/// plumbing the `Arc` through every renderer constructor. Mirrors the
/// `ACTIVE_SPINNER` pattern. `None` in headless `--print` mode and in
/// tests.
static ACTIVE_AGENT_REGISTRY: Mutex<Option<Arc<crate::commands::code_team::AgentRegistry>>> =
    Mutex::new(None);

/// Snapshot of the active agents for the footer panel. Returns an
/// empty vec when no REPL is running (headless/tests) so the spinner
/// simply omits the agents line.
fn active_agents_for_footer() -> Vec<Arc<crate::commands::code_team::AgentHandle>> {
    ACTIVE_AGENT_REGISTRY
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|r| r.active())
        .unwrap_or_default()
}

/// One-line summary of an active agent for the footer panel, e.g.
/// `●reviewer 12s read`. Clipped to `width` cols so a long current-
/// tool name never wraps the footer.
fn agent_footer_line(handle: &crate::commands::code_team::AgentHandle, width: usize) -> String {
    use crate::commands::code_team::{AgentColor, AgentStatus};
    let secs = handle.elapsed().as_secs();
    let icon = match handle.status() {
        AgentStatus::Spawning => "○",
        AgentStatus::Working => "✽",
        AgentStatus::NeedsInput => "⏸",
        AgentStatus::Idle => "∙",
        AgentStatus::Completed => "✓",
        AgentStatus::Failed => "✗",
        AgentStatus::Stopped => "⊘",
    };
    let cap = match handle.capability {
        crate::commands::code_team::AgentCapability::ReadOnly => "",
        crate::commands::code_team::AgentCapability::ReadWrite => "✎",
    };
    let kind = match handle.kind {
        crate::commands::code_team::AgentKind::Subagent { depth, .. } => {
            if depth > 0 {
                format!("· d{depth}")
            } else {
                String::new()
            }
        }
        crate::commands::code_team::AgentKind::Background { .. } => "· bg".to_string(),
        crate::commands::code_team::AgentKind::Teammate { .. } => "· team".to_string(),
    };
    // Layout: "  ✽ name  prompt…  tool  12s · kind"
    // Fixed: indent(2) + icon(1) + sp(1) + name(≤15) + sp(2) = 21
    // Suffix: sp(1) + tool(≤15) + sp(1) + time(4) + sp(1) + kind(≤7) = ≤29
    let name_w = 15usize;
    let name = clip_chars(&handle.name, name_w);
    let time = if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    };
    let tool = handle.current_tool().unwrap_or_default();
    let tool_w = 15usize;
    let tool_clipped = clip_chars(&tool, tool_w);
    let kind_w = 8usize;
    let kind_text = clip_chars(&kind, kind_w);
    // remaining width for prompt preview
    let fixed = 2 + 1 + 1 + name_w + 2 + 1 + tool_w + 1 + time.len() + 1 + kind_text.len();
    let prompt_w = width.saturating_sub(fixed);
    let prompt = if prompt_w > 3 {
        clip_chars(&handle.prompt_preview, prompt_w)
    } else {
        String::new()
    };
    let cap_str = if cap.is_empty() {
        String::new()
    } else {
        format!("{cap} ")
    };
    let body = format!(
        "{cap_str}{icon} {name:<name_w$}  {prompt}  {tool_clipped:<tool_w$} {time:>4} {kind_text}",
    );
    let body = clip_chars(&body, width.saturating_sub(2));
    format!("  {}", AgentColor::paint(handle.color, &body))
}

/// Erase and hide the live spinner footer so an interactive prompt can
/// paint without the ticker thread clobbering it. In sticky mode the
/// scroll region protects the footer, so only `visible` is toggled.
/// In non-sticky mode the footer is also erased. No-op when no spinner
/// is active. Pair every call with [`resume_active_footer`].
pub(crate) fn suspend_active_footer() {
    let core = ACTIVE_SPINNER.lock().ok().and_then(|g| g.clone());
    if let Some(core) = core {
        if let Ok(mut c) = core.lock() {
            c.visible = false;
            if !c.sticky {
                c.erase();
            }
        }
    }
}

/// Resume the footer ticker paused by [`suspend_active_footer`]. The
/// ticker repaints on its next tick. No-op when no spinner is active.
pub(crate) fn resume_active_footer() {
    let core = ACTIVE_SPINNER.lock().ok().and_then(|g| g.clone());
    if let Some(core) = core {
        if let Ok(mut c) = core.lock() {
            c.visible = true;
        }
    }
}

/// Bottom-line activity spinner. Lives on its own ticker thread so the
/// elapsed counter advances while the runtime thread is blocked on the
/// model stream; every write is serialized through [`SpinnerCore`]'s
/// mutex against the renderer's hide/show calls. Inert (no thread, no
/// output) when the chrome stream is not a TTY or styling is off.
struct Spinner {
    core: Option<Arc<Mutex<SpinnerCore>>>,
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn start(
        enabled: bool,
        stream: ChromeStream,
        styled: bool,
        sticky: bool,
        mode_flag: Option<ModeFlag>,
    ) -> Self {
        if !enabled {
            return Self {
                core: None,
                ticker: None,
            };
        }
        let core = Arc::new(Mutex::new(SpinnerCore {
            label: "thinking…",
            started: Instant::now(),
            output_chars: 0,
            visible: true,
            drawn_rows: 0,
            last_term_height: 0,
            stopped: false,
            stream,
            styled,
            sticky,
            mode_flag,
            queued_texts: Vec::new(),
            typed: String::new(),
        }));
        let ticker = {
            let core = Arc::clone(&core);
            std::thread::spawn(move || loop {
                std::thread::sleep(SPINNER_TICK);
                let Ok(mut c) = core.lock() else { break };
                if c.stopped {
                    break;
                }
                if c.visible {
                    c.draw();
                }
            })
        };
        // Register so an interactive prompt on another code path can
        // erase/pause this footer before painting over the same stream.
        set_active_spinner(Some(Arc::clone(&core)));
        Self {
            core: Some(core),
            ticker: Some(ticker),
        }
    }

    /// Pause the ticker. In sticky mode the DECSTBM scroll region
    /// protects the footer, so only `visible` is toggled. In non-sticky
    /// mode the footer is also erased so content output doesn't
    /// interleave with it.
    fn hide(&self) {
        if let Some(core) = &self.core {
            if let Ok(mut c) = core.lock() {
                c.visible = false;
                if !c.sticky {
                    c.erase();
                }
            }
        }
    }

    /// Let the ticker paint again (next tick, ≤ one `SPINNER_TICK` away).
    fn show(&self) {
        if let Some(core) = &self.core {
            if let Ok(mut c) = core.lock() {
                c.visible = true;
            }
        }
    }

    fn set_label(&self, label: &'static str) {
        if let Some(core) = &self.core {
            if let Ok(mut c) = core.lock() {
                c.label = label;
            }
        }
    }

    fn note_output_chars(&self, n: u64) {
        if let Some(core) = &self.core {
            if let Ok(mut c) = core.lock() {
                c.output_chars += n;
            }
        }
    }

    /// Erase, stop the ticker, and join it. Idempotent.
    fn stop(&mut self) {
        // Deregister before tearing down so a late suspend/resume from
        // another thread can't touch a stopped core. Done without the
        // core lock held to preserve the gate → ACTIVE_SPINNER → core
        // order.
        set_active_spinner(None);
        if let Some(core) = &self.core {
            if let Ok(mut c) = core.lock() {
                c.stopped = true;
                c.erase();
            }
        }
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
        self.core = None;
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Per-prompt stream renderer: markdown-renders assistant text
/// block-by-block (via [`MarkdownStream`], same as `libertai chat`),
/// draws `●`-marker tool lines with dimmed result previews, shows a
/// spinner while waiting, and announces saved-rule auto-approvals.
///
/// Shared by the interactive REPL (chrome on stdout) and the one-shot
/// path (chrome on stderr). `--print` mode never constructs one — its
/// raw-stdout contract is handled in `code.rs`.
pub(crate) struct TurnRenderer {
    md: MarkdownStream,
    spinner: Spinner,
    styled: bool,
    chrome: ChromeStream,
    started: Instant,
    /// `●` already printed for the current turn's text block.
    turn_text_open: bool,
    /// `finish_stream` already ran (it's called from both the AgentEnd
    /// event and the post-await cleanup, whichever comes first).
    finished: bool,
    approvals: Option<Arc<ApprovalState>>,
    mode: Option<ModeFlag>,
    /// SDK `ToolExecutionStart` means "model requested this tool",
    /// not necessarily "this tool is executing now". Keep the args so
    /// actual-start updates and fallback end rendering can print the
    /// right marker without misleading the user during queued delays.
    planned_tools: HashMap<String, (String, serde_json::Value)>,
    rendered_tool_markers: HashSet<String>,
}

impl TurnRenderer {
    pub(crate) fn new(
        chrome: ChromeStream,
        approvals: Option<Arc<ApprovalState>>,
        mode: Option<ModeFlag>,
        sticky: bool,
    ) -> Self {
        let chrome_tty = chrome.is_tty();
        let styled = crate::commands::chat_render::styling_enabled(chrome_tty);
        // Assistant turn marker (Claude Code convention): bold,
        // default-foreground `●` inline with the first line of the
        // answer — visually distinct from the cyan tool markers. Styled
        // per *stdout* (where the markdown lands), not the chrome
        // stream: NO_COLOR keeps the dot but drops the bold.
        let marker = if crate::commands::chat_render::styling_enabled({
            use std::io::IsTerminal;
            io::stdout().is_terminal()
        }) {
            format!("{BOLD}●{RESET} ")
        } else {
            "● ".to_string()
        };
        Self {
            md: MarkdownStream::with_turn_marker(
                crate::commands::chat_render::markdown_enabled_stdout(),
                marker,
            ),
            spinner: Spinner::start(chrome_tty, chrome, styled, sticky, mode.clone()),
            styled,
            chrome,
            started: Instant::now(),
            turn_text_open: false,
            finished: false,
            approvals,
            mode,
            planned_tools: HashMap::new(),
            rendered_tool_markers: HashSet::new(),
        }
    }

    /// Seconds since the prompt was submitted (renderer construction).
    pub(crate) fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn term_width(&self) -> usize {
        term_cols()
    }

    fn chrome_line(&self, line: &str) {
        self.chrome.write_str(&format!("{line}\n"));
    }

    fn dim_chrome_line(&self, line: &str) {
        if self.styled {
            self.chrome_line(&format!("{DIM}{line}{RESET}"));
        } else {
            self.chrome_line(line);
        }
    }

    pub(crate) fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                if delta.is_empty() {
                    return;
                }
                self.spinner.note_output_chars(delta.chars().count() as u64);
                self.spinner.set_label("writing…");
                self.spinner.hide();
                if !self.turn_text_open {
                    self.turn_text_open = true;
                    // Blank line above every reply block: separates the
                    // `❯` user prompt from the first reply, and separates
                    // tool output from the reply that follows it. The
                    // renderer is fresh per prompt and `turn_text_open`
                    // is reset by the tool path, so this fires once per
                    // text block without doubling up.
                    self.chrome_line("");
                    if self.md.renders_markdown() {
                        // Markdown mode: the marker rides inline with
                        // the first rendered line ("● Two active…"),
                        // with a 2-column hanging indent under it —
                        // handled inside MarkdownStream.
                        self.md.begin_marked_block();
                    } else if self.styled {
                        // Raw fallback (piped stdout / dumb terminal):
                        // keep the legacy standalone marker on the
                        // chrome stream so piped assistant text stays
                        // byte-identical plain text.
                        self.chrome_line(&format!("{CYAN}●{RESET}"));
                    } else {
                        self.chrome_line("●");
                    }
                }
                self.md.push(delta);
                self.spinner.show();
            }
            AgentEvent::TurnStart { .. } => {
                self.turn_text_open = false;
                self.planned_tools.clear();
                self.rendered_tool_markers.clear();
                // Belt-and-braces: a turn boundary must never arm the
                // next marker while older prose is still buffered (the
                // marker would attach to the previous turn's text).
                self.md.flush_pending();
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.planned_tools
                    .insert(tool_call_id.clone(), (tool_name.clone(), args.clone()));
                // pi emits start events for the whole tool batch before
                // execution begins. Do not render `● edit(...)` or
                // `● ask_user(...)` here: a previous tool can still be
                // running, which made the TUI look hung on a queued
                // later call. Actual-start updates render the marker.
                self.spinner.hide();
                self.turn_text_open = false;
                // Flush buffered prose before ANY tool output reaches
                // the terminal — including todo's self-rendered task
                // list, which used to print above assistant text that
                // preceded it in the stream (the early-return below
                // skipped this flush).
                self.md.flush_pending();
                if tool_name == "todo" {
                    // The todo tool renders its own formatted output.
                    return;
                }
                self.spinner.set_label("preparing tools…");
                self.spinner.show();
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                self.spinner.hide();
                if tool_name != "todo" {
                    if !self.rendered_tool_markers.contains(tool_call_id) {
                        self.render_tool_marker(tool_call_id, tool_name, None);
                    }
                    let text = tool_output_text(result);
                    for line in tool_result_preview(
                        &text,
                        *is_error,
                        self.term_width(),
                        TOOL_RESULT_PREVIEW_LINES,
                    ) {
                        self.dim_chrome_line(&line);
                    }
                }
                self.planned_tools.remove(tool_call_id);
                self.rendered_tool_markers.remove(tool_call_id);
                self.spinner.set_label("thinking…");
                self.spinner.show();
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => {
                if is_tool_started_update(partial_result) {
                    self.render_tool_marker(tool_call_id, tool_name, Some(args));
                    self.spinner.set_label(tool_running_label(tool_name));
                    self.spinner.show();
                    return;
                }
                if let Some(line) = smart_approval_audit_line(partial_result) {
                    self.spinner.hide();
                    self.dim_chrome_line(&format!("  {line}"));
                    self.spinner.show();
                }
            }
            AgentEvent::AutoCompactionStart { reason } => {
                self.spinner.hide();
                self.dim_chrome_line(&format!("● compacting · {reason}"));
                self.spinner.show();
            }
            AgentEvent::AutoCompactionEnd {
                aborted,
                error_message,
                ..
            } => {
                self.spinner.hide();
                self.dim_chrome_line(&format!(
                    "● {}",
                    compaction_end_text(*aborted, error_message.as_deref())
                ));
                self.spinner.show();
            }
            AgentEvent::AgentEnd { .. } => {
                self.finish_stream();
            }
            _ => {}
        }
    }

    /// Stop the spinner and flush buffered markdown. Idempotent; also
    /// invoked from the error/abort paths where AgentEnd never fired.
    pub(crate) fn finish_stream(&mut self) {
        self.spinner.stop();
        if self.finished {
            return;
        }
        self.finished = true;
        if self.md.saw_output() {
            self.md.finish();
        }
    }

    fn render_tool_marker(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        args: Option<&serde_json::Value>,
    ) {
        if self.rendered_tool_markers.contains(tool_call_id) || tool_name == "todo" {
            return;
        }
        self.spinner.hide();
        self.turn_text_open = false;
        // Flush any prose that arrived after the start event (or that the
        // start event never flushed, on the end-event fallback path).
        self.md.flush_pending();
        // A prose block (streamed earlier or just flushed) always ends
        // with a trailing blank line — suppress the marker's own
        // separator in that case so reply→tool gets one blank, not two.
        // Coming from the user prompt or a prior tool result there's no
        // trailing blank, so the marker emits one.
        if !self.md.take_prose_emitted() {
            self.chrome_line("");
        }
        let planned = self.planned_tools.get(tool_call_id);
        let marker_name = planned.map(|(name, _)| name.as_str()).unwrap_or(tool_name);
        let marker_args = args
            .or_else(|| planned.map(|(_, planned_args)| planned_args))
            .unwrap_or(&serde_json::Value::Null);
        self.chrome_line(&self.tool_marker_line(marker_name, marker_args));
        if let Some(label) = self.auto_allowed_rule_label(marker_name, marker_args) {
            self.dim_chrome_line(&format!(
                "  {}",
                crate::commands::code_term::auto_allowed_line(&label)
            ));
        }
        self.rendered_tool_markers.insert(tool_call_id.to_string());
    }

    fn tool_marker_line(&self, tool_name: &str, args: &serde_json::Value) -> String {
        let preview = crate::commands::code_tool_preview::tool_preview(tool_name, args);
        let detail = preview
            .strip_prefix(tool_name)
            .map(str::trim_start)
            .unwrap_or("");
        let detail = sanitize_terminal_preview_text(detail);
        let detail = clip_chars(
            &detail,
            self.term_width().saturating_sub(tool_name.len() + 6),
        );
        if self.styled {
            if detail.is_empty() {
                format!("{CYAN}●{RESET} {BOLD}{tool_name}{RESET}")
            } else {
                format!("{CYAN}●{RESET} {BOLD}{tool_name}{RESET}{DIM}({detail}){RESET}")
            }
        } else if detail.is_empty() {
            format!("● {tool_name}")
        } else {
            format!("● {tool_name}({detail})")
        }
    }

    /// `Some(rule_label)` when a persisted "always allow" rule resolves
    /// this call without a prompt — mirrors the gate order inside
    /// `ApprovalTool::execute` so we only announce what actually
    /// happened: plan mode denies first, accept-edits auto-allows path
    /// edits before rules are consulted, and the hardcoded read-only
    /// tools never prompt (so a line for them would be noise).
    fn auto_allowed_rule_label(&self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        let approvals = self.approvals.as_ref()?;
        if READ_ONLY_AUTO_ALLOW_TOOLS.contains(&tool_name) {
            return None;
        }
        match self.mode.as_ref().map(ModeFlag::get) {
            Some(Mode::Plan) => return None,
            Some(Mode::AcceptEdits) if is_path_edit_tool(tool_name) => return None,
            _ => {}
        }
        let subject = crate::commands::code_approvals::approval_subject(tool_name, args);
        approvals
            .always_rules()
            .iter()
            .any(|rule| rule.matches(tool_name, &subject.value))
            .then_some(subject.suggested_label)
    }
}

/// Tools `ApprovalState` auto-allows by name (read-only built-ins).
/// Mirrors the `auto_allow` set in `code_approvals.rs`.
const READ_ONLY_AUTO_ALLOW_TOOLS: &[&str] = &["read", "grep", "find", "ls", "bash_output"];

fn tool_running_label(tool_name: &str) -> &'static str {
    match tool_name {
        "ask_user" => "waiting for answer…",
        "edit" | "hashline_edit" | "write" | "notebook_edit" => "editing…",
        "bash" | "shell" => "running command…",
        "fetch" => "fetching…",
        "search" => "searching…",
        "task" => "running agent…",
        _ => "running tool…",
    }
}

fn is_tool_started_update(output: &pi::sdk::ToolOutput) -> bool {
    output
        .details
        .as_ref()
        .and_then(|details| details.get("kind"))
        .and_then(|kind| kind.as_str())
        == Some("tool_started")
}

pub(crate) fn smart_approval_audit_line(output: &pi::sdk::ToolOutput) -> Option<String> {
    let details = output.details.as_ref()?;
    if details.get("kind").and_then(|value| value.as_str()) != Some("smart_approval") {
        return None;
    }
    let decision = details
        .get("decision")
        .and_then(|value| value.as_str())
        .unwrap_or("updated");
    let tool = details
        .get("tool")
        .and_then(|value| value.as_str())
        .unwrap_or("tool");
    let reason = details
        .get("reason")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match (decision, reason) {
        ("approved", _) => format!("✓ smart-approved · {tool}"),
        ("denied", Some(reason)) => format!("✗ smart-denied · {tool}: {reason}"),
        ("denied", None) => format!("✗ smart-denied · {tool}"),
        (other, Some(reason)) => format!("smart approval {other} · {tool}: {reason}"),
        (other, None) => format!("smart approval {other} · {tool}"),
    })
}

/// Concatenated text blocks of a tool result.
fn tool_output_text(result: &pi::sdk::ToolOutput) -> String {
    let mut out = String::new();
    for block in &result.content {
        if let ContentBlock::Text(text) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}

/// Dimmed, indented preview of a tool result: first `max_lines`
/// non-empty lines, each truncated to the terminal width, then a
/// "… +N lines" tail when more follow. Empty output previews as
/// `(no output)` so a silent tool still visibly completed.
fn tool_result_preview(text: &str, is_error: bool, width: usize, max_lines: usize) -> Vec<String> {
    let head = if is_error { "  └ ✗ " } else { "  └ " };
    let cont = "    ";
    let budget = width.saturating_sub(head.chars().count() + 1).max(8);
    let safe_text = sanitize_terminal_preview_text(text);
    let lines: Vec<&str> = safe_text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return vec![format!("{head}(no output)")];
    }
    let mut out = Vec::with_capacity(max_lines + 1);
    for (i, line) in lines.iter().take(max_lines).enumerate() {
        let prefix = if i == 0 { head } else { cont };
        out.push(format!("{prefix}{}", clip_chars(line, budget)));
    }
    if lines.len() > max_lines {
        out.push(format!("{cont}… +{} lines", lines.len() - max_lines));
    }
    out
}

/// Clip to `max` **display cells** with a single `…` marker that fits
/// the budget (unlike the older `truncate_chars`, which appends "..."
/// beyond it). Counts terminal cells via rich's calculus
/// (`rich_rust::cells`: wide CJK/emoji = 2, combining marks = 0) — a
/// char- or byte-counted clip can still overflow a terminal row when
/// the text carries wide glyphs. Shared with `code_term`'s approval
/// row, which must fit one terminal line for its `\r ESC[2K` eraser to
/// work, and with the tool-result gutter (incl. guardrail warnings).
pub(crate) fn clip_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if rich_rust::cells::cell_len(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = rich_rust::cells::get_character_cell_size(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Strip terminal escape/control sequences from text that will be
/// echoed in the CLI chrome. Newlines and tabs survive because they are
/// meaningful user text; CR is normalized to LF so paste payloads and
/// previews share one shape.
pub(crate) fn sanitize_terminal_preview_text(input: &str) -> String {
    strip_terminal_sequences(&input.replace("\r\n", "\n").replace('\r', "\n"))
}

fn strip_terminal_sequences(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                i = skip_escape_sequence(bytes, i + 1);
            }
            0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f => {
                i += 1;
            }
            _ => {
                let s = &input[i..];
                let Some(ch) = s.chars().next() else {
                    break;
                };
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

fn skip_escape_sequence(bytes: &[u8], mut i: usize) -> usize {
    let Some(&kind) = bytes.get(i) else {
        return i;
    };
    i += 1;
    match kind {
        b'[' => {
            while let Some(&b) = bytes.get(i) {
                i += 1;
                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
            i
        }
        b']' | b'P' | b'^' | b'_' => {
            while i < bytes.len() {
                match bytes[i] {
                    0x07 => return i + 1,
                    0x1b if bytes.get(i + 1) == Some(&b'\\') => return i + 2,
                    _ => i += 1,
                }
            }
            i
        }
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => (i + 1).min(bytes.len()),
        0x40..=0x5f | 0x60..=0x7e => i,
        _ => i,
    }
}

/// `41s`, `2m08s` — elapsed-time fragment for spinner and stop line.
fn human_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Honest end-of-turn verb for each stop reason.
fn stop_reason_verb(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "done",
        StopReason::Length => "max tokens",
        StopReason::ToolUse => "tool-use",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

/// (M5/#35) The verb for a `StopReason::Length` turn, distinguishing the
/// two meanings of "length": an output-token cap (the model talked until
/// it hit `max_tokens`) vs a context-window cap (the prompt filled the
/// window so pi stopped before completing). When `is_ctx_limit` is true
/// the verb is `ctx limit` (distinct from the output-cap `max tokens`);
/// otherwise — and for every other stop reason — it falls back to
/// [`stop_reason_verb`]. Callers decide `is_ctx_limit` from
/// `context_tokens >= context_window - reserve` (and autocompact on).
fn stop_reason_verb_ctx(reason: &StopReason, is_ctx_limit: bool) -> &'static str {
    if is_ctx_limit && matches!(reason, StopReason::Length) {
        "ctx limit"
    } else {
        stop_reason_verb(reason)
    }
}

/// Dim end-of-turn line: `● done · 18.3k in · 272 out · 41s`. The "in"
/// figure is the same context-occupancy count the status bar shows
/// ([`context_tokens`]), so the two never disagree.
///
/// (M5/#35) The non-test call sites now use [`stop_line_text_ctx`] so they
/// can flag a `StopReason::Length` turn as a context-window cap. This
/// default-verb form is kept for the stop-line unit tests.
#[cfg(test)]
pub(crate) fn stop_line_text(
    reason: &StopReason,
    ctx_in: u64,
    out: u64,
    elapsed_secs: u64,
) -> String {
    // `is_ctx_limit=false` → `StopReason::Length` renders as `max tokens`
    // (the output-cap verb), the same behaviour this fn had before M5/#35
    // split the two Length meanings. Kept for tests that assert the default
    // verb; non-test call sites use [`stop_line_text_ctx`].
    stop_line_text_ctx(reason, false, ctx_in, out, elapsed_secs)
}

/// (M5/#35) Like [`stop_line_text`] but lets the caller flag a
/// `StopReason::Length` turn as a context-window limit (verb `ctx limit`)
/// rather than the default output-cap verb (`max tokens`). Used by the
/// REPL and TUI TurnEnd handlers, which know the context window + reserve
/// + autocompact flag and so can tell the two Length meanings apart.
pub(crate) fn stop_line_text_ctx(
    reason: &StopReason,
    is_ctx_limit: bool,
    ctx_in: u64,
    out: u64,
    elapsed_secs: u64,
) -> String {
    format!(
        "● {} · {} in · {} out · {}",
        stop_reason_verb_ctx(reason, is_ctx_limit),
        human_tokens(ctx_in),
        human_tokens(out),
        human_elapsed(elapsed_secs),
    )
}

/// (M5/#35) True when a `StopReason::Length` turn is a context-window
/// limit (not an output-token cap): the turn's context occupancy is at or
/// above the compaction reserve line, i.e. the prompt filled the window.
/// `reserve_tokens` is the [`Config::code_compaction_reserve_tokens`]
/// headroom below the window; within that headroom the stop is an output
/// cap, at/above it the stop is a context cap. Callers gate on
/// `auto_compaction_enabled` too — without autocompact the distinction
/// is moot (nothing will compact anyway), so we return false and the verb
/// stays `max tokens`.
pub(crate) fn is_ctx_limit_stop(
    reason: &StopReason,
    context_tokens: u64,
    context_window: u64,
    reserve_tokens: u64,
    auto_compaction_enabled: bool,
) -> bool {
    auto_compaction_enabled
        && matches!(reason, StopReason::Length)
        && context_window > 0
        && context_tokens >= context_window.saturating_sub(reserve_tokens)
}

// ---------------------------------------------------------------------------
// Queued messages — "message stacking" while a turn is in flight
// ---------------------------------------------------------------------------

/// How many queued messages get their own preview line before the rest
/// collapse into a "… and N more" summary row.
const MAX_QUEUED_PREVIEW_LINES: usize = 3;

/// Dim affordance lines for the queued stack — one `› queued: …` row
/// per message (first line only, cell-clipped to the terminal width),
/// shared by the mid-turn footer and the idle input bar. Pure plain
/// text: no ANSI here, so NO_COLOR terminals render it verbatim and
/// the caller decides the dimming.
fn queued_preview_lines(texts: &[String], width: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(texts.len().min(MAX_QUEUED_PREVIEW_LINES) + 1);
    for text in texts.iter().take(MAX_QUEUED_PREVIEW_LINES) {
        let safe_text = sanitize_terminal_preview_text(text);
        let first = safe_text.lines().next().unwrap_or("").trim_end();
        out.push(clip_chars(&format!("  › queued: {first}"), width));
    }
    if texts.len() > MAX_QUEUED_PREVIEW_LINES {
        out.push(clip_chars(
            &format!(
                "  › … and {} more queued",
                texts.len() - MAX_QUEUED_PREVIEW_LINES
            ),
            width,
        ));
    }
    out
}

/// The live mid-turn typing row: `❯ ` plus the tail of the buffer that
/// fits the width (the cursor is always at the end mid-turn, so the
/// tail window keeps it visible). Newlines (Alt+Enter) render as `⏎`.
/// Plain text — the footer drawer adds the prompt styling.
fn typed_preview_line(typed: &str, width: usize) -> String {
    let prefix = "\u{276f} ";
    if width == 0 {
        return String::new();
    }
    let prefix_width = rich_rust::cells::cell_len(prefix);
    if width <= prefix_width {
        return clip_chars(prefix, width);
    }
    let safe_typed = sanitize_terminal_preview_text(typed);
    let flat: String = safe_typed
        .chars()
        .map(|c| if c == '\n' { '⏎' } else { c })
        .collect();
    let budget = width - prefix_width;
    if rich_rust::cells::cell_len(&flat) <= budget {
        return format!("{prefix}{flat}");
    }
    // Keep the tail: walk back from the end until the budget (minus the
    // leading ellipsis cell) is filled.
    let mut tail = String::new();
    let mut used = rich_rust::cells::cell_len("…");
    for c in flat.chars().rev() {
        let w = rich_rust::cells::get_character_cell_size(c);
        if used + w > budget {
            break;
        }
        tail.push(c);
        used += w;
    }
    let tail: String = tail.chars().rev().collect();
    format!("{prefix}…{tail}")
}

/// `· N queued · ↑ edits` suffix for the spinner line while messages
/// are stacked (the per-message previews sit on their own rows below).
fn queued_spinner_suffix(queued: usize) -> String {
    match queued {
        0 => String::new(),
        1 => " · 1 queued · ↑ edits".to_string(),
        n => format!(" · {n} queued · ↑ edits"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test-only imports: these symbols are no longer used by production code
    // (the legacy REPL print/parse paths were removed), but a handful of
    // live-helper tests still construct messages with them. Kept here rather
    // than at module scope so `cargo build` (non-test) stays 0-warning.
    use pi::model::TextContent;
    use pi::model::UserContent;

    // (R4-LOG-2) `read_log_tail` must read ONLY the tail via seek (not the whole
    // file) and, when the file is larger than `max_bytes`, prepend its
    // `[truncated to last N bytes]` marker + drop the partial first line it
    // landed on. A file SMALLER than the budget must pass through whole with no
    // marker.
    #[test]
    fn read_log_tail_under_budget_returns_whole_file_no_marker() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("small.log");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let tail = read_log_tail(&path, 64_000).unwrap();
        assert_eq!(
            tail, "alpha\nbeta\ngamma\n",
            "under-budget file must pass through whole, no marker"
        );
    }

    #[test]
    fn read_log_tail_over_budget_prepends_marker_and_drops_partial_first_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("big.log");
        // Three distinct lines (160B each) so the budget can land mid-line and
        // leave at least one complete line after the partial first line is
        // dropped. body = line1\n line2\n line3\n = 483 bytes.
        let line1 = "aaaa".repeat(40); // 160 bytes
        let line2 = "bbbb".repeat(40); // 160 bytes
        let line3 = "cccc".repeat(40); // 160 bytes
        let body = format!("{line1}\n{line2}\n{line3}\n");
        std::fs::write(&path, &body).unwrap();

        // Budget = 200 → start = 483 - 200 = 283, which lands mid-line2 (line2
        // spans bytes 161..321). The tail therefore begins with a FRAGMENT of
        // line2 (a partial first line), which must be dropped so the tail
        // starts at a line boundary (line3). The marker is prepended.
        let budget = 200usize;
        let tail = read_log_tail(&path, budget).unwrap();
        assert!(
            tail.starts_with(&format!("[truncated to last {budget} bytes]\n")),
            "over-budget tail must start with the truncation marker, got: {tail:?}"
        );
        // After the marker, the first content line must be `line3` (the partial
        // line2 fragment was dropped).
        let after_marker = tail
            .strip_prefix(&format!("[truncated to last {budget} bytes]\n"))
            .unwrap();
        assert_eq!(
            after_marker,
            &format!("{line3}\n"),
            "partial first line must be dropped; tail must start at line3, got: {after_marker:?}"
        );
    }

    #[test]
    fn read_log_tail_missing_file_errors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nope.log");
        assert!(
            read_log_tail(&path, 1000).is_err(),
            "missing file must error, not panic"
        );
    }

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
        let actual = std::fs::canonicalize(result.stdout.trim()).unwrap();
        let expected = std::fs::canonicalize(temp.path()).unwrap();
        assert_eq!(actual, expected);
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
    fn usage_summary_empty_when_no_turns() {
        assert!(usage_summary(&[]).is_none());
    }

    #[test]
    fn status_line_template_normalizes_and_expands_known_tokens() {
        let status = BarStatus {
            model_label: "libertai/qwen".to_string(),
            input_tokens: 2048,
            context_window: 4096,
            output_style: Some("review".to_string()),
            status_line_template:
                "{backend}/{model} {mode} {style} {tokens} {ctx} {cost} {unknown}".to_string(),
            status_line_command: String::new(),
            estimated_cost: Some(0.1234),
        };
        let expanded =
            expand_status_line_template(&status.status_line_template, &status, Mode::Plan).unwrap();
        assert_eq!(
            expanded,
            "libertai/qwen plan review 2.0k 50% ~$0.12 {unknown}"
        );
    }

    #[test]
    fn status_line_cost_token_dashes_when_unpriced() {
        let status = BarStatus {
            model_label: "libertai/qwen".to_string(),
            input_tokens: 0,
            context_window: 4096,
            output_style: None,
            status_line_template: "{cost}".to_string(),
            status_line_command: String::new(),
            estimated_cost: None,
        };
        let expanded =
            expand_status_line_template(&status.status_line_template, &status, Mode::Normal)
                .unwrap();
        assert_eq!(expanded, "-");
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
            estimated_cost: None,
        };
        assert!(expand_status_line_template("", &status, Mode::Normal).is_none());
        assert_eq!(
            default_rule_text(&status, Mode::Normal),
            "libertai/qwen · normal · 50% ctx (512 / 1.0k)"
        );
    }

    #[test]
    fn context_tokens_counts_cache_reads_and_writes() {
        let usage = Usage {
            input: 10_300,
            output: 272,
            cache_read: 7_800,
            cache_write: 224,
            ..Default::default()
        };
        assert_eq!(context_tokens(&usage), 18_324);
        // No cache → plain input.
        let plain = Usage {
            input: 512,
            output: 8,
            ..Default::default()
        };
        assert_eq!(context_tokens(&plain), 512);
    }

    #[test]
    fn ctx_chip_pct_and_parenthetical_share_one_counter() {
        // Regression: the bar once showed a pct from one counter and a
        // (used / cap) pair from another. Both must derive from the
        // exact same `input_tokens` value.
        let status = BarStatus {
            model_label: "libertai/qwen".to_string(),
            input_tokens: 18_324,
            context_window: 32_768,
            output_style: None,
            status_line_template: String::new(),
            status_line_command: String::new(),
            estimated_cost: None,
        };
        let text = default_rule_text(&status, Mode::Normal);
        assert_eq!(text, "libertai/qwen · normal · 56% ctx (18.3k / 32.8k)");
        // And the displayed pct really is used/cap of the displayed pair.
        assert_eq!(context_percent(18_324, 32_768), 56);
    }

    #[test]
    fn context_window_for_is_hermetic_under_test() {
        // Unit tests must never read the dev machine's models.json or
        // catalog cache — the cfg!(test) gate pins the fallback.
        assert_eq!(
            context_window_for("libertai", "qwen3.6-35b-a3b"),
            FALLBACK_CONTEXT_WINDOW
        );
        assert_eq!(context_window_for("libertai", "unknown-model"), 32_768);
    }

    #[test]
    fn models_json_lookup_prefers_the_sessions_provider() {
        // The fixture mirrors what `ensure_libertai_registered` +
        // catalog enrichment write to pi's models.json (camelCase, real
        // window for qwen3.6-35b-a3b under `libertai`), plus desktop
        // backend entries that carry the same id with stale windows —
        // the session's provider must win, with the global scan only as
        // a fallback for ids the named provider doesn't carry.
        let root = json!({
            "providers": {
                "backend-glm-a392cad6": {
                    "models": [
                        {"id": "qwen3.6-35b-a3b", "contextWindow": 128000},
                        {"id": "third-party", "contextWindow": 131072}
                    ]
                },
                "libertai": {
                    "baseUrl": "https://api.libertai.io/v1",
                    "models": [
                        {"id": "qwen3.6-35b-a3b", "contextWindow": 262144},
                        {"id": "legacy-model", "contextWindow": 32768}
                    ]
                }
            }
        });
        assert_eq!(
            models_json_context_window(&root, "libertai", "qwen3.6-35b-a3b"),
            Some(262_144)
        );
        assert_eq!(
            models_json_context_window(&root, "backend-glm-a392cad6", "qwen3.6-35b-a3b"),
            Some(128_000)
        );
        // Model missing from the named provider → any other provider.
        assert_eq!(
            models_json_context_window(&root, "libertai", "third-party"),
            Some(131_072)
        );
        assert_eq!(
            models_json_context_window(&root, "libertai", "legacy-model"),
            Some(32_768)
        );
        // Unknown model → None, so resolution falls through to the
        // catalog and then the 32k fallback.
        assert_eq!(models_json_context_window(&root, "libertai", "nope"), None);
    }

    #[test]
    fn models_json_lookup_ignores_malformed_entries() {
        let root = json!({
            "providers": {
                "libertai": {
                    "models": [
                        {"id": "no-window"},
                        {"id": "zero-window", "contextWindow": 0},
                        {"id": "string-window", "contextWindow": "big"},
                        "not-an-object"
                    ]
                },
                "broken": {"models": "not-an-array"}
            }
        });
        for model in ["no-window", "zero-window", "string-window"] {
            assert_eq!(models_json_context_window(&root, "libertai", model), None);
        }
        // Entirely malformed roots resolve to None rather than panicking.
        assert_eq!(models_json_context_window(&json!([]), "p", "x"), None);
        assert_eq!(models_json_context_window(&json!({}), "p", "x"), None);
    }

    #[test]
    fn stop_line_formats_humanized_tokens_and_elapsed() {
        assert_eq!(
            stop_line_text(&StopReason::Stop, 18_324, 272, 41),
            "● done · 18.3k in · 272 out · 41s"
        );
        assert_eq!(
            stop_line_text(&StopReason::Length, 900, 1_200, 128),
            "● max tokens · 900 in · 1.2k out · 2m08s"
        );
        assert_eq!(
            stop_line_text(&StopReason::Aborted, 0, 0, 3),
            "● aborted · 0 in · 0 out · 3s"
        );
        assert_eq!(stop_reason_verb(&StopReason::Error), "error");
        assert_eq!(stop_reason_verb(&StopReason::ToolUse), "tool-use");
    }

    #[test]
    fn stop_line_text_ctx_distinguishes_length_meanings() {
        // is_ctx_limit=false → output-cap verb ("max tokens").
        assert_eq!(
            stop_line_text_ctx(&StopReason::Length, false, 900, 1_200, 128),
            "● max tokens · 900 in · 1.2k out · 2m08s"
        );
        // is_ctx_limit=true → context-window verb ("ctx limit").
        assert_eq!(
            stop_line_text_ctx(&StopReason::Length, true, 199_500, 1_200, 5),
            "● ctx limit · 199.5k in · 1.2k out · 5s"
        );
        // Non-Length reasons ignore is_ctx_limit entirely.
        assert_eq!(
            stop_line_text_ctx(&StopReason::Stop, true, 18_324, 272, 41),
            "● done · 18.3k in · 272 out · 41s"
        );
    }

    #[test]
    fn is_ctx_limit_stop_classifies_length_turns() {
        use super::is_ctx_limit_stop;
        // Autocompact off → never a ctx limit (the distinction is moot).
        assert!(!is_ctx_limit_stop(
            &StopReason::Length,
            199_500,
            200_000,
            10_000,
            false
        ));
        // At/above the reserve line (200_000 - 10_000 = 190_000) with
        // autocompact on → ctx limit.
        assert!(is_ctx_limit_stop(
            &StopReason::Length,
            190_000,
            200_000,
            10_000,
            true
        ));
        // Below the reserve line → output cap, not ctx limit.
        assert!(!is_ctx_limit_stop(
            &StopReason::Length,
            150_000,
            200_000,
            10_000,
            true
        ));
        // Non-Length reason → never a ctx limit even if tokens are huge.
        assert!(!is_ctx_limit_stop(
            &StopReason::Stop,
            199_500,
            200_000,
            10_000,
            true
        ));
        // Zero context window (unknown model) → can't classify, not ctx limit.
        assert!(!is_ctx_limit_stop(
            &StopReason::Length,
            199_500,
            0,
            10_000,
            true
        ));
        // Saturation: reserve larger than window must not underflow.
        assert!(is_ctx_limit_stop(&StopReason::Length, 1, 100, 1_000, true));
    }

    #[test]
    fn human_elapsed_switches_to_minutes_at_sixty() {
        assert_eq!(human_elapsed(0), "0s");
        assert_eq!(human_elapsed(59), "59s");
        assert_eq!(human_elapsed(60), "1m00s");
        assert_eq!(human_elapsed(754), "12m34s");
    }

    #[test]
    fn spinner_line_shows_tokens_only_once_output_streams() {
        assert_eq!(spinner_line_text("thinking…", 12, 0), "✳ thinking… 12s");
        assert_eq!(
            spinner_line_text("writing…", 23, 4_800),
            "✳ writing… 23s · 1.2k tokens"
        );
    }

    #[test]
    fn agent_footer_line_renders_name_elapsed_and_tool() {
        use crate::commands::code_team::{AgentColor, AgentKind, AgentRegistration, AgentRegistry};
        let registry = AgentRegistry::new();
        let h = registry.register(AgentRegistration {
            name: "reviewer".to_string(),
            kind: AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
            color: AgentColor::Green,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: PathBuf::from("/tmp"),
            model: "m".to_string(),
            prompt_preview: "p".to_string(),
            parent: None,
            pid: None,
            log_path: None,
        });
        h.set_current_tool(Some("read".to_string()));
        // Elapsed is ~0s right after spawn; assert the structure rather
        // than the exact second count.
        let line = agent_footer_line(&h, 80);
        assert!(line.contains("reviewer"), "line was: {line}");
        assert!(line.contains("read"), "line was: {line}");
        assert!(line.contains("0s"), "line was: {line}");
        // Spawning status gets the ○ icon; read-only agents have no cap prefix.
        assert!(line.contains("○ reviewer"), "line was: {line}");
    }

    #[test]
    fn agent_footer_line_markes_write_capable_with_pencil() {
        use crate::commands::code_team::{AgentColor, AgentKind, AgentRegistration, AgentRegistry};
        let registry = AgentRegistry::new();
        let h = registry.register(AgentRegistration {
            name: "builder".to_string(),
            kind: AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
            color: AgentColor::Blue,
            capability: crate::commands::code_team::AgentCapability::ReadWrite,
            cwd: PathBuf::from("/tmp"),
            model: "m".to_string(),
            prompt_preview: "p".to_string(),
            parent: None,
            pid: None,
            log_path: None,
        });
        let line = agent_footer_line(&h, 80);
        // Write-capable agents get the ✎ cap prefix before the status icon.
        assert!(line.contains("✎"), "line was: {line}");
        assert!(line.contains("builder"), "line was: {line}");
    }

    #[test]
    fn tool_running_label_names_slow_tool_phases() {
        assert_eq!(tool_running_label("ask_user"), "waiting for answer…");
        assert_eq!(tool_running_label("edit"), "editing…");
        assert_eq!(tool_running_label("hashline_edit"), "editing…");
        assert_eq!(tool_running_label("write"), "editing…");
        assert_eq!(tool_running_label("notebook_edit"), "editing…");
        assert_eq!(tool_running_label("bash"), "running command…");
        assert_eq!(tool_running_label("shell"), "running command…");
        assert_eq!(tool_running_label("task"), "running agent…");
        assert_eq!(tool_running_label("grep"), "running tool…");
    }

    #[test]
    fn tool_started_update_is_structured_not_textual() {
        let started = pi::sdk::ToolOutput {
            content: vec![],
            details: Some(serde_json::json!({
                "kind": "tool_started",
                "tool": "ask_user",
            })),
            is_error: false,
        };
        assert!(is_tool_started_update(&started));
        assert!(!is_tool_started_update(&empty_tool_output()));
    }

    #[test]
    fn tool_result_preview_truncates_lines_and_counts_overflow() {
        let text = "one\ntwo\n\nthree\nfour\nfive\nsix";
        let lines = tool_result_preview(text, false, 80, 4);
        assert_eq!(
            lines,
            vec![
                "  └ one".to_string(),
                "    two".to_string(),
                "    three".to_string(),
                "    four".to_string(),
                "    … +2 lines".to_string(),
            ]
        );
        // Wide line is clipped to the width budget with an ellipsis.
        let wide = tool_result_preview(&"x".repeat(300), false, 40, 4);
        assert_eq!(wide.len(), 1);
        assert!(wide[0].ends_with('…'));
        assert!(wide[0].chars().count() <= 40);
        // Errors get a ✗ in the gutter; empty output stays visible.
        let err = tool_result_preview("boom", true, 80, 4);
        assert_eq!(err, vec!["  └ ✗ boom".to_string()]);
        let empty = tool_result_preview("   \n\n", false, 80, 4);
        assert_eq!(empty, vec!["  └ (no output)".to_string()]);
    }

    #[test]
    fn tool_result_preview_strips_terminal_sequences_before_rendering() {
        let text = "\x1b]8;;https://example.com\x07link\x1b]8;;\x07\n\x1b[31mred\x1b[0m\nbell\x07";
        let lines = tool_result_preview(text, false, 80, 4);
        assert_eq!(
            lines,
            vec![
                "  └ link".to_string(),
                "    red".to_string(),
                "    bell".to_string(),
            ]
        );
        assert!(lines.iter().all(|line| !line.contains('\x1b')));
    }

    #[test]
    fn smart_approval_audit_line_renders_structured_updates() {
        let approved = pi::sdk::ToolOutput {
            content: vec![],
            details: Some(serde_json::json!({
                "kind": "smart_approval",
                "decision": "approved",
                "tool": "bash",
            })),
            is_error: false,
        };
        assert_eq!(
            smart_approval_audit_line(&approved),
            Some("✓ smart-approved · bash".to_string())
        );

        let denied = pi::sdk::ToolOutput {
            content: vec![],
            details: Some(serde_json::json!({
                "kind": "smart_approval",
                "decision": "denied",
                "tool": "write",
                "reason": "outside workspace",
            })),
            is_error: true,
        };
        assert_eq!(
            smart_approval_audit_line(&denied),
            Some("✗ smart-denied · write: outside workspace".to_string())
        );

        let unrelated = pi::sdk::ToolOutput {
            content: vec![],
            details: Some(serde_json::json!({"kind": "progress"})),
            is_error: false,
        };
        assert_eq!(smart_approval_audit_line(&unrelated), None);
    }

    #[test]
    fn queued_and_typed_previews_strip_terminal_sequences() {
        let texts = vec!["\x1b[31mred\x1b[0m\nsecond".to_string()];
        assert_eq!(queued_preview_lines(&texts, 80), vec!["  › queued: red"]);
        assert_eq!(
            typed_preview_line("a\x1b]0;title\x07b\x1b[2K\nc", 80),
            "❯ ab⏎c"
        );
    }

    #[test]
    fn typed_preview_line_keeps_the_tail_visible() {
        assert_eq!(typed_preview_line("", 40), "\u{276f} ");
        assert_eq!(typed_preview_line("hello", 40), "\u{276f} hello");
        // Long input: the end (where the cursor is) stays visible.
        let long = "abcdefghijklmnopqrstuvwxyz";
        let line = typed_preview_line(long, 20);
        assert!(line.starts_with("\u{276f} …"));
        assert!(line.ends_with("xyz"));
        assert!(rich_rust::cells::cell_len(&line) <= 20);
        for width in 0..10 {
            let line = typed_preview_line(long, width);
            assert!(
                rich_rust::cells::cell_len(&line) <= width,
                "typed row exceeds narrow width {width}: {line:?}"
            );
        }
        // Newlines (Alt+Enter) render as the ⏎ glyph.
        assert_eq!(typed_preview_line("a\nb", 40), "\u{276f} a⏎b");
    }

    #[test]
    fn queued_spinner_suffix_counts_the_stack() {
        assert_eq!(queued_spinner_suffix(0), "");
        assert_eq!(queued_spinner_suffix(1), " · 1 queued · ↑ edits");
        assert_eq!(queued_spinner_suffix(3), " · 3 queued · ↑ edits");
    }

    #[test]
    fn erase_seq_clears_exactly_the_drawn_rows() {
        assert_eq!(SpinnerCore::erase_seq(1), "\r\x1b[2K");
        assert_eq!(
            SpinnerCore::erase_seq(3),
            "\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K"
        );
    }

    #[test]
    fn clip_chars_is_char_safe() {
        assert_eq!(clip_chars("héllo wörld", 20), "héllo wörld");
        assert_eq!(clip_chars("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn clip_chars_counts_display_cells_not_chars_or_bytes() {
        // Wide CJK glyphs are 2 cells each: "你你你" = 6 cells. A
        // char-counted clip would call it 3 and overflow the row the
        // approval-prompt eraser assumes is a single line.
        assert_eq!(clip_chars("你你你", 6), "你你你");
        assert_eq!(clip_chars("你你你", 5), "你你…");
        assert_eq!(clip_chars("你你你", 4), "你…");
        // A wide glyph never straddles the budget edge.
        assert_eq!(clip_chars("a你b", 3), "a…");
    }

    #[test]
    fn guardrail_warning_preview_is_clipped_to_width_in_cells() {
        // The guardrail warning is prepended to the tool result text
        // and flows through the same dim gutter as any preview line —
        // it must clip to the terminal width by display cells.
        let warning = "tool-call guardrail warning: `read` has been called 6 consecutive \
                       times; consider summarizing progress or switching tactics";
        let lines = tool_result_preview(warning, false, 60, 4);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with('…'), "long warning not elided");
        assert!(
            rich_rust::cells::cell_len(&lines[0]) <= 60,
            "clipped gutter line exceeds the terminal width: {:?}",
            lines[0]
        );
    }

    #[test]
    fn tool_output_text_joins_text_blocks() {
        let result = pi::sdk::ToolOutput {
            content: vec![
                ContentBlock::Text(TextContent::new("first")),
                ContentBlock::Text(TextContent::new("second")),
            ],
            details: None,
            is_error: false,
        };
        assert_eq!(tool_output_text(&result), "first\nsecond");
        assert_eq!(tool_output_text(&empty_tool_output()), "");
    }

    #[test]
    fn compaction_end_text_reports_outcomes() {
        assert_eq!(compaction_end_text(true, None), "compaction aborted");
        assert_eq!(
            compaction_end_text(false, Some("disk full")),
            "compaction failed: disk full"
        );
        assert_eq!(compaction_end_text(false, None), "compaction finished");
    }

    #[test]
    fn default_rule_text_tracks_mode_and_cost() {
        let mut status = BarStatus {
            model_label: "libertai/qwen".to_string(),
            input_tokens: 512,
            context_window: 1024,
            output_style: None,
            status_line_template: String::new(),
            status_line_command: String::new(),
            estimated_cost: Some(0.4567),
        };
        assert_eq!(
            default_rule_text(&status, Mode::Plan),
            "libertai/qwen · plan · 50% ctx (512 / 1.0k) · ~$0.46"
        );
        // Shift+Tab back to normal is visible in the same snapshot.
        assert_eq!(
            default_rule_text(&status, Mode::AcceptEdits),
            "libertai/qwen · accept-edits · 50% ctx (512 / 1.0k) · ~$0.46"
        );
        // No context window (unknown model) degrades to model · mode.
        status.context_window = 0;
        status.estimated_cost = None;
        assert_eq!(
            default_rule_text(&status, Mode::Normal),
            "libertai/qwen · normal"
        );
    }

    #[test]
    fn status_line_command_output_uses_first_nonempty_line() {
        assert_eq!(
            first_status_line(" \n  dynamic branch  \nsecond"),
            "dynamic branch"
        );
    }

    #[test]
    fn status_line_command_runs_shell_and_reads_first_output_line() {
        let (value, error) = run_status_line_command("printf 'ready\\nsecond\\n'");
        assert_eq!(value, "ready");
        assert_eq!(error, "");
    }

    #[test]
    fn hotkey_lines_include_mode_history_and_interrupt_controls() {
        let joined = hotkey_lines().join("\n");
        assert!(joined.contains("Shift+Tab"));
        assert!(joined.contains("Up / Down"));
        assert!(joined.contains("Ctrl+Left / Ctrl+Right"));
        assert!(joined.contains("Alt+D / Ctrl+Delete"));
        assert!(joined.contains("Ctrl+C"));
        assert!(joined.contains("Esc"));
        assert!(joined.contains("Ctrl+D"));
        assert!(joined.contains("Alt+Enter / Ctrl+J"));
        assert!(joined.contains("bracketed paste"));
        // (MED-10) Ctrl+O opens an external editor — advertised in /hotkeys.
        assert!(
            joined.contains("Ctrl+O"),
            "/hotkeys must advertise Ctrl+O: {joined}"
        );
        // The inline @-mention file autocomplete — advertised in /hotkeys.
        assert!(
            joined.contains("autocomplete a file to mention"),
            "/hotkeys must advertise the @-mention popup: {joined}"
        );
    }

    #[test]
    fn tree_skip_rules_cover_noisy_directories() {
        assert!(should_skip_tree_entry(".git"));
        assert!(should_skip_tree_entry("target"));
        assert!(should_skip_tree_entry("node_modules"));
        assert!(!should_skip_tree_entry("src"));
    }

    #[test]
    fn recent_git_commits_reads_repo_history() {
        let lines = recent_git_commits_in(Path::new(env!("CARGO_MANIFEST_DIR")), 1).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0]
            .split_whitespace()
            .next()
            .is_some_and(|hash| hash.len() >= 7));
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
    fn compact_command_notes_accepts_only_compact_prefix() {
        assert_eq!(
            compact_command_notes("/compact keep setup"),
            Some("keep setup")
        );
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
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("show --json")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("inspect <event>")));
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
    fn mcp_json_payload_reports_exposure_and_servers() {
        let cfg = LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "docs".to_string(),
                crate::config::McpServerConfig {
                    transport: "stdio".to_string(),
                    command: "npx".to_string(),
                    args: vec![
                        "-y".to_string(),
                        "@modelcontextprotocol/server-docs".to_string(),
                    ],
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
        assert_eq!(
            payload["servers"][0]["target"],
            "npx '-y' '@modelcontextprotocol/server-docs'"
        );
        assert_eq!(payload["servers"][0]["env_vars"], 1);
        assert_eq!(payload["servers"][0]["headers"], 1);
        assert_eq!(payload["servers"][0]["enabled_tools"], 1);
        assert_eq!(payload["servers"][0]["enabled_resources"], 1);
        assert_eq!(payload["servers"][0]["enabled_prompts"], 1);
        assert_eq!(payload["will_write"], false);
        assert_eq!(payload["aliases"][0], "mcp");
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("probe write")));
        assert!(payload["supported_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("settings")));
    }

    #[test]
    fn format_mcp_server_details_lists_cache_without_secret_values() {
        let server = crate::config::McpServerConfig {
            transport: "stdio".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-docs".to_string(),
            ],
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
    fn background_agent_args_target_libertai_or_lcode() {
        let launch = BackgroundAgentLaunch {
            name: "reviewer".to_string(),
            provider: "libertai".to_string(),
            model: "qwen".to_string(),
            mode: Mode::Plan,
            prompt: "Use the task tool".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            agent: None,
            team: None,
            teammate_name: None,
            approval_socket_path: None,
        };
        assert_eq!(
            background_agent_args(Path::new("/usr/bin/libertai"), &launch),
            vec![
                "code",
                "--print",
                "--provider",
                "libertai",
                "--model",
                "qwen",
                "--mode",
                "plan",
                "Use the task tool"
            ]
        );
        assert_eq!(
            background_agent_args(Path::new("/usr/bin/lcode"), &launch),
            vec![
                "--print",
                "--provider",
                "libertai",
                "--model",
                "qwen",
                "--mode",
                "plan",
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
            agent: None,
            team: None,
            teammate_name: None,
            approval_socket_path: None,
        };
        assert_eq!(
            background_agent_args(Path::new("/usr/bin/libertai"), &launch),
            vec!["code", "--print", "--mode", "accept-edits", "Run review"]
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
            agent: None,
            team: None,
            teammate_name: None,
            approval_socket_path: None,
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
                "--print".to_string(),
                "--provider".to_string(),
                "libertai".to_string(),
                "--model".to_string(),
                "qwen".to_string(),
                "--mode".to_string(),
                "plan".to_string(),
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
            team: None,
            teammate_name: None,
        };
        assert_eq!(background_agent_record_id(&record), "bg-99-4242");
        record.run_id = "custom-run".to_string();
        assert_eq!(background_agent_record_id(&record), "custom-run");
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
                team: None,
                teammate_name: None,
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
                team: None,
                teammate_name: None,
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
    fn mention_command_arg_accepts_mention_only() {
        assert_eq!(
            mention_command_arg("/mention src/lib.rs summarize"),
            Some("src/lib.rs summarize")
        );
        assert_eq!(mention_command_arg("/mentions src/lib.rs"), None);
        assert_eq!(mention_command_arg("/mention"), None);
    }

    #[test]
    fn parse_mention_prompt_reuses_quoted_path_parsing() {
        let (path, prompt) = parse_mention_prompt("\"has space.txt\" explain").unwrap();
        assert_eq!(path, PathBuf::from("has space.txt"));
        assert_eq!(prompt, "explain");
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
    fn custom_slash_resolution_rejects_ambiguous_bare_names_and_prefixes() {
        fn command(
            namespace: Option<&str>,
            name: &str,
        ) -> crate::commands::code_slash_registry::CustomCommand {
            crate::commands::code_slash_registry::CustomCommand {
                name: name.to_string(),
                namespace: namespace.map(str::to_string),
                description: None,
                arg_hint: None,
                argument_names: Vec::new(),
                body: "Body".to_string(),
                source: crate::commands::code_slash_registry::CommandSource::Project,
                path: PathBuf::from(format!(
                    ".claude/commands/{}/{}.md",
                    namespace.unwrap_or(""),
                    name
                )),
            }
        }

        let commands = vec![
            command(Some("team"), "audit"),
            command(Some("ops"), "audit"),
            command(None, "apply"),
        ];

        assert!(matches!(
            resolve_custom_slash(&commands, "team/audit"),
            CustomSlashResolve::Hit(hit) if custom_slash_invocation_name(hit) == "team/audit"
        ));
        assert!(matches!(
            resolve_custom_slash(&commands, "apply"),
            CustomSlashResolve::Hit(hit) if custom_slash_invocation_name(hit) == "apply"
        ));
        assert_eq!(
            resolve_custom_slash(&commands, "audit"),
            CustomSlashResolve::Ambiguous(vec!["ops/audit".to_string(), "team/audit".to_string()])
        );
        assert_eq!(
            resolve_custom_slash(&commands, "a"),
            CustomSlashResolve::Ambiguous(vec![
                "apply".to_string(),
                "ops/audit".to_string(),
                "team/audit".to_string(),
            ])
        );
    }

    // ── M7a: OSC52 sequence + external-editor pure helpers ────────────────
    //
    // Hermetic tests for the pure helpers the M7a clipboard/OSC52 +
    // external-editor flow (`code_tui::app`) reuses: `osc52_sequence`
    // (assembles the `\x1b]52;c;<base64>\x07` clipboard-write escape), the
    // `resolve_editor` env-var precedence (`$VISUAL` → `$EDITOR` → `vi`),
    // and `quote_for_sh` / `quote_sh_string` (POSIX sh -c single-quoting). No
    // real terminal, no subprocess. `resolve_editor` reads process-global env
    // vars, so each env-touching test snapshots + restores VISUAL/EDITOR
    // (including their prior unset state) to avoid cross-test pollution.

    /// Snapshot the current VISUAL/EDITOR env state as a pair of
    /// `(Option<String>, Option<String>)` so a test can restore the EXACT
    /// prior state (set vs unset) on exit. Returns (visual, editor).
    fn snapshot_editor_env() -> (Option<String>, Option<String>) {
        (std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
    }

    /// Restore VISUAL/EDITOR to a snapshot taken by `snapshot_editor_env`,
    /// re-establishing the exact prior set/unset state.
    fn restore_editor_env((visual, editor): (Option<String>, Option<String>)) {
        match visual {
            Some(v) => std::env::set_var("VISUAL", v),
            None => std::env::remove_var("VISUAL"),
        }
        match editor {
            Some(v) => std::env::set_var("EDITOR", v),
            None => std::env::remove_var("EDITOR"),
        }
    }

    /// Fully clear VISUAL/EDITOR so `resolve_editor` reaches the `vi` fallback.
    fn clear_editor_env() {
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
    }

    /// Serialize the `resolve_editor_*` tests (which mutate the process-global
    /// VISUAL/EDITOR env vars) so their snapshot/set/assert/restore brackets
    /// don't race each other under parallel `cargo test` threads. The lock is
    /// held across the whole mutation window (snapshot → restore), so at most
    /// one env-touching test observes a stable env at a time.
    static EDITOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // (M7a-u1) `osc52_sequence` produces the well-formed OSC52 clipboard
    // escape: `\x1b]52;c;` + base64(payload) + `\x07` (BEL terminator). The
    // bg `/copy` arm ships this string back as `AgentMsg::Osc52`; the main
    // thread writes the bytes raw to stdout. Pin the exact framing so a
    // terminal reads it as a clipboard write (the `c` selection + BEL
    // terminator are load-bearing — a malformed sequence is silently ignored).
    #[test]
    fn osc52_sequence_emits_well_formed_escape() {
        let seq = osc52_sequence("hi");
        // OSC52 framing: ESC ] 52 ; c ; <base64> BEL
        assert!(
            seq.starts_with("\x1b]52;c;"),
            "OSC52 must start with ESC]52;c; — the 'c' clipboard selection, got {seq:?}"
        );
        assert!(
            seq.ends_with('\x07'),
            "OSC52 must end with BEL (\\x07), got {seq:?}"
        );
        // The middle is the base64 of the payload bytes.
        let middle = &seq["\x1b]52;c;".len()..seq.len() - 1];
        assert_eq!(
            BASE64_STANDARD.decode(middle).unwrap(),
            b"hi",
            "OSC52 payload must base64-decode to the input bytes"
        );
    }

    // (M7a-u2) `osc52_sequence` is the byte-for-byte path the bg `/copy` arm
    // uses; an empty payload still produces valid framing (empty base64).
    // This guards the bare-copy edge case (though the bg arm skips empty text).
    #[test]
    fn osc52_sequence_empty_payload_is_well_formed() {
        let seq = osc52_sequence("");
        assert_eq!(
            seq, "\x1b]52;c;\x07",
            "empty payload → empty base64 + framing"
        );
    }

    // (M7a-u3) `OSC52_MAX_TEXT_BYTES` is the cap the bg `/copy` arm uses to
    // decide "available via osc52" vs "unavailable (too large)". Pin the
    // constant so the size guard doesn't silently change (a regression here
    // would flip the status path).
    #[test]
    fn osc52_max_text_bytes_is_128k() {
        assert_eq!(OSC52_MAX_TEXT_BYTES, 128 * 1024);
    }

    // (M7a-u4) `resolve_editor` precedence: `$VISUAL` wins over `$EDITOR`.
    // Snapshot + restore the env so the test doesn't leak VISUAL/EDITOR into
    // sibling tests. The `EDITOR_ENV_LOCK` serializes the env-touching tests
    // (their mutation windows don't race under parallel test threads).
    #[test]
    fn resolve_editor_visual_wins_over_editor() {
        let _guard = EDITOR_ENV_LOCK.lock().unwrap();
        let snap = snapshot_editor_env();
        std::env::set_var("VISUAL", "visual-editor");
        std::env::set_var("EDITOR", "fallback-editor");
        let resolved = resolve_editor();
        restore_editor_env(snap);
        assert_eq!(
            resolved, "visual-editor",
            "VISUAL must take precedence over EDITOR"
        );
    }

    // (M7a-u6) `resolve_editor` falls back to `vi` when neither `$VISUAL` nor
    // `$EDITOR` is set — the final tier of the precedence chain.
    #[test]
    fn resolve_editor_defaults_to_vi_when_neither_set() {
        let _guard = EDITOR_ENV_LOCK.lock().unwrap();
        let snap = snapshot_editor_env();
        clear_editor_env();
        let resolved = resolve_editor();
        restore_editor_env(snap);
        assert_eq!(
            resolved, "vi",
            "vi is the final fallback when VISUAL and EDITOR are unset"
        );
    }

    // (M7a-u7) `quote_for_sh` single-quotes a path so a space-containing temp
    // path (the Ctrl+O flow's `NamedTempFile` path) round-trips safely through
    // `sh -c "{editor} {quoted_path}"`. Spaces inside the quotes are literal.
    #[test]
    fn quote_for_sh_wraps_path_with_spaces_in_single_quotes() {
        let path = std::path::Path::new("/tmp/some dir/editor draft.txt");
        let quoted = quote_for_sh(path);
        assert!(
            quoted.starts_with('\'') && quoted.ends_with('\''),
            "path must be single-quoted, got {quoted:?}"
        );
        // The inner content is the raw path (no spaces escaped — they're safe
        // inside single quotes).
        let inner = &quoted[1..quoted.len() - 1];
        assert!(
            inner.contains("some dir"),
            "spaces preserved inside quotes: {quoted:?}"
        );
        assert!(
            inner.contains("editor draft.txt"),
            "inner path verbatim: {quoted:?}"
        );
    }

    // (M7a-u8) `quote_sh_string` escapes embedded single quotes via the
    // POSIX `'\''` idiom so a path containing a quote (or any attacker-
    // controlled string) can't break out of the single-quoted context. This
    // is the injection guard for the `sh -c` editor launch.
    #[test]
    fn quote_sh_string_escapes_embedded_single_quotes() {
        let quoted = quote_sh_string("can't break'; rm -rf /; echo '");
        // The result is single-quoted; every embedded `'` became `'\''`.
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        assert!(
            quoted.contains("'\\''"),
            "embedded single quotes must be escaped as '\\'', got {quoted:?}"
        );
        // No unescaped single quote survives inside the quoted region that
        // would terminate the quoting context: the only `'` chars are part of
        // the `'\''` escape or the wrapping quotes.
        let inner = &quoted[1..quoted.len() - 1];
        // After unescaping `'\''` → `'`, the content round-trips to the input.
        let unescaped = inner.replace("'\\''", "'");
        assert_eq!(
            unescaped, "can't break'; rm -rf /; echo '",
            "the escaped string round-trips to the input (no shell breakout)"
        );
    }

    // (M7a-u9) `quote_sh_string` on a plain string with no special chars
    // still wraps it in single quotes (the consistent quoting the editor
    // launch relies on — no conditional quoting that an empty/space path
    // could bypass).
    #[test]
    fn quote_sh_string_wraps_plain_string_in_single_quotes() {
        assert_eq!(quote_sh_string("plain"), "'plain'");
        assert_eq!(
            quote_sh_string(""),
            "''",
            "empty string → empty single-quotes"
        );
        // A path with a `$` (shell-significant) is neutralized by the quotes.
        let quoted = quote_sh_string("/tmp/$HOME/x");
        assert_eq!(
            quoted, "'/tmp/$HOME/x'",
            "dollar is literal inside single quotes"
        );
    }

    // ── R2-COV-1: restored behavioral tests for the round-2-purged helpers ─
    //
    // The round-2 dead-code purge (dfe99c9) deleted ~155 behavioral tests that
    // exercised LIVE functions. These tests restore coverage for the helpers
    // that still exist + have live (non-test) callers. Each test was recovered
    // from `git show dfe99c9^:src/commands/code_ui.rs` and adapted to the
    // current signatures; assertions referencing helpers that were ALSO
    // deleted in the purge (e.g. `*_command_arg` arg extractors,
    // `help_command_arg_hint`, `tree_usage_text`, `count_runnable_hooks`) were
    // dropped rather than restored against deleted code.

    // (R2-COV-1) `parse_pr_comments_thread` + `parse_pr_comment_draft` are
    // the live `/pr_comments thread` parser chain (`stage_pr_comment_draft` →
    // `parse_pr_comment_draft` → `parse_pr_comments_thread`, exported to
    // app.rs). SAFETY-RELEVANT: the thread parser splits `<path>:<line>
    // <body>` and rejects a missing `:`, a zero/missing line, or an empty
    // body — pin the happy path + every rejection so a malformed target can't
    // stage a draft on the wrong line.
    #[test]
    fn parse_pr_comments_thread_requires_target_line_and_body() {
        // Happy path: `<path>:<line> <body>` → (path, line, body).
        assert_eq!(
            parse_pr_comments_thread("src/lib.rs:42 Needs a test.").unwrap(),
            ("src/lib.rs", 42, "Needs a test.")
        );
        // Missing `:line` → the target has no `:` separator → reject.
        assert!(parse_pr_comments_thread("src/lib.rs Needs a test.").is_err());
        // Zero line is not a positive integer → reject.
        assert!(parse_pr_comments_thread("src/lib.rs:0 Needs a test.").is_err());
        // Missing body (only the target token) → reject.
        assert!(parse_pr_comments_thread("src/lib.rs:42").is_err());
        // `parse_pr_comment_draft` wraps the triple into a `PrCommentDraft`.
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

    // (R2-COV-1) `parse_changelog_limit` is the live `/changelog` limit
    // parser (called from app.rs): empty + the default-list aliases yield
    // `CHANGELOG_DEFAULT_LIMIT`, a number is clamped to `[1, CHANGELOG_MAX_LIMIT]`,
    // and a non-numeric word is a usage error.
    #[test]
    fn parse_changelog_limit_defaults_and_clamps() {
        assert_eq!(parse_changelog_limit("").unwrap(), CHANGELOG_DEFAULT_LIMIT);
        assert_eq!(
            parse_changelog_limit("list").unwrap(),
            CHANGELOG_DEFAULT_LIMIT
        );
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
    }

    // (R2-COV-1) `changelog_json_request_arg` + `changelog_json_payload` +
    // `changelog_usage_text` are the live `/changelog --json` plumbing
    // (called from app.rs). The arg extractor recognizes the bare `json`/
    // `--json` + the `* --json` aliases (returning the empty arg = no extra)
    // and `json <n>`/`--json <n>` (returning the limit); the payload reports
    // the surface/command/limit/commits the terminal renderer expects.
    #[test]
    fn changelog_json_helpers_match_terminal_contract() {
        assert!(changelog_usage_text().contains("list|recent|latest"));
        assert!(changelog_usage_text().contains("status|state|show"));
        assert!(changelog_usage_text().contains("json|--json|status --json"));
        assert!(changelog_usage_text().contains("state --json|show --json"));
        assert!(changelog_usage_text().contains("list --json|recent --json|latest --json"));
        assert_eq!(changelog_json_request_arg("json"), Some(String::new()));
        assert_eq!(changelog_json_request_arg("--json"), Some(String::new()));
        assert_eq!(
            changelog_json_request_arg("state --json"),
            Some(String::new())
        );
        assert_eq!(
            changelog_json_request_arg("show --json"),
            Some(String::new())
        );
        assert_eq!(
            changelog_json_request_arg("list --json"),
            Some(String::new())
        );
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
        assert_eq!(
            payload["commits"][1]["summary"],
            "(HEAD -> main) second commit"
        );
    }

    // (R2-COV-1) `parse_forget_command` + `forget_usage_text` +
    // `forget_json_payload` are the live `/forget` parse + JSON plumbing
    // (dispatched from app.rs). Status/preview → `Status`, the `json`/`--json`
    // aliases → `Json`, an unknown word → `Usage`. The payload reports the
    // approvals state the terminal renderer surfaces.
    #[test]
    fn forget_command_parser_and_payload_match_terminal_contract() {
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

    // (R2-COV-1) `parse_notify_command` + `notify_usage_text` +
    // `notify_json_payload` are the live `/notify` parse + JSON plumbing
    // (dispatched from app.rs). Empty + status/state/show → `Status`, the
    // `json`/`--json` aliases → `Json`, on/enable/enabled → `On`,
    // off/disable/disabled/clear → `Off`, test/ping → `Test`, unknown →
    // `Usage`. The payload echoes the `code_turn_notifications` flag.
    #[test]
    fn notify_command_parser_and_payload_match_terminal_contract() {
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

    // (R2-COV-1) `parse_hooks_command` is the live `/hooks` parser
    // (dispatched from app.rs). Empty/list/diagnostics → `Status`, the
    // `json`/`--json` aliases → `Json`, open/settings/edit → `Open`,
    // `show <event>`/`inspect <event>` → `Show(event)`, bare `show` or a
    // multi-word `show` w/o a single event → `Usage`. `HOOKS_USAGE` carries
    // the terminal-contract substrings.
    #[test]
    fn hooks_command_parser_matches_terminal_contract() {
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
    }

    // (R2-COV-1) `parse_mcp_command` is the live `/mcp` parser (dispatched
    // from app.rs). Empty/list/diagnostics → `Status`, the `json`/`--json`
    // aliases → `Json`, `show <server>`/`inspect <server>` → `Show(server)`,
    // probe → `Probe`, `probe --save`/`probe --write`/`refresh` → `ProbeSave`,
    // reset/reset-sessions → `Reset`, open/settings/edit → `Open`, unknown →
    // `Usage`. `MCP_USAGE` carries the terminal-contract substrings.
    #[test]
    fn mcp_command_parser_matches_terminal_contract() {
        assert_eq!(parse_mcp_command(""), McpCommand::Status);
        assert_eq!(parse_mcp_command("list"), McpCommand::Status);
        assert_eq!(parse_mcp_command("json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("--json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("status --json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("list --json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("state --json"), McpCommand::Json);
        assert_eq!(parse_mcp_command("diagnostics --json"), McpCommand::Json);
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
    }

    // (R2-COV-1) `parse_vim_command` + `vim_json_payload` are the live
    // `/vim` parse + JSON plumbing (dispatched from app.rs). Empty/status/
    // current/info → `Status`, the `json`/`--json` aliases → `Json`,
    // on/enable/enabled/true → `Enable`, off/disable/disabled/false →
    // `Disable`, unknown → `Usage`. The payload echoes the global
    // `VIM_INPUT_ENABLED` flag (snapshot/restore to avoid cross-test
    // pollution). `VIM_USAGE` carries the terminal-contract substrings.
    #[test]
    fn vim_command_parser_and_payload_match_terminal_contract() {
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
        // `vim_json_payload` reads the process-global flag — snapshot + set
        // + restore so the assertion is hermetic and doesn't leak into
        // sibling tests.
        let prior = VIM_INPUT_ENABLED.load(Ordering::SeqCst);
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
        VIM_INPUT_ENABLED.store(prior, Ordering::SeqCst);
    }

    // (R2-COV-1) `parse_ide_command` + `ide_json_payload` are the live
    // `/ide` parse + JSON plumbing (dispatched from app.rs). Empty/status/
    // state/show → `Status`, the `json`/`--json` aliases → `Json`,
    // open/settings/edit → `Open`, unknown → `Usage`. `IDE_USAGE` carries
    // the terminal-contract substrings.
    #[test]
    fn ide_command_parser_and_payload_match_terminal_contract() {
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

    // (R2-COV-1) `parse_bug_command` + `bug_json_payload` are the live
    // `/bug` parse + JSON plumbing (dispatched from app.rs). Empty/report/
    // template/status/show → `Template`, the `json`/`--json` aliases →
    // `Json`, unknown → `Usage`. `BUG_USAGE` carries the terminal-contract
    // substrings; the payload reports the session's provider/model/mode/
    // output-style for the bug template.
    #[test]
    fn bug_command_parser_and_payload_match_terminal_contract() {
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

    // (R2-COV-1) `parse_hotkeys_command` + `hotkeys_usage_text` +
    // `hotkeys_json_payload` are the live `/hotkeys` parse + JSON plumbing
    // (dispatched from app.rs). Empty/list/help → `Show`, the `json`/`--json`
    // aliases → `Json`, unknown → `Usage`. The payload lists the `hotkey_lines`
    // shortcuts (including Shift+Tab).
    #[test]
    fn hotkeys_command_parser_and_payload_match_terminal_contract() {
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
        assert!(payload["shortcuts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "Shift+Tab"));
    }

    // (R2-COV-1) `parse_theme_command` + `theme_json_payload` are the live
    // `/theme` parse + JSON plumbing (dispatched from app.rs). Empty/status/
    // show/current/info → `Status`, the `json`/`--json` aliases → `Json`,
    // a known theme name → `Requested(name)`.
    #[test]
    fn theme_command_parser_and_payload_match_terminal_contract() {
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

    // (R2-COV-1) `normalized_hook_type` normalizes the Claude-MCP hook-type
    // spellings (`mcp-tool`/`mcptool`) to the canonical `mcp_tool` key used
    // by the hook grouping/section text. It is called from 6+ live sites in
    // the hook-rendering path; pin the normalization + the passthrough for
    // an already-canonical value.
    #[test]
    fn normalized_hook_type_accepts_claude_mcp_spellings() {
        assert_eq!(normalized_hook_type("mcp-tool"), "mcp_tool");
        assert_eq!(normalized_hook_type("mcptool"), "mcp_tool");
        assert_eq!(normalized_hook_type("MCP_TOOL"), "mcp_tool");
        assert_eq!(normalized_hook_type("Prompt"), "prompt");
    }

    // (R2-COV-1) `custom_slash_invocation_name` + `custom_slash_starts_with`
    // + `resolve_custom_slash` are the live custom-slash matching helpers
    // (called from app.rs's slash palette + the custom-slash resolve path).
    // A namespaced command's invocation name is `<namespace>/<name>`; a bare
    // command's is just `<name>`. `resolve_custom_slash` resolves a unique
    // exact-invocation or exact-name match, and reports `Ambiguous(sorted)`
    // when a bare name/prefix matches multiple commands.
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
        assert!(custom_slash_starts_with(&command, "team/aud"));
    }

    // (R2-COV-1) `model_list_source` is the live `/model --json` source URL
    // builder (called from app.rs): it formats `<api_base>/v1/models`, trimming
    // a trailing slash from the configured `api_base`.
    #[test]
    fn model_list_source_formats_api_base_models_endpoint() {
        let cfg = LibertaiConfig::default();
        assert_eq!(
            model_list_source(&cfg),
            format!("{}/v1/models", cfg.api_base.trim_end_matches('/'))
        );
        assert_eq!(
            model_list_source(&cfg),
            "https://api.libertai.io/v1/models",
            "default api_base → the canonical /v1/models endpoint"
        );
        // A trailing slash on a custom api_base must not produce `//v1/models`.
        let cfg_slash = LibertaiConfig {
            api_base: "https://example.com/api/".to_string(),
            ..LibertaiConfig::default()
        };
        assert_eq!(
            model_list_source(&cfg_slash),
            "https://example.com/api/v1/models"
        );
    }

    // (R2-COV-1) `render_project_tree` + `tree_json_request_arg` +
    // `project_tree_json_payload` are the live `/tree` plumbing (called from
    // app.rs): the renderer prints directories before files + skips the
    // noise dirs (target/.git/node_modules/...), the arg extractor
    // recognizes the `json`/`--json` aliases + `json <path>`/`<path> --json`,
    // and the payload reports the surface/command/entries the terminal
    // renderer expects.
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
        assert_eq!(
            tree_json_request_arg("MyDir --json"),
            Some("MyDir".to_string())
        );
        assert_eq!(tree_json_request_arg("src"), None);
        let payload = project_tree_json_payload(temp.path(), 20, "src --json").unwrap();
        assert_eq!(payload["surface"], "terminal");
        assert_eq!(payload["command"], "tree");
        assert_eq!(payload["query"], "src --json");
        assert_eq!(payload["aliases"][0], "tree");
        assert_eq!(payload["supported_actions"][1], "--json");
        assert_eq!(payload["supported_actions"][2], "status --json");
        assert_eq!(payload["supported_actions"][5], "path --json");
        assert_eq!(payload["entries"][0]["kind"], "dir");
        assert!(payload["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "main.rs"));
    }

    // (R2-COV-2) `resolve_editor` falls back to `$EDITOR` when `$VISUAL` is
    // unset — the middle tier of the precedence chain (siblings cover
    // VISUAL>EDITOR + vi-fallback). Snapshot + restore the env so the test
    // doesn't leak VISUAL/EDITOR into sibling tests; the EDITOR_ENV_LOCK
    // serializes the env-touching tests.
    #[test]
    fn resolve_editor_falls_back_to_editor_when_visual_unset() {
        let _guard = EDITOR_ENV_LOCK.lock().unwrap();
        let snap = snapshot_editor_env();
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", "nano");
        let resolved = resolve_editor();
        restore_editor_env(snap);
        assert_eq!(resolved, "nano", "EDITOR must be used when VISUAL is unset");
    }

    // (R2-COV-3) `usage_summary` tracks the context-token high-water mark
    // (max input across records) + the summed output tokens over a multi-
    // record slice, plus the last record's input/output + the last record's
    // context window/provider/model. Only the empty test
    // (`usage_summary_empty_when_no_turns`) survived the purge; this
    // restores the multi-record coverage.
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

    // ---- @-mention expansion + candidate walk ----

    #[test]
    fn expand_at_mentions_inlines_existing_relative_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("notes.txt"), "hello world").unwrap();
        let out = expand_at_mentions("see @notes.txt please", temp.path());
        assert!(
            out.starts_with("see @notes.txt please"),
            "the typed prompt must be preserved untouched at the front: {out:?}"
        );
        assert!(out.contains("Mentioned file: `notes.txt`"));
        assert!(out.contains("hello world"));
    }

    #[test]
    fn expand_at_mentions_skips_missing_absolute_and_parent_paths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("escape.txt"), "x").unwrap();
        // `@nope.txt` doesn't exist; `@/etc/hostname` is absolute;
        // `@../escape.txt` has a parent component (even though the file
        // exists one level up); `user@host` has no word-leading `@`.
        let sub = temp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let prompt = "ping @nope.txt @/etc/hostname @../escape.txt user@host";
        assert_eq!(
            expand_at_mentions(prompt, &sub),
            prompt,
            "nothing expandable must leave the prompt byte-identical"
        );
    }

    #[test]
    fn expand_at_mentions_strips_trailing_punctuation_and_dedups() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "alpha").unwrap();
        let out = expand_at_mentions("read @a.txt, then @a.txt again", temp.path());
        assert_eq!(
            out.matches("Mentioned file: `a.txt`").count(),
            1,
            "punctuation-trimmed + raw mentions of one file attach once: {out:?}"
        );
    }

    #[test]
    fn expand_at_mentions_skips_oversized_and_caps_file_count() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("big.txt"),
            vec![b'x'; MENTION_ATTACHMENT_MAX_BYTES + 1],
        )
        .unwrap();
        let prompt = "look @big.txt";
        assert_eq!(
            expand_at_mentions(prompt, temp.path()),
            prompt,
            "an over-cap file must not attach"
        );

        let mut prompt = String::from("many:");
        for i in 0..(MENTION_EXPAND_MAX_FILES + 3) {
            std::fs::write(temp.path().join(format!("f{i}.txt")), "x").unwrap();
            prompt.push_str(&format!(" @f{i}.txt"));
        }
        let out = expand_at_mentions(&prompt, temp.path());
        assert_eq!(
            out.matches("Mentioned file: ").count(),
            MENTION_EXPAND_MAX_FILES,
            "attachment count must cap at MENTION_EXPAND_MAX_FILES"
        );
    }

    #[test]
    fn mention_candidates_respects_gitignore_marks_dirs_and_sorts_shallow_first() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(temp.path().join("ignored.txt"), "x").unwrap();
        std::fs::write(temp.path().join("keep.txt"), "x").unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        std::fs::write(temp.path().join("sub").join("inner.txt"), "x").unwrap();

        let got = mention_candidates(temp.path());
        assert!(got.contains(&"keep.txt".to_string()), "{got:?}");
        assert!(
            got.contains(&"sub/".to_string()),
            "dirs must carry a trailing slash: {got:?}"
        );
        assert!(got.contains(&"sub/inner.txt".to_string()), "{got:?}");
        assert!(
            !got.iter().any(|p| p.contains("ignored.txt")),
            ".gitignore must be honored even without a .git dir (require_git(false)): {got:?}"
        );
        let pos_keep = got.iter().position(|p| p == "keep.txt").unwrap();
        let pos_inner = got.iter().position(|p| p == "sub/inner.txt").unwrap();
        assert!(
            pos_keep < pos_inner,
            "shallow entries must sort before deep ones: {got:?}"
        );
    }
}
