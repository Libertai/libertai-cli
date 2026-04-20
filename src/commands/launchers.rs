use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::commands::run::{base_env, exec_with_env};
use crate::config::{self, Config};

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

pub fn opencode(model: Option<String>, mut args: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    let api_key = crate::client::require_api_key(&cfg)?.to_string();

    let mut env = base_env(&cfg, model.as_deref())?;
    // opencode's openai-compatible adapter reads `apiKey` via {env:LIBERTAI_API_KEY}
    env.push(("LIBERTAI_API_KEY".into(), api_key));

    let path = sync_opencode_config(&cfg)?;
    eprintln!(
        "opencode: wrote provider \"libertai\" to {}",
        path.display()
    );

    // Forward the model selection as `libertai/<model>` unless the user
    // already passed --model / -m themselves.
    let has_model_flag = args.iter().any(|a| a == "--model" || a == "-m");
    if !has_model_flag {
        let chosen = model.unwrap_or_else(|| cfg.default_chat_model.clone());
        args.push("--model".into());
        args.push(format!("libertai/{chosen}"));
    }

    exec_with_env("opencode", &args, env)
}

fn opencode_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config dir"))?;
    Ok(base.join("opencode").join("opencode.json"))
}

/// Idempotently write a `libertai` provider into the user's opencode config.
/// Existing providers and other top-level keys are preserved.
fn sync_opencode_config(cfg: &Config) -> Result<PathBuf> {
    let path = opencode_config_path()?;

    let mut root: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| {
            format!(
                "parsing {} — fix or delete the file and try again",
                path.display()
            )
        })?
    } else {
        json!({ "$schema": "https://opencode.ai/config.json" })
    };

    // Deduplicate known model IDs across chat + launcher tiers.
    let mut ids = vec![
        cfg.default_chat_model.clone(),
        cfg.launcher_defaults.opus_model.clone(),
        cfg.launcher_defaults.sonnet_model.clone(),
        cfg.launcher_defaults.haiku_model.clone(),
    ];
    ids.sort();
    ids.dedup();

    let mut models = serde_json::Map::new();
    for id in &ids {
        models.insert(
            id.clone(),
            json!({
                "name": id,
                "limit": { "context": 32768, "output": 8192 }
            }),
        );
    }

    let provider_entry = json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "LibertAI",
        "options": {
            "baseURL": format!("{}/v1", cfg.api_base.trim_end_matches('/')),
            "apiKey": "{env:LIBERTAI_API_KEY}"
        },
        "models": models
    });

    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} root must be a JSON object", path.display()))?;
    let providers = obj
        .entry("provider")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} `provider` must be a JSON object", path.display()))?;
    providers.insert("libertai".into(), provider_entry);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(&root).context("rendering opencode.json")?;
    std::fs::write(&path, pretty).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
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
