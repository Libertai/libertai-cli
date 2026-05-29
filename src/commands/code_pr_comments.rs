use std::path::Path;
use std::process::Command;

use serde_json::Value as JsonValue;

const MAX_CAPTURE_CHARS: usize = 24_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCapture {
    pub command: String,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

impl CommandCapture {
    fn success(&self) -> bool {
        self.error.is_none() && self.status == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCommentsSnapshot {
    pub pr_view: CommandCapture,
    pub checks: Option<CommandCapture>,
    pub review_threads: Option<CommandCapture>,
}

pub fn build_pr_comments_prompt(scope: &str, snapshot: Option<&PrCommentsSnapshot>) -> String {
    let scope = scope.trim();
    let scope_line = if scope.is_empty() {
        "User-requested PR scope: infer the current branch's pull request.".to_string()
    } else {
        format!("User-requested PR scope: {scope}")
    };
    let snapshot_block = snapshot
        .map(render_snapshot_block)
        .unwrap_or_else(|| "Native PR snapshot: not collected.".to_string());
    format!(
        r#"Inspect pull request review comments for this repository and turn them into an actionable response plan.

{scope_line}

{snapshot_block}

Rules:
- Do not modify files or make commits.
- Start from the native PR snapshot above when it contains data; use git state only to verify whether comments are already addressed.
- If the snapshot is missing or failed, inspect git state: git status --short, git branch --show-current, git remote -v, and git diff --stat.
- If needed, use GitHub CLI to fill gaps: gh pr view --json number,url,headRefName,baseRefName,reviewDecision,comments,reviews,files, gh pr checks, and GitHub GraphQL reviewThreads.
- If the user supplied a PR number or URL, use that exact PR. Otherwise infer the PR for the current branch.
- Summarize unresolved review comments first, grouped by file and reviewer when possible.
- For each actionable comment, cite file:line when available, explain the requested change, and propose the minimal fix.
- Call out comments that appear already addressed by the current diff.
- When GitHub review thread IDs are present, identify which threads are safe to resolve after code changes land.
- If PR data cannot be loaded, report the exact command/error and suggest the next concrete command the user can run."#
    )
}

pub fn collect_pr_comments_snapshot(cwd: &Path, scope: &str) -> PrCommentsSnapshot {
    let selector = pr_selector(scope);
    let pr_view_args = pr_view_args(selector.as_deref());
    let pr_view = run_gh(cwd, &pr_view_args);
    let checks = if pr_view.success() {
        Some(run_gh(cwd, &pr_checks_args(selector.as_deref())))
    } else {
        None
    };
    let review_threads = pr_reference_from_view(&pr_view)
        .map(|pr| run_gh(cwd, &pr_review_threads_args(&pr)));
    PrCommentsSnapshot {
        pr_view,
        checks,
        review_threads,
    }
}

pub fn resolve_review_thread(cwd: &Path, thread_id: &str) -> CommandCapture {
    let thread_id = thread_id.trim();
    if thread_id.is_empty() {
        return CommandCapture {
            command: "gh api graphql".to_string(),
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("review thread id is required".to_string()),
        };
    }
    let args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        "query=mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{id isResolved}}}".to_string(),
        "-f".to_string(),
        format!("threadId={thread_id}"),
    ];
    run_gh(cwd, &args)
}

pub fn reply_review_thread(cwd: &Path, thread_id: &str, body: &str) -> CommandCapture {
    let thread_id = thread_id.trim();
    let body = body.trim();
    if thread_id.is_empty() {
        return CommandCapture {
            command: "gh api graphql".to_string(),
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("review thread id is required".to_string()),
        };
    }
    if body.is_empty() {
        return CommandCapture {
            command: "gh api graphql".to_string(),
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("review thread reply body is required".to_string()),
        };
    }
    let args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        "query=mutation($threadId:ID!,$body:String!){addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$threadId,body:$body}){comment{id body url}}}".to_string(),
        "-f".to_string(),
        format!("threadId={thread_id}"),
        "-f".to_string(),
        format!("body={body}"),
    ];
    run_gh(cwd, &args)
}

pub fn edit_review_comment(cwd: &Path, comment_id: &str, body: &str) -> CommandCapture {
    let comment_id = comment_id.trim();
    let body = body.trim();
    if comment_id.is_empty() {
        return CommandCapture {
            command: "gh api graphql".to_string(),
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("review comment id is required".to_string()),
        };
    }
    if body.is_empty() {
        return CommandCapture {
            command: "gh api graphql".to_string(),
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("review comment body is required".to_string()),
        };
    }
    let args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        "query=mutation($commentId:ID!,$body:String!){updatePullRequestReviewComment(input:{pullRequestReviewCommentId:$commentId,body:$body}){pullRequestReviewComment{id body url}}}".to_string(),
        "-f".to_string(),
        format!("commentId={comment_id}"),
        "-f".to_string(),
        format!("body={body}"),
    ];
    run_gh(cwd, &args)
}

