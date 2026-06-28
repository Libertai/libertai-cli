//! The `task` tool — our subagent / "Task" feature.
//!
//! Spawns a fresh `AgentSession` with an allowlisted, approval-wrapped
//! tool set (default: read/grep/find/ls only — the subagent is a
//! research helper, not a second hand on the keyboard). Inherits the
//! parent's [`ApprovalState`] so a tool the user already "always
//! allowed" doesn't re-prompt inside the child.
//!
//! Recursion is bounded by [`MAX_TASK_DEPTH`] in `code_factory.rs`:
//! once we're at the cap, the parent factory stops registering `task`,
//! so the agent sees it as unavailable and cannot chain further.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{
    create_agent_session, AbortHandle, AgentEvent, Result as PiResult, Tool, ToolExecution,
    ToolOutput, ToolUpdate,
};

use crate::commands::code_agents;
use crate::commands::code_approvals::{ApprovalState, ApprovalUi};
use crate::commands::code_factory::{LibertaiToolFactory, ModeFlag};
use crate::commands::code_session::{build_session_options, CodeSessionConfig, SessionPersistence};
use crate::commands::code_skills::{self, SkillPillar};
use crate::commands::code_team::{AgentHandle, AgentKind, AgentRegistry, AgentStatus};
use crate::config;

const NAME: &str = "task";
const LABEL: &str = "Task";
const DESCRIPTION: &str = concat!(
    "Run a focused subtask in an isolated agent session. Use when a ",
    "piece of research or a narrow lookup is well-defined and should ",
    "not clutter the main conversation. By default the child runs with ",
    "a read-only tool set (read, grep, find, ls); a named sub-agent's ",
    "`tools:` frontmatter can opt in to mutating tools (write, edit, ",
    "bash, …), in which case the child defaults to worktree isolation ",
    "and still goes through the normal approval flow."
);

/// Tools a subagent gets when no `tools:` frontmatter and no `tools`
/// argument narrow the set. This is a *default*, not a hard ceiling: a
/// named agent definition that lists `write`/`edit`/`bash` in its
/// frontmatter is honored (see M1.3), so a subagent can be made
/// write-capable. Worktree isolation and the shared approval state
/// remain the guardrails for write-capable subagents.
const TASK_DEFAULT_TOOLS: &[&str] = &["read", "grep", "find", "ls"];

pub struct TaskTool {
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    parent_depth: u8,
    cwd: PathBuf,
    registry: Arc<AgentRegistry>,
    /// The parent session's bash command wrapper, inherited by spawned
    /// subagents (M4/#23). `None` when the parent runs unsandboxed.
    bash_command_wrapper: Option<Vec<String>>,
}

/// (R4HUNT-1) RAII guard for an INLINE subagent's registry + abort
/// lifetime, held across the `prompt_with_abort` await. The parent's
/// abort (Ctrl+C / Esc / shared_abort) or any panic drops the
/// `prompt_with_abort` future (pi's `select(all_fut, abort_fut)` returns on
/// `Either::Right`/abort and DROPS `all_fut`, which holds the `execute`
/// future) BEFORE the manual cleanup arms in `execute` run. Without this
/// guard, that drop left the registry entry stuck at
/// `AgentStatus::Working` with `pid: None` — `poll_agent_status` SKIPS
/// `pid: None` handles (so it's NEVER reaped) and the agents panel showed a
/// stuck "working" subagent FOREVER, occupying the abort slot.
///
/// (R5-HUNT-B) The guard no longer owns the log-file lifetime — the SUCCESS,
/// FAILURE, and ABORT paths ALL defer log deletion to session teardown
/// (`cleanup_subagent_logs`, made panic-safe by R5-HUNT-A) so an overlay
/// already open on the subagent keeps its final output / failure reason /
/// partial output. The guard now reaps only the registry entry + abort slot
/// + status on the abort-drop path; the log file is left for teardown.
///
/// Held as `let _guard = SubagentGuard { ... }` AFTER `register` and ACROSS
/// the `prompt_with_abort(...).await`. Its [`Drop`] is idempotent via
/// `cleaned`: the explicit return arms in `execute` (Ok + Err) set
/// `guard.cleaned = true` AFTER their own `take_abort`/`set_status`/`remove`
/// (NEITHER arm removes the log — both defer it to session teardown per
/// R4HUNT-3 / R5-HUNT-B, so an overlay already open on the subagent keeps its
/// final output + failure reason), so on the normal path Drop is a NO-OP. On
/// the abort-drop path (`cleaned` still `false`) Drop fires the cleanup the
/// skipped arms would have: `registry.remove` + `take_abort` (ignore Err) +
/// `set_status(Failed)` (best-effort, only if still active) — but, per
/// R5-HUNT-B, it does NOT remove the log file (deferred to teardown too, so an
/// overlay open on an ABORTED subagent keeps the partial output). The log is
/// swept later by `cleanup_subagent_logs` (made panic-safe by R5-HUNT-A).
///
/// Borrows the registry via a cloned `Arc<AgentRegistry>` — `self.registry`
/// is already `Arc<AgentRegistry>` (thread-shared through the tool factory
/// for `poll_agent_status`'s snapshot path), so the guard owns its own
/// `Arc` clone and the registry stays reachable from a moved guard without
/// borrowing across the await (a `&'a AgentRegistry` can't span the await).
struct SubagentGuard {
    handle: Arc<AgentHandle>,
    registry: Arc<AgentRegistry>,
    cleaned: bool,
}

impl SubagentGuard {
    /// Build the guard AFTER `register`. The guard borrows nothing from the
    /// caller's stack — it owns its `Arc` clones — so it's safe to hold
    /// across the await + drop out of order.
    fn new(handle: Arc<AgentHandle>, registry: Arc<AgentRegistry>) -> Self {
        Self {
            handle,
            registry,
            cleaned: false,
        }
    }
}

impl Drop for SubagentGuard {
    fn drop(&mut self) {
        // No-op on the normal path: the explicit return arms set `cleaned`
        // after running their own cleanup, so Drop never re-enters here.
        if self.cleaned {
            return;
        }
        // Abort/panic-drop path: the `execute` future was dropped before its
        // manual arms ran. Reap everything the skipped arms would have, so the
        // subagent doesn't leak as a stuck "working" entry with an orphaned
        // registry slot + abort handle. (R5-HUNT-B) The LOG file is NOT removed
        // here — like the SUCCESS + FAILURE arms it is deferred to session
        // teardown (cleanup_subagent_logs, made panic-safe by R5-HUNT-A) so an
        // overlay ALREADY OPEN on an aborted subagent keeps its partial output
        // instead of going blank the moment the parent aborts it. The old code
        // deleted the log here immediately, blanking any open overlay.
        // Take the abort slot so a dropped mid-run agent can't be re-aborted
        // later; ignore the (already-None) Err — the whole point is recovery.
        let _ = self.handle.take_abort();
        // Only flip to Failed if still active — best-effort, mirrors the
        // explicit arms' set_status(Failed). `set_status` is poison-recovered
        // (see code_team.rs), so this never panics the dropping thread.
        if self.handle.status().is_active() {
            self.handle.set_status(AgentStatus::Failed);
        }
        self.registry.remove(self.handle.id);
    }
}

impl TaskTool {
    pub fn new(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        parent_depth: u8,
        cwd: PathBuf,
        registry: Arc<AgentRegistry>,
        bash_command_wrapper: Option<Vec<String>>,
    ) -> Self {
        Self {
            mode,
            approvals,
            ui,
            parent_depth,
            cwd,
            registry,
            bash_command_wrapper,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The subtask description the child agent will work on."
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional subset of tool names to enable (defaults to read, grep, find, ls)."
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Optional named sub-agent from .claude/agents or .libertai/agents."
                },
                "worktree": {
                    "type": "boolean",
                    "description": "When true, run the child in a temporary isolated workspace. Git repositories use a detached worktree at HEAD; non-git directories use a copied workspace snapshot."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["same-cwd", "worktree"],
                    "description": "Optional isolation mode. `worktree` is equivalent to worktree=true."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let prompt = match input.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(err_output("task tool requires a `prompt` string argument"));
            }
        };
        let subagent_type = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let requested_worktree = task_wants_worktree(&input);
        let requested_same_cwd = task_wants_same_cwd(&input);
        let agent = match subagent_type {
            Some(name) => match code_agents::find_agent(&self.cwd, name) {
                Ok(Some(agent)) => Some(agent),
                Ok(None) => {
                    let available = code_agents::agent_names(&self.cwd).unwrap_or_default();
                    let suffix = if available.is_empty() {
                        "no named sub-agents are configured".to_string()
                    } else {
                        format!("available sub-agents: {}", available.join(", "))
                    };
                    return Ok(err_output(&format!(
                        "task: unknown subagent_type `{name}` ({suffix})"
                    )));
                }
                Err(e) => return Ok(err_output(&format!("task: could not load agents: {e:#}"))),
            },
            None => None,
        };

