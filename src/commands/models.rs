use anyhow::{Context, Result};

use crate::client::{list_models, ModelList};
use crate::commands::output::Styler;
use crate::config::{self, load, Config};

pub fn run(refresh: bool, json: bool) -> Result<()> {
    let cfg = load()?;
    let list = list_models(&cfg)?;

    if refresh {
        let added = refresh_persisted_catalog(&cfg, &list)?;
        // Human-facing refresh notes go to stderr so `--json` (and plain
        // table piping) keep stdout machine-clean.
        if added == 0 {
            eprintln!(
                "refreshed: {} models from /v1/models; pi models.json already up to date",
                list.data.len()
            );
        } else {
            eprintln!(
                "refreshed: {} models from /v1/models; added {added} new model(s) to pi models.json",
                list.data.len()
            );
        }
    }

    if json {
        // Wire shape as returned by `/v1/models` (`{"data": [...]}`).
        println!(
            "{}",
            serde_json::to_string_pretty(&list).context("rendering model list")?
        );
        return Ok(());
    }

    let st = Styler::stdout();
    let id_width = list
        .data
        .iter()
        .map(|m| m.id.chars().count())
        .max()
        .unwrap_or(2)
        .max("ID".len());

    println!(
        "{:<id_width$}  {}",
        st.bold("ID"),
        st.bold("OWNED BY"),
        id_width = id_width
    );
    for m in &list.data {
        let owner = m.owned_by.as_deref().unwrap_or("-");
        println!("{:<id_width$}  {}", m.id, owner, id_width = id_width);
    }
    Ok(())
}

/// `--refresh`: sync the live `/v1/models` listing into the model catalog
/// persisted in pi's `models.json` (`providers.libertai.models`).
///
/// There is no response cache for `/v1/models` itself — every `libertai
/// models` run hits the API — but the *persisted* catalog that `libertai
/// code` reads is only seeded with `default_code_model` by
/// `code_models::ensure_libertai_registered`, so models launched after
/// install never become selectable in `/model` until something writes
/// them. This merges every fetched id that is missing; richer existing
/// entries (e.g. hand-edited context windows) are left untouched.
///
/// Returns the number of models added.
fn refresh_persisted_catalog(cfg: &Config, list: &ModelList) -> Result<usize> {
    // Guarantees the file and the `providers.libertai` entry exist (and
    // re-asserts baseUrl/apiKey indirection) before we merge into it.
    crate::commands::code_models::ensure_libertai_registered(cfg)?;

    let global_dir = pi::config::Config::global_dir();
    let models_path = pi::models::default_models_path(&global_dir);
    let raw = std::fs::read_to_string(&models_path)
        .with_context(|| format!("reading {}", models_path.display()))?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", models_path.display()))?;

    let models = root
        .get_mut("providers")
        .and_then(|p| p.get_mut("libertai"))
        .and_then(|l| l.get_mut("models"))
        .and_then(|m| m.as_array_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "providers.libertai.models missing in {} — re-run `libertai code` once to seed it",
                models_path.display()
            )
        })?;

    let mut added = 0usize;
    for entry in &list.data {
        let present = models
            .iter()
            .any(|m| m.get("id").and_then(|id| id.as_str()) == Some(entry.id.as_str()));
        if !present {
            // Same placeholder shape ensure_libertai_registered seeds:
            // /v1/models doesn't surface real context windows today.
            models.push(serde_json::json!({
                "id": entry.id,
                "name": entry.id,
                "api": "openai-completions",
                "contextWindow": 32768u32,
            }));
            added += 1;
        }
    }

    if added > 0 {
        let serialized = serde_json::to_string_pretty(&root).context("serializing models.json")?;
        config::write_file_secure(&models_path, serialized.as_bytes())
            .with_context(|| format!("writing {}", models_path.display()))?;
    }
    Ok(added)
}
