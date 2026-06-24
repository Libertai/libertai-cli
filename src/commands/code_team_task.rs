//! The `team_task` tool — shared task list for M3 teammates.
//!
//! In M3 a "team" is a set of named agents working on related sub-tasks,
//! each running as a separate OS process (a detached `libertai code`
//! subprocess). Teammates share a task list persisted as JSONL on disk
//! (one `TeamTask` per line) at `<team_dir>/tasks.jsonl`. This tool lets
//! each teammate read and update that shared list.
//!
//! It mirrors the single-process `todo` tool ([`crate::commands::code_todo`])
//! with two key differences:
//!   1. The list is persisted as JSONL on disk, not just rendered to
//!      stderr — so separate processes see each other's updates.
//!   2. Each task carries an `assignee` (a teammate name) so teammates
//!      can see who is working on what.
//!
//! Three operations:
//!   - `list`   — read all tasks and return them as text.
//!   - `update` — change a task's status and/or append notes by id.
//!   - `claim`  — set yourself as the assignee and mark the task active.
//!
//! Writes go through a temp file + rename so a crashed teammate never
//! leaves a half-written list (atomic on Unix).

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

const NAME: &str = "team_task";
const LABEL: &str = "TeamTask";
const DESCRIPTION: &str = concat!(
    "Read and update the shared team task list. Use `list` to see all ",
    "tasks with their assignees and statuses. Use `update` to change a ",
    "task's status (pending/active/completed/blocked) or add notes. Use ",
    "`claim` to assign a task to yourself and mark it active. The task ",
    "list is shared across all teammates, so check it frequently to avoid ",
    "duplicate work."
);

/// Lifecycle state of one team task. Mirrors the `todo` tool's status
/// set plus a `blocked` state for work that's stuck on an external
/// dependency (another teammate, a missing API, …).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TeamTaskStatus {
    Pending,
    Active,
    Completed,
    Blocked,
}

/// One entry in the shared team task list. Stored as a single JSON line
/// inside `<team_dir>/tasks.jsonl`; `id` is the stable key teammates
/// reference from `update`/`claim`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub title: String,
    pub assignee: Option<String>,
    pub status: TeamTaskStatus,
    pub notes: Vec<String>,
}

/// Parsed payload of a `team_task` call. `action` selects the
/// operation; the remaining fields are conditionally required
/// (`task_id` for `update`/`claim`, `status`/`notes` for `update`).
#[derive(Debug, Deserialize)]
struct TeamTaskInput {
    action: String,
    task_id: Option<String>,
    status: Option<String>,
    notes: Option<String>,
}

/// The `team_task` tool. Bound to one teammate (by name) and one team
/// directory on disk; the directory is something like
/// `.libertai/teams/<team-name>/`.
pub struct TeamTaskTool {
    team_dir: PathBuf,
    teammate_name: String,
}

impl TeamTaskTool {
    pub fn new(team_dir: PathBuf, teammate_name: String) -> Self {
        Self {
            team_dir,
            teammate_name,
        }
    }

    /// Path to the JSONL task list for this team.
    fn tasks_path(&self) -> PathBuf {
        self.team_dir.join("tasks.jsonl")
    }

    /// Read all tasks from disk. A missing file is treated as an empty
    /// list (the team hasn't created any tasks yet) rather than an
    /// error, so `list` works before the first write.
    fn read_tasks(&self) -> Result<Vec<TeamTask>> {
        let path = self.tasks_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read team tasks {}", path.display()))?;
        let mut tasks = Vec::new();
        for (i, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let task: TeamTask = serde_json::from_str(line)
                .with_context(|| format!("parse team task on line {}", i + 1))?;
            tasks.push(task);
        }
        Ok(tasks)
    }

