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

use crate::commands::code_approvals::{ApprovalState, ApprovalTool, ApprovalUi};
use crate::commands::code_task::TaskTool;
use crate::commands::code_ask_user::AskUserTool;
use crate::commands::code_todo::TodoTool;
use crate::commands::fetch_tool::FetchTool;
use crate::commands::image_tool::ImageGenTool;
use crate::commands::search_tool::SearchTool;
use crate::config::Config as LibertaiConfig;

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
    /// Path-editing tools (`write`, `edit`, `hashline_edit`) auto-
    /// allow without prompting; `bash` and other mutating tools
    /// still go through the approval flow. The middle tier
    /// between Normal (prompt for everything mutating) and Plan
    /// (deny everything mutating). Mirrors Claude Code's
    /// `acceptEdits` permission mode.
    AcceptEdits,
    /// Mutating tools (`bash`, `edit`, `write`, `hashline_edit`) are
    /// auto-denied without prompting; read-only tools still run.
    Plan,
}

impl Mode {
    fn as_u8(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::AcceptEdits => 1,
            Mode::Plan => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Mode::Normal,
            1 => Mode::AcceptEdits,
            _ => Mode::Plan,
        }
    }
}

/// True for tools that mutate a single file path supplied by the
/// model — these are the "edits" `Mode::AcceptEdits` auto-allows.
/// `bash` is excluded: its mutation surface is open-ended, so it
/// stays gated by the regular approval flow. Add new entries as
/// pi grows other path-edit tools.
pub(crate) fn is_path_edit_tool(name: &str) -> bool {
    matches!(name, "write" | "edit" | "hashline_edit")
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

/// Per-feature toggles for the factory. Lets the desktop's chat
/// pillar opt out of the `task` subagent and tune which web/image tools
/// register without forking the factory. Default tuning ships
/// search/fetch/`generate_image` ON across both desktop and CLI now —
/// terminal users get the same upgrade desktop pillars get.
#[derive(Debug, Clone, Default)]
pub struct FactoryFeatures {
    /// Enable the `task` subagent. Off for chat-pillar so a chat
    /// session can't recursively spawn coding agents.
    pub task: bool,
    /// Enable the `todo` task-list overlay. Always-on for code/agent;
    /// chat usually wants this too.
    pub todo: bool,
    /// Enable the LibertAI `/search` tool. Requires a libertai-cli
    /// `Config` with a valid api_key + search_base.
    pub search: bool,
    /// Enable the local `fetch` tool (raw HTTP via reqwest, no
    /// LibertAI dependency).
    pub fetch: bool,
    /// Enable the LibertAI `generate_image` tool. Requires a libertai-cli
    /// `Config` with a valid api_key.
    pub image: bool,
}

impl FactoryFeatures {
    /// Defaults for libertai-cli's own CLI/REPL: full tool surface
    /// turned on. Search and `generate_image` silently no-op when
    /// `libertai_cfg` is None at registry build time (no api_key →
    /// no LibertAI calls); local `fetch` registers regardless.
    pub fn cli_defaults() -> Self {
        Self {
            task: true,
            todo: true,
            search: true,
            fetch: true,
            image: true,
        }
    }
}

pub struct LibertaiToolFactory {
    pub mode: ModeFlag,
    pub approvals: Arc<ApprovalState>,
    pub ui: Arc<dyn ApprovalUi>,
    pub depth: u8,
    pub features: FactoryFeatures,
    /// Carrier for the libertai-cli `Config` when search/fetch are on.
    /// `None` is fine when both are off; the factory just won't
    /// register those tools. Captured as `Arc` so it's cheap to clone
    /// into each tool's per-instance state.
    pub libertai_cfg: Option<Arc<LibertaiConfig>>,
}

impl LibertaiToolFactory {
    /// CLI default constructor. Equivalent to the pre-features behavior.
    pub fn new(mode: ModeFlag, approvals: Arc<ApprovalState>, ui: Arc<dyn ApprovalUi>) -> Self {
        Self {
            mode,
            approvals,
            ui,
            depth: 0,
            features: FactoryFeatures::cli_defaults(),
            libertai_cfg: None,
        }
    }

    /// Feature-aware constructor. Used by the desktop to opt the
    /// chat pillar into search/fetch and out of the task subagent.
    pub fn new_with_features(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        features: FactoryFeatures,
        libertai_cfg: Option<Arc<LibertaiConfig>>,
    ) -> Self {
        Self {
            mode,
            approvals,
            ui,
            depth: 0,
            features,
            libertai_cfg,
        }
    }

    /// Factory for a child session spawned by the Task tool. Inherits
    /// the parent's mode flag (so a Shift+Tab in the parent REPL
    /// affects in-flight subagents too — desired), approval state,
    /// and approval UI (subagent prompts surface in the same place as
    /// parent prompts).
    pub fn child(&self) -> Self {
        Self {
            mode: self.mode.clone(),
            approvals: Arc::clone(&self.approvals),
            ui: Arc::clone(&self.ui),
            depth: self.depth.saturating_add(1),
            features: self.features.clone(),
            libertai_cfg: self.libertai_cfg.clone(),
        }
    }
}

impl ToolFactory for LibertaiToolFactory {
    fn create_tool_registry(&self, enabled: &[&str], cwd: &Path, config: &PiConfig) -> ToolRegistry {
        // 1. Snapshot pi's default tools for the enabled set. We don't
        //    filter by mode here — the registry stays stable for the
        //    whole session and the mode flag is checked at call time
        //    in `ApprovalTool::execute`.
        let defaults = default_tool_registry(enabled, cwd, config).into_tools();

        // 2. Wrap each in ApprovalTool, sharing the mode flag, approval
        //    allowlist, and approval UI.
        let mut wrapped: Vec<Box<dyn Tool>> = Vec::with_capacity(defaults.len() + 2);
        for tool in defaults {
            wrapped.push(Box::new(ApprovalTool::new(
                tool,
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )));
        }

        // 3. Add our own tools, gated by FactoryFeatures.
        //    - `todo`: task-list overlay. Read-only side effects (prints
        //      to stderr), so we register it unwrapped, it's safe in
        //      both modes.
        if self.features.todo {
            wrapped.push(Box::new(TodoTool::new()));
        }

        //    - `ask_user`: Claude-Code-style structured questions.
        //      Always-on across pillars: any agent can pause and ask
        //      the user a clarifying question. The default
        //      ApprovalUi::ask impl returns "cancelled" for UIs that
        //      don't surface this, so the LLM degrades gracefully on
        //      the terminal CLI.
        wrapped.push(Box::new(AskUserTool::new(Arc::clone(&self.ui))));

        //    - `task` (subagent): only when feature-on AND we still
        //      have depth headroom. Chat pillar opts out so a chat
        //      session can't recursively spawn coding agents.
        if self.features.task && self.depth < MAX_TASK_DEPTH {
            wrapped.push(Box::new(TaskTool::new(
                self.mode.clone(),
                Arc::clone(&self.approvals),
                Arc::clone(&self.ui),
                self.depth,
            )));
        }

        //    - `fetch`: local reqwest, no libertai dependency. Registers
        //      whenever the feature is on, regardless of cfg presence.
        if self.features.fetch {
            wrapped.push(Box::new(FetchTool::new()));
        }

        //    - `search` / `generate_image`: LibertAI-endpoint tools that
        //      need a libertai-cli `Config` carrier for the api_key /
        //      search_base. If the caller turned the feature on without
        //      supplying a Config we silently skip — failing here would
        //      surface as an opaque session-create error.
        if let Some(cfg) = self.libertai_cfg.as_ref() {
            if self.features.search {
                wrapped.push(Box::new(SearchTool::new(Arc::clone(cfg))));
            }
            if self.features.image {
                wrapped.push(Box::new(ImageGenTool::new(
                    Arc::clone(cfg),
                    Arc::new(cwd.to_path_buf()),
                )));
            }
        }

        ToolRegistry::from_tools(wrapped)
    }
}
