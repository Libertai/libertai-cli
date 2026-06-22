//! Top-level layout: scrollback (top) + footer (bottom).
//!
//! The footer is pinned to the bottom of the screen; the scrollback
//! fills the remaining space above it. ratatui handles double-buffering,
//! resize, and cursor positioning automatically.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::commands::code_tui::agents_panel;
use crate::commands::code_tui::app::{App, Phase};
use crate::commands::code_tui::footer;
use crate::commands::code_tui::input;
use crate::commands::code_tui::scrollback;
use crate::commands::code_tui::theme;

/// Draw the full TUI frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Compute footer height based on current state.
    let footer_height = compute_footer_height(app);

    // Split: scrollback (variable) + footer (fixed).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),         // scrollback — takes remaining space
            Constraint::Length(footer_height), // footer — pinned to bottom
        ])
        .split(area);

    let scrollback_area = chunks[0];
    let footer_area = chunks[1];

    // Draw scrollback transcript.
    scrollback::draw(frame, scrollback_area, app);

    // Draw footer (agents + spinner + queued + rule + input).
    draw_footer(frame, footer_area, app);

    // Draw approval modal overlay if active.
    if app.phase == Phase::Approval {
        draw_approval_modal(frame, area, app);
    }
}

/// Compute the footer height based on current state.
fn compute_footer_height(app: &App) -> u16 {
    let agents = app.registry.active();
    let agent_rows = agents.len().min((area_height(app) / 3) as usize).max(3) as u16;
    let agent_header = if agents.is_empty() { 0 } else { 1 };
    let queued_rows = app.queued.len().min(3) as u16;
    // spinner + queued + rule + input = 3 + queued_rows
    let base = 1 + queued_rows + 1 + 1; // spinner + queued + rule + input
    let total = agent_header + agent_rows + base;
    total.min(area_height(app).saturating_sub(1))
}

/// Get terminal height from the frame area.
fn area_height(_app: &App) -> u16 {
    crossterm::terminal::size()
        .ok()
        .filter(|(_, h)| *h > 0)
        .map(|(_, h)| h)
        .unwrap_or(24)
}

/// Draw the footer block: agents panel + spinner + queued + rule + input.
fn draw_footer(frame: &mut Frame, area: Rect, app: &mut App) {
    let agents = app.registry.active();
    let agent_rows = agents.len().min((area.height / 3) as usize).max(3) as u16;
    let agent_header = if agents.is_empty() { 0 } else { 1 };
    let queued_rows = app.queued.len().min(3) as u16;
    let spinner_h = 1u16;
    let rule_h = 1u16;
    let input_h = 1u16;

    let constraints: Vec<Constraint> = Vec::new();
    let _ = constraints;

    // Build vertical layout for the footer.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints({
            let mut c = Vec::new();
            if agent_header > 0 {
                c.push(Constraint::Length(agent_header));
            }
            if agent_rows > 0 {
                c.push(Constraint::Length(agent_rows));
            }
            c.push(Constraint::Length(spinner_h));
            for _ in 0..queued_rows {
                c.push(Constraint::Length(1));
            }
            c.push(Constraint::Length(rule_h));
            c.push(Constraint::Length(input_h));
            c
        })
        .split(area);

    let mut chunk_idx = 0;

    // Agent header + agent rows.
    if agent_header > 0 {
        agents_panel::draw_header(frame, chunks[chunk_idx], agents.len());
        chunk_idx += 1;
    }
    if agent_rows > 0 {
        agents_panel::draw(frame, chunks[chunk_idx], &agents, agent_rows as usize);
        chunk_idx += 1;
    }

    // Spinner.
    footer::draw_spinner(frame, chunks[chunk_idx], app);
    chunk_idx += 1;

    // Queued previews.
    for (i, queued_text) in app.queued.iter().take(queued_rows as usize).enumerate() {
        footer::draw_queued(frame, chunks[chunk_idx + i], queued_text);
    }
    chunk_idx += queued_rows as usize;

    // Rule line (status bar).
    footer::draw_rule(frame, chunks[chunk_idx], app);
    chunk_idx += 1;

    // Input bar.
    input::draw(frame, chunks[chunk_idx], app);
}

/// Draw the approval modal as a centered popup.
fn draw_approval_modal(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(approval) = &app.approval else {
        return;
    };

    // Modal size: 60% width, 5 rows tall, centered.
    let modal_width = (area.width as f32 * 0.6) as u16;
    let modal_height = 5u16;
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    // Clear the area under the modal.
    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            " Approval ",
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ));

    let lines = vec![
        Line::from(vec![
            Span::styled("Tool: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&approval.tool_name, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("Preview: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&approval.preview),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[y] Allow  ",
                Style::default().fg(theme::SUCCESS),
            ),
            Span::styled(
                "[a] Always  ",
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(
                "[n] Deny",
                Style::default().fg(theme::ERROR),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, modal_area);
}
