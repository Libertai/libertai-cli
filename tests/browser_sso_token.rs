//! Guards the browser-SSO token acquisition that both `libertai login` and
//! `libertai keys` ride on: the loopback + PKCE flow in
//! `commands::login::browser_sso_access_token`. A fake local account API
//! stands in for `/auth/exchange`, and the test plays the browser by hitting
//! the loopback callback itself.
//!
//! Offline tier-1: no model API call, no real network.

use std::sync::mpsc;
use std::thread;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};

use libertai_cli::commands::login::browser_sso_access_token;
use libertai_cli::config::Config;

/// Pull `redirect_uri`, `state`, and `challenge` out of the console authorize
/// URL handed to the `open` callback.
fn parse_authorize_url(authorize_url: &str) -> (String, String, String) {
    let url = url::Url::parse(authorize_url).expect("authorize URL parses");
    let mut redirect_uri = None;
    let mut state = None;
    let mut challenge = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "redirect_uri" => redirect_uri = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "challenge" => challenge = Some(v.into_owned()),
            _ => {}
        }
    }
    (
        redirect_uri.expect("authorize URL has redirect_uri"),
        state.expect("authorize URL has state"),
        challenge.expect("authorize URL has challenge"),
    )
}

/// The loopback server only answers once the flow starts waiting, so the
/// browser "redirect" has to arrive from another thread.
fn hit_callback(callback_url: String) {
    thread::spawn(move || {
        let _ = reqwest::blocking::get(&callback_url);
    });
}

#[test]
fn happy_path_returns_session_token_and_pkce_verifier_matches_challenge() {
    // Fake account API serving a single POST /auth/exchange.
    let server = tiny_http::Server::http("127.0.0.1:0").expect("fake account server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("fake account server addr")
        .port();
    let account_base = format!("http://127.0.0.1:{port}");

    let (challenge_tx, challenge_rx) = mpsc::channel::<String>();
    let exchange = thread::spawn(move || {
        let mut req = server.recv().expect("exchange request");
        assert_eq!(req.url(), "/auth/exchange");
        let mut body = String::new();
        req.as_reader()
            .read_to_string(&mut body)
            .expect("exchange body");
        let v: serde_json::Value = serde_json::from_str(&body).expect("exchange body is JSON");
        assert_eq!(
            v["code"], "probe-code",
            "exchange carries the callback code"
        );
        // PKCE: the verifier sent to /auth/exchange must hash to the challenge
        // that went out in the authorize URL.
        let challenge = challenge_rx.recv().expect("challenge from authorize URL");
        let verifier = v["verifier"].as_str().expect("exchange carries verifier");
        let hashed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(hashed, challenge, "verifier does not match PKCE challenge");

        let header = "Content-Type: application/json"
            .parse::<tiny_http::Header>()
            .unwrap();
        req.respond(
            tiny_http::Response::from_string(r#"{"access_token":"session-token-123"}"#)
                .with_header(header),
        )
        .expect("exchange response");
    });

    let cfg = Config {
        account_base,
        ..Config::default()
    };
    let token = browser_sso_access_token(&cfg, "probe", |authorize_url| {
        let (redirect_uri, state, challenge) = parse_authorize_url(authorize_url);
        challenge_tx.send(challenge).expect("send challenge");
        hit_callback(format!("{redirect_uri}?code=probe-code&state={state}"));
    })
    .expect("browser SSO flow succeeds");

    assert_eq!(token, "session-token-123");
    exchange.join().expect("exchange thread");
}

#[test]
fn callback_error_param_rejects_login() {
    // No exchange happens on the error path, so the (default, unreachable)
    // account base is never contacted.
    let cfg = Config::default();
    let err = browser_sso_access_token(&cfg, "probe", |authorize_url| {
        let (redirect_uri, _state, _challenge) = parse_authorize_url(authorize_url);
        hit_callback(format!("{redirect_uri}?error=access_denied"));
    })
    .expect_err("error callback must fail the flow");
    assert!(
        err.to_string().contains("login was rejected"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn state_mismatch_aborts_before_exchange() {
    let cfg = Config::default();
    let err = browser_sso_access_token(&cfg, "probe", |authorize_url| {
        let (redirect_uri, _state, _challenge) = parse_authorize_url(authorize_url);
        hit_callback(format!("{redirect_uri}?code=probe-code&state=forged"));
    })
    .expect_err("forged state must fail the flow");
    assert!(
        err.to_string().contains("state mismatch"),
        "unexpected error: {err:#}"
    );
}
