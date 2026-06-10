use serde_json::Value;

const MAX_FIELD_CHARS: usize = 80;
const MAX_JSON_CHARS: usize = 120;

pub(crate) fn tool_preview(tool_name: &str, args: &Value) -> String {
    let detail = match tool_name {
        "read" => read_preview(args),
        "bash" => str_arg(args, "command").map(short),
        "bash_output" => bash_output_preview(args),
        "kill_bash" => pid_arg(args, "pid").map(|pid| pid.to_string()),
        "edit" => str_arg(args, "path").map(str::to_string),
        "write" => write_preview(args),
        "grep" => grep_preview(args),
        "find" => find_preview(args),
        "ls" => str_arg(args, "path")
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(".")
            .to_string()
            .into(),
        "hashline_edit" => hashline_preview(args),
        "task" => task_preview(args),
        "ask_user" => ask_user_preview(args),
        "fetch" => str_arg(args, "url").map(short),
        "search" => str_arg(args, "query").map(short),
        "generate_image" => image_preview(args),
        _ => fallback_preview(args),
    };

    match detail {
        Some(detail) if !detail.is_empty() => format!("{tool_name} {detail}"),
        _ => tool_name.to_string(),
    }
}

fn read_preview(args: &Value) -> Option<String> {
    let mut out = str_arg(args, "path")?.to_string();
    if let Some(offset) = int_arg(args, "offset") {
        out.push(':');
        out.push_str(&offset.to_string());
    }
    if let Some(limit) = int_arg(args, "limit") {
        out.push('+');
        out.push_str(&limit.to_string());
    }
    if bool_arg(args, "hashline") {
        out.push_str(" hashline");
    }
    Some(out)
}

fn write_preview(args: &Value) -> Option<String> {
    let path = str_arg(args, "path")?;
    let bytes = str_arg(args, "content").map(str::len).unwrap_or(0);
    Some(format!("{path} ({bytes}B)"))
}

fn bash_output_preview(args: &Value) -> Option<String> {
    let path = str_arg(args, "logPath").or_else(|| str_arg(args, "log_path"))?;
    let mut out = short(path);
    if let Some(pid) = pid_arg(args, "pid") {
        out.push_str(" pid ");
        out.push_str(&pid.to_string());
    }
    Some(out)
}

fn grep_preview(args: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(pattern) = str_arg(args, "pattern") {
        parts.push(short(pattern));
    }
    if let Some(path) = str_arg(args, "path") {
        parts.push(format!("in {path}"));
    }
    if let Some(glob) = str_arg(args, "glob") {
        parts.push(format!("glob {glob}"));
    }
    if bool_arg(args, "literal") {
        parts.push("literal".to_string());
    }
    if bool_arg(args, "ignoreCase") {
        parts.push("ignore-case".to_string());
    }
    non_empty(parts.join(" "))
}

fn find_preview(args: &Value) -> Option<String> {
    let pattern = str_arg(args, "pattern")?;
    let path = str_arg(args, "path")
        .filter(|path| !path.trim().is_empty())
        .unwrap_or(".");
    Some(format!("{} in {path}", short(pattern)))
}

fn hashline_preview(args: &Value) -> Option<String> {
    let path = str_arg(args, "path")?;
    let count = args
        .get("edits")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    Some(format!("{path} ({count} edit{})", plural_s(count)))
}

fn task_preview(args: &Value) -> Option<String> {
    let prompt = str_arg(args, "prompt")?;
    let isolation = task_worktree_requested(args)
        .then_some("worktree")
        .unwrap_or("same-cwd");
    let head = match str_arg(args, "subagent_type") {
        Some(agent) if !agent.trim().is_empty() => format!("{agent} [{isolation}]"),
        _ => format!("[{isolation}]"),
    };
    Some(format!("{head}: {}", short(prompt)))
}

fn task_worktree_requested(args: &Value) -> bool {
    args.get("worktree")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || args
            .get("isolation")
            .and_then(Value::as_str)
            .map(|s| s.eq_ignore_ascii_case("worktree"))
            .unwrap_or(false)
}

