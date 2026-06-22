//! `libertai code` — our own-brand coding agent.
//!
//! Runs the pi_agent_rust agent loop against LibertAI end-to-end and
//! streams assistant text deltas to stdout with a lightweight,
//! non-interactive renderer. Interactive REPL mode (bottom-bar TUI,
//! raw-mode input, crossterm) lives in a separate task — this renderer
//! stays stream-only so it composes with pipes, tests, and redirection.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use dialoguer::console::Term;
use dialoguer::Select;

use pi::model::AssistantMessageEvent;
use pi::sdk::{create_agent_session, AgentEvent};

use crate::commands::code_approvals::{ApprovalState, ApprovalUi, NotifyOutcome, PromptChoice};
use crate::commands::code_factory::{FactoryFeatures, LibertaiToolFactory, Mode, ModeFlag};
use crate::commands::code_sandbox::{build_command_wrapper, strict_support_error, SandboxMode};
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
    mode: Option<String>,
    resume: Option<String>,
    continue_recent: bool,
    list_sessions: bool,
    all: bool,
    json: bool,
    sandbox: SandboxMode,
    print: bool,
    bg: bool,
    name: Option<String>,
    agent: Option<String>,
    team: Option<String>,
    teammate: Option<String>,
    args: Vec<String>,
) -> Result<()> {
    let cfg = config::load()?;
    // Pi's HTTP client reads PI_HTTP_REQUEST_TIMEOUT_SECS once via
    // OnceLock — set it before any pi-side request fires so the
    // configured idle timeout (cfg.http_timeout_secs, default 600s)
    // wins over pi's baked-in 60s.
    crate::commands::code_session::ensure_pi_http_timeout(cfg.http_timeout_secs);
    let model = model.unwrap_or_else(|| cfg.default_code_model.clone());
    let provider = provider.unwrap_or_else(|| cfg.default_code_provider.clone());
    let mode = parse_initial_mode(plan, mode.as_deref())?;

    // --team / --teammate: set env vars so the factory (and any child
    // background agents) register the team_task tool. This lets a user
    // run a teammate interactively: `libertai code --team myteam
    // --teammate alice`.
    if let Some(t) = team.as_ref() {
        std::env::set_var("LIBERTAI_TEAM", t);
    }
    if let Some(tn) = teammate.as_ref() {
        std::env::set_var("LIBERTAI_TEAMMATE", tn);
    }

    // Brand the base system prompt as LibertAI Code and hide the pi-only
    // docs block. Must precede any `build_system_prompt` call (here or in
    // the REPL / background teammates, which inherit the environment).
    crate::commands::code_identity_prompt::set_brand_env();

    // --list-sessions short-circuits before any agent setup.
    if list_sessions {
        return print_session_list(all, json);
    }

    let oneshot_prompt = if args.is_empty() && !print {
        None
    } else {
        Some(build_oneshot_prompt(&args, print)?)
    };

    // --bg: spawn a detached `libertai code` for the prompt and return
    // to the shell. The run shows up in `libertai agents`. Requires a
    // prompt (the trailing args); conflicts with --print (enforced by
    // clap) and the interactive REPL.
    if bg {
        return run_background(&cfg, &model, &provider, mode, name, agent, team, teammate, oneshot_prompt);
    }

    // Resolve --resume / --continue into an explicit session path, if any.
    let resume_path = resolve_resume_path(resume, continue_recent, print)?;

    // Resolve `--sandbox=auto` to a concrete mode. The CLI only runs
    // the code pillar today, which we treat as "trusted" (user runs
    // `libertai code` against their own machine, expects bash to touch
    // the host), so auto → off. The desktop applies its own per-pillar
    // remap on the worker thread.
    let sandbox = sandbox.resolve(/* is_untrusted = */ false);
    // When the user explicitly asked for strict, bail loudly if the
    // platform/distro can't deliver it — silently running unsandboxed
    // when the user opted in is worse than refusing to start.
    if matches!(sandbox, SandboxMode::Strict) {
        if let Some(reason) = strict_support_error() {
            anyhow::bail!(
                "--sandbox=strict is unavailable: {reason}\n\
                 Re-run without `--sandbox=strict` or fix the host sandbox support first.",
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

    if let Some(prompt) = oneshot_prompt {
        prepare_agent_environment(&cfg)?;

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
        let mode_flag = ModeFlag::new(mode);
        let factory = Arc::new(
            LibertaiToolFactory::new_with_features(
                mode_flag.clone(),
                Arc::clone(&approvals),
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
                print,
                approvals,
                mode_flag,
            )
            .await
        })
    } else {
        prepare_agent_environment(&cfg)?;
        // No prompt on the command line → interactive REPL.
        // Raw-mode UI + input bar + agent session reuse live in code_ui.
        code_ui::run_interactive(
            provider,
            model,
            mode,
            resume_path,
            bash_command_wrapper,
            Arc::new(cfg),
        )
    }
}

fn parse_initial_mode(plan: bool, mode: Option<&str>) -> Result<Mode> {
    let parsed = match mode.map(str::trim).filter(|value| !value.is_empty()) {
        Some("normal" | "default") => Mode::Normal,
        Some("accept-edits" | "accept_edits" | "accept" | "edits") => Mode::AcceptEdits,
        Some("plan" | "readonly" | "read-only") => Mode::Plan,
        Some(other) => bail!("unknown --mode `{other}` (expected normal, accept-edits, or plan)"),
        None => {
            return Ok(if plan { Mode::Plan } else { Mode::Normal });
        }
    };
    if plan && parsed != Mode::Plan {
        bail!("--plan conflicts with --mode {}", mode.unwrap_or_default());
    }
    Ok(parsed)
}

/// `--bg` path: spawn a detached `libertai code` for the prompt, print
/// its run id and management hints, and return to the shell. The run
/// is visible in `libertai agents` and `/agents background`.
///
/// When `--agent <name>` is given the raw prompt is rewritten into a
/// task-tool dispatch instruction (same format as `/agent <name> <task>`)
/// and the agent definition's model override is applied. The spawned
/// child receives the already-embedded prompt, so the launch's `agent`
/// field is left `None`.
#[allow(clippy::too_many_arguments)]
fn run_background(
    _cfg: &LibertaiConfig,
    model: &str,
    provider: &str,
    mode: Mode,
    name: Option<String>,
    agent: Option<String>,
    team: Option<String>,
    teammate: Option<String>,
    prompt: Option<String>,
) -> Result<()> {
    use crate::commands::code_ui::{background_agent_run_id, start_background_agent, BackgroundAgentLaunch};
    let prompt = prompt.ok_or_else(|| anyhow::anyhow!("--bg requires a prompt (pass it as trailing args)"))?;

    // --agent <name>: load the agent definition, build a task-tool
    // dispatch prompt, and apply the agent's model override if any.
    let (model, prompt) = if let Some(agent_name) = agent.as_ref() {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("cwd lookup failed: {e}"))?;
        let agents = crate::commands::code_agents::discover_agents(&cwd)?;
        let agent_def = agents
            .iter()
            .find(|a| a.name == agent_name.as_str())
            .or_else(|| agents.iter().find(|a| a.name.starts_with(agent_name.as_str())))
            .ok_or_else(|| {
                let suffix = if agents.is_empty() {
                    "no named sub-agents are configured".to_string()
                } else {
                    format!(
                        "available sub-agents: {}",
                        agents
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                anyhow::anyhow!("unknown agent `{agent_name}` ({suffix})")
            })?;
        let isolation = if agent_def.worktree {
            " and isolation: \"worktree\""
        } else {
            ""
        };
        let task_prompt = format!(
            "Use the task tool with subagent_type \"{}\"{} for this focused task:\n\n{}\n\nReturn the named sub-agent's findings and cite any files or commands it used.",
            agent_def.name, isolation, prompt
        );
        let resolved_model = agent_def.model.as_deref().unwrap_or(model).to_string();
        (resolved_model, task_prompt)
    } else {
        (model.to_string(), prompt)
    };

    let display_name = name.unwrap_or_else(|| slug_from_prompt(&prompt));
    let cwd = std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd lookup failed: {e}"))?;
    let launch = BackgroundAgentLaunch {
        name: display_name.clone(),
        provider: provider.to_string(),
        model,
        mode,
        prompt,
        cwd,
        agent: None,
        team,
        teammate_name: teammate,
    };
    let started = start_background_agent(&launch)?;
    let started_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let run_id = background_agent_run_id(started.pid, started_at_ms);
    println!("backgrounded · {run_id} · {display_name}");
    println!("  libertai agents             open the agent view");
    println!("  libertai agents --json      machine-readable listing");
    println!("  libertai agents --cwd .      scoped to this directory");
    Ok(())
}

/// Derive a filesystem-safe display name from a prompt: first few
/// words, lowercased, non-alphanumerics replaced with `-`.
pub(crate) fn slug_from_prompt(prompt: &str) -> String {
    let mut slug = String::new();
    for (words, word) in prompt.split_whitespace().enumerate() {
        if words >= 4 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        for ch in word.chars() {
            if ch.is_ascii_alphanumeric() {
                slug.push(ch.to_ascii_lowercase());
            } else {
                slug.push('-');
            }
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug
    }
}

fn prepare_agent_environment(cfg: &LibertaiConfig) -> Result<()> {
    // Make sure pi's models.json knows about libertai before any pi-side
    // code looks it up. Deliberately runs after local short-circuits and
    // prompt validation so auth errors don't hide command-usage problems.
    code_models::ensure_libertai_registered(cfg)?;
    // Point pi's MEMORY.md loader at our per-project memory root so
    // /remember-stored notes reach the system prompt.
    crate::commands::code_memory::ensure_memory_env()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_async(
    provider: String,
    model: String,
    prompt: String,
    factory: Arc<LibertaiToolFactory>,
    resume_path: Option<PathBuf>,
    bash_command_wrapper: Option<Vec<String>>,
    mode: Mode,
    cfg: Arc<LibertaiConfig>,
    print: bool,
    approvals: Arc<ApprovalState>,
    mode_flag: ModeFlag,
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
    let append_system_prompt = crate::commands::code_identity_prompt::apply(append_system_prompt);
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
    let prompt = crate::commands::code_mode_prompt::apply_turn_guidance(prompt, mode);
    let prompt = crate::commands::code_hooks::run_user_prompt_submit_hooks(cfg.as_ref(), &prompt)?;

    if print {
        // --print contract: raw deltas on stdout (scripts parse it),
        // minimal dim notices on stderr, no spinner, no markdown.
        let msg = handle
            .prompt(prompt, move |event| {
                crate::commands::code_hooks::run_post_tool_hooks(hook_cfg.as_ref(), &event);
                render(event);
            })
            .await
            .map_err(anyhow::Error::new)?;
        crate::commands::code_hooks::run_stop_hooks(cfg.as_ref());

        // Make sure we end on a newline regardless of whether the last
        // event was a TextDelta (which never emits one) or AgentEnd
        // (which does).
        println!();
        eprintln!(
            "model: {}/{} stop: {:?} in={} out={}",
            msg.provider, msg.model, msg.stop_reason, msg.usage.input, msg.usage.output
        );
        return Ok(());
    }

    // One-shot interactive run: same renderer as the REPL — markdown
    // when stdout is a TTY, ● tool markers + result previews and the
    // spinner on stderr (so `libertai code "x" > file` still captures
    // assistant text only).
    let renderer = Arc::new(std::sync::Mutex::new(code_ui::TurnRenderer::new(
        code_ui::ChromeStream::Stderr,
        Some(approvals),
        Some(mode_flag),
    )));
    let result = {
        let renderer = Arc::clone(&renderer);
        handle
            .prompt(prompt, move |event| {
                crate::commands::code_hooks::run_post_tool_hooks(hook_cfg.as_ref(), &event);
                if let Ok(mut renderer) = renderer.lock() {
                    renderer.on_event(&event);
                }
            })
            .await
    };
    let elapsed_secs = match renderer.lock() {
        Ok(mut renderer) => {
            renderer.finish_stream();
            renderer.elapsed_secs()
        }
        Err(_) => 0,
    };
    let msg = result.map_err(anyhow::Error::new)?;
    crate::commands::code_hooks::run_stop_hooks(cfg.as_ref());

    let (dim, reset) = crate::commands::output::stderr_dim_pair();
    eprintln!(
        "{dim}{}{reset}",
        code_ui::stop_line_text(
            &msg.stop_reason,
            code_ui::context_tokens(&msg.usage),
            msg.usage.output,
            elapsed_secs,
        )
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
        AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
            if let Some(line) = code_ui::smart_approval_audit_line(&partial_result) {
                let (dim, reset) = crate::commands::output::stderr_dim_pair();
                eprintln!("  {dim}[approval] {line}{reset}");
            }
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
    (!buf.trim().is_empty()).then_some(buf)
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
    fn allows_smart_approval(&self) -> bool {
        false
    }

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
fn resolve_resume_path(
    resume: Option<String>,
    continue_recent: bool,
    print: bool,
) -> Result<Option<PathBuf>> {
    if let Some(raw) = resume {
        let raw = raw.trim();
        if raw.is_empty() {
            // Bare `--resume` (no path) — open the interactive picker,
            // falling back to the most recent session in headless/non-TTY
            // contexts so scripts and `--print` don't block.
            return pick_session_path(print);
        }
        let p = PathBuf::from(raw);
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

/// Bare `--resume` session picker. Lists recent sessions for the current
/// cwd and prompts the user to choose one. In headless mode (`--print`) or
/// when stderr isn't a TTY, resumes the most recent session instead of
/// blocking on a prompt that can't render.
fn pick_session_path(headless: bool) -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir()?;
    let metas = list_past_sessions(Some(&cwd))?;
    if metas.is_empty() {
        bail!(
            "no past sessions for {} — pass a path to `--resume <PATH>`",
            cwd.display()
        );
    }
    if headless || !std::io::stderr().is_terminal() {
        return Ok(Some(PathBuf::from(metas[0].path.clone())));
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let labels: Vec<String> = metas
        .iter()
        .map(|m| {
            let when = format_relative_age(now_ms - m.last_modified_ms);
            let short_id: String = m.id.chars().take(8).collect();
            let name = m.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&short_id);
            format!("{when}  {} msgs  {name}  {}", m.message_count, m.path)
        })
        .collect();
    let term = Term::stderr();
    let choice = Select::new()
        .with_prompt("Pick a session to resume")
        .items(&labels)
        .default(0)
        .interact_on(&term)
        .map_err(|e| anyhow::anyhow!("session picker: {e}"))?;
    Ok(Some(PathBuf::from(metas[choice].path.clone())))
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
