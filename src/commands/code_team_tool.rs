//! The `spawn_team` tool — agent-initiated team creation.
//!
//! Lets the agent itself decide to spawn a team of background teammates
//! when the user's request warrants parallel work. Each teammate runs
//! as a separate OS process with the `team_task` and `mailbox` tools
//! registered, sharing a task list at `.libertai/teams/<team>/tasks.jsonl`
//! and a mailbox at `.libertai/teams/<team>/mailbox/<teammate>/`.
//!
//! This closes the loop: the user asks for parallel work → the agent
//! calls `spawn_team` → teammates run in the background → visible in
//! `libertai agents` with their team affiliation and mail badges.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use pi::tools::ToolEffects;

use crate::commands::code_factory::ModeFlag;
use crate::commands::code_team::AgentRegistry;
use crate::config;

const NAME: &str = "spawn_team";
const LABEL: &str = "SpawnTeam";

const DESCRIPTION: &str = concat!(
    "Spawn a team of background teammates to work on related sub-tasks in ",
    "parallel. Each teammate runs as a separate process with a shared task ",
    "list (via the `team_task` tool) and inter-teammate messaging (via the ",
    "`mailbox` tool). Use this when the user's request has 2+ independent ",
    "sub-tasks that can proceed in parallel — e.g. 'refactor the parser AND ",
    "wire the event system AND write tests'. Each teammate should specify a ",
    "name, a named sub-agent to run as (use 'general' for the default), and ",
    "a task description. The team is visible in `libertai agents` after spawning."
);

#[derive(Debug, Clone, Deserialize)]
struct SpawnTeamInput {
    team_name: String,
    teammates: Vec<TeammateInput>,
}

#[derive(Debug, Clone, Deserialize)]
struct TeammateInput {
    name: String,
    agent: String,
    task: String,
}

pub struct SpawnTeamTool {
    cwd: PathBuf,
    mode: ModeFlag,
    registry: Arc<AgentRegistry>,
}

impl SpawnTeamTool {
    pub fn new(cwd: PathBuf, mode: ModeFlag, registry: Arc<AgentRegistry>) -> Self {
        Self {
            cwd,
            mode,
            registry,
        }
    }
}

