//! Shared slash-command router adapters for the `libertai code` surfaces.
//!
//! After the ratatui migration, the interactive REPL lives in `code_tui::app`
//! and the one-shot rendering path lives in `code_ui`. Both surfaces need to
//! dispatch slash commands (`/model list`, `/skills`, `/memory show`, custom
//! template commands, and the `!`/`!!` shell escape) without re-implementing
//! the legacy parsing logic that still lives in `code_ui`.
//!
//! This module is the shared, **non-printing** boundary between the two
//! surfaces: it reuses `code_ui`'s `pub(crate)` pure-logic handlers (model
//! listing, custom-slash resolution, shell escape execution) and the pure data
//! readers in `code_skills` / `code_memory`, but instead of printing it
//! returns typed [`SlashOutcome`]s and plain-text strings that each surface
//! can render however it likes.
//!
//! Design:
//! - [`SlashOutcome`] types the result of a slash dispatch so a caller can
//!   route cleanly: `Render(text)` pushes a system transcript entry,
//!   `SendPrompt(text)` sends a prompt (custom template expansion), `RunOnBg`
//!   hands a [`BgCommand`] to the background thread (whose result arrives via
//!   `AgentMsg::CommandResult`), and `None` means handled with no output.
//! - The text-building adapters (`model_list_text`, `skills_list_text`,
//!   `memory_show_text`) are pure-ish (read-only I/O only) and return
//!   `String`s — the TUI runs them on the background thread via the matching
//!   [`BgCommand`] variant and renders the returned string as a system
//!   transcript entry.
//! - The custom-command split: [`resolve_custom`] is **synchronous** and only
//!   detects a hit / ambiguity / not-found against a cached command list (no
//!   session state needed). The actual template expansion is **async** (it
//!   needs the `AgentSessionHandle` for `${session_id}`/`${effort}` context),
//!   so it runs on the background thread as [`BgCommand::CustomPrompt`]. The
//!   TUI detects the hit synchronously, then sends the prompt for the bg
//!   thread to expand with `code_ui::build_custom_slash_prompt`.
//! - Shell escape: [`run_shell_escape_tui`] is the non-printing twin of
//!   `code_ui::run_shell_escape` — it calls `code_ui::execute_shell_escape`
//!   (which spawns the shell and captures stdout/stderr/exit) and returns a
//!   [`ShellEscapeTuiResult`] the caller renders as transcript lines, plus the
//!   prompt-context string for `pending_shell_contexts`.
//!
//! Nothing in this module prints. All rendering is the caller's job.

use std::path::Path;

use crate::commands::code_factory::Mode;
use crate::commands::code_skills::SkillPillar;
use crate::commands::code_ui;
use crate::commands::code_slash_registry::CustomCommand;
use crate::config::Config as LibertaiConfig;

/// The typed outcome of a slash-command dispatch.
///
/// The TUI's `handle_slash_command` maps each variant to transcript entries
/// and/or `Cmd`s:
/// - [`SlashOutcome::Render`] → push as `TranscriptEntry::System` (plus a
///   `TranscriptEntry::Blank` separator).
/// - [`SlashOutcome::SendPrompt`] → send via `Cmd::Prompt` (a custom template
///   expansion has already produced the final prompt text).
/// - [`SlashOutcome::RunOnBg`] → hand the [`BgCommand`] to the background
///   thread; its result string arrives via `AgentMsg::CommandResult` and is
///   pushed as a system transcript entry.
/// - [`SlashOutcome::None`] → handled with no output (e.g. `/clear` already
///   acted by clearing the transcript / sending `Cmd::Clear`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashOutcome {
    /// Render this text as a system transcript entry (+ blank separator).
    Render(String),
    /// Send this prompt to the pi session via `Cmd::Prompt`.
    SendPrompt(String),
    /// Run a read-only command on the background thread; the result arrives
    /// via `AgentMsg::CommandResult`.
    RunOnBg(BgCommand),
    /// Handled with no output.
    None,
}

