//! Prompt-shape probe for session-specific slash command guidance.
//! This keeps local workflow affordances visible in the assembled
//! prompt instead of relying on the model to infer them from UI docs.

use assert_cmd::Command;

mod common;

const REQUIRED_PHRASES: &[&str] = &[
    "Session-specific commands",
    "/review",
    "/security-review",
    "/pr_comments",
    "/agent <name>",
    "<task>",
    "/send",
    "/init --agent <notes>",
    "/memory",
    "/remember <kind>: <fact>",
    "/mcp",
    "/hooks",
    "/hook",
    "!<command>",
    "/loop",
    "/auto",
    "do not invent extra tasks",
];

#[test]
fn session_command_guidance_reaches_assembled_prompt() {
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
