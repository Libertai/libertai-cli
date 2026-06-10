//! Real model metadata — context windows and per-MTok pricing — for
//! LibertAI models.
//!
//! `/v1/models` only returns `{id, owned_by}`; the authoritative metadata
//! lives in a public, unauthenticated Aleph aggregate (key `LTAI_PRICING`)
//! published by the LibertAI pricing wallet — the same source the website
//! reads. Each text model carries
//! `pricing.text.{price_per_million_input_tokens, price_per_million_output_tokens}`
//! and `capabilities.text.{context_window, vision, reasoning, tee,
//! function_calling}`; the aggregate also lists image/search/embedding
//! entries (no `text` block) and a `redirections` table of model aliases
//! (`ltai-fast` → `qwen3.6-35b-a3b`, deprecated ids → successors).
//!
//! Network policy mirrors `update_check.rs`: blocking reqwest with short
//! timeouts, a 24h on-disk cache next to `config.toml`
//! (`model-catalog.json`), and graceful offline degradation — a stale
//! cache is better than nothing, and nothing simply yields `None` so
//! callers fall back to placeholders/dashes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config;

/// Env override for the aggregate URL (no config.toml key — the catalog
/// source is infrastructure, not a user preference). Set to `off` or the
/// empty string to disable catalog lookups entirely (air-gapped installs,
/// hermetic tests).
pub const CATALOG_URL_ENV: &str = "LIBERTAI_MODEL_CATALOG_URL";

/// `contextWindow` placeholder that libertai-cli wrote into pi's
/// `models.json` before the catalog existed. Recognised so enrichment can
/// replace it with the real value while leaving genuinely user-set
/// context windows untouched.
pub const LEGACY_PLACEHOLDER_CONTEXT_WINDOW: u64 = 32_768;

const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const CACHE_FILE: &str = "model-catalog.json";

// ── Aggregate wire types (unknown fields ignored by serde default) ─────────

#[derive(Debug, Deserialize)]
struct AggregateResponse {
    data: AggregateData,
}

