//! Register LibertAI as a custom provider in pi_agent_rust's `models.json`.
//!
//! pi has no built-in LibertAI provider, but it supports user-defined ones
//! via the `providers` map in `<global_dir>/models.json` (loaded by
//! `pi::models::ModelRegistry::load`). We merge — never clobber — so users
//! who have registered other providers keep them.
//!
//! The global_dir is resolved via pi's own API (`pi::config::Config::global_dir`
//! which honors `$PI_CODING_AGENT_DIR` and defaults to `~/.pi/agent`), and
//! the file path via `pi::models::default_models_path` (= `<dir>/models.json`).
//! Using pi's functions means we track whatever location pi actually reads,
//! rather than second-guessing with a hardcoded `~/.config/pi`.
//!
//! Fields are camelCase to match pi's `#[serde(rename_all = "camelCase")]`
//! on `ProviderConfig` / `ModelConfig`.
//!
//! The function is idempotent: running it repeatedly on a healthy file is a
//! no-op in terms of observable state (the libertai entry is overwritten
//! with fresh values from the current config, but surrounding providers are
//! preserved byte-for-byte via `serde_json::Value` round-tripping).
//!
//! **Security note:** We never write the plaintext API key into
//! `models.json`. The `apiKey` field is set to the indirection
//! [`MODELS_JSON_API_KEY_REF`] (`env:LIBERTAI_API_KEY`), which pi resolves
//! via `std::env::var` at registry-load time (`resolve_value_with_base` in
//! pi's `models.rs`). Because pi runs *embedded in this process*, we export
//! [`LIBERTAI_API_KEY_ENV`] here — before any pi-side registry load — so the
//! key only ever travels in memory. The single on-disk copy stays in
//! libertai's own `config.toml`, so key rotation and `libertai logout` have
//! exactly one file to manage. (Anyone running the standalone `pi` binary
//! against this models.json must export `LIBERTAI_API_KEY` themselves; the
//! launchers already do — see `launchers.rs`.)
//!
//! Belt-and-braces: the file is still written with mode `0o600` via
//! `config::write_file_secure` (other providers' entries may carry literal
//! keys), and we tighten the parent directory (`~/.pi/agent`) to `0o700` if
//! it is group/world-accessible — pi historically created it `0o755`.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::client::require_api_key;
use crate::config::{self, Config};

/// Env var pi's registry resolves the libertai `apiKey` from. Matches the
/// variable the launchers export for child tools (opencode et al.).
pub const LIBERTAI_API_KEY_ENV: &str = "LIBERTAI_API_KEY";
/// The literal `apiKey` value written to models.json: pi's
/// `resolve_value_with_base` treats an `env:`-prefixed string as an
/// environment-variable lookup at registry load, so no secret hits disk.
pub const MODELS_JSON_API_KEY_REF: &str = "env:LIBERTAI_API_KEY";

