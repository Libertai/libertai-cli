use anyhow::{bail, Context, Result};

use crate::cli::ConfigAction;
use crate::config::{self, config_path, mask_key};

pub fn run(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => show(),
        ConfigAction::Path => {
            println!("{}", config_path()?.display());
            Ok(())
        }
        ConfigAction::Set { key, value } => set(&key, &value),
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
