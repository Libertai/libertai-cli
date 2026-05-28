//! Agent-callable user notification tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_approvals::{ApprovalUi, NotifyOutcome};

const NAME: &str = "push_notification";
const LABEL: &str = "Push notification";
const DESCRIPTION: &str = "Ask the UI to show a user notification. Use this only when the user should be alerted while the agent continues or after completing a long-running task. Keep title and body short.";

#[derive(Debug, Deserialize)]
struct NotificationInput {
    title: String,
    body: String,
}

pub struct PushNotificationTool {
    ui: Arc<dyn ApprovalUi>,
}

impl PushNotificationTool {
    pub fn new(ui: Arc<dyn ApprovalUi>) -> Self {
        Self { ui }
    }
}

#[async_trait]
impl Tool for PushNotificationTool {
    fn name(&self) -> &str {
        NAME
    }

    fn label(&self) -> &str {
        LABEL
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short notification title."
                },
                "body": {
                    "type": "string",
                    "description": "Short notification body."
                }
            },
            "required": ["title", "body"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: NotificationInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `push_notification` payload: {e}"))),
        };
        let title = parsed.title.trim();
        let body = parsed.body.trim();
        if title.is_empty() || body.is_empty() {
            return Ok(err_output("push_notification requires non-empty title and body"));
        }

        match self.ui.notify(title, body).await {
            NotifyOutcome::Sent => Ok(text_output("notification sent", false)),
            NotifyOutcome::Skipped(reason) => Ok(text_output(
                &format!("notification skipped: {reason}"),
                false,
            )),
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

fn text_output(text: &str, is_error: bool) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        details: None,
        is_error,
    }
    .into()
}

fn err_output(text: &str) -> ToolExecution {
    text_output(text, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code_approvals::{AskOutcome, PromptChoice};
    use std::sync::Mutex;

    struct RecordingUi {
        sent: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl ApprovalUi for RecordingUi {
        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _always_rule: &str,
        ) -> PromptChoice {
            PromptChoice::Deny
        }

        async fn ask(&self, _payload: Value) -> AskOutcome {
            AskOutcome::Answer(json!({ "cancelled": true }))
        }

        async fn notify(&self, title: &str, body: &str) -> NotifyOutcome {
            self.sent
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
            NotifyOutcome::Sent
        }
    }

    #[test]
    fn sends_notification_through_ui() {
        asupersync::test_utils::run_test(|| async {
            let ui = Arc::new(RecordingUi {
                sent: Mutex::new(Vec::new()),
            });
            let tool = PushNotificationTool::new(ui.clone());
            let out = match tool
                .execute(
                    "call",
                    json!({ "title": "Done", "body": "Build finished" }),
                    None,
                )
                .await
                .unwrap()
            {
                ToolExecution::Done(o) => o,
                _ => panic!("expected done"),
            };
            assert!(!out.is_error);
            assert_eq!(
                ui.sent.lock().unwrap().as_slice(),
                &[("Done".to_string(), "Build finished".to_string())]
            );
        });
    }
}
