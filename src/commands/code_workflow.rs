//! The workflow engine (M6 #15) — a JavaScript orchestrator the agent
//! can drive from a tool call, modeled on Claude Code's `Workflow` tool.
//!
//! A workflow runs a user-supplied JS script inside an embedded
//! [QuickJS] sandbox (rquickjs 0.11, already in the lockfile transitively
//! via pi). The script calls host functions to spawn phase agents:
//!
//! - `agent(prompt, opts?)` — run one subagent, await its result string.
//! - `parallel([p1, p2, …])` — run N thunks concurrently, await all (barrier).
//! - `pipeline(items, stage1, stage2, …)` — run each item through every
//!   stage with no barrier between stages (item A can be in stage 3 while
//!   item B is still in stage 1).
//! - `phase(title, fn)` — group agents under a phase label for the viewer.
//! - `log(...args)` — emit a progress line to the parent turn.
//!
//! ## The nested-runtime deadlock, and how this avoids it
//!
//! The whole point of the engine is that `agent()` (and the agents inside
//! `parallel`/`pipeline`) runs a real subagent — a second pi session that
//! makes LLM calls and runs tools. That work happens on the SAME asupersync
//! runtime the parent turn is already driving (the bg thread's
//! `runtime.block_on`). If the workflow `execute` future merely `.await`ed
//! each subagent in turn it would block the runtime and the subagent's own
//! I/O could never make progress.
//!
//! We mirror the PROVEN pattern pi itself uses for its JS extension layer
//! (see `pi/extensions_js.rs`):
//!
//! 1. `agent()` is a **synchronous** host function (`Func::from`). It does
//!    NOT await anything. It allocates a fresh `call_id`, registers a
//!    pending completion slot, spawns the real subagent on the asupersync
//!    runtime via `RuntimeHandle::current_handle().spawn(...)`, and returns
//!    the `call_id` string immediately.
//! 2. A small JS prelude wraps every host call in `new Promise((resolve,
//!    reject) => { const id = native(...); __wf_pending.set(id, {resolve,
//!    reject}); })`. The resolve/reject closures live on the **JS heap**
//!    in a `Map` — we never hold Rust `Persistent` handles across an `.await`,
//!    so there is no re-entrant `ctx.with` from a spawned task.
//! 3. When the spawned subagent task completes, it writes its result into a
//!    `Mutex<Vec<PendingCompletion>>` on the bridge — pure data, no JS access.
//! 4. The drive loop interleaves: `rt.idle().await` (drains JS microtasks +
//!    polls rquickjs's own spawner — currently empty since we use no
//!    `ctx.spawn`), then drains the bridge's pending completions under a
//!    single `ctx.with(|c| __wf_complete(id, json))` — a JS function that
//!    looks up `id` in the `Map` and calls `resolve`/`reject`. Then
//!    `yield_now().await` so asupersync polls the spawned subagent tasks.
//!    Loop ends when `rt.is_job_pending()` is false AND the bridge has no
//!    in-flight subagents.
//!
//! This keeps ALL JS-context access on the drive-loop's logical thread,
//! serializes it, and lets the subagent tasks run concurrently on asupersync.
//! No re-entrancy → no QuickJS-mutex deadlock.
//!
//! ## Recursion / safety
//!
//! The tool refuses to run when the parent is already at
//! [`MAX_TASK_DEPTH`]‑1 (a workflow's phase agents run at parent_depth+1,
//! the same gate `TaskTool` uses, so a workflow at depth 2 spawns phase
//! agents at depth 3 — still `< MAX_TASK_DEPTH`).
//!
//! Each phase agent is registered in the shared [`AgentRegistry`] so the
//! live panel + `/agents` show it while it runs, guarded by a
//! [`WorkflowAgentGuard`] (the workflow analogue of `TaskTool`'s
//! `SubagentGuard`) that reaps the entry + abort slot on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use rquickjs::prelude::Func;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, IntoJs, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{
    create_agent_session, AbortHandle, AgentEvent, Result as PiResult, Tool, ToolExecution,
    ToolOutput, ToolUpdate,
};

use crate::commands::code_approvals::{ApprovalState, ApprovalUi};
use crate::commands::code_factory::{LibertaiToolFactory, ModeFlag, MAX_TASK_DEPTH};
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, SessionPersistence, DEFAULT_MAX_TOKENS,
};
use crate::commands::code_team::{AgentColor, AgentHandle, AgentKind, AgentRegistry, AgentStatus};

const NAME: &str = "workflow";
const LABEL: &str = "Workflow";
const DESCRIPTION: &str = concat!(
    "Run a multi-step JavaScript workflow that orchestrates several ",
    "subagents in parallel or as a pipeline. Use when a task decomposes ",
    "into independent subtasks that benefit from concurrent execution or ",
    "when you need a fan-out → verify → synthesize structure. The script ",
    "calls agent(prompt, opts?), parallel(thunks), pipeline(items, ",
    "...stages), phase(title, fn), and log(...args). agent() opts: label ",
    "(display name), tools (narrow within the ceiling), agent (a named ",
    "subagent type whose tools:/model: frontmatter is honored — this is ",
    "how a phase gets write access), schema (a JSON Schema; the promise ",
    "then resolves with the schema-validated object instead of prose). ",
    "The script's return value becomes the tool result. Phase agents run ",
    "as isolated subagents (read-only by default) and appear in /agents ",
    "and the live workflow tree while running."
);

/// Soft wall-clock cap for a whole workflow run, overridable via the env
/// var. Defends a runaway script (infinite `while` loop) from pinning the
/// bg thread forever. Aborts via dropping the `execute` future, which
/// drops in-flight subagent tasks → their `WorkflowAgentGuard`s reap.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const ENV_TIMEOUT_SECS: &str = "LIBERTAI_WORKFLOW_TIMEOUT_SECS";

/// Memory + stack caps for the QuickJS sandbox. Tight by design: workflow
/// scripts are orchestrators, not compute kernels — 64 MiB of heap and a
/// 1 MiB stack are ample and bound a runaway allocation.
const JS_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const JS_MAX_STACK_BYTES: usize = 1024 * 1024;

/// (WF-A3) Per-process run counter so every workflow gets a unique id
/// (`wf-<pid>-<n>`). The old `wf-<pid>` id collided across runs in one
/// session, so `WorkflowRegistry::remove(&id)` on an early failure reaped
/// every run and `/workflows` history was impossible. String ids stay
/// journal-friendly for the deferred resume feature.
static WF_COUNTER: AtomicU64 = AtomicU64::new(0);

/// (WF-A4) How many finished (non-Running) workflows the registry keeps as
/// session history for `/workflows`. Oldest pruned first.
const MAX_FINISHED_WORKFLOWS: usize = 20;

/// Cap on the script's synthesized result embedded in the tool output /
/// completion notification (WF-D). Full result lives only in JS.
const MAX_RESULT_CHARS: usize = 4096;

// ---------------------------------------------------------------------------
// Live state — registry of running/recently-finished workflows, mirrored on
// the `AgentRegistry` shape so `/workflows` can snapshot it.
// ---------------------------------------------------------------------------

/// Lifecycle of one workflow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStatus {
    /// Script is evaluating / phase agents are running.
    Running,
    /// All phase agents finished, script returned.
    Completed,
    /// Script threw or a phase agent failed terminally.
    Failed,
    /// Aborted by the user (parent-turn abort) or the wall-clock timeout.
    Stopped,
}

/// One phase within a workflow (a `phase(title, fn)` call). Mirrors the
/// `phase('Title', ...)` grouping the script declares.
#[derive(Clone)]
pub struct PhaseProgress {
    pub title: String,
    /// Agents in spawn order under this phase.
    pub agents: Vec<Arc<AgentHandle>>,
}

/// A live or recently-finished workflow. Serialize/Deserialize-ready for a
/// future `/export` (the fields are plain + owned), though the live viewer
/// reads it directly via [`WorkflowRegistry::snapshot`].
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    pub status: Mutex<WorkflowStatus>,
    pub started_at: Instant,
    /// (WF-A6) Set when the run reaches a terminal status so `elapsed()`
    /// freezes for finished rows in `/workflows` and the live tree.
    pub finished_at: Mutex<Option<Instant>>,
    pub phases: Mutex<Vec<PhaseProgress>>,
    /// (WF-E) True for a detached run: `execute` returned immediately and
    /// the TUI's `poll_background_workflows` owes the parent session a
    /// `<task-notification>` when the run reaches a terminal status.
    pub background: std::sync::atomic::AtomicBool,
    /// (WF-E) One-shot latch so the completion notification fires exactly
    /// once against the run-loop's tick polling.
    pub notified: std::sync::atomic::AtomicBool,
    /// (WF-E) The run's final summary (incl. the script result / error),
    /// written by the JS thread when the run settles. Embedded in the
    /// notification and shown by `/workflows`.
    pub outcome: Mutex<Option<String>>,
    /// (WF-E) `log()` lines. Always recorded here (`/workflows` reads
    /// them); additionally streamed via `on_update` for synchronous runs
    /// only — a background run's parent tool call is already Done, so late
    /// ToolUpdates would reference a finished call.
    pub logs: Mutex<Vec<String>>,
}

impl WorkflowState {
    fn new(id: String, name: String) -> Arc<Self> {
        Arc::new(Self {
            id,
            name,
            status: Mutex::new(WorkflowStatus::Running),
            started_at: Instant::now(),
            finished_at: Mutex::new(None),
            phases: Mutex::new(Vec::new()),
            background: std::sync::atomic::AtomicBool::new(false),
            notified: std::sync::atomic::AtomicBool::new(false),
            outcome: Mutex::new(None),
            logs: Mutex::new(Vec::new()),
        })
    }

    /// (WF-E) True when this detached run finished but the completion
    /// notification hasn't fired yet; flips the one-shot latch. Exactly-
    /// once against the 80 ms tick via `AtomicBool::swap`.
    pub fn take_pending_notification(&self) -> bool {
        self.background.load(Ordering::Relaxed)
            && self.status() != WorkflowStatus::Running
            && !self.notified.swap(true, Ordering::Relaxed)
    }

    pub fn status(&self) -> WorkflowStatus {
        *self.status.lock().unwrap()
    }

