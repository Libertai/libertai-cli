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

// The adapters below are wired into the TUI dispatch / background thread by
// the sibling M3a workstreams (router wiring, templates, shell escape). Until
// those land they are dead code, like the transitional handlers retained in
// `code_ui` for the same ratatui migration. Match `code_ui`'s convention.
#![allow(dead_code)]

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
    /// `/skills` — render the active code-pillar skill inventory as text.
    SkillsList,
    /// `/memory show` — render the current project memory state as text.
    MemoryShow,
    /// A custom slash template command. The bg thread expands the template
    /// against the live `AgentSessionHandle` (async) and sends the resulting
    /// prompt back; the TUI then submits it via `Cmd::Prompt`.
    CustomPrompt {
        /// Command name (without leading `/`), already resolved to a hit by
        /// [`resolve_custom`].
        name: String,
        /// Raw argument string to interpolate into the template.
        args: String,
    },
    /// A `!`/`!!` shell escape. The bg thread runs the command and returns
    /// the captured stdout/stderr/exit for rendering plus the prompt-context
    /// string for `pending_shell_contexts`.
    ShellEscape {
        /// Shell command line to execute.
        command: String,
        /// Optional argv prefix wrapping the shell (e.g. a sandbox wrapper).
        wrapper: Option<Vec<String>>,
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
fn invocation_name(cmd: &CustomCommand) -> String {
    cmd.namespace
        .as_deref()
        .filter(|namespace| !namespace.trim().is_empty())
        .map(|namespace| format!("{namespace}/{}", cmd.name))
        .unwrap_or_else(|| cmd.name.clone())
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
