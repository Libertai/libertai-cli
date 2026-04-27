//! Pi `Tool` impl for the LibertAI Search API. Wraps the existing
//! `post_search` client so the agent can call /search.libertai.io
//! directly. Used by the desktop's chat pillar (the small assistants
//! that answer questions outside a project tree); other pillars keep
//! the full code-tool surface unchanged.
//!
//! The result envelope follows the design handoff's citation
//! contract: `{ text: "...", cite: [{ title, url, snippet }] }`. The
//! desktop's renderer parses this in `parseCitations` and renders
//! `<cite-chip>` elements inline.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

use crate::client::{post_search, SearchRequest};
use crate::config::Config;

const NAME: &str = "search";
const LABEL: &str = "Search the web (LibertAI)";
const DESCRIPTION: &str = "Search the public web via LibertAI's /search endpoint. \
Returns up to N (default 8) titled hits with snippets and source URLs. \
The result is a JSON envelope that the desktop renderer surfaces as \
inline citation chips. Use this for chat-pillar sessions that need \
fresh facts the model wouldn't reliably know.";

#[derive(Debug, Clone, Deserialize)]
struct SearchInput {
    query: String,
    /// Limit on results returned. Defaults to 8 when missing.
    #[serde(default)]
    max_results: Option<u32>,
    /// Optional engine subset (e.g. ["bing", "google"]). Server-side
    /// default is used when missing.
    #[serde(default)]
    engines: Option<Vec<String>>,
    /// "general" | "news" | "images" — passes through to the server.
    #[serde(default)]
    search_type: Option<String>,
}

/// Built once per session. The `Config` is captured at construction
/// time rather than re-loaded on each call so the tool keeps working
/// even if the on-disk config changes mid-session (mirrors what
/// libertai-cli's REPL does).
pub struct SearchTool {
    cfg: Arc<Config>,
}

impl SearchTool {
    pub fn new(cfg: Arc<Config>) -> Self { Self { cfg } }
}

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str { NAME }
    fn label(&self) -> &str { LABEL }
    fn description(&self) -> &str { DESCRIPTION }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Free-text search query." },
                "max_results": { "type": "integer", "description": "Cap on returned hits (default 8)." },
                "engines": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional engine subset. Defaults to the server's mix."
                },
                "search_type": {
                    "type": "string",
                    "enum": ["general", "news", "images"],
                    "description": "Result corpus. Defaults to 'general'."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolOutput> {
        let parsed: SearchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `search` payload: {e}"))),
        };
        let req = SearchRequest {
            query: &parsed.query,
            engines: parsed.engines,
            max_results: Some(parsed.max_results.unwrap_or(8)),
            search_type: parsed.search_type,
        };

        // post_search is blocking reqwest. Pi runs each tool execute
        // on its async runtime; the call blocks the runtime for the
        // duration of the request (typically <2 s), which is acceptable
        // for search — the user is already waiting on the agent.
        let resp = match post_search(&self.cfg, &req) {
            Ok(r) => r,
            Err(e) => return Ok(err_output(&format!("search failed: {e:#}"))),
        };

        // Build the {text, cite} envelope the FE's parseCitations
        // expects. We trim each snippet to ~200 chars so the agent's
        // context window doesn't get swamped on broad queries.
        let mut cite = Vec::with_capacity(resp.results.len());
        let mut text = String::new();
        for (i, r) in resp.results.iter().enumerate() {
            let title = r.title.clone().unwrap_or_else(|| format!("result {}", i + 1));
            let url = r.url.clone();
            let snippet = r.snippet.as_deref()
                .map(|s| {
                    let trimmed: String = s.chars().take(200).collect();
                    if s.len() > trimmed.len() { format!("{trimmed}…") } else { trimmed }
                });
            text.push_str(&format!("[{}] {title}\n", i + 1));
            if let Some(u) = &url { text.push_str(&format!("    {u}\n")); }
            if let Some(s) = &snippet { text.push_str(&format!("    {s}\n")); }
            text.push('\n');
            cite.push(json!({ "title": title, "url": url, "snippet": snippet }));
        }
        if resp.results.is_empty() {
            text = "no results".to_string();
        }

        let envelope = json!({ "text": text.trim_end(), "cite": cite });
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
