use anyhow::{Context, Result};

use crate::client::list_models;
use crate::commands::model_catalog::{self};
use crate::commands::output::Styler;
use crate::config::load;

pub fn run(json: bool) -> Result<()> {
    let cfg = load()?;
    let list = list_models(&cfg)?;
    // Loaded only after /v1/models succeeded, so auth/network failures keep
    // their exit codes (3/4) without ever touching the catalog endpoint.
    // `None` (offline, disabled) degrades to dashes / wire-only JSON.
    let catalog = model_catalog::load();

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