#[derive(Debug, Deserialize)]
struct AggregateData {
    #[serde(rename = "LTAI_PRICING")]
    ltai_pricing: Catalog,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub models: Vec<CatalogModel>,
    #[serde(default)]
    pub redirections: Vec<Redirection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub hf_id: Option<String>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pricing {
    /// Only the `text` block matters to the CLI; image/search/embedding
    /// pricing shapes are ignored.
    #[serde(default)]
    pub text: Option<TextPricing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextPricing {
    pub price_per_million_input_tokens: f64,
    pub price_per_million_output_tokens: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub text: Option<TextCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextCapabilities {
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub tee: bool,
    #[serde(default)]
    pub function_calling: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Redirection {
    pub from: String,
    pub to: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

impl CatalogModel {
    pub fn text_pricing(&self) -> Option<&TextPricing> {
        self.pricing.as_ref()?.text.as_ref()
    }

    pub fn text_capabilities(&self) -> Option<&TextCapabilities> {
        self.capabilities.as_ref()?.text.as_ref()
    }
}

impl Catalog {
    /// Follow alias/deprecation redirections (`ltai-fast` →
    /// `qwen3.6-35b-a3b`). Bounded hops guard against a cyclic table.
    pub fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        let mut current = id;
        for _ in 0..4 {
            match self.redirections.iter().find(|r| r.from == current) {
                Some(r) => current = &r.to,
                None => break,
            }
        }
        current
    }

    /// The catalog entry for a *text* model id (after redirection
    /// resolution). Image/search/embedding entries yield `None`.
    ///
    /// `/v1/models` also serves `<base>-thinking` variants the aggregate
    /// doesn't list separately; they share the base model's context window
    /// and pricing, so an unmatched id falls back to its `-thinking`-less
    /// base before giving up.
    pub fn find_text(&self, id: &str) -> Option<&CatalogModel> {
        self.find_text_exact(id).or_else(|| {
            id.strip_suffix("-thinking")
                .and_then(|base| self.find_text_exact(base))
        })
    }

    fn find_text_exact(&self, id: &str) -> Option<&CatalogModel> {
        let resolved = self.resolve(id);
        self.models
            .iter()
            .find(|m| m.id == resolved)
            .filter(|m| m.text_capabilities().is_some() || m.text_pricing().is_some())
    }

    pub fn context_window(&self, id: &str) -> Option<u32> {
        self.find_text(id)?.text_capabilities()?.context_window
    }

    /// USD per million (input, output) tokens.
    pub fn token_rates(&self, id: &str) -> Option<(f64, f64)> {
        let p = self.find_text(id)?.text_pricing()?;
        Some((
            p.price_per_million_input_tokens,
            p.price_per_million_output_tokens,
        ))
    }
}

// ── Loading: 24h disk cache + graceful offline fallback ────────────────────

/// USD per million (input, output) tokens for `model`, from the cached
/// public catalog. Exposed for the REPL cost estimator: `code_ui.rs`
/// carries a hardcoded fallback pricing table (`model_token_rate_match`)
/// that should consult this first — that wiring is intentionally not done
/// here (`code_ui.rs` is owned elsewhere).
pub fn token_rates_for(model: &str) -> Option<(f64, f64)> {
    load()?.token_rates(model)
}

/// Load the catalog: fresh cache → use it; stale/missing cache → fetch
/// (and rewrite the cache); fetch failure → stale cache if any, else
/// `None`. Never errors — metadata is an enhancement, not a requirement.
pub fn load() -> Option<Catalog> {
    let url = catalog_url()?;
    let path = cache_path().ok()?;
    load_inner(&path, &url, now_unix())
}

/// Effective aggregate URL: `LIBERTAI_MODEL_CATALOG_URL` if set (with
/// `off`/empty disabling the catalog), else the built-in default.
pub fn catalog_url() -> Option<String> {
    match std::env::var(CATALOG_URL_ENV) {
        Ok(v) => {
            let v = v.trim().to_string();
            if v.is_empty() || v.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(v)
            }
        }
        Err(_) => Some(config::DEFAULT_MODEL_CATALOG_URL.to_string()),
    }
}

fn load_inner(cache_path: &Path, url: &str, now: u64) -> Option<Catalog> {
    let cached = read_cache(cache_path);
    if let Some(c) = &cached {
        if now.saturating_sub(c.fetched_at_unix) < CACHE_TTL_SECS {
            return Some(c.catalog.clone());
        }
    }
    match fetch(url) {
        Ok(catalog) => {
            let _ = write_cache(
                cache_path,
                &CatalogCache {
                    fetched_at_unix: now,
                    catalog: catalog.clone(),
                },
            );
            Some(catalog)
        }
        // Offline / aggregate down: a stale catalog beats no catalog.
        Err(_) => cached.map(|c| c.catalog),
    }
}

fn fetch(url: &str) -> Result<Catalog> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building catalog http client")?;
    let raw = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .text()
        .context("reading catalog response body")?;
    parse_aggregate(&raw)
}

/// Parse the raw aggregate response (`{"address", "data": {"LTAI_PRICING":
/// {...}}, "info"}`) into a [`Catalog`].
pub fn parse_aggregate(raw: &str) -> Result<Catalog> {
    let resp: AggregateResponse =
        serde_json::from_str(raw).context("parsing LTAI_PRICING aggregate")?;
    Ok(resp.data.ltai_pricing)
}

#[derive(Debug, Serialize, Deserialize)]
struct CatalogCache {
    fetched_at_unix: u64,
    catalog: Catalog,
}

fn cache_path() -> Result<PathBuf> {
    let cfg = config::config_path()?;
    let parent = cfg
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent"))?;
    Ok(parent.join(CACHE_FILE))
}

fn read_cache(path: &Path) -> Option<CatalogCache> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(path: &Path, cache: &CatalogCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        config::create_dir_secure(parent)?;
    }
    let raw = serde_json::to_string_pretty(cache)?;
    config::write_file_secure(path, raw.as_bytes())?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── pi models.json integration ──────────────────────────────────────────────

/// Build a fresh `providers.libertai.models[]` entry for pi's
/// `models.json`, enriched with real catalog data when available. Field
/// names match what pi's `ModelConfig` actually deserializes (camelCase:
/// `contextWindow`, `maxTokens`, `cost.{input,output,cacheRead,cacheWrite}`).
pub fn new_pi_model_entry(id: &str, catalog: Option<&Catalog>) -> Value {
    let mut entry = Map::new();
    entry.insert("id".to_string(), Value::String(id.to_string()));
    entry.insert("name".to_string(), Value::String(id.to_string()));
    entry.insert(
        "api".to_string(),
        Value::String("openai-completions".to_string()),
    );
    entry.insert(
        "contextWindow".to_string(),
        json!(LEGACY_PLACEHOLDER_CONTEXT_WINDOW),
    );
    if let Some(cat) = catalog {
        enrich_pi_model_entry(&mut entry, cat);
    }
    Value::Object(entry)
}

/// Merge catalog metadata into one pi `providers.libertai.models[]` entry
/// in place. Never clobbers a user's richer hand-set values:
///
/// - `contextWindow` is written only when missing, non-numeric, or still
///   the legacy 32768 placeholder this CLI used to seed;
/// - `cost` / `reasoning` / `input` are written only when absent;
/// - `name` only when absent or equal to the id (our placeholder shape).
///
/// Returns `true` when the entry changed.
pub fn enrich_pi_model_entry(entry: &mut Map<String, Value>, catalog: &Catalog) -> bool {
    let Some(id) = entry.get("id").and_then(|v| v.as_str()).map(str::to_string) else {
        return false;
    };
    let Some(model) = catalog.find_text(&id) else {
        return false;
    };

    let mut changed = false;
    if let Some(caps) = model.text_capabilities() {
        if let Some(ctx) = caps.context_window {
            let current = entry.get("contextWindow").and_then(Value::as_u64);
            let replaceable =
                current.is_none() || current == Some(LEGACY_PLACEHOLDER_CONTEXT_WINDOW);
            if replaceable && current != Some(u64::from(ctx)) {
                entry.insert("contextWindow".to_string(), json!(ctx));
                changed = true;
            }
        }
        if !entry.contains_key("reasoning") {
            entry.insert("reasoning".to_string(), Value::Bool(caps.reasoning));
            changed = true;
        }
        if caps.vision && !entry.contains_key("input") {
            entry.insert("input".to_string(), json!(["text", "image"]));
            changed = true;
        }
    }
    if let Some(pricing) = model.text_pricing() {
        if !entry.contains_key("cost") {
            // pi's ModelCost requires all four fields; LibertAI has no
            // separate cache pricing, so cache reads/writes are billed 0.
            entry.insert(
                "cost".to_string(),
                json!({
                    "input": pricing.price_per_million_input_tokens,
                    "output": pricing.price_per_million_output_tokens,
                    "cacheRead": 0.0,
                    "cacheWrite": 0.0,
                }),
            );
            changed = true;
        }
    }
    // Only an exact catalog match may supply the display name; aliases and
    // `-thinking` variants matched via fallback keep their id-shaped name
    // so pi's model picker doesn't show two entries with the same label.
    if model.id == id {
        if let Some(name) = model.name.as_deref() {
            let current = entry.get("name").and_then(|v| v.as_str());
            let placeholder = current.is_none() || current == Some(id.as_str());
            if placeholder && current != Some(name) {
                entry.insert("name".to_string(), Value::String(name.to_string()));
                changed = true;
            }
        }
    }
    changed
}

// ── Presentation helpers (shared by `libertai models`) ─────────────────────

/// Stable per-model metadata object for `libertai models --json`
/// (documented in the README "Scripting" section).
pub fn catalog_json_for(catalog: &Catalog, id: &str) -> Option<Value> {
    let model = catalog.find_text(id)?;
    let mut obj = Map::new();
    if model.id != id {
        // Alias / deprecated / `-thinking` id: record which catalog entry
        // supplied the metadata instead of claiming its display name.
        obj.insert("resolvedId".to_string(), Value::String(model.id.clone()));
    } else {
        if let Some(name) = &model.name {
            obj.insert("name".to_string(), Value::String(name.clone()));
        }
        if let Some(hf_id) = &model.hf_id {
            obj.insert("hfId".to_string(), Value::String(hf_id.clone()));
        }
    }
    if let Some(caps) = model.text_capabilities() {
        if let Some(ctx) = caps.context_window {
            obj.insert("contextWindow".to_string(), json!(ctx));
        }
        obj.insert("vision".to_string(), Value::Bool(caps.vision));
        obj.insert("reasoning".to_string(), Value::Bool(caps.reasoning));
        obj.insert("tee".to_string(), Value::Bool(caps.tee));
        obj.insert(
            "functionCalling".to_string(),
            Value::Bool(caps.function_calling),
        );
    }
    if let Some(pricing) = model.text_pricing() {
        obj.insert(
            "inputUsdPerMtok".to_string(),
            json!(pricing.price_per_million_input_tokens),
        );
        obj.insert(
            "outputUsdPerMtok".to_string(),
            json!(pricing.price_per_million_output_tokens),
        );
    }
    Some(Value::Object(obj))
}

/// `262144` → `"262k"`, `16000` → `"16k"`; sub-1000 windows print raw.
pub fn format_context_window(ctx: u32) -> String {
    if ctx >= 1000 {
        format!("{}k", (ctx + 500) / 1000)
    } else {
        ctx.to_string()
    }
}

/// `(0.15, 0.5)` → `"$0.15 / $0.50"` (USD per million tokens, in / out).
pub fn format_price_per_mtok(input: f64, output: f64) -> String {
    format!("${input:.2} / ${output:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/ltai_pricing_aggregate.json");

    fn fixture_catalog() -> Catalog {
        parse_aggregate(FIXTURE).expect("fixture parses")
    }

    #[test]
    fn parses_fixture_models_and_redirections() {
        let cat = fixture_catalog();
        assert_eq!(cat.models.len(), 11);
        assert!(!cat.redirections.is_empty());

        let m = cat.find_text("qwen3.6-35b-a3b").expect("text model found");
        let caps = m.text_capabilities().expect("text capabilities");
        assert_eq!(caps.context_window, Some(262_144));
        assert!(caps.vision);
        assert!(caps.reasoning);
        assert!(caps.function_calling);
        assert!(!caps.tee);
        let pricing = m.text_pricing().expect("text pricing");
        assert_eq!(pricing.price_per_million_input_tokens, 0.15);
        assert_eq!(pricing.price_per_million_output_tokens, 0.5);
    }

    #[test]
    fn context_window_and_rates_lookups() {
        let cat = fixture_catalog();
        assert_eq!(cat.context_window("hermes-3-8b-tee"), Some(16_000));
        assert_eq!(cat.token_rates("qwen3.5-122b-a10b"), Some((0.25, 1.75)));
        assert_eq!(cat.context_window("no-such-model"), None);
        assert_eq!(cat.token_rates("no-such-model"), None);
    }

    #[test]
    fn redirections_resolve_aliases_and_deprecations() {
        let cat = fixture_catalog();
        // Alias.
        assert_eq!(cat.resolve("ltai-fast"), "qwen3.6-35b-a3b");
        assert_eq!(cat.context_window("ltai-fast"), Some(262_144));
        // Deprecated id chains to its successor.
        assert_eq!(cat.token_rates("glm-4.7"), Some((0.25, 1.75)));
        // Untouched id passes through.
        assert_eq!(cat.resolve("qwen3.6-27b"), "qwen3.6-27b");
    }

    #[test]
    fn thinking_variants_inherit_base_model_metadata() {
        let cat = fixture_catalog();
        // `/v1/models` serves `-thinking` variants the aggregate doesn't
        // list; they share the base model's window and pricing.
        assert_eq!(
            cat.context_window("qwen3.6-35b-a3b-thinking"),
            Some(262_144)
        );
        assert_eq!(
            cat.token_rates("deepseek-v4-flash-thinking"),
            Some((0.25, 1.75))
        );
        assert!(cat.find_text("totally-unknown-thinking").is_none());
    }

    #[test]
    fn non_text_models_are_invisible_to_find_text() {
        let cat = fixture_catalog();
        assert!(cat.find_text("z-image-turbo").is_none(), "image model");
        assert!(cat.find_text("search/duckduckgo").is_none(), "search");
        assert!(cat.find_text("bge-m3").is_none(), "embedding");
    }

    #[test]
    fn cache_roundtrip_and_ttl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("model-catalog.json");
        let cache = CatalogCache {
            fetched_at_unix: 1_000_000,
            catalog: fixture_catalog(),
        };
        write_cache(&path, &cache).expect("cache writes");
        let read = read_cache(&path).expect("cache reads back");
        assert_eq!(read.fetched_at_unix, 1_000_000);
        assert_eq!(read.catalog.models.len(), 11);

        // Fresh cache: served without any fetch (the URL is unresolvable
        // garbage, so a fetch attempt would fail and prove itself absent
        // by still returning the cached catalog — but the fast path must
        // not even try).
        let fresh = load_inner(&path, "http://127.0.0.1:9/nope", 1_000_000 + 60)
            .expect("fresh cache served");
        assert_eq!(fresh.models.len(), 11);
    }

    #[test]
    fn stale_cache_survives_unreachable_aggregate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("model-catalog.json");
        write_cache(
            &path,
            &CatalogCache {
                fetched_at_unix: 0, // ancient
                catalog: fixture_catalog(),
            },
        )
        .expect("cache writes");
        let cat = load_inner(&path, "http://127.0.0.1:9/nope", now_unix())
            .expect("stale cache used as fallback");
        assert_eq!(cat.context_window("qwen3.6-35b-a3b"), Some(262_144));
    }