/// Read-only commands that need background-thread data (API calls, async
/// session state, or filesystem reads that shouldn't block the event loop).
///
/// The background thread dispatches on this enum, invokes the matching
/// [`code_slash_router`](self) adapter, and sends the resulting `String` back
/// to the main thread via `AgentMsg::CommandResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BgCommand {
    /// `/usage` / `/cost` — render the session usage summary + tool activity.
    /// (Requires the live `AgentSessionHandle`'s `UsageRecord`s.)
    Usage,
    /// `/doctor` — environment / config sanity check.
    Doctor,
    /// `/model list` — fetch the model catalog and render a text listing.
    /// Carries the scoped-model glob patterns to filter the listing.
    ModelList {
        /// Glob patterns restricting the listing (empty = all models).
        scoped_patterns: Vec<String>,
    },
    /// A custom slash template command. The bg thread expands the template
    /// against the live `AgentSessionHandle` (async) and sends the resulting
    /// prompt back; the TUI then submits it via `Cmd::Prompt`.
    ///
    /// (Batch B replaces the temporary `String::new()` stub with the real
    /// template expansion.)
    CustomPrompt {
        /// Command name (without leading `/`), already resolved to a hit by
        /// [`resolve_custom`].
        name: String,
        /// Raw argument string to interpolate into the template.
        args: String,
    },
    /// `/compact [notes]` — force-compaction of the conversation history now,
    /// optionally with user-supplied summarization notes. Runs on the bg
    /// thread (it needs the live `AgentSessionHandle` and emits compaction
    /// `AgentEvent`s, which `translate_event` maps to `AgentMsg::System` so
    /// compaction progress surfaces in the transcript with no new render
    /// code). `notes` carries the free-form instruction string; `None`/empty
    /// means compaction with no extra instructions. The status/json/preview
    /// sub-commands of `/compact` are handled synchronously on the main thread
    /// (they only need the config), so they never produce this variant.
    Compact {
        /// Optional user notes / summarization instructions for the
        /// compaction pass. `None` or empty → no extra instructions.
        notes: Option<String>,
    },
    /// `/changelog [count|json]` — render recent git commits as text or
    /// JSON. Runs on the bg thread because `recent_git_commits_in` shells
    /// out to `git log` (blocking I/O). The result text is rendered as a
    /// `CommandResult` system entry.
    Changelog {
        /// Parsed commit limit (clamped to `CHANGELOG_*_LIMIT`).
        limit: usize,
        /// Whether to emit the JSON payload instead of the text listing.
        json: bool,
    },
    /// `/tree [path|json]` — render the project tree as text or JSON. Runs on
    /// the bg thread because `render_project_tree` walks the filesystem
    /// (blocking I/O). The result text is rendered as a `CommandResult`
    /// system entry.
    Tree {
        /// Optional subdirectory to root the tree at (`None` = cwd).
        path: Option<String>,
        /// Whether to emit the JSON payload instead of the text tree.
        json: bool,
    },
    /// `/pr_comments [scope]` (bare inspect) — collect the GitHub PR snapshot
    /// (blocking `gh` calls) on the bg thread and build the inspection
    /// prompt. Unlike the other variants, the result is a **prompt**, shipped
    /// back as `AgentMsg::PromptReady` (the main thread submits it as a turn),
    /// NOT a `CommandResult` render. `scope` is the free-form PR selector
    /// (number/URL/branch hint; empty = infer the current branch's PR).
    PrCommentsInspect {
        /// Free-form PR scope passed verbatim to `build_pr_comments_prompt`.
        scope: String,
    },
    /// `/copy [status|info|json]` — copy the last assistant response to the
    /// terminal clipboard via OSC52, or report copy status. Runs on the bg
    /// thread because it needs the live `AgentSessionHandle` (the transcript
    /// is only owned there). For the bare copy, the bg arm READS the assistant
    /// text and ships the OSC52 SEQUENCE STRING back as `AgentMsg::Osc52`
    /// (the OSC52 WRITE must be main-thread — a bg `print!` would race the
    /// frame buffer since the bg stdout is shared with `terminal.draw`); the
    /// status/info/json subcommands return a status string that rides back as
    /// a `CommandResult` system line (status IS a transcript entry). `query`
    /// is the raw subcommand remainder (empty = bare copy / `last`).
    Copy {
        /// Raw subcommand remainder after `/copy` (`""` for the bare copy,
        /// `"status"`/`"info"`/`"json"` for the introspection variants).
        query: String,
    },
    /// `/diff [path]` — render the uncommitted diff vs HEAD. Runs on the bg
    /// thread because `git_diff_in` shells out to `git diff` (blocking I/O).
    /// Unlike the `CommandResult` variants, the raw diff is NOT a transcript
    /// line: the bg arm ships it back as `AgentMsg::DiffReady`, and the main
    /// thread stashes it on `App::pending_diff` and opens the `DiffView`
    /// overlay (which parses + styles it via `code_tui::diff::parse_diff`).
    /// `path` is the optional `-- <path>` filter (`None` = all changed files).
    Diff {
        /// Optional pathspec limiting the diff (`None` = full working-tree
        /// diff vs HEAD).
        path: Option<String>,
    },
    /// `/commit [message]` — stage all changes (`git add -A`) and create a
    /// git commit. This is a **blocking + mutating** subprocess, so it MUST
    /// run on the bg thread (the main thread owns the render loop; the bg
    /// thread already runs blocking git via `BgCommand::Changelog`/`Tree`).
    /// The result text rides back as a `CommandResult` system line
    /// (RENDERED). `add_all` stages the full working tree before committing
    /// (the minimal-cut `/commit <message>` path always stages everything);
    /// the bare `/commit` arm builds a prompt instead (see
    /// `handle_slash_command`).
    Commit {
        /// Conventional commit message body for `git commit -m <message>`.
        message: String,
        /// When true, run `git add -A` before `git commit -m <message>`
        /// (stage the entire working tree).
        add_all: bool,
    },
}

/// Result of a non-printing shell-escape run, for the TUI to render as
/// transcript lines (a `$ command` header followed by stdout, stderr, and the
/// exit status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellEscapeTuiResult {
    /// The command line that was run (for the `$ command` header).
    pub command: String,
    /// Captured stdout (already truncated to the display byte budget by
    /// `code_ui::execute_shell_escape`).
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Process exit code, if the shell exited normally.
    pub exit_code: Option<i32>,
    /// Prompt-context string for `pending_shell_contexts` (the
    /// `code_ui::shell_escape_prompt_context` block), so the TUI can append
    /// it to the pending shell contexts the next prompt carries.
    pub prompt_context: String,
}

