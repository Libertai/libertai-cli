//! Claude Code-style hooks for native CLI sessions.
//!
//! The desktop has a richer hook registry. The CLI intentionally keeps
//! this surface narrow: configured shell commands, HTTP handlers,
//! stdio MCP-tool handlers, and prompt/agent handlers are executed.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pi::model::ContentBlock;
use pi::sdk::{create_agent_session, AgentEvent, ToolOutput};
use serde_json::json;

use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};
use crate::commands::code_approvals::{
    ApprovalState, ApprovalUi, PromptChoice, ToolPolicy, ToolPolicyDecision,
};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, SessionPersistence, DEFAULT_MAX_TOKENS,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::config::{Config, HookCommandConfig};

const AGENT_HOOK_TOOLS: &[&str] = &["read", "grep", "find", "ls"];

pub struct SessionHookGuard {
    cfg: Arc<Config>,
}

impl SessionHookGuard {
    pub fn start(cfg: Arc<Config>) -> Self {
        reset_once_hook_state();
        run_lifecycle_hooks(cfg.as_ref(), "SessionStart", &cfg.hooks.session_start);
        Self { cfg }
    }
}

impl Drop for SessionHookGuard {
    fn drop(&mut self) {
        run_lifecycle_hooks(self.cfg.as_ref(), "SessionEnd", &self.cfg.hooks.session_end);
    }
}

pub fn run_stop_hooks(cfg: &Config) {
    run_lifecycle_hooks(cfg, "Stop", &cfg.hooks.stop);
}

pub fn run_notification_hooks(
    cfg: &Config,
    title: &str,
    body: &str,
    outcome: &crate::commands::code_approvals::NotifyOutcome,
) {
    if !cfg.hooks.notification.iter().any(is_runnable_hook) {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = notification_payload(&cwd, title, body, outcome);
    run_nonblocking_event_hooks("Notification", &cfg.hooks.notification, &cwd, &payload);
}

/// Fire `TeammateSpawn` hooks when a teammate is spawned by `/team
/// spawn` or `/team quick`. The payload includes the team name,
/// teammate name, task, and pid.
pub fn run_teammate_spawn_hooks(
    cfg: &Config,
    team_name: &str,
    teammate_name: &str,
    task: &str,
    pid: u32,
) {
    if !cfg.hooks.teammate_spawn.iter().any(is_runnable_hook) {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = json!({
        "event": "TeammateSpawn",
        "team": team_name,
        "teammate": teammate_name,
        "task": task,
        "pid": pid,
        "cwd": cwd.display().to_string(),
    });
    run_nonblocking_event_hooks("TeammateSpawn", &cfg.hooks.teammate_spawn, &cwd, &payload);
}

/// Fire `TeamComplete` hooks when all teammates in a team have finished.
/// Called by the team spawn logic after all teammates have been launched
/// (the hooks fire on spawn completion, not on task completion — true
/// task-completion detection would require monitoring the background
/// processes, which is deferred to a future milestone).
pub fn run_team_complete_hooks(cfg: &Config, team_name: &str, teammate_count: usize) {
    if !cfg.hooks.team_complete.iter().any(is_runnable_hook) {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = json!({
        "event": "TeamComplete",
        "team": team_name,
        "teammate_count": teammate_count,
        "cwd": cwd.display().to_string(),
    });
    run_nonblocking_event_hooks("TeamComplete", &cfg.hooks.team_complete, &cwd, &payload);
}

pub fn run_user_prompt_submit_hooks(cfg: &Config, prompt: &str) -> anyhow::Result<String> {
    if !cfg.hooks.user_prompt_submit.iter().any(is_runnable_hook) {
        return Ok(prompt.to_string());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = user_prompt_submit_payload(&cwd, prompt);
    let mut contexts: Vec<String> = Vec::new();

    for (idx, hook) in cfg.hooks.user_prompt_submit.iter().enumerate() {
        if !is_runnable_hook(hook) {
            continue;
        }
        if should_skip_once_hook("UserPromptSubmit", idx, hook) {
            continue;
        }
        if hook.async_hook {
            spawn_async_hook(hook, &cwd, &payload, "UserPromptSubmit");
            continue;
        }
        let run = run_configured_hook(hook, &cwd, &payload, "UserPromptSubmit");
        if run.status != 0 && !hook.continue_on_block {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            anyhow::bail!(
                "UserPromptSubmit hook `{}` blocked the prompt: {detail}",
                hook_target(hook)
            );
        }
        if let Some(context) = user_prompt_additional_context(&run.stdout) {
            contexts.push(context);
        }
    }

    if contexts.is_empty() {
        Ok(prompt.to_string())
    } else {
        Ok(format!(
            "{prompt}\n\nAdditional context from UserPromptSubmit hook:\n\n{}",
            contexts.join("\n\n")
        ))
    }
}

fn user_prompt_submit_payload(cwd: &std::path::Path, prompt: &str) -> serde_json::Value {
    json!({
        "event": "UserPromptSubmit",
        "cwd": cwd,
        "prompt": prompt,
        "userPrompt": prompt,
        "user_prompt": prompt,
    })
}

fn run_lifecycle_hooks(cfg: &Config, event_name: &str, hooks: &[HookCommandConfig]) {
    if !hooks.iter().any(is_runnable_hook) {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = lifecycle_payload(&cwd, cfg, event_name);
    run_nonblocking_event_hooks(event_name, hooks, &cwd, &payload);
}

fn run_nonblocking_event_hooks(
    event_name: &str,
    hooks: &[HookCommandConfig],
    cwd: &std::path::Path,
    payload: &serde_json::Value,
) {
    for (idx, hook) in hooks.iter().enumerate() {
        if !is_runnable_hook(hook) {
            continue;
        }
        if should_skip_once_hook(event_name, idx, hook) {
            continue;
        }
        if hook.async_hook {
            spawn_async_hook(hook, cwd, payload, event_name);
            continue;
        }
        let run = run_configured_hook(hook, cwd, payload, event_name);
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            eprintln!(
                "  \x1b[2m[hook {event_name}] {}: {}\x1b[0m",
                hook_target(hook),
                detail
            );
        }
    }
}

fn notification_payload(
    cwd: &std::path::Path,
    title: &str,
    body: &str,
    outcome: &crate::commands::code_approvals::NotifyOutcome,
) -> serde_json::Value {
    let (status, reason) = match outcome {
        crate::commands::code_approvals::NotifyOutcome::Sent => ("sent", None),
        crate::commands::code_approvals::NotifyOutcome::Skipped(reason) => {
            ("skipped", Some(reason.as_str()))
        }
    };
    json!({
        "event": "Notification",
        "cwd": cwd,
        "title": title,
        "body": body,
        "message": body,
        "status": status,
        "outcome": status,
        "reason": reason,
    })
}

fn lifecycle_payload(cwd: &std::path::Path, cfg: &Config, event_name: &str) -> serde_json::Value {
    json!({
        "event": event_name,
        "cwd": cwd,
        "provider": cfg.default_code_provider,
        "model": cfg.default_code_model,
        "defaultCodeProvider": cfg.default_code_provider,
        "defaultCodeModel": cfg.default_code_model,
        "default_code_provider": cfg.default_code_provider,
        "default_code_model": cfg.default_code_model,
    })
}

pub fn tool_policy_from_config(cfg: Arc<Config>) -> Option<Arc<dyn ToolPolicy>> {
    if cfg.hooks.pre_tool_use.iter().any(is_runnable_hook) {
        Some(Arc::new(ConfiguredHookPolicy { cfg }))
    } else {
        None
    }
}

fn is_runnable_hook(hook: &HookCommandConfig) -> bool {
    hook.enabled
        && if hook_is_http(hook) {
            !hook.url.trim().is_empty()
        } else if hook_is_prompt(hook) || hook_is_agent(hook) {
            !hook.prompt.trim().is_empty()
        } else if hook_is_mcp_tool(hook) {
            !hook.server.trim().is_empty() && !hook.tool.trim().is_empty()
        } else if hook_is_command(hook) {
            !hook.command.trim().is_empty()
        } else {
            false
        }
}

fn should_skip_once_hook(event_name: &str, idx: usize, hook: &HookCommandConfig) -> bool {
    if !hook.once {
        return false;
    }
    let key = format!("{event_name}:{idx}");
    !once_hook_keys().lock().expect("once hook lock").insert(key)
}

fn once_hook_keys() -> &'static Mutex<HashSet<String>> {
    static KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn reset_once_hook_state() {
    once_hook_keys().lock().expect("once hook lock").clear();
}

fn hook_is_http(hook: &HookCommandConfig) -> bool {
    hook.hook_type.trim().eq_ignore_ascii_case("http")
}

fn hook_is_command(hook: &HookCommandConfig) -> bool {
    let hook_type = hook.hook_type.trim();
    hook_type.is_empty() || hook_type.eq_ignore_ascii_case("command")
}

fn hook_is_prompt(hook: &HookCommandConfig) -> bool {
    hook.hook_type.trim().eq_ignore_ascii_case("prompt")
}

fn hook_is_agent(hook: &HookCommandConfig) -> bool {
    hook.hook_type.trim().eq_ignore_ascii_case("agent")
}

fn hook_is_mcp_tool(hook: &HookCommandConfig) -> bool {
    matches!(
        hook.hook_type.trim().to_ascii_lowercase().as_str(),
        "mcp_tool" | "mcp-tool" | "mcptool"
    )
}

fn hook_target(hook: &HookCommandConfig) -> &str {
    if hook_is_http(hook) {
        hook.url.trim()
    } else if hook_is_prompt(hook) || hook_is_agent(hook) {
        hook.prompt.trim()
    } else if hook_is_mcp_tool(hook) {
        hook.tool.trim()
    } else {
        hook.command.trim()
    }
}

struct ConfiguredHookPolicy {
    cfg: Arc<Config>,
}

impl ToolPolicy for ConfiguredHookPolicy {
    fn decide(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ToolPolicyDecision {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let payload = json!({
            "event": "PreToolUse",
            "cwd": cwd,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "argsJson": serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string()),
            "tool_use_id": tool_call_id,
            "tool_name": tool_name,
            "tool_input": input,
        });

        let mut saw_allow = false;
        let mut saw_ask = false;
        let mut ask_reason: Option<String> = None;
        let mut saw_defer = false;
        let mut updated_input: Option<serde_json::Value> = None;
        let mut contexts: Vec<String> = Vec::new();
        let mut deny_reason: Option<String> = None;

        for (idx, hook) in self.cfg.hooks.pre_tool_use.iter().enumerate() {
            if !is_runnable_hook(hook)
                || !hook_matches_tool(hook, tool_name)
                || !hook_condition_matches(hook, &payload)
            {
                continue;
            }
            if should_skip_once_hook("PreToolUse", idx, hook) {
                continue;
            }
            if hook.async_hook {
                spawn_async_hook(hook, &cwd, &payload, "PreToolUse");
                continue;
            }

            let run = run_configured_hook(hook, &cwd, &payload, "PreToolUse");
            if run.status == 2 {
                deny_reason = Some(
                    first_non_empty(&run.stderr, &run.stdout)
                        .unwrap_or_else(|| "PreToolUse hook denied this tool call".to_string()),
                );
                continue;
            }

            match pre_tool_decision_from_stdout(&run.stdout) {
                ToolPolicyDecision::Deny { reason } => {
                    deny_reason =
                        Some(reason.unwrap_or_else(|| {
                            "PreToolUse hook denied this tool call".to_string()
                        }));
                }
                ToolPolicyDecision::Allow {
                    updated_input: input,
                    additional_context,
                } => {
                    saw_allow = true;
                    if let Some(input) = input {
                        updated_input = Some(input);
                    }
                    if let Some(context) = non_empty(additional_context) {
                        contexts.push(context);
                    }
                }
                ToolPolicyDecision::Ask {
                    reason,
                    updated_input: input,
                    additional_context,
                } => {
                    saw_ask = true;
                    if let Some(reason) = non_empty(reason) {
                        ask_reason = Some(reason);
                    }
                    if let Some(input) = input {
                        updated_input = Some(input);
                    }
                    if let Some(context) = non_empty(additional_context) {
                        contexts.push(context);
                    }
                }
                ToolPolicyDecision::Defer => saw_defer = true,
                ToolPolicyDecision::NoDecision => {}
            }
        }

        if let Some(reason) = deny_reason {
            ToolPolicyDecision::Deny {
                reason: Some(reason),
            }
        } else if saw_defer {
            ToolPolicyDecision::Defer
        } else if saw_ask {
            ToolPolicyDecision::Ask {
                reason: ask_reason,
                updated_input,
                additional_context: (!contexts.is_empty()).then(|| contexts.join("\n\n")),
            }
        } else if saw_allow {
            ToolPolicyDecision::Allow {
                updated_input,
                additional_context: (!contexts.is_empty()).then(|| contexts.join("\n\n")),
            }
        } else {
            ToolPolicyDecision::NoDecision
        }
    }
}

pub fn run_post_tool_hooks(cfg: &Config, event: &AgentEvent) {
    let AgentEvent::ToolExecutionEnd {
        tool_call_id,
        tool_name,
        result,
        is_error,
    } = event
    else {
        return;
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let base_payload = tool_completion_payload(&cwd, tool_call_id, tool_name, result, *is_error);
    run_tool_completion_hooks(
        "PostToolUse",
        &cfg.hooks.post_tool_use,
        &cwd,
        tool_name,
        &payload_with_event(&base_payload, "PostToolUse"),
    );
    // PostToolUseFailure: same payload, fired only when the tool errored
    // (finding #25). Lets users hook failures distinctly from successes
    // without filtering `is_error` in a PostToolUse hook.
    if *is_error {
        run_tool_completion_hooks(
            "PostToolUseFailure",
            &cfg.hooks.post_tool_use_failure,
            &cwd,
            tool_name,
            &payload_with_event(&base_payload, "PostToolUseFailure"),
        );
    }
    if tool_name == "task" {
        run_tool_completion_hooks(
            "SubagentStop",
            &cfg.hooks.subagent_stop,
            &cwd,
            tool_name,
            &payload_with_event(&base_payload, "SubagentStop"),
        );
    }
}

/// Fire `SubagentStart` hooks when a `task` tool begins executing
/// (finding #25). Mirrors `run_post_tool_hooks`'s `SubagentStop` arm
/// but on `ToolExecutionStart`. Called from the same per-event callback.
pub fn run_tool_start_hooks(cfg: &Config, event: &AgentEvent) {
    let AgentEvent::ToolExecutionStart {
        tool_call_id,
        tool_name,
        ..
    } = event
    else {
        return;
    };
    if tool_name != "task" {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = json!({
        "event": "SubagentStart",
        "cwd": cwd,
        "toolCallId": tool_call_id,
        "toolName": tool_name,
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
    });
    run_tool_completion_hooks(
        "SubagentStart",
        &cfg.hooks.subagent_start,
        &cwd,
        tool_name,
        &payload,
    );
}

/// Fire `PreCompact` hooks just before auto-compaction runs (finding #25).
/// Called from the TUI's `AutoCompactionStart` handler. Payload mirrors
/// the lifecycle-hook shape so existing hook matchers (tool matchers
/// are a no-op here; condition matchers apply) keep working.
pub fn run_pre_compact_hooks(cfg: &Config, reason: &str) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = json!({
        "event": "PreCompact",
        "cwd": cwd,
        "reason": reason,
    });
    run_tool_completion_hooks(
        "PreCompact",
        &cfg.hooks.pre_compact,
        &cwd,
        "compact",
        &payload,
    );
}

/// Fire `PostCompact` hooks just after auto-compaction settles (M6 #31).
/// Called from the TUI's `AutoCompactionEnd` handler (the per-event
/// closure, since `translate_event` is a free fn with no `&Config`).
/// `tokens_after` is `None` now: pi's `auto_compaction_result_payload`
/// (agent.rs:6603) emits `tokensBefore` only — the post-figure waits on
/// pi P3. `Option<u64>` serializes to `null` when `None`, so hook
/// scripts can read `payload.tokensAfter ?? null` today and get a real
/// value the moment pi P3 adds `tokensAfter` to the payload. `duration_ms`
/// is measured TUI-side (no pi source). `aborted` mirrors the event's
/// `aborted` flag.
pub fn run_post_compact_hooks(
    cfg: &Config,
    reason: &str,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
    duration_ms: u64,
    aborted: bool,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = json!({
        "event": "PostCompact",
        "cwd": cwd,
        "reason": reason,
        "tokensBefore": tokens_before,
        "tokensAfter": tokens_after,
        "durationMs": duration_ms,
        "aborted": aborted,
    });
    run_tool_completion_hooks(
        "PostCompact",
        &cfg.hooks.post_compact,
        &cwd,
        "compact",
        &payload,
    );
}

/// Fire `PostToolBatch` hooks when a turn's tool batch has settled
/// (finding #25). The event stream has no explicit "batch boundary", so
/// this is fired at turn end as the best-available proxy for "the batch
/// of tool calls this turn has completed". Documented as an
/// approximation; users who need exact batch semantics should use
/// PostToolUse per-call.
pub fn run_post_tool_batch_hooks(cfg: &Config) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = json!({
        "event": "PostToolBatch",
        "cwd": cwd,
    });
    run_tool_completion_hooks(
        "PostToolBatch",
        &cfg.hooks.post_tool_batch,
        &cwd,
        "batch",
        &payload,
    );
}

fn run_tool_completion_hooks(
    event_name: &str,
    hooks: &[HookCommandConfig],
    cwd: &std::path::Path,
    tool_name: &str,
    payload: &serde_json::Value,
) {
    if !hooks.iter().any(is_runnable_hook) {
        return;
    }

    for (idx, hook) in hooks.iter().enumerate() {
        if !is_runnable_hook(hook)
            || !hook_matches_tool(hook, tool_name)
            || !hook_condition_matches(hook, payload)
        {
            continue;
        }
        if should_skip_once_hook(event_name, idx, hook) {
            continue;
        }
        if hook.async_hook {
            spawn_async_hook(hook, cwd, payload, event_name);
            continue;
        }
        let run = run_configured_hook(hook, cwd, payload, event_name);
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            eprintln!(
                "  \x1b[2m[hook {event_name}] {}: {}\x1b[0m",
                hook_target(hook),
                detail
            );
        }
    }
}

fn tool_completion_payload(
    cwd: &std::path::Path,
    tool_call_id: &str,
    tool_name: &str,
    result: &ToolOutput,
    is_error: bool,
) -> serde_json::Value {
    json!({
        "cwd": cwd,
        "toolCallId": tool_call_id,
        "toolName": tool_name,
        "result": result,
        "isError": is_error,
        "tool_use_id": tool_call_id,
        "tool_name": tool_name,
        "tool_response": result,
        "is_error": is_error,
    })
}

fn payload_with_event(base: &serde_json::Value, event_name: &str) -> serde_json::Value {
    let mut payload = base.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("event".to_string(), json!(event_name));
    }
    payload
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HookRun {
    status: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCallRun {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub transport: String,
    pub timeout_ms: u64,
    pub elapsed_ms: u64,
    pub raw: Option<serde_json::Value>,
}

struct PersistentStdioMcpClient {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
    next_id: u64,
    cached_resources: HashMap<String, serde_json::Value>,
}

impl PersistentStdioMcpClient {
    fn start(server: &crate::config::McpServerConfig, timeout: Duration) -> Result<Self, String> {
        let mut cmd = Command::new(server.command.trim());
        cmd.args(&server.args)
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn MCP server: {e}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP server did not expose stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP server did not expose stdout".to_string())?;
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let _ = tx.send(trimmed.to_string());
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        write_mcp_message(
            &mut stdin,
            &mcp_initialize_request_with_roots(1, &server.roots),
        )
        .map_err(|e| format!("writing MCP initialize request: {e}"))?;
        let init_response =
            wait_for_mcp_response_with_roots(&rx, Some(&mut stdin), &server.roots, 1, timeout)
                .map_err(|e| format!("MCP initialize failed: {e}"))?;
        let supports_resource_subscriptions =
            server_supports_resource_subscriptions(&init_response);
        mcp_response_result(init_response).map_err(|e| format!("MCP initialize failed: {e}"))?;
        write_mcp_message(&mut stdin, &mcp_initialized_notification())
            .map_err(|e| format!("writing MCP initialized notification: {e}"))?;
        let next_id = if supports_resource_subscriptions {
            subscribe_stdio_resources(&mut stdin, &rx, server, 2, timeout)?
        } else {
            2
        };

        Ok(Self {
            child,
            stdin,
            rx,
            reader: Some(reader),
            next_id,
            cached_resources: HashMap::new(),
        })
    }

    fn call(
        &mut self,
        server: &crate::config::McpServerConfig,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        if method == "resources/read" {
            if let Some(uri) = params.get("uri").and_then(serde_json::Value::as_str) {
                if let Some(result) = self.cached_resources.remove(uri) {
                    return Ok(result);
                }
            }
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        write_mcp_message(
            &mut self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
        )
        .map_err(|e| format!("writing MCP {method} request: {e}"))?;
        let mut notifications = Vec::new();
        let response = wait_for_mcp_response_with_roots_and_notifications(
            &self.rx,
            Some(&mut self.stdin),
            &server.roots,
            id,
            timeout,
            &mut notifications,
        )?;
        let result = mcp_response_result(response)?;
        self.refresh_notifications(server, notifications, timeout);
        Ok(result)
    }

    fn refresh_notifications(
        &mut self,
        server: &crate::config::McpServerConfig,
        notifications: Vec<McpNotification>,
        timeout: Duration,
    ) {
        for notification in notifications {
            let McpNotification::ResourceUpdated(uri) = notification;
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            if write_mcp_message(
                &mut self.stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "resources/read",
                    "params": { "uri": uri },
                }),
            )
            .is_err()
            {
                continue;
            }
            let mut nested = Vec::new();
            let Ok(response) = wait_for_mcp_response_with_roots_and_notifications(
                &self.rx,
                Some(&mut self.stdin),
                &server.roots,
                id,
                timeout,
                &mut nested,
            ) else {
                continue;
            };
            if let Ok(result) = mcp_response_result(response) {
                self.cached_resources.insert(uri, result);
            }
        }
    }

    fn shutdown(mut self) {
        let _ = self.stdin.flush();
        drop(self.stdin);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct PersistentHttpMcpClient {
    client: reqwest::blocking::Client,
    session_id: Option<String>,
    next_id: u64,
    cached_resources: HashMap<String, serde_json::Value>,
}

impl PersistentHttpMcpClient {
    fn start(server: &crate::config::McpServerConfig, timeout: Duration) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("building MCP HTTP client: {e}"))?;
        let url = server.url.trim();
        let (init_response, session_id) = post_mcp_http_message(
            &client,
            server,
            url,
            &mcp_initialize_request_with_roots(1, &server.roots),
            None,
            &server.roots,
            1,
        )?;
        let supports_resource_subscriptions =
            server_supports_resource_subscriptions(&init_response);
        mcp_response_result(init_response).map_err(|e| format!("MCP initialize failed: {e}"))?;
        post_mcp_http_notification(
            &client,
            server,
            url,
            &mcp_initialized_notification(),
            session_id.as_deref(),
        )?;
        let (next_id, session_id) = if supports_resource_subscriptions {
            subscribe_http_resources(&client, server, url, session_id, 2)?
        } else {
            (2, session_id)
        };
        Ok(Self {
            client,
            session_id,
            next_id,
            cached_resources: HashMap::new(),
        })
    }

    fn call(
        &mut self,
        server: &crate::config::McpServerConfig,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if method == "resources/read" {
            if let Some(uri) = params.get("uri").and_then(serde_json::Value::as_str) {
                if let Some(result) = self.cached_resources.remove(uri) {
                    return Ok(result);
                }
            }
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut notifications = Vec::new();
        let (response, session_id) = post_mcp_http_message_with_notifications(
            &self.client,
            server,
            server.url.trim(),
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
            self.session_id.as_deref(),
            &server.roots,
            id,
            &mut notifications,
        )?;
        if session_id.is_some() {
            self.session_id = session_id;
        }
        let result = mcp_response_result(response)?;
        self.refresh_notifications(server, notifications);
        Ok(result)
    }

    fn refresh_notifications(
        &mut self,
        server: &crate::config::McpServerConfig,
        notifications: Vec<McpNotification>,
    ) {
        for notification in notifications {
            let McpNotification::ResourceUpdated(uri) = notification;
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            let mut nested = Vec::new();
            let Ok((response, session_id)) = post_mcp_http_message_with_notifications(
                &self.client,
                server,
                server.url.trim(),
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "resources/read",
                    "params": { "uri": uri },
                }),
                self.session_id.as_deref(),
                &server.roots,
                id,
                &mut nested,
            ) else {
                continue;
            };
            if session_id.is_some() {
                self.session_id = session_id;
            }
            if let Ok(result) = mcp_response_result(response) {
                self.cached_resources.insert(uri, result);
            }
        }
    }
}

struct PersistentSseMcpClient {
    client: reqwest::blocking::Client,
    endpoint: String,
    rx: mpsc::Receiver<SseEvent>,
    _reader: thread::JoinHandle<()>,
    next_id: u64,
    cached_resources: HashMap<String, serde_json::Value>,
}

impl PersistentSseMcpClient {
    fn start(server: &crate::config::McpServerConfig, timeout: Duration) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("building MCP SSE client: {e}"))?;
        let stream = open_mcp_sse_stream(&client, server, server.url.trim())?;
        let (rx, reader) = read_mcp_sse_stream(stream);
        let endpoint = wait_for_mcp_sse_endpoint(&rx, server.url.trim(), timeout)?;
        post_mcp_sse_message(
            &client,
            server,
            &endpoint,
            &mcp_initialize_request_with_roots(1, &server.roots),
        )?;
        let init_response = wait_for_mcp_sse_response_with_roots(
            &rx,
            Some((&client, server, endpoint.as_str())),
            &server.roots,
            1,
            timeout,
        )
        .map_err(|e| format!("MCP initialize failed: {e}"))?;
        let supports_resource_subscriptions =
            server_supports_resource_subscriptions(&init_response);
        mcp_response_result(init_response).map_err(|e| format!("MCP initialize failed: {e}"))?;
        post_mcp_sse_message(&client, server, &endpoint, &mcp_initialized_notification())?;
        let next_id = if supports_resource_subscriptions {
            subscribe_sse_resources(&client, server, &endpoint, &rx, 2, timeout)?
        } else {
            2
        };
        Ok(Self {
            client,
            endpoint,
            rx,
            _reader: reader,
            next_id,
            cached_resources: HashMap::new(),
        })
    }

    fn call(
        &mut self,
        server: &crate::config::McpServerConfig,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        if method == "resources/read" {
            if let Some(uri) = params.get("uri").and_then(serde_json::Value::as_str) {
                if let Some(result) = self.cached_resources.remove(uri) {
                    return Ok(result);
                }
            }
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        post_mcp_sse_message(
            &self.client,
            server,
            &self.endpoint,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
        )?;
        let mut notifications = Vec::new();
        let response = wait_for_mcp_sse_response_with_roots_and_notifications(
            &self.rx,
            Some((&self.client, server, self.endpoint.as_str())),
            &server.roots,
            id,
            timeout,
            &mut notifications,
        )?;
        let result = mcp_response_result(response)?;
        self.refresh_notifications(server, notifications, timeout);
        Ok(result)
    }

    fn refresh_notifications(
        &mut self,
        server: &crate::config::McpServerConfig,
        notifications: Vec<McpNotification>,
        timeout: Duration,
    ) {
        for notification in notifications {
            let McpNotification::ResourceUpdated(uri) = notification;
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            if post_mcp_sse_message(
                &self.client,
                server,
                &self.endpoint,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "resources/read",
                    "params": { "uri": uri },
                }),
            )
            .is_err()
            {
                continue;
            }
            let mut nested = Vec::new();
            let Ok(response) = wait_for_mcp_sse_response_with_roots_and_notifications(
                &self.rx,
                Some((&self.client, server, self.endpoint.as_str())),
                &server.roots,
                id,
                timeout,
                &mut nested,
            ) else {
                continue;
            };
            if let Ok(result) = mcp_response_result(response) {
                self.cached_resources.insert(uri, result);
            }
        }
    }
}

