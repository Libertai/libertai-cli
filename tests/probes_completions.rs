//! Probes for `libertai completions <shell>` and the hidden `libertai man`
//! subcommand, plus freshness checks for the generated copies committed
//! under `packaging/` (the .deb ships those files verbatim — see the
//! `[package.metadata.deb]` assets in Cargo.toml).
//!
//! Offline tier-1: no model API call, no network.

use std::path::PathBuf;

use assert_cmd::Command;

fn libertai() -> Command {
    Command::cargo_bin("libertai").expect("libertai binary built")
}

fn packaging(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("packaging")
        .join(path)
}

/// Run `libertai completions <shell>` / `libertai man` and return stdout,
/// asserting the command exited 0 with a non-empty script naming the binary.
fn capture(args: &[&str]) -> String {
    let assert = libertai().args(args).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone())
        .unwrap_or_else(|_| panic!("`libertai {}` stdout not UTF-8", args.join(" ")));
    assert!(
        !stdout.trim().is_empty(),
        "`libertai {}` printed nothing",
        args.join(" ")
    );
    assert!(
        stdout.contains("libertai"),
        "`libertai {}` output does not mention the binary name; got:\n{stdout}",
        args.join(" ")
    );
    stdout
}

#[test]
fn completions_bash_emits_script() {
    let script = capture(&["completions", "bash"]);
    // The bash generator registers a `complete` rule for the binary.
    assert!(
        script.contains("complete"),
        "bash script missing a `complete` registration; got:\n{script}"
    );
}

#[test]
fn completions_cover_every_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        capture(&["completions", shell]);
    }
}

#[test]
fn man_page_renders_roff() {
    let page = capture(&["man"]);
    assert!(
        page.contains(".TH libertai"),
        "man output missing the roff .TH header; got:\n{}",
        &page[..page.len().min(400)]
    );
}

#[test]
fn man_subcommand_is_hidden_from_help() {
    let assert = libertai().arg("--help").assert().success();
    let help = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        !help.contains("\n  man"),
        "`man` should stay hidden in --help; got:\n{help}"
    );
    assert!(
        help.contains("completions"),
        "`completions` should be listed in --help; got:\n{help}"
    );
}

/// The .deb ships the committed copies under packaging/ — keep them in
/// lockstep with the live CLI surface (and the version in the man page's
/// .TH line). On failure, run `packaging/generate-assets.sh` and commit.
#[test]
fn committed_packaging_assets_are_fresh() {
    let cases = [
        (vec!["completions", "bash"], "completions/libertai.bash"),
        (vec!["completions", "zsh"], "completions/_libertai"),
        (vec!["completions", "fish"], "completions/libertai.fish"),
        (vec!["man"], "man/libertai.1"),
    ];
    for (args, rel) in cases {
        let live = capture(&args);
        let committed = std::fs::read_to_string(packaging(rel))
            .unwrap_or_else(|e| panic!("reading packaging/{rel}: {e}"));
        assert_eq!(
            committed, live,
            "packaging/{rel} is stale — run packaging/generate-assets.sh and commit the result"
        );
    }
}
