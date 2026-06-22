//! Scrollback transcript — renders the conversation history with
//! markdown formatting and a scrollbar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::commands::code_tui::app::{App, TranscriptEntry};
use crate::commands::code_tui::theme;

/// Draw the scrollback transcript.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    // Build lines from transcript entries.
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.transcript {
        match entry {
            TranscriptEntry::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        theme::glyph::USER_PROMPT,
                        theme::bold_accent(),
                    ),
                    Span::raw(" "),
                    Span::styled(text, theme::bold()),
                ]));
            }
            TranscriptEntry::Assistant(text) => {
                // For now, render as plain text with the ● marker.
                // TODO: integrate ratatui-markdown for rich rendering.
                lines.push(Line::from(vec![
                    Span::styled(
                        theme::glyph::ASSISTANT_MARKER,
                        theme::bold(),
                    ),
                    Span::raw(" "),
                    Span::raw(text),
                ]));
            }
            TranscriptEntry::Tool { name, detail } => {
                if detail.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(theme::glyph::TOOL_MARKER, theme::accent()),
                        Span::raw(" "),
                        Span::styled(name, theme::bold()),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(theme::glyph::TOOL_MARKER, theme::accent()),
                        Span::raw(" "),
                        Span::styled(name, theme::bold()),
                        Span::styled(format!("({detail})"), theme::muted()),
                    ]));
                }
            }
            TranscriptEntry::AutoAllowed(text) => {
                lines.push(Line::from(Span::styled(text, theme::muted())));
            }
            TranscriptEntry::System(text) => {
                lines.push(Line::from(Span::styled(text, theme::muted())));
            }
            TranscriptEntry::Blank => {
                lines.push(Line::from(""));
            }
        }
    }

    // Render with scroll.
    let paragraph = Paragraph::new(lines).scroll((app.scroll, 0));
    frame.render_widget(paragraph, area);

    // Draw scrollbar on the right edge.
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));
    let mut scrollbar_state = ScrollbarState::new(app.transcript.len())
        .position(app.scroll as usize)
        .viewport_content_length(area.height as usize);
    frame.render_stateful_widget(
        scrollbar,
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height),
        &mut scrollbar_state,
    );
}
