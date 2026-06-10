use anyhow::{bail, Context, Result};
use dialoguer::console::Term;
use dialoguer::{Confirm, Password};
use owo_colors::OwoColorize;

use crate::auth::wallet::{address_from_signing_key, personal_sign, signing_key_from_hex};
use crate::cli::KeysAction;
use crate::client::{
    auth_login, auth_message, create_api_key, delete_api_key, list_api_keys, ApiKeyCreate,
};
use crate::commands::auth_ui::{confirm_signing, validate_limit};
use crate::commands::login::{browser_sso_access_token, open_url};
use crate::config::{load, Config};

pub fn run(action: KeysAction) -> Result<()> {
    let cfg = load()?;
    match action {
        KeysAction::List => list(&cfg),
        KeysAction::Create { name, limit } => create(&cfg, name, limit),
        KeysAction::Delete { id } => delete(&cfg, id),
    }
}

/// Key management (`/api-keys`) requires a session token — the stored `LTAI_`
/// inference key cannot authenticate it. Legacy wallet logins sign for a JWT
/// locally; everyone else (browser-SSO and pasted-key logins) confirms in the
/// browser via the same loopback flow `libertai login` uses.
fn acquire_jwt(cfg: &Config) -> Result<String> {
    match (
        cfg.auth.wallet_address.as_deref(),
        cfg.auth.chain.as_deref(),
    ) {
        (Some(address), Some(chain)) => acquire_jwt_wallet(cfg, address, chain),
        _ => acquire_jwt_browser(cfg),
    }
}

fn acquire_jwt_browser(cfg: &Config) -> Result<String> {
    eprintln!(
        "{} Managing keys needs a quick sign-in confirmation in your browser.",
        "!".yellow()
    );
    browser_sso_access_token(cfg, "LibertAI CLI (key management)", |url| {
        eprintln!("Opening your browser to sign in…");
        eprintln!("If it doesn't open, visit:\n  {url}");
        let _ = open_url(url);
    })
}

fn acquire_jwt_wallet(cfg: &Config, address: &str, chain: &str) -> Result<String> {
    eprintln!("{} Signing in as {} on {}.", "!".yellow(), address, chain);
    let pk = zeroize::Zeroizing::new(
        Password::new()
            .with_prompt("Private key (hex)")
            .interact()
            .context("reading private key")?,
    );

    let sk = signing_key_from_hex(&pk)?;
    let derived = address_from_signing_key(&sk);
    if !derived.eq_ignore_ascii_case(address) {
        bail!(
            "derived address {} does not match configured wallet {}",
            derived,
            address
        );
    }
    let message = auth_message(cfg, chain, address)?;
    confirm_signing(&Term::stderr(), &cfg.account_base, &message)?;
    let signature = personal_sign(&sk, &message)?;
    let jwt = auth_login(cfg, chain, address, &signature)?;
    Ok(jwt)
}

fn list(cfg: &Config) -> Result<()> {
    let jwt = acquire_jwt(cfg)?;
    let rows = list_api_keys(cfg, &jwt)?;

    if rows.is_empty() {
        eprintln!("No API keys.");
        return Ok(());
    }

    let id_w = rows
        .iter()
        .map(|r| r.id.chars().count())
        .max()
        .unwrap_or(2)
        .max("ID".len());
    let name_w = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(4)
        .max("NAME".len());

    println!(
        "{:<id_w$}  {:<name_w$}  {:>14}  {:<20}  {}",
        "ID".bold(),
        "NAME".bold(),
        "MONTHLY LIMIT".bold(),
        "CREATED".bold(),
        "ACTIVE".bold(),
        id_w = id_w,
        name_w = name_w,
    );
    for r in &rows {
        let limit = r
            .monthly_limit
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "-".into());
        let created = r.created_at.as_deref().unwrap_or("-");
        let active = if r.is_active { "Y" } else { "N" };
        println!(
            "{:<id_w$}  {:<name_w$}  {:>14}  {:<20}  {}",
            r.id,
            r.name,
            limit,
            created,
            active,
            id_w = id_w,
            name_w = name_w,
        );
    }
    Ok(())
}

fn create(cfg: &Config, name: String, limit: Option<f64>) -> Result<()> {
    if let Some(v) = limit {
        validate_limit(v)?;
    }
    let jwt = acquire_jwt(cfg)?;
    let created = create_api_key(
        cfg,
        &jwt,
        &ApiKeyCreate {
            name: &name,
            monthly_limit: limit,
        },
    )?;

    eprintln!("{} created API key:", "ok:".green());
    eprintln!("id:   {}", created.id);
    eprintln!("name: {}", created.name);
    eprintln!("key:  {}", created.full_key.bold());
    eprintln!(
        "{} This is the only time this key will be shown.",
        "!".yellow().bold()
    );
    Ok(())
}

fn delete(cfg: &Config, id: String) -> Result<()> {
    let confirmed = Confirm::new()
        .with_prompt(format!("Delete key {id}?"))
        .default(false)
        .interact()
        .context("reading confirmation")?;
    if !confirmed {
        eprintln!("aborted.");
        return Ok(());
    }
    let jwt = acquire_jwt(cfg)?;
    delete_api_key(cfg, &jwt, &id)?;
    eprintln!("{} deleted key {}", "ok:".green(), id);
    Ok(())
}