/// Ensure `<pi_global_dir>/models.json` has an up-to-date `libertai` provider
/// entry wired to the current libertai-cli config. Creates the file (and
/// parent directory) if missing; merges with existing providers otherwise.
pub fn ensure_libertai_registered(cfg: &Config) -> Result<()> {
    // Require a logged-in key up front — pi would otherwise reject the
    // provider at call-time with a less obvious error.
    let api_key = require_api_key(cfg)?.to_string();

    // Hand the key to the embedded pi registry in memory: models.json only
    // carries the `env:` indirection, and pi resolves it from this process's
    // environment at registry load. Set unconditionally so config.toml stays
    // the source of truth (matching the old plaintext-write semantics where
    // the config key always won) — rotation via `libertai login` is honoured
    // even if a stale LIBERTAI_API_KEY was exported in the parent shell.
    std::env::set_var(LIBERTAI_API_KEY_ENV, &api_key);

    let global_dir = pi::config::Config::global_dir();
    let models_path = pi::models::default_models_path(&global_dir);

    // Parse the existing file (if any) as a generic Value so unknown fields
    // survive the round-trip untouched. Keep the raw bytes around so we can
    // skip the rewrite when nothing changed.
    let mut existing_raw: Option<String> = None;
    let mut root: Value = if models_path.exists() {
        let raw = std::fs::read_to_string(&models_path)
            .with_context(|| format!("reading {}", models_path.display()))?;
        let parsed = if raw.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", models_path.display()))?
        };
        existing_raw = Some(raw);
        parsed
    } else {
        Value::Object(Map::new())
    };

    // Guarantee `root` and `root.providers` are objects — a malformed file
    // at top level would otherwise silently get overwritten with just our
    // entry. Errors here signal a corrupt file that the user should inspect.
    let root_obj = root.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not a JSON object at the top level",
            models_path.display()
        )
    })?;
    let providers_entry = root_obj
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let providers = providers_entry.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "{} has a non-object `providers` field",
            models_path.display()
        )
    })?;

    let base_url = format!("{}/v1", cfg.api_base.trim_end_matches('/'));
    let default_model = cfg.default_code_model.clone();

    // Merge into the existing libertai entry rather than replacing it.
    // pi's `apply_custom_models` treats a non-empty `models` array as a
    // complete override and wipes pi's built-in catalog before pushing
    // only what's in the JSON. So if a previous run (or another tool
    // — e.g. the liberclaw-code desktop app fetching /v1/models) has
    // already populated `models` with the full catalog, clobbering it
    // with a single-element default-only array reduces the available
    // models to just `default_code_model` and breaks model swaps.
    //
    // Strategy:
    //   - update baseUrl / apiKey on every call so credential rotation
    //     and config edits are honoured;
    //   - leave a non-empty `models` array alone, only ensuring the
    //     current `default_code_model` is present;
    //   - seed an empty/missing array with the single default entry so
    //     fresh installs still get a usable libertai out of the box.
    let entry = providers
        .entry("libertai".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "providers.libertai in {} is not a JSON object",
                models_path.display()
            )
        })?;

    entry.insert("baseUrl".to_string(), Value::String(base_url));
    entry
        .entry("api".to_string())
        .or_insert_with(|| Value::String("openai-completions".into()));
    // Env indirection, never the literal key. Inserting (not `or_insert`)
    // also migrates files written by older CLI versions that persisted the
    // plaintext key here.
    entry.insert(
        "apiKey".to_string(),
        Value::String(MODELS_JSON_API_KEY_REF.to_string()),
    );
    entry
        .entry("authHeader".to_string())
        .or_insert_with(|| Value::Bool(true));

    // `contextWindow` defaults to a generous 32k. The libertai endpoint
    // doesn't surface real per-model context windows in /v1/models
    // today, so the placeholder libertai-cli has used since v0.1 is
    // good enough; if the array already has richer entries (e.g. from
    // a future server-side catalog ingest), we leave them untouched.
    let existing = entry
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let already_present = existing
        .iter()
        .any(|m| m.get("id").and_then(|id| id.as_str()) == Some(default_model.as_str()));
    let mut models_array = existing;
    if models_array.is_empty() || !already_present {
        models_array.push(json!({
            "id": default_model,
            "name": default_model,
            "api": "openai-completions",
            "contextWindow": 32768u32,
        }));
    }
    entry.insert("models".to_string(), Value::Array(models_array));

    // Pretty-print for human readability — the file is occasionally edited
    // by hand (pi docs recommend it) and diffs are easier this way.
    let serialized = serde_json::to_string_pretty(&root).context("serializing models.json")?;

    if let Some(parent) = models_path.parent() {
        config::create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        // pi may have pre-created its global dir 0o755; the file inside used
        // to carry a plaintext key (older CLI versions), so keep the dir
        // owner-only as defense in depth.
        config::tighten_dir_mode_700(parent)
            .with_context(|| format!("tightening perms on {}", parent.display()))?;
    }

    // Skip the rewrite when the content is already up to date, but still
    // re-assert 0600 in case the file pre-dates the secure-write path.
    if existing_raw.as_deref() == Some(serialized.as_str()) {
        config::set_file_mode_600(&models_path)
            .with_context(|| format!("chmod 0600 {}", models_path.display()))?;
        return Ok(());
    }
    config::write_file_secure(&models_path, serialized.as_bytes())
        .with_context(|| format!("writing {}", models_path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end through the pinned pi rev: registration must write only the
    /// `env:` indirection to disk, yet pi's `ModelRegistry::load` (running in
    /// this same process) must resolve it back to the real key in memory.
    ///
    /// Mutates process env (`PI_CODING_AGENT_DIR`, `LIBERTAI_API_KEY`) — this
    /// is the module's only test, and nothing else in the lib test binary
    /// reads `PI_CODING_AGENT_DIR` (same isolation argument as the
    /// `claude_code_import` tests that set `PI_HOME`).
    #[test]
    fn registered_provider_resolves_key_in_memory_only() {
        const KEY: &str = "LTAI_sk_unit_probe_inmemory_0000000000";
        let pi_dir = tempfile::tempdir().expect("pi tempdir");
        std::env::set_var("PI_CODING_AGENT_DIR", pi_dir.path());

        let mut cfg = Config::default();
        cfg.auth.api_key = Some(KEY.to_string());
        ensure_libertai_registered(&cfg).expect("registration succeeds");

        let models_path = pi_dir.path().join("models.json");
        let raw = std::fs::read_to_string(&models_path).expect("models.json written");
        assert!(
            !raw.contains(KEY),
            "plaintext key persisted to models.json:\n{raw}"
        );
        assert!(
            raw.contains(MODELS_JSON_API_KEY_REF),
            "models.json missing `{MODELS_JSON_API_KEY_REF}`:\n{raw}"
        );

        let auth = pi::auth::AuthStorage::load(pi_dir.path().join("auth.json"))
            .expect("empty auth storage loads");
        let registry = pi::models::ModelRegistry::load(&auth, Some(models_path));
        let entry = registry
            .find("libertai", &cfg.default_code_model)
            .expect("libertai default model registered");
        assert_eq!(
            entry.api_key.as_deref(),
            Some(KEY),
            "pi did not resolve env:LIBERTAI_API_KEY to the in-memory key"
        );

        std::env::remove_var("PI_CODING_AGENT_DIR");
        std::env::remove_var(LIBERTAI_API_KEY_ENV);
    }
}
