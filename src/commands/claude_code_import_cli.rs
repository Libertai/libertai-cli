//! CLI surface for Claude Code session discovery:
//! `libertai import claude-code list [--all] [--json]`.
//!
//! Read-only browse, matching the [`code_sandbox_cli`] pattern:
//! discovery logic lives in [`claude_code_import`], this module only
//! formats the result.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Local};

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cli::{ClaudeCodeImportAction, ImportAction};
use crate::client::{post_chat_blocking, ChatMessage, ChatRequest};
use crate::commands::claude_code_import::{
    build_summary_prompt, discover, encode_project_dir, estimate_tokens, linearize,
    render_transcript, write_pi_session_file, DiscoveredSession, LinearizedSession,
    PiSessionInputs,
};
use crate::config::{load, Config};

pub fn run(action: ImportAction) -> Result<()> {
    match action {
        ImportAction::ClaudeCode { action } => match action {
            ClaudeCodeImportAction::List { all, json } => list(all, json),
            ClaudeCodeImportAction::Show { id_or_path, all, json } => show(&id_or_path, all, json),
            ClaudeCodeImportAction::Summarize { id_or_path, all, model, print_prompt } => {
                summarize(&id_or_path, all, model, print_prompt)
            }
            ClaudeCodeImportAction::Import { id_or_path, all, model, provider, dry_run } => {
                import_session(&id_or_path, all, model, provider, dry_run)
            }
        },
    }
}

fn list(all_projects: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let sessions = discover(&cwd, all_projects)?;
    if json {
        let payload = serde_json::to_string_pretty(&sessions)?;
        println!("{payload}");
        return Ok(());
    }
    print_human(&cwd, all_projects, &sessions);
    Ok(())
}

fn print_human(cwd: &std::path::Path, all_projects: bool, sessions: &[DiscoveredSession]) {
    if sessions.is_empty() {
        if all_projects {
            println!("No Claude Code sessions found under $HOME/.claude/projects.");
        } else {
            println!(
                "No Claude Code sessions found for {}.\nPass --all to scan every project.",
                cwd.display()
            );
        }
        return;
    }

    let scope = if all_projects { "all projects" } else { &cwd.display().to_string() };
    println!("Claude Code sessions for {scope} ({} total):\n", sessions.len());
    for s in sessions {
        println!("  • {}", s.session_uuid);
        if let Some(cwd) = &s.recorded_cwd {
            println!("      project : {}", cwd.display());
        }
        if let Some(branch) = &s.git_branch {
            println!("      branch  : {branch}");
        }
        println!("      mtime   : {}", format_mtime(s.mtime));
        println!("      records : {}  ({})", s.record_count, format_size(s.size_bytes));
        if let Some(summary) = &s.claude_code_summary {
            println!("      summary : {summary}");
        }
        if let Some(first) = &s.first_user_message {
            println!("      first   : {first}");
        }
        println!("      path    : {}", s.jsonl_path.display());
        println!();
    }
}

fn format_mtime(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    DateTime::<Local>::from(UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn show(id_or_path: &str, all_projects: bool, json: bool) -> Result<()> {
    let path = resolve_session(id_or_path, all_projects)?;
    let lin = linearize(&path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&lin)?);
        return Ok(());
    }
    println!("Session : {}", lin.session_uuid);
    if let Some(cwd) = &lin.recorded_cwd {
        println!("Project : {}", cwd.display());
    }
    if let Some(b) = &lin.git_branch {
        println!("Branch  : {b}");
    }
    println!(
        "Records : {} total, {} on live branch (dropped {} sidechain, {} branch)",
        lin.total_records,
        lin.messages.len(),
        lin.dropped_sidechain,
        lin.dropped_branches,
    );
    if !lin.read_files.is_empty() {
        println!("Read    : {} file(s)", lin.read_files.len());
        for p in &lin.read_files { println!("          {}", p.display()); }
    }
    if !lin.modified_files.is_empty() {
        println!("Modified: {} file(s)", lin.modified_files.len());
        for p in &lin.modified_files { println!("          {}", p.display()); }
    }
    println!();
    print!("{}", render_transcript(&lin));
    Ok(())
}

