//! Tool-call approval layer.
//!
//! Wraps every mutating built-in tool (`bash`, `edit`, `write`,
//! `hashline_edit`) in an [`ApprovalTool`] that pauses the agent stream,
//! renders a preview of what's about to run, and waits for a decision via
//! a pluggable [`ApprovalUi`]: allow once, always allow,
//! or deny (with optional reason fed back to the agent so it can
//! course-correct).
//!
//! Read-only tools (`read`, `grep`, `find`, `ls`, `bash_output`) are
//! auto-allowed — the approval UI for them would be pure noise.
//!
//! [`ApprovalState`] can be session-scoped or backed by an on-disk
//! allow-rule store. The UI is supplied separately (see [`ApprovalUi`])
//! so the same approval-gating logic powers the
//! terminal CLI ([`TerminalApprovalUi`] in `code_term`) and the desktop
//! app (a callback-based UI implemented in the Tauri crate).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result as AnyhowResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_aux::{SmartApproval, SmartApprovalVerdict};
use crate::commands::code_diff::{read_preview_file, EditJournal, JournalEntry};
use crate::commands::code_factory::{is_path_edit_tool, Mode, ModeFlag};

/// User decision for a single approval prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptChoice {
    /// Run this tool call once.
    Allow,
    /// Run this tool call and remember "always allow this tool".
    AlwaysAllow,
    /// Run this tool call and remember "allow this rule for the rest
    /// of this session" (cleared by `/forget` or session end). Falls
    /// between `Allow` (one-shot) and `AlwaysAllow` (persisted to disk).
    AllowSession,
    /// Reject this tool call. The agent receives a denial output.
    Deny,
    /// (M4/#10) Run this tool call and persist an "always allow" rule
    /// at the PREFIX scope — the broadest-but-still-scoped variant. For
    /// bash that's `<bin> <first-arg> *` (e.g. `npm run *` for
    /// `npm run build`), tighter than `GrantRoot`'s `<bin> *`. For path
    /// tools this is the directory-trust rule `<dir> *`. Only offered
    /// when [`ApprovalSubject::prefix_rule`] is `Some`; falls back to
    /// `AlwaysAllow`'s default rule otherwise.
    Prefix,
    /// (M4/#10) Run this tool call and persist an "always allow" rule
    /// at the ROOT scope — the binary/whole-tool tier. For bash that's
    /// `<bin> *` (e.g. `npm *` covers `npm install`, `npm run build`,
    /// …). Only offered when [`ApprovalSubject::root_rule`] is `Some`;
    /// falls back to `AlwaysAllow`'s default rule otherwise.
    GrantRoot,
    /// (M4/#10) Run this tool call and persist an "always allow" rule
    /// at the DOMAIN scope — for path tools, trust every path under a
    /// common ancestor directory (`<dir> *`). For bash it's currently
    /// the same as `GrantRoot` (bash has no directory concept). Only
    /// offered when [`ApprovalSubject::domain_rule`] is `Some`; falls
    /// back to `AlwaysAllow`'s default rule otherwise.
    Domain,
    /// The UI cannot get an answer right now (e.g. desktop app closed
    /// while the modal was open). The tool wrapper translates this
    /// into [`ToolExecution::Paused`] so the agent loop suspends and
    /// the request resumes on the next session start.
    Paused {
        request_id: String,
        payload: serde_json::Value,
    },
}

/// Optional host-side policy that can observe a tool call before the
/// regular approval prompt. Desktop uses this for Claude Code-style
/// PreToolUse hooks.
pub trait ToolPolicy: Send + Sync {
    fn decide(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ToolPolicyDecision;
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolPolicyDecision {
    NoDecision,
    Allow {
        updated_input: Option<serde_json::Value>,
        additional_context: Option<String>,
    },
    Ask {
        reason: Option<String>,
        updated_input: Option<serde_json::Value>,
        additional_context: Option<String>,
    },
    Defer,
    Deny {
        reason: Option<String>,
    },
}

/// Result of an [`ApprovalUi::ask`] call. Mirrors [`PromptChoice`]'s
/// pause story: a UI that can't currently surface the questions
/// returns `Paused`, and [`AskUserTool`] turns that into
/// [`ToolExecution::Paused`].
#[derive(Debug, Clone)]
pub enum AskOutcome {
    /// The user answered. The opaque JSON payload is what the LLM
    /// receives as the tool result content.
    Answer(serde_json::Value),
    /// The UI is unavailable (process exit / app close); the agent
    /// loop should suspend until [`ApprovalUi::ask`] is re-invoked
    /// with this `payload` via the tool's resume hook.
    Paused {
        request_id: String,
        payload: serde_json::Value,
    },
}

/// Result of an agent-requested desktop/user notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// The UI accepted or displayed the notification.
    Sent,
    /// The UI does not support notifications or they are disabled.
    Skipped(String),
}

/// A single allow rule binding a tool to a command/path pattern.
///
/// Prompt-created rules are exact, even when the command/path contains `*`.
/// Explicit wildcard rules can opt into glob-lite `*` matching.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AllowRule {
    /// Lowercase canonical tool name: `"bash"`, `"edit"`, `"write"`, etc.
    pub tool: String,
    /// Command/path/value pattern. Empty = matches all uses of the tool.
    pub pattern: String,
    /// Whether `pattern` should be interpreted with `*` wildcard semantics.
    pub wildcard: bool,
}

impl AllowRule {
    pub fn exact(tool: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            pattern: pattern.into(),
            wildcard: false,
        }
    }

    pub fn wildcard(tool: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            pattern: pattern.into(),
            wildcard: true,
        }
    }

    pub fn tool_all(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            pattern: String::new(),
            wildcard: false,
        }
    }

    /// Returns true when this rule applies to `(tool_name, value)`.
    pub fn matches(&self, tool_name: &str, value: &str) -> bool {
        if self.tool != tool_name {
            return false;
        }
        if self.pattern.is_empty() {
            return true;
        }
        if self.wildcard {
            wildcard_match(&self.pattern, value)
        } else {
            self.pattern == value
        }
    }
}

/// Raw, unsanitized data extracted from a tool call before any display
/// processing. Used for matching and rule construction — never drawn from
/// sanitized/truncated preview text.
pub struct ApprovalSubject {
    /// The raw command string, file path, or URL for pattern matching.
    pub value: String,
    /// The rule to record if the user presses "always allow".
    pub suggested_rule: AllowRule,
    /// Human-readable label shown in the UI, e.g. `"bash(npm run build)"`.
    pub suggested_label: String,
    /// (M4/#10) Optional PREFIX-scope rule — `<bin> <first-arg> *` for
    /// bash, `<dir> *` for path tools. When `Some`, the UI offers a
    /// `Prefix` choice that records this instead of `suggested_rule`.
    /// `None` when the call doesn't have a meaningful prefix tier (e.g.
    /// bare `npm` with no args, or a path tool whose file has no dir).
    pub prefix_rule: Option<AllowRule>,
    /// (M4/#10) Optional ROOT-scope rule — `<bin> *` for bash. `None`
    /// when there's no broader root tier beyond `suggested_rule` (e.g.
    /// `suggested_rule` already IS the binary-only form).
    pub root_rule: Option<AllowRule>,
    /// (M4/#10) Optional DOMAIN-scope rule — `<dir> *` for path tools.
    /// `None` for non-path tools (bash has no directory concept).
    pub domain_rule: Option<AllowRule>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredAllowRules {
    #[serde(default)]
    rules: Vec<StoredAllowRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAllowRule {
    tool: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    wildcard: bool,
    #[serde(default = "always_scope")]
    scope: String,
}

impl StoredAllowRule {
    fn from_rule(rule: &AllowRule) -> Self {
        Self {
            tool: rule.tool.clone(),
            pattern: rule.pattern.clone(),
            wildcard: rule.wildcard,
            scope: always_scope(),
        }
    }

    fn into_rule(self) -> Option<AllowRule> {
        if self.scope != "always" {
            return None;
        }
        Some(AllowRule {
            tool: self.tool,
            pattern: self.pattern,
            wildcard: self.wildcard,
        })
    }
}

fn always_scope() -> String {
    "always".to_string()
}

/// Extract an [`ApprovalSubject`] from the raw JSON input of a tool call.
///
/// Reads raw JSON fields without sanitization or truncation so the
/// resulting rule matches exactly what the model produced. The caller
/// should sanitize separately for UI display (see [`preview_call`]).
pub fn approval_subject(tool: &str, input: &serde_json::Value) -> ApprovalSubject {
    approval_subject_with_base(tool, input, None)
}

/// Like [`approval_subject`], but path-tool subjects (write / edit /
/// hashline_edit / notebook_*) are absolutized against `base` when the
/// model supplied a relative path. Without this, a directory-trust
/// rule recorded as `/project/**` never matches a call whose `path` is
/// `src/foo.ts`, and the user keeps getting prompted inside a
/// directory they explicitly trusted. Matching AND the suggested
/// always-rule both use the absolutized form so recorded rules stay
/// consistent regardless of how the model spelled the path.
pub fn approval_subject_with_base(
    tool: &str,
    input: &serde_json::Value,
    base: Option<&Path>,
) -> ApprovalSubject {
    let abs = |p: &str| absolutize_for_match(p, base);
    let (value, rule, label, prefix_rule, root_rule, domain_rule) = match tool {
        "bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing command>");
            let s = cmd.to_string();
            // (Issue-2: bash allow-rule granularity) The "always allow"
            // rule for bash is keyed on the command's FIRST TOKEN (the
            // binary), NOT the full command string. bash commands almost
            // never repeat byte-identically (git status vs git status
            // --short, cd prefixes, env prefixes, varying flags), so a
            // whole-command exact rule never re-matched and the user was
            // re-prompted every call ("the allow rules don't work"). Keying
            // on the binary lets one "always allow npm" cover `npm run
            // build`, `npm run test`, `npm run build --watch`, etc. The
            // `value` matched at prompt time is still the FULL command
            // (so a too-broad rule can't widen what the user already
            // approved mid-prompt); only the recorded rule narrows to the
            // binary. When the command has args we record a wildcard
            // `"<bin> *"` (matches the binary followed by any args, but
            // NOT a bare invocation of a differently-named binary); when
            // it's a single token we record the exact binary name. The
            // label shows the scope so the user knows they're trusting
            // the binary, not the exact command.
            let first_token = first_bash_token(cmd);
            // The missing-command placeholder + an all-whitespace command have
            // no real binary to key on — fall back to an exact rule on the
            // full string (which never matches a real command, so it won't
            // auto-allow anything; the label stays readable).
            //
            // (Round-9) The recorded rule's PATTERN must be non-empty: an
            // empty pattern means `AllowRule::matches` returns true for ANY
            // command of this tool (`self.pattern.is_empty() -> true`), so
            // "always allow" on a genuinely-empty bash command
            // (`{"command": ""}`) would silently grant a blanket bash bypass.
            // Use a sentinel that no real command matches. The matched VALUE
            // stays the real command (`s`, even if empty) — only the recorded
            // rule narrows to the sentinel. (Pre-existing latent: the empty-
            // pattern = match-all `matches()` semantics is intentional via
            // `AllowRule::tool_all`, but this fallback arm must not reach it.)
            let no_real_binary =
                cmd.trim().is_empty() || first_token.is_empty() || first_token == "<missing";
            let (rule, label, prefix_rule, root_rule) = if no_real_binary {
                let rule_pattern = if s.is_empty() {
                    "<no command>".to_string()
                } else {
                    s.clone()
                };
                (
                    AllowRule::exact(tool, rule_pattern.clone()),
                    format!("bash({rule_pattern})"),
                    None,
                    None,
                )
            } else if cmd_trimmed_has_args(cmd) {
                // (M4/#10) Default "always allow" now records the PREFIX
                // scope — `<bin> <first-arg> *` (e.g. `npm run *` for
                // `npm run build`) — which is what users almost always
                // mean by "always allow npm run build" (trust `npm run`,
                // not bare `npm install`). The broader ROOT tier (`npm *`,
                // i.e. trust the whole binary) is offered as `GrantRoot`.
                // The matched VALUE stays the full command so a too-broad
                // rule can't widen what the user already approved mid-prompt.
                let root_pat = format!("{first_token} *");
                let root = AllowRule::wildcard(tool, root_pat.clone());
                let first_two = first_two_tokens(cmd);
                let prefix = match first_two {
                    Some(second) if second != first_token => {
                        let prefix_pat = format!("{first_token} {second} *");
                        Some(AllowRule::wildcard(tool, prefix_pat.clone()))
                    }
                    _ => None,
                };
                // The suggested (default) rule is the prefix when available
                // (e.g. `npm run *`), else the root (`npm *`).
                let (suggested, slabel) = match &prefix {
                    Some(p) => (p.clone(), format!("bash({})", p.pattern)),
                    None => (root.clone(), format!("bash({root_pat})")),
                };
                (suggested, slabel, prefix, Some(root))
            } else {
                (
                    AllowRule::exact(tool, first_token.clone()),
                    format!("bash({first_token})"),
                    None,
                    None,
                )
            };
            (s, rule, label, prefix_rule, root_rule, None)
        }
        "bash_output" => {
            let path = field(input, "logPath")
                .or_else(|| field(input, "log_path"))
                .unwrap_or("<missing log path>");
            let pid = input_pid(input)
                .map(|pid| format!(" pid {pid}"))
                .unwrap_or_default();
            let s = format!("{path}{pid}");
            (
                s.clone(),
                AllowRule::exact(tool, s.clone()),
                format!("bash_output({s})"),
                None,
                None,
                None,
            )
        }
        "kill_bash" => {
            let s = input_pid(input)
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "<missing pid>".to_string());
            (
                s.clone(),
                AllowRule::exact(tool, s.clone()),
                format!("kill_bash({s})"),
                None,
                None,
                None,
            )
        }
        "write" | "edit" | "hashline_edit" | "notebook_edit" | "notebook_execute" => {
            // The model passes `path` as a string. A MISSING field falls back
            // to the `<missing path>` placeholder (non-empty, safe). An
            // EXPLICIT empty string `{"path": ""}` resolves to `s = ""` after
            // absolutization (`absolutize_for_match` early-returns the empty
            // string), which would record `AllowRule::exact(tool, "")` — and
            // `AllowRule::matches` treats an EMPTY pattern as match-ALL (the
            // `self.pattern.is_empty() -> true` path reserved for
            // `AllowRule::tool_all`). That is a FALSE-ALLOW: a single "always
            // allow" on a malformed empty-path write/edit would silently
            // pre-approve every future write/edit against ANY path
            // (e.g. /etc/passwd, ~/.ssh/id_rsa) for the rest of the session.
            //
            // (Round-10) Mirror the round-9 bash sentinel: when the resolved
            // path is empty, record a non-empty sentinel pattern (`<missing
            // path>`, which no real path equals and which `absolutize_for_match`
            // passes through unchanged via the `starts_with('<')` early-return)
            // so the rule can NEVER match-all. The matched VALUE stays the
            // real (possibly empty) path `s` — only the recorded rule narrows
            // to the sentinel. This closes the false-allow the round-9 audit
            // found left open for the path-edit sibling arms.
            //
            // (M4/#10) When the resolved path has a parent directory, offer
            // a DOMAIN-scope rule — `<dir> *` (e.g. `/proj/src *`) — so the
            // user can trust the whole directory at once. The exact-file rule
            // stays the default (tightest); DOMAIN is opt-in. We only derive
            // it for non-sentinel, real paths.
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing path>");
            let s = abs(path);
            let (rule, label) = if s.is_empty() {
                let sentinel = "<missing path>".to_string();
                (
                    AllowRule::exact(tool, sentinel.clone()),
                    format!("{tool}({sentinel})"),
                )
            } else {
                (AllowRule::exact(tool, s.clone()), format!("{tool}({s})"))
            };
            // Derive `<dir> *` from the parent. Skip when there's no parent
            // (a bare filename) or the path is a sentinel — a directory-trust
            // rule on `<` is meaningless.
            let domain_rule =
                parent_dir_wildcard(&s).map(|dir_pat| AllowRule::wildcard(tool, dir_pat.clone()));
            (s, rule, label, None, None, domain_rule)
        }
        // Unknown/future wrapped tools fall back to exact raw-JSON matching
        // instead of whole-tool approval.
        other => {
            let s = input.to_string();
            (
                s.clone(),
                AllowRule::exact(other, s.clone()),
                format!("{other}({s})"),
                None,
                None,
                None,
            )
        }
    };
    ApprovalSubject {
        value,
        suggested_rule: rule,
        suggested_label: label,
        prefix_rule,
        root_rule,
        domain_rule,
    }
}

