//! Guards the on-disk config shape and the key-masking helper.

use libertai_cli::config::{
    mask_key, Auth, Config, HookCommandConfig, HooksConfig, LauncherDefaults, McpServerConfig,
    DEFAULT_API_BASE, DEFAULT_CHAT_MODEL, DEFAULT_CODE_MODEL, DEFAULT_FAST_MODEL,
    DEFAULT_HAIKU_MODEL, DEFAULT_IMAGE_MODEL, DEFAULT_OPUS_MODEL,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn empty_toml_parses_as_defaults() {
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.api_base, DEFAULT_API_BASE);
    assert_eq!(cfg.default_chat_model, DEFAULT_CHAT_MODEL);
    assert_eq!(cfg.default_code_model, DEFAULT_CODE_MODEL);
    assert_eq!(cfg.default_image_model, DEFAULT_IMAGE_MODEL);
    assert_eq!(cfg.launcher_defaults.opus_model, DEFAULT_OPUS_MODEL);
    assert_eq!(cfg.launcher_defaults.sonnet_model, DEFAULT_FAST_MODEL);
    assert_eq!(cfg.launcher_defaults.haiku_model, DEFAULT_HAIKU_MODEL);
    assert!(cfg.status_line_template.is_empty());
    assert!(cfg.hooks.user_prompt_submit.is_empty());
    assert!(cfg.hooks.pre_tool_use.is_empty());
    assert!(cfg.hooks.post_tool_use.is_empty());
    assert!(cfg.hooks.subagent_stop.is_empty());
    assert!(cfg.hooks.session_start.is_empty());
    assert!(cfg.hooks.stop.is_empty());
    assert!(cfg.hooks.session_end.is_empty());
    assert!(cfg.hooks.notification.is_empty());
    assert!(cfg.hooks.teammate_spawn.is_empty());
    assert!(cfg.hooks.task_complete.is_empty());
    assert!(cfg.hooks.team_complete.is_empty());
    assert!(cfg.mcp_servers.is_empty());
    assert!(cfg.auth.api_key.is_none());
}