static MCP_STDIO_CLIENTS: OnceLock<Mutex<HashMap<String, PersistentStdioMcpClient>>> =
    OnceLock::new();
static MCP_HTTP_CLIENTS: OnceLock<Mutex<HashMap<String, PersistentHttpMcpClient>>> =
    OnceLock::new();
static MCP_SSE_CLIENTS: OnceLock<Mutex<HashMap<String, PersistentSseMcpClient>>> = OnceLock::new();

fn mcp_stdio_clients() -> &'static Mutex<HashMap<String, PersistentStdioMcpClient>> {
    MCP_STDIO_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mcp_http_clients() -> &'static Mutex<HashMap<String, PersistentHttpMcpClient>> {
    MCP_HTTP_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mcp_sse_clients() -> &'static Mutex<HashMap<String, PersistentSseMcpClient>> {
    MCP_SSE_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn reset_mcp_cli_sessions() -> usize {
    let mut count = 0;
    if let Ok(mut clients) = mcp_stdio_clients().lock() {
        count += clients.len();
        for (_, client) in clients.drain() {
            client.shutdown();
        }
    }
    if let Ok(mut clients) = mcp_http_clients().lock() {
        count += clients.len();
        clients.clear();
    }
    if let Ok(mut clients) = mcp_sse_clients().lock() {
        count += clients.len();
        clients.clear();
    }
    count
}

#[cfg(test)]
fn reset_mcp_cli_session_for_config(
    server_name: &str,
    server: &crate::config::McpServerConfig,
) -> bool {
    let mut removed = false;
    let stdio_key = mcp_stdio_client_key(server_name, server);
    if let Ok(mut clients) = mcp_stdio_clients().lock() {
        if let Some(client) = clients.remove(&stdio_key) {
            client.shutdown();
            removed = true;
        }
    }
    let http_key = mcp_http_client_key(server_name, server);
    if let Ok(mut clients) = mcp_http_clients().lock() {
        removed |= clients.remove(&http_key).is_some();
    }
    let sse_key = mcp_sse_client_key(server_name, server);
    if let Ok(mut clients) = mcp_sse_clients().lock() {
        removed |= clients.remove(&sse_key).is_some();
    }
    removed
}

fn spawn_async_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
    event_name: &str,
) {
    let hook = hook.clone();
    let cwd = cwd.to_path_buf();
    let payload = payload.clone();
    let event_name = event_name.to_string();
    std::thread::spawn(move || {
        let run = run_detached_hook(&hook, &cwd, &payload, &event_name);
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            eprintln!(
                "  \x1b[2m[hook {event_name} async] {}: {}\x1b[0m",
                hook_target(&hook),
                detail
            );
        }
    });
}

