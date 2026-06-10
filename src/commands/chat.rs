//! `libertai chat` — streaming chat REPL.
//!
//! Two modes, selected by whether stdin is a terminal:
//!
//! - **Interactive** (stdin is a TTY): rustyline-backed editor with
//!   persisted history (`<config dir>/chat-history.txt`, same location
//!   scheme as `code-history.json`), bracketed-paste multiline input,
//!   markdown-rendered responses (progressive, block-by-block via
//!   `chat_render::MarkdownStream`), chat-local slash commands, and
//!   Ctrl-C cancelling an in-flight response without exiting.
//! - **Piped** (stdin redirected): the legacy line-at-a-time loop with
//!   raw-text streaming to stdout, preserved byte-for-byte so existing
//!   scripts keep working.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use reqwest::header::CONTENT_TYPE;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::client::{list_models, post_chat_blocking, ChatMessage, ChatRequest};
use crate::commands::chat_render::{markdown_enabled_stdout, styling_enabled, MarkdownStream};
use crate::config::{libertai_config_dir, load, Config};

/// Set by the SIGINT handler while a response is streaming; checked
/// between SSE lines so Ctrl-C cancels the response instead of killing
/// the REPL. While rustyline owns the terminal (raw mode) Ctrl-C is a
/// key event, not a signal, so the handler stays out of the way there.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the process SIGINT handler once (the `ctrlc` crate rejects a
/// second registration — same pattern as `code_ui::install_ctrlc_handler`).
fn install_ctrlc_handler() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
    });
}

pub fn run(model: Option<String>, system: Option<String>) -> Result<()> {
    let cfg = load()?;
    let mut model = model.unwrap_or_else(|| cfg.default_chat_model.clone());

    let mut history: Vec<ChatMessage> = Vec::new();
    if let Some(sys) = system {
        history.push(ChatMessage {
            role: "system".to_string(),
            content: sys,
        });
    }

    if std::io::stdin().is_terminal() {
        run_interactive(&cfg, &mut model, &mut history)
    } else {
        run_piped(&cfg, &model, &mut history)
    }
}

// ── piped (legacy) mode ─────────────────────────────────────────────────────

/// stdin is redirected: behave exactly like the pre-overhaul REPL — read
/// lines, stream raw deltas to stdout, prompts and banner on stderr.
fn run_piped(cfg: &Config, model: &str, history: &mut Vec<ChatMessage>) -> Result<()> {
    let accents = styling_enabled(std::io::stderr().is_terminal());
    eprintln!(
        "{}",
        paint(
            &format!("LibertAI chat — model: {model}. Ctrl-D or /exit to quit."),
            Accent::Info,
            accents,
        )
    );

    let stdin = std::io::stdin();
    loop {
        eprint!("{}", paint("> ", Accent::Prompt, accents));
        std::io::stderr().flush().ok();

        let mut buf = String::new();
        let n = stdin.lock().read_line(&mut buf)?;
        if n == 0 {
            eprintln!();
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            break;
        }
        send_turn(cfg, model, history, line, false, accents, false);
    }
    Ok(())
}

// ── interactive mode ────────────────────────────────────────────────────────

fn run_interactive(cfg: &Config, model: &mut String, history: &mut Vec<ChatMessage>) -> Result<()> {
    install_ctrlc_handler();
    let render = markdown_enabled_stdout();
    let accents = styling_enabled(std::io::stderr().is_terminal());

    print_header(model, accents);

    let mut rl = DefaultEditor::new().context("initializing line editor")?;
    let history_path = chat_history_path().ok();
    if let Some(path) = &history_path {
        // First run: no file yet, that's fine.
        let _ = rl.load_history(path);
    }

    loop {
        INTERRUPTED.store(false, Ordering::SeqCst);

        let line = match rl.readline(&prompt_for(accents)) {
            Ok(l) => l,
            // Ctrl-C at the prompt clears the current input.
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl-D on an empty line exits.
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e).context("reading input"),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let _ = rl.add_history_entry(line);
        if let Some(path) = &history_path {
            persist_history(&mut rl, path);
        }

        if let Some(cmd) = line.strip_prefix('/') {
            match handle_slash(cmd, cfg, model, history, accents) {
                SlashOutcome::Continue => continue,
                SlashOutcome::Exit => break,
            }
        }

        send_turn(cfg, model, history, line, render, accents, accents);
    }
    Ok(())
}

fn print_header(model: &str, accents: bool) {
    let title = format!("LibertAI chat · {model}");
    let hints = "Ctrl-D to exit · Ctrl-C cancels a response · /help for commands";
    if accents {
        eprintln!("{}", title.cyan().bold());
        eprintln!("{}", hints.dimmed());
    } else {
        eprintln!("{title}");
        eprintln!("{hints}");
    }
    eprintln!();
}

