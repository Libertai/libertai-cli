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
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

const NAME: &str = "todo";
const LABEL: &str = "Todo";
const DESCRIPTION: &str = concat!(
    "Maintain a visible task list for the user. Call this whenever you ",
    "start planning multi-step work or when a step's status changes: ",
    "mark items as pending, active (in progress), or completed. The UI ",
    "re-renders the whole list on each call. Keep item text short — ",
    "one short sentence each. Keep exactly one item active (in progress) ",
    "at a time — set the previous one to completed when you move to the ",
    "next, so the list always reflects what you're doing right now."
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
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: TodoInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!("invalid `todo` payload: {e}")));
            }
        };
        // Collapse duplicate items before anything renders or counts:
        // models occasionally send the same item twice in one call
        // (e.g. two quick consecutive updates merged into one), which
        // used to draw the same line twice inside a single task list.
        let items = display_items(&parsed.items);

        // Route the task list to the TUI via the tool-update channel so
        // ratatui's main thread renders the pinned overlay in place —
        // NEVER raw stderr. The TUI owns the alternate screen on stdout;
        // an `eprintln!` from this background-thread tool writes raw ANSI
        // escapes to the live terminal mid-`terminal.draw`, corrupting the
        // frame (the "todo breaks the UI" symptom). `on_update` is `Some`
        // whenever the session has an event sink (the TUI); the dispatcher
        // in `app.rs` turns `details.kind == "todo"` into `AgentMsg::Todo`.
        if let Some(on_update) = &on_update {
            on_update(ToolUpdate {
                content: Vec::new(),
                details: Some(serde_json::json!({
                    "kind": "todo",
                    // `TodoItem` derives Serialize — round-trips back to
                    // `Vec<TodoItem>` in the TUI dispatcher.
                    "items": items,
                })),
            });
        } else {
            // Headless (`--print` / one-shot / no event sink): stderr is a
            // legit output stream here, so the legacy dim render is safe
            // and preserves the existing print-mode behavior.
            render_todo_list(&items);
        }

        // Give the model a cheap confirmation it can see in the tool
        // result message (models sometimes spin if a tool returns an
        // empty string).
        let summary = summarize(&items);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(summary))],
            details: None,
            is_error: false,
        }
        .into())
    }

    fn is_read_only(&self) -> bool {
        // No filesystem / network writes; safe to mark as read-only so
        // pi's parallelism allowances are preserved.
        true
    }
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
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

/// Items as displayed: duplicates (same whitespace-trimmed text)
/// collapse to one entry in first-appearance order, carrying the *last*
/// status seen for that text — when two consecutive updates get merged
/// into one `items` array, the later status is the current one.
fn display_items(items: &[TodoItem]) -> Vec<TodoItem> {
    let mut out: Vec<TodoItem> = Vec::with_capacity(items.len());
    for item in items {
        let key = item.text.trim();
        if let Some(existing) = out.iter_mut().find(|seen| seen.text.trim() == key) {
            existing.status = item.status;
        } else {
            out.push(item.clone());
        }
    }
    out
}

/// One pre-styled stderr line per item. Pure so the duplicate-collapse
/// and glyph/style mapping are unit-testable without capturing stderr.
fn todo_lines(items: &[TodoItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            let (glyph, colour) = match item.status {
                TodoStatus::Completed => ("\u{2611}", "\x1b[32m"), // ☑ green
                TodoStatus::Active => ("\u{25a0}", "\x1b[33;1m"),  // ■ bold amber
                TodoStatus::Pending => ("\u{2610}", "\x1b[2m"),    // ☐ dim
            };
            format!(
                "  {colour}{glyph}\x1b[0m {}{}\x1b[0m",
                match item.status {
                    TodoStatus::Active => "\x1b[1m",
                    TodoStatus::Completed => "\x1b[2m",
                    TodoStatus::Pending => "",
                },
                item.text
            )
        })
        .collect()
}

