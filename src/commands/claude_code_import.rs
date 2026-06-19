//! Discover Claude Code session JSONL files on disk.
//!
//! Claude Code persists per-project transcripts as JSONL under
//! `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`, where
//! `<encoded-cwd>` is the absolute project path with every `/`
//! replaced by `-`. This module surfaces those files so a user can
//! pick one to seed a new pi session via the "Import from Claude
//! Code" flow (spec: docs/claude-code-import.md).
//!
//! Discovery only — no LLM calls and no summarisation. The actual
//! import path (parse linearised history → ask the model for a
//! compaction summary → emit a `SessionEntry::Compaction`) lands in
//! a follow-up; keeping this module read-only lets the desktop reuse
//! the same primitives for its picker UI.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Metadata for one discovered Claude Code session.
///
/// `jsonl_path` is the absolute on-disk file; `recorded_cwd` is the
/// project path Claude Code itself wrote into the records (preferred
/// over reverse-decoding the dir name, which is ambiguous if the real
/// path contains `-`).
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredSession {
    pub session_uuid: String,
    pub project_dir: PathBuf,
    pub recorded_cwd: Option<PathBuf>,
    pub jsonl_path: PathBuf,
    #[serde(with = "systime_serde")]
    pub mtime: SystemTime,
    pub size_bytes: u64,
    pub record_count: usize,
    pub first_user_message: Option<String>,
    pub claude_code_summary: Option<String>,
    pub git_branch: Option<String>,
}

/// Encode an absolute path the way Claude Code does for its
/// `~/.claude/projects/<encoded>` directory names: every `/` → `-`.
/// Lossy if the source path itself contains `-`, but that's Claude
/// Code's own convention and we mirror it bit-for-bit.
pub fn encode_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

/// Where Claude Code keeps its per-project session JSONLs.
/// Honours `CLAUDE_CONFIG_DIR` (Claude Code's own override) so users
/// with a non-default install path get discovery for free; falls back
/// to `~/.claude/` otherwise. The `/projects` suffix is Claude Code's
/// own convention and stays the same in both cases.
pub fn claude_code_projects_root() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !custom.is_empty() {
            return Some(PathBuf::from(custom).join("projects"));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// List Claude Code sessions, newest first.
///
/// When `all_projects` is false (default), restrict to the encoded
/// dir for `cwd`. When true, walk every project directory under
/// `~/.claude/projects`.
pub fn discover(cwd: &Path, all_projects: bool) -> Result<Vec<DiscoveredSession>> {
    let root = match claude_code_projects_root() {
        Some(r) => r,
        None => return Ok(Vec::new()),
    };
    if !root.exists() {
        return Ok(Vec::new());
    }

    let project_dirs = if all_projects {
        list_project_dirs(&root)?
    } else {
        let encoded = encode_project_dir(cwd);
        let p = root.join(&encoded);
        if p.is_dir() {
            vec![p]
        } else {
            Vec::new()
        }
    };

    let mut sessions = Vec::new();
    for proj in &project_dirs {
        let entries =
            std::fs::read_dir(proj).with_context(|| format!("read_dir({})", proj.display()))?;
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(s) = describe(&path, proj) {
                sessions.push(s);
            }
        }
    }

    sessions.sort_by_key(|session| std::cmp::Reverse(session.mtime));
    Ok(sessions)
}

fn list_project_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push(entry.path());
        }
    }
    Ok(out)
}

fn describe(path: &Path, project_dir: &Path) -> Option<DiscoveredSession> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let session_uuid = path.file_stem()?.to_str()?.to_string();

    let peek = peek_jsonl(path).unwrap_or_default();
    Some(DiscoveredSession {
        session_uuid,
        project_dir: project_dir.to_path_buf(),
        recorded_cwd: peek.recorded_cwd,
        jsonl_path: path.to_path_buf(),
        mtime,
        size_bytes: meta.len(),
        record_count: peek.record_count,
        first_user_message: peek.first_user_message,
        claude_code_summary: peek.claude_code_summary,
        git_branch: peek.git_branch,
    })
}

