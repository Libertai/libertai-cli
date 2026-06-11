//! `libertai mcp` — an MCP server over stdio exposing LibertAI web search
//! and page fetch, so MCP clients (Claude Code, Claude Desktop, Cursor,
//! Cline, …) can use LibertAI's search API as a tool.
//!
//! Transport is line-delimited JSON-RPC 2.0 on stdin/stdout, protocol
//! revision `2025-03-26` — the same revision the CLI's own MCP *client*
//! paths speak (see `code_mcp.rs` / `code_hooks.rs`). Frames are the only
//! thing ever written to stdout; logs go to stderr.
//!
//! Two tools:
//! - `web_search` — wraps the existing [`post_search`] client
//!   (`search.libertai.io`), the same path `libertai search` uses.
//! - `fetch_page` — wraps [`local_fetch`], the same path `libertai fetch`
//!   uses, with a 100k-char cap suited to MCP tool results.
//!
//! Auth: the API key comes from `~/.config/libertai/config.toml` (set by
//! `libertai login`) or the `LIBERTAI_API_KEY` env var. A missing key is a
//! *tool* error with setup instructions, never a crash — MCP clients keep
//! the server alive and surface the message to the model/user.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::client::{post_search, SearchRequest, SearchResponse};
use crate::commands::fetch_tool::{local_fetch, FetchedPage};
use crate::config::{load, Config};

/// MCP protocol revision served. Kept in lockstep with the client side
/// (`code_mcp.rs`); both speak line-delimited JSON over stdio.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Body cap for `fetch_page` results. Larger than the interactive
/// `libertai fetch` cap (16k) because MCP clients feed the text straight
/// into a model context; `local_fetch` appends a truncation note when hit.
const FETCH_MAX_CHARS: usize = 100_000;

const SEARCH_TYPES: [&str; 4] = ["web", "news", "images", "academic"];

const NO_KEY_HELP: &str = "LibertAI API key not configured — the web_search tool needs one.\n\
\n\
Set it up either way:\n\
  1. Run `libertai login` (browser sign-in; stores the key in ~/.config/libertai/config.toml), or\n\
  2. Set the LIBERTAI_API_KEY environment variable in this MCP server's config\n\
     (create a key at https://console.libertai.io or with `libertai keys create`).\n\
\n\
Docs: https://docs.libertai.io";

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