/// Render the task list to stderr. Called from inside `execute`, which
/// pi awaits, so this happens synchronously between streaming deltas.
/// Each call prints a complete fresh copy (callers pass already-deduped
/// items via [`display_items`]); the most recent copy is the
/// bottom-most one, right above the input bar.
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
    for line in todo_lines(items) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            text: text.to_string(),
            status,
        }
    }

    #[test]
    fn duplicate_item_text_renders_once_with_latest_status() {
        // Two consecutive updates merged into one items array: the same
        // task appears twice — once active, once completed. The display
        // must show one line carrying the most recent status.
        let merged = [
            item(
                "Fix peer discovery: add room-scoped broadcast so peers find each other",
                TodoStatus::Active,
            ),
            item(
                "Fix peer discovery: add room-scoped broadcast so peers find each other",
                TodoStatus::Completed,
            ),
            item("Verify fix works in two tabs", TodoStatus::Pending),
        ];
        let display = display_items(&merged);
        assert_eq!(display.len(), 2);
        assert_eq!(display[0].status, TodoStatus::Completed);
        assert_eq!(display[1].status, TodoStatus::Pending);

        let lines = todo_lines(&display);
        assert_eq!(lines.len(), 2);
        let dup_count = lines
            .iter()
            .filter(|l| l.contains("Fix peer discovery"))
            .count();
        assert_eq!(dup_count, 1, "duplicate task rendered twice: {lines:?}");
    }

    #[test]
    fn verbatim_duplicates_with_same_status_collapse() {
        // The exact session symptom: the same active line twice.
        let items = [
            item("Fix peer discovery", TodoStatus::Active),
            item("Fix peer discovery", TodoStatus::Active),
            item("Verify fix works in two tabs", TodoStatus::Pending),
        ];
        let lines = todo_lines(&display_items(&items));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn two_sequential_states_each_render_complete_and_deduped() {
        // Two separate todo calls (state A then state B) are two
        // independent renders; neither leaks into or dedupes against
        // the other, and each is internally duplicate-free.
        let state_a = [
            item("Fix peer discovery", TodoStatus::Active),
            item("Verify fix works in two tabs", TodoStatus::Pending),
        ];
        let state_b = [
            item("Fix peer discovery", TodoStatus::Completed),
            item("Verify fix works in two tabs", TodoStatus::Active),
        ];
        let lines_a = todo_lines(&display_items(&state_a));
        let lines_b = todo_lines(&display_items(&state_b));
        assert_eq!(lines_a.len(), 2);
        assert_eq!(lines_b.len(), 2);
        // Status progression is visible across the two renders.
        assert!(lines_a[0].contains("\u{25a0}"), "A: item 1 active");
        assert!(lines_b[0].contains("\u{2611}"), "B: item 1 completed");
        assert!(lines_b[1].contains("\u{25a0}"), "B: item 2 active");
    }

    #[test]
    fn distinct_items_pass_through_untouched() {
        let items = [
            item("one", TodoStatus::Completed),
            item("two", TodoStatus::Active),
            item("three", TodoStatus::Pending),
        ];
        let display = display_items(&items);
        assert_eq!(display.len(), 3);
        assert_eq!(todo_lines(&display).len(), 3);
        // Whitespace-only variations of the same text still collapse.
        let padded = [
            item("one", TodoStatus::Active),
            item("  one  ", TodoStatus::Completed),
        ];
        let display = display_items(&padded);
        assert_eq!(display.len(), 1);
        assert_eq!(display[0].status, TodoStatus::Completed);
    }

    #[test]
    fn summary_counts_the_deduped_list() {
        let merged = [
            item("a", TodoStatus::Active),
            item("a", TodoStatus::Completed),
            item("b", TodoStatus::Pending),
        ];
        assert_eq!(
            summarize(&display_items(&merged)),
            "todo: 1/2 complete, 0 active"
        );
    }
}
