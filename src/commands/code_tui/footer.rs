//! Footer: spinner line, queued-preview lines, and rule (status) line.
//!
//! During a turn the footer shows:
//! ```text
//! ⠋ working…  ●bash(npm run build)
//! › queued message one
//! › queued message two
//! ─ model ─ tokens ─ mode ─ cost ─
//! ```

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::commands::code_tui::app::App;
use crate::commands::code_tui::theme;

/// Draw the spinner line: `⠋ label…  ●tool(detail)`.
pub fn draw_spinner(frame: &mut Frame, area: Rect, app: &App) {
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
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// Draw the rule line (status bar): `─ model ─ tokens ─ mode ─`.
pub fn draw_rule(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();

    // Model label.
    spans.push(Span::styled(
        &app.bar.model_label,
        ratatui::style::Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    ));

    // Token count (if available).
    if app.bar.input_tokens > 0 {
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

    // Context window.
    if app.bar.context_window > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("ctx {}k", app.bar.context_window / 1000),
            theme::muted(),
        ));
    }

    // Estimated cost.
    if let Some(cost) = app.bar.estimated_cost {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("${cost:.2}"), theme::muted()));
    }

    // Mode.
    let mode = app.mode.get();
    let mode_label = match mode {
        crate::commands::code_factory::Mode::Normal => "",
        crate::commands::code_factory::Mode::AcceptEdits => "accept-edits",
        crate::commands::code_factory::Mode::Plan => "plan",
    };
    if !mode_label.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(mode_label, theme::warning()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