/// Join a possibly-relative tool path onto `base` and normalize `.` /
/// `..` segments lexically (no filesystem access — the target usually
/// doesn't exist yet for `write`). Absolute paths and missing bases
/// pass through unchanged, as does the `<missing path>` placeholder.
fn absolutize_for_match(path: &str, base: Option<&Path>) -> String {
    let Some(base) = base else {
        return path.to_string();
    };
    if path.is_empty() || path.starts_with('/') || path.starts_with('<') {
        return path.to_string();
    }
    let mut parts: Vec<&str> = base
        .to_str()
        .map(|b| b.split('/').filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// (Issue-2: bash allow-rule granularity) Extract the first token of a bash
/// command string — the binary name — so a bash "always allow" rule can key on
/// the binary rather than the whole command. Leading whitespace is trimmed; the
/// token is the run of non-whitespace chars before the first whitespace. Shell
/// metacharacters are NOT parsed (a `cd foo && npm run build` command's first
/// token is `cd`, not `npm` — a deliberately conservative first cut; a shell
/// parser can refine this later). Returns the empty string for an all-whitespace
/// command (the caller's `unwrap_or("<missing command>")` already guards the
/// missing-command case, and an empty pattern means "match all bash", which is
/// safer to avoid — but `cmd_trimmed_has_args` returns false for all-whitespace
/// so we record `exact("bash", "")`, which never matches a real command).
fn first_bash_token(cmd: &str) -> String {
    cmd.trim_start()
        .split(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_string()
}

/// (M4/#10) The first two whitespace-separated tokens of `cmd` after
/// trimming, returning `None` when there's no second token. Used to build
/// the PREFIX-scope bash rule (`<bin> <first-arg> *`, e.g. `npm run *`).
/// Returns the second token only; the binary is already `first_bash_token`.
fn first_two_tokens(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let _first = parts.next()?;
    let second = parts.next()?;
    Some(second.to_string())
}

/// (M4/#10) Derive a `<dir> *` directory-trust pattern from a resolved
/// path, for the path tools' DOMAIN scope. Returns the parent directory
/// (with a trailing ` *`) when the path has a real parent, else `None`
/// (bare filename, root path, or a `<`-prefixed sentinel). The wildcard
/// matches everything under the directory via `wildcard_match`.
fn parent_dir_wildcard(resolved: &str) -> Option<String> {
    if resolved.is_empty() || resolved.starts_with('<') {
        return None;
    }
    let parent = Path::new(resolved).parent()?;
    let parent_str = parent.to_str()?;
    if parent_str.is_empty() {
        return None;
    }
    Some(format!("{parent_str} *"))
}

/// (Issue-2) True when the trimmed command has at least one whitespace-separated
/// argument after the first token — i.e. it's `<bin> <args...>` rather than a
/// bare `<bin>`. Decides whether the recorded rule is a `"<bin> *"` wildcard
/// (has args) or an exact `"<bin>"` (no args).
fn cmd_trimmed_has_args(cmd: &str) -> bool {
    // Split on any whitespace run; >1 non-empty token means args follow the
    // binary. (A trailing-space-only command like "npm " yields one token.)
    cmd.split_whitespace().count() > 1
}

/// Match `text` against a `*`-wildcard pattern.
///
/// - Empty pattern → always matches (the "whole tool" rule).
/// - No `*` in pattern → exact match (`text == pattern`).
/// - Single or multiple `*` → split on `*` and verify each segment appears
///   in order (standard glob-lite semantics).
///
/// The function is public so tests and future static rule parsers can use
/// it without going through `AllowRule`.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if !pattern.contains('*') {
        return text == pattern;
    }

    let parts: Vec<&str> = pattern.split('*').collect();

    // The first segment must be a prefix.
    if !parts.is_empty() && !parts[0].is_empty() && !text.starts_with(parts[0]) {
        return false;
    }
    // The last segment must be a suffix.
    let last = parts.len() - 1;
    if last > 0 && !parts[last].is_empty() && !text.ends_with(parts[last]) {
        return false;
    }
    // Interior segments must appear in order at non-overlapping positions.
    let mut pos = if parts[0].is_empty() {
        0
    } else {
        parts[0].len()
    };
    for &seg in &parts[1..last] {
        if seg.is_empty() {
            continue;
        }
        match text[pos..].find(seg) {
            Some(idx) => pos += idx + seg.len(),
            None => return false,
        }
    }
    true
}

/// Renders an approval prompt and returns the user's choice.
///
/// The trait is async because the desktop implementation awaits a
/// `tokio::sync::oneshot::Receiver` while a frontend modal collects the
/// user's response. The terminal implementation does its work
/// synchronously inside the async body, that's fine because pi awaits
/// `Tool::execute` sequentially, so blocking the executor briefly
/// doesn't starve other in-flight work on the same session.
///
/// `ask` is the parallel back-channel for the `ask_user` tool: the
/// agent calls it with a structured questions payload and the UI
/// returns the user's answers. The default impl returns a `cancelled`
/// response so existing UI implementations (e.g. the terminal) keep
/// compiling without behavior changes; only UIs that want to surface
/// the ask flow override it.
///
/// `decide` receives three arguments:
/// - `tool_name` — the tool name (e.g. `"bash"`).
/// - `preview` — sanitized display text (safe to print).
/// - `always_rule` — the rule label for the "always allow" option
///   (e.g. `"bash(npm run build)"`), already sanitized/defanged.
#[async_trait]
pub trait ApprovalUi: Send + Sync {
    async fn decide(&self, tool_name: &str, preview: &str, always_rule: &str) -> PromptChoice;
    /// Whether this UI may use smart approval as a substitute for a
    /// manual approval prompt. Headless UIs return false so fresh
    /// mutating calls still require an explicit remembered rule.
    fn allows_smart_approval(&self) -> bool {
        true
    }
    async fn ask(&self, _payload: serde_json::Value) -> AskOutcome {
        AskOutcome::Answer(serde_json::json!({
            "cancelled": true,
            "reason": "ASK_NOT_SUPPORTED",
        }))
    }
    /// Re-fire a previously paused approval request. Default impl
    /// errors; UIs that ever return [`PromptChoice::Paused`] from
    /// [`Self::decide`] must override this to pick the request back up
    /// using `payload` (which carries the original tool name, preview,
    /// always_rule, etc. as serialised by the tool wrapper).
    async fn resume_decide(&self, _request_id: &str, _payload: serde_json::Value) -> PromptChoice {
        PromptChoice::Deny
    }
    /// Re-fire a previously paused ask_user request. Same contract as
    /// [`Self::resume_decide`] but for ask_user. Default impl mirrors
    /// the legacy "cancelled" envelope.
    async fn resume_ask(&self, _request_id: &str, _payload: serde_json::Value) -> AskOutcome {
        AskOutcome::Answer(serde_json::json!({
            "cancelled": true,
            "reason": "RESUME_NOT_SUPPORTED",
        }))
    }
    /// Fire-and-forget user notification channel used by the
    /// `push_notification` tool. Default keeps non-desktop clients
    /// compiling and tells the model the notification was unavailable.
    async fn notify(&self, _title: &str, _body: &str) -> NotifyOutcome {
        NotifyOutcome::Skipped("NOTIFY_NOT_SUPPORTED".to_string())
    }
}

/// Approval memory. By default it is session-scoped; CLI sessions can
/// opt into an on-disk allow-rule store.
///
/// Holds two allowlists:
/// - `auto_allow` (hardcoded, read-only built-ins): never prompt.
/// - `always_allow` (user-promoted via `AlwaysAllow`): never prompt,
///   keyed by tool+pattern [`AllowRule`].
pub struct ApprovalState {
    always_allow: Mutex<Vec<AllowRule>>,
    /// Session-scoped rules: consulted like `always_allow` but never
    /// persisted. Used for broad in-session passes (e.g. the desktop's
    /// "trust this directory" granting bash a session-wide pass) that
    /// shouldn't survive into every future session. Cleared by
    /// [`ApprovalState::forget`].
    session_allow: Mutex<Vec<AllowRule>>,
    auto_allow: HashSet<String>,
    persistent_path: Option<PathBuf>,
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalState {
    pub fn new() -> Self {
        Self {
            always_allow: Mutex::new(Vec::new()),
            session_allow: Mutex::new(Vec::new()),
            auto_allow: ["read", "grep", "find", "ls", "bash_output"]
                .into_iter()
                .map(String::from)
                .collect(),
            persistent_path: None,
        }
    }

    pub fn with_persistent_store(path: PathBuf) -> AnyhowResult<Self> {
        let rules = load_allow_rules(&path)?;
        Ok(Self {
            always_allow: Mutex::new(rules),
            session_allow: Mutex::new(Vec::new()),
            auto_allow: ["read", "grep", "find", "ls", "bash_output"]
                .into_iter()
                .map(String::from)
                .collect(),
            persistent_path: Some(path),
        })
    }

    /// True when the tool+value pair is on either allowlist.
    ///
    /// `auto_allow` matches by tool name alone (read-only tools).
    /// `always_allow` checks each [`AllowRule`] against `(tool_name, value)`.
    pub fn is_pre_allowed(&self, tool_name: &str, value: &str) -> bool {
        if self.auto_allow.contains(tool_name) {
            return true;
        }
        if self
            .session_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|rule| rule.matches(tool_name, value))
        {
            return true;
        }
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|rule| rule.matches(tool_name, value))
    }

    /// Record a rule. Deduplicates identical rules and persists the
    /// updated set when this state has an on-disk store. Persisting
    /// merges with whatever is on disk first: several live sessions
    /// share the store file, and a wholesale write of this session's
    /// list used to erase rules other sessions had recorded since this
    /// one loaded.
    pub fn record_always(&self, rule: AllowRule) {
        let mut list = self
            .always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !list.contains(&rule) {
            list.push(rule);
            if let Some(path) = self.persistent_path.as_ref() {
                if let Ok(on_disk) = load_allow_rules(path) {
                    for r in on_disk {
                        if !list.contains(&r) {
                            list.push(r);
                        }
                    }
                }
            }
            self.persist_locked_rules(&list);
        }
    }

    /// Record a rule for THIS session only — consulted like a normal
    /// always rule but never written to the persistent store. Used for
    /// broad passes the user grants while working (the desktop's
    /// "trust this directory" giving bash a session-wide pass) where
    /// persisting globally would be far more than they asked for.
    pub fn record_session(&self, rule: AllowRule) {
        let mut list = self
            .session_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !list.contains(&rule) {
            list.push(rule);
        }
    }

    /// Snapshot the user-promoted allow rules. Used by desktop settings
    /// management; read-only auto-allow defaults are intentionally not
    /// included because they are not user-managed memory.
    pub fn always_rules(&self) -> Vec<AllowRule> {
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Remove one remembered allow rule by its current list index.
    /// Returns false when the index is stale or out of range.
    pub fn remove_always(&self, index: usize) -> bool {
        let mut list = self
            .always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index >= list.len() {
            return false;
        }
        list.remove(index);
        self.persist_locked_rules(&list);
        true
    }

    /// Drop every "always allow" entry and clear the persistent store if
    /// this state is backed by one.
    /// Invoked by the `/forget` slash command in the REPL.
    pub fn forget(&self) {
        let mut list = self
            .always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        list.clear();
        self.session_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.persist_locked_rules(&[]);
    }

    fn persist_locked_rules(&self, rules: &[AllowRule]) {
        if let Some(path) = self.persistent_path.as_ref() {
            if let Err(err) = save_allow_rules(path, rules) {
                eprintln!("warning: failed to persist approval rules: {err:#}");
            }
        }
    }
}

fn load_allow_rules(path: &Path) -> AnyhowResult<Vec<AllowRule>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading approval rules {}", path.display()))?;
    let stored: StoredAllowRules = toml::from_str(&raw)
        .with_context(|| format!("parsing approval rules {}", path.display()))?;
    Ok(stored
        .rules
        .into_iter()
        .filter_map(StoredAllowRule::into_rule)
        .collect())
}

