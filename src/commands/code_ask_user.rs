//! The `ask_user` tool, Claude-Code-style structured questions.
//!
//! Lets the agent pause and ask the user one or more questions
//! (multi-choice or free-form) before continuing. Mirrors Claude
//! Code's `AskUserQuestion` tool so the LLM behaves identically when
//! running on LiberClaw vs Claude Code.
//!
//! Suspend/resume happens via [`ApprovalUi::ask`]: the desktop UI
//! emits a Tauri event, awaits the user's response on a `oneshot`,
//! and returns the answers as JSON. The terminal UI gets the default
//! "cancelled" impl since interactive multi-choice prompts in a TUI
//! is its own can of worms; the LLM sees `cancelled: true` and can
//! adapt.
//!
//! Tool input shape (matches `AskUserQuestion`):
//!
//! ```jsonc
//! {
//!   "questions": [
//!     {
//!       "header": "Short label",
//!       "question": "Full question text",
//!       "multiSelect": false,
//!       "options": [
//!         { "label": "Option A", "description": "..." },
//!         { "label": "Other",    "description": "Type a custom answer" }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Tool output shape:
//!
//! ```jsonc
//! {
//!   "answers": [
//!     { "header": "Short label", "selected": ["Option A"], "other": null }
//!   ]
//! }
//! ```
//!
//! On cancel:
//!
//! ```jsonc
//! { "cancelled": true, "reason": "USER_DECLINED" }
//! ```

use std::sync::Arc;

use async_trait::async_trait;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

use crate::commands::code_approvals::ApprovalUi;

const NAME: &str = "ask_user";
const LABEL: &str = "Ask user";
const DESCRIPTION: &str = concat!(
    "Pause the agent loop and ask the user one or more clarifying ",
    "questions before continuing. Use this when you genuinely need ",
    "input the user has and you don't (which file, which approach, ",
    "which API endpoint, naming choices). Each question carries a ",
    "short header, the full text, and a list of options the user can ",
    "pick from. Include an \"Other\" option when free-form input is ",
    "useful. Set multiSelect=true to allow picking several options. ",
    "You receive the answers as the tool result and can continue. If ",
    "the user cancels, the result is { cancelled: true, reason: ",
    "\"USER_DECLINED\" } and you should stop or adapt accordingly.",
);

pub struct AskUserTool {
    ui: Arc<dyn ApprovalUi>,
}

impl AskUserTool {
    pub fn new(ui: Arc<dyn ApprovalUi>) -> Self {
        Self { ui }
    }
}

#[async_trait]
impl Tool for AskUserTool {
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
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more questions to surface in a single user prompt.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "description": "Short label shown as the question card title.",
                            },
                            "question": {
                                "type": "string",
                                "description": "Full question text shown under the header.",
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "description": "Whether the user can pick multiple options. Defaults to false.",
                            },
                            "options": {
                                "type": "array",
                                "description": "List of pickable options. Include an \"Other\" entry to allow free-form text.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Short option label.",
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Optional clarifying text shown next to the label.",
                                        },
                                    },
                                    "required": ["label"],
                                },
                            },
                        },
                        "required": ["header", "question", "options"],
                    },
                },
            },
            "required": ["questions"],
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolOutput> {
        // Validate the input shape before suspending the agent on the
        // UI: a malformed payload should fail fast as a tool error so
        // the LLM can self-correct, not block the user with a broken
        // question card.
        if !input.is_object() || !input.get("questions").map_or(false, |q| q.is_array()) {
            return Ok(err_output(
                "ask_user: input must be { questions: [{ header, question, options[, multiSelect] }, ...] }",
            ));
        }

        let response = self.ui.ask(input).await;

        // Render the answer back to the LLM as a single JSON text
        // block. The agent loop treats this as the tool result, the
        // LLM keeps the structured form for downstream reasoning.
        let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(body))],
            details: None,
            is_error: response
                .get("cancelled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    fn is_read_only(&self) -> bool {
        // No filesystem or network writes; preserves pi's parallelism
        // allowances. The "side effect" is purely user-interactive.
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