/// Outcome of synchronously resolving a custom slash command against a cached
/// command list (mirrors `code_ui::resolve_custom_slash` but with a public
/// result type, since `code_ui`'s `CustomSlashResolve` is module-private).
///
/// This is the **synchronous** half of the custom-command split: it detects a
/// hit / ambiguity / not-found without any session state. The async template
/// expansion runs later on the background thread (see [`BgCommand::CustomPrompt`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomResolveResult<'a> {
    /// Unique match — the resolved command (the caller expands it async).
    Hit(&'a CustomCommand),
    /// No matching command.
    NotFound,
    /// Multiple commands match the prefix; list the invocation names so the
    /// user can disambiguate.
    Ambiguous(Vec<String>),
}

/// Build a plain-text model listing for `/model list`, reusing `code_ui`'s
/// `model_list_source` / `model_list_provider` and the model catalog.
///
/// Mirrors what `code_ui::print_model_list` prints, but returns the string
/// instead of printing it. When the model fetch fails, the returned string
/// carries the error (prefixed like the legacy `eprintln`) so the caller can
/// render it uniformly as a system transcript entry.
///
/// This performs a network call (`crate::client::list_models`) and should be
/// run on the background thread.
pub fn model_list_text(cfg: &LibertaiConfig, scoped_patterns: &[String]) -> String {
    let provider = code_ui::model_list_provider();
    let source = code_ui::model_list_source(cfg);
    match crate::client::list_models(cfg) {
        Ok(list) => {
            let ids: Vec<String> = list.data.into_iter().map(|entry| entry.id).collect();
            let scoped = scoped_model_ids(provider, &ids, scoped_patterns);
            let mut out = String::new();
            out.push_str("models\n");
            if !scoped_patterns.is_empty() {
                out.push_str(&format!("  scope: {}\n", scoped_patterns.join(", ")));
            }
            out.push_str(&format!("  source: {source}\n"));
            for id in &scoped {
                out.push_str(&format!("  - {provider}/{id}\n"));
            }
            out
        }
        Err(e) => format!("  /model list: {e:#}"),
    }
}

/// Render the active code-pillar skill inventory as text for `/skills`,
/// reusing `code_skills::skill_inventory` (`SkillPillar::Code`).
///
/// One line per skill: name, enabled state, source kind, and (when present) a
/// short description. Pure read-only I/O; safe to run on either thread, but
/// exposed as a [`BgCommand`] variant for uniform dispatch.
pub fn skills_list_text() -> String {
    let cwd = std::env::current_dir().ok();
    match crate::commands::code_skills::skill_inventory(SkillPillar::Code, cwd.as_deref()) {
        Ok(entries) => {
            let mut out = String::new();
            if entries.is_empty() {
                out.push_str("skills: none active for this project.\n");
                return out;
            }
            out.push_str("skills\n");
            for entry in entries {
                let state = if entry.enabled { "on" } else { "off" };
                let name_line = format!("  - {} [{}]", entry.name, state);
                match entry.description.trim() {
                    "" => out.push_str(&format!("{name_line}\n")),
                    desc => out.push_str(&format!("{name_line} — {desc}\n")),
                }
            }
            out
        }
        Err(e) => format!("  /skills: {e:#}"),
    }
}

/// Render the current project memory state as text for `/memory show`,
/// reusing `code_memory::memory_file_for` / `read_memory`.
///
/// Lists the memory file path and, when the file exists, a short preview of
/// its contents (first non-empty lines). Pure read-only I/O.
pub fn memory_show_text() -> String {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return format!("  /memory show: could not resolve cwd: {e}"),
    };
    match crate::commands::code_memory::read_memory(&cwd) {
        Ok(doc) => {
            let mut out = String::new();
            out.push_str("memory\n");
            out.push_str(&format!("  path: {}\n", doc.path.display()));
            if !doc.exists {
                out.push_str("  (no memory file yet — use /remember <text> to add a note)\n");
                return out;
            }
            let trimmed = doc.content.trim();
            if trimmed.is_empty() {
                out.push_str("  (memory file is empty)\n");
            } else {
                let preview = preview_lines(trimmed, 8);
                out.push_str("  preview:\n");
                for line in preview {
                    out.push_str(&format!("    {line}\n"));
                }
            }
            out
        }
        Err(e) => format!("  /memory show: {e:#}"),
    }
}

// ── M6a: pure status/template text builders for the main-thread slash arms ──
//
// Each mirrors the matching `print_*` body in `code_ui` but returns the text
// instead of printing it (no ANSI escapes — the TUI renders these as a plain
// `TranscriptEntry::System` line). The router's main-thread `handle_slash`
// arms call these for the `status`/empty sub-commands and the bumped
// `*_json_payload` for the `json` sub-commands.

/// `/ide` status text (mirrors `print_ide_status` Status branch).
pub fn ide_status_text() -> String {
    let mut out = String::new();
    out.push_str("ide\n");
    out.push_str("  status: no dedicated VS Code / JetBrains integration is bundled today.\n");
    out.push_str(
        "  terminal: run libertai code inside your project, or use the desktop workspace for project navigation.\n",
    );
    out
}

/// `/ide` open hint (mirrors `print_ide_status` Open branch).
pub fn ide_open_text() -> String {
    let mut out = String::new();
    out.push_str("ide\n");
    out.push_str("  /ide open: no IDE bridge is available to open from the terminal CLI yet.\n");
    out.push_str(
        "  desktop: use the desktop app workspace and external editor integration for project files.\n",
    );
    out
}

