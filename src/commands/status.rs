use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::{self, mask_key};

pub fn run() -> Result<()> {
    let cfg = config::load()?;

    println!("{}", "LibertAI status".bold().underline());
    println!("  {:<22} {}", "API base:".dimmed(), cfg.api_base);
    if cfg.account_base != cfg.api_base {
        println!("  {:<22} {}", "Account base:".dimmed(), cfg.account_base);
    }
    println!(
        "  {:<22} {}",
        "Default chat model:".dimmed(),
        cfg.default_chat_model
    );
    println!(
        "  {:<22} {}",
        "Default image model:".dimmed(),
        cfg.default_image_model
    );

    println!("  {}", "Launcher defaults:".dimmed());
    println!(
        "    {:<20} {}",
        "opus:".dimmed(),
        cfg.launcher_defaults.opus_model
    );
    println!(
        "    {:<20} {}",
        "sonnet:".dimmed(),
        cfg.launcher_defaults.sonnet_model
    );
    println!(
        "    {:<20} {}",
        "haiku:".dimmed(),
        cfg.launcher_defaults.haiku_model
    );

    match cfg.auth.api_key.as_deref() {
        Some(k) => println!(
            "  {:<22} {}",
            "Auth:".dimmed(),
            mask_key(k).green()
        ),
        None => println!(
            "  {:<22} {}",
            "Auth:".dimmed(),
            "not logged in".red()
        ),
    }

    if let Some(addr) = cfg.auth.wallet_address.as_deref() {
        let chain = cfg.auth.chain.as_deref().unwrap_or("?");
        println!(
            "  {:<22} {} ({})",
            "Wallet:".dimmed(),
            addr,
            chain
        );
    }

    Ok(())
}