    #[test]
    fn no_cache_and_unreachable_aggregate_yields_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("model-catalog.json");
        assert!(load_inner(&path, "http://127.0.0.1:9/nope", now_unix()).is_none());
        assert!(!path.exists(), "failed fetch must not write a cache file");
    }

    #[test]
    fn new_entry_gets_real_context_cost_and_name() {
        let cat = fixture_catalog();
        let entry = new_pi_model_entry("qwen3.6-35b-a3b", Some(&cat));
        assert_eq!(
            entry.get("contextWindow").and_then(Value::as_u64),
            Some(262_144)
        );
        assert_eq!(
            entry.pointer("/cost/input").and_then(Value::as_f64),
            Some(0.15)
        );
        assert_eq!(
            entry.pointer("/cost/output").and_then(Value::as_f64),
            Some(0.5)
        );
        assert_eq!(entry.get("reasoning").and_then(Value::as_bool), Some(true));
        assert_eq!(
            entry.get("name").and_then(Value::as_str),
            Some("Qwen3.6-35B-A3B")
        );
    }

    #[test]
    fn new_entry_without_catalog_keeps_placeholder() {
        let entry = new_pi_model_entry("qwen3.6-35b-a3b", None);
        assert_eq!(
            entry.get("contextWindow").and_then(Value::as_u64),
            Some(LEGACY_PLACEHOLDER_CONTEXT_WINDOW)
        );
        assert!(entry.get("cost").is_none());
    }

    #[test]
    fn enrich_replaces_legacy_placeholder_but_not_user_values() {
        let cat = fixture_catalog();

        // Legacy placeholder → upgraded to the real window.
        let mut placeholder = obj(json!({
            "id": "qwen3.6-35b-a3b",
            "name": "qwen3.6-35b-a3b",
            "api": "openai-completions",
            "contextWindow": 32768,
        }));
        assert!(enrich_pi_model_entry(&mut placeholder, &cat));
        assert_eq!(
            placeholder.get("contextWindow").and_then(Value::as_u64),
            Some(262_144)
        );
        assert!(placeholder.contains_key("cost"));

        // User-set window and cost → untouched.
        let mut user = obj(json!({
            "id": "qwen3.6-35b-a3b",
            "name": "My Tuned Qwen",
            "contextWindow": 200000,
            "maxTokens": 9999,
            "cost": {"input": 9.0, "output": 9.0, "cacheRead": 9.0, "cacheWrite": 9.0},
            "reasoning": false,
        }));
        enrich_pi_model_entry(&mut user, &cat);
        assert_eq!(
            user.get("contextWindow").and_then(Value::as_u64),
            Some(200_000)
        );
        assert_eq!(user.pointer_cost_input(), Some(9.0));
        assert_eq!(
            user.get("name").and_then(Value::as_str),
            Some("My Tuned Qwen")
        );
        assert_eq!(user.get("reasoning").and_then(Value::as_bool), Some(false));
        assert_eq!(user.get("maxTokens").and_then(Value::as_u64), Some(9999));

        // Unknown model: no-op.
        let mut unknown = obj(json!({"id": "not-in-catalog", "contextWindow": 32768}));
        assert!(!enrich_pi_model_entry(&mut unknown, &cat));
    }

    #[test]
    fn enriched_entry_deserializes_as_pi_model_config() {
        // Guard against drift from what pi actually consumes: the entry we
        // write must round-trip through the pinned pi rev's `ModelConfig`
        // (camelCase `contextWindow` + full `cost` object).
        let cat = fixture_catalog();
        let entry = new_pi_model_entry("qwen3.5-122b-a10b", Some(&cat));
        let cfg: pi::models::ModelConfig =
            serde_json::from_value(entry).expect("pi parses our entry");
        assert_eq!(cfg.context_window, Some(262_144));
        let cost = cfg.cost.expect("cost present");
        assert_eq!(cost.input, 0.25);
        assert_eq!(cost.output, 1.75);
        assert_eq!(cfg.reasoning, Some(true));
    }

    #[test]
    fn catalog_json_shape_is_stable() {
        let cat = fixture_catalog();
        let v = catalog_json_for(&cat, "deepseek-v4-flash").expect("metadata");
        assert_eq!(
            v.get("contextWindow").and_then(Value::as_u64),
            Some(200_000)
        );
        assert_eq!(v.get("inputUsdPerMtok").and_then(Value::as_f64), Some(0.25));
        assert_eq!(
            v.get("outputUsdPerMtok").and_then(Value::as_f64),
            Some(1.75)
        );
        assert_eq!(v.get("reasoning").and_then(Value::as_bool), Some(true));
        assert_eq!(
            v.get("name").and_then(Value::as_str),
            Some("DeepSeek V4 Flash")
        );
        assert!(catalog_json_for(&cat, "z-image-turbo").is_none());

        // Suffix/alias matches expose the source entry instead of borrowing
        // its display name.
        let thinking = catalog_json_for(&cat, "deepseek-v4-flash-thinking").expect("metadata");
        assert_eq!(
            thinking.get("resolvedId").and_then(Value::as_str),
            Some("deepseek-v4-flash")
        );
        assert!(thinking.get("name").is_none());
        assert_eq!(
            thinking.get("contextWindow").and_then(Value::as_u64),
            Some(200_000)
        );
    }

    #[test]
    fn enrich_does_not_rename_thinking_variants() {
        let cat = fixture_catalog();
        let entry = new_pi_model_entry("qwen3.6-35b-a3b-thinking", Some(&cat));
        // Metadata flows from the base model…
        assert_eq!(
            entry.get("contextWindow").and_then(Value::as_u64),
            Some(262_144)
        );
        assert!(entry.get("cost").is_some());
        // …but the label stays distinct.
        assert_eq!(
            entry.get("name").and_then(Value::as_str),
            Some("qwen3.6-35b-a3b-thinking")
        );
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_context_window(262_144), "262k");
        assert_eq!(format_context_window(16_000), "16k");
        assert_eq!(format_context_window(512), "512");
        assert_eq!(format_price_per_mtok(0.15, 0.5), "$0.15 / $0.50");
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("object").clone()
    }

    trait CostInput {
        fn pointer_cost_input(&self) -> Option<f64>;
    }
    impl CostInput for Map<String, Value> {
        fn pointer_cost_input(&self) -> Option<f64> {
            self.get("cost")?.get("input")?.as_f64()
        }
    }
}