fn save_allow_rules(path: &Path, rules: &[AllowRule]) -> AnyhowResult<()> {
    if let Some(parent) = path.parent() {
        crate::config::create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let stored = StoredAllowRules {
        rules: rules.iter().map(StoredAllowRule::from_rule).collect(),
    };
    let raw = toml::to_string_pretty(&stored).context("serializing approval rules")?;
    crate::config::write_file_secure(path, raw.as_bytes())
        .with_context(|| format!("writing approval rules {}", path.display()))?;
    Ok(())
}

/// Wraps any `pi::sdk::Tool` with two gates:
///
/// 1. The approval UI (allow / always / deny) for mutating tools.
/// 2. A short-circuit denial when the shared [`ModeFlag`] says we're in
///    [`Mode::Plan`] and this tool isn't read-only — the tool registry
///    stays stable across mode toggles so message history survives.
pub struct ApprovalTool {
    inner: Box<dyn Tool>,
    state: Arc<ApprovalState>,
    /// Session working directory used to absolutize relative path
    /// subjects before rule matching (see `approval_subject_with_base`).
    base_dir: Option<PathBuf>,
    mode: ModeFlag,
    ui: Arc<dyn ApprovalUi>,
    policy: Option<Arc<dyn ToolPolicy>>,
    smart_approval: Option<Arc<dyn SmartApproval>>,
    /// Shared edit journal. When set, `execute_inner` records each
    /// successful mutating tool's before/after content so the main
    /// thread's `/undo` can revert it. `None` for bare/test tools.
    journal: Option<Arc<EditJournal>>,
}

impl ApprovalTool {
    pub fn new(
        inner: Box<dyn Tool>,
        state: Arc<ApprovalState>,
        mode: ModeFlag,
        ui: Arc<dyn ApprovalUi>,
    ) -> Self {
        Self {
            inner,
            state,
            base_dir: None,
            mode,
            ui,
            policy: None,
            smart_approval: None,
            journal: None,
        }
    }

    pub fn with_base_dir(mut self, base_dir: Option<PathBuf>) -> Self {
        self.base_dir = base_dir;
        self
    }

    pub fn with_policy(mut self, policy: Option<Arc<dyn ToolPolicy>>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_smart_approval(mut self, smart: Option<Arc<dyn SmartApproval>>) -> Self {
        self.smart_approval = smart;
        self
    }

    /// Attach the shared [`EditJournal`] so `execute_inner` can record each
    /// successful edit for `/undo`. The journal is `Option` so a bare
    /// `ApprovalTool::new` (e.g. unit tests) keeps compiling unchanged —
    /// only the factory wires a real one. Mirrors `with_smart_approval`'s
    /// builder shape so the ctor signature stays stable.
    pub fn with_journal(mut self, journal: Arc<EditJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    async fn execute_inner(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        emit_tool_started_update(on_update.as_deref(), self.inner.name());
        let snapshot = std::env::current_dir().ok().and_then(|cwd| {
            crate::commands::code_diff::file_snapshot_before_tool(self.inner.name(), &input, &cwd)
        });
        let result = self.inner.execute(tool_call_id, input, on_update).await;

        // Record the edit for `/undo` BEFORE the post-execution diff is
        // appended. We only journal a SUCCESSFUL edit — a tool that erred
        // left the file unchanged (or in an error state the user should
        // inspect), so pushing an undo entry would let `/undo` revert a
        // no-op or, worse, clobber a half-applied mutation. Reuses the free
        // `FileSnapshot` the approval layer already captured (no second
        // read-before-tool); only the post-content is read, on success.
        if let Some(j) = &self.journal {
            if let Some(snap) = &snapshot {
                let succeeded = matches!(
                    result.as_ref().ok(),
                    Some(ToolExecution::Done(output)) if !output.is_error
                );
                if succeeded {
                    // Re-read the post-content from the snapshot's resolved
                    // path with the same capped-read helper that captured
                    // `before`, so the entry mirrors the free `FileSnapshot`
                    // exactly (path, resolved, before, after). We only read
                    // on success to avoid a wasted file read on a failed edit.
                    let after = read_preview_file(&snap.resolved);
                    j.push(JournalEntry {
                        path: snap.path.clone(),
                        resolved: snap.resolved.clone(),
                        before: snap.before.clone(),
                        after,
                    });
                }
            }
        }

        with_post_execution_diff(result, snapshot.as_ref())
    }
}

#[async_trait]
impl Tool for ApprovalTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> serde_json::Value {
        self.inner.parameters()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let name = self.inner.name();
        let policy_decision = self
            .policy
            .as_ref()
            .map(|policy| policy.decide(tool_call_id, name, &input))
            .unwrap_or(ToolPolicyDecision::NoDecision);
        if let ToolPolicyDecision::Deny { reason } = policy_decision.clone() {
            return Ok(denial_output(reason).into());
        }
        if matches!(policy_decision, ToolPolicyDecision::Defer) {
            return Ok(defer_output(tool_call_id, name, &input));
        }
        let effective_input = policy_updated_input(&policy_decision, &input);

        // Plan mode short-circuit: mutating tools are auto-denied
        // without a prompt. The agent sees a tool error, learns the
        // tool isn't available right now, and adapts. Read-only tools
        // pass straight through.
        if matches!(self.mode.get(), Mode::Plan) && !self.inner.is_read_only() {
            return Ok(plan_denial_output(self.inner.name()).into());
        }

        // Bypass mode short-circuit: mutating tools auto-allow with no
        // UI consultation — the whole point of `--dangerously-skip-permissions`.
        // Read-only tools pass straight through (they always would). The
        // consent gate that *enables* this mode lives in `code.rs`
        // (it refuses to enter Bypass in `--print`/background unless a
        // sentinel file shows prior interactive consent); by the time
        // we reach here the mode is legitimately set, so we trust it.
        // Mirrors Codex's `AskForApproval::Never`.
        if matches!(self.mode.get(), Mode::Bypass) && !self.inner.is_read_only() {
            return with_policy_context(
                self.execute_inner(tool_call_id, effective_input, on_update)
                    .await,
                &policy_decision,
            );
        }

        // AcceptEdits short-circuit: path-edit tools (write / edit /
        // hashline_edit) auto-allow without a modal. bash and any
        // other mutating tools still go through the regular
        // approval flow below, so the user retains a gate on
        // shell exec while drafting code. Mirrors Claude Code's
        // `acceptEdits` permission tier.
        if matches!(self.mode.get(), Mode::AcceptEdits) && is_path_edit_tool(name) {
            return with_policy_context(
                self.execute_inner(tool_call_id, effective_input, on_update)
                    .await,
                &policy_decision,
            );
        }
        let subject = approval_subject_with_base(name, &effective_input, self.base_dir.as_deref());
        if !matches!(policy_decision, ToolPolicyDecision::Ask { .. })
            && self.state.is_pre_allowed(name, &subject.value)
        {
            return with_policy_context(
                self.execute_inner(tool_call_id, effective_input, on_update)
                    .await,
                &policy_decision,
            );
        }
        if matches!(policy_decision, ToolPolicyDecision::Allow { .. }) {
            return with_policy_context(
                self.execute_inner(tool_call_id, effective_input, on_update)
                    .await,
                &policy_decision,
            );
        }

        // Build sanitized display text from the *raw* input (not from
        // subject.value, which is unsanitized). preview_call handles
        // all sanitization and formatting.
        let mut preview = preview_call(name, &effective_input);
        if let ToolPolicyDecision::Ask {
            reason: Some(reason),
            ..
        } = &policy_decision
        {
            preview = format!("{preview}\n\nPreToolUse hook requested confirmation: {reason}");
        }
        if !matches!(policy_decision, ToolPolicyDecision::Ask { .. })
            && self.ui.allows_smart_approval()
        {
            if let Some(smart) = self.smart_approval.as_ref() {
                match smart.decide(name, &preview, &effective_input).await {
                    SmartApprovalVerdict::Approve => {
                        emit_smart_approval_update(on_update.as_deref(), name, "approved", None);
                        return with_policy_context(
                            self.execute_inner(tool_call_id, effective_input, on_update)
                                .await,
                            &policy_decision,
                        );
                    }
                    SmartApprovalVerdict::Deny { reason } => {
                        emit_smart_approval_update(
                            on_update.as_deref(),
                            name,
                            "denied",
                            reason.as_deref(),
                        );
                        return Ok(denial_output(smart_denial_reason(reason)).into());
                    }
                    SmartApprovalVerdict::Escalate { .. } => {}
                }
            }
        }
        let always_label = sanitize_inline(&subject.suggested_label);
        match self.ui.decide(name, &preview, &always_label).await {
            PromptChoice::Allow => with_policy_context(
                self.execute_inner(tool_call_id, effective_input, on_update)
                    .await,
                &policy_decision,
            ),
            choice @ (PromptChoice::AlwaysAllow
            | PromptChoice::AllowSession
            | PromptChoice::Prefix
            | PromptChoice::GrantRoot
            | PromptChoice::Domain) => {
                // (M4/#10) All allow-family choices that record a rule go
                // through here: the scope variants (`Prefix`/`GrantRoot`/
                // `Domain`) pick their candidate rule off the subject,
                // falling back to `suggested_rule` when the candidate is
                // absent. `AlwaysAllow`/`AllowSession` use `suggested_rule`.
                let rule = rule_for_choice(&choice, &subject).cloned();
                let is_session = matches!(choice, PromptChoice::AllowSession);
                match rule {
                    Some(r) if is_session => self.state.record_session(r),
                    Some(r) => self.state.record_always(r),
                    None => {}
                }
                with_policy_context(
                    self.execute_inner(tool_call_id, effective_input, on_update)
                        .await,
                    &policy_decision,
                )
            }
            PromptChoice::Deny => Ok(denial_output(None).into()),
            PromptChoice::Paused {
                request_id,
                payload,
            } => Ok(wrap_paused_approval(request_id, payload, &input)),
        }
    }

    async fn resume(
        &self,
        tool_call_id: &str,
        request_id: &str,
        payload: serde_json::Value,
    ) -> PiResult<ToolExecution> {
        // Unwrap our own pause envelope: { ui_payload, tool_input }.
        // We need tool_input to re-run the regular non-UI gates and
        // then the inner tool on Allow.
        let (ui_payload, tool_input) = unwrap_paused_approval(payload);
        match self.ui.resume_decide(request_id, ui_payload).await {
            PromptChoice::Allow => {
                self.execute_resumed_approval(tool_call_id, tool_input, ResumeRecord::None)
                    .await
            }
            choice @ (PromptChoice::AlwaysAllow
            | PromptChoice::AllowSession
            | PromptChoice::Prefix
            | PromptChoice::GrantRoot
            | PromptChoice::Domain) => {
                self.execute_resumed_approval(
                    tool_call_id,
                    tool_input,
                    ResumeRecord::Record { choice },
                )
                .await
            }
            PromptChoice::Deny => Ok(denial_output(None).into()),
            PromptChoice::Paused {
                request_id,
                payload,
            } => Ok(wrap_paused_approval(request_id, payload, &tool_input)),
        }
    }
}

/// How a resumed (un-paused) approval should record its decision.
/// Carries the user's `PromptChoice` so the M4/#10 scope variants
/// (`Prefix`/`GrantRoot`/`Domain`) survive the pause/resume boundary —
/// the rule is resolved against the subject at resume time, after the
/// model re-supplies `tool_input`.
#[derive(Clone)]
enum ResumeRecord {
    None,
    Record { choice: PromptChoice },
}

/// (M4/#10) Resolve which `AllowRule` a `PromptChoice` records, for the
/// always/session tiers. The scope variants (`Prefix`/`GrantRoot`/`Domain`)
/// pick their candidate rule off the subject when present, falling back to
/// the default `suggested_rule` when the candidate is `None` (so a UI that
/// offers, say, `Prefix` for a bare command without a prefix tier still
/// records a sensible rule instead of silently allowing nothing).
///
/// Returns `None` for choices that don't record a rule at all (`Allow`,
/// `Deny`, `Paused`).
fn rule_for_choice<'a>(
    choice: &PromptChoice,
    subject: &'a ApprovalSubject,
) -> Option<&'a AllowRule> {
    match choice {
        PromptChoice::Allow | PromptChoice::Deny | PromptChoice::Paused { .. } => None,
        PromptChoice::AlwaysAllow | PromptChoice::AllowSession => Some(&subject.suggested_rule),
        PromptChoice::Prefix => subject
            .prefix_rule
            .as_ref()
            .or(Some(&subject.suggested_rule)),
        PromptChoice::GrantRoot => subject.root_rule.as_ref().or(Some(&subject.suggested_rule)),
        PromptChoice::Domain => subject
            .domain_rule
            .as_ref()
            .or(Some(&subject.suggested_rule)),
    }
}

