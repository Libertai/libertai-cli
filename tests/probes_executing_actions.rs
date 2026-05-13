//! S1-C probe — verifies the expanded "Executing actions with care"
//! block in `libertai-harness` reaches the assembled system prompt.
//! Asserts presence of distinguishing phrases from parity-doc section B.

use assert_cmd::Command;

const REQUIRED_PHRASES: &[&str] = &[
    "Executing actions with care",
    "blast radius",
    "reversibility",
    "hard-to-reverse",
    "third-party uploads",
    "scope of authorization",
    "investigate before deleting",
];

#[test]
fn executing_actions_block_reaches_assembled_prompt() {
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
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