    /// Write the full task list back atomically: serialize each task to
    /// one JSON line, write to `tasks.jsonl.tmp` in the same directory,
    /// then `rename` over the real path. On Unix the rename is atomic,
    /// so a crash mid-write never corrupts the list teammates see.
    fn write_tasks(&self, tasks: &[TeamTask]) -> Result<()> {
        std::fs::create_dir_all(&self.team_dir)
            .with_context(|| format!("create team dir {}", self.team_dir.display()))?;
        let path = self.tasks_path();
        let tmp = self.team_dir.join("tasks.jsonl.tmp");
        let mut out = String::new();
        for task in tasks {
            // `to_string` uses compact JSON (no newlines), keeping each
            // task on exactly one line — the JSONL invariant.
            let line = serde_json::to_string(task)
                .with_context(|| format!("serialize task {}", task.id))?;
            out.push_str(&line);
            out.push('\n');
        }
        std::fs::write(&tmp, &out)
            .with_context(|| format!("write temp tasks {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

impl Default for TeamTaskTool {
    fn default() -> Self {
        Self::new(PathBuf::new(), String::new())
    }
}

#[async_trait]
impl Tool for TeamTaskTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "update", "claim"],
                    "description": "Operation to perform."
                },
                "task_id": {
                    "type": "string",
                    "description": "The task id (for update/claim)."
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "active", "completed", "blocked"],
                    "description": "New status (for update)."
                },
                "notes": {
                    "type": "string",
                    "description": "Additional notes to append to the task (for update)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: TeamTaskInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!("invalid `team_task` payload: {e}")));
            }
        };

        match parsed.action.as_str() {
            "list" => {
                let tasks = match self.read_tasks() {
                    Ok(t) => t,
                    Err(e) => return Ok(err_output(&format!("read tasks: {e}"))),
                };
                Ok(text_output(&render_task_list(&tasks)))
            }
            "update" => {
                let task_id = match parsed.task_id {
                    Some(id) => id,
                    None => return Ok(err_output("`update` requires `task_id`")),
                };
                // Validate the status string up front so we never write
                // back a half-applied update for a bad enum value.
                let new_status = match parsed.status.as_deref() {
                    None => None,
                    Some(s) => match parse_status(s) {
                        Some(st) => Some(st),
                        None => return Ok(err_output(&format!("invalid `status`: {s}"))),
                    },
                };
                let mut tasks = match self.read_tasks() {
                    Ok(t) => t,
                    Err(e) => return Ok(err_output(&format!("read tasks: {e}"))),
                };
                let task = match tasks.iter_mut().find(|t| t.id == task_id) {
                    Some(t) => t,
                    None => return Ok(err_output(&format!("task not found: {task_id}"))),
                };
                update_task(task, new_status, parsed.notes.as_deref());
                let summary = render_task(task);
                if let Err(e) = self.write_tasks(&tasks) {
                    return Ok(err_output(&format!("write tasks: {e}")));
                }
                Ok(text_output(&format!("Updated task {task_id}:\n{summary}")))
            }
            "claim" => {
                let task_id = match parsed.task_id {
                    Some(id) => id,
                    None => return Ok(err_output("`claim` requires `task_id`")),
                };
                let mut tasks = match self.read_tasks() {
                    Ok(t) => t,
                    Err(e) => return Ok(err_output(&format!("read tasks: {e}"))),
                };
                let task = match tasks.iter_mut().find(|t| t.id == task_id) {
                    Some(t) => t,
                    None => return Ok(err_output(&format!("task not found: {task_id}"))),
                };
                claim_task(task, &self.teammate_name);
                let summary = render_task(task);
                if let Err(e) = self.write_tasks(&tasks) {
                    return Ok(err_output(&format!("write tasks: {e}")));
                }
                Ok(text_output(&format!("Claimed task {task_id}:\n{summary}")))
            }
            other => Ok(err_output(&format!("unknown `action`: {other}"))),
        }
    }

    fn is_read_only(&self) -> bool {
        // `update`/`claim` write the JSONL list to disk, so this is
        // not safe to run in parallel with other write tools.
        false
    }
}

// ---- pure helpers (unit-tested, no file I/O) ----

/// Render the whole list as the text returned by `list`. An empty list
/// (missing file or no tasks yet) renders as `"No tasks yet."`.
fn render_task_list(tasks: &[TeamTask]) -> String {
    if tasks.is_empty() {
        return "No tasks yet.".to_string();
    }
    let total = tasks.len();
    let completed = count_status(tasks, TeamTaskStatus::Completed);
    let active = count_status(tasks, TeamTaskStatus::Active);
    let pending = count_status(tasks, TeamTaskStatus::Pending);
    let blocked = count_status(tasks, TeamTaskStatus::Blocked);

    // Only mention non-zero buckets, in display order completed →
    // active → pending → blocked, matching the example in the spec.
    let mut segments: Vec<String> = Vec::new();
    if completed > 0 {
        segments.push(format!("{completed} completed"));
    }
    if active > 0 {
        segments.push(format!("{active} active"));
    }
    if pending > 0 {
        segments.push(format!("{pending} pending"));
    }
    if blocked > 0 {
        segments.push(format!("{blocked} blocked"));
    }
    let task_word = if total == 1 { "task" } else { "tasks" };
    let header = if segments.is_empty() {
        format!("Team task list ({total} {task_word}):")
    } else {
        format!(
            "Team task list ({total} {task_word}: {}):",
            segments.join(", ")
        )
    };

    let mut out = header;
    for task in tasks {
        out.push('\n');
        out.push_str(&render_task_line(task));
    }
    out
}

