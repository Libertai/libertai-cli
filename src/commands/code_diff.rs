use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

const MAX_DIFF_LINES: usize = 80;
const MAX_FILE_PREVIEW_BYTES: u64 = 128 * 1024;

pub(crate) fn approval_diff_preview(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "edit" => edit_diff(input),
        "write" => write_diff(input),
        "hashline_edit" => hashline_summary(input),
        _ => None,
    }
}

pub(crate) fn approval_diff_preview_with_cwd(
    tool: &str,
    input: &Value,
    cwd: &Path,
) -> Option<String> {
    match tool {
        "edit" => edit_file_diff(input, cwd).or_else(|| edit_diff(input)),
        "write" => write_file_diff(input, cwd).or_else(|| write_diff(input)),
        "hashline_edit" => hashline_summary(input),
        _ => None,
    }
}

pub(crate) struct FileSnapshot {
    pub(crate) path: String,
    pub(crate) resolved: PathBuf,
    pub(crate) before: Option<String>,
}

pub(crate) fn file_snapshot_before_tool(
    tool: &str,
    input: &Value,
    cwd: &Path,
) -> Option<FileSnapshot> {
    if !matches!(tool, "edit" | "write" | "hashline_edit") {
        return None;
    }
    let path = input.get("path").and_then(Value::as_str)?;
    let resolved = resolve_under_cwd(path, cwd)?;
    Some(FileSnapshot {
        path: path.to_string(),
        before: read_preview_file(&resolved),
        resolved,
    })
}

pub(crate) fn post_execution_diff(snapshot: &FileSnapshot) -> Option<String> {
    let after = read_preview_file(&snapshot.resolved);
    match (&snapshot.before, after) {
        (Some(before), Some(after)) if before != &after => {
            Some(render_line_diff(&snapshot.path, before, &after))
        }
        (None, Some(after)) => Some(render_new_file_diff(&snapshot.path, &after)),
        (Some(before), None) => Some(render_deleted_file_diff(&snapshot.path, before)),
        _ => None,
    }
}

/// One recorded filesystem mutation, captured by [`EditJournal`] so `/undo`
/// can revert it. Mirrors the free [`FileSnapshot`] the approval layer already
/// takes before a mutating tool runs: `before` is the pre-edit content (or
/// `None` when the file didn't exist), `after` is the post-edit content (or
/// `None` when the edit deleted it). `/undo` restores `before`, deleting the
/// file outright when `before` was `None` (the edit had created it).
pub struct JournalEntry {
    pub path: String,
    pub resolved: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Shared, append-most-recent ring buffer of [`JournalEntry`]s. Follows the
/// same `Arc<Mutex<…>>` pattern as [`ApprovalState`]: the background thread
/// (where `ApprovalTool::execute_inner` runs) `push`es after each edit, and
/// the main thread `pop`s on `/undo`. Capped at 50 entries so a long session
/// can't grow it without bound; the oldest entry is dropped first.
///
/// Mutex access uses the M6b poison-recovery pattern
/// (`unwrap_or_else(std::sync::PoisonError::into_inner)`) — a poisoned journal
/// (a panic on one thread mid-lock) shouldn't crash the TUI; we recover the
/// inner guard and keep serving `/undo`s with whatever survived.
pub struct EditJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

/// Maximum number of undo entries retained. Older edits are evicted
/// (ring-buffer semantics) so a long session doesn't accumulate unbounded
/// snapshots.
const JOURNAL_MAX_ENTRIES: usize = 50;

impl EditJournal {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Append an entry, evicting the oldest when the ring is full. Called
    /// from the background thread right after a mutating tool succeeds.
    pub(crate) fn push(&self, entry: JournalEntry) {
        let mut g = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.len() >= JOURNAL_MAX_ENTRIES {
            g.remove(0);
        }
        g.push(entry);
    }

    /// Pop the most recent entry (LIFO undo). Called from the main thread on
    /// `/undo`. Returns `None` when the journal is empty.
    pub(crate) fn pop(&self) -> Option<JournalEntry> {
        let mut g = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.pop()
    }

