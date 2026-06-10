//! `libertai code` — our own-brand coding agent.
//!
//! Runs the pi_agent_rust agent loop against LibertAI end-to-end and
//! streams assistant text deltas to stdout with a lightweight,
//! non-interactive renderer. Interactive REPL mode (bottom-bar TUI,
//! raw-mode input, crossterm) lives in a separate task — this renderer
//! stays stream-only so it composes with pipes, tests, and redirection.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};

use pi::model::AssistantMessageEvent;
use pi::sdk::{create_agent_session, AgentEvent};

use crate::commands::code_approvals::{ApprovalState, ApprovalUi, NotifyOutcome, PromptChoice};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_sandbox::{build_command_wrapper, is_strict_supported, SandboxMode};
use crate::commands::code_session::{
    build_session_options, list_past_sessions, most_recent_session, CodeSessionConfig,
    SessionPersistence,
};
use crate::commands::code_skills::{self, SkillPillar};
use crate::commands::code_term::TerminalApprovalUi;
use crate::commands::{code_models, code_ui};
use crate::config::{self, Config as LibertaiConfig};

#[allow(clippy::too_many_arguments)]
pub fn run(
    model: Option<String>,
    provider: Option<String>,
    plan: bool,
    resume: Option<PathBuf>,
    continue_recent: bool,
    list_sessions: bool,
    all: bool,
    json: bool,
    sandbox: SandboxMode,
    print: bool,
    args: Vec<String>,
) -> Result<()> {
    let cfg = config::load()?;
    // Pi's HTTP client reads PI_HTTP_REQUEST_TIMEOUT_SECS once via
    // OnceLock — set it before any pi-side request fires so the
    // configured idle timeout (cfg.http_timeout_secs, default 600s)
    // wins over pi's baked-in 60s.
    crate::commands::code_session::ensure_pi_http_timeout(cfg.http_timeout_secs);
    // Make sure pi's models.json knows about libertai before any pi-side
    // code looks it up. Runs first so auth / FS errors surface before we
    // spin up the async runtime.
    code_models::ensure_libertai_registered(&cfg)?;
    // Point pi's MEMORY.md loader at our per-project memory root so
    // /remember-stored notes reach the system prompt.
    crate::commands::code_memory::ensure_memory_env()?;

    let model = model.unwrap_or_else(|| cfg.default_code_model.clone());
    let provider = provider.unwrap_or_else(|| cfg.default_code_provider.clone());
    let mode = if plan { Mode::Plan } else { Mode::Normal };

    // --list-sessions short-circuits before any agent setup.
    if list_sessions {
        return print_session_list(all, json);
    }

    // Resolve --resume / --continue into an explicit session path, if any.
    let resume_path = resolve_resume_path(resume, continue_recent)?;

    // Resolve `--sandbox=auto` to a concrete mode. The CLI only runs
    // the code pillar today, which we treat as "trusted" (user runs
    // `libertai code` against their own machine, expects bash to touch
    // the host), so auto → off. The desktop applies its own per-pillar
    // remap on the worker thread.
    let sandbox = sandbox.resolve(/* is_untrusted = */ false);
    // When the user explicitly asked for strict, bail loudly if the
    // platform/distro can't deliver it — silently running unsandboxed
    // when the user opted in is worse than refusing to start.
    if matches!(sandbox, SandboxMode::Strict) && !is_strict_supported() {
        if cfg!(target_os = "linux") {
            anyhow::bail!(
                "--sandbox=strict requires `bwrap` on PATH but it wasn't found. \
                 Install it (Debian/Ubuntu: `apt install bubblewrap`; \
                 Fedora/RHEL: `dnf install bubblewrap`; \
                 Arch: `pacman -S bubblewrap`; \
                 NixOS: add `bubblewrap` to your shell or system packages) \
                 and re-run, or drop `--sandbox=strict`.",
            );
        } else {
            anyhow::bail!(
                "--sandbox=strict is Linux-only today (macOS and Windows \
                 backends are tracked as follow-ups). Re-run without \
                 `--sandbox=strict` to use the default unsandboxed bash.",
            );
        }
    }
    let bash_command_wrapper = build_command_wrapper(
        sandbox,
        &std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd lookup failed: {e}"))?,
        // CLI doesn't carry a persisted SandboxPolicy override today;
        // host-detected defaults apply verbatim. The desktop passes
        // `Some(&policy)` to let users uncheck binds in settings.
        None,
    );

    if args.is_empty() && !print {
        // No prompt on the command line → interactive REPL.
        // Raw-mode UI + input bar + agent session reuse live in code_ui.
        return code_ui::run_interactive(
            provider,
            model,
            mode,
            resume_path,
            bash_command_wrapper,
            Arc::new(cfg),
        );
    }

    let prompt = build_oneshot_prompt(&args, print)?;

    // pi uses asupersync as its async runtime (not tokio).
    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|e| anyhow::anyhow!("asupersync reactor: {e}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!("asupersync runtime: {e}"))?;

    // Non-interactive path honours --plan too, in case someone wants a
    // one-shot planning run: `libertai code --plan "refactor src/foo"`.
    // The flag is created here even though it can't be toggled from a
    // one-shot — it's part of the factory's contract now.
    let approvals = Arc::new(ApprovalState::with_persistent_store(
        crate::config::allow_rules_path()?,
    )?);
    // --print never blocks on the terminal: anything that would need an
    // interactive approval is auto-denied instead. Without it, one-shot
    // runs keep the terminal micro-prompt (the user is still at a TTY).
    let ui: Arc<dyn ApprovalUi> = if print {
        Arc::new(PrintModeApprovalUi)
    } else {
        Arc::new(TerminalApprovalUi)
    };
    let cfg = Arc::new(cfg);
    let factory = Arc::new(
        LibertaiToolFactory::new_with_features(
            ModeFlag::new(mode),
            approvals,
            ui,
            FactoryFeatures::cli_defaults(),
            Some(Arc::clone(&cfg)),
        )
        .with_tool_policy(crate::commands::code_hooks::tool_policy_from_config(
            Arc::clone(&cfg),
        )),
    );

    runtime.block_on(async move {
        run_async(
            provider,
            model,
            prompt,
            factory,
            resume_path,
            bash_command_wrapper,
            mode,
            cfg,
        )
        .await
    })
}

async fn run_async(
    provider: String,
    model: String,
    prompt: String,
    factory: Arc<LibertaiToolFactory>,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    mode: Mode,
    cfg: Arc<LibertaiConfig>,
) -> Result<()> {
    // One-shots are typically piped — print only the agent's response,
    // never replay prior history (it would corrupt downstream output).
    // The agent itself still sees the full message history because pi
    // loads it from the JSONL on the way up.
    let persistence = match resume_path {
        Some(p) => SessionPersistence::Resume(p),
        None => SessionPersistence::Fresh,
    };
    let max_tokens = Some(crate::commands::code_session::DEFAULT_MAX_TOKENS);
    let skill_cwd = std::env::current_dir().ok();
    let append_system_prompt =
        code_skills::prompt_for_pillar(SkillPillar::Code, skill_cwd.as_deref())?;
    // Git context is injected once by pi (build_git_context); do not duplicate it here.
    let append_system_prompt = crate::commands::code_mode_prompt::apply(append_system_prompt, mode);
    let options = build_session_options(CodeSessionConfig {
        provider,
        model,
        working_directory: None,
        include_cwd_in_prompt: true,
        max_tool_iterations: 50,
        tool_factory: factory,
        persistence,
        enabled_tools: None,
        append_system_prompt,
        max_tokens,
        bash_command_wrapper,
        auto_compaction_enabled: cfg.code_auto_compaction_enabled,
        compaction_reserve_tokens: cfg.code_compaction_reserve_tokens,
        compaction_keep_recent_tokens: cfg.code_compaction_keep_recent_tokens,
    });

    // anyhow::Error::new preserves the underlying pi::sdk::Error so
    // downcast-based checks (e.g. Aborted detection) keep working.
    let mut handle = create_agent_session(options)
        .await
        .map_err(|e| anyhow::Error::new(e).context("create_agent_session"))?;
    handle.set_max_tokens(max_tokens);

    let _session_hooks = crate::commands::code_hooks::SessionHookGuard::start(Arc::clone(&cfg));
    let hook_cfg = Arc::clone(&cfg);
    let prompt = crate::commands::code_hooks::run_user_prompt_submit_hooks(cfg.as_ref(), &prompt)?;
    let msg = handle
        .prompt(prompt, move |event| {
            crate::commands::code_hooks::run_post_tool_hooks(hook_cfg.as_ref(), &event);
            render(event);
        })
        .await
        .map_err(anyhow::Error::new)?;
    crate::commands::code_hooks::run_stop_hooks(cfg.as_ref());

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
            let (dim, reset) = crate::commands::output::stderr_dim_pair();
            eprintln!("\n  {dim}[turn {turn_index}]{reset}");
        }
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } if tool_name != "todo" => {
            let preview = crate::commands::code_tool_preview::tool_preview(&tool_name, &args);
            let (dim, reset) = crate::commands::output::stderr_dim_pair();
            eprintln!("  {dim}[tool] {preview}{reset}");
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

/// Assemble the prompt for a one-shot (non-REPL) run.
///
/// Without `--print` the trailing args are the whole prompt and stdin
/// is left untouched (the terminal approval prompt may still need it).
/// With `--print`, `claude -p` semantics apply: the prompt can come
/// from the args, from piped stdin, or both — piped stdin is placed
/// above the args prompt as context.
fn build_oneshot_prompt(args: &[String], print: bool) -> Result<String> {
    let arg_prompt = args.join(" ");
    if !print {
        return Ok(arg_prompt);
    }
    match (read_piped_stdin(), arg_prompt.is_empty()) {
        (Some(ctx), false) => Ok(format!("{ctx}\n\n{arg_prompt}")),
        (Some(ctx), true) => Ok(ctx),
        (None, false) => Ok(arg_prompt),
        (None, true) => bail!(
            "--print needs a prompt: pass it as arguments \
             (`libertai code -p \"fix the build\"`) or pipe it on stdin",
        ),
    }
}

/// Read piped stdin to EOF. Returns `None` when stdin is a TTY (nothing
/// was piped) or the piped input is blank.
fn read_piped_stdin() -> Option<String> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = String::new();
    stdin.read_to_string(&mut buf).ok()?;
    let trimmed = buf.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `--print` approval UI: never blocks on the terminal.
///
/// Read-only tools and persisted allow rules are resolved before the UI
/// is consulted (see `ApprovalState`), so this only fires for calls the
/// user hasn't pre-approved — those are denied with a stderr note,
/// mirroring how `claude -p` refuses un-permitted tools rather than
/// hanging a script on a hidden prompt.
struct PrintModeApprovalUi;

#[async_trait::async_trait]
impl ApprovalUi for PrintModeApprovalUi {
    async fn decide(&self, tool_name: &str, _preview: &str, always_rule: &str) -> PromptChoice {
        let (dim, reset) = crate::commands::output::stderr_dim_pair();
        eprintln!(
            "  {dim}[print] {tool_name} needs approval — auto-denied (non-interactive). \
             Pre-approve it by running interactively once and choosing \
             \"always allow\" ({always_rule}).{reset}"
        );
        PromptChoice::Deny
    }

    async fn notify(&self, title: &str, body: &str) -> NotifyOutcome {
        // Plain stderr rendering is already headless-safe; reuse it.
        crate::commands::code_term::notify_terminal(title, body)
    }
}

/// Resolve `--resume <path>` / `--continue` to an explicit JSONL path.
///
/// Returns `Ok(None)` for "no resume requested". `--resume` and
/// `--continue` are mutually exclusive at the clap layer so we never see
/// both set here.
fn resolve_resume_path(resume: Option<PathBuf>, continue_recent: bool) -> Result<Option<PathBuf>> {
    if let Some(p) = resume {
        if !p.exists() {
            bail!("--resume: session file not found: {}", p.display());
        }
        return Ok(Some(p));
    }
    if continue_recent {
        let cwd = std::env::current_dir()?;
        let recent = most_recent_session(&cwd)?
            .ok_or_else(|| anyhow::anyhow!("no past sessions for {}", cwd.display()))?;
        return Ok(Some(PathBuf::from(recent.path)));
    }
    Ok(None)
}

/// Print recent session metadata sorted recency-desc, then exit.
///
/// `all = false` filters to the current cwd; `all = true` lists every
/// project pi has tracked. With `json = true`, a JSON array (stable field
/// names, possibly empty) is the only thing written to stdout.
fn print_session_list(all: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let metas = if all {
        list_past_sessions(None)?
    } else {
        list_past_sessions(Some(&cwd))?
    };

    if json {
        let rows: Vec<serde_json::Value> = metas
            .iter()
            .map(|m| {
                serde_json::json!({
                    "path": m.path,
                    "id": m.id,
                    "cwd": m.cwd,
                    "name": m.name,
                    "timestamp": m.timestamp,
                    "message_count": m.message_count,
                    "last_modified_ms": m.last_modified_ms,
                    "size_bytes": m.size_bytes,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows)
                .map_err(|e| anyhow::anyhow!("rendering session list: {e}"))?
        );
        return Ok(());
    }

    if metas.is_empty() {
        if all {
            println!("no past sessions");
        } else {
            println!("no past sessions for {}", cwd.display());
        }
        return Ok(());
    }

    // Compact one-line-per-row layout: relative-age · #msgs · name? · path.
    // Path goes last so terminals that wrap don't push it off-screen.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    for m in metas {
        let name = m.name.as_deref().unwrap_or("");
        let when = format_relative_age(now_ms - m.last_modified_ms);
        if name.is_empty() {
            println!("{:>10}  {:>4} msgs  {}", when, m.message_count, m.path);
        } else {
            println!(
                "{:>10}  {:>4} msgs  {}  {}",
                when, m.message_count, name, m.path
            );
        }
    }
    Ok(())
}

/// "12s ago", "5m ago", "3h ago", "2d ago" — relative-time string for
/// the session list. Avoids adding a date-formatting dep.
fn format_relative_age(diff_ms: i64) -> String {
    if diff_ms < 0 {
        return "just now".into();
    }
    let s = diff_ms / 1000;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}
