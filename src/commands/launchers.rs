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

    // Drop bundled skills (image gen, …) into ~/.claude/skills/ on first run.
    // Safe to call every launch: install_if_missing() leaves existing files alone.
    match crate::commands::skills::install_if_missing(crate::commands::skills::Host::Claude) {
        Ok(n) if n > 0 => eprintln!("claude: installed {n} bundled skill(s)"),
        Ok(_) => {}
        Err(e) => eprintln!("claude: could not install bundled skills: {e:#}"),
    }

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

    let (path, model_count) = sync_opencode_config(&cfg)?;
    eprintln!(
        "opencode: wrote provider \"libertai\" ({model_count} models) to {}",
        path.display()
    );

    // opencode reads Claude-format skills from ~/.claude/skills/, so the same
    // bundled skills (image, search, …) are picked up automatically.
    match crate::commands::skills::install_if_missing(crate::commands::skills::Host::Claude) {
        Ok(n) if n > 0 => eprintln!("opencode: installed {n} bundled skill(s)"),
        Ok(_) => {}
        Err(e) => eprintln!("opencode: could not install bundled skills: {e:#}"),
    }

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

/// Heuristic filter for models that are plausibly usable as chat models in
/// opencode. The `/v1/models` endpoint returns image and embedding models
/// alongside chat ones, and opencode would happily try to send chat requests
/// at them and fail confusingly. This filter errs on the side of including
/// unknown models (chat is the majority case).
fn is_chat_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    const EXCLUDES: &[&str] = &[
        "image",
        "diffusion",
        "sdxl",
        "flux",
        "dall-e",
        "dalle",
        "embed",
        "embedding",
        "whisper",
        "tts",
        "audio",
        "rerank",
    ];
    !EXCLUDES.iter().any(|needle| lower.contains(needle))
}

fn opencode_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config dir"))?;
    Ok(base.join("opencode").join("opencode.json"))
}

/// Idempotently write a `libertai` provider into the user's opencode config.
/// Existing providers and other top-level keys are preserved.
fn sync_opencode_config(cfg: &Config) -> Result<(PathBuf, usize)> {
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

    // Fetch available models from the server. Fall back to the tier defaults
    // if the call fails (offline, stale key, transient 5xx) so `opencode`
    // still launches with *something* that works.
    let tier_defaults = [
        cfg.default_chat_model.clone(),
        cfg.launcher_defaults.opus_model.clone(),
        cfg.launcher_defaults.sonnet_model.clone(),
        cfg.launcher_defaults.haiku_model.clone(),
    ];
    let mut ids: Vec<String> = match crate::client::list_models(cfg) {
        Ok(list) => list
            .data
            .into_iter()
            .map(|m| m.id)
            .filter(|id| is_chat_model(id))
            .collect(),
        Err(e) => {
            eprintln!(
                "opencode: could not list models from {} ({e}); using tier defaults only",
                cfg.api_base
            );
            Vec::new()
        }
    };
    // Always include the tier defaults so `--model libertai/<tier>` resolves
    // even if the server list omits one.
    ids.extend(tier_defaults.iter().cloned());
    ids.sort();
    ids.dedup();

    let mut models = serde_json::Map::new();
    for id in &ids {
        models.insert(
            id.clone(),
            json!({
                "name": id,
                "limit": { "context": 200_000, "output": 16_384 }
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
    Ok((path, ids.len()))
}

pub fn aider(model: Option<String>, mut args: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    let env = base_env(&cfg, model.as_deref())?;

    // Aider has no skills/MCP system — it just reads files passed via --read
    // into context. Synthesize an instructions file with the same guidance
    // the Claude/OpenCode skills provide, and auto-add --read.
    match sync_aider_instructions() {
        Ok(path) => {
            let already_read = args
                .iter()
                .any(|a| a.as_str() == path.to_string_lossy().as_ref());
            if !already_read {
                args.insert(0, "--read".into());
                args.insert(1, path.to_string_lossy().into_owned());
            }
            eprintln!("aider: reading libertai tool docs from {}", path.display());
        }
        Err(e) => eprintln!("aider: could not write libertai instructions: {e:#}"),
    }

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

/// Write `~/.config/libertai/aider-instructions.md` with the same CLI guidance
/// that the Claude/OpenCode skills carry. Overwritten every launch so changes
/// to the bundled skill content propagate.
fn sync_aider_instructions() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config dir"))?;
    let path = base.join("libertai").join("aider-instructions.md");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, AIDER_INSTRUCTIONS)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

const AIDER_INSTRUCTIONS: &str = include_str!("../skills_content/aider-instructions.md");

#[cfg(test)]
mod tests {
    use super::is_chat_model;

    #[test]
    fn filters_image_models() {
        assert!(!is_chat_model("z-image-turbo"));
        assert!(!is_chat_model("stable-diffusion-xl"));
        assert!(!is_chat_model("flux-schnell"));
    }

    #[test]
    fn filters_embedding_and_audio() {
        assert!(!is_chat_model("text-embedding-3-small"));
        assert!(!is_chat_model("whisper-large-v3"));
        assert!(!is_chat_model("tts-1"));
    }

    #[test]
    fn keeps_chat_models() {
        assert!(is_chat_model("qwen3.5-122b-a10b"));
        assert!(is_chat_model("gemma-4-31b-it"));
        assert!(is_chat_model("hermes-3-8b-tee"));
    }
}
