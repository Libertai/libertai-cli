//! Command-only Claude Code-style hooks for native CLI sessions.
//!
//! The desktop has a richer hook registry. The CLI intentionally keeps
//! this surface narrow: only configured shell commands are executed, and
//! imported HTTP/prompt/agent/MCP hook handlers are not run natively.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::commands::code_approvals::{ToolPolicy, ToolPolicyDecision};
use crate::config::{Config, HookCommandConfig};

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

            let run = run_shell_hook(hook, &cwd, &payload);
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct HookRun {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run_shell_hook(
    hook: &HookCommandConfig,
    cwd: &std::path::Path,
    payload: &serde_json::Value,
) -> HookRun {
    let mut cmd = shell_command(&hook.command, hook.shell.trim());
    let spawn = cmd
        .current_dir(cwd)
        .env("LIBERTAI_HOOK_EVENT", "PreToolUse")
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
    matcher
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .any(|part| {
            part == "*"
                || part.eq_ignore_ascii_case(tool_name)
                || (part.contains('*') && wildcard_match(part, tool_name))
        })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let mut rest = value.as_str();
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
    fn matcher_accepts_exact_alternative_and_glob() {
        let hook = HookCommandConfig {
            matcher: "Read|Bash|mcp__github__*".to_string(),
            command: "true".to_string(),
            ..HookCommandConfig::default()
        };
        assert!(hook_matches_tool(&hook, "bash"));
        assert!(hook_matches_tool(&hook, "mcp__github__issue"));
        assert!(!hook_matches_tool(&hook, "write"));
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
        let run = run_shell_hook(&hook, cwd.path(), &json!({"event":"PreToolUse"}));
        assert_eq!(run.status, 0);
        assert!(run.stdout.starts_with("PreToolUse|"));
        assert!(run.stdout.contains("\"event\":\"PreToolUse\""));
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
}
