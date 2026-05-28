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

use crate::commands::code_approvals::{ApprovalState, ApprovalUi};
use crate::commands::code_agents;
use crate::commands::code_factory::{LibertaiToolFactory, ModeFlag};
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, SessionPersistence,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::config;

const NAME: &str = "task";
const LABEL: &str = "Task";
const DESCRIPTION: &str = concat!(
    "Run a focused subtask in an isolated agent session. Use when a ",
    "piece of research or a narrow lookup is well-defined and should ",
    "not clutter the main conversation. The child runs with a fixed ",
    "read-only tool set (read, grep, find, ls) — subagents cannot ",
    "mutate the filesystem, even if the caller names other tools."
);

/// Tools a subagent is ever allowed to run, regardless of what the
/// caller passes in. This is a hard ceiling, not a default: even if a
/// compromised or prompt-injected model names `bash` or `write` in the
/// `tools` argument, those names are filtered out here and the child
/// session gets the intersection with this list.
const TASK_TOOL_ALLOWLIST: &[&str] = &["read", "grep", "find", "ls"];

pub struct TaskTool {
    mode: ModeFlag,
    approvals: Arc<ApprovalState>,
    ui: Arc<dyn ApprovalUi>,
    parent_depth: u8,
    cwd: PathBuf,
}

impl TaskTool {
    pub fn new(
        mode: ModeFlag,
        approvals: Arc<ApprovalState>,
        ui: Arc<dyn ApprovalUi>,
        parent_depth: u8,
        cwd: PathBuf,
    ) -> Self {
        Self {
            mode,
            approvals,
            ui,
            parent_depth,
            cwd,
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
                    "description": "When true, run the child in a temporary git worktree checked out at HEAD."
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
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let prompt = match input.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(err_output(
                    "task tool requires a `prompt` string argument",
                ));
            }
        };
        let subagent_type = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let wants_worktree = task_wants_worktree(&input);
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

        // Intersect the caller's tool list with our read-only allowlist.
        // A missing or empty `tools` argument falls back to the full
        // allowlist; an all-invalid list drops to the allowlist as well
        // (rather than giving the child an empty tool registry and no
        // way to research).
        let requested: Vec<String> = input
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let agent_tools = agent.as_ref().and_then(|a| a.tools.clone());
        let ceiling: Vec<String> = agent_tools
            .unwrap_or_else(|| TASK_TOOL_ALLOWLIST.iter().map(|&s| s.to_string()).collect())
            .into_iter()
            .filter(|name| TASK_TOOL_ALLOWLIST.contains(&name.as_str()))
            .collect();
        let ceiling = if ceiling.is_empty() {
            TASK_TOOL_ALLOWLIST.iter().map(|&s| s.to_string()).collect()
        } else {
            ceiling
        };
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
        let factory = LibertaiToolFactory {
            mode: self.mode.clone(),
            approvals: Arc::clone(&self.approvals),
            ui: Arc::clone(&self.ui),
            depth: self.parent_depth,
            features,
            libertai_cfg: Some(Arc::new(cfg.clone())),
        }
        .child();

        let max_tokens = Some(crate::commands::code_session::DEFAULT_MAX_TOKENS);
        let worktree = if wants_worktree {
            match TaskWorktree::create(&self.cwd) {
                Ok(worktree) => Some(worktree),
                Err(e) => {
                    return Ok(err_output(&format!(
                        "task: could not create isolated worktree: {e:#}"
                    )))
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
            append_parts.push(format!(
                "## Named sub-agent: {}\n\n{}",
                agent.name, agent.system_prompt
            ));
        }
        let append_system_prompt = if append_parts.is_empty() {
            None
        } else {
            Some(append_parts.join("\n\n"))
        };
        let append_system_prompt = crate::commands::code_env_prompt::append_environment_prompt(
            append_system_prompt,
            Some(&child_cwd),
        );
        let model = agent
            .as_ref()
            .and_then(|a| a.model.clone())
            .unwrap_or_else(|| cfg.default_code_model.clone());
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
        });

        if let Some(agent) = agent.as_ref() {
            eprintln!(
                "\n  \x1b[2m[subagent:{}] running: {prompt}\x1b[0m",
                agent.name
            );
        } else if wants_worktree {
            eprintln!(
                "\n  \x1b[2m[subagent] running in isolated worktree: {prompt}\x1b[0m"
            );
        } else {
            eprintln!("\n  \x1b[2m[subagent] running: {prompt}\x1b[0m");
        }

        let mut handle = match create_agent_session(options).await {
            Ok(h) => h,
            Err(e) => return Ok(err_output(&format!("task: session init failed: {e}"))),
        };
        handle.set_max_tokens(max_tokens);

        let assistant = match handle.prompt(prompt, render_child).await {
            Ok(msg) => msg,
            Err(e) => return Ok(err_output(&format!("task: run failed: {e}"))),
        };

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

struct TaskWorktree {
    repo_root: PathBuf,
    path: PathBuf,
    temp_root: PathBuf,
}

impl TaskWorktree {
    fn create(cwd: &Path) -> Result<Self, String> {
        let root = git_stdout(cwd, ["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(root.trim());
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
            repo_root: root,
            path,
            temp_root,
        })
    }
}

impl Drop for TaskWorktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .status();
        let _ = std::fs::remove_dir_all(&self.temp_root);
    }
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

/// Render events from the child session with a dim `subagent:` prefix
/// so they're visually distinct from the parent's main stream.
fn render_child(event: AgentEvent) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: pi::model::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            use std::io::Write;
            eprint!("\x1b[2m{delta}\x1b[0m");
            let _ = std::io::stderr().flush();
        }
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            eprintln!("\n  \x1b[2m[subagent tool] {tool_name}\x1b[0m");
        }
        AgentEvent::AgentEnd { .. } => {
            eprintln!();
        }
        _ => {}
    }
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
    use super::{task_wants_worktree, TaskWorktree};
    use serde_json::json;
    use std::process::Command;

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
