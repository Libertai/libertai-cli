//! S1-C probe — verifies the `libertai-harness` skill (which carries the
//! "Executing actions with care" guardrails) is advertised in the assembled
//! system prompt.
//!
//! Since `feat(M5/#7): Skill tool + latent skill registry`, skill *bodies* no
//! longer live in the base prompt — the prompt surfaces a latent registry
//! (name + description) and the model loads a body on demand via the `skill`
//! tool. So this probe asserts the harness is present in that registry, not
//! that its body text is inlined.

use assert_cmd::Command;

mod common;

const REQUIRED_PHRASES: &[&str] = &[
    // The latent-registry header…
    "## Available Agent Skills",
    // …lists the harness skill by name…
    "### libertai-harness",
    // …with its description (mentions the execution-caution posture)…
    "execution caution",
    // …and tells the model to pull a body via the skill tool.
    "skill` tool",
];

#[test]
fn harness_skill_is_advertised_in_latent_registry() {
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
