//! CLI surface for the bash sandbox: `libertai sandbox <action>`.
//!
//! Pure presentation layer — the policy logic lives in
//! [`crate::commands::code_sandbox`]. This module just maps clap
//! arguments to that module's functions and renders the result.

use anyhow::Result;

use crate::cli::SandboxAction;
use crate::commands::code_sandbox::{detect_strict_profile, format_profile_text};

pub fn run(action: SandboxAction) -> Result<()> {
    match action {
        SandboxAction::Info { json } => info(json),
    }
}

fn info(json: bool) -> Result<()> {
    // `sandbox info` is descriptive, never mutating. We use the
    // process cwd as the would-be session cwd so the output reflects
    // what a real `libertai code --sandbox=strict` from this directory
    // would expose.
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("cwd lookup failed: {e}"))?;
    let profile = detect_strict_profile(&cwd);
    if json {
        let text = serde_json::to_string_pretty(&profile)
            .map_err(|e| anyhow::anyhow!("serialize profile: {e}"))?;
        println!("{text}");
    } else {
        print!("{}", format_profile_text(&profile));
    }
    Ok(())
}