/// One indented line per task, e.g.
/// `  [completed] t1: Refactor parser — assigned to alice`.
/// The bracketed status is left-aligned to the width of the longest
/// label (`[completed]` = 11 chars) so the id columns line up.
fn render_task_line(task: &TeamTask) -> String {
    let label = format!("[{}]", status_label(task.status));
    let assignee = match &task.assignee {
        Some(name) => format!("assigned to {name}"),
        None => "unassigned".to_string(),
    };
    format!("  {:<11} {}: {} — {}", label, task.id, task.title, assignee)
}

/// Render a single task with its accumulated notes, for the
/// `update`/`claim` result messages.
fn render_task(task: &TeamTask) -> String {
    let mut out = render_task_line(task);
    if !task.notes.is_empty() {
        out.push_str("\n  Notes:");
        for note in &task.notes {
            out.push_str(&format!("\n    - {note}"));
        }
    }
    out
}

fn status_label(status: TeamTaskStatus) -> &'static str {
    match status {
        TeamTaskStatus::Pending => "pending",
        TeamTaskStatus::Active => "active",
        TeamTaskStatus::Completed => "completed",
        TeamTaskStatus::Blocked => "blocked",
    }
}

fn count_status(tasks: &[TeamTask], status: TeamTaskStatus) -> usize {
    tasks.iter().filter(|t| t.status == status).count()
}

/// Parse a status string (case-insensitive). Returns `None` for an
/// unknown value so the caller can surface a clean error instead of
/// silently defaulting.
fn parse_status(s: &str) -> Option<TeamTaskStatus> {
    match s.trim().to_ascii_lowercase().as_str() {
        "pending" => Some(TeamTaskStatus::Pending),
        "active" => Some(TeamTaskStatus::Active),
        "completed" => Some(TeamTaskStatus::Completed),
        "blocked" => Some(TeamTaskStatus::Blocked),
        _ => None,
    }
}

/// Apply an `update` to a task in place: set the status if provided,
/// and append a (trimmed, non-empty) note if provided.
fn update_task(task: &mut TeamTask, new_status: Option<TeamTaskStatus>, notes: Option<&str>) {
    if let Some(status) = new_status {
        task.status = status;
    }
    if let Some(note) = notes {
        let note = note.trim();
        if !note.is_empty() {
            task.notes.push(note.to_string());
        }
    }
}

/// Claim a task: set yourself as the assignee and mark it active.
fn claim_task(task: &mut TeamTask, teammate: &str) {
    task.assignee = Some(teammate.to_string());
    task.status = TeamTaskStatus::Active;
}

// ---- tool output constructors ----