fn ask_user_preview(args: &Value) -> Option<String> {
    let questions = args.get("questions")?.as_array()?;
    let count = questions.len();
    let first = questions
        .first()
        .and_then(|q| str_arg(q, "question").or_else(|| str_arg(q, "header")))
        .map(short);
    match first {
        Some(first) if count > 1 => Some(format!("{count} questions; {first}")),
        Some(first) => Some(first),
        None => Some(format!("{count} question{}", plural_s(count))),
    }
}

fn image_preview(args: &Value) -> Option<String> {
    let filename = str_arg(args, "filename")?;
    let prompt = str_arg(args, "prompt").map(short);
    match prompt {
        Some(prompt) => Some(format!("{filename}: {prompt}")),
        None => Some(filename.to_string()),
    }
}

fn fallback_preview(args: &Value) -> Option<String> {
    for key in ["path", "command", "query", "url", "prompt", "filename"] {
        if let Some(value) = str_arg(args, key) {
            return Some(short(value));
        }
    }
    match args {
        Value::Object(map) if map.is_empty() => None,
        Value::Null => None,
        _ => Some(truncate(&compact(args), MAX_JSON_CHARS)),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn int_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn pid_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|pid| i64::try_from(pid).ok()))
            .or_else(|| value.as_str().and_then(|pid| pid.parse().ok()))
    })
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn short(value: &str) -> String {
    truncate(&collapse_ws(value), MAX_FIELD_CHARS)
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn collapse_ws(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
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
    fn previews_file_tools_with_primary_args() {
        assert_eq!(
            tool_preview("read", &json!({"path":"src/lib.rs","offset":12,"limit":40})),
            "read src/lib.rs:12+40"
        );
        assert_eq!(
            tool_preview("write", &json!({"path":"notes.md","content":"hello"})),
            "write notes.md (5B)"
        );
        assert_eq!(
            tool_preview(
                "hashline_edit",
                &json!({"path":"src/lib.rs","edits":[{},{}]})
            ),
            "hashline_edit src/lib.rs (2 edits)"
        );
    }

    #[test]
    fn previews_search_and_shell_tools_compactly() {
        assert_eq!(
            tool_preview("bash", &json!({"command":"cargo test --lib"})),
            "bash cargo test --lib"
        );
        assert_eq!(
            tool_preview(
                "bash_output",
                &json!({"logPath":"/tmp/pi-bash-bg-123.log","pid":1234})
            ),
            "bash_output /tmp/pi-bash-bg-123.log pid 1234"
        );
        assert_eq!(
            tool_preview("kill_bash", &json!({"pid":1234})),
            "kill_bash 1234"
        );
        assert_eq!(
            tool_preview(
                "grep",
                &json!({"pattern":"AgentEvent","path":"src","glob":"*.rs","ignoreCase":true})
            ),
            "grep AgentEvent in src glob *.rs ignore-case"
        );
        assert_eq!(
            tool_preview("find", &json!({"pattern":"*.rs"})),
            "find *.rs in ."
        );
    }

    #[test]
    fn previews_agent_and_user_tools() {
        assert_eq!(
            tool_preview(
                "task",
                &json!({"subagent_type":"reviewer","prompt":"inspect the diff"})
            ),
            "task reviewer [same-cwd]: inspect the diff"
        );
        assert_eq!(
            tool_preview(
                "task",
                &json!({"subagent_type":"reviewer","prompt":"inspect the diff","worktree":true})
            ),
            "task reviewer [worktree]: inspect the diff"
        );
        assert_eq!(
            tool_preview("ask_user", &json!({"questions":[{"question":"Proceed?"}]})),
            "ask_user Proceed?"
        );
    }

    #[test]
    fn fallback_uses_known_primary_fields_or_compact_json() {
        assert_eq!(
            tool_preview("custom", &json!({"query":"look this up"})),
            "custom look this up"
        );
        assert_eq!(
            tool_preview("custom", &json!({"count":2,"enabled":true})),
            "custom {\"count\":2,\"enabled\":true}"
        );
    }

    #[test]
    fn long_fields_are_collapsed_and_truncated() {
        let long = "one\n".repeat(60);
        let preview = tool_preview("bash", &json!({"command": long}));
        assert!(preview.starts_with("bash one one one"));
        assert!(preview.ends_with("..."));
    }
}
