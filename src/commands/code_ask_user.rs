//! The `ask_user` tool, Claude-Code-style structured questions.
//!
//! Lets the agent pause and ask the user one or more questions
//! (multi-choice or free-form) before continuing. Mirrors Claude
//! Code's `AskUserQuestion` tool so the LLM behaves identically when
//! running on LiberClaw vs Claude Code.
//!
//! Suspend/resume happens via [`ApprovalUi::ask`]: the desktop UI
//! emits a Tauri event, awaits the user's response on a `oneshot`,
//! and returns the answers as JSON. The terminal UI renders an
//! arrow/number-navigable chooser inline (see `code_term::ask_user`)
//! and returns the answers synchronously; if the user hits Esc it
//! sends `cancelled: true` so the LLM can adapt.
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
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_approvals::AskOutcome;

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
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        // Validate the input shape before suspending the agent on the
        // UI: a malformed payload should fail fast as a tool error so
        // the LLM can self-correct, not block the user with a broken
        // question card.
        if !input.is_object() || !input.get("questions").is_some_and(|q| q.is_array()) {
            return Ok(err_output(
                "ask_user: input must be { questions: [{ header, question, options[, multiSelect] }, ...] }",
            )
            .into());
        }
        emit_tool_started_update(on_update.as_deref());

        // We pass the same `input` to the UI so it has the questions
        // payload to render. On Paused we wrap the UI's payload with
        // the original questions so the resume hook re-emits the same
        // card stack.
        match self.ui.ask(input.clone()).await {
            AskOutcome::Answer(response) => Ok(answer_output(response).into()),
            AskOutcome::Paused {
                request_id,
                payload,
            } => Ok(wrap_paused_ask(request_id, payload, &input)),
        }
    }

    async fn resume(
        &self,
        _tool_call_id: &str,
        request_id: &str,
        payload: serde_json::Value,
    ) -> PiResult<ToolExecution> {
        let (ui_payload, questions) = unwrap_paused_ask(payload);
        // Prefer the persisted questions when re-firing so the user
        // sees the original cards even if the UI's payload didn't
        // round-trip them. UIs can ignore `ui_payload` and just use
        // questions, or use ui_payload to recover any in-flight state
        // (e.g. partial answers).
        let resume_payload = serde_json::json!({
            "ui_payload": ui_payload,
            "questions": questions,
        });
        match self.ui.resume_ask(request_id, resume_payload).await {
            AskOutcome::Answer(response) => Ok(answer_output(response).into()),
            AskOutcome::Paused {
                request_id,
                payload,
            } => Ok(wrap_paused_ask(request_id, payload, &questions)),
        }
    }

    fn is_read_only(&self) -> bool {
        // This is interactive and must be an execution barrier: the
        // user prompt should appear exactly where the model placed it,
        // not in a parallel read-only batch after unrelated tools.
        false
    }
}

fn emit_tool_started_update(on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>) {
    let Some(on_update) = on_update else {
        return;
    };
    on_update(ToolUpdate {
        content: Vec::new(),
        details: Some(serde_json::json!({
            "kind": "tool_started",
            "tool": NAME,
        })),
    });
}

/// Build the LLM-facing tool result content from an answer envelope.
fn answer_output(response: serde_json::Value) -> ToolOutput {
    let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(body))],
        details: None,
        is_error: response
            .get("cancelled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn wrap_paused_ask(
    request_id: String,
    ui_payload: serde_json::Value,
    questions: &serde_json::Value,
) -> ToolExecution {
    ToolExecution::Paused {
        request_id,
        kind: "ask_user".to_string(),
        payload: serde_json::json!({
            "ui_payload": ui_payload,
            "questions": questions,
        }),
    }
}

fn unwrap_paused_ask(payload: serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    if let serde_json::Value::Object(mut obj) = payload {
        let ui_payload = obj.remove("ui_payload").unwrap_or(serde_json::Value::Null);
        let questions = obj.remove("questions").unwrap_or(serde_json::Value::Null);
        (ui_payload, questions)
    } else {
        (serde_json::Value::Null, serde_json::Value::Null)
    }
}

fn err_output(msg: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct AnswerUi;

    #[async_trait]
    impl ApprovalUi for AnswerUi {
        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _always_rule: &str,
        ) -> crate::commands::code_approvals::PromptChoice {
            crate::commands::code_approvals::PromptChoice::Deny
        }

        async fn ask(&self, _payload: serde_json::Value) -> AskOutcome {
            AskOutcome::Answer(serde_json::json!({
                "answers": [{"header": "Scope", "selected": ["Here"]}]
            }))
        }
    }

    fn valid_input() -> serde_json::Value {
        serde_json::json!({
            "questions": [{
                "header": "Scope",
                "question": "Where?",
                "options": [{"label": "Here"}],
            }]
        })
    }

    #[test]
    fn ask_user_is_an_interactive_execution_barrier() {
        let tool = AskUserTool::new(Arc::new(AnswerUi));
        assert!(!tool.is_read_only());
    }

    #[test]
    fn ask_user_emits_actual_start_update_before_prompting() {
        let tool = AskUserTool::new(Arc::new(AnswerUi));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&updates);
        let execution = futures::executor::block_on(tool.execute(
            "ask-1",
            valid_input(),
            Some(Box::new(move |update| {
                seen.lock().unwrap().push(update);
            })),
        ))
        .unwrap();
        assert!(matches!(execution, ToolExecution::Done(_)));
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].details.as_ref().unwrap()["kind"], "tool_started");
        assert_eq!(updates[0].details.as_ref().unwrap()["tool"], NAME);
    }
}
