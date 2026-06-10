//! Native `/init` support for `libertai code`.
//!
//! Creates a small `AGENTS.md` bootstrap file from facts visible in the
//! repository without spending a model turn. Existing files are left
//! untouched; users can edit them directly once created.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub path: PathBuf,
    pub created: bool,
    pub content: String,
}

pub fn init_project(cwd: &Path) -> Result<InitResult> {
    init_project_with_notes(cwd, None)
}

pub fn init_project_with_notes(cwd: &Path, notes: Option<&str>) -> Result<InitResult> {
    let path = cwd.join("AGENTS.md");
    if path.exists() {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        return Ok(InitResult {
            path,
            created: false,
            content,
        });
    }
    let content = build_agents_md(cwd, notes)?;
    std::fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
    Ok(InitResult {
        path,
        created: true,
        content,
    })
}

pub fn agents_md_candidate(cwd: &Path, notes: Option<&str>) -> Result<String> {
    build_agents_md(cwd, notes)
}

pub fn onboarding_guide(cwd: &Path) -> Result<String> {
    let project = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("project");
    let mut lines = vec![
        format!("# {project} onboarding guide"),
        String::new(),
        "Share this local guide with a teammate or a future agent session. It is generated from repository files already present on disk.".to_string(),
        String::new(),
        "## Project snapshot".to_string(),
    ];
    let facts = project_fact_lines(cwd);
    if facts.is_empty() {
        lines.push("- Inspect the repository before making changes.".to_string());
    } else {
        lines.extend(facts);
    }

    lines.extend([String::new(), "## First commands to know".to_string()]);
    lines.extend(command_lines(cwd));

    lines.extend([String::new(), "## Important paths".to_string()]);
    let structure = structure_lines(cwd);
    if structure.is_empty() {
        lines.push("- Inspect the repository tree before choosing files to edit.".to_string());
    } else {
        lines.extend(structure);
    }

    let guidance = existing_guidance_summary(cwd);
    if !guidance.is_empty() {
        lines.extend([String::new(), "## Existing agent guidance".to_string()]);
        lines.extend(guidance);
    }

    lines.extend([
        String::new(),
        "## Working rules".to_string(),
        "- Keep changes scoped to the requested task.".to_string(),
        "- Prefer existing project commands and conventions over new tooling.".to_string(),
        "- Run the smallest relevant verification before handing work back.".to_string(),
        "- Cite changed files with line numbers when summarizing work.".to_string(),
        String::new(),
    ]);
    Ok(lines.join("\n"))
}

pub fn init_agent_prompt(notes: Option<&str>) -> String {
    const INIT_PROMPT: &str = r#"Initialize project context for this repository by creating or
updating AGENTS.md at the project root. AGENTS.md is the agent's
onboarding doc - future sessions read it automatically via pi's
AGENTS.md / CLAUDE.md ancestor walk and use it as part of the
system prompt.

1. Check whether AGENTS.md or CLAUDE.md already exists at the
   project root. If one does, read it and do not overwrite it
   without explicit ask_user approval. First produce a merge proposal
   with:
   - a fenced markdown block headed `AGENTS.md candidate` containing
     the complete proposed AGENTS.md content
   - a `Merge plan` section listing the exact headings/bullets to add,
     replace, or leave untouched
   - a unified diff against the existing file when possible

2. Otherwise, inspect the repo to identify:
   - the primary language and framework (read package.json /
     Cargo.toml / pyproject.toml / go.mod / etc.)
   - exact build / lint / test commands
   - the project structure: which directories matter, where
     source lives, where tests live
   - conventions visible in CONTRIBUTING.md / README.md /
     existing code style

3. Write AGENTS.md with these sections, terse - every line
   should carry a fact a future agent needs. Skip any section
   you cannot fill in from inspection; do not invent.

   # <project name>
   one-line summary.

   ## Build & test
   - install: <cmd>
   - lint:    <cmd>
   - test:    <cmd>

   ## Structure
   - src/      - <one-line>
   - tests/    - <one-line>
   - <etc.>

   ## Conventions
   - bullet list of code-style / process rules

4. If you write AGENTS.md, report what you wrote, citing the file path
   as AGENTS.md:1. If you do not write it because an existing guidance
   file needs user approval, report the candidate block, merge plan, and
   diff instead of making changes."#;
    let trimmed = notes.unwrap_or("").trim();
    if trimmed.is_empty() {
        INIT_PROMPT.to_string()
    } else {
        format!("{INIT_PROMPT}\n\nUser-provided project notes to consider:\n{trimmed}")
    }
}

