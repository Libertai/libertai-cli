//! Probes for `libertai code --acp` — the Agent Client Protocol server.
//!
//! ACP is line-delimited JSON-RPC 2.0 over stdio, so the contract under test
//! is a byte-level one: **every byte written to stdout is a JSON-RPC message
//! and nothing else**. A single stray banner, prompt, progress line or ANSI
//! reset desynchronises the editor's framing and the handshake dies with no
//! useful error, so these probes spawn the real binary and inspect the raw
//! stdout bytes rather than trusting the code path by reading.
//!
//! Offline tier-1: no model API call, no network. The handshake
//! (`initialize` + `session/new`) is served entirely from the local model
//! registry — a prompt would need inference, the setup here does not.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

/// A hermetic environment for one ACP run: a fake `XDG_CONFIG_HOME`/`HOME`
/// carrying a libertai config with an API key, plus an isolated pi agent
/// directory so `models.json` is written under the tempdir.
struct AcpEnv {
    config_home: TempDir,
    pi_dir: TempDir,
}

impl AcpEnv {
    fn new() -> Self {
        Self {
            config_home: common::fake_config_home(),
            pi_dir: tempfile::tempdir().expect("pi tempdir"),
        }
    }

    fn config_dir(&self) -> std::path::PathBuf {
        self.config_home.path().join("libertai")
    }

    /// Plant an update-check cache claiming a much newer release, and make
    /// sure the update check is not disabled by config. This is the state in
    /// which the startup banner would fire on any other subcommand.
    fn arm_update_check(&self) {
        let dir = self.config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(
            dir.join("update-check.json"),
            // `last_check_unix: 0` is also maximally stale, so the background
            // refresh path is exercised too.
            r#"{"last_check_unix":0,"latest_version_seen":"999.0.0"}"#,
        )
        .expect("write update cache");
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(bin_path());
        cmd.env("XDG_CONFIG_HOME", self.config_home.path())
            .env("HOME", self.config_home.path())
            .env("PI_CODING_AGENT_DIR", self.pi_dir.path())
            // Hermetic: never fetch the public pricing catalog from a test.
            .env("LIBERTAI_MODEL_CATALOG_URL", "off")
            // Clear the CI/NO_UPDATE_CHECK escape hatches so the update check
            // is genuinely armed rather than skipped for an unrelated reason.
            .env_remove("NO_UPDATE_CHECK")
            .env_remove("CI")
            .args(["code", "--acp"]);
        cmd
    }
}

/// Path to the `libertai` binary under test. `assert_cmd`'s `cargo_bin` gives
/// the same path but only as a `Command`; ACP needs piped stdio and a manual
/// wait, so the path is taken directly.
fn bin_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test exe path");
    // .../target/<profile>/deps/probes_acp-<hash>
    let deps = exe.parent().expect("deps dir");
    let profile: &Path = deps.parent().expect("profile dir");
    let candidate = profile.join(format!("libertai{}", std::env::consts::EXE_SUFFIX));
    assert!(
        candidate.exists(),
        "libertai binary not found at {} — build it first",
        candidate.display()
    );
    candidate
}

/// Drive one ACP conversation: spawn the server, write `requests` (one JSON
/// object per line), close stdin, and return `(stdout_bytes, stderr_string)`.
fn run_acp(env: &AcpEnv, requests: &[&str]) -> (Vec<u8>, String) {
    let mut child = env
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn libertai code --acp");

    {
        let stdin = child.stdin.as_mut().expect("acp stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write acp request");
        }
        stdin.flush().expect("flush acp stdin");
    }
    // Dropping stdin signals EOF, which ends the server's read loop.
    drop(child.stdin.take());

    let output = wait_with_timeout(child, Duration::from_secs(120));
    (
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `Child::wait_with_output` has no timeout; a hung ACP server would hang the
/// whole suite instead of failing. Poll for the deadline, then kill.
fn wait_with_timeout(mut child: std::process::Child, timeout: Duration) -> std::process::Output {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                panic!("`libertai code --acp` did not exit within {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    child.wait_with_output().expect("acp output")
}

/// Parse stdout as the ACP wire format: newline-delimited JSON-RPC messages.
/// Fails loudly (printing the offending bytes) on anything else, which is
/// exactly the failure an editor would hit.
fn parse_wire(stdout: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(stdout)
        .unwrap_or_else(|e| panic!("ACP stdout is not UTF-8 ({e}): {stdout:?}"));

    // No ANSI escapes, no carriage returns (progress bars / spinners), no
    // other control bytes: only newline separators are legal framing.
    for (index, byte) in stdout.iter().enumerate() {
        assert!(
            *byte == b'\n' || !byte.is_ascii_control(),
            "ACP stdout carries a control byte {byte:#04x} at offset {index} \
             (ANSI escape, \\r progress output, or similar); full stdout:\n{text}"
        );
    }

    assert!(
        !text.is_empty(),
        "ACP server produced no stdout at all (expected JSON-RPC responses)"
    );
    assert!(
        text.ends_with('\n'),
        "ACP stdout does not end on a frame boundary:\n{text}"
    );

    let mut messages = Vec::new();
    for (index, line) in text.lines().enumerate() {
        assert!(
            !line.trim().is_empty(),
            "blank line at frame {index} in ACP stdout:\n{text}"
        );
        // Byte-clean framing: nothing before or between messages. A leading
        // banner would show up here as a parse failure on frame 0.
        let value: Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("frame {index} of ACP stdout is not JSON-RPC ({e}):\n{line}\n--- full stdout:\n{text}")
        });
        assert_eq!(
            value.get("jsonrpc").and_then(Value::as_str),
            Some("2.0"),
            "frame {index} is JSON but not JSON-RPC 2.0: {line}"
        );
        messages.push(value);
    }
    messages
}