/// `/ide` usage text.
pub fn ide_usage_text() -> String {
    format!("  usage: {}\n", code_ui::IDE_USAGE)
}

/// `/hotkeys` status text (mirrors `print_hotkeys`, folding the bumped
/// `hotkey_lines`).
pub fn hotkeys_text() -> String {
    let mut out = String::new();
    out.push_str("hotkeys\n");
    for line in code_ui::hotkey_lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

/// `/hotkeys` usage text.
pub fn hotkeys_usage_text() -> String {
    format!("  usage: {}\n", code_ui::hotkeys_usage_text())
}

/// `/theme` status text (mirrors `print_theme_status` Status branch).
pub fn theme_status_text() -> String {
    let mut out = String::new();
    out.push_str("theme\n");
    out.push_str("  desktop: /theme system|dark|light|high-contrast updates the app appearance.\n");
    out.push_str("  terminal: colors are controlled by your terminal emulator; libertai code uses ANSI styling only.\n");
    out.push_str("  status aliases: /theme status, /theme show, /theme current, /theme info, /theme json\n");
    out
}

/// `/theme` requested text (mirrors `print_theme_status` Requested branch).
pub fn theme_requested_text(requested: &str) -> String {
    if requested.is_empty() {
        return theme_status_text();
    }
    format!("theme\n  requested theme: {requested}\n")
}

/// `/vim` status text (mirrors `print_vim_status` Status branch).
pub fn vim_status_text() -> String {
    let mut out = String::new();
    out.push_str("vim\n");
    out.push_str(&format!(
        "  status: {}\n",
        if code_ui::vim_input_enabled() { "on" } else { "off" }
    ));
    out.push_str(
        "  terminal: Vim input supports insert/normal mode: Esc, i/a/I/A, h/l/0/$, x, and Enter.\n",
    );
    out
}

/// `/vim` usage text.
pub fn vim_usage_text() -> String {
    format!("  usage: {}\n", code_ui::VIM_USAGE)
}

/// `/bug` template text (mirrors `print_bug_template`), threading the live
/// provider/model/mode/output-style from the App's status bar.
pub fn bug_template_text(provider: &str, model: &str, mode: Mode, output_style: Option<&str>) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let mut out = String::new();
    out.push_str("bug report\n");
    out.push_str("Include this diagnostic block with the issue:\n");
    out.push('\n');
    out.push_str("- app: libertai-cli\n");
    out.push_str("- branch: integrated-code\n");
    out.push_str(&format!("- provider: {provider}\n"));
    out.push_str(&format!("- model: {model}\n"));
    out.push_str(&format!("- mode: {}\n", code_ui::mode_label(mode)));
    out.push_str(&format!("- output-style: {}\n", output_style.unwrap_or("default")));
    out.push_str(&format!("- cwd: {cwd}\n"));
    out.push('\n');
    out.push_str("Describe:\n");
    out.push_str("- What you expected\n");
    out.push_str("- What happened\n");
    out.push_str("- The last command or prompt you ran\n");
    out.push_str("- Whether it reproduces in a fresh `libertai code` session\n");
    out.push('\n');
    out
}

/// `/bug` usage text.
pub fn bug_usage_text() -> String {
    let mut out = String::new();
    out.push_str("bug report\n");
    out.push_str(&format!("  usage: {}\n", code_ui::BUG_USAGE));
    out
}

/// `/hooks` status text (mirrors `print_hooks_status`): one section per
/// hook event, then the footer notes. Reuses the bumped `hook_event_rows` +
/// `hook_section_text`.
pub fn hooks_status_text(cfg: &LibertaiConfig) -> String {
    let mut out = String::new();
    out.push_str("hooks\n");
    for (event, hooks) in code_ui::hook_event_rows(cfg) {
        out.push_str(&code_ui::hook_section_text(event, hooks));
    }
    out.push_str("  UserPromptSubmit hooks run before the prompt reaches the agent and may block it.\n");
    out.push_str("  PreToolUse hooks may return permissionDecision allow|ask|defer|deny.\n");
    out.push_str("  PostToolUse hooks run after tool execution and cannot alter the result.\n");
    out.push_str("  SubagentStop hooks run after task-tool subagents finish.\n");
    out.push_str("  Notification hooks run after agent-requested push notifications.\n");
    out.push_str("  lifecycle hooks warn on nonzero exit and do not block the session.\n");
    out.push_str("  command, HTTP, MCP-tool, prompt, and agent hook handlers are executed natively.\n");
    out.push_str(&format!("  usage: {}\n", code_ui::HOOKS_USAGE));
    out.push('\n');
    out
}

/// `/hooks` open hint (mirrors `print_hooks_open_hint`).
pub fn hooks_open_text() -> String {
    let mut out = String::new();
    out.push_str("hooks\n");
    out.push_str("  /hooks open: open Desktop Settings > Hooks for graphical hook management.\n");
    out.push_str("  terminal: edit hook rows in the LibertAI config file; /hooks status shows the active rows.\n");
    out.push('\n');
    out
}

/// `/hooks` usage text.
pub fn hooks_usage_text() -> String {
    let mut out = String::new();
    out.push_str("hooks\n");
    out.push_str(&format!("  usage: {}\n", code_ui::HOOKS_USAGE));
    out.push('\n');
    out
}