impl ApprovalTool {
    async fn execute_resumed_approval(
        &self,
        tool_call_id: &str,
        tool_input: serde_json::Value,
        record: ResumeRecord,
    ) -> PiResult<ToolExecution> {
        let name = self.inner.name();
        let policy_decision = self
            .policy
            .as_ref()
            .map(|policy| policy.decide(tool_call_id, name, &tool_input))
            .unwrap_or(ToolPolicyDecision::NoDecision);
        if let ToolPolicyDecision::Deny { reason } = policy_decision.clone() {
            return Ok(denial_output(reason).into());
        }
        if matches!(policy_decision, ToolPolicyDecision::Defer) {
            return Ok(defer_output(tool_call_id, name, &tool_input));
        }
        let effective_input = policy_updated_input(&policy_decision, &tool_input);

        if matches!(self.mode.get(), Mode::Plan) && !self.inner.is_read_only() {
            return Ok(plan_denial_output(self.inner.name()).into());
        }

        let subject = approval_subject_with_base(name, &effective_input, self.base_dir.as_deref());

        if !matches!(policy_decision, ToolPolicyDecision::Ask { .. })
            && self.ui.allows_smart_approval()
        {
            if let Some(smart) = self.smart_approval.as_ref() {
                let preview = preview_call(name, &effective_input);
                if let SmartApprovalVerdict::Deny { reason } =
                    smart.decide(name, &preview, &effective_input).await
                {
                    return Ok(denial_output(smart_denial_reason(reason)).into());
                }
            }
        }

        if let ResumeRecord::Record { choice } = &record {
            // (M4/#10) Resolve the scope rule against the freshly-rebuilt
            // subject (the user picked a scope at pause time; the rule
            // pattern must reflect the resumed tool_input).
            if let Some(r) = rule_for_choice(choice, &subject).cloned() {
                if matches!(choice, PromptChoice::AllowSession) {
                    self.state.record_session(r);
                } else {
                    self.state.record_always(r);
                }
            }
        }

        with_policy_context(
            self.execute_inner(tool_call_id, effective_input, None)
                .await,
            &policy_decision,
        )
    }
}

fn policy_updated_input(
    decision: &ToolPolicyDecision,
    original: &serde_json::Value,
) -> serde_json::Value {
    match decision {
        ToolPolicyDecision::Allow {
            updated_input: Some(input),
            ..
        }
        | ToolPolicyDecision::Ask {
            updated_input: Some(input),
            ..
        } => input.clone(),
        _ => original.clone(),
    }
}

fn with_policy_context(
    result: PiResult<ToolExecution>,
    decision: &ToolPolicyDecision,
) -> PiResult<ToolExecution> {
    let Some(context) = policy_additional_context(decision) else {
        return result;
    };
    result.map(|execution| match execution {
        ToolExecution::Done(mut output) => {
            output
                .content
                .push(ContentBlock::Text(TextContent::new(format!(
                    "Additional context from PreToolUse hook:\n\n{context}"
                ))));
            ToolExecution::Done(output)
        }
        paused => paused,
    })
}

fn with_post_execution_diff(
    result: PiResult<ToolExecution>,
    snapshot: Option<&crate::commands::code_diff::FileSnapshot>,
) -> PiResult<ToolExecution> {
    let Some(snapshot) = snapshot else {
        return result;
    };
    let Some(diff) = crate::commands::code_diff::post_execution_diff(snapshot) else {
        return result;
    };
    result.map(|execution| match execution {
        ToolExecution::Done(mut output) if !output.is_error => {
            output
                .content
                .push(ContentBlock::Text(TextContent::new(format!(
                    "Filesystem delta after execution:\n{diff}"
                ))));
            ToolExecution::Done(output)
        }
        other => other,
    })
}

fn policy_additional_context(decision: &ToolPolicyDecision) -> Option<&str> {
    match decision {
        ToolPolicyDecision::Allow {
            additional_context: Some(context),
            ..
        } if !context.trim().is_empty() => Some(context.as_str()),
        ToolPolicyDecision::Ask {
            additional_context: Some(context),
            ..
        } if !context.trim().is_empty() => Some(context.as_str()),
        _ => None,
    }
}

fn defer_output(tool_call_id: &str, tool_name: &str, input: &serde_json::Value) -> ToolExecution {
    ToolExecution::Paused {
        request_id: if tool_call_id.is_empty() {
            format!("pre-tool-defer-{tool_name}")
        } else {
            tool_call_id.to_string()
        },
        kind: "pre_tool_defer".to_string(),
        payload: serde_json::json!({
            "tool_name": tool_name,
            "tool_input": input,
        }),
    }
}

/// Wrap the UI's pause payload alongside the original tool input so
/// the resume hook has everything it needs to re-run the inner tool
/// on Allow without consulting the agent loop.
fn wrap_paused_approval(
    request_id: String,
    ui_payload: serde_json::Value,
    tool_input: &serde_json::Value,
) -> ToolExecution {
    ToolExecution::Paused {
        request_id,
        kind: "approval".to_string(),
        payload: serde_json::json!({
            "ui_payload": ui_payload,
            "tool_input": tool_input,
        }),
    }
}

fn unwrap_paused_approval(payload: serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    if let serde_json::Value::Object(mut obj) = payload {
        let ui_payload = obj.remove("ui_payload").unwrap_or(serde_json::Value::Null);
        let tool_input = obj.remove("tool_input").unwrap_or(serde_json::Value::Null);
        (ui_payload, tool_input)
    } else {
        (serde_json::Value::Null, serde_json::Value::Null)
    }
}

/// Tool output for an explicit user denial.
pub fn denial_output(reason: Option<String>) -> ToolOutput {
    let text = reason.unwrap_or_else(|| {
        "user denied execution of this tool call; ask them for alternative approaches or a different strategy".into()
    });
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
}

fn smart_denial_reason(reason: Option<String>) -> Option<String> {
    Some(match reason {
        Some(reason) if !reason.trim().is_empty() => {
            format!("smart approval denied this tool call: {}", reason.trim())
        }
        _ => "smart approval denied this tool call".to_string(),
    })
}

fn emit_smart_approval_update(
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    tool_name: &str,
    decision: &str,
    reason: Option<&str>,
) {
    let Some(on_update) = on_update else {
        return;
    };
    let reason = reason.map(str::trim).filter(|value| !value.is_empty());
    let text = match (decision, reason) {
        ("approved", _) => format!("smart approval auto-approved `{tool_name}`"),
        ("denied", Some(reason)) => {
            format!("smart approval auto-denied `{tool_name}`: {reason}")
        }
        ("denied", None) => format!("smart approval auto-denied `{tool_name}`"),
        _ => format!("smart approval {decision} `{tool_name}`"),
    };
    on_update(ToolUpdate {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(serde_json::json!({
            "kind": "smart_approval",
            "decision": decision,
            "tool": tool_name,
            "reason": reason,
        })),
    });
}

fn emit_tool_started_update(
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    tool_name: &str,
) {
    let Some(on_update) = on_update else {
        return;
    };
    on_update(ToolUpdate {
        content: Vec::new(),
        details: Some(serde_json::json!({
            "kind": "tool_started",
            "tool": tool_name,
        })),
    });
}

fn plan_denial_output(tool_name: &str) -> ToolOutput {
    let text = format!(
        "session is in plan mode: `{tool_name}` is unavailable. \
         You can read, search, and reason — describe the changes you'd \
         make and ask the user to switch to normal mode (Shift+Tab or \
         /plan) when they're ready to apply them."
    );
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
}

/// Maximum chars to display from a single tool-argument value. A model
/// that generates a 100 KB heredoc would otherwise bury the approval
/// menu off-screen and the user approves by habit — an attack surface
/// for prompt injection.
pub const MAX_PREVIEW_CHARS: usize = 400;

/// Render a one-line preview for the approval UI.
///
/// All model-controlled strings pass through `sanitize`, which:
/// 1. Strips ANSI escape sequences (`\x1b[...m`, etc.) — a malicious
///    payload could otherwise clear the screen and spoof a benign
///    approval prompt.
/// 2. Drops ASCII control bytes other than `\n` / `\t`.
/// 3. Caps length at `MAX_PREVIEW_CHARS` with a `…` suffix.
pub fn preview_call(tool: &str, input: &serde_json::Value) -> String {
    match tool {
        "bash" => sanitize(
            input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing command>"),
        ),
        "bash_output" => {
            let path = sanitize(
                field(input, "logPath")
                    .or_else(|| field(input, "log_path"))
                    .unwrap_or("<missing log path>"),
            );
            match input_pid(input) {
                Some(pid) => format!("bash_output {path} (pid {pid})"),
                None => format!("bash_output {path}"),
            }
        }
        "kill_bash" => {
            let pid = input_pid(input)
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "<missing pid>".to_string());
            format!("kill_bash {pid}")
        }
        "write" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            let len = input
                .get("content")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            with_diff(format!("write {path} ({len} bytes)"), tool, input)
        }
        "edit" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            with_diff(format!("edit {path}"), tool, input)
        }
        "hashline_edit" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            with_diff(format!("hashline_edit {path}"), tool, input)
        }
        "notebook_edit" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            let mode = sanitize(field(input, "mode").unwrap_or("replace"));
            let cell = input
                .get("cell_index")
                .and_then(|v| v.as_u64())
                .map_or_else(|| "?".to_string(), |v| v.to_string());
            format!("notebook_edit {path} cell {cell} ({mode})")
        }
        "notebook_execute" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            let timeout = input
                .get("timeout_seconds")
                .and_then(|v| v.as_u64())
                .map_or_else(|| "120".to_string(), |v| v.to_string());
            format!("notebook_execute {path} (timeout {timeout}s)")
        }
        _ => {
            let raw = input.to_string();
            let clipped = sanitize(&raw);
            format!("{tool}: {clipped}")
        }
    }
}

