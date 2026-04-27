//! Pi `Tool` impl for the LibertAI Fetch API. Wraps `post_fetch` so
//! the agent can pull article-mode content from a URL. Pairs with
//! search_tool — the search tool surfaces URLs, the fetch tool reads
//! them.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

use crate::client::post_fetch;
use crate::config::Config;

const NAME: &str = "fetch";
const LABEL: &str = "Fetch URL contents (LibertAI)";
const DESCRIPTION: &str = "Fetch and extract the readable content of a public URL via \
LibertAI's /fetch endpoint. Returns the page title and main content \
text (article-mode). Use after `search` to pull the body of a result \
the agent wants to read closely.";

#[derive(Debug, Clone, Deserialize)]
struct FetchInput {
    url: String,
}

pub struct FetchTool {
    cfg: Arc<Config>,
}

impl FetchTool {
    pub fn new(cfg: Arc<Config>) -> Self { Self { cfg } }
}

#[async_trait]
impl Tool for FetchTool {
    fn name(&self) -> &str { NAME }
    fn label(&self) -> &str { LABEL }
    fn description(&self) -> &str { DESCRIPTION }

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
    ) -> PiResult<ToolOutput> {
        let parsed: FetchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `fetch` payload: {e}"))),
        };
        // post_fetch is blocking reqwest — same caveat as search_tool.
        let resp = match post_fetch(&self.cfg, &parsed.url) {
            Ok(r) => r,
            Err(e) => return Ok(err_output(&format!("fetch failed: {e:#}"))),
        };

        let title = resp.title.clone().unwrap_or_default();
        let url = resp.url.clone().unwrap_or_else(|| parsed.url.clone());
        let content = resp.content.clone().unwrap_or_default();
        // Cap the body so a multi-megabyte page doesn't blow the
        // context window. Prefer the first 16k chars — the agent can
        // re-fetch with a more specific query if it needs more.
        let body: String = if content.chars().count() > 16_000 {
            let head: String = content.chars().take(16_000).collect();
            format!("{head}\n\n…[truncated; first 16000 chars]")
        } else {
            content
        };

        let envelope = json!({
            "text": format!("{title}\n{url}\n\n{body}").trim().to_string(),
            "cite": [ { "title": title, "url": url } ],
        });
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(envelope.to_string()))],
            details: None,
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool { true }
}

fn err_output(msg: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
}