#[async_trait]
impl Tool for SpawnTeamTool {
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
                "team_name": {
                    "type": "string",
                    "description": "A short name for the team (used for the task list dir and display)."
                },
                "teammates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Teammate display name." },
                            "agent": { "type": "string", "description": "Named sub-agent to run as (e.g. 'coder', 'reviewer'). Use 'general' for the default agent." },
                            "task": { "type": "string", "description": "The task for this teammate." }
                        },
                        "required": ["name", "agent", "task"]
                    }
                }
            },
            "required": ["team_name", "teammates"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: SpawnTeamInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `spawn_team` payload: {e}"))),
        };

        if parsed.team_name.trim().is_empty() {
            return Ok(err_output("`team_name` must not be empty"));
        }
        if parsed.teammates.is_empty() {
            return Ok(err_output("`teammates` must contain at least one teammate"));
        }
        if parsed.teammates.len() > 10 {
            return Ok(err_output("a team can have at most 10 teammates"));
        }

        // Validate each teammate spec before spawning anything.
        for t in &parsed.teammates {
            if t.name.trim().is_empty() {
                return Ok(err_output("each teammate needs a non-empty `name`"));
            }
            if t.agent.trim().is_empty() {
                return Ok(err_output(&format!(
                    "teammate `{}` needs a non-empty `agent`",
                    t.name
                )));
            }
            if t.task.trim().is_empty() {
                return Ok(err_output(&format!(
                    "teammate `{}` needs a non-empty `task`",
                    t.name
                )));
            }
        }

        // Build a TeamManifest from the input and delegate to the
        // existing spawn pipeline. We use the same code path as
        // `/team spawn` so registry registration and hook firing stay
        // in one place.
        let manifest = crate::commands::code_team_spawn::TeamManifest {
            model: None,
            provider: None,
            mode: None,
            teammates: parsed
                .teammates
                .iter()
                .map(|t| crate::commands::code_team_spawn::TeammateSpec {
                    name: t.name.clone(),
                    agent: t.agent.clone(),
                    task: t.task.clone(),
                    model: None,
                })
                .collect(),
        };

        // Initialize the shared task list first.
        let team_dir = match crate::commands::code_team_spawn::init_team_tasks(
            &parsed.team_name,
            &manifest,
            &self.cwd,
        ) {
            Ok(dir) => dir,
            Err(e) => return Ok(err_output(&format!("failed to init team task list: {e:#}"))),
        };

        // Load config for provider/model defaults. Teammates that
        // don't override model in the manifest inherit these.
        let cfg = match config::load() {
            Ok(c) => c,
            Err(e) => return Ok(err_output(&format!("failed to load config: {e:#}"))),
        };
        let provider = cfg.default_code_provider.clone();
        let model = cfg.default_code_model.clone();

        // Spawn all teammates. We pass the registry so they show up in
        // the live panel as AgentKind::Teammate.
        //
        // (Issue-1) Propagate the parent TUI's approval socket so sub-teammates
        // spawned by an agent (e.g. a teammate calling `spawn_team` to fan out
        // further) route THEIR approvals back to the same user-facing TUI
        // modal. The socket path is inherited from the env var the parent set
        // on us; if we're not running under a TUI (`LIBERTAI_APPROVAL_SOCKET`
        // unset) this is `None` and the sub-teammates auto-deny (safe).
        let approval_socket_path: Option<std::path::PathBuf> =
            std::env::var(crate::commands::code_approval_ipc::APPROVAL_SOCKET_ENV)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from);
        let spawned = match crate::commands::code_team_spawn::spawn_team(
            &parsed.team_name,
            &manifest,
            &self.cwd,
            &provider,
            &model,
            self.mode.get(),
            Some(&self.registry),
            approval_socket_path.as_deref(),
        ) {
            Ok(s) => s,
            Err(e) => return Ok(err_output(&format!("failed to spawn team: {e:#}"))),
        };

        // Build a summary the agent can see in the tool result.
        let mut summary = format!(
            "Team `{}` spawned with {} teammate(s):\n",
            parsed.team_name,
            spawned.len()
        );
        for t in &spawned {
            summary.push_str(&format!(
                "\n  {} · pid {} · run_id {}\n  task: {}\n  log: {}",
                t.name,
                t.pid,
                t.run_id,
                parsed
                    .teammates
                    .iter()
                    .find(|s| s.name == t.name)
                    .map(|s| s.task.as_str())
                    .unwrap_or(""),
                t.log_path.display()
            ));
        }
        summary.push_str(&format!(
            "\n\nTask list: {}\nTeammates can coordinate via the `team_task` and `mailbox` tools.\nThe user can monitor progress by pressing [tab] in the REPL.",
            team_dir.display()
        ));

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(summary))],
            details: None,
            is_error: false,
        }
        .into())
    }

    fn effects(&self) -> ToolEffects {
        // Spawns background processes and writes to disk.
        ToolEffects::write()
    }
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_label_are_stable() {
        assert_eq!(NAME, "spawn_team");
        assert_eq!(LABEL, "SpawnTeam");
    }

    #[test]
    fn description_mentions_parallel_and_mailbox() {
        assert!(DESCRIPTION.contains("parallel"));
        assert!(DESCRIPTION.contains("mailbox"));
        assert!(DESCRIPTION.contains("team_task"));
    }

    #[test]
    fn parameters_require_team_name_and_teammates() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "team_name": { "type": "string" },
                "teammates": { "type": "array" }
            },
            "required": ["team_name", "teammates"]
        });
        let tool_schema = SpawnTeamTool::new(
            PathBuf::from("/tmp"),
            ModeFlag::new(crate::commands::code_factory::Mode::Normal),
            AgentRegistry::new(),
        )
        .parameters();
        assert_eq!(tool_schema["required"], schema["required"]);
        assert_eq!(tool_schema["type"], "object");
    }
}