fn run_detached_shell_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
    event_name: &str,
) -> HookRun {
    let command = hook_command_line(hook);
    let mut cmd = shell_command(&command, hook.shell.trim());
    let spawn = cmd
        .current_dir(cwd)
        .env("LIBERTAI_HOOK_EVENT", event_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawn else {
        return HookRun {
            status: 127,
            stdout: String::new(),
            stderr: "failed to spawn hook command".to_string(),
        };
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    HookRun {
        status: 0,
        stdout: String::new(),
        stderr: String::new(),
    }
}

fn run_detached_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
    event_name: &str,
) -> HookRun {
    if hook_is_http(hook) {
        run_http_hook(hook, payload, event_name)
    } else if hook_is_mcp_tool(hook) {
        run_mcp_tool_hook(hook, payload)
    } else if hook_is_prompt(hook) {
        run_prompt_hook(hook, payload)
    } else if hook_is_agent(hook) {
        run_agent_hook(hook, payload)
    } else {
        run_detached_shell_hook(hook, cwd, payload, event_name)
    }
}

fn run_configured_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
    event_name: &str,
) -> HookRun {
    if hook_is_http(hook) {
        run_http_hook(hook, payload, event_name)
    } else if hook_is_mcp_tool(hook) {
        run_mcp_tool_hook(hook, payload)
    } else if hook_is_prompt(hook) {
        run_prompt_hook(hook, payload)
    } else if hook_is_agent(hook) {
        run_agent_hook(hook, payload)
    } else {
        run_shell_hook(hook, cwd, payload, event_name)
    }
}

fn run_http_hook(
    hook: &HookCommandConfig,
    payload: &serde_json::Value,
    event_name: &str,
) -> HookRun {
    let timeout = Duration::from_secs(hook.timeout.filter(|secs| *secs > 0).unwrap_or(30));
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("building HTTP hook client: {e}"),
            };
        }
    };

    let mut request = client
        .post(hook.url.trim())
        .header("content-type", "application/json")
        .header("x-libertai-hook-event", event_name)
        .json(payload);
    for (name, value) in &hook.headers {
        request = request.header(
            name.as_str(),
            expand_allowed_env_vars(value, &hook.allowed_env_vars),
        );
    }

    let response = match request.send() {
        Ok(response) => response,
        Err(e) => {
            return HookRun {
                status: if e.is_timeout() { 124 } else { 1 },
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };
    let status = response.status();
    let body = match response.text() {
        Ok(body) => body.trim().to_string(),
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("reading HTTP hook response: {e}"),
            };
        }
    };
    HookRun {
        status: if status.is_success() {
            0
        } else {
            i32::from(status.as_u16())
        },
        stdout: body,
        stderr: if status.is_success() {
            String::new()
        } else {
            format!("HTTP hook returned {status}")
        },
    }
}

fn run_mcp_tool_hook(hook: &HookCommandConfig, payload: &serde_json::Value) -> HookRun {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("loading MCP hook config: {e:#}"),
            };
        }
    };
    run_mcp_tool_hook_with_config(hook, payload, &cfg)
}

fn run_mcp_tool_hook_with_config(
    hook: &HookCommandConfig,
    payload: &serde_json::Value,
    cfg: &Config,
) -> HookRun {
    let server_name = hook.server.trim();
    let tool_name = hook.tool.trim();
    if server_name.is_empty() || tool_name.is_empty() {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: "MCP hook requires server and tool".to_string(),
        };
    }
    let Some(server) = cfg.mcp_servers.get(server_name) else {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP hook server `{server_name}` is not configured"),
        };
    };
    if !server.url.trim().is_empty() {
        return if server.transport.trim().eq_ignore_ascii_case("sse") {
            run_mcp_legacy_sse_tool_hook(server_name, server, hook, payload)
        } else {
            run_mcp_http_tool_hook(server_name, server, hook, payload)
        };
    }
    if server.command.trim().is_empty() {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP hook server `{server_name}` has no command"),
        };
    }

    let timeout = Duration::from_secs(hook.timeout.filter(|secs| *secs > 0).unwrap_or(30));
    let mut cmd = Command::new(server.command.trim());
    cmd.args(&server.args)
        .envs(&server.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawn = cmd.spawn();
    let Ok(mut child) = spawn else {
        return HookRun {
            status: 127,
            stdout: String::new(),
            stderr: format!("failed to spawn MCP hook server `{server_name}`"),
        };
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP hook server `{server_name}` did not expose stdin"),
        };
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP hook server `{server_name}` did not expose stdout"),
        };
    };
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = tx.send(trimmed.to_string());
                    }
                }
                Err(_) => break,
            }
        }
    });

    let input = hook.input.clone().unwrap_or_else(|| payload.clone());
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "libertai-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    });
    if let Err(e) = write_mcp_message(&mut stdin, &init) {
        let _ = child.kill();
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("writing MCP initialize request: {e}"),
        };
    }
    if let Err(e) = wait_for_mcp_response(&rx, 1, timeout).and_then(mcp_response_result) {
        let _ = child.kill();
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP initialize failed: {e}"),
        };
    }
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    });
    if let Err(e) = write_mcp_message(&mut stdin, &initialized) {
        let _ = child.kill();
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("writing MCP initialized notification: {e}"),
        };
    }
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": input,
        },
    });
    if let Err(e) = write_mcp_message(&mut stdin, &call) {
        let _ = child.kill();
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("writing MCP tools/call request: {e}"),
        };
    }
    let response = match wait_for_mcp_response(&rx, 2, timeout).and_then(mcp_response_result) {
        Ok(response) => response,
        Err(e) => {
            let _ = child.kill();
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("MCP tools/call failed: {e}"),
            };
        }
    };
    let _ = stdin.flush();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let stderr_text = stderr
        .map(|stderr| {
            let mut reader = BufReader::new(stderr);
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            text.trim().to_string()
        })
        .unwrap_or_default();
    let _ = reader.join();
    let stdout = mcp_tool_output_text(&response);
    let is_error = response
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    HookRun {
        status: if is_error { 1 } else { 0 },
        stdout,
        stderr: if is_error {
            mcp_tool_output_text(&response)
        } else {
            stderr_text
        },
    }
}

pub fn call_mcp_tool_with_config(
    cfg: &Config,
    server: &str,
    tool: &str,
    arguments: serde_json::Value,
    timeout: Option<u64>,
) -> McpToolCallRun {
    call_mcp_method_with_config(
        cfg,
        server,
        "tools/call",
        json!({
            "name": tool,
            "arguments": arguments,
        }),
        timeout,
    )
}

pub fn call_mcp_method_with_config(
    cfg: &Config,
    server_name: &str,
    method: &str,
    params: serde_json::Value,
    timeout: Option<u64>,
) -> McpToolCallRun {
    let server_name = server_name.trim();
    let method = method.trim();
    if server_name.is_empty() || method.is_empty() {
        return McpToolCallRun {
            status: 1,
            stdout: String::new(),
            stderr: "MCP call requires server and method".to_string(),
            transport: String::new(),
            timeout_ms: 0,
            elapsed_ms: 0,
            raw: None,
        };
    }
    let Some(server) = cfg.mcp_servers.get(server_name) else {
        return McpToolCallRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP server `{server_name}` is not configured"),
            transport: String::new(),
            timeout_ms: 0,
            elapsed_ms: 0,
            raw: None,
        };
    };
    let timeout = Duration::from_secs(timeout.filter(|secs| *secs > 0).unwrap_or(30));
    let started = Instant::now();
    let (transport, result) = if !server.url.trim().is_empty() {
        if server.transport.trim().eq_ignore_ascii_case("sse") {
            (
                "sse".to_string(),
                call_mcp_legacy_sse_method(server_name, server, method, params, timeout),
            )
        } else {
            (
                if server.transport.trim().is_empty() {
                    "http".to_string()
                } else {
                    server.transport.trim().to_ascii_lowercase()
                },
                call_mcp_http_method(server_name, server, method, params, timeout),
            )
        }
    } else if !server.command.trim().is_empty() {
        (
            "stdio".to_string(),
            call_mcp_stdio_method(server_name, server, method, params, timeout),
        )
    } else {
        (
            String::new(),
            Err(format!("MCP server `{server_name}` has no command or url")),
        )
    };
    match result {
        Ok(result) => McpToolCallRun {
            status: 0,
            stdout: mcp_result_output_text(&result),
            stderr: String::new(),
            transport,
            timeout_ms: timeout.as_millis() as u64,
            elapsed_ms: started.elapsed().as_millis() as u64,
            raw: Some(result),
        },
        Err(e) => McpToolCallRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP {method} failed for `{server_name}`: {e}"),
            transport,
            timeout_ms: timeout.as_millis() as u64,
            elapsed_ms: started.elapsed().as_millis() as u64,
            raw: None,
        },
    }
}

fn call_mcp_stdio_method(
    server_name: &str,
    server: &crate::config::McpServerConfig,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let key = mcp_stdio_client_key(server_name, server);
    let mut clients = mcp_stdio_clients()
        .lock()
        .map_err(|_| "MCP stdio session registry is unavailable".to_string())?;

    if !clients.contains_key(&key) {
        let client = PersistentStdioMcpClient::start(server, timeout)?;
        clients.insert(key.clone(), client);
    }

    let result = clients
        .get_mut(&key)
        .ok_or_else(|| "MCP stdio session was not registered".to_string())?
        .call(server, method, params.clone(), timeout);
    if result.is_ok() {
        return result;
    }

    if let Some(client) = clients.remove(&key) {
        client.shutdown();
    }
    let mut client = PersistentStdioMcpClient::start(server, timeout)?;
    let retry = client.call(server, method, params, timeout);
    if retry.is_ok() {
        clients.insert(key, client);
    } else {
        client.shutdown();
    }
    retry
}

fn mcp_stdio_client_key(server_name: &str, server: &crate::config::McpServerConfig) -> String {
    let mut env = server.env.iter().collect::<Vec<_>>();
    env.sort_by_key(|(left, _)| *left);
    json!({
        "name": server_name,
        "command": server.command,
        "args": server.args,
        "env": env,
        "roots": server.roots,
        "resources": subscription_uris(server),
    })
    .to_string()
}

fn call_mcp_http_method(
    server_name: &str,
    server: &crate::config::McpServerConfig,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let key = mcp_http_client_key(server_name, server);
    let mut clients = mcp_http_clients()
        .lock()
        .map_err(|_| "MCP HTTP session registry is unavailable".to_string())?;

    if !clients.contains_key(&key) {
        let client = PersistentHttpMcpClient::start(server, timeout)?;
        clients.insert(key.clone(), client);
    }

    let result = clients
        .get_mut(&key)
        .ok_or_else(|| "MCP HTTP session was not registered".to_string())?
        .call(server, method, params.clone());
    if result.is_ok() {
        return result;
    }

    clients.remove(&key);
    let mut client = PersistentHttpMcpClient::start(server, timeout)?;
    let retry = client.call(server, method, params);
    if retry.is_ok() {
        clients.insert(key, client);
    }
    retry
}

fn mcp_http_client_key(server_name: &str, server: &crate::config::McpServerConfig) -> String {
    let mut headers = server.headers.iter().collect::<Vec<_>>();
    headers.sort_by_key(|(left, _)| *left);
    json!({
        "name": server_name,
        "url": server.url,
        "transport": server.transport,
        "headers": headers,
        "roots": server.roots,
        "resources": subscription_uris(server),
    })
    .to_string()
}

fn call_mcp_legacy_sse_method(
    server_name: &str,
    server: &crate::config::McpServerConfig,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let key = mcp_sse_client_key(server_name, server);
    let mut clients = mcp_sse_clients()
        .lock()
        .map_err(|_| "MCP SSE session registry is unavailable".to_string())?;

    if !clients.contains_key(&key) {
        let client = PersistentSseMcpClient::start(server, timeout)?;
        clients.insert(key.clone(), client);
    }

    let result = clients
        .get_mut(&key)
        .ok_or_else(|| "MCP SSE session was not registered".to_string())?
        .call(server, method, params.clone(), timeout);
    if result.is_ok() {
        return result;
    }

    clients.remove(&key);
    let mut client = PersistentSseMcpClient::start(server, timeout)?;
    let retry = client.call(server, method, params, timeout);
    if retry.is_ok() {
        clients.insert(key, client);
    }
    retry
}

fn mcp_sse_client_key(server_name: &str, server: &crate::config::McpServerConfig) -> String {
    let mut headers = server.headers.iter().collect::<Vec<_>>();
    headers.sort_by_key(|(left, _)| *left);
    json!({
        "name": server_name,
        "url": server.url,
        "transport": server.transport,
        "headers": headers,
        "roots": server.roots,
        "resources": subscription_uris(server),
    })
    .to_string()
}

fn run_mcp_http_tool_hook(
    server_name: &str,
    server: &crate::config::McpServerConfig,
    hook: &HookCommandConfig,
    payload: &serde_json::Value,
) -> HookRun {
    let url = server.url.trim();
    let timeout = Duration::from_secs(hook.timeout.filter(|secs| *secs > 0).unwrap_or(30));
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("building MCP HTTP client: {e}"),
            };
        }
    };
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "libertai-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    });
    let (init_response, session_id) =
        match post_mcp_http_message(&client, server, url, &init, None, &server.roots, 1) {
            Ok(response) => response,
            Err(e) => {
                return HookRun {
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("MCP HTTP initialize failed for `{server_name}`: {e}"),
                };
            }
        };
    if let Err(e) = mcp_response_result(init_response) {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP HTTP initialize failed for `{server_name}`: {e}"),
        };
    }
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    });
    if let Err(e) =
        post_mcp_http_notification(&client, server, url, &initialized, session_id.as_deref())
    {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP HTTP initialized notification failed for `{server_name}`: {e}"),
        };
    }
    let input = hook.input.clone().unwrap_or_else(|| payload.clone());
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": hook.tool.trim(),
            "arguments": input,
        },
    });
    let (call_response, _) = match post_mcp_http_message(
        &client,
        server,
        url,
        &call,
        session_id.as_deref(),
        &server.roots,
        2,
    ) {
        Ok(response) => response,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("MCP HTTP tools/call failed for `{server_name}`: {e}"),
            };
        }
    };
    let response = match mcp_response_result(call_response) {
        Ok(response) => response,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("MCP HTTP tools/call failed for `{server_name}`: {e}"),
            };
        }
    };
    let stdout = mcp_tool_output_text(&response);
    let is_error = response
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    HookRun {
        status: if is_error { 1 } else { 0 },
        stdout,
        stderr: if is_error {
            mcp_tool_output_text(&response)
        } else {
            String::new()
        },
    }
}

fn post_mcp_http_message(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
    roots: &[String],
    id: u64,
) -> Result<(serde_json::Value, Option<String>), String> {
    post_mcp_http_message_with_notifications(
        client,
        server,
        url,
        message,
        session_id,
        roots,
        id,
        &mut Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
fn post_mcp_http_message_with_notifications(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
    roots: &[String],
    id: u64,
    notifications: &mut Vec<McpNotification>,
) -> Result<(serde_json::Value, Option<String>), String> {
    let response = send_mcp_http_request(client, server, url, message, session_id)
        .map_err(|e| e.to_string())?;
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| session_id.map(str::to_string));
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", body.trim()));
    }
    let value = if content_type.contains("text/event-stream") {
        parse_mcp_http_sse_response_with_roots_and_notifications(
            client,
            server,
            url,
            session_id.as_deref(),
            roots,
            &body,
            id,
            notifications,
        )?
    } else if body.trim().is_empty() {
        return Err("empty MCP HTTP response".to_string());
    } else {
        serde_json::from_str::<serde_json::Value>(body.trim())
            .map_err(|e| format!("invalid MCP HTTP JSON response: {e}"))?
    };
    Ok((value, session_id))
}

fn post_mcp_http_notification(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
) -> Result<(), String> {
    let response = send_mcp_http_request(client, server, url, message, session_id)
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().map_err(|e| e.to_string())?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}: {}", body.trim()))
    }
}

fn send_mcp_http_request(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
    message: &serde_json::Value,
    session_id: Option<&str>,
) -> reqwest::Result<reqwest::blocking::Response> {
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header("mcp-protocol-version", "2025-03-26")
        .json(message);
    if let Some(session_id) = session_id {
        request = request.header("mcp-session-id", session_id);
    }
    for (name, value) in &server.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request.send()
}

