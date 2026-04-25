//! Tool registry assembly for `libertai code`.
//!
//! Hands pi a `SessionOptions::tool_factory` that:
//!
//! 1. Asks pi for its default built-in tool set (read/bash/edit/write/…).
//! 2. Wraps every tool in an [`ApprovalTool`] so `bash`/`edit`/`write`/
//!    `hashline_edit` prompt before execution — the shared
//!    [`ApprovalState`] keeps "always allow" memory scoped to this
//!    session, and the shared [`ModeFlag`] lets the wrapper short-
//!    circuit mutating calls when the session is in [`Mode::Plan`].
//! 3. Appends our own tools: `todo` (task-list overlay) and `task`
//!    (subagent), unless we've hit the recursion cap.
//!
//! The factory itself doesn't filter by mode any more — every tool is
//! always registered. The mode flag is consulted at *call time* by
//! `ApprovalTool::execute`, which means toggling Normal ↔ Plan does
//! not require rebuilding the session and so message history is
//! preserved across `Shift+Tab` / `/plan`.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use pi::sdk::{default_tool_registry, Config as PiConfig, Tool, ToolFactory, ToolRegistry};

use crate::commands::code_approvals::{ApprovalState, ApprovalTool};
use crate::commands::code_task::TaskTool;
use crate::commands::code_todo::TodoTool;

/// Recursion cap for the `task` subagent. A parent session creates a
/// factory with `depth = 0`; each nested Task increments it before
/// building the child's factory. When the cap is hit, Task is simply not
/// registered on the child so the chain terminates.
pub const MAX_TASK_DEPTH: u8 = 3;

/// Run modes for `libertai code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Full tool set. Mutating tools gated by [`ApprovalTool`].
    Normal,
    /// Mutating tools (`bash`, `edit`, `write`, `hashline_edit`) are
    /// auto-denied without prompting; read-only tools still run.
    Plan,
}

impl Mode {
    fn as_u8(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::Plan => 1,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Mode::Normal,
            _ => Mode::Plan,
        }
    }
}

/// Shared, atomically-toggleable mode for an interactive session.
///
/// Cloneable (the `Arc` underneath shares state). The REPL holds one,
/// hands clones to every `ApprovalTool` and to `TaskTool`, and flips
/// it via [`ModeFlag::set`] when the user types `/plan` or hits
/// Shift+Tab. Tool wrappers read the current value at the moment of
/// execution.
#[derive(Clone)]
pub struct ModeFlag(Arc<AtomicU8>);

impl ModeFlag {
    pub fn new(initial: Mode) -> Self {
        Self(Arc::new(AtomicU8::new(initial.as_u8())))
    }

    pub fn get(&self) -> Mode {
        Mode::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, m: Mode) {
        self.0.store(m.as_u8(), Ordering::Relaxed);
    }
}

pub struct LibertaiToolFactory {
    pub mode: ModeFlag,
    pub approvals: Arc<ApprovalState>,
    pub depth: u8,
}

impl LibertaiToolFactory {
    pub fn new(mode: ModeFlag, approvals: Arc<ApprovalState>) -> Self {
        Self {
            mode,
            approvals,
            depth: 0,
        }
    }

    /// Factory for a child session spawned by the Task tool. Inherits
    /// the parent's mode flag (so a Shift+Tab in the parent REPL
    /// affects in-flight subagents too — desired) and approval state.
    pub fn child(&self) -> Self {
        Self {
            mode: self.mode.clone(),
            approvals: Arc::clone(&self.approvals),
            depth: self.depth.saturating_add(1),
        }
    }
}

impl ToolFactory for LibertaiToolFactory {
    fn build(&self, enabled: &[&str], cwd: &Path, config: &PiConfig) -> ToolRegistry {
        // 1. Snapshot pi's default tools for the enabled set. We don't
        //    filter by mode here — the registry stays stable for the
        //    whole session and the mode flag is checked at call time
        //    in `ApprovalTool::execute`.
        let defaults = default_tool_registry(enabled, cwd, config).into_tools();

        // 2. Wrap each in ApprovalTool, sharing the mode flag and
        //    approval allowlist.
        let mut wrapped: Vec<Box<dyn Tool>> = Vec::with_capacity(defaults.len() + 2);
        for tool in defaults {
            wrapped.push(Box::new(ApprovalTool::new(
                tool,
                Arc::clone(&self.approvals),
                self.mode.clone(),
            )));
        }

        // 3. Add our own tools.
        //    - `todo`: task-list overlay. Read-only side effects (prints
        //      to stderr), so we register it unwrapped — it's safe in
        //      both modes.
        wrapped.push(Box::new(TodoTool::new()));

        //    - `task` (subagent): only if we still have depth headroom.
        if self.depth < MAX_TASK_DEPTH {
            wrapped.push(Box::new(TaskTool::new(
                self.mode.clone(),
                Arc::clone(&self.approvals),
                self.depth,
            )));
        }

        ToolRegistry::from_tools(wrapped)
    }
}
