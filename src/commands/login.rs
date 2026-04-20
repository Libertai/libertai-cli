use anyhow::{Context, Result};
use dialoguer::console::Term;
use dialoguer::{Input, Select};

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

    let mut cfg = config::load().unwrap_or_default();

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
        Some(
            limit_str
                .trim()
                .parse::<f64>()
                .context("monthly limit must be a number")?,
        )
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

fn open_url(url: &str) -> Result<()> {
    let candidates = if cfg!(target_os = "macos") {
        &["open"][..]
    } else if cfg!(target_os = "windows") {
        &["start"][..]
    } else {
        &["xdg-open"][..]
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