        // Resolve the child's tool set. A named agent definition's
        // `tools:` frontmatter is the ceiling — honored fully, so a
        // definition can opt a subagent into write/edit/bash (M1.3).
        // With no `tools:` frontmatter the ceiling is the read-only
        // `TASK_DEFAULT_TOOLS`, preserving the historical safety
        // default. The caller's `tools` argument intersects with the
        // ceiling; a missing/empty argument falls back to the ceiling,
        // and an all-filtered-out argument falls back to the ceiling
        // too (so the child always has at least the read-only set).
        let requested: Vec<String> = input
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let ceiling: Vec<String> = agent
            .as_ref()
            .and_then(|a| a.tools.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| TASK_DEFAULT_TOOLS.iter().map(|&s| s.to_string()).collect());
        let filtered: Vec<String> = if requested.is_empty() {
            ceiling.clone()
        } else {
            let f: Vec<String> = requested
                .into_iter()
                .filter(|name| ceiling.iter().any(|allowed| allowed == name))
                .collect();
            if f.is_empty() {
                ceiling
            } else {
                f
            }
        };

        // Load our own Config — subtask runs against the same LibertAI
        // endpoint + model the parent is on. `code_models.rs` has
        // already registered libertai in pi's custom provider table by
        // the time this fires from a running parent session.
        let cfg = match config::load() {
            Ok(c) => c,
            Err(e) => {
                return Ok(err_output(&format!(
                    "task: could not load libertai config: {e}"
                )));
            }
        };

        // Child factory: shared mode flag + shared approval state, but
        // with parent_depth + 1 so deeper nesting hits the recursion
        // cap. `LibertaiToolFactory::child` is the one place that
        // increments depth.
        //
        // Subagents are research helpers (read-only TASK_TOOL_ALLOWLIST
        // above), so we turn `image` off — image generation is mutating
        // and out of scope for a research subagent. Search and local
        // fetch stay on; both are read-only and useful for lookup.
        let mut features = crate::commands::code_factory::FactoryFeatures::cli_defaults();
        features.image = false;
        let cfg = Arc::new(cfg.clone());
        let factory = LibertaiToolFactory {
            mode: self.mode.clone(),
            approvals: Arc::clone(&self.approvals),
            ui: Arc::clone(&self.ui),
            depth: self.parent_depth,
            features,
            registry: Arc::clone(&self.registry),
            libertai_cfg: Some(Arc::clone(&cfg)),
            tool_policy: None,
            smart_approval: crate::commands::code_aux::smart_approval_from_config(Arc::clone(&cfg)),
            safe_root_override: None,
            // Subagents run in an isolated worktree (see below), so they
            // get their own edit journal rather than sharing the parent
            // session's — `/undo` on the parent reverts parent-session
            // edits only, not subagent worktree mutations.
            edit_journal: Arc::new(crate::commands::code_diff::EditJournal::new()),
            team: None,
            teammate_name: None,
            // (M4/#23) Inherit the parent's bash wrapper so a nested
            // subagent-of-a-subagent stays sandboxed too. `child()` below
            // propagates this onward.
            bash_command_wrapper: self.bash_command_wrapper.clone(),
            // (M5/#7) The subagent's skill prompt is built from `self.cwd`
            // (the parent's working dir — see the prompt_for_pillar call
            // above), but the subagent RUNS in an isolated git worktree
            // whose `create_tool_registry(cwd=…)` is the worktree path.
            // Git worktrees don't copy gitignored `.claude/skills/` etc.,
            // so the `skill` tool would otherwise advertise a project
            // skill in the prompt but fail to load it from the worktree.
            // Point the tool at the parent cwd so it scans the same dir
            // the prompt was built from. `child()` propagates this to
            // nested subagents too.
            skill_cwd: Some(self.cwd.clone()),
            // (M5/#16) Subagents don't share the parent TUI's context
            // snapshot (it's updated by the parent's `Usage` handler,
            // which the subagent's own session never feeds). So the
            // `context_status` / `request_compaction` tools aren't
            // registered on subagents — they're a main-session affordance.
            context_snapshot: None,
            // (M5/#17) Subagents don't host a cron timer (only the
            // parent TUI does); leave the store unset so the cron tools
            // aren't registered on subagents.
            cron_store: None,
            // (M6/#15) Subagents don't host a workflow registry (only the
            // parent TUI does); leave unset so the WorkflowTool isn't
            // registered on subagents. Workflows spawn their own phase
            // agents; nesting a workflow inside a subagent would blow
            // past MAX_TASK_DEPTH.
            workflows: None,
        }
        .child();

        // Worktree isolation: explicit `same-cwd` wins (no isolation),
        // then an explicit `worktree` request, then the definition's
        // `worktree:` frontmatter. As a safety default (M1.3), a
        // write-capable subagent — one whose resolved tools include
        // write/edit/bash — defaults into a worktree even when the
        // definition didn't set `worktree: true`, so its mutations land
        // in an isolated checkout rather than the live working copy.
        let capability = crate::commands::code_team::AgentCapability::from_tools(&filtered);
        let is_write_capable = !matches!(
            capability,
            crate::commands::code_team::AgentCapability::ReadOnly
        );
        let wants_worktree = if requested_same_cwd {
            false
        } else {
            requested_worktree || agent.as_ref().is_some_and(|a| a.worktree) || is_write_capable
        };
        let max_tokens = Some(crate::commands::code_session::DEFAULT_MAX_TOKENS);
        let worktree = if wants_worktree {
            match TaskWorktree::create(&self.cwd) {
                Ok(worktree) => Some(worktree),
                Err(e) => {
                    return Ok(err_output(&format!(
                        "task: could not create isolated worktree: {e:#}"
                    )));
                }
            }
        } else {
            None
        };
        let child_cwd = worktree
            .as_ref()
            .map(|w| w.path.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let mut append_parts = Vec::new();
        if let Ok(Some(skills)) = code_skills::prompt_for_pillar(SkillPillar::Code, Some(&self.cwd))
        {
            append_parts.push(skills);
        }
        if let Some(agent) = agent.as_ref() {
            append_parts.push(named_subagent_prompt(agent));
        }
        let append_system_prompt = if append_parts.is_empty() {
            None
        } else {
            Some(append_parts.join("\n\n"))
        };
        let append_system_prompt =
            crate::commands::code_identity_prompt::apply(append_system_prompt);
        // Git context is injected once by pi (build_git_context); do not duplicate it here.
        let model = agent
            .as_ref()
            .and_then(|a| a.model.clone())
            .unwrap_or_else(|| cfg.default_code_model.clone());
        // Capture for the registry registration below — `model` is
        // moved into `CodeSessionConfig` on the next line.
        let model_for_handle = model.clone();
        let options = build_session_options(CodeSessionConfig {
            provider: cfg.default_code_provider.clone(),
            model,
            working_directory: Some(child_cwd.clone()),
            include_cwd_in_prompt: true,
            max_tool_iterations: 25,
            tool_factory: Arc::new(factory),
            // Subagents are nested scratch sessions — their JSONL would
            // pollute the user-facing session list with noise.
            persistence: SessionPersistence::Ephemeral,
            enabled_tools: Some(filtered),
            append_system_prompt,
            max_tokens,
            // (M4/#23) Subagents inherit the parent's bash command wrapper.
            // pi applies the wrapper PER bash invocation
            // (`tools.rs Command::new(wrapper[0])`), not process-wide, so
            // the prior "the outer bwrap already wraps nested calls"
            // assumption was false — a `--sandbox=strict` parent's
            // subagents ran UNSANDBOXED. Thread the parent's wrapper here.
            bash_command_wrapper: self.bash_command_wrapper.clone(),
            auto_compaction_enabled: cfg.code_auto_compaction_enabled,
            compaction_reserve_tokens: cfg.code_compaction_reserve_tokens,
            compaction_keep_recent_tokens: cfg.code_compaction_keep_recent_tokens,
        });

        if let Some(agent) = agent.as_ref() {
            eprintln!(
                "\n  \x1b[2m[subagent:{}] running: {prompt}\x1b[0m",
                agent.name
            );
        } else if wants_worktree {
            eprintln!("\n  \x1b[2m[subagent] running in isolated workspace: {prompt}\x1b[0m");
        } else {
            eprintln!("\n  \x1b[2m[subagent] running: {prompt}\x1b[0m");
        }

        // Register the subagent in the shared live registry so the
        // panel and agent view can show it while it runs. In-process
        // subagents are ephemeral: the handle is removed on return.
        let display_name = agent
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "subagent".to_string());
        let prompt_preview: String = prompt.chars().take(80).collect();
        // (R4-1) Tee the subagent's streamed text to a per-subagent log
        // file (mirroring background agents / teammates) so the overlay
        // reads via `read_agent_log_cached` and survives the 5000-entry
        // transcript ring evicting the subagent's earliest entries on a
        // long session. Without this, an in-process subagent registered
        // with `log_path: None` falls through to `agent_transcript_from_
        // memory` which scans the CAPPED ring — once trimmed, a STILL-
        // running subagent's overlay shows truncated/empty history. The
        // file is created here (secure, 0600) and appended to in the
        // `Subagent*` arms of `handle_agent_msg` on the main thread
        // (between draws — satisfies the out-of-band-write constraint).
        // A creation failure is non-fatal: we fall back to `log_path:
        // None` (the prior in-memory-only path) so the turn still runs.
        let subagent_log_path = create_subagent_log_file(&display_name).ok();
        let handle_arc = self
            .registry
            .register(crate::commands::code_team::AgentRegistration {
                name: display_name.clone(),
                kind: AgentKind::Subagent {
                    depth: self.parent_depth,
                    parent: None,
                },
                color: agent.as_ref().and_then(|a| a.color).unwrap_or_else(|| {
                    crate::commands::code_team::AgentColor::color_for_name(&display_name)
                }),
                capability,
                cwd: child_cwd.clone(),
                model: model_for_handle,
                prompt_preview,
                parent: None,
                pid: None,
                log_path: subagent_log_path.clone(),
            });