fn response_for(messages: &[Value], id: u64) -> &Value {
    messages
        .iter()
        .find(|m| m.get("id").and_then(Value::as_u64) == Some(id))
        .unwrap_or_else(|| panic!("no JSON-RPC response with id {id} in {messages:#?}"))
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
const SESSION_NEW: &str = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"."}}"#;

/// The core contract: a real `initialize` + `session/new` handshake against
/// the built binary, with stdout carrying clean JSON-RPC and nothing else.
#[test]
fn acp_handshake_completes_over_clean_stdout() {
    let env = AcpEnv::new();
    let (stdout, stderr) = run_acp(&env, &[INITIALIZE, SESSION_NEW]);
    let messages = parse_wire(&stdout);

    assert_eq!(
        messages.len(),
        2,
        "expected exactly one response per request; stderr was:\n{stderr}"
    );

    // initialize: capability exchange, integer protocol version.
    let init = response_for(&messages, 1);
    let result = init
        .get("result")
        .unwrap_or_else(|| panic!("initialize returned no result: {init}"));
    assert!(
        result
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .is_some(),
        "initialize result missing an integer protocolVersion: {result}"
    );
    assert!(
        result.get("agentCapabilities").is_some(),
        "initialize result missing agentCapabilities: {result}"
    );

    // session/new: a real session, backed by the LibertAI provider that
    // `libertai code` would have used. This is the assertion that the
    // adapter's model/auth/endpoint resolution actually reached the engine.
    let session = response_for(&messages, 2);
    let result = session
        .get("result")
        .unwrap_or_else(|| panic!("session/new returned no result (stderr:\n{stderr}): {session}"));
    assert!(
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "session/new result missing a sessionId: {result}"
    );
    let models = result
        .get("models")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("session/new result missing the model list: {result}"));
    assert!(
        models
            .iter()
            .any(|m| m.get("provider").and_then(Value::as_str) == Some("libertai")),
        "session/new offered no libertai model — the editor would not be talking to \
         LibertAI inference: {result}"
    );
}

/// Same handshake with the update check armed: a cached "999.0.0 is
/// available" notice that would print a banner on a normal *interactive*
/// subcommand must not put a single byte on the ACP wire.
///
/// Scope, honestly: this asserts the end-to-end invariant (stdout stays
/// byte-clean), but it does not isolate `is_stdio_protocol_command`. The probe
/// captures stdout through a pipe, and `update_check` already returns early
/// when `stdout().is_terminal()` is false (`src/update_check.rs:71`), so the
/// banner is suppressed here by that check whether or not the ACP guard
/// exists. Isolating the guard would need a PTY-backed stdout; until then,
/// treat a pass as "the wire is clean", not as "the guard works".
#[test]
fn acp_stdout_stays_clean_when_an_update_check_would_fire() {
    let env = AcpEnv::new();
    env.arm_update_check();

    let (stdout, _stderr) = run_acp(&env, &[INITIALIZE]);
    let messages = parse_wire(&stdout);

    assert_eq!(
        messages.len(),
        1,
        "expected exactly the initialize response"
    );
    assert!(
        response_for(&messages, 1).get("result").is_some(),
        "initialize failed with the update check armed: {messages:#?}"
    );
    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("999.0.0"),
        "the update banner leaked onto the ACP wire:\n{text}"
    );
}

/// A malformed line must produce a JSON-RPC parse error *on the wire*, not a
/// panic, a plain-text message, or a dead server: the framing survives bad
/// input, which is what keeps an editor session recoverable.
#[test]
fn acp_reports_malformed_input_as_json_rpc_and_keeps_serving() {
    let env = AcpEnv::new();
    let (stdout, _stderr) = run_acp(&env, &["not json at all", INITIALIZE]);
    let messages = parse_wire(&stdout);

    assert_eq!(messages.len(), 2, "expected an error then the response");
    assert_eq!(
        messages[0].get("error").and_then(|e| e.get("code")),
        Some(&Value::from(-32700)),
        "first frame should be a JSON-RPC parse error: {:#?}",
        messages[0]
    );
    assert!(
        response_for(&messages, 1).get("result").is_some(),
        "the server stopped serving after malformed input: {messages:#?}"
    );
}