#[derive(Default)]
struct PeekInfo {
    record_count: usize,
    first_user_message: Option<String>,
    claude_code_summary: Option<String>,
    recorded_cwd: Option<PathBuf>,
    git_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RecordHead {
    #[serde(rename = "type")]
    typ: Option<String>,
    summary: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    message: Option<RecordMessage>,
}

#[derive(Debug, Deserialize)]
struct RecordMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

fn peek_jsonl(path: &Path) -> Result<PeekInfo> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut info = PeekInfo::default();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        info.record_count += 1;
        let rec: RecordHead = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if info.claude_code_summary.is_none() && rec.typ.as_deref() == Some("summary") {
            info.claude_code_summary = rec.summary;
        }
        if info.recorded_cwd.is_none() {
            if let Some(c) = rec.cwd {
                info.recorded_cwd = Some(PathBuf::from(c));
            }
        }
        if info.git_branch.is_none() && rec.git_branch.is_some() {
            info.git_branch = rec.git_branch;
        }
        if info.first_user_message.is_none() && rec.typ.as_deref() == Some("user") {
            if let Some(msg) = rec.message {
                if msg.role.as_deref() == Some("user") {
                    info.first_user_message = msg
                        .content
                        .and_then(extract_text)
                        .map(|s| truncate(&s, 120));
                }
            }
        }
    }
    Ok(info)
}

/// Pull the first plain-text snippet from a `message.content` value.
/// Claude Code writes this as either a bare string or a content-block
/// array (`[{type: "text", text: ...}, {type: "tool_use", ...}]`).
fn extract_text(value: serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = value.as_array() {
        for item in arr {
            if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ──────────────────────────────────────────────────────────────────────
// Linearization
// ──────────────────────────────────────────────────────────────────────
//
// Claude Code stores branches when the user re-runs a turn: each
// record carries a `parentUuid`, and a re-prompt forks a new chain
// from the parent that's NOT the previous leaf. The "live" thread is
// the chain ending at the most recent leaf.
//
// `linearize` picks that chain and yields a flat `Vec<LinearMessage>`
// the summariser can consume. Read/Edit/Write tool-use blocks also
// feed `read_files` / `modified_files` so the pi `Compaction` entry
// we emit later can populate `details.readFiles` /
// `details.modifiedFiles` the same way `/compact` does.

/// One step in the linearised transcript. `Tool` and `ToolResult` are
/// kept as one-liners with their payloads truncated — the importer
/// uses these to build the summarisation prompt, not to replay the
/// raw tool calls (their IDs and pi's tool-call ID space don't agree).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinearMessage {
    User {
        text: String,
        timestamp: Option<String>,
    },
    Assistant {
        text: String,
        timestamp: Option<String>,
    },
    ToolUse {
        name: String,
        args_preview: String,
        timestamp: Option<String>,
    },
    ToolResult {
        preview: String,
        is_error: bool,
        timestamp: Option<String>,
    },
}

/// Output of [`linearize`]: the picked branch plus stats the picker UI
/// can show ("3 branches discarded", "8 sidechain records skipped").
#[derive(Debug, Clone, Serialize)]
pub struct LinearizedSession {
    pub session_uuid: String,
    pub recorded_cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub messages: Vec<LinearMessage>,
    pub read_files: BTreeSet<PathBuf>,
    pub modified_files: BTreeSet<PathBuf>,
    pub dropped_sidechain: usize,
    pub dropped_branches: usize,
    pub total_records: usize,
}

#[derive(Debug, Deserialize)]
struct RawRecord {
    uuid: Option<String>,
    #[serde(rename = "parentUuid")]
    parent_uuid: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(rename = "type")]
    typ: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    message: Option<serde_json::Value>,
    /// Claude Code flags harness-injected records (local-command
    /// caveats etc.) with `isMeta: true`. They're user-role in API
    /// terms but were never typed by the human.
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    /// Provenance of synthetic user records, e.g.
    /// `{"kind": "task-notification"}` for background-task completion
    /// notices. Human-typed messages carry no `origin` field.
    origin: Option<serde_json::Value>,
}

impl RawRecord {
    /// True for user-role records the Claude Code harness injected
    /// rather than the human typing them. These pollute a linearised
    /// transcript ("you: Background command … completed") so both the
    /// import summary and the desktop's resume hydration skip them.
    fn is_harness_injected(&self) -> bool {
        if self.is_meta {
            return true;
        }
        matches!(
            self.origin
                .as_ref()
                .and_then(|o| o.get("kind"))
                .and_then(|k| k.as_str()),
            Some("task-notification")
        )
    }
}

/// Read a Claude Code session JSONL, pick the live branch, and emit a
/// linear transcript.
pub fn linearize(jsonl_path: &Path) -> Result<LinearizedSession> {
    use std::io::{BufRead, BufReader};

    let session_uuid = jsonl_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let file = std::fs::File::open(jsonl_path)
        .with_context(|| format!("open {}", jsonl_path.display()))?;
    let reader = BufReader::new(file);

    let mut records: Vec<RawRecord> = Vec::new();
    let mut dropped_sidechain = 0usize;
    let mut recorded_cwd: Option<PathBuf> = None;
    let mut git_branch: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let rec: RawRecord = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rec.is_sidechain {
            dropped_sidechain += 1;
            continue;
        }
        if recorded_cwd.is_none() {
            if let Some(c) = &rec.cwd {
                recorded_cwd = Some(PathBuf::from(c));
            }
        }
        if git_branch.is_none() {
            if let Some(b) = &rec.git_branch {
                git_branch = Some(b.clone());
            }
        }
        records.push(rec);
    }

    let total_records = records.len();
    let chain = pick_live_branch(&records);
    let dropped_branches = total_records.saturating_sub(chain.len()).saturating_sub(
        // `summary` index records aren't part of any user-visible branch;
        // don't count them against the "branches dropped" stat.
        records
            .iter()
            .filter(|r| r.typ.as_deref() == Some("summary"))
            .count(),
    );

    let mut messages = Vec::new();
    let mut read_files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut modified_files: BTreeSet<PathBuf> = BTreeSet::new();

    for idx in chain {
        let rec = &records[idx];
        let typ = rec.typ.as_deref().unwrap_or("");
        if typ == "summary" || typ == "system" {
            continue;
        }
        if rec.is_harness_injected() {
            continue;
        }
        let timestamp = rec.timestamp.clone();
        let role = role_of(rec);
        let content = match &rec.message {
            Some(m) => m.get("content").cloned(),
            None => None,
        };
        let Some(content) = content else { continue };
        for block in iter_content_blocks(&content) {
            match classify_block(&block) {
                Block::Text(text) => match role {
                    Role::User => messages.push(LinearMessage::User {
                        text,
                        timestamp: timestamp.clone(),
                    }),
                    Role::Assistant => messages.push(LinearMessage::Assistant {
                        text,
                        timestamp: timestamp.clone(),
                    }),
                    Role::Unknown => {}
                },
                Block::ToolUse { name, input } => {
                    record_file_ops(&name, &input, &mut read_files, &mut modified_files);
                    messages.push(LinearMessage::ToolUse {
                        args_preview: tool_args_preview(&input),
                        name,
                        timestamp: timestamp.clone(),
                    });
                }
                Block::ToolResult { content, is_error } => {
                    messages.push(LinearMessage::ToolResult {
                        preview: tool_result_preview(&content),
                        is_error,
                        timestamp: timestamp.clone(),
                    });
                }
                Block::Other => {}
            }
        }
    }

    Ok(LinearizedSession {
        session_uuid,
        recorded_cwd,
        git_branch,
        messages,
        read_files,
        modified_files,
        dropped_sidechain,
        dropped_branches,
        total_records,
    })
}

