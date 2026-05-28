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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDocument {
    pub path: PathBuf,
    pub content: String,
    pub exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryClearResult {
    pub path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMemoryNote {
    pub kind: MemoryKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReference {
    pub line_number: usize,
    pub text: String,
    pub target: Option<String>,
    pub status: MemoryReferenceStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryReferenceStatus {
    Ok,
    Missing,
    External,
    Unparsed,
}

impl MemoryReferenceStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::External => "external",
            Self::Unparsed => "unparsed",
        }
    }
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
    append_memory_with_kind(cwd, MemoryKind::Project, text)
}

/// Parse a user-entered memory note and append it with the requested
/// category. Accepted forms:
///
/// - `user: prefers terse answers`
/// - `feedback: avoid noisy status updates`
/// - `project: run cargo check before commits`
/// - `reference: API docs live at ...`
/// - `--type user prefers terse answers`
pub fn append_memory_from_input(cwd: &Path, input: &str) -> Result<PathBuf> {
    let parsed = parse_memory_note(input);
    append_memory_with_kind(cwd, parsed.kind, &parsed.text)
}

pub fn append_memory_with_kind(cwd: &Path, kind: MemoryKind, text: &str) -> Result<PathBuf> {
    let path = memory_file_for(cwd)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let stamp = Local::now().format("%Y-%m-%d %H:%M");
    let line = format!("- {stamp} [{}] {}\n", kind.label(), text.trim());

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

pub fn parse_memory_note(input: &str) -> ParsedMemoryNote {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("--type ") {
        let mut parts = rest.trim_start().splitn(2, char::is_whitespace);
        if let Some(kind_raw) = parts.next() {
            if let Some(kind) = parse_memory_kind(kind_raw) {
                return ParsedMemoryNote {
                    kind,
                    text: parts.next().unwrap_or("").trim().to_string(),
                };
            }
        }
    }
    if let Some((prefix, rest)) = trimmed.split_once(':') {
        if let Some(kind) = parse_memory_kind(prefix.trim()) {
            return ParsedMemoryNote {
                kind,
                text: rest.trim().to_string(),
            };
        }
    }
    ParsedMemoryNote {
        kind: MemoryKind::Project,
        text: trimmed.to_string(),
    }
}

fn parse_memory_kind(raw: &str) -> Option<MemoryKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "user" => Some(MemoryKind::User),
        "feedback" => Some(MemoryKind::Feedback),
        "project" => Some(MemoryKind::Project),
        "reference" | "ref" => Some(MemoryKind::Reference),
        _ => None,
    }
}

/// Ensure the project's MEMORY.md exists and return its path.
pub fn ensure_memory_file(cwd: &Path) -> Result<PathBuf> {
    let path = memory_file_for(cwd)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
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

pub fn verify_memory_references(cwd: &Path) -> Result<Vec<MemoryReference>> {
    let doc = read_memory(cwd)?;
    Ok(verify_memory_references_in_content(cwd, &doc.content))
}

pub fn verify_memory_references_in_content(cwd: &Path, content: &str) -> Vec<MemoryReference> {
    content
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| memory_reference_from_line(cwd, idx + 1, line))
        .collect()
}

fn memory_reference_from_line(cwd: &Path, line_number: usize, line: &str) -> Option<MemoryReference> {
    let marker = "[reference]";
    let marker_idx = line.find(marker)?;
    let text = line[marker_idx + marker.len()..].trim().to_string();
    let Some(target) = extract_reference_target(cwd, &text) else {
        return Some(MemoryReference {
            line_number,
            text,
            target: None,
            status: MemoryReferenceStatus::Unparsed,
            detail: "no URL or local path target found".to_string(),
        });
    };
    if is_external_reference(&target) {
        return Some(MemoryReference {
            line_number,
            text,
            target: Some(target),
            status: MemoryReferenceStatus::External,
            detail: "external reference; not checked locally".to_string(),
        });
    }
    let path = resolve_reference_path(cwd, &target);
    if path.exists() {
        Some(MemoryReference {
            line_number,
            text,
            target: Some(target),
            status: MemoryReferenceStatus::Ok,
            detail: path.display().to_string(),
        })
    } else {
        Some(MemoryReference {
            line_number,
            text,
            target: Some(target),
            status: MemoryReferenceStatus::Missing,
            detail: path.display().to_string(),
        })
    }
}

fn extract_reference_target(cwd: &Path, text: &str) -> Option<String> {
    if let Some(start) = text.find("](") {
        let rest = &text[start + 2..];
        if let Some(end) = rest.find(')') {
            return clean_reference_token(&rest[..end]);
        }
    }
    for raw in text.split_whitespace() {
        if let Some(cleaned) = clean_reference_token(raw) {
            if is_external_reference(&cleaned)
                || cleaned.starts_with("file:")
                || cleaned.starts_with('/')
                || cleaned.starts_with("./")
                || cleaned.starts_with("../")
                || cleaned.contains('/')
                || Path::new(&cleaned).extension().is_some()
                || cwd.join(&cleaned).exists()
            {
                return Some(cleaned);
            }
        }
    }
    None
}