    pub fn set_status(&self, s: WorkflowStatus) {
        *self.status.lock().unwrap() = s;
        // (WF-A6) Freeze the clock on the FIRST terminal transition.
        if s != WorkflowStatus::Running {
            let mut fin = self.finished_at.lock().unwrap();
            if fin.is_none() {
                *fin = Some(Instant::now());
            }
        }
    }

    /// (WF-F) True when the run reached a terminal status within `within`
    /// — the live tree keeps a short "completion flash" so the final state
    /// is visible before the tree row disappears.
    pub fn finished_within(&self, within: std::time::Duration) -> bool {
        self.finished_at
            .lock()
            .unwrap()
            .is_some_and(|f| f.elapsed() <= within)
    }

    /// Wall-clock runtime: live for Running, frozen at the terminal
    /// transition for finished runs.
    pub fn elapsed(&self) -> std::time::Duration {
        match *self.finished_at.lock().unwrap() {
            Some(fin) => fin.duration_since(self.started_at),
            None => self.started_at.elapsed(),
        }
    }

    /// All agent handles across all phases, in phase→spawn order. Used by
    /// `/workflows` to render the per-agent rows.
    pub fn agents(&self) -> Vec<Arc<AgentHandle>> {
        self.phases
            .lock()
            .unwrap()
            .iter()
            .flat_map(|p| p.agents.iter().cloned())
            .collect()
    }
}

/// Shared table of live + recently-finished workflows, mirroring
/// [`AgentRegistry`]. Threaded through the tool factory as
/// `Arc<WorkflowRegistry>`; the TUI's `/workflows` reads `snapshot()`.
#[derive(Default)]
pub struct WorkflowRegistry {
    workflows: Mutex<Vec<Arc<WorkflowState>>>,
}

impl WorkflowRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, state: Arc<WorkflowState>) {
        let mut ws = self.workflows.lock().unwrap();
        ws.push(state);
        // (WF-A4) Keep finished runs as session history, but bounded:
        // prune the OLDEST non-Running entries beyond the cap. Running
        // entries are never pruned.
        let finished = ws
            .iter()
            .filter(|w| w.status() != WorkflowStatus::Running)
            .count();
        let mut to_prune = finished.saturating_sub(MAX_FINISHED_WORKFLOWS);
        if to_prune > 0 {
            ws.retain(|w| {
                if to_prune > 0 && w.status() != WorkflowStatus::Running {
                    to_prune -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn remove(&self, id: &str) {
        self.workflows.lock().unwrap().retain(|w| w.id != id);
    }

    /// Test seam for other modules' unit tests (`WorkflowState::new` is
    /// private): register and return a fresh Running state.
    #[cfg(test)]
    pub(crate) fn register_for_test(&self, id: String, name: String) -> Arc<WorkflowState> {
        let s = WorkflowState::new(id, name);
        self.register(Arc::clone(&s));
        s
    }

    pub fn snapshot(&self) -> Vec<Arc<WorkflowState>> {
        // Oldest first, mirroring AgentRegistry::snapshot.
        self.workflows.lock().unwrap().clone()
    }

    pub fn active_count(&self) -> usize {
        self.workflows
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.status() == WorkflowStatus::Running)
            .count()
    }
}

// ---------------------------------------------------------------------------
// The bridge — Arc-shared between the drive loop and the spawned subagent
// tasks. Single-threaded by construction (the workflow runs entirely on the
// bg thread's asupersync runtime), but `Send + Sync` is required because
// `RuntimeHandle::spawn` demands `Future + Send + 'static`.
// ---------------------------------------------------------------------------

/// A result waiting to be delivered to JS by the drive loop. Produced by a
/// spawned subagent task, drained under `ctx.with` by the loop.
enum PendingCompletion {
    Resolve { id: String, json: String },
    Reject { id: String, message: String },
}

/// Shared state between the host functions (sync, on the JS thread) and the
/// spawned subagent tasks (asupersync). The host fns allocate `call_id`s +
/// bump `in_flight`; the tasks push completions; the drive loop drains them.
struct WorkflowBridge {
    /// Monotonic call-id generator (sync host fns read this).
    next_id: AtomicU64,
    /// Completions queued by spawned tasks, drained by the drive loop. The
    /// ONLY cross-thread channel — pure data, no JS handles.
    completions: Mutex<Vec<PendingCompletion>>,
    /// Number of spawned subagent tasks not yet resolved/rejected. The drive
    /// loop continues while this is > 0 (even if JS microtasks are idle).
    in_flight: AtomicU64,
    /// Progress lines emitted by `log(...)`, surfaced to the parent turn.
    /// The drive loop drains these into the `on_update` callback.
    logs: Mutex<Vec<String>>,
    /// (WF-D) The script's resolved value, JSON-encoded — written by the
    /// `__wf_native_result` host fn from the `.then` chain wrapping the
    /// async IIFE. `None` when the script never settled (timeout/abort).
    result: Mutex<Option<String>>,
    /// (WF-D) A top-level rejection / thrown error, written by
    /// `__wf_native_error`. Previously these vanished entirely and the run
    /// reported "completed"; now they flip the run to Failed.
    script_error: Mutex<Option<String>>,
}

impl WorkflowBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            completions: Mutex::new(Vec::new()),
            in_flight: AtomicU64::new(0),
            logs: Mutex::new(Vec::new()),
            result: Mutex::new(None),
            script_error: Mutex::new(None),
        })
    }

    fn alloc_id(&self) -> String {
        format!("wf-call-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn drain_completions(&self) -> Vec<PendingCompletion> {
        std::mem::take(&mut *self.completions.lock().unwrap())
    }

    fn drain_logs(&self) -> Vec<String> {
        std::mem::take(&mut *self.logs.lock().unwrap())
    }

    fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }
}

/// RAII guard for a workflow phase agent, mirroring `TaskTool`'s
/// `SubagentGuard`. On the abort-drop path (parent abort drops the
/// `execute` future, which drops in-flight subagent tasks → drops their
/// guards), reaps the registry entry + abort slot + sets Failed if still
/// active. Idempotent via `cleaned`.
struct WorkflowAgentGuard {
    handle: Arc<AgentHandle>,
    registry: Arc<AgentRegistry>,
    cleaned: bool,
}

impl WorkflowAgentGuard {
    fn new(handle: Arc<AgentHandle>, registry: Arc<AgentRegistry>) -> Self {
        Self {
            handle,
            registry,
            cleaned: false,
        }
    }
}

impl Drop for WorkflowAgentGuard {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let _ = self.handle.take_abort();
        if self.handle.status().is_active() {
            self.handle.set_status(AgentStatus::Failed);
        }
        self.registry.remove(self.handle.id);
    }
}

// ---------------------------------------------------------------------------
// Phase-agent dispatch — extracted from the `__wf_native_agent` host fn so
// the closure passed to `Func::new` stays one line (deep async-in-closure
// nesting is hard to keep brace-balanced by hand). All the captured context
// lives in this struct, cloned once per host-fn closure.
// ---------------------------------------------------------------------------

struct AgentSpawnCtx {
    bridge: Arc<WorkflowBridge>,
    state: Arc<WorkflowState>,
    registry: Arc<AgentRegistry>,
    cfg: Arc<crate::config::Config>,
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    parent_depth: u8,
    cwd: PathBuf,
    bash_wrapper: Option<Vec<String>>,
    /// The BG asupersync runtime handle — phase agents spawn onto THIS
    /// (where their I/O + the LLM stream run), NOT the JS thread's local
    /// runtime. `Send`, so it crosses from the JS thread back to the bg.
    bg_handle: asupersync::runtime::RuntimeHandle,
}

/// (WF-B/WF-C) Everything `run_phase_agent` needs beyond the prompt,
/// resolved synchronously at dispatch time so registration already knows
/// the true capability and label.
struct PhaseAgentSpec {
    /// Resolved tool set (ceiling ∩ requested, TaskTool policy).
    tools: Vec<String>,
    /// Named-agent system-prompt wrap (`named_subagent_prompt`), if any.
    append_system_prompt: Option<String>,
    /// Model override from the named agent's frontmatter, if any.
    model: Option<String>,
    /// (WF-C) JSON schema the agent must satisfy via `structured_output`.
    schema: Option<serde_json::Value>,
}

/// Push an immediate rejection for `id` (dispatch-time failure: unknown
/// agent type, bad schema, dead runtime). Mirrors the spawn-failure path:
/// completion queued for the drive loop + in-flight released.
fn reject_dispatch(ctx: &AgentSpawnCtx, id: String, message: String) {
    ctx.bridge
        .completions
        .lock()
        .unwrap()
        .push(PendingCompletion::Reject { id, message });
    ctx.bridge.in_flight.fetch_sub(1, Ordering::Relaxed);
}