    /// Current depth of the journal (lock-then-len). Exposed as a
    /// `pub(crate)` test seam so cross-module tests can assert journal depth
    /// (`#[allow(dead_code)]` because non-test builds don't read it yet).
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// True when no undo entries remain. `pub(crate)` test seam; unused in
    /// non-test builds (`#[allow(dead_code)]`).
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for EditJournal {
    fn default() -> Self {
        Self::new()
    }
}

fn edit_diff(input: &Value) -> Option<String> {
    let old_text = input.get("oldText").and_then(Value::as_str)?;
    let new_text = input.get("newText").and_then(Value::as_str)?;
    Some(render_unified("oldText", "newText", old_text, new_text))
}

fn edit_file_diff(input: &Value, cwd: &Path) -> Option<String> {
    let path = input.get("path").and_then(Value::as_str)?;
    let old_text = input.get("oldText").and_then(Value::as_str)?;
    let new_text = input.get("newText").and_then(Value::as_str)?;
    let resolved = resolve_under_cwd(path, cwd)?;
    let before = read_preview_file(&resolved)?;
    let after = before.replacen(old_text, new_text, 1);
    if before == after {
        return None;
    }
    Some(render_line_diff(path, &before, &after))
}

fn write_diff(input: &Value) -> Option<String> {
    let content = input.get("content").and_then(Value::as_str)?;
    let mut lines = vec!["--- /dev/null".to_string(), "+++ proposed".to_string()];
    for line in content.lines() {
        lines.push(format!("+{line}"));
    }
    Some(cap_lines(lines))
}

fn write_file_diff(input: &Value, cwd: &Path) -> Option<String> {
    let path = input.get("path").and_then(Value::as_str)?;
    let content = input.get("content").and_then(Value::as_str)?;
    let resolved = resolve_under_cwd(path, cwd)?;
    match read_preview_file(&resolved) {
        Some(before) => Some(render_line_diff(path, &before, content)),
        None => write_diff(input),
    }
}

fn hashline_summary(input: &Value) -> Option<String> {
    let edits = input.get("edits")?.as_array()?;
    let mut lines = vec!["hashline operations:".to_string()];
    for (idx, edit) in edits.iter().enumerate() {
        let op = edit.get("op").and_then(Value::as_str).unwrap_or("replace");
        let pos = edit.get("pos").and_then(Value::as_str).unwrap_or("BOF/EOF");
        let end = edit.get("end").and_then(Value::as_str);
        let changed = line_count(edit.get("lines"));
        let range = match end {
            Some(end) if !end.trim().is_empty() => format!("{pos}..{end}"),
            _ => pos.to_string(),
        };
        lines.push(format!(
            "{}. {op} {range} with {changed} line{}",
            idx + 1,
            plural_s(changed)
        ));
    }
    Some(cap_lines(lines))
}

pub(crate) fn render_line_diff(path: &str, before: &str, after: &str) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    if before_lines == after_lines {
        return "no text change".to_string();
    }

    let mut prefix = 0;
    while prefix < before_lines.len()
        && prefix < after_lines.len()
        && before_lines[prefix] == after_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix + prefix < before_lines.len()
        && suffix + prefix < after_lines.len()
        && before_lines[before_lines.len() - 1 - suffix]
            == after_lines[after_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mut lines = vec![format!("--- {path}"), format!("+++ proposed/{path}")];
    let context_start = prefix.saturating_sub(3);
    for line in &before_lines[context_start..prefix] {
        lines.push(format!(" {line}"));
    }
    for line in &before_lines[prefix..before_lines.len() - suffix] {
        lines.push(format!("-{line}"));
    }
    for line in &after_lines[prefix..after_lines.len() - suffix] {
        lines.push(format!("+{line}"));
    }
    let context_end = (before_lines.len() - suffix + 3).min(before_lines.len());
    for line in &before_lines[before_lines.len() - suffix..context_end] {
        lines.push(format!(" {line}"));
    }
    cap_lines(lines)
}

fn render_new_file_diff(path: &str, content: &str) -> String {
    let mut lines = vec!["--- /dev/null".to_string(), format!("+++ {path}")];
    for line in content.lines() {
        lines.push(format!("+{line}"));
    }
    cap_lines(lines)
}

fn render_deleted_file_diff(path: &str, content: &str) -> String {
    let mut lines = vec![format!("--- {path}"), "+++ /dev/null".to_string()];
    for line in content.lines() {
        lines.push(format!("-{line}"));
    }
    cap_lines(lines)
}

fn render_unified(old_label: &str, new_label: &str, old_text: &str, new_text: &str) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    if old_lines == new_lines {
        return "no text change".to_string();
    }

