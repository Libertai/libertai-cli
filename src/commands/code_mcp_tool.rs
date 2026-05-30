//! Agent-callable MCP bridge for configured terminal `mcpServers`.

use std::sync::Arc;

use async_trait::async_trait;
use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use serde::Deserialize;
use serde_json::json;

use crate::config::{Config, McpPromptConfig, McpResourceConfig, McpToolConfig};

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

#[derive(Debug, Clone, Deserialize)]
struct McpReadResourceInput {
    server: String,
    uri: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpGetPromptInput {
    server: String,
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
    #[serde(default)]
    timeout: Option<u64>,
}

pub struct McpCallTool {
    cfg: Arc<Config>,
}

pub struct McpReadResourceTool {
    cfg: Arc<Config>,
}

pub struct McpGetPromptTool {
    cfg: Arc<Config>,
}

impl McpCallTool {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

impl McpReadResourceTool {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

impl McpGetPromptTool {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

pub struct NamedMcpTool {
    name: String,
    label: String,
    description: String,
    parameters: serde_json::Value,
    cfg: Arc<Config>,
    server: String,
    tool: String,
}

impl NamedMcpTool {
    fn new(cfg: Arc<Config>, server: &str, tool: &McpToolConfig) -> Option<Self> {
        let tool_name = tool.name.trim();
        if tool_name.is_empty() || !tool.enabled {
            return None;
        }
        let name = named_mcp_tool_name(server, tool_name)?;
        let description = if tool.description.trim().is_empty() {
            format!("Call MCP tool `{tool_name}` on configured server `{server}`.")
        } else {
            tool.description.trim().to_string()
        };
        let parameters = tool
            .input_schema
            .clone()
            .filter(|schema| schema.is_object())
            .unwrap_or_else(default_named_tool_schema);
        Some(Self {
            name,
            label: format!("MCP {server}/{tool_name}"),
            description,
            parameters,
            cfg,
            server: server.to_string(),
            tool: tool_name.to_string(),
        })
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

#[async_trait]
impl Tool for McpReadResourceTool {
    fn name(&self) -> &str {
        "mcp_read_resource"
    }

    fn label(&self) -> &str {
        "Read MCP resource"
    }

    fn description(&self) -> &str {
        "Read a cached resource from a configured MCP server. Requires a server name and resource URI."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Configured MCP server name from mcpServers." },
                "uri": { "type": "string", "description": "Resource URI advertised by the MCP server." },
                "timeout": { "type": "integer", "minimum": 1, "maximum": 300 }
            },
            "required": ["server", "uri"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: McpReadResourceInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(e) => return Ok(output(true, format!("invalid `mcp_read_resource` payload: {e}"), None)),
        };
        let server = parsed.server.trim();
        let uri = parsed.uri.trim();
        if server.is_empty() || uri.is_empty() {
            return Ok(output(true, "`mcp_read_resource` requires non-empty `server` and `uri` fields".to_string(), None));
        }
        let run = crate::commands::code_hooks::call_mcp_method_with_config(
            self.cfg.as_ref(),
            server,
            "resources/read",
            json!({ "uri": uri }),
            parsed.timeout,
        );
        Ok(mcp_run_output("resources/read", server, uri, run))
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[async_trait]
impl Tool for McpGetPromptTool {
    fn name(&self) -> &str {
        "mcp_get_prompt"
    }

    fn label(&self) -> &str {
        "Get MCP prompt"
    }

    fn description(&self) -> &str {
        "Get a cached prompt from a configured MCP server. Requires a server name, prompt name, and optional arguments."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Configured MCP server name from mcpServers." },
                "name": { "type": "string", "description": "Prompt name advertised by the MCP server." },
                "arguments": { "type": "object", "description": "Optional prompt arguments." },
                "timeout": { "type": "integer", "minimum": 1, "maximum": 300 }
            },
            "required": ["server", "name"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: McpGetPromptInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(e) => return Ok(output(true, format!("invalid `mcp_get_prompt` payload: {e}"), None)),
        };
        let server = parsed.server.trim();
        let name = parsed.name.trim();
        if server.is_empty() || name.is_empty() {
            return Ok(output(true, "`mcp_get_prompt` requires non-empty `server` and `name` fields".to_string(), None));
        }
        let arguments = parsed
            .arguments
            .as_object()
            .cloned()
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| json!({}));
        let run = crate::commands::code_hooks::call_mcp_method_with_config(
            self.cfg.as_ref(),
            server,
            "prompts/get",
            json!({ "name": name, "arguments": arguments }),
            parsed.timeout,
        );
        Ok(mcp_run_output("prompts/get", server, name, run))
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[async_trait]
impl Tool for NamedMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let run = crate::commands::code_hooks::call_mcp_tool_with_config(
            self.cfg.as_ref(),
            &self.server,
            &self.tool,
            input,
            None,
        );
        let details = json!({
            "operation": "tools/call",
            "server": self.server,
            "tool": self.tool,
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

pub fn named_mcp_tools(cfg: Arc<Config>) -> Vec<Box<dyn Tool>> {
    let mut servers = cfg.mcp_servers.keys().cloned().collect::<Vec<_>>();
    servers.sort();
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for server in servers {
        let Some(server_cfg) = cfg.mcp_servers.get(&server) else {
            continue;
        };
        for tool in &server_cfg.tools {
            if let Some(named) = NamedMcpTool::new(Arc::clone(&cfg), &server, tool) {
                tools.push(Box::new(named));
            }
        }
    }
    tools
}

pub fn cached_mcp_context_tools(cfg: Arc<Config>) -> Vec<Box<dyn Tool>> {
    let has_resources = cfg
        .mcp_servers
        .values()
        .any(|server| server.resources.iter().any(enabled_resource));
    let has_prompts = cfg
        .mcp_servers
        .values()
        .any(|server| server.prompts.iter().any(enabled_prompt));
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    if has_resources {
        tools.push(Box::new(McpReadResourceTool::new(Arc::clone(&cfg))));
    }
    if has_prompts {
        tools.push(Box::new(McpGetPromptTool::new(cfg)));
    }
    tools
}

fn enabled_resource(resource: &McpResourceConfig) -> bool {
    resource.enabled && !resource.uri.trim().is_empty()
}

fn enabled_prompt(prompt: &McpPromptConfig) -> bool {
    prompt.enabled && !prompt.name.trim().is_empty()
}

fn named_mcp_tool_name(server: &str, tool: &str) -> Option<String> {
    let server = sanitize_tool_segment(server);
    let tool = sanitize_tool_segment(tool);
    if server.is_empty() || tool.is_empty() {
        None
    } else {
        Some(format!("mcp__{server}__{tool}"))
    }
}

fn sanitize_tool_segment(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;
    for ch in value.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if ch == '_' || ch == '-' || ch.is_ascii_whitespace() {
            Some('_')
        } else {
            None
        };
        let Some(ch) = next else {
            continue;
        };
        if ch == '_' {
            if last_was_underscore {
                continue;
            }
            last_was_underscore = true;
        } else {
            last_was_underscore = false;
        }
        out.push(ch);
    }
    out.trim_matches('_').to_string()
}

fn default_named_tool_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": true
    })
}

fn output(is_error: bool, text: String, details: Option<serde_json::Value>) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details,
        is_error,
    }
    .into()
}

fn mcp_run_output(
    operation: &str,
    server: &str,
    subject: &str,
    run: crate::commands::code_hooks::McpToolCallRun,
) -> ToolExecution {
    let details = json!({
        "operation": operation,
        "server": server,
        "subject": subject,
        "status": run.status,
        "stdout": run.stdout,
        "stderr": run.stderr,
    });
    if run.status == 0 {
        output(false, run.stdout, Some(details))
    } else {
        let message = if run.stderr.trim().is_empty() {
            run.stdout
        } else {
            run.stderr
        };
        output(true, message, Some(details))
    }
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

    #[test]
    fn named_mcp_tools_register_enabled_cached_tools() {
        let cfg = Arc::new(Config {
            mcp_servers: std::collections::HashMap::from([(
                "GitHub Docs".to_string(),
                crate::config::McpServerConfig {
                    tools: vec![
                        McpToolConfig {
                            name: "search-docs".to_string(),
                            description: "Search docs".to_string(),
                            input_schema: Some(json!({
                                "type": "object",
                                "properties": {
                                    "query": { "type": "string" }
                                },
                                "required": ["query"]
                            })),
                            ..McpToolConfig::default()
                        },
                        McpToolConfig {
                            name: "admin".to_string(),
                            enabled: false,
                            ..McpToolConfig::default()
                        },
                    ],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..Config::default()
        });
        let tools = named_mcp_tools(cfg);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__github_docs__search_docs");
        assert_eq!(tools[0].description(), "Search docs");
        assert_eq!(tools[0].parameters()["required"], json!(["query"]));
        assert!(!tools[0].is_read_only());
    }

    #[test]
    fn cached_mcp_context_tools_register_from_cached_resources_and_prompts() {
        let cfg = Arc::new(Config {
            mcp_servers: std::collections::HashMap::from([(
                "docs".to_string(),
                crate::config::McpServerConfig {
                    resources: vec![
                        McpResourceConfig {
                            uri: "file:///repo/README.md".to_string(),
                            ..McpResourceConfig::default()
                        },
                        McpResourceConfig {
                            uri: "file:///repo/private.md".to_string(),
                            enabled: false,
                            ..McpResourceConfig::default()
                        },
                    ],
                    prompts: vec![McpPromptConfig {
                        name: "summarize".to_string(),
                        ..McpPromptConfig::default()
                    }],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..Config::default()
        });
        let tools = cached_mcp_context_tools(cfg);
        let names = tools.iter().map(|tool| tool.name()).collect::<Vec<_>>();
        assert_eq!(names, vec!["mcp_read_resource", "mcp_get_prompt"]);
        assert!(tools.iter().all(|tool| tool.is_read_only()));
    }

    #[test]
    fn mcp_read_resource_reports_missing_server_as_tool_error() {
        asupersync::test_utils::run_test(|| async {
            let tool = McpReadResourceTool::new(Arc::new(Config::default()));
            let execution = tool
                .execute(
                    "call-1",
                    json!({ "server": "docs", "uri": "file:///repo/README.md" }),
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
            assert!(text.contains("MCP server `docs` is not configured"));
        });
    }

    #[test]
    fn named_mcp_tool_name_sanitizes_segments() {
        assert_eq!(
            named_mcp_tool_name("GitHub Docs", "search-docs").as_deref(),
            Some("mcp__github_docs__search_docs")
        );
        assert_eq!(named_mcp_tool_name("!!!", "search").as_deref(), None);
    }
}
