//! HTTP client wrappers for LibertAI's inference (`/v1/*`) and account
//! (`/auth/*`, `/api-keys/*`) APIs. Uses `reqwest::blocking` deliberately
//! — blocking simplifies the REPL + launcher code and removes the tokio
//! runtime from the dependency tree.

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;

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
        return anyhow!(
            "{ctx}: request timed out{after} — the model may still be generating. \
             Raise the timeout with `libertai config set http_timeout_secs {bump}`, \
             or try a faster/smaller model."
        );
    }
    anyhow::Error::new(e).context(format!("{ctx}"))
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
    cfg.auth
        .api_key
        .as_deref()
        .ok_or_else(|| anyhow!("not logged in — run `libertai login` first"))
}

// ── Inference (OpenAI-compatible) ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[derive(Debug, Deserialize)]
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
}

pub fn post_chat_blocking(cfg: &Config, req: &ChatRequest) -> Result<reqwest::blocking::Response> {
    let key = require_api_key(cfg)?;
    let url = format!(
        "{}/v1/chat/completions",
        cfg.api_base.trim_end_matches('/')
    );
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
    #[serde(default)]
    #[allow(dead_code)]
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

#[derive(Debug, Serialize)]
pub struct FetchRequest<'a> {
    pub url: &'a str,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FetchResponse {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub word_count: Option<u32>,
}

pub fn post_fetch(cfg: &Config, target: &str) -> Result<FetchResponse> {
    let key = require_api_key(cfg)?;
    let url = format!("{}/fetch", cfg.search_base.trim_end_matches('/'));
    let resp = http(cfg)?
        .post(&url)
        .bearer_auth(key)
        .json(&FetchRequest { url: target })
        .send()
        .map_err(|e| annotate_send_err(e, format!("POST {url}"), Some(cfg.http_timeout_secs)))?;
    let resp = check_status(resp, &url)?;
    resp.json::<FetchResponse>()
        .context("parsing /fetch response")
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

#[derive(Debug, Deserialize)]
pub struct AuthLoginResponse {
    pub access_token: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub address: Option<String>,
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

#[derive(Debug, Deserialize)]
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
    #[allow(dead_code)]
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
    resp.json::<FullApiKey>().context("parsing create-key response")
}

pub fn delete_api_key(cfg: &Config, jwt: &str, id: &str) -> Result<()> {
    let url = format!(
        "{}/api-keys/{id}",
        cfg.account_base.trim_end_matches('/')
    );
    let resp = http(cfg)?
        .delete(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .send()
        .map_err(|e| annotate_send_err(e, format!("DELETE {url}"), Some(cfg.http_timeout_secs)))?;
    let _ = check_status(resp, &url)?;
    Ok(())
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
    Err(anyhow!("{url} → {status}: {truncated}"))
}
