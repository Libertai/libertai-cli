use anyhow::{bail, Context, Result};

use crate::cli::ConfigAction;
use crate::config::{
    self, config_path, mask_key, DEFAULT_API_BASE, DEFAULT_CHAT_MODEL, DEFAULT_CHECK_FOR_UPDATES,
    DEFAULT_CODE_AUTO_COMPACTION_ENABLED, DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS,
    DEFAULT_CODE_COMPACTION_RESERVE_TOKENS, DEFAULT_CODE_COMPACTION_TOKEN_BUDGET_COMPACT,
    DEFAULT_CODE_MODEL, DEFAULT_CODE_PROVIDER, DEFAULT_CODE_TURN_NOTIFICATIONS, DEFAULT_FAST_MODEL,
    DEFAULT_HTTP_TIMEOUT_SECS, DEFAULT_IMAGE_MODEL, DEFAULT_OPUS_MODEL,
    DEFAULT_SMART_APPROVAL_ENABLED, DEFAULT_SMART_APPROVAL_MODEL,
};

pub fn run(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => show(),
        ConfigAction::Path => {
            println!("{}", config_path()?.display());
            Ok(())
        }
        ConfigAction::Set { key, value } => set(&key, &value),
        ConfigAction::Unset { key } => unset(&key),
    }
}

fn show() -> Result<()> {
    let mut cfg = config::load()?;
    if let Some(k) = cfg.auth.api_key.as_ref() {
        cfg.auth.api_key = Some(mask_key(k));
    }
    let rendered = toml::to_string_pretty(&cfg).context("serializing config")?;
    println!("{rendered}");
    Ok(())
}

fn set(key: &str, value: &str) -> Result<()> {
    let mut cfg = config::load()?;
    match key {
        "api_base" => cfg.api_base = value.to_string(),
        "account_base" => cfg.account_base = value.to_string(),
        "default_chat_model" => cfg.default_chat_model = value.to_string(),
        "default_code_model" => cfg.default_code_model = value.to_string(),
        "default_code_provider" => cfg.default_code_provider = value.to_string(),
        "default_image_model" => cfg.default_image_model = value.to_string(),
        "launcher_defaults.opus_model" => {
            cfg.launcher_defaults.opus_model = value.to_string()
        }
        "launcher_defaults.sonnet_model" => {
            cfg.launcher_defaults.sonnet_model = value.to_string()
        }
        "launcher_defaults.haiku_model" => {
            cfg.launcher_defaults.haiku_model = value.to_string()
        }
        "http_timeout_secs" => {
            let secs: u64 = value
                .parse()
                .with_context(|| format!("http_timeout_secs must be a positive integer, got {value}"))?;
            if secs == 0 {
                bail!("http_timeout_secs must be >= 1");
            }
            cfg.http_timeout_secs = secs;
        }
        "check_for_updates" => {
            cfg.check_for_updates = value.parse::<bool>().with_context(|| {
                format!("check_for_updates must be true or false, got {value}")
            })?;
        }
        "smart_approval_enabled" => {
            cfg.smart_approval_enabled = value.parse::<bool>().with_context(|| {
                format!("smart_approval_enabled must be true or false, got {value}")
            })?;
        }
        "smart_approval_model" => {
            if value.trim().is_empty() {
                bail!("smart_approval_model must not be empty");
            }
            cfg.smart_approval_model = value.to_string();
        }
        "code_auto_compaction_enabled" => {
            cfg.code_auto_compaction_enabled = value.parse::<bool>().with_context(|| {
                format!("code_auto_compaction_enabled must be true or false, got {value}")
            })?;
        }
        "code_compaction_reserve_tokens" => {
            cfg.code_compaction_reserve_tokens =
                parse_positive_u32("code_compaction_reserve_tokens", value)?;
        }
        "code_compaction_keep_recent_tokens" => {
            cfg.code_compaction_keep_recent_tokens =
                parse_positive_u32("code_compaction_keep_recent_tokens", value)?;
        }
        "code_compaction_token_budget_compact" => {
            cfg.code_compaction_token_budget_compact = value.parse::<bool>().with_context(|| {
                format!(
                    "code_compaction_token_budget_compact must be true or false, got {value}"
                )
            })?;
        }
        "code_turn_notifications" => {
            cfg.code_turn_notifications = value.parse::<bool>().with_context(|| {
                format!("code_turn_notifications must be true or false, got {value}")
            })?;
        }
        k if k.starts_with("auth.") => bail!(
            "'{k}' is managed by `libertai login`; edit manually at {} if you know what you're doing",
            config_path()?.display()
        ),
        _ => bail!("unknown config key: {key}"),
    }
    config::save(&cfg)?;
    eprintln!("Set {key} = {value}");
    Ok(())
}

