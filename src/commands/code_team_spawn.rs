//! Team lifecycle: manifest parsing, spawning teammates as background
//! processes, and team status listing.
//!
//! In M3 a "team" is a set of named agents ("teammates") working on
//! related sub-tasks. Each teammate runs as a separate OS process — a
//! detached `libertai code` subprocess launched via
//! [`crate::commands::code_ui::start_background_agent`]. Teams are
//! defined by a TOML manifest at `.libertai/teams/<team-name>.toml` and
//! spawned from the REPL `/team` slash command.
//!
//! This module owns the manifest data model and the spawn/list helpers;
//! the REPL slash-command handler is responsible for user-facing
//! printing around `spawn_team` / `init_team_tasks`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::commands::code_factory::Mode;
use crate::commands::code_team_task::{TeamTask, TeamTaskStatus};
use crate::commands::code_ui::{
    background_agent_run_id, start_background_agent, BackgroundAgentLaunch,
};

/// Parsed team manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamManifest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// `"normal"` | `"accept-edits"` | `"plan"`. Defaults to `"normal"`
    /// at spawn time when unset (see [`parse_mode`]).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    #[serde(rename = "teammate")]
    pub teammates: Vec<TeammateSpec>,
}

/// One teammate definition from the manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct TeammateSpec {
    pub name: String,
    pub agent: String,
    pub task: String,
    /// Per-teammate model override; falls back to the team-level
    /// [`TeamManifest::model`] and then the caller-supplied default.
    #[serde(default)]
    pub model: Option<String>,
}

/// Result of spawning a teammate as a background process.
#[derive(Debug, Clone)]
pub struct SpawnedTeammate {
    pub name: String,
    pub pid: u32,
    pub log_path: PathBuf,
    pub run_id: String,
}

/// Quick-team teammate (from an ad-hoc `"task1 | task2 | ..."` spec).
#[derive(Debug, Clone)]
pub struct QuickTeammate {
    /// `"agent-1"`, `"agent-2"`, …  — sequential, no gaps for empty parts.
    pub name: String,
    pub task: String,
}

/// Parse a team manifest from a TOML string.
///
/// Validates that the manifest defines at least one teammate and that
/// every teammate has a non-empty `name`, `agent`, and `task`.
pub fn parse_manifest(content: &str) -> Result<TeamManifest> {
    let manifest: TeamManifest = toml::from_str(content)
        .map_err(|e| anyhow::anyhow!("team manifest parse error: {e}"))?;
    if manifest.teammates.is_empty() {
        anyhow::bail!("team manifest has no teammates");
    }
    for t in &manifest.teammates {
        if t.name.trim().is_empty() {
            anyhow::bail!("teammate is missing a name");
        }
        if t.agent.trim().is_empty() {
            anyhow::bail!("teammate `{}` is missing an agent", t.name);
        }
        if t.task.trim().is_empty() {
            anyhow::bail!("teammate `{}` is missing a task", t.name);
        }
    }
    Ok(manifest)
}

