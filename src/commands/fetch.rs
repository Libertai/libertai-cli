use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::commands::fetch_tool::local_fetch;

const MAX_CHARS: usize = 16_000;

pub fn run(url: String, json: bool) -> Result<()> {
    let page = local_fetch(&url, MAX_CHARS).context("fetching url")?;

    if json {
        let raw = serde_json::to_string_pretty(&serde_json::json!({
            "url": page.final_url,
            "title": page.title,
            "content": page.text,
        }))
        .context("rendering response")?;
        println!("{raw}");
        return Ok(());
    }

    println!("{}", page.title.bold());
    println!("{}", page.final_url.cyan());
    println!();
    if page.text.is_empty() {
        eprintln!("no content extracted");
    } else {
        println!("{}", page.text);
    }

    Ok(())
}
