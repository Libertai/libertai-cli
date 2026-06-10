use anyhow::{Context, Result};

use crate::client::{list_models, ModelList};
use crate::commands::model_catalog::{self, Catalog};
use crate::commands::output::Styler;
use crate::config::{self, load, Config};

pub fn run(refresh: bool, json: bool) -> Result<()> {
    let cfg = load()?;
    let list = list_models(&cfg)?;
    // Loaded only after /v1/models succeeded, so auth/network failures keep
    // their exit codes (3/4) without ever touching the catalog endpoint.
    // `None` (offline, disabled) degrades to dashes / wire-only JSON.
    let catalog = model_catalog::load();

    if refresh {
        let added = refresh_persisted_catalog(&cfg, &list, catalog.as_ref())?;
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
        // Wire shape as returned by `/v1/models` (`{"data": [...]}`), with a
        // `catalog` object added per model when public-catalog metadata is
        // available (documented in the README "Scripting" section).
        let mut value = serde_json::to_value(&list).context("rendering model list")?;
        if let Some(cat) = catalog.as_ref() {
            if let Some(items) = value.get_mut("data").and_then(|d| d.as_array_mut()) {
                for item in items {
                    let Some(obj) = item.as_object_mut() else {
                        continue;
                    };
                    let Some(id) = obj.get("id").and_then(|v| v.as_str()).map(str::to_string)
                    else {
                        continue;
                    };
                    if let Some(meta) = model_catalog::catalog_json_for(cat, &id) {
                        obj.insert("catalog".to_string(), meta);
                    }
                }
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&value).context("rendering model list")?
        );
        return Ok(());
    }

    let st = Styler::stdout();
    let rows: Vec<(String, String, String, String)> = list
        .data
        .iter()
        .map(|m| {
            let owner = m.owned_by.as_deref().unwrap_or("-").to_string();
            let meta = catalog.as_ref().and_then(|c| c.find_text(&m.id));
            let context = meta
                .and_then(|x| x.text_capabilities())
                .and_then(|c| c.context_window)
                .map(model_catalog::format_context_window)
                .unwrap_or_else(|| "-".to_string());
            let price = meta
                .and_then(|x| x.text_pricing())
                .map(|p| {
                    model_catalog::format_price_per_mtok(
                        p.price_per_million_input_tokens,
                        p.price_per_million_output_tokens,
                    )
                })
                .unwrap_or_else(|| "-".to_string());
            (m.id.clone(), owner, context, price)
        })
        .collect();

    let width = |header: &str, col: usize| -> usize {
        rows.iter()
            .map(|r| match col {
                0 => r.0.chars().count(),
                1 => r.1.chars().count(),
                _ => r.2.chars().count(),
            })
            .max()
            .unwrap_or(0)
            .max(header.chars().count())
    };
    let id_width = width("ID", 0);
    let owner_width = width("OWNED BY", 1);
    let ctx_width = width("CONTEXT", 2);

    println!(
        "{:<id_width$}  {:<owner_width$}  {:<ctx_width$}  {}",
        st.bold("ID"),
        st.bold("OWNED BY"),
        st.bold("CONTEXT"),
        st.bold("PRICE IN/OUT ($/MTOK)"),
        id_width = id_width,
        owner_width = owner_width,
        ctx_width = ctx_width,
    );
    for (id, owner, context, price) in &rows {
        println!(
            "{id:<id_width$}  {owner:<owner_width$}  {context:<ctx_width$}  {price}",
            id_width = id_width,
            owner_width = owner_width,
            ctx_width = ctx_width,
        );
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
/// them. This merges every fetched id that is missing — new entries carry
/// real context windows and cost from the public catalog when available —
/// while richer existing entries (e.g. hand-edited context windows) are
/// left untouched (`ensure_libertai_registered` upgrades only our own
/// legacy 32k placeholders).
///
/// Returns the number of models added.
fn refresh_persisted_catalog(
    cfg: &Config,
    list: &ModelList,
    catalog: Option<&Catalog>,
) -> Result<usize> {
    // Guarantees the file and the `providers.libertai` entry exist (and
    // re-asserts baseUrl/apiKey indirection, plus catalog enrichment of
    // existing entries) before we merge into it.
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
            models.push(model_catalog::new_pi_model_entry(&entry.id, catalog));
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