/// Discover team manifests in a directory. Returns `(team_name,
/// manifest_path)` pairs for every `.toml` file in `.libertai/teams/`,
/// sorted by name. Returns an empty vec if the teams directory does
/// not exist.
pub fn discover_teams(cwd: &Path) -> Result<Vec<(String, PathBuf)>> {
    let dir = cwd.join(".libertai").join("teams");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push((stem.to_string(), path));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Resolve a team by name: looks for `.libertai/teams/<name>.toml`. On
/// miss, bails with a helpful message listing the available teams.
pub fn resolve_team(cwd: &Path, name: &str) -> Result<TeamManifest> {
    let path = cwd
        .join(".libertai")
        .join("teams")
        .join(format!("{name}.toml"));
    if !path.exists() {
        let available = discover_teams(cwd)?
            .into_iter()
            .map(|(n, _)| n)
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "team `{name}` not found at {}\navailable teams: {available}",
            path.display()
        );
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_manifest(&raw)
}

/// Spawn all teammates in a team as background processes. Returns the
/// list of started teammates (`name`, `pid`, `log_path`, `run_id`).
///
/// Per-teammate `model` overrides the team-level [`TeamManifest::model`],
/// which in turn overrides the caller-supplied `model` default. The
/// same precedence applies to `provider`/`mode` (which are team-level
/// only). The `LIBERTAI_TEAM` env var is not threaded through here —
/// the parent process is expected to set it before calling this so
/// the spawned children inherit it.
pub fn spawn_team(
    team_name: &str,
    manifest: &TeamManifest,
    cwd: &Path,
    provider: &str,
    model: &str,
    mode: Mode,
) -> Result<Vec<SpawnedTeammate>> {
    let mut spawned = Vec::with_capacity(manifest.teammates.len());
    for teammate in &manifest.teammates {
        // Teammate model overrides team-level, which overrides the caller default.
        let resolved_model = teammate
            .model
            .as_deref()
            .or(manifest.model.as_deref())
            .unwrap_or(model);
        // Provider and mode are team-level only.
        let resolved_provider = manifest.provider.as_deref().unwrap_or(provider);
        let resolved_mode = parse_mode(manifest.mode.as_deref()).unwrap_or(mode);

        let launch = BackgroundAgentLaunch {
            name: teammate.name.clone(),
            provider: resolved_provider.to_string(),
            model: resolved_model.to_string(),
            mode: resolved_mode,
            prompt: teammate.task.clone(),
            cwd: cwd.to_path_buf(),
            agent: Some(teammate.agent.clone()),
            team: Some(team_name.to_string()),
            teammate_name: Some(teammate.name.clone()),
        };
        let started = start_background_agent(&launch)
            .with_context(|| format!("spawning teammate `{}`", teammate.name))?;
        // run_id needs a timestamp; compute it right after spawn, mirroring
        // code_ui::background_agent_record which calls now_epoch_ms() post-spawn.
        let started_at_ms = now_epoch_ms();
        let run_id = background_agent_run_id(started.pid, started_at_ms);
        spawned.push(SpawnedTeammate {
            name: teammate.name.clone(),
            pid: started.pid,
            log_path: started.log_path,
            run_id,
        });
    }
    Ok(spawned)
}

/// Create an ad-hoc team from a quick spec: `"task1 | task2 | task3"`.
/// Auto-generates teammate names `agent-1`, `agent-2`, … (sequential,
/// skipping empty parts).
pub fn quick_team_spec(spec: &str) -> Vec<QuickTeammate> {
    let mut out = Vec::new();
    let mut n = 0;
    for part in spec.split('|') {
        let task = part.trim();
        if task.is_empty() {
            continue;
        }
        n += 1;
        out.push(QuickTeammate {
            name: format!("agent-{n}"),
            task: task.to_string(),
        });
    }
    out
}

/// Print team status: list all teammates in a team with their names and
/// a clipped one-line task. Used by the `/team status` slash command.
/// We don't yet track which PIDs belong to which team (M3.6), so this
/// just renders the manifest's teammates and points at `libertai agents`.
pub fn print_team_status(team_name: &str, cwd: &Path) -> Result<()> {
    let manifest = resolve_team(cwd, team_name)?;
    println!(
        "Team {team_name} ({} teammates):",
        manifest.teammates.len()
    );
    for t in &manifest.teammates {
        let task = clip(&t.task, 60);
        println!("  {}  \x1b[2m{}\x1b[0m", t.name, task);
    }
    println!("\x1b[2mruns visible via `libertai agents`\x1b[0m");
    Ok(())
}

/// Initialize the shared task list for a team. Creates the team
/// directory `.libertai/teams/<team_name>/` and writes `tasks.jsonl`
/// with one task per teammate (`id` `t1`, `t2`, …, status `pending`,
/// `assignee` = teammate name, `title` = teammate task).
///
/// Returns the team directory path.
pub fn init_team_tasks(
    team_name: &str,
    manifest: &TeamManifest,
    cwd: &Path,
) -> Result<PathBuf> {
    let dir = cwd
        .join(".libertai")
        .join("teams")
        .join(team_name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tasks_path = dir.join("tasks.jsonl");
    let mut content = String::new();
    for (i, t) in manifest.teammates.iter().enumerate() {
        let task = TeamTask {
            id: format!("t{}", i + 1),
            title: t.task.clone(),
            assignee: Some(t.name.clone()),
            status: TeamTaskStatus::Pending,
            notes: Vec::new(),
        };
        let line = serde_json::to_string(&task)
            .with_context(|| format!("serializing task for `{}`", t.name))?;
        content.push_str(&line);
        content.push('\n');
    }
    fs::write(&tasks_path, content)
        .with_context(|| format!("writing {}", tasks_path.display()))?;
    Ok(dir)
}

/// Parse a mode string from the manifest into a [`Mode`]. Accepts a few
/// spelling variants for ergonomics. Returns `None` for unknown values
/// (the caller then falls back to its own default) and for `None`
/// (unset). Empty/whitespace strings are treated as unset.
fn parse_mode(s: Option<&str>) -> Option<Mode> {
    match s.map(str::trim).filter(|v| !v.is_empty()) {
        Some("normal" | "default") => Some(Mode::Normal),
        Some("accept-edits" | "accept_edits" | "accept" | "edits") => Some(Mode::AcceptEdits),
        Some("plan" | "readonly" | "read-only") => Some(Mode::Plan),
        Some(_) => None,
        None => None,
    }
}

/// Truncate `s` to at most `max` chars (Unicode-safe), appending `…`
/// when something was cut. Used for one-line status previews.
fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .nth(max)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}…", &s[..end])
}

