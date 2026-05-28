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
    pub backup_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryImportResult {
    pub path: PathBuf,
    pub source_path: PathBuf,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMemoryNote {
    pub kind: MemoryKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFileEntry {
    pub kind: MemoryKind,
    pub path: PathBuf,
    pub title: String,
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

pub fn memory_entries_dir_for(cwd: &Path) -> Result<PathBuf> {
    let index = memory_file_for(cwd)?;
    let parent = index
        .parent()
        .ok_or_else(|| anyhow::anyhow!("memory path has no parent: {}", index.display()))?;
    Ok(parent.join("memory"))
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
    let sidecar = write_memory_entry_file(cwd, kind, text.trim(), &stamp.to_string())?;
    let relative = sidecar_path_for_index(&path, &sidecar);
    let line = format!(
        "- {stamp} ([entry]({})) [{}] {}\n",
        relative.display(),
        kind.label(),
        text.trim()
    );

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

pub fn import_memory_file(cwd: &Path, source: &Path) -> Result<MemoryImportResult> {
    const MAX_IMPORT_BYTES: usize = 256 * 1024;
    let source_path = if source.is_absolute() {
        source.to_path_buf()
    } else {
        cwd.join(source)
    };
    let meta = std::fs::metadata(&source_path)
        .with_context(|| format!("reading metadata for {}", source_path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("memory import source is not a file: {}", source_path.display());
    }
    if meta.len() > MAX_IMPORT_BYTES as u64 {
        anyhow::bail!("memory import source is too large; keep imports under 256 KiB");
    }
    let content = std::fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        anyhow::bail!("memory import source is empty: {}", source_path.display());
    }
    let note = format!("Imported from `{}`.\n\n{}", source_path.display(), trimmed);
    let path = append_memory_with_kind(cwd, MemoryKind::Project, &note)?;
    Ok(MemoryImportResult {
        path,
        source_path,
        bytes: content.len(),
    })
}

fn write_memory_entry_file(
    cwd: &Path,
    kind: MemoryKind,
    text: &str,
    stamp: &str,
) -> Result<PathBuf> {
    let entries = memory_entries_dir_for(cwd)?;
    let dir = entries.join(kind.label());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = unique_memory_entry_path(&dir, stamp, text);
    let content = format!(
        "# {} memory\n\n- created: {stamp}\n- kind: {}\n- project: {}\n\n{}\n",
        kind.label(),
        kind.label(),
        cwd.display(),
        text
    );
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn unique_memory_entry_path(dir: &Path, stamp: &str, text: &str) -> PathBuf {
    let base = format!(
        "{}-{}",
        stamp.replace(['-', ':', ' '], ""),
        slugify_memory_text(text)
    );
    let mut path = dir.join(format!("{base}.md"));
    for suffix in 2.. {
        if !path.exists() {
            return path;
        }
        path = dir.join(format!("{base}-{suffix}.md"));
    }
    unreachable!("unbounded suffix search should always return");
}

fn slugify_memory_text(text: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "note".to_string()
    } else {
        slug
    }
}

fn sidecar_path_for_index(index_path: &Path, sidecar_path: &Path) -> PathBuf {
    index_path
        .parent()
        .and_then(|parent| sidecar_path.strip_prefix(parent).ok())
        .unwrap_or(sidecar_path)
        .to_path_buf()
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
    let entries_dir = memory_entries_dir_for(cwd)?;
    clear_memory_path(path, entries_dir)
}

fn clear_memory_path(path: PathBuf, entries_dir: PathBuf) -> Result<MemoryClearResult> {
    let backup_dir = if entries_dir.exists() {
        let backup = next_backup_dir(&entries_dir);
        std::fs::rename(&entries_dir, &backup).with_context(|| {
            format!("moving {} to {}", entries_dir.display(), backup.display())
        })?;
        Some(backup)
    } else {
        None
    };
    if !path.exists() {
        return Ok(MemoryClearResult {
            path,
            backup_path: None,
            backup_dir,
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
        backup_dir,
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

fn next_backup_dir(path: &Path) -> PathBuf {
    let first = path.with_file_name("memory.bak");
    if !first.exists() {
        return first;
    }
    for i in 2.. {
        let candidate = path.with_file_name(format!("memory.bak.{i}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded backup suffix search should always return");
}

pub fn list_memory_files(cwd: &Path) -> Result<Vec<MemoryFileEntry>> {
    let root = memory_entries_dir_for(cwd)?;
    let mut out = Vec::new();
    for kind in [
        MemoryKind::User,
        MemoryKind::Feedback,
        MemoryKind::Project,
        MemoryKind::Reference,
    ] {
        let dir = root.join(kind.label());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            out.push(MemoryFileEntry {
                kind,
                title: memory_file_title(&path),
                path,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn memory_file_title(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find_map(|line| line.strip_prefix("# ").map(str::trim))
                .map(str::to_string)
        })
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("memory")
                .to_string()
        })
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

        let entries = temp.path().join("memory");
        let result = clear_memory_path(path.clone(), entries).unwrap();

        assert_eq!(result.backup_path.as_ref(), Some(&backup));
        assert!(result.backup_dir.is_none());
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
        assert!(content.contains("([entry](memory/reference/"));
    }

    #[test]
    fn append_memory_with_kind_writes_sidecar_file() {
        let temp = tempfile::tempdir().unwrap();
        append_memory_with_kind(temp.path(), MemoryKind::User, "prefers terse answers").unwrap();

        let files = list_memory_files(temp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, MemoryKind::User);
        assert!(files[0].path.parent().is_some_and(|path| path.ends_with("user")));
        let content = std::fs::read_to_string(&files[0].path).unwrap();
        assert!(content.contains("- kind: user"));
        assert!(content.contains("prefers terse answers"));
    }

    #[test]
    fn import_memory_file_appends_source_content() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("CLAUDE.md");
        std::fs::write(&source, "# Notes\n\n- run cargo test\n").unwrap();

        let result = import_memory_file(temp.path(), Path::new("CLAUDE.md")).unwrap();
        assert_eq!(result.source_path, source);
        assert!(result.bytes > 0);

        let content = std::fs::read_to_string(result.path).unwrap();
        assert!(content.contains("[project] Imported from"));
        assert!(content.contains("CLAUDE.md"));
        assert!(content.contains("run cargo test"));
        assert_eq!(list_memory_files(temp.path()).unwrap().len(), 1);
    }

    #[test]
    fn clear_memory_path_moves_sidecar_dir_to_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("MEMORY.md");
        let entries = temp.path().join("memory");
        std::fs::create_dir_all(entries.join("project")).unwrap();
        std::fs::write(entries.join("project").join("note.md"), "note").unwrap();

        let result = clear_memory_path(path, entries.clone()).unwrap();

        let backup = result.backup_dir.unwrap();
        assert!(!entries.exists());
        assert_eq!(std::fs::read_to_string(backup.join("project").join("note.md")).unwrap(), "note");
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