/// Register the phase-agent handle, spawn it on asupersync, and return its
/// `call_id`. Called synchronously from the `__wf_native_agent` host fn.
/// The spawned task runs `run_phase_agent`, then pushes a completion onto
/// the bridge for the drive loop to deliver to JS.
fn dispatch_phase_agent(
    ctx: &AgentSpawnCtx,
    prompt: String,
    label: Option<String>,
    tools_json: Option<String>,
    agent_name: Option<String>,
    schema_json: Option<String>,
) -> String {
    let id = ctx.bridge.alloc_id();
    ctx.bridge.in_flight.fetch_add(1, Ordering::Relaxed);

    // (WF-B2) Named subagent type (`opts.agent`), resolved synchronously
    // at dispatch so the tool ceiling, model, and capability are known
    // BEFORE registration. Unknown name → immediate reject.
    let agent_def = match &agent_name {
        Some(name) => match crate::commands::code_agents::find_agent(&ctx.cwd, name) {
            Ok(Some(def)) => Some(def),
            Ok(None) => {
                reject_dispatch(
                    ctx,
                    id.clone(),
                    format!("workflow agent(): unknown agent type `{name}`"),
                );
                return id;
            }
            Err(e) => {
                reject_dispatch(
                    ctx,
                    id.clone(),
                    format!("workflow agent(): could not load agents: {e:#}"),
                );
                return id;
            }
        },
        None => None,
    };

    // (WF-C) Schema for structured output. A present-but-invalid schema is
    // a script bug — reject rather than silently degrade to prose.
    let schema: Option<serde_json::Value> = match &schema_json {
        Some(s) => match serde_json::from_str(s) {
            Ok(v) => Some(v),
            Err(e) => {
                reject_dispatch(
                    ctx,
                    id.clone(),
                    format!("workflow agent(): bad schema: {e}"),
                );
                return id;
            }
        },
        None => None,
    };

    // (WF-B1) Tool policy mirrors TaskTool (shared helper): ceiling is the
    // named definition's `tools:` frontmatter, else TASK_DEFAULT_TOOLS;
    // `opts.tools` narrows within the ceiling. Phase agents are no longer
    // hard-locked read-only — a write-capable named agent is honored, and
    // ApprovalTool still wraps every mutating tool.
    let requested: Vec<String> = tools_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default();
    let ceiling: Vec<String> = agent_def
        .as_ref()
        .and_then(|a| a.tools.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            crate::commands::code_task::TASK_DEFAULT_TOOLS
                .iter()
                .map(|&s| s.to_string())
                .collect()
        });
    let mut tools = crate::commands::code_task::resolve_subagent_tools(ceiling, requested);
    if schema.is_some() && !tools.iter().any(|t| t == "structured_output") {
        // (WF-C) The schema contract needs the structured_output tool; it
        // is registered on every session but filtered by enabled_tools.
        tools.push("structured_output".to_string());
    }
    let capability = crate::commands::code_team::AgentCapability::from_tools(&tools);

    // (WF-B4) Scannable panel labels: `wf:<phase>/<label>`, with fallbacks
    // `main` (no phase() yet) and `agent-<n>` (no label).
    let phase_title = ctx
        .state
        .phases
        .lock()
        .unwrap()
        .last()
        .map(|p| p.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "main".to_string());
    let agent_n = ctx.state.agents().len() + 1;
    let display_name = format!(
        "wf:{phase_title}/{}",
        label
            .clone()
            .or_else(|| agent_name.clone())
            .unwrap_or_else(|| format!("agent-{agent_n}"))
    );
    let prompt_preview: String = prompt.chars().take(80).collect();

    let model = agent_def.as_ref().and_then(|a| a.model.clone());
    let handle = ctx
        .registry
        .register(crate::commands::code_team::AgentRegistration {
            name: display_name.clone(),
            kind: AgentKind::Subagent {
                depth: ctx.parent_depth + 1,
                parent: None,
            },
            color: AgentColor::color_for_name(&display_name),
            capability,
            cwd: ctx.cwd.clone(),
            model: model
                .clone()
                .unwrap_or_else(|| ctx.cfg.default_code_model.clone()),
            prompt_preview,
            parent: None,
            pid: None,
            log_path: None,
        });
    handle.set_status(AgentStatus::Working);

    // Track under the current phase (or a synthetic "main" phase if
    // phase() was never called).
    {
        let mut phases = ctx.state.phases.lock().unwrap();
        if phases.last().is_none() {
            phases.push(PhaseProgress {
                title: "main".to_string(),
                agents: vec![],
            });
        }
        phases.last_mut().unwrap().agents.push(Arc::clone(&handle));
    }

    let spec = PhaseAgentSpec {
        tools,
        append_system_prompt: agent_def
            .as_ref()
            .map(crate::commands::code_task::named_subagent_prompt),
        model,
        schema,
    };

    let spawn_ctx = SpawnTaskCtx {
        bridge: Arc::clone(&ctx.bridge),
        registry: Arc::clone(&ctx.registry),
        cfg: Arc::clone(&ctx.cfg),
        mode: ctx.mode.clone(),
        approvals: Arc::clone(&ctx.approvals),
        ui: Arc::clone(&ctx.ui),
        depth: ctx.parent_depth + 1,
        cwd: ctx.cwd.clone(),
        bash_wrapper: ctx.bash_wrapper.clone(),
    };
    // Spawn the phase agent onto the BG runtime (via the captured handle),
    // not the JS thread's local runtime. If the handle is somehow dead,
    // reject immediately so the script's `await` unblocks with an error.
    let id_for_return = id.clone();
    let id_for_task = id.clone();
    if ctx
        .bg_handle
        .try_spawn(async move {
            spawn_phase_agent_task(spawn_ctx, id_for_task, handle, prompt, spec).await;
        })
        .is_err()
    {
        reject_dispatch(
            ctx,
            id,
            "workflow: bg runtime unavailable to spawn the subagent".to_string(),
        );
    }
    id_for_return
}

/// Captured context for the spawned subagent task (everything the task
/// needs that isn't `Clone`-cheaply from `AgentSpawnCtx` Arcs).
struct SpawnTaskCtx {
    bridge: Arc<WorkflowBridge>,
    registry: Arc<AgentRegistry>,
    cfg: Arc<crate::config::Config>,
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    depth: u8,
    cwd: PathBuf,
    bash_wrapper: Option<Vec<String>>,
}

/// The spawned task body: run the phase agent, finalize its status, and
/// push a completion onto the bridge for the drive loop to settle in JS.
/// (WF-C) With a schema, the promise resolves with the schema-validated
/// object's JSON (captured from the agent's `structured_output` call)
/// instead of the assistant's prose; a schema'd agent that never produced
/// validated output is a REJECTION — the script must be able to trust the
/// shape.
async fn spawn_phase_agent_task(
    ctx: SpawnTaskCtx,
    id: String,
    handle: Arc<AgentHandle>,
    prompt: String,
    spec: PhaseAgentSpec,
) {
    let _guard = WorkflowAgentGuard::new(Arc::clone(&handle), Arc::clone(&ctx.registry));
    let schema_requested = spec.schema.is_some();
    let result = run_phase_agent(
        ctx.mode.clone(),
        Arc::clone(&ctx.approvals),
        Arc::clone(&ctx.ui),
        ctx.depth,
        ctx.cwd.clone(),
        Arc::clone(&ctx.registry),
        Arc::clone(&ctx.cfg),
        ctx.bash_wrapper.clone(),
        prompt,
        spec,
    )
    .await;
    let completion = match result {
        Ok(agent_result) => {
            handle.set_status(AgentStatus::Completed);
            ctx.registry.remove(handle.id);
            match (schema_requested, agent_result.structured) {
                (true, Some(data)) => PendingCompletion::Resolve {
                    id,
                    json: data.to_string(),
                },
                (true, None) => PendingCompletion::Reject {
                    id,
                    message: "workflow agent(): schema requested but the phase agent \
                              did not return schema-validated output"
                        .to_string(),
                },
                (false, _) => PendingCompletion::Resolve {
                    id,
                    json: serde_json::to_string(&agent_result.text)
                        .unwrap_or_else(|_| "\"\"".to_string()),
                },
            }
        }
        Err(e) => {
            handle.set_status(AgentStatus::Failed);
            ctx.registry.remove(handle.id);
            PendingCompletion::Reject { id, message: e }
        }
    };
    ctx.bridge.completions.lock().unwrap().push(completion);
    ctx.bridge.in_flight.fetch_sub(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Dedicated-thread runner — owns the `!Send` QuickJS runtime/context so the
// `execute` future (which must be `Send`) only holds a channel receiver.
// ---------------------------------------------------------------------------

/// Everything the JS thread needs. All fields are `Send` (the `on_update`
/// callback is `Box<dyn Fn + Send + Sync>`; the `RuntimeHandle` is `Send`).
struct WorkflowRunCtx {
    script: String,
    wf_name: String,
    cfg: Arc<crate::config::Config>,
    state: Arc<WorkflowState>,
    bridge: Arc<WorkflowBridge>,
    registry: Arc<AgentRegistry>,
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    parent_depth: u8,
    cwd: PathBuf,
    bash_wrapper: Option<Vec<String>>,
    bg_handle: asupersync::runtime::RuntimeHandle,
    on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
}

/// The final outcome of a workflow run, produced on the JS thread and sent
/// back to `execute` via the channel. Carries the textual summary + the
/// `workflow_result` details, mirroring the shape `execute` used to build
/// inline.
struct WorkflowRunResult {
    summary: String,
    wf_name: String,
    final_status: WorkflowStatus,
    /// (WF-D) The script's resolved value, JSON-encoded (already truncated
    /// into `summary` for display; kept raw here for the details payload).
    result_json: Option<String>,
    /// (WF-D) Top-level script error (eval failure or unhandled rejection).
    script_error: Option<String>,
}

impl WorkflowRunResult {
    fn into_output(self) -> ToolExecution {
        let mut details = serde_json::json!({
            "kind": "workflow_result",
            "workflow": self.wf_name,
            "status": match self.final_status {
                WorkflowStatus::Completed => "completed",
                WorkflowStatus::Failed => "failed",
                WorkflowStatus::Stopped => "stopped",
                WorkflowStatus::Running => "completed",
            },
        });
        if let Some(json) = &self.result_json {
            // Best-effort: embed the parsed value so downstream consumers
            // get structure, falling back to the raw string.
            details["result"] = serde_json::from_str(json)
                .unwrap_or_else(|_| serde_json::Value::String(json.clone()));
        }
        if let Some(err) = &self.script_error {
            details["script_error"] = serde_json::Value::String(err.clone());
        }
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(self.summary))],
            details: Some(details),
            is_error: matches!(
                self.final_status,
                WorkflowStatus::Failed | WorkflowStatus::Stopped
            ),
        }
        .into()
    }
}

/// The thread entry point. Builds a `current_thread()` asupersync runtime
/// (separate from the bg runtime) that drives the rquickjs `AsyncRuntime`,
/// sets up the host functions, runs the drive loop, and sends the result.
///
/// (WF-E) `result_tx` is `Some` for a synchronous run (the parent tool
/// call blocks on it) and `None` for a background run — backgroundness is
/// explicit, not a silent send to a dropped receiver. Either way the final
/// summary lands in `state.outcome` so `/workflows` and the completion
/// notification can read it.
fn run_workflow_on_thread(ctx: WorkflowRunCtx, result_tx: Option<mpsc::Sender<WorkflowRunResult>>) {
    let outcome = run_workflow_inner(&ctx);
    *ctx.state.outcome.lock().unwrap() = Some(outcome.summary.clone());
    // For synchronous runs, emit a workflow_end ToolUpdate so the TUI
    // surfaces a single end line (mirrors TaskTool's subagent_end).
    // Best-effort — if the receiver is gone (parent aborted), the send is
    // a no-op. Background runs have `on_update: None` by construction
    // (their parent call is Done; the notification path takes over).
    let outcome_label = match outcome.final_status {
        WorkflowStatus::Completed => "completed",
        WorkflowStatus::Failed => "failed",
        WorkflowStatus::Stopped => "stopped",
        WorkflowStatus::Running => "completed",
    };
    if let Some(on_update) = &ctx.on_update {
        on_update(ToolUpdate {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "\n[workflow {} {}]\n",
                ctx.wf_name, outcome_label
            )))],
            // (WF-A5) Keys must match the TUI translator (`app.rs`
            // "workflow_end" arm), which reads `name` + `id` — the old
            // `workflow` key left both empty in every end line.
            details: Some(serde_json::json!({
                "kind": "workflow_end",
                "name": ctx.wf_name,
                "id": ctx.state.id,
                "outcome": outcome_label,
            })),
        });
    }
    if let Some(tx) = result_tx {
        let _ = tx.send(outcome);
    }
}

