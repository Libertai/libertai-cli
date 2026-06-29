//! Prompt-shape probe for review and verification discipline.
//!
//! That guidance lives in the `libertai-harness` skill. Since `feat(M5/#7)`
//! skill *bodies* are latent — loaded on demand via the `skill` tool rather
//! than inlined in the base prompt (and inclusion is non-deterministic per
//! invocation). So this probe asserts the harness skill is advertised in the
//! latent registry (the reachable on-ramp to that guidance), not that its body
//! text is inlined.

use assert_cmd::Command;

mod common;

const REQUIRED_PHRASES: &[&str] = &[
    // The latent-registry header…
    "## Available Agent Skills",
    // …lists the harness skill (which carries the review/verification rules)…
    "### libertai-harness",
    // …and tells the model to pull the body via the skill tool.
    "skill` tool",
];

#[test]
fn review_and_verification_skill_is_advertised_in_latent_registry() {
    let config_home = common::fake_config_home();
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
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
