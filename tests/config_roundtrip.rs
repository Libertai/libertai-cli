//! Guards the on-disk config shape and the key-masking helper.

use libertai_cli::config::{mask_key, Auth, Config, HookCommandConfig, HooksConfig, LauncherDefaults};

#[test]
fn empty_toml_parses_as_defaults() {
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.api_base, "https://api.libertai.io");
    assert_eq!(cfg.default_chat_model, "qwen3.5-122b-a10b");
    assert_eq!(cfg.default_code_model, "qwen3.6-35b-a3b");
    assert_eq!(cfg.default_image_model, "z-image-turbo");
    assert_eq!(cfg.launcher_defaults.opus_model, "gemma-4-31b-it");
    assert_eq!(cfg.launcher_defaults.sonnet_model, "qwen3.6-35b-a3b");
    assert_eq!(cfg.launcher_defaults.haiku_model, "qwen3.6-35b-a3b");
    assert!(cfg.status_line_template.is_empty());
    assert!(cfg.hooks.user_prompt_submit.is_empty());
    assert!(cfg.hooks.pre_tool_use.is_empty());
    assert!(cfg.hooks.post_tool_use.is_empty());
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
        },
        launcher_defaults: LauncherDefaults {
            opus_model: "opus-x".into(),
            ..Default::default()
        },
        status_line_template: "{model} {ctx}".into(),
        hooks: HooksConfig {
            user_prompt_submit: vec![HookCommandConfig {
                command: "scripts/user-prompt-submit.sh".into(),
                timeout: Some(2),
                ..HookCommandConfig::default()
            }],
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
                ..HookCommandConfig::default()
            }],
        },
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
    assert_eq!(round.launcher_defaults.opus_model, "opus-x");
    assert_eq!(round.status_line_template, "{model} {ctx}");
    assert_eq!(round.hooks.user_prompt_submit.len(), 1);
    assert_eq!(
        round.hooks.user_prompt_submit[0].command,
        "scripts/user-prompt-submit.sh"
    );
    assert_eq!(round.hooks.user_prompt_submit[0].timeout, Some(2));
    assert_eq!(round.hooks.pre_tool_use.len(), 1);
    assert_eq!(round.hooks.pre_tool_use[0].matcher, "bash|write");
    assert_eq!(round.hooks.pre_tool_use[0].command, "scripts/pre-tool-use.sh");
    assert_eq!(round.hooks.pre_tool_use[0].timeout, Some(5));
    assert_eq!(round.hooks.post_tool_use.len(), 1);
    assert_eq!(round.hooks.post_tool_use[0].matcher, "bash");
    assert_eq!(round.hooks.post_tool_use[0].command, "scripts/post-tool-use.sh");
    assert_eq!(round.hooks.post_tool_use[0].timeout, Some(3));
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