type SearchFn = dyn Fn(&Config, &SearchRequest<'_>) -> Result<SearchResponse>;
type FetchFn = dyn Fn(&str, usize) -> Result<FetchedPage>;

/// The server's dispatch state. Search/fetch backends and the env-var key
/// are injected so unit tests can drive the full JSON-RPC surface without
/// network or process-global env mutation.
pub struct McpServer {
    cfg: Config,
    env_api_key: Option<String>,
    search: Box<SearchFn>,
    fetch: Box<FetchFn>,
}

impl McpServer {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            env_api_key: std::env::var("LIBERTAI_API_KEY").ok(),
            search: Box::new(post_search),
            fetch: Box::new(local_fetch),
        }
    }

    /// Key resolution order: config.toml (`libertai login`), then the
    /// `LIBERTAI_API_KEY` env var. `None` means the no-key tool error.
    fn resolve_api_key(&self) -> Option<String> {
        self.cfg
            .auth
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| self.env_api_key.clone().filter(|k| !k.trim().is_empty()))
    }

    /// Parse one stdin line and dispatch it. `None` means "write nothing"
    /// (notifications). Malformed JSON yields a -32700 with a null id.
    pub fn handle_line(&self, line: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(line) {
            Ok(msg) => self.handle_message(msg),
            Err(e) => Some(rpc_error(
                Value::Null,
                PARSE_ERROR,
                &format!("parse error: {e}"),
            )),
        }
    }

    /// Dispatch a parsed JSON-RPC message (single or batch).
    pub fn handle_message(&self, msg: Value) -> Option<Value> {
        if let Value::Array(batch) = msg {
            // JSON-RPC 2.0 batch: respond with an array of the non-empty
            // responses; an empty batch is itself an invalid request.
            if batch.is_empty() {
                return Some(rpc_error(Value::Null, INVALID_REQUEST, "empty batch"));
            }
            let responses: Vec<Value> = batch
                .into_iter()
                .filter_map(|m| self.handle_single(m))
                .collect();
            return if responses.is_empty() {
                None
            } else {
                Some(Value::Array(responses))
            };
        }
        self.handle_single(msg)
    }

    fn handle_single(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // No method: either a response to a server-initiated request
            // (we never send any — ignore) or a malformed request.
            return match id {
                Some(id) if !id.is_null() => Some(rpc_error(id, INVALID_REQUEST, "missing method")),
                _ => None,
            };
        };

        // Notifications (no id) get no response. `notifications/initialized`
        // and `notifications/cancelled` are the expected ones; anything else
        // is ignored per JSON-RPC.
        let id = id.filter(|id| !id.is_null())?;

        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            "initialize" => Some(rpc_result(id, self.initialize_result())),
            "ping" => Some(rpc_result(id, json!({}))),
            "tools/list" => Some(rpc_result(id, json!({ "tools": tool_definitions() }))),
            "tools/call" => Some(self.tools_call(id, &params)),
            other => Some(rpc_error(
                id,
                METHOD_NOT_FOUND,
                &format!("method not found: {other}"),
            )),
        }
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "libertai",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "LibertAI web tools: `web_search` queries multiple search \
        engines through LibertAI's privacy-preserving search API and `fetch_page` retrieves a \
        URL as cleaned plain text. Search first, then fetch the most promising results.",
        })
    }

    fn tools_call(&self, id: Value, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match name {
            "web_search" => self.call_web_search(id, &args),
            "fetch_page" => self.call_fetch_page(id, &args),
            other => rpc_error(id, INVALID_PARAMS, &format!("unknown tool: {other}")),
        }
    }

    fn call_web_search(&self, id: Value, args: &Value) -> Value {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => {
                return rpc_error(
                    id,
                    INVALID_PARAMS,
                    "web_search: `query` (non-empty string) is required",
                )
            }
        };
        let search_type = match args.get("search_type") {
            None | Some(Value::Null) => "web".to_string(),
            Some(Value::String(s)) if SEARCH_TYPES.contains(&s.as_str()) => s.clone(),
            Some(other) => {
                return rpc_error(
                    id,
                    INVALID_PARAMS,
                    &format!(
                        "web_search: `search_type` must be one of {} — got {other}",
                        SEARCH_TYPES.join("|")
                    ),
                )
            }
        };
        let engines = match args.get("engines") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                let mut engines = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) => engines.push(s.to_string()),
                        None => {
                            return rpc_error(
                                id,
                                INVALID_PARAMS,
                                "web_search: `engines` must be an array of strings",
                            )
                        }
                    }
                }
                Some(engines)
            }
            Some(_) => {
                return rpc_error(
                    id,
                    INVALID_PARAMS,
                    "web_search: `engines` must be an array of strings",
                )
            }
        };
        let max_results = match args.get("max_results") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                Some(n) => Some(n),
                None => {
                    return rpc_error(
                        id,
                        INVALID_PARAMS,
                        "web_search: `max_results` must be a non-negative integer",
                    )
                }
            },
        };

        let Some(key) = self.resolve_api_key() else {
            return tool_result(id, NO_KEY_HELP.to_string(), true);
        };
        let mut cfg = self.cfg.clone();
        cfg.auth.api_key = Some(key);

        let req = SearchRequest {
            query: &query,
            engines,
            max_results,
            search_type: Some(search_type.clone()),
        };
        match (self.search)(&cfg, &req) {
            Ok(resp) => tool_result(id, render_search(&query, &search_type, &resp), false),
            Err(e) => tool_result(id, format!("web_search failed: {e:#}"), true),
        }
    }

    fn call_fetch_page(&self, id: Value, args: &Value) -> Value {
        let url = match args.get("url").and_then(Value::as_str) {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => {
                return rpc_error(
                    id,
                    INVALID_PARAMS,
                    "fetch_page: `url` (non-empty string) is required",
                )
            }
        };
        match (self.fetch)(&url, FETCH_MAX_CHARS) {
            Ok(page) => tool_result(id, render_page(&page), false),
            Err(e) => tool_result(id, format!("fetch_page failed: {e:#}"), true),
        }
    }
}

