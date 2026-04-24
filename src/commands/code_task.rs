//! The `task` tool — our subagent / "Task" feature.
//!
//! Spawns a fresh `AgentSession` with an allowlisted, approval-wrapped
//! tool set (default: read/grep/find/ls only — the subagent is a
//! research helper, not a second hand on the keyboard). Inherits the
//! parent's [`ApprovalState`] so a tool the user already "always
//! allowed" doesn't re-prompt inside the child.
//!
//! Recursion is bounded by [`MAX_TASK_DEPTH`] in `code_factory.rs`:
//! once we're at the cap, the parent factory stops registering `task`,
//! so the agent sees it as unavailable and cannot chain further.

use std::sync::Arc;

use async_trait::async_trait;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{
    create_agent_session, AgentEvent, Result as PiResult, SessionOptions, Tool, ToolOutput,
    ToolUpdate,
};

use crate::commands::code_approvals::ApprovalState;
use crate::commands::code_factory::{LibertaiToolFactory, Mode};
use crate::config;

const NAME: &str = "task";
const LABEL: &str = "Task";
const DESCRIPTION: &str = concat!(
    "Run a focused subtask in an isolated agent session. Use when a ",
    "piece of research or a narrow lookup is well-defined and should ",
    "not clutter the main conversation. The child has its own history ",
    "and its own tool allowlist (defaults to read/grep/find/ls)."
);

pub struct TaskTool {
    mode: Mode,
    approvals: Arc<ApprovalState>,
    parent_depth: u8,
}

impl TaskTool {
    pub fn new(mode: Mode, approvals: Arc<ApprovalState>, parent_depth: u8) -> Self {
        Self {
            mode,
            approvals,
            parent_depth,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
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
                "prompt": {
                    "type": "string",
                    "description": "The subtask description the child agent will work on."
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional subset of tool names to enable (defaults to read, grep, find, ls)."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolOutput> {
        let prompt = match input.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(err_output(
                    "task tool requires a `prompt` string argument",
                ));
            }
        };

        let tools: Option<Vec<String>> = input
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });

        // Load our own Config — subtask runs against the same LibertAI
        // endpoint + model the parent is on. `code_models.rs` has
        // already registered libertai in pi's custom provider table by
        // the time this fires from a running parent session.
        let cfg = match config::load() {
            Ok(c) => c,
            Err(e) => {
                return Ok(err_output(&format!(
                    "task: could not load libertai config: {e}"
                )));
            }
        };

        // Child factory: same mode + shared approval state, but with
        // parent_depth + 1 so deeper nesting hits the recursion cap.
        let mut factory = LibertaiToolFactory::new(self.mode, Arc::clone(&self.approvals));
        factory.depth = self.parent_depth.saturating_add(1);

        let options = SessionOptions {
            provider: Some(cfg.default_code_provider.clone()),
            model: Some(cfg.default_code_model.clone()),
            no_session: true,
            max_tool_iterations: 25,
            enabled_tools: tools.or_else(|| {
                Some(
                    ["read", "grep", "find", "ls"]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                )
            }),
            tool_factory: Some(Arc::new(factory)),
            ..SessionOptions::default()
        };

        eprintln!("\n  \x1b[2m[subagent] running: {prompt}\x1b[0m");

        let mut handle = match create_agent_session(options).await {
            Ok(h) => h,
            Err(e) => return Ok(err_output(&format!("task: session init failed: {e}"))),
        };

        let assistant = match handle.prompt(prompt, render_child).await {
            Ok(msg) => msg,
            Err(e) => return Ok(err_output(&format!("task: run failed: {e}"))),
        };

        // Collapse the child assistant's text blocks into a single
        // string; tool-call / thinking blocks are dropped (the parent
        // doesn't need to see the child's internal moves).
        let text = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool {
        // The child may mutate (if its tool allowlist includes write/etc),
        // so we can't claim this as read-only.
        false
    }
}

/// Render events from the child session with a dim `subagent:` prefix
/// so they're visually distinct from the parent's main stream.
fn render_child(event: AgentEvent) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: pi::model::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            use std::io::Write;
            eprint!("\x1b[2m{delta}\x1b[0m");
            let _ = std::io::stderr().flush();
        }
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            eprintln!("\n  \x1b[2m[subagent tool] {tool_name}\x1b[0m");
        }
        AgentEvent::AgentEnd { .. } => {
            eprintln!();
        }
        _ => {}
    }
}

fn err_output(text: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
}
