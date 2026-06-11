//! Probes for `libertai mcp` — spawn the real binary with piped stdio and
//! drive an MCP handshake end-to-end: initialize → notifications/initialized
//! → tools/list → ping → unknown method. Asserts stdout carries nothing but
//! line-delimited JSON-RPC frames.
//!
//! Offline tier-1: no model API call, no network (no tools/call here — the
//! tool backends are covered by unit tests in `src/commands/mcp.rs`).

use assert_cmd::Command;
use serde_json::Value;

fn run_mcp(stdin: &str) -> Vec<Value> {
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .arg("mcp")
        .write_stdin(stdin)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone())
        .expect("`libertai mcp` stdout not UTF-8");
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap_or_else(|e| {
                panic!("`libertai mcp` wrote a non-JSON stdout line ({e}): {line}")
            })
        })
        .collect()
}

#[test]
fn mcp_handshake_initialize_then_tools_list() {
    let frames = run_mcp(concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
    ));
    assert_eq!(frames.len(), 2, "expected exactly 2 frames: {frames:?}");

    let init = &frames[0];
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(init["result"]["serverInfo"]["name"], "libertai");

    let list = &frames[1];
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools/list result has a tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["web_search", "fetch_page"], "{frames:?}");
    for tool in tools {
        assert!(
            tool["inputSchema"]["type"] == "object",
            "tool missing object inputSchema: {tool}"
        );
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "tool missing description: {tool}"
        );
    }
}

#[test]
fn mcp_ping_and_unknown_method() {
    let frames = run_mcp(concat!(
        r#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":"b","method":"resources/list"}"#,
        "\n",
    ));
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["id"], "a");
    assert_eq!(frames[0]["result"], serde_json::json!({}));
    assert_eq!(frames[1]["id"], "b");
    assert_eq!(frames[1]["error"]["code"], -32601);
}

#[test]
fn mcp_exits_cleanly_on_eof_without_input() {
    let frames = run_mcp("");
    assert!(frames.is_empty(), "unexpected stdout frames: {frames:?}");
}