fn clean_reference_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('`').trim_matches('"').trim_matches('\'');
    let trimmed = trimmed.trim_end_matches(|c: char| matches!(c, ',' | '.' | ';' | ':'));
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn is_external_reference(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

fn resolve_reference_path(cwd: &Path, target: &str) -> PathBuf {
    let raw = target.strip_prefix("file://").or_else(|| target.strip_prefix("file:")).unwrap_or(target);
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Clear MEMORY.md, preserving existing content in a numbered backup.
pub fn clear_memory(cwd: &Path) -> Result<MemoryClearResult> {
    let path = memory_file_for(cwd)?;
    clear_memory_path(path)
}

fn clear_memory_path(path: PathBuf) -> Result<MemoryClearResult> {
    if !path.exists() {
        return Ok(MemoryClearResult {
            path,
            backup_path: None,
        });
    }
    let backup_path = next_backup_path(&path);
    std::fs::rename(&path, &backup_path)
        .with_context(|| format!("moving {} to {}", path.display(), backup_path.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, "").with_context(|| format!("clearing {}", path.display()))?;
    Ok(MemoryClearResult {
        path,
        backup_path: Some(backup_path),
    })
}

fn next_backup_path(path: &Path) -> PathBuf {
    let first = path.with_extension("md.bak");
    if !first.exists() {
        return first;
    }
    for i in 2.. {
        let candidate = path.with_extension(format!("md.bak.{i}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded backup suffix search should always return");
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

    #[test]
    fn clear_memory_path_moves_existing_content_to_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("MEMORY.md");
        std::fs::write(&path, "- keep this\n").unwrap();
        let backup = next_backup_path(&path);

        let result = clear_memory_path(path.clone()).unwrap();

        assert_eq!(result.backup_path.as_ref(), Some(&backup));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "- keep this\n");
    }

    #[test]
    fn next_backup_path_skips_existing_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("MEMORY.md");
        std::fs::write(path.with_extension("md.bak"), "old").unwrap();
        assert_eq!(next_backup_path(&path), path.with_extension("md.bak.2"));
    }

    #[test]
    fn parse_memory_note_defaults_to_project() {
        assert_eq!(
            parse_memory_note("run cargo check"),
            ParsedMemoryNote {
                kind: MemoryKind::Project,
                text: "run cargo check".into(),
            }
        );
    }

    #[test]
    fn parse_memory_note_accepts_colon_kind() {
        assert_eq!(
            parse_memory_note("feedback: avoid noisy status"),
            ParsedMemoryNote {
                kind: MemoryKind::Feedback,
                text: "avoid noisy status".into(),
            }
        );
        assert_eq!(
            parse_memory_note("ref: https://example.test"),
            ParsedMemoryNote {
                kind: MemoryKind::Reference,
                text: "https://example.test".into(),
            }
        );
    }

    #[test]
    fn parse_memory_note_accepts_type_flag() {
        assert_eq!(
            parse_memory_note("--type user prefers terse answers"),
            ParsedMemoryNote {
                kind: MemoryKind::User,
                text: "prefers terse answers".into(),
            }
        );
    }

    #[test]
    fn append_memory_with_kind_writes_category_label() {
        let temp = tempfile::tempdir().unwrap();
        let path = append_memory_with_kind(temp.path(), MemoryKind::Reference, "docs url").unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("[reference] docs url"));
    }

    #[test]
    fn verify_memory_references_checks_local_paths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("docs.md"), "hi").unwrap();
        let content = "- 2026-01-01 [reference] `docs.md`\n- 2026-01-01 [reference] missing.md\n";
        let refs = verify_memory_references_in_content(temp.path(), content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].status, MemoryReferenceStatus::Ok);
        assert_eq!(refs[0].target.as_deref(), Some("docs.md"));
        assert_eq!(refs[1].status, MemoryReferenceStatus::Missing);
    }

    #[test]
    fn verify_memory_references_marks_external_and_unparsed() {
        let temp = tempfile::tempdir().unwrap();
        let content = "- [reference] [api](https://example.test/docs)\n- [reference] ask Sam about this\n";
        let refs = verify_memory_references_in_content(temp.path(), content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].status, MemoryReferenceStatus::External);
        assert_eq!(refs[0].target.as_deref(), Some("https://example.test/docs"));
        assert_eq!(refs[1].status, MemoryReferenceStatus::Unparsed);
    }
}