fn with_diff(header: String, tool: &str, input: &serde_json::Value) -> String {
    let diff = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            crate::commands::code_diff::approval_diff_preview_with_cwd(tool, input, &cwd)
        })
        .or_else(|| crate::commands::code_diff::approval_diff_preview(tool, input));
    match diff {
        Some(diff) if !diff.is_empty() => format!("{header}\n{}", sanitize(&diff)),
        _ => header,
    }
}

fn field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

fn input_pid(input: &serde_json::Value) -> Option<i64> {
    input.get("pid").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|pid| i64::try_from(pid).ok()))
            .or_else(|| value.as_str().and_then(|pid| pid.parse().ok()))
    })
}

/// Defang a model-supplied string so it can't rewrite the terminal
/// under the approval prompt.
///
/// ANSI escape handling is a three-state machine (Normal → AfterEsc →
/// InCsi) because a CSI sequence is `\x1b [ params terminator`, and
/// the `[` itself lives in the 0x40..=0x7E "final byte" range — so
/// naively terminating on that range ends the sequence at `[` and
/// leaks the payload bytes through. Other ESC variants (single-char
/// `ESC c`, `ESC D`, etc.) are handled by the AfterEsc branch.
///
/// Also drops ASCII control bytes (below 0x20 and 0x7F) except `\n`
/// and `\t`, and truncates past `MAX_PREVIEW_CHARS` with a `…`.
pub fn sanitize(s: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        AfterEsc,
        InCsi,
    }
    let mut out = String::with_capacity(s.len().min(MAX_PREVIEW_CHARS + 1));
    let mut state = State::Normal;
    for c in s.chars() {
        match state {
            State::AfterEsc => {
                state = if c == '[' {
                    State::InCsi
                } else {
                    State::Normal
                };
                continue;
            }
            State::InCsi => {
                // CSI params are in 0x20..=0x3F; terminator is 0x40..=0x7E.
                if matches!(c, '\x40'..='\x7e') {
                    state = State::Normal;
                }
                continue;
            }
            State::Normal => {}
        }
        if c == '\x1b' {
            state = State::AfterEsc;
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        if out.chars().count() >= MAX_PREVIEW_CHARS {
            out.push('\u{2026}');
            break;
        }
        out.push(c);
    }
    out
}

