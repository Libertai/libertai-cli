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
    create_agent_session, AgentEvent, Result as PiResult, Tool, ToolOutput, ToolUpdate,
};

use crate::commands::code_approvals::{ApprovalState, ApprovalUi};
use crate::commands::code_factory::{LibertaiToolFactory, ModeFlag};
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, SessionPersistence,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::config;

const NAME: &str = "task";
const LABEL: &str = "Task";
const DESCRIPTION: &str = concat!(
    "Run a focused subtask in an isolated agent session. Use when a ",
    "piece of research or a narrow lookup is well-defined and should ",
    "not clutter the main conversation. The child runs with a fixed ",
    "read-only tool set (read, grep, find, ls) — subagents cannot ",
    "mutate the filesystem, even if the caller names other tools."
);

/// Tools a subagent is ever allowed to run, regardless of what the
/// caller passes in. This is a hard ceiling, not a default: even if a
/// compromised or prompt-injected model names `bash` or `write` in the
/// `tools` argument, those names are filtered out here and the child
/// session gets the intersection with this list.
const TASK_TOOL_ALLOWLIST: &[&str] = &["read", "grep", "find", "ls"];

pub struct TaskTool {
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    parent_depth: u8,
}

impl TaskTool {
    pub fn new(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        parent_depth: u8,
    ) -> Self {
        Self {
            mode,
            approvals,
            ui,
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

        // Intersect the caller's tool list with our read-only allowlist.
        // A missing or empty `tools` argument falls back to the full
        // allowlist; an all-invalid list drops to the allowlist as well
        // (rather than giving the child an empty tool registry and no
        // way to research).
        let requested: Vec<String> = input
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let filtered: Vec<String> = if requested.is_empty() {
            TASK_TOOL_ALLOWLIST.iter().map(|&s| s.to_string()).collect()
        } else {
            let f: Vec<String> = requested
                .into_iter()
                .filter(|name| TASK_TOOL_ALLOWLIST.contains(&name.as_str()))
                .collect();
            if f.is_empty() {
                TASK_TOOL_ALLOWLIST.iter().map(|&s| s.to_string()).collect()
            } else {
                f
            }
        };

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

        // Child factory: shared mode flag + shared approval state, but
        // with parent_depth + 1 so deeper nesting hits the recursion
        // cap. `LibertaiToolFactory::child` is the one place that
        // increments depth.
        let factory = LibertaiToolFactory {
            mode: self.mode.clone(),
            approvals: Arc::clone(&self.approvals),
            ui: Arc::clone(&self.ui),
            depth: self.parent_depth,
            // Subagents inherit CLI defaults (task on, todo on, search/fetch off)
            // so a parent code session can recursively spawn coding subagents
            // exactly as it does today. The desktop's chat pillar opts out of
            // task entirely so this branch never spawns from chat.
            features: crate::commands::code_factory::FactoryFeatures::cli_defaults(),
            libertai_cfg: None,
        }
        .child();

        let max_tokens = Some(crate::commands::code_session::DEFAULT_MAX_TOKENS);
        let skill_cwd = std::env::current_dir().ok();
        let append_system_prompt =
            code_skills::prompt_for_pillar(SkillPillar::Code, skill_cwd.as_deref())
                .ok()
                .flatten();
        let options = build_session_options(CodeSessionConfig {
            provider: cfg.default_code_provider.clone(),
            model: cfg.default_code_model.clone(),
            working_directory: None,
            include_cwd_in_prompt: true,
            max_tool_iterations: 25,
            tool_factory: Arc::new(factory),
            // Subagents are nested scratch sessions — their JSONL would
            // pollute the user-facing session list with noise.
            persistence: SessionPersistence::Ephemeral,
            enabled_tools: Some(filtered),
            append_system_prompt,
            max_tokens,
        });

        eprintln!("\n  \x1b[2m[subagent] running: {prompt}\x1b[0m");

        let mut handle = match create_agent_session(options).await {
            Ok(h) => h,
            Err(e) => return Ok(err_output(&format!("task: session init failed: {e}"))),
        };
        handle.set_max_tokens(max_tokens);

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
