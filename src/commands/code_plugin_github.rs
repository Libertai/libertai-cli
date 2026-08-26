//! GitHub commit-signature verification — an identity anchor for
//! GitHub-hosted plugins, complementing the wallet content signature.
//!
//! When a plugin's source is a GitHub repo pinned to a commit SHA, we ask the
//! GitHub API whether that commit is signature-**verified** (the same
//! "Verified" badge GitHub shows). Because the SHA covers the whole tree,
//! a verified commit binds the installed content to a key GitHub attributes to
//! a real GitHub account — the identity anchor a bare wallet signature lacks.
//!
//! Honest scope: this only applies to GitHub sources with a pinned SHA, needs
//! network access, and moves the trust root to GitHub. For every other case
//! (GitLab, archives, local paths, offline) the wallet signature in
//! [`super::code_plugin_sign`] remains the mechanism.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Whether a plugin's pinned GitHub commit is signature-verified by GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GithubVerification {
    /// Not a GitHub source, or no SHA to check — nothing to verify here.
    NotApplicable,
    /// The check could not be completed (offline, rate-limited, not found).
    Unknown(String),
    /// GitHub reports the commit signature as NOT verified (`reason`).
    Unverified(String),
    /// GitHub verified the commit signature. `login` is the GitHub account it
    /// is attributed to (when known); `reason` is GitHub's status reason.
    Verified {
        login: Option<String>,
        reason: String,
    },
}

impl GithubVerification {
    /// One-line label for the audit/install display.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            GithubVerification::NotApplicable => "n/a".to_string(),
            GithubVerification::Unknown(why) => format!("unknown ({why})"),
            GithubVerification::Unverified(reason) => format!("not verified ({reason})"),
            GithubVerification::Verified {
                login: Some(login), ..
            } => format!("verified via GitHub (@{login})"),
            GithubVerification::Verified {
                login: None,
                reason,
            } => {
                format!("verified via GitHub ({reason})")
            }
        }
    }

    /// Whether GitHub confirms the commit signature is verified.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self, GithubVerification::Verified { .. })
    }
}

/// Query the GitHub API for the verification status of `repo`'s commit `sha`.
/// `repo` is `owner/name`. Never errors — any failure maps to
/// [`GithubVerification::Unknown`], since this is advisory. Honors
/// `GITHUB_TOKEN`/`GH_TOKEN` for higher rate limits and private repos.
#[must_use]
pub fn verify_github_commit(repo: &str, sha: &str) -> GithubVerification {
    // `repo` comes from the trusted marketplace manifest and `sha` is a hex
    // commit id from `git rev-parse HEAD` (not arbitrary user input), so
    // direct interpolation is safe — same trust posture as `materialize_plugin`.
    let url = format!("https://api.github.com/repos/{repo}/commits/{sha}");
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(e) => return GithubVerification::Unknown(e.to_string()),
    };

    let mut req = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .filter(|t| !t.is_empty())
    {
        req = req.bearer_auth(token);
    }

    let resp = match req
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
    {
        Ok(resp) => resp,
        Err(e) => return GithubVerification::Unknown(e.to_string()),
    };
    match resp.json::<serde_json::Value>() {
        Ok(value) => parse_commit_verification(&value),
        Err(e) => GithubVerification::Unknown(e.to_string()),
    }
}

/// Classify a GitHub `GET /repos/{o}/{r}/commits/{sha}` response body. Split
/// out from the HTTP call so it is testable without the network.
fn parse_commit_verification(value: &serde_json::Value) -> GithubVerification {
    let verification = value.get("commit").and_then(|c| c.get("verification"));
    let verified = verification
        .and_then(|v| v.get("verified"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let reason = verification
        .and_then(|v| v.get("reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let login = value
        .get("author")
        .and_then(|a| a.get("login"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if verified {
        GithubVerification::Verified { login, reason }
    } else {
        GithubVerification::Unverified(reason)
    }
}

/// Parse `owner/name` out of a git URL when it points at **github.com**; used
/// to extend GitHub verification to `url`-type sources, not only `github` ones.
/// GitHub Enterprise hosts (e.g. `github.company.com`) are intentionally not
/// matched — verification targets the public `api.github.com` only.
#[must_use]
pub fn github_repo_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("ssh://github.com/"))?;
    // Strip a trailing slash, then exactly one `.git` suffix (strip_suffix,
    // not trim_end_matches, so a repo legitimately named `git` is preserved).
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    if parts.next().is_some() {
        return None; // more than owner/name — a subpath, not a repo root
    }
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verified_commit_with_login() {
        let json = serde_json::json!({
            "commit": { "verification": { "verified": true, "reason": "valid" } },
            "author": { "login": "ayghri" }
        });
        match parse_commit_verification(&json) {
            GithubVerification::Verified { login, reason } => {
                assert_eq!(login.as_deref(), Some("ayghri"));
                assert_eq!(reason, "valid");
            }
            other => panic!("expected Verified, got {other:?}"),
        }
        assert!(parse_commit_verification(&json).is_verified());
    }

    #[test]
    fn verified_without_login_labels_by_reason() {
        // GitHub's `author` is null when the committer email matches no account.
        let json = serde_json::json!({
            "commit": { "verification": { "verified": true, "reason": "valid" } },
            "author": serde_json::Value::Null
        });
        let status = parse_commit_verification(&json);
        assert!(matches!(
            status,
            GithubVerification::Verified { login: None, .. }
        ));
        let label = status.label();
        assert!(label.contains("verified via GitHub"), "{label}");
        assert!(label.contains("valid"), "{label}");
    }

    #[test]
    fn parses_unverified_commit() {
        let json = serde_json::json!({
            "commit": { "verification": { "verified": false, "reason": "unsigned" } },
            "author": serde_json::Value::Null
        });
        assert_eq!(
            parse_commit_verification(&json),
            GithubVerification::Unverified("unsigned".to_string())
        );
    }

    #[test]
    fn missing_verification_field_is_unverified() {
        let json = serde_json::json!({ "commit": {} });
        assert!(matches!(
            parse_commit_verification(&json),
            GithubVerification::Unverified(_)
        ));
    }

    #[test]
    fn label_covers_all_variants() {
        assert_eq!(GithubVerification::NotApplicable.label(), "n/a");
        assert!(GithubVerification::Unknown("rate limited".to_string())
            .label()
            .contains("unknown"));
        assert!(GithubVerification::Unverified("unsigned".to_string())
            .label()
            .contains("not verified"));
    }

    #[test]
    fn repo_named_git_is_preserved() {
        assert_eq!(
            github_repo_from_url("https://github.com/owner/git"),
            Some("owner/git".to_string())
        );
        assert_eq!(
            github_repo_from_url("https://github.com/owner/git.git"),
            Some("owner/git".to_string())
        );
    }

    #[test]
    fn extracts_repo_from_github_urls() {
        assert_eq!(
            github_repo_from_url("https://github.com/ayghri/i-have-adhd.git"),
            Some("ayghri/i-have-adhd".to_string())
        );
        assert_eq!(
            github_repo_from_url("git@github.com:owner/repo"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            github_repo_from_url("ssh://github.com/owner/repo.git"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            github_repo_from_url("https://gitlab.com/owner/repo.git"),
            None
        );
        // A subpath is not a repo root.
        assert_eq!(
            github_repo_from_url("https://github.com/owner/repo/tree/x"),
            None
        );
    }
}
