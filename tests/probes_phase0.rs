//! Sprint 0 probe — verifies the `LIBERTAI_DUMP_SYSTEM_PROMPT` +
//! `LIBERTAI_DUMP_AND_EXIT` env vars added to `pi_agent_rust` work
//! and that the assembled system prompt reaches stderr with both
//! sentinels. Every subsequent prompt-shape probe relies on this.
//!
//! Offline tier-1: no model API call, no network.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;

const BEGIN_SENTINEL: &str = "===BEGIN SYSTEM PROMPT===";
const END_SENTINEL: &str = "===END SYSTEM PROMPT===";

/// The libertai-harness skill is always loaded; its "## Tone and style"
/// heading proves the skill content reached the assembled prompt.
const HARNESS_MARKER: &str = "## Tone and style";

#[test]
fn dump_env_var_prints_assembled_prompt_and_exits() {
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains(BEGIN_SENTINEL),
        "stderr missing begin sentinel; got:\n{stderr}"
    );
    assert!(
        stderr.contains(END_SENTINEL),
        "stderr missing end sentinel; got:\n{stderr}"
    );
    assert!(
        stderr.contains(HARNESS_MARKER),
        "stderr missing libertai-harness marker '{HARNESS_MARKER}'; got:\n{stderr}"
    );
}

#[test]
fn dump_disabled_without_env_var() {
    Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .args(["code", "--help"])
        .assert()
        .success()
        .stderr(contains(BEGIN_SENTINEL).not());
}
