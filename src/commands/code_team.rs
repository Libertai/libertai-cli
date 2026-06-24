//! Unified agent identity & registry.
//!
//! One identity layer spanning in-process subagents and background
//! agent runs, so "how many agents / who is who / what is each doing"
//! is answerable from a single source of truth. The registry is shared
//! by reference (`Arc`) through the tool factory (see `code_factory`)
//! so a subagent spawned by the `task` tool and a background run
//! launched from the REPL end up in the same live table.
//!
//! This module owns the data model and registry only; the live UI
//! panel that renders `AgentRegistry::snapshot()` lives in `code_ui`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use pi::sdk::AbortHandle;

use crate::commands::code_agents::AgentDefinition;

/// Stable identifier for one running or recently-finished agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_str(&self) -> String {
        self.0.to_string()
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

/// What kind of agent this is. Determines how it was launched and how
/// it can be inspected or controlled from the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    /// An in-process subagent spawned by the `task` tool. `depth` is
    /// the nesting level (0 = top-level child of the REPL, 1 = grand-
    /// child, …). `parent` is the parent agent's id (the REPL itself
    /// has no id, so a top-level subagent's parent is `None`).
    Subagent { depth: u8, parent: Option<AgentId> },
    /// A detached OS process running `libertai code`, launched from
    /// the REPL or the agent view. Identified by pid + our run id.
    Background { pid: u32, run_id: String },
    /// A team teammate (M3). Stubbed now so the registry can hold the
    /// slot before the team system is wired up.
    Teammate { team: String },
}

/// Lifecycle state for one agent. Mirrors Claude Code's agent-view
/// states so users transfer skills directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Created but the session hasn't started producing yet.
    Spawning,
    /// Actively running tools or generating a response.
    Working,
    /// Blocked on a permission prompt or a clarifying question.
    NeedsInput,
    /// Turn finished, waiting for the next prompt. (Background runs
    /// reach this between turns; in-process subagents are removed
    /// instead, since they don't persist.)
    Idle,
    /// Task finished successfully.
    Completed,
    /// Task ended with an error.
    Failed,
    /// Stopped by the user (Ctrl+C, `/stop`, kill).
    Stopped,
}

impl AgentStatus {
    /// True for states that count as "active" in the live panel and
    /// the agent-view Working group.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            AgentStatus::Spawning | AgentStatus::Working | AgentStatus::NeedsInput
        )
    }
}

/// Display color for one agent. Assigned from the agent definition's
/// `color:` frontmatter when present, otherwise derived from a stable
/// hash of the name so unstyled agents still get a consistent color
/// across the panel, the transcript, and the agent view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentColor {
    Red,
    Green,
    Yellow,
    Blue,
    Purple,
    Cyan,
    Orange,
    Pink,
    Dim,
}

impl AgentColor {
    /// Parse a `color:` frontmatter value. Unknown values fall back to
    /// [`AgentColor::color_for_name`], so a typo degrades gracefully
    /// rather than failing agent discovery.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "red" => Some(Self::Red),
            "green" => Some(Self::Green),
            "yellow" => Some(Self::Yellow),
            "blue" => Some(Self::Blue),
            "purple" => Some(Self::Purple),
            "cyan" => Some(Self::Cyan),
            "orange" => Some(Self::Orange),
            "pink" => Some(Self::Pink),
            "dim" | "gray" | "grey" => Some(Self::Dim),
            "" => None,
            _ => None,
        }
    }

    /// Stable per-name color, used when no `color:` frontmatter is set.
    /// Picks from the 8 vivid palette entries so every agent gets a
    /// visible color; `Dim` is reserved for explicit opt-in.
    pub fn color_for_name(name: &str) -> Self {
        const PALETTE: [AgentColor; 8] = [
            AgentColor::Red,
            AgentColor::Green,
            AgentColor::Yellow,
            AgentColor::Blue,
            AgentColor::Purple,
            AgentColor::Cyan,
            AgentColor::Orange,
            AgentColor::Pink,
        ];
        let mut hash: u64 = 0;
        for byte in name.as_bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(*byte as u64);
        }
        PALETTE[(hash % PALETTE.len() as u64) as usize]
    }

    /// Render `text` in this color using ANSI escapes. Used by the
    /// live panel and the agent view. Returns `text` unchanged when
    /// the color is `Dim` (the caller applies dim via `owo_colors`).
    pub fn paint(self, text: &str) -> String {
        use owo_colors::OwoColorize;
        match self {
            Self::Red => text.red().to_string(),
            Self::Green => text.green().to_string(),
            Self::Yellow => text.yellow().to_string(),
            Self::Blue => text.blue().to_string(),
            Self::Purple => text.purple().to_string(),
            Self::Cyan => text.cyan().to_string(),
            Self::Orange => text.bright_yellow().to_string(),
            Self::Pink => text.bright_magenta().to_string(),
            Self::Dim => text.dimmed().to_string(),
        }
    }
}