pub fn extract_agents_md_candidate(text: &str) -> Option<String> {
    let normalized = text.replace("\r\n", "\n");
    let mut saw_label = false;
    let mut in_fence = false;
    let mut collected = Vec::new();
    for line in normalized.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if !in_fence
            && lower.contains("agents.md")
            && (lower.contains("candidate") || lower.contains("proposed"))
        {
            saw_label = true;
            continue;
        }
        if !in_fence && trimmed.starts_with("```") {
            let info = trimmed.trim_start_matches('`').trim().to_ascii_lowercase();
            if saw_label || info.contains("agents.md") || info.contains("candidate") {
                in_fence = true;
                saw_label = false;
                continue;
            }
        }
        if in_fence {
            if trimmed.starts_with("```") {
                let candidate = collected.join("\n").trim().to_string();
                return (!candidate.is_empty()).then_some(candidate);
            }
            collected.push(line.to_string());
        }
    }
    None
}

fn build_agents_md(cwd: &Path, notes: Option<&str>) -> Result<String> {
    let project = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("project");
    let mut lines = vec![
        format!("# {project}"),
        String::new(),
        "## Project facts".to_string(),
    ];
    let facts = project_fact_lines(cwd);
    if facts.is_empty() {
        lines.push(
            "- Identify the project type and entry points before making changes.".to_string(),
        );
    } else {
        lines.extend(facts);
    }
    lines.extend([String::new(), "## Build & test".to_string()]);
    for line in command_lines(cwd) {
        lines.push(line);
    }
    lines.push(String::new());
    lines.push("## Structure".to_string());
    let structure = structure_lines(cwd);
    if structure.is_empty() {
        lines.push("- Inspect the repository tree before making changes.".to_string());
    } else {
        lines.extend(structure);
    }
    lines.push(String::new());
    lines.push("## Conventions".to_string());
    if cwd.join("CONTRIBUTING.md").exists() {
        lines.push("- Read `CONTRIBUTING.md` before changing project conventions.".to_string());
    }
    if cwd.join(".editorconfig").exists() {
        lines.push("- Respect `.editorconfig` formatting rules.".to_string());
    }
    if let Some(note) = clean_user_note(notes) {
        lines.push(format!("- User-provided project note: {note}"));
    }
    lines.push("- Keep changes scoped to the requested task.".to_string());
    lines.push("- Prefer existing project patterns and commands over new tooling.".to_string());
    lines.push("- Run the relevant checks before handing work back.".to_string());
    lines.push(String::new());
    Ok(lines.join("\n"))
}

fn clean_user_note(notes: Option<&str>) -> Option<String> {
    notes
        .map(|note| note.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|note| !note.is_empty())
        .map(|note| truncate_sentence(&note))
}

fn existing_guidance_summary(cwd: &Path) -> Vec<String> {
    ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .filter_map(|name| {
            let path = cwd.join(name);
            let raw = std::fs::read_to_string(&path).ok()?;
            let excerpt = raw
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(8)
                .collect::<Vec<_>>()
                .join(" ");
            if excerpt.is_empty() {
                Some(format!("- `{name}` exists but is empty."))
            } else {
                Some(format!("- `{name}`: {}", truncate_sentence(&excerpt)))
            }
        })
        .collect()
}