#[test]
fn save_then_load_preserves_fields() {
    let cfg = Config {
        default_chat_model: "test-model".into(),
        auth: Auth {
            api_key: Some("LTAI_sk_abcdefgh12345678".into()),
            wallet_address: Some("0xabcdef".into()),
            chain: Some("base".into()),
            expires_at: Some("2026-09-03T12:00:00Z".into()),
            device_id: Some("test-device-id".into()),
        },
        launcher_defaults: LauncherDefaults {
            opus_model: "opus-x".into(),
            ..Default::default()
        },
        status_line_template: "{model} {ctx}".into(),
        hooks: HooksConfig {
            user_prompt_submit: vec![
                HookCommandConfig {
                    command: "scripts/user-prompt-submit.sh".into(),
                    args: vec!["--flag".into(), "two words".into()],
                    timeout: Some(2),
                    continue_on_block: true,
                    ..HookCommandConfig::default()
                },
                HookCommandConfig {
                    hook_type: "mcp_tool".into(),
                    server: "policy".into(),
                    tool: "check_prompt".into(),
                    input: Some(json!({ "level": "strict" })),
                    source: "project".into(),
                    status_message: "checking policy".into(),
                    review_policy: "manual-review".into(),
                    once: true,
                    async_rewake: true,
                    extra: BTreeMap::from([("customFlag".to_string(), json!(true))]),
                    ..HookCommandConfig::default()
                },
            ],
            pre_tool_use: vec![HookCommandConfig {
                matcher: "bash|write".into(),
                command: "scripts/pre-tool-use.sh".into(),
                timeout: Some(5),
                ..HookCommandConfig::default()
            }],
            post_tool_use: vec![HookCommandConfig {
                matcher: "bash".into(),
                command: "scripts/post-tool-use.sh".into(),
                timeout: Some(3),
                async_hook: true,
                ..HookCommandConfig::default()
            }],
            subagent_stop: vec![HookCommandConfig {
                matcher: "task".into(),
                command: "scripts/subagent-stop.sh".into(),
                ..HookCommandConfig::default()
            }],
            session_start: vec![HookCommandConfig {
                command: "scripts/session-start.sh".into(),
                ..HookCommandConfig::default()
            }],
            stop: vec![HookCommandConfig {
                command: "scripts/stop.sh".into(),
                ..HookCommandConfig::default()
            }],
            session_end: vec![HookCommandConfig {
                command: "scripts/session-end.sh".into(),
                ..HookCommandConfig::default()
            }],
            notification: vec![HookCommandConfig {
                command: "scripts/notification.sh".into(),
                ..HookCommandConfig::default()
            }],
            teammate_spawn: vec![HookCommandConfig {
                command: "scripts/teammate-spawn.sh".into(),
                ..HookCommandConfig::default()
            }],
            task_complete: vec![HookCommandConfig {
                command: "scripts/task-complete.sh".into(),
                ..HookCommandConfig::default()
            }],
            team_complete: vec![HookCommandConfig {
                command: "scripts/team-complete.sh".into(),
                ..HookCommandConfig::default()
            }],
        },
        mcp_servers: std::collections::HashMap::from([(
            "policy".to_string(),
            McpServerConfig {
                transport: "http".into(),
                command: "node".into(),
                args: vec!["servers/policy.mjs".into()],
                env: std::collections::HashMap::from([(
                    "POLICY_LEVEL".to_string(),
                    "strict".to_string(),
                )]),
                url: "https://policy.example.test/mcp".into(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer test".to_string(),
                )]),
                ..McpServerConfig::default()
            },
        )]),
        ..Default::default()
    };

    let rendered = toml::to_string_pretty(&cfg).unwrap();
    let round: Config = toml::from_str(&rendered).unwrap();

    assert_eq!(round.default_chat_model, "test-model");
    assert_eq!(
        round.auth.api_key.as_deref(),
        Some("LTAI_sk_abcdefgh12345678")
    );
    assert_eq!(round.auth.wallet_address.as_deref(), Some("0xabcdef"));
    assert_eq!(round.auth.chain.as_deref(), Some("base"));
    assert_eq!(
        round.auth.expires_at.as_deref(),
        Some("2026-09-03T12:00:00Z")
    );
    assert_eq!(round.auth.device_id.as_deref(), Some("test-device-id"));
    assert_eq!(round.launcher_defaults.opus_model, "opus-x");
    assert_eq!(round.status_line_template, "{model} {ctx}");
    assert_eq!(round.hooks.user_prompt_submit.len(), 2);
    assert_eq!(
        round.hooks.user_prompt_submit[0].command,
        "scripts/user-prompt-submit.sh"
    );
    assert_eq!(
        round.hooks.user_prompt_submit[0].args,
        vec!["--flag".to_string(), "two words".to_string()]
    );
    assert_eq!(round.hooks.user_prompt_submit[0].timeout, Some(2));
    assert!(round.hooks.user_prompt_submit[0].continue_on_block);
    assert_eq!(round.hooks.user_prompt_submit[1].hook_type, "mcp_tool");
    assert_eq!(round.hooks.user_prompt_submit[1].server, "policy");
    assert_eq!(round.hooks.user_prompt_submit[1].tool, "check_prompt");
    assert_eq!(
        round.hooks.user_prompt_submit[1].input,
        Some(json!({ "level": "strict" }))
    );
    assert!(round.hooks.user_prompt_submit[1].once);
    assert!(round.hooks.user_prompt_submit[1].async_rewake);
    assert_eq!(round.hooks.user_prompt_submit[1].source, "project");
    assert_eq!(
        round.hooks.user_prompt_submit[1].status_message,
        "checking policy"
    );
    assert_eq!(
        round.hooks.user_prompt_submit[1].review_policy,
        "manual-review"
    );
    assert_eq!(
        round.hooks.user_prompt_submit[1].extra.get("customFlag"),
        Some(&json!(true))
    );
    assert_eq!(round.hooks.pre_tool_use.len(), 1);
    assert_eq!(round.hooks.pre_tool_use[0].matcher, "bash|write");
    assert_eq!(
        round.hooks.pre_tool_use[0].command,
        "scripts/pre-tool-use.sh"
    );
    assert_eq!(round.hooks.pre_tool_use[0].timeout, Some(5));
    assert_eq!(round.hooks.post_tool_use.len(), 1);
    assert_eq!(round.hooks.post_tool_use[0].matcher, "bash");
    assert_eq!(
        round.hooks.post_tool_use[0].command,
        "scripts/post-tool-use.sh"
    );
    assert_eq!(round.hooks.post_tool_use[0].timeout, Some(3));
    assert!(round.hooks.post_tool_use[0].async_hook);
    assert_eq!(round.hooks.subagent_stop.len(), 1);
    assert_eq!(round.hooks.subagent_stop[0].matcher, "task");
    assert_eq!(
        round.hooks.subagent_stop[0].command,
        "scripts/subagent-stop.sh"
    );
    assert_eq!(round.hooks.session_start.len(), 1);
    assert_eq!(
        round.hooks.session_start[0].command,
        "scripts/session-start.sh"
    );
    assert_eq!(round.hooks.stop.len(), 1);
    assert_eq!(round.hooks.stop[0].command, "scripts/stop.sh");
    assert_eq!(round.hooks.session_end.len(), 1);
    assert_eq!(round.hooks.session_end[0].command, "scripts/session-end.sh");
    assert_eq!(round.hooks.notification.len(), 1);
    assert_eq!(
        round.hooks.notification[0].command,
        "scripts/notification.sh"
    );
    assert_eq!(round.hooks.teammate_spawn.len(), 1);
    assert_eq!(
        round.hooks.teammate_spawn[0].command,
        "scripts/teammate-spawn.sh"
    );
    assert_eq!(round.hooks.task_complete.len(), 1);
    assert_eq!(
        round.hooks.task_complete[0].command,
        "scripts/task-complete.sh"
    );
    assert_eq!(round.hooks.team_complete.len(), 1);
    assert_eq!(
        round.hooks.team_complete[0].command,
        "scripts/team-complete.sh"
    );
    let policy_server = round.mcp_servers.get("policy").unwrap();
    assert_eq!(policy_server.transport, "http");
    assert_eq!(policy_server.command, "node");
    assert_eq!(policy_server.args, vec!["servers/policy.mjs".to_string()]);
    assert_eq!(
        policy_server.env.get("POLICY_LEVEL").map(String::as_str),
        Some("strict")
    );
    assert_eq!(policy_server.url, "https://policy.example.test/mcp");
    assert_eq!(
        policy_server
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer test")
    );
}

