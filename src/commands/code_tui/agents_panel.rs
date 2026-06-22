//! Agents panel: live agent list with status icons and prompt previews.
//!
//! Renders below the scrollback transcript, above the spinner.
//! Each row: `○ agent-name  tool-name  prompt preview…`.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::commands::code_team::{AgentCapability, AgentHandle, AgentStatus};
use crate::commands::code_tui::theme;
use crate::commands::code_tui::theme::glyph;

/// Draw the agents panel header: `── agents (N) ──`.
pub fn draw_header(frame: &mut Frame, area: Rect, count: usize) {
    let label = format!(" agents ({count}) ");
    let dash_count = area
        .width
        .saturating_sub(label.len() as u16) as usize;
    let line = Line::from(vec![
        Span::styled(
            glyph::DIVIDER.to_string().repeat(dash_count),
            theme::muted(),
        ),
        Span::styled(label, theme::bold_muted()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw the agent rows.
pub fn draw(frame: &mut Frame, area: Rect, agents: &[Arc<AgentHandle>], max_rows: usize) {
    let lines: Vec<Line> = agents
        .iter()
        .take(max_rows)
        .map(|handle| {
            let status = handle.status();
            let icon = glyph::status_icon(status);
            let color = theme::agent_color_for(handle.color);

            let mut spans = Vec::new();

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
            spans.push(Span::styled(&handle.name, Style::default().fg(color)));

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

