use anyhow::{bail, Context, Result};

use crate::cli::ConfigAction;
use crate::config::{
    self, config_path, mask_key, DEFAULT_API_BASE, DEFAULT_CHAT_MODEL, DEFAULT_FAST_MODEL,
    DEFAULT_IMAGE_MODEL, DEFAULT_OPUS_MODEL,
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
            cfg.default_image_model = DEFAULT_IMAGE_MODEL.into();
            cfg.launcher_defaults.opus_model = DEFAULT_OPUS_MODEL.into();
            cfg.launcher_defaults.sonnet_model = DEFAULT_FAST_MODEL.into();
            cfg.launcher_defaults.haiku_model = DEFAULT_FAST_MODEL.into();
        }
        "api_base" => cfg.api_base = DEFAULT_API_BASE.into(),
        "account_base" => cfg.account_base = DEFAULT_API_BASE.into(),
        "default_chat_model" => cfg.default_chat_model = DEFAULT_CHAT_MODEL.into(),
        "default_image_model" => cfg.default_image_model = DEFAULT_IMAGE_MODEL.into(),
        "launcher_defaults" => {
            cfg.launcher_defaults.opus_model = DEFAULT_OPUS_MODEL.into();
            cfg.launcher_defaults.sonnet_model = DEFAULT_FAST_MODEL.into();
            cfg.launcher_defaults.haiku_model = DEFAULT_FAST_MODEL.into();
        }
        "launcher_defaults.opus_model" => cfg.launcher_defaults.opus_model = DEFAULT_OPUS_MODEL.into(),
        "launcher_defaults.sonnet_model" => cfg.launcher_defaults.sonnet_model = DEFAULT_FAST_MODEL.into(),
        "launcher_defaults.haiku_model" => cfg.launcher_defaults.haiku_model = DEFAULT_FAST_MODEL.into(),
        k if k.starts_with("auth.") => bail!(
            "'{k}' is managed by `libertai login`/`libertai logout`; unset is not supported"
        ),
        _ => bail!("unknown config key: {key} (use `all` to reset everything)"),
    }
    config::save(&cfg)?;
    eprintln!("Reset {key} to built-in default");
    Ok(())
}