/// Whether an agent can mutate the filesystem. Surfaced from the
/// agent definition's `tools` list; the panel badges write-capable
/// agents so they're distinguishable from read-only research helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCapability {
    ReadOnly,
    ReadWrite,
}

impl AgentCapability {
    /// Derive a capability from a resolved tool list. Any mutating
    /// tool name (matching the existing `is_path_edit_tool` set plus
    /// `bash` and `hashline_edit`) makes the agent read-write.
    pub fn from_tools(tools: &[String]) -> Self {
        const MUTATORS: &[&str] = &[
            "write",
            "edit",
            "hashline_edit",
            "bash",
            "notebook_edit",
            "notebook_execute",
        ];
        if tools.iter().any(|t| MUTATORS.contains(&t.as_str())) {
            Self::ReadWrite
        } else {
            Self::ReadOnly
        }
    }
}

/// One agent's live state, held by the registry. Fields the UI reads
/// are wrapped in `Arc<Mutex<…>>` so the panel can snapshot them
/// without holding the registry lock.
pub struct AgentHandle {
    pub id: AgentId,
    /// Display name: the agent definition's `name` for subagents, the
    /// lead-assigned name for teammates, or the run name for background
    /// agents.
    pub name: String,
    pub kind: AgentKind,
    pub color: AgentColor,
    pub capability: AgentCapability,
    pub cwd: PathBuf,
    pub model: String,
    /// First line of the prompt that launched this agent, truncated.
    pub prompt_preview: String,
    pub spawned_at: Instant,
    pub status: Arc<Mutex<AgentStatus>>,
    /// Name of the tool currently running inside this agent, if any.
    /// Updated from the `subagent_tool_start`/`_end` events.
    pub current_tool: Arc<Mutex<Option<String>>>,
    /// Parent agent id, for rendering the nesting tree in the panel.
    /// `None` for top-level subagents and background runs.
    pub parent: Option<AgentId>,
    /// OS process id for background agents / teammates. `None` for
    /// in-process subagents. Used by the TUI to poll whether the
    /// process is still alive.
    pub pid: Option<u32>,
    /// Log file path for background agents / teammates. `None` for
    /// in-process subagents. The TUI reads this to show the agent's
    /// output in the overlay view.
    pub log_path: Option<PathBuf>,
    /// Per-subagent abort handle. Set by the spawner after the child
    /// session exists (so the main thread can request a stop), and taken
    /// (cleared) once the run finishes so a finished agent can't be
    /// aborted. `Arc<Mutex<Option<_>>>` mirrors the main turn's
    /// `SharedAbort` so the setter/taker paths line up.
    pub abort: Arc<Mutex<Option<AbortHandle>>>,
}

