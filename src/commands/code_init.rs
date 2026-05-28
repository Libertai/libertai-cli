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
    let path = cwd.join("AGENTS.md");
    if path.exists() {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        return Ok(InitResult {
            path,
            created: false,
            content,
        });
    }
    let content = build_agents_md(cwd)?;
    std::fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
    Ok(InitResult {
        path,
        created: true,
        content,
    })
}

fn build_agents_md(cwd: &Path) -> Result<String> {
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
        lines.push("- Identify the project type and entry points before making changes.".to_string());
    } else {
        lines.extend(facts);
    }
    lines.extend([
        String::new(),
        "## Build & test".to_string(),
    ]);
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
    lines.push("- Keep changes scoped to the requested task.".to_string());
    lines.push("- Prefer existing project patterns and commands over new tooling.".to_string());
    lines.push("- Run the relevant checks before handing work back.".to_string());
    lines.push(String::new());
    Ok(lines.join("\n"))
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
            lines.push(format!("- build: `{manager} run build`"));
        }
        if scripts.contains(&"test".to_string()) {
            lines.push(format!("- test: `{manager} test`"));
        }
        if scripts.contains(&"lint".to_string()) {
            lines.push(format!("- lint: `{manager} run lint`"));
        }
        if lines.len() == 1 {
            lines.push(format!("- test: inspect `package.json` scripts before choosing a `{manager}` command"));
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
    let Some(obj) = package_json(cwd)
        .and_then(|v| v.get("scripts").and_then(|s| s.as_object()).cloned())
    else {
        return Vec::new();
    };
    obj.keys().cloned().collect()
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
        assert_eq!(std::fs::read_to_string(result.path).unwrap(), result.content);
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
        assert!(result.content.contains("build: `pnpm run build`"));
        assert!(result.content.contains("test: `pnpm test`"));
        assert!(result.content.contains("lint: `pnpm run lint`"));
    }

    #[test]
    fn init_project_records_python_and_go_project_names() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("pyproject.toml"), "[project]\nname = 'worker'\n")
            .unwrap();
        std::fs::write(temp.path().join("go.mod"), "module example.com/service\n").unwrap();
        std::fs::write(temp.path().join("Dockerfile"), "FROM scratch\n").unwrap();

        let result = init_project(temp.path()).unwrap();

        assert!(result.content.contains("Python project: `worker`"));
        assert!(result.content.contains("Go module: `example.com/service`"));
        assert!(result.content.contains("Uses Docker: `Dockerfile`"));
    }
}
