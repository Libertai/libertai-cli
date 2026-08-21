use std::io::{IsTerminal, Write};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use dialoguer::console::Term;
use dialoguer::Select;
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::client::{create_cli_api_key, exchange_code, list_models};
use crate::config::{self, config_path, mask_key, Auth, Config};

const DEFAULT_CONSOLE_BASE: &str = "https://console.libertai.io";

pub fn run() -> Result<()> {
    let term = Term::stderr();
    let options = &[
        "Sign in with your browser (recommended)",
        "Paste API key (inference only — no usage/keys)",
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
    eprintln!(
        "Logged in. Key: {masked}{expiry_note}  →  {}",
        path.display()
    );
    Ok(())
}

/// CLI wrapper around the reusable browser-SSO flow: prints progress + the
/// manual-fallback URL to stderr and opens the system browser.
fn login_with_browser(cfg: &mut Config) -> Result<()> {
    browser_sso_login(cfg, "LibertAI CLI", |url| {
        eprintln!("Opening your browser to sign in…");
        eprintln!("If it doesn't open, visit:\n  {url}");
        print_login_qr(url);
        let _ = open_url(url);
    })
}

/// Reusable browser SSO via OAuth-style loopback + PKCE. Shared by the CLI and the
/// desktop app (which depends on this crate):
///  1. start a local one-shot HTTP server on 127.0.0.1:<port>
///  2. call `open` with the console /cli authorize URL so the frontend opens it
///     (system browser, webview, …) and can surface it as a manual fallback
///  3. the console redirects the browser back to the loopback with a one-time code
///  4. exchange code + PKCE verifier for a session token, then mint this device's CLI key
///
/// `client` is a human label for the app starting the flow (e.g. "LibertAI CLI",
/// "LibertAI Desktop"); the console authorize page shows it.
///
/// On success, `cfg.auth` is updated with the minted key (the caller persists `cfg`).
pub fn browser_sso_login(cfg: &mut Config, client: &str, open: impl FnOnce(&str)) -> Result<()> {
    let pair = browser_sso_access_token(cfg, client, open)?;

    // Per-device key: a stable random id (persisted in config) keeps this device's key
    // name unique, so logging in elsewhere mints a separate key instead of rotating —
    // and disconnecting — this one. Re-login on this device reuses the id (rotates in place).
    let device_id = cfg.auth.device_id.clone().unwrap_or_else(new_device_id);
    cfg.auth.device_id = Some(device_id.clone());
    let host = format!("{}-{}", device_hostname(), device_id);
    let created =
        create_cli_api_key(cfg, &pair.access_token, &host).context("creating CLI API key")?;

    cfg.auth.expires_at = created.expires_at;
    cfg.auth.api_key = Some(created.full_key);
    cfg.auth.refresh_token = Some(pair.refresh_token);
    cfg.auth.wallet_address = None;
    cfg.auth.chain = None;
    Ok(())
}

/// Steps 1–4 of the browser SSO flow, stopping after the code exchange: returns
/// the short-lived session access token without minting a key. `libertai login`
/// uses it to then mint this device's CLI key; `libertai keys` uses the token
/// directly, because the account's key-management endpoints (`/api-keys`)
/// require a session token — the stored `LTAI_` inference key cannot
/// authenticate them.
pub fn browser_sso_access_token(
    cfg: &Config,
    client: &str,
    open: impl FnOnce(&str),
) -> Result<crate::client::TokenPair> {
    // PKCE: keep `verifier` secret; send only its SHA256 (the challenge).
    let mut vbytes = [0u8; 32];
    rand::rng().fill_bytes(&mut vbytes);
    let verifier = URL_SAFE_NO_PAD.encode(vbytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut sbytes = [0u8; 16];
    rand::rng().fill_bytes(&mut sbytes);
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
        .append_pair("challenge", &challenge)
        .append_pair("client", client);
    let authorize = authorize.to_string();

    open(&authorize);

    // Wait for the one-time code from EITHER the loopback callback OR a code
    // the user pastes by hand (remote-browser fallback). State is verified
    // whenever it is known — always for the loopback path, and for a pasted
    // full redirect URL; a bare pasted code carries none, and the PKCE
    // verifier still guards the exchange.
    let (code, returned_state) = collect_login_code(server)?;
    if let Some(returned) = returned_state {
        if returned != state {
            anyhow::bail!("login state mismatch — aborting (possible interference)");
        }
    }

    exchange_code(cfg, &code, &verifier).context("exchanging login code")
}

/// Random 8-hex-char id identifying this CLI install (not security-sensitive).
fn new_device_id() -> String {
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Best-effort short hostname for a recognizable key name in the console; falls back
/// to the HOSTNAME env var, then "device". Uniqueness comes from the device id, not this.
/// Uses the gethostname(2) syscall wrapper rather than shelling out to a `hostname`
/// binary, which doesn't exist on minimal containers or Windows.
fn device_hostname() -> String {
    let raw = gethostname::gethostname()
        .into_string()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "device".into());
    // Strip the mDNS suffix and keep the leading label: "Rezas-MBP.local" -> "Rezas-MBP".
    raw.split('.').next().unwrap_or("device").to_string()
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
    // Bound the wait so a closed/abandoned browser doesn't hang the caller forever
    // (the loopback can't otherwise detect that the user gave up).
    let request = match server
        .recv_timeout(std::time::Duration::from_secs(300))
        .context("waiting for the browser login callback")?
    {
        Some(req) => req,
        None => {
            anyhow::bail!("timed out waiting for browser sign-in — no response after 5 minutes")
        }
    };

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

/// Print a scannable QR code of the sign-in URL to stderr so the user can open
/// it from a phone or another device — the remote-browser path. Only shown on
/// an interactive terminal; a render failure (e.g. the URL is too long to
/// encode) is silently skipped, since the printed URL is the real fallback.
fn print_login_qr(url: &str) {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::EcLevel;
    if !std::io::stderr().is_terminal() {
        return;
    }
    // Error-correction level L (lowest redundancy) keeps the module count — and
    // so the on-screen footprint — as small as the URL allows. A terminal QR is
    // read from a clean, undamaged screen, so the extra recovery of higher
    // levels buys nothing here. Dense1x2 packs two vertical modules per cell,
    // the one mapping that renders each module square; the quiet zone (light
    // border) is required for scanners to lock on.
    let Ok(code) = qrcode::QrCode::with_error_correction_level(url.as_bytes(), EcLevel::L) else {
        return;
    };
    let rendered = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    eprintln!("Or scan this QR code to sign in from your phone:");
    eprintln!("{rendered}");
}

/// Wait for the login code from EITHER the loopback callback OR, in an
/// interactive terminal, a code the user pastes by hand. Racing the two covers
/// the remote-browser case: a browser on another machine can't reach this
/// process's `127.0.0.1:<port>` loopback, so the redirect never arrives here —
/// but the user can copy the `code` (or the whole redirect URL) out of the
/// browser's address bar and paste it instead.
///
/// Returns the `code` plus the `state` when known (always for the loopback
/// path; for the manual path only when the pasted text carried it). Whichever
/// source produces input first wins; the loser is abandoned — the process is
/// short-lived, so a still-blocked reader thread is harmless.
fn collect_login_code(server: tiny_http::Server) -> Result<(String, Option<String>)> {
    enum Msg {
        Callback(Result<(String, String)>),
        Manual(String),
    }
    let (tx, rx) = std::sync::mpsc::channel();

    let tx_cb = tx.clone();
    std::thread::spawn(move || {
        let _ = tx_cb.send(Msg::Callback(wait_for_callback(server)));
    });

    // Only offer the manual paste when stdin is an interactive terminal, so the
    // desktop-app (GUI) caller and non-interactive test harness keep the plain
    // loopback behaviour and never spawn a reader that blocks on stdin.
    if std::io::stdin().is_terminal() {
        eprintln!();
        eprintln!("If your browser is on a different machine it can't reach this terminal's");
        eprintln!("local callback. Sign in there, then paste the code from the address bar");
        eprintln!("(the value after `code=`, or the whole redirect URL).");
        eprint!("Enter the code: ");
        let _ = std::io::stderr().flush();
        std::thread::spawn(move || {
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                let _ = tx.send(Msg::Manual(line));
            }
        });
    } else {
        drop(tx);
    }

    match rx.recv_timeout(std::time::Duration::from_secs(300)) {
        Ok(Msg::Callback(res)) => res.map(|(code, state)| (code, Some(state))),
        Ok(Msg::Manual(line)) => parse_manual_code(&line)
            .ok_or_else(|| anyhow!("could not find a login code in the pasted text")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!("timed out waiting for sign-in — no response after 5 minutes")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("sign-in aborted before a code was received")
        }
    }
}

/// Parse the code out of text the user pasted at the manual prompt. Accepts a
/// bare code, a `code=…&state=…` query fragment, or a full redirect URL, and
/// returns `(code, state?)`. Returns `None` when no `code` can be found.
fn parse_manual_code(input: &str) -> Option<(String, Option<String>)> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // A plain token with no URL/query structure is the code itself. Anything
    // that looks like a URL or query string is parsed, and MUST yield a
    // `code=` — a codeless redirect (e.g. an `error=` or state-only URL) is
    // rejected rather than mistaken for a bare code.
    let looks_structured =
        s.contains("://") || s.contains('?') || s.contains("code=") || s.contains("state=");
    if !looks_structured {
        return Some((s.to_string(), None));
    }
    let url_str = if s.contains("://") {
        s.to_string()
    } else {
        let query = s.rsplit('?').next().unwrap_or(s);
        format!("http://127.0.0.1/?{}", query.trim_start_matches(['?', '&']))
    };
    let url = url::Url::parse(&url_str).ok()?;
    let mut code = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    Some((code?, state))
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
    cfg.auth.refresh_token = None; // clear any stale session from a different account
    Ok(())
}

pub(crate) fn open_url(url: &str) -> Result<()> {
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
    use super::parse_manual_code;

    #[test]
    fn parses_a_full_redirect_url() {
        let (code, state) =
            parse_manual_code("http://127.0.0.1:54321/callback?code=abc123&state=xyz").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parses_a_bare_query_fragment() {
        let (code, state) = parse_manual_code("  code=abc123&state=xyz  ").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn treats_a_bare_token_as_the_code() {
        let (code, state) = parse_manual_code("abc123").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, None);
    }

    #[test]
    fn rejects_empty_or_codeless_input() {
        assert!(parse_manual_code("   ").is_none());
        assert!(parse_manual_code("http://127.0.0.1/callback?state=xyz").is_none());
    }
}
