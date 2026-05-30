//! Claude-compatible custom slash command discovery for the CLI REPL.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: Option<String>,
    pub arg_hint: Option<String>,
    pub body: String,
    pub source: CommandSource,
    pub path: PathBuf,
}

pub fn discover(cwd: &Path) -> Vec<CustomCommand> {
    discover_with_home(cwd, dirs::home_dir().as_deref(), dirs::config_dir().as_deref())
}

fn discover_with_home(
    cwd: &Path,
    home: Option<&Path>,
    config: Option<&Path>,
) -> Vec<CustomCommand> {
    let mut out = Vec::new();
    if let Some(home) = home {
        scan_dir(&home.join(".claude").join("commands"), CommandSource::User, &mut out);
    }
    if let Some(config) = config {
        scan_dir(&config.join("libertai").join("commands"), CommandSource::User, &mut out);
    }
    if let Some(home) = home {
        scan_dir(&home.join(".liberclaw").join("commands"), CommandSource::User, &mut out);
    }
    scan_dir(&cwd.join(".claude").join("commands"), CommandSource::Project, &mut out);
    scan_dir(&cwd.join(".libertai").join("commands"), CommandSource::Project, &mut out);
    scan_dir(&cwd.join(".liberclaw").join("commands"), CommandSource::Project, &mut out);
    dedupe_by_name(&mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn expand(command: &CustomCommand, args: &str) -> String {
    expand_body(&command.body, args)
}

fn expand_body(body: &str, args: &str) -> String {
    let args = args.trim();
    let positional = split_command_args(args);
    let mut out = String::with_capacity(body.len() + args.len());
    let mut i = 0usize;
    let mut used_args = false;
    while i < body.len() {
        let rest = &body[i..];
        if rest.starts_with("{{") {
            if let Some(end) = rest.find("}}") {
                if rest[2..end].trim().eq_ignore_ascii_case("args") {
                    out.push_str(args);
                    used_args = true;
                    i += end + "}}".len();
                    continue;
                }
            }
        }
        if let Some(indexed) = rest.strip_prefix("$ARGUMENTS[") {
            if let Some(end) = indexed.find(']') {
                if let Ok(idx) = indexed[..end].parse::<usize>() {
                    if let Some(value) = positional.get(idx) {
                        out.push_str(value);
                    }
                    used_args = true;
                    i += "$ARGUMENTS[".len() + end + "]".len();
                    continue;
                }
            }
        }
        if rest.starts_with("$ARGUMENTS") {
            out.push_str(args);
            used_args = true;
            i += "$ARGUMENTS".len();
            continue;
        }
        let bytes = rest.as_bytes();
        if bytes.first() == Some(&b'$') && bytes.get(1).is_some_and(u8::is_ascii_digit) {
            let mut end = 1usize;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if let Ok(idx) = rest[1..end].parse::<usize>() {
                if let Some(value) = positional.get(idx) {
                    out.push_str(value);
                }
                used_args = true;
                i += end;
                continue;
            }
        }
        let ch = rest.chars().next().expect("non-empty rest");
        out.push(ch);
        i += ch.len_utf8();
    }
    if !used_args && !args.is_empty() {
        if !out.ends_with('\n') {
            out.push_str("\n\n");
        }
        out.push_str("ARGUMENTS: ");
        out.push_str(args);
    }
    out
}

fn split_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in args.trim().chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn scan_dir(dir: &Path, source: CommandSource, out: &mut Vec<CustomCommand>) {
    scan_dir_inner(dir, dir, source, out);
}

fn scan_dir_inner(root: &Path, dir: &Path, source: CommandSource, out: &mut Vec<CustomCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_inner(root, &path, source, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Some(cmd) = load_file(root, &path, source) {
            out.push(cmd);
        }
    }
}

fn load_file(root: &Path, path: &Path, source: CommandSource) -> Option<CustomCommand> {
    let raw = std::fs::read_to_string(path).ok()?;
    let name = command_name(root, path)?;
    if name.is_empty() {
        return None;
    }
    let (frontmatter, body) = split_frontmatter(&raw);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let mut description = None;
    let mut arg_hint = None;
    for (key, value) in frontmatter {
        match key.as_str() {
            "description" => description = Some(value),
            "argHint" | "arg_hint" | "arg-hint" | "argument-hint" => arg_hint = Some(value),
            _ => {}
        }
    }
    Some(CustomCommand {
        name,
        description,
        arg_hint,
        body: body.to_string(),
        source,
        path: path.to_path_buf(),
    })
}

fn command_name(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        let value = part.to_str()?;
        if value.is_empty() {
            return None;
        }
        parts.push(value.to_string());
    }
    let last = parts.last_mut()?;
    if !last.ends_with(".md") {
        return None;
    }
    last.truncate(last.len().saturating_sub(".md".len()));
    if last.is_empty() {
        return None;
    }
    Some(parts.join("/").to_lowercase())
}

fn split_frontmatter(input: &str) -> (Vec<(String, String)>, &str) {
    let trimmed = input.trim_start_matches('\u{FEFF}');
    if !trimmed.starts_with("---") {
        return (Vec::new(), input);
    }
    let Some(open_end) = trimmed.find('\n') else {
        return (Vec::new(), input);
    };
    let after_open = &trimmed[open_end + 1..];
    let mut walked = 0usize;
    let mut closing_end = None;
    for line in after_open.split_inclusive('\n') {
        let trimmed_line = line.trim_end_matches(['\n', '\r']);
        if trimmed_line == "---" {
            closing_end = Some(walked + line.len());
            break;
        }
        walked += line.len();
    }
    let Some(close_end) = closing_end else {
        return (Vec::new(), input);
    };
    let fm = &after_open[..walked];
    let body = &after_open[close_end..];
    let mut pairs = Vec::new();
    for line in fm.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            pairs.push((key.trim().to_string(), unquote(value.trim())));
        }
    }
    (pairs, body)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn dedupe_by_name(out: &mut Vec<CustomCommand>) {
    let mut latest = std::collections::HashMap::new();
    for (idx, cmd) in out.iter().enumerate() {
        latest.insert(cmd.name.clone(), idx);
    }
    let keep: std::collections::HashSet<usize> = latest.values().copied().collect();
    let mut idx = 0usize;
    out.retain(|_| {
        let yes = keep.contains(&idx);
        idx += 1;
        yes
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn discovers_project_command_with_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/commands/review.md"),
            "---\ndescription: Review diff\nargument-hint: scope\n---\nReview {{args}}",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "review");
        assert_eq!(cmds[0].description.as_deref(), Some("Review diff"));
        assert_eq!(cmds[0].arg_hint.as_deref(), Some("scope"));
        assert_eq!(expand(&cmds[0], "src"), "Review src");
    }

    #[test]
    fn project_command_overrides_user_command() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("repo");
        write(&home.join(".claude/commands/demo.md"), "user");
        write(&cwd.join(".libertai/commands/demo.md"), "project");

        let cmds = discover_with_home(&cwd, Some(&home), None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].body, "project");
        assert_eq!(cmds[0].source, CommandSource::Project);
    }

    #[test]
    fn discovers_nested_commands_with_path_names() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/commands/team/review.md"),
            "---\ndescription: Team review\n---\nReview {{args}}",
        );
        write(
            &temp.path().join(".libertai/commands/team/review.md"),
            "Project review {{args}}",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "team/review");
        assert_eq!(cmds[0].body, "Project review {{args}}");
        assert_eq!(expand(&cmds[0], "src"), "Project review src");
    }

    #[test]
    fn expands_claude_argument_placeholders() {
        let command = CustomCommand {
            name: "review".into(),
            description: None,
            arg_hint: None,
            body: "all=$ARGUMENTS first=$0 second=$1 indexed=$ARGUMENTS[1] missing=$3 legacy={{ args }}".into(),
            source: CommandSource::Project,
            path: PathBuf::from(".claude/commands/review.md"),
        };

        assert_eq!(
            expand(&command, r#"src/lib.rs "high priority""#),
            "all=src/lib.rs \"high priority\" first=src/lib.rs second=high priority indexed=high priority missing= legacy=src/lib.rs \"high priority\""
        );
    }

    #[test]
    fn appends_arguments_when_template_has_no_placeholders() {
        let command = CustomCommand {
            name: "plain".into(),
            description: None,
            arg_hint: None,
            body: "Review this carefully.".into(),
            source: CommandSource::Project,
            path: PathBuf::from(".claude/commands/plain.md"),
        };

        assert_eq!(
            expand(&command, "src/lib.rs"),
            "Review this carefully.\n\nARGUMENTS: src/lib.rs"
        );
    }
}
