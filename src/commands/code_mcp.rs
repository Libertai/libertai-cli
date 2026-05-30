//! Minimal terminal MCP probing for configured `mcpServers`.
//!
//! This is deliberately a short-lived probe path, not the live tool registry.
//! It proves CLI-side stdio and Streamable HTTP discovery against the same
//! config shape used by hooks and gives `/mcp probe` useful diagnostics.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;

use crate::config::{Config, McpServerConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProbeReport {
    pub servers: Vec<McpServerProbe>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerProbe {
    pub name: String,
    pub transport: String,
    pub status: McpProbeStatus,
    pub tools: Vec<String>,
    pub resources: Vec<String>,
    pub prompts: Vec<String>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProbeStatus {
    Ok,
    Warning,
    Error,
}

impl McpProbeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

pub fn probe_configured_servers(cfg: &Config, timeout: Duration) -> McpProbeReport {
    let mut servers = Vec::new();
    let mut names = cfg.mcp_servers.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let server = &cfg.mcp_servers[&name];
        servers.push(probe_server(&name, server, timeout));
    }
    McpProbeReport { servers }
}

fn probe_server(name: &str, server: &McpServerConfig, timeout: Duration) -> McpServerProbe {
    let transport = mcp_transport_label(server);
    let result = if !server.url.trim().is_empty() {
        if server.transport.trim().eq_ignore_ascii_case("sse") {
            Err("legacy SSE probing is not implemented in the terminal CLI yet".to_string())
        } else {
            probe_http_server(server, timeout)
        }
    } else if !server.command.trim().is_empty() {
        probe_stdio_server(server, timeout)
    } else {
        Err("server has no command or url".to_string())
    };
    match result {
        Ok(mut inventory) => {
            let status = if inventory.diagnostics.is_empty() {
                McpProbeStatus::Ok
            } else {
                McpProbeStatus::Warning
            };
            inventory.tools.sort();
            inventory.resources.sort();
            inventory.prompts.sort();
            McpServerProbe {
                name: name.to_string(),
                transport,
                status,
                tools: inventory.tools,
                resources: inventory.resources,
                prompts: inventory.prompts,
                diagnostics: inventory.diagnostics,
            }
        }
        Err(error) => McpServerProbe {
            name: name.to_string(),
            transport,
            status: McpProbeStatus::Error,
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            diagnostics: vec![error],
        },
    }
}

fn mcp_transport_label(server: &McpServerConfig) -> String {
    if !server.url.trim().is_empty() {
        if server.transport.trim().eq_ignore_ascii_case("sse") {
            "legacy-sse".to_string()
        } else {
            "streamable-http".to_string()
        }
    } else {
        "stdio".to_string()
    }
}

#[derive(Debug, Default)]
struct McpInventory {
    tools: Vec<String>,
    resources: Vec<String>,
    prompts: Vec<String>,
    diagnostics: Vec<String>,
}

fn probe_stdio_server(
    server: &McpServerConfig,
    timeout: Duration,
) -> Result<McpInventory, String> {
    let mut cmd = Command::new(server.command.trim());
    cmd.args(&server.args)
        .envs(&server.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn stdio server: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "stdio server did not expose stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdio server did not expose stdout".to_string())?;
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = tx.send(trimmed.to_string());
                    }
                }
                Err(_) => break,
            }
        }
    });

    let result = probe_stdio_inventory(&mut stdin, &rx, timeout);
    cleanup_stdio_probe(stdin, child, stderr, reader, result)
}

fn probe_stdio_inventory(
    stdin: &mut ChildStdin,
    rx: &mpsc::Receiver<String>,
    timeout: Duration,
) -> Result<McpInventory, String> {
    write_mcp_message(stdin, &initialize_request(1))
        .map_err(|e| format!("writing initialize request: {e}"))?;
    let init = wait_for_stdio_response(rx, 1, timeout)?;
    mcp_response_result(init).map_err(|e| format!("initialize failed: {e}"))?;
    write_mcp_message(stdin, &initialized_notification())
        .map_err(|e| format!("writing initialized notification: {e}"))?;

    let mut inventory = McpInventory::default();
    for request in [
        ("tools/list", 2_u64, "tools"),
        ("resources/list", 3_u64, "resources"),
        ("prompts/list", 4_u64, "prompts"),
    ] {
        let (method, id, key) = request;
        write_mcp_message(stdin, &list_request(id, method))
            .map_err(|e| format!("writing {method} request: {e}"))?;
        match wait_for_stdio_response(rx, id, timeout).and_then(mcp_response_result) {
            Ok(result) => inventory.extend(key, &result),
            Err(e) => inventory.diagnostics.push(format!("{method}: {e}")),
        }
    }
    Ok(inventory)
}

fn cleanup_stdio_probe(
    mut stdin: ChildStdin,
    mut child: Child,
    stderr: Option<ChildStderr>,
    reader: thread::JoinHandle<()>,
    result: Result<McpInventory, String>,
) -> Result<McpInventory, String> {
    let _ = stdin.flush();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let stderr_text = stderr
        .map(|stderr| {
            let mut reader = BufReader::new(stderr);
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            text.trim().to_string()
        })
        .unwrap_or_default();
    let _ = reader.join();
    match result {
        Ok(mut inventory) => {
            if !stderr_text.is_empty() {
                inventory.diagnostics.push(format!("stderr: {stderr_text}"));
            }
            Ok(inventory)
        }
        Err(e) if stderr_text.is_empty() => Err(e),
        Err(e) => Err(format!("{e}; stderr: {stderr_text}")),
    }
}

