//! `libertai code` — our own-brand coding agent.
//!
//! Runs the pi_agent_rust agent loop against LibertAI end-to-end and
//! streams assistant text deltas to stdout with a lightweight,
//! non-interactive renderer. Interactive REPL mode (bottom-bar TUI,
//! raw-mode input, crossterm) lives in a separate task — this renderer
//! stays stream-only so it composes with pipes, tests, and redirection.

use std::sync::Arc;

use anyhow::Result;

use pi::model::AssistantMessageEvent;
use pi::sdk::{create_agent_session, AgentEvent, SessionOptions};

use crate::commands::code_approvals::ApprovalState;
use crate::commands::code_factory::{LibertaiToolFactory, Mode};
use crate::commands::{code_models, code_ui};
use crate::config;

pub fn run(
    model: Option<String>,
    provider: Option<String>,
    plan: bool,
    args: Vec<String>,
) -> Result<()> {
    let cfg = config::load()?;
    // Make sure pi's models.json knows about libertai before any pi-side
    // code looks it up. Runs first so auth / FS errors surface before we
    // spin up the async runtime.
    code_models::ensure_libertai_registered(&cfg)?;

    let model = model.unwrap_or_else(|| cfg.default_code_model.clone());
    let provider = provider.unwrap_or_else(|| cfg.default_code_provider.clone());
    let mode = if plan { Mode::Plan } else { Mode::Normal };

    if args.is_empty() {
        // No prompt on the command line → interactive REPL.
        // Raw-mode UI + input bar + agent session reuse live in code_ui.
        return code_ui::run_interactive(provider, model, mode);
    }

    let prompt = args.join(" ");

    // pi uses asupersync as its async runtime (not tokio).
    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|e| anyhow::anyhow!("asupersync reactor: {e}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!("asupersync runtime: {e}"))?;

    // Non-interactive path honours --plan too, in case someone wants a
    // one-shot planning run: `libertai code --plan "refactor src/foo"`.
    let approvals = Arc::new(ApprovalState::new());
    let factory = Arc::new(LibertaiToolFactory::new(mode, approvals));

    runtime.block_on(async move { run_async(provider, model, prompt, factory).await })
}

async fn run_async(
    provider: String,
    model: String,
    prompt: String,
    factory: Arc<LibertaiToolFactory>,
) -> Result<()> {
    let options = SessionOptions {
        provider: Some(provider),
        model: Some(model),
        // v0: ephemeral session. Persistence / session resumption lands
        // with the interactive REPL in a follow-up.
        no_session: true,
        max_tool_iterations: 50,
        tool_factory: Some(factory),
        ..SessionOptions::default()
    };

    let mut handle = create_agent_session(options)
        .await
        .map_err(|e| anyhow::anyhow!("create_agent_session: {e}"))?;

    let msg = handle
        .prompt(prompt, render)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Make sure we end on a newline regardless of whether the last event
    // was a TextDelta (which never emits one) or AgentEnd (which does).
    println!();

    eprintln!(
        "model: {}/{} stop: {:?} in={} out={}",
        msg.provider, msg.model, msg.stop_reason, msg.usage.input, msg.usage.output
    );

    Ok(())
}

/// Per-event renderer for non-interactive streaming output.
///
/// Text deltas go to stdout so they can be piped; everything else
/// (turn markers, tool execution notices) goes to stderr in dim ANSI
/// so it stays out of pipelines. This mirrors the contract in
/// `feedback_own_renderer.md`: we do our own rendering, we don't inherit
/// pi's TUI.
fn render(event: AgentEvent) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            use std::io::Write;
            print!("{delta}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::TurnStart { turn_index, .. } => {
            eprintln!("\n  \x1b[2m[turn {turn_index}]\x1b[0m");
        }
        AgentEvent::ToolExecutionStart { tool_name, .. } if tool_name != "todo" => {
            eprintln!("  \x1b[2m[tool] {tool_name}\x1b[0m");
        }
        AgentEvent::AgentEnd { .. } => {
            // AgentEnd fires at the tail of the agent loop; a newline here
            // flushes any trailing delta line so the usage-stats eprintln
            // in run_async starts on its own line.
            println!();
        }
        _ => {}
    }
}
