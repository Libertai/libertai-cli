//! Footer: spinner line, queued-preview lines, and rule (status) line.
//!
//! During a turn the footer shows:
//! ```text
//! ⠋ working…  ●bash(npm run build)
//! › queued message one
//! › queued message two
//! ─ model ─ tokens ─ mode ─ cost ─
//! ```

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::commands::code_factory::Mode;
use crate::commands::code_tui::app::App;
use crate::commands::code_tui::theme;
use crate::commands::code_ui::BarStatus as LegacyBarStatus;
use crate::commands::code_ui::{
    context_percent, expand_status_line_template, status_line_command_text,
};

/// Draw the spinner line: `⠋ label…  ●tool(detail)  · mm:ss  · esc to stop`.
/// Only shown during Streaming phase; blank otherwise.
pub fn draw_spinner(frame: &mut Frame, area: Rect, app: &App) {
    if app.phase != crate::commands::code_tui::app::Phase::Streaming {
        return;
    }

    let spinner = theme::SPINNER_FRAMES[app.spinner_idx];

    let mut spans = vec![
        Span::styled(spinner, theme::dim_accent()),
        Span::raw(" "),
        Span::styled(app.spinner_label, theme::muted()),
    ];

    // If there's a current tool running in the main session, show it.
    if let Some(tool_name) = &app.current_tool {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(theme::glyph::TOOL_MARKER, theme::accent()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(tool_name, theme::bold()));
        if !app.current_tool_detail.is_empty() {
            spans.push(Span::styled(
                format!("({})", app.current_tool_detail),
                theme::muted(),
            ));
        }
    }

    // Live elapsed since the turn started (finding #18), rendered as mm:ss.
    if let Some(start) = app.turn_started {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            fmt_elapsed_compact(start.elapsed()),
            theme::muted(),
        ));
    }

    // Dim esc-to-stop hint during streaming (finding #20).
    spans.push(Span::raw("  "));
    spans.push(Span::styled("· esc to stop", theme::dim_muted()));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Format a duration as compact `m:ss` (or `s` under a minute).
