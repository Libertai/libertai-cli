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
        Span::styled("⎯ task list ⎯", theme::dim_muted()),
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

/// Draw the rule line (status bar): `─ model ─ tokens ─ mode ─ cost ─`.
///
/// When a `/statusline` template is configured (`app.bar.status_line_template`
/// non-empty), the expanded template replaces the default spans. Otherwise the
/// default chips are shown: model, context-% + token k-count, cost, mode,
/// cwd basename, git branch, and the agent tab hint.
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

    let mut spans = Vec::new();

    // Model label.
    spans.push(Span::styled(
        &app.bar.model_label,
        ratatui::style::Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    ));

    // Context usage: percentage first (more useful), then the k-count.
    if app.bar.context_window > 0 {
        let pct = context_percent(app.bar.input_tokens, app.bar.context_window);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("ctx {pct}%"), theme::muted()));
        // Token k-count alongside the percentage.
        if app.bar.input_tokens >= 1000 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{:.1}k", app.bar.input_tokens as f64 / 1000.0),
                theme::muted(),
            ));
        } else if app.bar.input_tokens > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{}tok", app.bar.input_tokens),
                theme::muted(),
            ));
        }
    } else if app.bar.input_tokens > 0 {
        // No context window known — fall back to a bare token count.
        spans.push(Span::raw("  "));
        if app.bar.input_tokens < 1000 {
            spans.push(Span::styled(
                format!("{}tok", app.bar.input_tokens),
                theme::muted(),
            ));
        } else {
            spans.push(Span::styled(
                format!("{:.1}k", app.bar.input_tokens as f64 / 1000.0),
                theme::muted(),
            ));
        }
    }

    // Estimated cost. Mirrors the legacy `~$` semantics (the template
    // expander and `/status` both prefix `~` and suppress $0.00), so the
    // default-chip path does too: a zero session cost renders no chip.
    if let Some(cost) = app.bar.estimated_cost.filter(|c| *c > 0.0) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("~${cost:.2}"), theme::muted()));
    }

    // Mode.
    let mode_label = match mode {
        Mode::Normal => "",
        Mode::AcceptEdits => "accept-edits",
        Mode::Plan => "plan",
        Mode::Bypass => "bypass",
    };
    if !mode_label.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(mode_label, theme::warning()));
    }

    // cwd chip — basename only; the full path lives in /status.
    if !app.bar.cwd.is_empty() {
        let basename = std::path::Path::new(&app.bar.cwd)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty());
        if let Some(name) = basename {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("· {name}"), theme::muted()));
        }
    }

    // git branch chip — `· git: <branch>` (plain prefix; no branch glyph in
    // the theme yet, so a plain `git:` avoids a missing-glyph box).
    if let Some(branch) = &app.bar.git_branch {
        if !branch.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("· git: {branch}"), theme::muted()));
        }
    }

    // Tab hint when agents are present and not already focused.
    let agent_count = app.registry.total_count();
    if agent_count > 0 {
        spans.push(Span::raw("  "));
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
        spans.push(Span::styled(hint, theme::accent()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
