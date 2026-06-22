//! Scrollback transcript — renders the conversation history with
//! markdown formatting and a scrollbar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

use crate::commands::code_tui::app::{App, TranscriptEntry};
use crate::commands::code_tui::markdown;
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
                // Render markdown: headings, bold, italic, code, lists, etc.
                // The `●` marker goes on the first rendered line.
                let md_lines = markdown::render(text);
                if md_lines.is_empty() {
                    // Empty assistant text — just show the marker.
                    lines.push(Line::from(vec![
                        Span::styled(theme::glyph::ASSISTANT_MARKER, theme::bold()),
                        Span::raw(" "),
                    ]));
                } else {
                    for (i, md_line) in md_lines.into_iter().enumerate() {
                        if i == 0 {
                            let mut v = vec![
                                Span::styled(theme::glyph::ASSISTANT_MARKER, theme::bold()),
                                Span::raw(" "),
                            ];
                            v.extend(md_line.spans);
                            lines.push(Line::from(v));
                        } else {
                            lines.push(md_line);
                        }
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
            TranscriptEntry::SubagentText { agent_name, text } => {
                // Look up the agent's color from the registry.
                let color = app
                    .registry
                    .find_by_name(agent_name)
                    .map(|h| theme::agent_color_for(h.color))
                    .unwrap_or(theme::MUTED);
                let md_lines = markdown::render(text);
                for (i, md_line) in md_lines.into_iter().enumerate() {
                    if i == 0 {
                        let mut v = vec![
                            Span::styled(agent_name.clone(), ratatui::style::Style::default().fg(color).add_modifier(ratatui::style::Modifier::BOLD)),
                            Span::raw(" "),
                        ];
                        v.extend(md_line.spans);
                        lines.push(Line::from(v));
                    } else {
                        lines.push(md_line);
                    }
                }
            }
            TranscriptEntry::SubagentTool {
                agent_name,
                tool_name,
            } => {
                let color = app
                    .registry
                    .find_by_name(agent_name)
                    .map(|h| theme::agent_color_for(h.color))
                    .unwrap_or(theme::MUTED);
                lines.push(Line::from(vec![
                    Span::styled(agent_name.clone(), ratatui::style::Style::default().fg(color).add_modifier(ratatui::style::Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(theme::glyph::TOOL_MARKER, ratatui::style::Style::default().fg(color)),
                    Span::raw(" "),
                    Span::styled(tool_name, theme::muted()),
                ]));
            }
            TranscriptEntry::SubagentEnd { agent_name } => {
                let color = app
                    .registry
                    .find_by_name(agent_name)
                    .map(|h| theme::agent_color_for(h.color))
                    .unwrap_or(theme::MUTED);
                lines.push(Line::from(Span::styled(
                    format!("{agent_name} done"),
                    ratatui::style::Style::default().fg(color),
                )));
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

    // `app.scroll` is "offset from bottom" (0 = latest).  But
    // `Paragraph::scroll()` expects "offset from top".  Convert:
    //   scroll_from_top = max(0, total_lines − viewport − scroll_from_bottom)
    let total_lines = lines.len();
    let viewport = area.height as usize;
    let max_from_top = total_lines.saturating_sub(viewport);
    let scroll_from_top =
        max_from_top.saturating_sub(app.scroll as usize).min(max_from_top);

    // Render with scroll + wrap.
    let paragraph = Paragraph::new(lines)
        .scroll((scroll_from_top as u16, 0))
        .wrap(Wrap::default());
    frame.render_widget(paragraph, para_area);

    // Draw scrollbar in the freed rightmost column.
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));
    let mut scrollbar_state = ScrollbarState::new(max_from_top)
        .position(scroll_from_top)
        .viewport_content_length(area.height as usize);
    frame.render_stateful_widget(
        scrollbar,
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height),
        &mut scrollbar_state,
    );
}
