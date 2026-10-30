use anyhow::{Context, Result};

use crate::client::list_models;
use crate::commands::model_catalog;
use crate::commands::output::Styler;
use crate::config::load;

pub fn run(json: bool) -> Result<()> {
    let cfg = load()?;
    let list = list_models(&cfg)?;

    if json {
        // Wire shape as returned by `/v1/models` (`{"data": [...]}`).
        println!(
            "{}",
            serde_json::to_string_pretty(&list).context("rendering model list")?
        );
        return Ok(());
    }

    let st = Styler::stdout();
    let rows: Vec<(String, String, String, String)> = list
        .data
        .iter()
        .map(|m| {
            let owner = m.owned_by.as_deref().unwrap_or("-").to_string();
            let context = m
                .context_length
                .map(model_catalog::format_context_window)
                .unwrap_or_else(|| "-".to_string());
            let price = m
                .rates_per_mtok()
                .map(|(input, output, _)| model_catalog::format_price_per_mtok(input, output))
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
