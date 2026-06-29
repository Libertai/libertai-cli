//! The `send_message` tool (M5/#21) — push-based inter-agent messaging.
//!
//! `mailbox` (M4) is file-based *polling*: a teammate calls `check` to
//! read its inbox, so there's no push notification. `send_message` adds
//! the push half — it writes to the recipient's mailbox (same durable
//! file backing) AND, for `to: "main"`, routes to the parent's
//! transcript. The parent TUI polls `mailbox/main/` each tick and
//! surfaces new messages as a delivered `TranscriptEntry::System` event,
//! so a teammate can push a finding to the parent mid-work without the
//! parent polling.
//!
//! ## `to: "main"` routing
//!
//! The parent (the TUI process) is the agent named `"main"` in mailbox
//! terms. A teammate's `send_message(to: "main", …)` writes a file to
//! `<team_dir>/mailbox/main/`; the parent's per-tick poll (in
//! `run_loop`, alongside `poll_agent_status`) reads new (unread)
//! messages there, renders each as `› message from <from>: <subject>
//! — <body>`, marks it read, and pushes the line — the "delivered event
//! the loop surfaces" the plan calls for. Messages to another teammate
//! land in that teammate's mailbox as usual; the teammate surfaces them
//! via `mailbox check`.
//!
//! ## Why a separate tool, not a `mailbox` action
//!
//! `mailbox`'s `send` is teammate→teammate polling delivery.
//! `send_message` carries the push semantics + the `to: "main"` arm +
//! a distinct name the model reaches for when it wants to *notify*
//! rather than *post*. Keeping the durable file backing (reusing
//! `code_mailbox` helpers) means a message survives a parent crash and
//! `mailbox check` still sees it.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_mailbox::{
    mailbox_dir_for, now_epoch_ms, short_uuid, write_message, MailMessage,
};

const NAME: &str = "send_message";
const LABEL: &str = "Send message";
const DESCRIPTION: &str = concat!(
    "Push a message to a teammate or to the parent (main). Unlike ",
    "`mailbox`, this is push delivery: a `to: \"main\"` message surfaces ",
    "in the parent's transcript immediately (the parent polls its inbox), ",
    "and a message to a teammate lands in their mailbox. The recipient ",
    "name `\"main\"` routes to the parent session. Use this to report a ",
    "finding, ask for direction, or hand off work mid-task rather than ",
    "waiting for the recipient to poll. The message is also written to the ",
    "recipient's mailbox file so it survives a crash."
);

/// The recipient name that routes to the parent TUI session. A
/// teammate's mailbox dir for the parent is `<team_dir>/mailbox/main/`.
pub(crate) const MAIN_RECIPIENT: &str = "main";

#[derive(Debug, Deserialize)]
struct SendMessageInput {
    to: String,
    subject: String,
    body: String,
}

/// The `send_message` tool. Bound to one team directory + the sender's
/// name (the teammate, or `"main"` for the parent).
pub struct SendMessageTool {
    team_dir: PathBuf,
    from: String,
}

impl SendMessageTool {
    pub fn new(team_dir: PathBuf, from: String) -> Self {
        Self { team_dir, from }
    }
}

impl Default for SendMessageTool {
    fn default() -> Self {
        Self::new(PathBuf::new(), String::new())
    }
}

#[async_trait]
impl Tool for SendMessageTool {
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
                "to": {
                    "type": "string",
                    "description": "Recipient teammate name, or \"main\" for the parent session."
                },
                "subject": {
                    "type": "string",
                    "description": "Short subject line."
                },
                "body": {
                    "type": "string",
                    "description": "Message body."
                }
            },
            "required": ["to", "subject", "body"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: SendMessageInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!("invalid `send_message` payload: {e}")));
            }
        };
        let to = parsed.to.trim();
        let subject = parsed.subject.trim();
        let body = parsed.body.trim();
        if to.is_empty() || subject.is_empty() || body.is_empty() {
            return Ok(err_output(
                "`send_message` requires non-empty `to`, `subject`, and `body`",
            ));
        }
        // Refuse sending to yourself — a self-message would land in your
        // own mailbox and (for `main`) surface in your own transcript,
        // which is never the intent. Mirrors `mailbox`'s implicit
        // assumption that sender ≠ recipient.
        if to == self.from {
            return Ok(err_output("cannot send a message to yourself"));
        }
        let msg = MailMessage {
            id: format!("msg-{}", short_uuid()),
            from: self.from.clone(),
            to: to.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            sent_at_ms: now_epoch_ms(),
            read: false,
        };
        let dir = mailbox_dir_for(&self.team_dir, to);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Ok(err_output(&format!(
                "create mailbox dir {}: {e}",
                dir.display()
            )));
        }
        if let Err(e) = write_message(&dir, &msg) {
            return Ok(err_output(&format!("write message: {e}")));
        }
        let routing = if to == MAIN_RECIPIENT {
            " (surfaced in the parent transcript)"
        } else {
            " (recipient polls their mailbox)"
        };
        Ok(text_output(&format!(
            "Message sent to {to}: {subject}{routing}"
        )))
    }

    fn is_read_only(&self) -> bool {
        // Writes a JSON file to the recipient's mailbox.
        false
    }
}

