use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use dialoguer::console::Term;
use dialoguer::{Input, Select};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::auth::wallet::{address_from_signing_key, personal_sign, signing_key_from_hex};
use crate::client::{
    auth_login, auth_message, create_api_key, create_cli_api_key, exchange_code, list_models, ApiKeyCreate,
};
use crate::commands::auth_ui::{confirm_signing, validate_limit};
use crate::config::{self, config_path, mask_key, Auth, Config};

const DEFAULT_CONSOLE_BASE: &str = "https://console.libertai.io";

pub fn run() -> Result<()> {
    let term = Term::stderr();
    let options = &[
        "Sign in with your browser (recommended)",
        "Paste API key",
        "Sign with wallet private key (Base)",
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
        0 => login_with_browser(&mut cfg)?,
        1 => login_with_api_key(&mut cfg, &term)?,
        2 => login_with_wallet(&mut cfg, &term)?,
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
    let expiry_note = cfg
        .auth
        .expires_at
        .as_deref()
        .map(|e| format!("  (expires {})", e.split('T').next().unwrap_or(e)))
        .unwrap_or_default();
    eprintln!("Logged in. Key: {masked}{expiry_note}  →  {}", path.display());
    Ok(())
}

/// Browser SSO via OAuth-style loopback + PKCE:
///  1. start a local one-shot HTTP server on 127.0.0.1:<port>
///  2. open the console /cli page (the user signs in by any method, then approves)
///  3. the console redirects the browser back to the loopback with a one-time code
///  4. exchange code + PKCE verifier for a session token, then mint this device's CLI key
fn login_with_browser(cfg: &mut Config) -> Result<()> {
    // PKCE: keep `verifier` secret; send only its SHA256 (the challenge).
    let mut vbytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut vbytes);
    let verifier = URL_SAFE_NO_PAD.encode(vbytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut sbytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut sbytes);
    let state = URL_SAFE_NO_PAD.encode(sbytes);

    // One-shot loopback server; OS picks a free port.
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| anyhow!("could not start local login server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("could not determine local login server port"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut authorize = url::Url::parse(&format!("{}/cli", console_base()))
        .context("building console authorize URL")?;
    authorize
        .query_pairs_mut()
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("challenge", &challenge);
    let authorize = authorize.to_string();

    eprintln!("Opening your browser to sign in…");
    eprintln!("If it doesn't open, visit:\n  {authorize}");
    let _ = open_url(&authorize);

    // Block until the browser hits the loopback callback (single request).
    let (code, returned_state) = wait_for_callback(server)?;
    if returned_state != state {
        anyhow::bail!("login state mismatch — aborting (possible interference)");
    }

    let access_token = exchange_code(cfg, &code, &verifier).context("exchanging login code")?;
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into());
    let created = create_cli_api_key(cfg, &access_token, &host).context("creating CLI API key")?;

    cfg.auth.expires_at = created.expires_at;
    cfg.auth.api_key = Some(created.full_key);
    cfg.auth.wallet_address = None;
    cfg.auth.chain = None;
    Ok(())
}

fn console_base() -> String {
    std::env::var("LIBERTAI_CONSOLE_URL")
        .unwrap_or_else(|_| DEFAULT_CONSOLE_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// A clean, centered standalone page shown in the browser after the redirect.
fn callback_page(accent: &str, glyph: &str, title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>LibertAI CLI</title>\
<style>html,body{{height:100%;margin:0}}\
body{{display:flex;align-items:center;justify-content:center;\
font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\
background:#0b0b0f;color:#e5e7eb}}\
.card{{text-align:center;padding:2.5rem 3rem;max-width:24rem}}\
.badge{{width:56px;height:56px;border-radius:9999px;background:{accent};color:#fff;\
font-size:30px;line-height:56px;margin:0 auto 1.25rem}}\
h1{{font-size:1.25rem;font-weight:600;margin:0 0 .5rem}}\
p{{margin:0;color:#9ca3af;font-size:.95rem;line-height:1.4}}</style></head>\
<body><div class=\"card\"><div class=\"badge\">{glyph}</div>\
<h1>{title}</h1><p>{message}</p></div></body></html>"
    )
}

/// Serve one request to `/callback`, reply with a "you can close this tab" page,
/// and return its `code` + `state` query params.
fn wait_for_callback(server: tiny_http::Server) -> Result<(String, String)> {
    let request = server.recv().context("waiting for the browser login callback")?;

    // The request line carries only the path+query; parse it with a dummy base.
    let parsed = url::Url::parse(&format!("http://127.0.0.1{}", request.url()))
        .context("parsing login callback URL")?;
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut err: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => err = Some(v.into_owned()),
            _ => {}
        }
    }

    let ok = code.is_some() && err.is_none();
    let body = if ok {
        callback_page(
            "#10b981",
            "\u{2713}",
            "Signed in to LibertAI",
            "You can now close this page and return to your terminal.",
        )
    } else {
        callback_page(
            "#ef4444",
            "\u{00d7}",
            "Sign-in failed",
            "Something went wrong. Return to your terminal and run libertai login again.",
        )
    };
    let header = "Content-Type: text/html; charset=utf-8"
        .parse::<tiny_http::Header>()
        .map_err(|_| anyhow!("building response header"))?;
    let _ = request.respond(tiny_http::Response::from_string(body).with_header(header));

    if let Some(e) = err {
        anyhow::bail!("login was rejected: {e}");
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err(anyhow!("login callback missing code/state")),
    }
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
    cfg.auth.expires_at = None; // pasted keys carry no CLI expiry
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
    cfg.auth.expires_at = None; // wallet-created keys don't expire
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