/// Drain pending `log()` lines: record them on the state (always) and
/// stream them to the parent turn via `on_update` (sync runs only).
fn flush_workflow_logs(ctx: &WorkflowRunCtx) {
    let logs = ctx.bridge.drain_logs();
    if logs.is_empty() {
        return;
    }
    ctx.state.logs.lock().unwrap().extend(logs.iter().cloned());
    if let Some(on_update) = &ctx.on_update {
        for line in logs {
            on_update(ToolUpdate {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "[workflow:{}] {line}\n",
                    ctx.wf_name
                )))],
                details: Some(serde_json::json!({
                    "kind": "workflow_log",
                    "workflow": ctx.wf_name,
                    "line": line,
                })),
            });
        }
    }
}

/// The synchronous core: build the sandbox, run the drive loop, return the
/// outcome. Runs inside `block_on` on the dedicated thread's own
/// `current_thread()` runtime — NOT the bg runtime. The phase-agent tasks
/// are spawned onto the BG runtime via `ctx.bg_handle`.
fn run_workflow_inner(ctx: &WorkflowRunCtx) -> WorkflowRunResult {
    // Local asupersync runtime on THIS thread to drive rquickjs.
    let reactor = match asupersync::runtime::reactor::create_reactor() {
        Ok(r) => r,
        Err(e) => {
            ctx.state.set_status(WorkflowStatus::Failed);
            return WorkflowRunResult {
                summary: format!("workflow {}: QuickJS reactor init: {e}", ctx.wf_name),
                wf_name: ctx.wf_name.clone(),
                final_status: WorkflowStatus::Failed,
                result_json: None,
                script_error: None,
            };
        }
    };
    let runtime = match asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            ctx.state.set_status(WorkflowStatus::Failed);
            return WorkflowRunResult {
                summary: format!("workflow {}: QuickJS runtime init: {e}", ctx.wf_name),
                wf_name: ctx.wf_name.clone(),
                final_status: WorkflowStatus::Failed,
                result_json: None,
                script_error: None,
            };
        }
    };

    let result = runtime.block_on(async move { run_workflow_async(ctx).await });
    result
}

/// (WF-G) Offline engine selftest, invoked via
/// `LIBERTAI_WORKFLOW_SELFTEST=<script> libertai code`. Runs `script`
/// through the REAL engine (dedicated JS thread, drive loop, prelude) with
/// no LLM session — `log`/`phase`/values/errors all work; `agent()` calls
/// have no model behind them, so selftest scripts should not use them
/// (they would run until the workflow timeout). Prints the summary + log
/// lines to stderr; exit status reflects the run status. This gives the
/// offline probe suite a real end-to-end hook.
pub fn run_selftest(script: &str, cfg: Arc<crate::config::Config>) -> anyhow::Result<()> {
    use crate::commands::code_approvals::PromptChoice;

    struct DenyUi;
    #[async_trait]
    impl ApprovalUi for DenyUi {
        async fn decide(&self, _tool: &str, _preview: &str, _rule: &str) -> PromptChoice {
            PromptChoice::Deny
        }
    }

    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|e| anyhow::anyhow!("selftest reactor init: {e}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!("selftest runtime init: {e}"))?;

    let state = WorkflowState::new(next_wf_id(), "selftest".to_string());
    let state_for_logs = Arc::clone(&state);
    let script = script.to_string();
    let outcome = runtime.block_on(async move {
        let bg_handle = asupersync::runtime::Runtime::current_handle()
            .ok_or_else(|| anyhow::anyhow!("selftest: no runtime handle"))?;
        let ctx = WorkflowRunCtx {
            script,
            wf_name: "selftest".to_string(),
            cfg,
            state,
            bridge: WorkflowBridge::new(),
            registry: AgentRegistry::new(),
            mode: ModeFlag::new(crate::commands::code_factory::Mode::Normal),
            approvals: Arc::new(ApprovalState::new()),
            ui: Arc::new(DenyUi),
            parent_depth: 0,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            bash_wrapper: None,
            bg_handle,
            on_update: None,
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || run_workflow_on_thread(ctx, Some(tx)));
        // Poll + yield (same reasoning as WorkflowTool::execute — never
        // block the current_thread runtime).
        loop {
            match rx.try_recv() {
                Ok(r) => break Ok::<_, anyhow::Error>(r),
                Err(mpsc::TryRecvError::Empty) => asupersync::runtime::yield_now().await,
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("selftest: JS thread exited without a result")
                }
            }
        }
    })?;

    for line in state_for_logs.logs.lock().unwrap().iter() {
        eprintln!("workflow selftest log: {line}");
    }
    eprintln!("workflow selftest: {}", outcome.summary);
    if outcome.final_status == WorkflowStatus::Completed {
        Ok(())
    } else {
        anyhow::bail!("workflow selftest {}", outcome.summary)
    }
}