        let mut handle = match create_agent_session(options).await {
            Ok(h) => {
                handle_arc.set_status(AgentStatus::Working);
                h
            }
            Err(e) => {
                handle_arc.set_status(AgentStatus::Failed);
                if let Some(p) = &subagent_log_path {
                    remove_subagent_log_file(p);
                }
                self.registry.remove(handle_arc.id);
                return Ok(err_output(&format!("task: session init failed: {e}")));
            }
        };
        handle.set_max_tokens(max_tokens);

        let child_updates: Option<Arc<dyn Fn(ToolUpdate) + Send + Sync>> = on_update.map(Arc::from);
        let handle_for_render = Arc::clone(&handle_arc);
        let name_for_render = display_name.clone();
        let render = {
            let child_updates = child_updates.clone();
            move |event: AgentEvent| {
                update_handle_from_event(&handle_for_render, &event);
                render_child(event, child_updates.as_deref(), &name_for_render)
            }
        };

        // Create the abort pair up front so the main thread can stop this
        // child mid-run. The handle lives on the shared `AgentHandle` (so
        // the TUI's stop command can reach it) and the signal goes to the
        // child prompt below; `take`ing it on completion (both branches)
        // guarantees a finished agent can't be aborted afterward.
        let (abort_handle, abort_signal) = AbortHandle::new();
        handle_arc.set_abort(abort_handle);

        // (R4HUNT-1) Hold the SubagentGuard ACROSS the prompt_with_abort
        // await. If the parent aborts (Ctrl+C / Esc / shared_abort) or the
        // future panics, pi's `select(all_fut, abort_fut)` returns on the
        // abort and DROPS all_fut (holding this `execute` future) BEFORE
        // the manual cleanup arms below run. The guard's Drop reaps the
        // registry entry + abort slot + Failed status so the
        // subagent doesn't leak as a stuck "working" entry (poll_agent_
        // status skips pid:None handles, so an unreaped one stays forever).
        // The explicit arms below set `guard.cleaned = true` after their
        // own cleanup, so on the normal path Drop is a no-op. Built here
        // (after register + set_abort) so it owns the live handle Arc +
        // a registry Arc clone; dropped at the end of `execute`. (R5-HUNT-B)
        // The guard no longer owns the log path — log deletion is deferred
        // to session teardown on ALL paths (SUCCESS/FAILURE/ABORT) so an
        // overlay open on the subagent keeps its output.
        let mut guard = SubagentGuard::new(Arc::clone(&handle_arc), Arc::clone(&self.registry));

        let assistant = match handle.prompt_with_abort(prompt, abort_signal, render).await {
            Ok(msg) => {
                let _ = handle_arc.take_abort();
                handle_arc.set_status(AgentStatus::Completed);
                // (R4HUNT-3) SUCCESS path: remove from the registry (the
                // overlay can't be RE-opened after completion) but DEFER the
                // log-file deletion to session teardown so the already-open
                // overlay keeps its final output (it reads via the path
                // stored on the AgentOverlay, surviving registry.remove).
                self.registry.remove(handle_arc.id);
                guard.cleaned = true;
                msg
            }
            Err(e) => {
                let _ = handle_arc.take_abort();
                handle_arc.set_status(AgentStatus::Failed);
                // (R5-HUNT-B) FAILURE path: like the SUCCESS path (R4HUNT-3),
                // remove from the registry (the overlay can't be RE-opened
                // after failure) but DEFER the log-file deletion to session
                // teardown so an overlay ALREADY OPEN on this subagent keeps
                // its final output AND the failure reason (it reads via the
                // path stored on the AgentOverlay, surviving registry.remove).
                // The old code deleted the log here immediately, blanking any
                // overlay open on a subagent that then FAILED — the user lost
                // the final output + the error. Teardown's
                // cleanup_subagent_logs (made panic-safe by R5-HUNT-A) sweeps
                // it later.
                self.registry.remove(handle_arc.id);
                guard.cleaned = true;
                return Ok(err_output(&format!("task: run failed: {e}")));
            }
        };
        // Ok fall-through (non-early-return): the Success arm above already
        // ran take_abort + set_status(Completed) + registry.remove + marked
        // the guard cleaned. Nothing more to do — the guard drops as a
        // no-op. (The log is intentionally NOT removed here; see R4HUNT-3.)
        let _ = guard;

        // Collapse the child assistant's text blocks into a single
        // string; tool-call / thinking blocks are dropped (the parent
        // doesn't need to see the child's internal moves).
        let text = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
        }
        .into())
    }

    fn is_read_only(&self) -> bool {
        // The child may mutate (if its tool allowlist includes write/etc),
        // so we can't claim this as read-only.
        false
    }
}

fn task_wants_worktree(input: &serde_json::Value) -> bool {
    if input
        .get("worktree")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    input
        .get("isolation")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("worktree"))
        .unwrap_or(false)
}

fn task_wants_same_cwd(input: &serde_json::Value) -> bool {
    input
        .get("same_cwd")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || input
            .get("isolation")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("same-cwd") || s.eq_ignore_ascii_case("same_cwd"))
            .unwrap_or(false)
}

fn named_subagent_prompt(agent: &code_agents::AgentDefinition) -> String {
    format!(
        "## Named sub-agent: {name}\n\n\
You are running as the `{name}` sub-agent inside a parent LibertAI session. \
Apply the role instructions below as your primary scope. Keep the task narrow, \
use only the tools exposed to you, and return concise findings for the parent \
agent to relay or act on. Do not invent follow-up work outside the delegated \
task.\n\n\
When the parent asks for structured, machine-readable output (a findings list, \
a record, a config object), call the `structured_output` tool with a JSON \
`schema` describing the expected shape and your `data`; the tool validates the \
shape and echoes it back, or reports the violated path(s) to fix and retry. \
Prefer this over asserting the shape in prose.\n\n\
### Role instructions\n\n{body}",
        name = agent.name,
        body = agent.system_prompt.trim()
    )
}

struct TaskWorktree {
    repo_root: Option<PathBuf>,
    path: PathBuf,
    temp_root: PathBuf,
}

impl TaskWorktree {
    fn create(cwd: &Path) -> Result<Self, String> {
        match git_stdout(cwd, ["rev-parse", "--show-toplevel"]) {
            Ok(root) => Self::create_git_worktree(PathBuf::from(root.trim())),
            Err(_) => Self::create_snapshot(cwd),
        }
    }

    fn create_git_worktree(root: PathBuf) -> Result<Self, String> {
        let temp_root = std::env::temp_dir().join(format!(
            "libertai-task-worktree-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&temp_root).map_err(|e| format!("tempdir: {e}"))?;
        let path = temp_root.join("checkout");
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg("HEAD")
            .status()
            .map_err(|e| format!("git worktree add: {e}"))?;
        if !status.success() {
            return Err(format!("git worktree add failed with status {status}"));
        }
        Ok(Self {
            repo_root: Some(root),
            path,
            temp_root,
        })
    }

    fn create_snapshot(cwd: &Path) -> Result<Self, String> {
        let temp_root = std::env::temp_dir().join(format!(
            "libertai-task-snapshot-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&temp_root).map_err(|e| format!("tempdir: {e}"))?;
        let path = temp_root.join("workspace");
        std::fs::create_dir_all(&path).map_err(|e| format!("snapshot dir: {e}"))?;
        copy_workspace_snapshot(cwd, &path)?;
        Ok(Self {
            repo_root: None,
            path,
            temp_root,
        })
    }
}

impl Drop for TaskWorktree {
    fn drop(&mut self) {
        if let Some(repo_root) = self.repo_root.as_ref() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path)
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.temp_root);
    }
}

fn copy_workspace_snapshot(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(src).map_err(|e| format!("read snapshot source: {e}"))? {
        let entry = entry.map_err(|e| format!("read snapshot entry: {e}"))?;
        let name = entry.file_name();
        if should_skip_snapshot_entry(&name.to_string_lossy()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|e| format!("read snapshot file type: {e}"))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            std::fs::create_dir_all(&to).map_err(|e| format!("create snapshot dir: {e}"))?;
            copy_workspace_snapshot(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                format!(
                    "copy snapshot file {} -> {}: {e}",
                    from.display(),
                    to.display()
                )
            })?;
        }
    }
    Ok(())
}

fn should_skip_snapshot_entry(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | ".next"
            | ".nuxt"
            | ".svelte-kit"
            | ".cache"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

/// (R5HUNT-1) Process-wide monotonic counter used as the per-subagent log
/// path's uniqueness suffix. The old `unique_suffix` was a PURE wall-clock
/// `SystemTime::now().as_nanos()` — two subagents created in the same
/// nanosecond with the same display name collided on the path (the exact
/// hazard R4HUNT-4 set out to eliminate), and a cross-restart same-nanos +
/// same-name collision would APPEND to a stale log (worse now that the
/// success path defers deletion — a stale log from a prior session could be
/// appended to if the clock repeats). A monotonic `AtomicU64` guarantees
/// uniqueness WITHIN a process regardless of wall-clock resolution or
/// repeats; combined with `started_at` millis (kept for human-readability)
/// and the safe_name, the path is unique across the session. Cross-PROCESS
/// uniqueness still relies on millis + the safe_name not colliding
/// simultaneously — acceptable; the in-process collision was the real hazard.
static SUBAGENT_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    SUBAGENT_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(stderr.trim().to_string());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("git output was not utf-8: {e}"))
}

