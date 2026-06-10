use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::{self, mask_key};

pub fn run() -> Result<()> {
    let cfg = config::load()?;

    println!("{}", "LibertAI status".bold().underline());
    println!("  {:<22} {}", "API base:".dimmed(), cfg.api_base);
    if cfg.account_base != cfg.api_base {
        println!("  {:<22} {}", "Account base:".dimmed(), cfg.account_base);
    }
    println!(
        "  {:<22} {}",
        "Default chat model:".dimmed(),
        cfg.default_chat_model
    );
    println!(
        "  {:<22} {}",
        "Default code model:".dimmed(),
        cfg.default_code_model
    );
    println!(
        "  {:<22} {}",
        "Default image model:".dimmed(),
        cfg.default_image_model
    );
    println!(
        "  {:<22} {}{}",
        "Smart approvals:".dimmed(),
        if cfg.smart_approval_enabled {
            "enabled".green().to_string()
        } else {
            "disabled".dimmed().to_string()
        },
        if cfg.smart_approval_enabled {
            format!(" ({})", cfg.smart_approval_model)
        } else {
            String::new()
        }
    );
    println!(
        "  {:<22} {} (reserve={}, keep_recent={})",
        "Auto compaction:".dimmed(),
        if cfg.code_auto_compaction_enabled {
            "enabled".green().to_string()
        } else {
            "disabled".dimmed().to_string()
        },
        cfg.code_compaction_reserve_tokens,
        cfg.code_compaction_keep_recent_tokens
    );
    println!(
        "  {:<22} {}",
        "UserPrompt hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.user_prompt_submit)
    );
    println!(
        "  {:<22} {}",
        "PreToolUse hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.pre_tool_use)
    );
    println!(
        "  {:<22} {}",
        "PostToolUse hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.post_tool_use)
    );
    println!(
        "  {:<22} {}",
        "SubagentStop hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.subagent_stop)
    );
    println!(
        "  {:<22} {}",
        "SessionStart hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.session_start)
    );
    println!(
        "  {:<22} {}",
        "Stop hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.stop)
    );
    println!(
        "  {:<22} {}",
        "SessionEnd hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.session_end)
    );
    println!(
        "  {:<22} {}",
        "Notification hooks:".dimmed(),
        runnable_hook_count(&cfg.hooks.notification)
    );

    println!("  {}", "Launcher defaults:".dimmed());
    println!(
        "    {:<20} {}",
        "opus:".dimmed(),
        cfg.launcher_defaults.opus_model
    );
    println!(
        "    {:<20} {}",
        "sonnet:".dimmed(),
        cfg.launcher_defaults.sonnet_model
    );
    println!(
        "    {:<20} {}",
        "haiku:".dimmed(),
        cfg.launcher_defaults.haiku_model
    );

    match cfg.auth.api_key.as_deref() {
        Some(k) => println!("  {:<22} {}", "Auth:".dimmed(), mask_key(k).green()),
        None => println!("  {:<22} {}", "Auth:".dimmed(), "not logged in".red()),
    }

    if let Some(exp) = cfg.auth.expires_at.as_deref() {
        let date = exp.split('T').next().unwrap_or(exp);
        println!(
            "  {:<22} {} {}",
            "Key expires:".dimmed(),
            date,
            "(run `libertai login` to renew)".dimmed()
        );
    }

    if let Some(addr) = cfg.auth.wallet_address.as_deref() {
        let chain = cfg.auth.chain.as_deref().unwrap_or("?");
        println!("  {:<22} {} ({})", "Wallet:".dimmed(), addr, chain);
    }

    Ok(())
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
