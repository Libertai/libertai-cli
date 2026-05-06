//! Pi `Tool` impl for fetching public URLs locally with `reqwest::blocking`.
//!
//! Replaces the previous LibertAI `/fetch` wrapper. Returns the page's
//! `<title>` (best-effort regex), the final URL after redirects, and up
//! to 16k chars of body text. Strips HTML to plain text via a tiny
//! tag-stripping pass — full readability extraction is out of scope.
//!
//! The result envelope keeps the same `{ text, cite }` shape the FE
//! renderer expects so `parseCitations` keeps working unchanged.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

const NAME: &str = "fetch";
const LABEL: &str = "Fetch URL contents";
const DESCRIPTION: &str = "Fetch the contents of a public http(s) URL. Returns the page \
title, final URL after redirects, and up to 16,000 characters of body text \
(HTML is stripped to plain text). Use this to read a page the agent has just \
discovered via `search` or that the user has linked to.";

/// Body-size cap for the returned text. Mirrors the previous LibertAI
/// fetch tool so context-window pressure stays predictable.
const MAX_CHARS: usize = 16_000;

#[derive(Debug, Clone, Deserialize)]
struct FetchInput {
    url: String,
}

pub struct FetchTool;

impl FetchTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for FetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FetchTool {
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
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL to fetch." }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: FetchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `fetch` payload: {e}"))),
        };

        let page = match local_fetch(&parsed.url, MAX_CHARS) {
            Ok(p) => p,
            Err(e) => return Ok(err_output(&format!("fetch failed: {e}"))),
        };

        let envelope = json!({
            "text": format!("{}\n{}\n\n{}", page.title, page.final_url, page.text)
                .trim()
                .to_string(),
            "cite": [ { "title": page.title, "url": page.final_url } ],
        });
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(envelope.to_string()))],
            details: None,
            is_error: false,
        }
        .into())
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

/// Result of a one-shot HTTP GET + body text extraction.
pub struct FetchedPage {
    pub final_url: String,
    pub title: String,
    pub text: String,
}

/// One-shot HTTP GET with redirect following, body-size cap, and a
/// best-effort HTML→text pass. Shared between the agent `fetch` tool
/// and the standalone `libertai fetch` CLI command so both behave
/// identically.
pub fn local_fetch(url: &str, max_chars: usize) -> anyhow::Result<FetchedPage> {
    use anyhow::{anyhow, Context};

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(anyhow!("only http(s) URLs are allowed"));
    }

    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    if !status.is_success() {
        return Err(anyhow!("HTTP {status} from {final_url}"));
    }
    let body = resp
        .text()
        .with_context(|| format!("decoding body from {final_url}"))?;

    let title = extract_title(&body).unwrap_or_else(|| final_url.clone());
    let text = strip_to_text(&body, max_chars);

    Ok(FetchedPage {
        final_url,
        title,
        text,
    })
}

fn http_client() -> anyhow::Result<&'static reqwest::blocking::Client> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let built = reqwest::blocking::Client::builder()
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()?;
    Ok(CLIENT.get_or_init(|| built))
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let after = open + "<title".len();
    let gt = lower[after..].find('>')? + after + 1;
    let close = lower[gt..].find("</title>")? + gt;
    let raw = html.get(gt..close)?.trim();
    if raw.is_empty() {
        None
    } else {
        Some(decode_entities(raw))
    }
}

/// Strip HTML tags + collapse whitespace, then truncate to `max_chars`.
/// Not a readability pass — just enough that an LLM can read the page.
fn strip_to_text(html: &str, max_chars: usize) -> String {
    // Drop <script>, <style>, <noscript>, and HTML comments wholesale —
    // they're noise for an LLM and often dwarf the visible body.
    let mut buf = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(skip) = skip_block(&lower, i, "<script", "</script>")
            .or_else(|| skip_block(&lower, i, "<style", "</style>"))
            .or_else(|| skip_block(&lower, i, "<noscript", "</noscript>"))
            .or_else(|| skip_comment(&lower, i))
        {
            i = skip;
            continue;
        }
        buf.push(bytes[i] as char);
        i += 1;
    }
    // Tag-strip + entity-decode, then collapse whitespace.
    let stripped = strip_tags(&buf);
    let decoded = decode_entities(&stripped);
    let collapsed = collapse_whitespace(&decoded);
    if collapsed.chars().count() > max_chars {
        let head: String = collapsed.chars().take(max_chars).collect();
        format!("{head}\n\n…[truncated; first {max_chars} chars]")
    } else {
        collapsed
    }
}

fn skip_block(lower: &str, i: usize, open: &str, close: &str) -> Option<usize> {
    let from = lower.get(i..)?;
    if !from.starts_with(open) {
        return None;
    }
    let close_at = from.find(close)?;
    Some(i + close_at + close.len())
}

fn skip_comment(lower: &str, i: usize) -> Option<usize> {
    let from = lower.get(i..)?;
    if !from.starts_with("<!--") {
        return None;
    }
    let close_at = from.find("-->")?;
    Some(i + close_at + "-->".len())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_tag = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_tag {
            if c == b'>' {
                in_tag = false;
                out.push(' ');
            }
        } else if c == b'<' {
            in_tag = true;
        } else {
            out.push(c as char);
        }
        i += 1;
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    let mut consecutive_newlines = 0u8;
    for ch in s.chars() {
        if ch == '\n' {
            consecutive_newlines = consecutive_newlines.saturating_add(1);
            if consecutive_newlines <= 2 {
                out.push('\n');
            }
            last_was_space = true;
        } else if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            consecutive_newlines = 0;
            last_was_space = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_title_basic() {
        assert_eq!(
            extract_title("<html><head><title>Hello</title></head>").as_deref(),
            Some("Hello"),
        );
    }

    #[test]
    fn extract_title_with_attrs() {
        assert_eq!(
            extract_title("<title lang=\"en\">  Spaced  </title>").as_deref(),
            Some("Spaced"),
        );
    }

    #[test]
    fn strip_drops_scripts_and_styles() {
        let html = "<script>alert(1)</script>before<style>p{}</style>middle<!-- c -->after";
        let out = strip_to_text(html, 1000);
        assert_eq!(out, "beforemiddleafter");
    }

    #[test]
    fn strip_decodes_entities() {
        assert_eq!(strip_to_text("a &amp; b &lt;c&gt;", 100), "a & b <c>");
    }

    #[test]
    fn strip_truncates() {
        let body = "x".repeat(200);
        let out = strip_to_text(&body, 50);
        assert!(out.starts_with(&"x".repeat(50)));
        assert!(out.contains("…[truncated; first 50 chars]"));
    }
}