#[test]
fn async_hook_alias_roundtrips_to_async_field() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PostToolUse]]
matcher = "bash"
command = "scripts/post-tool-use.sh"
asyncHook = true
"#,
    )
    .unwrap();

    assert!(cfg.hooks.post_tool_use[0].async_hook);
    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains("async = true"));
}

#[test]
fn hook_matcher_array_deserializes_to_pipe_matcher() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PreToolUse]]
matcher = ["bash", "write", "  ", "mcp__github__*"]
command = "scripts/pre-tool-use.sh"
"#,
    )
    .unwrap();

    assert_eq!(
        cfg.hooks.pre_tool_use[0].matcher,
        "bash|write|mcp__github__*"
    );
    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains(r#"matcher = "bash|write|mcp__github__*""#));
}

#[test]
fn hook_matchers_alias_deserializes_to_matcher() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PreToolUse]]
matchers = ["bash", "write"]
command = "scripts/pre-tool-use.sh"
"#,
    )
    .unwrap();

    assert_eq!(cfg.hooks.pre_tool_use[0].matcher, "bash|write");
    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains(r#"matcher = "bash|write""#));
    assert!(!rendered.contains("matchers"));
}

#[test]
fn hook_command_array_deserializes_to_command_and_args() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PreToolUse]]
matcher = "bash"
command = ["scripts/pre-tool-use.sh", "--tool", "Bash(rm *)"]
args = ["--mode", "strict mode"]
"#,
    )
    .unwrap();

    let hook = &cfg.hooks.pre_tool_use[0];
    assert_eq!(hook.command, "scripts/pre-tool-use.sh");
    assert_eq!(
        hook.args,
        vec![
            "--tool".to_string(),
            "Bash(rm *)".to_string(),
            "--mode".to_string(),
            "strict mode".to_string()
        ]
    );
    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains(r#"command = "scripts/pre-tool-use.sh""#));
    let round: Config = toml::from_str(&rendered).unwrap();
    assert_eq!(
        round.hooks.pre_tool_use[0].command,
        "scripts/pre-tool-use.sh"
    );
    assert_eq!(round.hooks.pre_tool_use[0].args, hook.args);
}

