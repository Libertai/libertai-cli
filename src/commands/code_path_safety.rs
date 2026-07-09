//! Sensitive-path deny list for mutating file tools.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use pi::tools::ToolEffects;

const SAFE_ROOT_ENV: &str = "LIBERTAI_WRITE_SAFE_ROOT";

pub struct PathSafetyTool {
    inner: Box<dyn Tool>,
    cwd: PathBuf,
    safe_root: Option<PathBuf>,
}

impl PathSafetyTool {
    pub fn new(inner: Box<dyn Tool>, cwd: PathBuf, safe_root: Option<PathBuf>) -> Self {
        Self {
            inner,
            cwd,
            safe_root,
        }
    }
}

#[async_trait]
impl Tool for PathSafetyTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn effects(&self) -> ToolEffects {
        self.inner.effects()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        if let Some(raw_path) = input.get("path").and_then(Value::as_str) {
            if let Err(reason) = check_write_path(raw_path, &self.cwd, self.safe_root.as_deref()) {
                return Ok(denied_output(&reason).into());
            }
        }
        self.inner.execute(tool_call_id, input, on_update).await
    }

    async fn resume(
        &self,
        tool_call_id: &str,
        request_id: &str,
        payload: Value,
    ) -> PiResult<ToolExecution> {
        self.inner.resume(tool_call_id, request_id, payload).await
    }
}

pub fn safe_root_from_env(cwd: &Path) -> Option<PathBuf> {
    std::env::var_os(SAFE_ROOT_ENV)
        .filter(|value| !value.is_empty())
        .map(|value| resolve_user_path(Path::new(&value), cwd))
}

pub fn is_path_mutation_tool(name: &str) -> bool {
    matches!(
        name,
        "write" | "edit" | "hashline_edit" | "notebook_edit" | "notebook_execute"
    )
}

fn check_write_path(raw_path: &str, cwd: &Path, safe_root: Option<&Path>) -> Result<(), String> {
    let path = resolve_user_path(Path::new(raw_path), cwd);
    let normalized = normalize_path(&path);

    if let Some(reason) = sensitive_path_reason(&normalized) {
        return Err(reason);
    }

    if let Some(root) = safe_root {
        let root = normalize_path(root);
        if !normalized.starts_with(&root) {
            return Err(format!(
                "write denied: `{}` is outside {SAFE_ROOT_ENV} `{}`",
                normalized.display(),
                root.display()
            ));
        }
    }

    Ok(())
}

fn sensitive_path_reason(path: &Path) -> Option<String> {
    let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    let lower_name = file_name.to_ascii_lowercase();
    let components = path_components_lower(path);

    if path == Path::new("/etc/passwd") || path == Path::new("/etc/shadow") {
        return Some(format!(
            "write denied: `{}` is a system account file",
            path.display()
        ));
    }

    if matches!(
        lower_name.as_str(),
        ".bashrc"
            | ".bash_profile"
            | ".bash_login"
            | ".profile"
            | ".zshrc"
            | ".zprofile"
            | ".zlogin"
            | ".netrc"
    ) {
        return Some(format!(
            "write denied: `{}` is a sensitive shell/authentication file",
            path.display()
        ));
    }

    if lower_name == ".env" || lower_name.starts_with(".env.") {
        return Some(format!(
            "write denied: `{}` may contain environment secrets",
            path.display()
        ));
    }

    if components.iter().any(|part| part == ".ssh")
        && (lower_name == "config"
            || lower_name == "authorized_keys"
            || lower_name == "known_hosts"
            || lower_name.starts_with("id_"))
    {
        return Some(format!(
            "write denied: `{}` is inside an SSH credential/config path",
            path.display()
        ));
    }

    if contains_subpath(&components, &[".aws"])
        || contains_subpath(&components, &[".config", "gcloud"])
        || contains_subpath(&components, &[".azure"])
    {
        return Some(format!(
            "write denied: `{}` is inside a cloud credential directory",
            path.display()
        ));
    }

    None
}

fn resolve_user_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn path_components_lower(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(|s| s.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

fn contains_subpath(parts: &[String], needle: &[&str]) -> bool {
    parts
        .windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(a, b)| a == b))
}

fn denied_output(reason: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(reason.to_string()))],
        details: Some(json!({ "guardrail": "path_safety" })),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        PathBuf::from("/workspace/project")
    }

    #[test]
    fn denies_env_files() {
        let err = check_write_path(".env.local", &cwd(), None).unwrap_err();
        assert!(err.contains("environment secrets"));
    }

    #[test]
    fn denies_ssh_keys() {
        let err = check_write_path("../user/.ssh/id_ed25519", &cwd(), None).unwrap_err();
        assert!(err.contains("SSH"));
    }

    #[test]
    fn denies_cloud_credentials() {
        let err = check_write_path(
            "/home/me/.config/gcloud/application_default_credentials.json",
            &cwd(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("cloud credential"));
    }

    #[test]
    fn safe_root_denies_outside_paths() {
        let err = check_write_path(
            "src/lib.rs",
            &cwd(),
            Some(Path::new("/workspace/project/docs")),
        )
        .unwrap_err();
        assert!(err.contains(SAFE_ROOT_ENV));
    }

    #[test]
    fn safe_root_allows_inside_paths() {
        check_write_path(
            "docs/notes.md",
            &cwd(),
            Some(Path::new("/workspace/project/docs")),
        )
        .unwrap();
    }

    #[test]
    fn ordinary_project_file_is_allowed() {
        check_write_path("src/main.rs", &cwd(), None).unwrap();
    }
}
