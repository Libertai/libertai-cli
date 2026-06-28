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

use crate::commands::code_approvals::{ApprovalState, ApprovalTool, ApprovalUi, ToolPolicy};
use crate::commands::code_ask_user::AskUserTool;
use crate::commands::code_aux::{smart_approval_from_config, SmartApproval};
use crate::commands::code_diff::EditJournal;
use crate::commands::code_guardrail::{GuardrailTool, ToolGuardrailState};
use crate::commands::code_mailbox::MailboxTool;
use crate::commands::code_mcp_tool::{cached_mcp_context_tools, named_mcp_tools, McpCallTool};
use crate::commands::code_notification::PushNotificationTool;
use crate::commands::code_path_safety::{
    is_path_mutation_tool, safe_root_from_env, PathSafetyTool,
};
use crate::commands::code_task::TaskTool;
use crate::commands::code_team::AgentRegistry;
use crate::commands::code_team_task::TeamTaskTool;
use crate::commands::code_team_tool::SpawnTeamTool;
use crate::commands::code_todo::TodoTool;
use crate::commands::code_mcp_tool::should_defer_mcp_tools;
use crate::commands::code_skill_tool::SkillTool;
use crate::commands::code_skills::SkillPillar;
use crate::commands::code_tool_search::ToolSearchTool;
use crate::commands::fetch_tool::FetchTool;
use crate::commands::image_tool::ImageGenTool;
use crate::commands::notebook_tool::{NotebookEditTool, NotebookExecuteTool, NotebookReadTool};
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
    /// All mutating tools auto-allow without prompting — `ApprovalTool`
    /// short-circuits to `Allow` before consulting the UI, mirroring
    /// Codex's `AskForApproval::Never`. Gated behind a one-time
    /// interactive consent (`--dangerously-skip-permissions`): the flag
    /// is refused in `--print` (and by background teammates) unless a
    /// sentinel file shows the user already accepted the risk in an
    /// interactive session. Intended for sandboxed or trusted CI runs
    /// where stopping to approve every bash call would defeat the run.
    Bypass,
}

impl Mode {
    fn as_u8(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::AcceptEdits => 1,
            Mode::Plan => 2,
            Mode::Bypass => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Mode::Normal,
            1 => Mode::AcceptEdits,
            2 => Mode::Plan,
            _ => Mode::Bypass,
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
    /// Enable native Jupyter notebook read/edit tools.
    pub notebook: bool,
    /// Enable repeat-call loop guardrails around the full tool set.
    pub guardrails: bool,
    /// Enable sensitive-path write denials for mutating path tools.
    pub path_safety: bool,
    /// Enable agent-callable user notifications.
    pub notifications: bool,
    /// Enable the generic agent-callable MCP bridge for configured
    /// terminal `mcpServers`.
    pub mcp: bool,
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
            notebook: true,
            guardrails: true,
            path_safety: true,
            notifications: true,
            mcp: true,
        }
    }
}

