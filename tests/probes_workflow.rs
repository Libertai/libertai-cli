//! (WF-G) Workflow-engine probes — run real scripts through the QuickJS
//! engine end-to-end via the `LIBERTAI_WORKFLOW_SELFTEST` hook, fully
//! offline (no LLM, no session, no terminal). Regression coverage for the
//! WF-A engine fixes: a valid async-IIFE wrapper, `log`/`phase`/return
//! plumbing, script-error → failed-run mapping, and exit-status truth.

use assert_cmd::Command;
use predicates::prelude::*;

mod common;

fn selftest_cmd(script: &str) -> Command {
    let config_home = common::fake_config_home();
    let mut cmd = Command::cargo_bin("libertai").expect("libertai binary built");
    cmd.env("XDG_CONFIG_HOME", config_home.path())
        .env("HOME", config_home.path())
        .env("LIBERTAI_WORKFLOW_SELFTEST", script)
        // Keep a hard bound in case a probe script regresses into
        // something that never settles.
        .env("LIBERTAI_WORKFLOW_TIMEOUT_SECS", "30")
        .args(["code"]);
    // The TempDir must outlive the command run; leak it for the test's
    // lifetime (process-scoped, cleaned by the OS temp reaper).
    std::mem::forget(config_home);
    cmd
}

#[test]
fn selftest_script_completes_with_logs_and_result() {
    selftest_cmd(
        "log('probe-log-line'); phase('scan', () => {}); return {answer: 42, list: [1,2]};",
    )
    .assert()
    .success()
    .stderr(predicate::str::contains(
        "workflow selftest log: probe-log-line",
    ))
    .stderr(predicate::str::contains("completed"))
    .stderr(predicate::str::contains("1 phases"))
    .stderr(predicate::str::contains("\"answer\":42"));
}

#[test]
fn selftest_script_throw_fails_with_nonzero_exit() {
    selftest_cmd("throw new Error('probe-boom');")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed"))
        .stderr(predicate::str::contains("probe-boom"));
}

#[test]
fn selftest_syntax_error_fails() {
    selftest_cmd("this is not ((( javascript")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed"));
}

#[test]
fn selftest_async_and_pipeline_prelude_work_offline() {
    // Exercises the prelude's pipeline() and top-level await without any
    // agent() calls: stages are plain async transforms.
    selftest_cmd(
        "const out = await pipeline([1, 2, 3], async (x) => x * 10, async (x) => x + 1); \
         log('pipeline: ' + JSON.stringify(out)); return out;",
    )
    .assert()
    .success()
    .stderr(predicate::str::contains("pipeline: [11,21,31]"))
    .stderr(predicate::str::contains("completed"));
}