/// `/mcp` status text (mirrors `print_mcp_status` Status branch), loading
/// the config fresh (same as the legacy path) so the count + exposure match.
pub fn mcp_status_text() -> String {
    let mut out = String::new();
    out.push_str("mcp\n");
    out.push_str("  terminal registry: stdio, Streamable HTTP, and legacy SSE mcpServers from config.toml are available to MCP-tool hooks, mcp_call, and cached named MCP tools\n");
    out.push_str("  native CLI tools: generic mcp_call is registered when mcpServers exist; cached tools[] register as mcp__server__tool names, resources[] as mcp_read_resource, and prompts[] as mcp_get_prompt\n");
    match crate::config::load() {
        Ok(cfg) if cfg.mcp_servers.is_empty() => {
            out.push_str("  configured servers: 0\n");
        }
        Ok(cfg) => {
            let exposure = code_ui::mcp_exposure_summary(&cfg);
            out.push_str(&format!("  configured servers: {}\n", cfg.mcp_servers.len()));
            out.push_str(&format!(
                "  native exposure: mcp_call {}, {} named MCP tool(s), mcp_read_resource {}, mcp_get_prompt {}, {} resource subscription candidate(s)\n",
                if exposure.mcp_call { "on" } else { "off" },
                exposure.named_tools,
                if exposure.resource_reader { "on" } else { "off" },
                if exposure.prompt_getter { "on" } else { "off" },
                exposure.subscription_candidates
            ));
        }
        Err(e) => {
            out.push_str(&format!("  configured servers: config load failed: {e:#}\n"));
        }
    }
    out.push_str("  desktop: Settings > MCP owns stdio/HTTP/SSE server discovery, probing, and richer cache management\n");
    out.push_str("  tools: CLI executes generic mcp_call, cached named mcp__server__tool entries, mcp_read_resource, mcp_get_prompt, and MCP-tool hook handlers from mcpServers\n");
    out.push_str(&format!("  usage: {}\n", code_ui::MCP_USAGE));
    out
}

/// `/mcp` open hint (mirrors `print_mcp_status` Open branch).
pub fn mcp_open_text() -> String {
    "mcp\n  /mcp open: open Desktop Settings > MCP for live server management. The terminal CLI has no MCP settings pane.\n".to_string()
}

/// `/mcp` usage text.
pub fn mcp_usage_text() -> String {
    format!("mcp\n  usage: {}\n", code_ui::MCP_USAGE)
}

/// `/mcp show <server>` text: resolve the server against the loaded config
/// (exact then prefix match) and render via the bumped
/// `format_mcp_server_details`. Mirrors `print_mcp_server_details`.
pub fn mcp_show_text(name: &str) -> String {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => return format!("  /mcp: config load failed: {e:#}\n"),
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
        return format!("  /mcp: no configured server found for `{name}`\n");
    };
    code_ui::format_mcp_server_details(server_name, server)
}

/// `/forget` status text (mirrors `print_forget_status`).
pub fn forget_status_text(approvals: &crate::commands::code_approvals::ApprovalState) -> String {
    format!(
        "  /forget: ready. Running `/forget` with no arguments clears {} saved allow rule(s); read-only tools stay auto-approved.\n",
        approvals.always_rules().len()
    )
}

/// `/forget` usage text.
pub fn forget_usage_text() -> String {
    format!("  usage: {}\n", code_ui::forget_usage_text())
}

/// `/notify` status text (mirrors `print_notify_status`).
pub fn notify_status_text(cfg: &LibertaiConfig) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  turn notifications: {}\n",
        if cfg.code_turn_notifications { "on" } else { "off" }
    ));
    out.push_str("  agent push notifications: terminal bell + visible notification block\n");
    out.push_str(&format!("  usage: {}\n", code_ui::notify_usage_text()));
    out
}

/// `/notify` usage text.
pub fn notify_usage_text() -> String {
    format!("  usage: {}\n", code_ui::notify_usage_text())
}

// ── M6a Batch C: /changelog + /tree text builders (mirrors the matching
// `print_*` bodies in `code_ui`, but returns the text instead of printing —
// no ANSI escapes; the TUI renders these as a `CommandResult` system entry).
// The bg thread calls these from the `BgCommand::Changelog` / `BgCommand::Tree`
// dispatch arms. `cwd` is threaded in (the bg thread captures it at spawn
// time) so the pure `*_in(cwd, …)` helpers don't re-resolve `current_dir`.

/// `/changelog` text listing (mirrors `print_changelog`): a `changelog`
/// header followed by one indented line per recent commit. On an empty
/// result emits the "no commits found" line; on an error surfaces it
/// prefixed like the legacy `eprintln`.
pub fn changelog_text(cwd: &Path, limit: usize) -> String {
    match code_ui::recent_git_commits_in(cwd, limit) {
        Ok(lines) if lines.is_empty() => {
            "  /changelog: no commits found.\n".to_string()
        }
        Ok(lines) => {
            let mut out = String::new();
            out.push_str("changelog\n");
            for line in lines {
                out.push_str(&format!("  {line}\n"));
            }
            out
        }
        Err(e) => format!("  /changelog: {e:#}\n"),
    }
}

