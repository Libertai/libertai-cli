//! Probes — output discipline: the NO_COLOR styling gate, `--json`
//! purity, and the differentiated exit-code contract (1 generic, 2 usage,
//! 3 auth, 4 network, 5 API).
//!
//! Offline tier-1: `status` runs fully offline; the network-flavoured
//! probes point `api_base` at a closed localhost port so failures are
//! deterministic connection refusals, never real traffic.

use std::time::Duration;

use assert_cmd::Command;

mod common;

/// Config home whose `api_base` is a closed localhost port — any model
/// API call fails with connection-refused instead of reaching the real
/// backend. (Must be `https`: config validation rejects plain-http bases.)
fn config_home_unreachable_backend() -> tempfile::TempDir {
    config_home_with(
        "api_base = \"https://127.0.0.1:9\"\n\n\
         [auth]\napi_key = \"LTAI_sk_probe_config_00000000000000000000\"\n",
    )
}

/// Config home with no stored API key at all (logged-out state).
fn config_home_logged_out() -> tempfile::TempDir {
    config_home_with("api_base = \"https://127.0.0.1:9\"\n")
}

fn config_home_with(contents: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config tempdir");
    for config_dir in [
        home.path().join("libertai"),
        home.path()
            .join("Library")
            .join("Application Support")
            .join("libertai"),
    ] {
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), contents).unwrap();
    }
    home
}

fn assert_no_escape_bytes(label: &str, bytes: &[u8]) {
    assert!(
        !bytes.contains(&0x1b),
        "{label} contains ESC bytes:\n{}",
        String::from_utf8_lossy(bytes)
    );
}

#[test]
fn status_no_color_piped_emits_no_ansi() {
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("NO_COLOR", "1")
        .timeout(Duration::from_secs(60))
        .arg("status")
        .assert()
        .success();

    let out = assert.get_output();
    assert_no_escape_bytes("status stdout", &out.stdout);
    assert_no_escape_bytes("status stderr", &out.stderr);
}

#[test]
fn status_json_is_pure_and_parseable() {
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .timeout(Duration::from_secs(60))
        .args(["status", "--json"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("status --json stdout is not pure JSON ({e}):\n{stdout}"));
    assert!(value.get("api_base").is_some(), "missing api_base: {value}");
    assert!(
        value.pointer("/defaults/chat_model").is_some(),
        "missing defaults.chat_model: {value}"
    );
    assert_eq!(
        value.pointer("/auth/logged_in"),
        Some(&serde_json::Value::Bool(true)),
        "expected auth.logged_in=true: {value}"
    );
    assert_no_escape_bytes("status --json stdout", &assert.get_output().stdout);
}

#[test]
fn models_unreachable_backend_exits_with_network_code_and_no_ansi() {
    let config_home = config_home_unreachable_backend();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("NO_COLOR", "1")
        .timeout(Duration::from_secs(120))
        .arg("models")
        .assert()
        .code(4);

    let out = assert.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("error:"),
        "stderr missing clean `error:` line; got:\n{stderr}"
    );
    assert_no_escape_bytes("models stdout", &out.stdout);
    assert_no_escape_bytes("models stderr", &out.stderr);
}

#[test]
fn models_logged_out_exits_with_auth_code() {
    let config_home = config_home_logged_out();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("NO_COLOR", "1")
        .timeout(Duration::from_secs(60))
        .arg("models")
        .assert()
        .code(3);

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("libertai login"),
        "auth failure should point at `libertai login`; got:\n{stderr}"
    );
}

#[test]
fn keys_list_fails_cleanly_with_no_ansi() {
    // Wallet-flavoured auth so `keys list` takes the non-browser path:
    // the private-key prompt cannot be answered on a piped stdin, so the
    // run must fail fast (never hang) and, with NO_COLOR, the sign-in
    // banner on stderr must carry no escape bytes.
    let config_home = config_home_with(
        "api_base = \"https://127.0.0.1:9\"\n\n\
         [auth]\n\
         api_key = \"LTAI_sk_probe_config_00000000000000000000\"\n\
         wallet_address = \"0x0000000000000000000000000000000000000001\"\n\
         chain = \"base\"\n",
    );
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("NO_COLOR", "1")
        .timeout(Duration::from_secs(60))
        .args(["keys", "list"])
        .write_stdin("\n")
        .assert()
        .failure();

    let out = assert.get_output();
    assert_no_escape_bytes("keys list stdout", &out.stdout);
    assert_no_escape_bytes("keys list stderr", &out.stderr);
}

#[test]
fn code_list_sessions_json_is_pure_and_parseable() {
    let config_home = common::fake_config_home();
    let pi_dir = tempfile::tempdir().expect("pi tempdir");
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("PI_CODING_AGENT_DIR", pi_dir.path())
        .timeout(Duration::from_secs(60))
        .args(["code", "--list-sessions", "--json"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("code --list-sessions --json stdout is not pure JSON ({e}):\n{stdout}")
    });
    assert!(
        value.is_array(),
        "expected a JSON array of sessions: {value}"
    );
}
