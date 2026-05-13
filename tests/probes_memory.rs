//! S1-D probe — verifies the per-project `MEMORY.md` round-trip:
//!
//! 1. With `LIBERTAI_HOME` pointing at a tempdir and a hand-crafted
//!    `MEMORY.md` containing a unique marker, the dumped system
//!    prompt contains a `## Memory` section with that marker.
//! 2. With no MEMORY.md present, the section is absent.
//!
//! The encoding of the cwd → directory-name matches `pi::app::
//! encode_project_cwd` (canonical-cwd `/` → `-` with leading `-`
//! stripped); the probe replicates it inline so the test fails
//! visibly if either side changes.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn encode_cwd(p: &Path) -> String {
    let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical
        .to_string_lossy()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_string()
}

#[test]
fn memory_file_round_trips_into_prompt() {
    let home = tempfile::tempdir().expect("home tempdir");
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let encoded = encode_cwd(cwd.path());
    let memory_dir = home.path().join("projects").join(&encoded);
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("MEMORY.md"),
        "- 2026-05-12 12:00 probe-marker-libertai-memory-v1\n",
    )
    .unwrap();

    let assert = Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .current_dir(cwd.path())
        .env("LIBERTAI_HOME", home.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("## Memory"),
        "expected '## Memory' section in dumped prompt; got:\n{stderr}"
    );
    assert!(
        stderr.contains("probe-marker-libertai-memory-v1"),
        "expected probe marker in dumped prompt; got:\n{stderr}"
    );
}

#[test]
fn memory_section_absent_when_no_file() {
    let home = tempfile::tempdir().expect("home tempdir");
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    Command::cargo_bin("libertai")
        .expect("libertai binary built")
        .current_dir(cwd.path())
        .env("LIBERTAI_HOME", home.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success()
        .stderr(predicate::str::contains("## Memory").not());
}
