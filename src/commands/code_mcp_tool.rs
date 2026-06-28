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
            Err(e) => {
                return Ok(output(
                    true,
                    format!("invalid `mcp_call` payload: {e}"),
                    None,
                ))
            }
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
        let details = mcp_run_details("tools/call", server, "tool", tool, &run);
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
            Err(e) => {
                return Ok(output(
                    true,
                    format!("invalid `mcp_read_resource` payload: {e}"),
                    None,
                ))
            }
        };
        let server = parsed.server.trim();
        let uri = parsed.uri.trim();
        if server.is_empty() || uri.is_empty() {
            return Ok(output(
                true,
                "`mcp_read_resource` requires non-empty `server` and `uri` fields".to_string(),
                None,
            ));
        }
        let run = crate::commands::code_hooks::call_mcp_method_with_config(
            self.cfg.as_ref(),
            server,
            "resources/read",
            json!({ "uri": uri }),
            parsed.timeout,
        );
        Ok(mcp_run_output("resources/read", server, "uri", uri, run))
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
            Err(e) => {
                return Ok(output(
                    true,
                    format!("invalid `mcp_get_prompt` payload: {e}"),
                    None,
                ))
            }
        };
        let server = parsed.server.trim();
        let name = parsed.name.trim();
        if server.is_empty() || name.is_empty() {
            return Ok(output(
                true,
                "`mcp_get_prompt` requires non-empty `server` and `name` fields".to_string(),
                None,
            ));
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
        Ok(mcp_run_output("prompts/get", server, "prompt", name, run))
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
        let details = mcp_run_details("tools/call", &self.server, "tool", &self.tool, &run);
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

/// (M5/#11) Metadata for one enabled MCP tool, as `tool_search` surfaces
/// it. Mirrors the fields `NamedMcpTool` would have registered eagerly —
/// the tool's `mcp__server__tool` name, its server, its raw tool name
/// (for `mcp_call`), and its description. Built from the same
/// `NamedMcpTool::new` gate (enabled + non-empty name + sanitizable) so
/// the search results and the eager registry never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolMetadata {
    pub server: String,
    pub tool: String,
    pub qualified_name: String,
    pub description: String,
}

/// (M5/#11) All enabled MCP tools across configured servers, in stable
/// (server-sorted, then config) order — the catalog `tool_search`
/// matches against. Reuses [`NamedMcpTool::new`] gating so a tool only
/// appears here iff it would have been registered eagerly.
pub fn mcp_tool_metadata(cfg: &Config) -> Vec<McpToolMetadata> {
    let mut servers = cfg.mcp_servers.keys().cloned().collect::<Vec<_>>();
    servers.sort();
    let mut out = Vec::new();
    for server in servers {
        let Some(server_cfg) = cfg.mcp_servers.get(&server) else {
            continue;
        };
        for tool in &server_cfg.tools {
            // Mirror NamedMcpTool::new's gating exactly so the search
            // catalog and the eager registry agree on which tools exist:
            // non-empty name + enabled + sanitizable into `mcp__server__tool`.
            let tool_name = tool.name.trim();
            if tool_name.is_empty() || !tool.enabled {
                continue;
            }
            let Some(qualified) = named_mcp_tool_name(&server, tool_name) else {
                continue;
            };
            let description = if tool.description.trim().is_empty() {
                format!("Call MCP tool `{tool_name}` on configured server `{server}`.")
            } else {
                tool.description.trim().to_string()
            };
            out.push(McpToolMetadata {
                server: server.clone(),
                tool: tool_name.to_string(),
                qualified_name: qualified,
                description,
            });
        }
    }
    out
}

/// (M5/#11) Above this many enabled MCP tools, the factory defers the
/// eager `mcp__server__tool` wrappers and registers `mcp_call` +
/// `tool_search` instead — the named wrappers' definitions would bloat
/// the system prompt more than the model uses them. Overridable via the
/// env var so tests can pin it.
pub const DEFAULT_MCP_TOOL_SEARCH_THRESHOLD: usize = 20;

/// (M5/#11) Threshold above which named MCP tools are deferred to
/// `tool_search`. Reads `LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD`; falls
/// back to [`DEFAULT_MCP_TOOL_SEARCH_THRESHOLD`]. A value of `0`
/// means "always defer" (even a single tool); a very large value means
/// "never defer" (the legacy eager behavior).
pub fn mcp_tool_search_threshold() -> usize {
    // Empty/invalid env falls back to the default (so a stray
    // `LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD=` doesn't silently defer
    // everything). A valid 0 means "always defer".
    match std::env::var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD") {
        Ok(raw) if !raw.trim().is_empty() => {
            raw.trim().parse::<usize>().unwrap_or(DEFAULT_MCP_TOOL_SEARCH_THRESHOLD)
        }
        _ => DEFAULT_MCP_TOOL_SEARCH_THRESHOLD,
    }
}

