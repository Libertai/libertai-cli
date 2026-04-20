use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::client::{post_search, SearchRequest};
use crate::config::load;

pub fn run(
    query: String,
    engines: Option<Vec<String>>,
    max_results: Option<u32>,
    search_type: Option<String>,
    json: bool,
) -> Result<()> {
    let cfg = load()?;
    let req = SearchRequest {
        query: &query,
        engines,
        max_results,
        search_type,
    };
    let resp = post_search(&cfg, &req).context("calling search API")?;

    if json {
        let raw = serde_json::to_string_pretty(&resp).context("rendering results")?;
        println!("{raw}");
        return Ok(());
    }

    if resp.results.is_empty() {
        eprintln!("no results");
        return Ok(());
    }

    for (i, r) in resp.results.iter().enumerate() {
        let title = r.title.as_deref().unwrap_or("(no title)");
        let url = r.url.as_deref().unwrap_or("");
        let snippet = r.snippet.as_deref().unwrap_or("");
        println!("{} {}", format!("{:>2}.", i + 1).dimmed(), title.bold());
        if !url.is_empty() {
            println!("    {}", url.cyan());
        }
        if !snippet.is_empty() {
            println!("    {snippet}");
        }
        if let Some(engine) = &r.engine {
            let found = if r.found_in.len() > 1 {
                format!(" (also in {})", r.found_in.join(", "))
            } else {
                String::new()
            };
            println!("    {}{found}", format!("via {engine}").dimmed());
        }
        println!();
    }

    Ok(())
}
