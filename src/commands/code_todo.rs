//! The `todo` tool — Claude-Code-style task-list overlay.
//!
//! When the agent plans multi-step work, it calls `todo(items=[...])`
//! and we render the current list inline (checkbox glyphs, the active
//! item highlighted). Each subsequent call updates the list in place
//! visually — the tool prints a fresh render so the most recent copy
//! is always the bottom-most one, right above the input bar.
//!
//! The tool itself is cheap: no filesystem side effects, so we skip the
//! approval gate. That's why the factory registers it alongside the
//! approval-wrapped built-ins rather than wrapping it.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

const NAME: &str = "todo";
const LABEL: &str = "Todo";
const DESCRIPTION: &str = concat!(
    "Maintain a visible task list for the user. Call this whenever you ",
    "start planning multi-step work or when a step's status changes: ",
    "mark items as pending, active (in progress), or completed. The UI ",
    "re-renders the whole list on each call. Keep item text short — ",
    "one short sentence each."
);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Pending,
    Active,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Deserialize)]
struct TodoInput {
    items: Vec<TodoItem>,
}

pub struct TodoTool;

impl TodoTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "One-line description of the task." },
                            "status": { "type": "string", "enum": ["pending", "active", "completed"] }
                        },
                        "required": ["text", "status"]
                    }
                }
            },
            "required": ["items"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolOutput> {
        let parsed: TodoInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!("invalid `todo` payload: {e}")));
            }
        };
        render_todo_list(&parsed.items);

        // Give the model a cheap confirmation it can see in the tool
        // result message (models sometimes spin if a tool returns an
        // empty string).
        let summary = summarize(&parsed.items);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(summary))],
            details: None,
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool {
        // No filesystem / network writes; safe to mark as read-only so
        // pi's parallelism allowances are preserved.
        true
    }
}

fn err_output(msg: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
}

fn summarize(items: &[TodoItem]) -> String {
    let total = items.len();
    let done = items
        .iter()
        .filter(|i| matches!(i.status, TodoStatus::Completed))
        .count();
    let active = items
        .iter()
        .filter(|i| matches!(i.status, TodoStatus::Active))
        .count();
    format!("todo: {done}/{total} complete, {active} active")
}

/// Render the task list to stderr. Called from inside `execute`, which
/// pi awaits, so this happens synchronously between streaming deltas.
///
/// Output shape (glyphs chosen to match the Claude Code screenshot):
///
/// ```text
///   ⎯ task list ⎯
///   ☒  rebuild the parser
///   ▪  wire the new event
///   ☐  bench the fallback
/// ```
fn render_todo_list(items: &[TodoItem]) {
    if items.is_empty() {
        eprintln!("  \x1b[2m(todo list cleared)\x1b[0m");
        return;
    }
    eprintln!();
    eprintln!("  \x1b[2m⎯ task list ⎯\x1b[0m");
    for item in items {
        let (glyph, colour) = match item.status {
            TodoStatus::Completed => ("\u{2611}", "\x1b[32m"),   // ☑ green
            TodoStatus::Active => ("\u{25a0}", "\x1b[33;1m"),    // ■ bold amber
            TodoStatus::Pending => ("\u{2610}", "\x1b[2m"),      // ☐ dim
        };
        eprintln!(
            "  {colour}{glyph}\x1b[0m {}{}\x1b[0m",
            match item.status {
                TodoStatus::Active => "\x1b[1m",
                TodoStatus::Completed => "\x1b[2m",
                TodoStatus::Pending => "",
            },
            item.text
        );
    }
}