fn text_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: false,
    }
    .into()
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(
        id: &str,
        title: &str,
        assignee: Option<&str>,
        status: TeamTaskStatus,
        notes: Vec<&str>,
    ) -> TeamTask {
        TeamTask {
            id: id.to_string(),
            title: title.to_string(),
            assignee: assignee.map(|s| s.to_string()),
            status,
            notes: notes.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ---- render_task_list ----

    #[test]
    fn render_empty_list_says_no_tasks_yet() {
        assert_eq!(render_task_list(&[]), "No tasks yet.");
    }

    #[test]
    fn render_mixed_statuses_and_assignees_matches_spec_shape() {
        let tasks = [
            task(
                "t1",
                "Refactor parser",
                Some("alice"),
                TeamTaskStatus::Completed,
                vec![],
            ),
            task(
                "t2",
                "Wire new event",
                Some("bob"),
                TeamTaskStatus::Active,
                vec![],
            ),
            task(
                "t3",
                "Bench the fallback",
                None,
                TeamTaskStatus::Pending,
                vec![],
            ),
            task("t4", "Write docs", None, TeamTaskStatus::Pending, vec![]),
        ];
        let out = render_task_list(&tasks);
        assert!(
            out.contains("Team task list (4 tasks: 1 completed, 1 active, 2 pending):"),
            "header wrong: {out}"
        );
        // Exact line shapes, including the alignment padding that puts
        // the id column at a fixed offset.
        assert!(
            out.contains("  [completed] t1: Refactor parser — assigned to alice"),
            "t1 line wrong: {out}"
        );
        assert!(
            out.contains("  [active]    t2: Wire new event — assigned to bob"),
            "t2 line wrong: {out}"
        );
        assert!(
            out.contains("  [pending]   t3: Bench the fallback — unassigned"),
            "t3 line wrong: {out}"
        );
        assert!(
            out.contains("  [pending]   t4: Write docs — unassigned"),
            "t4 line wrong: {out}"
        );
    }

    #[test]
    fn render_blocked_status_and_counts() {
        let tasks = [
            task("t1", "A", None, TeamTaskStatus::Active, vec![]),
            task("t2", "B", None, TeamTaskStatus::Blocked, vec![]),
            task("t3", "C", None, TeamTaskStatus::Blocked, vec![]),
        ];
        let out = render_task_list(&tasks);
        assert!(
            out.contains("Team task list (3 tasks: 1 active, 2 blocked):"),
            "blocked count header wrong: {out}"
        );
        assert!(
            out.contains("  [blocked]   t2: B — unassigned"),
            "blocked line: {out}"
        );
    }

    #[test]
    fn render_single_task_uses_singular_word() {
        let tasks = [task(
            "t1",
            "Solo",
            Some("alice"),
            TeamTaskStatus::Active,
            vec![],
        )];
        let out = render_task_list(&tasks);
        assert!(out.contains("(1 task:"), "singular header wrong: {out}");
    }

    // ---- parse_status ----

    #[test]
    fn parse_status_known_values_case_insensitive() {
        assert_eq!(parse_status("pending"), Some(TeamTaskStatus::Pending));
        assert_eq!(parse_status("ACTIVE"), Some(TeamTaskStatus::Active));
        assert_eq!(parse_status("Completed"), Some(TeamTaskStatus::Completed));
        assert_eq!(parse_status("blocked"), Some(TeamTaskStatus::Blocked));
        // surrounding whitespace is tolerated.
        assert_eq!(parse_status("  active "), Some(TeamTaskStatus::Active));
    }

    #[test]
    fn parse_status_rejects_unknown() {
        assert_eq!(parse_status("done"), None);
        assert_eq!(parse_status("in progress"), None);
        assert_eq!(parse_status(""), None);
    }

    // ---- update_task ----

    #[test]
    fn update_task_changes_status() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(&mut t, Some(TeamTaskStatus::Completed), None);
        assert_eq!(t.status, TeamTaskStatus::Completed);
        assert!(t.notes.is_empty(), "no notes should be appended");
    }

    #[test]
    fn update_task_appends_note() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec!["prior"]);
        update_task(&mut t, None, Some("hit a snag"));
        assert_eq!(t.status, TeamTaskStatus::Pending, "status unchanged");
        assert_eq!(t.notes, vec!["prior".to_string(), "hit a snag".to_string()]);
    }

    #[test]
    fn update_task_changes_status_and_appends_note() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(
            &mut t,
            Some(TeamTaskStatus::Blocked),
            Some("waiting on API"),
        );
        assert_eq!(t.status, TeamTaskStatus::Blocked);
        assert_eq!(t.notes, vec!["waiting on API".to_string()]);
    }

    #[test]
    fn update_task_ignores_empty_note() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(&mut t, None, Some("   "));
        assert!(t.notes.is_empty(), "whitespace-only note was appended");
    }

    // ---- claim_task ----

    #[test]
    fn claim_task_sets_assignee_and_active() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        claim_task(&mut t, "alice");
        assert_eq!(t.assignee.as_deref(), Some("alice"));
        assert_eq!(t.status, TeamTaskStatus::Active);
    }

    #[test]
    fn claim_task_overwrites_existing_assignee() {
        let mut t = task("t1", "X", Some("bob"), TeamTaskStatus::Completed, vec![]);
        claim_task(&mut t, "alice");
        assert_eq!(t.assignee.as_deref(), Some("alice"));
        assert_eq!(t.status, TeamTaskStatus::Active);
    }

    // ---- render_task (notes surface in update/claim results) ----

    #[test]
    fn render_task_includes_notes_section() {
        let t = task(
            "t1",
            "X",
            Some("alice"),
            TeamTaskStatus::Active,
            vec!["one", "two"],
        );
        let out = render_task(&t);
        assert!(
            out.contains("  [active]    t1: X — assigned to alice"),
            "line: {out}"
        );
        assert!(out.contains("  Notes:"), "notes header missing: {out}");
        assert!(out.contains("    - one"), "note one missing: {out}");
        assert!(out.contains("    - two"), "note two missing: {out}");
    }
}