/// `/changelog json` payload (mirrors `print_changelog_json`): pretty-prints
/// the bumped `changelog_json_payload` built from `recent_git_commits_in`.
/// `query` is the parsed json request arg (often empty); on an error it
/// surfaces the message like the legacy path.
pub fn changelog_json_text(cwd: &Path, limit: usize, query: &str) -> String {
    match code_ui::recent_git_commits_in(cwd, limit) {
        Ok(lines) => {
            match serde_json::to_string_pretty(&code_ui::changelog_json_payload(limit, query, lines))
            {
                Ok(raw) => raw,
                Err(e) => format!("  /changelog json: {e:#}\n"),
            }
        }
        Err(e) => format!("  /changelog: {e:#}\n"),
    }
}

/// `/tree` text listing (mirrors `print_project_tree`): the rendered tree
/// (a bold root line + children). On an error surfaces it prefixed like the
/// legacy `eprintln`.
pub fn tree_text(path: Option<&str>) -> String {
    match code_ui::tree_root(path) {
        Ok(root) => match code_ui::render_project_tree(&root, code_ui::TREE_MAX_ENTRIES) {
            Ok(tree) => tree,
            Err(e) => format!("  /tree: {e:#}\n"),
        },
        Err(e) => format!("  /tree: {e:#}\n"),
    }
}

/// `/tree json` payload (mirrors `print_project_tree_json`): pretty-prints
/// the bumped `project_tree_json_payload`. `query` is the parsed json
/// request arg (often empty).
pub fn tree_json_text(path: Option<&str>, query: &str) -> String {
    match code_ui::tree_root(path) {
        Ok(root) => match code_ui::project_tree_json_payload(&root, code_ui::TREE_MAX_ENTRIES, query)
        {
            Ok(payload) => match serde_json::to_string_pretty(&payload) {
                Ok(raw) => raw,
                Err(e) => format!("  /tree json: {e:#}\n"),
            },
            Err(e) => format!("  /tree json: {e:#}\n"),
        },
        Err(e) => format!("  /tree json: {e:#}\n"),
    }
}

/// Synchronously resolve a custom slash command name against a cached list of
/// discovered commands, returning a typed [`CustomResolveResult`].
///
/// This is the non-printing, sync half of the custom-command split: it mirrors
/// `code_ui::resolve_custom_slash` (exact invocation → exact name → prefix) but
/// exposes a public result type. The caller (TUI) caches the
/// `Vec<CustomCommand>` from `code_slash_registry::discover` and passes it in;
/// on a [`CustomResolveResult::Hit`] it sends a [`BgCommand::CustomPrompt`] so
/// the background thread expands the template with the async
/// `code_ui::build_custom_slash_prompt`.
pub fn resolve_custom<'a>(
    commands: &'a [CustomCommand],
    name: &str,
) -> CustomResolveResult<'a> {
    let needle = name.trim().trim_start_matches('/').to_ascii_lowercase();
    if needle.is_empty() {
        return CustomResolveResult::NotFound;
    }

    // Exact invocation-name match (case-insensitive): `namespace/name`.
    let exact_invocation: Vec<&CustomCommand> = commands
        .iter()
        .filter(|cmd| invocation_name(cmd).eq_ignore_ascii_case(&needle))
        .collect();
    if let Some(hit) = unique_match(exact_invocation) {
        return hit;
    }

    // Exact bare-name match.
    let exact_name: Vec<&CustomCommand> = commands.iter().filter(|cmd| cmd.name == needle).collect();
    if let Some(hit) = unique_match(exact_name) {
        return hit;
    }

    // Prefix match (on either bare name or `namespace/name`).
    let prefix: Vec<&CustomCommand> = commands
        .iter()
        .filter(|cmd| cmd.name.starts_with(&needle) || invocation_name(cmd).to_ascii_lowercase().starts_with(&needle))
        .collect();
    unique_match(prefix).unwrap_or(CustomResolveResult::NotFound)
}

