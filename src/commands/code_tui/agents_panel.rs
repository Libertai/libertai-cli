//! Agents panel: live agent list with status icons and prompt previews.
//!
//! Renders below the scrollback transcript, above the spinner.
//! Each row: `○ agent-name  tool-name  prompt preview…`.
//! When focused (Tab), the selected agent is highlighted and
//! navigable with Up/Down/Enter.

use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::commands::code_team::{AgentCapability, AgentHandle, AgentStatus};
use crate::commands::code_tui::theme;
use crate::commands::code_tui::theme::glyph;

/// Draw the agents panel header: `── agents (N) ──`.
pub fn draw_header(frame: &mut Frame, area: Rect, count: usize, focused: bool) {
    let label = if focused {
        format!(" agents ({count}) — ↑↓ select · enter view · esc back ")
    } else {
        format!(" agents ({count}) ")
    };
    let dash_count = area.width.saturating_sub(label.len() as u16) as usize;
    let style = if focused {
        theme::bold_accent()
    } else {
        theme::bold_muted()
    };
    let line = Line::from(vec![
        Span::styled(
            glyph::DIVIDER.to_string().repeat(dash_count),
            if focused {
                theme::accent()
            } else {
                theme::muted()
            },
        ),
        Span::styled(label, style),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw the agent rows. `selected` is the index of the highlighted
/// agent (only when `focused` is true). `scroll_offset` is the number
/// of agents skipped from the top so the selected one is always
/// visible when the list is longer than the available rows.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    agents: &[Arc<AgentHandle>],
    max_rows: usize,
    scroll_offset: usize,
    selected: usize,
    focused: bool,
) {
    let lines: Vec<Line> = agents
        .iter()
        .skip(scroll_offset)
        .take(max_rows)
        .enumerate()
        .map(|(i, handle)| {
            let actual_index = i + scroll_offset;
            let status = handle.status();
            let icon = glyph::status_icon(status);
            let color = theme::agent_color_for(handle.color);

            let mut spans = Vec::new();

            // Selection indicator.
            if focused && actual_index == selected {
                spans.push(Span::styled("▸ ", theme::bold_accent()));
            } else {
                spans.push(Span::raw("  "));
            }

            // Status icon — colored by status.
            let icon_style = match status {
                AgentStatus::Spawning => theme::muted(),
                AgentStatus::Working => theme::accent(),
                AgentStatus::NeedsInput => theme::warning(),
                AgentStatus::Idle => theme::muted(),
                AgentStatus::Completed => theme::success(),
                AgentStatus::Failed => theme::error(),
                AgentStatus::Stopped => theme::muted(),
            };
            spans.push(Span::styled(icon, icon_style));
            spans.push(Span::raw(" "));

            // Write-capable badge.
            if handle.capability == AgentCapability::ReadWrite {
                spans.push(Span::styled(
                    glyph::READ_WRITE_CAP,
                    Style::default().fg(color),
                ));
                spans.push(Span::raw(" "));
            }

            // Agent name — colored by agent color.
            let name_style = if focused && actual_index == selected {
                Style::default().fg(color).add_modifier(
                    ratatui::style::Modifier::BOLD | ratatui::style::Modifier::REVERSED,
                )
            } else {
                Style::default().fg(color)
            };
            spans.push(Span::styled(&handle.name, name_style));

            // Current tool.
            if let Some(tool) = handle.current_tool() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(tool, theme::muted()));
            }

            // Prompt preview — dimmed, right-truncated to fit.
            if !handle.prompt_preview.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(&handle.prompt_preview, theme::muted()));
            }

            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}