/// The async drive loop, run on the dedicated thread's runtime. Owns the
/// `!Send` rquickjs handles across `.await` — which is fine here because
/// this future is run via `block_on` on THIS single thread, NOT boxed as a
/// `Send` future (unlike `Tool::execute`, which is why that one delegates
/// here via a channel).
async fn run_workflow_async(ctx: &WorkflowRunCtx) -> WorkflowRunResult {
    let rt = match AsyncRuntime::new() {
        Ok(r) => r,
        Err(e) => {
            ctx.state.set_status(WorkflowStatus::Failed);
            return WorkflowRunResult {
                summary: format!("workflow {}: QuickJS runtime init: {e}", ctx.wf_name),
                wf_name: ctx.wf_name.clone(),
                final_status: WorkflowStatus::Failed,
                result_json: None,
                script_error: None,
            };
        }
    };
    rt.set_memory_limit(JS_MEMORY_LIMIT_BYTES).await;
    rt.set_max_stack_size(JS_MAX_STACK_BYTES).await;
    let context = match AsyncContext::full(&rt).await {
        Ok(c) => c,
        Err(e) => {
            ctx.state.set_status(WorkflowStatus::Failed);
            return WorkflowRunResult {
                summary: format!("workflow {}: QuickJS context init: {e}", ctx.wf_name),
                wf_name: ctx.wf_name.clone(),
                final_status: WorkflowStatus::Failed,
                result_json: None,
                script_error: None,
            };
        }
    };

    // Register the sync host functions + install the prelude.
    let bridge = Arc::clone(&ctx.bridge);
    let state = Arc::clone(&ctx.state);
    let registry = Arc::clone(&ctx.registry);
    let cfg = Arc::clone(&ctx.cfg);
    let mode = ctx.mode.clone();
    let approvals = Arc::clone(&ctx.approvals);
    let ui = Arc::clone(&ctx.ui);
    let parent_depth = ctx.parent_depth;
    let cwd = ctx.cwd.clone();
    let bash_wrapper = ctx.bash_wrapper.clone();
    let bg_handle = ctx.bg_handle.clone();
    let setup_res: Result<(), rquickjs::Error> = context
        .with(|js| -> Result<(), rquickjs::Error> {
            let globals = js.globals();

            let agent_ctx = AgentSpawnCtx {
                bridge: Arc::clone(&bridge),
                state: Arc::clone(&state),
                registry: Arc::clone(&registry),
                cfg: Arc::clone(&cfg),
                mode: mode.clone(),
                approvals: Arc::clone(&approvals),
                ui: Arc::clone(&ui),
                parent_depth,
                cwd: cwd.clone(),
                bash_wrapper: bash_wrapper.clone(),
                bg_handle: bg_handle.clone(),
            };
            globals.set(
                "__wf_native_agent",
                Func::from(
                    move |_c: Ctx,
                          prompt: String,
                          label: Option<String>,
                          tools_json: Option<String>,
                          agent_name: Option<String>,
                          schema_json: Option<String>|
                          -> rquickjs::Result<String> {
                        Ok(dispatch_phase_agent(
                            &agent_ctx,
                            prompt,
                            label,
                            tools_json,
                            agent_name,
                            schema_json,
                        ))
                    },
                ),
            )?;

            let bridge_log = Arc::clone(&bridge);
            globals.set(
                "__wf_native_log",
                Func::from(move |_c: Ctx, line: String| -> rquickjs::Result<()> {
                    bridge_log.logs.lock().unwrap().push(line);
                    Ok(())
                }),
            )?;

            let state_phase = Arc::clone(&state);
            globals.set(
                "__wf_native_phase",
                Func::from(move |_c: Ctx, title: String| -> rquickjs::Result<()> {
                    state_phase.phases.lock().unwrap().push(PhaseProgress {
                        title,
                        agents: vec![],
                    });
                    Ok(())
                }),
            )?;

            // (WF-D) Script settlement capture: the `.then` chain around
            // the async IIFE reports the resolved value (JSON) or the
            // top-level error here. Previously both were discarded.
            let bridge_result = Arc::clone(&bridge);
            globals.set(
                "__wf_native_result",
                Func::from(move |_c: Ctx, json: String| -> rquickjs::Result<()> {
                    *bridge_result.result.lock().unwrap() = Some(json);
                    Ok(())
                }),
            )?;
            let bridge_error = Arc::clone(&bridge);
            globals.set(
                "__wf_native_error",
                Func::from(move |_c: Ctx, message: String| -> rquickjs::Result<()> {
                    *bridge_error.script_error.lock().unwrap() = Some(message);
                    Ok(())
                }),
            )?;

            js.eval::<(), _>(JS_PRELUDE)?;
            Ok(())
        })
        .await;
    if let Err(e) = setup_res {
        ctx.state.set_status(WorkflowStatus::Failed);
        return WorkflowRunResult {
            summary: format!("workflow {}: sandbox setup: {e}", ctx.wf_name),
            wf_name: ctx.wf_name.clone(),
            final_status: WorkflowStatus::Failed,
            result_json: None,
            script_error: None,
        };
    }

    // Evaluate the user script inside an async IIFE so top-level await
    // works and a thrown error rejects the returned promise. We discard the
    // returned Promise value — it carries rquickjs's `'js` lifetime and
    // can't escape the `with` closure. The drive loop detects completion via
    // `rt.idle()` + the in-flight counter, not via this return.
    //
    // (WF-A1) The arrow `=>` was missing here — `(async () {…})()` is a
    // SyntaxError, so every script failed instantly and the run still
    // reported "completed (0 agents)". The `.then` chain (WF-D) captures
    // the script's resolved value / top-level error via the native fns;
    // JSON.stringify is guarded (circular values fall back to String(v)).
    let script_body = format!(
        "(async () => {{\n{}\n}})().then(function (v) {{\n\
           let s; try {{ s = JSON.stringify(v === undefined ? null : v); }}\n\
           catch (e) {{ s = JSON.stringify(String(v)); }}\n\
           __wf_native_result(s === undefined ? \"null\" : s);\n\
         }}, function (e) {{\n\
           // QuickJS Error.stack has frames only, no message — send both.\n\
           __wf_native_error(String(e) + (e && e.stack ? '\\n' + e.stack : ''));\n\
         }})",
        ctx.script
    );
    let eval_ok: Result<(), rquickjs::Error> = context
        .with(|js| -> Result<(), rquickjs::Error> {
            // Ignore the returned Promise; a thrown syntax error surfaces as
            // the drive loop terminating immediately with no completions.
            let _: Value = js.eval(script_body)?;
            Ok(())
        })
        .await;
    // (WF-A2) A syntax/eval error must fail the RUN, not just log — the old
    // code logged and fell through to "completed (0 agents)".
    let mut eval_failed: Option<String> = None;
    if let Err(e) = eval_ok {
        let msg = format!("script error: {e}");
        if let Some(on_update) = &ctx.on_update {
            on_update(ToolUpdate {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "[workflow:{}] {msg}\n",
                    ctx.wf_name
                )))],
                details: Some(serde_json::json!({
                    "kind": "workflow_log",
                    "workflow": ctx.wf_name,
                    "line": msg,
                })),
            });
        }
        eval_failed = Some(msg);
    }

    let timeout_secs = std::env::var(ENV_TIMEOUT_SECS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let deadline = Instant::now() + std::time::Duration::from_secs(timeout_secs);

    let mut timed_out = false;
    loop {
        // Drain completions the spawned tasks queued → settle their
        // promises via the JS `__wf_complete` function, all under one
        // `ctx.with` (serializes JS-context access; no re-entrancy).
        let completions = ctx.bridge.drain_completions();
        if !completions.is_empty() {
            let _ = context
                .with(|js| -> Result<(), rquickjs::Error> {
                    let globals = js.globals();
                    let complete: Function = globals.get("__wf_complete")?;
                    for comp in completions {
                        match comp {
                            PendingCompletion::Resolve { id, json } => {
                                let f = complete.clone();
                                let id_v = id.into_js(&js)?;
                                let ok_v = true.into_js(&js)?;
                                let p_v = json.into_js(&js)?;
                                f.call::<_, ()>((id_v, ok_v, p_v))?;
                            }
                            PendingCompletion::Reject { id, message } => {
                                let f = complete.clone();
                                let id_v = id.into_js(&js)?;
                                let ok_v = false.into_js(&js)?;
                                let p_v = message.into_js(&js)?;
                                f.call::<_, ()>((id_v, ok_v, p_v))?;
                            }
                        }
                    }
                    Ok(())
                })
                .await;
        }

        // Surface log() lines: always recorded on the state (WF-E — the
        // live tree + /workflows read them, and background runs have no
        // on_update), and additionally streamed to the parent turn for
        // synchronous runs (best-effort).
        flush_workflow_logs(ctx);

        // Drain JS microtasks (resolve/reject callbacks, promise chains).
        rt.idle().await;

        // Termination: JS idle AND no in-flight subagents.
        let js_idle = !rt.is_job_pending().await;
        if js_idle && ctx.bridge.in_flight() == 0 {
            break;
        }
        // Wall-clock safety net.
        if Instant::now() > deadline {
            timed_out = true;
            break;
        }
        // Yield so the asupersync executor on THIS thread polls its own
        // tasks (and so this loop doesn't busy-spin). The phase-agent tasks
        // run on the BG runtime (via bg_handle), not here, but their
        // completions land in the bridge which we drain above.
        asupersync::runtime::yield_now().await;
    }

    // (WF-G) Final log flush: `log()` lines pushed during the LAST
    // microtask drain (e.g. right before the script settles) arrive after
    // the loop's drain but before the break — without this they were lost.
    flush_workflow_logs(ctx);

    // (WF-D) Script-level failure signals collected during the run.
    let script_error = eval_failed.or_else(|| ctx.bridge.script_error.lock().unwrap().clone());
    let result_json = ctx.bridge.result.lock().unwrap().clone();

    let final_status = if timed_out {
        ctx.state.set_status(WorkflowStatus::Stopped);
        WorkflowStatus::Stopped
    } else {
        let any_failed = ctx
            .state
            .agents()
            .iter()
            .any(|h| h.status() == AgentStatus::Failed);
        let s = if any_failed || script_error.is_some() {
            WorkflowStatus::Failed
        } else {
            WorkflowStatus::Completed
        };
        ctx.state.set_status(s);
        s
    };

    let mut summary = format!(
        "workflow {} {} ({} agents across {} phases)",
        ctx.wf_name,
        match final_status {
            WorkflowStatus::Completed => "completed",
            WorkflowStatus::Failed => "failed",
            WorkflowStatus::Stopped => "stopped",
            WorkflowStatus::Running => "completed",
        },
        ctx.state.agents().len(),
        ctx.state.phases.lock().unwrap().len(),
    );
    // (WF-D) Surface what the script actually produced: the parent model
    // (and the completion notification) finally see the synthesized value
    // instead of bare counts. Truncated — orchestration results are meant
    // to be summaries, not payload dumps.
    if let Some(err) = &script_error {
        summary.push_str(&format!("\n{err}"));
    } else if let Some(json) = &result_json {
        if json != "null" {
            let mut shown = json.clone();
            if shown.chars().count() > MAX_RESULT_CHARS {
                shown = shown.chars().take(MAX_RESULT_CHARS).collect::<String>() + "… (truncated)";
            }
            summary.push_str(&format!("\nresult: {shown}"));
        }
    }
    WorkflowRunResult {
        summary,
        wf_name: ctx.wf_name.clone(),
        final_status,
        result_json,
        script_error,
    }
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

pub struct WorkflowTool {
    pub mode: ModeFlag,
    pub approvals: Arc<ApprovalState>,
    pub ui: Arc<dyn ApprovalUi>,
    pub parent_depth: u8,
    pub cwd: PathBuf,
    pub registry: Arc<AgentRegistry>,
    pub workflows: Arc<WorkflowRegistry>,
    pub bash_command_wrapper: Option<Vec<String>>,
    pub libertai_cfg: Option<Arc<crate::config::Config>>,
}

impl WorkflowTool {
    #[allow(clippy::too_many_arguments)] // mirrors TaskTool::new's shape
    pub fn new(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        parent_depth: u8,
        cwd: PathBuf,
        registry: Arc<AgentRegistry>,
        workflows: Arc<WorkflowRegistry>,
        bash_command_wrapper: Option<Vec<String>>,
        libertai_cfg: Option<Arc<crate::config::Config>>,
    ) -> Self {
        Self {
            mode,
            approvals,
            ui,
            parent_depth,
            cwd,
            registry,
            workflows,
            bash_command_wrapper,
            libertai_cfg,
        }
    }
}

/// (WF-A3) Allocate a session-unique workflow run id: `wf-<pid>-<n>`.
fn next_wf_id() -> String {
    format!(
        "wf-{}-{}",
        std::process::id(),
        WF_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
    )
}

fn err_output(text: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
    .into()
}

/// The JS prelude installed before the user script. Defines `agent`,
/// `parallel`, `pipeline`, `phase`, `log` as JS wrappers around the native
/// `__wf_native_agent` / `__wf_native_log` sync functions, plus the
/// `__wf_pending` Map and `__wf_complete` settler the drive loop calls.
///
/// `parallel`/`pipeline` are expressed in JS (not Rust) so they compose
/// with `await` naturally and stay ~20 lines total — the Rust side only
/// needs the single `agent()` primitive.
const JS_PRELUDE: &str = r#"
(function () {
  const __wf_pending = new Map();
  globalThis.__wf_pending = __wf_pending;

  // Drive-loop entry point: settle the promise for `id` with `json`
  // (resolved) or throw (rejected). Called from Rust under ctx.with.
  globalThis.__wf_complete = function (id, ok, payload) {
    const slot = __wf_pending.get(id);
    if (!slot) return;
    __wf_pending.delete(id);
    if (ok) {
      slot.resolve(JSON.parse(payload));
    } else {
      slot.reject(new Error(payload));
    }
  };

  // agent(prompt, opts?) → Promise<string|object>. `opts` is an optional
  // object with `label` (display name in /workflows + agents panel),
  // `tools` (array, narrows within the agent's ceiling), `agent` (a named
  // subagent type from .claude/agents/), and `schema` (a JSON Schema —
  // the promise then resolves with the schema-validated OBJECT instead of
  // prose). The native fn returns a call_id synchronously; we stash
  // resolve/reject keyed by it and let the drive loop settle.
  globalThis.agent = function (prompt, opts) {
    opts = opts || {};
    const label = (opts && typeof opts.label === 'string') ? opts.label : null;
    const tools = (opts && Array.isArray(opts.tools)) ? opts.tools : null;
    const agentType = (opts && typeof opts.agent === 'string') ? opts.agent : null;
    const schema = (opts && opts.schema !== undefined && opts.schema !== null)
      ? JSON.stringify(opts.schema) : null;
    return new Promise(function (resolve, reject) {
      const id = __wf_native_agent(prompt, label,
        tools ? JSON.stringify(tools) : null, agentType, schema);
      __wf_pending.set(id, { resolve: resolve, reject: reject });
    });
  };

  globalThis.log = function () {
    const parts = Array.prototype.slice.call(arguments).map(String);
    __wf_native_log(parts.join(' '));
  };

  // phase(title, fn) — run fn(); the native side records the phase label
  // so agents spawned inside fn are grouped under it in /workflows.
  globalThis.phase = function (title, fn) {
    __wf_native_phase(String(title));
    return fn();
  };

  // parallel([thunks]) → Promise<results>. A barrier: awaits all. Each
  // thunk is () => Promise. Errors propagate (Promise.all semantics).
  globalThis.parallel = function (thunks) {
    return Promise.all(thunks.map(function (t) { return t(); }));
  };

  // pipeline(items, ...stages) → Promise<results>. Each item runs through
  // every stage with NO barrier between stages — item A can be in stage 3
  // while item B is still in stage 1. Each stage receives
  // (prevResult, originalItem, index). A stage that throws drops that item
  // to null and skips its remaining stages.
  globalThis.pipeline = function (items, stage1, stage2) {
    const stages = Array.prototype.slice.call(arguments, 1);
    return Promise.all(items.map(function (item, index) {
      return (async function () {
        let prev = item;
        for (const stage of stages) {
          prev = await stage(prev, item, index);
        }
        return prev;
      })().catch(function () { return null; });
    }));
  };
})();
"#;

#[async_trait]
impl Tool for WorkflowTool {
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
                "script": {
                    "type": "string",
                    "description": "JavaScript workflow body. Calls agent(prompt, {label, tools, agent, schema}), parallel([thunks]), pipeline(items, ...stages), phase(title, fn), log(...args). The body is wrapped in an async function, so top-level await is allowed; `return <value>` becomes the workflow result."
                },
                "name": {
                    "type": "string",
                    "description": "Optional human-readable name for this workflow run (shown in /workflows). Defaults to 'workflow'."
                },
                "background": {
                    "type": "boolean",
                    "description": "Default true: return immediately with the run id and receive a <task-notification> with the result when the workflow completes. Set false to block this tool call until the workflow finishes and get the result inline."
                }
            },
            "required": ["script"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        // Depth gate: a workflow's phase agents run at parent_depth+1.
        // Refuse if that would reach MAX_TASK_DEPTH (mirrors TaskTool's
        // `self.depth < MAX_TASK_DEPTH` gate — the child() +1 must keep us
        // strictly under the cap).
        if self.parent_depth + 1 >= MAX_TASK_DEPTH {
            return Ok(err_output(&format!(
                "workflow: nesting cap reached (parent depth {}, phase agents would run at {} >= MAX_TASK_DEPTH {})",
                self.parent_depth,
                self.parent_depth + 1,
                MAX_TASK_DEPTH
            )));
        }

        let script = match input.get("script").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Ok(err_output(
                    "workflow tool requires a `script` string argument",
                ))
            }
        };
        let wf_name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("workflow")
            .to_string();

        let cfg = match &self.libertai_cfg {
            Some(c) => Arc::clone(c),
            None => match crate::config::load() {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    return Ok(err_output(&format!(
                        "workflow: could not load libertai config: {e}"
                    )));
                }
            },
        };

        // Workflow id + live state, registered for /workflows. (WF-A3)
        // Unique per run — pid alone collided across runs in one session.
        let wf_id = next_wf_id();
        let state = WorkflowState::new(wf_id.clone(), wf_name.clone());
        self.workflows.register(Arc::clone(&state));

        let bridge = WorkflowBridge::new();

        // Capture the bg asupersync runtime's handle BEFORE spawning the JS
        // thread. `RuntimeHandle` is `Send` (it's designed to spawn "from any
        // thread"), so the dedicated JS thread can use it to spawn phase-
        // agent tasks back onto the bg runtime — where the subagent's I/O
        // actually runs. Without this, the JS thread (a separate OS thread)
        // couldn't reach the bg runtime.
        let bg_handle = asupersync::runtime::Runtime::current_handle();
        if bg_handle.is_none() {
            state.set_status(WorkflowStatus::Failed);
            self.workflows.remove(&wf_id);
            return Ok(err_output(
                "workflow: no asupersync runtime on this thread to host phase agents",
            ));
        }

        // (WF-E) Background is the DEFAULT (Claude Code parity): the tool
        // returns immediately with the run id and the TUI injects a
        // `<task-notification>` turn when the run settles. `background:
        // false` keeps the blocking path. Safe default because the tool is
        // only registered when the TUI injects a workflow registry — the
        // notification poller always exists.
        let background = input
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        state
            .background
            .store(background, std::sync::atomic::Ordering::Relaxed);

        // The JS QuickJS context is `!Send`, so it CANNOT live across an
        // `.await` in this `execute` future (pi's `Tool::execute` is
        // `#[async_trait]` → `Pin<Box<dyn Future + Send>>`). Mirror pi's own
        // JS-extension layer: run the QuickJS runtime on a DEDICATED OS
        // thread, and communicate via channels. The `execute` future (on the
        // bg runtime) only holds `Send` data + the channel receiver.
        //
        // (WF-E) Backgroundness is explicit: a background run gets NO
        // result channel and NO on_update (its parent tool call is Done
        // before the run settles; late ToolUpdates would reference a
        // finished call). Logs/outcome land on the WorkflowState instead.
        let (result_tx, result_rx) = if background {
            (None, None)
        } else {
            let (tx, rx) = mpsc::channel::<WorkflowRunResult>();
            (Some(tx), Some(rx))
        };
        let spawn_ctx = WorkflowRunCtx {
            script,
            wf_name: wf_name.clone(),
            cfg,
            state: Arc::clone(&state),
            bridge,
            registry: Arc::clone(&self.registry),
            mode: self.mode.clone(),
            approvals: Arc::clone(&self.approvals),
            ui: Arc::clone(&self.ui),
            parent_depth: self.parent_depth,
            cwd: self.cwd.clone(),
            bash_wrapper: self.bash_command_wrapper.clone(),
            bg_handle: bg_handle.expect("checked above"),
            on_update: if background { None } else { on_update },
        };

        // Detach the JS thread. It owns the QuickJS runtime + context,
        // runs the drive loop, and sends the final result. We don't join —
        // the channel is the synchronization. If `execute`'s future is
        // dropped (parent abort), the thread keeps running until the
        // wall-clock timeout (the in-flight subagent tasks' guards reap
        // the registry entries); a dropped `result_rx` makes `result_tx`
        // sends a no-op. The timeout env var bounds the worst case.
        std::thread::Builder::new()
            .name(format!("libertai-workflow-{wf_id}"))
            .spawn(move || run_workflow_on_thread(spawn_ctx, result_tx))
            .map_err(|e| {
                state.set_status(WorkflowStatus::Failed);
                self.workflows.remove(&wf_id);
                pi::sdk::Error::tool(NAME.to_string(), format!("workflow: thread spawn: {e}"))
            })?;

        let Some(result_rx) = result_rx else {
            // (WF-E) Background: return immediately with the run id.
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Workflow \"{wf_name}\" started in the background (id: {wf_id}). \
                     Continue with other work — you will receive a <task-notification> \
                     with the result when it completes. Live progress: /workflows."
                )))],
                details: Some(serde_json::json!({
                    "kind": "workflow_started",
                    "id": wf_id,
                    "name": wf_name,
                })),
                is_error: false,
            }
            .into());
        };

        // Await the result WITHOUT blocking the runtime thread: the bg
        // asupersync runtime is current_thread and the phase agents spawn
        // onto it via `bg_handle` — a blocking `recv()` here would freeze
        // the very executor those agents need and deadlock the run the
        // moment a script calls agent(). Poll + yield instead (the same
        // pattern the JS drive loop uses on its own runtime).
        let result = loop {
            match result_rx.try_recv() {
                Ok(r) => break r,
                Err(mpsc::TryRecvError::Empty) => {
                    asupersync::runtime::yield_now().await;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(pi::sdk::Error::tool(
                        NAME.to_string(),
                        "workflow: JS thread exited without a result".to_string(),
                    ));
                }
            }
        };
        let _ = self.workflows.snapshot();
        Ok(result.into_output())
    }
}

