//! The `tool_search` tool — defer-loading for MCP tools (M5/#11).
//!
//! When a session configures many MCP servers/tools, eagerly registering
//! every `mcp__server__tool` wrapper bloats the system prompt with tool
//! definitions the model mostly won't use. Above
//! [`DEFAULT_MCP_TOOL_SEARCH_THRESHOLD`] enabled tools, the factory
//! instead registers only the generic `mcp_call` bridge + this `tool_search`
//! tool. The model searches for the right tool by query, then calls it via
//! `mcp_call(server, tool, arguments)` — same capability, far fewer tool
//! definitions in the prompt.
//!
//! Read-only (no writes, no MCP calls itself — it only reads the
//! configured tool catalog), so the factory registers it unwrapped, like
//! `todo`/`skill`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_mcp_tool::{mcp_tool_metadata, McpToolMetadata};
use crate::config::Config;

const NAME: &str = "tool_search";
const LABEL: &str = "Search MCP tools";

const DESCRIPTION: &str = concat!(
    "Find MCP tools by keyword across all configured `mcpServers` when the named ",
    "`mcp__server__tool` wrappers are not registered (sessions with many MCP tools ",
    "defer them). Pass a `query` (tool name, server name, or capability keyword); ",
    "returns matching tools as `mcp__server__tool` names + descriptions + the ",
    "`mcp_call(server, tool, arguments)` invocation to use. Call `mcp_call` to invoke ",
    "a matched tool."
);

const DEFAULT_LIMIT: usize = 20;
const HARD_LIMIT: usize = 50;

#[derive(Debug, Clone, Deserialize)]
struct ToolSearchInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ToolSearchTool {
    cfg: Arc<Config>,
}

impl ToolSearchTool {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
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
                "query": {
                    "type": "string",
                    "description": "Keywords to match against MCP tool names, server names, and descriptions."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HARD_LIMIT as i64,
                    "description": "Max matches to return (default 20, capped 50)."
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
    ) -> PiResult<ToolExecution> {
        let parsed: ToolSearchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!("invalid `tool_search` payload: {e}")));
            }
        };
        let query = parsed.query.trim();
        if query.is_empty() {
            return Ok(err_output("`tool_search` requires a non-empty `query`"));
        }
        let limit = parsed.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, HARD_LIMIT);
        let metadata = mcp_tool_metadata(self.cfg.as_ref());
        let matches = search_mcp_tools(&metadata, query, limit);
        Ok(render_matches(query, &matches, &metadata))
    }

    fn is_read_only(&self) -> bool {
        // Reads the configured tool catalog only; no MCP calls, no writes.
        true
    }
}

/// (M5/#11) Pure substring matcher over the MCP tool catalog. Case-
/// insensitive; a tool matches when the query (split on whitespace into
/// terms) has ANY term that appears in the tool's qualified name, server
/// name, raw tool name, or description. Ranked so tools with more term
/// matches + matches in the name (over description-only) sort first.
/// Pure so the ranking is unit-testable without a config.
pub fn search_mcp_tools<'a>(
    metadata: &'a [McpToolMetadata],
    query: &str,
    limit: usize,
) -> Vec<&'a McpToolMetadata> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, usize, &McpToolMetadata)> = metadata
        .iter()
        .filter_map(|m| {
            let hay_name = format!("{} {}", m.qualified_name, m.tool).to_ascii_lowercase();
            let hay_full =
                format!("{} {} {}", m.server, m.qualified_name, m.description).to_ascii_lowercase();
            // Term matches in the name (qualified + raw) count double so a
            // name hit outranks a description-only hit.
            let mut name_hits = 0usize;
            let mut total_hits = 0usize;
            for term in &terms {
                let t = term.as_str();
                let in_name = hay_name.contains(t);
                let in_full = hay_full.contains(t);
                if in_name {
                    name_hits += 1;
                }
                if in_name || in_full {
                    total_hits += 1;
                }
            }
            if total_hits == 0 {
                None
            } else {
                // Rank: more total hits first, then more name hits.
                Some((total_hits, name_hits, m))
            }
        })
        .collect();
    // Stable sort by (total_hits desc, name_hits desc) — preserve catalog
    // order (server-sorted) among ties so results are deterministic.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.server.cmp(&b.2.server))
            .then_with(|| a.2.tool.cmp(&b.2.tool))
    });
    scored.into_iter().take(limit).map(|(_, _, m)| m).collect()
}

