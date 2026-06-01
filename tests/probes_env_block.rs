//! S1-A probe — verifies the `## Git context` block is injected into
//! the assembled system prompt when `libertai code` runs inside a git
//! work tree.
//!
//! Builds a tiny throwaway git repo in a tempdir, runs `libertai code`
//! against it with the dump env vars, and asserts the block appears.
//! Also asserts the block is *absent* in a non-git tempdir so we know
//! the gate works.

use assert_cmd::Command;
use predicates::prelude::*;
use std::process::Command as ShCommand;

fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = ShCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "probe")
            .env("GIT_AUTHOR_EMAIL", "probe@example.invalid")
            .env("GIT_COMMITTER_NAME", "probe")
            .env("GIT_COMMITTER_EMAIL", "probe@example.invalid")
            .status()
            .expect("git command");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.name", "probe-user"]);
    run(&["config", "user.email", "probe@example.invalid"]);
    std::fs::write(dir.join("README.md"), "probe\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "probe initial commit"]);
}

#[test]
fn git_context_block_present_in_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());

    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .current_dir(tmp.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    for needle in &[
        "## Git context",
        "## main",
        "probe initial commit",
        "Git user: probe-user",
    ] {
        assert!(
            stderr.contains(needle),
            "missing {needle:?} in dumped prompt; got:\n{stderr}"
        );
    }
}

#[test]
fn git_context_block_absent_outside_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .current_dir(tmp.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success()
        .stderr(predicate::str::contains("## Git context").not());
}