fn summarize(
    id_or_path: &str,
    all_projects: bool,
    model: Option<String>,
    print_prompt: bool,
) -> Result<()> {
    let path = resolve_session(id_or_path, all_projects)?;
    let lin = linearize(&path)?;
    let (system, user) = build_summary_prompt(&lin);

    if print_prompt {
        println!("--- SYSTEM ---\n{system}\n\n--- USER ---\n{user}");
        return Ok(());
    }

    let cfg = load()?;
    let model = model.unwrap_or_else(|| cfg.default_chat_model.clone());
    let content = call_summarizer(&cfg, &model, &lin, &system, &user)?;
    if content.ends_with('\n') { print!("{content}"); } else { println!("{content}"); }
    Ok(())
}

fn import_session(
    id_or_path: &str,
    all_projects: bool,
    model: Option<String>,
    provider: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let source_path = resolve_session(id_or_path, all_projects)?;
    let lin = linearize(&source_path)?;
    let target_cwd = lin
        .recorded_cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let (system, user) = build_summary_prompt(&lin);

    let cfg = load()?;
    let model = model.unwrap_or_else(|| cfg.default_chat_model.clone());
    let provider = provider.unwrap_or_else(|| cfg.default_code_provider.clone());
    let summary = call_summarizer(&cfg, &model, &lin, &system, &user)?;

    let tokens_before = estimate_tokens(&user);
    let inputs = PiSessionInputs {
        cwd: &target_cwd,
        source_session_uuid: &lin.session_uuid,
        source_path: &source_path,
        summary: &summary,
        tokens_before,
        provider: Some(provider),
        model_id: Some(cfg.default_code_model.clone()),
        read_files: &lin.read_files,
        modified_files: &lin.modified_files,
    };

    if dry_run {
        println!(
            "would write pi session for cwd={} (tokens_before={}, summary={} chars)",
            target_cwd.display(),
            tokens_before,
            summary.len(),
        );
        return Ok(());
    }

    let written = write_pi_session_file(inputs)?;
    eprintln!(
        "Imported Claude Code session {} → pi session {}.\nOpen it with:\n  \
         libertai code --continue        (from {})\n  \
         libertai code --resume {}",
        lin.session_uuid,
        written.session_id,
        target_cwd.display(),
        written.jsonl_path.display(),
    );
    Ok(())
}

fn call_summarizer(
    cfg: &Config,
    model: &str,
    lin: &LinearizedSession,
    system: &str,
    user: &str,
) -> Result<String> {
    eprintln!(
        "Summarising {} ({} live-branch messages, {} read / {} modified files) via {}...",
        lin.session_uuid,
        lin.messages.len(),
        lin.read_files.len(),
        lin.modified_files.len(),
        model,
    );
    let req = ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage { role: "system".to_string(), content: system.to_string() },
            ChatMessage { role: "user".to_string(), content: user.to_string() },
        ],
        stream: Some(false),
        max_tokens: None,
    };
    let resp = post_chat_blocking(cfg, &req)?;
    let body: serde_json::Value =
        resp.json().context("parsing /v1/chat/completions response")?;
    let content = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .context("response missing choices[0].message.content")?;
    Ok(content.to_string())
}

/// Accept either an absolute/relative path to a `.jsonl` file or a
/// bare session UUID. UUIDs are resolved against the encoded project
/// dir for the current cwd; pass `--all` to widen to every project.
fn resolve_session(id_or_path: &str, all_projects: bool) -> Result<PathBuf> {
    let direct = Path::new(id_or_path);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let cwd = std::env::current_dir()?;
    let candidates = discover(&cwd, all_projects)?;
    for s in &candidates {
        if s.session_uuid == id_or_path {
            return Ok(s.jsonl_path.clone());
        }
    }
    if !all_projects {
        anyhow::bail!(
            "no session with uuid `{id_or_path}` under {} (try --all to scan every project, \
             or pass the full path to a .jsonl)",
            cwd.join(format!(".claude/projects/{}/", encode_project_dir(&cwd))).display(),
        );
    }
    anyhow::bail!("no session with uuid `{id_or_path}` anywhere under $HOME/.claude/projects");
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
