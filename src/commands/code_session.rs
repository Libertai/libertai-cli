//! Single-source-of-truth helpers for building a `pi::sdk::SessionOptions`
//! and listing past sessions on disk.
//!
//! Three callers used to build `SessionOptions` by hand and all set
//! `no_session: true` — `code::run_async` (one-shot CLI), `code_ui::build_handle`
//! (interactive REPL), `code_task` (Task-tool subagents). The
//! `liberclaw-code` desktop app duplicated the same construction. Routing
//! everyone through [`build_session_options`] makes turning persistence
//! on or off — and resuming a saved session — a single-line change at the
//! call site, while keeping the `pi` mapping in one place.
//!
//! Listing helpers wrap `pi::session_index::SessionIndex` so callers don't
//! need to depend on `pi`'s internal index module directly.
//!
//! Subagents launched via the `Task` tool stay [`SessionPersistence::Ephemeral`]
//! by design: they're nested scratch sessions and shouldn't pollute the
//! on-disk store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use pi::sdk::{SessionOptions, ToolFactory};
use pi::session_index::SessionIndex;

pub use pi::session_index::SessionMeta;

/// Whether and how this agent session is backed by a JSONL file on disk.
pub enum SessionPersistence {
    /// No JSONL written. Used for nested subagents (Task tool) and any
    /// caller that wants throwaway state.
    Ephemeral,
    /// New session, persisted to pi's default sessions dir
    /// (`Config::sessions_dir()`, keyed by encoded cwd).
    Fresh,
    /// Continue a specific JSONL file. The agent rehydrates message
    /// history from disk and appends new turns to the same file.
    Resume(PathBuf),
}

/// Inputs for [`build_session_options`].
///
/// Mirrors the subset of `pi::sdk::SessionOptions` that the `code`
/// subcommand and its embedders actually configure today. Anything pi
/// adds in the future and we don't override here keeps its
/// `SessionOptions::default()` value.
pub struct CodeSessionConfig {
    pub provider: String,
    pub model: String,
    pub working_directory: Option<PathBuf>,
    pub include_cwd_in_prompt: bool,
    pub max_tool_iterations: usize,
    pub tool_factory: Arc<dyn ToolFactory>,
    pub persistence: SessionPersistence,
    /// Restrict to a specific built-in tool subset. `None` (default)
    /// uses pi's full enabled tool set; only the Task subagent path
    /// currently filters this down.
    pub enabled_tools: Option<Vec<String>>,
}

/// Map a [`CodeSessionConfig`] to a fully-populated `SessionOptions`.
pub fn build_session_options(cfg: CodeSessionConfig) -> SessionOptions {
    let (no_session, session_path) = match cfg.persistence {
        SessionPersistence::Ephemeral => (true, None),
        SessionPersistence::Fresh => (false, None),
        SessionPersistence::Resume(p) => (false, Some(p)),
    };

    SessionOptions {
        provider: Some(cfg.provider),
        model: Some(cfg.model),
        no_session,
        session_path,
        // Leave session_dir at default — pi falls back to
        // `Config::sessions_dir()` keyed by encoded cwd, which is what
        // every consumer wants today.
        max_tool_iterations: cfg.max_tool_iterations,
        tool_factory: Some(cfg.tool_factory),
        working_directory: cfg.working_directory,
        include_cwd_in_prompt: cfg.include_cwd_in_prompt,
        enabled_tools: cfg.enabled_tools,
        ..SessionOptions::default()
    }
}

/// List sessions previously persisted by pi, sorted recency-desc.
///
/// `cwd = None` returns sessions across every project. `cwd = Some(p)`
/// filters to that exact working directory (string match — pi indexes
/// the cwd verbatim, not canonicalised).
pub fn list_past_sessions(cwd: Option<&Path>) -> Result<Vec<SessionMeta>> {
    let index = SessionIndex::new();
    let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
    index
        .list_sessions(cwd_str.as_deref())
        .map_err(anyhow::Error::new)
}

/// Resolve "the most recent session for this cwd" — used by `--continue`.
pub fn most_recent_session(cwd: &Path) -> Result<Option<SessionMeta>> {
    Ok(list_past_sessions(Some(cwd))?.into_iter().next())
}
