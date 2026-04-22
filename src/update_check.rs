//! Passive update check: on startup, look up the latest release from GitHub
//! at most once every 24h (in a background thread so we never block `main`),
//! cache the result next to `config.toml`, and on a *later* startup print a
//! one-line stderr banner if the cached latest version is newer than what
//! we're running. Follows the `rustup` / `gh` pattern.
//!
//! There is no self-replacing `libertai update` subcommand — updates flow
//! through the system package manager (apt / brew) or `cargo install`.

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/Libertai/libertai-cli/releases/latest";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    last_check_unix: u64,
    latest_version_seen: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
}

/// Called once from the CLI dispatcher at startup. Cheap (reads a small
/// JSON file) and may spawn a detached background thread to refresh the
/// cache; never blocks.
pub fn maybe_notify(cfg: &Config, subcommand: &str) {
    if !should_check(cfg, subcommand) {
        return;
    }
    let cache = read_cache();
    if let Some(ref c) = cache {
        let current = env!("CARGO_PKG_VERSION");
        if is_strictly_newer(&c.latest_version_seen, current) {
            print_banner(&c.latest_version_seen, current);
        }
    }
    let stale = cache
        .as_ref()
        .map(|c| now_unix().saturating_sub(c.last_check_unix) >= CHECK_INTERVAL_SECS)
        .unwrap_or(true);
    if stale {
        spawn_refresh();
    }
}

fn should_check(cfg: &Config, subcommand: &str) -> bool {
    if !cfg.check_for_updates {
        return false;
    }
    if env::var_os("NO_UPDATE_CHECK").is_some() {
        return false;
    }
    if env::var_os("CI").is_some() {
        return false;
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    // Skip on commands that own their own terminal flow — we don't want a
    // banner in the middle of a password prompt or a `config show` dump.
    !matches!(subcommand, "login" | "logout" | "config")
}

fn spawn_refresh() {
    thread::spawn(|| {
        let _ = refresh();
    });
}

fn refresh() -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp: ReleaseResponse = client
        .get(LATEST_RELEASE_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()?
        .error_for_status()?
        .json()?;
    let latest = resp.tag_name.trim_start_matches('v').to_string();
    write_cache(&Cache {
        last_check_unix: now_unix(),
        latest_version_seen: latest,
    })
}

fn cache_path() -> Result<PathBuf> {
    let cfg = config::config_path()?;
    let parent = cfg
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent"))?;
    Ok(parent.join("update-check.json"))
}

fn read_cache() -> Option<Cache> {
    let raw = fs::read_to_string(cache_path().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(cache: &Cache) -> Result<()> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        config::create_dir_secure(parent)?;
    }
    let raw = serde_json::to_string_pretty(cache)?;
    config::write_file_secure(&path, raw.as_bytes())?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn print_banner(latest: &str, current: &str) {
    let hint = upgrade_hint();
    eprintln!(
        "{} libertai {} is available (you have {}). {}",
        "→".yellow().bold(),
        latest.bold(),
        current,
        hint.dimmed(),
    );
}

fn upgrade_hint() -> String {
    let exe = env::current_exe().ok();
    let p = exe.as_ref().and_then(|p| p.to_str()).unwrap_or("");
    if p.contains("/.cargo/bin/") {
        "Run: cargo install libertai-cli --force".into()
    } else if p.contains("/Cellar/") || p.contains("/opt/homebrew/") {
        "Run: brew upgrade libertai".into()
    } else if p.starts_with("/usr/bin/") {
        "Run: sudo apt upgrade libertai-cli".into()
    } else {
        "See: https://github.com/Libertai/libertai-cli/releases/latest".into()
    }
}

// ─── Semver-ish comparison (major.minor.patch[-prerelease]) ───────────────
// Hand-rolled to honour the "no new deps" direction. Ignores build metadata
// (`+...`) because we never publish it. Stable > prerelease; prerelease
// strings compare lexicographically (rc1 < rc2; good enough for our flow).

fn is_strictly_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(r)) => cmp_versions(&c, &r) == std::cmp::Ordering::Greater,
        _ => false,
    }
}

fn parse_version(s: &str) -> Option<(u32, u32, u32, Option<String>)> {
    let s = s.trim().trim_start_matches('v');
    let (base, pre) = match s.find('-') {
        Some(i) => (&s[..i], Some(s[i + 1..].to_string())),
        None => (s, None),
    };
    let mut parts = base.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, pre))
}

fn cmp_versions(
    a: &(u32, u32, u32, Option<String>),
    b: &(u32, u32, u32, Option<String>),
) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)) {
        Equal => match (&a.3, &b.3) {
            (None, None) => Equal,
            (None, Some(_)) => Greater,
            (Some(_), None) => Less,
            (Some(x), Some(y)) => x.cmp(y),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_major() {
        assert!(is_strictly_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn newer_patch() {
        assert!(is_strictly_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn stable_newer_than_prerelease() {
        assert!(is_strictly_newer("0.2.0", "0.2.0-rc1"));
    }

    #[test]
    fn prerelease_older_than_stable() {
        assert!(!is_strictly_newer("0.2.0-rc1", "0.2.0"));
    }

    #[test]
    fn rc_ordering() {
        assert!(is_strictly_newer("0.2.0-rc2", "0.2.0-rc1"));
    }

    #[test]
    fn equal_is_not_newer() {
        assert!(!is_strictly_newer("0.2.0", "0.2.0"));
    }

    #[test]
    fn v_prefix_ok() {
        assert!(is_strictly_newer("v0.2.0", "0.1.0"));
    }

    #[test]
    fn garbage_does_not_panic() {
        assert!(!is_strictly_newer("garbage", "0.1.0"));
        assert!(!is_strictly_newer("0.1.0", "garbage"));
    }
}
