//! Auxiliary model helpers for `libertai code`.
//!
//! Smart approvals are intentionally opt-in and conservative: only a
//! well-formed first-line verdict bypasses the normal approval UI.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};
use crate::config::Config as LibertaiConfig;

const SMART_APPROVAL_TIMEOUT_SECS: u64 = 4;
const SMART_APPROVAL_MAX_TOKENS: u32 = 16;
const SMART_APPROVAL_INPUT_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartApprovalVerdict {
    Approve,
    Deny { reason: Option<String> },
    Escalate { reason: Option<String> },
}

#[async_trait]
pub trait SmartApproval: Send + Sync {
    async fn decide(
        &self,
        tool_name: &str,
        preview: &str,
        input: &serde_json::Value,
    ) -> SmartApprovalVerdict;
}

pub fn smart_approval_from_config(cfg: Arc<LibertaiConfig>) -> Option<Arc<dyn SmartApproval>> {
    if !cfg.smart_approval_enabled {
        return None;
    }
    if cfg.auth.api_key.as_deref().unwrap_or("").trim().is_empty() {
        return None;
    }
    if cfg.smart_approval_model.trim().is_empty() {
        return None;
    }
    Some(Arc::new(LlmSmartApproval::new(cfg)))
}

pub struct LlmSmartApproval {
    cfg: Arc<LibertaiConfig>,
}

impl LlmSmartApproval {
    pub fn new(cfg: Arc<LibertaiConfig>) -> Self {
        Self { cfg }
    }

    fn decide_blocking(
        &self,
        tool_name: &str,
        preview: &str,
        input: &serde_json::Value,
    ) -> Result<SmartApprovalVerdict> {
        let mut cfg = self.cfg.as_ref().clone();
        cfg.http_timeout_secs = cfg.http_timeout_secs.clamp(1, SMART_APPROVAL_TIMEOUT_SECS);
        let req = ChatRequest {
            model: cfg.smart_approval_model.clone(),
            messages: build_smart_approval_messages(tool_name, preview, input),
            stream: Some(false),
            max_tokens: Some(SMART_APPROVAL_MAX_TOKENS),
        };
        let resp = post_chat_blocking(&cfg, &req)?;
        let body: serde_json::Value = resp
            .json()
            .context("parsing smart approval /v1/chat/completions response")?;
        let content = body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .context("smart approval response missing choices[0].message.content")?;
        Ok(parse_smart_approval_response(content))
    }
}

#[async_trait]
impl SmartApproval for LlmSmartApproval {
    async fn decide(
        &self,
        tool_name: &str,
        preview: &str,
        input: &serde_json::Value,
    ) -> SmartApprovalVerdict {
        let cfg = Arc::clone(&self.cfg);
        let tool_name = tool_name.to_string();
        let preview = preview.to_string();
        let input = input.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let smart = LlmSmartApproval { cfg };
            let verdict = smart
                .decide_blocking(&tool_name, &preview, &input)
                .unwrap_or_else(|err| SmartApprovalVerdict::Escalate {
                    reason: Some(format!("smart approval unavailable: {err:#}")),
                });
            let _ = tx.send(verdict);
        });
        match rx.recv_timeout(Duration::from_secs(SMART_APPROVAL_TIMEOUT_SECS)) {
            Ok(verdict) => verdict,
            Err(mpsc::RecvTimeoutError::Timeout) => SmartApprovalVerdict::Escalate {
                reason: Some(format!(
                    "smart approval timed out after {SMART_APPROVAL_TIMEOUT_SECS}s"
                )),
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => SmartApprovalVerdict::Escalate {
                reason: Some("smart approval worker stopped before returning".to_string()),
            },
        }
    }
}

fn build_smart_approval_messages(
    tool_name: &str,
    preview: &str,
    input: &serde_json::Value,
) -> Vec<ChatMessage> {
    let input = truncate_chars(&input.to_string(), SMART_APPROVAL_INPUT_MAX_CHARS);
    let preview = truncate_chars(preview, SMART_APPROVAL_INPUT_MAX_CHARS);
    vec![
        ChatMessage {
            role: "system".to_string(),
            content: concat!(
                "You are a strict tool approval classifier for a local coding agent. ",
                "Reply with exactly one first-line verdict: APPROVE, DENY, or ESCALATE. ",
                "APPROVE only if the tool call is routine, scoped to the user's likely task, ",
                "and low risk. DENY if it is destructive, credential-seeking, exfiltrating, ",
                "or unrelated. ESCALATE when uncertain or when human context is needed. ",
                "Do not add prose before the verdict."
            )
            .to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!(
                "Tool: {tool_name}\nPreview:\n{preview}\n\nRaw JSON input:\n{input}\n\nVerdict:"
            ),
        },
    ]
}

fn parse_smart_approval_response(text: &str) -> SmartApprovalVerdict {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut parts = first.trim().splitn(2, char::is_whitespace);
    let verdict = parts
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_uppercase();
    let reason = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    match verdict.as_str() {
        "APPROVE" => SmartApprovalVerdict::Approve,
        "DENY" => SmartApprovalVerdict::Deny { reason },
        "ESCALATE" => SmartApprovalVerdict::Escalate { reason },
        _ => SmartApprovalVerdict::Escalate {
            reason: Some("unrecognized smart approval verdict".to_string()),
        },
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_smart_approval_verdicts() {
        assert_eq!(
            parse_smart_approval_response("APPROVE\nbecause"),
            SmartApprovalVerdict::Approve
        );
        assert_eq!(
            parse_smart_approval_response("DENY removes too much"),
            SmartApprovalVerdict::Deny {
                reason: Some("removes too much".to_string())
            }
        );
        assert_eq!(
            parse_smart_approval_response("ESCALATE needs context"),
            SmartApprovalVerdict::Escalate {
                reason: Some("needs context".to_string())
            }
        );
    }

    #[test]
    fn malformed_smart_approval_escalates() {
        assert_eq!(
            parse_smart_approval_response("probably fine"),
            SmartApprovalVerdict::Escalate {
                reason: Some("unrecognized smart approval verdict".to_string())
            }
        );
    }

    #[test]
    fn smart_approval_prompt_caps_large_inputs() {
        let messages = build_smart_approval_messages(
            "bash",
            &"p".repeat(SMART_APPROVAL_INPUT_MAX_CHARS + 100),
            &serde_json::json!({"command": "x".repeat(SMART_APPROVAL_INPUT_MAX_CHARS + 100)}),
        );
        assert!(messages[1].content.contains("..."));
        assert!(messages[1].content.len() < SMART_APPROVAL_INPUT_MAX_CHARS * 3);
    }
}