/// rustyline prompt: `(raw, styled)` pair — rustyline itself falls back
/// to the raw form on non-TTY / NO_COLOR / unsupported terminals. The
/// styled form must keep the same display width as the raw one.
fn prompt_for(accents: bool) -> (&'static str, &'static str) {
    if accents {
        ("❯ ", "\x1b[1;32m❯\x1b[0m ")
    } else {
        ("> ", "> ")
    }
}

fn chat_history_path() -> Result<PathBuf> {
    Ok(libertai_config_dir()?.join("chat-history.txt"))
}

fn persist_history(rl: &mut DefaultEditor, path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Appends only the new entries; falls back to a full save when the
    // file does not exist yet. History persistence is best-effort — a
    // read-only config dir must not break the chat itself.
    if rl.append_history(path).is_err() {
        let _ = rl.save_history(path);
    }
}

// ── slash commands ──────────────────────────────────────────────────────────

enum SlashOutcome {
    Continue,
    Exit,
}

fn handle_slash(
    cmd: &str,
    cfg: &Config,
    model: &mut String,
    history: &mut Vec<ChatMessage>,
    accents: bool,
) -> SlashOutcome {
    let (name, arg) = match cmd.split_once(char::is_whitespace) {
        Some((n, rest)) => (n, rest.trim()),
        None => (cmd, ""),
    };

    match name {
        "exit" | "quit" => return SlashOutcome::Exit,
        "help" => print_help(accents),
        "clear" => {
            history.retain(|m| m.role == "system");
            note("conversation cleared", accents);
        }
        "system" => {
            if arg.is_empty() {
                match history.iter().find(|m| m.role == "system") {
                    Some(m) => note(&format!("system prompt: {}", m.content), accents),
                    None => note("no system prompt set — /system <text> to set one", accents),
                }
            } else {
                match history.iter_mut().find(|m| m.role == "system") {
                    Some(m) => m.content = arg.to_string(),
                    None => history.insert(
                        0,
                        ChatMessage {
                            role: "system".to_string(),
                            content: arg.to_string(),
                        },
                    ),
                }
                note("system prompt updated", accents);
            }
        }
        "model" => switch_model(cfg, model, arg, accents),
        other => warn(&format!("unknown command /{other} — try /help"), accents),
    }
    SlashOutcome::Continue
}

fn print_help(accents: bool) {
    let lines = [
        ("/model", "list available models"),
        ("/model <name|N>", "switch model (name or list index)"),
        ("/system", "show the current system prompt"),
        ("/system <text>", "set or replace the system prompt"),
        ("/clear", "clear the conversation (keeps system prompt)"),
        ("/help", "this help"),
        ("/exit", "quit (also /quit, or Ctrl-D)"),
    ];
    for (cmd, desc) in lines {
        if accents {
            eprintln!("  {:<16} {}", cmd.green(), desc.dimmed());
        } else {
            eprintln!("  {cmd:<16} {desc}");
        }
    }
}

/// `/model` — bare: list models, marking the active one. With an
/// argument: switch by exact id or by 1-based index into the list.
fn switch_model(cfg: &Config, model: &mut String, arg: &str, accents: bool) {
    let list = match list_models(cfg) {
        Ok(l) => Some(l.data),
        Err(e) => {
            // Listing requires the network; switching by name shouldn't.
            if arg.is_empty() {
                warn(&format!("could not list models: {e}"), accents);
                return;
            }
            None
        }
    };

    if arg.is_empty() {
        let Some(models) = list else { return };
        for (i, m) in models.iter().enumerate() {
            let marker = if m.id == *model { "●" } else { " " };
            if accents {
                eprintln!("  {} {:>2}. {}", marker.green(), i + 1, m.id);
            } else {
                eprintln!("  {} {:>2}. {}", marker, i + 1, m.id);
            }
        }
        note("switch with /model <name|N>", accents);
        return;
    }

    let chosen = match (arg.parse::<usize>(), &list) {
        (Ok(n), Some(models)) if n >= 1 && n <= models.len() => models[n - 1].id.clone(),
        (Ok(_), Some(models)) => {
            warn(
                &format!("index out of range (1..{})", models.len()),
                accents,
            );
            return;
        }
        _ => {
            if let Some(models) = &list {
                if !models.iter().any(|m| m.id == arg) {
                    warn("model not in /v1/models list — using it anyway", accents);
                }
            }
            arg.to_string()
        }
    };

    *model = chosen;
    note(&format!("model → {model}"), accents);
}

fn note(msg: &str, accents: bool) {
    if accents {
        eprintln!("{}", format!("  {msg}").dimmed());
    } else {
        eprintln!("  {msg}");
    }
}

fn warn(msg: &str, accents: bool) {
    if accents {
        eprintln!("  {}", msg.yellow());
    } else {
        eprintln!("  {msg}");
    }
}

