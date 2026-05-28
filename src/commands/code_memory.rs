//! Per-project memory (`/remember <text>` command).
//!
//! Stores small dated notes the user wants kept across sessions for the
//! current working directory. The memory file is loaded into the system
//! prompt by `pi::app::load_project_memory`, which we configure via the
//! `PI_PROJECT_MEMORY_DIR` env var (set in [`ensure_memory_env`]).
//!
//! Path layout:
//!
//! ```text
//!   ${memory_root}/${encoded-cwd}/MEMORY.md
//! ```
//!
//! Where `memory_root` is `LIBERTAI_HOME/projects` if `LIBERTAI_HOME`
//! is set (test/dev override), otherwise
//! `${dirs::config_dir}/libertai/projects` — e.g.
//! `~/.config/libertai/projects/` on Linux.
//!
//! `encoded-cwd` matches `pi::app::encode_project_cwd`: canonical cwd
//! with `/` → `-` and any leading `-` stripped. Same encoding as
//! Claude Code's `~/.claude/projects/<encoded>/` so users coming from
//! there get continuity.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDocument {
    pub path: PathBuf,
    pub content: String,
    pub exists: bool,
}

/// Resolve the directory under which all per-project memory dirs live.
/// `LIBERTAI_HOME` takes priority for tests; otherwise the XDG config
/// dir. Always returns a path even if the dir doesn't exist yet —
/// [`append_memory`] creates it on first write.
pub fn memory_root() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("LIBERTAI_HOME") {
        return Ok(Path::new(&home).join("projects"));
    }
    let base = dirs::config_dir().context("resolving user config dir")?;
    Ok(base.join("libertai").join("projects"))
}

/// Resolve the MEMORY.md path for a given cwd.
pub fn memory_file_for(cwd: &Path) -> Result<PathBuf> {
    let root = memory_root()?;
    let encoded = pi::app::encode_project_cwd(cwd);
    Ok(root.join(encoded).join("MEMORY.md"))
}

/// Make sure `PI_PROJECT_MEMORY_DIR` is set so pi's loader picks up
/// our memory files. Call once at session startup. Honors any value
/// the user has already set in the environment (e.g. probes).
pub fn ensure_memory_env() -> Result<()> {
    if std::env::var_os("PI_PROJECT_MEMORY_DIR").is_some() {
        return Ok(());
    }
    let root = memory_root()?;
    // SAFETY: we set this once at process start before any worker
    // threads spawn, so a single-threaded write is sound.
    unsafe { std::env::set_var("PI_PROJECT_MEMORY_DIR", &root) };
    Ok(())
}

/// Append `text` as a dated bullet to the project's MEMORY.md.
/// Creates parent directories and the file if needed.
pub fn append_memory(cwd: &Path, text: &str) -> Result<PathBuf> {
    let path = memory_file_for(cwd)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let stamp = Local::now().format("%Y-%m-%d %H:%M");
    let line = format!("- {stamp} {}\n", text.trim());

    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;
    Ok(path)
}

/// Read the project's MEMORY.md without creating it.
pub fn read_memory(cwd: &Path) -> Result<MemoryDocument> {
    let path = memory_file_for(cwd)?;
    read_memory_path(path)
}

fn read_memory_path(path: PathBuf) -> Result<MemoryDocument> {
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(MemoryDocument {
            path,
            content,
            exists: true,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemoryDocument {
            path,
            content: String::new(),
            exists: false,
        }),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_memory_file_reports_existing_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("MEMORY.md");
        std::fs::write(&path, "- keep this\n").unwrap();

        let doc = read_memory_path(path.clone()).unwrap();
        assert!(doc.exists);
        assert_eq!(doc.path, path);
        assert_eq!(doc.content, "- keep this\n");
    }

    #[test]
    fn read_memory_file_reports_missing_without_creating() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing").join("MEMORY.md");

        let doc = read_memory_path(path.clone()).unwrap();
        assert!(!doc.exists);
        assert_eq!(doc.path, path);
        assert!(doc.content.is_empty());
    }
}
