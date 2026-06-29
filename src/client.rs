//! HTTP client wrappers for LibertAI's inference (`/v1/*`) and account
//! (`/auth/*`, `/api-keys/*`) APIs. Uses `reqwest::blocking` deliberately
//! — blocking simplifies the REPL + launcher code and removes the tokio
//! runtime from the dependency tree.

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;

// ── error classification ────────────────────────────────────────────────────

/// Failure category threaded through the anyhow chain so `main` can map
/// errors onto differentiated exit codes (auth → 3, network → 4, API → 5;
/// see `exit_code` in `main.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Login required or credentials rejected (missing key, HTTP 401).
    Auth,
    /// The backend could not be reached (connect/DNS/TLS/timeout — the
    /// request never produced an HTTP response).
    Network,
    /// The backend answered with a non-success HTTP status other than 401.
    Api,
}

/// An error carrying an [`ErrorClass`]. It lives somewhere in the anyhow
/// source chain (possibly wrapped by `.context(..)` layers); classify a
/// final error with [`error_class`].
#[derive(Debug)]
pub struct ClassifiedError {
    pub class: ErrorClass,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ClassifiedError {
    pub fn classified(class: ErrorClass, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            class,
            message: message.into(),
            source: None,
        })
    }

    fn with_source(
        class: ErrorClass,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> anyhow::Error {
        anyhow::Error::new(Self {
            class,
            message: message.into(),
            source: Some(Box::new(source)),
        })
    }
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// First [`ErrorClass`] found in the error chain (outermost first), or
/// `None` for unclassified errors (generic exit code 1).
pub fn error_class(err: &anyhow::Error) -> Option<ErrorClass> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedError>().map(|c| c.class))
}

/// The CLI's exit-code contract, shared by the `libertai` and `lcode`
/// binaries (documented in the README "Scripting" section):
///
///   0  success
///   1  generic failure
///   2  usage error (emitted by clap before dispatch runs)
///   3  auth required or rejected — run `libertai login`
///   4  network/connect failure (backend unreachable, DNS, timeout)
///   5  server-side API error (the backend answered a non-401 4xx/5xx)
pub fn exit_code(err: &anyhow::Error) -> i32 {
    match error_class(err) {
        Some(ErrorClass::Auth) => 3,
        Some(ErrorClass::Network) => 4,
        Some(ErrorClass::Api) => 5,
        None => 1,
    }
}

pub fn http(cfg: &Config) -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs))
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")
}

/// Rewrite a reqwest `send()` error into an anyhow error, adding a
/// remediation hint when the failure is a timeout so the user knows which
/// knob to turn (either the config key or a cheaper model).
fn annotate_send_err(
    e: reqwest::Error,
    ctx: impl std::fmt::Display,
    timeout_secs: Option<u64>,
) -> anyhow::Error {
    if e.is_timeout() {
        let after = timeout_secs
            .map(|s| format!(" after {s}s"))
            .unwrap_or_default();
        let bump = timeout_secs
            .map(|s| s.saturating_mul(2).max(300))
            .unwrap_or(300);
        return ClassifiedError::classified(
            ErrorClass::Network,
            format!(
                "{ctx}: request timed out{after} — the model may still be generating. \
                 Raise the timeout with `libertai config set http_timeout_secs {bump}`, \
                 or try a faster/smaller model."
            ),
        );
    }
    // Any other `send()` failure means no HTTP response was received
    // (connection refused, DNS, TLS, broken pipe) — a network failure.
    ClassifiedError::with_source(ErrorClass::Network, format!("{ctx}"), e)
}

/// Client for long-lived streaming responses (e.g. SSE chat completions).
/// `.timeout` would apply as a total-request deadline including body
/// receipt, truncating any stream longer than the limit. The reqwest
/// blocking builder does not expose `.read_timeout` (that lives on the
/// async builder only in 0.12), so we bound connect time and leave the
/// overall request timeout unset — the server keeps the connection alive
/// for as long as it is streaming tokens.
pub fn http_stream() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building streaming http client")
}