// ---- tool output constructors ----

fn text_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: false,
    }
    .into()
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::test_utils::run_test;

    fn tool(dir: &std::path::Path, from: &str) -> SendMessageTool {
        SendMessageTool::new(dir.to_path_buf(), from.to_string())
    }

    fn read_inbox(dir: &std::path::Path, who: &str) -> Vec<MailMessage> {
        let inbox = dir.join("mailbox").join(who);
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&inbox) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.filter_map(Result::ok) {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(&p) {
                if let Ok(m) = serde_json::from_str::<MailMessage>(&s) {
                    out.push(m);
                }
            }
        }
        out
    }

    #[test]
    fn send_to_teammate_writes_mailbox_file() {
        run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let t = tool(dir.path(), "alice");
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({
                        "to": "bob",
                        "subject": "found it",
                        "body": "the bug is in parser.rs",
                    }),
                    None,
                )
                .await
                .unwrap();
            match exec {
                ToolExecution::Done(out) => {
                    assert!(!out.is_error);
                    let txt = match out.content.first() {
                        Some(ContentBlock::Text(t)) => &t.text,
                        _ => panic!("no text"),
                    };
                    assert!(txt.contains("Message sent to bob: found it"), "{txt}");
                    assert!(txt.contains("recipient polls"), "{txt}");
                }
                _ => panic!("expected Done"),
            }
            let bob = read_inbox(dir.path(), "bob");
            assert_eq!(bob.len(), 1);
            assert_eq!(bob[0].from, "alice");
            assert_eq!(bob[0].to, "bob");
            assert_eq!(bob[0].subject, "found it");
            assert!(!bob[0].read);
        });
    }

    #[test]
    fn send_to_main_routes_to_main_mailbox() {
        run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let t = tool(dir.path(), "carol");
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({
                        "to": "main",
                        "subject": "done",
                        "body": "all green",
                    }),
                    None,
                )
                .await
                .unwrap();
            match exec {
                ToolExecution::Done(out) => {
                    let txt = match out.content.first() {
                        Some(ContentBlock::Text(t)) => &t.text,
                        _ => panic!("no text"),
                    };
                    assert!(txt.contains("surfaced in the parent transcript"), "{txt}");
                }
                _ => panic!("expected Done"),
            }
            let main = read_inbox(dir.path(), "main");
            assert_eq!(main.len(), 1);
            assert_eq!(main[0].to, "main");
            assert_eq!(main[0].from, "carol");
        });
    }

    #[test]
    fn send_rejects_empty_fields() {
        run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let t = tool(dir.path(), "alice");
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({ "to": "bob", "subject": "", "body": "x" }),
                    None,
                )
                .await
                .unwrap();
            match exec {
                ToolExecution::Done(out) => {
                    assert!(out.is_error);
                }
                _ => panic!("expected Done"),
            }
        });
    }

    #[test]
    fn send_rejects_self_message() {
        run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let t = tool(dir.path(), "alice");
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({
                        "to": "alice",
                        "subject": "note",
                        "body": "to myself",
                    }),
                    None,
                )
                .await
                .unwrap();
            match exec {
                ToolExecution::Done(out) => {
                    assert!(out.is_error);
                    let txt = match out.content.first() {
                        Some(ContentBlock::Text(t)) => &t.text,
                        _ => panic!("no text"),
                    };
                    assert!(txt.contains("yourself"), "{txt}");
                }
                _ => panic!("expected Done"),
            }
            // Nothing written.
            assert!(read_inbox(dir.path(), "alice").is_empty());
        });
    }

    #[test]
    fn send_trims_whitespace_fields() {
        run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let t = tool(dir.path(), "alice");
            t.execute(
                "c1",
                serde_json::json!({
                    "to": "  bob  ",
                    "subject": "  hi  ",
                    "body": "  body  ",
                }),
                None,
            )
            .await
            .unwrap();
            let bob = read_inbox(dir.path(), "bob");
            assert_eq!(bob.len(), 1);
            assert_eq!(bob[0].to, "bob");
            assert_eq!(bob[0].subject, "hi");
            assert_eq!(bob[0].body, "body");
        });
    }
}