/// `libertai mcp` entry point: serve MCP over stdin/stdout until EOF.
pub fn run() -> Result<()> {
    // A broken config must not kill the server — fall back to defaults so
    // initialize/tools/list still work and tool calls explain themselves.
    let cfg = load().unwrap_or_else(|e| {
        eprintln!("libertai mcp: config load failed ({e:#}); using defaults");
        Config::default()
    });
    let server = McpServer::new(cfg);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&server, stdin.lock(), stdout.lock())
}

/// Read line-delimited JSON-RPC frames from `reader`, write responses to
/// `writer`. Returns when the client closes the stream (EOF).
fn serve(server: &McpServer, reader: impl BufRead, mut writer: impl Write) -> Result<()> {
    eprintln!(
        "libertai mcp: serving MCP {PROTOCOL_VERSION} over stdio (tools: web_search, fetch_page)"
    );
    for line in reader.lines() {
        let line = line.context("reading stdin")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(trimmed) {
            let mut frame = serde_json::to_string(&response).context("encoding response")?;
            frame.push('\n');
            writer
                .write_all(frame.as_bytes())
                .context("writing stdout")?;
            writer.flush().context("flushing stdout")?;
        }
    }
    Ok(())
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "web_search",
            "description": "Search the web via LibertAI's privacy-preserving multi-engine \
    search API. Returns titled results with URLs, snippets, and the engines each result was \
    found in (cross-engine hits are a consensus signal). Use for fresh facts, news, images, \
    or academic sources the model wouldn't reliably know.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Free-text search query."
                    },
                    "search_type": {
                        "type": "string",
                        "enum": SEARCH_TYPES,
                        "description": "Result corpus (default: web)."
                    },
                    "engines": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional engine subset (e.g. [\"google\", \"bing\"]). Defaults to the server's mix."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Cap on returned results."
                    }
                },
                "required": ["query"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": true }
        },
        {
            "name": "fetch_page",
            "description": "Fetch a public http(s) URL and return its title, final URL after \
    redirects, and the page body as cleaned plain text (truncated past 100,000 characters, with \
    a note). Use after web_search to read a promising result in full.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http(s) URL to fetch."
                    }
                },
                "required": ["url"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": true }
        }
    ])
}

/// Stable, parseable text rendering of search results:
///
/// ```text
/// 2 results for "rust mcp" (type: web)
///
/// 1. Title
///    URL: https://example.com
///    Snippet text…
///    Engine: google (also found in: bing, duckduckgo)
/// ```
fn render_search(query: &str, search_type: &str, resp: &SearchResponse) -> String {
    if resp.results.is_empty() {
        return format!("No results for \"{query}\" (type: {search_type}).");
    }
    let mut out = format!(
        "{} result{} for \"{query}\" (type: {search_type})\n",
        resp.results.len(),
        if resp.results.len() == 1 { "" } else { "s" },
    );
    for (i, r) in resp.results.iter().enumerate() {
        out.push('\n');
        out.push_str(&format!(
            "{}. {}\n",
            i + 1,
            r.title.as_deref().unwrap_or("(no title)")
        ));
        if let Some(url) = r.url.as_deref().filter(|u| !u.is_empty()) {
            out.push_str(&format!("   URL: {url}\n"));
        }
        if let Some(snippet) = r.snippet.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("   {snippet}\n"));
        }
        if let Some(source) = r.source.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("   Source: {source}\n"));
        }
        if let Some(published) = r.published_at.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("   Published: {published}\n"));
        }
        if let Some(image_url) = r.image_url.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("   Image: {image_url}\n"));
        }
        if let Some(engine) = r.engine.as_deref().filter(|s| !s.is_empty()) {
            let also: Vec<&str> = r
                .found_in
                .iter()
                .map(String::as_str)
                .filter(|e| *e != engine)
                .collect();
            if also.is_empty() {
                out.push_str(&format!("   Engine: {engine}\n"));
            } else {
                out.push_str(&format!(
                    "   Engine: {engine} (also found in: {})\n",
                    also.join(", ")
                ));
            }
        }
    }
    out.trim_end().to_string()
}

