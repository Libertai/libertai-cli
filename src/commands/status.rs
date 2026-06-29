use anyhow::{Context, Result};

use crate::commands::output::Styler;
use crate::config::{self, mask_key, Config};

pub fn run(json: bool) -> Result<()> {
    let cfg = config::load()?;
    if json {
        return print_json(&cfg);
    }
    print_human(&cfg);
    Ok(())
}

fn auth_json(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "logged_in": cfg.auth.api_key.is_some(),
        "has_session": cfg.auth.refresh_token.is_some(),
        "api_key_masked": cfg.auth.api_key.as_deref().map(mask_key),
        "expires_at": cfg.auth.expires_at,
        "wallet_address": cfg.auth.wallet_address,
        "chain": cfg.auth.chain,
    })
}

/// Machine-readable status: auth state, base URLs, defaults. Goes to
/// stdout with nothing else; field names are part of the CLI contract.
fn print_json(cfg: &Config) -> Result<()> {
    let value = serde_json::json!({
        "api_base": cfg.api_base,
        "account_base": cfg.account_base,
        "search_base": cfg.search_base,
        "defaults": {
            "chat_model": cfg.default_chat_model,
            "code_model": cfg.default_code_model,
            "code_provider": cfg.default_code_provider,
            "image_model": cfg.default_image_model,
            "launcher": {
                "opus": cfg.launcher_defaults.opus_model,
                "sonnet": cfg.launcher_defaults.sonnet_model,
                "haiku": cfg.launcher_defaults.haiku_model,
            },
        },
        "auth": auth_json(cfg),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&value).context("rendering status")?
    );
    Ok(())
}

fn print_human(cfg: &Config) {
    let st = Styler::stdout();

    println!("{}", st.heading("LibertAI status"));
    println!("  {:<22} {}", st.dimmed("API base:"), cfg.api_base);
    if cfg.account_base != cfg.api_base {
        println!("  {:<22} {}", st.dimmed("Account base:"), cfg.account_base);
    }
    println!(
        "  {:<22} {}",
        st.dimmed("Default chat model:"),
        cfg.default_chat_model
    );
    println!(
        "  {:<22} {}",
        st.dimmed("Default code model:"),
        cfg.default_code_model
    );
    println!(
        "  {:<22} {}",
        st.dimmed("Default image model:"),
        cfg.default_image_model
    );
    println!(
        "  {:<22} {}{}",
        st.dimmed("Smart approvals:"),
        if cfg.smart_approval_enabled {
            st.green("enabled")
        } else {
            st.dimmed("disabled")
        },
        if cfg.smart_approval_enabled {
            format!(" ({})", cfg.smart_approval_model)
        } else {
            String::new()
        }
    );
    println!(
        "  {:<22} {} (reserve={}, keep_recent={}, budget={})",
        st.dimmed("Auto compaction:"),
        if cfg.code_auto_compaction_enabled {
            st.green("enabled")
        } else {
            st.dimmed("disabled")
        },
        cfg.code_compaction_reserve_tokens,
        cfg.code_compaction_keep_recent_tokens,
        if cfg.code_compaction_token_budget_compact {
            st.yellow("on")
        } else {
            st.dimmed("off")
        }
    );
    println!(
        "  {:<22} {}",
        st.dimmed("UserPrompt hooks:"),
        runnable_hook_count(&cfg.hooks.user_prompt_submit)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("PreToolUse hooks:"),
        runnable_hook_count(&cfg.hooks.pre_tool_use)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("PostToolUse hooks:"),
        runnable_hook_count(&cfg.hooks.post_tool_use)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("SubagentStop hooks:"),
        runnable_hook_count(&cfg.hooks.subagent_stop)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("SessionStart hooks:"),
        runnable_hook_count(&cfg.hooks.session_start)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("Stop hooks:"),
        runnable_hook_count(&cfg.hooks.stop)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("SessionEnd hooks:"),
        runnable_hook_count(&cfg.hooks.session_end)
    );
    println!(
        "  {:<22} {}",
        st.dimmed("Notification hooks:"),
        runnable_hook_count(&cfg.hooks.notification)
    );

    println!("  {}", st.dimmed("Launcher defaults:"));
    println!(
        "    {:<20} {}",
        st.dimmed("opus:"),
        cfg.launcher_defaults.opus_model
    );
    println!(
        "    {:<20} {}",
        st.dimmed("sonnet:"),
        cfg.launcher_defaults.sonnet_model
    );
    println!(
        "    {:<20} {}",
        st.dimmed("haiku:"),
        cfg.launcher_defaults.haiku_model
    );

    match cfg.auth.api_key.as_deref() {
        Some(k) => println!("  {:<22} {}", st.dimmed("Auth:"), st.green(&mask_key(k))),
        None => println!("  {:<22} {}", st.dimmed("Auth:"), st.red("not logged in")),
    }
    println!(
        "  {:<22} {}",
        st.dimmed("Session:"),
        if cfg.auth.refresh_token.is_some() {
            st.green("active")
        } else {
            st.dimmed("none (browser sign-in needed for usage)")
        }
    );

    if let Some(exp) = cfg.auth.expires_at.as_deref() {
        let date = exp.split('T').next().unwrap_or(exp);
        println!(
            "  {:<22} {} {}",
            st.dimmed("Key expires:"),
            date,
            st.dimmed("(run `libertai login` to renew)")
        );
    }

    if let Some(addr) = cfg.auth.wallet_address.as_deref() {
        let chain = cfg.auth.chain.as_deref().unwrap_or("?");
        println!("  {:<22} {} ({})", st.dimmed("Wallet:"), addr, chain);
    }
}

