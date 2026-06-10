//! `libertai ask` — one-shot prompt, non-streaming.
//!
//! Output contract: when stdout is a terminal the answer is rendered as
//! markdown (rich_rust via pi's console — honours NO_COLOR and terminal
//! width); when stdout is piped/redirected the raw model text is printed
//! unchanged so `libertai ask ... | jq`-style scripting keeps working.

use anyhow::{Context, Result};

use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};
use crate::commands::chat_render::markdown_enabled_stdout;
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
        max_tokens: None,
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

    if markdown_enabled_stdout() {
        // TTY: pretty markdown. render_markdown guarantees a trailing
        // newline of its own.
        pi::tui::PiConsole::new().render_markdown(content);
    } else {
        print!("{}", raw_output(content));
    }
    Ok(())
}

/// Piped/non-TTY form of the answer: the model text byte-for-byte with
/// exactly one trailing newline appended when missing — never any ANSI.
/// This is the scriptability contract pinned by `tests/probes_chat_ask.rs`
/// and the unit tests below.
fn raw_output(content: &str) -> String {
    if content.ends_with('\n') {
        content.to_string()
    } else {
        format!("{content}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::raw_output;

    #[test]
    fn raw_output_passes_markdown_through_unchanged() {
        let md = "# Title\n\nSome **bold** text\n\n```rust\nfn main() {}\n```";
        let out = raw_output(md);
        assert_eq!(out, format!("{md}\n"));
        assert!(!out.contains('\u{1b}'), "raw output must not contain ANSI");
    }

    #[test]
    fn raw_output_does_not_double_trailing_newline() {
        assert_eq!(raw_output("done\n"), "done\n");
    }
}
