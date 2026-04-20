use anyhow::{Context, Result};
use dialoguer::console::Term;
use dialoguer::{Confirm, Input, Select};
use owo_colors::OwoColorize;

use crate::auth::wallet::{address_from_signing_key, personal_sign, signing_key_from_hex};
use crate::client::{auth_login, auth_message, create_api_key, list_models, ApiKeyCreate};
use crate::config::{self, config_path, mask_key, Auth, Config};

pub fn run() -> Result<()> {
    let term = Term::stderr();
    let options = &[
        "Paste API key",
        "Sign with wallet (Base)",
        "Open browser to console.libertai.io",
    ];

    let choice = Select::new()
        .with_prompt("How would you like to log in?")
        .items(options)
        .default(0)
        .interact_on(&term)
        .context("reading login choice")?;

    let mut cfg = config::load().context(
        "refusing to overwrite an unreadable config — fix or delete ~/.config/libertai/config.toml first",
    )?;

    match choice {
        0 => login_with_api_key(&mut cfg, &term)?,
        1 => login_with_wallet(&mut cfg, &term)?,
        2 => {
            eprintln!("Open https://console.libertai.io in your browser, create an API key, then paste it here.");
            let _ = open_url("https://console.libertai.io");
            login_with_api_key(&mut cfg, &term)?;
        }
        _ => unreachable!(),
    }

    config::save(&cfg)?;

    let masked = cfg
        .auth
        .api_key
        .as_deref()
        .map(mask_key)
        .unwrap_or_else(|| "<none>".to_string());
    let path = config_path()?;
    eprintln!("Logged in. Key: {masked}  →  {}", path.display());
    Ok(())
}

fn login_with_api_key(cfg: &mut Config, term: &Term) -> Result<()> {
    eprint!("API key: ");
    let key = term.read_secure_line().context("reading api key")?;
    let key = key.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("no api key entered");
    }

    let probe = Config {
        auth: Auth {
            api_key: Some(key.clone()),
            ..cfg.auth.clone()
        },
        ..cfg.clone()
    };
    list_models(&probe).context("verifying API key via /v1/models")?;

    cfg.auth.api_key = Some(key);
    Ok(())
}

fn login_with_wallet(cfg: &mut Config, term: &Term) -> Result<()> {
    eprint!("Private key (hex, with or without 0x): ");
    let pk_hex = zeroize::Zeroizing::new(
        term.read_secure_line().context("reading private key")?,
    );

    let sk = signing_key_from_hex(pk_hex.trim())?;
    let address = address_from_signing_key(&sk);
    eprintln!("Address: {address}");

    let message = auth_message(cfg, "base", &address).context("fetching auth message")?;
    confirm_signing(term, &cfg.account_base, &message)?;
    let signature = personal_sign(&sk, &message)?;
    let jwt = auth_login(cfg, "base", &address, &signature)
        .context("logging in with signature")?;

    let default_name = format!(
        "libertai-cli@{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into())
    );
    let name: String = Input::new()
        .with_prompt("API key name")
        .default(default_name)
        .interact_on(term)
        .context("reading key name")?;

    let limit_str: String = Input::new()
        .with_prompt("Monthly limit in USD (leave empty for none)")
        .allow_empty(true)
        .default(String::new())
        .interact_on(term)
        .context("reading monthly limit")?;
    let monthly_limit = if limit_str.trim().is_empty() {
        None
    } else {
        let v: f64 = limit_str
            .trim()
            .parse()
            .context("monthly limit must be a number")?;
        validate_limit(v)?;
        Some(v)
    };

    let created = create_api_key(
        cfg,
        &jwt,
        &ApiKeyCreate {
            name: &name,
            monthly_limit,
        },
    )
    .context("creating API key")?;

    cfg.auth.api_key = Some(created.full_key);
    cfg.auth.wallet_address = Some(address);
    cfg.auth.chain = Some("base".into());
    Ok(())
}

pub(crate) fn validate_limit(v: f64) -> Result<()> {
    if !v.is_finite() || v < 0.0 {
        anyhow::bail!("monthly limit must be a finite non-negative number (got {v})");
    }
    Ok(())
}

fn confirm_signing(term: &Term, account_base: &str, message: &str) -> Result<()> {
    let host = url::Url::parse(account_base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| account_base.to_string());
    eprintln!();
    eprintln!(
        "{}",
        "The server is asking you to sign this message:".yellow().bold()
    );
    eprintln!("  host:    {host}");
    eprintln!("  message: {message}");
    eprintln!();
    let ok = Confirm::new()
        .with_prompt("Sign this message with your private key?")
        .default(false)
        .interact_on(term)
        .context("reading signing confirmation")?;
    if !ok {
        anyhow::bail!("signing cancelled");
    }
    Ok(())
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &["open"]
        } else {
            &["xdg-open"]
        };
        for cmd in candidates {
            if std::process::Command::new(cmd)
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_limit;

    #[test]
    fn accepts_zero_and_positive() {
        assert!(validate_limit(0.0).is_ok());
        assert!(validate_limit(5.0).is_ok());
        assert!(validate_limit(1_000_000.0).is_ok());
    }

    #[test]
    fn rejects_negative() {
        assert!(validate_limit(-0.01).is_err());
        assert!(validate_limit(-1.0).is_err());
    }

    #[test]
    fn rejects_non_finite() {
        assert!(validate_limit(f64::NAN).is_err());
        assert!(validate_limit(f64::INFINITY).is_err());
        assert!(validate_limit(f64::NEG_INFINITY).is_err());
    }
}