/// Non-printing shell-escape runner: spawns the shell via
/// `code_ui::execute_shell_escape`, captures stdout/stderr/exit, and returns a
/// [`ShellEscapeTuiResult`] for the TUI to render as transcript lines.
///
/// Unlike `code_ui::run_shell_escape` (which prints), this never touches
/// stdout/stderr directly. `cwd` is taken from `std::env::current_dir()` so the
/// caller doesn't have to thread it through. On spawn failure the error is
/// surfaced via [`ShellEscapeTuiResult`] fields (empty buffers, `None` exit,
/// and an error message folded into `prompt_context`) so the caller can still
/// render a transcript entry.
pub fn run_shell_escape_tui(command: &str, wrapper: Option<&[String]>) -> ShellEscapeTuiResult {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            return ShellEscapeTuiResult {
                command: command.to_string(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                prompt_context: format!("shell: could not resolve cwd: {e}"),
            };
        }
    };
    match code_ui::execute_shell_escape(&cwd, command, wrapper) {
        Ok(result) => {
            let prompt_context = code_ui::shell_escape_prompt_context(command, &result);
            ShellEscapeTuiResult {
                command: command.to_string(),
                stdout: result.stdout,
                stderr: result.stderr,
                exit_code: result.exit_code,
                prompt_context,
            }
        }
        Err(e) => ShellEscapeTuiResult {
            command: command.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            prompt_context: format!("shell: {e:#}"),
        },
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// The invocation name for a custom command: `namespace/name` when a
/// namespace is set, otherwise the bare `name`. Mirrors the module-private
/// `code_ui::custom_slash_invocation_name`, built here from `CustomCommand`'s
/// public fields.
pub(crate) fn invocation_name(cmd: &CustomCommand) -> String {
    cmd.namespace
        .as_deref()
        .filter(|namespace| !namespace.trim().is_empty())
        .map(|namespace| format!("{namespace}/{}", cmd.name))
        .unwrap_or_else(|| cmd.name.clone())
}

/// The slash-palette entries (`(invocation_name, description)`) for a set of
/// custom commands, in discovery order. Each invocation name is the same
/// `namespace/name` (or bare `name`) that [`resolve_custom`] matches against,
/// so a palette selection round-trips cleanly through the resolver. A missing
/// description falls back to a placeholder so the palette row is never blank.
pub(crate) fn custom_invocation_names(
    commands: &[CustomCommand],
) -> Vec<(String, String)> {
    commands
        .iter()
        .map(|cmd| {
            let name = invocation_name(cmd);
            let desc = cmd
                .description
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("custom command")
                .to_string();
            (name, desc)
        })
        .collect()
}

/// Pick a unique match from a filtered slice, mirroring
/// `code_ui::unique_custom_slash_match`.
fn unique_match<'a>(matches: Vec<&'a CustomCommand>) -> Option<CustomResolveResult<'a>> {
    match matches.as_slice() {
        [] => None,
        [hit] => Some(CustomResolveResult::Hit(hit)),
        _ => {
            let mut names: Vec<String> = matches.into_iter().map(invocation_name).collect();
            names.sort();
            names.dedup();
            Some(CustomResolveResult::Ambiguous(names))
        }
    }
}

/// Filter model ids by scoped glob patterns, mirroring
/// `code_ui::scoped_model_ids` (case-insensitive glob on the bare id and on
/// `provider/id`); falls back to all ids when patterns match nothing.
fn scoped_model_ids(provider: &str, ids: &[String], scoped_patterns: &[String]) -> Vec<String> {
    if scoped_patterns.is_empty() {
        return ids.to_vec();
    }
    let matched: Vec<String> = ids
        .iter()
        .filter(|id| {
            scoped_patterns
                .iter()
                .any(|pattern| matches_scoped_pattern(provider, id, pattern))
        })
        .cloned()
        .collect();
    if matched.is_empty() {
        ids.to_vec()
    } else {
        matched
    }
}

fn matches_scoped_pattern(provider: &str, model_id: &str, pattern: &str) -> bool {
    glob_match_ci(pattern, model_id) || glob_match_ci(pattern, &format!("{provider}/{model_id}"))
}

