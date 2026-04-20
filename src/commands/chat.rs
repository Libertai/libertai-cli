use anyhow::Result;
use owo_colors::OwoColorize;
use reqwest::header::CONTENT_TYPE;
use std::io::{BufRead, BufReader, Write};

use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};
use crate::config::load;

pub fn run(model: Option<String>, system: Option<String>) -> Result<()> {
    let cfg = load()?;
    let model = model.unwrap_or_else(|| cfg.default_chat_model.clone());

    let mut history: Vec<ChatMessage> = Vec::new();
    if let Some(sys) = system {
        history.push(ChatMessage {
            role: "system".to_string(),
            content: sys,
        });
    }

    eprintln!(
        "{}",
        format!("LibertAI chat — model: {model}. Ctrl-D or /exit to quit.").cyan()
    );

    let stdin = std::io::stdin();
    loop {
        eprint!("{}", "> ".green());
        std::io::stderr().flush().ok();

        let mut buf = String::new();
        let n = stdin.lock().read_line(&mut buf)?;
        if n == 0 {
            eprintln!();
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            break;
        }

        history.push(ChatMessage {
            role: "user".to_string(),
            content: line.to_string(),
        });

        let req = ChatRequest {
            model: model.clone(),
            messages: history.clone(),
            stream: Some(true),
        };

        let resp = match post_chat_blocking(&cfg, &req) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} {e}", "error:".red());
                history.pop();
                continue;
            }
        };

        // If the server didn't actually give us an SSE stream (e.g. it
        // rejected `stream:true` and returned a plain JSON error), the
        // line-by-line `data: ` parser below would silently swallow every
        // line and we'd print nothing. Detect that up front and surface
        // whatever the server did return.
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase().contains("text/event-stream"))
            .unwrap_or(false);
        if !is_sse {
            let body = resp.text().unwrap_or_default();
            let shown = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
                    .or_else(|| v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()));
                match msg {
                    Some(m) => format!("error: {m}"),
                    None => {
                        let t = truncate_2k(&body);
                        format!("unexpected non-SSE response: {t}")
                    }
                }
            } else {
                let t = truncate_2k(&body);
                format!("unexpected non-SSE response: {t}")
            };
            eprintln!("{}", shown.red());
            history.pop();
            continue;
        }

        let reader = BufReader::new(resp);
        let mut assistant = String::new();
        let mut stream_err: Option<anyhow::Error> = None;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    stream_err = Some(anyhow::anyhow!(e));
                    break;
                }
            };
            // SSE: only `data:` lines carry JSON payloads. Skip blank lines,
            // `:` comments, `event:`, `id:`, and anything else without
            // attempting to parse it as JSON.
            let payload = match line.strip_prefix("data: ") {
                Some(p) => p,
                None => continue,
            };
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                break;
            }
            let v: serde_json::Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(delta) = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
            {
                print!("{delta}");
                std::io::stdout().flush().ok();
                assistant.push_str(delta);
            }
        }

        println!();

        if let Some(e) = stream_err {
            eprintln!("{} {e}", "error:".red());
            history.pop();
            continue;
        }

        history.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant,
        });
    }

    Ok(())
}

fn truncate_2k(s: &str) -> String {
    const LIMIT: usize = 2048;
    if s.chars().count() > LIMIT {
        let mut out: String = s.chars().take(LIMIT).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}