impl AgentHandle {
    pub fn status(&self) -> AgentStatus {
        // Poison-recovery (#21): if a thread panicked while holding this
        // lock, recover the (possibly stale) status instead of panicking
        // the whole TUI. A stale status is strictly better than a dead UI.
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_status(&self, status: AgentStatus) {
        // Poison-recovery (#21): see `status()`. A poisoned lock yields the
        // (possibly stale) guard via `into_inner()`, so the write proceeds on
        // the recovered inner value rather than panicking the TUI — same
        // semantics as the reader. A stale status beats a dead UI.
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    pub fn current_tool(&self) -> Option<String> {
        // Poison-recovery (#21): see `status()`.
        self.current_tool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_current_tool(&self, tool: Option<String>) {
        // Poison-recovery (#21): see `current_tool()`. Mirror the reader —
        // write into the recovered inner value instead of panicking.
        *self.current_tool.lock().unwrap_or_else(|e| e.into_inner()) = tool;
    }

    /// Take ownership of the stored abort handle, if any. Returns the
    /// handle so the caller can drive the abort, and leaves the slot
    /// empty. Used by the spawn path after a run finishes so a finished
    /// agent can't be aborted.
    pub fn take_abort(&self) -> Option<AbortHandle> {
        // Poison-recovery (#21): see `abort_handle()`. Take off the
        // recovered inner value instead of panicking.
        self.abort.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Clone of the stored abort handle, if any (cheap — the inner is
    /// `Arc`). The handle stays in the slot, so the main thread can
    /// request a stop without the spawner having to coordinate.
    pub fn abort_handle(&self) -> Option<AbortHandle> {
        // Poison-recovery (#21): see `status()`.
        self.abort.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Store the abort handle for a run the spawner just kicked off.
    /// Called once the child session exists (so a handle to stop it
    /// exists too).
    pub fn set_abort(&self, h: AbortHandle) {
        // Poison-recovery (#21): see `abort_handle()`. Write into the
        // recovered inner value instead of panicking.
        *self.abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    /// Elapsed since spawn, for the panel's per-row timer.
    pub fn elapsed(&self) -> Duration {
        self.spawned_at.elapsed()
    }
}

/// Inputs needed to register an agent. Kept as a plain struct so the
/// `task` tool and the background launcher build it the same way.
pub struct AgentRegistration {
    pub name: String,
    pub kind: AgentKind,
    pub color: AgentColor,
    pub capability: AgentCapability,
    pub cwd: PathBuf,
    pub model: String,
    pub prompt_preview: String,
    pub parent: Option<AgentId>,
    /// OS process id for background agents / teammates (`None` for
    /// in-process subagents).
    pub pid: Option<u32>,
    /// Log file path for background agents / teammates (`None` for
    /// in-process subagents).
    pub log_path: Option<PathBuf>,
}

impl AgentRegistry {
    /// Build a registration from a discovered agent definition plus
    /// the caller's resolved tool list. The color comes from the
    /// definition's `color:` frontmatter, falling back to a stable
    /// name hash. The capability comes from the resolved tool list.
    pub fn registration_for(
        definition: &AgentDefinition,
        resolved_tools: &[String],
        kind: AgentKind,
        cwd: PathBuf,
        model: String,
        prompt_preview: String,
        parent: Option<AgentId>,
    ) -> AgentRegistration {
        let color = definition
            .color
            .unwrap_or_else(|| AgentColor::color_for_name(&definition.name));
        let capability = AgentCapability::from_tools(resolved_tools);
        AgentRegistration {
            name: definition.name.clone(),
            kind,
            color,
            capability,
            cwd,
            model,
            prompt_preview,
            parent,
            pid: None,
            log_path: None,
        }
    }
}

/// Shared, thread-safe table of live and recently-finished agents.
/// Threading an `Arc<AgentRegistry>` through the tool factory (the
/// same way `ApprovalState` and `ModeFlag` are shared) means an
/// in-process subagent and a background run land in the same table.
#[derive(Default)]
pub struct AgentRegistry {
    handles: Mutex<HashMap<AgentId, Arc<AgentHandle>>>,
}

impl AgentRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a new agent. Returns the shared handle so the caller
    /// can update its status/current_tool as events arrive, and so the
    /// `task` tool can remove it when the subagent returns.
    pub fn register(&self, reg: AgentRegistration) -> Arc<AgentHandle> {
        let id = AgentId::new();
        let handle = Arc::new(AgentHandle {
            id,
            name: reg.name,
            kind: reg.kind,
            color: reg.color,
            capability: reg.capability,
            cwd: reg.cwd,
            model: reg.model,
            prompt_preview: reg.prompt_preview,
            spawned_at: Instant::now(),
            status: Arc::new(Mutex::new(AgentStatus::Spawning)),
            current_tool: Arc::new(Mutex::new(None)),
            parent: reg.parent,
            pid: reg.pid,
            log_path: reg.log_path,
            abort: Arc::new(Mutex::new(None)),
        });
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::clone(&handle));
        handle
    }

    /// Drop an agent from the live table (called when an in-process
    /// subagent returns). Background runs stay registered so the agent
    /// view can show their final state; the view prunes old records
    /// from `runs.jsonl` separately.
    pub fn remove(&self, id: AgentId) {
        // Poison-recovery (#21): see `snapshot()`. Remove from the
        // recovered inner map instead of panicking.
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// Update an agent's status by id. No-op if it was already removed.
    pub fn set_status(&self, id: AgentId, status: AgentStatus) {
        // Poison-recovery (#21): see `snapshot()`. Read off the recovered
        // inner map instead of panicking; the handle's own `set_status`
        // recovers its status lock too.
        if let Some(h) = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            h.set_status(status);
        }
    }

    /// All handles, sorted by spawn time (oldest first). Used by the
    /// agent view and the live panel to render agents in a stable order.
    pub fn snapshot(&self) -> Vec<Arc<AgentHandle>> {
        // Poison-recovery (#21): a stale snapshot beats a dead TUI.
        let mut handles: Vec<Arc<AgentHandle>> = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        handles.sort_by_key(|h| h.spawned_at);
        handles
    }

    /// Handles in active states (Spawning/Working/NeedsInput). Used by
    /// the live panel and the status-bar count.
    pub fn active(&self) -> Vec<Arc<AgentHandle>> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|h| h.status().is_active())
            .cloned()
            .collect()
    }

    /// Count of active agents, cheap enough to call on every spinner
    /// tick.
    pub fn active_count(&self) -> usize {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|h| h.status().is_active())
            .count()
    }

    /// Total count of all agents (active + completed + failed). Used
    /// by the footer hint so the tab indicator shows even when all
    /// agents have finished.
    pub fn total_count(&self) -> usize {
        self.handles.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Find a handle by agent name. Returns the first match (names
    /// are unique in practice). Used by the TUI to look up an agent's
    /// color for transcript attribution.
    pub fn find_by_name(&self, name: &str) -> Option<Arc<AgentHandle>> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|h| h.name == name)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(name: &str, kind: AgentKind) -> AgentRegistration {
        AgentRegistration {
            name: name.to_string(),
            kind,
            color: AgentColor::color_for_name(name),
            capability: AgentCapability::ReadOnly,
            cwd: PathBuf::from("/tmp"),
            model: "test".to_string(),
            prompt_preview: "preview".to_string(),
            parent: None,
            pid: None,
            log_path: None,
        }
    }

    #[test]
    fn register_and_snapshot() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        assert_eq!(registry.active_count(), 1);
        assert_eq!(registry.snapshot().len(), 1);
        assert_eq!(h.name, "reviewer");
        assert_eq!(h.status(), AgentStatus::Spawning);
    }

    #[test]
    fn active_filters_finished() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        assert_eq!(registry.active_count(), 1);
        h.set_status(AgentStatus::Completed);
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn remove_drops_handle() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        registry.remove(h.id);
        assert_eq!(registry.snapshot().len(), 0);
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn color_for_name_is_stable() {
        assert_eq!(
            AgentColor::color_for_name("reviewer"),
            AgentColor::color_for_name("reviewer")
        );
        assert_ne!(
            AgentColor::color_for_name("reviewer"),
            AgentColor::color_for_name("researcher")
        );
    }

    #[test]
    fn color_parse_known_values() {
        assert_eq!(AgentColor::parse("red"), Some(AgentColor::Red));
        assert_eq!(AgentColor::parse(" Blue "), Some(AgentColor::Blue));
        assert_eq!(AgentColor::parse("nope"), None);
        assert_eq!(AgentColor::parse(""), None);
    }

    #[test]
    fn capability_from_tools() {
        assert_eq!(AgentCapability::from_tools(&[]), AgentCapability::ReadOnly);
        assert_eq!(
            AgentCapability::from_tools(&["read".to_string()]),
            AgentCapability::ReadOnly
        );
        assert_eq!(
            AgentCapability::from_tools(&["read".to_string(), "write".to_string()]),
            AgentCapability::ReadWrite
        );
        assert_eq!(
            AgentCapability::from_tools(&["bash".to_string()]),
            AgentCapability::ReadWrite
        );
    }

    #[test]
    fn current_tool_updates() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        assert_eq!(h.current_tool(), None);
        h.set_current_tool(Some("read".to_string()));
        assert_eq!(h.current_tool(), Some("read".to_string()));
        h.set_current_tool(None);
        assert_eq!(h.current_tool(), None);
    }

    // --- M5b-abort: AgentHandle.abort slot --------------------------------

    // (M5b-abort-1a) A freshly-registered agent has no abort handle set —
    // the spawner stores one only after the child session exists, so the
    // default is `None`. Pins the `register` contract the spawn path relies
    // on (it can call `set_abort` without first clearing a stale slot).
    #[test]
    fn abort_slot_defaults_to_none_after_register() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        assert!(
            h.abort.lock().unwrap().is_none(),
            "a freshly-registered agent must have no abort handle"
        );
        // The accessors agree on the default.
        assert!(h.abort_handle().is_none());
        assert!(h.take_abort().is_none());
    }