/// Update a registered agent handle from a child session event, so
/// the live panel can show the subagent's current tool and working
/// state. This rides on the same event stream `render_child` consumes
/// — no new plumbing — and is a no-op when the handle was already
/// removed (e.g. the subagent returned and the registry dropped it).
fn update_handle_from_event(handle: &Arc<AgentHandle>, event: &AgentEvent) {
    match event {
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            handle.set_current_tool(Some(tool_name.clone()));
            handle.set_status(AgentStatus::Working);
        }
        AgentEvent::ToolExecutionEnd { .. } => {
            handle.set_current_tool(None);
        }
        AgentEvent::AgentEnd { .. } => {
            handle.set_current_tool(None);
        }
        _ => {}
    }
}

/// Render events from the child session. Sends structured `ToolUpdate`s
/// via `on_update` so the parent's event loop can route subagent text
/// to the TUI transcript with agent attribution. The raw `eprint!`
/// output is kept for the one-shot path (non-TUI) where stderr dim
/// text is the only rendering channel.
fn render_child(
    event: AgentEvent,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    agent_name: &str,
) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: pi::model::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            use std::io::Write;
            eprint!("\x1b[2m{delta}\x1b[0m");
            let _ = std::io::stderr().flush();
            send_child_update(on_update, "subagent_text_delta", &delta, agent_name);
        }
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            eprintln!("\n  \x1b[2m[subagent tool] {tool_name}\x1b[0m");
            // Pack the tool args into the details JSON so the TUI can
            // show what the subagent invoked (the one-shot eprint! path
            // above is unchanged). Built directly — like the tool_end arm
            // — so `args` (a serde_json::Value) lands in details.args
            // rather than being flattened into a single Text block. The
            // content's first text block stays the bare tool name, since
            // the TUI's subagent_tool_start arm extracts the name from
            // content[0] (matching the prior send_child_update behavior).
            if let Some(on_update) = on_update {
                on_update(ToolUpdate {
                    content: vec![ContentBlock::Text(TextContent::new(tool_name.clone()))],
                    details: Some(serde_json::json!({
                        "kind": "subagent_tool_start",
                        "agent": agent_name,
                        "tool": tool_name,
                        "args": args,
                    })),
                });
            }
        }
        AgentEvent::ToolExecutionUpdate {
            tool_name,
            tool_call_id,
            partial_result,
            ..
        } => {
            if let Some(on_update) = on_update {
                on_update(ToolUpdate {
                    content: partial_result.content,
                    details: Some(serde_json::json!({
                        "kind": "subagent_tool_update",
                        "agent": agent_name,
                        "tool": tool_name,
                        "toolCallId": tool_call_id,
                        "details": partial_result.details,
                    })),
                });
            }
        }
        AgentEvent::ToolExecutionEnd {
            tool_name,
            tool_call_id,
            result,
            is_error,
        } => {
            eprintln!("  \x1b[2m[subagent tool done] {tool_name}\x1b[0m");
            if let Some(on_update) = on_update {
                on_update(ToolUpdate {
                    content: result.content,
                    details: Some(serde_json::json!({
                        "kind": "subagent_tool_end",
                        "agent": agent_name,
                        "tool": tool_name,
                        "toolCallId": tool_call_id,
                        "isError": is_error,
                        "details": result.details,
                    })),
                });
            }
        }
        AgentEvent::AgentEnd { error, .. } => {
            eprintln!();
            // Map the child's terminal state to an outcome the TUI can
            // render. pi's `AgentEnd` carries `error: Option<String>`
            // (no StopReason/Aborted distinction at this rev), so we
            // reduce to completed/failed. The one-shot eprint! path
            // keeps its "[subagent done]" content; the TUI reads
            // details.outcome.
            //
            // (MED-4) On abort, pi sets the error to "Aborted" (see
            // `build_abort_message` in agent.rs). Sniff for it and emit
            // "stopped" instead of "failed" so the TUI renders a single,
            // accurate "stopped" outcome line — not a misleading "failed"
            // that would double up with the main-thread "stopped {name}"
            // line. Chose option (a): sniff in render_child so the bg-side
            // outcome is authoritative and matches the AbortHandle path.
            let outcome = match &error {
                Some(msg) if msg.to_ascii_lowercase().contains("aborted") => "stopped",
                Some(_) => "failed",
                None => "completed",
            };
            if let Some(on_update) = on_update {
                on_update(ToolUpdate {
                    content: vec![ContentBlock::Text(TextContent::new(
                        "\n[subagent done]\n".to_string(),
                    ))],
                    details: Some(serde_json::json!({
                        "kind": "subagent_end",
                        "agent": agent_name,
                        "outcome": outcome,
                    })),
                });
            }
        }
        _ => {}
    }
}

fn send_child_update(
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    kind: &str,
    text: &str,
    agent_name: &str,
) {
    let Some(on_update) = on_update else {
        return;
    };
    on_update(ToolUpdate {
        content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        details: Some(serde_json::json!({ "kind": kind, "agent": agent_name })),
    });
}

fn err_output(text: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
    .into()
}

/// (R4HUNT-4) Directory holding per-subagent log files, a sibling of the
/// background-agents dir under the LibertAI config root. Distinct from
/// `code-background-agents/` so two agents (or a subagent + a background
/// agent) with the same name in the same millisecond can never collide on
/// the same path — the old code reused [`code_ui::background_agent_log_path`]
/// which names files `<millis>-<safe_name>.log` in the shared dir, so a
/// same-name-same-millis pair got the SAME path and two writers corrupted
/// both logs.
pub(crate) fn subagent_log_dir() -> std::io::Result<PathBuf> {
    let root =
        crate::config::libertai_config_dir().map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(root.join("code-subagents"))
}

/// (R4-1 + R4HUNT-4) Create a per-subagent log file so the TUI overlay
/// reads the subagent's streamed text via `read_agent_log_cached` /
/// `read_agent_log_typed` instead of the capped 5000-entry transcript
/// ring. Without this, a still-running in-process subagent's earliest
/// entries get evicted once the ring trims, leaving its overlay
/// truncated/empty.
///
/// (R4HUNT-4) Lives in its OWN `code-subagents/` subdir (sibling of
/// `code-background-agents/`) with a distinct name
/// `subagent-<millis>-<seq>-<safe_name>.log`. The `<millis>` (wall clock,
/// human-readable) + `<seq>` (a process-wide MONOTONIC counter — see
/// `unique_suffix`, R5HUNT-1) pair is unique across two subagents created
/// in the same millisecond: the monotonic `seq` never repeats within a
/// process, so combined with millis it rules out the same-name-same-ms
/// collision the old shared-dir `<millis>-<name>` naming had (and the
/// same-nanos collision the wall-clock `as_nanos()` suffix had). 0600 perms
/// via [`config::open_append_secure`]. The file is appended to in the
/// `Subagent*` arms of `handle_agent_msg` on the main thread (between
/// draws), and (on the SUCCESS path) deferred to session teardown via
/// `cleanup_subagent_logs` (R4HUNT-3); the FAILURE/abort paths remove it
/// immediately via the `SubagentGuard` Drop (R4HUNT-1). Returns the path on
/// success; a creation failure is non-fatal (the caller falls back to
/// `log_path: None`, the prior in-memory-only path).
fn create_subagent_log_file(name: &str) -> std::io::Result<PathBuf> {
    let dir = subagent_log_dir()?;
    config::create_dir_secure(&dir).map_err(|e| std::io::Error::other(e.to_string()))?;
    let _ = config::tighten_dir_mode_700(&dir);
    let safe_name: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let safe_name = safe_name.trim_matches('-');
    let safe_name = if safe_name.is_empty() {
        "agent"
    } else {
        safe_name
    };
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let suffix = unique_suffix();
    let log_path = dir.join(format!("subagent-{started_at}-{suffix}-{safe_name}.log"));
    // Create + open (append) so the file exists immediately (the overlay
    // may read it before the first SubagentText lands) and is 0600.
    let _ = config::open_append_secure(&log_path);
    Ok(log_path)
}

/// (R4-1 + R5-HUNT-B) Best-effort delete of a subagent's log file. As of
/// R5-HUNT-B, the SUCCESS, FAILURE, and ABORT paths all DEFER log deletion to
/// session teardown (`cleanup_subagent_logs`) so an overlay already open on
/// the subagent keeps its final output / failure reason / partial output. The
/// ONE remaining caller of this helper is the Session-init-fail arm of
/// `execute`: that arm runs synchronously on a `create_agent_session` error,
/// BEFORE the guard is built and before any `await`, so no overlay could be
/// open yet — an immediate delete is correct there. A missing/unreadable file
/// is silently ignored.
fn remove_subagent_log_file(log_path: &Path) {
    let _ = std::fs::remove_file(log_path);
}

