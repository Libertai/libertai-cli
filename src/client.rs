//! HTTP client wrappers for LibertAI's inference (`/v1/*`) and account
//! (`/auth/*`, `/api-keys/*`) APIs. Uses `reqwest::blocking` deliberately
//! — blocking simplifies the REPL + launcher code and removes the tokio
//! runtime from the dependency tree.

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;

pub fn http() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")
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
    let resp = http()?
        .get(&url)
        .bearer_auth(key)
        .send()
        .with_context(|| format!("GET {url}"))?;
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
    let resp = http_stream()?
        .post(&url)
        .bearer_auth(key)
        .json(req)
        .send()
        .with_context(|| format!("POST {url}"))?;
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
    let resp = http()?
        .post(&url)
        .bearer_auth(key)
        .json(req)
        .send()
        .with_context(|| format!("POST {url}"))?;
    let resp = check_status(resp, &url)?;
    resp.json::<ImageResponse>()
        .context("parsing /v1/images/generations response")
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
    let resp = http()?
        .post(&url)
        .json(&AuthMessageRequest { chain, address })
        .send()
        .with_context(|| format!("POST {url}"))?;
    let resp = check_status(resp, &url)?;
    Ok(resp.json::<AuthMessageResponse>()?.message)
}

pub fn auth_login(cfg: &Config, chain: &str, address: &str, signature: &str) -> Result<String> {
    let url = format!("{}/auth/login", cfg.account_base.trim_end_matches('/'));
    let resp = http()?
        .post(&url)
        .json(&AuthLoginRequest {
            chain,
            address,
            signature,
        })
        .send()
        .with_context(|| format!("POST {url}"))?;
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
    let resp = http()?
        .get(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .send()
        .with_context(|| format!("GET {url}"))?;
    let resp = check_status(resp, &url)?;
    Ok(resp
        .json::<ApiKeyListResponse>()
        .context("parsing /api-keys response")?
        .keys)
}

pub fn create_api_key(cfg: &Config, jwt: &str, body: &ApiKeyCreate<'_>) -> Result<FullApiKey> {
    let url = format!("{}/api-keys", cfg.account_base.trim_end_matches('/'));
    let resp = http()?
        .post(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .json(body)
        .send()
        .with_context(|| format!("POST {url}"))?;
    let resp = check_status(resp, &url)?;
    resp.json::<FullApiKey>().context("parsing create-key response")
}

pub fn delete_api_key(cfg: &Config, jwt: &str, id: &str) -> Result<()> {
    let url = format!(
        "{}/api-keys/{id}",
        cfg.account_base.trim_end_matches('/')
    );
    let resp = http()?
        .delete(&url)
        .header(reqwest::header::COOKIE, format!("libertai_auth={jwt}"))
        .send()
        .with_context(|| format!("DELETE {url}"))?;
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
