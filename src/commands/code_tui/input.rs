//! Input bar: prompt glyph + soft-wrapped editor rows.
//!
//! In Idle and Streaming phases the editor is active and shows the `❯`
//! prompt glyph to its left. In Approval/Ask phases a hint is shown
//! instead.
//!
//! (B4-INPUT-WIDTH) The editor *model* is still `tui_textarea::TextArea`
//! (all key handling goes through it in app.rs), but the bar renders the
//! buffer itself as soft-wrapped visual rows via `input_layout`, the same
//! module `view::draw` sizes the bar with — so the height the footer
//! allocated and the rows drawn here can never disagree. Continuation rows
//! start at the same column as the first row (under the `❯ ` gutter),
//! Claude Code style.
//!
//! (B4-NO-SELECTION-RENDER) Rendering the rows ourselves means
//! tui-textarea's selection highlighting is no longer drawn. The app never
//! starts a selection (no `start_selection` call anywhere), so nothing
//! user-visible is lost.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::commands::code_tui::app::{App, Phase};
use crate::commands::code_tui::input_layout;
use crate::commands::code_tui::theme;

/// Draw the input bar.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if (app.phase == Phase::Idle || app.phase == Phase::Streaming)
        && app.focus == crate::commands::code_tui::app::Focus::Input
    {
        // Split: 2 cols for `❯ ` + rest for the editor rows.
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

        // Empty buffer → placeholder (the raw widget used to draw this).
        if app.textarea.is_empty() {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                "type your message…",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(placeholder, chunks[1]);
            frame.set_cursor_position((chunks[1].x, chunks[1].y));
            return;
        }

        // Wrap layout — same width input as view::draw used for sizing
        // (`area` here IS the footer's input chunk, full frame width).
        let lines_src = app.textarea.lines();
        let layout =
            input_layout::wrap_layout(lines_src, input_layout::input_wrap_width(area.width));
        let (cursor_vrow, cursor_vcol) =
            input_layout::visual_cursor(&layout, lines_src, app.textarea.cursor());

        // Visible window. `app.input_scroll` was clamped in view::draw
        // against this exact layout + height; the min() here is defensive.
        let height = chunks[1].height as usize;
        let scroll = app.input_scroll.min(layout.len().saturating_sub(1));
        let end = (scroll + height.max(1)).min(layout.len());

        let cursor_style = Style::default().bg(Color::Cyan).fg(Color::Black);
        let mut rows: Vec<Line> = Vec::with_capacity(end - scroll);
        for (vidx, vrow) in layout[scroll..end].iter().enumerate() {
            let text = input_layout::row_text(&lines_src[vrow.line_idx], vrow);
            if scroll + vidx == cursor_vrow {
                // Split the row at the cursor char and paint the cell under
                // it, preserving the old block-cursor look
                // (`set_cursor_style` on the widget).
                let cursor_char = app.textarea.cursor().1.saturating_sub(vrow.start_char);
                let before: String = text.chars().take(cursor_char).collect();
                let under: String = text
                    .chars()
                    .nth(cursor_char)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());
                let after: String = text.chars().skip(cursor_char + 1).collect();
                rows.push(Line::from(vec![
                    Span::raw(before),
                    Span::styled(under, cursor_style),
                    Span::raw(after),
                ]));
            } else {
                rows.push(Line::from(Span::raw(text.to_string())));
            }
        }
        frame.render_widget(Paragraph::new(rows), chunks[1]);

        // Position the terminal cursor on the wrapped cell.
        if cursor_vrow >= scroll && cursor_vrow < end {
            let cursor_x = chunks[1].x.saturating_add(cursor_vcol as u16);
            let cursor_y = chunks[1].y.saturating_add((cursor_vrow - scroll) as u16);
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    } else if app.focus == crate::commands::code_tui::app::Focus::Agents {
        // Agent panel is focused — show hint.
        let line = Line::from(vec![
            Span::styled(theme::glyph::USER_PROMPT, theme::bold_accent()),
            Span::raw(" "),
            Span::styled("(browsing agents — tab/esc to return)", theme::muted()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
