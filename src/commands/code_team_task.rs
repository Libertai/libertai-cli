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
//!   - `link`   — add `blocks`/`blockedBy` edges between tasks (M5/#19),
//!     so parallel teammates can coordinate non-overlapping ready work
//!     without a coordination prompt.
//!
//! ## Task graph (M5/#19)
//!
//! Each task carries optional `blocks` (ids this task blocks from
//! starting) and `blocked_by` (ids that must complete before this task
//! is ready). A task is **ready** (unblocked) when every id in
//! `blocked_by` refers to a `Completed` task (or `blocked_by` is empty).
//! `render_task_line` shows a `ready`/`blocked` marker so a teammate
//! scanning the list can claim non-overlapping ready work. The flat
//! `todo` tool is untouched — this is the team-only dependency layer.
//!
//! Writes go through a temp file + rename so a crashed teammate never
//! leaves a half-written list (atomic on Unix).

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use pi::tools::ToolEffects;

const NAME: &str = "team_task";
const LABEL: &str = "TeamTask";
const DESCRIPTION: &str = concat!(
    "Read and update the shared team task list. Use `list` to see all ",
    "tasks with their assignees, statuses, and ready/blocked markers. ",
    "Use `update` to change a task's status (pending/active/completed/",
    "blocked), set its owner, or add notes. Use `claim` to assign a ",
    "task to yourself and mark it active. Use `link` to add `blocks`/",
    "`blockedBy` edges so teammates pick non-overlapping ready work. A ",
    "task is ready when every task in its `blockedBy` is completed. The ",
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
/// reference from `update`/`claim`/`link`.
///
/// `blocks` / `blocked_by` / `owner` (M5/#19) are `#[serde(default)]`
/// so JSONL lines written before the fields existed still deserialize
/// (an old list loaded, re-saved, then re-loaded round-trips).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub title: String,
    pub assignee: Option<String>,
    pub status: TeamTaskStatus,
    pub notes: Vec<String>,
    /// Ids of tasks this task blocks from starting. A teammate should
    /// not claim a task whose id appears in another task's `blocked_by`
    /// until this one is `Completed`.
    #[serde(default)]
    pub blocks: Vec<String>,
    /// Ids that must be `Completed` before this task is ready. Empty (or
    /// all-completed) → ready. Stale ids (no matching task) count as
    /// satisfied — a typo shouldn't pin a task forever.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Optional owner distinct from the current `assignee` — the
    /// teammate the task is destined for, set at planning time. `claim`
    /// sets `assignee` (who's working it now); `owner` is the intended
    /// recipient. Mirrors Claude Code's `TaskUpdate::owner`.
    #[serde(default)]
    pub owner: Option<String>,
}

