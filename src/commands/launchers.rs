use anyhow::Result;

use crate::commands::run::{base_env, exec_with_env};
use crate::config;

pub fn claude(
    model: Option<String>,
    opus: Option<String>,
    sonnet: Option<String>,
    haiku: Option<String>,
    args: Vec<String>,
) -> Result<()> {
    let cfg = config::load()?;
    let mut env = base_env(&cfg, model.as_deref())?;

    let opus_model = opus
        .or_else(|| model.clone())
        .unwrap_or_else(|| cfg.launcher_defaults.opus_model.clone());
    let sonnet_model = sonnet
        .or_else(|| model.clone())
        .unwrap_or_else(|| cfg.launcher_defaults.sonnet_model.clone());
    let haiku_model = haiku
        .or_else(|| model.clone())
        .unwrap_or_else(|| cfg.launcher_defaults.haiku_model.clone());

    env.extend([
        ("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into()),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ),
        ("DISABLE_TELEMETRY".into(), "1".into()),
        ("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), opus_model),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), sonnet_model),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), haiku_model),
    ]);

    exec_with_env("claude", &args, env)
}

pub fn opencode(model: Option<String>, args: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    let env = base_env(&cfg, model.as_deref())?;
    exec_with_env("opencode", &args, env)
}

pub fn aider(model: Option<String>, mut args: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    let env = base_env(&cfg, model.as_deref())?;

    let has_model_flag = args.iter().any(|a| a == "--model");
    match (model.as_deref(), has_model_flag) {
        (Some(m), _) => {
            args.push("--model".into());
            args.push(format!("openai/{m}"));
        }
        (None, false) => {
            args.push("--model".into());
            args.push(format!("openai/{}", cfg.default_chat_model));
        }
        (None, true) => {}
    }

    exec_with_env("aider", &args, env)
}