fn unset(key: &str) -> Result<()> {
    let mut cfg = config::load()?;
    match key {
        "all" => {
            cfg.api_base = DEFAULT_API_BASE.into();
            cfg.account_base = DEFAULT_API_BASE.into();
            cfg.default_chat_model = DEFAULT_CHAT_MODEL.into();
            cfg.default_code_model = DEFAULT_CODE_MODEL.into();
            cfg.default_code_provider = DEFAULT_CODE_PROVIDER.into();
            cfg.default_image_model = DEFAULT_IMAGE_MODEL.into();
            cfg.launcher_defaults.opus_model = DEFAULT_OPUS_MODEL.into();
            cfg.launcher_defaults.sonnet_model = DEFAULT_FAST_MODEL.into();
            cfg.launcher_defaults.haiku_model = DEFAULT_FAST_MODEL.into();
            cfg.http_timeout_secs = DEFAULT_HTTP_TIMEOUT_SECS;
            cfg.check_for_updates = DEFAULT_CHECK_FOR_UPDATES;
            cfg.smart_approval_enabled = DEFAULT_SMART_APPROVAL_ENABLED;
            cfg.smart_approval_model = DEFAULT_SMART_APPROVAL_MODEL.into();
            cfg.code_auto_compaction_enabled = DEFAULT_CODE_AUTO_COMPACTION_ENABLED;
            cfg.code_compaction_reserve_tokens = DEFAULT_CODE_COMPACTION_RESERVE_TOKENS;
            cfg.code_compaction_keep_recent_tokens = DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS;
            cfg.code_compaction_token_budget_compact = DEFAULT_CODE_COMPACTION_TOKEN_BUDGET_COMPACT;
            cfg.code_turn_notifications = DEFAULT_CODE_TURN_NOTIFICATIONS;
            cfg.hooks = Default::default();
        }
        "api_base" => cfg.api_base = DEFAULT_API_BASE.into(),
        "account_base" => cfg.account_base = DEFAULT_API_BASE.into(),
        "default_chat_model" => cfg.default_chat_model = DEFAULT_CHAT_MODEL.into(),
        "default_code_model" => cfg.default_code_model = DEFAULT_CODE_MODEL.into(),
        "default_code_provider" => cfg.default_code_provider = DEFAULT_CODE_PROVIDER.into(),
        "default_image_model" => cfg.default_image_model = DEFAULT_IMAGE_MODEL.into(),
        "launcher_defaults" => {
            cfg.launcher_defaults.opus_model = DEFAULT_OPUS_MODEL.into();
            cfg.launcher_defaults.sonnet_model = DEFAULT_FAST_MODEL.into();
            cfg.launcher_defaults.haiku_model = DEFAULT_FAST_MODEL.into();
        }
        "launcher_defaults.opus_model" => {
            cfg.launcher_defaults.opus_model = DEFAULT_OPUS_MODEL.into()
        }
        "launcher_defaults.sonnet_model" => {
            cfg.launcher_defaults.sonnet_model = DEFAULT_FAST_MODEL.into()
        }
        "launcher_defaults.haiku_model" => {
            cfg.launcher_defaults.haiku_model = DEFAULT_FAST_MODEL.into()
        }
        "http_timeout_secs" => cfg.http_timeout_secs = DEFAULT_HTTP_TIMEOUT_SECS,
        "check_for_updates" => cfg.check_for_updates = DEFAULT_CHECK_FOR_UPDATES,
        "smart_approval_enabled" => cfg.smart_approval_enabled = DEFAULT_SMART_APPROVAL_ENABLED,
        "smart_approval_model" => cfg.smart_approval_model = DEFAULT_SMART_APPROVAL_MODEL.into(),
        "code_auto_compaction_enabled" => {
            cfg.code_auto_compaction_enabled = DEFAULT_CODE_AUTO_COMPACTION_ENABLED
        }
        "code_compaction_reserve_tokens" => {
            cfg.code_compaction_reserve_tokens = DEFAULT_CODE_COMPACTION_RESERVE_TOKENS
        }
        "code_compaction_keep_recent_tokens" => {
            cfg.code_compaction_keep_recent_tokens = DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS
        }
        "code_compaction_token_budget_compact" => {
            cfg.code_compaction_token_budget_compact = DEFAULT_CODE_COMPACTION_TOKEN_BUDGET_COMPACT
        }
        "code_turn_notifications" => cfg.code_turn_notifications = DEFAULT_CODE_TURN_NOTIFICATIONS,
        "hooks" => cfg.hooks = Default::default(),
        k if k.starts_with("auth.") => {
            bail!("'{k}' is managed by `libertai login`/`libertai logout`; unset is not supported")
        }
        _ => bail!("unknown config key: {key} (use `all` to reset everything)"),
    }
    config::save(&cfg)?;
    eprintln!("Reset {key} to built-in default");
    Ok(())
}

fn parse_positive_u32(key: &str, value: &str) -> Result<u32> {
    let parsed: u32 = value
        .parse()
        .with_context(|| format!("{key} must be a positive integer, got {value}"))?;
    if parsed == 0 {
        bail!("{key} must be >= 1");
    }
    Ok(parsed)
}
