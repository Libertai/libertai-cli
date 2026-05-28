use serde_json::Value;

const MAX_DIFF_LINES: usize = 80;

pub(crate) fn approval_diff_preview(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "edit" => edit_diff(input),
        "write" => write_diff(input),
        "hashline_edit" => hashline_summary(input),
        _ => None,
    }
}

fn edit_diff(input: &Value) -> Option<String> {
    let old_text = input.get("oldText").and_then(Value::as_str)?;
    let new_text = input.get("newText").and_then(Value::as_str)?;
    Some(render_unified("oldText", "newText", old_text, new_text))
}

fn write_diff(input: &Value) -> Option<String> {
    let content = input.get("content").and_then(Value::as_str)?;
    let mut lines = vec!["--- /dev/null".to_string(), "+++ proposed".to_string()];
    for line in content.lines() {
        lines.push(format!("+{line}"));
    }
    Some(cap_lines(lines))
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
        let content = (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let preview = approval_diff_preview("write", &json!({"content": content})).unwrap();
        assert!(preview.contains("lines omitted"));
        assert!(preview.lines().count() <= MAX_DIFF_LINES + 1);
    }
}
