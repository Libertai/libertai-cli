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

use crate::commands::code_approvals::{ApprovalUi, AskOutcome, NotifyOutcome, PromptChoice};
use crate::commands::code_ui::{clip_chars, sanitize_terminal_preview_text};

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

    async fn resume_decide(&self, _request_id: &str, payload: serde_json::Value) -> PromptChoice {
        let Some(tool_name) = payload
            .get("tool_name")
            .or_else(|| payload.get("name"))
            .and_then(|v| v.as_str())
        else {
            return PromptChoice::Deny;
        };
        let preview = payload
            .get("preview")
            .or_else(|| payload.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let always_rule = payload
            .get("always_rule")
            .or_else(|| payload.get("rule"))
            .and_then(|v| v.as_str())
            .unwrap_or(tool_name);
        prompt(tool_name, preview, always_rule)
    }

    async fn ask(&self, payload: serde_json::Value) -> AskOutcome {
        ask_user(&payload)
    }

    async fn resume_ask(&self, _request_id: &str, payload: serde_json::Value) -> AskOutcome {
        let payload = payload.get("questions").unwrap_or(&payload);
        ask_user(payload)
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
    let _gate = terminal_event_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::commands::code_ui::suspend_active_footer();
    eprint!("\x07");
    eprintln!();
    eprintln!("  \x1b[35;1mnotification\x1b[0m \x1b[1m{}\x1b[0m", title);
    for line in body.lines() {
        eprintln!("  \x1b[2m│\x1b[0m {}", line);
    }
    crate::commands::code_ui::resume_active_footer();
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
    // Pause the live spinner footer so its ticker thread can't repaint
    // over this menu. Without this the menu was overwritten and the
    // agent looked hung while it waited for an unseen keystroke.
    crate::commands::code_ui::suspend_active_footer();
    let mut stderr = std::io::stderr();
    let width = prompt_width();

    eprintln!();
    eprintln!("  \x1b[33;1m⎯ tool approval ⎯\x1b[0m");
    eprintln!("  \x1b[1m{tool_name}\x1b[0m");
    let preview = sanitize_terminal_preview_text(preview);
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
            crate::commands::code_ui::resume_active_footer();
            return choice;
        }
    };
    let choice = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => match prompt_choice_for_key(code, modifiers) {
                Some(choice) => break choice,
                None => continue,
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
    crate::commands::code_ui::resume_active_footer();
    choice
}

fn parse_cooked_choice(line: &str) -> PromptChoice {
    match line.trim().chars().next().unwrap_or('d') {
        'a' => PromptChoice::Allow,
        'A' => PromptChoice::AlwaysAllow,
        _ => PromptChoice::Deny,
    }
}

fn prompt_choice_for_key(code: KeyCode, modifiers: KeyModifiers) -> Option<PromptChoice> {
    match (code, modifiers) {
        // `Char('a') + SHIFT` is unreachable on most terminals
        // (Shift uppercases to `A`), but handle it defensively.
        (KeyCode::Char('a'), _) => Some(PromptChoice::Allow),
        (KeyCode::Char('A'), _) => Some(PromptChoice::AlwaysAllow),
        (KeyCode::Char('d') | KeyCode::Char('D'), _) => Some(PromptChoice::Deny),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(PromptChoice::Deny),
        (KeyCode::Esc, _) => Some(PromptChoice::Deny),
        _ => None,
    }
}

fn style_preview_line(line: &str) -> String {
    let line = sanitize_terminal_preview_text(line);
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
    line
}

/// Build the `{ cancelled: true, reason }` envelope the `ask_user` tool
/// turns into a tool result the LLM can adapt to.
fn ask_cancelled(reason: &str) -> AskOutcome {
    AskOutcome::Answer(serde_json::json!({
        "cancelled": true,
        "reason": reason,
    }))
}

/// One parsed option: a short label and optional clarifying text.
struct AskOption {
    label: String,
    description: Option<String>,
}

