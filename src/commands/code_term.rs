//! Terminal primitives shared by the `libertai code` UI modules.
//!
//! Lives here so both the REPL's input bar (`code_ui.rs`) and the
//! approval micro-prompt (the `TerminalApprovalUi` implementation
//! below) use the same RAII guard — otherwise a panic during an
//! approval prompt would leak raw mode and leave the user's terminal
//! broken.

use std::io::Write;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute, terminal,
};

use crate::commands::code_approvals::{ApprovalUi, NotifyOutcome, PromptChoice};

/// RAII guard that enables raw mode on construction and disables it
/// on drop (including the panic-unwind path).
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode().map_err(|e| anyhow::anyhow!("enable_raw_mode: {e}"))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// RAII guard that enables bracketed paste (`ESC[?2004h`) on
/// construction and disables it (`ESC[?2004l`) on drop — including the
/// panic-unwind path, so a crash mid-edit can't leave the user's shell
/// receiving `ESC[200~`-wrapped pastes.
///
/// Kept separate from [`RawModeGuard`]: the approval micro-prompt wants
/// raw mode for a single keystroke and must *not* capture pastes, while
/// the input bar wants both. Terminals without bracketed-paste support
/// ignore the enable sequence and keep delivering plain key events.
pub struct BracketedPasteGuard;

impl BracketedPasteGuard {
    pub fn enter() -> Result<Self> {
        execute!(std::io::stdout(), EnableBracketedPaste)
            .map_err(|e| anyhow::anyhow!("enable bracketed paste: {e}"))?;
        Ok(Self)
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
}

/// Terminal-flavoured `ApprovalUi`: renders a single-key micro-menu on
/// stderr and reads a keystroke in raw mode. Falls back to a cooked
/// line read on non-TTY stdin (e.g. during automated tests piping in
/// answers).
///
/// `decide` is `async fn` to satisfy the trait, but the body is purely
/// synchronous I/O — pi awaits `Tool::execute` sequentially, so briefly
/// blocking the asupersync executor here does not starve other work
/// on the same session.
pub struct TerminalApprovalUi;

#[async_trait]
impl ApprovalUi for TerminalApprovalUi {
    async fn decide(&self, tool_name: &str, preview: &str, always_rule: &str) -> PromptChoice {
        prompt(tool_name, preview, always_rule)
    }

    async fn notify(&self, title: &str, body: &str) -> NotifyOutcome {
        notify_terminal(title, body)
    }
}

pub(crate) fn notify_terminal(title: &str, body: &str) -> NotifyOutcome {
    let title = title.trim();
    let body = body.trim();
    if title.is_empty() || body.is_empty() {
        return NotifyOutcome::Skipped("EMPTY_NOTIFICATION".to_string());
    }
    eprint!("\x07");
    eprintln!();
    eprintln!("  \x1b[35;1mnotification\x1b[0m \x1b[1m{}\x1b[0m", title);
    for line in body.lines() {
        eprintln!("  \x1b[2m│\x1b[0m {}", line);
    }
    NotifyOutcome::Sent
}

/// One-line summary printed *instead of* an interactive prompt when a
/// persisted allow-rule (chosen via "always allow" in a past session)
/// resolves an approval without user input. Rendered dim by the caller.
pub(crate) fn auto_allowed_line(rule_label: &str) -> String {
    format!("✓ auto-allowed · {rule_label} matches saved rule")
}

/// One-line summary of an interactive approval decision. Replaces the
/// option row after the user answers, so the transcript reads as a
/// clean resolution instead of a menu with the answer glued on.
fn resolution_line(choice: &PromptChoice, always_rule: &str) -> String {
    match choice {
        PromptChoice::Allow => "✓ allowed once".to_string(),
        PromptChoice::AlwaysAllow => format!("✓ always allowed · saved rule {always_rule}"),
        PromptChoice::Deny => "✗ denied".to_string(),
        // Terminal UI never returns Paused (it blocks until the user
        // answers); guard the match for completeness.
        PromptChoice::Paused { .. } => "⏸ paused".to_string(),
    }
}

/// Block until the user picks allow/always/deny.
fn prompt(tool_name: &str, preview: &str, always_rule: &str) -> PromptChoice {
    let mut stderr = std::io::stderr();

    eprintln!();
    eprintln!("  \x1b[33;1m⎯ tool approval ⎯\x1b[0m");
    eprintln!("  \x1b[1m{tool_name}\x1b[0m");
    for line in preview.lines() {
        eprintln!("  \x1b[2m│\x1b[0m {}", style_preview_line(line));
    }
    eprint!("  \x1b[2m[a]\x1b[0m allow once  \x1b[2m[A]\x1b[0m always allow ({always_rule})  \x1b[2m[d]\x1b[0m deny ");
    let _ = stderr.flush();

    // Brief raw-mode single-key read via the shared RAII guard so a
    // panic between enter and disable can't leak raw mode. If raw mode
    // isn't available (e.g. non-TTY), fall back to a cooked-line read.
    let _guard = match RawModeGuard::enter() {
        Ok(g) => g,
        Err(_) => {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let choice = parse_cooked_choice(&line);
            eprintln!();
            eprintln!("  \x1b[2m{}\x1b[0m", resolution_line(&choice, always_rule));
            return choice;
        }
    };
    let choice = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => match (code, modifiers) {
                // `Char('a') + SHIFT` is unreachable on most terminals
                // (Shift uppercases to `A`), but handle it defensively.
                (KeyCode::Char('a'), _) => break PromptChoice::Allow,
                (KeyCode::Char('A'), _) => break PromptChoice::AlwaysAllow,
                (KeyCode::Char('d') | KeyCode::Char('D'), _) => break PromptChoice::Deny,
                (KeyCode::Enter, _) => break PromptChoice::Allow,
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => break PromptChoice::Deny,
                (KeyCode::Esc, _) => break PromptChoice::Deny,
                _ => continue,
            },
            Ok(_) => continue,
            Err(_) => break PromptChoice::Deny,
        }
    };
    drop(_guard);
    // Erase the option row and replace it with the resolution, so the
    // scrollback shows what happened rather than the menu.
    eprint!("\r\x1b[2K");
    eprintln!("  \x1b[2m{}\x1b[0m", resolution_line(&choice, always_rule));
    choice
}