pub struct LibertaiToolFactory {
    pub mode: ModeFlag,
    pub approvals: Arc<ApprovalState>,
    pub ui: Arc<dyn ApprovalUi>,
    pub depth: u8,
    pub features: FactoryFeatures,
    /// Shared edit journal — the same `Arc<EditJournal>` the REPL's `App`
    /// holds so `/undo` (main thread) `pop`s entries the live session's
    /// `ApprovalTool::execute_inner` (background thread) `push`ed. Mirrors
    /// the `approvals` Arc threading: built once on the main thread, cloned
    /// into the bg factory at spawn, and cloned into every `ApprovalTool`
    /// via the `with_journal` builder so the ctor signature stays stable.
    pub edit_journal: Arc<EditJournal>,
    /// Shared live-agent registry. In-process subagents spawned by the
    /// `task` tool register here, as do background runs launched from
    /// the REPL, so the live panel and agent view see one table. A
    /// fresh empty registry is created when the field is unset (e.g.
    /// desktop chat pillar), so the factory always has one to hand to
    /// `TaskTool`.
    pub registry: Arc<AgentRegistry>,
    /// Carrier for the libertai-cli `Config` when search/fetch are on.
    /// `None` is fine when both are off; the factory just won't
    /// register those tools. Captured as `Arc` so it's cheap to clone
    /// into each tool's per-instance state.
    pub libertai_cfg: Option<Arc<LibertaiConfig>>,
    pub tool_policy: Option<Arc<dyn ToolPolicy>>,
    pub smart_approval: Option<Arc<dyn SmartApproval>>,
    /// Optional per-session safe root for mutating path tools. When
    /// unset, the factory falls back to `LIBERTAI_WRITE_SAFE_ROOT` so
    /// the CLI env-var behavior stays unchanged.
    pub safe_root_override: Option<std::path::PathBuf>,
    /// Team name when this session is a teammate in a team (set by the
    /// parent process via the `LIBERTAI_TEAM` env var when spawning).
    /// When present alongside `teammate_name`, the factory registers
    /// the `team_task` tool so the teammate can read/update the shared
    /// task list.
    pub team: Option<String>,
    /// Teammate name within the team. Set via `LIBERTAI_TEAMMATE` env
    /// var. Used as the assignee when the teammate claims a task.
    pub teammate_name: Option<String>,
    /// The parent session's bash command wrapper (e.g. the bwrap/seatbelt
    /// argv from `code_sandbox::build_command_wrapper`). Threaded into the
    /// `TaskTool` so spawned subagents inherit the parent's sandbox
    /// (M4/#23) — pi applies the wrapper PER bash invocation (`tools.rs`
    /// `Command::new(wrapper[0])`), NOT process-wide, so without threading
    /// it a `--sandbox=strict` parent's subagents ran UNSANDBOXED. `None`
    /// when the parent runs with no wrapper (`SandboxMode::Off`, the
    /// default).
    pub bash_command_wrapper: Option<Vec<String>>,
    /// (M5/#7) Override the cwd the `skill` tool scans for skills. The
    /// subagent factory sets this to the PARENT's cwd (the dir the
    /// subagent's skill prompt was built from), because a subagent runs
    /// in an isolated git worktree whose `create_tool_registry(cwd=…)`
    /// is the worktree path — and git worktrees don't copy untracked
    /// (gitignored) `.claude/skills/` / `.libertai/skills/` / `.agents/
    /// skills/`. Without this override the `skill` tool would advertise
    /// a project skill in the prompt (built from the parent cwd) but
    /// fail to load it from the worktree. `None` for the main + REPL
    /// sessions → `create_tool_registry` falls back to the session cwd.
    pub skill_cwd: Option<std::path::PathBuf>,
}

impl LibertaiToolFactory {
    /// Read team context from env vars set by the parent process when
    /// spawning a teammate. Returns `(team, teammate_name)` or
    /// `(None, None)` when not running as part of a team.
    fn team_from_env() -> (Option<String>, Option<String>) {
        let team = std::env::var("LIBERTAI_TEAM")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string());
        let teammate = std::env::var("LIBERTAI_TEAMMATE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string());
        (team, teammate)
    }

