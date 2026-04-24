//! Tool registry assembly for `libertai code`.
//!
//! Hands pi a `SessionOptions::tool_factory` that:
//!
//! 1. Asks pi for its default built-in tool set (read/bash/edit/write/…).
//! 2. Filters out mutating tools when in [`Mode::Plan`].
//! 3. Wraps every survivor in an [`ApprovalTool`] so `bash`/`edit`/
//!    `write`/`hashline_edit` prompt before execution — the shared
//!    [`ApprovalState`] keeps "always allow" memory scoped to this
//!    session.
//! 4. Appends our own tools: the `task` subagent, and (later) the
//!    `todo` task-list tool that the UI subscribes to.

use std::path::Path;
use std::sync::Arc;

use pi::sdk::{default_tool_registry, Config as PiConfig, Tool, ToolFactory, ToolRegistry};

use crate::commands::code_approvals::{ApprovalState, ApprovalTool};
use crate::commands::code_task::TaskTool;

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
    /// Read-only tools only; mutating ones are absent from the registry.
    Plan,
}

impl Mode {
    /// Whether a tool-name is admissible in this mode. Called on the
    /// built-ins pi hands us; our own tools decide per-mode separately.
    pub fn allows(self, tool_name: &str, is_read_only: bool) -> bool {
        match self {
            Mode::Normal => true,
            Mode::Plan => is_read_only || matches!(tool_name, "read" | "grep" | "find" | "ls"),
        }
    }
}

pub struct LibertaiToolFactory {
    pub mode: Mode,
    pub approvals: Arc<ApprovalState>,
    pub depth: u8,
}

impl LibertaiToolFactory {
    pub fn new(mode: Mode, approvals: Arc<ApprovalState>) -> Self {
        Self {
            mode,
            approvals,
            depth: 0,
        }
    }

    /// Factory for a child session spawned by the Task tool.
    pub fn child(&self) -> Self {
        Self {
            mode: self.mode,
            approvals: Arc::clone(&self.approvals),
            depth: self.depth.saturating_add(1),
        }
    }
}

impl ToolFactory for LibertaiToolFactory {
    fn build(&self, enabled: &[&str], cwd: &Path, config: &PiConfig) -> ToolRegistry {
        // 1. Snapshot pi's default tools for the enabled set.
        let defaults = default_tool_registry(enabled, cwd, config).into_tools();

        // 2. Filter + wrap.
        let mut wrapped: Vec<Box<dyn Tool>> = Vec::with_capacity(defaults.len() + 1);
        for tool in defaults {
            let name = tool.name().to_string();
            let ro = tool.is_read_only();
            if !self.mode.allows(&name, ro) {
                continue;
            }
            wrapped.push(Box::new(ApprovalTool::new(tool, Arc::clone(&self.approvals))));
        }

        // 3. Add our own tools.
        //    - `task` (subagent): only if we still have depth headroom.
        if self.depth < MAX_TASK_DEPTH {
            wrapped.push(Box::new(TaskTool::new(
                self.mode,
                Arc::clone(&self.approvals),
                self.depth,
            )));
        }

        ToolRegistry::from_tools(wrapped)
    }
}