/// Parse the `options` array of a single question into [`AskOption`]s,
/// dropping any entry without a string `label`.
fn parse_ask_options(question: &serde_json::Value) -> Vec<AskOption> {
    question
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    let label =
                        sanitize_terminal_preview_text(o.get("label").and_then(|v| v.as_str())?);
                    let description = o
                        .get("description")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(sanitize_terminal_preview_text);
                    Some(AskOption { label, description })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Assemble the per-question answer object documented in
/// `code_ask_user`: `{ header, selected: [labels], other: text|null }`.
fn ask_answer(header: &str, selected: Vec<String>, other: Option<String>) -> serde_json::Value {
    serde_json::json!({
        "header": header,
        "selected": selected,
        "other": other,
    })
}

/// Interactive terminal implementation of the `ask_user` tool: renders
/// each question's options as a single-key/arrow-navigable chooser and
/// returns the structured answers. Blocks the runtime thread for the
/// duration (pi awaits `Tool::execute` sequentially, same contract as
/// the approval prompt). Esc / Ctrl-C cancels the whole stack.
fn ask_user(payload: &serde_json::Value) -> AskOutcome {
    let Some(questions) = payload.get("questions").and_then(|q| q.as_array()) else {
        return ask_cancelled("USER_DECLINED");
    };
    if questions.is_empty() {
        return ask_cancelled("USER_DECLINED");
    }

    // Own the keyboard for the whole exchange and pause the spinner
    // footer so it can't repaint over the cards (same hang-avoidance as
    // the approval prompt).
    let _gate = terminal_event_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::commands::code_ui::suspend_active_footer();

    eprintln!();
    eprintln!("  \x1b[36;1m⎯ a question for you ⎯\x1b[0m");

    let mut answers = Vec::with_capacity(questions.len());
    for question in questions {
        match ask_one(question) {
            Some(answer) => answers.push(answer),
            None => {
                crate::commands::code_ui::resume_active_footer();
                return ask_cancelled("USER_DECLINED");
            }
        }
    }

    crate::commands::code_ui::resume_active_footer();
    AskOutcome::Answer(serde_json::json!({ "answers": answers }))
}

/// Render and resolve one question. Returns `None` if the user cancels.
fn ask_one(question: &serde_json::Value) -> Option<serde_json::Value> {
    let header = question
        .get("header")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let header = sanitize_terminal_preview_text(header);
    let text = question
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let text = sanitize_terminal_preview_text(text);
    let multi = question
        .get("multiSelect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let options = parse_ask_options(question);

    eprintln!();
    if !header.is_empty() {
        eprintln!("  \x1b[1m{header}\x1b[0m");
    }
    if !text.is_empty() {
        let width = prompt_width();
        for line in text.lines() {
            eprintln!("  {}", clip_chars(line, width.saturating_sub(2).max(8)));
        }
    }

    // No options → free-form only.
    if options.is_empty() {
        let answer = ask_free_text()?;
        return Some(ask_answer(&header, Vec::new(), Some(answer)));
    }

    if multi {
        eprintln!("  \x1b[2m↑/↓ move · space toggle · enter confirm · esc cancel\x1b[0m");
    } else {
        eprintln!("  \x1b[2m↑/↓ move · 1-9 / enter pick · esc cancel\x1b[0m");
    }

    let selection = select_options(&options, multi)?;

    let mut selected = Vec::new();
    let mut other = None;
    for idx in selection {
        let label = &options[idx].label;
        if label.eq_ignore_ascii_case("other") {
            // The "Other" option means the user wants to type a custom
            // answer; collect it as free-form text.
            other = Some(ask_free_text()?);
        } else {
            selected.push(label.clone());
        }
    }
    Some(ask_answer(&header, selected, other))
}

/// Render the option list for [`select_options`] as one terminal-ready
/// string (lines joined by `\r\n`, no trailing newline so the caller can
/// erase exactly `rows` rows). `cursor` is highlighted; in multi-select
/// mode `checked` rows show a filled box.
fn render_ask_options(
    options: &[AskOption],
    cursor: usize,
    checked: &[bool],
    multi: bool,
) -> String {
    let width = prompt_width();
    let mut lines = Vec::with_capacity(options.len());
    for (i, opt) in options.iter().enumerate() {
        let pointer = if i == cursor { "›" } else { " " };
        let mark = if multi {
            if checked.get(i).copied().unwrap_or(false) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let opt_label = sanitize_terminal_preview_text(&opt.label);
        let mut label = format!("{}. {}", i + 1, opt_label);
        if let Some(desc) = &opt.description {
            let desc = sanitize_terminal_preview_text(desc);
            label.push_str(&format!(" — {desc}"));
        }
        let body = format!("  {pointer} {mark}{label}");
        let body = clip_chars(&body, width);
        // Bold the row under the cursor; dim the rest.
        let styled = if i == cursor {
            format!("\x1b[1m{body}\x1b[0m")
        } else {
            format!("\x1b[2m{body}\x1b[0m")
        };
        lines.push(styled);
    }
    lines.join("\r\n")
}

/// Drive the option chooser. Returns the chosen option indices (one for
/// single-select, zero or more for multi-select) or `None` on cancel.
/// Falls back to a cooked numbered read when raw mode is unavailable
/// (non-TTY stdin, e.g. piped tests).
fn select_options(options: &[AskOption], multi: bool) -> Option<Vec<usize>> {
    let n = options.len();
    let _guard = match RawModeGuard::enter() {
        Ok(g) => g,
        Err(_) => return select_options_cooked(options, multi),
    };

    let mut cursor = 0usize;
    let mut checked = vec![false; n];
    let mut prev_rows = 0usize;

    let chosen = loop {
        let block = render_ask_options(options, cursor, &checked, multi);
        let rows = block.matches("\r\n").count() + 1;
        let mut out = String::new();
        if prev_rows > 0 {
            // Move to the top of the previous block and clear downward.
            if prev_rows > 1 {
                out.push_str(&format!("\x1b[{}A", prev_rows - 1));
            }
            out.push_str("\r\x1b[0J");
        }
        out.push_str(&block);
        eprint!("{out}");
        let _ = std::io::stderr().flush();
        prev_rows = rows;

        match event::read() {
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => match (code, modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), _) => {
                    cursor = if cursor == 0 { n - 1 } else { cursor - 1 };
                }
                (KeyCode::Down | KeyCode::Char('j'), _) => {
                    cursor = (cursor + 1) % n;
                }
                (KeyCode::Char(c), _) if c.is_ascii_digit() && c != '0' => {
                    let idx = c as usize - '1' as usize;
                    if idx < n {
                        if multi {
                            checked[idx] = !checked[idx];
                            cursor = idx;
                        } else {
                            break Some(vec![idx]);
                        }
                    }
                }
                (KeyCode::Char(' '), _) if multi => {
                    checked[cursor] = !checked[cursor];
                }
                (KeyCode::Enter, _) => {
                    if multi {
                        break Some((0..n).filter(|i| checked[*i]).collect());
                    }
                    break Some(vec![cursor]);
                }
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break None,
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    // Leave the resolved list on screen and move past it.
    eprintln!();
    chosen
}

/// Cooked fallback for [`select_options`]: prints a numbered list and
/// reads 1-based indices from a line (comma/space separated for
/// multi-select). Empty / EOF input cancels.
fn select_options_cooked(options: &[AskOption], multi: bool) -> Option<Vec<usize>> {
    for (i, opt) in options.iter().enumerate() {
        match &opt.description {
            Some(desc) => eprintln!("  {}. {} — {desc}", i + 1, opt.label),
            None => eprintln!("  {}. {}", i + 1, opt.label),
        }
    }
    if multi {
        eprint!("  pick (e.g. 1,3): ");
    } else {
        eprint!("  pick (1-{}): ", options.len());
    }
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }
    let picks: Vec<usize> = line
        .split([',', ' ', '\t'])
        .filter_map(|tok| tok.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= options.len())
        .map(|n| n - 1)
        .collect();
    if picks.is_empty() {
        return None;
    }
    if multi {
        Some(picks)
    } else {
        Some(vec![picks[0]])
    }
}

/// Read a single free-form answer line. Prefer raw key handling because
/// this can run while the mid-turn input pump has stdin in cbreak/no-echo
/// mode; cooked `read_line` would not echo or edit correctly there.
fn ask_free_text() -> Option<String> {
    eprint!("  \x1b[2myour answer:\x1b[0m ");
    let _ = std::io::stderr().flush();
    if let Ok(_guard) = RawModeGuard::enter() {
        return ask_free_text_raw();
    }
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }
    Some(line.trim().to_string())
}

fn ask_free_text_raw() -> Option<String> {
    let mut answer = String::new();
    loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => match (code, modifiers) {
                (KeyCode::Enter, _) => {
                    eprintln!();
                    return Some(answer.trim().to_string());
                }
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    eprintln!();
                    return None;
                }
                (KeyCode::Backspace, _) => {
                    if answer.pop().is_some() {
                        eprint!("\x08 \x08");
                        let _ = std::io::stderr().flush();
                    }
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    if !c.is_control() {
                        answer.push(c);
                        eprint!("{c}");
                        let _ = std::io::stderr().flush();
                    }
                }
                _ => {}
            },
            Ok(Event::Paste(data)) => {
                let safe = sanitize_terminal_preview_text(&data)
                    .replace(['\n', '\t'], " ")
                    .trim()
                    .to_string();
                if !safe.is_empty() {
                    answer.push_str(&safe);
                    eprint!("{safe}");
                    let _ = std::io::stderr().flush();
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
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
    fn preview_line_styling_strips_terminal_sequences_before_styling() {
        assert_eq!(
            style_preview_line("\x1b[31m+new\x1b[0m\x1b]0;title\x07"),
            "\x1b[32m+new\x1b[0m"
        );
        assert_eq!(style_preview_line("plain\x07\x7f"), "plain");
    }

    #[test]
    fn approval_prompt_keys_require_explicit_choice() {
        assert_eq!(
            prompt_choice_for_key(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(PromptChoice::Allow)
        );
        assert_eq!(
            prompt_choice_for_key(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Some(PromptChoice::AlwaysAllow)
        );
        assert_eq!(
            prompt_choice_for_key(KeyCode::Char('d'), KeyModifiers::NONE),
            Some(PromptChoice::Deny)
        );
        assert_eq!(
            prompt_choice_for_key(KeyCode::Enter, KeyModifiers::NONE),
            None
        );
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

    fn ask_answer_envelope(outcome: &AskOutcome) -> serde_json::Value {
        match outcome {
            AskOutcome::Answer(v) => v.clone(),
            AskOutcome::Paused { .. } => panic!("terminal ask never pauses"),
        }
    }

    #[test]
    fn ask_user_cancels_on_missing_or_empty_questions() {
        let no_questions = ask_answer_envelope(&ask_user(&serde_json::json!({})));
        assert_eq!(no_questions["cancelled"], serde_json::json!(true));
        assert_eq!(no_questions["reason"], serde_json::json!("USER_DECLINED"));

        let empty = ask_answer_envelope(&ask_user(&serde_json::json!({ "questions": [] })));
        assert_eq!(empty["cancelled"], serde_json::json!(true));
    }

    #[test]
    fn parse_ask_options_keeps_labels_and_drops_blank_descriptions() {
        let q = serde_json::json!({
            "options": [
                { "label": "Keep", "description": "leave it in" },
                { "label": "Strip", "description": "   " },
                { "description": "no label" },
            ]
        });
        let opts = parse_ask_options(&q);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Keep");
        assert_eq!(opts[0].description.as_deref(), Some("leave it in"));
        assert_eq!(opts[1].label, "Strip");
        assert_eq!(opts[1].description, None);
    }

    #[test]
    fn parse_ask_options_sanitizes_display_text() {
        let q = serde_json::json!({
            "options": [
                { "label": "\u{1b}[31mKeep\u{1b}[0m", "description": "desc\u{7}\u{1b}]0;x\u{7}" },
            ]
        });
        let opts = parse_ask_options(&q);
        assert_eq!(opts[0].label, "Keep");
        assert_eq!(opts[0].description.as_deref(), Some("desc"));
    }

    #[test]
    fn render_ask_options_outputs_sanitized_text() {
        let options = vec![AskOption {
            label: "\x1b[31mAlpha\x1b[0m".to_string(),
            description: Some("Beta\x1b]0;x\x07".to_string()),
        }];
        let block = render_ask_options(&options, 0, &[false], false);
        let plain = strip_ansi(&block);
        assert!(plain.contains("Alpha — Beta"));
        assert!(!plain.contains('\x1b'));
    }

    #[test]
    fn ask_answer_shapes_the_documented_envelope() {
        let answer = ask_answer("Cleanup", vec!["Strip".to_string()], None);
        assert_eq!(answer["header"], serde_json::json!("Cleanup"));
        assert_eq!(answer["selected"], serde_json::json!(["Strip"]));
        assert_eq!(answer["other"], serde_json::json!(null));

        let custom = ask_answer("Name", Vec::new(), Some("widget".to_string()));
        assert_eq!(custom["selected"], serde_json::json!([]));
        assert_eq!(custom["other"], serde_json::json!("widget"));
    }

    #[test]
    fn rendered_options_mark_cursor_and_checks() {
        let options = vec![
            AskOption {
                label: "Alpha".to_string(),
                description: None,
            },
            AskOption {
                label: "Beta".to_string(),
                description: Some("second".to_string()),
            },
        ];
        let checked = vec![false, true];
        let block = render_ask_options(&options, 1, &checked, true);
        let plain = strip_ansi(&block);
        let lines: Vec<&str> = plain.split("\r\n").collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("[ ] 1. Alpha"));
        assert!(lines[1].contains("› [x] 2. Beta — second"));
    }
}
