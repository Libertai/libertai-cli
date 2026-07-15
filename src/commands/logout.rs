//! `libertai logout` — clear credentials without leaving plaintext keys on disk.
//!
//! Older CLI versions renamed `config.toml` to `config.toml.bak.<epoch>` on
//! logout, which kept the API key (and every previously rotated key) on disk
//! forever. Logout now:
//!
//!   - strips the secret/credential fields from `[auth]` in `config.toml`
//!     *in place*, preserving non-secret preferences (base URLs, default
//!     models, hooks, MCP servers, …) and the per-install `device_id` so a
//!     re-login on this machine reuses its key name;
//!   - purges the same fields from any stray `config.toml.bak.*` files left
//!     behind by prior versions (unparseable backups are deleted outright —
//!     they are disposable by definition and may embed a key);
//!   - replaces a plaintext libertai `apiKey` in pi's `models.json` with the
//!     `env:` indirection, scrubbing registrations written by older versions
//!     of `libertai code`.

use anyhow::{Context, Result};
use std::path::Path;

use crate::commands::code_models::MODELS_JSON_API_KEY_REF;
use crate::config::{config_path, libertai_config_dir, set_file_mode_600, write_file_secure};

/// `[auth]` fields that must not survive logout. `device_id` is deliberately
/// absent: it is a non-secret per-install identifier whose whole point is to
/// persist across login cycles (see `config::Auth`).
const SECRET_AUTH_FIELDS: &[&str] = &[
    "api_key",
    "expires_at",
    "wallet_address",
    "chain",
    "refresh_token",
];

pub fn run() -> Result<()> {
    // Best-effort: revoke the session server-side before we wipe the token
    // locally. Network/credential failures must not block local logout.
    if let Ok(cfg) = crate::config::load() {
        if let Some(rtok) = cfg.auth.refresh_token.as_deref() {
            let _ = crate::client::revoke_session(&cfg, rtok);
        }
    }

    let path = config_path()?;
    let mut cleaned_anything = false;

    if path.exists() {
        cleaned_anything |= scrub_config_file(&path)?;
    }

    // Prior versions left key-bearing `config.toml.bak.<epoch>` files behind;
    // purge their secrets even when the live config is already clean.
    let dir = libertai_config_dir()?;
    if dir.exists() {
        cleaned_anything |= purge_stale_backups(&dir)?;
    }

    // Older `libertai code` runs persisted the plaintext key into pi's
    // models.json; swap it for the in-memory env indirection.
    cleaned_anything |= scrub_pi_models_json()?;

    if !cleaned_anything {
        eprintln!("already logged out");
    }
    Ok(())
}

/// Strip secrets from the live config, keeping every non-secret preference.
/// Returns true when anything was removed (or the file had to be deleted).
fn scrub_config_file(path: &Path) -> Result<bool> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(mut root) = raw.parse::<toml::Value>() else {
        // A config we cannot parse may still embed a key; the only way to
        // guarantee no plaintext key survives is to remove it. config::load
        // would have rejected it anyway.
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        eprintln!(
            "Logged out. {} was not valid TOML — removed it entirely (preferences could not be preserved).",
            path.display()
        );
        return Ok(true);
    };

    let removed = strip_secret_fields(&mut root);
    if removed.is_empty() {
        return Ok(false);
    }

    if root.as_table().is_some_and(toml::Table::is_empty) {
        // Nothing but credentials in there — no preferences worth keeping.
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        eprintln!(
            "Logged out. Removed {} from {} (file deleted — it held nothing else).",
            removed.join(", "),
            path.display()
        );
    } else {
        let serialized = toml::to_string_pretty(&root).context("serializing config")?;
        write_file_secure(path, serialized.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "Logged out. Removed {} from {} (preferences kept).",
            removed.join(", "),
            path.display()
        );
    }
    Ok(true)
}

