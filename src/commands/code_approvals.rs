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
#[async_trait]
pub trait ApprovalUi: Send + Sync {
    async fn decide(&self, tool_name: &str, preview: &str) -> PromptChoice;
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
///   the rest of the session.
pub struct ApprovalState {
    always_allow: Mutex<HashSet<String>>,
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
            always_allow: Mutex::new(HashSet::new()),
            auto_allow: ["read", "grep", "find", "ls"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }

    /// True when the tool is on either allowlist and should run without prompting.
    pub fn is_pre_allowed(&self, tool_name: &str) -> bool {
        if self.auto_allow.contains(tool_name) {
            return true;
        }
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(tool_name)
    }

    /// Record that the user picked "always allow" for this tool name.
    pub fn record_always(&self, tool_name: String) {
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tool_name);
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
        if self.state.is_pre_allowed(name) {
            return self.inner.execute(tool_call_id, input, on_update).await;
        }

        let preview = preview_call(name, &input);
        match self.ui.decide(name, &preview).await {
            PromptChoice::Allow => self.inner.execute(tool_call_id, input, on_update).await,
            PromptChoice::AlwaysAllow => {
                self.state.record_always(name.to_string());
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
    fn sanitize_handles_single_char_escape() {
        // ESC c (non-CSI) is a full reset. We drop ESC + the next char.
        assert_eq!(sanitize("before\x1bcafter"), "beforeafter");
    }

    /// `MockApprovalUi` replays a fixed script of choices, then panics if
    /// the agent asks for more — handy for asserting that an "always
    /// allow" decision actually short-circuits subsequent calls.
    struct MockApprovalUi {
        script: Mutex<std::collections::VecDeque<PromptChoice>>,
    }
    impl MockApprovalUi {
        fn new(script: Vec<PromptChoice>) -> Self {
            Self {
                script: Mutex::new(script.into()),
            }
        }
    }
    #[async_trait]
    impl ApprovalUi for MockApprovalUi {
        async fn decide(&self, _tool_name: &str, _preview: &str) -> PromptChoice {
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockApprovalUi script exhausted")
        }
    }

    #[test]
    fn pre_allowed_skips_ui() {
        let state = ApprovalState::new();
        // `read` is in auto_allow.
        assert!(state.is_pre_allowed("read"));
        // `bash` is not — until promoted.
        assert!(!state.is_pre_allowed("bash"));
        state.record_always("bash".to_string());
        assert!(state.is_pre_allowed("bash"));
        state.forget();
        assert!(!state.is_pre_allowed("bash"));
    }

    #[tokio::test]
    async fn always_allow_persists_for_session() {
        // Asserts that the second `bash` invocation does not consult
        // the UI — the script only has one entry and the test would
        // panic on a second decide() call.
        let state = Arc::new(ApprovalState::new());
        let ui: Arc<dyn ApprovalUi> = Arc::new(MockApprovalUi::new(vec![PromptChoice::AlwaysAllow]));

        // First call: ui returns AlwaysAllow → recorded.
        let preview = "echo hi";
        let choice = ui.decide("bash", preview).await;
        assert_eq!(choice, PromptChoice::AlwaysAllow);
        state.record_always("bash".to_string());

        // Second call: pre-allowed, no ui consult.
        assert!(state.is_pre_allowed("bash"));
    }
}