fn parse_cooked_choice(line: &str) -> PromptChoice {
    match line.trim().chars().next().unwrap_or('d') {
        'a' => PromptChoice::Allow,
        'A' => PromptChoice::AlwaysAllow,
        _ => PromptChoice::Deny,
    }
}

fn style_preview_line(line: &str) -> String {
    if line.starts_with("--- ") || line.starts_with("+++ ") {
        return format!("\x1b[36;1m{line}\x1b[0m");
    }
    if line.starts_with('+') {
        return format!("\x1b[32m{line}\x1b[0m");
    }
    if line.starts_with('-') {
        return format!("\x1b[31m{line}\x1b[0m");
    }
    if line.starts_with("... ") && line.ends_with(" lines omitted") {
        return format!("\x1b[2m{line}\x1b[0m");
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_line_styling_highlights_diff_lines() {
        assert_eq!(
            style_preview_line("--- src/lib.rs"),
            "\x1b[36;1m--- src/lib.rs\x1b[0m"
        );
        assert_eq!(
            style_preview_line("+++ proposed/src/lib.rs"),
            "\x1b[36;1m+++ proposed/src/lib.rs\x1b[0m"
        );
        assert_eq!(style_preview_line("+new"), "\x1b[32m+new\x1b[0m");
        assert_eq!(style_preview_line("-old"), "\x1b[31m-old\x1b[0m");
        assert_eq!(
            style_preview_line("... 12 lines omitted"),
            "\x1b[2m... 12 lines omitted\x1b[0m"
        );
        assert_eq!(style_preview_line(" context"), " context");
    }

    #[test]
    fn resolution_lines_replace_the_option_row() {
        assert_eq!(
            resolution_line(&PromptChoice::Allow, "bash(ls -R)"),
            "✓ allowed once"
        );
        assert_eq!(
            resolution_line(&PromptChoice::AlwaysAllow, "bash(ls -R)"),
            "✓ always allowed · saved rule bash(ls -R)"
        );
        assert_eq!(
            resolution_line(&PromptChoice::Deny, "bash(ls -R)"),
            "✗ denied"
        );
    }

    #[test]
    fn auto_allowed_line_names_the_saved_rule() {
        assert_eq!(
            auto_allowed_line("bash(ls -R)"),
            "✓ auto-allowed · bash(ls -R) matches saved rule"
        );
    }

    #[test]
    fn terminal_notifications_report_sent_for_non_empty_payloads() {
        assert_eq!(
            notify_terminal("Done", "Agent turn complete"),
            NotifyOutcome::Sent
        );
        assert_eq!(
            notify_terminal(" ", "Agent turn complete"),
            NotifyOutcome::Skipped("EMPTY_NOTIFICATION".to_string())
        );
    }
}
