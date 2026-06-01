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
    /// Start in plan mode (read-only tools; toggle with Shift+Tab or /plan).
    #[arg(long)]
    plan: bool,
    /// Resume a saved session by JSONL path.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["continue_recent", "list_sessions"])]
    resume: Option<std::path::PathBuf>,
    /// Resume the most recent session for the current cwd.
    #[arg(long = "continue", conflicts_with_all = ["resume", "list_sessions"])]
    continue_recent: bool,
    /// Print recent sessions and exit.
    #[arg(long, conflicts_with_all = ["resume", "continue_recent"])]
    list_sessions: bool,
    /// With `--list-sessions`, list every project (not just cwd).
    #[arg(long, requires = "list_sessions")]
    all: bool,
    /// Sandbox the bash tool (`off` / `strict` / `auto`). See
    /// `libertai code --help` for full details. Default: `off`.
    #[arg(long, value_enum, env = "LIBERTAI_SANDBOX", default_value_t = libertai_cli::commands::code_sandbox::SandboxMode::Off)]
    sandbox: libertai_cli::commands::code_sandbox::SandboxMode,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() {
    let parsed = LcodeCli::parse();
    let cli = Cli {
        command: Command::Code {
            model: parsed.model,
            provider: parsed.provider,
            plan: parsed.plan,
            resume: parsed.resume,
            continue_recent: parsed.continue_recent,
            list_sessions: parsed.list_sessions,
            all: parsed.all,
            sandbox: parsed.sandbox,
            args: parsed.args,
        },
    };
    if let Err(e) = libertai_cli::cli::dispatch(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
