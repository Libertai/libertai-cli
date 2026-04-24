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
//! **Security note:** The file itself is written with mode `0o600` via
//! `config::write_file_secure`, so only the owner can read it. The *parent
//! directory* (`~/.pi/agent`) is created by pi itself with whatever default
//! perms pi uses (typically `0o755`) — if we created it we'd set `0o700`,
//! but if pi pre-created it `create_dir_secure` early-returns without
//! tightening. On shared multi-user machines this means the file path is
//! discoverable even though the file contents are not. Tightening pi's
//! global_dir perms would require an upstream change; flagged here so we
//! remember the limitation.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::client::require_api_key;
use crate::config::{self, Config};

/// Ensure `<pi_global_dir>/models.json` has an up-to-date `libertai` provider
/// entry wired to the current libertai-cli config. Creates the file (and
/// parent directory) if missing; merges with existing providers otherwise.
pub fn ensure_libertai_registered(cfg: &Config) -> Result<()> {
    // Require a logged-in key up front — pi would otherwise reject the
    // provider at call-time with a less obvious error.
    let api_key = require_api_key(cfg)?.to_string();

    let global_dir = pi::config::Config::global_dir();
    let models_path = pi::models::default_models_path(&global_dir);

    // Parse the existing file (if any) as a generic Value so unknown fields
    // survive the round-trip untouched.
    let mut root: Value = if models_path.exists() {
        let raw = std::fs::read_to_string(&models_path)
            .with_context(|| format!("reading {}", models_path.display()))?;
        if raw.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", models_path.display()))?
        }
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

    // `openai-completions` routes through pi's OpenAI Chat Completions
    // client (POST /v1/chat/completions), which is what LibertAI exposes.
    // The bare value "openai" is only valid when the provider *name* is
    // also "openai" (canonical provider); for custom names like "libertai"
    // pi needs the fully-qualified api identifier.
    let libertai_entry = json!({
        "baseUrl": base_url,
        "api": "openai-completions",
        "apiKey": api_key,
        "authHeader": true,
        "models": [
            {
                "id": default_model,
                "name": default_model,
                "api": "openai-completions",
                "contextWindow": 32768u32,
            }
        ],
    });

    providers.insert("libertai".to_string(), libertai_entry);

    // Pretty-print for human readability — the file is occasionally edited
    // by hand (pi docs recommend it) and diffs are easier this way.
    let serialized = serde_json::to_string_pretty(&root)
        .context("serializing models.json")?;

    if let Some(parent) = models_path.parent() {
        config::create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    config::write_file_secure(&models_path, serialized.as_bytes())
        .with_context(|| format!("writing {}", models_path.display()))?;

    Ok(())
}
