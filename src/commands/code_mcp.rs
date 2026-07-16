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
            probe_legacy_sse_server(server, timeout)
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

fn probe_stdio_server(server: &McpServerConfig, timeout: Duration) -> Result<McpInventory, String> {
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

fn probe_http_server(server: &McpServerConfig, timeout: Duration) -> Result<McpInventory, String> {
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

fn probe_legacy_sse_server(
    server: &McpServerConfig,
    timeout: Duration,
) -> Result<McpInventory, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let url = server.url.trim();
    let stream = open_mcp_sse_stream(&client, server, url)?;
    let (rx, _reader) = read_mcp_sse_stream(stream);
    let endpoint = wait_for_mcp_sse_endpoint(&rx, url, timeout)?;
    post_mcp_sse_message(&client, server, &endpoint, &initialize_request(1))?;
    let init = wait_for_mcp_sse_response(&rx, 1, timeout)?;
    mcp_response_result(init).map_err(|e| format!("initialize failed: {e}"))?;
    post_mcp_sse_message(&client, server, &endpoint, &initialized_notification())?;

    let mut inventory = McpInventory::default();
    for request in [
        ("tools/list", 2_u64, "tools"),
        ("resources/list", 3_u64, "resources"),
        ("prompts/list", 4_u64, "prompts"),
    ] {
        let (method, id, key) = request;
        match post_mcp_sse_message(&client, server, &endpoint, &list_request(id, method))
            .and_then(|()| wait_for_mcp_sse_response(&rx, id, timeout))
            .and_then(mcp_response_result)
        {
            Ok(result) => inventory.extend(key, &result),
            Err(e) => inventory.diagnostics.push(format!("{method}: {e}")),
        }
    }
    Ok(inventory)
}

fn open_mcp_sse_stream(
    client: &reqwest::blocking::Client,
    server: &McpServerConfig,
    url: &str,
) -> Result<reqwest::blocking::Response, String> {
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .header("mcp-protocol-version", "2025-03-26");
    for (name, value) in &server.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().map_err(|e| e.to_string())?;
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        Err(format!("HTTP {status}"))
    }
}

fn post_mcp_sse_message(
    client: &reqwest::blocking::Client,
    server: &McpServerConfig,
    endpoint: &str,
    message: &serde_json::Value,
) -> Result<(), String> {
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header("mcp-protocol-version", "2025-03-26")
        .json(message);
    for (name, value) in &server.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().map_err(|e| e.to_string())?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}: {}", body.trim()))
    }
}

#[derive(Debug)]
struct SseEvent {
    event: String,
    data: String,
}

fn read_mcp_sse_stream(
    response: reqwest::blocking::Response,
) -> (mpsc::Receiver<SseEvent>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(response);
        let mut line = String::new();
        let mut event = String::new();
        let mut data = Vec::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        if !event.is_empty() || !data.is_empty() {
                            let _ = tx.send(SseEvent {
                                event: if event.is_empty() {
                                    "message".to_string()
                                } else {
                                    event.clone()
                                },
                                data: data.join("\n"),
                            });
                            event.clear();
                            data.clear();
                        }
                        continue;
                    }
                    if let Some(value) = trimmed.strip_prefix("event:") {
                        event = value.trim_start().to_string();
                    } else if let Some(value) = trimmed.strip_prefix("data:") {
                        data.push(value.trim_start().to_string());
                    }
                }
                Err(_) => break,
            }
        }
    });
    (rx, reader)
}

fn wait_for_mcp_sse_endpoint(
    rx: &mpsc::Receiver<SseEvent>,
    base_url: &str,
    timeout: Duration,
) -> Result<String, String> {
    let event = wait_for_mcp_sse_event(rx, timeout, |event| {
        event.event == "endpoint" || event.data.starts_with('/') || event.data.starts_with("http")
    })?;
    resolve_mcp_sse_endpoint(base_url, event.data.trim())
}

fn wait_for_mcp_sse_response(
    rx: &mpsc::Receiver<SseEvent>,
    id: u64,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let event = wait_for_mcp_sse_event(rx, timeout, |event| {
        serde_json::from_str::<serde_json::Value>(&event.data)
            .ok()
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .map(|found| found == id)
            })
            .unwrap_or(false)
    })?;
    serde_json::from_str::<serde_json::Value>(&event.data)
        .map_err(|e| format!("invalid MCP SSE JSON response: {e}"))
}