#[derive(Copy, Clone)]
enum Role {
    User,
    Assistant,
    Unknown,
}

fn role_of(rec: &RawRecord) -> Role {
    match rec.typ.as_deref() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => Role::Unknown,
    }
}

fn iter_content_blocks(value: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(s) = value.as_str() {
        return vec![serde_json::json!({ "type": "text", "text": s })];
    }
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    Vec::new()
}

enum Block {
    Text(String),
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        content: serde_json::Value,
        is_error: bool,
    },
    Other,
}

fn classify_block(block: &serde_json::Value) -> Block {
    let typ = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match typ {
        "text" => Block::Text(
            block
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        "tool_use" => Block::ToolUse {
            name: block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string(),
            input: block
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        "tool_result" => Block::ToolResult {
            content: block
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            is_error: block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
        _ => Block::Other,
    }
}

/// File-touching Claude Code tools whose `input` carries a path we
/// want to surface. Edit/Write/MultiEdit all *mutate*; Read just reads.
/// NotebookEdit uses `notebook_path` instead of `file_path`. Bash and
/// Grep are intentionally absent — they can touch files too but the
/// argument shape is freeform, and trying to parse `rm -rf` out of a
/// bash command is more trouble than it's worth.
fn record_file_ops(
    tool: &str,
    input: &serde_json::Value,
    read_files: &mut BTreeSet<PathBuf>,
    modified_files: &mut BTreeSet<PathBuf>,
) {
    let path_str = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(|v| v.as_str());
    let Some(p) = path_str else { return };
    let path = PathBuf::from(p);
    match tool {
        "Read" => {
            read_files.insert(path);
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            modified_files.insert(path);
        }
        _ => {}
    }
}

const TOOL_ARGS_PREVIEW_MAX: usize = 160;
const TOOL_RESULT_PREVIEW_MAX: usize = 400;

fn tool_args_preview(input: &serde_json::Value) -> String {
    let compact = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
    truncate(&compact, TOOL_ARGS_PREVIEW_MAX)
}

fn tool_result_preview(content: &serde_json::Value) -> String {
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        out
    } else {
        serde_json::to_string(content).unwrap_or_default()
    };
    truncate(&text, TOOL_RESULT_PREVIEW_MAX)
}

/// Choose the "live" branch: pick the leaf with the latest timestamp
/// (ties broken by file order), then walk `parentUuid` back to root.
fn pick_live_branch(records: &[RawRecord]) -> Vec<usize> {
    let mut by_uuid: HashMap<&str, usize> = HashMap::new();
    let mut children: HashSet<&str> = HashSet::new();
    for (i, r) in records.iter().enumerate() {
        if let Some(u) = r.uuid.as_deref() {
            by_uuid.insert(u, i);
        }
        if let Some(p) = r.parent_uuid.as_deref() {
            children.insert(p);
        }
    }
    let mut leaves: Vec<usize> = Vec::new();
    for (i, r) in records.iter().enumerate() {
        let uuid = match r.uuid.as_deref() {
            Some(u) => u,
            None => continue,
        };
        if r.typ.as_deref() == Some("summary") {
            continue;
        }
        if !children.contains(uuid) {
            leaves.push(i);
        }
    }
    if leaves.is_empty() {
        return (0..records.len())
            .filter(|&i| records[i].typ.as_deref() != Some("summary"))
            .collect();
    }
    leaves.sort_by(|&a, &b| {
        records[b]
            .timestamp
            .cmp(&records[a].timestamp)
            .then_with(|| b.cmp(&a))
    });
    let leaf = leaves[0];
    let mut chain: Vec<usize> = Vec::new();
    let mut cursor = Some(leaf);
    while let Some(idx) = cursor {
        chain.push(idx);
        cursor = records[idx]
            .parent_uuid
            .as_deref()
            .and_then(|p| by_uuid.get(p).copied());
    }
    chain.reverse();
    chain
}

/// Build the (system, user) prompt pair the LibertAI backend gets
/// when summarising an imported Claude Code transcript.
///
/// Mirrors pi's `SUMMARIZATION_PROMPT` template exactly so the
/// resulting checkpoint reads the same as one produced by `/compact`.
/// A short preamble clarifies that the transcript is from another
/// agent — without it, models tend to editorialise ("I don't recall
/// this conversation...") instead of summarising what they're shown.
pub fn build_summary_prompt(session: &LinearizedSession) -> (String, String) {
    let preamble = match (&session.recorded_cwd, &session.git_branch) {
        (Some(cwd), Some(branch)) => format!(
            "The transcript below is an external Claude Code session from {} (branch `{}`). \
             You did not produce it. Treat it as conversation history and summarise it for the \
             agent that will continue this work.\n\n",
            cwd.display(),
            branch,
        ),
        (Some(cwd), None) => format!(
            "The transcript below is an external Claude Code session from {}. \
             You did not produce it. Treat it as conversation history and summarise it for the \
             agent that will continue this work.\n\n",
            cwd.display(),
        ),
        _ => "The transcript below is an external Claude Code session. You did not produce \
              it. Treat it as conversation history and summarise it for the agent that will \
              continue this work.\n\n"
            .to_string(),
    };

    let system = format!("{preamble}{COMPACTION_SUMMARIZATION_PROMPT}");
    let user = render_transcript(session);
    (system, user)
}

/// Lifted from pi (`compaction.rs::SUMMARIZATION_PROMPT`) so the
/// imported checkpoint matches the structure pi produces for
/// `/compact`. Keep these in sync; if pi's template changes, mirror
/// it here.
const COMPACTION_SUMMARIZATION_PROMPT: &str =
    "The messages above are a conversation to summarize. Create a structured context \
     checkpoint summary that another LLM will use to continue the work.\n\n\
     Use this EXACT format:\n\n\
     ## Goal\n\
     [What is the user trying to accomplish? Can be multiple items if the session covers \
     different tasks.]\n\n\
     ## Constraints & Preferences\n\
     - [Any constraints, preferences, or requirements mentioned by user]\n\
     - [Or \"(none)\" if none were mentioned]\n\n\
     ## Progress\n\
     ### Done\n\
     - [x] [Completed tasks/changes]\n\n\
     ### In Progress\n\
     - [ ] [Current work]\n\n\
     ### Blocked\n\
     - [Issues preventing progress, if any]\n\n\
     ## Key Decisions\n\
     - **[Decision]**: [Brief rationale]\n\n\
     ## Next Steps\n\
     1. [Ordered list of what should happen next]\n\n\
     ## Critical Context\n\
     - [Any data, examples, or references needed to continue]\n\
     - [Or \"(none)\" if not applicable]\n\n\
     Keep each section concise. Preserve exact file paths, function names, and error \
     messages.";

// ──────────────────────────────────────────────────────────────────────
// Pi session emission
// ──────────────────────────────────────────────────────────────────────
//
// Write a new pi session file consisting of:
//   1. A `SessionHeader` (type=session, version, id, timestamp, cwd, model).
//   2. A single `SessionEntry::Compaction` carrying the model-generated
//      summary, `tokens_before` (estimated from raw transcript length),
//      and `details.{readFiles, modifiedFiles, source, sourcePath,
//      sourceSessionUuid}`.
//
// Pi treats the compaction as "everything prior is summarised", so the
// session opens with the checkpoint already in context — same UX as
// resuming a session right after `/compact`.
//
// File layout mirrors pi's:
//   `$PI_HOME/agent/sessions/<encoded-cwd>/<iso-fsafe>_<8-hex>.jsonl`
// where `<iso-fsafe>` has `:` replaced by `-` for cross-FS safety and
// `<encoded-cwd>` is `pi::session::encode_cwd(cwd)`.

/// Inputs the emitter needs to construct the pi session. Provider /
/// model default to "libertai" / "default" if unspecified — pi rewrites
/// them on first prompt, but a populated value reads better in the
/// session picker before the user prompts.
#[derive(Debug, Clone)]
pub struct PiSessionInputs<'a> {
    pub cwd: &'a Path,
    pub source_session_uuid: &'a str,
    pub source_path: &'a Path,
    pub summary: &'a str,
    pub tokens_before: u64,
    pub provider: Option<String>,
    pub model_id: Option<String>,
    pub read_files: &'a std::collections::BTreeSet<PathBuf>,
    pub modified_files: &'a std::collections::BTreeSet<PathBuf>,
}

/// Result of [`write_pi_session_file`]: the path of the new JSONL plus
/// the session id pi will use to identify it.
#[derive(Debug, Clone, Serialize)]
pub struct WrittenPiSession {
    pub session_id: String,
    pub jsonl_path: PathBuf,
}

/// Write the pi session file to disk and return the path + id.
///
/// Uses pi's own `SessionHeader` / `SessionEntry::Compaction` types so
/// the JSON shape can't drift from what pi loads; if pi changes its
/// schema, this fails to compile rather than silently producing
/// broken sessions.
pub fn write_pi_session_file(inputs: PiSessionInputs<'_>) -> Result<WrittenPiSession> {
    use pi::session::{encode_cwd, CompactionEntry, EntryBase, SessionEntry, SessionHeader};
    use pi::session_index::SessionIndex;

    let sessions_root = pi_sessions_root()?;
    let dir = sessions_root.join(encode_cwd(inputs.cwd));
    std::fs::create_dir_all(&dir).with_context(|| format!("create_dir_all({})", dir.display()))?;

    let mut header = SessionHeader::new();
    header.cwd = inputs.cwd.display().to_string();
    header.provider = inputs.provider.clone();
    header.model_id = inputs.model_id.clone();
    let session_id = header.id.clone();

    let header_entry_id = short_id();
    let compaction_id = short_id();
    let kept_id_placeholder = short_id();
    let details = serde_json::json!({
        "readFiles": inputs.read_files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "modifiedFiles": inputs.modified_files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "source": "claude-code",
        "sourcePath": inputs.source_path.display().to_string(),
        "sourceSessionUuid": inputs.source_session_uuid,
    });

    let compaction = SessionEntry::Compaction(CompactionEntry {
        base: EntryBase::new(Some(header_entry_id.clone()), compaction_id.clone()),
        summary: inputs.summary.to_string(),
        first_kept_entry_id: kept_id_placeholder,
        tokens_before: inputs.tokens_before,
        details: Some(details),
        from_hook: Some(false),
    });

    let file_name = format!(
        "{}_{}.jsonl",
        fs_safe_timestamp(&header.timestamp),
        &session_id[..8]
    );
    let jsonl_path = dir.join(file_name);

    let mut buf = serde_json::to_string(&header).context("serialize SessionHeader")?;
    buf.push('\n');
    buf.push_str(&serde_json::to_string(&compaction).context("serialize Compaction entry")?);
    buf.push('\n');
    std::fs::write(&jsonl_path, buf).with_context(|| format!("write {}", jsonl_path.display()))?;

    // Register in pi's SQLite index so `libertai code --list-sessions`
    // and the desktop picker see the new session immediately. Without
    // this, the file is on disk but pi only walks it on a full reindex.
    let _ = SessionIndex::for_sessions_root(&sessions_root).index_session_snapshot(
        &jsonl_path,
        &header,
        1,
        None,
    );

    Ok(WrittenPiSession {
        session_id,
        jsonl_path,
    })
}

/// `$PI_HOME` override (used by tests) → `~/.pi`, suffixed with
/// `agent/sessions`. Mirrors pi's own resolution but doesn't link
/// against pi's `Config` (which has heavier deps).
fn pi_sessions_root() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("PI_HOME") {
        return Ok(PathBuf::from(custom).join("agent").join("sessions"));
    }
    let home = dirs::home_dir().context("HOME unset")?;
    Ok(home.join(".pi").join("agent").join("sessions"))
}

