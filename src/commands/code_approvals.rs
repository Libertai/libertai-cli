//! Tool-call approval layer.
//!
//! Wraps every mutating built-in tool (`bash`, `edit`, `write`,
//! `hashline_edit`) in an [`ApprovalTool`] that pauses the agent stream,
//! renders a preview of what's about to run, and waits for a decision via
//! a pluggable [`ApprovalUi`]: allow once, always allow (session-scoped),
//! or deny (with optional reason fed back to the agent so it can
//! course-correct).
//!
//! Read-only tools (`read`, `grep`, `find`, `ls`) are auto-allowed — the
//! approval UI for them would be pure noise.
//!
//! [`ApprovalState`] is session-scoped: no on-disk persistence. A new
//! session starts with an empty allowlist. The UI is supplied separately
//! (see [`ApprovalUi`]) so the same approval-gating logic powers the
//! terminal CLI ([`TerminalApprovalUi`] in `code_term`) and the desktop
//! app (a callback-based UI implemented in the Tauri crate).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

use crate::commands::code_factory::{Mode, ModeFlag};

/// User decision for a single approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptChoice {
    /// Run this tool call once.
    Allow,
    /// Run this tool call and remember "always allow this tool" for the session.
    AlwaysAllow,
    /// Reject this tool call. The agent receives a denial output.
    Deny,
}

/// A single allow rule binding a tool to a command/path pattern.
///
/// Prompt-created rules are exact, even when the command/path contains `*`.
/// Explicit wildcard rules can opt into glob-lite `*` matching.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
}

/// Extract an [`ApprovalSubject`] from the raw JSON input of a tool call.
///
/// Reads raw JSON fields without sanitization or truncation so the
/// resulting rule matches exactly what the model produced. The caller
/// should sanitize separately for UI display (see [`preview_call`]).
pub fn approval_subject(tool: &str, input: &serde_json::Value) -> ApprovalSubject {
    let (value, rule, label) = match tool {
        "bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing command>");
            let s = cmd.to_string();
            (
                s.clone(),
                AllowRule::exact(tool, s.clone()),
                format!("bash({s})"),
            )
        }
        "write" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing path>");
            let s = path.to_string();
            (
                s.clone(),
                AllowRule::exact(tool, s.clone()),
                format!("write({s})"),
            )
        }
        "edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing path>");
            let s = path.to_string();
            (
                s.clone(),
                AllowRule::exact(tool, s.clone()),
                format!("edit({s})"),
            )
        }
        "hashline_edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing path>");
            let s = path.to_string();
            (
                s.clone(),
                AllowRule::exact(tool, s.clone()),
                format!("hashline_edit({s})"),
            )
        }
        // Unknown/future wrapped tools fall back to exact raw-JSON matching
        // instead of whole-tool approval.
        other => {
            let s = input.to_string();
            (
                s.clone(),
                AllowRule::exact(other, s.clone()),
                format!("{other}({s})"),
            )
        }
    };
    ApprovalSubject {
        value,
        suggested_rule: rule,
        suggested_label: label,
    }
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
    let mut pos = if parts[0].is_empty() { 0 } else { parts[0].len() };
    for i in 1..last {
        let seg = parts[i];
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
    async fn ask(&self, _payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "cancelled": true,
            "reason": "ASK_NOT_SUPPORTED",
        })
    }
}

/// Session-scoped approval memory. Resets on every launch.
///
/// Holds two allowlists:
/// - `auto_allow` (hardcoded, read-only built-ins): never prompt.
/// - `always_allow` (user-promoted via `AlwaysAllow`): never prompt for
///   the rest of the session, keyed by tool+pattern [`AllowRule`].
pub struct ApprovalState {
    always_allow: Mutex<Vec<AllowRule>>,
    auto_allow: HashSet<String>,
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
            auto_allow: ["read", "grep", "find", "ls"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }

    /// True when the tool+value pair is on either allowlist.
    ///
    /// `auto_allow` matches by tool name alone (read-only tools).
    /// `always_allow` checks each [`AllowRule`] against `(tool_name, value)`.
    pub fn is_pre_allowed(&self, tool_name: &str, value: &str) -> bool {
        if self.auto_allow.contains(tool_name) {
            return true;
        }
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|rule| rule.matches(tool_name, value))
    }

    /// Record a rule for the session. Deduplicates identical rules.
    pub fn record_always(&self, rule: AllowRule) {
        let mut list = self
            .always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !list.contains(&rule) {
            list.push(rule);
        }
    }

    /// Drop every "always allow" entry collected this session.
    /// Invoked by the `/forget` slash command in the REPL.
    pub fn forget(&self) {
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
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
    mode: ModeFlag,
    ui: Arc<dyn ApprovalUi>,
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
            mode,
            ui,
        }
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
    ) -> PiResult<ToolOutput> {
        // Plan mode short-circuit: mutating tools are auto-denied
        // without a prompt. The agent sees a tool error, learns the
        // tool isn't available right now, and adapts. Read-only tools
        // pass straight through.
        if matches!(self.mode.get(), Mode::Plan) && !self.inner.is_read_only() {
            return Ok(plan_denial_output(self.inner.name()));
        }

        let name = self.inner.name();
        let subject = approval_subject(name, &input);
        if self.state.is_pre_allowed(name, &subject.value) {
            return self.inner.execute(tool_call_id, input, on_update).await;
        }

        // Build sanitized display text from the *raw* input (not from
        // subject.value, which is unsanitized). preview_call handles
        // all sanitization and formatting.
        let preview = preview_call(name, &input);
        let always_label = sanitize_inline(&subject.suggested_label);
        match self.ui.decide(name, &preview, &always_label).await {
            PromptChoice::Allow => self.inner.execute(tool_call_id, input, on_update).await,
            PromptChoice::AlwaysAllow => {
                self.state.record_always(subject.suggested_rule);
                self.inner.execute(tool_call_id, input, on_update).await
            }
            PromptChoice::Deny => Ok(denial_output(None)),
        }
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
        "write" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            let len = input
                .get("content")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            format!("write {path} ({len} bytes)")
        }
        "edit" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            format!("edit {path}")
        }
        "hashline_edit" => {
            let path = sanitize(field(input, "path").unwrap_or("<missing path>"));
            format!("hashline_edit {path}")
        }
        _ => {
            let raw = input.to_string();
            let clipped = sanitize(&raw);
            format!("{tool}: {clipped}")
        }
    }
}

fn field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
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
                state = if c == '[' { State::InCsi } else { State::Normal };
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
        assert!(!wildcard_match("npm run build", "npm run build && rm -rf /"));
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

    // ── approval_subject ────────────────────────────────────────────

    #[test]
    fn subject_bash_extracts_command() {
        let input = serde_json::json!({"command": "npm run build"});
        let subj = approval_subject("bash", &input);
        assert_eq!(subj.value, "npm run build");
        assert_eq!(subj.suggested_rule.pattern, "npm run build");
        assert_eq!(subj.suggested_rule.tool, "bash");
        assert_eq!(subj.suggested_label, "bash(npm run build)");
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
        assert!(!state.is_pre_allowed("bash", "anything"));
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
}