fn probe_http_server(
    server: &McpServerConfig,
    timeout: Duration,
) -> Result<McpInventory, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let url = server.url.trim();
    let (init, session_id) =
        post_mcp_http_message(&client, server, url, &initialize_request(1), None, 1)?;
    mcp_response_result(init).map_err(|e| format!("initialize failed: {e}"))?;
    post_mcp_http_notification(
        &client,
        server,
        url,
        &initialized_notification(),
        session_id.as_deref(),
    )?;
    let mut inventory = McpInventory::default();
    for request in [
        ("tools/list", 2_u64, "tools"),
        ("resources/list", 3_u64, "resources"),
        ("prompts/list", 4_u64, "prompts"),
    ] {
        let (method, id, key) = request;
        match post_mcp_http_message(
            &client,
            server,
            url,
            &list_request(id, method),
            session_id.as_deref(),
            id,
        )
        .and_then(|(response, _)| mcp_response_result(response))
        {
            Ok(result) => inventory.extend(key, &result),
            Err(e) => inventory.diagnostics.push(format!("{method}: {e}")),
        }
    }
    Ok(inventory)
}

impl McpInventory {
    fn extend(&mut self, key: &str, result: &serde_json::Value) {
        let Some(items) = result.get(key).and_then(serde_json::Value::as_array) else {
            return;
        };
        let names = items.iter().filter_map(mcp_inventory_item_label);
        match key {
            "tools" => self.tools.extend(names),
            "resources" => self.resources.extend(names),
            "prompts" => self.prompts.extend(names),
            _ => {}
        }
    }
}

fn mcp_inventory_item_label(value: &serde_json::Value) -> Option<String> {
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("uri").and_then(serde_json::Value::as_str))
        .map(str::to_string)
}

fn initialize_request(id: u64) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "libertai-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    })
}

fn initialized_notification() -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    })
}

fn list_request(id: u64, method: &str) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {},
    })
}

fn write_mcp_message(stdin: &mut impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

fn wait_for_stdio_response(
    rx: &mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for response id {id}"));
        }
        let line = rx
            .recv_timeout(remaining)
            .map_err(|_| format!("timed out waiting for response id {id}"))?;
        let value = serde_json::from_str::<serde_json::Value>(&line)
            .map_err(|e| format!("invalid JSON-RPC message from MCP server: {e}"))?;
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
}

fn post_mcp_http_message(
    client: &reqwest::blocking::Client,
    server: &McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
    id: u64,
) -> Result<(serde_json::Value, Option<String>), String> {
    let response = send_mcp_http_request(client, server, url, message, session_id)
        .map_err(|e| e.to_string())?;
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| session_id.map(str::to_string));
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", body.trim()));
    }
    let value = if content_type.contains("text/event-stream") {
        parse_mcp_sse_response(&body, id)?
    } else if body.trim().is_empty() {
        return Err("empty MCP HTTP response".to_string());
    } else {
        serde_json::from_str::<serde_json::Value>(body.trim())
            .map_err(|e| format!("invalid MCP HTTP JSON response: {e}"))?
    };
    Ok((value, session_id))
}

fn post_mcp_http_notification(
    client: &reqwest::blocking::Client,
    server: &McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
) -> Result<(), String> {
    let response = send_mcp_http_request(client, server, url, message, session_id)
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().map_err(|e| e.to_string())?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}: {}", body.trim()))
    }
}

fn send_mcp_http_request(
    client: &reqwest::blocking::Client,
    server: &McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
) -> reqwest::Result<reqwest::blocking::Response> {
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-03-26")
        .json(message);
    if let Some(session_id) = session_id {
        request = request.header("mcp-session-id", session_id);
    }
    for (name, value) in &server.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request.send()
}

fn parse_mcp_sse_response(body: &str, id: u64) -> Result<serde_json::Value, String> {
    let mut data_lines = Vec::new();
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }
    for data in data_lines {
        if data == "[DONE]" {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(|e| format!("invalid MCP HTTP SSE data: {e}"))?;
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
    Err(format!("missing MCP HTTP SSE response id {id}"))
}

fn mcp_response_result(response: serde_json::Value) -> Result<serde_json::Value, String> {
    if let Some(error) = response.get("error") {
        return Err(error.to_string());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| "missing result".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_collects_tool_resource_and_prompt_names() {
        let mut inventory = McpInventory::default();
        inventory.extend("tools", &json!({"tools":[{"name":"search"},{"name":"read"}]}));
        inventory.extend(
            "resources",
            &json!({"resources":[{"uri":"file:///tmp/a"},{"name":"docs"}]}),
        );
        inventory.extend("prompts", &json!({"prompts":[{"name":"review"}]}));
        assert_eq!(inventory.tools, vec!["search", "read"]);
        assert_eq!(inventory.resources, vec!["file:///tmp/a", "docs"]);
        assert_eq!(inventory.prompts, vec!["review"]);
    }

    #[test]
    fn sse_http_response_parser_reads_matching_id() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n\
                    event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n";
        let value = parse_mcp_sse_response(body, 2).unwrap();
        assert_eq!(value["id"], 2);
    }

    #[test]
    fn probe_reports_configured_server_without_command_or_url() {
        let cfg = Config {
            mcp_servers: std::collections::HashMap::from([(
                "empty".to_string(),
                McpServerConfig::default(),
            )]),
            ..Config::default()
        };
        let report = probe_configured_servers(&cfg, Duration::from_millis(1));
        assert_eq!(report.servers.len(), 1);
        assert_eq!(report.servers[0].name, "empty");
        assert_eq!(report.servers[0].transport, "stdio");
        assert_eq!(report.servers[0].status, McpProbeStatus::Error);
        assert!(report.servers[0].diagnostics[0].contains("no command or url"));
    }
}
