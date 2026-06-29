//! Prompt-shape probe — the LibertAI Code identity block is prepended to the
//! assembled system prompt and precedes the skill content.
//!
//! Offline tier-1: no model API call, no network. Relies on the same
//! `LIBERTAI_DUMP_SYSTEM_PROMPT` + `LIBERTAI_DUMP_AND_EXIT` mechanism as
//! `probes_phase0`.

use assert_cmd::Command;

mod common;

const BEGIN_SENTINEL: &str = "===BEGIN SYSTEM PROMPT===";
const END_SENTINEL: &str = "===END SYSTEM PROMPT===";

/// The identity block's lead sentence (from `code_identity_prompt.rs`).
const IDENTITY_LEAD: &str = "You are **LibertAI Code**";

/// A marker from the appended skills registry (the libertai-harness entry),
/// used to assert ordering. Skill bodies are latent since `feat(M5/#7)`, so
/// this is the registry entry rather than a body heading.
const HARNESS_MARKER: &str = "### libertai-harness";

#[test]
fn dumped_prompt_leads_with_libertai_identity() {
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
    assert!(
        stderr.contains(BEGIN_SENTINEL) && stderr.contains(END_SENTINEL),
        "stderr missing dump sentinels; got:\n{stderr}"
    );
    assert!(
        stderr.contains(IDENTITY_LEAD),
        "system prompt missing LibertAI identity lead '{IDENTITY_LEAD}'"
    );
    // The identity block must precede the skill content so the model reads
    // the identity correction before the harness guidance.
    let id = stderr
        .find(IDENTITY_LEAD)
        .expect("identity lead present (checked above)");
    let harness = stderr
        .find(HARNESS_MARKER)
        .expect("harness registry entry present");
    assert!(
        id < harness,
        "identity block should precede the harness skill marker"
    );
}

#[test]
fn dumped_prompt_lists_code_pillar_tools() {
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
    // Tools the base pi prompt omits but the code pillar registers.
    for tool in [
        "spawn_team",
        "team_task",
        "mailbox",
        "ask_user",
        "notebook_read",
    ] {
        assert!(
            stderr.contains(&format!("`{tool}`")),
            "system prompt missing code-pillar tool `{tool}`"
        );
    }
}
