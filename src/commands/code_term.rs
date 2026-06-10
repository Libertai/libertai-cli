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
use crate::commands::code_ui::clip_chars;

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

/// Serializes interactive keyboard ownership between `code_ui`'s
/// mid-turn input pump (which polls crossterm events so the user can
/// queue messages while the agent streams) and this module's approval
/// micro-prompt. crossterm's event queue is process-global: two threads
/// reading it would steal each other's keys. The approval prompt holds
/// this lock for its whole single-key read; the pump holds it only
/// around each short `poll`+`read`, so an approval acquires ownership
/// within one poll interval and every keystroke after the menu paints
/// answers the menu — never the queue editor.
pub(crate) fn terminal_event_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &GATE
}

/// Width budget when the terminal size is unknown (non-TTY stderr).
const FALLBACK_PROMPT_WIDTH: usize = 100;

/// Current terminal width for the single-line option row / resolution
/// line. Both are erased with `\r ESC[2K`, which only clears one
/// terminal line — anything that wraps would leave stale text behind.
fn prompt_width() -> usize {
    terminal::size()
        .ok()
        .map(|(cols, _)| cols as usize)
        .filter(|cols| *cols > 0)
        .unwrap_or(FALLBACK_PROMPT_WIDTH)
}

/// The `[a]/[A]/[d]` option row with the saved-rule preview clipped so
/// the whole row fits `width` columns and never wraps (the eraser
/// assumes one line). Display-only: the full rule is still what gets
/// persisted on an "always allow" answer.
fn option_row(always_rule: &str, width: usize) -> String {
    // Plain-text glyphs around the rule preview; must match the format
    // string below with the ANSI stripped.
    const PREFIX_PLAIN: &str = "  [a] allow once  [A] always allow (";
    const SUFFIX_PLAIN: &str = ")  [d] deny ";
    let budget = width
        .saturating_sub(PREFIX_PLAIN.chars().count() + SUFFIX_PLAIN.chars().count())
        .max(8);
    let rule = clip_chars(always_rule, budget);
    format!(
        "  \x1b[2m[a]\x1b[0m allow once  \x1b[2m[A]\x1b[0m always allow ({rule})  \x1b[2m[d]\x1b[0m deny "
    )
}

/// [`resolution_line`] clipped to one terminal line (it embeds the same
/// rule label as the option row, so a long rule would wrap it too). The
/// two columns are the caller's leading indent.
fn clipped_resolution_line(choice: &PromptChoice, always_rule: &str, width: usize) -> String {
    clip_chars(
        &resolution_line(choice, always_rule),
        width.saturating_sub(2).max(8),
    )
}

/// Block until the user picks allow/always/deny.
fn prompt(tool_name: &str, preview: &str, always_rule: &str) -> PromptChoice {
    // Take keyboard ownership BEFORE the menu paints: any keystroke
    // typed after the menu is visible must answer the menu, not leak
    // into the mid-turn queue editor.
    let _gate = terminal_event_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut stderr = std::io::stderr();
    let width = prompt_width();

    eprintln!();
    eprintln!("  \x1b[33;1m⎯ tool approval ⎯\x1b[0m");
    eprintln!("  \x1b[1m{tool_name}\x1b[0m");
    for line in preview.lines() {
        eprintln!("  \x1b[2m│\x1b[0m {}", style_preview_line(line));
    }
    eprint!("{}", option_row(always_rule, width));
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
            eprintln!(
                "  \x1b[2m{}\x1b[0m",
                clipped_resolution_line(&choice, always_rule, width)
            );
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
    // scrollback shows what happened rather than the menu. The row was
    // clipped to one terminal line above, so the one-line eraser is
    // guaranteed to remove all of it.
    eprint!("\r\x1b[2K");
    eprintln!(
        "  \x1b[2m{}\x1b[0m",
        clipped_resolution_line(&choice, always_rule, width)
    );
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

    use crate::commands::chat_render::strip_ansi;

    #[test]
    fn option_row_fits_width_and_elides_long_rules() {
        let long_rule = "bash(find . -maxdepth 3 -not -path './target/*' -not -path './.git/*' -name '*.rs' -print)";
        let width = 80;
        let row = option_row(long_rule, width);
        let visible = strip_ansi(&row);
        assert!(
            visible.chars().count() <= width,
            "row wraps at {width} cols ({} chars): {visible:?}",
            visible.chars().count()
        );
        assert!(visible.contains('…'), "long rule not elided: {visible:?}");
        // The menu chrome survives the clipping.
        assert!(visible.starts_with("  [a] allow once  [A] always allow ("));
        assert!(visible.ends_with(")  [d] deny "));
    }

    #[test]
    fn option_row_keeps_short_rules_verbatim() {
        let row = option_row("bash(ls -R)", 100);
        assert_eq!(
            strip_ansi(&row),
            "  [a] allow once  [A] always allow (bash(ls -R))  [d] deny "
        );
    }

    #[test]
    fn option_row_survives_tiny_widths() {
        // Degenerate terminal: the rule budget floors at 8 chars so the
        // row stays parseable even if it can't fit.
        let row = option_row("bash(very long rule here)", 10);
        let visible = strip_ansi(&row);
        assert!(visible.contains("allow once"));
        assert!(visible.contains('…'));
    }

    #[test]
    fn resolution_line_is_clipped_to_one_terminal_line() {
        let long_rule = "bash(find . -maxdepth 3 -not -path './target/*' -not -path './.git/*' -name '*.rs' -print)";
        let width = 60;
        let line = clipped_resolution_line(&PromptChoice::AlwaysAllow, long_rule, width);
        // Printed as "  {line}" → total must fit the terminal width.
        assert!(
            line.chars().count() + 2 <= width,
            "resolution line wraps: {line:?}"
        );
        assert!(line.ends_with('…'));
        // Short resolutions pass through untouched.
        assert_eq!(
            clipped_resolution_line(&PromptChoice::Deny, long_rule, width),
            "✗ denied"
        );
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
