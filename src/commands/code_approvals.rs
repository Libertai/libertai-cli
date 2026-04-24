//! Tool-call approval layer.
//!
//! Wraps every mutating built-in tool (`bash`, `edit`, `write`,
//! `hashline_edit`) in an [`ApprovalTool`] that pauses the agent stream,
//! renders a preview of what's about to run, and waits for a single-key
//! decision: allow once, always allow (session-scoped), or deny (with
//! optional reason fed back to the agent so it can course-correct).
//!
//! Read-only tools (`read`, `grep`, `find`, `ls`) are auto-allowed — the
//! approval UI for them would be pure noise.
//!
//! `ApprovalState` is session-scoped: no on-disk persistence. A new
//! `libertai code` launch starts with an empty allowlist.

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

/// Session-scoped approval memory. Resets on every launch.
pub struct ApprovalState {
    /// Tool names promoted to "always allow" via the micro-menu.
    always_allow: Mutex<HashSet<String>>,
    /// Tool names that never need to prompt (read-only built-ins).
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

    /// Drop every "always allow" entry collected this session.
    /// Invoked by the `/forget` slash command in the REPL.
    pub fn forget(&self) {
        self.always_allow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn decide(&self, tool_name: &str, preview: &str) -> Decision {
        if self.auto_allow.contains(tool_name) {
            return Decision::Allow;
        }
        {
            let guard = self
                .always_allow
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.contains(tool_name) {
                return Decision::Allow;
            }
        }
        match prompt(tool_name, preview) {
            PromptChoice::Allow => Decision::Allow,
            PromptChoice::AlwaysAllow => {
                self.always_allow
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(tool_name.to_string());
                Decision::Allow
            }
            PromptChoice::Deny => Decision::Deny(None),
        }
    }
}

enum Decision {
    Allow,
    Deny(Option<String>),
}

enum PromptChoice {
    Allow,
    AlwaysAllow,
    Deny,
}

/// Wraps any `pi::sdk::Tool` with the approval gate defined above.
pub struct ApprovalTool {
    inner: Box<dyn Tool>,
    state: Arc<ApprovalState>,
}

impl ApprovalTool {
    pub fn new(inner: Box<dyn Tool>, state: Arc<ApprovalState>) -> Self {
        Self { inner, state }
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
        let preview = preview_call(self.inner.name(), &input);
        match self.state.decide(self.inner.name(), &preview) {
            Decision::Allow => self.inner.execute(tool_call_id, input, on_update).await,
            Decision::Deny(reason) => Ok(denial_output(reason)),
        }
    }
}

fn denial_output(reason: Option<String>) -> ToolOutput {
    let text = reason.unwrap_or_else(|| {
        "user denied execution of this tool call; ask them for alternative approaches or a different strategy".into()
    });
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
}

/// Render a one-line preview for the approval menu.
fn preview_call(tool: &str, input: &serde_json::Value) -> String {
    match tool {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map_or_else(|| "<missing command>".to_string(), str::to_string),
        "write" => {
            let path = field(input, "path").unwrap_or("<missing path>");
            let len = input
                .get("content")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            format!("write {path} ({len} bytes)")
        }
        "edit" => {
            let path = field(input, "path").unwrap_or("<missing path>");
            format!("edit {path}")
        }
        "hashline_edit" => {
            let path = field(input, "path").unwrap_or("<missing path>");
            format!("hashline_edit {path}")
        }
        _ => {
            let raw = input.to_string();
            let clipped: String = raw.chars().take(200).collect();
            format!("{tool}: {clipped}")
        }
    }
}

fn field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

/// Block until the user picks allow/always/deny. Uses crossterm raw mode
/// to read a single keystroke without requiring Enter.
///
/// This is called from inside an asupersync task (pi's Agent awaits
/// `execute` sequentially), so blocking on stdin is acceptable — nothing
/// else on the runtime is making progress while a tool call is in flight.
fn prompt(tool_name: &str, preview: &str) -> PromptChoice {
    let mut stderr = std::io::stderr();

    eprintln!();
    eprintln!("  \x1b[33;1m⎯ tool approval ⎯\x1b[0m");
    eprintln!("  \x1b[1m{tool_name}\x1b[0m");
    for line in preview.lines() {
        eprintln!("  \x1b[2m│\x1b[0m {line}");
    }
    eprint!("  \x1b[2m[a]\x1b[0m allow once  \x1b[2m[A]\x1b[0m always allow  \x1b[2m[d]\x1b[0m deny: ");
    let _ = stderr.flush();

    // Brief raw-mode single-key read. If raw mode isn't available (e.g.
    // non-TTY), fall back to reading a whole line and looking at the
    // first char.
    if terminal::enable_raw_mode().is_err() {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        eprintln!();
        return parse_cooked_choice(&line);
    }
    let choice = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent { code, modifiers, .. })) => match (code, modifiers) {
                (KeyCode::Char('a'), KeyModifiers::NONE)
                | (KeyCode::Char('a'), KeyModifiers::SHIFT) => break PromptChoice::Allow,
                (KeyCode::Char('A'), _) => break PromptChoice::AlwaysAllow,
                (KeyCode::Char('d'), _) | (KeyCode::Char('D'), _) => break PromptChoice::Deny,
                (KeyCode::Enter, _) => break PromptChoice::Allow,
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => break PromptChoice::Deny,
                (KeyCode::Esc, _) => break PromptChoice::Deny,
                _ => continue,
            },
            Ok(_) => continue,
            Err(_) => break PromptChoice::Deny,
        }
    };
    let _ = terminal::disable_raw_mode();
    // Echo the decision on its own line so subsequent streaming output
    // flows below, not on top of the prompt.
    let label = match choice {
        PromptChoice::Allow => "allowed",
        PromptChoice::AlwaysAllow => "always allowed",
        PromptChoice::Deny => "denied",
    };
    eprintln!("\x1b[2m{label}\x1b[0m");
    choice
}

fn parse_cooked_choice(line: &str) -> PromptChoice {
    match line.trim().chars().next().unwrap_or('d') {
        'a' => PromptChoice::Allow,
        'A' => PromptChoice::AlwaysAllow,
        _ => PromptChoice::Deny,
    }
}