/// `2026-05-21T13:40:42.123Z` → `2026-05-21T13-40-42.123Z`.
/// Colon is invalid in filenames on Windows/FAT; pi already does this
/// substitution for the same reason.
fn fs_safe_timestamp(ts: &str) -> String {
    ts.replace(':', "-")
}

fn short_id() -> String {
    let uuid = uuid::Uuid::new_v4().to_string();
    uuid.chars().filter(|c| *c != '-').take(8).collect()
}

/// Estimate token count from raw transcript length (~4 chars/token).
/// Conservative — overcounts a bit on code-heavy content, which is the
/// safe direction for `tokens_before` (pi uses it to size the prompt
/// budget after compaction; a slightly inflated number just leaves
/// more headroom).
pub fn estimate_tokens(text: &str) -> u64 {
    let len = text.chars().count() as u64;
    len.div_ceil(4)
}

/// Render the linearised session as a plain-text transcript suitable
/// for previewing in the CLI / piping into a summariser prompt.
pub fn render_transcript(session: &LinearizedSession) -> String {
    let mut out = String::new();
    for msg in &session.messages {
        match msg {
            LinearMessage::User { text, .. } => {
                out.push_str("### USER\n");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            LinearMessage::Assistant { text, .. } => {
                out.push_str("### ASSISTANT\n");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            LinearMessage::ToolUse {
                name, args_preview, ..
            } => {
                out.push_str(&format!("### TOOL {name}\n{args_preview}\n\n"));
            }
            LinearMessage::ToolResult {
                preview, is_error, ..
            } => {
                out.push_str(if *is_error {
                    "### TOOL RESULT (error)\n"
                } else {
                    "### TOOL RESULT\n"
                });
                out.push_str(preview);
                out.push_str("\n\n");
            }
        }
    }
    out
}

/// Serialize `SystemTime` as RFC 3339 (UTC) for JSON output.
mod systime_serde {
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::{DateTime, Utc};
    use serde::Serializer;

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dt = DateTime::<Utc>::from_timestamp(secs as i64, 0).unwrap_or_default();
        s.serialize_str(&dt.to_rfc3339())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn encode_basic() {
        assert_eq!(
            encode_project_dir(Path::new("/home/aliel/foo")),
            "-home-aliel-foo"
        );
    }

    #[test]
    fn extract_text_string_content() {
        let v: serde_json::Value = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(extract_text(v), Some("hello".to_string()));
    }

    #[test]
    fn extract_text_blocks_content() {
        let v: serde_json::Value = serde_json::json!([
            { "type": "text", "text": "first" },
            { "type": "tool_use", "name": "Read" },
        ]);
        assert_eq!(extract_text(v), Some("first".to_string()));
    }

    #[test]
    fn discover_returns_empty_when_no_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("nowhere");
        std::env::set_var("HOME", tmp.path()); // unsafe in parallel
        let sessions = discover(&cwd, false).unwrap();
        assert!(sessions.is_empty());
    }

    fn write_records(path: &Path, records: &[serde_json::Value]) {
        let mut f = std::fs::File::create(path).unwrap();
        for r in records {
            writeln!(f, "{}", r).unwrap();
        }
    }

    fn user(uuid: &str, parent: Option<&str>, ts: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user", "uuid": uuid, "parentUuid": parent,
            "timestamp": ts, "isSidechain": false,
            "message": { "role": "user", "content": text },
        })
    }

    fn assistant_text(uuid: &str, parent: &str, ts: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "parentUuid": parent,
            "timestamp": ts, "isSidechain": false,
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
        })
    }

    fn assistant_tool_use(
        uuid: &str,
        parent: &str,
        ts: &str,
        tool: &str,
        input: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "parentUuid": parent,
            "timestamp": ts, "isSidechain": false,
            "message": { "role": "assistant", "content": [
                { "type": "tool_use", "id": "t1", "name": tool, "input": input },
            ]},
        })
    }

    #[test]
    fn linearize_picks_latest_leaf_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        // root → u1 → a1 (early leaf) AND root → u1 → a2 (later leaf, the "live" branch).
        write_records(
            &path,
            &[
                user("u1", None, "2026-05-21T10:00:00Z", "hello"),
                assistant_text("a1", "u1", "2026-05-21T10:00:05Z", "old reply"),
                assistant_text("a2", "u1", "2026-05-21T10:01:00Z", "new reply"),
            ],
        );
        let lin = linearize(&path).unwrap();
        let texts: Vec<&str> = lin
            .messages
            .iter()
            .filter_map(|m| match m {
                LinearMessage::Assistant { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["new reply"], "expected the latest-leaf branch");
        assert_eq!(lin.dropped_sidechain, 0);
    }

    #[test]
    fn linearize_drops_harness_injected_user_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let notification = serde_json::json!({
            "type": "user", "uuid": "n1", "parentUuid": "u1",
            "timestamp": "2026-05-21T10:00:10Z",
            "origin": { "kind": "task-notification" },
            "message": { "role": "user",
                "content": "<task-notification>Background command done</task-notification>" },
        });
        let meta = serde_json::json!({
            "type": "user", "uuid": "m1", "parentUuid": "n1",
            "timestamp": "2026-05-21T10:00:20Z", "isMeta": true,
            "message": { "role": "user", "content": "Caveat: local command output" },
        });
        write_records(
            &path,
            &[
                user("u1", None, "2026-05-21T10:00:00Z", "real question"),
                notification,
                meta,
                assistant_text("a1", "m1", "2026-05-21T10:01:00Z", "reply"),
            ],
        );
        let lin = linearize(&path).unwrap();
        let users: Vec<&str> = lin
            .messages
            .iter()
            .filter_map(|m| match m {
                LinearMessage::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            users,
            vec!["real question"],
            "task-notification / isMeta records must not surface as user turns"
        );
    }

    #[test]
    fn linearize_drops_sidechain_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let sidechain = serde_json::json!({
            "type": "user", "uuid": "sc1", "parentUuid": null,
            "timestamp": "2026-05-21T10:00:00Z", "isSidechain": true,
            "message": { "role": "user", "content": "sub-agent prompt" },
        });
        write_records(
            &path,
            &[
                sidechain,
                user("u1", None, "2026-05-21T10:00:00Z", "hi"),
                assistant_text("a1", "u1", "2026-05-21T10:00:05Z", "hello"),
            ],
        );
        let lin = linearize(&path).unwrap();
        assert_eq!(lin.dropped_sidechain, 1);
        assert_eq!(lin.messages.len(), 2);
    }

    #[test]
    fn linearize_records_file_ops_per_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_records(
            &path,
            &[
                user("u1", None, "2026-05-21T10:00:00Z", "go"),
                assistant_tool_use(
                    "a1",
                    "u1",
                    "2026-05-21T10:00:05Z",
                    "Read",
                    serde_json::json!({ "file_path": "/x/read-me.rs" }),
                ),
                assistant_tool_use(
                    "a2",
                    "a1",
                    "2026-05-21T10:00:06Z",
                    "Edit",
                    serde_json::json!({ "file_path": "/x/edit-me.rs", "old_string": "", "new_string": "" }),
                ),
                assistant_tool_use(
                    "a3",
                    "a2",
                    "2026-05-21T10:00:07Z",
                    "Write",
                    serde_json::json!({ "file_path": "/x/new.rs", "content": "" }),
                ),
                assistant_tool_use(
                    "a4",
                    "a3",
                    "2026-05-21T10:00:08Z",
                    "Bash",
                    serde_json::json!({ "command": "ls /tmp" }),
                ),
            ],
        );
        let lin = linearize(&path).unwrap();
        let read: Vec<_> = lin.read_files.iter().collect();
        let modified: Vec<_> = lin.modified_files.iter().collect();
        assert_eq!(read, vec![&PathBuf::from("/x/read-me.rs")]);
        assert_eq!(
            modified,
            vec![&PathBuf::from("/x/edit-me.rs"), &PathBuf::from("/x/new.rs")],
        );
    }

    #[test]
    fn write_pi_session_file_roundtrips_via_pi_deserialize() {
        use pi::session::{SessionEntry, SessionHeader};
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("PI_HOME", tmp.path()); // unsafe under parallel tests; this module's tests are isolated

        let cwd = PathBuf::from("/proj/foo");
        let source = tmp.path().join("source.jsonl");
        std::fs::write(&source, "").unwrap();
        let mut read_files = BTreeSet::new();
        read_files.insert(PathBuf::from("/proj/foo/a.rs"));
        let modified_files = BTreeSet::new();

        let written = write_pi_session_file(PiSessionInputs {
            cwd: &cwd,
            source_session_uuid: "abc-123",
            source_path: &source,
            summary: "## Goal\nDo X",
            tokens_before: 1234,
            provider: Some("libertai".to_string()),
            model_id: Some("test-model".to_string()),
            read_files: &read_files,
            modified_files: &modified_files,
        })
        .unwrap();

        let body = std::fs::read_to_string(&written.jsonl_path).unwrap();
        let mut lines = body.lines();
        let header: SessionHeader = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header.r#type, "session");
        assert_eq!(header.cwd, "/proj/foo");
        assert_eq!(header.id, written.session_id);
        let entry: SessionEntry = serde_json::from_str(lines.next().unwrap()).unwrap();
        match entry {
            SessionEntry::Compaction(c) => {
                assert_eq!(c.summary, "## Goal\nDo X");
                assert_eq!(c.tokens_before, 1234);
                let details = c.details.expect("details");
                assert_eq!(details["source"], "claude-code");
                assert_eq!(details["sourceSessionUuid"], "abc-123");
                assert_eq!(details["readFiles"], serde_json::json!(["/proj/foo/a.rs"]),);
            }
            _ => panic!("expected Compaction entry"),
        }
        assert!(lines.next().is_none(), "no extra lines");

        // File landed under the encoded cwd dir.
        assert!(
            written
                .jsonl_path
                .parent()
                .unwrap()
                .to_string_lossy()
                .ends_with("--proj-foo--"),
            "path: {}",
            written.jsonl_path.display(),
        );
    }

    #[test]
    fn build_summary_prompt_carries_cwd_and_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let with_meta = serde_json::json!({
            "type": "user", "uuid": "u1", "parentUuid": null,
            "timestamp": "2026-05-21T10:00:00Z", "isSidechain": false,
            "cwd": "/proj", "gitBranch": "feat/x",
            "message": { "role": "user", "content": "do the thing" },
        });
        write_records(
            &path,
            &[
                with_meta,
                assistant_text("a1", "u1", "2026-05-21T10:00:05Z", "thing done"),
            ],
        );
        let lin = linearize(&path).unwrap();
        let (system, user) = build_summary_prompt(&lin);
        assert!(system.contains("/proj"), "system mentions cwd: {system}");
        assert!(
            system.contains("feat/x"),
            "system mentions branch: {system}"
        );
        assert!(system.contains("## Goal"), "system carries pi template");
        assert!(user.contains("### USER\ndo the thing"));
        assert!(user.contains("### ASSISTANT\nthing done"));
    }

    #[test]
    fn render_transcript_groups_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_records(
            &path,
            &[
                user("u1", None, "2026-05-21T10:00:00Z", "hello"),
                assistant_text("a1", "u1", "2026-05-21T10:00:05Z", "hi back"),
            ],
        );
        let lin = linearize(&path).unwrap();
        let rendered = render_transcript(&lin);
        assert!(rendered.contains("### USER\nhello"));
        assert!(rendered.contains("### ASSISTANT\nhi back"));
    }

    #[test]
    fn peek_extracts_summary_and_first_user() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({"type": "summary", "summary": "Old session about X", "leafUuid": "abc"})
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "type": "user",
                "cwd": "/home/x/proj",
                "gitBranch": "main",
                "message": { "role": "user", "content": "hi there" },
            })
        )
        .unwrap();
        let info = peek_jsonl(&path).unwrap();
        assert_eq!(info.record_count, 2);
        assert_eq!(
            info.claude_code_summary.as_deref(),
            Some("Old session about X")
        );
        assert_eq!(info.first_user_message.as_deref(), Some("hi there"));
        assert_eq!(info.git_branch.as_deref(), Some("main"));
        assert_eq!(info.recorded_cwd, Some(PathBuf::from("/home/x/proj")));
    }
}