/// Parsed payload of a `team_task` call. `action` selects the
/// operation; the remaining fields are conditionally required
/// (`task_id` for `update`/`claim`/`link`, `status`/`notes`/`owner` for
/// `update`, `blocks`/`blocked_by` for `link`).
#[derive(Debug, Deserialize)]
struct TeamTaskInput {
    action: String,
    task_id: Option<String>,
    status: Option<String>,
    notes: Option<String>,
    owner: Option<String>,
    /// `link` adds these ids to the task's `blocks` (this task blocks
    /// them). Deduped against the existing list.
    #[serde(default)]
    blocks: Vec<String>,
    /// `link` adds these ids to the task's `blocked_by` (they block this
    /// task). Deduped against the existing list.
    #[serde(default)]
    blocked_by: Vec<String>,
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
                    "enum": ["list", "update", "claim", "link"],
                    "description": "Operation to perform."
                },
                "task_id": {
                    "type": "string",
                    "description": "The task id (for update/claim/link)."
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "active", "completed", "blocked"],
                    "description": "New status (for update)."
                },
                "notes": {
                    "type": "string",
                    "description": "Additional notes to append to the task (for update)."
                },
                "owner": {
                    "type": "string",
                    "description": "Set the task's owner (the teammate the task is destined for). For update."
                },
                "blocks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task ids this task blocks from starting (for link). Added (deduped) to the task's `blocks`."
                },
                "blocked_by": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task ids that must complete before this task is ready (for link). Added (deduped) to the task's `blocked_by`."
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
                let idx = match tasks.iter().position(|t| t.id == task_id) {
                    Some(i) => i,
                    None => return Ok(err_output(&format!("task not found: {task_id}"))),
                };
                update_task(
                    &mut tasks[idx],
                    new_status,
                    parsed.notes.as_deref(),
                    parsed.owner.as_deref(),
                );
                let summary = render_task(&tasks[idx], &tasks);
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
                let idx = match tasks.iter().position(|t| t.id == task_id) {
                    Some(i) => i,
                    None => return Ok(err_output(&format!("task not found: {task_id}"))),
                };
                claim_task(&mut tasks[idx], &self.teammate_name);
                let summary = render_task(&tasks[idx], &tasks);
                if let Err(e) = self.write_tasks(&tasks) {
                    return Ok(err_output(&format!("write tasks: {e}")));
                }
                Ok(text_output(&format!("Claimed task {task_id}:\n{summary}")))
            }
            "link" => {
                // (M5/#19) Add `blocks`/`blocked_by` edges to a task. The
                // ids are validated against the existing list (a typo
                // returns an error rather than silently recording a
                // dangling edge). Edges are deduped; self-edges are
                // dropped (a task blocking itself can never become
                // ready). The reciprocal `blocks`/`blocked_by` entry is
                // NOT auto-added — the graph is directed and the caller
                // owns both ends, matching Claude Code's explicit
                // `addBlocks`/`addBlockedBy` model.
                let task_id = match parsed.task_id {
                    Some(id) => id,
                    None => return Ok(err_output("`link` requires `task_id`")),
                };
                if parsed.blocks.is_empty() && parsed.blocked_by.is_empty() {
                    return Ok(err_output(
                        "`link` requires at least one of `blocks`/`blocked_by`",
                    ));
                }
                let mut tasks = match self.read_tasks() {
                    Ok(t) => t,
                    Err(e) => return Ok(err_output(&format!("read tasks: {e}"))),
                };
                let known: std::collections::HashSet<&String> =
                    tasks.iter().map(|t| &t.id).collect();
                let validate_ids = |ids: &[String], field: &str| -> Result<(), String> {
                    for id in ids {
                        if id == &task_id {
                            // self-edge: silently dropped below, not an error
                            continue;
                        }
                        if !known.contains(id) {
                            return Err(format!("`{field}` references unknown task id: {id}"));
                        }
                    }
                    Ok(())
                };
                if let Err(msg) = validate_ids(&parsed.blocks, "blocks") {
                    return Ok(err_output(&msg));
                }
                if let Err(msg) = validate_ids(&parsed.blocked_by, "blocked_by") {
                    return Ok(err_output(&msg));
                }
                let idx = match tasks.iter().position(|t| t.id == task_id) {
                    Some(i) => i,
                    None => return Ok(err_output(&format!("task not found: {task_id}"))),
                };
                link_task(&mut tasks[idx], &parsed.blocks, &parsed.blocked_by);
                let summary = render_task(&tasks[idx], &tasks);
                if let Err(e) = self.write_tasks(&tasks) {
                    return Ok(err_output(&format!("write tasks: {e}")));
                }
                Ok(text_output(&format!("Linked task {task_id}:\n{summary}")))
            }
            other => Ok(err_output(&format!("unknown `action`: {other}"))),
        }
    }

    fn effects(&self) -> ToolEffects {
        // `update`/`claim` write the JSONL list to disk, so this is
        // not safe to run in parallel with other write tools.
        ToolEffects::write()
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
        out.push_str(&render_task_line(task, tasks));
    }
    out
}

