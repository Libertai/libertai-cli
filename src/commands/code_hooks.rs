//! Claude Code-style hooks for native CLI sessions.
//!
//! The desktop has a richer hook registry. The CLI intentionally keeps
//! this surface narrow: configured shell commands, HTTP handlers,
//! stdio MCP-tool handlers, and prompt/agent handlers are executed.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pi::model::ContentBlock;
use pi::sdk::{create_agent_session, AgentEvent, ToolOutput};
use serde_json::json;

use crate::commands::code_approvals::{
    ApprovalState, ApprovalUi, PromptChoice, ToolPolicy, ToolPolicyDecision,
};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_session::{
    build_session_options, CodeSessionConfig, SessionPersistence, DEFAULT_MAX_TOKENS,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::config::{Config, HookCommandConfig};
use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};

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
            spawn_async_hook(hook, &cwd, &payload, event_name);
            continue;
        }
        let run = run_configured_hook(hook, &cwd, &payload, event_name);
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
    once_hook_keys()
        .lock()
        .expect("once hook lock")
        .clear();
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
                deny_reason = Some(first_non_empty(&run.stderr, &run.stdout).unwrap_or_else(|| {
                    "PreToolUse hook denied this tool call".to_string()
                }));
                continue;
            }

            match pre_tool_decision_from_stdout(&run.stdout) {
                ToolPolicyDecision::Deny { reason } => {
                    deny_reason = Some(
                        reason.unwrap_or_else(|| "PreToolUse hook denied this tool call".to_string()),
                    );
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
            let detail = first_non_empty(&run.stderr, &run.stdout).unwrap_or_else(|| {
                format!("hook exited with status {}", run.status)
            });
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

fn write_mcp_message(stdin: &mut impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

fn wait_for_mcp_response(
    rx: &mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
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
    let model = hook
        .model
        .trim()
        .is_empty()
        .then(|| cfg.default_code_model.clone())
        .unwrap_or_else(|| hook.model.trim().to_string());
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
    let append_system_prompt =
        crate::commands::code_env_prompt::append_environment_prompt(append_system_prompt, cwd.as_deref());
    let prompt = prompt_hook_user_content(&hook.prompt, &payload);
    let model = hook
        .model
        .trim()
        .is_empty()
        .then(|| cfg.default_code_model.clone())
        .unwrap_or_else(|| hook.model.trim().to_string());
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
        enabled_tools: Some(AGENT_HOOK_TOOLS.iter().map(|tool| tool.to_string()).collect()),
        append_system_prompt,
        max_tokens,
        bash_command_wrapper: None,
        auto_compaction_enabled: cfg.code_auto_compaction_enabled,
        compaction_reserve_tokens: cfg.code_compaction_reserve_tokens,
        compaction_keep_recent_tokens: cfg.code_compaction_keep_recent_tokens,
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
            content: "You are running as a Claude Code-style hook handler. Return only the hook output. For PreToolUse decisions, return valid JSON using permissionDecision and optional permissionDecisionReason, updatedInput, and additionalContext fields.".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content,
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

    if let Some(timeout) = hook.timeout.filter(|secs| *secs > 0).map(Duration::from_secs) {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    return match child.wait_with_output() {
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                            let timeout_msg = format!("hook timed out after {}s", timeout.as_secs());
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
        if cfg!(windows) { "cmd" } else { "sh" }
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
    if let Some(input) = payload.get("tool_input").or_else(|| payload.get("toolInput")) {
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
    use crate::config::{HooksConfig, McpServerConfig};
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
                },
            )]),
            ..Config::default()
        };
        let run = run_mcp_tool_hook_with_config(&hook, &json!({"event":"PreToolUse"}), &cfg);
        assert_eq!(run.status, 0);
        assert_eq!(run.stdout, "policy ok");
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
        headers.insert("x-test-token".to_string(), "token-$LIBERTAI_HOOK_TOKEN".to_string());
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
        assert!(messages[0].content.contains("Claude Code-style hook handler"));
        assert!(messages[1].content.contains("Review this payload."));
        assert!(messages[1].content.contains("\"toolName\": \"bash\""));
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
        for _ in 0..100 {
            if event_path.exists() && payload_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(event_path).unwrap(), "PostToolUse");
        assert!(
            std::fs::read_to_string(payload_path)
                .unwrap()
                .contains("\"event\":\"PostToolUse\"")
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
        let payload =
            lifecycle_payload(std::path::Path::new("/tmp/project"), &cfg, "SessionStart");

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