/// Defang a short string intended to stay on one prompt line.
///
/// Unlike [`sanitize`], this replaces newlines and tabs with spaces so a
/// model-controlled command/path cannot visually split the approval menu.
pub fn sanitize_inline(s: &str) -> String {
    sanitize(s)
        .chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeTool;

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            "bash"
        }

        fn label(&self) -> &str {
            "Bash"
        }

        fn description(&self) -> &str {
            "fake mutating shell tool"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }

        fn is_read_only(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> PiResult<ToolExecution> {
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("ran"))],
                details: None,
                is_error: false,
            }
            .into())
        }
    }

    struct CountingUi {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ApprovalUi for CountingUi {
        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _always_rule: &str,
        ) -> PromptChoice {
            self.calls.fetch_add(1, Ordering::Relaxed);
            PromptChoice::Deny
        }
    }

    struct HeadlessCountingUi {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ApprovalUi for HeadlessCountingUi {
        fn allows_smart_approval(&self) -> bool {
            false
        }

        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _always_rule: &str,
        ) -> PromptChoice {
            self.calls.fetch_add(1, Ordering::Relaxed);
            PromptChoice::Deny
        }
    }

    struct AllowingUi;

    #[async_trait]
    impl ApprovalUi for AllowingUi {
        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _always_rule: &str,
        ) -> PromptChoice {
            PromptChoice::Allow
        }
    }

    struct StaticSmartApproval(SmartApprovalVerdict);

    #[async_trait]
    impl SmartApproval for StaticSmartApproval {
        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _input: &serde_json::Value,
        ) -> SmartApprovalVerdict {
            self.0.clone()
        }
    }

    fn approval_text(output: ToolOutput) -> String {
        output
            .content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn update_text(update: &ToolUpdate) -> String {
        update
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn update_kind(update: &ToolUpdate) -> Option<&str> {
        update
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(|kind| kind.as_str())
    }

    #[test]
    fn sanitize_strips_clear_screen_spoof() {
        let evil = "\x1b[2J\x1b[H FAKE PROMPT [a]llow: echo hi\nrm -rf ~/";
        let clean = sanitize(evil);
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains("rm -rf"));
    }

    #[test]
    fn sanitize_drops_cursor_up_line_kill() {
        let evil = "../legit.py\x1b[A\x1b[K../etc/passwd";
        assert_eq!(sanitize(evil), "../legit.py../etc/passwd");
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "x".repeat(MAX_PREVIEW_CHARS * 2);
        let clean = sanitize(&long);
        assert!(clean.ends_with('\u{2026}'));
        assert!(clean.chars().count() <= MAX_PREVIEW_CHARS + 1);
    }

    #[test]
    fn sanitize_keeps_newlines_and_tabs() {
        assert_eq!(sanitize("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn sanitize_inline_flattens_newlines_and_tabs() {
        assert_eq!(
            sanitize_inline("bash(echo a\n[A] fake\tmenu)"),
            "bash(echo a [A] fake menu)"
        );
    }

    #[test]
    fn policy_allow_can_replace_tool_input() {
        let original = serde_json::json!({"command": "npm test"});
        let decision = ToolPolicyDecision::Allow {
            updated_input: Some(serde_json::json!({"command": "npm run lint"})),
            additional_context: None,
        };
        assert_eq!(
            policy_updated_input(&decision, &original),
            serde_json::json!({"command": "npm run lint"})
        );
    }

    #[test]
    fn policy_ask_can_replace_tool_input() {
        let original = serde_json::json!({"command": "npm test"});
        let decision = ToolPolicyDecision::Ask {
            reason: Some("prefer lint first".to_string()),
            updated_input: Some(serde_json::json!({"command": "npm run lint"})),
            additional_context: None,
        };
        assert_eq!(
            policy_updated_input(&decision, &original),
            serde_json::json!({"command": "npm run lint"})
        );
    }

    #[test]
    fn policy_context_appends_to_tool_output() {
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("ok"))],
            details: None,
            is_error: false,
        };
        let decision = ToolPolicyDecision::Allow {
            updated_input: None,
            additional_context: Some("remember to cite files".to_string()),
        };
        let wrapped = with_policy_context(Ok(output.into()), &decision).unwrap();
        let ToolExecution::Done(output) = wrapped else {
            panic!("expected done output");
        };
        let text = output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ok"));
        assert!(text.contains("remember to cite files"));
    }

    #[test]
    fn post_execution_diff_appends_to_successful_tool_output() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notes.txt");
        std::fs::write(&path, "before\n").unwrap();
        let snapshot = crate::commands::code_diff::file_snapshot_before_tool(
            "write",
            &serde_json::json!({"path":"notes.txt"}),
            temp.path(),
        )
        .unwrap();
        std::fs::write(&path, "after\n").unwrap();
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("ok"))],
            details: None,
            is_error: false,
        };

        let wrapped = with_post_execution_diff(Ok(output.into()), Some(&snapshot)).unwrap();
        let ToolExecution::Done(output) = wrapped else {
            panic!("expected done output");
        };
        let text = output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Filesystem delta after execution"));
        assert!(text.contains("-before"));
        assert!(text.contains("+after"));
    }

    // ── EditJournal recording ────────────────────────────────────────

    /// Fake `write`-like tool: writes `input["content"]` to `input["path"]`
    /// (resolved against cwd) and returns a successful non-error output.
    /// Used to exercise `execute_inner`'s journal push end-to-end.
    struct WriteFileTool;

    #[async_trait]
    impl Tool for WriteFileTool {
        fn name(&self) -> &str {
            "write"
        }
        fn label(&self) -> &str {
            "Write"
        }
        fn description(&self) -> &str {
            "fake write tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> PiResult<ToolExecution> {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("out.txt");
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            std::fs::write(path, content).ok();
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("wrote"))],
                details: None,
                is_error: false,
            }
            .into())
        }
    }

    #[test]
    fn execute_inner_journals_successful_edit_and_skips_failed() {
        // `execute_inner` resolves the tool path against the process cwd
        // (it calls `std::env::current_dir()` for the snapshot), so pin the
        // cwd to a temp dir for the duration of BOTH scenarios. We keep
        // them in ONE test function so the process-global cwd isn't raced
        // by a sibling test — there is no `serial_test` in this crate.
        let temp = tempfile::tempdir().unwrap();
        let prior_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(temp.path()).unwrap();

        // ── scenario 1: a successful edit is journaled ──
        let target = temp.path().join("notes.txt");
        std::fs::write(&target, "before\n").unwrap();

        let journal = Arc::new(crate::commands::code_diff::EditJournal::new());
        let tool = ApprovalTool::new(
            Box::new(WriteFileTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(AllowingUi),
        )
        .with_base_dir(Some(temp.path().to_path_buf()))
        .with_journal(Arc::clone(&journal));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"path":"notes.txt","content":"after\n"}),
            None,
        ))
        .unwrap();
        assert!(matches!(execution, ToolExecution::Done(_)));

        // The journal captured exactly one entry: before=before, after=after.
        assert_eq!(journal.len(), 1);
        let entry = journal.pop().unwrap();
        assert_eq!(entry.path, "notes.txt");
        assert_eq!(entry.before.as_deref(), Some("before\n"));
        assert_eq!(entry.after.as_deref(), Some("after\n"));
        assert!(journal.is_empty());

        // ── scenario 2: a failed edit is NOT journaled ──
        // A DeniedTool returns an error output; `execute_inner` must skip
        // the push so `/undo` never reverts a no-op.
        struct DeniedTool;
        #[async_trait]
        impl Tool for DeniedTool {
            fn name(&self) -> &str {
                "write"
            }
            fn label(&self) -> &str {
                "Write"
            }
            fn description(&self) -> &str {
                "fake failing write tool"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_read_only(&self) -> bool {
                false
            }
            async fn execute(
                &self,
                _tool_call_id: &str,
                _input: serde_json::Value,
                _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
            ) -> PiResult<ToolExecution> {
                Ok(ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new("boom"))],
                    details: None,
                    is_error: true,
                }
                .into())
            }
        }

        let failing_tool = ApprovalTool::new(
            Box::new(DeniedTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(AllowingUi),
        )
        .with_base_dir(Some(temp.path().to_path_buf()))
        .with_journal(Arc::clone(&journal));

        let _ = futures::executor::block_on(failing_tool.execute(
            "call-2",
            serde_json::json!({"path":"notes.txt","content":"after\n"}),
            None,
        ))
        .unwrap();

        assert!(journal.is_empty(), "a failed edit must not be journaled");

        // Restore the process cwd so we don't perturb sibling tests in the
        // same binary that happen to read it.
        if let Some(p) = prior_cwd {
            let _ = std::env::set_current_dir(p);
        }
    }

    #[test]
    fn smart_approval_approve_bypasses_manual_prompt() {
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(CountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Approve,
        ))));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"command":"cargo test"}),
            None,
        ))
        .unwrap();

        let ToolExecution::Done(output) = execution else {
            panic!("expected done output");
        };
        assert_eq!(approval_text(output), "ran");
        assert_eq!(ui_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn smart_approval_approve_emits_decision_update() {
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let updates: Arc<Mutex<Vec<ToolUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&updates);
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(CountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Approve,
        ))));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"command":"cargo test"}),
            Some(Box::new(move |update| {
                seen.lock().unwrap().push(update);
            })),
        ))
        .unwrap();

        assert!(matches!(execution, ToolExecution::Done(_)));
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(update_kind(&updates[0]), Some("smart_approval"));
        assert!(update_text(&updates[0]).contains("smart approval auto-approved `bash`"));
        assert_eq!(
            updates[0].details.as_ref().unwrap()["kind"],
            "smart_approval"
        );
        assert_eq!(updates[0].details.as_ref().unwrap()["decision"], "approved");
        assert_eq!(update_kind(&updates[1]), Some("tool_started"));
        assert_eq!(ui_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ui_can_disable_smart_approval_for_unremembered_mutating_call() {
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(HeadlessCountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Approve,
        ))));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"command":"cargo test"}),
            None,
        ))
        .unwrap();

        let ToolExecution::Done(output) = execution else {
            panic!("expected done output");
        };
        assert!(output.is_error);
        assert!(approval_text(output).contains("user denied execution"));
        assert_eq!(ui_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ui_smart_approval_disable_still_honors_remembered_rules() {
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(ApprovalState::new());
        state.record_always(AllowRule::exact("bash", "cargo test"));
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::clone(&state),
            ModeFlag::new(Mode::Normal),
            Arc::new(HeadlessCountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Deny {
                reason: Some("would deny if consulted".to_string()),
            },
        ))));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"command":"cargo test"}),
            None,
        ))
        .unwrap();

        let ToolExecution::Done(output) = execution else {
            panic!("expected done output");
        };
        assert!(!output.is_error);
        assert_eq!(approval_text(output), "ran");
        assert_eq!(ui_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn smart_approval_deny_returns_tool_error_without_prompt() {
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(CountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Deny {
                reason: Some("dangerous command".to_string()),
            },
        ))));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"command":"rm -rf target"}),
            None,
        ))
        .unwrap();

        let ToolExecution::Done(output) = execution else {
            panic!("expected done output");
        };
        assert!(output.is_error);
        assert!(approval_text(output).contains("smart approval denied"));
        assert_eq!(ui_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn resumed_always_allow_records_after_smart_veto() {
        let state = Arc::new(ApprovalState::new());
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::clone(&state),
            ModeFlag::new(Mode::Normal),
            Arc::new(CountingUi {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Deny {
                reason: Some("nope".to_string()),
            },
        ))));

        let execution = futures::executor::block_on(tool.execute_resumed_approval(
            "call-1",
            serde_json::json!({"command":"cargo test"}),
            ResumeRecord::Record {
                choice: PromptChoice::AlwaysAllow,
            },
        ))
        .unwrap();

        let ToolExecution::Done(output) = execution else {
            panic!("expected done output");
        };
        assert!(output.is_error);
        assert!(!state.is_pre_allowed("bash", "cargo test"));
    }

    #[test]
    fn smart_approval_deny_emits_decision_update() {
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let updates: Arc<Mutex<Vec<ToolUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&updates);
        let tool = ApprovalTool::new(
            Box::new(FakeTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Normal),
            Arc::new(CountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_smart_approval(Some(Arc::new(StaticSmartApproval(
            SmartApprovalVerdict::Deny {
                reason: Some("dangerous command".to_string()),
            },
        ))));

        let execution = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"command":"rm -rf target"}),
            Some(Box::new(move |update| {
                seen.lock().unwrap().push(update);
            })),
        ))
        .unwrap();

        assert!(matches!(execution, ToolExecution::Done(_)));
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert!(update_text(&updates[0])
            .contains("smart approval auto-denied `bash`: dangerous command"));
        assert_eq!(
            updates[0].details.as_ref().unwrap()["kind"],
            "smart_approval"
        );
        assert_eq!(updates[0].details.as_ref().unwrap()["decision"], "denied");
        assert_eq!(
            updates[0].details.as_ref().unwrap()["reason"],
            "dangerous command"
        );
        assert_eq!(ui_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn policy_defer_returns_pause_sentinel() {
        let execution = defer_output("call-1", "read", &serde_json::json!({"path":"README.md"}));
        let ToolExecution::Paused {
            request_id,
            kind,
            payload,
        } = execution
        else {
            panic!("expected paused output");
        };
        assert_eq!(request_id, "call-1");
        assert_eq!(kind, "pre_tool_defer");
        assert_eq!(payload["tool_name"], "read");
    }

    #[test]
    fn sanitize_handles_single_char_escape() {
        // ESC c (non-CSI) is a full reset. We drop ESC + the next char.
        assert_eq!(sanitize("before\x1bcafter"), "beforeafter");
    }

    // ── wildcard_match ──────────────────────────────────────────────

    #[test]
    fn wildcard_empty_matches_anything() {
        assert!(wildcard_match("", "anything goes"));
    }

    #[test]
    fn wildcard_exact() {
        assert!(wildcard_match("npm run build", "npm run build"));
        assert!(!wildcard_match("npm run build", "npm run test"));
    }

    #[test]
    fn wildcard_prefix() {
        assert!(wildcard_match("npm run *", "npm run build"));
        assert!(wildcard_match("npm run *", "npm run test"));
        assert!(!wildcard_match("npm run *", "npm-run-script"));
    }

    #[test]
    fn wildcard_suffix() {
        assert!(wildcard_match("* install", "npm install"));
        assert!(wildcard_match("* install", "cargo install"));
        assert!(!wildcard_match("* install", "installing"));
    }

    #[test]
    fn wildcard_middle() {
        assert!(wildcard_match("git * main", "git checkout main"));
        assert!(wildcard_match("git * main", "git log --oneline main"));
    }

    #[test]
    fn wildcard_multiple_stars() {
        assert!(wildcard_match("a*b*c", "aXbYc"));
        assert!(wildcard_match("a*b*c", "abc"));
        assert!(!wildcard_match("a*b*c", "acXb"));
    }

    #[test]
    fn exact_does_not_match_compound() {
        // Security property: an exact rule for a simple command must not
        // match a compound command that starts the same way.
        assert!(!wildcard_match(
            "npm run build",
            "npm run build && rm -rf /"
        ));
    }

    #[test]
    fn wildcard_just_star() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*", ""));
    }

    // ── AllowRule ───────────────────────────────────────────────────

    #[test]
    fn rule_empty_pattern_matches_all() {
        let rule = AllowRule::tool_all("bash");
        assert!(rule.matches("bash", "anything"));
        assert!(!rule.matches("edit", "anything"));
    }

    #[test]
    fn rule_exact_command() {
        let rule = AllowRule::exact("bash", "npm run build");
        assert!(rule.matches("bash", "npm run build"));
        assert!(!rule.matches("bash", "npm run test"));
    }

    #[test]
    fn rule_exact_treats_star_literally() {
        let rule = AllowRule::exact("bash", "echo *");
        assert!(rule.matches("bash", "echo *"));
        assert!(!rule.matches("bash", "echo hello"));
        assert!(!rule.matches("bash", "echo hello && rm -rf /"));
    }

    #[test]
    fn rule_wildcard_opt_in_matches_star() {
        let rule = AllowRule::wildcard("bash", "echo *");
        assert!(rule.matches("bash", "echo hello"));
    }

    #[test]
    fn rule_exact_path_does_not_overmatch_prefix() {
        let rule = AllowRule::exact("edit", "src/foo");
        assert!(rule.matches("edit", "src/foo"));
        assert!(!rule.matches("edit", "src/foobar"));
    }

    // ── preview_call ────────────────────────────────────────────────

    #[test]
    fn preview_write_includes_added_content_diff() {
        let input =
            serde_json::json!({"path":"__new_file_preview_test__.rs","content":"fn main() {}\n"});
        let preview = preview_call("write", &input);
        assert!(preview.starts_with("write __new_file_preview_test__.rs"));
        assert!(preview.contains("--- /dev/null"));
        assert!(preview.contains("+fn main() {}"));
    }

    #[test]
    fn preview_edit_includes_old_new_diff() {
        let input = serde_json::json!({
            "path":"src/lib.rs",
            "oldText":"let x = 1;",
            "newText":"let x = 2;"
        });
        let preview = preview_call("edit", &input);
        assert!(preview.starts_with("edit src/lib.rs"));
        assert!(preview.contains("-let x = 1;"));
        assert!(preview.contains("+let x = 2;"));
    }

    #[test]
    fn preview_hashline_edit_summarizes_operations() {
        let input = serde_json::json!({
            "path":"src/lib.rs",
            "edits":[{"op":"append","pos":"10#AA","lines":["x"]}]
        });
        let preview = preview_call("hashline_edit", &input);
        assert!(preview.starts_with("hashline_edit src/lib.rs"));
        assert!(preview.contains("1. append 10#AA with 1 line"));
    }

    #[test]
    fn preview_background_bash_companions() {
        assert_eq!(
            preview_call(
                "bash_output",
                &serde_json::json!({"logPath":"/tmp/pi-bash-bg-123.log","pid":1234})
            ),
            "bash_output /tmp/pi-bash-bg-123.log (pid 1234)"
        );
        assert_eq!(
            preview_call("kill_bash", &serde_json::json!({"pid":"1234"})),
            "kill_bash 1234"
        );
    }

    // ── approval_subject ────────────────────────────────────────────

    #[test]
    fn subject_bash_extracts_command() {
        let input = serde_json::json!({"command": "npm run build"});
        let subj = approval_subject("bash", &input);
        // (M4/#10) The matched value is the FULL command, but the suggested
        // "always allow" rule now keys on the PREFIX (`<bin> <first-arg> *`)
        // — `npm run *` — so one rule covers `npm run build`, `npm run test`,
        // etc. The broader ROOT tier (`npm *`, the whole binary) is offered
        // as `root_rule` (GrantRoot).
        assert_eq!(subj.value, "npm run build");
        assert_eq!(subj.suggested_rule.tool, "bash");
        assert!(subj.suggested_rule.wildcard, "bash rule is a wildcard");
        assert_eq!(subj.suggested_rule.pattern, "npm run *");
        assert_eq!(subj.suggested_label, "bash(npm run *)");
        // ROOT candidate is the binary-only tier.
        let root = subj
            .root_rule
            .expect("root_rule present for bash with args");
        assert!(root.wildcard);
        assert_eq!(root.pattern, "npm *");
        // PREFIX candidate equals the suggested rule (the default IS prefix).
        let prefix = subj
            .prefix_rule
            .expect("prefix_rule present for bash with args");
        assert_eq!(prefix.pattern, "npm run *");
        // No domain tier for bash.
        assert!(subj.domain_rule.is_none());
    }

    #[test]
    fn subject_bash_single_token_is_exact() {
        // A bare binary with no args records an exact rule (no wildcard),
        // and offers no broader tiers.
        let input = serde_json::json!({"command": "git"});
        let subj = approval_subject("bash", &input);
        assert_eq!(subj.value, "git");
        assert_eq!(subj.suggested_rule.tool, "bash");
        assert!(
            !subj.suggested_rule.wildcard,
            "single-token bash rule is exact"
        );
        assert_eq!(subj.suggested_rule.pattern, "git");
        assert_eq!(subj.suggested_label, "bash(git)");
        assert!(subj.prefix_rule.is_none());
        assert!(subj.root_rule.is_none());
        assert!(subj.domain_rule.is_none());
    }

    #[test]
    fn subject_bash_leading_whitespace_is_trimmed() {
        let input = serde_json::json!({"command": "   npm   run build  "});
        let subj = approval_subject("bash", &input);
        assert_eq!(subj.value, "   npm   run build  ");
        assert_eq!(subj.suggested_rule.pattern, "npm run *");
        assert_eq!(subj.suggested_label, "bash(npm run *)");
    }

    #[test]
    fn bash_always_allow_on_prefix_covers_varied_subcommands() {
        // (Issue-2 + M4/#10) The user-reported bug: "always allow" on
        // `npm run build` never re-matched `npm run test`, so the user was
        // re-prompted every call. With the PREFIX-keyed wildcard rule
        // (`npm run *`, the new default), one allow covers all `npm run …`
        // subcommands. A bare `npm install` is NOT covered (that needs the
        // ROOT tier `npm *` via GrantRoot).
        let state = ApprovalState::new();
        let input = serde_json::json!({"command": "npm run build"});
        let subj = approval_subject("bash", &input);
        state.record_always(subj.suggested_rule);
        // The same prefix with different args is pre-allowed.
        assert!(
            state.is_pre_allowed("bash", "npm run test"),
            "npm run test covered by the npm run * rule"
        );
        assert!(
            state.is_pre_allowed("bash", "npm run build --watch"),
            "npm run build --watch covered by the npm run * rule"
        );
        // A different binary is NOT pre-allowed (the rule doesn't over-match
        // to other tools).
        assert!(
            !state.is_pre_allowed("bash", "cargo test"),
            "cargo test NOT covered by the npm run * rule"
        );
        // A bare `npm` (no args) is NOT matched by `npm run *` (the wildcard
        // requires the `npm run ` prefix) — a deliberate precision choice.
        assert!(
            !state.is_pre_allowed("bash", "npm"),
            "bare npm not covered by 'npm run *' (requires the prefix)"
        );
        // `npm install` is NOT covered by the prefix rule — that needs the
        // ROOT tier (`npm *` via GrantRoot). Confirms the prefix default is
        // tighter than the old binary-only behavior.
        assert!(
            !state.is_pre_allowed("bash", "npm install"),
            "npm install NOT covered by 'npm run *' — use GrantRoot (npm *) for that"
        );
    }

    // ── M4/#10 per-call scope choices ─────────────────────────────────

    #[test]
    fn rule_for_choice_resolves_scope_variants() {
        // bash `npm run build`: suggested=prefix `npm run *`, root=`npm *`.
        let input = serde_json::json!({"command": "npm run build"});
        let subj = approval_subject("bash", &input);
        assert_eq!(
            rule_for_choice(&PromptChoice::AlwaysAllow, &subj)
                .unwrap()
                .pattern,
            "npm run *"
        );
        assert_eq!(
            rule_for_choice(&PromptChoice::Prefix, &subj)
                .unwrap()
                .pattern,
            "npm run *"
        );
        assert_eq!(
            rule_for_choice(&PromptChoice::GrantRoot, &subj)
                .unwrap()
                .pattern,
            "npm *"
        );
        // Domain falls back to suggested (no domain tier for bash).
        assert_eq!(
            rule_for_choice(&PromptChoice::Domain, &subj)
                .unwrap()
                .pattern,
            "npm run *"
        );
        // Allow/Deny record nothing.
        assert!(rule_for_choice(&PromptChoice::Allow, &subj).is_none());
        assert!(rule_for_choice(&PromptChoice::Deny, &subj).is_none());
    }

    #[test]
    fn rule_for_choice_falls_back_when_no_candidate() {
        // A bare `git` (no args) has no prefix/root/domain tiers; the scope
        // choices fall back to the suggested (exact) rule rather than None.
        let input = serde_json::json!({"command": "git"});
        let subj = approval_subject("bash", &input);
        assert_eq!(
            rule_for_choice(&PromptChoice::Prefix, &subj)
                .unwrap()
                .pattern,
            "git"
        );
        assert_eq!(
            rule_for_choice(&PromptChoice::GrantRoot, &subj)
                .unwrap()
                .pattern,
            "git"
        );
        assert_eq!(
            rule_for_choice(&PromptChoice::Domain, &subj)
                .unwrap()
                .pattern,
            "git"
        );
    }

    #[test]
    fn path_tool_subject_offers_domain_scope() {
        // write to `src/main.rs` (no base) → value=src/main.rs, domain=`src *`.
        let input = serde_json::json!({"path": "src/main.rs", "content": "x"});
        let subj = approval_subject("write", &input);
        assert_eq!(subj.suggested_rule.pattern, "src/main.rs");
        assert!(!subj.suggested_rule.wildcard, "default path rule is exact");
        let domain = subj
            .domain_rule
            .expect("domain_rule present for a path with a parent dir");
        assert!(domain.wildcard);
        assert_eq!(domain.pattern, "src *");
        // No prefix/root tiers for path tools.
        assert!(subj.prefix_rule.is_none());
        assert!(subj.root_rule.is_none());
    }

    #[test]
    fn path_tool_bare_filename_has_no_domain_scope() {
        // A bare filename (no parent dir) offers no domain tier.
        let input = serde_json::json!({"path": "README.md", "content": "x"});
        let subj = approval_subject("write", &input);
        assert_eq!(subj.suggested_rule.pattern, "README.md");
        assert!(subj.domain_rule.is_none(), "bare filename has no parent");
    }

    #[test]
    fn approval_tool_records_root_rule_on_grant_root_choice() {
        // Choosing GrantRoot on `npm run build` records `npm *` (the binary
        // tier), NOT the default `npm run *` prefix. This exercises the same
        // `rule_for_choice` + `record_always` glue the live `execute` arm runs
        // (the arm is trivial glue; the rule resolution is the logic under
        // test).
        let state = Arc::new(ApprovalState::new());
        let bash_input = serde_json::json!({"command": "npm run build"});
        let subj = approval_subject("bash", &bash_input);
        let root_rule = rule_for_choice(&PromptChoice::GrantRoot, &subj)
            .unwrap()
            .clone();
        state.record_always(root_rule);
        // `npm install` IS now covered by the root rule (the whole binary).
        assert!(
            state.is_pre_allowed("bash", "npm install"),
            "GrantRoot's npm * covers npm install"
        );
        assert!(
            state.is_pre_allowed("bash", "npm run build"),
            "GrantRoot's npm * covers npm run build"
        );
    }

    #[test]
    fn approval_tool_records_prefix_rule_on_prefix_choice() {
        // Choosing Prefix records the prefix tier; bare `npm install` is NOT
        // covered (that's the tighter-than-root point of the prefix scope).
        let state = Arc::new(ApprovalState::new());
        let bash_input = serde_json::json!({"command": "npm run build"});
        let subj = approval_subject("bash", &bash_input);
        let prefix_rule = rule_for_choice(&PromptChoice::Prefix, &subj)
            .unwrap()
            .clone();
        state.record_always(prefix_rule);
        assert!(
            state.is_pre_allowed("bash", "npm run build"),
            "Prefix covers npm run build"
        );
        assert!(
            !state.is_pre_allowed("bash", "npm install"),
            "Prefix does NOT cover npm install (use GrantRoot)"
        );
    }

    #[test]
    fn approval_tool_execute_grant_root_records_root_rule_end_to_end() {
        // End-to-end through ApprovalTool::execute: a UI that returns
        // GrantRoot must record a rule, proven by a SECOND call with the
        // same tool being pre-allowed WITHOUT consulting the UI. The
        // second call uses a CountingUi (Deny) — if the rule was recorded,
        // the second call short-circuits at is_pre_allowed and the UI is
        // never asked.
        //
        // GrantRoot on a write tool has no root candidate → falls back to
        // the suggested (exact-path) rule, so the second call to the SAME
        // absolute path is pre-allowed. Uses absolute paths so we don't pin
        // the process cwd (avoids racing the cwd-pinning journal test).
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a.txt");
        let target_str = target.to_string_lossy().to_string();

        struct GrantRootUi;
        #[async_trait]
        impl ApprovalUi for GrantRootUi {
            async fn decide(&self, _: &str, _: &str, _: &str) -> PromptChoice {
                PromptChoice::GrantRoot
            }
        }
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(ApprovalState::new());
        // First tool uses GrantRootUi → records a rule (exact path fallback).
        let tool = ApprovalTool::new(
            Box::new(WriteFileTool),
            Arc::clone(&state),
            ModeFlag::new(Mode::Normal),
            Arc::new(GrantRootUi),
        )
        .with_base_dir(Some(temp.path().to_path_buf()));
        let _ = futures::executor::block_on(tool.execute(
            "call-1",
            serde_json::json!({"path": target_str, "content": "x"}),
            None,
        ));

        // Second tool uses a Deny-counting UI; the same path is now
        // pre-allowed, so the UI is NOT consulted.
        let tool2 = ApprovalTool::new(
            Box::new(WriteFileTool),
            Arc::clone(&state),
            ModeFlag::new(Mode::Normal),
            Arc::new(CountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_base_dir(Some(temp.path().to_path_buf()));
        let _ = futures::executor::block_on(tool2.execute(
            "call-2",
            serde_json::json!({"path": target_str, "content": "y"}),
            None,
        ));
        assert_eq!(
            ui_calls.load(Ordering::Relaxed),
            0,
            "second call pre-allowed by the recorded rule; UI not consulted"
        );
    }

    #[test]
    fn bash_empty_command_always_allow_is_not_a_blanket_bypass() {
        // (Round-9) A genuinely-empty bash command (`{"command": ""}`) has no
        // binary to key on, so "always allow" falls to the no-real-binary
        // arm. The recorded rule's pattern MUST be non-empty: an empty
        // pattern means `AllowRule::matches` returns true for ANY bash
        // command (the `self.pattern.is_empty() -> true` path reserved for
        // `AllowRule::tool_all`), which would silently grant a blanket bash
        // bypass. The fix records a sentinel (`<no command>`) that no real
        // command matches. The matched VALUE stays the real (empty) command.
        let state = ApprovalState::new();
        let input = serde_json::json!({"command": ""});
        let subj = approval_subject("bash", &input);
        assert_eq!(subj.value, "", "value is the real (empty) command");
        assert!(
            !subj.suggested_rule.wildcard,
            "empty-command rule is exact (a sentinel), not a wildcard"
        );
        assert!(
            !subj.suggested_rule.pattern.is_empty(),
            "empty-command rule pattern must NOT be empty (would match-all)"
        );
        state.record_always(subj.suggested_rule);
        // The recorded rule must NOT pre-allow any real bash command.
        assert!(
            !state.is_pre_allowed("bash", "rm -rf /"),
            "empty-command allow must not bypass a real command"
        );
        assert!(
            !state.is_pre_allowed("bash", "echo hi"),
            "empty-command allow must not bypass echo either"
        );
        assert!(
            !state.is_pre_allowed("bash", ""),
            "empty-command allow does not even match the empty command again"
        );
    }

    #[test]
    fn bash_missing_command_always_allow_is_not_a_blanket_bypass() {
        // (Round-9) A MISSING command field falls back to the `<missing
        // command>` placeholder (non-empty), so its rule is already safe —
        // but pin it so a future refactor of the placeholder doesn't reopen
        // the empty-pattern hole. The rule must not match-all.
        let state = ApprovalState::new();
        let input = serde_json::json!({});
        let subj = approval_subject("bash", &input);
        assert!(
            !subj.suggested_rule.pattern.is_empty(),
            "missing-command rule pattern must not be empty"
        );
        state.record_always(subj.suggested_rule);
        assert!(
            !state.is_pre_allowed("bash", "rm -rf /"),
            "missing-command allow must not bypass a real command"
        );
    }

    #[test]
    fn path_edit_empty_path_always_allow_is_not_a_blanket_bypass() {
        // (Round-10) The round-9 audit found the empty-pattern match-ALL
        // false-allow left OPEN for the path-edit sibling arms: a model sending
        // `{"path": ""}` (explicit empty string, NOT a missing field) resolved
        // to `s = ""` (absolutize_for_match early-returns ""), which recorded
        // `AllowRule::exact(tool, "")` — and `AllowRule::matches` treats an
        // empty pattern as match-ALL (the `self.pattern.is_empty() -> true`
        // path reserved for `AllowRule::tool_all`). A single "always allow"
        // on a malformed empty-path write/edit/notebook would silently
        // pre-approve every future call of that tool against ANY path
        // (e.g. /etc/passwd, ~/.ssh/id_rsa). The fix records a non-empty
        // sentinel (`<missing path>`) when the resolved path is empty, so the
        // rule can never match-all. The matched VALUE stays the real (empty)
        // path. Covers write/edit/hashline_edit/notebook_edit/notebook_execute.
        let state = ApprovalState::new();
        for tool in [
            "write",
            "edit",
            "hashline_edit",
            "notebook_edit",
            "notebook_execute",
        ] {
            let input = serde_json::json!({"path": ""});
            let subj = approval_subject(tool, &input);
            assert_eq!(subj.value, "", "{tool}: value is the real (empty) path");
            assert!(
                !subj.suggested_rule.pattern.is_empty(),
                "{tool}: empty-path rule pattern must NOT be empty (would match-all)"
            );
            assert!(
                !subj.suggested_rule.wildcard,
                "{tool}: empty-path rule is exact (a sentinel), not a wildcard"
            );
            assert_eq!(
                subj.suggested_rule.pattern, "<missing path>",
                "{tool}: empty-path rule uses the sentinel pattern"
            );
            state.record_always(subj.suggested_rule.clone());
            // The recorded rule must NOT pre-allow any real path.
            assert!(
                !state.is_pre_allowed(tool, "/etc/passwd"),
                "{tool}: empty-path allow must not bypass a real path"
            );
            assert!(
                !state.is_pre_allowed(tool, "/home/jon/.ssh/id_rsa"),
                "{tool}: empty-path allow must not bypass the ssh key either"
            );
            assert!(
                !state.is_pre_allowed(tool, ""),
                "{tool}: empty-path allow does not even match the empty path again"
            );
            state.forget(); // reset between tools so they don't accumulate
        }
    }

    #[test]
    fn path_edit_missing_path_uses_safe_sentinel() {
        // (Round-10) A MISSING path field already fell back to the `<missing
        // path>` placeholder (non-empty, safe). Pin it so a future refactor of
        // the placeholder / the path-edit arm doesn't reopen the empty-pattern
        // hole (mirrors the bash missing-command pin).
        let state = ApprovalState::new();
        let input = serde_json::json!({});
        let subj = approval_subject("write", &input);
        assert_eq!(subj.value, "<missing path>");
        assert!(
            !subj.suggested_rule.pattern.is_empty(),
            "missing-path rule pattern must not be empty"
        );
        state.record_always(subj.suggested_rule);
        assert!(
            !state.is_pre_allowed("write", "/etc/passwd"),
            "missing-path allow must not bypass a real path"
        );
    }

    #[test]
    fn subject_write_extracts_path() {
        let input = serde_json::json!({"path": "src/main.rs", "content": "..."});
        let subj = approval_subject("write", &input);
        assert_eq!(subj.value, "src/main.rs");
        assert_eq!(subj.suggested_rule.tool, "write");
        assert_eq!(subj.suggested_label, "write(src/main.rs)");
    }

    #[test]
    fn subject_edit_extracts_path() {
        let input =
            serde_json::json!({"path": "src/lib.rs", "old_string": "foo", "new_string": "bar"});
        let subj = approval_subject("edit", &input);
        assert_eq!(subj.value, "src/lib.rs");
        assert_eq!(subj.suggested_rule.tool, "edit");
    }

    #[test]
    fn subject_missing_command_uses_placeholder() {
        let input = serde_json::json!({});
        let subj = approval_subject("bash", &input);
        assert_eq!(subj.value, "<missing command>");
        assert_eq!(subj.suggested_label, "bash(<missing command>)");
    }

    #[test]
    fn subject_background_bash_companions_extract_primary_args() {
        let output = approval_subject(
            "bash_output",
            &serde_json::json!({"log_path":"/tmp/pi-bash-bg-123.log","pid":1234}),
        );
        assert_eq!(output.value, "/tmp/pi-bash-bg-123.log pid 1234");
        assert_eq!(output.suggested_rule.tool, "bash_output");
        assert_eq!(
            output.suggested_label,
            "bash_output(/tmp/pi-bash-bg-123.log pid 1234)"
        );

        let kill = approval_subject("kill_bash", &serde_json::json!({"pid":1234}));
        assert_eq!(kill.value, "1234");
        assert_eq!(kill.suggested_rule.tool, "kill_bash");
        assert_eq!(kill.suggested_label, "kill_bash(1234)");
    }

    #[test]
    fn subject_unknown_tool_uses_tool_name() {
        let input = serde_json::json!({"url": "https://example.com"});
        let subj = approval_subject("WebFetch", &input);
        let raw = input.to_string();
        assert_eq!(subj.value, raw);
        assert_eq!(subj.suggested_rule.pattern, raw);
        assert_eq!(subj.suggested_label, format!("WebFetch({})", input));
    }

    // ── ApprovalState ───────────────────────────────────────────────

    #[test]
    fn auto_allow_read_only_tools() {
        let state = ApprovalState::new();
        assert!(state.is_pre_allowed("read", ""));
        assert!(state.is_pre_allowed("grep", ""));
        assert!(state.is_pre_allowed("find", ""));
        assert!(state.is_pre_allowed("ls", ""));
        assert!(state.is_pre_allowed("bash_output", ""));
        assert!(!state.is_pre_allowed("bash", "anything"));
        assert!(!state.is_pre_allowed("kill_bash", "1234"));
    }

    #[test]
    fn record_always_bash_exact() {
        let state = ApprovalState::new();
        let rule = AllowRule::exact("bash", "npm run build");
        state.record_always(rule);
        assert!(state.is_pre_allowed("bash", "npm run build"));
        assert!(!state.is_pre_allowed("bash", "npm run test"));
    }

    #[test]
    fn forget_clears_rules() {
        let state = ApprovalState::new();
        let rule = AllowRule::exact("bash", "echo hi");
        state.record_always(rule);
        assert!(state.is_pre_allowed("bash", "echo hi"));
        state.forget();
        assert!(!state.is_pre_allowed("bash", "echo hi"));
    }

    #[test]
    fn relative_path_subject_matches_directory_trust_rule() {
        // The exact bug from the field: user clicks "trust this
        // directory" (rule = /project/**), the model then calls write
        // with a RELATIVE path — without absolutization the rule never
        // matches and the prompt keeps coming back.
        let state = ApprovalState::new();
        state.record_always(AllowRule::wildcard(
            "write",
            "/home/jon/repos/experiments/**",
        ));
        let base = Path::new("/home/jon/repos/experiments");
        let input = serde_json::json!({ "path": "bicycle-race/src/imports.ts" });
        let subject = approval_subject_with_base("write", &input, Some(base));
        assert_eq!(
            subject.value,
            "/home/jon/repos/experiments/bicycle-race/src/imports.ts"
        );
        assert!(state.is_pre_allowed("write", &subject.value));
        // The suggested exact rule records the absolute form too.
        assert_eq!(subject.suggested_rule.pattern, subject.value);
        // Traversal does not escape lexically: ../../etc resolves.
        let sneaky = serde_json::json!({ "path": "../outside.txt" });
        let s2 = approval_subject_with_base("write", &sneaky, Some(base));
        assert_eq!(s2.value, "/home/jon/repos/outside.txt");
        assert!(!state.is_pre_allowed("write", &s2.value));
        // Absolute paths pass through untouched; no base = no change.
        let abs = serde_json::json!({ "path": "/tmp/x.rs" });
        assert_eq!(
            approval_subject_with_base("write", &abs, Some(base)).value,
            "/tmp/x.rs"
        );
        assert_eq!(approval_subject("write", &sneaky).value, "../outside.txt");
    }

    #[test]
    fn session_rules_allow_but_never_persist() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("allow-rules.toml");
        let state = ApprovalState::with_persistent_store(path.clone()).unwrap();

        state.record_session(AllowRule::tool_all("bash"));
        assert!(state.is_pre_allowed("bash", "literally anything"));

        // A persisted write (triggered by a regular always rule) must
        // not carry the session rule onto disk.
        state.record_always(AllowRule::exact("edit", "/tmp/a.rs"));
        let reloaded = ApprovalState::with_persistent_store(path).unwrap();
        assert!(reloaded.is_pre_allowed("edit", "/tmp/a.rs"));
        assert!(!reloaded.is_pre_allowed("bash", "literally anything"));

        // forget() clears session rules too.
        state.forget();
        assert!(!state.is_pre_allowed("bash", "literally anything"));
    }

    #[test]
    fn record_always_merges_with_disk_instead_of_clobbering() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("allow-rules.toml");
        // Two live sessions share the same store file.
        let a = ApprovalState::with_persistent_store(path.clone()).unwrap();
        let b = ApprovalState::with_persistent_store(path.clone()).unwrap();

        a.record_always(AllowRule::exact("bash", "cargo test"));
        // b loaded BEFORE a's rule existed; recording from b used to
        // overwrite the file with b's list only, losing a's rule.
        b.record_always(AllowRule::exact("bash", "npm run dev"));

        let reloaded = ApprovalState::with_persistent_store(path).unwrap();
        assert!(
            reloaded.is_pre_allowed("bash", "cargo test"),
            "a's rule survived"
        );
        assert!(
            reloaded.is_pre_allowed("bash", "npm run dev"),
            "b's rule survived"
        );
    }

    #[test]
    fn persistent_store_roundtrips_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("allow-rules.toml");
        let state = ApprovalState::with_persistent_store(path.clone()).unwrap();

        state.record_always(AllowRule::exact("bash", "cargo test"));
        state.record_always(AllowRule::wildcard("edit", "src/*.rs"));

        let reloaded = ApprovalState::with_persistent_store(path).unwrap();
        assert!(reloaded.is_pre_allowed("bash", "cargo test"));
        assert!(reloaded.is_pre_allowed("edit", "src/lib.rs"));
        assert!(!reloaded.is_pre_allowed("bash", "cargo check"));
    }

    #[test]
    fn persistent_store_forget_clears_disk_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("allow-rules.toml");
        let state = ApprovalState::with_persistent_store(path.clone()).unwrap();

        state.record_always(AllowRule::exact("bash", "echo hi"));
        assert!(state.is_pre_allowed("bash", "echo hi"));
        state.forget();

        let reloaded = ApprovalState::with_persistent_store(path).unwrap();
        assert!(!reloaded.is_pre_allowed("bash", "echo hi"));
    }

    #[test]
    fn persistent_store_lists_and_removes_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("allow-rules.toml");
        let state = ApprovalState::with_persistent_store(path.clone()).unwrap();

        state.record_always(AllowRule::exact("bash", "cargo test"));
        state.record_always(AllowRule::wildcard("edit", "src/*.rs"));

        let rules = state.always_rules();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], AllowRule::exact("bash", "cargo test"));
        assert!(state.remove_always(0));
        assert!(!state.remove_always(10));

        let reloaded = ApprovalState::with_persistent_store(path).unwrap();
        assert!(!reloaded.is_pre_allowed("bash", "cargo test"));
        assert!(reloaded.is_pre_allowed("edit", "src/lib.rs"));
        assert_eq!(
            reloaded.always_rules(),
            vec![AllowRule::wildcard("edit", "src/*.rs")]
        );
    }

    #[test]
    fn persistent_store_ignores_session_scoped_disk_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("allow-rules.toml");
        std::fs::write(
            &path,
            r#"
[[rules]]
tool = "bash"
pattern = "echo persisted"
scope = "always"

[[rules]]
tool = "bash"
pattern = "echo session"
scope = "session"
"#,
        )
        .unwrap();

        let state = ApprovalState::with_persistent_store(path).unwrap();
        assert!(state.is_pre_allowed("bash", "echo persisted"));
        assert!(!state.is_pre_allowed("bash", "echo session"));
    }

    #[test]
    fn record_whole_tool_rule() {
        let state = ApprovalState::new();
        let rule = AllowRule::tool_all("edit");
        state.record_always(rule);
        assert!(state.is_pre_allowed("edit", "anything"));
        assert!(!state.is_pre_allowed("write", "anything"));
    }

    #[test]
    fn always_allow_persists_for_session() {
        // Verifies that recording a rule means subsequent is_pre_allowed
        // checks pass without consulting the UI.
        let state = ApprovalState::new();
        let rule = AllowRule::exact("bash", "echo hi");
        state.record_always(rule);

        assert!(state.is_pre_allowed("bash", "echo hi"));
        assert!(!state.is_pre_allowed("bash", "echo bye"));
    }

    // ── Mode::Bypass (M4/#5) ─────────────────────────────────────────

    #[test]
    fn mode_bypass_survives_modeflag_round_trip() {
        // `ModeFlag` stores the mode as a u8 (via the private as_u8/from_u8)
        // and is the cloneable handle shared across tools + subagents, so a
        // round-trip through it is the real "does the u8 encoding keep
        // Bypass" test the runtime cares about.
        let flag = ModeFlag::new(Mode::Bypass);
        assert_eq!(flag.get(), Mode::Bypass);
        // A cloned flag (as subagents receive) keeps Bypass too.
        assert_eq!(flag.clone().get(), Mode::Bypass);
    }

    #[test]
    fn bypass_auto_allows_mutating_tool_without_consulting_ui() {
        // A UI that Denies everything and counts calls. Bypass must run
        // the mutating tool AND never ask the UI — proving the short-
        // circuit fires before `ui.decide`. Mirrors Codex's
        // `AskForApproval::Never`.
        //
        // We deliberately do NOT pin the process cwd (unlike the journal
        // test below): `WriteFileTool` writes a relative `path`, so we pass
        // an ABSOLUTE path instead. That keeps this test cwd-race-free with
        // `execute_inner_journals_successful_edit_and_skips_failed`, which
        // owns the process cwd for the duration of its run.
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("out.txt");
        let target_str = target.to_string_lossy().to_string();
        let ui_calls = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalTool::new(
            Box::new(WriteFileTool),
            Arc::new(ApprovalState::new()),
            ModeFlag::new(Mode::Bypass),
            Arc::new(CountingUi {
                calls: Arc::clone(&ui_calls),
            }),
        )
        .with_base_dir(Some(temp.path().to_path_buf()));

        let execution = futures::executor::block_on(tool.execute(
            "call-bypass",
            serde_json::json!({"path": target_str, "content":"bypassed\n"}),
            None,
        ))
        .unwrap();
        assert!(matches!(execution, ToolExecution::Done(_)));
        // The mutating tool actually ran.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "bypassed\n");
        // The UI was NEVER consulted.
        assert_eq!(ui_calls.load(Ordering::Relaxed), 0);
    }
}
