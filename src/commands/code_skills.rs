//! Agent Skills loader for `libertai code` and LiberClaw desktop sessions.
//!
//! The on-disk format follows https://agentskills.io/specification:
//! a skill directory containing `SKILL.md` with YAML frontmatter and a
//! Markdown body. LibertAI-specific routing lives under `metadata.*`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

impl SkillSource {
    fn kind(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Project(_) => "project",
            Self::User(_) => "user",
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Self::Builtin => None,
            Self::Project(path) | Self::User(path) => Some(path.as_path()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillInventoryEntry {
    pub name: String,
    pub description: String,
    pub allowed_tools: Option<String>,
    pub body: String,
    pub source: String,
    pub source_kind: String,
    pub path: Option<PathBuf>,
    pub agent_created: bool,
    pub enabled: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DisabledSkillsConfig {
    #[serde(default)]
    disabled: Vec<String>,
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

    // (M5/#7) Latent registry: surface each active skill's name +
    // description only. The bodies are deliberately NOT in the system
    // prompt — they bloat it and most are irrelevant to a given turn. The
    // model loads a skill's full body on demand via the `skill` tool
    // (see `code_skill_tool.rs`), mirroring Claude Code's Skill-tool model.
    let mut out = String::from("## Available Agent Skills\n\n");
    out.push_str(
        "The following Agent Skills are available for this session. Their full bodies are \
         NOT shown here to keep this prompt short — call the `skill` tool with a skill's \
         `name` to load its complete instructions when a task seems to match. Read each \
         description to decide which skill applies.\n",
    );
    for skill in &skills {
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
    }
    Ok(Some(out))
}

/// (M5/#7) Load a single active skill's full body by name. Returns `None`
/// when no active (pillar-matching, non-disabled) skill has that name, so
/// the `skill` tool can report "unknown skill" and list the available
/// names to drive a retry. Reuses [`active_skills`] so pillar gating and
/// the disabled-skills list are honored — a disabled or wrong-pillar
/// skill is not loadable by name.
pub fn load_skill_by_name(
    pillar: SkillPillar,
    cwd: Option<&Path>,
    name: &str,
) -> Result<Option<AgentSkill>> {
    let name = name.trim();
    Ok(active_skills(pillar, cwd)?
        .into_iter()
        .find(|skill| skill.name == name))
}

/// (M5/#7) Active skill names only — the list the `skill` tool returns
/// when the model asks for an unknown name (drives a retry with the
/// correct spelling).
pub fn active_skill_name_list(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Vec<String>> {
    active_skill_names(pillar, cwd)
}

pub fn active_skill_names(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Vec<String>> {
    Ok(active_skills(pillar, cwd)?
        .into_iter()
        .map(|skill| skill.name)
        .collect())
}

pub fn active_skills(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Vec<AgentSkill>> {
    let disabled = load_disabled_skill_names()?;
    Ok(collect_matching_skills(pillar, cwd)?
        .into_iter()
        .filter(|skill| !disabled.contains(&skill.name))
        .collect())
}

pub fn skill_inventory(
    pillar: SkillPillar,
    cwd: Option<&Path>,
) -> Result<Vec<SkillInventoryEntry>> {
    let disabled = load_disabled_skill_names()?;
    Ok(collect_matching_skills(pillar, cwd)?
        .into_iter()
        .map(|skill| {
            let agent_created = skill
                .metadata
                .get("libertai.created_by")
                .or_else(|| skill.metadata.get("libertai.created-by"))
                .map(|value| value.eq_ignore_ascii_case("agent"))
                .unwrap_or(false);
            SkillInventoryEntry {
                enabled: !disabled.contains(&skill.name),
                name: skill.name,
                description: skill.description,
                allowed_tools: skill.allowed_tools,
                body: skill.body,
                source: source_label(&skill.source),
                source_kind: skill.source.kind().to_string(),
                path: skill.source.path().map(Path::to_path_buf),
                agent_created,
            }
        })
        .collect())
}

pub fn set_skill_enabled(name: &str, enabled: bool) -> Result<()> {
    let name = name.trim();
    validate_name(name)?;
    let mut disabled = load_disabled_skill_names()?;
    if enabled {
        disabled.remove(name);
    } else {
        disabled.insert(name.to_string());
    }
    save_disabled_skill_names(&disabled)
}

fn collect_matching_skills(pillar: SkillPillar, cwd: Option<&Path>) -> Result<Vec<AgentSkill>> {
    collect_matching_skills_with_roots(
        pillar,
        cwd,
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
    )
}

fn collect_matching_skills_with_roots(
    pillar: SkillPillar,
    cwd: Option<&Path>,
    home: Option<&Path>,
    config: Option<&Path>,
) -> Result<Vec<AgentSkill>> {
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
            &cwd.join(".claude").join("skills"),
            SkillSourceKind::Project,
            pillar,
            &mut skills,
        )?;
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

    if let Some(home) = home {
        load_skill_dir(
            &home.join(".claude").join("skills"),
            SkillSourceKind::User,
            pillar,
            &mut skills,
        )?;
    }

    if let Some(config) = config {
        load_skill_dir(
            &config.join("libertai").join("skills"),
            SkillSourceKind::User,
            pillar,
            &mut skills,
        )?;
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.dedup_by(|a, b| a.name == b.name);
    Ok(skills)
}

fn disabled_skills_path() -> Result<PathBuf> {
    Ok(crate::config::libertai_config_dir()?.join("disabled-skills.toml"))
}

fn load_disabled_skill_names() -> Result<BTreeSet<String>> {
    let path = disabled_skills_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg: DisabledSkillsConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg
        .disabled
        .into_iter()
        .filter_map(|name| {
            let name = name.trim().to_string();
            validate_name(&name).ok().map(|_| name)
        })
        .collect())
}

fn save_disabled_skill_names(disabled: &BTreeSet<String>) -> Result<()> {
    let path = disabled_skills_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let cfg = DisabledSkillsConfig {
        disabled: disabled.iter().cloned().collect(),
    };
    let raw = toml::to_string_pretty(&cfg).context("serializing disabled skills")?;
    std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
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

pub(crate) fn parse_skill_md(
    text: &str,
    dir_name: Option<&str>,
    source: SkillSource,
) -> Result<AgentSkill> {
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
        .split([',', ' ', ';'])
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

/// (M5/#7) Serializes tests that read or mutate the process-global
/// disabled-skills config (`libertai_config_dir()/disabled-skills.toml`).
/// `active_skills` / `load_skill_by_name` / `prompt_for_pillar` all read
/// that file, and the `DisabledGuard` in `code_skill_tool::tests` writes
/// it — without this lock a parallel sibling asserting "harness is in
/// the prompt" races a test that's temporarily disabled harness. Lock
/// held by every test that touches the active-skill set. Lives at module
/// scope (not inside `mod tests`) so `code_skill_tool::tests` can name
/// it as `code_skills::SKILLS_CONFIG_TEST_LOCK`.
#[cfg(test)]
pub(crate) static SKILLS_CONFIG_TEST_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

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
        // alphabetically. Held under the config lock: a parallel sibling
        // (DisabledGuard in code_skill_tool::tests) can disable a builtin
        // mid-test, which would drop it from this list.
        let _guard = super::SKILLS_CONFIG_TEST_LOCK.lock().unwrap();
        let names = active_skill_names(SkillPillar::Code, None).expect("names");
        assert_eq!(names, vec!["libertai-code-workflow", "libertai-harness"]);
    }

    #[test]
    fn inventory_entries_mark_disabled_names() {
        let mut disabled = BTreeSet::new();
        disabled.insert("libertai-harness".to_string());
        let entries = collect_matching_skills(SkillPillar::Code, None)
            .expect("skills")
            .into_iter()
            .map(|skill| {
                let agent_created = skill
                    .metadata
                    .get("libertai.created_by")
                    .or_else(|| skill.metadata.get("libertai.created-by"))
                    .map(|value| value.eq_ignore_ascii_case("agent"))
                    .unwrap_or(false);
                SkillInventoryEntry {
                    enabled: !disabled.contains(&skill.name),
                    name: skill.name,
                    description: skill.description,
                    allowed_tools: skill.allowed_tools,
                    body: skill.body,
                    source: source_label(&skill.source),
                    source_kind: skill.source.kind().to_string(),
                    path: skill.source.path().map(Path::to_path_buf),
                    agent_created,
                }
            })
            .collect::<Vec<_>>();

        let harness = entries
            .iter()
            .find(|skill| skill.name == "libertai-harness")
            .expect("harness");
        assert!(!harness.enabled);
        let workflow = entries
            .iter()
            .find(|skill| skill.name == "libertai-code-workflow")
            .expect("workflow");
        assert!(workflow.enabled);
    }

    #[test]
    fn inventory_entry_carries_source_metadata() {
        let skill = parse_skill_md(
            "---\nname: proposed-skill\ndescription: Proposed skill.\nmetadata:\n  libertai.pillars: code\n  libertai.created_by: agent\n---\n# Body\n",
            Some("proposed-skill"),
            SkillSource::User(PathBuf::from("/tmp/proposed-skill")),
        )
        .expect("parse");
        let entry = SkillInventoryEntry {
            enabled: true,
            name: skill.name,
            description: skill.description,
            allowed_tools: skill.allowed_tools,
            body: skill.body,
            source: source_label(&skill.source),
            source_kind: skill.source.kind().to_string(),
            path: skill.source.path().map(Path::to_path_buf),
            agent_created: skill
                .metadata
                .get("libertai.created_by")
                .map(|value| value.eq_ignore_ascii_case("agent"))
                .unwrap_or(false),
        };

        assert_eq!(entry.source_kind, "user");
        assert_eq!(
            entry.path.as_deref(),
            Some(Path::new("/tmp/proposed-skill"))
        );
        assert!(entry.agent_created);
    }

    #[test]
    fn project_claude_skill_root_is_loaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".claude/skills/claude-review");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: claude-review\ndescription: Claude-compatible review skill.\nmetadata:\n  libertai.pillars: code\n---\nPrefer focused review findings.\n",
        )
        .expect("write skill");

        let entries = skill_inventory(SkillPillar::Code, Some(dir.path())).expect("inventory");
        let skill = entries
            .iter()
            .find(|skill| skill.name == "claude-review")
            .expect(".claude skill");
        assert_eq!(skill.source_kind, "project");
        assert!(skill
            .path
            .as_ref()
            .expect("path")
            .ends_with(".claude/skills/claude-review"));
        assert!(skill.body.contains("Prefer focused review findings."));
    }

    #[test]
    fn user_libertai_skill_root_uses_config_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let config = dir.path().join("xdg-config");
        let skill_dir = config.join("libertai/skills/config-review");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: config-review\ndescription: Config-dir review skill.\nmetadata:\n  libertai.pillars: code\n---\nUse the configured skill root.\n",
        )
        .expect("write skill");

        let skills =
            collect_matching_skills_with_roots(SkillPillar::Code, None, Some(&home), Some(&config))
                .expect("skills");
        let skill = skills
            .iter()
            .find(|skill| skill.name == "config-review")
            .expect("config-dir skill");
        assert!(matches!(skill.source, SkillSource::User(_)));
        assert!(source_label(&skill.source).contains("xdg-config/libertai/skills/config-review"));
        assert!(skill.body.contains("Use the configured skill root."));
    }

    #[test]
    fn code_prompt_lists_skills_as_latent_registry_without_bodies() {
        // (M5/#7) prompt_for_pillar now surfaces a latent registry
        // (name + description only); bodies load via the `skill` tool.
        // Held under the config lock so a parallel DisabledGuard can't
        // drop a builtin mid-assertion.
        let _guard = super::SKILLS_CONFIG_TEST_LOCK.lock().unwrap();
        let prompt = prompt_for_pillar(SkillPillar::Code, None)
            .expect("prompt")
            .expect("code prompt");
        // Registry header + the instruction to use the `skill` tool.
        assert!(
            prompt.contains("## Available Agent Skills"),
            "header: {prompt:?}"
        );
        assert!(
            prompt.contains("call the `skill` tool"),
            "tool hint: {prompt:?}"
        );
        // The harness + code-workflow skills are active for Code; their
        // names + descriptions appear…
        assert!(
            prompt.contains("### libertai-harness"),
            "harness name: {prompt:?}"
        );
        assert!(
            prompt.contains("### libertai-code-workflow"),
            "workflow name: {prompt:?}"
        );
        // …but their bodies do NOT — the whole point of the refactor.
        assert!(
            !prompt.contains("## Auto memory"),
            "harness body leaked into prompt: {prompt:?}"
        );
        assert!(
            !prompt.contains("/remember <kind>: <short fact>"),
            "harness body leaked into prompt: {prompt:?}"
        );
    }

    #[test]
    fn harness_auto_memory_protocol_is_loadable_by_name() {
        // (M5/#7) The auto-memory protocol that used to live in the
        // system prompt is now reachable via load_skill_by_name — the
        // `skill` tool hands this body to the model on call. Held under
        // the config lock so a parallel DisabledGuard can't disable
        // harness mid-load.
        let _guard = super::SKILLS_CONFIG_TEST_LOCK.lock().unwrap();
        let cwd = std::env::current_dir().ok();
        let harness = load_skill_by_name(SkillPillar::Code, cwd.as_deref(), "libertai-harness")
            .expect("load")
            .expect("harness active for code");
        assert_eq!(harness.name, "libertai-harness");
        assert!(harness.body.contains("## Auto memory"));
        assert!(harness.body.contains("stable user preferences"));
        assert!(harness.body.contains("durable repository facts"));
        assert!(harness.body.contains("Do not save transient facts"));
        assert!(harness.body.contains("/remember <kind>: <short fact>"));
    }
}
