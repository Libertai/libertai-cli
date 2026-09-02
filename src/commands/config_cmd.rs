use anyhow::{bail, Context, Result};

use crate::cli::ConfigAction;
use crate::config::{self, config_path, mask_key, Config};

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
        "launcher_defaults.fable_model" => {
            cfg.launcher_defaults.fable_model = value.to_string()
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
    reset_key(&mut cfg, key)?;
    config::save(&cfg)?;
    eprintln!("Reset {key} to built-in default");
    Ok(())
}

/// Every arm must land on the same value `Config::default()` would produce —
/// anything else makes `unset` write an explicit non-default into the file,
/// since serde's `skip_serializing_if` only elides true defaults.
fn reset_key(cfg: &mut Config, key: &str) -> Result<()> {
    let d = Config::default();
    match key {
        "all" => {
            // Credentials, MCP servers, plugins and status-line settings survive:
            // `unset all` resets tunables, not identity or installed state.
            cfg.api_base = d.api_base;
            cfg.account_base = d.account_base;
            cfg.default_chat_model = d.default_chat_model;
            cfg.default_code_model = d.default_code_model;
            cfg.default_code_provider = d.default_code_provider;
            cfg.default_image_model = d.default_image_model;
            cfg.launcher_defaults = d.launcher_defaults;
            cfg.http_timeout_secs = d.http_timeout_secs;
            cfg.check_for_updates = d.check_for_updates;
            cfg.smart_approval_enabled = d.smart_approval_enabled;
            cfg.smart_approval_model = d.smart_approval_model;
            cfg.code_auto_compaction_enabled = d.code_auto_compaction_enabled;
            cfg.code_compaction_reserve_tokens = d.code_compaction_reserve_tokens;
            cfg.code_compaction_keep_recent_tokens = d.code_compaction_keep_recent_tokens;
            cfg.code_compaction_token_budget_compact = d.code_compaction_token_budget_compact;
            cfg.code_turn_notifications = d.code_turn_notifications;
            cfg.hooks = d.hooks;
        }
        "api_base" => cfg.api_base = d.api_base,
        "account_base" => cfg.account_base = d.account_base,
        "default_chat_model" => cfg.default_chat_model = d.default_chat_model,
        "default_code_model" => cfg.default_code_model = d.default_code_model,
        "default_code_provider" => cfg.default_code_provider = d.default_code_provider,
        "default_image_model" => cfg.default_image_model = d.default_image_model,
        "launcher_defaults" => cfg.launcher_defaults = d.launcher_defaults,
        "launcher_defaults.opus_model" => {
            cfg.launcher_defaults.opus_model = d.launcher_defaults.opus_model
        }
        "launcher_defaults.sonnet_model" => {
            cfg.launcher_defaults.sonnet_model = d.launcher_defaults.sonnet_model
        }
        "launcher_defaults.fable_model" => {
            cfg.launcher_defaults.fable_model = d.launcher_defaults.fable_model
        }
        "launcher_defaults.haiku_model" => {
            cfg.launcher_defaults.haiku_model = d.launcher_defaults.haiku_model
        }
        "http_timeout_secs" => cfg.http_timeout_secs = d.http_timeout_secs,
        "check_for_updates" => cfg.check_for_updates = d.check_for_updates,
        "smart_approval_enabled" => cfg.smart_approval_enabled = d.smart_approval_enabled,
        "smart_approval_model" => cfg.smart_approval_model = d.smart_approval_model,
        "code_auto_compaction_enabled" => {
            cfg.code_auto_compaction_enabled = d.code_auto_compaction_enabled
        }
        "code_compaction_reserve_tokens" => {
            cfg.code_compaction_reserve_tokens = d.code_compaction_reserve_tokens
        }
        "code_compaction_keep_recent_tokens" => {
            cfg.code_compaction_keep_recent_tokens = d.code_compaction_keep_recent_tokens
        }
        "code_compaction_token_budget_compact" => {
            cfg.code_compaction_token_budget_compact = d.code_compaction_token_budget_compact
        }
        "code_turn_notifications" => cfg.code_turn_notifications = d.code_turn_notifications,
        "hooks" => cfg.hooks = d.hooks,
        k if k.starts_with("auth.") => {
            bail!("'{k}' is managed by `libertai login`/`libertai logout`; unset is not supported")
        }
        _ => bail!("unknown config key: {key} (use `all` to reset everything)"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LauncherDefaults;

    /// Keys `unset` accepts, other than the group resets covered separately.
    const RESETTABLE_KEYS: &[&str] = &[
        "api_base",
        "account_base",
        "default_chat_model",
        "default_code_model",
        "default_code_provider",
        "default_image_model",
        "launcher_defaults.opus_model",
        "launcher_defaults.sonnet_model",
        "launcher_defaults.fable_model",
        "launcher_defaults.haiku_model",
        "http_timeout_secs",
        "check_for_updates",
        "smart_approval_enabled",
        "smart_approval_model",
        "code_auto_compaction_enabled",
        "code_compaction_reserve_tokens",
        "code_compaction_keep_recent_tokens",
        "code_compaction_token_budget_compact",
        "code_turn_notifications",
        "hooks",
    ];

    /// A config where every resettable field differs from the default, so a
    /// reset that writes the wrong value can't accidentally match.
    fn scrambled() -> Config {
        let d = Config::default();
        Config {
            api_base: "https://wrong.example".into(),
            account_base: "https://wrong.example".into(),
            default_chat_model: "wrong-chat".into(),
            default_code_model: "wrong-code".into(),
            default_code_provider: "wrong-provider".into(),
            default_image_model: "wrong-image".into(),
            launcher_defaults: LauncherDefaults {
                opus_model: "wrong-opus".into(),
                sonnet_model: "wrong-sonnet".into(),
                fable_model: "wrong-fable".into(),
                haiku_model: "wrong-haiku".into(),
            },
            http_timeout_secs: 7,
            check_for_updates: !d.check_for_updates,
            smart_approval_enabled: !d.smart_approval_enabled,
            smart_approval_model: "wrong-approval".into(),
            code_auto_compaction_enabled: !d.code_auto_compaction_enabled,
            code_compaction_reserve_tokens: 1,
            code_compaction_keep_recent_tokens: 1,
            code_compaction_token_budget_compact: !d.code_compaction_token_budget_compact,
            code_turn_notifications: !d.code_turn_notifications,
            ..d
        }
    }

    /// Serialization is the oracle: a field left at its default is skipped by
    /// `skip_serializing_if`, so an empty result proves every reset landed on
    /// the built-in default. `[auth]` is always emitted and carries no tunable.
    fn non_default_keys(cfg: &Config) -> String {
        toml::to_string_pretty(cfg)
            .expect("serializing config")
            .lines()
            .filter(|l| !l.trim().is_empty() && *l != "[auth]")
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn reset_all_restores_every_tunable_to_its_default() {
        let mut cfg = scrambled();
        reset_key(&mut cfg, "all").expect("reset all");
        assert_eq!(
            non_default_keys(&cfg),
            "",
            "`unset all` left non-default values behind"
        );
    }

    #[test]
    fn resetting_each_key_individually_restores_its_default() {
        let mut cfg = scrambled();
        for key in RESETTABLE_KEYS {
            reset_key(&mut cfg, key).unwrap_or_else(|e| panic!("reset {key}: {e}"));
        }
        assert_eq!(
            non_default_keys(&cfg),
            "",
            "resetting every key one at a time did not restore the defaults"
        );
    }

    #[test]
    fn resetting_launcher_group_restores_all_four_aliases() {
        let mut cfg = scrambled();
        reset_key(&mut cfg, "launcher_defaults").expect("reset launcher_defaults");
        let d = Config::default();
        assert_eq!(
            cfg.launcher_defaults.opus_model,
            d.launcher_defaults.opus_model
        );
        assert_eq!(
            cfg.launcher_defaults.sonnet_model,
            d.launcher_defaults.sonnet_model
        );
        assert_eq!(
            cfg.launcher_defaults.fable_model,
            d.launcher_defaults.fable_model
        );
        assert_eq!(
            cfg.launcher_defaults.haiku_model,
            d.launcher_defaults.haiku_model
        );
    }

    #[test]
    fn auth_and_unknown_keys_are_rejected() {
        let mut cfg = Config::default();
        assert!(reset_key(&mut cfg, "auth.api_key").is_err());
        assert!(reset_key(&mut cfg, "nope").is_err());
    }
}