/// Render the matched tools as a text block the model can act on. When
/// there are no matches, return an `is_error` result listing the total
/// tool count + a hint to broaden the query — drives a retry.
fn render_matches(
    query: &str,
    matches: &[&McpToolMetadata],
    all: &[McpToolMetadata],
) -> ToolExecution {
    if matches.is_empty() {
        let msg = format!(
            "No MCP tools matched `{query}`. {} configured tool(s) available; try a broader query or a server name.",
            all.len()
        );
        return ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(msg))],
            details: None,
            is_error: true,
        }
        .into();
    }
    let mut text = format!(
        "Found {} MCP tool(s) matching `{query}`:\n\n",
        matches.len()
    );
    for m in matches {
        text.push_str(&format!(
            "- `{}` — {}\n  call `mcp_call` with server=`{}`, tool=`{}`\n",
            m.qualified_name, m.description, m.server, m.tool
        ));
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
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
    use crate::commands::code_mcp_tool::McpToolMetadata;

    fn md(server: &str, tool: &str, desc: &str) -> McpToolMetadata {
        McpToolMetadata {
            server: server.to_string(),
            tool: tool.to_string(),
            qualified_name: format!("mcp__{server}__{tool}"),
            description: desc.to_string(),
        }
    }

    fn catalog() -> Vec<McpToolMetadata> {
        vec![
            md("github", "create_issue", "Open a new GitHub issue."),
            md("github", "list_issues", "List issues in a repo."),
            md(
                "slack",
                "post_message",
                "Post a message to a Slack channel.",
            ),
            md("slack", "search_messages", "Search Slack message history."),
            md("linear", "create_issue", "Create a Linear issue."),
        ]
    }

    #[test]
    fn matches_by_tool_name_substring_case_insensitive() {
        let cat = catalog();
        let hits = search_mcp_tools(&cat, "issue", 10);
        // Both github/create_issue, github/list_issues, linear/create_issue
        // contain "issue" in the name; list_issues matches in the name too.
        let names: Vec<&str> = hits.iter().map(|m| m.qualified_name.as_str()).collect();
        assert!(names.contains(&"mcp__github__create_issue"));
        assert!(names.contains(&"mcp__github__list_issues"));
        assert!(names.contains(&"mcp__linear__create_issue"));
        assert!(!names.contains(&"mcp__slack__post_message"));
    }

    #[test]
    fn matches_by_description_when_name_misses() {
        let cat = catalog();
        let hits = search_mcp_tools(&cat, "Slack channel", 10);
        let names: Vec<&str> = hits.iter().map(|m| m.qualified_name.as_str()).collect();
        assert!(names.contains(&"mcp__slack__post_message"));
    }

    #[test]
    fn name_hits_outrank_description_only_hits() {
        let cat = catalog();
        // "issue" appears in github/create_issue (name) and in
        // linear/create_issue (name). "search" appears in slack/
        // search_messages (name). Query "issue" should rank the two
        // name-hits above any description-only hit.
        let hits = search_mcp_tools(&cat, "issue", 10);
        assert!(!hits.is_empty());
        // Every returned hit has "issue" in either name or description.
        for m in &hits {
            let hay =
                format!("{} {} {}", m.server, m.qualified_name, m.description).to_ascii_lowercase();
            assert!(hay.contains("issue"), "irrelevant hit: {:?}", m);
        }
    }

    #[test]
    fn multi_term_query_requires_at_least_one_term_match() {
        let cat = catalog();
        // "github" matches server; "nonexistent" matches nothing — any
        // term matching is enough (OR semantics).
        let hits = search_mcp_tools(&cat, "github nonexistent", 10);
        let names: Vec<&str> = hits.iter().map(|m| m.qualified_name.as_str()).collect();
        assert!(names.contains(&"mcp__github__create_issue"));
        assert!(names.contains(&"mcp__github__list_issues"));
    }

    #[test]
    fn limit_caps_results() {
        let cat = catalog();
        let hits = search_mcp_tools(&cat, "issue", 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn empty_query_returns_nothing() {
        let cat = catalog();
        assert!(search_mcp_tools(&cat, "   ", 10).is_empty());
    }

    #[test]
    fn no_matches_is_error_with_count() {
        let cat = catalog();
        let out = render_matches("zzz", &[], &cat);
        let ToolExecution::Done(tool_output) = out else {
            panic!("expected Done");
        };
        assert!(tool_output.is_error);
        let text = tool_output
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("No MCP tools matched `zzz`"));
        assert!(text.contains("5 configured tool(s)"));
    }

    #[test]
    fn matches_render_with_invocation_hint() {
        let cat = catalog();
        let hits = search_mcp_tools(&cat, "post_message", 10);
        let out = render_matches("post_message", &hits, &cat);
        let ToolExecution::Done(tool_output) = out else {
            panic!("expected Done");
        };
        assert!(!tool_output.is_error);
        let text = tool_output
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("mcp__slack__post_message"));
        assert!(text.contains("server=`slack`"));
        assert!(text.contains("tool=`post_message`"));
        assert!(text.contains("mcp_call"));
    }
}
