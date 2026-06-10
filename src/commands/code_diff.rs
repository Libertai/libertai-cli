use std::path::{Component, Path, PathBuf};

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
    path: String,
    resolved: PathBuf,
    before: Option<String>,
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

fn read_preview_file(path: &Path) -> Option<String> {
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
}
