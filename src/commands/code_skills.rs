//! Agent Skills loader for `libertai code` and LiberClaw desktop sessions.
//!
//! The on-disk format follows https://agentskills.io/specification:
//! a skill directory containing `SKILL.md` with YAML frontmatter and a
//! Markdown body. LibertAI-specific routing lives under `metadata.*`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPillar {
    Code,
    Agent,
    Chat,
}

impl SkillPillar {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "code" => Some(Self::Code),
            "agent" => Some(Self::Agent),
            "chat" => Some(Self::Chat),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Agent => "agent",
            Self::Chat => "chat",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentSkill {
    pub name: String,
    pub description: String,
    pub allowed_tools: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub body: String,
    pub source: SkillSource,
}

#[derive(Debug, Clone)]
pub enum SkillSource {
    Builtin,
    Project(PathBuf),
    User(PathBuf),
}

struct BuiltinSkill {
    dir_name: &'static str,
    body: &'static str,
}

const BUILTINS: &[BuiltinSkill] = &[
    // Harness applies to every pillar (libertai.pillars: any) —
    // behavioral guidance + per-tool usage notes that shape tone,
    // tool selection, and execution caution. Alphabetical sort in
    // active_skills puts code-workflow before harness in the final
    // prompt; both still reach the model.
    BuiltinSkill {
        dir_name: "libertai-harness",
        body: include_str!("../agent_skills/libertai-harness/SKILL.md"),
    },
    BuiltinSkill {
        dir_name: "libertai-chat-research",
        body: include_str!("../agent_skills/libertai-chat-research/SKILL.md"),
    },
    BuiltinSkill {
        dir_name: "libertai-agent-ops",
        body: include_str!("../agent_skills/libertai-agent-ops/SKILL.md"),
    },
    BuiltinSkill {
        dir_name: "libertai-code-workflow",
        body: include_str!("../agent_skills/libertai-code-workflow/SKILL.md"),
    },
];

pub fn prompt_for_pillar(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Option<String>> {
    let skills = active_skills(pillar, cwd)?;
    if skills.is_empty() {
        return Ok(None);
    }

    let mut out = String::from("## Active Agent Skills\n\n");
    out.push_str("The following Agent Skills are active for this session. Apply them when relevant.\n");
    for skill in skills {
        out.push_str("\n### ");
        out.push_str(&skill.name);
        out.push('\n');
        out.push_str("Description: ");
        out.push_str(&skill.description);
        out.push('\n');
        if let Some(allowed) = skill.allowed_tools.as_deref() {
            out.push_str("Allowed tools hint: ");
            out.push_str(allowed);
            out.push('\n');
        }
        out.push_str("Source: ");
        out.push_str(&source_label(&skill.source));
        out.push_str("\n\n");
        out.push_str(skill.body.trim());
        out.push('\n');
    }
    Ok(Some(out))
}

pub fn active_skill_names(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Vec<String>> {
    Ok(active_skills(pillar, cwd)?
        .into_iter()
        .map(|skill| skill.name)
        .collect())
}

pub fn active_skills(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Vec<AgentSkill>> {
    let mut skills = Vec::new();

    for builtin in BUILTINS {
        let parsed = parse_skill_md(builtin.body, Some(builtin.dir_name), SkillSource::Builtin)
            .with_context(|| format!("parsing bundled skill {}", builtin.dir_name))?;
        if skill_matches_pillar(&parsed, pillar) {
            skills.push(parsed);
        }
    }

    if let Some(cwd) = cwd {
        load_skill_dir(
            &cwd.join(".libertai").join("skills"),
            SkillSourceKind::Project,
            pillar,
            &mut skills,
        )?;
        load_skill_dir(
            &cwd.join(".agents").join("skills"),
            SkillSourceKind::Project,
            pillar,
            &mut skills,
        )?;
    }

    if let Some(home) = dirs::home_dir() {
        load_skill_dir(
            &home.join(".config").join("libertai").join("skills"),
            SkillSourceKind::User,
            pillar,
            &mut skills,
        )?;
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.dedup_by(|a, b| a.name == b.name);
    Ok(skills)
}

#[derive(Clone, Copy)]
enum SkillSourceKind {
    Project,
    User,
}

fn load_skill_dir(
    base: &Path,
    kind: SkillSourceKind,
    pillar: SkillPillar,
    out: &mut Vec<AgentSkill>,
) -> Result<()> {
    if !base.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(base).with_context(|| format!("reading {}", base.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_path = path.join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&skill_path)
            .with_context(|| format!("reading {}", skill_path.display()))?;
        let dir_name = path.file_name().and_then(|s| s.to_str());
        let source = match kind {
            SkillSourceKind::Project => SkillSource::Project(path.clone()),
            SkillSourceKind::User => SkillSource::User(path.clone()),
        };
        let skill = parse_skill_md(&text, dir_name, source)
            .with_context(|| format!("parsing {}", skill_path.display()))?;
        if skill_matches_pillar(&skill, pillar) {
            out.push(skill);
        }
    }
    Ok(())
}

fn parse_skill_md(text: &str, dir_name: Option<&str>, source: SkillSource) -> Result<AgentSkill> {
    let (frontmatter, body) = split_frontmatter(text)?;
    let mut name = None;
    let mut description = None;
    let mut allowed_tools = None;
    let mut metadata = BTreeMap::new();
    let mut in_metadata = false;

    for raw in frontmatter.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        if raw.starts_with(' ') || raw.starts_with('\t') {
            if in_metadata {
                if let Some((k, v)) = split_key_value(line.trim()) {
                    metadata.insert(k.to_string(), unquote(v));
                }
            }
            continue;
        }

        in_metadata = false;
        if let Some((k, v)) = split_key_value(line) {
            match k {
                "name" => name = Some(unquote(v)),
                "description" => description = Some(unquote(v)),
                "allowed-tools" => allowed_tools = Some(unquote(v)),
                "metadata" => in_metadata = true,
                _ => {}
            }
        }
    }

    let name = name.context("skill frontmatter missing `name`")?;
    validate_name(&name)?;
    if let Some(dir_name) = dir_name {
        if name != dir_name {
            anyhow::bail!("skill name `{name}` does not match directory `{dir_name}`");
        }
    }

    let description = description.context("skill frontmatter missing `description`")?;
    if description.trim().is_empty() {
        anyhow::bail!("skill `{name}` has an empty description");
    }

    Ok(AgentSkill {
        name,
        description,
        allowed_tools,
        metadata,
        body: body.to_string(),
        source,
    })
}

fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .context("SKILL.md must start with YAML frontmatter")?;
    let marker = if let Some(i) = rest.find("\n---\n") {
        (i, 5)
    } else if let Some(i) = rest.find("\r\n---\r\n") {
        (i, 7)
    } else {
        anyhow::bail!("SKILL.md frontmatter is not closed with ---");
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

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("skill name must be 1-64 characters");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        anyhow::bail!("invalid skill name `{name}`");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!("invalid skill name `{name}`");
    }
    Ok(())
}

fn skill_matches_pillar(skill: &AgentSkill, pillar: SkillPillar) -> bool {
    let Some(pillars) = skill.metadata.get("libertai.pillars") else {
        return false;
    };
    pillars
        .split(|c: char| c == ',' || c == ' ' || c == ';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|p| p == "any" || p == pillar.as_str())
}

fn source_label(source: &SkillSource) -> String {
    match source {
        SkillSource::Builtin => "builtin".to_string(),
        SkillSource::Project(path) => format!("project:{}", path.display()),
        SkillSource::User(path) => format!("user:{}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_skill_with_metadata() {
        let skill = parse_skill_md(
            "---\nname: demo-skill\ndescription: Demo skill.\nallowed-tools: search fetch\nmetadata:\n  libertai.pillars: chat code\n---\n# Body\n",
            Some("demo-skill"),
            SkillSource::Builtin,
        )
        .expect("parse");

        assert_eq!(skill.name, "demo-skill");
        assert_eq!(skill.allowed_tools.as_deref(), Some("search fetch"));
        assert!(skill_matches_pillar(&skill, SkillPillar::Chat));
        assert!(skill_matches_pillar(&skill, SkillPillar::Code));
        assert!(!skill_matches_pillar(&skill, SkillPillar::Agent));
    }

    #[test]
    fn rejects_name_directory_mismatch() {
        let err = parse_skill_md(
            "---\nname: demo-skill\ndescription: Demo skill.\n---\n# Body\n",
            Some("other-skill"),
            SkillSource::Builtin,
        )
        .expect_err("mismatch should fail");
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn selects_builtin_pillar_skills() {
        // libertai-harness ships with `libertai.pillars: any` so it
        // joins every pillar; active_skills sorts the merged set
        // alphabetically.
        let names = active_skill_names(SkillPillar::Code, None).expect("names");
        assert_eq!(names, vec!["libertai-code-workflow", "libertai-harness"]);
    }
}