/// Case-insensitive glob match (`?` single char, `*` run), mirroring
/// `code_ui::glob_match_case_insensitive`.
fn glob_match_ci(pattern: &str, value: &str) -> bool {
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

/// First up to `max` non-empty lines of a trimmed memory body, for the
/// `/memory show` preview.
fn preview_lines(body: &str, max: usize) -> Vec<&str> {
    body.lines().filter(|line| !line.trim().is_empty()).take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code_ui::ShellEscapeAction;
    use crate::commands::code_slash_registry;
    use std::path::PathBuf;

    fn cmd(name: &str, namespace: Option<&str>) -> CustomCommand {
        CustomCommand {
            name: name.to_string(),
            namespace: namespace.map(String::from),
            description: None,
            arg_hint: None,
            argument_names: Vec::new(),
            body: String::new(),
            source: code_slash_registry::CommandSource::Project,
            path: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn resolve_custom_exact_name() {
        let commands = vec![cmd("apply", None)];
        match resolve_custom(&commands, "apply") {
            CustomResolveResult::Hit(hit) => assert_eq!(hit.name, "apply"),
            other => panic!("expected hit, got {other:?}"),
        }
    }

    #[test]
    fn resolve_custom_namespaced_invocation() {
        let commands = vec![cmd("audit", Some("team"))];
        match resolve_custom(&commands, "team/audit") {
            CustomResolveResult::Hit(hit) => assert_eq!(hit.name, "audit"),
            other => panic!("expected hit, got {other:?}"),
        }
        // Bare-name prefix also resolves a single namespaced command.
        match resolve_custom(&commands, "au") {
            CustomResolveResult::Hit(hit) => assert_eq!(hit.name, "audit"),
            other => panic!("expected prefix hit, got {other:?}"),
        }
    }

    #[test]
    fn resolve_custom_ambiguous_prefix() {
        let commands = vec![cmd("audit", Some("ops")), cmd("audit", Some("team"))];
        match resolve_custom(&commands, "audit") {
            CustomResolveResult::Ambiguous(names) => {
                assert_eq!(names, vec!["ops/audit".to_string(), "team/audit".to_string()]);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_custom_not_found() {
        let commands = vec![cmd("apply", None)];
        assert_eq!(resolve_custom(&commands, "nope"), CustomResolveResult::NotFound);
    }

    #[test]
    fn resolve_custom_empty_name_is_not_found() {
        assert_eq!(resolve_custom(&[], "/"), CustomResolveResult::NotFound);
        assert_eq!(resolve_custom(&[], "   "), CustomResolveResult::NotFound);
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match_ci("gpt-4*", "gpt-4o"));
        assert!(glob_match_ci("gpt-4?", "gpt-4o"));
        assert!(!glob_match_ci("gpt-4?", "gpt-4o-mini"));
        assert!(glob_match_ci("libertai/*", "libertai/gpt-4o"));
    }

    #[test]
    fn scoped_model_ids_falls_back_when_no_match() {
        let ids = vec!["a".to_string(), "b".to_string()];
        // Pattern matches nothing → return all ids.
        let scoped = scoped_model_ids("libertai", &ids, &["zzz*".to_string()]);
        assert_eq!(scoped, ids);
        // Pattern matches one → return just that one.
        let scoped = scoped_model_ids("libertai", &ids, &["a".to_string()]);
        assert_eq!(scoped, vec!["a".to_string()]);
    }

    // --- M3a: shell-escape parsing (mirrors code_ui::shell_escape_command) ---

    #[test]
    fn shell_escape_command_parses_run_command() {
        assert_eq!(
            code_ui::shell_escape_command("ls", None),
            ShellEscapeAction::Run("ls".to_string())
        );
    }

    #[test]
    fn shell_escape_command_repeats_previous_command() {
        // `!` with a known previous command repeats it verbatim.
        assert_eq!(
            code_ui::shell_escape_command("!", Some("git status")),
            ShellEscapeAction::Run("git status".to_string())
        );
    }

    #[test]
    fn shell_escape_command_empty_is_usage() {
        // An empty rest (bare `!` with nothing after) is usage guidance.
        match code_ui::shell_escape_command("", None) {
            ShellEscapeAction::Usage(_) => {}
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn shell_escape_command_repeat_with_no_previous_is_usage() {
        // `!` with no last command on record surfaces the dedicated usage hint.
        match code_ui::shell_escape_command("!", None) {
            ShellEscapeAction::Usage(msg) => {
                assert!(
                    msg.contains("no previous shell command to repeat"),
                    "unexpected usage message: {msg}"
                );
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    // --- M3a: shell-escape execution (hermetic, deterministic) ---------------

    #[test]
    fn run_shell_escape_true_exits_zero_with_empty_stdout() {
        // `true` is guaranteed present on every POSIX shell, exits 0, no output.
        let res = run_shell_escape_tui("true", None);
        assert_eq!(res.exit_code, Some(0), "exit_code: {:?}", res.exit_code);
        assert!(res.stdout.is_empty(), "stdout should be empty: {:?}", res.stdout);
        assert!(res.stderr.is_empty(), "stderr should be empty: {:?}", res.stderr);
        // The prompt-context block still records the run for the next prompt.
        assert!(res.prompt_context.contains("$ true"));
    }

    #[test]
    fn run_shell_escape_false_exits_nonzero() {
        // `false` is the guaranteed-present companion of `true`; exits 1.
        let res = run_shell_escape_tui("false", None);
        assert_eq!(res.exit_code, Some(1), "exit_code: {:?}", res.exit_code);
        assert!(res.stdout.is_empty());
    }

    #[test]
    fn run_shell_escape_echo_captures_stdout() {
        // `echo` is a shell builtin + guaranteed present; output is captured.
        let res = run_shell_escape_tui("echo hi", None);
        assert_eq!(res.exit_code, Some(0));
        assert!(
            res.stdout.contains("hi"),
            "expected 'hi' in stdout, got: {:?}",
            res.stdout
        );
        // The prompt context surfaces the captured stdout for the next prompt.
        assert!(res.prompt_context.contains("hi"));
    }

    // --- M3a: custom template resolution -------------------------------------
    //
    // The synchronous `resolve_custom` only detects a hit; the template
    // expansion is `code_slash_registry::expand_with_context`. We exercise the
    // MISS path (empty discovered vec → NotFound for any name) and the HIT
    // path (a fake CustomCommand resolves to a Hit, then expands to the
    // expected prompt) so the full resolve→expand flow is covered without
    // depending on disk fixtures or async session state.

    #[test]
    fn resolve_custom_empty_vec_misses_for_any_name() {
        // MISS path: no discovered commands → every name is NotFound.
        assert_eq!(resolve_custom(&[], "anything"), CustomResolveResult::NotFound);
        assert_eq!(resolve_custom(&[], "/apply"), CustomResolveResult::NotFound);
    }

    #[test]
    fn resolve_custom_hit_then_expand_with_context() {
        // HIT path: a fake custom command with a `{{args}}` body resolves to a
        // Hit, and expand_with_context interpolates the args into the body.
        let mut command = cmd("apply", None);
        command.body = "Please apply the following change: {{args}}".to_string();
        let commands = vec![command];

        // resolve_custom finds the command by bare name.
        let hit = match resolve_custom(&commands, "apply") {
            CustomResolveResult::Hit(hit) => hit,
            other => panic!("expected hit, got {other:?}"),
        };
        assert_eq!(hit.name, "apply");

        // expand_with_context interpolates the args into the body. The default
        // ExpansionContext (no session_id/effort) is enough for `{{args}}`.
        let ctx = code_slash_registry::ExpansionContext::default();
        let expanded = code_slash_registry::expand_with_context(hit, "fix the bug", &ctx);
        assert!(
            expanded.contains("Please apply the following change: fix the bug"),
            "unexpected expansion: {expanded}"
        );
        // The {{args}} token is fully consumed.
        assert!(!expanded.contains("{{args}}"), "expansion left a token: {expanded}");
    }
}
