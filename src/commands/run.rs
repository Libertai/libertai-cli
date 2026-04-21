use anyhow::{bail, Result};
#[cfg(not(unix))]
use anyhow::Context;
use std::process::Command;

use crate::config::{self, Config};

pub fn base_env(cfg: &Config, override_model: Option<&str>) -> Result<Vec<(String, String)>> {
    let api_key = crate::client::require_api_key(cfg)?.to_string();

    let v1 = format!("{}/v1", cfg.api_base.trim_end_matches('/'));
    let anthropic_base = cfg.api_base.trim_end_matches('/').to_string();

    let mut env = vec![
        ("OPENAI_API_KEY".into(), api_key.clone()),
        ("OPENAI_BASE_URL".into(), v1.clone()),
        ("OPENAI_API_BASE".into(), v1),
        ("ANTHROPIC_BASE_URL".into(), anthropic_base),
        ("ANTHROPIC_AUTH_TOKEN".into(), api_key),
    ];

    if let Some(m) = override_model {
        env.push(("LIBERTAI_MODEL".into(), m.into()));
    }

    Ok(env)
}

pub fn exec_with_env(program: &str, args: &[String], env: Vec<(String, String)>) -> Result<()> {
    if which(program).is_none() {
        let hint = install_hint(program);
        match hint {
            Some(h) => bail!("{program} not found on PATH — install hint: {h}"),
            None => bail!("{program} not found on PATH"),
        }
    }

    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(anyhow::Error::from(err).context(format!("failed to exec {program}")))
    }

    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .with_context(|| format!("failed to spawn {program}"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        let p = std::path::PathBuf::from(program);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn install_hint(program: &str) -> Option<&'static str> {
    match program {
        "claude" => Some("npm i -g @anthropic-ai/claude-code"),
        "opencode" => Some("npm i -g opencode-ai"),
        "aider" => Some("pipx install aider-install && aider-install"),
        "claw" => Some(
            "build from source: git clone https://github.com/ultraworkers/claw-code \
             && cd claw-code/rust && cargo install --path crates/rusty-claude-cli --force",
        ),
        _ => None,
    }
}

pub fn run(model: Option<String>, argv: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    if argv.is_empty() {
        bail!("no command given");
    }
    let env = base_env(&cfg, model.as_deref())?;
    let program = &argv[0];
    let args = &argv[1..];
    exec_with_env(program, args, env)
}