#[test]
fn hook_review_policy_deserializes_as_named_field() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PreToolUse]]
matcher = "bash"
command = "scripts/pre-tool-use.sh"
reviewPolicy = "strict"
customFlag = true
"#,
    )
    .unwrap();

    let hook = &cfg.hooks.pre_tool_use[0];
    assert_eq!(hook.review_policy, "strict");
    assert_eq!(hook.extra.get("customFlag"), Some(&json!(true)));
    assert!(!hook.extra.contains_key("reviewPolicy"));
    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains(r#"reviewPolicy = "strict""#));
}

#[test]
fn claude_style_nested_hook_group_expands_handlers() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PreToolUse]]
matcher = ["bash", "write"]
if = "Bash(rm *)"
timeout = "11"
source = "project"
reviewPolicy = "strict"
asyncHook = true
continueOnBlock = true
once = true
asyncRewake = true
shell = "bash"
teamFlag = "shared"

[[hooks.PreToolUse.hooks]]
command = ["scripts/pre-tool-use.sh", "--policy"]

[[hooks.PreToolUse.hooks]]
type = "http"
url = "https://example.test/hook"
timeout = 3
"#,
    )
    .unwrap();

    assert_eq!(cfg.hooks.pre_tool_use.len(), 2);
    let command = &cfg.hooks.pre_tool_use[0];
    assert_eq!(command.matcher, "bash|write");
    assert_eq!(command.if_condition, "Bash(rm *)");
    assert_eq!(command.command, "scripts/pre-tool-use.sh");
    assert_eq!(command.args, vec!["--policy".to_string()]);
    assert_eq!(command.timeout, Some(11));
    assert_eq!(command.source, "project");
    assert_eq!(command.review_policy, "strict");
    assert!(command.async_hook);
    assert!(command.continue_on_block);
    assert!(command.once);
    assert!(command.async_rewake);
    assert_eq!(command.shell, "bash");
    assert_eq!(command.extra.get("teamFlag"), Some(&json!("shared")));

    let http = &cfg.hooks.pre_tool_use[1];
    assert_eq!(http.hook_type, "http");
    assert_eq!(http.matcher, "bash|write");
    assert_eq!(http.if_condition, "Bash(rm *)");
    assert_eq!(http.url, "https://example.test/hook");
    assert_eq!(http.timeout, Some(3));
    assert_eq!(http.source, "project");
    assert_eq!(http.review_policy, "strict");
    assert!(http.async_hook);
    assert!(http.continue_on_block);
    assert!(http.once);
    assert!(http.async_rewake);
    assert_eq!(http.shell, "bash");

    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains(r#"matcher = "bash|write""#));
    assert!(rendered.contains(r#"command = "scripts/pre-tool-use.sh""#));
    assert!(rendered.contains("continueOnBlock = true"));
    assert!(rendered.contains("once = true"));
    assert!(rendered.contains("asyncRewake = true"));
    assert!(rendered.contains(r#"shell = "bash""#));
    assert!(rendered.contains(r#"reviewPolicy = "strict""#));
    assert!(!rendered.contains("[[hooks.PreToolUse.hooks]]"));
}

#[test]
fn hook_timeout_string_deserializes_to_seconds() {
    let cfg: Config = toml::from_str(
        r#"
[[hooks.PostToolUse]]
matcher = "bash"
command = "scripts/post-tool-use.sh"
timeout = "9"
"#,
    )
    .unwrap();

    assert_eq!(cfg.hooks.post_tool_use[0].timeout, Some(9));
    let rendered = toml::to_string_pretty(&cfg).unwrap();
    assert!(rendered.contains("timeout = 9"));
}

#[test]
fn mask_key_hides_middle() {
    let masked = mask_key("LTAI_sk_abcdefgh12345678");
    assert!(masked.starts_with("LTAI"));
    assert!(masked.ends_with("5678"));
    assert!(masked.contains("****"));
}

#[test]
fn mask_key_short_key_all_stars() {
    assert_eq!(mask_key("short"), "*****");
    assert_eq!(mask_key(""), "");
}