/// What a phase agent produced: the assistant's final prose plus, when a
/// schema was requested (WF-C), the schema-validated `structured_output`
/// payload captured from the agent's tool events.
struct PhaseAgentResult {
    text: String,
    structured: Option<serde_json::Value>,
}

/// (WF-C) Pull the validated payload out of a `structured_output`
/// `ToolExecutionEnd` event, if that's what `event` is. Pure, for tests.
fn extract_structured_data(event: &AgentEvent) -> Option<serde_json::Value> {
    match event {
        AgentEvent::ToolExecutionEnd {
            tool_name,
            result,
            is_error,
            ..
        } if tool_name == "structured_output" && !is_error => {
            let details = result.details.as_ref()?;
            if details.get("validated").and_then(|v| v.as_bool()) == Some(true) {
                details.get("data").cloned()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// (WF-C) Prompt suffix binding the phase agent to the schema contract.
fn schema_prompt_suffix(schema: &serde_json::Value) -> String {
    format!(
        "\n\n## Required output\n\
         You MUST call the `structured_output` tool exactly once before \
         finishing, passing this JSON `schema` verbatim and your result as \
         `data`. The tool validates the shape; fix and retry on validation \
         errors. Your promise to the caller is the validated object — prose \
         alone is a failure.\n\n```json\n{schema}\n```"
    )
}

/// Run one phase agent — a trimmed near-copy of `TaskTool::execute`'s
/// session-build + await path (no worktree — a documented follow-up for
/// write-capable phase agents). Tool policy, named-agent wrap, model
/// override, and schema all arrive pre-resolved in `spec` (WF-B/WF-C).
/// Returns the assistant's final text + captured structured payload, or an
/// error string.
#[allow(clippy::too_many_arguments)] // trimmed near-copy of TaskTool::execute's session-build
async fn run_phase_agent(
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    depth: u8,
    cwd: PathBuf,
    registry: Arc<AgentRegistry>,
    cfg: Arc<crate::config::Config>,
    bash_command_wrapper: Option<Vec<String>>,
    prompt: String,
    spec: PhaseAgentSpec,
) -> Result<PhaseAgentResult, String> {
    let filtered = spec.tools;
    let prompt = match &spec.schema {
        Some(schema) => format!("{prompt}{}", schema_prompt_suffix(schema)),
        None => prompt,
    };

    let mut features = crate::commands::code_factory::FactoryFeatures::cli_defaults();
    features.image = false;
    let factory = LibertaiToolFactory {
        mode: mode.clone(),
        approvals: Arc::clone(&approvals),
        ui: Arc::clone(&ui),
        depth,
        features,
        registry: Arc::clone(&registry),
        libertai_cfg: Some(Arc::clone(&cfg)),
        tool_policy: None,
        smart_approval: crate::commands::code_aux::smart_approval_from_config(Arc::clone(&cfg)),
        safe_root_override: None,
        edit_journal: Arc::new(crate::commands::code_diff::EditJournal::new()),
        team: None,
        teammate_name: None,
        bash_command_wrapper: bash_command_wrapper.clone(),
        skill_cwd: Some(cwd.clone()),
        context_snapshot: None,
        cron_store: None,
        // (M6/#15) Phase agents don't host a workflow registry (a workflow
        // nesting inside a workflow would blow past MAX_TASK_DEPTH).
        // Leave unset.
        workflows: None,
    }
    .child();

    // (WF-B2) Named-agent frontmatter can override the model.
    let model = spec.model.unwrap_or_else(|| cfg.default_code_model.clone());
    let options = build_session_options(CodeSessionConfig {
        provider: cfg.default_code_provider.clone(),
        model: model.clone(),
        working_directory: Some(cwd.clone()),
        include_cwd_in_prompt: true,
        max_tool_iterations: 25,
        tool_factory: Arc::new(factory),
        persistence: SessionPersistence::Ephemeral,
        enabled_tools: Some(filtered),
        append_system_prompt: spec.append_system_prompt,
        max_tokens: Some(DEFAULT_MAX_TOKENS),
        bash_command_wrapper: bash_command_wrapper.clone(),
        auto_compaction_enabled: cfg.code_auto_compaction_enabled,
        compaction_reserve_tokens: cfg.code_compaction_reserve_tokens,
        compaction_keep_recent_tokens: cfg.code_compaction_keep_recent_tokens,
        compaction_token_budget_compact: Some(cfg.code_compaction_token_budget_compact),
    });

    let mut handle = create_agent_session(options)
        .await
        .map_err(|e| format!("session init failed: {e}"))?;
    handle.set_max_tokens(Some(DEFAULT_MAX_TOKENS));
    let (abort_handle, abort_signal) = AbortHandle::new();
    // The handle's abort slot was already set during registration in the
    // caller; re-setting here would clobber. Instead we keep the signal
    // for prompt_with_abort and rely on the WorkflowAgentGuard for cleanup.
    let _ = abort_handle;

    // (WF-C) Capture the validated structured_output payload from the
    // agent's tool events (interior mutability — the event callback is a
    // plain Fn). The LAST successful call wins, matching "call it exactly
    // once" in the prompt contract.
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_in = Arc::clone(&captured);
    let assistant = handle
        .prompt_with_abort(prompt, abort_signal, move |event: AgentEvent| {
            if let Some(data) = extract_structured_data(&event) {
                *captured_in.lock().unwrap() = Some(data);
            }
        })
        .await
        .map_err(|e| format!("run failed: {e}"))?;
    let text = assistant
        .content
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let structured = captured.lock().unwrap().take();
    Ok(PhaseAgentResult { text, structured })
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

    /// Run `script` through the REAL engine (dedicated JS thread + drive
    /// loop) with no `agent()` calls, returning the run result. Uses the
    /// test runtime only to mint the `bg_handle` the ctx requires; the
    /// blocking `recv` is safe because a no-agent script never spawns onto
    /// that runtime.
    fn run_engine(script: &str) -> WorkflowRunResult {
        let out: Arc<Mutex<Option<WorkflowRunResult>>> = Arc::new(Mutex::new(None));
        let out_in = Arc::clone(&out);
        let script = script.to_string();
        asupersync::test_utils::run_test(move || {
            let out_in = Arc::clone(&out_in);
            let script = script.clone();
            async move {
                let bg_handle =
                    asupersync::runtime::Runtime::current_handle().expect("test runtime handle");
                let state = WorkflowState::new("wf-test-1".into(), "t".into());
                let ctx = WorkflowRunCtx {
                    script: script.to_string(),
                    wf_name: "t".into(),
                    cfg: Arc::new(crate::config::Config::default()),
                    state,
                    bridge: WorkflowBridge::new(),
                    registry: AgentRegistry::new(),
                    mode: ModeFlag::new(crate::commands::code_factory::Mode::Normal),
                    approvals: Arc::new(ApprovalState::new()),
                    ui: Arc::new(AllowingUi),
                    parent_depth: 0,
                    cwd: PathBuf::from("."),
                    bash_wrapper: None,
                    bg_handle,
                    on_update: None,
                };
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || run_workflow_on_thread(ctx, Some(tx)));
                let r = rx.recv().expect("workflow result");
                *out_in.lock().unwrap() = Some(r);
            }
        });
        let r = out.lock().unwrap().take();
        r.expect("result set")
    }

    /// (WF-A1) Regression: the async-IIFE wrapper must be VALID JS — the
    /// missing `=>` made every script a SyntaxError that still reported
    /// "completed (0 agents)". A trivial script must now complete AND its
    /// return value must be captured (WF-D).
    #[test]
    fn engine_completes_script_and_captures_result() {
        let r = run_engine("log('hello'); phase('p1', () => {}); return {ok: 1};");
        assert_eq!(
            r.final_status,
            WorkflowStatus::Completed,
            "summary: {}",
            r.summary
        );
        assert!(r.script_error.is_none(), "err: {:?}", r.script_error);
        let json = r.result_json.expect("script return captured");
        assert!(json.contains("\"ok\":1"), "result json: {json}");
        assert!(r.summary.contains("completed"));
        assert!(r.summary.contains("result:"), "summary: {}", r.summary);
    }

    /// (WF-A2/WF-D) A thrown top-level error marks the run Failed and is
    /// surfaced (previously it vanished and the run said "completed").
    #[test]
    fn engine_script_throw_marks_failed() {
        let r = run_engine("throw new Error('boom');");
        assert_eq!(r.final_status, WorkflowStatus::Failed);
        let err = r.script_error.expect("script error captured");
        assert!(err.contains("boom"), "err: {err}");
        assert!(r.summary.contains("failed"), "summary: {}", r.summary);
    }

    /// (WF-A2) A syntax error also fails the run.
    #[test]
    fn engine_syntax_error_marks_failed() {
        let r = run_engine("this is definitely not javascript ((");
        assert_eq!(r.final_status, WorkflowStatus::Failed);
        assert!(
            r.script_error.is_some(),
            "syntax error must be captured, summary: {}",
            r.summary
        );
    }

    /// (WF-B2) An unknown named agent type rejects the agent() promise at
    /// dispatch — end-to-end through the real engine, no LLM needed. The
    /// script catches the rejection, proving completion delivery works.
    #[test]
    fn engine_unknown_agent_type_rejects_promise() {
        let r = run_engine(
            "try { await agent('x', {agent: 'no-such-agent-xyz'}); return 'unexpected'; } \
             catch (e) { return 'caught: ' + String(e); }",
        );
        assert_eq!(
            r.final_status,
            WorkflowStatus::Completed,
            "summary: {}",
            r.summary
        );
        let json = r.result_json.expect("result");
        assert!(
            json.contains("caught:") && json.contains("unknown agent type"),
            "result: {json}"
        );
    }

    /// (WF-C) extract_structured_data pulls the validated payload out of a
    /// structured_output ToolExecutionEnd and nothing else.
    #[test]
    fn extract_structured_data_captures_validated_payload() {
        let mk = |tool: &str, is_error: bool, details: Option<serde_json::Value>| {
            AgentEvent::ToolExecutionEnd {
                tool_name: tool.to_string(),
                tool_call_id: "c1".to_string(),
                result: ToolOutput {
                    content: vec![],
                    details,
                    is_error,
                },
                is_error,
            }
        };
        // Happy path.
        let ev = mk(
            "structured_output",
            false,
            Some(serde_json::json!({"validated": true, "data": {"x": 1}})),
        );
        assert_eq!(
            extract_structured_data(&ev),
            Some(serde_json::json!({"x": 1}))
        );
        // Wrong tool.
        let ev = mk(
            "read",
            false,
            Some(serde_json::json!({"validated": true, "data": 1})),
        );
        assert_eq!(extract_structured_data(&ev), None);
        // Errored call.
        let ev = mk(
            "structured_output",
            true,
            Some(serde_json::json!({"validated": true, "data": 1})),
        );
        assert_eq!(extract_structured_data(&ev), None);
        // Not validated.
        let ev = mk(
            "structured_output",
            false,
            Some(serde_json::json!({"validated": false})),
        );
        assert_eq!(extract_structured_data(&ev), None);
    }

    /// (WF-B/WF-C) The prelude threads agent type + schema to the native fn.
    #[test]
    fn prelude_passes_agent_and_schema() {
        assert!(JS_PRELUDE.contains("opts.agent"));
        assert!(JS_PRELUDE.contains("opts.schema"));
        assert!(JS_PRELUDE.contains("agentType, schema"));
    }

    /// (WF-E) A background run returns `workflow_started` immediately;
    /// the run settles on its own thread, records its outcome on the
    /// state, and the notification latch fires exactly once.
    #[test]
    fn background_execute_returns_immediately_and_latches_once() {
        asupersync::test_utils::run_test(|| async {
            let workflows = WorkflowRegistry::new();
            let tool = WorkflowTool::new(
                ModeFlag::new(crate::commands::code_factory::Mode::Normal),
                Arc::new(ApprovalState::new()),
                Arc::new(AllowingUi),
                0,
                PathBuf::from("."),
                AgentRegistry::new(),
                Arc::clone(&workflows),
                None,
                Some(Arc::new(crate::config::Config::default())),
            );
            let exec = tool
                .execute(
                    "c1",
                    serde_json::json!({"script": "return 42;", "name": "bg-test"}),
                    None,
                )
                .await
                .expect("execute");
            let ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            assert!(!out.is_error);
            let details = out.details.expect("details");
            assert_eq!(details["kind"], "workflow_started");
            let id = details["id"].as_str().expect("id").to_string();

            // The JS thread settles on its own; poll the registry (bounded).
            let state = workflows
                .snapshot()
                .into_iter()
                .find(|w| w.id == id)
                .expect("registered");
            for _ in 0..200 {
                if state.status() != WorkflowStatus::Running {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(state.status(), WorkflowStatus::Completed);
            assert!(
                state
                    .outcome
                    .lock()
                    .unwrap()
                    .as_deref()
                    .is_some_and(|o| o.contains("42")),
                "outcome must carry the script result"
            );
            // Exactly-once notification latch.
            assert!(state.take_pending_notification());
            assert!(!state.take_pending_notification());
        });
    }

    /// (WF-E) `background: false` keeps the blocking path and returns the
    /// script result inline.
    #[test]
    fn sync_execute_blocks_and_returns_result_inline() {
        asupersync::test_utils::run_test(|| async {
            let workflows = WorkflowRegistry::new();
            let tool = WorkflowTool::new(
                ModeFlag::new(crate::commands::code_factory::Mode::Normal),
                Arc::new(ApprovalState::new()),
                Arc::new(AllowingUi),
                0,
                PathBuf::from("."),
                AgentRegistry::new(),
                Arc::clone(&workflows),
                None,
                Some(Arc::new(crate::config::Config::default())),
            );
            let exec = tool
                .execute(
                    "c1",
                    serde_json::json!({
                        "script": "return {answer: 7};",
                        "name": "sync-test",
                        "background": false
                    }),
                    None,
                )
                .await
                .expect("execute");
            let ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            assert!(!out.is_error);
            let details = out.details.expect("details");
            assert_eq!(details["kind"], "workflow_result");
            assert_eq!(details["status"], "completed");
            assert_eq!(details["result"]["answer"], 7);
            // A sync run never owes a notification.
            let state = &workflows.snapshot()[0];
            assert!(!state.take_pending_notification());
        });
    }

    /// (WF-A3) Run ids are unique within the session.
    #[test]
    fn wf_ids_unique_per_run() {
        let a = next_wf_id();
        let b = next_wf_id();
        assert_ne!(a, b);
        assert!(a.starts_with("wf-"));
    }

    /// (WF-A4) The registry keeps at most MAX_FINISHED_WORKFLOWS finished
    /// runs (oldest pruned), never pruning Running ones.
    #[test]
    fn registry_caps_finished_history() {
        let reg = WorkflowRegistry::new();
        let running = WorkflowState::new("wf-run".into(), "live".into());
        reg.register(Arc::clone(&running));
        for i in 0..(MAX_FINISHED_WORKFLOWS + 5) {
            let s = WorkflowState::new(format!("wf-f{i}"), "done".into());
            s.set_status(WorkflowStatus::Completed);
            reg.register(s);
        }
        let snap = reg.snapshot();
        let finished = snap
            .iter()
            .filter(|w| w.status() != WorkflowStatus::Running)
            .count();
        assert_eq!(finished, MAX_FINISHED_WORKFLOWS);
        assert!(
            snap.iter().any(|w| w.id == "wf-run"),
            "running entry must never be pruned"
        );
        // Oldest finished were pruned first.
        assert!(!snap.iter().any(|w| w.id == "wf-f0"));
        assert!(snap.iter().any(|w| w.id == "wf-f5"));
    }

    /// (WF-A6) elapsed() freezes at the terminal transition.
    #[test]
    fn elapsed_freezes_on_terminal_status() {
        let s = WorkflowState::new("wf-e".into(), "e".into());
        s.set_status(WorkflowStatus::Completed);
        let e1 = s.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert_eq!(s.elapsed(), e1, "elapsed must not advance after finish");
    }

    /// WorkflowRegistry register/snapshot/remove behaves like AgentRegistry.
    #[test]
    fn registry_register_snapshot_remove() {
        let reg = WorkflowRegistry::new();
        assert_eq!(reg.snapshot().len(), 0);
        let s1 = WorkflowState::new("wf-1".into(), "one".into());
        let s2 = WorkflowState::new("wf-2".into(), "two".into());
        reg.register(Arc::clone(&s1));
        reg.register(Arc::clone(&s2));
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].id, "wf-1");
        assert_eq!(snap[1].id, "wf-2");
        assert_eq!(reg.active_count(), 2);
        s2.set_status(WorkflowStatus::Completed);
        assert_eq!(reg.active_count(), 1);
        reg.remove("wf-1");
        assert_eq!(reg.snapshot().len(), 1);
        assert_eq!(reg.snapshot()[0].id, "wf-2");
    }

    /// WorkflowState phase grouping + agents() flattens in spawn order.
    #[test]
    fn state_phases_flatten_in_order() {
        let reg = AgentRegistry::new();
        let s = WorkflowState::new("wf-x".into(), "x".into());
        // Two phases, two agents each.
        s.phases.lock().unwrap().push(PhaseProgress {
            title: "find".into(),
            agents: vec![
                reg.register(crate::commands::code_team::AgentRegistration {
                    name: "a1".into(),
                    kind: AgentKind::Subagent {
                        depth: 1,
                        parent: None,
                    },
                    color: AgentColor::Red,
                    capability: crate::commands::code_team::AgentCapability::ReadOnly,
                    cwd: PathBuf::from("."),
                    model: "m".into(),
                    prompt_preview: "".into(),
                    parent: None,
                    pid: None,
                    log_path: None,
                }),
                reg.register(crate::commands::code_team::AgentRegistration {
                    name: "a2".into(),
                    kind: AgentKind::Subagent {
                        depth: 1,
                        parent: None,
                    },
                    color: AgentColor::Blue,
                    capability: crate::commands::code_team::AgentCapability::ReadOnly,
                    cwd: PathBuf::from("."),
                    model: "m".into(),
                    prompt_preview: "".into(),
                    parent: None,
                    pid: None,
                    log_path: None,
                }),
            ],
        });
        s.phases.lock().unwrap().push(PhaseProgress {
            title: "verify".into(),
            agents: vec![reg.register(crate::commands::code_team::AgentRegistration {
                name: "a3".into(),
                kind: AgentKind::Subagent {
                    depth: 1,
                    parent: None,
                },
                color: AgentColor::Green,
                capability: crate::commands::code_team::AgentCapability::ReadOnly,
                cwd: PathBuf::from("."),
                model: "m".into(),
                prompt_preview: "".into(),
                parent: None,
                pid: None,
                log_path: None,
            })],
        });
        let agents = s.agents();
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].name, "a1");
        assert_eq!(agents[1].name, "a2");
        assert_eq!(agents[2].name, "a3");
    }

    /// WorkflowAgentGuard reaps the registry entry on drop (abort path).
    #[test]
    fn agent_guard_reaps_on_drop() {
        let reg = AgentRegistry::new();
        let handle = reg.register(crate::commands::code_team::AgentRegistration {
            name: "g".into(),
            kind: AgentKind::Subagent {
                depth: 1,
                parent: None,
            },
            color: AgentColor::Red,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: "m".into(),
            prompt_preview: "".into(),
            parent: None,
            pid: None,
            log_path: None,
        });
        assert_eq!(reg.total_count(), 1);
        let _guard = WorkflowAgentGuard::new(Arc::clone(&handle), Arc::clone(&reg));
        // Drop the guard — entry removed, status flipped to Failed.
        drop(_guard);
        assert_eq!(reg.total_count(), 0);
        assert_eq!(handle.status(), AgentStatus::Failed);
    }

    /// WorkflowAgentGuard is a no-op when `cleaned` is set (normal path).
    #[test]
    fn agent_guard_noop_when_cleaned() {
        let reg = AgentRegistry::new();
        let handle = reg.register(crate::commands::code_team::AgentRegistration {
            name: "g2".into(),
            kind: AgentKind::Subagent {
                depth: 1,
                parent: None,
            },
            color: AgentColor::Red,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: PathBuf::from("."),
            model: "m".into(),
            prompt_preview: "".into(),
            parent: None,
            pid: None,
            log_path: None,
        });
        handle.set_status(AgentStatus::Completed);
        let mut guard = WorkflowAgentGuard::new(Arc::clone(&handle), Arc::clone(&reg));
        guard.cleaned = true;
        drop(guard);
        // Not reaped: status stays Completed (the normal path already
        // removed the entry; here we didn't, mirroring TaskTool where the
        // explicit arm removes BEFORE setting cleaned).
        assert_eq!(handle.status(), AgentStatus::Completed);
    }

    /// The depth gate refuses a workflow that would spawn phase agents at
    /// or above MAX_TASK_DEPTH. This mirrors TaskTool's gate.
    #[test]
    // Asserts on MAX_TASK_DEPTH are constant by design — they guard the gate's
    // arithmetic invariant, so they must fire if the const ever changes.
    #[allow(clippy::assertions_on_constants)]
    fn depth_gate_arithmetic() {
        // parent_depth + 1 >= MAX_TASK_DEPTH → refuse.
        assert!(MAX_TASK_DEPTH <= 3);
        // At parent_depth 2, phase agents run at 3 → 3 >= 3 → refuse.
        assert!(2u8 + 1 >= MAX_TASK_DEPTH);
        // At parent_depth 1, phase agents run at 2 → 2 >= 3 is false → ok.
        assert!(1u8 + 1 < MAX_TASK_DEPTH);
    }

    /// JS_PRELUDE defines the expected globals (smoke check on the source).
    #[test]
    fn prelude_defines_host_fns() {
        assert!(JS_PRELUDE.contains("globalThis.agent"));
        assert!(JS_PRELUDE.contains("globalThis.parallel"));
        assert!(JS_PRELUDE.contains("globalThis.pipeline"));
        assert!(JS_PRELUDE.contains("globalThis.phase"));
        assert!(JS_PRELUDE.contains("globalThis.log"));
        assert!(JS_PRELUDE.contains("globalThis.__wf_complete"));
        assert!(JS_PRELUDE.contains("__wf_native_agent"));
    }

    /// WorkflowStatus transitions are exhaustive + renderable.
    #[test]
    fn status_transitions() {
        let s = WorkflowState::new("wf-t".into(), "t".into());
        assert_eq!(s.status(), WorkflowStatus::Running);
        s.set_status(WorkflowStatus::Completed);
        assert_eq!(s.status(), WorkflowStatus::Completed);
        s.set_status(WorkflowStatus::Failed);
        assert_eq!(s.status(), WorkflowStatus::Failed);
        s.set_status(WorkflowStatus::Stopped);
        assert_eq!(s.status(), WorkflowStatus::Stopped);
    }
}
