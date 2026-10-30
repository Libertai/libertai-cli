use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::commands::run::{base_env, exec_with_env};
use crate::config::{self, Config};

pub fn claude(
    model: Option<String>,
    opus: Option<String>,
    sonnet: Option<String>,
    fable: Option<String>,
    haiku: Option<String>,
    mut args: Vec<String>,
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
    let fable_model = fable
        .or_else(|| model.clone())
        .unwrap_or_else(|| cfg.launcher_defaults.fable_model.clone());
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
        ("ANTHROPIC_DEFAULT_FABLE_MODEL".into(), fable_model),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), haiku_model),
        // Force an explicit auto-compact window so Claude Code treats the
        // model's context window as `source:"env"` rather than the
        // `source:"auto"` fallback it uses for unknown (non-claude) model ids
        // like our `glm-5.2`. Without this, Claude Code's reactive-compact
        // gate vetoes compaction whenever the window source is "auto" and
        // Anthropic's `tengu_amber_redwood3` server flag is off (which it is,
        // and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` suppresses the flag
        // fetch too). The result: usage hits 100% and auto-compact never
        // fires, so the full context is re-sent every turn — exactly when a
        // backend 503 ("All servers unavailable for model ...") takes down the
        // session. 200000 matches Claude Code's own 200k fallback for unknown
        // models, so this only unblocks the gate; it does not change *when*
        // compaction fires relative to a real 200k model (~167k tokens). The
        // model's real ceiling (~262k) is higher, but the window resolver caps
        // any env value at the model's known context (200k for glm-5.2), so we
        // can't raise it past 200k here anyway. See memory:
        // claude-compact-never-fires.
        ("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "200000".into()),
    ]);

    // The tier vars above remap the `opus`/`sonnet`/`fable`/`haiku` aliases
    // used in `model:` frontmatter and elsewhere, but they do not control
    // how subagents (the Task/Agent tool) pick a model: Claude Code resolves a
    // subagent's model as
    //   CLAUDE_CODE_SUBAGENT_MODEL → per-invocation `model` → the subagent
    //   definition's `model` frontmatter → the main conversation's model.
    // Some agent definitions pin a hardcoded Anthropic id like
    // `claude-opus-4-8`, which emits a model id the LibertAI backend
    // doesn't know, so the subagent fails with "model … doesn't exist" —
    // the "sometimes" failure when `--model` is passed.
    //
    // When the user asks for a single uniform model, force every subagent onto
    // it via CLAUDE_CODE_SUBAGENT_MODEL (precedence 1, above frontmatter and
    // the per-invocation `model` param). We only force subagents when
    // `--model` is given; without it the tier defaults are all valid
    // LibertAI ids and subagents keep their tier differentiation.
    if let Some(m) = &model {
        env.push(("CLAUDE_CODE_SUBAGENT_MODEL".into(), m.clone()));
    }

    // Pin the main-conversation model. The tier env vars above only remap the
    // opus/sonnet/haiku *aliases*; they don't set which model the session
    // starts on. Claude Code resolves that from the `--model` flag, then
    // ~/.claude/settings.json. Since that settings file is shared with the
    // user's real Claude Code, it often pins a real-Anthropic id like
    // "opus[1m]" that the LibertAI backend rejects with "model may not exist".
    // Forward a valid LibertAI model (the user's --model, else the opus tier)
    // unless the user already passed their own --model in the trailing args.
    // /model still switches it mid-session.
    let has_model_flag = args.iter().any(|a| a == "--model" || a == "-m");
    if !has_model_flag {
        let main_model = model
            .clone()
            .unwrap_or_else(|| cfg.launcher_defaults.opus_model.clone());
        args.push("--model".into());
        args.push(main_model);
    }

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

    // Bundled skills (image, search, …) go to opencode's own global skill
    // dir (~/.config/opencode/skills/) so launching opencode never touches
    // the user's ~/.claude/ setup.
    match crate::commands::skills::install_if_missing(crate::commands::skills::Host::OpenCode) {
        Ok(n) if n > 0 => eprintln!("opencode: installed {n} bundled skill(s)"),
        Ok(_) => {}
        Err(e) => eprintln!("opencode: could not install bundled skills: {e:#}"),
    }

    // Set via inline config: opencode 2.x's TUI rejects `--model`.
    let chosen = model.unwrap_or_else(|| cfg.default_code_model.clone());
    env.push((
        "OPENCODE_CONFIG_CONTENT".into(),
        json!({ "model": format!("libertai/{chosen}") }).to_string(),
    ));

    // opencode 2.x otherwise attaches to a shared background service, which
    // never sees this process's env (API key, inline config).
    let picks_server = args.iter().any(|a| a == "--server" || a == "--standalone");
    if !picks_server && opencode_major_version().is_some_and(|v| v >= 2) {
        args.push("--standalone".into());
    }

    exec_with_env("opencode", &args, env)
}

fn opencode_major_version() -> Option<u32> {
    let out = std::process::Command::new("opencode")
        .arg("--version")
        .output()
        .ok()?;
    parse_major_version(&String::from_utf8_lossy(&out.stdout))
}

/// Accepts `1.2.3` and `opencode v2.2.2` style output.
fn parse_major_version(s: &str) -> Option<u32> {
    s.split_whitespace()
        .find_map(|w| w.trim_start_matches('v').split('.').next()?.parse().ok())
}

