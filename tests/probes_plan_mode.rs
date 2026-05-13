//! S1-B probe — verifies the plan-mode prompt addendum is injected
//! when (and only when) `libertai code --plan` is invoked.

use assert_cmd::Command;
use predicates::prelude::*;

const PLAN_HEADER: &str = "## Plan mode";

#[test]
fn plan_mode_addendum_present_with_flag() {
    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "--plan", "-p", "probe-ignored"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains(PLAN_HEADER),
        "expected '{PLAN_HEADER}' in dumped prompt under --plan; got:\n{stderr}"
    );
    assert!(
        stderr.contains("### Plan"),
        "expected '### Plan' heading guidance in dumped prompt; got:\n{stderr}"
    );
}

#[test]
fn plan_mode_addendum_absent_without_flag() {
    Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success()
        .stderr(predicate::str::contains(PLAN_HEADER).not());
}
