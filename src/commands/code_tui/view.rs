//! Top-level layout: scrollback (top) + footer (bottom).
//!
//! The footer is pinned to the bottom of the screen; the scrollback
//! fills the remaining space above it. ratatui handles double-buffering,
//! resize, and cursor positioning automatically.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::commands::code_team::AgentHandle;
use crate::commands::code_tui::agents_panel;
use crate::commands::code_tui::app::{App, Phase, Focus};
use crate::commands::code_tui::footer;
use crate::commands::code_tui::input;
use crate::commands::code_tui::scrollback;
use crate::commands::code_tui::theme;

/// Draw the full TUI frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Snapshot agents once per frame — all agents, not just active,
    // so completed/failed ones remain visible.
    let agents = app.registry.snapshot();

    // Compute footer height from the frame area, not a separate syscall.
    let footer_height = compute_footer_height(&agents, &app.queued, area.height);

    // Split: scrollback (variable) + footer (fixed).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                // scrollback — takes remaining space
            Constraint::Length(footer_height),  // footer — pinned to bottom
        ])
        .split(area);

    let scrollback_area = chunks[0];
    let footer_area = chunks[1];

    // Draw scrollback transcript.
    scrollback::draw(frame, scrollback_area, app);

    // Draw footer (agents + spinner + queued + rule + input).
    draw_footer(frame, footer_area, app, &agents);

    // Draw approval modal overlay if active.
    if app.phase == Phase::Approval {
        draw_approval_modal(frame, area, app);
    }

    // Draw ask-user modal overlay if active.
    if app.phase == Phase::Ask {
        draw_ask_modal(frame, area, app);
    }

    // Draw agent output overlay if active.
    if app.agent_overlay.is_some() {
        draw_agent_overlay(frame, area, app);
    }
}

/// Compute the footer height from the snapshot of agents and queued msgs.
///
/// `term_height` is the frame's area height — not a separate syscall.
fn compute_footer_height(agents: &[Arc<AgentHandle>], queued: &[String], term_height: u16) -> u16 {
    let agent_header = if agents.is_empty() { 0 } else { 1 };
    let agent_rows = if agents.is_empty() {
        0
    } else {
        agents.len().min((term_height / 3) as usize) as u16
    };
    let queued_rows = queued.len().min(3) as u16;
    // spinner + queued + rule + input
    let base = 1 + queued_rows + 1 + 1;
    let total = agent_header + agent_rows + base;
    total.min(term_height.saturating_sub(1))
}

