//! Offline tier-1 probes for the `chat`/`ask` UX overhaul.
//!
//! Guarantees pinned here:
//! - `libertai chat` with piped stdin keeps the legacy non-interactive
//!   behavior: banner + `> ` prompt on stderr, `/exit` and EOF quit
//!   cleanly, request errors are reported on stderr without killing the
//!   loop, and stdout never carries ANSI escapes.
//! - `libertai ask` keeps its flags (`--model`) and exits with an error
//!   on stderr (stdout untouched) when the request fails.
//!
//! The raw "model text passes through unchanged" half of the contract is
//! pinned by unit tests next to the code (`ask::tests::raw_output_*`,
//! `chat_render::tests::raw_mode_buffers_nothing`) because the config
//! layer deliberately refuses non-HTTPS `api_base` values, so a loopback
//! HTTP fixture server cannot be wired in without weakening that gate.
//!
//! No external network: the error-path probes point `api_base` at a
//! closed 127.0.0.1 port (connection refused, instantly and offline).

use std::net::TcpListener;

use assert_cmd::Command;

mod common;

/// A 127.0.0.1 port that is almost certainly closed: bind to an
/// ephemeral port, note it, then drop the listener.
fn closed_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Config home whose `api_base` points at a closed local port. The
/// scheme must stay https — `config::load` rejects anything else.
fn config_home_for_port(port: u16) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config tempdir");
    for config_dir in [
        home.path().join("libertai"),
        home.path()
            .join("Library")
            .join("Application Support")
            .join("libertai"),
    ] {
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "api_base = \"https://127.0.0.1:{port}\"\n\n[auth]\napi_key = \"LTAI_sk_probe_config_00000000000000000000\"\n"
            ),
        )
        .unwrap();
    }
    home
}

#[test]
fn chat_piped_empty_stdin_prints_banner_and_exits() {
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .arg("chat")
        .write_stdin("")
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("LibertAI chat — model:"),
        "banner missing from stderr; got:\n{stderr}"
    );
    let stdout = &assert.get_output().stdout;
    assert!(
        stdout.is_empty(),
        "no input should produce no stdout; got: {:?}",
        String::from_utf8_lossy(stdout)
    );
}

#[test]
fn chat_piped_exit_command_quits_without_network() {
    let config_home = common::fake_config_home();
    Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .arg("chat")
        .write_stdin("/exit\n")
        .assert()
        .success();
}

#[test]
fn chat_piped_request_error_keeps_loop_alive_and_stdout_clean() {
    let config_home = config_home_for_port(closed_loopback_port());
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .arg("chat")
        // Two prompts: the loop must survive the first failure and try
        // again, then exit 0 on EOF — exactly the legacy behavior.
        .write_stdin("hello\nstill alive?\n")
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("error:"),
        "request failure should be reported on stderr; got:\n{stderr}"
    );
    assert_eq!(
        stderr.matches("error:").count(),
        2,
        "both turns should fail and be reported; got:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        !stdout.contains('\u{1b}'),
        "piped chat stdout must not contain ANSI escapes; got: {stdout:?}"
    );
}

#[test]
fn ask_request_error_exits_nonzero_with_clean_stdout() {
    let config_home = config_home_for_port(closed_loopback_port());
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .args(["ask", "--model", "probe-model", "say something"])
        .assert()
        .failure();

    let stdout = &assert.get_output().stdout;
    assert!(
        stdout.is_empty(),
        "failed ask must not write to stdout; got: {:?}",
        String::from_utf8_lossy(stdout)
    );
}