    // (M5b-abort-1b) `set_abort` stores a handle, `take_abort` clears it
    // (returning the handle so the caller can drive the abort), and the slot
    // is empty afterward — the exact lifecycle the spawn path uses so a
    // finished agent can't be aborted a second time.
    #[test]
    fn abort_slot_set_take_clears() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));

        let (handle, _signal) = AbortHandle::new();
        h.set_abort(handle);
        assert!(
            h.abort.lock().unwrap().is_some(),
            "set_abort must store the handle"
        );
        assert!(
            h.abort_handle().is_some(),
            "abort_handle must reflect the stored handle"
        );

        let taken = h.take_abort();
        assert!(taken.is_some(), "take_abort must return the stored handle");
        assert!(
            h.abort.lock().unwrap().is_none(),
            "take_abort must clear the slot so a finished agent can't be aborted"
        );
        // A second take is empty — the slot was already drained.
        assert!(h.take_abort().is_none());
    }

    // (M5b-abort-2) The cross-thread trigger actually fires: cloning the
    // handle (as `abort_handle` does — the inner is `Arc`), calling
    // `.abort()` on the clone, and observing `is_aborted()` true on the
    // original signal pins the AbortHandle/AbortSignal pairing the spawn
    // path threads through `prompt_with_abort`. A handle taken off the slot
    // (the path the bg thread's `Cmd::StopAgent` uses) still aborts.
    #[test]
    fn abort_handle_abort_is_observable_via_signal() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));

        let (handle, signal) = AbortHandle::new();
        h.set_abort(handle);

        assert!(!signal.is_aborted(), "signal must start clear");

        // The bg thread takes the handle off the slot (its `Cmd::StopAgent`
        // path) and aborts via the taken handle.
        let taken = h.take_abort().expect("handle was set");
        taken.abort();

        assert!(
            signal.is_aborted(),
            "aborting a taken handle must mark the paired signal aborted"
        );
        assert!(h.abort.lock().unwrap().is_none(), "taking cleared the slot");
    }

    // --- #21: mutex-poison recovery ---------------------------------------

    /// Poison a `Mutex<T>` by panicking while holding its lock (caught via
    /// `catch_unwind`), then assert the inner value is still readable. This
    /// is the recovery contract the `AgentHandle` / `AgentRegistry` accessors
    /// rely on: a poisoned lock yields the (possibly stale) inner value via
    /// `unwrap_or_else(|e| e.into_inner())` instead of panicking the whole TUI.
    /// A stale status is strictly better than a dead UI.
    #[test]
    fn poisoned_mutex_inner_is_recoverable() {
        let m = std::sync::Mutex::new(42i32);
        // Hold the lock and panic; the lock becomes poisoned, but the guard
        // (and its 42) is dropped during unwinding, leaving the inner value
        // intact and recoverable.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("poison the mutex");
        }));
        assert!(result.is_err(), "the inner closure must panic");
        assert!(
            m.is_poisoned(),
            "the mutex must be poisoned after a panic-in-lock"
        );
        // The poison-recovery accessor pattern: `.into_inner()` extracts the
        // inner value out of the `PoisonError` instead of propagating the
        // panic. This is exactly what `AgentHandle::status()` does.
        let recovered = m.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(*recovered, 42, "recovered the inner value, not a panic");
    }

    /// `AgentHandle::status()` recovers from a poisoned status lock: poison
    /// the inner `Arc<Mutex<AgentStatus>>` by panicking while locked, then
    /// assert `status()` returns the (stale) inner value rather than
    /// panicking. Pins the `unwrap_or_else(|e| e.into_inner())` line at the
    /// accessor — the real recovery the TUI depends on.
    #[test]
    fn agent_status_recovers_from_poisoned_lock() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        // Establish a known status before poisoning.
        h.set_status(AgentStatus::Working);
        assert_eq!(h.status(), AgentStatus::Working);

        // Poison the status lock by panicking while holding it.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = h.status.lock().unwrap();
            panic!("poison the status lock");
        }));
        assert!(result.is_err(), "inner closure must panic");
        assert!(h.status.is_poisoned(), "status lock is poisoned");

        // The accessor must NOT panic — it recovers the stale inner value.
        // (A stale status beats a dead TUI.)
        let recovered = h.status();
        assert_eq!(
            recovered,
            AgentStatus::Working,
            "status() recovers the stale value from a poisoned lock: {recovered:?}"
        );
    }

    /// `AgentHandle::current_tool()` recovers from a poisoned tool lock,
    /// mirroring the status recovery. Poison the lock, assert the accessor
    /// returns the stale inner value instead of panicking.
    #[test]
    fn agent_current_tool_recovers_from_poisoned_lock() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        h.set_current_tool(Some("bash".to_string()));
        assert_eq!(h.current_tool(), Some("bash".to_string()));

        // Poison the current_tool lock.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = h.current_tool.lock().unwrap();
            panic!("poison the current_tool lock");
        }));
        assert!(result.is_err(), "inner closure must panic");
        assert!(
            h.current_tool.is_poisoned(),
            "current_tool lock is poisoned"
        );

        // Accessor recovers the stale value, no panic.
        assert_eq!(
            h.current_tool(),
            Some("bash".to_string()),
            "current_tool() recovers the stale value from a poisoned lock"
        );
    }

    /// `AgentRegistry::snapshot()` recovers from a poisoned handles lock:
    /// poison the registry's `handles` map, then assert `snapshot()` returns
    /// the (stale) set of handles instead of panicking. Pins the
    /// `unwrap_or_else(|e| e.into_inner())` at the registry accessors that the
    /// agent panel reads on every tick.
    #[test]
    fn registry_snapshot_recovers_from_poisoned_lock() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        assert_eq!(registry.snapshot().len(), 1);

        // Poison the registry's handles lock. Reach the private field via
        // `super::*` (tests live in the same module).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.handles.lock().unwrap();
            panic!("poison the registry handles lock");
        }));
        assert!(result.is_err(), "inner closure must panic");
        assert!(
            registry.handles.is_poisoned(),
            "registry handles lock is poisoned"
        );

        // snapshot() must recover and return the stale handle, not panic.
        let snap = registry.snapshot();
        assert_eq!(
            snap.len(),
            1,
            "snapshot() recovers the stale handle map (len={})",
            snap.len()
        );
        assert_eq!(snap[0].name, "reviewer");
        // The other registry accessors use the same recovery; spot-check
        // active_count / total_count don't panic on a poisoned lock either.
        assert_eq!(
            registry.total_count(),
            1,
            "total_count recovers from poison"
        );
        assert_eq!(
            registry.active_count(),
            1,
            "active_count recovers from poison"
        );
        // The handle is still usable (its own locks are unpoisoned).
        assert_eq!(h.status(), AgentStatus::Spawning);
    }

    /// `abort_handle()` recovers from a poisoned abort lock — the third
    /// accessor using the poison-recovery pattern. Poison the abort lock and
    /// assert the accessor returns the stale inner value.
    #[test]
    fn agent_abort_handle_recovers_from_poisoned_lock() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        let (handle, _signal) = AbortHandle::new();
        h.set_abort(handle);
        assert!(h.abort_handle().is_some(), "abort handle is set");

        // Poison the abort lock.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = h.abort.lock().unwrap();
            panic!("poison the abort lock");
        }));
        assert!(result.is_err(), "inner closure must panic");
        assert!(h.abort.is_poisoned(), "abort lock is poisoned");

        // abort_handle() recovers the stale inner value, no panic.
        assert!(
            h.abort_handle().is_some(),
            "abort_handle() recovers the stale handle from a poisoned lock"
        );
    }

    /// `AgentHandle::set_status` recovers from a poisoned status lock — the
    /// writer-side counterpart to `agent_status_recovers_from_poisoned_lock`.
    /// The reader-side test pins that `status()` does not panic; this pins that
    /// the *writer* `set_status` does not panic either. This is the regression
    /// guard for the poison-writes fix: a panic-while-holding-the-lock poisons
    /// the mutex, and `poll_agent_status` calls `handle.set_status(Idle)` on
    /// every reaped agent every 80ms tick — so a poisoned status mutex that
    /// crashed the write would crash the TUI on the next tick. The recovered
    /// guard is mut, so the write proceeds on the poisoned-but-recovered inner
    /// value (same semantics as the readers). After the write, `status()`
    /// reads back the value we just set — confirming the write landed.
    #[test]
    fn agent_set_status_recovers_from_poisoned_lock() {
        let registry = AgentRegistry::new();
        let h = registry.register(reg(
            "reviewer",
            AgentKind::Subagent {
                depth: 0,
                parent: None,
            },
        ));
        // Establish a known status before poisoning.
        h.set_status(AgentStatus::Working);
        assert_eq!(h.status(), AgentStatus::Working);

        // Poison the status lock by panicking while holding it.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = h.status.lock().unwrap();
            panic!("poison the status lock");
        }));
        assert!(result.is_err(), "inner closure must panic");
        assert!(h.status.is_poisoned(), "status lock is poisoned");

        // The writer must NOT panic — it recovers the (poisoned-but-recovered)
        // guard and writes into the inner value. Wrap in catch_unwind so a
        // regression (a panic) is surfaced as a test failure rather than
        // aborting the test binary.
        let write_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            h.set_status(AgentStatus::Idle);
        }));
        assert!(
            write_result.is_ok(),
            "set_status must not panic on a poisoned lock (regression: poison-writes fix)"
        );

        // The write landed: status() reads back the value we just set. Both
        // sides recover the same poisoned-but-recovered inner value.
        assert_eq!(
            h.status(),
            AgentStatus::Idle,
            "set_status wrote through the recovered (poisoned) guard: status reflects the new value"
        );
    }
}
