//! Prompt-shape probe for the harness text-output guidance. This keeps
//! Claude-style output constraints in the assembled prompt, where they
//! affect every native code session.

use assert_cmd::Command;

mod common;

const REQUIRED_PHRASES: &[&str] = &[
    "cannot see raw tool calls",
    "decisive lines",
    "do not create planning documents",
    "keep plans in the conversation or todo tool",
    "docstrings or module comments",
];

#[test]
fn text_output_guidance_reaches_assembled_prompt() {
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