/// Remove [`SECRET_AUTH_FIELDS`] from `[auth]`, dropping the table when it
/// ends up empty. Returns the names of the fields actually removed.
fn strip_secret_fields(root: &mut toml::Value) -> Vec<&'static str> {
    let mut removed = Vec::new();
    let Some(auth) = root.get_mut("auth").and_then(toml::Value::as_table_mut) else {
        return removed;
    };
    for field in SECRET_AUTH_FIELDS {
        if auth.remove(*field).is_some() {
            removed.push(*field);
        }
    }
    let auth_empty = auth.is_empty();
    if auth_empty {
        if let Some(table) = root.as_table_mut() {
            table.remove("auth");
        }
    }
    removed
}

/// Scrub secrets out of `config.toml.bak.*` files written by earlier logout
/// implementations. Returns true when any backup was modified or deleted.
fn purge_stale_backups(dir: &Path) -> Result<bool> {
    let mut cleaned = false;
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("config.toml.bak.") {
            continue;
        }
        let backup = entry.path();
        let raw = std::fs::read_to_string(&backup)
            .with_context(|| format!("reading {}", backup.display()))?;
        match raw.parse::<toml::Value>() {
            Ok(mut root) => {
                let removed = strip_secret_fields(&mut root);
                if removed.is_empty() {
                    // Nothing secret inside, but the file may pre-date the
                    // chmod-on-backup fix; quietly re-assert owner-only.
                    set_file_mode_600(&backup)
                        .with_context(|| format!("chmod 0600 {}", backup.display()))?;
                    continue;
                }
                let serialized =
                    toml::to_string_pretty(&root).context("serializing backup config")?;
                write_file_secure(&backup, serialized.as_bytes())
                    .with_context(|| format!("writing {}", backup.display()))?;
                eprintln!(
                    "Removed {} from old backup {}.",
                    removed.join(", "),
                    backup.display()
                );
            }
            Err(_) => {
                // Can't prove it holds no key — delete it. Backups from old
                // logouts carry no data the live config doesn't.
                std::fs::remove_file(&backup)
                    .with_context(|| format!("removing {}", backup.display()))?;
                eprintln!("Deleted unreadable old backup {}.", backup.display());
            }
        }
        cleaned = true;
    }
    Ok(cleaned)
}

/// Replace a literal libertai `apiKey` in pi's models.json with the `env:`
/// indirection. Leaves other providers — and any indirection the user set up
/// themselves (`env:` / `file:` / `!cmd`) — untouched. Returns true when the
/// file was rewritten.
fn scrub_pi_models_json() -> Result<bool> {
    let global_dir = pi::config::Config::global_dir();
    let models_path = pi::models::default_models_path(&global_dir);
    if !models_path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&models_path)
        .with_context(|| format!("reading {}", models_path.display()))?;
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&raw) else {
        // Not ours to delete (it can hold other providers' config), but the
        // user should know a key may linger inside.
        eprintln!(
            "warning: {} is not valid JSON — could not scrub a possible libertai apiKey; \
             please inspect it manually",
            models_path.display()
        );
        return Ok(false);
    };

    let Some(api_key) = root
        .get_mut("providers")
        .and_then(|p| p.get_mut("libertai"))
        .and_then(|l| l.get_mut("apiKey"))
    else {
        return Ok(false);
    };
    // Only literal values are secrets; pi's indirections resolve at load time
    // and contain nothing sensitive.
    let is_literal_secret = api_key.as_str().is_none_or(|s| {
        !s.is_empty() && !s.starts_with("env:") && !s.starts_with("file:") && !s.starts_with('!')
    });
    if !is_literal_secret {
        return Ok(false);
    }
    *api_key = serde_json::Value::String(MODELS_JSON_API_KEY_REF.to_string());

    let serialized = serde_json::to_string_pretty(&root).context("serializing models.json")?;
    write_file_secure(&models_path, serialized.as_bytes())
        .with_context(|| format!("writing {}", models_path.display()))?;
    eprintln!(
        "Scrubbed plaintext libertai apiKey from {} (replaced with {}).",
        models_path.display(),
        MODELS_JSON_API_KEY_REF
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_fields_include_refresh_token() {
        assert!(SECRET_AUTH_FIELDS.contains(&"refresh_token"));
    }
}