pub fn require_api_key(cfg: &Config) -> Result<&str> {
    cfg.auth.api_key.as_deref().ok_or_else(|| {
        ClassifiedError::classified(
            ErrorClass::Auth,
            "not logged in — run `libertai login` first",
        )
    })
}

// ── Inference (OpenAI-compatible) ───────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
}

/// Mirrors the `/v1/models` wire shape (`{"data": [...]}`), so
/// `libertai models --json` can emit the listing as returned.
#[derive(Debug, Deserialize, Serialize)]
pub struct ModelList {
    pub data: Vec<ModelEntry>,
}

pub fn list_models(cfg: &Config) -> Result<ModelList> {
    let key = require_api_key(cfg)?;
    let url = format!("{}/v1/models", cfg.api_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .get(&url)
        .bearer_auth(key)
        .send()
        .map_err(|e| annotate_send_err(e, format!("GET {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<ModelList>()
        .context("parsing /v1/models response")
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

pub fn post_chat_blocking(cfg: &Config, req: &ChatRequest) -> Result<reqwest::blocking::Response> {
    let key = require_api_key(cfg)?;
    let url = format!("{}/v1/chat/completions", cfg.api_base.trim_end_matches('/'));
    // Streaming must not carry a total-request timeout — the connection lives
    // as long as the server is emitting tokens. Non-streaming (`ask`) bounds
    // the whole generation by `http_timeout_secs` so a slow model does not
    // hang the terminal forever.
    let streaming = req.stream.unwrap_or(false);
    let (client, timeout_hint) = if streaming {
        (http_stream()?, None)
    } else {
        (http(cfg)?, Some(cfg.http_timeout_secs))
    };
    let resp = client
        .post(&url)
        .bearer_auth(key)
        .json(req)
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), timeout_hint))?;
    let resp = check_status(resp, &url)?;
    Ok(resp)
}

// ── Images ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    pub size: String,
    pub n: u32,
}

#[derive(Debug, Deserialize)]
pub struct ImageDatum {
    pub b64_json: String,
}

#[derive(Debug, Deserialize)]
pub struct ImageResponse {
    pub data: Vec<ImageDatum>,
}

pub fn post_image(cfg: &Config, req: &ImageRequest) -> Result<ImageResponse> {
    let key = require_api_key(cfg)?;
    let url = format!(
        "{}/v1/images/generations",
        cfg.api_base.trim_end_matches('/')
    );
    let resp = http(cfg)?
        .post(&url)
        .bearer_auth(key)
        .json(req)
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<ImageResponse>()
        .context("parsing /v1/images/generations response")
}

// ── Search (search.libertai.io) ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SearchRequest<'a> {
    pub query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engines: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_type: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchResult {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub rank: Option<u32>,
    #[serde(default)]
    pub found_in: Vec<String>,
    #[serde(default)]
    pub search_type: Option<String>,
    // News
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    // Images
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    /// Opaque server-side metadata — never read by the CLI itself, but
    /// round-tripped into `libertai search --json` output.
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

pub fn post_search(cfg: &Config, req: &SearchRequest<'_>) -> Result<SearchResponse> {
    let key = require_api_key(cfg)?;
    let url = format!("{}/search", cfg.search_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .bearer_auth(key)
        .json(req)
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<SearchResponse>()
        .context("parsing /search response")
}

// ── Account (/auth, /api-keys) ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AuthMessageRequest<'a> {
    pub chain: &'a str,
    pub address: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct AuthMessageResponse {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct AuthLoginRequest<'a> {
    pub chain: &'a str,
    pub address: &'a str,
    pub signature: &'a str,
}

// The upstream response also carries an `address` field; serde ignores
// unknown fields, and the CLI never reads it, so it is not modelled here.
#[derive(Debug, Deserialize)]
pub struct AuthLoginResponse {
    pub access_token: String,
}

pub fn auth_message(cfg: &Config, chain: &str, address: &str) -> Result<String> {
    let url = format!("{}/auth/message", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .json(&AuthMessageRequest { chain, address })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    Ok(resp.json::<AuthMessageResponse>()?.message)
}

pub fn auth_login(cfg: &Config, chain: &str, address: &str, signature: &str) -> Result<String> {
    let url = format!("{}/auth/login", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .json(&AuthLoginRequest {
            chain,
            address,
            signature,
        })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    Ok(resp.json::<AuthLoginResponse>()?.access_token)
}

// ── CLI browser-SSO (loopback + PKCE) ────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ExchangeRequest<'a> {
    pub code: &'a str,
    pub verifier: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Serialize)]
pub struct CliApiKeyCreate<'a> {
    pub host: &'a str,
}

/// Exchange a one-time code (+ PKCE verifier) for the session token pair.
/// The refresh token is the persistent (30-day, revocable) credential; the
/// access token is short-lived. Callers persist the refresh token.
pub fn exchange_code(cfg: &Config, code: &str, verifier: &str) -> Result<TokenPair> {
    let url = format!("{}/auth/exchange", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .json(&ExchangeRequest { code, verifier })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<TokenPair>()
        .context("parsing /auth/exchange response")
}

/// Mint (or rotate) this device's CLI API key, authenticating with the session token.
pub fn create_cli_api_key(cfg: &Config, access_token: &str, host: &str) -> Result<FullApiKey> {
    let url = format!("{}/api-keys/cli", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .bearer_auth(access_token)
        .json(&CliApiKeyCreate { host })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<FullApiKey>()
        .context("parsing CLI key response")
}

/// One row from `GET /api-keys`. Also serialized verbatim into
/// `libertai keys list --json` output, so field names are stable.
#[derive(Debug, Deserialize, Serialize)]
pub struct ApiKeyRow {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub monthly_limit: Option<f64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub user_address: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiKeyListResponse {
    pub keys: Vec<ApiKeyRow>,
}

#[derive(Debug, Deserialize)]
pub struct FullApiKey {
    pub id: String,
    pub name: String,
    /// The one-time full key value (`LTAI_...`). Upstream serializes this as
    /// `full_key`; the sibling `key` field on the upstream model is only the
    /// masked preview and is intentionally ignored here.
    pub full_key: String,
    /// ISO-8601 expiry (CLI keys expire; null for keys that never do).
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiKeyCreate<'a> {
    pub name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_limit: Option<f64>,
}

pub fn list_api_keys(cfg: &Config, jwt: &str) -> Result<Vec<ApiKeyRow>> {
    let url = format!("{}/api-keys", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .get(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .send()
        .map_err(|e| annotate_send_err(e, format!("GET {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    Ok(resp
        .json::<ApiKeyListResponse>()
        .context("parsing /api-keys response")?
        .keys)
}

pub fn create_api_key(cfg: &Config, jwt: &str, body: &ApiKeyCreate<'_>) -> Result<FullApiKey> {
    let url = format!("{}/api-keys", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .json(body)
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<FullApiKey>()
        .context("parsing create-key response")
}

pub fn delete_api_key(cfg: &Config, jwt: &str, id: &str) -> Result<()> {
    let url = format!("{}/api-keys/{id}", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .delete(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .send()
        .map_err(|e| annotate_send_err(e, format!("DELETE {url}"), Some(cfg.http_timeout_secs)))?;
    let _ = check_status(resp, &url)?;
    Ok(())
}

// ── Session refresh / revoke + account usage ────────────────────────────────

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    refresh_token: &'a str,
}

/// Rotate the refresh token (one-time use) for a fresh access/refresh pair.
/// The returned refresh token MUST be persisted — the old one is now invalid.
pub fn refresh_session(cfg: &Config, refresh_token: &str) -> Result<TokenPair> {
    let url = format!("{}/auth/refresh", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .json(&RefreshRequest { refresh_token })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<TokenPair>()
        .context("parsing /auth/refresh response")
}

/// Best-effort server-side session revocation (used by logout).
pub fn revoke_session(cfg: &Config, refresh_token: &str) -> Result<()> {
    let url = format!("{}/auth/logout", cfg.account_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .json(&RefreshRequest { refresh_token })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let _ = check_status(resp, &url)?;
    Ok(())
}

/// Mirrors the backend `SubscriptionResponse`. Every window/credit field is
/// optional there, so all are `Option` here. The named fields are a stable
/// contract; `extra` captures any other fields the backend returns so
/// `libertai usage --json` re-emits them too (lossless as the API evolves).
#[derive(Debug, Deserialize, Serialize)]
pub struct Subscription {
    pub tier: String,
    #[serde(default)]
    pub has_subscription: bool,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub window_5h_used: Option<f64>,
    #[serde(default)]
    pub window_5h_limit: Option<f64>,
    #[serde(default)]
    pub window_5h_resets_at: Option<String>,
    #[serde(default)]
    pub weekly_used: Option<f64>,
    #[serde(default)]
    pub weekly_limit: Option<f64>,
    #[serde(default)]
    pub weekly_resets_at: Option<String>,
    #[serde(default)]
    pub prepaid_balance: Option<f64>,
    /// Fields the backend adds beyond the ones above, preserved verbatim so
    /// `--json` never silently drops new API data.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Fetch the caller's subscription + allowance snapshot. Needs a session JWT
/// (the `LTAI_` inference key cannot authenticate this endpoint).
pub fn get_subscription(cfg: &Config, access_token: &str) -> Result<Subscription> {
    let url = format!(
        "{}/payments/subscription",
        cfg.account_base.trim_end_matches('/')
    );
    let resp = http(cfg)?
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .map_err(|e| annotate_send_err(e, format!("GET {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<Subscription>()
        .context("parsing /payments/subscription response")
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Consumes `resp`. On success, returns it unchanged so the caller can
/// continue reading the body. On failure, reads up to 1 KiB of the body
/// (safe: bearer/cookie tokens live only in the *request* headers) and
/// returns an error of the form `"{url} → {status}: {body}"` with the
/// body truncated with `"…"` if longer than 1024 chars.
fn check_status(
    resp: reqwest::blocking::Response,
    url: &str,
) -> Result<reqwest::blocking::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().unwrap_or_default();
    let truncated = if body.chars().count() > 1024 {
        let mut s: String = body.chars().take(1024).collect();
        s.push('…');
        s
    } else {
        body
    };
    // A 401 on an authenticated call almost always means the stored key is invalid,
    // revoked, or (for CLI keys) expired — point the user at a fresh login.
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ClassifiedError::classified(
            ErrorClass::Auth,
            format!(
                "{url} → {status}: {truncated}\n\
                 Your API key may be invalid or expired — run `libertai login` to sign in again."
            ),
        ));
    }
    Err(ClassifiedError::classified(
        ErrorClass::Api,
        format!("{url} → {status}: {truncated}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_class_found_through_context_layers() {
        let err = ClassifiedError::classified(ErrorClass::Auth, "no key")
            .context("loading models")
            .context("outermost");
        assert_eq!(error_class(&err), Some(ErrorClass::Auth));
    }

    #[test]
    fn error_class_none_for_plain_anyhow() {
        let err = anyhow::anyhow!("something else").context("outer");
        assert_eq!(error_class(&err), None);
    }

    #[test]
    fn classified_error_preserves_source_in_display_chain() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = ClassifiedError::with_source(ErrorClass::Network, "GET http://x", io);
        let rendered = format!("{err:#}");
        assert!(rendered.contains("GET http://x"), "got: {rendered}");
        assert!(rendered.contains("refused"), "got: {rendered}");
        assert_eq!(error_class(&err), Some(ErrorClass::Network));
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn subscription_parses_with_missing_optional_fields() {
        // Backend marks every window/credit field optional; absent ones default.
        let json = r#"{"tier":"go","has_subscription":true}"#;
        let sub: Subscription = serde_json::from_str(json).unwrap();
        assert_eq!(sub.tier, "go");
        assert!(sub.has_subscription);
        assert_eq!(sub.window_5h_used, None);
        assert_eq!(sub.prepaid_balance, None);
    }

    #[test]
    fn token_pair_parses_exchange_shape() {
        let json = r#"{"access_token":"a","refresh_token":"r"}"#;
        let pair: TokenPair = serde_json::from_str(json).unwrap();
        assert_eq!(pair.access_token, "a");
        assert_eq!(pair.refresh_token, "r");
    }
}