#[allow(clippy::too_many_arguments)]
fn parse_mcp_http_sse_response_with_roots_and_notifications(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
    session_id: Option<&str>,
    roots: &[String],
    body: &str,
    id: u64,
    notifications: &mut Vec<McpNotification>,
) -> Result<serde_json::Value, String> {
    let mut data_lines = Vec::new();
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }
    for data in data_lines {
        if data == "[DONE]" {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("invalid MCP HTTP SSE data: {e}"))?;
        if let Some(notification) = mcp_notification(&value) {
            notifications.push(notification);
            continue;
        }
        if is_roots_list_request(&value) {
            send_mcp_http_request(
                client,
                server,
                url,
                &roots_list_response(value["id"].clone(), roots),
                session_id,
            )
            .map_err(|e| e.to_string())?;
            continue;
        }
        if is_sampling_create_message_request(&value) {
            send_mcp_http_request(
                client,
                server,
                url,
                &sampling_create_message_response(value["id"].clone(), &value),
                session_id,
            )
            .map_err(|e| e.to_string())?;
            continue;
        }
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
    Err(format!("missing MCP HTTP SSE response id {id}"))
}

fn run_mcp_legacy_sse_tool_hook(
    server_name: &str,
    server: &crate::config::McpServerConfig,
    hook: &HookCommandConfig,
    payload: &serde_json::Value,
) -> HookRun {
    let url = server.url.trim();
    let timeout = Duration::from_secs(hook.timeout.filter(|secs| *secs > 0).unwrap_or(30));
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("building MCP SSE client: {e}"),
            };
        }
    };
    let stream = match open_mcp_sse_stream(&client, server, url) {
        Ok(stream) => stream,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("MCP SSE connect failed for `{server_name}`: {e}"),
            };
        }
    };
    let (rx, _reader) = read_mcp_sse_stream(stream);
    let endpoint = match wait_for_mcp_sse_endpoint(&rx, url, timeout) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("MCP SSE endpoint failed for `{server_name}`: {e}"),
            };
        }
    };
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "libertai-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    });
    if let Err(e) = post_mcp_sse_message(&client, server, &endpoint, &init) {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP SSE initialize POST failed for `{server_name}`: {e}"),
        };
    }
    if let Err(e) = wait_for_mcp_sse_response(&rx, 1, timeout).and_then(mcp_response_result) {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP SSE initialize failed for `{server_name}`: {e}"),
        };
    }
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    });
    if let Err(e) = post_mcp_sse_message(&client, server, &endpoint, &initialized) {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP SSE initialized notification failed for `{server_name}`: {e}"),
        };
    }
    let input = hook.input.clone().unwrap_or_else(|| payload.clone());
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": hook.tool.trim(),
            "arguments": input,
        },
    });
    if let Err(e) = post_mcp_sse_message(&client, server, &endpoint, &call) {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("MCP SSE tools/call POST failed for `{server_name}`: {e}"),
        };
    }
    let response = match wait_for_mcp_sse_response(&rx, 2, timeout).and_then(mcp_response_result) {
        Ok(response) => response,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("MCP SSE tools/call failed for `{server_name}`: {e}"),
            };
        }
    };
    let stdout = mcp_tool_output_text(&response);
    let is_error = response
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    HookRun {
        status: if is_error { 1 } else { 0 },
        stdout,
        stderr: if is_error {
            mcp_tool_output_text(&response)
        } else {
            String::new()
        },
    }
}

fn open_mcp_sse_stream(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
) -> Result<reqwest::blocking::Response, String> {
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .header("mcp-protocol-version", "2025-03-26");
    for (name, value) in &server.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().map_err(|e| e.to_string())?;
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        Err(format!("HTTP {status}"))
    }
}

fn post_mcp_sse_message(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    endpoint: &str,
    message: &serde_json::Value,
) -> Result<(), String> {
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header("mcp-protocol-version", "2025-03-26")
        .json(message);
    for (name, value) in &server.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().map_err(|e| e.to_string())?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}: {}", body.trim()))
    }
}

#[derive(Debug)]
struct SseEvent {
    event: String,
    data: String,
}

fn read_mcp_sse_stream(
    response: reqwest::blocking::Response,
) -> (mpsc::Receiver<SseEvent>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(response);
        let mut line = String::new();
        let mut event = String::new();
        let mut data = Vec::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        if !event.is_empty() || !data.is_empty() {
                            let _ = tx.send(SseEvent {
                                event: if event.is_empty() {
                                    "message".to_string()
                                } else {
                                    event.clone()
                                },
                                data: data.join("\n"),
                            });
                            event.clear();
                            data.clear();
                        }
                        continue;
                    }
                    if let Some(value) = trimmed.strip_prefix("event:") {
                        event = value.trim_start().to_string();
                    } else if let Some(value) = trimmed.strip_prefix("data:") {
                        data.push(value.trim_start().to_string());
                    }
                }
                Err(_) => break,
            }
        }
    });
    (rx, reader)
}

fn wait_for_mcp_sse_endpoint(
    rx: &mpsc::Receiver<SseEvent>,
    base_url: &str,
    timeout: Duration,
) -> Result<String, String> {
    let event = wait_for_mcp_sse_event(rx, timeout, |event| {
        event.event == "endpoint" || event.data.starts_with('/') || event.data.starts_with("http")
    })?;
    resolve_mcp_sse_endpoint(base_url, event.data.trim())
}

fn wait_for_mcp_sse_response(
    rx: &mpsc::Receiver<SseEvent>,
    id: u64,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    wait_for_mcp_sse_response_with_roots(rx, None, &[], id, timeout)
}

fn wait_for_mcp_sse_response_with_roots(
    rx: &mpsc::Receiver<SseEvent>,
    responder: Option<(
        &reqwest::blocking::Client,
        &crate::config::McpServerConfig,
        &str,
    )>,
    roots: &[String],
    id: u64,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    wait_for_mcp_sse_response_with_roots_and_notifications(
        rx,
        responder,
        roots,
        id,
        timeout,
        &mut Vec::new(),
    )
}

fn wait_for_mcp_sse_response_with_roots_and_notifications(
    rx: &mpsc::Receiver<SseEvent>,
    responder: Option<(
        &reqwest::blocking::Client,
        &crate::config::McpServerConfig,
        &str,
    )>,
    roots: &[String],
    id: u64,
    timeout: Duration,
    notifications: &mut Vec<McpNotification>,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for SSE event".to_string());
        }
        let event = rx
            .recv_timeout(remaining)
            .map_err(|_| "timed out waiting for SSE event".to_string())?;
        let value = serde_json::from_str::<serde_json::Value>(&event.data)
            .map_err(|e| format!("invalid MCP SSE JSON response: {e}"))?;
        if let Some(notification) = mcp_notification(&value) {
            notifications.push(notification);
            continue;
        }
        if is_roots_list_request(&value) {
            if let Some((client, server, endpoint)) = responder {
                post_mcp_sse_message(
                    client,
                    server,
                    endpoint,
                    &roots_list_response(value["id"].clone(), roots),
                )?;
            }
            continue;
        }
        if is_sampling_create_message_request(&value) {
            if let Some((client, server, endpoint)) = responder {
                post_mcp_sse_message(
                    client,
                    server,
                    endpoint,
                    &sampling_create_message_response(value["id"].clone(), &value),
                )?;
            }
            continue;
        }
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
}

fn wait_for_mcp_sse_event<F>(
    rx: &mpsc::Receiver<SseEvent>,
    timeout: Duration,
    mut matches: F,
) -> Result<SseEvent, String>
where
    F: FnMut(&SseEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for SSE event".to_string());
        }
        let event = rx
            .recv_timeout(remaining)
            .map_err(|_| "timed out waiting for SSE event".to_string())?;
        if matches(&event) {
            return Ok(event);
        }
    }
}

fn resolve_mcp_sse_endpoint(base_url: &str, endpoint: &str) -> Result<String, String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let base = url::Url::parse(base_url).map_err(|e| e.to_string())?;
    base.join(endpoint)
        .map(|url| url.to_string())
        .map_err(|e| e.to_string())
}

fn write_mcp_message<W: Write + ?Sized>(
    stdin: &mut W,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

fn mcp_initialize_request_with_roots(id: u64, roots: &[String]) -> serde_json::Value {
    let mut capabilities = serde_json::Map::new();
    capabilities.insert("sampling".to_string(), json!({}));
    if roots.iter().any(|root| !root.trim().is_empty()) {
        capabilities.insert("roots".to_string(), json!({ "listChanged": true }));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": serde_json::Value::Object(capabilities),
            "clientInfo": {
                "name": "libertai-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    })
}

fn mcp_initialized_notification() -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    })
}

fn resources_subscribe_request(id: u64, uri: &str) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "resources/subscribe",
        "params": {
            "uri": uri,
        },
    })
}

fn server_supports_resource_subscriptions(initialize_response: &serde_json::Value) -> bool {
    initialize_response
        .get("result")
        .and_then(|result| result.get("capabilities"))
        .and_then(|capabilities| capabilities.get("resources"))
        .and_then(|resources| resources.get("subscribe"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn subscription_uris(server: &crate::config::McpServerConfig) -> Vec<String> {
    let mut seen = HashSet::new();
    server
        .resources
        .iter()
        .filter(|resource| resource.enabled)
        .map(|resource| resource.uri.trim())
        .filter(|uri| !uri.is_empty())
        .filter(|uri| seen.insert((*uri).to_string()))
        .map(str::to_string)
        .collect()
}

fn subscribe_stdio_resources(
    stdin: &mut dyn Write,
    rx: &mpsc::Receiver<String>,
    server: &crate::config::McpServerConfig,
    mut next_id: u64,
    timeout: Duration,
) -> Result<u64, String> {
    for uri in subscription_uris(server) {
        let id = next_id;
        next_id = next_id.saturating_add(1);
        write_mcp_message(stdin, &resources_subscribe_request(id, &uri))
            .map_err(|e| format!("writing MCP resources/subscribe request: {e}"))?;
        let _ = wait_for_mcp_response_with_roots(rx, Some(&mut *stdin), &server.roots, id, timeout);
    }
    Ok(next_id)
}

fn subscribe_http_resources(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    url: &str,
    mut session_id: Option<String>,
    mut next_id: u64,
) -> Result<(u64, Option<String>), String> {
    for uri in subscription_uris(server) {
        let id = next_id;
        next_id = next_id.saturating_add(1);
        match post_mcp_http_message(
            client,
            server,
            url,
            &resources_subscribe_request(id, &uri),
            session_id.as_deref(),
            &server.roots,
            id,
        ) {
            Ok((response, next_session_id)) => {
                if response.get("error").is_none() && next_session_id.is_some() {
                    session_id = next_session_id;
                }
            }
            Err(_) => continue,
        }
    }
    Ok((next_id, session_id))
}

fn subscribe_sse_resources(
    client: &reqwest::blocking::Client,
    server: &crate::config::McpServerConfig,
    endpoint: &str,
    rx: &mpsc::Receiver<SseEvent>,
    mut next_id: u64,
    timeout: Duration,
) -> Result<u64, String> {
    for uri in subscription_uris(server) {
        let id = next_id;
        next_id = next_id.saturating_add(1);
        if post_mcp_sse_message(
            client,
            server,
            endpoint,
            &resources_subscribe_request(id, &uri),
        )
        .is_ok()
        {
            let _ = wait_for_mcp_sse_response_with_roots(
                rx,
                Some((client, server, endpoint)),
                &server.roots,
                id,
                timeout,
            );
        }
    }
    Ok(next_id)
}

fn wait_for_mcp_response(
    rx: &mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    wait_for_mcp_response_with_roots(rx, None, &[], id, timeout)
}

fn wait_for_mcp_response_with_roots(
    rx: &mpsc::Receiver<String>,
    stdin: Option<&mut dyn Write>,
    roots: &[String],
    id: u64,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    wait_for_mcp_response_with_roots_and_notifications(
        rx,
        stdin,
        roots,
        id,
        timeout,
        &mut Vec::new(),
    )
}

fn wait_for_mcp_response_with_roots_and_notifications(
    rx: &mpsc::Receiver<String>,
    mut stdin: Option<&mut dyn Write>,
    roots: &[String],
    id: u64,
    timeout: Duration,
    notifications: &mut Vec<McpNotification>,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for response id {id}"));
        }
        let line = rx
            .recv_timeout(remaining)
            .map_err(|_| format!("timed out waiting for response id {id}"))?;
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| format!("invalid JSON-RPC message from MCP server: {e}"))?;
        if let Some(notification) = mcp_notification(&value) {
            notifications.push(notification);
            continue;
        }
        if is_roots_list_request(&value) {
            if let Some(stdin) = stdin.as_deref_mut() {
                write_mcp_message(stdin, &roots_list_response(value["id"].clone(), roots))
                    .map_err(|e| format!("writing MCP roots/list response: {e}"))?;
            }
            continue;
        }
        if is_sampling_create_message_request(&value) {
            if let Some(stdin) = stdin.as_deref_mut() {
                write_mcp_message(
                    stdin,
                    &sampling_create_message_response(value["id"].clone(), &value),
                )
                .map_err(|e| format!("writing MCP sampling/createMessage response: {e}"))?;
            }
            continue;
        }
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
}

fn mcp_response_result(response: serde_json::Value) -> Result<serde_json::Value, String> {
    if let Some(error) = response.get("error") {
        return Err(error.to_string());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| "missing result".to_string())
}

fn is_roots_list_request(value: &serde_json::Value) -> bool {
    value.get("method").and_then(serde_json::Value::as_str) == Some("roots/list")
        && value.get("id").is_some()
        && value.get("result").is_none()
        && value.get("error").is_none()
}

fn is_sampling_create_message_request(value: &serde_json::Value) -> bool {
    value.get("method").and_then(serde_json::Value::as_str) == Some("sampling/createMessage")
        && value.get("id").is_some()
        && value.get("result").is_none()
        && value.get("error").is_none()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpNotification {
    ResourceUpdated(String),
}

fn mcp_notification(value: &serde_json::Value) -> Option<McpNotification> {
    match value.get("method").and_then(serde_json::Value::as_str) {
        Some("notifications/resources/updated") if value.get("id").is_none() => value
            .get("params")
            .and_then(|params| params.get("uri"))
            .and_then(serde_json::Value::as_str)
            .map(|uri| McpNotification::ResourceUpdated(uri.to_string())),
        _ => None,
    }
}

fn roots_list_response(id: serde_json::Value, roots: &[String]) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "roots": roots.iter()
                .filter_map(|root| root_entry(root))
                .collect::<Vec<_>>()
        }
    })
}

fn root_entry(root: &str) -> Option<serde_json::Value> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(json!({
        "uri": root_uri(trimmed),
        "name": root_name(trimmed),
    }))
}

fn root_uri(root: &str) -> String {
    if root.contains("://") {
        return root.to_string();
    }
    url::Url::from_file_path(std::path::Path::new(root))
        .map(|url| url.to_string())
        .unwrap_or_else(|_| root.to_string())
}

fn root_name(root: &str) -> String {
    if let Some(name) = std::path::Path::new(root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
    {
        return name.to_string();
    }
    root.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(root)
        .to_string()
}

fn sampling_create_message_response(
    id: serde_json::Value,
    request: &serde_json::Value,
) -> serde_json::Value {
    match run_sampling_create_message(request) {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": format!("sampling/createMessage failed: {err}"),
            },
        }),
    }
}

fn run_sampling_create_message(request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let cfg = crate::config::load().map_err(|e| format!("loading LibertAI config: {e:#}"))?;
    run_sampling_create_message_with_config(&cfg, request)
}