fn pr_selector(scope: &str) -> Option<String> {
    let trimmed = scope.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let looks_like_pr = first.chars().all(|ch| ch.is_ascii_digit())
        || first.starts_with("http://")
        || first.starts_with("https://")
        || first.contains("/pull/");
    looks_like_pr.then(|| first.to_string())
}

fn pr_view_args(selector: Option<&str>) -> Vec<String> {
    let mut args = vec!["pr".to_string(), "view".to_string()];
    if let Some(selector) = selector {
        args.push(selector.to_string());
    }
    args.extend([
        "--json".to_string(),
        "number,url,headRefName,baseRefName,reviewDecision,comments,reviews,files".to_string(),
    ]);
    args
}

fn pr_checks_args(selector: Option<&str>) -> Vec<String> {
    let mut args = vec!["pr".to_string(), "checks".to_string()];
    if let Some(selector) = selector {
        args.push(selector.to_string());
    }
    args
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrReference {
    owner: String,
    repo: String,
    number: u64,
}

fn pr_reference_from_view(capture: &CommandCapture) -> Option<PrReference> {
    if !capture.success() {
        return None;
    }
    let value: JsonValue = serde_json::from_str(capture.stdout.trim()).ok()?;
    let number = value.get("number")?.as_u64()?;
    let url = value.get("url")?.as_str()?;
    let marker = "github.com/";
    let rest = url.split_once(marker)?.1;
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(PrReference {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

fn pr_review_threads_args(pr: &PrReference) -> Vec<String> {
    vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        "query=query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100){nodes{id isResolved comments(first:20){nodes{id body path line author{login} createdAt}}}}}}}".to_string(),
        "-f".to_string(),
        format!("owner={}", pr.owner),
        "-f".to_string(),
        format!("name={}", pr.repo),
        "-F".to_string(),
        format!("number={}", pr.number),
    ]
}

fn run_gh(cwd: &Path, args: &[String]) -> CommandCapture {
    let command = format!("gh {}", shell_join(args));
    match Command::new("gh").args(args).current_dir(cwd).output() {
        Ok(output) => CommandCapture {
            command,
            status: output.status.code(),
            stdout: truncate_capture(&String::from_utf8_lossy(&output.stdout)),
            stderr: truncate_capture(&String::from_utf8_lossy(&output.stderr)),
            error: None,
        },
        Err(err) => CommandCapture {
            command,
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(err.to_string()),
        },
    }
}

fn render_snapshot_block(snapshot: &PrCommentsSnapshot) -> String {
    let mut out = String::from("Native PR snapshot:\n");
    render_capture(&mut out, "gh pr view", &snapshot.pr_view);
    if let Some(checks) = &snapshot.checks {
        out.push('\n');
        render_capture(&mut out, "gh pr checks", checks);
    }
    if let Some(review_threads) = &snapshot.review_threads {
        out.push('\n');
        render_capture(&mut out, "gh pr review threads", review_threads);
    }
    out
}

fn render_capture(out: &mut String, label: &str, capture: &CommandCapture) {
    out.push_str(&format!(
        "- {label}: `{}` exited {}\n",
        capture.command,
        capture
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "not started".to_string())
    ));
    if let Some(error) = &capture.error {
        out.push_str("  error:\n```text\n");
        out.push_str(error);
        out.push_str("\n```\n");
    }
    if !capture.stdout.trim().is_empty() {
        out.push_str("  stdout:\n```json\n");
        out.push_str(capture.stdout.trim());
        out.push_str("\n```\n");
    }
    if !capture.stderr.trim().is_empty() {
        out.push_str("  stderr:\n```text\n");
        out.push_str(capture.stderr.trim());
        out.push_str("\n```\n");
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg
                .chars()
                .all(|ch| {
                    ch.is_ascii_alphanumeric()
                        || matches!(ch, '-' | '_' | '/' | ':' | '.' | ',')
                })
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_capture(value: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= MAX_CAPTURE_CHARS {
            out.push_str("\n... output truncated ...");
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_accepts_numbers_and_urls_only() {
        assert_eq!(pr_selector("42"), Some("42".to_string()));
        assert_eq!(
            pr_selector("https://github.com/o/r/pull/42 details"),
            Some("https://github.com/o/r/pull/42".to_string())
        );
        assert_eq!(pr_selector("auth flow"), None);
        assert_eq!(pr_selector(""), None);
    }

    #[test]
    fn prompt_includes_snapshot_when_available() {
        let snapshot = PrCommentsSnapshot {
            pr_view: CommandCapture {
                command: "gh pr view 42 --json number".to_string(),
                status: Some(0),
                stdout: r#"{"number":42}"#.to_string(),
                stderr: String::new(),
                error: None,
            },
            checks: Some(CommandCapture {
                command: "gh pr checks 42".to_string(),
                status: Some(1),
                stdout: String::new(),
                stderr: "no checks".to_string(),
                error: None,
            }),
            review_threads: Some(CommandCapture {
                command: "gh api graphql".to_string(),
                status: Some(0),
                stdout: r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":"PRRT_1","isResolved":false}]}}}}}"#.to_string(),
                stderr: String::new(),
                error: None,
            }),
        };
        let prompt = build_pr_comments_prompt("42", Some(&snapshot));
        assert!(prompt.contains("User-requested PR scope: 42"));
        assert!(prompt.contains("Native PR snapshot:"));
        assert!(prompt.contains(r#"{"number":42}"#));
        assert!(prompt.contains("no checks"));
        assert!(prompt.contains("PRRT_1"));
    }

    #[test]
    fn prompt_without_snapshot_preserves_fallback_instructions() {
        let prompt = build_pr_comments_prompt("", None);
        assert!(prompt.contains("Native PR snapshot: not collected."));
        assert!(prompt.contains("infer the current branch"));
        assert!(prompt.contains("gh pr view"));
    }

    #[test]
    fn pr_reference_reads_github_url_from_pr_view() {
        let capture = CommandCapture {
            command: "gh pr view".to_string(),
            status: Some(0),
            stdout: r#"{"number":42,"url":"https://github.com/Libertai/libertai-code-desktop/pull/42"}"#.to_string(),
            stderr: String::new(),
            error: None,
        };
        let pr = pr_reference_from_view(&capture).unwrap();
        assert_eq!(pr.owner, "Libertai");
        assert_eq!(pr.repo, "libertai-code-desktop");
        assert_eq!(pr.number, 42);
    }

    #[test]
    fn review_threads_args_query_pr_threads() {
        let args = pr_review_threads_args(&PrReference {
            owner: "Libertai".to_string(),
            repo: "libertai-code-desktop".to_string(),
            number: 42,
        });
        let joined = args.join(" ");
        assert!(joined.contains("reviewThreads"));
        assert!(joined.contains("owner=Libertai"));
        assert!(joined.contains("name=libertai-code-desktop"));
        assert!(joined.contains("number=42"));
    }

    #[test]
    fn resolve_review_thread_uses_github_graphql_mutation() {
        let capture = resolve_review_thread(Path::new("."), "");
        assert_eq!(capture.error.as_deref(), Some("review thread id is required"));
    }

    #[test]
    fn reply_review_thread_validates_required_fields() {
        let capture = reply_review_thread(Path::new("."), "", "fixed");
        assert_eq!(capture.error.as_deref(), Some("review thread id is required"));
        let capture = reply_review_thread(Path::new("."), "PRRT_1", "");
        assert_eq!(
            capture.error.as_deref(),
            Some("review thread reply body is required")
        );
    }

    #[test]
    fn edit_review_comment_validates_required_fields() {
        let capture = edit_review_comment(Path::new("."), "", "updated");
        assert_eq!(capture.error.as_deref(), Some("review comment id is required"));
        let capture = edit_review_comment(Path::new("."), "PRRC_1", "");
        assert_eq!(
            capture.error.as_deref(),
            Some("review comment body is required")
        );
    }
}