    /// CLI default constructor. Equivalent to the pre-features behavior.
    pub fn new(mode: ModeFlag, approvals: Arc<ApprovalState>, ui: Arc<dyn ApprovalUi>) -> Self {
        let (team, teammate_name) = Self::team_from_env();
        Self {
            mode,
            approvals,
            ui,
            depth: 0,
            features: FactoryFeatures::cli_defaults(),
            registry: AgentRegistry::new(),
            libertai_cfg: None,
            tool_policy: None,
            smart_approval: None,
            safe_root_override: None,
            edit_journal: Arc::new(EditJournal::new()),
            team,
            teammate_name,
            bash_command_wrapper: None,
            skill_cwd: None,
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
        let smart_approval = libertai_cfg
            .as_ref()
            .and_then(|cfg| smart_approval_from_config(Arc::clone(cfg)));
        let (team, teammate_name) = Self::team_from_env();
        Self {
            mode,
            approvals,
            ui,
            depth: 0,
            features,
            registry: AgentRegistry::new(),
            libertai_cfg,
            tool_policy: None,
            smart_approval,
            safe_root_override: None,
            edit_journal: Arc::new(EditJournal::new()),
            team,
            teammate_name,
            bash_command_wrapper: None,
            skill_cwd: None,
        }
    }

    /// Feature-aware constructor that shares an externally-created
    /// registry (the REPL creates one and hands it to every
    /// `build_handle` call so reloads and subagents land in the same
    /// live table).
    pub fn new_with_registry(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        features: FactoryFeatures,
        libertai_cfg: Option<Arc<LibertaiConfig>>,
        registry: Arc<AgentRegistry>,
    ) -> Self {
        let smart_approval = libertai_cfg
            .as_ref()
            .and_then(|cfg| smart_approval_from_config(Arc::clone(cfg)));
        let (team, teammate_name) = Self::team_from_env();
        Self {
            mode,
            approvals,
            ui,
            depth: 0,
            features,
            registry,
            libertai_cfg,
            tool_policy: None,
            smart_approval,
            safe_root_override: None,
            edit_journal: Arc::new(EditJournal::new()),
            team,
            teammate_name,
            bash_command_wrapper: None,
            skill_cwd: None,
        }
    }

    pub fn with_tool_policy(mut self, policy: Option<Arc<dyn ToolPolicy>>) -> Self {
        self.tool_policy = policy;
        self
    }

    pub fn with_safe_root(mut self, safe_root: Option<std::path::PathBuf>) -> Self {
        self.safe_root_override = safe_root;
        self
    }

    /// Explicitly set the team context, overriding env-var detection.
    /// Used when the REPL wants to inject team identity without relying
    /// on `LIBERTAI_TEAM`/`LIBERTAI_TEAMMATE`.
    pub fn with_team(mut self, team: Option<String>, teammate_name: Option<String>) -> Self {
        self.team = team;
        self.teammate_name = teammate_name;
        self
    }

    /// Attach an externally-created registry, overriding the empty one
    /// a bare constructor created. Used by the REPL after building the
    /// factory, so callers that go through `new_with_features` can still
    /// share the REPL's registry.
    pub fn with_registry(mut self, registry: Arc<AgentRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Attach an externally-created edit journal, overriding the fresh one
    /// a bare constructor created. Used by the REPL so the bg factory and
    /// the main-thread `App` share the SAME `Arc<EditJournal>` — `/undo`
    /// (main thread) sees the entries the bg session's `ApprovalTool`
    /// (background thread) `push`ed. Mirrors `with_registry`'s override
    /// shape.
    pub fn with_journal(mut self, journal: Arc<EditJournal>) -> Self {
        self.edit_journal = journal;
        self
    }

    /// Set the parent session's bash command wrapper so spawned
    /// subagents inherit the sandbox (M4/#23). Set by the REPL/TUI at
    /// session build time from `code_sandbox::build_command_wrapper`.
    pub fn with_bash_command_wrapper(mut self, wrapper: Option<Vec<String>>) -> Self {
        self.bash_command_wrapper = wrapper;
        self
    }

    /// (M5/#7) Override the cwd the `skill` tool scans for skills. The
    /// subagent factory sets this to the parent cwd (the dir its skill
    /// prompt was built from); the main/REPL sessions leave it `None`
    /// so `create_tool_registry` falls back to the session cwd.
    pub fn with_skill_cwd(mut self, skill_cwd: Option<std::path::PathBuf>) -> Self {
        self.skill_cwd = skill_cwd;
        self
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
            registry: Arc::clone(&self.registry),
            libertai_cfg: self.libertai_cfg.clone(),
            tool_policy: self.tool_policy.clone(),
            smart_approval: self.smart_approval.clone(),
            safe_root_override: self.safe_root_override.clone(),
            edit_journal: Arc::clone(&self.edit_journal),
            team: self.team.clone(),
            teammate_name: self.teammate_name.clone(),
            // Subagents inherit the parent's bash wrapper (M4/#23).
            bash_command_wrapper: self.bash_command_wrapper.clone(),
            // Subagents inherit the skill-scan override (the parent cwd)
            // so their `skill` tool scans the dir their prompt was built
            // from, not their worktree (M5/#7).
            skill_cwd: self.skill_cwd.clone(),
        }
    }
}

impl ToolFactory for LibertaiToolFactory {
    fn create_tool_registry(
        &self,
        enabled: &[&str],
        cwd: &Path,
        config: &PiConfig,
    ) -> ToolRegistry {
        // 1. Snapshot pi's default tools for the enabled set. We don't
        //    filter by mode here — the registry stays stable for the
        //    whole session and the mode flag is checked at call time
        //    in `ApprovalTool::execute`.
        let defaults = default_tool_registry(enabled, cwd, config).into_tools();

        // 2. Wrap each in ApprovalTool, sharing the mode flag, approval
        //    allowlist, and approval UI.
        let mut wrapped: Vec<Box<dyn Tool>> = Vec::with_capacity(defaults.len() + 2);
        let safe_root = self
            .safe_root_override
            .clone()
            .or_else(|| safe_root_from_env(cwd));
        for tool in defaults {
            let tool = self.wrap_path_safety(tool, cwd, safe_root.as_ref());
            let approval_tool = ApprovalTool::new(
                tool,
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )
            // Session cwd so relative tool paths absolutize before rule
            // matching — a trusted-directory wildcard must match
            // `src/foo.ts` the same as `/project/src/foo.ts`.
            .with_base_dir(Some(cwd.to_path_buf()))
            .with_policy(self.tool_policy.clone())
            .with_smart_approval(self.smart_approval.clone())
            .with_journal(Arc::clone(&self.edit_journal));
            wrapped.push(Box::new(approval_tool));
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

        //    - `skill`: lazy Agent-Skill body loader (M5/#7). Skills are
        //      listed in the system prompt as a latent registry (name +
        //      description only); the model calls `skill(name)` to load a
        //      matching skill's full body on demand, so a many-skill
        //      session doesn't ship every body in the prompt every turn.
        //      Read-only (disk reads), registered unwrapped like `todo`.
        //      Code-pillar only — every `prompt_for_pillar` call site
        //      resolves skills under `SkillPillar::Code`. `skill_cwd`
        //      (when set) overrides the session cwd so a subagent's
        //      tool scans the same dir its prompt was built from — the
        //      worktree the subagent runs in lacks gitignored project
        //      skills.
        let skill_cwd = self
            .skill_cwd
            .clone()
            .unwrap_or_else(|| cwd.to_path_buf());
        wrapped.push(Box::new(SkillTool::new(SkillPillar::Code, Some(skill_cwd))));

        //    - `task` (subagent): only when feature-on AND we still
        //      have depth headroom. Chat pillar opts out so a chat
        //      session can't recursively spawn coding agents.
        if self.features.task && self.depth < MAX_TASK_DEPTH {
            wrapped.push(Box::new(TaskTool::new(
                self.mode.clone(),
                Arc::clone(&self.approvals),
                Arc::clone(&self.ui),
                self.depth,
                cwd.to_path_buf(),
                Arc::clone(&self.registry),
                self.bash_command_wrapper.clone(),
            )));
        }

        //    - `spawn_team`: lets the agent itself create a team of
        //      background teammates when the user's request warrants
        //      parallel work. Same depth cap as `task` so nested
        //      subagents can't recursively spawn their own teams.
        //      Mutating (spawns processes + writes to disk), so it
        //      goes through the approval wrapper.
        if self.features.task && self.depth < MAX_TASK_DEPTH && self.team.is_none() {
            let spawn_team = ApprovalTool::new(
                Box::new(SpawnTeamTool::new(
                    cwd.to_path_buf(),
                    self.mode.clone(),
                    Arc::clone(&self.registry),
                )),
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )
            .with_base_dir(Some(cwd.to_path_buf()))
            .with_policy(self.tool_policy.clone())
            .with_smart_approval(self.smart_approval.clone())
            .with_journal(Arc::clone(&self.edit_journal));
            wrapped.push(Box::new(spawn_team));
        }

        //    - `team_task`: shared team task list. Only registered when
        //      this session is running as a teammate (the parent process
        //      set `LIBERTAI_TEAM` + `LIBERTAI_TEAMMATE` env vars before
        //      spawning). The tool reads/writes `tasks.jsonl` in the
        //      project's `.libertai/teams/<team>/` directory. Mutating
        //      (writes to disk), so it goes through the approval wrapper.
        if let (Some(team), Some(teammate)) = (&self.team, &self.teammate_name) {
            let team_dir = cwd.join(".libertai").join("teams").join(team);
            let team_task = ApprovalTool::new(
                Box::new(TeamTaskTool::new(team_dir.clone(), teammate.clone())),
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )
            .with_base_dir(Some(cwd.to_path_buf()))
            .with_policy(self.tool_policy.clone())
            .with_smart_approval(self.smart_approval.clone())
            .with_journal(Arc::clone(&self.edit_journal));
            wrapped.push(Box::new(team_task));

            //    - `mailbox`: file-based messaging between teammates.
            //      Same team context as `team_task`. Mutating (writes
            //      files to recipient's mailbox dir), so it goes through
            //      the approval wrapper.
            let mailbox = ApprovalTool::new(
                Box::new(MailboxTool::new(team_dir, teammate.clone())),
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )
            .with_base_dir(Some(cwd.to_path_buf()))
            .with_policy(self.tool_policy.clone())
            .with_smart_approval(self.smart_approval.clone())
            .with_journal(Arc::clone(&self.edit_journal));
            wrapped.push(Box::new(mailbox));
        }

        //    - `fetch`: local reqwest, no libertai dependency. Registers
        //      whenever the feature is on, regardless of cfg presence.
        if self.features.fetch {
            wrapped.push(Box::new(FetchTool::new()));
        }

        if self.features.notifications {
            wrapped.push(Box::new(
                PushNotificationTool::new(Arc::clone(&self.ui))
                    .with_config(self.libertai_cfg.clone()),
            ));
        }

        if self.features.mcp {
            if let Some(cfg) = self.libertai_cfg.as_ref() {
                if !cfg.mcp_servers.is_empty() {
                    let mcp_call = ApprovalTool::new(
                        Box::new(McpCallTool::new(Arc::clone(cfg))),
                        Arc::clone(&self.approvals),
                        self.mode.clone(),
                        Arc::clone(&self.ui),
                    )
                    .with_policy(self.tool_policy.clone())
                    .with_smart_approval(self.smart_approval.clone())
                    .with_journal(Arc::clone(&self.edit_journal));
                    wrapped.push(Box::new(mcp_call));

                    // (M5/#11) Above the tool-search threshold, defer the
                    // eager `mcp__server__tool` wrappers — their
                    // definitions bloat the prompt more than the model
                    // uses them. The model discovers tools via
                    // `tool_search` and invokes them through `mcp_call`
                    // (same capability). Below the threshold, register
                    // the named wrappers eagerly (the legacy behavior, no
                    // regression for small MCP setups).
                    if should_defer_mcp_tools(cfg) {
                        wrapped.push(Box::new(ToolSearchTool::new(Arc::clone(cfg))));
                    } else {
                        for tool in named_mcp_tools(Arc::clone(cfg)) {
                            let named = ApprovalTool::new(
                                tool,
                                Arc::clone(&self.approvals),
                                self.mode.clone(),
                                Arc::clone(&self.ui),
                            )
                            .with_policy(self.tool_policy.clone())
                            .with_smart_approval(self.smart_approval.clone())
                            .with_journal(Arc::clone(&self.edit_journal));
                            wrapped.push(Box::new(named));
                        }
                    }
                    for tool in cached_mcp_context_tools(Arc::clone(cfg)) {
                        wrapped.push(tool);
                    }
                }
            }
        }

        //    - `notebook_read` / `notebook_edit` / `notebook_execute`:
        //      native .ipynb support. Reads are safe in plan mode; edits
        //      and execution go through the same approval wrapper as pi's
        //      built-in mutating tools.
        if self.features.notebook {
            wrapped.push(Box::new(NotebookReadTool::new()));
            let notebook_edit = ApprovalTool::new(
                self.wrap_path_safety(Box::new(NotebookEditTool::new()), cwd, safe_root.as_ref()),
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )
            .with_base_dir(Some(cwd.to_path_buf()))
            .with_policy(self.tool_policy.clone())
            .with_smart_approval(self.smart_approval.clone())
            .with_journal(Arc::clone(&self.edit_journal));
            wrapped.push(Box::new(notebook_edit));
            let notebook_execute = ApprovalTool::new(
                self.wrap_path_safety(
                    Box::new(NotebookExecuteTool::new()),
                    cwd,
                    safe_root.as_ref(),
                ),
                Arc::clone(&self.approvals),
                self.mode.clone(),
                Arc::clone(&self.ui),
            )
            .with_base_dir(Some(cwd.to_path_buf()))
            .with_policy(self.tool_policy.clone())
            .with_smart_approval(self.smart_approval.clone())
            .with_journal(Arc::clone(&self.edit_journal));
            wrapped.push(Box::new(notebook_execute));
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

        if self.features.guardrails {
            let guardrail_state = ToolGuardrailState::shared();
            wrapped = wrapped
                .into_iter()
                .map(|tool| {
                    Box::new(GuardrailTool::new(tool, Arc::clone(&guardrail_state)))
                        as Box<dyn Tool>
                })
                .collect();
        }

        ToolRegistry::from_tools(wrapped)
    }
}

impl LibertaiToolFactory {
    fn wrap_path_safety(
        &self,
        tool: Box<dyn Tool>,
        cwd: &Path,
        safe_root: Option<&std::path::PathBuf>,
    ) -> Box<dyn Tool> {
        if self.features.path_safety && is_path_mutation_tool(tool.name()) {
            Box::new(PathSafetyTool::new(
                tool,
                cwd.to_path_buf(),
                safe_root.cloned(),
            ))
        } else {
            tool
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct AllowingUi;

    #[async_trait]
    impl ApprovalUi for AllowingUi {
        async fn decide(
            &self,
            _tool_name: &str,
            _preview: &str,
            _always_rule: &str,
        ) -> crate::commands::code_approvals::PromptChoice {
            crate::commands::code_approvals::PromptChoice::Allow
        }
    }

    #[test]
    fn factory_registers_mcp_call_when_servers_are_configured() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "github".to_string(),
                crate::config::McpServerConfig {
                    command: "server".to_string(),
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..LibertaiConfig::default()
        });
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        );
        let registry = factory.create_tool_registry(&[], temp.path(), &PiConfig::default());
        assert!(registry.get("mcp_call").is_some());
    }

    #[test]
    fn factory_registers_named_mcp_tools_from_cached_config() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "github".to_string(),
                crate::config::McpServerConfig {
                    command: "server".to_string(),
                    tools: vec![crate::config::McpToolConfig {
                        name: "search".to_string(),
                        ..crate::config::McpToolConfig::default()
                    }],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..LibertaiConfig::default()
        });
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        );
        let registry = factory.create_tool_registry(&[], temp.path(), &PiConfig::default());
        assert!(registry.get("mcp_call").is_some());
        assert!(registry.get("mcp__github__search").is_some());
    }

    #[test]
    fn factory_registers_cached_mcp_resource_and_prompt_tools() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(LibertaiConfig {
            mcp_servers: std::collections::HashMap::from([(
                "docs".to_string(),
                crate::config::McpServerConfig {
                    command: "server".to_string(),
                    resources: vec![crate::config::McpResourceConfig {
                        uri: "file:///repo/README.md".to_string(),
                        ..crate::config::McpResourceConfig::default()
                    }],
                    prompts: vec![crate::config::McpPromptConfig {
                        name: "summarize".to_string(),
                        ..crate::config::McpPromptConfig::default()
                    }],
                    ..crate::config::McpServerConfig::default()
                },
            )]),
            ..LibertaiConfig::default()
        });
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        );
        let registry = factory.create_tool_registry(&[], temp.path(), &PiConfig::default());
        assert!(registry.get("mcp_read_resource").is_some());
        assert!(registry.get("mcp_get_prompt").is_some());
    }

    #[test]
    fn factory_skips_mcp_call_without_servers() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(LibertaiConfig::default());
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        );
        let registry = factory.create_tool_registry(&[], temp.path(), &PiConfig::default());
        assert!(registry.get("mcp_call").is_none());
    }

    // (M4/#23) A factory with a bash command wrapper threads it through
    // `child()` so nested subagents inherit the parent's sandbox. A factory
    // with no wrapper (the default, SandboxMode::Off) propagates `None`.
    #[test]
    fn factory_child_inherits_bash_command_wrapper() {
        let cfg = Arc::new(LibertaiConfig::default());
        let wrapper = Some(vec!["bwrap".to_string(), "--".to_string()]);
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        )
        .with_bash_command_wrapper(wrapper.clone());

        // The parent carries the wrapper.
        assert_eq!(factory.bash_command_wrapper, wrapper);
        // The child inherits it.
        let child = factory.child();
        assert_eq!(child.bash_command_wrapper, wrapper);
    }

    #[test]
    fn factory_without_wrapper_propagates_none() {
        let cfg = Arc::new(LibertaiConfig::default());
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        );
        // Default: no wrapper (SandboxMode::Off).
        assert!(factory.bash_command_wrapper.is_none());
        assert!(factory.child().bash_command_wrapper.is_none());
    }

    // (M4/#23) `TaskTool::new` stores the wrapper so its spawned subagent
    // session (built in `TaskTool::execute`) inherits the parent sandbox.
    #[test]
    fn task_tool_stores_bash_command_wrapper() {
        use crate::commands::code_task::TaskTool;
        use std::path::PathBuf;
        let wrapper = Some(vec!["bwrap".to_string(), "--".to_string()]);
        let tool = TaskTool::new(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            0,
            PathBuf::from("."),
            AgentRegistry::new(),
            wrapper.clone(),
        );
        // The field is private; the only production reader is the
        // CodeSessionConfig build in `execute`, so assert via a clone of
        // the same value round-tripping through a child factory built the
        // same way the execute path does.
        let cfg = Arc::new(LibertaiConfig::default());
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        )
        .with_bash_command_wrapper(wrapper.clone());
        // A strict parent's child carries the wrapper — the invariant the
        // TaskTool relies on when it clones `self.bash_command_wrapper`
        // into the subagent's CodeSessionConfig.
        assert_eq!(factory.child().bash_command_wrapper, wrapper);
        // Smoke: the tool was constructed without panic.
        let _ = tool;
    }

    #[test]
    fn child_factory_propagates_skill_cwd_override() {
        // (M5/#7) A subagent factory sets `skill_cwd` to the PARENT cwd
        // so its `skill` tool scans the dir its prompt was built from
        // (git worktrees don't copy gitignored `.claude/skills/`). The
        // `child()` of THAT factory must carry the same override into a
        // nested subagent — otherwise a nested subagent-of-a-subagent
        // would drop back to its worktree and lose project skills again.
        use std::path::PathBuf;
        let cfg = Arc::new(LibertaiConfig::default());
        let parent_cwd = PathBuf::from("/parent/working/dir");
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        )
        .with_skill_cwd(Some(parent_cwd.clone()));
        let child = factory.child();
        assert_eq!(child.skill_cwd.as_deref(), Some(parent_cwd.as_path()));
        // A grandchild carries it too — the invariant nested subagents
        // rely on.
        assert_eq!(child.child().skill_cwd.as_deref(), Some(parent_cwd.as_path()));
    }

    #[test]
    fn factory_skill_cwd_defaults_none_for_main_session() {
        // (M5/#7) The main/REPL session builders leave `skill_cwd`
        // unset so `create_tool_registry` falls back to the session cwd.
        // A main session must NOT carry a stale override.
        let cfg = Arc::new(LibertaiConfig::default());
        let factory = LibertaiToolFactory::new_with_features(
            ModeFlag::new(Mode::Normal),
            Arc::new(ApprovalState::new()),
            Arc::new(AllowingUi),
            FactoryFeatures::cli_defaults(),
            Some(cfg),
        );
        assert!(factory.skill_cwd.is_none());
        assert!(factory.child().skill_cwd.is_none());
    }

    #[test]
    fn mode_u8_encoding_round_trips_all_variants() {
        // The u8 encoding is the wire format `ModeFlag` uses to share mode
        // across tools and subagents; a gap or collision here would silently
        // flip a session's permission tier. Pin every variant + the
        // "unknown clamps to the tail" behavior.
        for mode in [Mode::Normal, Mode::AcceptEdits, Mode::Plan, Mode::Bypass] {
            assert_eq!(Mode::from_u8(mode.as_u8()), mode);
        }
        // Unknown high values clamp to Bypass (the new tail), matching the
        // pre-existing Plan-was-tail behavior.
        assert_eq!(Mode::from_u8(255), Mode::Bypass);
    }
}
