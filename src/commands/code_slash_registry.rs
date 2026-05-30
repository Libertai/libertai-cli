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
    command.body.replace("{{args}}", args.trim())
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
            "argHint" | "arg_hint" | "arg-hint" => arg_hint = Some(value),
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
            "---\ndescription: Review diff\nargHint: scope\n---\nReview {{args}}",
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
}
