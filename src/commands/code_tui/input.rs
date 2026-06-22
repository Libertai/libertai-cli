//! Input bar: prompt glyph + tui-textarea widget.
//!
//! In Idle phase the textarea is active and shows the `❯` prompt
//! glyph to its left. In Streaming/Approval phase the textarea is
//! dimmed and a hint is shown instead.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::commands::code_tui::app::{App, Phase};
use crate::commands::code_tui::theme;

/// Draw the input bar.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if app.phase == Phase::Idle {
        // Split: 2 cols for `❯ ` + rest for textarea.
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);

        // Prompt glyph.
        let prompt = Paragraph::new(Line::from(Span::styled(
            theme::glyph::USER_PROMPT,
            theme::bold_accent(),
        )));
        frame.render_widget(prompt, chunks[0]);

        // Textarea.
        frame.render_widget(&app.textarea, chunks[1]);

        // Position the terminal cursor inside the textarea.
        let (row, col) = app.textarea.cursor();
        let cursor_x = chunks[1].x.saturating_add(col as u16);
        let cursor_y = chunks[1].y.saturating_add(row as u16);
        frame.set_cursor_position((cursor_x, cursor_y));
    } else {
        // Dimmed — show a hint.
        let line = Line::from(vec![
            Span::styled(theme::glyph::USER_PROMPT, theme::bold_accent()),
            Span::raw(" "),
            Span::styled("(Ctrl+C to abort)", theme::muted()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}