fn render_page(page: &FetchedPage) -> String {
    let mut out = format!("{}\nURL: {}\n\n", page.title, page.final_url);
    if page.text.is_empty() {
        out.push_str("(no text content extracted)");
    } else {
        out.push_str(&page.text);
    }
    out
}

// ── JSON-RPC frame helpers ──────────────────────────────────────────────────

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// An MCP tool result. Execution failures (no key, API error, bad URL) are
/// reported here with `isError: true` — never as protocol errors and never
/// as a process exit — so the model can read the message and recover.
fn tool_result(id: Value, text: String, is_error: bool) -> Value {
    rpc_result(
        id,
        json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": is_error,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SearchResult;

    /// Server with stubbed backends: search echoes a canned two-hit
    /// response, fetch echoes a canned page. `key` controls the config key;
    /// the env-var slot stays empty so tests are immune to the host env.
    fn test_server(key: Option<&str>) -> McpServer {
        let mut cfg = Config::default();
        cfg.auth.api_key = key.map(str::to_string);
        McpServer {
            cfg,
            env_api_key: None,
            search: Box::new(|_cfg, req| {
                Ok(SearchResponse {
                    results: vec![
                        SearchResult {
                            title: Some(format!("Hit for {}", req.query)),
                            url: Some("https://one.example".into()),
                            snippet: Some("First snippet".into()),
                            engine: Some("google".into()),
                            rank: Some(1),
                            found_in: vec!["google".into(), "bing".into()],
                            search_type: None,
                            published_at: None,
                            source: None,
                            thumbnail_url: None,
                            image_url: None,
                            width: None,
                            height: None,
                        },
                        SearchResult {
                            title: None,
                            url: Some("https://two.example".into()),
                            snippet: None,
                            engine: None,
                            rank: None,
                            found_in: Vec::new(),
                            search_type: None,
                            published_at: None,
                            source: None,
                            thumbnail_url: None,
                            image_url: None,
                            width: None,
                            height: None,
                        },
                    ],
                    meta: None,
                })
            }),
            fetch: Box::new(|url, _max| {
                Ok(FetchedPage {
                    final_url: url.to_string(),
                    title: "Example Title".into(),
                    text: "Example body text.".into(),
                })
            }),
        }
    }

    fn call(server: &McpServer, raw: &str) -> Value {
        server
            .handle_line(raw)
            .expect("expected a response for a request")
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let server = test_server(Some("LTAI_test"));
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        );
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "libertai");
    }

    #[test]
    fn initialized_notification_gets_no_response() {
        let server = test_server(None);
        assert!(server
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
    }

    #[test]
    fn ping_returns_empty_result() {
        let server = test_server(None);
        let resp = call(&server, r#"{"jsonrpc":"2.0","id":"p1","method":"ping"}"#);
        assert_eq!(resp["id"], "p1");
        assert_eq!(resp["result"], json!({}));
    }

    #[test]
    fn tools_list_exposes_both_tools_with_schemas() {
        let server = test_server(None);
        let resp = call(&server, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["search_type"]["enum"],
            json!(SEARCH_TYPES)
        );
        assert_eq!(tools[1]["name"], "fetch_page");
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["url"]));
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let server = test_server(None);
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
        );
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn malformed_json_is_parse_error() {
        let server = test_server(None);
        let resp = call(&server, "{not json");
        assert_eq!(resp["error"]["code"], PARSE_ERROR);
        assert_eq!(resp["id"], Value::Null);
    }

    #[test]
    fn request_without_method_is_invalid() {
        let server = test_server(None);
        let resp = call(&server, r#"{"jsonrpc":"2.0","id":9}"#);
        assert_eq!(resp["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn web_search_renders_results_with_engine_consensus() {
        let server = test_server(Some("LTAI_test"));
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"web_search","arguments":{"query":"rust mcp","max_results":5}}}"#,
        );
        let result = &resp["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("2 results for \"rust mcp\" (type: web)"),
            "{text}"
        );
        assert!(text.contains("1. Hit for rust mcp"), "{text}");
        assert!(text.contains("URL: https://one.example"), "{text}");
        assert!(text.contains("First snippet"), "{text}");
        assert!(
            text.contains("Engine: google (also found in: bing)"),
            "{text}"
        );
        assert!(text.contains("2. (no title)"), "{text}");
    }

    #[test]
    fn web_search_without_key_is_tool_error_with_setup_help() {
        let server = test_server(None);
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"web_search","arguments":{"query":"anything"}}}"#,
        );
        let result = &resp["result"];
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("libertai login"), "{text}");
        assert!(text.contains("LIBERTAI_API_KEY"), "{text}");
        assert!(text.contains("https://docs.libertai.io"), "{text}");
        // No-key is a *tool* error, never a protocol error.
        assert!(resp.get("error").is_none());
    }

    #[test]
    fn env_api_key_is_a_fallback_for_missing_config_key() {
        let mut server = test_server(None);
        server.env_api_key = Some("LTAI_env".into());
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"web_search","arguments":{"query":"q"}}}"#,
        );
        assert_eq!(resp["result"]["isError"], false);
    }

    #[test]
    fn web_search_rejects_bad_search_type() {
        let server = test_server(Some("LTAI_test"));
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"web_search","arguments":{"query":"q","search_type":"videos"}}}"#,
        );
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn web_search_requires_query() {
        let server = test_server(Some("LTAI_test"));
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"web_search","arguments":{}}}"#,
        );
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn fetch_page_renders_title_url_and_body() {
        let server = test_server(None);
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"fetch_page","arguments":{"url":"https://example.com"}}}"#,
        );
        let result = &resp["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("Example Title\nURL: https://example.com"),
            "{text}"
        );
        assert!(text.contains("Example body text."), "{text}");
    }

    #[test]
    fn fetch_page_failure_is_tool_error() {
        let mut server = test_server(None);
        server.fetch = Box::new(|_url, _max| Err(anyhow::anyhow!("HTTP 404")));
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"fetch_page","arguments":{"url":"https://example.com/missing"}}}"#,
        );
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("HTTP 404"), "{text}");
    }

    #[test]
    fn unknown_tool_is_invalid_params() {
        let server = test_server(None);
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"image_gen","arguments":{}}}"#,
        );
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn batch_requests_yield_batched_responses() {
        let server = test_server(None);
        let resp = call(
            &server,
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#,
        );
        let batch = resp.as_array().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["id"], 1);
        assert_eq!(batch[1]["id"], 2);
    }

    #[test]
    fn serve_loop_writes_one_frame_per_request_and_stops_at_eof() {
        let server = test_server(None);
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
            {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
            \n\
            {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut out = Vec::new();
        serve(&server, &input[..], &mut out).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(lines.len(), 2, "stdout: {lines:?}");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(second["id"], 2);
        assert_eq!(second["result"]["tools"].as_array().unwrap().len(), 2);
    }
}
