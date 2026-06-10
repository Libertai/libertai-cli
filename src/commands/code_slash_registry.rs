//! Claude-compatible custom slash command discovery for the CLI REPL.

use std::path::{Path, PathBuf};

const DESCRIPTION_LIMIT: usize = 1536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub namespace: Option<String>,
    pub description: Option<String>,
    pub arg_hint: Option<String>,
    pub argument_names: Vec<String>,
    pub body: String,
    pub source: CommandSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpansionContext {
    pub session_id: Option<String>,
    pub effort: Option<String>,
}

pub fn discover(cwd: &Path) -> Vec<CustomCommand> {
    discover_with_home(
        cwd,
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
    )
}

fn discover_with_home(
    cwd: &Path,
    home: Option<&Path>,
    config: Option<&Path>,
) -> Vec<CustomCommand> {
    let mut out = Vec::new();
    if let Some(home) = home {
        scan_dir(
            &home.join(".claude").join("commands"),
            CommandSource::User,
            &mut out,
        );
    }
    if let Some(config) = config {
        scan_dir(
            &config.join("libertai").join("commands"),
            CommandSource::User,
            &mut out,
        );
    }
    if let Some(home) = home {
        scan_dir(
            &home.join(".liberclaw").join("commands"),
            CommandSource::User,
            &mut out,
        );
    }
    if let Some(config) = config {
        scan_skill_dir(
            &config.join("libertai").join("skills"),
            CommandSource::User,
            &mut out,
        );
    }
    if let Some(home) = home {
        scan_skill_dir(
            &home.join(".claude").join("skills"),
            CommandSource::User,
            &mut out,
        );
    }
    scan_dir(
        &cwd.join(".claude").join("commands"),
        CommandSource::Project,
        &mut out,
    );
    scan_dir(
        &cwd.join(".libertai").join("commands"),
        CommandSource::Project,
        &mut out,
    );
    scan_dir(
        &cwd.join(".liberclaw").join("commands"),
        CommandSource::Project,
        &mut out,
    );
    scan_skill_dir(
        &cwd.join(".claude").join("skills"),
        CommandSource::Project,
        &mut out,
    );
    scan_skill_dir(
        &cwd.join(".libertai").join("skills"),
        CommandSource::Project,
        &mut out,
    );
    dedupe_by_name(&mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn expand(command: &CustomCommand, args: &str) -> String {
    expand_with_context(command, args, &ExpansionContext::default())
}

pub fn expand_with_context(
    command: &CustomCommand,
    args: &str,
    context: &ExpansionContext,
) -> String {
    let skill_dir = command
        .path
        .parent()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    expand_body(
        &command.body,
        args,
        &command.argument_names,
        context,
        &skill_dir,
    )
}

fn expand_body(
    body: &str,
    args: &str,
    argument_names: &[String],
    context: &ExpansionContext,
    skill_dir: &str,
) -> String {
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
        if rest.starts_with("${CLAUDE_SESSION_ID}") {
            out.push_str(context.session_id.as_deref().unwrap_or_default());
            i += "${CLAUDE_SESSION_ID}".len();
            continue;
        }
        if rest.starts_with("${CLAUDE_EFFORT}") {
            out.push_str(context.effort.as_deref().unwrap_or_default());
            i += "${CLAUDE_EFFORT}".len();
            continue;
        }
        if rest.starts_with("${CLAUDE_SKILL_DIR}") {
            out.push_str(skill_dir);
            i += "${CLAUDE_SKILL_DIR}".len();
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
        if let Some((name, len)) = named_placeholder(rest) {
            if let Some(idx) = argument_names
                .iter()
                .position(|candidate| candidate == name)
            {
                if let Some(value) = positional.get(idx) {
                    out.push_str(value);
                }
                used_args = true;
                i += len;
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

fn named_placeholder(rest: &str) -> Option<(&str, usize)> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let first = *bytes.get(1)?;
    if !first.is_ascii_alphabetic() && first != b'_' {
        return None;
    }
    let mut end = 2usize;
    while let Some(byte) = bytes.get(end) {
        if byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-' {
            end += 1;
        } else {
            break;
        }
    }
    Some((&rest[1..end], end))
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

fn scan_skill_dir(dir: &Path, source: CommandSource, out: &mut Vec<CustomCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_path = path.join("SKILL.md");
        if skill_path.is_file() {
            if let Some(cmd) = load_skill_file(&path, &skill_path, source) {
                out.push(cmd);
            }
        }
    }
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
    let mut when_to_use = None;
    let mut arg_hint = None;
    let mut argument_names = Vec::new();
    for (key, value) in frontmatter {
        match key.as_str() {
            "description" => description = Some(value),
            "when_to_use" | "when-to-use" => when_to_use = Some(value),
            "argHint" | "arg_hint" | "arg-hint" | "argument-hint" => arg_hint = Some(value),
            "arguments" => argument_names = parse_argument_names(&value),
            "user-invocable" if is_false(&value) => return None,
            _ => {}
        }
    }
    let description = prompt_description(description, when_to_use, body);
    Some(CustomCommand {
        name,
        namespace: command_namespace(root, path),
        description,
        arg_hint,
        argument_names,
        body: body.to_string(),
        source,
        path: path.to_path_buf(),
    })
}

fn load_skill_file(dir: &Path, path: &Path, source: CommandSource) -> Option<CustomCommand> {
    let raw = std::fs::read_to_string(path).ok()?;
    let name = dir.file_name()?.to_str()?.to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let (frontmatter, body) = split_frontmatter(&raw);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let mut description = None;
    let mut when_to_use = None;
    let mut arg_hint = None;
    let mut argument_names = Vec::new();
    for (key, value) in frontmatter {
        match key.as_str() {
            "description" => description = Some(value),
            "when_to_use" | "when-to-use" => when_to_use = Some(value),
            "argHint" | "arg_hint" | "arg-hint" | "argument-hint" => arg_hint = Some(value),
            "arguments" => argument_names = parse_argument_names(&value),
            "user-invocable" if is_false(&value) => return None,
            _ => {}
        }
    }
    let description = prompt_description(description, when_to_use, body);
    Some(CustomCommand {
        name,
        namespace: None,
        description,
        arg_hint,
        argument_names,
        body: body.to_string(),
        source,
        path: path.to_path_buf(),
    })
}

fn prompt_description(
    description: Option<String>,
    when_to_use: Option<String>,
    body: &str,
) -> Option<String> {
    let base = description
        .filter(|value| !value.trim().is_empty())
        .or_else(|| first_body_paragraph(body));
    match (base, when_to_use.filter(|value| !value.trim().is_empty())) {
        (Some(base), Some(when)) => Some(truncate_description(&format!(
            "{} {}",
            base.trim(),
            when.trim()
        ))),
        (Some(base), None) => Some(truncate_description(base.trim())),
        (None, Some(when)) => Some(truncate_description(when.trim())),
        (None, None) => None,
    }
}

fn truncate_description(value: &str) -> String {
    value.chars().take(DESCRIPTION_LIMIT).collect()
}

fn first_body_paragraph(body: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in body.lines().map(str::trim) {
        if line.is_empty() {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        if lines.is_empty() && line.starts_with('#') {
            continue;
        }
        lines.push(line);
    }
    (!lines.is_empty()).then(|| lines.join(" "))
}

fn command_name(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let filename = relative.file_name()?.to_str()?;
    if !filename.ends_with(".md") {
        return None;
    }
    let name = &filename[..filename.len().saturating_sub(".md".len())];
    if name.is_empty() {
        return None;
    }
    Some(name.to_lowercase())
}

fn command_namespace(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parent = relative.parent()?;
    let mut parts = Vec::new();
    for component in parent.components() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        let value = part.to_str()?;
        if !value.is_empty() {
            parts.push(value.to_ascii_lowercase());
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
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
    let mut list_key = None;
    let mut list_items = Vec::new();
    for raw in fm.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if list_key.is_some() && line.starts_with('-') {
            list_items.push(unquote(line.trim_start_matches('-').trim()));
            continue;
        }
        if let Some(key) = list_key.take() {
            pairs.push((key, list_items.join(" ")));
            list_items.clear();
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim();
            if value.is_empty() {
                list_key = Some(key);
            } else {
                pairs.push((key, unquote(value)));
            }
        }
    }
    if let Some(key) = list_key {
        pairs.push((key, list_items.join(" ")));
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

fn is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false" | "no" | "0"
    )
}

fn parse_argument_names(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let list = trimmed
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(trimmed);
    list.split(|ch: char| ch == ',' || ch.is_whitespace())
        .map(|name| name.trim().trim_matches(['"', '\'']))
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn dedupe_by_name(out: &mut Vec<CustomCommand>) {
    let mut latest = std::collections::HashMap::new();
    for (idx, cmd) in out.iter().enumerate() {
        latest.insert((cmd.namespace.clone(), cmd.name.clone()), idx);
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
        assert_eq!(cmds[0].namespace, None);
        assert_eq!(cmds[0].description.as_deref(), Some("Review diff"));
        assert_eq!(cmds[0].arg_hint.as_deref(), Some("scope"));
        assert!(cmds[0].argument_names.is_empty());
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
    fn discovers_nested_commands_with_namespace_metadata() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/commands/team/audit.md"),
            "---\ndescription: Team audit\n---\nReview {{args}}",
        );
        write(
            &temp.path().join(".libertai/commands/team/audit.md"),
            "Project audit {{args}}",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "audit");
        assert_eq!(cmds[0].namespace.as_deref(), Some("team"));
        assert_eq!(cmds[0].body, "Project audit {{args}}");
        assert_eq!(expand(&cmds[0], "src"), "Project audit src");
    }

    #[test]
    fn expands_claude_argument_placeholders() {
        let command = CustomCommand {
            name: "review".into(),
            description: None,
            arg_hint: None,
            argument_names: Vec::new(),
            body: "all=$ARGUMENTS first=$0 second=$1 indexed=$ARGUMENTS[1] missing=$3 legacy={{ args }}".into(),
            source: CommandSource::Project,
            namespace: None,
            path: PathBuf::from(".claude/commands/review.md"),
        };

        assert_eq!(
            expand(&command, r#"src/lib.rs "high priority""#),
            "all=src/lib.rs \"high priority\" first=src/lib.rs second=high priority indexed=high priority missing= legacy=src/lib.rs \"high priority\""
        );
    }

    #[test]
    fn expands_named_argument_placeholders() {
        let command = CustomCommand {
            name: "review".into(),
            description: None,
            arg_hint: None,
            argument_names: vec!["path".into(), "priority".into()],
            body: "Review $path at $priority. Keep $unknown literal.".into(),
            source: CommandSource::Project,
            namespace: None,
            path: PathBuf::from(".claude/commands/review.md"),
        };

        assert_eq!(
            expand(&command, r#"src/lib.rs "high priority""#),
            "Review src/lib.rs at high priority. Keep $unknown literal."
        );
    }

    #[test]
    fn parses_named_argument_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/commands/review.md"),
            "---\narguments:\n  - path\n  - priority\nwhen_to_use: Use for focused review.\n---\nReview $path at $priority",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(cmds[0].argument_names, vec!["path", "priority"]);
        assert_eq!(
            cmds[0].description.as_deref(),
            Some("Review $path at $priority Use for focused review.")
        );
        assert_eq!(
            expand(&cmds[0], r#"src/lib.rs "high priority""#),
            "Review src/lib.rs at high priority"
        );
    }

    #[test]
    fn caps_prompt_descriptions_to_claude_listing_limit() {
        let temp = tempfile::tempdir().unwrap();
        let long = "x".repeat(DESCRIPTION_LIMIT + 32);
        write(
            &temp.path().join(".claude/commands/long.md"),
            &format!("---\ndescription: {long}\n---\nBody"),
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(
            cmds[0].description.as_ref().unwrap().chars().count(),
            DESCRIPTION_LIMIT
        );
    }

    #[test]
    fn user_invocable_false_commands_are_hidden() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/commands/hidden.md"),
            "---\ndescription: Hidden\nuser-invocable: false\n---\nHidden command",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert!(cmds.is_empty());
    }

    #[test]
    fn discovers_skills_as_slash_commands() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/skills/summarize/SKILL.md"),
            "---\ndescription: Summarize changes\nwhen_to_use: Use for review prep.\nargument-hint: scope\narguments: [scope]\n---\nSummarize $scope from ${CLAUDE_SKILL_DIR}.",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "summarize");
        assert_eq!(
            cmds[0].description.as_deref(),
            Some("Summarize changes Use for review prep.")
        );
        assert_eq!(cmds[0].arg_hint.as_deref(), Some("scope"));
        assert_eq!(cmds[0].argument_names, vec!["scope"]);
        assert_eq!(
            expand(&cmds[0], "repo"),
            format!(
                "Summarize repo from {}.",
                temp.path().join(".claude/skills/summarize").display()
            )
        );
    }

    #[test]
    fn skill_description_falls_back_to_first_paragraph() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/skills/explain/SKILL.md"),
            "---\nargument-hint: topic\n---\n# Explain\n\nExplain the topic clearly.\nInclude tradeoffs.",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(
            cmds[0].description.as_deref(),
            Some("Explain the topic clearly. Include tradeoffs.")
        );
    }

    #[test]
    fn skill_entry_overrides_same_named_command() {
        let temp = tempfile::tempdir().unwrap();
        write(&temp.path().join(".claude/commands/deploy.md"), "command");
        write(
            &temp.path().join(".claude/skills/deploy/SKILL.md"),
            "---\ndescription: Deploy skill\n---\nskill",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].body, "skill");
        assert_eq!(
            cmds[0].path.file_name().and_then(|name| name.to_str()),
            Some("SKILL.md")
        );
    }

    #[test]
    fn project_command_overrides_same_named_user_skill() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("repo");
        write(
            &home.join(".claude/skills/deploy/SKILL.md"),
            "---\ndescription: User deploy skill\n---\nuser skill",
        );
        write(&cwd.join(".claude/commands/deploy.md"), "project command");

        let cmds = discover_with_home(&cwd, Some(&home), None);

        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].body, "project command");
        assert_eq!(cmds[0].source, CommandSource::Project);
    }

    #[test]
    fn user_invocable_false_skills_are_hidden_from_slash_commands() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".claude/skills/background/SKILL.md"),
            "---\ndescription: Background knowledge\nuser-invocable: false\n---\nHidden",
        );

        let cmds = discover_with_home(temp.path(), None, None);

        assert!(cmds.is_empty());
    }

    #[test]
    fn expands_claude_context_placeholders() {
        let command = CustomCommand {
            name: "session-log".into(),
            description: None,
            arg_hint: None,
            argument_names: Vec::new(),
            body: "id=${CLAUDE_SESSION_ID} effort=${CLAUDE_EFFORT} dir=${CLAUDE_SKILL_DIR}".into(),
            source: CommandSource::Project,
            namespace: None,
            path: PathBuf::from("/repo/.claude/commands/session-log.md"),
        };

        assert_eq!(
            expand_with_context(
                &command,
                "",
                &ExpansionContext {
                    session_id: Some("sess-123".into()),
                    effort: Some("high".into()),
                },
            ),
            "id=sess-123 effort=high dir=/repo/.claude/commands"
        );
    }

    #[test]
    fn appends_arguments_when_template_has_no_placeholders() {
        let command = CustomCommand {
            name: "plain".into(),
            description: None,
            arg_hint: None,
            argument_names: Vec::new(),
            body: "Review this carefully.".into(),
            source: CommandSource::Project,
            namespace: None,
            path: PathBuf::from(".claude/commands/plain.md"),
        };

        assert_eq!(
            expand(&command, "src/lib.rs"),
            "Review this carefully.\n\nARGUMENTS: src/lib.rs"
        );
    }
}
