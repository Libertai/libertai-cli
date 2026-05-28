//! Native `/init` support for `libertai code`.
//!
//! Creates a small `AGENTS.md` bootstrap file from facts visible in the
//! repository without spending a model turn. Existing files are left
//! untouched; users can edit them directly once created.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
        "## Build & test".to_string(),
    ];
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
        return vec![
            "- build/check: `cargo check --locked`".to_string(),
            "- test: `cargo test --locked`".to_string(),
        ];
    }
    if cwd.join("package.json").exists() {
        let manager = js_package_manager(cwd);
        return vec![
            format!("- install: `{manager} install`"),
            format!("- build: `{manager} run build`"),
            format!("- test: `{manager} test`"),
        ];
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
}