fn command_lines(cwd: &Path) -> Vec<String> {
    if cwd.join("Cargo.toml").exists() {
        let mut lines = vec!["- build/check: `cargo check --locked`".to_string()];
        lines.push("- test: `cargo test --locked`".to_string());
        return lines;
    }
    if cwd.join("package.json").exists() {
        let manager = js_package_manager(cwd);
        let scripts = package_scripts(cwd);
        let mut lines = vec![format!("- install: `{manager} install`")];
        if scripts.contains(&"build".to_string()) {
            lines.push(package_command_line(cwd, manager, "build"));
        }
        if scripts.contains(&"test".to_string()) {
            lines.push(package_command_line(cwd, manager, "test"));
        }
        if scripts.contains(&"lint".to_string()) {
            lines.push(package_command_line(cwd, manager, "lint"));
        }
        if lines.len() == 1 {
            lines.push(format!(
                "- test: inspect `package.json` scripts before choosing a `{manager}` command"
            ));
        }
        return lines;
    }
    if cwd.join("pyproject.toml").exists() {
        return vec![
            "- install: `uv sync`".to_string(),
            "- test: `uv run pytest`".to_string(),
        ];
    }
    if cwd.join("go.mod").exists() {
        return vec![
            "- build/check: `go test ./...`".to_string(),
            "- test: `go test ./...`".to_string(),
        ];
    }
    vec!["- test: identify the project test command before changing behavior.".to_string()]
}

fn project_fact_lines(cwd: &Path) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(title) = markdown_title(&cwd.join("README.md")) {
        facts.push(format!("- README title: `{title}`."));
    }
    if let Some(summary) = markdown_summary(&cwd.join("README.md")) {
        facts.push(format!("- README summary: {summary}"));
    }
    if let Some(name) = cargo_project_name(cwd) {
        facts.push(format!("- Rust project: `{name}`."));
    }
    if let Some(name) = package_json_name(cwd) {
        facts.push(format!("- JavaScript package: `{name}`."));
    }
    if let Some(name) = pyproject_name(cwd) {
        facts.push(format!("- Python project: `{name}`."));
    }
    if let Some(module) = go_module_name(cwd) {
        facts.push(format!("- Go module: `{module}`."));
    }
    for (path, label) in [
        ("docker-compose.yml", "Docker Compose"),
        ("docker-compose.yaml", "Docker Compose"),
        ("Dockerfile", "Docker"),
        (".github/workflows", "GitHub Actions"),
        ("Makefile", "Makefile"),
    ] {
        if cwd.join(path).exists() {
            facts.push(format!("- Uses {label}: `{path}`."));
        }
    }
    for path in [
        "CONTRIBUTING.md",
        ".editorconfig",
        "rust-toolchain.toml",
        "mise.toml",
    ] {
        if cwd.join(path).exists() {
            facts.push(format!("- Project guidance/config: `{path}`."));
        }
    }
    facts
}