/// (M5/#11) True when the eager `named_mcp_tools` wrappers should be
/// deferred in favor of `tool_search` + `mcp_call` — i.e. when the
/// enabled-tool count exceeds the threshold. The factory calls this to
/// decide whether to push the named wrappers or the search tool.
pub fn should_defer_mcp_tools(cfg: &Config) -> bool {
    let count = mcp_tool_metadata(cfg).len();
    count > mcp_tool_search_threshold()
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

/// (M5/#11) Serializes the two threshold-env tests: one sets the env to
/// various values, the other removes it / sets invalid values — both
/// mutate the process-global `LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD` env
/// var, so without this lock they race and the "falls back" test can
/// observe a value the "respects" test just set.
#[cfg(test)]
pub(crate) static MCP_THRESHOLD_TEST_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

fn default_named_tool_schema() -> serde_json::Value {
    json!({        "type": "object",
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
    subject_key: &str,
    subject: &str,
    run: crate::commands::code_hooks::McpToolCallRun,
) -> ToolExecution {
    let details = mcp_run_details(operation, server, subject_key, subject, &run);
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

fn mcp_run_details(
    operation: &str,
    server: &str,
    subject_key: &str,
    subject: &str,
    run: &crate::commands::code_hooks::McpToolCallRun,
) -> serde_json::Value {
    let mut details = serde_json::Map::new();
    details.insert("kind".to_string(), json!("mcp_call_diagnostics"));
    details.insert("operation".to_string(), json!(operation));
    details.insert(
        "status".to_string(),
        json!(if run.status == 0 { "ok" } else { "error" }),
    );
    details.insert("server".to_string(), json!(server));
    details.insert(subject_key.to_string(), json!(subject));
    if !run.transport.trim().is_empty() {
        details.insert("transport".to_string(), json!(run.transport));
    }
    details.insert("timeoutMs".to_string(), json!(run.timeout_ms));
    details.insert("elapsedMs".to_string(), json!(run.elapsed_ms));
    details.insert("stdout".to_string(), json!(run.stdout));
    details.insert("stderr".to_string(), json!(run.stderr));
    if let Some(raw) = &run.raw {
        details.insert("raw".to_string(), raw.clone());
    }
    serde_json::Value::Object(details)
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
            assert!(text.contains("MCP server `github` is not configured"));
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
    fn mcp_run_details_match_desktop_diagnostics_shape() {
        let run = crate::commands::code_hooks::McpToolCallRun {
            status: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            transport: "stdio".to_string(),
            timeout_ms: 30_000,
            elapsed_ms: 12,
            raw: Some(json!({"content":[{"type":"text","text":"ok"}]})),
        };
        let details = mcp_run_details("tools/call", "docs", "tool", "search", &run);
        assert_eq!(details["kind"], json!("mcp_call_diagnostics"));
        assert_eq!(details["operation"], json!("tools/call"));
        assert_eq!(details["status"], json!("ok"));
        assert_eq!(details["server"], json!("docs"));
        assert_eq!(details["tool"], json!("search"));
        assert_eq!(details["transport"], json!("stdio"));
        assert_eq!(details["timeoutMs"], json!(30_000));
        assert_eq!(details["elapsedMs"], json!(12));
        assert_eq!(details["raw"]["content"][0]["text"], json!("ok"));
    }

    #[test]
    fn named_mcp_tool_name_sanitizes_segments() {
        assert_eq!(
            named_mcp_tool_name("GitHub Docs", "search-docs").as_deref(),
            Some("mcp__github_docs__search_docs")
        );
        assert_eq!(named_mcp_tool_name("!!!", "search").as_deref(), None);
    }

    // ---- (M5/#11) tool_search gating ----

    fn server_with_tools(count: usize) -> crate::config::McpServerConfig {
        let tools = (0..count)
            .map(|i| McpToolConfig {
                name: format!("tool_{i}"),
                description: format!("Tool number {i}"),
                ..McpToolConfig::default()
            })
            .collect();
        crate::config::McpServerConfig {
            tools,
            ..crate::config::McpServerConfig::default()
        }
    }

    #[test]
    fn mcp_tool_metadata_lists_enabled_tools_in_server_order() {
        let cfg = Config {
            mcp_servers: std::collections::HashMap::from([
                (
                    "zebra".to_string(),
                    crate::config::McpServerConfig {
                        tools: vec![McpToolConfig {
                            name: "z1".to_string(),
                            ..McpToolConfig::default()
                        }],
                        ..crate::config::McpServerConfig::default()
                    },
                ),
                (
                    "alpha".to_string(),
                    crate::config::McpServerConfig {
                        tools: vec![
                            McpToolConfig {
                                name: "a1".to_string(),
                                ..McpToolConfig::default()
                            },
                            McpToolConfig {
                                name: "disabled".to_string(),
                                enabled: false,
                                ..McpToolConfig::default()
                            },
                        ],
                        ..crate::config::McpServerConfig::default()
                    },
                ),
            ]),
            ..Config::default()
        };
        let meta = mcp_tool_metadata(&cfg);
        // Servers sorted alpha-first; disabled tool excluded.
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].server, "alpha");
        assert_eq!(meta[0].tool, "a1");
        assert_eq!(meta[0].qualified_name, "mcp__alpha__a1");
        assert_eq!(meta[1].server, "zebra");
        assert_eq!(meta[1].tool, "z1");
        // Disabled tool is absent.
        assert!(meta.iter().all(|m| m.tool != "disabled"));
    }

    #[test]
    fn mcp_tool_metadata_synthesizes_description_when_missing() {
        let cfg = Config {
            mcp_servers: std::collections::HashMap::from([(
                "github".to_string(),
                crate::config::McpServerConfig {
                    tools: vec![McpToolConfig {
                        name: "create_issue".to_string(),
                        description: String::new(),
                        ..McpToolConfig::default()
                    }],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..Config::default()
        };
        let meta = mcp_tool_metadata(&cfg);
        assert_eq!(meta.len(), 1);
        assert!(meta[0].description.contains("create_issue"));
        assert!(meta[0].description.contains("github"));
    }

    #[test]
    fn should_defer_mcp_tools_respects_threshold_env() {
        // Process-global env: set it for this test, restore on drop. Use a
        // unique value so a concurrent sibling setting a different one
        // still makes this assertion deterministic about THIS config's
        // count vs. the threshold we just set. Held under the threshold
        // lock so the parallel `falls_back` test can't flip the env mid-run.
        let _lock = super::MCP_THRESHOLD_TEST_LOCK
            .lock()
            .expect("mcp threshold test lock");
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                std::env::remove_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD");
            }
        }
        let _guard = EnvGuard;

        // 5 enabled tools.
        let cfg = Config {
            mcp_servers: std::collections::HashMap::from([(
                "srv".to_string(),
                server_with_tools(5),
            )]),
            ..Config::default()
        };

        // Threshold 10 → 5 tools do NOT defer (legacy eager behavior).
        std::env::set_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD", "10");
        assert!(!should_defer_mcp_tools(&cfg), "5 ≤ 10 → no defer");

        // Threshold 0 → always defer (even the 5 tools).
        std::env::set_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD", "0");
        assert!(should_defer_mcp_tools(&cfg), "5 > 0 → defer");

        // Threshold 5 → exactly-equal does NOT defer (strict >).
        std::env::set_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD", "5");
        assert!(!should_defer_mcp_tools(&cfg), "5 > 5 is false → no defer");

        // Threshold 4 → defer (5 > 4).
        std::env::set_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD", "4");
        assert!(should_defer_mcp_tools(&cfg), "5 > 4 → defer");
    }

    #[test]
    fn mcp_tool_search_threshold_falls_back_on_invalid_env() {
        let _lock = super::MCP_THRESHOLD_TEST_LOCK
            .lock()
            .expect("mcp threshold test lock");
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                std::env::remove_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD");
            }
        }
        let _guard = EnvGuard;

        std::env::set_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD", "not-a-number");
        assert_eq!(mcp_tool_search_threshold(), DEFAULT_MCP_TOOL_SEARCH_THRESHOLD);
        std::env::set_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD", "");
        assert_eq!(mcp_tool_search_threshold(), DEFAULT_MCP_TOOL_SEARCH_THRESHOLD);
        std::env::remove_var("LIBERTAI_MCP_TOOL_SEARCH_THRESHOLD");
        assert_eq!(mcp_tool_search_threshold(), DEFAULT_MCP_TOOL_SEARCH_THRESHOLD);
    }
}