// ── request/stream plumbing ─────────────────────────────────────────────────

enum Accent {
    Info,
    Prompt,
    Error,
}

fn paint(s: &str, accent: Accent, enabled: bool) -> String {
    if !enabled {
        return s.to_string();
    }
    match accent {
        Accent::Info => s.cyan().to_string(),
        Accent::Prompt => s.green().to_string(),
        Accent::Error => s.red().to_string(),
    }
}

/// Waiting-for-first-token spinner (interactive mode, styled stderr
/// only). indicatif hides itself on non-TTY stderr anyway; the `accents`
/// gate additionally keeps it off under NO_COLOR / TERM=dumb.
fn new_spinner() -> indicatif::ProgressBar {
    let pb = indicatif::ProgressBar::new_spinner().with_message("thinking…");
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

/// Send one user turn and stream the reply. Errors are reported on
/// stderr and roll the user message back so the next turn starts clean —
/// the REPL itself never dies on a failed request.
fn send_turn(
    cfg: &Config,
    model: &str,
    history: &mut Vec<ChatMessage>,
    user_text: &str,
    render: bool,
    accents: bool,
    spinner: bool,
) {
    history.push(ChatMessage {
        role: "user".to_string(),
        content: user_text.to_string(),
    });

    let req = ChatRequest {
        model: model.to_string(),
        messages: history.clone(),
        stream: Some(true),
        max_tokens: None,
    };

    let mut pb = if spinner { Some(new_spinner()) } else { None };
    // Stop the spinner before anything else writes to the terminal.
    let clear_spinner = |pb: &mut Option<indicatif::ProgressBar>| {
        if let Some(p) = pb.take() {
            p.finish_and_clear();
        }
    };

    let resp = match post_chat_blocking(cfg, &req) {
        Ok(r) => r,
        Err(e) => {
            clear_spinner(&mut pb);
            eprintln!("{} {e}", paint("error:", Accent::Error, accents));
            history.pop();
            return;
        }
    };

    // If the server didn't actually give us an SSE stream (e.g. it
    // rejected `stream:true` and returned a plain JSON error), the
    // line-by-line `data: ` parser below would silently swallow every
    // line and we'd print nothing. Detect that up front and surface
    // whatever the server did return.
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false);
    if !is_sse {
        let body = resp.text().unwrap_or_default();
        let shown = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .map(|s| s.to_string())
                })
                .or_else(|| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                });
            match msg {
                Some(m) => format!("error: {m}"),
                None => {
                    let t = truncate_2k(&body);
                    format!("unexpected non-SSE response: {t}")
                }
            }
        } else {
            let t = truncate_2k(&body);
            format!("unexpected non-SSE response: {t}")
        };
        clear_spinner(&mut pb);
        eprintln!("{}", paint(&shown, Accent::Error, accents));
        history.pop();
        return;
    }

    let reader = BufReader::new(resp);
    let mut assistant = String::new();
    let mut sink = MarkdownStream::new(render);
    let mut stream_err: Option<anyhow::Error> = None;
    let mut interrupted = false;

    for line in reader.lines() {
        // Ctrl-C during the stream: stop reading, keep what arrived.
        // Dropping the reader closes the connection. (The check runs per
        // SSE line, so cancellation lands on the next token from the
        // server.)
        if INTERRUPTED.swap(false, Ordering::SeqCst) {
            interrupted = true;
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                stream_err = Some(anyhow::anyhow!(e));
                break;
            }
        };
        // SSE: only `data:` lines carry JSON payloads. Skip blank lines,
        // `:` comments, `event:`, `id:`, and anything else without
        // attempting to parse it as JSON.
        let payload = match line.strip_prefix("data: ") {
            Some(p) => p,
            None => continue,
        };
        if payload.is_empty() {
            continue;
        }
        if payload == "[DONE]" {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(delta) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
        {
            if !delta.is_empty() {
                clear_spinner(&mut pb);
            }
            sink.push(delta);
            assistant.push_str(delta);
        }
    }

    clear_spinner(&mut pb);
    sink.finish();

    if interrupted {
        note("— interrupted —", accents);
    }

    if let Some(e) = stream_err {
        eprintln!("{} {e}", paint("error:", Accent::Error, accents));
        history.pop();
        return;
    }

    if interrupted && assistant.is_empty() {
        // Nothing arrived before the cancel: drop the user turn so a
        // retry doesn't double it.
        history.pop();
        return;
    }

    history.push(ChatMessage {
        role: "assistant".to_string(),
        content: assistant,
    });
}

fn truncate_2k(s: &str) -> String {
    const LIMIT: usize = 2048;
    if s.chars().count() > LIMIT {
        let mut out: String = s.chars().take(LIMIT).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}