fn runnable_hook_count(hooks: &[crate::config::HookCommandConfig]) -> usize {
    hooks
        .iter()
        .filter(|hook| {
            hook.enabled
                && if hook.hook_type.trim().eq_ignore_ascii_case("http") {
                    !hook.url.trim().is_empty()
                } else if hook.hook_type.trim().eq_ignore_ascii_case("prompt")
                    || hook.hook_type.trim().eq_ignore_ascii_case("agent")
                {
                    !hook.prompt.trim().is_empty()
                } else {
                    let hook_type = hook.hook_type.trim();
                    (hook_type.is_empty() || hook_type.eq_ignore_ascii_case("command"))
                        && !hook.command.trim().is_empty()
                }
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_reports_session_presence() {
        let mut cfg = crate::config::Config::default();
        cfg.auth.refresh_token = Some("rtok".into());
        let v = super::auth_json(&cfg);
        assert_eq!(v["has_session"], true);
        cfg.auth.refresh_token = None;
        let v = super::auth_json(&cfg);
        assert_eq!(v["has_session"], false);
    }

    #[test]
    fn runnable_hook_count_includes_native_handler_types() {
        let hooks = vec![
            crate::config::HookCommandConfig {
                command: "scripts/hook.sh".to_string(),
                ..Default::default()
            },
            crate::config::HookCommandConfig {
                hook_type: "http".to_string(),
                url: "http://127.0.0.1/hook".to_string(),
                ..Default::default()
            },
            crate::config::HookCommandConfig {
                hook_type: "prompt".to_string(),
                prompt: "Review this event.".to_string(),
                ..Default::default()
            },
            crate::config::HookCommandConfig {
                hook_type: "agent".to_string(),
                prompt: "Inspect this event.".to_string(),
                ..Default::default()
            },
            crate::config::HookCommandConfig {
                enabled: false,
                command: "scripts/disabled.sh".to_string(),
                ..Default::default()
            },
            crate::config::HookCommandConfig {
                hook_type: "mcp_tool".to_string(),
                command: "ignored".to_string(),
                ..Default::default()
            },
        ];
        assert_eq!(runnable_hook_count(&hooks), 4);
    }
}