/// Draw the footer block: agents panel + spinner + queued + rule + input.
fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    agents: &[Arc<AgentHandle>],
) {
    let agent_header = if agents.is_empty() { 0 } else { 1 };
    let agent_rows = if agents.is_empty() {
        0
    } else {
        agents.len().min((area.height / 3) as usize) as u16
    };
    let queued_rows = app.queued.len().min(3) as u16;
    let spinner_h = 1u16;
    let rule_h = 1u16;
    let input_h = 1u16;

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
        agents_panel::draw_header(frame, chunks[chunk_idx], agents.len(), app.focus == Focus::Agents);
        chunk_idx += 1;
    }
    if agent_rows > 0 {
        let max_rows = agent_rows as usize;
        // Scroll offset so the selected agent is always visible.
        let scroll_offset = app.agent_selection.saturating_sub(max_rows.saturating_sub(1));
        agents_panel::draw(frame, chunks[chunk_idx], agents, max_rows, scroll_offset, app.agent_selection, app.focus == Focus::Agents);
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
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let Some(approval) = &app.approval else {
        return;
    };

    // Modal size: 70% width, max 80% height, centered.
    let modal_width = (area.width as f32 * 0.7) as u16;
    let max_modal_height = (area.height as f32 * 0.8) as u16;
    let usable_width = modal_width.saturating_sub(4) as usize;

    // Count wrapped lines for preview — account for both explicit
    // newlines and word-wrap at the usable width.
    let preview_line_count: usize = approval
        .preview
        .lines()
        .map(|line| {
            let chars = line.chars().count();
            if chars == 0 {
                1
            } else {
                ((chars + usable_width.saturating_sub(1)) / usable_width.max(1)).max(1)
            }
        })
        .sum();

    // Fixed content lines: tool (1) + preview-prefix-line (1) + always_rule (1)
    // + blank (1) + controls (1) = 5. Plus 2 border rows = 7.
    let fixed_lines = 7;
    let needed_height = fixed_lines + preview_line_count.saturating_sub(1);
    let modal_height = needed_height.min(max_modal_height as usize) as u16;

    // How many preview lines fit without clipping the controls?
    let inner_height = modal_height.saturating_sub(2) as usize; // minus borders
    let reserved_lines = 4; // tool + always_rule + blank + controls
    let max_preview_lines = inner_height.saturating_sub(reserved_lines).max(1);

    // Truncate the preview to fit, appending an ellipsis if cut.
    let truncated_preview = truncate_preview(&approval.preview, max_preview_lines, usable_width);

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
            Span::raw(&truncated_preview),
        ]),
        Line::from(vec![
            Span::styled("Always rule: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&approval.always_rule, Style::default().fg(theme::ACCENT)),
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

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap::default());
    frame.render_widget(paragraph, modal_area);
}

/// Truncate a multi-line preview to at most `max_lines` visual lines
/// (after word-wrapping at `width`), appending `…` if truncated.
fn truncate_preview(preview: &str, max_lines: usize, width: usize) -> String {
    let mut visual_lines = 0usize;
    let mut out = String::new();
    for line in preview.lines() {
        let wrapped = if line.chars().count() == 0 {
            1
        } else {
            ((line.chars().count() + width.saturating_sub(1)) / width.max(1)).max(1)
        };
        if visual_lines + wrapped > max_lines {
            // Fill remaining lines with the start of this line.
            let remaining = max_lines.saturating_sub(visual_lines);
            if remaining > 0 {
                let chars_per_line = width.max(1);
                let take_chars = remaining * chars_per_line;
                let truncated: String = line.chars().take(take_chars).collect();
                out.push_str(&truncated);
                out.push('…');
            } else {
                out.push('…');
            }
            break;
        }
        out.push_str(line);
        out.push('\n');
        visual_lines += wrapped;
    }
    out.trim_end_matches('\n').to_string()
}

/// Draw the ask-user modal as a centered popup.
fn draw_ask_modal(frame: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

    let Some(modal) = app.ask.as_mut() else {
        return;
    };

    let q = modal.current_question().clone();
    let total = modal.questions.len();
    let current = modal.current + 1;

    // Compute modal height based on content.
    let content_lines = if modal.free_text_mode {
        4 // question + blank + input + hint
    } else {
        q.options.len() + 4 // question + optional header + hint + blank + options
    };
    let modal_height = (content_lines as u16 + 4).min(area.height.saturating_sub(2));
    let modal_width = (area.width as f32 * 0.7) as u16;
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let title = format!(" Question {current}/{total} ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            title,
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    if modal.free_text_mode {
        // Free-text input mode.
        let lines = vec![
            Line::from(vec![
                Span::styled("Q: ", Style::default().fg(theme::MUTED)),
                Span::styled(&q.question, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("❯ ", theme::bold_accent()),
                Span::raw(&modal.free_text),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "[enter] submit  [esc] cancel",
                Style::default().fg(theme::MUTED),
            )),
        ];
        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
        // Set cursor position.
        let cursor_x = inner.x + 2 + modal.free_text.chars().count() as u16;
        let cursor_y = inner.y + 2;
        frame.set_cursor_position((cursor_x, cursor_y));
    } else {
        // Options list mode.
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Q: ", Style::default().fg(theme::MUTED)),
                Span::styled(&q.question, Style::default().add_modifier(Modifier::BOLD)),
            ]),
        ];
        if !q.header.is_empty() {
            lines.push(Line::from(Span::styled(
                &q.header,
                Style::default().fg(theme::ACCENT),
            )));
        }

        let hint = if q.multi_select {
            "↑↓ move · space toggle · enter confirm · esc cancel"
        } else {
            "↑↓ move · 1-9 pick · enter confirm · esc cancel"
        };
        lines.push(Line::from(Span::styled(hint, Style::default().fg(theme::MUTED))));

        // Build option items.
        let items: Vec<ListItem> = q
            .options
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                let marker = if modal.selected.contains(&i) {
                    "◆ "
                } else {
                    "○ "
                };
                let mut spans = vec![
                    Span::styled(marker, Style::default().fg(theme::ACCENT)),
                    Span::raw(&opt.label),
                ];
                if let Some(desc) = &opt.description {
                    spans.push(Span::styled(
                        format!(" — {desc}"),
                        Style::default().fg(theme::MUTED),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        // Render the header (question + optional header + hint),
        // then the list below it.
        let header_height = lines.len() as u16 + 1; // +1 for blank separator
        let header_area = Rect {
            height: header_height,
            ..inner
        };
        let header_para = Paragraph::new(lines);
        frame.render_widget(header_para, header_area);

        let list_area = Rect {
            y: header_area.y + header_area.height,
            height: inner.height.saturating_sub(header_area.height),
            ..inner
        };
        let list = List::new(items)
            .style(Style::default())
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, list_area, &mut modal.list_state);
    }
}

/// Draw the agent output overlay — a near-fullscreen popup showing
/// the selected agent's transcript (text + tool calls).
fn draw_agent_overlay(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let Some(overlay) = &app.agent_overlay else {
        return;
    };

    // Look up the agent handle for color.
    let color = app
        .registry
        .find_by_name(&overlay.agent_name)
        .map(|h| theme::agent_color_for(h.color))
        .unwrap_or(theme::MUTED);

    // Collect this agent's transcript.
    let agent_lines = crate::commands::code_tui::app::agent_transcript(app, &overlay.agent_name);

    // Overlay: 80% width, 80% height, centered.
    let overlay_width = (area.width as f32 * 0.8) as u16;
    let overlay_height = (area.height as f32 * 0.8) as u16;
    let overlay_x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let overlay_y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(overlay_x, overlay_y, overlay_width, overlay_height);

    frame.render_widget(Clear, overlay_area);

    let title = format!(" {} — esc/tab to close ", overlay.agent_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));

    // Build the content lines.
    let mut lines: Vec<Line> = Vec::new();
    for text in &agent_lines {
        for line in text.lines() {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no output yet)",
            Style::default().fg(theme::MUTED),
        )));
    }

    // Scroll calculation (same bottom-anchoring as scrollback).
    let usable_width = overlay_width.saturating_sub(4) as usize;
    let total_visual: usize = lines
        .iter()
        .map(|l| {
            let chars: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            if chars == 0 {
                1
            } else {
                ((chars + usable_width.saturating_sub(1)) / usable_width.max(1)).max(1)
            }
        })
        .sum();
    let inner_height = overlay_height.saturating_sub(2) as usize; // minus borders
    let max_from_top = total_visual.saturating_sub(inner_height);
    let scroll_from_top =
        max_from_top.saturating_sub(overlay.scroll as usize).min(max_from_top);

    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top as u16, 0))
        .wrap(Wrap::default());
    frame.render_widget(para, overlay_area);
}
