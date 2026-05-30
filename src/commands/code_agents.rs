//! Named sub-agent definitions for the `task` tool.
//!
//! Claude Code-compatible project definitions live in
//! `.claude/agents/<name>.md`; LibertAI also reads `.libertai/agents`
//! and user-level `~/.config/libertai/agents` / `~/.claude/agents`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    Project(PathBuf),
    User(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub worktree: bool,
    pub system_prompt: String,
    pub source: AgentSource,
}

/// Discover named sub-agents for `cwd`. Project definitions override
/// user definitions by name, matching custom slash command precedence.
pub fn discover_agents(cwd: &Path) -> Result<Vec<AgentDefinition>> {
    let mut by_name = BTreeMap::<String, AgentDefinition>::new();
    for dir in user_agent_dirs() {
        load_dir(&dir, AgentSource::User(dir.clone()), &mut by_name)?;
    }
    for dir in project_agent_dirs(cwd) {
        load_dir(&dir, AgentSource::Project(dir.clone()), &mut by_name)?;
    }
    Ok(by_name.into_values().collect())
}

pub fn find_agent(cwd: &Path, name: &str) -> Result<Option<AgentDefinition>> {
    let needle = name.trim().trim_start_matches('@');
    if needle.is_empty() {
        return Ok(None);
    }
    let agents = discover_agents(cwd)?;
    Ok(agents
        .iter()
        .find(|a| a.name == needle)
        .cloned()
        .or_else(|| agents.into_iter().find(|a| a.name.starts_with(needle))))
}

pub fn agent_names(cwd: &Path) -> Result<Vec<String>> {
    Ok(discover_agents(cwd)?.into_iter().map(|a| a.name).collect())
}

pub fn create_project_agent(
    cwd: &Path,
    name: &str,
    description: Option<&str>,
    worktree: bool,
) -> Result<PathBuf> {
    let name = name.trim().trim_start_matches('@');
    validate_name(name)?;
    let dir = cwd.join(".libertai").join("agents");
    let path = dir.join(format!("{name}.md"));
    if path.exists() {
        anyhow::bail!("agent `{name}` already exists at {}", path.display());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let description = description
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Focused project sub-agent");
    let content = render_agent_template(name, description, worktree);
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn delete_agent(cwd: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim().trim_start_matches('@');
    validate_name(name)?;
    let Some(agent) = find_agent(cwd, name)? else {
        anyhow::bail!("agent `{name}` not found");
    };
    let dir = match agent.source {
        AgentSource::Project(dir) | AgentSource::User(dir) => dir,
    };
    let path = dir.join(format!("{}.md", agent.name));
    if path.extension().and_then(|s| s.to_str()) != Some("md") {
        anyhow::bail!("agent files must use the .md extension");
    }
    if !path.exists() {
        anyhow::bail!("agent file does not exist: {}", path.display());
    }
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    Ok(path)
}

fn project_agent_dirs(cwd: &Path) -> Vec<PathBuf> {
    vec![cwd.join(".claude").join("agents"), cwd.join(".libertai").join("agents")]
}

fn user_agent_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".claude").join("agents"));
    }
    if let Some(config) = dirs::config_dir() {
        dirs.push(config.join("libertai").join("agents"));
    }
    dirs
}

fn load_dir(
    dir: &Path,
    source: AgentSource,
    out: &mut BTreeMap<String, AgentDefinition>,
) -> Result<()> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in read_dir {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file_name = path.file_stem().and_then(|s| s.to_str());
        let agent = parse_agent_md(&text, file_name, source.clone())
            .with_context(|| format!("parsing {}", path.display()))?;
        out.insert(agent.name.clone(), agent);
    }
    Ok(())
}

pub(crate) fn parse_agent_md(
    text: &str,
    file_name: Option<&str>,
    source: AgentSource,
) -> Result<AgentDefinition> {
    let (frontmatter, body) = split_frontmatter(text)?;
    let mut name = file_name.map(str::to_string);
    let mut description = None;
    let mut tools = None;
    let mut model = None;
    let mut worktree = false;

    for raw in frontmatter.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || raw.starts_with(' ') || raw.starts_with('\t') {
            continue;
        }
        let Some((k, v)) = split_key_value(line) else {
            continue;
        };
        match k {
            "name" => name = Some(unquote(v)),
            "description" => description = Some(unquote(v)),
            "tools" | "allowed-tools" => tools = Some(parse_list(v)),
            "model" => model = Some(unquote(v)),
            "worktree" => worktree = parse_bool(v),
            "isolation" => worktree = unquote(v).eq_ignore_ascii_case("worktree"),
            _ => {}
        }
    }

    let name = name.context("agent definition missing name")?;
    validate_name(&name)?;
    if let Some(file_name) = file_name {
        if name != file_name {
            anyhow::bail!("agent name `{name}` does not match file `{file_name}`");
        }
    }
    let description = description.unwrap_or_else(|| "Named sub-agent".to_string());
    if body.trim().is_empty() {
        anyhow::bail!("agent `{name}` has an empty system prompt");
    }
    Ok(AgentDefinition {
        name,
        description,
        tools: tools.filter(|t| !t.is_empty()),
        model: model.filter(|m| !m.trim().is_empty()),
        worktree,
        system_prompt: body.trim().to_string(),
        source,
    })
}

fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok(("", text));
    };
    let marker = if let Some(i) = rest.find("\n---\n") {
        (i, 5)
    } else if let Some(i) = rest.find("\r\n---\r\n") {
        (i, 7)
    } else {
        anyhow::bail!("agent frontmatter is not closed with ---");
    };
    Ok((&rest[..marker.0], &rest[marker.0 + marker.1..]))
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    Some((k.trim(), v.trim()))
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
        {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

fn parse_list(value: &str) -> Vec<String> {
    let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(|s| unquote(s).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_bool(value: &str) -> bool {
    matches!(
        unquote(value).to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "worktree"
    )
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("agent name must be 1-64 characters");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        anyhow::bail!("invalid agent name `{name}`");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!("invalid agent name `{name}`");
    }
    Ok(())
}

fn render_agent_template(name: &str, description: &str, worktree: bool) -> String {
    let description = yaml_single_quote(description);
    let worktree = if worktree { "true" } else { "false" };
    format!(
        "---\nname: {name}\ndescription: {description}\ntools: read, grep, find, ls\nworktree: {worktree}\n---\nYou are a focused project sub-agent. Inspect only the files and commands needed for the assigned task, report findings concisely, and cite file paths or commands you used.\n"
    )
}

fn yaml_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''").replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_frontmatter() {
        let agent = parse_agent_md(
            "---\nname: reviewer\ndescription: Reviews changes\ntools: read, grep, find\nmodel: gpt-4o\n---\nFocus on correctness.",
            Some("reviewer"),
            AgentSource::User(PathBuf::from("/tmp/agents")),
        )
        .expect("parse");

        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "Reviews changes");
        assert_eq!(agent.tools, Some(vec!["read".into(), "grep".into(), "find".into()]));
        assert_eq!(agent.model.as_deref(), Some("gpt-4o"));
        assert!(!agent.worktree);
        assert_eq!(agent.system_prompt, "Focus on correctness.");
    }

    #[test]
    fn parses_agent_worktree_default() {
        let agent = parse_agent_md(
            "---\nname: reviewer\nworktree: true\n---\nFocus on correctness.",
            Some("reviewer"),
            AgentSource::User(PathBuf::from("/tmp/agents")),
        )
        .expect("parse");
        assert!(agent.worktree);

        let agent = parse_agent_md(
            "---\nname: reviewer\nisolation: worktree\n---\nFocus on correctness.",
            Some("reviewer"),
            AgentSource::User(PathBuf::from("/tmp/agents")),
        )
        .expect("parse");
        assert!(agent.worktree);
    }

    #[test]
    fn creates_project_agent_template() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create_project_agent(
            tmp.path(),
            "reviewer",
            Some("Reviews project changes"),
            true,
        )
        .unwrap();
        assert_eq!(
            path,
            tmp.path().join(".libertai").join("agents").join("reviewer.md")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("name: reviewer"));
        assert!(text.contains("description: 'Reviews project changes'"));
        assert!(text.contains("worktree: true"));

        let agent = parse_agent_md(
            &text,
            Some("reviewer"),
            AgentSource::Project(path.parent().unwrap().to_path_buf()),
        )
        .unwrap();
        assert_eq!(agent.name, "reviewer");
        assert!(agent.worktree);
        assert!(create_project_agent(tmp.path(), "reviewer", None, false).is_err());
    }

    #[test]
    fn deletes_discovered_agent_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create_project_agent(tmp.path(), "reviewer", None, false).unwrap();
        assert!(path.exists());

        let deleted = delete_agent(tmp.path(), "reviewer").unwrap();
        assert_eq!(deleted, path);
        assert!(!deleted.exists());
        assert!(delete_agent(tmp.path(), "reviewer").is_err());
    }

    #[test]
    fn project_agents_override_user_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        let user_agents = tmp.path().join("user-agents");
        let project_agents = cwd.join(".claude").join("agents");
        std::fs::create_dir_all(&user_agents).unwrap();
        std::fs::create_dir_all(&project_agents).unwrap();
        std::fs::write(
            user_agents.join("reviewer.md"),
            "---\ndescription: User\n---\nUser prompt.",
        )
        .unwrap();
        std::fs::write(
            project_agents.join("reviewer.md"),
            "---\ndescription: Project\n---\nProject prompt.",
        )
        .unwrap();

        let mut by_name = BTreeMap::new();
        load_dir(&user_agents, AgentSource::User(user_agents.clone()), &mut by_name).unwrap();
        load_dir(
            &project_agents,
            AgentSource::Project(project_agents.clone()),
            &mut by_name,
        )
        .unwrap();
        let agents: Vec<_> = by_name.into_values().collect();

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].description, "Project");
        assert_eq!(agents[0].system_prompt, "Project prompt.");
    }
}
