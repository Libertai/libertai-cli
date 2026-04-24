use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "libertai",
    version,
    about = "LibertAI CLI — inference, images, and agent-tool launchers.",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Log in (paste API key or sign with wallet).
    Login,
    /// Clear saved credentials (keeps a .bak of the previous config).
    Logout,
    /// Show current auth state and defaults.
    Status,

    /// Manage API keys.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },

    /// List available models.
    Models {
        /// Bypass cache (not yet used; placeholder for future caching).
        #[arg(long)]
        refresh: bool,
    },

    /// One-shot prompt, non-streaming.
    Ask {
        /// The prompt (rest of the args are joined with spaces).
        #[arg(required = true)]
        prompt: Vec<String>,
        #[arg(long)]
        model: Option<String>,
    },

    /// Streaming chat REPL (Ctrl-D to exit).
    Chat {
        #[arg(long)]
        model: Option<String>,
        /// Optional system prompt.
        #[arg(long)]
        system: Option<String>,
    },

    /// Web search via LibertAI's search API (search.libertai.io).
    Search {
        /// The search query.
        #[arg(required = true)]
        query: Vec<String>,
        /// Engines to query (comma-separated). Defaults to google,bing,duckduckgo.
        #[arg(long, value_delimiter = ',')]
        engines: Option<Vec<String>>,
        #[arg(long)]
        max_results: Option<u32>,
        /// web | news | images (defaults to web).
        #[arg(long = "type", alias = "search-type")]
        search_type: Option<String>,
        /// Dump the raw JSON response instead of a pretty list.
        #[arg(long)]
        json: bool,
    },

    /// Fetch a URL and return cleaned article text via search.libertai.io.
    Fetch {
        /// The URL to fetch.
        #[arg(required = true)]
        url: String,
        /// Dump the raw JSON response instead of pretty-printed text.
        #[arg(long)]
        json: bool,
    },

    /// Generate an image.
    Image {
        #[arg(required = true)]
        prompt: Vec<String>,
        #[arg(long)]
        model: Option<String>,
        /// WIDTHxHEIGHT, e.g. 1024x1024
        #[arg(long, default_value = "1024x1024")]
        size: String,
        #[arg(long, short = 'n', default_value_t = 1)]
        n: u32,
        /// Output file (single image) or prefix (multi, e.g. `out` → out-0.png, out-1.png).
        #[arg(long, short = 'o', default_value = "libertai-image.png")]
        out: String,
        /// Overwrite `--out` if it already exists.
        #[arg(long, short = 'f')]
        force: bool,
    },

    /// Launch an arbitrary command with LibertAI env vars injected.
    Run {
        #[arg(long)]
        model: Option<String>,
        /// Command and its arguments after `--`.
        #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },

    /// Launch Claude Code against LibertAI.
    Claude {
        /// Override all three model tiers at once.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        opus: Option<String>,
        #[arg(long)]
        sonnet: Option<String>,
        #[arg(long)]
        haiku: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch OpenCode against LibertAI.
    Opencode {
        #[arg(long)]
        model: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Aider against LibertAI.
    Aider {
        #[arg(long)]
        model: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Claw Code (ultraworkers/claw-code) against LibertAI.
    Claw {
        #[arg(long)]
        model: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// LibertAI's own coding agent, powered by pi_agent_rust.
    ///
    /// Alias: `lcode` (as a separate binary).
    Code {
        /// Model override (defaults to `default_code_model` from config).
        #[arg(long)]
        model: Option<String>,
        /// Provider override (defaults to `default_code_provider` from config, or "libertai").
        #[arg(long)]
        provider: Option<String>,
        /// Start in plan mode: the agent can read/grep/find/ls but
        /// cannot run bash, write, or edit files until you toggle
        /// back to normal (Shift+Tab or /plan).
        #[arg(long)]
        plan: bool,
        /// Initial prompt (non-interactive mode if `--print`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Config file operations.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Install/list/uninstall the bundled agent skills (image gen etc).
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeysAction {
    /// List all API keys for the current account.
    List,
    /// Create a new API key.
    Create {
        name: String,
        /// Monthly spending limit in USD.
        #[arg(long)]
        limit: Option<f64>,
    },
    /// Delete an API key by id.
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// List the bundled skills this CLI knows how to install.
    List,
    /// Install (or refresh) the bundled skills into Claude Code's skill dir.
    /// Defaults to the user-wide location (`~/.claude/skills/`); pass
    /// `--project` to install into `.claude/skills/` in the current directory.
    Install {
        #[arg(long)]
        project: bool,
    },
    /// Remove the bundled skills installed by this CLI.
    Uninstall {
        #[arg(long)]
        project: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print current config.
    Show,
    /// Print config file path.
    Path,
    /// Set a single dotted key, e.g. `default_chat_model gemma-3-27b`.
    Set { key: String, value: String },
    /// Reset a key to its current built-in default so future bumps propagate.
    /// Use `all` to reset every non-auth field.
    Unset { key: String },
}

pub fn dispatch(cli: Cli) -> Result<()> {
    let subcommand = command_name(&cli.command);
    if let Ok(cfg) = crate::config::load() {
        crate::update_check::maybe_notify(&cfg, subcommand);
    }
    match cli.command {
        Command::Login => crate::commands::login::run(),
        Command::Logout => crate::commands::logout::run(),
        Command::Status => crate::commands::status::run(),
        Command::Keys { action } => crate::commands::keys::run(action),
        Command::Models { refresh } => crate::commands::models::run(refresh),
        Command::Ask { prompt, model } => crate::commands::ask::run(prompt.join(" "), model),
        Command::Search {
            query,
            engines,
            max_results,
            search_type,
            json,
        } => crate::commands::search::run(query.join(" "), engines, max_results, search_type, json),
        Command::Fetch { url, json } => crate::commands::fetch::run(url, json),
        Command::Chat { model, system } => crate::commands::chat::run(model, system),
        Command::Image {
            prompt,
            model,
            size,
            n,
            out,
            force,
        } => crate::commands::image::run(prompt.join(" "), model, size, n, out, force),
        Command::Run { model, argv } => crate::commands::run::run(model, argv),
        Command::Claude {
            model,
            opus,
            sonnet,
            haiku,
            args,
        } => crate::commands::launchers::claude(model, opus, sonnet, haiku, args),
        Command::Opencode { model, args } => crate::commands::launchers::opencode(model, args),
        Command::Aider { model, args } => crate::commands::launchers::aider(model, args),
        Command::Claw { model, args } => crate::commands::launchers::claw(model, args),
        Command::Code {
            model,
            provider,
            plan,
            args,
        } => crate::commands::code::run(model, provider, plan, args),
        Command::Config { action } => crate::commands::config_cmd::run(action),
        Command::Skills { action } => crate::commands::skills::run(action),
    }
}

fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Login => "login",
        Command::Logout => "logout",
        Command::Status => "status",
        Command::Keys { .. } => "keys",
        Command::Models { .. } => "models",
        Command::Ask { .. } => "ask",
        Command::Chat { .. } => "chat",
        Command::Search { .. } => "search",
        Command::Fetch { .. } => "fetch",
        Command::Image { .. } => "image",
        Command::Run { .. } => "run",
        Command::Claude { .. } => "claude",
        Command::Opencode { .. } => "opencode",
        Command::Aider { .. } => "aider",
        Command::Claw { .. } => "claw",
        Command::Code { .. } => "code",
        Command::Config { .. } => "config",
        Command::Skills { .. } => "skills",
    }
}
