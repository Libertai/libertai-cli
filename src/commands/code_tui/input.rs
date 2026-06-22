//! Input bar: prompt glyph + text buffer + cursor.
//!
//! In Idle phase the input bar is active and shows the `❯` prompt.
//! In Streaming/Approval phase the input bar is dimmed.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::commands::code_tui::app::{App, Phase};
use crate::commands::code_tui::theme;

/// Draw the input bar.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let prompt_glyph = theme::glyph::USER_PROMPT;

    let mut spans = vec![
        Span::styled(prompt_glyph, theme::bold_accent()),
        Span::raw(" "),
    ];

    if app.phase == Phase::Idle {
        spans.push(Span::raw(&app.input_buffer));
    } else {
        // Dimmed — show a hint.
        spans.push(Span::styled("(Ctrl+C to abort)", theme::muted()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // Set cursor position at end of input buffer (Idle only).
    // Use char count, not byte length, so Unicode positions are correct.
    if app.phase == Phase::Idle {
        let cursor_x = area
            .x
            .saturating_add(2)
            .saturating_add(app.input_buffer.chars().count() as u16);
        let cursor_y = area.y;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}
