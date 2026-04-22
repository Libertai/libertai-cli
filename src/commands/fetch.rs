use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::client::post_fetch;
use crate::config::load;

pub fn run(url: String, json: bool) -> Result<()> {
    let cfg = load()?;
    let resp = post_fetch(&cfg, &url).context("calling fetch API")?;

    if json {
        let raw = serde_json::to_string_pretty(&resp).context("rendering response")?;
        println!("{raw}");
        return Ok(());
    }

    let title = resp.title.as_deref().unwrap_or("(no title)");
    let shown_url = resp.url.as_deref().unwrap_or(&url);
    println!("{}", title.bold());
    println!("{}", shown_url.cyan());
    if let Some(n) = resp.word_count {
        println!("{}", format!("{n} words").dimmed());
    }
    println!();
    if let Some(content) = resp.content.as_deref() {
        println!("{content}");
    } else {
        eprintln!("no content extracted");
    }

    Ok(())
}
