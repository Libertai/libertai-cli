//! Prompt-shape probe for review and verification discipline.
//! These instructions close a Claude Code quality gap that ordinary
//! unit tests cannot observe directly: review findings first, and
//! only claim completion from checks that exercise the changed behavior.

use assert_cmd::Command;

mod common;

const REQUIRED_PHRASES: &[&str] = &[
    "Review and verification",
    "default to review mode",
    "Do not modify files unless",
    "Lead with findings",
    "ordered by severity",
    "file_path:line_number",
    "smallest concrete fix",
    "residual test or coverage gap",
    "narrowest checks",
    "changed behavior",
    "Report verification honestly",
    "could not run",
    "passing unrelated test",
];

#[test]
fn review_and_verification_guidance_reaches_assembled_prompt() {
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let lower = stderr.to_lowercase();
    for phrase in REQUIRED_PHRASES {
        assert!(
            lower.contains(&phrase.to_lowercase()),
            "missing required phrase {phrase:?} in dumped prompt; got:\n{stderr}"
        );
    }
}
