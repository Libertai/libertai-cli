//! Session environment prompt helpers.
//!
//! Pi already injects cwd and date. This module adds the git context
//! that Claude Code-style agents rely on for branch-aware work: branch,
//! user, short status, and recent commits.

use std::path::Path;
use std::process::Command;

const MAX_STATUS_LINES: usize = 40;
const MAX_LOG_LINES: usize = 8;

pub fn append_environment_prompt(append: Option<String>, cwd: Option<&Path>) -> Option<String> {
    let env = cwd.and_then(environment_prompt);
    match (append, env) {
        (Some(mut append), Some(env)) => {
            append.push_str("\n\n");
            append.push_str(&env);
            Some(append)
        }
        (Some(append), None) => Some(append),
        (None, Some(env)) => Some(env),
        (None, None) => None,
    }
}

fn environment_prompt(cwd: &Path) -> Option<String> {
    let branch = git_stdout(cwd, &["branch", "--show-current"])
        .or_else(|| git_stdout(cwd, &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let user_name = git_stdout(cwd, &["config", "user.name"]).unwrap_or_default();
    let user_email = git_stdout(cwd, &["config", "user.email"]).unwrap_or_default();
    let status = git_lines(cwd, &["status", "--short", "--branch"], MAX_STATUS_LINES)?;
    let commits = git_lines(cwd, &["log", "--oneline", "-8"], MAX_LOG_LINES).unwrap_or_default();

    let mut out = String::from("## Environment\n\n");
    out.push_str("- cwd: ");
    out.push_str(&cwd.display().to_string());
    out.push('\n');
    out.push_str("- git branch: ");
    out.push_str(branch.trim());
    out.push('\n');
    if !user_name.trim().is_empty() || !user_email.trim().is_empty() {
        out.push_str("- git user: ");
        if !user_name.trim().is_empty() {
            out.push_str(user_name.trim());
        }
        if !user_email.trim().is_empty() {
            if !user_name.trim().is_empty() {
                out.push_str(" <");
                out.push_str(user_email.trim());
                out.push('>');
            } else {
                out.push_str(user_email.trim());
            }
        }
        out.push('\n');
    }
    out.push_str("\n### Git status\n\n```text\n");
    out.push_str(&status.join("\n"));
    if status.len() == MAX_STATUS_LINES {
        out.push_str("\n... status truncated ...");
    }
    out.push_str("\n```\n");
    if !commits.is_empty() {
        out.push_str("\n### Recent commits\n\n```text\n");
        out.push_str(&commits.join("\n"));
        out.push_str("\n```\n");
    }
    Some(out)
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn git_lines(cwd: &Path, args: &[&str], limit: usize) -> Option<Vec<String>> {
    let text = git_stdout(cwd, args)?;
    Some(text.lines().take(limit).map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::append_environment_prompt;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn non_git_repo_leaves_existing_prompt_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = append_environment_prompt(Some("skills".to_string()), Some(temp.path()));
        assert_eq!(prompt.as_deref(), Some("skills"));
    }

    #[test]
    fn git_repo_adds_branch_status_and_commits() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.email", "test@example.invalid"]);
        git(temp.path(), &["config", "user.name", "Test User"]);
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        git(temp.path(), &["add", "README.md"]);
        git(temp.path(), &["commit", "-m", "init"]);

        let prompt = append_environment_prompt(None, Some(temp.path())).unwrap();
        assert!(prompt.contains("## Environment"));
        assert!(prompt.contains("- cwd: "));
        assert!(prompt.contains("- git branch: "));
        assert!(prompt.contains("git user: Test User <test@example.invalid>"));
        assert!(prompt.contains("## master") || prompt.contains("## main"));
        assert!(prompt.contains("init"));
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed with {status}");
    }
}
