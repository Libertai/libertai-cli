//! Command-only Claude Code-style hooks for native CLI sessions.
//!
//! The desktop has a richer hook registry. The CLI intentionally keeps
//! this surface narrow: only configured shell commands are executed, and
//! imported HTTP/prompt/agent/MCP hook handlers are not run natively.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi::sdk::{AgentEvent, ToolOutput};
use serde_json::json;

use crate::commands::code_approvals::{ToolPolicy, ToolPolicyDecision};
use crate::config::{Config, HookCommandConfig};

pub struct SessionHookGuard {
    cfg: Arc<Config>,
}

impl SessionHookGuard {
    pub fn start(cfg: Arc<Config>) -> Self {
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

pub fn run_user_prompt_submit_hooks(cfg: &Config, prompt: &str) -> anyhow::Result<String> {
    if !cfg.hooks.user_prompt_submit.iter().any(is_runnable_hook) {
        return Ok(prompt.to_string());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = user_prompt_submit_payload(&cwd, prompt);
    let mut contexts: Vec<String> = Vec::new();

    for hook in &cfg.hooks.user_prompt_submit {
        if !is_runnable_hook(hook) {
            continue;
        }
        if hook.async_hook {
            spawn_async_hook(hook, &cwd, &payload, "UserPromptSubmit");
            continue;
        }
        let run = run_shell_hook(hook, &cwd, &payload, "UserPromptSubmit");
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            anyhow::bail!(
                "UserPromptSubmit hook `{}` blocked the prompt: {detail}",
                hook.command.trim()
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

    for hook in hooks {
        if !is_runnable_hook(hook) {
            continue;
        }
        if hook.async_hook {
            spawn_async_hook(hook, &cwd, &payload, event_name);
            continue;
        }
        let run = run_shell_hook(hook, &cwd, &payload, event_name);
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            eprintln!(
                "  \x1b[2m[hook {event_name}] {}: {}\x1b[0m",
                hook.command.trim(),
                detail
            );
        }
    }
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
    hook.enabled && !hook.command.trim().is_empty()
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

        for hook in &self.cfg.hooks.pre_tool_use {
            if !is_runnable_hook(hook) || !hook_matches_tool(hook, tool_name) {
                continue;
            }
            if hook.async_hook {
                spawn_async_hook(hook, &cwd, &payload, "PreToolUse");
                continue;
            }

            let run = run_shell_hook(hook, &cwd, &payload, "PreToolUse");
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

    if !cfg.hooks.post_tool_use.iter().any(is_runnable_hook) {
        return;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = post_tool_payload(&cwd, tool_call_id, tool_name, result, *is_error);

    for hook in &cfg.hooks.post_tool_use {
        if !is_runnable_hook(hook) || !hook_matches_tool(hook, tool_name) {
            continue;
        }
        if hook.async_hook {
            spawn_async_hook(hook, &cwd, &payload, "PostToolUse");
            continue;
        }
        let run = run_shell_hook(hook, &cwd, &payload, "PostToolUse");
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout).unwrap_or_else(|| {
                format!("hook exited with status {}", run.status)
            });
            eprintln!(
                "  \x1b[2m[hook PostToolUse] {}: {}\x1b[0m",
                hook.command.trim(),
                detail
            );
        }
    }
}

fn post_tool_payload(
    cwd: &std::path::Path,
    tool_call_id: &str,
    tool_name: &str,
    result: &ToolOutput,
    is_error: bool,
) -> serde_json::Value {
    json!({
        "event": "PostToolUse",
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
        let run = run_detached_shell_hook(&hook, &cwd, &payload, &event_name);
        if run.status != 0 {
            let detail = first_non_empty(&run.stderr, &run.stdout)
                .unwrap_or_else(|| format!("hook exited with status {}", run.status));
            eprintln!(
                "  \x1b[2m[hook {event_name} async] {}: {}\x1b[0m",
                hook.command.trim(),
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
    let mut cmd = shell_command(&hook.command, hook.shell.trim());
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

fn run_shell_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
    event_name: &str,
) -> HookRun {
    let mut cmd = shell_command(&hook.command, hook.shell.trim());
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

fn hook_matches_tool(hook: &HookCommandConfig, tool_name: &str) -> bool {
    let matcher = hook.matcher.trim();
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    matcher_alternatives(matcher)
        .into_iter()
        .any(|part| matcher_part_matches(&part, tool_name))
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
    use crate::config::HooksConfig;

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
    fn post_tool_payload_includes_compatibility_fields() {
        let result = ToolOutput {
            content: Vec::new(),
            details: Some(json!({"ok": true})),
            is_error: false,
        };
        let payload = post_tool_payload(
            std::path::Path::new("/tmp/project"),
            "call-1",
            "bash",
            &result,
            false,
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