fn package_json(cwd: &Path) -> Option<JsonValue> {
    let raw = std::fs::read_to_string(cwd.join("package.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn package_json_name(cwd: &Path) -> Option<String> {
    package_json(cwd)?
        .get("name")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

fn package_scripts(cwd: &Path) -> Vec<String> {
    let Some(obj) =
        package_json(cwd).and_then(|v| v.get("scripts").and_then(|s| s.as_object()).cloned())
    else {
        return Vec::new();
    };
    obj.keys().cloned().collect()
}

fn package_script(cwd: &Path, name: &str) -> Option<String> {
    package_json(cwd)?
        .get("scripts")?
        .get(name)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

fn package_command_line(cwd: &Path, manager: &str, name: &str) -> String {
    let command = if name == "test" {
        format!("{manager} test")
    } else {
        format!("{manager} run {name}")
    };
    match package_script(cwd, name) {
        Some(script) => format!("- {name}: `{command}` (script: `{script}`)"),
        None => format!("- {name}: `{command}`"),
    }
}

fn markdown_title(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("# "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(truncate_sentence)
}

fn markdown_summary(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with("```")
                && !line.starts_with('!')
        })
        .find(|line| line.chars().any(char::is_alphabetic))
        .map(truncate_sentence)
}

fn truncate_sentence(value: &str) -> String {
    const MAX_CHARS: usize = 140;
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (idx, ch) in collapsed.chars().enumerate() {
        if idx >= MAX_CHARS {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn cargo_project_name(cwd: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(cwd.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    if let Some(name) = parsed
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        return Some(name.trim().to_string());
    }
    let members = parsed
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())?;
    Some(format!("workspace with {} member(s)", members.len()))
}

fn pyproject_name(cwd: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(cwd.join("pyproject.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    parsed
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

fn go_module_name(cwd: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(cwd.join("go.mod")).ok()?;
    raw.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("module "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn js_package_manager(cwd: &Path) -> &'static str {
    if cwd.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if cwd.join("yarn.lock").exists() {
        "yarn"
    } else if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
        "bun"
    } else {
        "npm"
    }
}

fn structure_lines(cwd: &Path) -> Vec<String> {
    [
        ("src", "source code"),
        ("app", "application code"),
        ("js", "frontend JavaScript"),
        ("src-tauri", "Tauri/Rust backend"),
        ("tests", "tests"),
        ("test", "tests"),
        ("scripts", "project scripts"),
        ("bin", "executables"),
        (".github/workflows", "GitHub Actions workflows"),
        ("docs", "documentation"),
        ("crates", "Rust workspace crates"),
        ("packages", "workspace packages"),
    ]
    .into_iter()
    .filter(|(dir, _)| cwd.join(dir).is_dir())
    .map(|(dir, label)| format!("- `{dir}/` - {label}."))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_project_creates_agents_md_for_rust_repo() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();

        let result = init_project(temp.path()).unwrap();

        assert!(result.created);
        assert!(result.path.ends_with("AGENTS.md"));
        assert!(result.content.contains("Rust project: `demo`"));
        assert!(result.content.contains("cargo check --locked"));
        assert!(result.content.contains("`src/`"));
        assert_eq!(
            std::fs::read_to_string(result.path).unwrap(),
            result.content
        );
    }

    #[test]
    fn init_project_preserves_existing_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("AGENTS.md");
        std::fs::write(&path, "custom\n").unwrap();

        let result = init_project(temp.path()).unwrap();

        assert!(!result.created);
        assert_eq!(result.content, "custom\n");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "custom\n");
    }

    #[test]
    fn agents_md_candidate_builds_without_overwriting_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("AGENTS.md");
        std::fs::write(&path, "custom\n").unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();

        let candidate = agents_md_candidate(temp.path(), Some(" prefer fast checks ")).unwrap();

        assert!(candidate.contains("Rust project: `demo`"));
        assert!(candidate.contains("User-provided project note: prefer fast checks"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "custom\n");
    }

    #[test]
    fn init_project_with_notes_adds_user_project_note() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();

        let result = init_project_with_notes(
            temp.path(),
            Some(" prefer snapshot tests and document public APIs "),
        )
        .unwrap();

        assert!(result.created);
        assert!(result.content.contains(
            "User-provided project note: prefer snapshot tests and document public APIs"
        ));
    }

    #[test]
    fn init_agent_prompt_adds_optional_notes_without_writing() {
        let prompt = init_agent_prompt(Some(" prefer existing Makefile targets "));

        assert!(prompt.contains("creating or\nupdating AGENTS.md"));
        assert!(prompt.contains("without explicit ask_user approval"));
        assert!(prompt.contains("AGENTS.md candidate"));
        assert!(prompt.contains("Merge plan"));
        assert!(prompt.contains("unified diff"));
        assert!(prompt.contains(
            "User-provided project notes to consider:\nprefer existing Makefile targets"
        ));
    }

    #[test]
    fn init_agent_prompt_omits_empty_notes_section() {
        let prompt = init_agent_prompt(Some("   "));

        assert!(
            prompt.contains("candidate block, merge plan, and\n   diff instead of making changes")
        );
        assert!(!prompt.contains("User-provided project notes"));
    }

    #[test]
    fn package_manager_prefers_pnpm_lock() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(js_package_manager(temp.path()), "pnpm");
    }

    #[test]
    fn init_project_uses_package_json_scripts_and_name() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"web-app","scripts":{"build":"vite build","lint":"eslint .","test":"vitest"}}"#,
        )
        .unwrap();
        std::fs::write(temp.path().join("pnpm-lock.yaml"), "").unwrap();

        let result = init_project(temp.path()).unwrap();

        assert!(result.content.contains("JavaScript package: `web-app`"));
        assert!(result
            .content
            .contains("build: `pnpm run build` (script: `vite build`)"));
        assert!(result
            .content
            .contains("test: `pnpm test` (script: `vitest`)"));
        assert!(result
            .content
            .contains("lint: `pnpm run lint` (script: `eslint .`)"));
    }

    #[test]
    fn extracts_fenced_agents_candidate_from_agent_response() {
        let text = r#"Here is the merge proposal.

AGENTS.md candidate

```markdown
# Demo

## Build & test
- test: cargo test
```

Merge plan:
- append Build & test
"#;
        assert_eq!(
            extract_agents_md_candidate(text).as_deref(),
            Some("# Demo\n\n## Build & test\n- test: cargo test")
        );
    }

    #[test]
    fn onboarding_guide_uses_repo_facts_and_existing_guidance() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        std::fs::write(temp.path().join("README.md"), "# Demo\n\nA test project.\n").unwrap();
        std::fs::write(
            temp.path().join("AGENTS.md"),
            "# Demo agents\n\nUse cargo test.\n",
        )
        .unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();

        let guide = onboarding_guide(temp.path()).unwrap();

        assert!(guide.contains("Rust project: `demo`"));
        assert!(guide.contains("README title: `Demo`"));
        assert!(guide.contains("cargo test --locked"));
        assert!(guide.contains("`src/`"));
        assert!(guide.contains("`AGENTS.md`: # Demo agents Use cargo test."));
    }

    #[test]
    fn init_project_records_python_and_go_project_names() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("pyproject.toml"),
            "[project]\nname = 'worker'\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("go.mod"), "module example.com/service\n").unwrap();
        std::fs::write(temp.path().join("Dockerfile"), "FROM scratch\n").unwrap();

        let result = init_project(temp.path()).unwrap();

        assert!(result.content.contains("Python project: `worker`"));
        assert!(result.content.contains("Go module: `example.com/service`"));
        assert!(result.content.contains("Uses Docker: `Dockerfile`"));
    }

    #[test]
    fn init_project_uses_readme_and_contributing_context() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("README.md"),
            "# Demo App\n\nA focused app for testing initializer context.\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("CONTRIBUTING.md"), "Run checks.\n").unwrap();
        std::fs::write(temp.path().join(".editorconfig"), "root = true\n").unwrap();
        std::fs::create_dir(temp.path().join("scripts")).unwrap();
        std::fs::create_dir_all(temp.path().join(".github/workflows")).unwrap();

        let result = init_project(temp.path()).unwrap();

        assert!(result.content.contains("README title: `Demo App`"));
        assert!(result.content.contains("README summary: A focused app"));
        assert!(result
            .content
            .contains("Project guidance/config: `CONTRIBUTING.md`"));
        assert!(result.content.contains("Read `CONTRIBUTING.md`"));
        assert!(result.content.contains("Respect `.editorconfig`"));
        assert!(result.content.contains("`scripts/` - project scripts"));
        assert!(result.content.contains("`.github/workflows/`"));
    }
}