/// One indented line per task, e.g.
/// `  [completed] t1: Refactor parser — assigned to alice · ready`.
/// The bracketed status is left-aligned to the width of the longest
/// label (`[completed]` = 11 chars) so the id columns line up. The
/// trailing `· ready`/`· blocked` marker (M5/#19) is shown only when
/// the task has dependency edges — a flat task omits it (no graph
/// noise for the common case).
fn render_task_line(task: &TeamTask, all: &[TeamTask]) -> String {
    let label = format!("[{}]", status_label(task.status));
    let assignee = match &task.assignee {
        Some(name) => format!("assigned to {name}"),
        None => "unassigned".to_string(),
    };
    let mut line = format!("  {:<11} {}: {} — {}", label, task.id, task.title, assignee);
    if let Some(owner) = &task.owner {
        if task.assignee.as_deref() != Some(owner) {
            line.push_str(&format!(" · owner {owner}"));
        }
    }
    if has_edges(task) {
        let marker = if is_ready(task, all) {
            "ready"
        } else {
            "blocked"
        };
        line.push_str(&format!(" · {marker}"));
    }
    line
}

/// Render a single task with its accumulated notes (+edges), for the
/// `update`/`claim`/`link` result messages.
fn render_task(task: &TeamTask, all: &[TeamTask]) -> String {
    let mut out = render_task_line(task, all);
    if !task.notes.is_empty() {
        out.push_str("\n  Notes:");
        for note in &task.notes {
            out.push_str(&format!("\n    - {note}"));
        }
    }
    if !task.blocks.is_empty() {
        out.push_str(&format!("\n  Blocks: {}", task.blocks.join(", ")));
    }
    if !task.blocked_by.is_empty() {
        out.push_str(&format!("\n  Blocked by: {}", task.blocked_by.join(", ")));
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
/// set the owner if provided, and append a (trimmed, non-empty) note if
/// provided. `owner = Some("")` clears the owner (so the caller can
/// unset it); `owner = None` leaves it untouched.
fn update_task(
    task: &mut TeamTask,
    new_status: Option<TeamTaskStatus>,
    notes: Option<&str>,
    owner: Option<&str>,
) {
    if let Some(status) = new_status {
        task.status = status;
    }
    if let Some(owner) = owner {
        let owner = owner.trim();
        task.owner = if owner.is_empty() {
            None
        } else {
            Some(owner.to_string())
        };
    }
    if let Some(note) = notes {
        let note = note.trim();
        if !note.is_empty() {
            task.notes.push(note.to_string());
        }
    }
}

/// Add `blocks`/`blocked_by` edges to a task, deduping against the
/// existing lists and dropping self-edges (a task blocking itself can
/// never become ready). The reciprocal entry on the other task is NOT
/// added — the graph is directed and the caller owns both ends.
fn link_task(task: &mut TeamTask, blocks: &[String], blocked_by: &[String]) {
    fn add_unique(dst: &mut Vec<String>, src: &[String], self_id: &str) {
        for id in src {
            let id = id.trim();
            if id.is_empty() || id == self_id {
                continue;
            }
            if !dst.iter().any(|e| e == id) {
                dst.push(id.to_string());
            }
        }
    }
    add_unique(&mut task.blocks, blocks, &task.id);
    add_unique(&mut task.blocked_by, blocked_by, &task.id);
}

/// True if the task has any dependency edges (either direction). Used
/// to decide whether to show the `· ready`/`· blocked` marker — flat
/// tasks (no edges) omit it, keeping the common case free of graph
/// noise.
fn has_edges(task: &TeamTask) -> bool {
    !task.blocks.is_empty() || !task.blocked_by.is_empty()
}

/// A task is **ready** when every id in `blocked_by` refers to a
/// `Completed` task. Stale ids (no matching task) count as satisfied
/// — a typo or a deleted dependency shouldn't pin a task forever.
/// A task with no `blocked_by` is always ready.
fn is_ready(task: &TeamTask, all: &[TeamTask]) -> bool {
    task.blocked_by.iter().all(|dep| {
        match all.iter().find(|t| &t.id == dep) {
            Some(t) => t.status == TeamTaskStatus::Completed,
            // Stale id → treat as satisfied.
            None => true,
        }
    })
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

    fn tool(dir: &std::path::Path) -> TeamTaskTool {
        TeamTaskTool::new(dir.to_path_buf(), "alice".to_string())
    }

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
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            owner: None,
        }
    }

    /// Build a task with edges for the graph tests.
    fn task_with_edges(
        id: &str,
        title: &str,
        status: TeamTaskStatus,
        blocks: Vec<&str>,
        blocked_by: Vec<&str>,
    ) -> TeamTask {
        TeamTask {
            id: id.to_string(),
            title: title.to_string(),
            assignee: None,
            status,
            notes: Vec::new(),
            blocks: blocks.iter().map(|s| s.to_string()).collect(),
            blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
            owner: None,
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
        update_task(&mut t, Some(TeamTaskStatus::Completed), None, None);
        assert_eq!(t.status, TeamTaskStatus::Completed);
        assert!(t.notes.is_empty(), "no notes should be appended");
    }

    #[test]
    fn update_task_appends_note() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec!["prior"]);
        update_task(&mut t, None, Some("hit a snag"), None);
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
            None,
        );
        assert_eq!(t.status, TeamTaskStatus::Blocked);
        assert_eq!(t.notes, vec!["waiting on API".to_string()]);
    }

    #[test]
    fn update_task_ignores_empty_note() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(&mut t, None, Some("   "), None);
        assert!(t.notes.is_empty(), "whitespace-only note was appended");
    }

    #[test]
    fn update_task_sets_owner() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(&mut t, None, None, Some("alice"));
        assert_eq!(t.owner.as_deref(), Some("alice"));
    }

    #[test]
    fn update_task_clears_owner_with_empty_string() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(&mut t, None, None, Some("alice"));
        update_task(&mut t, None, None, Some(""));
        assert!(t.owner.is_none(), "empty owner string should clear owner");
    }

    #[test]
    fn update_task_ignores_whitespace_owner() {
        let mut t = task("t1", "X", None, TeamTaskStatus::Pending, vec![]);
        update_task(&mut t, None, None, Some("   "));
        assert!(t.owner.is_none(), "whitespace-only owner should clear");
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
        let out = render_task(&t, std::slice::from_ref(&t));
        assert!(
            out.contains("  [active]    t1: X — assigned to alice"),
            "line: {out}"
        );
        assert!(out.contains("  Notes:"), "notes header missing: {out}");
        assert!(out.contains("    - one"), "note one missing: {out}");
        assert!(out.contains("    - two"), "note two missing: {out}");
    }

    // ---- link_task / is_ready / render markers (M5/#19) ----

    #[test]
    fn link_task_adds_blocks_and_blocked_by_dedup() {
        let mut t = task_with_edges("t1", "X", TeamTaskStatus::Pending, vec![], vec![]);
        link_task(
            &mut t,
            &["t2".to_string(), "t3".to_string()],
            &["t4".to_string()],
        );
        assert_eq!(t.blocks, vec!["t2".to_string(), "t3".to_string()]);
        assert_eq!(t.blocked_by, vec!["t4".to_string()]);
        // Dedup: re-adding an existing edge is a no-op.
        link_task(&mut t, &["t2".to_string()], &[]);
        assert_eq!(t.blocks, vec!["t2".to_string(), "t3".to_string()]);
    }

    #[test]
    fn link_task_drops_self_edge() {
        let mut t = task_with_edges("t1", "X", TeamTaskStatus::Pending, vec![], vec![]);
        // A task blocking itself can never become ready — drop it.
        link_task(&mut t, &["t1".to_string()], &["t1".to_string()]);
        assert!(t.blocks.is_empty(), "self-edge added to blocks");
        assert!(t.blocked_by.is_empty(), "self-edge added to blocked_by");
    }

    #[test]
    fn link_task_trims_and_ignores_empty_ids() {
        let mut t = task_with_edges("t1", "X", TeamTaskStatus::Pending, vec![], vec![]);
        link_task(&mut t, &["  t2 ".to_string(), "".to_string()], &[]);
        assert_eq!(t.blocks, vec!["t2".to_string()]);
    }

    #[test]
    fn is_ready_when_no_deps() {
        let t = task_with_edges("t1", "X", TeamTaskStatus::Pending, vec![], vec![]);
        assert!(is_ready(&t, &[]));
    }

    #[test]
    fn is_ready_when_all_deps_completed() {
        let tasks = vec![
            task_with_edges("t1", "A", TeamTaskStatus::Pending, vec![], vec!["t2", "t3"]),
            task("t2", "B", None, TeamTaskStatus::Completed, vec![]),
            task("t3", "C", None, TeamTaskStatus::Completed, vec![]),
        ];
        assert!(is_ready(&tasks[0], &tasks));
    }

    #[test]
    fn is_blocked_when_a_dep_is_not_completed() {
        let tasks = vec![
            task_with_edges("t1", "A", TeamTaskStatus::Pending, vec![], vec!["t2"]),
            task("t2", "B", None, TeamTaskStatus::Active, vec![]),
        ];
        assert!(!is_ready(&tasks[0], &tasks));
    }

    #[test]
    fn is_ready_treats_stale_dep_as_satisfied() {
        // A typo or deleted dependency shouldn't pin a task forever.
        let t = task_with_edges("t1", "A", TeamTaskStatus::Pending, vec![], vec!["ghost"]);
        assert!(is_ready(&t, std::slice::from_ref(&t)));
    }

    #[test]
    fn render_task_line_shows_ready_for_unblocked_edged_task() {
        let tasks = vec![
            task_with_edges("t1", "A", TeamTaskStatus::Pending, vec!["t2"], vec![]),
            task_with_edges("t2", "B", TeamTaskStatus::Completed, vec![], vec![]),
        ];
        // t2 completed → t1 (blocked_by none) is ready. t1 has edges so
        // the marker shows.
        let line = render_task_line(&tasks[0], &tasks);
        assert!(line.contains("· ready"), "ready marker missing: {line}");
    }

    #[test]
    fn render_task_line_shows_blocked_when_dep_pending() {
        let tasks = vec![
            task_with_edges("t1", "A", TeamTaskStatus::Pending, vec![], vec!["t2"]),
            task("t2", "B", None, TeamTaskStatus::Pending, vec![]),
        ];
        let line = render_task_line(&tasks[0], &tasks);
        assert!(line.contains("· blocked"), "blocked marker missing: {line}");
    }

    #[test]
    fn render_task_line_omits_marker_for_flat_task() {
        // No edges → no marker (no graph noise for the common case).
        let t = task("t1", "A", Some("alice"), TeamTaskStatus::Active, vec![]);
        let line = render_task_line(&t, std::slice::from_ref(&t));
        assert!(!line.contains("· ready"), "flat task showed ready: {line}");
        assert!(
            !line.contains("· blocked"),
            "flat task showed blocked: {line}"
        );
    }

    #[test]
    fn render_task_line_shows_owner_when_distinct_from_assignee() {
        let mut t = task("t1", "A", Some("alice"), TeamTaskStatus::Active, vec![]);
        t.owner = Some("bob".to_string());
        let line = render_task_line(&t, std::slice::from_ref(&t));
        assert!(line.contains("· owner bob"), "owner marker missing: {line}");
    }

    #[test]
    fn render_task_line_omits_owner_when_assignee_matches() {
        let mut t = task("t1", "A", Some("alice"), TeamTaskStatus::Active, vec![]);
        t.owner = Some("alice".to_string());
        let line = render_task_line(&t, std::slice::from_ref(&t));
        assert!(!line.contains("· owner"), "redundant owner shown: {line}");
    }

    #[test]
    fn render_task_lists_edges_in_detail() {
        let t = task_with_edges("t1", "A", TeamTaskStatus::Pending, vec!["t2"], vec!["t3"]);
        let out = render_task(&t, std::slice::from_ref(&t));
        assert!(out.contains("Blocks: t2"), "blocks line missing: {out}");
        assert!(
            out.contains("Blocked by: t3"),
            "blocked_by line missing: {out}"
        );
    }

    #[test]
    fn team_task_round_trips_through_jsonl_with_edges() {
        // Old JSONL (pre-#19) must still deserialize; new fields default.
        let old_line = r#"{"id":"t1","title":"X","assignee":"a","status":"pending","notes":[]}"#;
        let t: TeamTask = serde_json::from_str(old_line).unwrap();
        assert!(t.blocks.is_empty());
        assert!(t.blocked_by.is_empty());
        assert!(t.owner.is_none());
        // Round-trip with edges set.
        let mut t = t;
        t.blocks = vec!["t2".to_string()];
        t.blocked_by = vec!["t3".to_string()];
        t.owner = Some("bob".to_string());
        let s = serde_json::to_string(&t).unwrap();
        let back: TeamTask = serde_json::from_str(&s).unwrap();
        assert_eq!(back.blocks, vec!["t2".to_string()]);
        assert_eq!(back.blocked_by, vec!["t3".to_string()]);
        assert_eq!(back.owner.as_deref(), Some("bob"));
    }

    // ---- tool-level (link action via execute) ----

    fn run<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        asupersync::test_utils::run_test(f);
    }

    fn seed(dir: &std::path::Path, tasks: &[TeamTask]) {
        let path = dir.join("tasks.jsonl");
        let mut out = String::new();
        for t in tasks {
            out.push_str(&serde_json::to_string(t).unwrap());
            out.push('\n');
        }
        std::fs::write(&path, out).unwrap();
    }

    fn done_text(exec: ToolExecution) -> String {
        match exec {
            ToolExecution::Done(out) => {
                assert!(!out.is_error, "unexpected error output");
                match out.content.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => panic!("no text content"),
                }
            }
            _ => panic!("expected Done"),
        }
    }

    fn err_text(exec: ToolExecution) -> String {
        match exec {
            ToolExecution::Done(out) => {
                assert!(out.is_error, "expected error output");
                match out.content.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => panic!("no text content"),
                }
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn link_action_adds_blocked_by_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            &[
                task("t1", "A", None, TeamTaskStatus::Pending, vec![]),
                task("t2", "B", None, TeamTaskStatus::Pending, vec![]),
            ],
        );
        run(|| async {
            let t = tool(dir.path());
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({
                        "action": "link",
                        "task_id": "t1",
                        "blocked_by": ["t2"],
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = done_text(exec);
            assert!(text.contains("Blocked by: t2"), "edges missing: {text}");
            // Persisted to disk.
            let on_disk = std::fs::read_to_string(dir.path().join("tasks.jsonl")).unwrap();
            assert!(
                on_disk.contains(r#""blocked_by":["t2"]"#),
                "not persisted: {on_disk}"
            );
        });
    }

    #[test]
    fn link_action_rejects_unknown_task_id() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            &[task("t1", "A", None, TeamTaskStatus::Pending, vec![])],
        );
        run(|| async {
            let t = tool(dir.path());
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({
                        "action": "link",
                        "task_id": "t1",
                        "blocked_by": ["ghost"],
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = err_text(exec);
            assert!(text.contains("unknown task id"), "wrong error: {text}");
        });
    }

    #[test]
    fn link_action_requires_at_least_one_edge() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            &[task("t1", "A", None, TeamTaskStatus::Pending, vec![])],
        );
        run(|| async {
            let t = tool(dir.path());
            let exec = t
                .execute(
                    "c1",
                    serde_json::json!({ "action": "link", "task_id": "t1" }),
                    None,
                )
                .await
                .unwrap();
            let text = err_text(exec);
            assert!(text.contains("at least one"), "wrong error: {text}");
        });
    }

    #[test]
    fn list_action_shows_ready_marker_for_unblocked_task() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            &[
                task_with_edges("t1", "A", TeamTaskStatus::Pending, vec![], vec!["t2"]),
                task("t2", "B", None, TeamTaskStatus::Completed, vec![]),
            ],
        );
        run(|| async {
            let t = tool(dir.path());
            let exec = t
                .execute("c1", serde_json::json!({ "action": "list" }), None)
                .await
                .unwrap();
            let text = done_text(exec);
            assert!(text.contains("· ready"), "ready marker missing: {text}");
        });
    }
}
