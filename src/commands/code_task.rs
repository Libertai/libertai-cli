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
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{
    create_agent_session, AgentEvent, Result as PiResult, Tool, ToolExecution, ToolOutput,
    ToolUpdate,
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
}

impl TaskTool {
    pub fn new(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        parent_depth: u8,
        cwd: PathBuf,
        registry: Arc<AgentRegistry>,
    ) -> Self {
        Self {
            mode,
            approvals,
            ui,
            parent_depth,
            cwd,
            registry,
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
            team: None,
            teammate_name: None,
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
        let is_write_capable = !matches!(capability, crate::commands::code_team::AgentCapability::ReadOnly);
        let wants_worktree = if requested_same_cwd {
            false
        } else {
            requested_worktree
                || agent.as_ref().is_some_and(|a| a.worktree)
                || is_write_capable
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
            // Subagent bash inherits the parent's sandbox indirectly:
            // it runs through the same process, so any bwrap wrapping
            // the outer agent already wraps the nested calls too. No
            // need to plumb the argv a second time.
            bash_command_wrapper: None,
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
        let handle_arc = self.registry.register(
            crate::commands::code_team::AgentRegistration {
                name: display_name.clone(),
                kind: AgentKind::Subagent {
                    depth: self.parent_depth,
                    parent: None,
                },
                color: agent
                    .as_ref()
                    .and_then(|a| a.color)
                    .unwrap_or_else(|| crate::commands::code_team::AgentColor::color_for_name(&display_name)),
                capability,
                cwd: child_cwd.clone(),
                model: model_for_handle,
                prompt_preview,
                parent: None,
            },
        );

        let mut handle = match create_agent_session(options).await {
            Ok(h) => {
                handle_arc.set_status(AgentStatus::Working);
                h
            }
            Err(e) => {
                handle_arc.set_status(AgentStatus::Failed);
                self.registry.remove(handle_arc.id);
                return Ok(err_output(&format!("task: session init failed: {e}")));
            }
        };
        handle.set_max_tokens(max_tokens);

        let child_updates: Option<Arc<dyn Fn(ToolUpdate) + Send + Sync>> = on_update.map(Arc::from);
        let handle_for_render = Arc::clone(&handle_arc);
        let render = {
            let child_updates = child_updates.clone();
            move |event: AgentEvent| {
                update_handle_from_event(&handle_for_render, &event);
                render_child(event, child_updates.as_deref())
            }
        };
        let assistant = match handle.prompt(prompt, render).await {
            Ok(msg) => {
                handle_arc.set_status(AgentStatus::Completed);
                msg
            }
            Err(e) => {
                handle_arc.set_status(AgentStatus::Failed);
                self.registry.remove(handle_arc.id);
                return Ok(err_output(&format!("task: run failed: {e}")));
            }
        };
        self.registry.remove(handle_arc.id);

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

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
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

/// Render events from the child session with a dim `subagent:` prefix
/// so they're visually distinct from the parent's main stream.
fn render_child(event: AgentEvent, on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: pi::model::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            use std::io::Write;
            eprint!("\x1b[2m{delta}\x1b[0m");
            let _ = std::io::stderr().flush();
            send_child_update(on_update, "subagent_text_delta", &delta);
        }
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            eprintln!("\n  \x1b[2m[subagent tool] {tool_name}\x1b[0m");
            send_child_update(
                on_update,
                "subagent_tool_start",
                &format!("\n[subagent tool] {tool_name}\n"),
            );
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
                        "tool": tool_name,
                        "toolCallId": tool_call_id,
                        "isError": is_error,
                        "details": result.details,
                    })),
                });
            }
        }
        AgentEvent::AgentEnd { .. } => {
            eprintln!();
            send_child_update(on_update, "subagent_end", "\n[subagent done]\n");
        }
        _ => {}
    }
}

fn send_child_update(
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    kind: &str,
    text: &str,
) {
    let Some(on_update) = on_update else {
        return;
    };
    on_update(ToolUpdate {
        content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        details: Some(serde_json::json!({ "kind": kind })),
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

#[cfg(test)]
mod tests {
    use super::{
        named_subagent_prompt, render_child, should_skip_snapshot_entry, task_wants_same_cwd,
        task_wants_worktree, TaskWorktree,
    };
    use crate::commands::code_agents::{AgentDefinition, AgentSource};
    use pi::model::{ContentBlock, TextContent};
    use pi::sdk::{AgentEvent, ToolOutput, ToolUpdate};
    use serde_json::json;
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
        );

        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(tool_update_text(&updates[0]), "partial child read");
        assert_eq!(
            updates[0].details.as_ref().unwrap()["kind"],
            "subagent_tool_update"
        );
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
        assert_eq!(updates[1].details.as_ref().unwrap()["isError"], false);
        assert_eq!(updates[1].details.as_ref().unwrap()["details"]["bytes"], 24);
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
}