fn fmt_elapsed_compact(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// (B4-INPUT-HINT) Draw the keymap hint shown above the input bar while a
/// multi-line draft is being composed. Style matches the spinner's dim
/// `· esc to stop` hint.
pub fn draw_input_hint(frame: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        "⏎ send · \\⏎ or ctrl+j newline · ctrl+o editor",
        theme::dim_muted(),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw a single queued-preview line: `› text`.
pub fn draw_queued(frame: &mut Frame, area: Rect, text: &str) {
    let line = Line::from(vec![
        Span::styled(theme::glyph::QUEUED, theme::muted()),
        Span::raw(" "),
        Span::styled(text, theme::muted()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw the pinned task-list overlay published by the `todo` tool via
/// `AgentMsg::Todo` (stored on `App::todo`). Renders in place at the top
/// of the footer — repeated `todo` calls UPDATE this block instead of
/// scrolling, matching Claude Code. Replaces the old raw-stderr
/// `eprintln!` render that corrupted the alternate screen.
///
/// Shape:
/// ```text
///   ⎯ task list ⎯
///   ☑  rebuild the parser
///   ■  wire the new event
///   ☐  bench the fallback
/// ```
/// `area` is the full block allocated by `draw_footer` (1 header row +
/// 1 row per item). Truncation is the scrollback's job — the footer
/// height is capped by `compute_footer_height` so this never overflows
/// the terminal.
pub fn draw_todo(frame: &mut Frame, area: Rect, items: &[crate::commands::code_todo::TodoItem]) {
    use crate::commands::code_todo::TodoStatus;

    let mut lines: Vec<Line> = Vec::with_capacity(items.len() + 1);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("⎯ task list ⎯", theme::separator()),
    ]));

    for item in items {
        let (glyph, glyph_style, text_style) = match item.status {
            TodoStatus::Completed => (theme::glyph::CHECKED, theme::success(), theme::dim_muted()),
            TodoStatus::Active => (theme::glyph::ACTIVE, theme::warning(), theme::bold()),
            TodoStatus::Pending => (theme::glyph::UNCHECKED, theme::muted(), theme::muted()),
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(item.text.clone(), text_style),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Draw the rule line (status bar). The status chips are rendered on a
/// full-width separator rule so the line reads as a clean horizontal
/// divider between the transcript and the input bar:
/// `────── model ── ctx 3% ── ~$0.01 ───────`
/// The rule is drawn as an alternating sequence of dash spans and chip
/// spans; the layout is computed from the area width so it always spans
/// the terminal exactly.
pub fn draw_rule(frame: &mut Frame, area: Rect, app: &App) {
    let mode = app.mode.get();

    // Map the TUI BarStatus 1:1 onto the legacy code_ui::BarStatus so the
    // shared template expander can be reused. The two structs now share
    // field names; only cwd/git_branch are legacy-only and left at default.
    let legacy_barstatus = LegacyBarStatus {
        model_label: app.bar.model_label.clone(),
        input_tokens: app.bar.input_tokens,
        context_window: app.bar.context_window,
        output_style: app.bar.output_style.clone(),
        status_line_template: app.bar.status_line_template.clone(),
        status_line_command: app.bar.status_line_command.clone(),
        estimated_cost: app.bar.estimated_cost,
    };

    // Status-line command (highest precedence): `/statusline-command <cmd>`
    // stores a shell command whose stdout replaces the rule line. Render it as
    // a single muted Span line, same as the template branch below. Legacy
    // precedence: command > template > default.
    if !app.bar.status_line_command.is_empty() {
        if let Some(rendered) = status_line_command_text(&app.bar.status_line_command) {
            let line = Line::from(vec![Span::styled(rendered, theme::muted())]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
    }

    // Custom /statusline template overrides the default chips.
    if !app.bar.status_line_template.is_empty() {
        if let Some(rendered) =
            expand_status_line_template(&app.bar.status_line_template, &legacy_barstatus, mode)
        {
            let line = Line::from(vec![Span::styled(rendered, theme::muted())]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
    }

    // Default chips: rendered on a full-width separator rule. Each chip
    // is a styled span sequence; the rule dashes are distributed in the
    // gaps between them.
    let mut chips: Vec<Vec<Span>> = Vec::new();

    // Model label.
    chips.push(vec![Span::styled(
        app.bar.model_label.clone(),
        ratatui::style::Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    )]);

    // Context usage: percentage first (more useful), then the k-count.
    let mut ctx_spans: Vec<Span> = Vec::new();
    if app.bar.context_window > 0 {
        let pct = context_percent(app.bar.input_tokens, app.bar.context_window);
        ctx_spans.push(Span::styled(format!("ctx {pct}%"), theme::muted()));
        // Token k-count alongside the percentage.
        if app.bar.input_tokens >= 1000 {
            ctx_spans.push(Span::styled(
                format!(" · {:.1}k", app.bar.input_tokens as f64 / 1000.0),
                theme::muted(),
            ));
        } else if app.bar.input_tokens > 0 {
            ctx_spans.push(Span::styled(
                format!(" · {}tok", app.bar.input_tokens),
                theme::muted(),
            ));
        }
    } else if app.bar.input_tokens > 0 {
        // No context window known — fall back to a bare token count.
        if app.bar.input_tokens < 1000 {
            ctx_spans.push(Span::styled(
                format!("{}tok", app.bar.input_tokens),
                theme::muted(),
            ));
        } else {
            ctx_spans.push(Span::styled(
                format!("{:.1}k", app.bar.input_tokens as f64 / 1000.0),
                theme::muted(),
            ));
        }
    }
    if !ctx_spans.is_empty() {
        chips.push(ctx_spans);
    }

    // Estimated cost. Mirrors the legacy `~$` semantics (the template
    // expander and `/status` both prefix `~` and suppress $0.00), so the
    // default-chip path does too: a zero session cost renders no chip.
    if let Some(cost) = app.bar.estimated_cost.filter(|c| *c > 0.0) {
        chips.push(vec![Span::styled(format!("~${cost:.2}"), theme::muted())]);
    }

    // Mode.
    let mode_label = match mode {
        Mode::Normal => "",
        Mode::AcceptEdits => "accept-edits",
        Mode::Plan => "plan",
        Mode::Bypass => "bypass",
    };
    if !mode_label.is_empty() {
        chips.push(vec![Span::styled(mode_label, theme::warning())]);
    }

    // cwd chip — basename only; the full path lives in /status.
    if !app.bar.cwd.is_empty() {
        let basename = std::path::Path::new(&app.bar.cwd)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty());
        if let Some(name) = basename {
            chips.push(vec![Span::styled(name.to_string(), theme::muted())]);
        }
    }

    // git branch chip — `git: <branch>` (plain prefix; no branch glyph in
    // the theme yet, so a plain `git:` avoids a missing-glyph box).
    if let Some(branch) = &app.bar.git_branch {
        if !branch.is_empty() {
            chips.push(vec![Span::styled(format!("git: {branch}"), theme::muted())]);
        }
    }

    // Tab hint when agents are present and not already focused.
    let agent_count = app.registry.total_count();
    if agent_count > 0 {
        let hint = match app.focus {
            crate::commands::code_tui::app::Focus::Input => {
                format!(
                    "[tab] {} agent{}",
                    agent_count,
                    if agent_count > 1 { "s" } else { "" }
                )
            }
            crate::commands::code_tui::app::Focus::Agents => "[esc] back to input".to_string(),
        };
        chips.push(vec![Span::styled(hint, theme::accent())]);
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans_on_rule(
            chips,
            area.width as usize,
            theme::glyph::DIVIDER,
            theme::separator(),
        ))),
        area,
    );
}

/// Lay out status chips on a full-width separator rule.
///
/// The chips are separated by dash runs so the whole line spans exactly
/// `width` cells: `──── claude-3 ── ctx 3% ── ~$0.01 ────────`. Dashes are
/// distributed left-to-right in the remaining space; the last leftover
/// dashes are appended on the right so the line is always flush.
pub(crate) fn spans_on_rule(
    chips: Vec<Vec<Span<'static>>>,
    width: usize,
    dash: char,
    rule_style: ratatui::style::Style,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;

    const GAP: usize = 1; // dashes of rule between chips (each side)

    let chip_width = |spans: &[Span]| spans.iter().map(|s| s.content.width()).sum::<usize>();

    let mut chips = chips;
    // Degenerate widths: draw nothing (can't fit a rule).
    if width < 2 || chips.is_empty() {
        return Vec::new();
    }
    // Drop chips that individually overflow the line — better a shorter
    // rule than an overlong one.
    chips.retain(|c| chip_width(c) + 2 <= width);

    let total_chip_w: usize = chips.iter().map(|c| chip_width(c)).sum();
    let n = chips.len();
    let gaps = n + 1;
    let leftover = width.saturating_sub(total_chip_w + gaps * GAP);

    // Spread the leftover dashes evenly across the gaps: each gap gets
    // GAP + leftover/gaps, and the first `leftover % gaps` gaps get one
    // more so the rule spans exactly `width`.
    let gap_len = |i: usize| GAP + leftover / gaps + usize::from(i < leftover % gaps);

    let mut spans: Vec<Span> = Vec::new();
    for (i, chip) in chips.iter().enumerate() {
        spans.push(Span::styled(
            dash.to_string().repeat(gap_len(i)),
            rule_style,
        ));
        spans.extend(chip.iter().cloned());
    }
    spans.push(Span::styled(
        dash.to_string().repeat(gap_len(gaps - 1)),
        rule_style,
    ));
    spans
}
