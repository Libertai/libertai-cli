//! Probe — `libertai code --print` / `-p` headless mode.
//!
//! Proves the print path never enters the raw-mode TUI: with no prompt
//! it fails fast with a usage error, with a piped-stdin prompt it runs
//! the headless path (dump-and-exit short-circuits before any network),
//! and with an unreachable backend it exits nonzero with a clean error
//! instead of hanging.
//!
//! Offline tier-1: no model API call, no network (the backend probe
//! points at a closed localhost port).

use std::time::Duration;

use assert_cmd::Command;

mod common;

const BEGIN_SENTINEL: &str = "===BEGIN SYSTEM PROMPT===";

/// Like `common::fake_config_home` but points `api_base` at a closed
/// localhost port so any model call fails with connection-refused
/// instead of reaching the real backend.
fn fake_config_home_unreachable_backend() -> tempfile::TempDir {
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
            "api_base = \"http://127.0.0.1:9\"\n\n[auth]\napi_key = \"LTAI_sk_probe_config_00000000000000000000\"\n",
        )
        .unwrap();
    }
    home
}

#[test]
fn print_without_prompt_or_stdin_fails_fast() {
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .timeout(Duration::from_secs(60))
        .args(["code", "-p"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("--print needs a prompt"),
        "stderr missing the --print usage error; got:\n{stderr}"
    );
}

#[test]
fn print_accepts_prompt_from_piped_stdin() {
    // Dump-and-exit fires once the headless session assembles its
    // system prompt — i.e. after the stdin prompt was accepted but
    // before any network call. Success + sentinel proves `-p` with a
    // piped prompt takes the headless path, not the REPL.
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .timeout(Duration::from_secs(60))
        .args(["code", "-p"])
        .write_stdin("probe prompt piped on stdin")
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains(BEGIN_SENTINEL),
        "stderr missing begin sentinel; got:\n{stderr}"
    );
}

#[test]
fn print_fails_cleanly_when_backend_unreachable() {
    // No dump-and-exit here: the run goes all the way to the model call
    // and must exit nonzero with a clean `error:` line — completing at
    // all (within the timeout) proves it didn't hang in raw mode.
    let config_home = fake_config_home_unreachable_backend();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .timeout(Duration::from_secs(120))
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("error:"),
        "stderr missing clean `error:` line; got:\n{stderr}"
    );
}