/// Current epoch time in milliseconds. Matches `code_ui::now_epoch_ms`
/// (which is private there), used to stamp a `run_id` right after spawn.
fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thin wrapper around `parse_manifest` for test readability.
    fn manifest_from_toml(content: &str) -> Result<TeamManifest> {
        parse_manifest(content)
    }

    #[test]
    fn parse_mode_variants() {
        assert_eq!(parse_mode(Some("normal")), Some(Mode::Normal));
        assert_eq!(parse_mode(Some("default")), Some(Mode::Normal));
        assert_eq!(parse_mode(Some("accept-edits")), Some(Mode::AcceptEdits));
        assert_eq!(parse_mode(Some("accept_edits")), Some(Mode::AcceptEdits));
        assert_eq!(parse_mode(Some("accept")), Some(Mode::AcceptEdits));
        assert_eq!(parse_mode(Some("edits")), Some(Mode::AcceptEdits));
        assert_eq!(parse_mode(Some("plan")), Some(Mode::Plan));
        assert_eq!(parse_mode(Some("readonly")), Some(Mode::Plan));
        assert_eq!(parse_mode(Some("read-only")), Some(Mode::Plan));
        assert_eq!(parse_mode(Some("bogus")), None);
        assert_eq!(parse_mode(None), None);
        // Empty / whitespace are treated as unset.
        assert_eq!(parse_mode(Some("")), None);
        assert_eq!(parse_mode(Some("   ")), None);
        // Surrounding whitespace is trimmed.
        assert_eq!(parse_mode(Some("  normal  ")), Some(Mode::Normal));
    }

    #[test]
    fn quick_team_spec_three() {
        let v = quick_team_spec("task1 | task2 | task3");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].name, "agent-1");
        assert_eq!(v[0].task, "task1");
        assert_eq!(v[1].name, "agent-2");
        assert_eq!(v[1].task, "task2");
        assert_eq!(v[2].name, "agent-3");
        assert_eq!(v[2].task, "task3");
    }

    #[test]
    fn quick_team_spec_trims() {
        let v = quick_team_spec("  spaced  |  other  ");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].task, "spaced");
        assert_eq!(v[1].task, "other");
    }

    #[test]
    fn quick_team_spec_empty() {
        assert!(quick_team_spec("").is_empty());
    }

    #[test]
    fn quick_team_spec_single() {
        let v = quick_team_spec("only one");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "agent-1");
        assert_eq!(v[0].task, "only one");
    }

    #[test]
    fn quick_team_spec_skips_empty_parts() {
        // Empty parts are dropped and numbering stays sequential.
        let v = quick_team_spec("a |  | c |   ");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "agent-1");
        assert_eq!(v[0].task, "a");
        assert_eq!(v[1].name, "agent-2");
        assert_eq!(v[1].task, "c");
    }

    #[test]
    fn manifest_valid() {
        let toml = r#"
model = "glm-5.2"
[[teammate]]
name = "alice"
agent = "coder"
task = "Refactor the parser"
[[teammate]]
name = "bob"
agent = "coder"
task = "Wire the event system"
model = "claude-sonnet"
"#;
        let m = manifest_from_toml(toml).expect("parses valid manifest");
        assert_eq!(m.model.as_deref(), Some("glm-5.2"));
        assert!(m.provider.is_none());
        assert!(m.mode.is_none());
        assert_eq!(m.teammates.len(), 2);
        assert_eq!(m.teammates[0].name, "alice");
        assert_eq!(m.teammates[0].agent, "coder");
        assert_eq!(m.teammates[0].task, "Refactor the parser");
        assert!(m.teammates[0].model.is_none());
        assert_eq!(m.teammates[1].name, "bob");
        assert_eq!(m.teammates[1].model.as_deref(), Some("claude-sonnet"));
    }

    #[test]
    fn manifest_empty_teammates_errors() {
        let toml = r#"
model = "glm-5.2"
"#;
        assert!(manifest_from_toml(toml).is_err());
    }

    #[test]
    fn manifest_missing_fields_errors() {
        // `task` is a required teammate field; omitting it fails TOML parsing.
        let toml = r#"
[[teammate]]
name = "solo"
agent = "coder"
"#;
        assert!(manifest_from_toml(toml).is_err());
    }

    #[test]
    fn manifest_blank_task_errors() {
        // Present but blank — caught by our validation, not the parser.
        let toml = r#"
[[teammate]]
name = "solo"
agent = "coder"
task = ""
"#;
        assert!(manifest_from_toml(toml).is_err());
    }

    #[test]
    fn clip_short_and_long() {
        assert_eq!(clip("hello", 60), "hello");
        let long: String = "x".repeat(100);
        let c = clip(&long, 60);
        assert_eq!(c.chars().count(), 61); // 60 chars + ellipsis
        assert!(c.ends_with('…'));
    }
}