    let mut lines = vec![format!("--- {old_label}"), format!("+++ {new_label}")];
    for line in &old_lines {
        lines.push(format!("-{line}"));
    }
    for line in &new_lines {
        lines.push(format!("+{line}"));
    }
    cap_lines(lines)
}

pub(crate) fn read_preview_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_PREVIEW_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn resolve_under_cwd(path: &str, cwd: &Path) -> Option<PathBuf> {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = normalize(&joined);
    if normalized.starts_with(cwd) {
        Some(normalized)
    } else {
        None
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn cap_lines(mut lines: Vec<String>) -> String {
    if lines.len() > MAX_DIFF_LINES {
        let omitted = lines.len() - MAX_DIFF_LINES;
        lines.truncate(MAX_DIFF_LINES);
        lines.push(format!("... {omitted} lines omitted"));
    }
    lines.join("\n")
}

fn line_count(value: Option<&Value>) -> usize {
    match value {
        Some(Value::Array(lines)) => lines.len(),
        Some(Value::String(text)) => text.lines().count().max(1),
        Some(Value::Null) => 0,
        _ => 0,
    }
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_preview_renders_old_and_new_text() {
        let preview = approval_diff_preview(
            "edit",
            &json!({"oldText":"let a = 1;","newText":"let a = 2;"}),
        )
        .unwrap();
        assert!(preview.contains("--- oldText"));
        assert!(preview.contains("-let a = 1;"));
        assert!(preview.contains("+let a = 2;"));
    }

    #[test]
    fn write_preview_renders_added_content() {
        let preview = approval_diff_preview("write", &json!({"content":"alpha\nbeta"})).unwrap();
        assert_eq!(preview, "--- /dev/null\n+++ proposed\n+alpha\n+beta");
    }

    #[test]
    fn write_preview_compares_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
        let preview = approval_diff_preview_with_cwd(
            "write",
            &json!({"path":"notes.txt","content":"alpha\ngamma\n"}),
            temp.path(),
        )
        .unwrap();
        assert!(preview.contains("--- notes.txt"));
        assert!(preview.contains(" alpha"));
        assert!(preview.contains("-beta"));
        assert!(preview.contains("+gamma"));
    }

    #[test]
    fn edit_preview_compares_result_against_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn main() {\n    old();\n}\n").unwrap();
        let preview = approval_diff_preview_with_cwd(
            "edit",
            &json!({"path":"lib.rs","oldText":"old();","newText":"new();"}),
            temp.path(),
        )
        .unwrap();
        assert!(preview.contains("--- lib.rs"));
        assert!(preview.contains("-    old();"));
        assert!(preview.contains("+    new();"));
    }

    #[test]
    fn cwd_preview_rejects_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let preview = approval_diff_preview_with_cwd(
            "write",
            &json!({"path":"../outside.txt","content":"hello"}),
            temp.path(),
        )
        .unwrap();
        assert_eq!(preview, "--- /dev/null\n+++ proposed\n+hello");
    }

    #[test]
    fn hashline_summary_counts_operations() {
        let preview = approval_diff_preview(
            "hashline_edit",
            &json!({"edits":[{"op":"replace","pos":"2#AA","lines":["x","y"]},{"op":"append","lines":"z"}]}),
        )
        .unwrap();
        assert!(preview.contains("1. replace 2#AA with 2 lines"));
        assert!(preview.contains("2. append BOF/EOF with 1 line"));
    }