/// (R4HUNT-3 + R5HUNT-2 + R5-HUNT-A) Remove every SUBAGENT LOG file in the
/// `code-subagents/` dir at session teardown. The SUCCESS-path subagent logs
/// are deferred (not deleted on completion, so the completed subagent's
/// overlay keeps reading its final output until the user closes it); this
/// sweeps them all once the session tears down so temp files don't
/// accumulate across sessions. Best-effort — a missing dir or an unreadable
/// file is silently ignored. Called from `app::run`'s exit path (the explicit
/// call documents the intent) AND by the [`SubagentLogSweeper`] Drop guard
/// (R5-HUNT-A) so the sweep ALSO fires on a panic mid-`run_loop` (Drop runs
/// during unwind). Both are idempotent; the guard's Drop sweeping an
/// already-swept dir on the success path is harmless.
///
/// (R5HUNT-2) Removal is GATED on the file name matching what
/// [`create_subagent_log_file`] produces — `subagent-<millis>-<seq>-<name>.log`
/// — i.e. `starts_with("subagent-")` AND `ends_with(".log")`. The old loop
/// removed EVERY `is_file()` entry with NO name guard, silently destroying
/// any user/tool-dropped file (NOTES, .tmp, README, a stray non-log file) in
/// `code-subagents/`. The guard keeps non-conforming files intact.
pub fn cleanup_subagent_logs() {
    if let Ok(dir) = subagent_log_dir() {
        sweep_subagent_logs_in(&dir);
    }
}