fn run_sampling_create_message_with_config(
    cfg: &Config,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let params = request
        .get("params")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "missing params".to_string())?;
    let max_tokens = params
        .get("maxTokens")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "missing required maxTokens".to_string())?
        .min(u64::from(u32::MAX)) as u32;
    let model = sampling_model(params, &cfg.default_code_model);
    let messages = sampling_chat_messages(params)?;
    let req = ChatRequest {
        model: model.clone(),
        messages,
        stream: Some(false),
        max_tokens: Some(max_tokens),
    };
    let resp = post_chat_blocking(cfg, &req)
        .map_err(|e| format!("sampling chat request failed: {e:#}"))?;
    let body = resp
        .json::<serde_json::Value>()
        .map_err(|e| format!("parsing sampling /v1/chat/completions response: {e}"))?;
    let text = prompt_hook_content(&body)
        .ok_or_else(|| "response missing choices[0].message.content".to_string())?;
    Ok(json!({
        "role": "assistant",
        "content": {
            "type": "text",
            "text": text,
        },
        "model": model,
        "stopReason": sampling_stop_reason(&body),
    }))
}

fn sampling_model(
    params: &serde_json::Map<String, serde_json::Value>,
    default_model: &str,
) -> String {
    params
        .get("modelPreferences")
        .and_then(|prefs| prefs.get("hints"))
        .and_then(serde_json::Value::as_array)
        .and_then(|hints| {
            hints
                .iter()
                .find_map(|hint| hint.get("name").and_then(serde_json::Value::as_str))
        })
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(default_model)
        .to_string()
}

fn sampling_chat_messages(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<ChatMessage>, String> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = params
        .get("systemPrompt")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string().into(),
        });
    }
    let source = params
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;
    for message in source {
        let role = match message.get("role").and_then(serde_json::Value::as_str) {
            Some("assistant") => "assistant",
            _ => "user",
        };
        messages.push(ChatMessage {
            role: role.to_string(),
            content: sampling_content_text(message.get("content").unwrap_or(message)).into(),
        });
    }
    if messages.is_empty() {
        return Err("missing messages".to_string());
    }
    Ok(messages)
}

fn sampling_content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(sampling_content_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(object) => {
            match object.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => object
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Some("image") => {
                    let mime = object
                        .get("mimeType")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("image");
                    let bytes = object
                        .get("data")
                        .and_then(serde_json::Value::as_str)
                        .map(str::len)
                        .unwrap_or(0);
                    format!("[MCP sampling image content: {mime}, {bytes} base64 bytes]")
                }
                Some("audio") => {
                    let mime = object
                        .get("mimeType")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("audio");
                    let bytes = object
                        .get("data")
                        .and_then(serde_json::Value::as_str)
                        .map(str::len)
                        .unwrap_or(0);
                    format!("[MCP sampling audio content: {mime}, {bytes} base64 bytes]")
                }
                _ => compact_json(content),
            }
        }
        _ => compact_json(content),
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn sampling_stop_reason(body: &serde_json::Value) -> String {
    match body
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("stop")
    {
        "length" => "maxTokens",
        "tool_calls" | "function_call" => "toolUse",
        "content_filter" => "contentFilter",
        _ => "endTurn",
    }
    .to_string()
}

fn mcp_tool_output_text(result: &serde_json::Value) -> String {
    result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| block.as_str())
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| result.to_string())
}

fn mcp_result_output_text(result: &serde_json::Value) -> String {
    if result.get("content").is_some() {
        return mcp_tool_output_text(result);
    }
    if let Some(messages) = result.get("messages").and_then(serde_json::Value::as_array) {
        let text = messages
            .iter()
            .filter_map(|message| message.get("content"))
            .filter_map(|content| {
                content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| content.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return text;
        }
    }
    result.to_string()
}

fn expand_allowed_env_vars(value: &str, allowed_env_vars: &[String]) -> String {
    let mut out = value.to_string();
    for name in allowed_env_vars {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if let Ok(env_value) = std::env::var(name) {
            out = out.replace(&format!("${name}"), &env_value);
            out = out.replace(&format!("${{{name}}}"), &env_value);
        }
    }
    out
}

fn run_prompt_hook(hook: &HookCommandConfig, payload: &serde_json::Value) -> HookRun {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("loading prompt hook config: {e:#}"),
            };
        }
    };
    let model = if hook.model.trim().is_empty() {
        cfg.default_code_model.clone()
    } else {
        hook.model.trim().to_string()
    };
    let req = ChatRequest {
        model,
        messages: prompt_hook_messages(&hook.prompt, payload),
        stream: Some(false),
        max_tokens: None,
    };
    let resp = match post_chat_blocking(&cfg, &req) {
        Ok(resp) => resp,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("prompt hook request failed: {e:#}"),
            };
        }
    };
    let body = match resp.json::<serde_json::Value>() {
        Ok(body) => body,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("parsing prompt hook response: {e}"),
            };
        }
    };
    match prompt_hook_content(&body) {
        Some(content) => HookRun {
            status: 0,
            stdout: content,
            stderr: String::new(),
        },
        None => HookRun {
            status: 1,
            stdout: String::new(),
            stderr: "prompt hook response missing choices[0].message.content".to_string(),
        },
    }
}

fn run_agent_hook(hook: &HookCommandConfig, payload: &serde_json::Value) -> HookRun {
    let cfg = match crate::config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("loading agent hook config: {e:#}"),
            };
        }
    };
    crate::commands::code_session::ensure_pi_http_timeout(cfg.http_timeout_secs);
    if let Err(e) = crate::commands::code_models::ensure_libertai_registered(&cfg) {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("registering agent hook model provider: {e:#}"),
        };
    }
    if let Err(e) = crate::commands::code_memory::ensure_memory_env() {
        return HookRun {
            status: 1,
            stdout: String::new(),
            stderr: format!("preparing agent hook memory environment: {e:#}"),
        };
    }

    let reactor = match asupersync::runtime::reactor::create_reactor() {
        Ok(reactor) => reactor,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("creating agent hook reactor: {e}"),
            };
        }
    };
    let runtime = match asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("creating agent hook runtime: {e}"),
            };
        }
    };

    let hook = hook.clone();
    let cfg = Arc::new(cfg);
    let payload = payload.clone();
    runtime.block_on(async move { run_agent_hook_async(&hook, payload, cfg).await })
}

async fn run_agent_hook_async(
    hook: &HookCommandConfig,
    payload: serde_json::Value,
    cfg: Arc<Config>,
) -> HookRun {
    let cwd = std::env::current_dir().ok();
    let append_system_prompt =
        match code_skills::prompt_for_pillar(SkillPillar::Code, cwd.as_deref()) {
            Ok(prompt) => prompt,
            Err(e) => {
                return HookRun {
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("loading agent hook skills: {e:#}"),
                };
            }
        };
    let append_system_prompt = crate::commands::code_identity_prompt::apply(append_system_prompt);
    // Git context is injected once by pi (build_git_context); do not duplicate it here.
    let prompt = prompt_hook_user_content(&hook.prompt, &payload);
    let model = if hook.model.trim().is_empty() {
        cfg.default_code_model.clone()
    } else {
        hook.model.trim().to_string()
    };
    let approvals = Arc::new(ApprovalState::new());
    let ui = Arc::new(HookApprovalUi);
    let factory = Arc::new(LibertaiToolFactory::new_with_features(
        ModeFlag::new(Mode::Plan),
        approvals,
        ui,
        FactoryFeatures::cli_defaults(),
        Some(Arc::clone(&cfg)),
    ));
    let max_tokens = Some(DEFAULT_MAX_TOKENS);
    let options = build_session_options(CodeSessionConfig {
        provider: cfg.default_code_provider.clone(),
        model,
        working_directory: cwd.clone(),
        include_cwd_in_prompt: true,
        max_tool_iterations: 25,
        tool_factory: factory,
        persistence: SessionPersistence::Ephemeral,
        enabled_tools: Some(
            AGENT_HOOK_TOOLS
                .iter()
                .map(|tool| tool.to_string())
                .collect(),
        ),
        append_system_prompt,
        max_tokens,
        // (M4/#23) Hook-spawned agents run OUT-OF-BAND — not as children of
        // a running `--sandbox=strict` code session — so there's no parent
        // bash wrapper to inherit. The sandbox mode comes from the CLI
        // `--sandbox` flag (not the persisted `LibertaiConfig`), which the
        // hook path doesn't carry, so deriving one here would either always
        // be `Off` (→ None, what we set) or guess a mode the user didn't
        // choose. Left `None` deliberately; revisit if hooks gain a sandbox
        // knob.
        bash_command_wrapper: None,
        auto_compaction_enabled: cfg.code_auto_compaction_enabled,
        compaction_reserve_tokens: cfg.code_compaction_reserve_tokens,
        compaction_keep_recent_tokens: cfg.code_compaction_keep_recent_tokens,
        compaction_token_budget_compact: Some(cfg.code_compaction_token_budget_compact),
    });
    let mut handle = match create_agent_session(options).await {
        Ok(handle) => handle,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("creating agent hook session: {e}"),
            };
        }
    };
    handle.set_max_tokens(max_tokens);
    let msg = match handle.prompt(prompt, |_| {}).await {
        Ok(msg) => msg,
        Err(e) => {
            return HookRun {
                status: 1,
                stdout: String::new(),
                stderr: format!("running agent hook session: {e}"),
            };
        }
    };
    let stdout = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if stdout.is_empty() {
        HookRun {
            status: 1,
            stdout,
            stderr: "agent hook response was empty".to_string(),
        }
    } else {
        HookRun {
            status: 0,
            stdout,
            stderr: String::new(),
        }
    }
}

struct HookApprovalUi;

#[async_trait]
impl ApprovalUi for HookApprovalUi {
    async fn decide(&self, _tool_name: &str, _preview: &str, _always_rule: &str) -> PromptChoice {
        PromptChoice::Deny
    }
}

fn prompt_hook_messages(prompt: &str, payload: &serde_json::Value) -> Vec<ChatMessage> {
    let content = prompt_hook_user_content(prompt, payload);
    vec![
        ChatMessage {
            role: "system".to_string(),
            content: "You are running as a Claude Code-style hook handler. Return only the hook output. For PreToolUse decisions, return valid JSON using permissionDecision and optional permissionDecisionReason, updatedInput, and additionalContext fields.".to_string().into(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: content.into(),
        },
    ]
}

fn prompt_hook_user_content(prompt: &str, payload: &serde_json::Value) -> String {
    let payload = serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string());
    format!(
        "{}\n\nHook event payload:\n```json\n{}\n```",
        prompt.trim(),
        payload
    )
}

fn prompt_hook_content(body: &serde_json::Value) -> Option<String> {
    body.get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|content| !content.is_empty())
        .map(str::to_string)
}