    #[test]
    fn diff_preview_caps_long_output() {
        let content = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = approval_diff_preview("write", &json!({"content": content})).unwrap();
        assert!(preview.contains("lines omitted"));
        assert!(preview.lines().count() <= MAX_DIFF_LINES + 1);
    }

    #[test]
    fn post_execution_diff_compares_snapshot_to_final_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notes.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let snapshot =
            file_snapshot_before_tool("write", &json!({"path":"notes.txt"}), temp.path()).unwrap();

        std::fs::write(&path, "alpha\ngamma\n").unwrap();
        let diff = post_execution_diff(&snapshot).unwrap();

        assert!(diff.contains("--- notes.txt"));
        assert!(diff.contains("-beta"));
        assert!(diff.contains("+gamma"));
    }

    #[test]
    fn post_execution_diff_renders_new_file_from_missing_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot =
            file_snapshot_before_tool("write", &json!({"path":"new.txt"}), temp.path()).unwrap();

        std::fs::write(temp.path().join("new.txt"), "hello\n").unwrap();
        let diff = post_execution_diff(&snapshot).unwrap();

        assert_eq!(diff, "--- /dev/null\n+++ new.txt\n+hello");
    }

    // ── EditJournal ─────────────────────────────────────────────────

    #[test]
    fn journal_starts_empty_and_default_matches_new() {
        let j = EditJournal::new();
        assert!(j.is_empty());
        assert_eq!(j.len(), 0);
        let j2 = EditJournal::default();
        assert!(j2.is_empty());
    }

    #[test]
    fn journal_push_then_pop_returns_last_entry_lifo() {
        let j = EditJournal::new();
        j.push(JournalEntry {
            path: "a.rs".to_string(),
            resolved: PathBuf::from("/tmp/a.rs"),
            before: None,
            after: Some("new".to_string()),
        });
        j.push(JournalEntry {
            path: "b.rs".to_string(),
            resolved: PathBuf::from("/tmp/b.rs"),
            before: Some("old".to_string()),
            after: Some("newer".to_string()),
        });
        assert_eq!(j.len(), 2);

        let last = j.pop().unwrap();
        assert_eq!(last.path, "b.rs");
        assert_eq!(j.len(), 1);
        let first = j.pop().unwrap();
        assert_eq!(first.path, "a.rs");
        assert_eq!(j.len(), 0);
        assert!(j.is_empty());
        assert!(j.pop().is_none());
    }

    #[test]
    fn journal_evicts_oldest_when_cap_reached() {
        let j = EditJournal::new();
        // JOURNAL_MAX_ENTRIES is 50; push a few more and confirm only the
        // most recent 50 survive, oldest-first eviction.
        for i in 0..(JOURNAL_MAX_ENTRIES + 3) {
            j.push(JournalEntry {
                path: format!("file{i}"),
                resolved: PathBuf::from(format!("/tmp/file{i}")),
                before: None,
                after: None,
            });
        }
        assert_eq!(j.len(), JOURNAL_MAX_ENTRIES);
        // The first three (file0..file2) were evicted; file3 is now the
        // oldest. Pop drains most-recent-first, so the last popped is the
        // oldest survivor.
        let mut popped = Vec::new();
        while let Some(e) = j.pop() {
            popped.push(e.path);
        }
        assert_eq!(
            popped.first().map(String::as_str).unwrap(),
            format!("file{}", JOURNAL_MAX_ENTRIES + 2)
        );
        assert_eq!(popped.last().map(String::as_str).unwrap(), "file3");
        // The evicted entries (file0, file1, file2) must be absent. Use exact
        // equality — `starts_with` would also (wrongly) flag survivors like
        // "file10".."file19" / "file20".."file29" that merely share a prefix.
        assert!(
            !popped.contains(&"file0".to_string())
                && !popped.contains(&"file1".to_string())
                && !popped.contains(&"file2".to_string()),
            "evicted entries must not appear: {popped:?}"
        );
        // Every survivor (file3..file52) must be present exactly once.
        for i in 3..(JOURNAL_MAX_ENTRIES + 3) {
            assert!(popped.contains(&format!("file{i}")), "missing file{i}");
        }
    }
}
