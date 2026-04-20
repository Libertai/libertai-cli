use anyhow::{Context, Result};

use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};
use crate::config::load;

pub fn run(prompt: String, model: Option<String>) -> Result<()> {
    let cfg = load()?;
    let model = model.unwrap_or_else(|| cfg.default_chat_model.clone());

    let req = ChatRequest {
        model,
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }],
        stream: Some(false),
    };

    let resp = post_chat_blocking(&cfg, &req)?;
    let body: serde_json::Value = resp
        .json()
        .context("parsing /v1/chat/completions response")?;

    let content = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .context("response missing choices[0].message.content")?;

    if content.ends_with('\n') {
        print!("{content}");
    } else {
        println!("{content}");
    }
    Ok(())
}