fn opencode_config_path() -> Result<PathBuf> {
    // opencode reads its global config from $XDG_CONFIG_HOME/opencode (else
    // ~/.config/opencode) on every platform, including macOS — not the
    // Application Support dir that dirs::config_dir() returns there.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .ok_or_else(|| anyhow!("could not determine opencode config dir"))?;
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

    let listed = match crate::client::list_models(cfg) {
        Ok(list) => Some(list.data),
        Err(e) => {
            eprintln!(
                "opencode: could not list models from {} ({e}); keeping existing models",
                cfg.api_base
            );
            None
        }
    };
    let tier_defaults: Vec<&str> = [
        cfg.default_chat_model.as_str(),
        cfg.default_code_model.as_str(),
    ]
    .into_iter()
    .chain(cfg.launcher_defaults.tiers())
    .collect();
    let models = opencode_models(
        listed.as_deref(),
        root.pointer("/provider/libertai/models"),
        &tier_defaults,
    );
    let model_count = models.len();

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
    Ok((path, model_count))
}

/// The `provider.libertai.models` map. `listed` is the `/v1/models` listing,
/// `None` when it couldn't be fetched: the already-registered `existing`
/// entries are then kept as written, so an offline launch neither drops
/// models the user depends on nor degrades their limits/cost to fallbacks.
/// Tier defaults are always present so `--model libertai/<tier>` resolves.
fn opencode_models(
    listed: Option<&[crate::client::ModelEntry]>,
    existing: Option<&Value>,
    tier_defaults: &[&str],
) -> serde_json::Map<String, Value> {
    let mut models = match listed {
        Some(list) => list
            .iter()
            .filter(|m| m.is_chat())
            .map(|m| {
                (
                    m.id.clone(),
                    crate::commands::model_catalog::opencode_model_entry(&m.id, Some(m)),
                )
            })
            .collect(),
        None => existing
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    };
    for &id in tier_defaults {
        models
            .entry(id)
            .or_insert_with(|| crate::commands::model_catalog::opencode_model_entry(id, None));
    }
    models
}

/// Launch Claw Code (ultraworkers/claw-code) pointed at LibertAI.
///
/// Claw rejects bare model names with `invalid_model_syntax` and only routes
/// via `ANTHROPIC_BASE_URL` for names starting with `claude`/`anthropic/`
/// (without stripping the prefix). The working path is its OpenAI-compatible
/// route, reached by prefixing the model with `openai/`. When/if upstream
/// accepts arbitrary model names under `ANTHROPIC_BASE_URL`, this prefix and
/// the whole OpenAI detour can be dropped.
pub fn claw(model: Option<String>, mut args: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    let env = base_env(&cfg, model.as_deref())?;

    let has_model_flag = args.iter().any(|a| a == "--model");
    if !has_model_flag {
        let chosen = model.unwrap_or_else(|| cfg.default_code_model.clone());
        args.push("--model".into());
        args.push(format!("openai/{chosen}"));
    }

    exec_with_env("claw", &args, env)
}

/// Launch Hermes Agent against LibertAI.
///
/// Sets the OpenAI/Anthropic credential env vars (see `base_env`) and exports
/// `LIBERTAI_MODEL` so Hermes picks up the libertai-cli default when the user
/// doesn't pass `--model`.
pub fn hermes(model: Option<String>, args: Vec<String>) -> Result<()> {
    let cfg = config::load()?;
    let chosen = model.unwrap_or_else(|| cfg.default_code_model.clone());
    let env = base_env(&cfg, Some(&chosen))?;
    exec_with_env("hermes", &args, env)
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
            args.push(format!("openai/{}", cfg.default_code_model));
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
    use super::{opencode_models, parse_major_version};
    use crate::client::ModelEntry;
    use serde_json::{json, Value};

    #[test]
    fn parses_opencode_version_output() {
        assert_eq!(parse_major_version("opencode v2.2.2\n"), Some(2));
        assert_eq!(parse_major_version("1.14.3\n"), Some(1));
        assert_eq!(parse_major_version(""), None);
    }

    fn existing() -> Value {
        json!({
            "old-model": {"name": "old-model", "limit": {"context": 1048576, "output": 32768}},
            "glm-5.3": {"name": "glm-5.3", "limit": {"context": 262144, "output": 32768}}
        })
    }

    #[test]
    fn opencode_models_offline_keeps_existing_and_adds_tier_defaults() {
        let models = opencode_models(None, Some(&existing()), &["glm-5.3", "new-tier"]);
        assert_eq!(models.len(), 3);
        assert_eq!(models["old-model"], existing()["old-model"]);
        assert_eq!(models["glm-5.3"], existing()["glm-5.3"]);
        assert_eq!(models["new-tier"]["limit"]["context"], 200_000);
    }

    #[test]
    fn opencode_models_listed_replaces_existing_and_drops_non_chat() {
        let listed: Vec<ModelEntry> = serde_json::from_value(json!([
            {
                "id": "deepseek-v4.1-flash-1017",
                "context_length": 1048576,
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}
            },
            {"id": "z-image-turbo", "name": "Z-Image Turbo"}
        ]))
        .unwrap();
        let models = opencode_models(Some(&listed), Some(&existing()), &["glm-5.3"]);
        let mut ids: Vec<&str> = models.keys().map(String::as_str).collect();
        ids.sort();
        assert_eq!(ids, ["deepseek-v4.1-flash-1017", "glm-5.3"]);
        assert_eq!(
            models["deepseek-v4.1-flash-1017"]["limit"]["context"],
            1_048_576
        );
        // Unlisted tier default: fallback entry, not the stale existing one.
        assert_eq!(models["glm-5.3"]["limit"]["context"], 200_000);
    }
}
