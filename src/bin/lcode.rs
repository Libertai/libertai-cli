//! `lcode` — short alias binary for `libertai code`.
//!
//! Re-enters the same dispatch that `libertai code ...` would hit, sparing
//! users a two-token invocation for the most-used subcommand.

use clap::Parser;
use libertai_cli::cli::{Cli, Command};

#[derive(Debug, Parser)]
#[command(
    name = "lcode",
    version,
    about = "LibertAI coding agent (alias of `libertai code`).",
    propagate_version = true
)]
struct LcodeCli {
    /// Model override (defaults to `default_code_model` from config).
    #[arg(long)]
    model: Option<String>,
    /// Provider override (defaults to `default_code_provider` from config).
    #[arg(long)]
    provider: Option<String>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() {
    let parsed = LcodeCli::parse();
    let cli = Cli {
        command: Command::Code {
            model: parsed.model,
            provider: parsed.provider,
            args: parsed.args,
        },
    };
    if let Err(e) = libertai_cli::cli::dispatch(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
