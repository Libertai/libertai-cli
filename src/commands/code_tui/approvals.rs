//! Ratatui approval UI — implements `ApprovalUi` trait using a
//! channel-based modal overlay.
//!
//! The background thread (pi session) calls `decide()`, which sends
//! an `ApprovalRequest` message to the main thread's event loop. The
//! main thread shows a modal, collects the user's key press, and sends
//! the `PromptChoice` back through the oneshot channel.

use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::code_approvals::{
    AskOutcome, ApprovalUi, NotifyOutcome, PromptChoice,
};

/// Ratatui-based approval UI. Shared between the background thread
/// (which calls `decide`/`ask`) and the main thread (which renders
/// the modal and sends the choice back).
pub struct RatatuiApprovalUi {
    /// Sender to the main thread's event loop.
    tx: std::sync::mpsc::Sender<crate::commands::code_tui::app::AgentMsg>,
}

impl RatatuiApprovalUi {
    pub fn new(tx: std::sync::mpsc::Sender<crate::commands::code_tui::app::AgentMsg>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl ApprovalUi for RatatuiApprovalUi {
    async fn decide(
        &self,
        tool_name: &str,
        preview: &str,
        always_rule: &str,
    ) -> PromptChoice {
        let (resp_tx, resp_rx) = std::sync::mpsc::channel();

        let msg = crate::commands::code_tui::app::AgentMsg::ApprovalRequest {
            tool_name: tool_name.to_string(),
            preview: preview.to_string(),
            always_rule: always_rule.to_string(),
            responder: resp_tx,
        };

        if self.tx.send(msg).is_err() {
            return PromptChoice::Deny;
        }

        resp_rx.recv().unwrap_or(PromptChoice::Deny)
    }

    async fn ask(&self, _payload: serde_json::Value) -> AskOutcome {
        // TODO: wire up ask_user modal
        AskOutcome::Answer(serde_json::json!({
            "cancelled": true,
            "reason": "ASK_NOT_SUPPORTED",
        }))
    }

    async fn notify(&self, _title: &str, _body: &str) -> NotifyOutcome {
        NotifyOutcome::Skipped("NOTIFY_NOT_SUPPORTED".to_string())
    }
}

/// Helper to create an `Arc<dyn ApprovalUi>` for the factory.
pub fn arc(
    tx: std::sync::mpsc::Sender<crate::commands::code_tui::app::AgentMsg>,
) -> Arc<dyn ApprovalUi> {
    Arc::new(RatatuiApprovalUi::new(tx))
}