fn run_shell_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
    event_name: &str,
) -> HookRun {
    let command = hook_command_line(hook);
    let mut cmd = shell_command(&command, hook.shell.trim());
    let spawn = cmd
        .current_dir(cwd)
        .env("LIBERTAI_HOOK_EVENT", event_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = spawn else {
        return HookRun {
            status: 127,
            stdout: String::new(),
            stderr: "failed to spawn hook command".to_string(),
        };
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }

    if let Some(timeout) = hook
        .timeout
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
    {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    return match child.wait_with_output() {
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                            let timeout_msg =
                                format!("hook timed out after {}s", timeout.as_secs());
                            HookRun {
                                status: 124,
                                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                                stderr: if stderr.is_empty() {
                                    timeout_msg
                                } else {
                                    format!("{stderr}\n{timeout_msg}")
                                },
                            }
                        }
                        Err(e) => HookRun {
                            status: 124,
                            stdout: String::new(),
                            stderr: format!(
                                "hook timed out after {}s; failed to collect output: {e}",
                                timeout.as_secs()
                            ),
                        },
                    };
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    return HookRun {
                        status: 1,
                        stdout: String::new(),
                        stderr: e.to_string(),
                    };
                }
            }
        }
    }

    match child.wait_with_output() {
        Ok(output) => HookRun {
            status: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Err(e) => HookRun {
            status: 1,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

fn shell_command(command: &str, shell: &str) -> Command {
    let mut cmd = if shell.is_empty() {
        if cfg!(windows) {
            Command::new("cmd")
        } else {
            Command::new("sh")
        }
    } else {
        Command::new(shell)
    };
    let shell_name = if shell.is_empty() {
        if cfg!(windows) {
            "cmd"
        } else {
            "sh"
        }
    } else {
        std::path::Path::new(shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(shell)
    };
    if shell_name.eq_ignore_ascii_case("cmd") || shell_name.eq_ignore_ascii_case("cmd.exe") {
        cmd.args(["/C", command]);
    } else {
        cmd.args(["-c", command]);
    }
    cmd
}

pub fn hook_command_display(hook: &HookCommandConfig) -> String {
    hook_command_line(hook)
}

fn hook_command_line(hook: &HookCommandConfig) -> String {
    command_line_from_parts(&hook.command, &hook.args)
}

fn command_line_from_parts(command: &str, args: &[String]) -> String {
    let mut parts = Vec::new();
    if !command.trim().is_empty() {
        parts.push(command.to_string());
    }
    for arg in args {
        parts.push(shell_quote_arg(arg));
    }
    parts.join(" ")
}

fn shell_quote_arg(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn hook_matches_tool(hook: &HookCommandConfig, tool_name: &str) -> bool {
    let matcher = hook.matcher.trim();
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    matcher_alternatives(matcher)
        .into_iter()
        .any(|part| matcher_part_matches(&part, tool_name))
}

fn hook_condition_matches(hook: &HookCommandConfig, payload: &serde_json::Value) -> bool {
    let condition = hook.if_condition.trim();
    if condition.is_empty() {
        return true;
    }

    let tool_name = payload
        .get("toolName")
        .or_else(|| payload.get("tool_name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let Some((tool_pattern, arg_pattern)) = parse_tool_condition(condition) else {
        return false;
    };
    if !tool_name_matches(tool_pattern, tool_name) {
        return false;
    }
    let Some(arg_pattern) = arg_pattern else {
        return true;
    };
    let Some(value) = condition_input_value(tool_name, payload) else {
        return false;
    };
    wildcard_match(arg_pattern, &value)
}

fn parse_tool_condition(condition: &str) -> Option<(&str, Option<&str>)> {
    if let Some(open) = condition.find('(') {
        if condition.ends_with(')') && open > 0 {
            let tool = condition[..open].trim();
            let args = condition[open + 1..condition.len() - 1].trim();
            if !tool.is_empty() {
                return Some((tool, Some(args)));
            }
        }
        return None;
    }
    Some((condition.trim(), None)).filter(|(tool, _)| !tool.is_empty())
}

fn tool_name_matches(pattern: &str, tool_name: &str) -> bool {
    matcher_alternatives(pattern).into_iter().any(|part| {
        let native = claude_tool_alias(&part).unwrap_or(part.as_str());
        native.eq_ignore_ascii_case(tool_name) || matcher_part_matches(&part, tool_name)
    })
}

fn claude_tool_alias(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "bash" => Some("bash"),
        "read" => Some("read"),
        "write" => Some("write"),
        "edit" | "multiedit" => Some("edit"),
        "grep" => Some("grep"),
        "glob" | "find" => Some("find"),
        "ls" => Some("ls"),
        _ => None,
    }
}

fn condition_input_value(tool_name: &str, payload: &serde_json::Value) -> Option<String> {
    if let Some(input) = payload
        .get("tool_input")
        .or_else(|| payload.get("toolInput"))
    {
        let tool = tool_name.to_ascii_lowercase();
        for key in condition_input_keys(&tool) {
            if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
                return Some(value.to_string());
            }
        }
        return Some(input.to_string());
    }
    let args_json = payload
        .get("argsJson")
        .or_else(|| payload.get("args_json"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)?;
    if let Ok(input) = serde_json::from_str::<serde_json::Value>(&args_json) {
        let tool = tool_name.to_ascii_lowercase();
        for key in condition_input_keys(&tool) {
            if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
                return Some(value.to_string());
            }
        }
    }
    Some(args_json)
}

fn condition_input_keys(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "bash" => &["command"],
        "read" | "write" | "edit" | "hashline_edit" | "notebook_read" | "notebook_edit"
        | "notebook_execute" => &["path", "file_path", "filePath", "notebook_path"],
        "grep" => &["pattern"],
        "find" => &["pattern", "path"],
        _ => &["path", "command", "pattern"],
    }
}

fn matcher_part_matches(part: &str, tool_name: &str) -> bool {
    let part = part.trim();
    if part.is_empty() {
        return false;
    }
    if part == "*" {
        return true;
    }
    if let Some(pattern) = part.strip_prefix("regex:") {
        return regex_matches(pattern, tool_name);
    }
    if let Some(pattern) = slash_regex_pattern(part) {
        return regex_matches(pattern, tool_name);
    }
    part == tool_name || (part.contains('*') && wildcard_match(part, tool_name))
}

fn matcher_alternatives(matcher: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_slash_regex = false;
    let mut escaped = false;

    for ch in matcher.chars() {
        if ch == '|' && !in_slash_regex {
            let part = current.trim();
            if !part.is_empty() {
                parts.push(part.to_string());
            }
            current.clear();
            escaped = false;
            continue;
        }

        if ch == '/' && !escaped {
            if in_slash_regex {
                in_slash_regex = false;
            } else if current.trim().is_empty() {
                in_slash_regex = true;
            }
        }

        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
        current.push(ch);
    }

    let part = current.trim();
    if !part.is_empty() {
        parts.push(part.to_string());
    }
    parts
}

fn slash_regex_pattern(part: &str) -> Option<&str> {
    let part = part.trim();
    if !part.starts_with('/') || !part.ends_with('/') || part.len() < 2 {
        return None;
    }
    Some(&part[1..part.len() - 1])
}

fn regex_matches(pattern: &str, tool_name: &str) -> bool {
    regex::Regex::new(pattern)
        .map(|regex| regex.is_match(tool_name))
        .unwrap_or(false)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let mut rest = value;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }
        let Some(idx) = rest.find(part) else {
            return false;
        };
        if first && !pattern.starts_with('*') && idx != 0 {
            return false;
        }
        rest = &rest[idx + part.len()..];
        first = false;
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn pre_tool_decision_from_stdout(stdout: &str) -> ToolPolicyDecision {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return ToolPolicyDecision::NoDecision;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return ToolPolicyDecision::NoDecision;
    };
    let specific = value.get("hookSpecificOutput").unwrap_or(&value);
    let decision = specific
        .get("permissionDecision")
        .or_else(|| value.get("permissionDecision"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match decision.as_str() {
        "allow" => ToolPolicyDecision::Allow {
            updated_input: specific
                .get("updatedInput")
                .or_else(|| value.get("updatedInput"))
                .cloned(),
            additional_context: string_field(specific, &value, "additionalContext"),
        },
        "ask" => ToolPolicyDecision::Ask {
            reason: string_field(specific, &value, "permissionDecisionReason")
                .or_else(|| string_field(specific, &value, "reason")),
            updated_input: specific
                .get("updatedInput")
                .or_else(|| value.get("updatedInput"))
                .cloned(),
            additional_context: string_field(specific, &value, "additionalContext"),
        },
        "defer" => ToolPolicyDecision::Defer,
        "deny" => ToolPolicyDecision::Deny {
            reason: string_field(specific, &value, "permissionDecisionReason")
                .or_else(|| string_field(specific, &value, "reason")),
        },
        _ => ToolPolicyDecision::NoDecision,
    }
}

fn user_prompt_additional_context(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return None;
    };
    let specific = value.get("hookSpecificOutput").unwrap_or(&value);
    non_empty(string_field(specific, &value, "additionalContext"))
}

fn string_field(
    specific: &serde_json::Value,
    value: &serde_json::Value,
    field: &str,
) -> Option<String> {
    specific
        .get(field)
        .or_else(|| value.get(field))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn first_non_empty(left: &str, right: &str) -> Option<String> {
    [left, right]
        .into_iter()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HooksConfig, McpResourceConfig, McpServerConfig};
    use std::collections::HashMap;

    #[test]
    fn agent_hook_tool_allowlist_is_read_only() {
        assert_eq!(AGENT_HOOK_TOOLS, &["read", "grep", "find", "ls"]);
    }

    #[test]
    fn prompt_hook_user_content_is_shared_by_prompt_and_agent_hooks() {
        let content = prompt_hook_user_content(
            "Inspect the hook payload",
            &json!({"event":"PreToolUse","toolName":"read"}),
        );
        assert!(content.starts_with("Inspect the hook payload"));
        assert!(content.contains("Hook event payload:"));
        assert!(content.contains("\"toolName\": \"read\""));
    }

    #[test]
    fn mcp_tool_output_text_reads_text_blocks() {
        let result = json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ]
        });
        assert_eq!(mcp_tool_output_text(&result), "first\nsecond");
    }

    #[test]
    fn mcp_result_output_text_reads_prompt_messages() {
        let result = json!({
            "messages": [
                {
                    "role": "user",
                    "content": { "type": "text", "text": "summarize this" }
                },
                {
                    "role": "assistant",
                    "content": { "type": "text", "text": "ok" }
                }
            ]
        });
        assert_eq!(mcp_result_output_text(&result), "summarize this\nok");
    }

    #[test]
    fn mcp_initialize_advertises_roots_when_configured() {
        let init = mcp_initialize_request_with_roots(1, &["file:///repo".to_string()]);
        assert!(init["params"]["capabilities"]["sampling"].is_object());
        assert_eq!(init["params"]["capabilities"]["roots"]["listChanged"], true);
        let empty = mcp_initialize_request_with_roots(1, &[]);
        assert!(empty["params"]["capabilities"]["sampling"].is_object());
        assert!(empty["params"]["capabilities"].get("roots").is_none());
    }

    #[test]
    fn mcp_sampling_request_renders_chat_messages() {
        let params = json!({
            "systemPrompt": "Follow project instructions.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}, {"type": "image", "mimeType": "image/png", "data": "abcd"}]},
                {"role": "assistant", "content": {"type": "text", "text": "hi"}}
            ]
        });
        let messages = sampling_chat_messages(params.as_object().unwrap()).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content.text(), "Follow project instructions.");
        assert_eq!(messages[1].role, "user");
        assert_eq!(
            messages[1].content.text(),
            "hello\n[MCP sampling image content: image/png, 4 base64 bytes]"
        );
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].content.text(), "hi");
    }

    #[test]
    fn mcp_sampling_model_prefers_first_hint() {
        let params = json!({
            "modelPreferences": {
                "hints": [
                    {"name": " preferred-code-model "},
                    {"name": "second"}
                ]
            }
        });
        assert_eq!(
            sampling_model(params.as_object().unwrap(), "fallback-model"),
            "preferred-code-model"
        );
        assert_eq!(
            sampling_model(&serde_json::Map::new(), "fallback-model"),
            "fallback-model"
        );
    }

    #[test]
    fn mcp_sampling_stop_reason_maps_finish_reason() {
        assert_eq!(
            sampling_stop_reason(&json!({"choices":[{"finish_reason":"length"}]})),
            "maxTokens"
        );
        assert_eq!(
            sampling_stop_reason(&json!({"choices":[{"finish_reason":"tool_calls"}]})),
            "toolUse"
        );
        assert_eq!(
            sampling_stop_reason(&json!({"choices":[{"finish_reason":"stop"}]})),
            "endTurn"
        );
    }

    #[test]
    fn mcp_roots_list_response_renders_uri_and_path_roots() {
        let response = roots_list_response(
            json!(99),
            &["file:///repo".to_string(), "/tmp/docs".to_string()],
        );
        let roots = response["result"]["roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0]["uri"], "file:///repo");
        assert_eq!(roots[0]["name"], "repo");
        assert_eq!(roots[1]["name"], "docs");
        assert!(roots[1]["uri"]
            .as_str()
            .unwrap()
            .starts_with("file:///tmp/docs"));
    }

    #[test]
    fn mcp_subscription_uris_include_enabled_cached_resources_once() {
        let server = McpServerConfig {
            resources: vec![
                McpResourceConfig {
                    uri: " file:///repo/context.md ".to_string(),
                    ..McpResourceConfig::default()
                },
                McpResourceConfig {
                    uri: "file:///repo/context.md".to_string(),
                    ..McpResourceConfig::default()
                },
                McpResourceConfig {
                    uri: "file:///repo/disabled.md".to_string(),
                    enabled: false,
                    ..McpResourceConfig::default()
                },
            ],
            ..McpServerConfig::default()
        };
        assert_eq!(
            subscription_uris(&server),
            vec!["file:///repo/context.md".to_string()]
        );
    }

    #[test]
    fn mcp_tool_hook_calls_stdio_server() {
        let hook = HookCommandConfig {
            hook_type: "mcp_tool".to_string(),
            server: "policy".to_string(),
            tool: "check".to_string(),
            input: Some(json!({"level":"strict"})),
            timeout: Some(2),
            ..HookCommandConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([(
                "policy".to_string(),
                McpServerConfig {
                    command: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        concat!(
                            "read init; ",
                            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                            "read initialized; ",
                            "read call; ",
                            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"policy ok\"}],\"isError\":false}}';"
                        )
                        .to_string(),
                    ],
                    env: HashMap::new(),
                    ..McpServerConfig::default()
                },
            )]),
            ..Config::default()
        };
        let run = run_mcp_tool_hook_with_config(&hook, &json!({"event":"PreToolUse"}), &cfg);
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "policy ok");
    }

    #[test]
    fn mcp_method_call_reads_stdio_resource() {
        let server = McpServerConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "read init; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                    "read initialized; ",
                    "read call; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"contents\":[{\"uri\":\"file:///repo/README.md\",\"mimeType\":\"text/markdown\",\"text\":\"hello docs\"}]}}';"
                )
                .to_string(),
            ],
            env: HashMap::new(),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-resource-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_method_with_config(
            &cfg,
            "docs-resource-test",
            "resources/read",
            json!({"uri":"file:///repo/README.md"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert!(run.stdout.contains("hello docs"));
        reset_mcp_cli_session_for_config("docs-resource-test", &server);
    }

    #[test]
    fn mcp_tool_call_answers_stdio_roots_list_request() {
        let server = McpServerConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "read init; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                    "read initialized; ",
                    "read call; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"roots/list\",\"params\":{}}'; ",
                    "read roots; ",
                    "case \"$roots\" in ",
                    "*'file:///repo'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"roots ok\"}],\"isError\":false}}' ;; ",
                    "*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32000,\"message\":\"missing roots\"}}' ;; ",
                    "esac;"
                )
                .to_string(),
            ],
            roots: vec!["file:///repo".to_string()],
            env: HashMap::new(),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-roots-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_tool_with_config(
            &cfg,
            "docs-roots-test",
            "search",
            json!({"query":"roots"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "roots ok");
        assert!(reset_mcp_cli_session_for_config("docs-roots-test", &server));
    }

    #[test]
    fn mcp_tool_call_answers_stdio_sampling_request() {
        let server = McpServerConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "read init; ",
                    "case \"$init\" in *'\"sampling\":{}'*) : ;; *) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"missing sampling capability\"}}'; exit 0 ;; esac; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                    "read initialized; ",
                    "read call; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"sampling/createMessage\",\"params\":{\"messages\":[]}}'; ",
                    "read sampling; ",
                    "case \"$sampling\" in ",
                    "*'sampling/createMessage failed'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"sampling answered\"}],\"isError\":false}}' ;; ",
                    "*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32000,\"message\":\"missing sampling response\"}}' ;; ",
                    "esac;"
                )
                .to_string(),
            ],
            env: HashMap::new(),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-sampling-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_tool_with_config(
            &cfg,
            "docs-sampling-test",
            "search",
            json!({"query":"sampling"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "sampling answered");
        assert!(reset_mcp_cli_session_for_config(
            "docs-sampling-test",
            &server
        ));
    }

    #[test]
    fn mcp_tool_call_subscribes_enabled_stdio_resources() {
        let server = McpServerConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "read init; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{\"resources\":{\"subscribe\":true}},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                    "read initialized; ",
                    "read subscribe; ",
                    "case \"$subscribe\" in ",
                    "*'resources/subscribe'*'file:///repo/context.md'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"file:///repo/context.md\"}}'; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}' ;; ",
                    "*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32000,\"message\":\"missing subscribe\"}}' ;; ",
                    "esac; ",
                    "read call; ",
                    "case \"$call\" in ",
                    "*'\"id\":3'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"subscribed\"}],\"isError\":false}}' ;; ",
                    "*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32000,\"message\":\"wrong call id\"}}' ;; ",
                    "esac;"
                )
                .to_string(),
            ],
            resources: vec![McpResourceConfig {
                uri: "file:///repo/context.md".to_string(),
                ..McpResourceConfig::default()
            }],
            env: HashMap::new(),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-subscribe-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_tool_with_config(
            &cfg,
            "docs-subscribe-test",
            "search",
            json!({"query":"subscriptions"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "subscribed");
        assert!(reset_mcp_cli_session_for_config(
            "docs-subscribe-test",
            &server
        ));
    }

    #[test]
    fn mcp_tool_call_refreshes_updated_stdio_resource_for_next_read() {
        let server = McpServerConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "read init; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{\"resources\":{\"subscribe\":true}},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                    "read initialized; ",
                    "read subscribe; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}'; ",
                    "read call; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"file:///repo/context.md\"}}'; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"tool ok\"}],\"isError\":false}}'; ",
                    "read refresh; ",
                    "case \"$refresh\" in ",
                    "*'resources/read'*'file:///repo/context.md'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"contents\":[{\"uri\":\"file:///repo/context.md\",\"mimeType\":\"text/plain\",\"text\":\"fresh context\"}]}}' ;; ",
                    "*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":4,\"error\":{\"code\":-32000,\"message\":\"missing refresh\"}}' ;; ",
                    "esac; ",
                    "sleep 1;"
                )
                .to_string(),
            ],
            resources: vec![McpResourceConfig {
                uri: "file:///repo/context.md".to_string(),
                ..McpResourceConfig::default()
            }],
            env: HashMap::new(),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-refresh-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_tool_with_config(
            &cfg,
            "docs-refresh-test",
            "search",
            json!({"query":"context"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "tool ok");

        let read = call_mcp_method_with_config(
            &cfg,
            "docs-refresh-test",
            "resources/read",
            json!({"uri":"file:///repo/context.md"}),
            Some(2),
        );
        assert_eq!(read.status, 0, "stderr: {}", read.stderr);
        assert!(read.stdout.contains("fresh context"));
        assert!(reset_mcp_cli_session_for_config(
            "docs-refresh-test",
            &server
        ));
    }

    #[test]
    fn mcp_tool_call_refreshes_updated_http_resource_for_next_read() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for idx in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let mut text = String::new();
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.contains("\r\n\r\n") {
                        let header_end = text.find("\r\n\r\n").unwrap() + 4;
                        let headers = &text[..header_end];
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if text.len() >= header_end + content_len {
                            break;
                        }
                    }
                }

                let (status, headers, body) = match idx {
                    0 => (
                        "200 OK",
                        "Content-Type: application/json\r\nMcp-Session-Id: session-refresh\r\n",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"resources":{"subscribe":true}},"serverInfo":{"name":"test","version":"1"}}}"#.to_string(),
                    ),
                    1 => ("202 Accepted", "", String::new()),
                    2 => {
                        assert!(text.contains("resources/subscribe"), "{text}");
                        (
                            "200 OK",
                            "Content-Type: application/json\r\n",
                            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string(),
                        )
                    }
                    3 => {
                        assert!(text.contains("tools/call"), "{text}");
                        (
                            "200 OK",
                            "Content-Type: text/event-stream\r\n",
                            concat!(
                                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"file:///repo/context.md\"}}\n\n",
                                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"tool ok\"}],\"isError\":false}}\n\n"
                            )
                            .to_string(),
                        )
                    }
                    _ => {
                        assert!(text.contains("resources/read"), "{text}");
                        (
                            "200 OK",
                            "Content-Type: application/json\r\n",
                            r#"{"jsonrpc":"2.0","id":4,"result":{"contents":[{"uri":"file:///repo/context.md","mimeType":"text/plain","text":"fresh http context"}]}}"#.to_string(),
                        )
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let server = McpServerConfig {
            url: format!("http://{addr}/mcp"),
            resources: vec![McpResourceConfig {
                uri: "file:///repo/context.md".to_string(),
                ..McpResourceConfig::default()
            }],
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-http-refresh-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_tool_with_config(
            &cfg,
            "docs-http-refresh-test",
            "search",
            json!({"query":"context"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "tool ok");

        let read = call_mcp_method_with_config(
            &cfg,
            "docs-http-refresh-test",
            "resources/read",
            json!({"uri":"file:///repo/context.md"}),
            Some(2),
        );
        assert_eq!(read.status, 0, "stderr: {}", read.stderr);
        assert!(read.stdout.contains("fresh http context"));
        assert!(reset_mcp_cli_session_for_config(
            "docs-http-refresh-test",
            &server
        ));
        handle.join().unwrap();
    }

    #[test]
    fn mcp_tool_call_refreshes_updated_sse_resource_for_next_read() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => return stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                panic!("timed out accepting legacy SSE refresh test connection");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("accepting legacy SSE refresh test connection: {e}"),
                    }
                }
            }

            fn write_sse_event(stream: &mut impl Write, event: &str) {
                stream.write_all(event.as_bytes()).unwrap();
                stream.flush().unwrap();
            }

            let mut sse_stream = accept_with_timeout(&listener);
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
            sse_stream.write_all(response.as_bytes()).unwrap();
            write_sse_event(
                &mut sse_stream,
                &format!("event: endpoint\ndata: http://{addr}/messages\n\n"),
            );
            for idx in 0..5 {
                let mut post_stream = accept_with_timeout(&listener);
                post_stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0u8; 8192];
                let mut text = String::new();
                loop {
                    let n = post_stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.contains("\r\n\r\n") {
                        let header_end = text.find("\r\n\r\n").unwrap() + 4;
                        let headers = &text[..header_end];
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if text.len() >= header_end + content_len {
                            break;
                        }
                    }
                }
                let post_response =
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                post_stream.write_all(post_response.as_bytes()).unwrap();
                match idx {
                    0 => write_sse_event(
                        &mut sse_stream,
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{\"resources\":{\"subscribe\":true}},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n\n",
                    ),
                    2 => {
                        assert!(text.contains("resources/subscribe"), "{text}");
                        assert!(text.contains("file:///repo/context.md"), "{text}");
                        write_sse_event(
                            &mut sse_stream,
                            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n",
                        );
                    }
                    3 => {
                        assert!(text.contains("tools/call"), "{text}");
                        write_sse_event(
                            &mut sse_stream,
                            concat!(
                                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"file:///repo/context.md\"}}\n\n",
                                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"tool ok\"}],\"isError\":false}}\n\n",
                            ),
                        );
                    }
                    4 => {
                        assert!(text.contains("resources/read"), "{text}");
                        assert!(text.contains("file:///repo/context.md"), "{text}");
                        write_sse_event(
                            &mut sse_stream,
                            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"contents\":[{\"uri\":\"file:///repo/context.md\",\"mimeType\":\"text/plain\",\"text\":\"fresh sse context\"}]}}\n\n",
                        );
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        });

        let server = McpServerConfig {
            transport: "sse".to_string(),
            url: format!("http://{addr}/sse"),
            resources: vec![McpResourceConfig {
                uri: "file:///repo/context.md".to_string(),
                ..McpResourceConfig::default()
            }],
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-sse-refresh-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let run = call_mcp_tool_with_config(
            &cfg,
            "docs-sse-refresh-test",
            "search",
            json!({"query":"context"}),
            Some(2),
        );
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "tool ok");
        assert_eq!(run.transport, "sse");

        let read = call_mcp_method_with_config(
            &cfg,
            "docs-sse-refresh-test",
            "resources/read",
            json!({"uri":"file:///repo/context.md"}),
            Some(2),
        );
        assert_eq!(read.status, 0, "stderr: {}", read.stderr);
        assert!(read.stdout.contains("fresh sse context"));
        assert!(reset_mcp_cli_session_for_config(
            "docs-sse-refresh-test",
            &server
        ));
        handle.join().unwrap();
    }

    #[test]
    fn mcp_tool_calls_reuse_stdio_session_until_reset() {
        let server = McpServerConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "read init; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"1\"}}}'; ",
                    "read initialized; ",
                    "read call1; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}],\"isError\":false}}'; ",
                    "read call2; ",
                    "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}],\"isError\":false}}'; ",
                    "sleep 5;"
                )
                .to_string(),
            ],
            env: HashMap::new(),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-reuse-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let first = call_mcp_tool_with_config(
            &cfg,
            "docs-reuse-test",
            "search",
            json!({"query":"one"}),
            Some(2),
        );
        assert_eq!(first.status, 0, "stderr: {}", first.stderr);
        assert_eq!(first.stdout, "first");

        let second = call_mcp_tool_with_config(
            &cfg,
            "docs-reuse-test",
            "search",
            json!({"query":"two"}),
            Some(2),
        );
        assert_eq!(second.status, 0, "stderr: {}", second.stderr);
        assert_eq!(second.stdout, "second");
        assert!(reset_mcp_cli_session_for_config("docs-reuse-test", &server));
    }

    #[test]
    fn mcp_tool_calls_reuse_streamable_http_session_until_reset() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for idx in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let mut text = String::new();
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.contains("\r\n\r\n") {
                        let header_end = text.find("\r\n\r\n").unwrap() + 4;
                        let headers = &text[..header_end];
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if text.len() >= header_end + content_len {
                            break;
                        }
                    }
                }

                if idx > 0 {
                    assert!(
                        text.contains("mcp-session-id: session-1")
                            || text.contains("Mcp-Session-Id: session-1"),
                        "request {idx} did not reuse session id: {text}"
                    );
                }
                let (status, headers, body) = match idx {
                    0 => (
                        "200 OK",
                        "Content-Type: application/json\r\nMcp-Session-Id: session-1\r\n",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#.to_string(),
                    ),
                    1 => ("202 Accepted", "", String::new()),
                    2 => (
                        "200 OK",
                        "Content-Type: application/json\r\n",
                        r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"first http"}],"isError":false}}"#.to_string(),
                    ),
                    _ => (
                        "200 OK",
                        "Content-Type: application/json\r\n",
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"second http"}],"isError":false}}"#.to_string(),
                    ),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let server = McpServerConfig {
            url: format!("http://{addr}/mcp"),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-http-reuse-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let first = call_mcp_tool_with_config(
            &cfg,
            "docs-http-reuse-test",
            "search",
            json!({"query":"one"}),
            Some(2),
        );
        assert_eq!(first.status, 0, "stderr: {}", first.stderr);
        assert_eq!(first.stdout, "first http");
        assert_eq!(first.transport, "http");

        let second = call_mcp_tool_with_config(
            &cfg,
            "docs-http-reuse-test",
            "search",
            json!({"query":"two"}),
            Some(2),
        );
        assert_eq!(second.status, 0, "stderr: {}", second.stderr);
        assert_eq!(second.stdout, "second http");
        assert_eq!(second.transport, "http");
        assert!(reset_mcp_cli_session_for_config(
            "docs-http-reuse-test",
            &server
        ));
        handle.join().unwrap();
    }

    #[test]
    fn mcp_tool_hook_calls_streamable_http_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for idx in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let mut text = String::new();
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.contains("\r\n\r\n") {
                        let header_end = text.find("\r\n\r\n").unwrap() + 4;
                        let headers = &text[..header_end];
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if text.len() >= header_end + content_len {
                            break;
                        }
                    }
                }
                assert!(
                    text.contains("authorization: Bearer test")
                        || text.contains("Authorization: Bearer test")
                );
                let (status, headers, body) = match idx {
                    0 => (
                        "200 OK",
                        "Content-Type: application/json\r\nMcp-Session-Id: session-1\r\n",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#.to_string(),
                    ),
                    1 => ("202 Accepted", "", String::new()),
                    _ => (
                        "200 OK",
                        "Content-Type: text/event-stream\r\n",
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"http policy ok\"}],\"isError\":false}}\n\n".to_string(),
                    ),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let hook = HookCommandConfig {
            hook_type: "mcp_tool".to_string(),
            server: "policy".to_string(),
            tool: "check".to_string(),
            input: Some(json!({"level":"strict"})),
            timeout: Some(2),
            ..HookCommandConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([(
                "policy".to_string(),
                McpServerConfig {
                    url: format!("http://{addr}/mcp"),
                    headers: HashMap::from([(
                        "Authorization".to_string(),
                        "Bearer test".to_string(),
                    )]),
                    ..McpServerConfig::default()
                },
            )]),
            ..Config::default()
        };
        let run = run_mcp_tool_hook_with_config(&hook, &json!({"event":"PreToolUse"}), &cfg);
        handle.join().unwrap();
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "http policy ok");
    }

    #[test]
    fn mcp_tool_calls_reuse_legacy_sse_session_until_reset() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => return stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                panic!("timed out accepting legacy SSE test connection");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("accepting legacy SSE test connection: {e}"),
                    }
                }
            }

            fn write_sse_event(stream: &mut impl Write, event: &str) {
                stream.write_all(event.as_bytes()).unwrap();
                stream.flush().unwrap();
            }

            let mut sse_stream = accept_with_timeout(&listener);
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
            sse_stream.write_all(response.as_bytes()).unwrap();
            write_sse_event(
                &mut sse_stream,
                &format!("event: endpoint\ndata: http://{addr}/messages\n\n"),
            );
            for idx in 0..4 {
                let mut post_stream = accept_with_timeout(&listener);
                post_stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0u8; 8192];
                let mut text = String::new();
                loop {
                    let n = post_stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.contains("\r\n\r\n") {
                        let header_end = text.find("\r\n\r\n").unwrap() + 4;
                        let headers = &text[..header_end];
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if text.len() >= header_end + content_len {
                            break;
                        }
                    }
                }
                let post_response =
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                post_stream.write_all(post_response.as_bytes()).unwrap();
                match idx {
                    0 => write_sse_event(
                        &mut sse_stream,
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{}}}\n\n",
                    ),
                    2 => write_sse_event(
                        &mut sse_stream,
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"first sse\"}],\"isError\":false}}\n\n",
                    ),
                    3 => write_sse_event(
                        &mut sse_stream,
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"second sse\"}],\"isError\":false}}\n\n",
                    ),
                    _ => {}
                }
            }
        });

        let server = McpServerConfig {
            transport: "sse".to_string(),
            url: format!("http://{addr}/sse"),
            ..McpServerConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([("docs-sse-reuse-test".to_string(), server.clone())]),
            ..Config::default()
        };
        let first = call_mcp_tool_with_config(
            &cfg,
            "docs-sse-reuse-test",
            "search",
            json!({"query":"one"}),
            Some(10),
        );
        assert_eq!(first.status, 0, "stderr: {}", first.stderr);
        assert_eq!(first.stdout, "first sse");
        assert_eq!(first.transport, "sse");

        let second = call_mcp_tool_with_config(
            &cfg,
            "docs-sse-reuse-test",
            "search",
            json!({"query":"two"}),
            Some(10),
        );
        assert_eq!(second.status, 0, "stderr: {}", second.stderr);
        assert_eq!(second.stdout, "second sse");
        assert_eq!(second.transport, "sse");
        assert!(reset_mcp_cli_session_for_config(
            "docs-sse-reuse-test",
            &server
        ));
        handle.join().unwrap();
    }

    #[test]
    fn mcp_tool_hook_calls_legacy_sse_server() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => return stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                panic!("timed out accepting legacy SSE test connection");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("accepting legacy SSE test connection: {e}"),
                    }
                }
            }

            fn write_sse_event(stream: &mut impl Write, event: &str) {
                stream.write_all(event.as_bytes()).unwrap();
                stream.flush().unwrap();
            }

            let mut sse_stream = accept_with_timeout(&listener);
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
            sse_stream.write_all(response.as_bytes()).unwrap();
            write_sse_event(
                &mut sse_stream,
                &format!("event: endpoint\ndata: http://{addr}/messages\n\n"),
            );
            for idx in 0..3 {
                let mut post_stream = accept_with_timeout(&listener);
                post_stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0u8; 8192];
                let mut text = String::new();
                loop {
                    let n = post_stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.contains("\r\n\r\n") {
                        let header_end = text.find("\r\n\r\n").unwrap() + 4;
                        let headers = &text[..header_end];
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if text.len() >= header_end + content_len {
                            break;
                        }
                    }
                }
                let post_response =
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                post_stream.write_all(post_response.as_bytes()).unwrap();
                match idx {
                    0 => write_sse_event(
                        &mut sse_stream,
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{}}}\n\n",
                    ),
                    2 => write_sse_event(
                        &mut sse_stream,
                        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"sse policy ok\"}],\"isError\":false}}\n\n",
                    ),
                    _ => {}
                }
            }
        });

        let hook = HookCommandConfig {
            hook_type: "mcp_tool".to_string(),
            server: "policy".to_string(),
            tool: "check".to_string(),
            input: Some(json!({"level":"strict"})),
            timeout: Some(10),
            ..HookCommandConfig::default()
        };
        let cfg = Config {
            mcp_servers: HashMap::from([(
                "policy".to_string(),
                McpServerConfig {
                    transport: "sse".to_string(),
                    url: format!("http://{addr}/sse"),
                    ..McpServerConfig::default()
                },
            )]),
            ..Config::default()
        };
        let run = run_mcp_tool_hook_with_config(&hook, &json!({"event":"PreToolUse"}), &cfg);
        handle.join().unwrap();
        assert_eq!(run.status, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "sse policy ok");
    }

    #[test]
    fn matcher_accepts_case_sensitive_exact_alternative_and_glob() {
        let hook = HookCommandConfig {
            matcher: "Read|bash|mcp__github__*".to_string(),
            command: "true".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(hook_matches_tool(&hook, "bash"));
        assert!(hook_matches_tool(&hook, "mcp__github__issue"));
        assert!(!hook_matches_tool(&hook, "read"));
        assert!(!hook_matches_tool(&hook, "write"));
    }

    #[test]
    fn matcher_accepts_regex_forms_and_regex_alternation_pipes() {
        let hook = HookCommandConfig {
            matcher: "regex:^mcp__[a-z]+__issue$|/^(bash|write)$/".to_string(),
            command: "true".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(hook_matches_tool(&hook, "mcp__github__issue"));
        assert!(hook_matches_tool(&hook, "bash"));
        assert!(hook_matches_tool(&hook, "write"));
        assert!(!hook_matches_tool(&hook, "read"));
    }

    #[test]
    fn parses_hook_specific_decisions() {
        let decision = pre_tool_decision_from_stdout(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"confirm","updatedInput":{"command":"pwd"},"additionalContext":"cwd only"}}"#,
        );
        assert_eq!(
            decision,
            ToolPolicyDecision::Ask {
                reason: Some("confirm".to_string()),
                updated_input: Some(json!({"command":"pwd"})),
                additional_context: Some("cwd only".to_string()),
            }
        );
    }

    #[test]
    fn shell_hook_receives_payload_and_event_env() {
        let cwd = tempfile::tempdir().unwrap();
        let hook = HookCommandConfig {
            command: "printf '%s|' \"$LIBERTAI_HOOK_EVENT\"; cat".to_string(),
            ..HookCommandConfig::default()
        };
        let run = run_shell_hook(
            &hook,
            cwd.path(),
            &json!({"event":"PostToolUse"}),
            "PostToolUse",
        );
        assert_eq!(run.status, 0);
        assert!(run.stdout.starts_with("PostToolUse|"));
        assert!(run.stdout.contains("\"event\":\"PostToolUse\""));
    }

    #[test]
    fn shell_hook_appends_quoted_args() {
        let cwd = tempfile::tempdir().unwrap();
        let hook = HookCommandConfig {
            command: "printf '%s|%s'".to_string(),
            args: vec!["two words".to_string(), "quote's".to_string()],
            ..HookCommandConfig::default()
        };
        let run = run_shell_hook(&hook, cwd.path(), &json!({}), "PostToolUse");
        assert_eq!(run.status, 0);
        assert_eq!(run.stdout, "two words|quote's");
        assert_eq!(
            hook_command_display(&hook),
            "printf '%s|%s' 'two words' 'quote'\\''s'"
        );
    }

    #[test]
    fn http_hook_posts_json_payload_and_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let received_thread = received.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 4096];
            let mut text = String::new();
            loop {
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let headers = &text[..header_end];
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    let body_len = text.len().saturating_sub(header_end + 4);
                    if body_len >= content_len {
                        break;
                    }
                }
            }
            *received_thread.lock().unwrap() = text;
            std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\ncontent-length: 41\r\nconnection: close\r\n\r\n{\"additionalContext\":\"http hook context\"}",
            )
            .unwrap();
        });

        std::env::set_var("LIBERTAI_HOOK_TOKEN", "secret");
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "x-test-token".to_string(),
            "token-$LIBERTAI_HOOK_TOKEN".to_string(),
        );
        let hook = HookCommandConfig {
            hook_type: "http".to_string(),
            url: format!("http://{addr}/hook"),
            headers,
            allowed_env_vars: vec!["LIBERTAI_HOOK_TOKEN".to_string()],
            ..HookCommandConfig::default()
        };
        let run = run_configured_hook(
            &hook,
            std::path::Path::new("."),
            &json!({"event":"UserPromptSubmit","prompt":"review"}),
            "UserPromptSubmit",
        );
        assert_eq!(run.status, 0);
        assert_eq!(run.stdout, r#"{"additionalContext":"http hook context"}"#);
        let request = received.lock().unwrap().clone();
        assert!(request.contains("POST /hook HTTP/1.1"));
        assert!(request.contains("x-libertai-hook-event: UserPromptSubmit"));
        assert!(request.contains("x-test-token: token-secret"));
        assert!(request.contains("\"prompt\":\"review\""));
    }

    #[test]
    fn http_hook_reports_non_success_status() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 403 Forbidden\r\ncontent-length: 7\r\nconnection: close\r\n\r\ndenied\n",
            );
        });
        let hook = HookCommandConfig {
            hook_type: "http".to_string(),
            url: format!("http://{addr}/hook"),
            ..HookCommandConfig::default()
        };
        let run = run_configured_hook(
            &hook,
            std::path::Path::new("."),
            &json!({"event":"PreToolUse"}),
            "PreToolUse",
        );
        assert_eq!(run.status, 403);
        assert_eq!(run.stdout, "denied");
        assert!(run.stderr.contains("HTTP hook returned"));
    }

    #[test]
    fn prompt_hook_messages_include_prompt_and_payload() {
        let messages = prompt_hook_messages(
            "Review this payload.",
            &json!({"event":"PreToolUse","toolName":"bash"}),
        );
        assert_eq!(messages.len(), 2);
        assert!(messages[0]
            .content
            .text()
            .contains("Claude Code-style hook handler"));
        assert!(messages[1].content.text().contains("Review this payload."));
        assert!(messages[1]
            .content
            .text()
            .contains("\"toolName\": \"bash\""));
    }

    #[test]
    fn prompt_hook_content_reads_chat_completion() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": "  {\"permissionDecision\":\"allow\"}  "
                }
            }]
        });
        assert_eq!(
            prompt_hook_content(&body).as_deref(),
            Some(r#"{"permissionDecision":"allow"}"#)
        );
        assert_eq!(prompt_hook_content(&json!({"choices":[]})), None);
    }

    #[test]
    fn detached_hook_receives_payload_and_event_env() {
        let cwd = tempfile::tempdir().unwrap();
        let hook = HookCommandConfig {
            command: "printf '%s' \"$LIBERTAI_HOOK_EVENT\" > async-event.txt; \
                      cat > async-payload.json"
                .to_string(),
            async_hook: true,
            ..HookCommandConfig::default()
        };

        let run = run_detached_shell_hook(
            &hook,
            cwd.path(),
            &json!({"event":"PostToolUse"}),
            "PostToolUse",
        );
        assert_eq!(run.status, 0);

        let event_path = cwd.path().join("async-event.txt");
        let payload_path = cwd.path().join("async-payload.json");
        // Poll on file *content*: the detached shell creates each file (via `>`
        // / `cat >`) before it finishes writing, so an exists-only check races
        // the write and reads a half-written file on a slow runner.
        let mut event = String::new();
        let mut payload = String::new();
        for _ in 0..200 {
            event = std::fs::read_to_string(&event_path).unwrap_or_default();
            payload = std::fs::read_to_string(&payload_path).unwrap_or_default();
            if event == "PostToolUse" && payload.contains("\"event\":\"PostToolUse\"") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(event, "PostToolUse");
        assert!(
            payload.contains("\"event\":\"PostToolUse\""),
            "payload: {payload:?}"
        );
    }

    #[test]
    fn subagent_stop_hook_receives_task_payload_and_event_env() {
        let cwd = tempfile::tempdir().unwrap();
        let hook = HookCommandConfig {
            matcher: "task".to_string(),
            command: "printf '%s|' \"$LIBERTAI_HOOK_EVENT\"; cat".to_string(),
            ..HookCommandConfig::default()
        };
        let output = ToolOutput {
            content: Vec::new(),
            details: None,
            is_error: false,
        };
        let payload = payload_with_event(
            &tool_completion_payload(cwd.path(), "toolu_1", "task", &output, false),
            "SubagentStop",
        );

        let run = run_shell_hook(&hook, cwd.path(), &payload, "SubagentStop");
        assert_eq!(run.status, 0);
        assert!(run.stdout.starts_with("SubagentStop|"));
        assert!(run.stdout.contains("\"event\":\"SubagentStop\""));
        assert!(run.stdout.contains("\"toolName\":\"task\""));
    }

    #[test]
    fn subagent_stop_hooks_reuse_tool_matchers() {
        let cwd = tempfile::tempdir().unwrap();
        let mut hooks = vec![
            HookCommandConfig {
                matcher: "read".to_string(),
                command: "printf bad > ran.txt".to_string(),
                ..HookCommandConfig::default()
            },
            HookCommandConfig {
                matcher: "task".to_string(),
                command: "printf ok > ran.txt".to_string(),
                ..HookCommandConfig::default()
            },
        ];
        let payload = json!({"event": "SubagentStop", "toolName": "task"});
        run_tool_completion_hooks("SubagentStop", &hooks, cwd.path(), "task", &payload);
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("ran.txt")).unwrap(),
            "ok"
        );

        hooks[1].enabled = false;
        std::fs::remove_file(cwd.path().join("ran.txt")).unwrap();
        run_tool_completion_hooks("SubagentStop", &hooks, cwd.path(), "task", &payload);
        assert!(!cwd.path().join("ran.txt").exists());
    }

    #[test]
    fn hook_condition_matches_claude_tool_argument_rules() {
        let hook = HookCommandConfig {
            if_condition: "Bash(rm *)".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(hook_condition_matches(
            &hook,
            &json!({
                "toolName": "bash",
                "tool_input": { "command": "rm -rf target" }
            })
        ));
        assert!(hook_condition_matches(
            &hook,
            &json!({
                "tool_name": "bash",
                "argsJson": "{\"command\":\"rm -rf tmp\"}"
            })
        ));
        assert!(!hook_condition_matches(
            &hook,
            &json!({
                "toolName": "bash",
                "tool_input": { "command": "cargo test" }
            })
        ));
        assert!(!hook_condition_matches(
            &hook,
            &json!({
                "toolName": "write",
                "tool_input": { "path": "README.md" }
            })
        ));

        let empty = HookCommandConfig::default();
        assert!(hook_condition_matches(&empty, &json!({})));

        let malformed = HookCommandConfig {
            if_condition: "Bash(rm *".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(!hook_condition_matches(
            &malformed,
            &json!({
                "toolName": "bash",
                "tool_input": { "command": "rm -rf target" }
            })
        ));

        let edit_hook = HookCommandConfig {
            if_condition: "Edit(*.ts)".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(hook_condition_matches(
            &edit_hook,
            &json!({
                "toolName": "edit",
                "tool_input": { "path": "src/app.ts" }
            })
        ));
        assert!(!hook_condition_matches(
            &edit_hook,
            &json!({
                "toolName": "edit",
                "tool_input": { "path": "src/app.rs" }
            })
        ));

        let tool_only = HookCommandConfig {
            if_condition: "Bash|Write".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(hook_condition_matches(
            &tool_only,
            &json!({
                "toolName": "write",
                "tool_input": { "path": "README.md" }
            })
        ));
    }

    #[test]
    fn config_policy_denies_from_matching_hook() {
        let cfg = Arc::new(Config {
            hooks: HooksConfig {
                pre_tool_use: vec![HookCommandConfig {
                    matcher: "write".to_string(),
                    command: "printf '{\"permissionDecision\":\"deny\",\"reason\":\"blocked\"}'"
                        .to_string(),
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        });
        let policy = tool_policy_from_config(cfg).unwrap();
        assert_eq!(
            policy.decide("call-1", "write", &json!({"path":"secret.txt"})),
            ToolPolicyDecision::Deny {
                reason: Some("blocked".to_string())
            }
        );
        assert_eq!(
            policy.decide("call-2", "read", &json!({"path":"secret.txt"})),
            ToolPolicyDecision::NoDecision
        );
    }

    #[test]
    fn config_policy_filters_by_hook_condition() {
        let cfg = Arc::new(Config {
            hooks: HooksConfig {
                pre_tool_use: vec![HookCommandConfig {
                    matcher: "bash".to_string(),
                    if_condition: "Bash(rm *)".to_string(),
                    command: "printf '{\"permissionDecision\":\"deny\",\"reason\":\"blocked\"}'"
                        .to_string(),
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        });
        let policy = tool_policy_from_config(cfg).unwrap();
        assert_eq!(
            policy.decide("call-1", "bash", &json!({"command":"cargo test"})),
            ToolPolicyDecision::NoDecision
        );
        assert_eq!(
            policy.decide("call-2", "bash", &json!({"command":"rm -rf target"})),
            ToolPolicyDecision::Deny {
                reason: Some("blocked".to_string())
            }
        );
    }

    #[test]
    fn user_prompt_hook_appends_additional_context() {
        let cfg = Config {
            hooks: HooksConfig {
                user_prompt_submit: vec![HookCommandConfig {
                    command: "printf '{\"additionalContext\":\"repo policy\"}'".to_string(),
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        };

        let prompt = run_user_prompt_submit_hooks(&cfg, "review this").unwrap();
        assert!(prompt.starts_with("review this"));
        assert!(prompt.contains("Additional context from UserPromptSubmit hook"));
        assert!(prompt.contains("repo policy"));
    }

    #[test]
    fn post_compact_hook_receives_payload_and_event_env() {
        // PostCompact (M6 #31) fires after auto-compaction settles, with
        // a payload carrying the event name, reason, tokens-before/after,
        // duration, and aborted flag. tokens_after is null until pi P3.
        let cwd = tempfile::tempdir().unwrap();
        let out = cwd.path().join("out.json");
        let cfg = Config {
            hooks: HooksConfig {
                post_compact: vec![HookCommandConfig {
                    command: format!(
                        "printf '%s|' \"$LIBERTAI_HOOK_EVENT\" > {}; cat >> {}",
                        out.display(),
                        out.display()
                    ),
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        };

        run_post_compact_hooks(&cfg, "auto", Some(142_000), None, 2_100, false);

        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.starts_with("PostCompact|"), "{written}");
        let json_part = &written["PostCompact|".len()..];
        let v: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(v["event"], "PostCompact");
        assert_eq!(v["reason"], "auto");
        assert_eq!(v["tokensBefore"], 142_000);
        assert!(
            v["tokensAfter"].is_null(),
            "tokensAfter is null until pi P3"
        );
        assert_eq!(v["durationMs"], 2_100);
        assert_eq!(v["aborted"], false);
    }

    #[test]
    fn post_compact_hook_skips_when_none_configured() {
        // No PostCompact hooks → no fire (and no panic).
        let cfg = Config::default();
        run_post_compact_hooks(&cfg, "auto", Some(1_000), None, 100, true);
        // Nothing to assert beyond "didn't panic" — no hooks ran.
    }

    #[test]
    fn user_prompt_hook_blocks_on_nonzero_exit() {
        let cfg = Config {
            hooks: HooksConfig {
                user_prompt_submit: vec![HookCommandConfig {
                    command: "printf 'blocked'; exit 2".to_string(),
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        };

        let err = run_user_prompt_submit_hooks(&cfg, "review this").unwrap_err();
        assert!(err.to_string().contains("blocked the prompt"));
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn user_prompt_hook_continue_on_block_keeps_prompt() {
        let cfg = Config {
            hooks: HooksConfig {
                user_prompt_submit: vec![HookCommandConfig {
                    command: "printf 'blocked'; exit 2".to_string(),
                    continue_on_block: true,
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        };

        let prompt = run_user_prompt_submit_hooks(&cfg, "review this").unwrap();
        assert_eq!(prompt, "review this");
    }

    #[test]
    fn once_user_prompt_hook_runs_only_once() {
        reset_once_hook_state();
        let cfg = Config {
            hooks: HooksConfig {
                user_prompt_submit: vec![HookCommandConfig {
                    command: "printf '{\"additionalContext\":\"run once\"}'".to_string(),
                    once: true,
                    ..HookCommandConfig::default()
                }],
                ..HooksConfig::default()
            },
            ..Config::default()
        };

        let first = run_user_prompt_submit_hooks(&cfg, "review this").unwrap();
        let second = run_user_prompt_submit_hooks(&cfg, "review this").unwrap();

        assert!(first.contains("run once"));
        assert_eq!(second, "review this");
    }

    #[test]
    fn lifecycle_payload_includes_event_and_model_fields() {
        let cfg = Config {
            default_code_provider: "libertai".to_string(),
            default_code_model: "test-code-model".to_string(),
            ..Config::default()
        };
        let payload = lifecycle_payload(std::path::Path::new("/tmp/project"), &cfg, "SessionStart");

        assert_eq!(payload["event"], "SessionStart");
        assert_eq!(payload["provider"], "libertai");
        assert_eq!(payload["model"], "test-code-model");
        assert_eq!(payload["defaultCodeProvider"], "libertai");
        assert_eq!(payload["default_code_model"], "test-code-model");
    }

    #[test]
    fn tool_completion_payload_includes_compatibility_fields() {
        let result = ToolOutput {
            content: Vec::new(),
            details: Some(json!({"ok": true})),
            is_error: false,
        };
        let payload = payload_with_event(
            &tool_completion_payload(
                std::path::Path::new("/tmp/project"),
                "call-1",
                "bash",
                &result,
                false,
            ),
            "PostToolUse",
        );

        assert_eq!(payload["event"], "PostToolUse");
        assert_eq!(payload["toolName"], "bash");
        assert_eq!(payload["tool_name"], "bash");
        assert_eq!(payload["isError"], false);
        assert_eq!(payload["is_error"], false);
        assert_eq!(payload["result"]["details"], json!({"ok": true}));
        assert_eq!(payload["tool_response"]["details"], json!({"ok": true}));
    }
}
