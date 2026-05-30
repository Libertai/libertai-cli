//! Agent-callable MCP bridge for configured terminal `mcpServers`.

use std::sync::Arc;

use async_trait::async_trait;
use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use serde::Deserialize;
use serde_json::json;

use crate::config::Config;

const NAME: &str = "mcp_call";
const LABEL: &str = "Call MCP tool";
const DESCRIPTION: &str = "Call a tool exposed by a configured MCP server from \
`mcpServers`. Use this when the user asks for work that depends on an external \
MCP integration. Requires a server name, tool name, and JSON arguments.";

#[derive(Debug, Clone, Deserialize)]
struct McpCallInput {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: serde_json::Value,
    #[serde(default)]
    timeout: Option<u64>,
}

pub struct McpCallTool {
    cfg: Arc<Config>,
}

impl McpCallTool {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for McpCallTool {
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
                "server": {
                    "type": "string",
                    "description": "Configured MCP server name from mcpServers."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name exposed by the MCP server."
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON arguments to pass to the MCP tool."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 300,
                    "description": "Optional call timeout in seconds."
                }
            },
            "required": ["server", "tool"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: McpCallInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(e) => return Ok(output(true, format!("invalid `mcp_call` payload: {e}"), None)),
        };
        let server = parsed.server.trim();
        let tool = parsed.tool.trim();
        if server.is_empty() || tool.is_empty() {
            return Ok(output(
                true,
                "`mcp_call` requires non-empty `server` and `tool` fields".to_string(),
                None,
            ));
        }
        let timeout = parsed.timeout.filter(|secs| *secs > 0);
        let run = crate::commands::code_hooks::call_mcp_tool_with_config(
            self.cfg.as_ref(),
            server,
            tool,
            parsed.arguments,
            timeout,
        );
        let details = json!({
            "operation": "tools/call",
            "server": server,
            "tool": tool,
            "status": run.status,
            "stdout": run.stdout,
            "stderr": run.stderr,
        });
        if run.status == 0 {
            Ok(output(false, run.stdout, Some(details)))
        } else {
            let message = if run.stderr.trim().is_empty() {
                run.stdout
            } else {
                run.stderr
            };
            Ok(output(true, message, Some(details)))
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

fn output(is_error: bool, text: String, details: Option<serde_json::Value>) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details,
        is_error,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::sdk::Tool;

    #[test]
    fn mcp_call_tool_schema_requires_server_and_tool() {
        let cfg = Arc::new(Config::default());
        let tool = McpCallTool::new(cfg);
        assert_eq!(tool.name(), "mcp_call");
        assert_eq!(tool.parameters()["required"], json!(["server", "tool"]));
        assert!(!tool.is_read_only());
    }

    #[test]
    fn mcp_call_tool_reports_missing_server_as_tool_error() {
        asupersync::test_utils::run_test(|| async {
            let cfg = Arc::new(Config::default());
            let tool = McpCallTool::new(cfg);
            let execution = tool
                .execute(
                    "call-1",
                    json!({
                        "server": "github",
                        "tool": "search",
                        "arguments": { "query": "rust" }
                    }),
                    None,
                )
                .await
                .unwrap();
            let pi::sdk::ToolExecution::Done(output) = execution else {
                panic!("expected done output");
            };
            assert!(output.is_error);
            let text = output
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("MCP hook server `github` is not configured"));
        });
    }
}
