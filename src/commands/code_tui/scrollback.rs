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
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Build lines from transcript entries.
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.transcript {
        match entry {
            TranscriptEntry::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled(theme::glyph::USER_PROMPT, theme::bold_accent()),
                    Span::raw(" "),
                    Span::styled(text, theme::bold()),
                ]));
            }
            TranscriptEntry::Assistant(text) => {
                // Split on newlines — each paragraph gets its own Line.
                // The `●` marker only goes on the first paragraph.
                // TODO: integrate ratatui-markdown for rich rendering.
                for (i, para) in text.split('\n').enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(theme::glyph::ASSISTANT_MARKER, theme::bold()),
                            Span::raw(" "),
                            Span::raw(para),
                        ]));
                    } else {
                        lines.push(Line::from(Span::raw(para)));
                    }
                }
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

    // Reserve a 1-column right margin for the scrollbar so it doesn't
    // clobber the last column of wrapped text.
    let para_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    // Render with scroll.
    let paragraph = Paragraph::new(lines).scroll((app.scroll, 0));
    frame.render_widget(paragraph, para_area);

    // Draw scrollbar in the freed rightmost column.
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