/// (R5-HUNT-A) Sweep the subagent-log files in `dir` using the R5HUNT-2
/// name guard. Split out of [`cleanup_subagent_logs`] so the
/// [`SubagentLogSweeper`] Drop guard (and its unit test) can target an
/// arbitrary directory — the production guard points at `subagent_log_dir()`
/// (via [`SubagentLogSweeper::new`]), the test guard points at a temp dir
/// (via [`SubagentLogSweeper::for_dir`]). Best-effort; a missing/unreadable
/// dir or file is silently ignored. Only regular files matching the
/// `subagent-...-<name>.log` naming are removed (defensive: never recurse,
/// never follow a stray symlink out of the dir).
fn sweep_subagent_logs_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // (R5HUNT-2) Only remove files whose name matches what
        // `create_subagent_log_file` produces (`subagent-...-<name>.log`),
        // so a user/tool-dropped non-log file (NOTES, README, .tmp) in the
        // dir survives the teardown sweep.
        let is_subagent_log = path.file_name().is_some_and(|n| {
            n.to_str()
                .is_some_and(|s| s.starts_with("subagent-") && s.ends_with(".log"))
        });
        if is_subagent_log {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// (R5-HUNT-A) RAII guard whose [`Drop`] runs [`sweep_subagent_logs_in`] (and
/// thus [`cleanup_subagent_logs`] for the production dir), so the deferred
/// subagent-log sweep fires on BOTH the normal return path AND a panic
/// mid-`run_loop`. Constructed early in `app::run`, right after the
/// `TerminalGuard` + `BracketedPasteGuard` are acquired, so it drops LAST (in
/// reverse declaration order): terminal restore + paste-disable run first,
/// THEN this sweep. The explicit `cleanup_subagent_logs()` call at the end of
/// `run` (R4HUNT-3) still fires on the success path — this guard covers the
/// panic path the explicit call is skipped on (a plain statement after
/// `run_loop` is unreachable once `run_loop` panics). `app::run` is NOT
/// `catch_unwind`-wrapped, but the release profile is `panic = unwind` (no
/// `panic =` in `Cargo.toml`, so the Rust default applies), so a panic
/// unwinds through `run` and this Drop runs during the unwind — exactly the
/// `TerminalGuard`/`BracketedPasteGuard` discipline already used in `run`.
///
/// Holds an `Option<PathBuf>`: `None` (the production default via [`new`])
/// resolves to `subagent_log_dir()` at drop time (so a config-dir move/tilde
/// expansion between construction and drop is honored); `Some(dir)` (the test
/// ctor [`for_dir`]) sweeps a fixed temp dir. The guard is `Send` (it holds
/// only an `Option<PathBuf>`).
pub(crate) struct SubagentLogSweeper {
    dir: Option<PathBuf>,
}

impl SubagentLogSweeper {
    /// Production ctor: sweep `subagent_log_dir()` on drop. Resolves the dir
    /// lazily at drop time (not at construction) so the sweep honors the
    /// config dir as it exists at teardown.
    pub(crate) fn new() -> Self {
        Self { dir: None }
    }

    /// Test ctor: sweep a fixed `dir` on drop. Used by the R5-HUNT-A unit
    /// test to point the guard at a temp dir with a sentinel subagent log.
    #[cfg(test)]
    fn for_dir(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }
}

impl Drop for SubagentLogSweeper {
    fn drop(&mut self) {
        match &self.dir {
            // Test path: sweep the fixed temp dir.
            Some(dir) => sweep_subagent_logs_in(dir),
            // Production path: resolve the config dir lazily + sweep.
            None => cleanup_subagent_logs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_subagent_logs, create_subagent_log_file, named_subagent_prompt, render_child,
        should_skip_snapshot_entry, subagent_log_dir, sweep_subagent_logs_in, task_wants_same_cwd,
        task_wants_worktree, SubagentGuard, SubagentLogSweeper, TaskWorktree,
    };
    use crate::commands::code_agents::{AgentDefinition, AgentSource};
    use crate::commands::code_team::{AgentKind, AgentRegistration, AgentRegistry, AgentStatus};
    use pi::model::{ContentBlock, TextContent};
    use pi::sdk::{AbortHandle, AgentEvent, ToolOutput, ToolUpdate};
    use serde_json::json;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    #[test]
    fn task_worktree_flag_accepts_boolean() {
        assert!(task_wants_worktree(&json!({"worktree": true})));
        assert!(!task_wants_worktree(&json!({"worktree": false})));
    }

    #[test]
    fn task_worktree_flag_accepts_isolation_mode() {
        assert!(task_wants_worktree(&json!({"isolation": "worktree"})));
        assert!(task_wants_worktree(&json!({"isolation": "WorkTree"})));
        assert!(!task_wants_worktree(&json!({"isolation": "same-cwd"})));
    }

    #[test]
    fn task_same_cwd_flag_accepts_override() {
        assert!(task_wants_same_cwd(&json!({"same_cwd": true})));
        assert!(task_wants_same_cwd(&json!({"isolation": "same-cwd"})));
        assert!(task_wants_same_cwd(&json!({"isolation": "same_cwd"})));
        assert!(!task_wants_same_cwd(&json!({"isolation": "worktree"})));
    }

    #[test]
    fn task_worktree_creates_checkout_and_cleans_temp_root() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "init"]);

        let temp_root;
        let checkout;
        {
            let worktree = TaskWorktree::create(&repo).unwrap();
            temp_root = worktree.temp_root.clone();
            checkout = worktree.path.clone();
            assert!(checkout.join("README.md").exists());
            assert!(temp_root.exists());
        }
        assert!(!checkout.exists());
        assert!(!temp_root.exists());
    }

    #[test]
    fn task_worktree_falls_back_to_snapshot_outside_git() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("plain");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("notes.txt"), "hello\n").unwrap();
        std::fs::create_dir(cwd.join("node_modules")).unwrap();
        std::fs::write(cwd.join("node_modules/skip.txt"), "skip\n").unwrap();

        let temp_root;
        let snapshot;
        {
            let worktree = TaskWorktree::create(&cwd).unwrap();
            temp_root = worktree.temp_root.clone();
            snapshot = worktree.path.clone();
            assert!(snapshot.join("notes.txt").exists());
            assert!(!snapshot.join("node_modules/skip.txt").exists());
            assert!(temp_root.exists());
        }
        assert!(!snapshot.exists());
        assert!(!temp_root.exists());
    }

    #[test]
    fn snapshot_skips_noisy_build_directories() {
        assert!(should_skip_snapshot_entry(".git"));
        assert!(should_skip_snapshot_entry("target"));
        assert!(should_skip_snapshot_entry("node_modules"));
        assert!(!should_skip_snapshot_entry("src"));
    }

    #[test]
    fn named_subagent_prompt_wraps_role_with_scope_guidance() {
        let agent = AgentDefinition {
            name: "reviewer".to_string(),
            description: "Reviews changes".to_string(),
            tools: None,
            model: None,
            worktree: false,
            color: None,
            system_prompt: "Focus on correctness.".to_string(),
            source: AgentSource::Project(tempfile::tempdir().unwrap().path().to_path_buf()),
        };

        let prompt = named_subagent_prompt(&agent);
        assert!(prompt.contains("## Named sub-agent: reviewer"));
        assert!(prompt.contains("running as the `reviewer` sub-agent"));
        assert!(prompt.contains("return concise findings for the parent"));
        assert!(prompt.contains("Do not invent follow-up work"));
        assert!(prompt.contains("### Role instructions"));
        assert!(prompt.contains("Focus on correctness."));
        // (M5/#14) The prompt tells the subagent about the structured_output
        // tool for schema-validated results.
        assert!(prompt.contains("structured_output"));
        assert!(prompt.contains("JSON `schema`"));
    }

    #[test]
    fn render_child_forwards_tool_update_and_end_metadata() {
        let updates = Arc::new(Mutex::new(Vec::<ToolUpdate>::new()));
        let sink = {
            let updates = Arc::clone(&updates);
            move |update: ToolUpdate| updates.lock().unwrap().push(update)
        };

        render_child(
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "child-read-1".to_string(),
                tool_name: "read".to_string(),
                args: json!({"path": "README.md"}),
                partial_result: ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new("partial child read"))],
                    details: Some(json!({"path": "README.md", "bytes": 12})),
                    is_error: false,
                },
            },
            Some(&sink),
            "reviewer",
        );
        render_child(
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "child-read-1".to_string(),
                tool_name: "read".to_string(),
                result: ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new("final child read"))],
                    details: Some(json!({"path": "README.md", "bytes": 24})),
                    is_error: false,
                },
                is_error: false,
            },
            Some(&sink),
            "reviewer",
        );

        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(tool_update_text(&updates[0]), "partial child read");
        assert_eq!(
            updates[0].details.as_ref().unwrap()["kind"],
            "subagent_tool_update"
        );
        assert_eq!(updates[0].details.as_ref().unwrap()["agent"], "reviewer");
        assert_eq!(updates[0].details.as_ref().unwrap()["tool"], "read");
        assert_eq!(
            updates[0].details.as_ref().unwrap()["toolCallId"],
            "child-read-1"
        );
        assert_eq!(updates[0].details.as_ref().unwrap()["details"]["bytes"], 12);
        assert_eq!(tool_update_text(&updates[1]), "final child read");
        assert_eq!(
            updates[1].details.as_ref().unwrap()["kind"],
            "subagent_tool_end"
        );
        assert_eq!(updates[1].details.as_ref().unwrap()["agent"], "reviewer");
        assert_eq!(updates[1].details.as_ref().unwrap()["isError"], false);
        assert_eq!(updates[1].details.as_ref().unwrap()["details"]["bytes"], 24);
    }

    // (MED-4) An aborted inline subagent's AgentEnd carries error "Aborted"
    // (pi's `build_abort_message`). render_child must map that to outcome
    // "stopped" — not "failed" — so the TUI renders a single accurate line
    // instead of a misleading "failed" that would double up with the
    // main-thread "stopped {name}" line.
    #[test]
    fn render_child_aborted_agent_end_maps_to_stopped() {
        let updates = Arc::new(Mutex::new(Vec::<ToolUpdate>::new()));
        let sink = {
            let updates = Arc::clone(&updates);
            move |update: ToolUpdate| updates.lock().unwrap().push(update)
        };

        render_child(
            AgentEvent::AgentEnd {
                session_id: "s1".into(),
                messages: Vec::new(),
                error: Some("Aborted".to_string()),
            },
            Some(&sink),
            "coder",
        );

        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 1, "AgentEnd emits one update");
        assert_eq!(updates[0].details.as_ref().unwrap()["kind"], "subagent_end");
        assert_eq!(
            updates[0].details.as_ref().unwrap()["outcome"],
            "stopped",
            "Aborted error must map to 'stopped', not 'failed'"
        );
    }

    // (MED-4 corollary) A non-abort error still maps to "failed".
    #[test]
    fn render_child_failed_agent_end_maps_to_failed() {
        let updates = Arc::new(Mutex::new(Vec::<ToolUpdate>::new()));
        let sink = {
            let updates = Arc::clone(&updates);
            move |update: ToolUpdate| updates.lock().unwrap().push(update)
        };

        render_child(
            AgentEvent::AgentEnd {
                session_id: "s1".into(),
                messages: Vec::new(),
                error: Some("network: connection reset".to_string()),
            },
            Some(&sink),
            "coder",
        );

        let updates = updates.lock().unwrap();
        assert_eq!(
            updates[0].details.as_ref().unwrap()["outcome"],
            "failed",
            "non-abort error must map to 'failed'"
        );
    }

    // (MED-4 corollary) A clean end (no error) maps to "completed".
    #[test]
    fn render_child_clean_agent_end_maps_to_completed() {
        let updates = Arc::new(Mutex::new(Vec::<ToolUpdate>::new()));
        let sink = {
            let updates = Arc::clone(&updates);
            move |update: ToolUpdate| updates.lock().unwrap().push(update)
        };

        render_child(
            AgentEvent::AgentEnd {
                session_id: "s1".into(),
                messages: Vec::new(),
                error: None,
            },
            Some(&sink),
            "coder",
        );

        let updates = updates.lock().unwrap();
        assert_eq!(
            updates[0].details.as_ref().unwrap()["outcome"],
            "completed",
            "no-error end must map to 'completed'"
        );
    }

    fn tool_update_text(update: &ToolUpdate) -> &str {
        match update.content.first() {
            Some(ContentBlock::Text(text)) => text.text.as_str(),
            other => panic!("expected text update, got {other:?}"),
        }
    }

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    // --- R4HUNT-1: SubagentGuard Drop reaps the abort-dropped subagent --------

    // (R4HUNT-1) The load-bearing regression: parent abort (Ctrl+C / Esc /
    // shared_abort) or a panic drops the `prompt_with_abort` future BEFORE the
    // manual cleanup arms in `execute` run — leaving the registry entry stuck
    // at Working (pid:None → poll_agent_status SKIPS it → never reaped → stuck
    // "working" subagent FOREVER, abort slot occupied). This test constructs a
    // SubagentGuard in the aborted state (`cleaned` still false, i.e. the
    // explicit arms never ran) and drops it WITHOUT running the explicit-arm
    // cleanup, then asserts the guard's Drop fired the reaping sequence:
    // registry entry removed, abort slot drained, status flipped to Failed.
    //
    // (R5-HUNT-B) The log file is now KEPT by the abort-drop path (deferred to
    // teardown) so an overlay already open on an ABORTED subagent keeps its
    // partial output instead of going blank. The old assertion that Drop
    // removes the log is inverted here to pin the new deferral behavior.
    #[test]
    fn r4hunt1_guard_drop_reaps_aborted_subagent_lifecycle() {
        let registry = AgentRegistry::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("aborted.log");
        // Create the log file so we can assert its SURVIVAL under R5-HUNT-B.
        std::fs::write(&log_path, b"partial subagent output\n").expect("seed log");
        // Register a subagent in Working state with an abort handle set (the
        // exact state at the point `execute` awaits prompt_with_abort).
        let handle = registry.register(AgentRegistration {
            name: "aborted".to_string(),
            kind: AgentKind::Subagent {
                depth: 1,
                parent: None,
            },
            color: crate::commands::code_team::AgentColor::Dim,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: Some(log_path.clone()),
        });
        handle.set_status(AgentStatus::Working);
        let (abort_handle, _signal) = AbortHandle::new();
        handle.set_abort(abort_handle);

        assert_eq!(registry.snapshot().len(), 1, "registered");
        assert!(handle.abort.lock().unwrap().is_some(), "abort slot set");
        assert!(log_path.exists(), "log file seeded");

        // Build the guard as `execute` does (right before the await), then
        // drop it WITHOUT marking `cleaned = true` — simulating the abort/
        // panic drop path where the explicit arms never ran.
        {
            let _guard = SubagentGuard::new(Arc::clone(&handle), Arc::clone(&registry));
            // Guard is dropped here, before any explicit-arm cleanup.
        }

        // The guard's Drop fired the reaping sequence (registry/abort/status),
        // but (R5-HUNT-B) the log SURVIVES — deferred to teardown so an overlay
        // already open on the aborted subagent keeps its partial output.
        assert!(
            log_path.exists(),
            "Drop KEPT the log file (R5-HUNT-B deferral)"
        );
        assert_eq!(
            registry.snapshot().len(),
            0,
            "Drop removed the registry entry"
        );
        assert!(
            handle.abort.lock().unwrap().is_none(),
            "Drop drained the abort slot (take_abort ran)"
        );
        assert_eq!(
            handle.status(),
            AgentStatus::Failed,
            "Drop flipped the active status to Failed (not stuck Working)"
        );
    }

    // (R4HUNT-1) The normal path: the explicit Ok/Err arms mark `cleaned = true`
    // AFTER their own cleanup, so the guard's Drop is a no-op. Assert the guard
    // does NOT re-remove an already-gone log file or re-flip a Completed status.
    #[test]
    fn r4hunt1_guard_drop_is_noop_when_cleaned_on_normal_path() {
        let registry = AgentRegistry::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("completed.log");
        std::fs::write(&log_path, b"done\n").expect("seed log");
        let handle = registry.register(AgentRegistration {
            name: "completed".to_string(),
            kind: AgentKind::Subagent {
                depth: 1,
                parent: None,
            },
            color: crate::commands::code_team::AgentColor::Dim,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: Some(log_path.clone()),
        });
        // Simulate the explicit Ok arm: take_abort + set_status(Completed) +
        // registry.remove + mark guard.cleaned = true (the Ok arm does NOT
        // remove the log — R4HUNT-3 defers it to teardown).
        let _ = handle.take_abort();
        handle.set_status(AgentStatus::Completed);
        registry.remove(handle.id);

        {
            let mut guard = SubagentGuard::new(Arc::clone(&handle), Arc::clone(&registry));
            guard.cleaned = true; // explicit arm already cleaned up
                                  // Guard drops here as a no-op.
        }
        // No-op Drop: the log file is NOT removed (R4HUNT-3 defers it), the
        // status stays Completed (not overwritten to Failed), and the registry
        // stays empty (no double-remove, though remove is idempotent anyway).
        assert!(
            log_path.exists(),
            "no-op Drop left the success-path log intact (R4HUNT-3)"
        );
        assert_eq!(
            handle.status(),
            AgentStatus::Completed,
            "no-op Drop kept Completed"
        );
        assert_eq!(registry.snapshot().len(), 0, "registry stays empty");
    }

    // --- R5-HUNT-B: FAILURE path keeps the log for an open overlay -------------

    // (R5-HUNT-B) An overlay can be open on a still-running subagent (Tab into
    // the agents panel + Enter-to-open-overlay are NOT gated on
    // Phase::Streaming). If that subagent then FAILS, the Err arm of `execute`
    // must DEFER the log deletion to teardown (not delete it immediately) so
    // the already-open overlay keeps its final output AND the failure reason
    // — the user can see WHY it failed. The old Err arm deleted the log right
    // away, blanking the overlay. This test drives the Err-arm body (the exact
    // sequence the production arm runs: take_abort + set_status(Failed) +
    // registry.remove + guard.cleaned=true, NO log removal) and asserts the log
    // SURVIVES + the status is Failed + the registry is empty + the overlay's
    // captured path still reads the failure output.
    #[test]
    fn r5huntb_failure_arm_keeps_log_for_open_overlay() {
        let registry = AgentRegistry::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("failed-subagent.log");
        // The subagent wrote some output then failed — the log carries BOTH the
        // partial work AND the failure reason (simulated by seeding the file).
        let failure_output = "partial work...\nERROR: tool rejected: permission denied";
        std::fs::write(&log_path, failure_output).expect("seed failed-subagent log");
        let handle = registry.register(AgentRegistration {
            name: "failed".to_string(),
            kind: AgentKind::Subagent {
                depth: 1,
                parent: None,
            },
            color: crate::commands::code_team::AgentColor::Dim,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: String::new(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            // The overlay captures log_path at OPEN time; the registry entry
            // carries the same path.
            log_path: Some(log_path.clone()),
        });
        handle.set_status(AgentStatus::Working);
        let (abort_handle, _signal) = AbortHandle::new();
        handle.set_abort(abort_handle);

        // Simulate the Err arm of `execute` EXACTLY (R5-HUNT-B fix): the arm
        // does take_abort + set_status(Failed) + registry.remove + marks the
        // guard cleaned — and NO longer calls remove_subagent_log_file.
        {
            let mut guard = SubagentGuard::new(Arc::clone(&handle), Arc::clone(&registry));
            // --- Err-arm body (mirrors code_task.rs `Err(e) => { ... }`) ---
            let _ = handle.take_abort();
            handle.set_status(AgentStatus::Failed);
            // (R5-HUNT-B) NO remove_subagent_log_file here — deferred to teardown.
            registry.remove(handle.id);
            guard.cleaned = true;
            // Guard drops here as a no-op (cleaned == true).
        }

        // The log SURVIVES the Err arm — an overlay open on this subagent keeps
        // the final output + the failure reason instead of going blank.
        assert!(
            log_path.exists(),
            "Err arm KEPT the log (R5-HUNT-B deferral)"
        );
        let on_disk = std::fs::read_to_string(&log_path).expect("log readable");
        assert!(
            on_disk.contains("ERROR: tool rejected"),
            "overlay can read the failure reason from the deferred log"
        );
        // Status flipped to Failed (so the panel reflects the failure), the
        // registry entry is gone (overlay can't be RE-opened, but the one
        // already open keeps reading via its captured path).
        assert_eq!(handle.status(), AgentStatus::Failed, "Err arm set Failed");
        assert_eq!(
            registry.snapshot().len(),
            0,
            "Err arm removed the registry entry"
        );
    }

    // --- R4HUNT-4: distinct subagent log path -----------------------------------

    // (R4HUNT-4 + R5HUNT-1) A subagent log path lives in the `code-subagents/`
    // dir (NOT the shared `code-background-agents/` dir) and carries a
    // `subagent-` prefix + a `subagent-<millis>-<seq>-<safe_name>.log` name
    // whose `<seq>` is a process-wide MONOTONIC counter (R5HUNT-1, replacing
    // the wall-clock `as_nanos()` suffix), so two same-name subagents (or a
    // subagent + a background agent) created in the same millisecond get
    // DISTINCT paths instead of colliding. This pins the new naming against a
    // regression that re-introduces the shared-dir collision.
    #[test]
    fn r4hunt4_subagent_log_path_is_distinct_from_background_dir() {
        let path = create_subagent_log_file("researcher").expect("create log");
        let parent = path.parent().expect("path has parent");
        // Lives under code-subagents/, NOT code-background-agents/.
        assert!(
            parent.ends_with("code-subagents"),
            "subagent log must live in code-subagents/, got {}",
            parent.display()
        );
        // The file name carries the subagent- prefix (distinct from the
        // background dir's `<millis>-<name>` naming).
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("file name");
        assert!(
            name.starts_with("subagent-"),
            "subagent log name must start with 'subagent-', got {name}"
        );
        assert!(
            name.ends_with("-researcher.log"),
            "subagent log name must end with the safe name, got {name}"
        );
        // The dir is the registered subagent_log_dir().
        let dir = subagent_log_dir().expect("subagent_log_dir");
        assert_eq!(parent, dir.as_path(), "parent matches subagent_log_dir()");
        // 0600 perms (unix only).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("file exists")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "subagent log must be 0600, got {mode:o}");
        }
        // Clean up the temp file we created in the shared config dir. Don't
        // remove the dir itself — other concurrent tests (and the production
        // path) write into it; a `remove_dir` here would race them (nuking the
        // dir out from under a sibling test's open). The dir is a persistent
        // cache; `cleanup_subagent_logs` sweeps stray files at teardown.
        let _ = std::fs::remove_file(&path);
    }

    // (R4HUNT-4 + R5HUNT-1) Two same-name subagents created back-to-back get
    // DISTINCT paths. With the old wall-clock `as_nanos()` suffix two calls in
    // the same nanosecond would have collided; the monotonic counter (R5HUNT-1)
    // guarantees distinctness regardless of wall-clock resolution. The old
    // shared-dir naming would also have collided here.
    #[test]
    fn r4hunt4_two_same_name_subagents_get_distinct_paths() {
        let a = create_subagent_log_file("coder").expect("create log a");
        let b = create_subagent_log_file("coder").expect("create log b");
        assert_ne!(a, b, "same-name same-ms subagents must get distinct paths");
        // Parse the monotonic `<seq>` out of each name and assert it strictly
        // increases — the load-bearing guarantee of R5HUNT-1. Name shape is
        // `subagent-<millis>-<seq>-<safe_name>.log`.
        let seq_of = |p: &std::path::Path| -> u64 {
            let name = p.file_name().and_then(|n| n.to_str()).expect("file name");
            // Strip the `subagent-` prefix + the `-coder.log` tail.
            let inner = name.strip_prefix("subagent-").expect("subagent- prefix");
            let core = inner.strip_suffix("-coder.log").expect("-coder.log suffix");
            // core == `<millis>-<seq>`; the seq is the last `-`-delimited run.
            core.rsplit_once('-')
                .expect("<millis>-<seq> shape")
                .1
                .parse::<u64>()
                .expect("seq is a u64")
        };
        let seq_a = seq_of(&a);
        let seq_b = seq_of(&b);
        assert!(
            seq_b > seq_a,
            "monotonic counter must strictly increase: a={seq_a} b={seq_b}"
        );
        // Clean up the files (not the shared dir — see the test above).
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    // (R5HUNT-1) The monotonic counter is process-wide, so a burst of many
    // back-to-back same-name subagent-log creations all get DISTINCT paths
    // (no two share a `<seq>`). Under the old wall-clock `as_nanos()` suffix a
    // tight loop could repeat a nanos value on platforms with coarse clocks
    // and two creations would collide. This pins the monotonic guarantee
    // against a regression back to a clock-based suffix.
    #[test]
    fn r5hunt1_unique_suffix_is_monotonic_across_burst() {
        let paths: Vec<_> = (0..64)
            .map(|_| create_subagent_log_file("burst").expect("create log"))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for p in &paths {
            assert!(
                seen.insert(p.clone()),
                "duplicate path in burst — monotonic counter regressed"
            );
        }
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }

    // --- R5HUNT-2: cleanup_subagent_logs name guard -----------------------------

    // (R5HUNT-2) cleanup_subagent_logs must ONLY remove files whose name
    // matches what create_subagent_log_file produces (`subagent-...-<name>.log`).
    // The old loop removed EVERY is_file() entry with NO name guard, silently
    // destroying any user/tool-dropped file (NOTES, README, .tmp) in
    // code-subagents/. This drops a real subagent log + several non-conforming
    // files into subagent_log_dir(), runs cleanup, and asserts the subagent
    // log IS removed while the non-conforming files SURVIVE.
    #[test]
    fn r5hunt2_cleanup_subagent_logs_keeps_non_conforming_files() {
        let dir = subagent_log_dir().expect("subagent_log_dir");
        let _ = std::fs::create_dir_all(&dir);
        // A real subagent log produced by the production creator — must be swept.
        let real_log = create_subagent_log_file("hunter").expect("create real subagent log");
        assert!(real_log.starts_with(&dir), "real log lives in subagent dir");
        // Non-conforming files a user/tool might drop in the dir — must survive.
        let notes = dir.join("NOTES.md");
        let readme = dir.join("README");
        let tmp = dir.join("scratch.tmp");
        let stray_log = dir.join("agent.log"); // ends .log but no subagent- prefix
        let stray_prefix = dir.join("subagent-notes.txt"); // prefix but no .log
        std::fs::write(&notes, b"keep me").expect("seed notes");
        std::fs::write(&readme, b"keep me").expect("seed readme");
        std::fs::write(&tmp, b"keep me").expect("seed tmp");
        std::fs::write(&stray_log, b"keep me").expect("seed stray_log");
        std::fs::write(&stray_prefix, b"keep me").expect("seed stray_prefix");

        cleanup_subagent_logs();

        // The real subagent log (subagent-...-hunter.log) IS removed.
        assert!(!real_log.exists(), "real subagent log swept by cleanup");
        // Every non-conforming file SURVIVES.
        assert!(notes.exists(), "NOTES.md survives (R5HUNT-2 name guard)");
        assert!(readme.exists(), "README survives (R5HUNT-2 name guard)");
        assert!(tmp.exists(), "scratch.tmp survives (R5HUNT-2 name guard)");
        assert!(
            stray_log.exists(),
            "agent.log (no subagent- prefix) survives"
        );
        assert!(
            stray_prefix.exists(),
            "subagent-notes.txt (no .log suffix) survives"
        );

        // Clean up the survivors + the dir's now-empty state. Don't remove the
        // dir itself — see r4hunt4_subagent_log_path_is_distinct_from_background_dir.
        let _ = std::fs::remove_file(&notes);
        let _ = std::fs::remove_file(&readme);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&stray_log);
        let _ = std::fs::remove_file(&stray_prefix);
    }

    // --- R6HUNT-1: startup sweep reaps cross-restart stale logs -----------------

    // (R6HUNT-1) A prior session that HARD-CRASHED (no Drop ran) leaves stale
    // subagent logs on disk. The per-subagent log name is
    // subagent-{millis}-{seq}-{name}.log where seq is a process-wide
    // AtomicU64 (SUBAGENT_SEQ) that RESETS to 0 on every restart, so the
    // effective cross-restart key is millis + name. A fresh session's first
    // same-named subagent created in the same 1ms window would collide on the
    // path + (append-only open) append onto the stale file, corrupting the
    // overlay. The fix: app::run calls cleanup_subagent_logs() at STARTUP
    // (before run_loop) to reap stale logs first. This test seeds a stale log
    // with the EXACT cross-restart-collision name shape a prior crashed session
    // would leave (millis=1700000000000, seq=0 — the fresh process's first
    // subagent's seq), plus a non-conforming file, into an ISOLATED temp dir,
    // then calls sweep_subagent_logs_in (the exact function cleanup_subagent_logs
    // delegates to at startup), and asserts the stale log IS reaped while
    // non-conforming files SURVIVE — pinning that the startup sweep closes the
    // collision window without harming user files. Uses a temp dir (not the
    // real subagent_log_dir) so it doesn't race with the other tests that
    // share the real config dir.
    #[test]
    fn r6hunt1_startup_sweep_reaps_cross_restart_stale_log() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_path_buf();
        // A stale log a prior HARD-CRASHED session left behind. The exact
        // cross-restart-collision shape: a prior session's seq reset to 0 is
        // indistinguishable from THIS process's first subagent's seq=0, and if
        // the millis + name coincide, create_subagent_log_file (append-only)
        // would append onto this file. The startup sweep must reap it first.
        let stale = dir.join("subagent-1700000000000-0-researcher.log");
        std::fs::write(&stale, b"prior crashed session\n").expect("seed stale log");
        assert!(stale.exists(), "stale log seeded");
        // A second stale log with a different name — also reaped (all
        // subagent-*.log are swept, not just collisions).
        let stale2 = dir.join("subagent-1700000000001-1-coder.log");
        std::fs::write(&stale2, b"another stale log\n").expect("seed stale2");
        // Non-conforming files a user/tool might drop — must SURVIVE the
        // startup sweep (R5HUNT-2 name guard).
        let notes = dir.join("NOTES.md");
        let stray_prefix = dir.join("subagent-notes.txt"); // prefix but no .log
        std::fs::write(&notes, b"keep me").expect("seed notes");
        std::fs::write(&stray_prefix, b"keep me").expect("seed stray_prefix");

        // The startup sweep app::run performs — cleanup_subagent_logs() is
        // exactly `subagent_log_dir().map(sweep_subagent_logs_in)`, so this
        // exercises the identical sweep logic against an isolated dir.
        sweep_subagent_logs_in(&dir);

        // Both stale subagent logs are reaped — the cross-restart collision
        // window is closed before create_subagent_log_file can append to them.
        assert!(
            !stale.exists(),
            "R6HUNT-1: stale cross-restart log reaped at startup"
        );
        assert!(
            !stale2.exists(),
            "R6HUNT-1: second stale log reaped at startup"
        );
        // Non-conforming files SURVIVE (R5HUNT-2 name guard honored at startup).
        assert!(
            notes.exists(),
            "R6HUNT-1: NOTES.md survives the startup sweep"
        );
        assert!(
            stray_prefix.exists(),
            "R6HUNT-1: subagent-notes.txt survives the startup sweep"
        );
        // The temp dir cleans itself up on drop.
    }

    // --- R5-HUNT-A: panic-safe teardown Drop guard ------------------------------

    // (R5-HUNT-A) The subagent-log teardown sweep MUST run even when `run_loop`
    // panics — `app::run` is NOT `catch_unwind`-wrapped, so the explicit
    // `cleanup_subagent_logs()` after `run_loop` is skipped on a panic, and the
    // deferred subagent logs would be orphaned on disk. The `SubagentLogSweeper`
    // RAII guard (constructed at the top of `run`) fires its Drop during the
    // unwind, sweeping the logs. This test pins that the guard's Drop runs DURING
    // an unwind: it constructs a `SubagentLogSweeper::for_dir(temp)` over a temp
    // dir holding a sentinel subagent log, holds the guard inside a
    // `catch_unwind` closure that PANICS, and asserts the sentinel was removed
    // (Drop ran during the unwind) — mirroring how a panic mid-`run_loop` drops
    // the sweeper.
    #[test]
    fn r5hunta_sweeper_drop_runs_during_unwind_and_sweeps_logs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_path_buf();
        // Seed a sentinel subagent log (R5HUNT-2 name shape) + a non-conforming
        // file (must survive — pins that the sweeper uses the name guard).
        let sentinel = dir.join("subagent-12345-7-hunter.log");
        std::fs::write(&sentinel, b"orphaned log\n").expect("seed sentinel");
        let stray = dir.join("NOTES.md");
        std::fs::write(&stray, b"keep me\n").expect("seed stray");

        assert!(sentinel.exists(), "sentinel seeded");
        assert!(stray.exists(), "stray seeded");

        // Hold the sweeper across a panic. catch_unwind lets the panic unwind
        // (running the guard's Drop during the unwind) then resumes; AssertUnwindSafe
        // is sound here — the sweeper holds only an Option<PathBuf> (UnwindSafe)
        // and the closure doesn't share mutable state with the outside.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Construct the guard INSIDE the closure so it's dropped during the
            // unwind when the panic propagates — the exact `run` shape (guard
            // constructed early, dropped by the unwind).
            let _sweeper = SubagentLogSweeper::for_dir(dir.clone());
            panic!("simulated panic in run_loop");
        }));
        assert!(result.is_err(), "the closure panicked as expected");

        // The guard's Drop ran DURING the unwind + swept the sentinel subagent log.
        assert!(
            !sentinel.exists(),
            "R5-HUNT-A: sweeper Drop removed the subagent log during the unwind"
        );
        // (R5HUNT-2) The non-conforming file SURVIVES the sweeper too.
        assert!(
            stray.exists(),
            "R5-HUNT-A: sweeper honored the R5HUNT-2 name guard (NOTES.md survived)"
        );
    }

    // (R5-HUNT-A) The production `SubagentLogSweeper::new()` (dir == None)
    // resolves to `subagent_log_dir()` at drop time and sweeps it. This pins
    // that the None arm actually sweeps the real config dir (so a regression
    // that no-ops the None arm is caught): seed a real subagent log into
    // `subagent_log_dir()`, drop a `new()` sweeper, assert the log is gone.
    #[test]
    fn r5hunta_sweeper_new_sweeps_real_config_dir() {
        let real = create_subagent_log_file("sweeper-target").expect("create real log");
        assert!(real.exists(), "real log seeded in config dir");
        {
            let _sweeper = SubagentLogSweeper::new();
            // Sweeper drops here on the normal return path.
        }
        assert!(
            !real.exists(),
            "R5-HUNT-A: SubagentLogSweeper::new() swept the real config dir on drop"
        );
    }

    // (R5-HUNT-A) Normal-return drop: a `for_dir` sweeper dropped normally (no
    // panic) also sweeps. This pins the success-path behavior (the explicit
    // `cleanup_subagent_logs()` call in `run` documents the intent, but the
    // sweeper's Drop is the actual mechanism on the early-return/panic paths).
    #[test]
    fn r5hunta_sweeper_drop_sweeps_on_normal_return() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_path_buf();
        let sentinel = dir.join("subagent-9-3-researcher.log");
        std::fs::write(&sentinel, b"done\n").expect("seed sentinel");
        {
            let _sweeper = SubagentLogSweeper::for_dir(dir.clone());
            // Normal return — no panic. Sweeper drops here.
        }
        assert!(
            !sentinel.exists(),
            "R5-HUNT-A: sweeper Drop swept the log on normal return"
        );
    }

    // (R5-HUNT-A + R5HUNT-2) Direct unit test of `sweep_subagent_logs_in` — the
    // helper the sweeper + `cleanup_subagent_logs` delegate to — to pin the
    // name guard independent of the guard's Drop timing.
    #[test]
    fn r5hunta_sweep_subagent_logs_in_name_guard() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let real = dir.join("subagent-1-1-x.log");
        let stray = dir.join("README");
        std::fs::write(&real, b"log\n").expect("seed real");
        std::fs::write(&stray, b"keep\n").expect("seed stray");
        sweep_subagent_logs_in(dir);
        assert!(!real.exists(), "subagent log swept");
        assert!(stray.exists(), "non-conforming file survived");
    }
}