fn wait_for_mcp_sse_event<F>(
    rx: &mpsc::Receiver<SseEvent>,
    timeout: Duration,
    mut matches: F,
) -> Result<SseEvent, String>
where
    F: FnMut(&SseEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for SSE event".to_string());
        }
        let event = rx
            .recv_timeout(remaining)
            .map_err(|_| "timed out waiting for SSE event".to_string())?;
        if matches(&event) {
            return Ok(event);
        }
    }
}

fn resolve_mcp_sse_endpoint(base_url: &str, endpoint: &str) -> Result<String, String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let base = url::Url::parse(base_url).map_err(|e| e.to_string())?;
    base.join(endpoint)
        .map(|url| url.to_string())
        .map_err(|e| e.to_string())
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
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
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
        inventory.extend(
            "tools",
            &json!({"tools":[{"name":"search"},{"name":"read"}]}),
        );
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

    #[test]
    fn probe_legacy_sse_server_lists_inventory() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
            // Must exceed the client's per-step probe timeout so a slow CI
            // runner exhausts the client first (probe error with
            // diagnostics), not this harness thread (opaque join panic).
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => return stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            panic!("timed out accepting legacy SSE probe connection");
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accepting legacy SSE probe connection: {e}"),
                }
            }
        }

        fn write_sse_chunk(stream: &mut impl Write, event: &str) {
            write!(stream, "{:x}\r\n{}\r\n", event.len(), event).unwrap();
            stream.flush().unwrap();
        }

        fn read_request(stream: &mut TcpStream) -> String {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 8192];
            let mut text = String::new();
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if text.contains("\r\n\r\n") {
                    let header_end = text.find("\r\n\r\n").unwrap() + 4;
                    let headers = &text[..header_end];
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if text.len() >= header_end + content_len {
                        break;
                    }
                }
            }
            text
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut sse_stream = accept_with_timeout(&listener);
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n";
            sse_stream.write_all(response.as_bytes()).unwrap();
            write_sse_chunk(
                &mut sse_stream,
                &format!("event: endpoint\ndata: http://{addr}/messages\n\n"),
            );

            for idx in 0..5 {
                let mut post_stream = accept_with_timeout(&listener);
                let request = read_request(&mut post_stream);
                let post_response =
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                post_stream.write_all(post_response.as_bytes()).unwrap();
                let event = match idx {
                    0 => Some(
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{}}}\n\n",
                    ),
                    2 => {
                        assert!(request.contains("\"method\":\"tools/list\""));
                        Some(
                            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"search\"}]}}\n\n",
                        )
                    }
                    3 => {
                        assert!(request.contains("\"method\":\"resources/list\""));
                        Some(
                            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"resources\":[{\"uri\":\"file:///tmp/a\"}]}}\n\n",
                        )
                    }
                    4 => {
                        assert!(request.contains("\"method\":\"prompts/list\""));
                        Some(
                            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"prompts\":[{\"name\":\"review\"}]}}\n\n",
                        )
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    write_sse_chunk(&mut sse_stream, event);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        });

        let cfg = Config {
            mcp_servers: std::collections::HashMap::from([(
                "policy".to_string(),
                McpServerConfig {
                    transport: "sse".to_string(),
                    url: format!("http://{addr}/sse"),
                    ..McpServerConfig::default()
                },
            )]),
            ..Config::default()
        };
        // Per-step timeout: generous so loaded CI runners never trip it —
        // the fake server replies instantly, so the happy path is unaffected.
        let report = probe_configured_servers(&cfg, Duration::from_secs(30));
        assert_eq!(report.servers.len(), 1);
        let server = &report.servers[0];
        assert_eq!(
            server.status,
            McpProbeStatus::Ok,
            "{:?}",
            server.diagnostics
        );
        handle.join().unwrap();
        assert_eq!(server.transport, "legacy-sse");
        assert_eq!(server.tools, vec!["search"]);
        assert_eq!(server.resources, vec!["file:///tmp/a"]);
        assert_eq!(server.prompts, vec!["review"]);
    }
}
