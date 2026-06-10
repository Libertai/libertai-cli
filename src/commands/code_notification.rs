//! Agent-callable user notification tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_approvals::{ApprovalUi, NotifyOutcome};
use crate::config::Config as LibertaiConfig;

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
    cfg: Option<Arc<LibertaiConfig>>,
}

impl PushNotificationTool {
    pub fn new(ui: Arc<dyn ApprovalUi>) -> Self {
        Self { ui, cfg: None }
    }

    pub fn with_config(mut self, cfg: Option<Arc<LibertaiConfig>>) -> Self {
        self.cfg = cfg;
        self
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
            Err(e) => {
                return Ok(err_output(&format!(
                    "invalid `push_notification` payload: {e}"
                )))
            }
        };
        let title = parsed.title.trim();
        let body = parsed.body.trim();
        if title.is_empty() || body.is_empty() {
            return Ok(err_output(
                "push_notification requires non-empty title and body",
            ));
        }

        let outcome = self.ui.notify(title, body).await;
        if let Some(cfg) = self.cfg.as_deref() {
            crate::commands::code_hooks::run_notification_hooks(cfg, title, body, &outcome);
        }

        match outcome {
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

    #[test]
    fn runs_notification_hooks_after_notify() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let output = cwd.path().join("notification.txt");
            let ui = Arc::new(RecordingUi {
                sent: Mutex::new(Vec::new()),
            });
            let cfg = Arc::new(LibertaiConfig {
                hooks: crate::config::HooksConfig {
                    notification: vec![crate::config::HookCommandConfig {
                        command: format!("printf \"$LIBERTAI_HOOK_EVENT\" > {}", output.display()),
                        ..crate::config::HookCommandConfig::default()
                    }],
                    ..crate::config::HooksConfig::default()
                },
                ..LibertaiConfig::default()
            });
            let tool = PushNotificationTool::new(ui).with_config(Some(cfg));
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
            assert_eq!(std::fs::read_to_string(output).unwrap(), "Notification");
        });
    }
}
