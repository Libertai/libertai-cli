//! Top-level layout: scrollback (top) + footer (bottom).
//!
//! The footer is pinned to the bottom of the screen; the scrollback
//! fills the remaining space above it. ratatui handles double-buffering,
//! resize, and cursor positioning automatically.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;

use crate::commands::code_team::AgentHandle;
use crate::commands::code_tui::agents_panel;
use crate::commands::code_tui::app::{App, Phase, Focus, SubagentOutcome, TranscriptEntry};
use crate::commands::code_tui::footer;
use crate::commands::code_tui::input;
use crate::commands::code_tui::markdown;
use crate::commands::code_tui::scrollback;
use crate::commands::code_tui::theme;
use crate::commands::code_tui::wrap;

/// Draw the full TUI frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Snapshot agents once per frame — all agents, not just active,
    // so completed/failed ones remain visible.
    let agents = app.registry.snapshot();

    // Compute the agent-row count once from the terminal height (not the
    // footer area height) and share it between height sizing and layout so
    // the two never disagree on the `term_height / 3` denominator
    // (tui-bugs #11).
    let agent_rows = agent_rows(&agents, area.height);

    // Compute footer height from the frame area, not a separate syscall.
    let footer_height = compute_footer_height(agent_rows, &app.queued, area.height);

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
    draw_footer(frame, footer_area, app, &agents, agent_rows);

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

    // Draw diff viewer overlay if active (M7b `/diff`).
    if app.diff_view.is_some() {
        draw_diff_view(frame, area, app);
    }
}

/// Agent row count for the footer's agents panel, clamped to a third of
/// the terminal height. Computed once in [`draw`] and shared with
/// [`compute_footer_height`] (to size the footer) and [`draw_footer`]
/// (to lay out + render the panel) so the two never disagree on the
/// `term_height / 3` denominator (tui-bugs #11).
fn agent_rows(agents: &[Arc<AgentHandle>], term_height: u16) -> u16 {
    if agents.is_empty() {
        0
    } else {
        agents.len().min((term_height / 3) as usize) as u16
    }
}

/// Compute the footer height from the precomputed agent-row count and
/// the snapshot of queued msgs.
///
/// `term_height` is the frame's area height — not a separate syscall.
/// `agent_rows` is computed once by the caller via [`agent_rows`] and
/// passed in so the height and the layout use the same value.
fn compute_footer_height(agent_rows: u16, queued: &[String], term_height: u16) -> u16 {
    let agent_header = if agent_rows > 0 { 1 } else { 0 };
    let queued_rows = queued.len().min(3) as u16;
    // spinner + queued + rule + input
    let base = 1 + queued_rows + 1 + 1;
    let total = agent_header + agent_rows + base;
    total.min(term_height.saturating_sub(1))
}

/// Draw the footer block: agents panel + spinner + queued + rule + input.
///
/// `agent_rows` is the precomputed panel row count (see [`agent_rows`]),
/// sized off the terminal height — not `area.height`, which is the
/// already-clamped footer area and would disagree with the height
/// computation (tui-bugs #11).
fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    agents: &[Arc<AgentHandle>],
    agent_rows: u16,
) {
    let agent_header = if agents.is_empty() { 0 } else { 1 };
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
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(approval) = &app.approval else {
        return;
    };

    // Modal size: 70% width, max 80% height, centered.
    let modal_width = (area.width as f32 * 0.7) as u16;
    let max_modal_height = (area.height as f32 * 0.8) as u16;
    let usable_width = modal_width.saturating_sub(4) as usize;

    // Pre-wrap the preview into explicit Lines (word-wrapped to the
    // usable width). This gives us an exact line count that matches
    // what will be rendered — no relying on Paragraph::wrap, which
    // uses WordWrapper and can produce more lines than a naive char
    // count predicts (words aren't broken mid-word).
    let prefix = "Preview: ";
    let prefix_len = prefix.chars().count();
    let wrapped_preview = wrap::word_wrap(&approval.preview, usable_width, prefix_len);

    // Fixed lines: tool (1) + always_rule (1) + blank (1) + controls (1) = 4.
    // Plus 2 border rows.
    let fixed_inner = 4;
    let preview_lines = wrapped_preview.len();
    let needed_height = 2 + fixed_inner + preview_lines;
    let modal_height = needed_height.min(max_modal_height as usize) as u16;

    // How many preview lines fit without clipping the controls?
    let inner_height = modal_height.saturating_sub(2) as usize;
    let max_preview_lines = inner_height.saturating_sub(fixed_inner).max(1);

    // Truncate preview lines if needed, appending an ellipsis.
    let display_lines: Vec<String> = if wrapped_preview.len() <= max_preview_lines {
        wrapped_preview
    } else {
        let mut out: Vec<String> = wrapped_preview[..max_preview_lines.saturating_sub(1)]
            .to_vec();
        let mut last = wrapped_preview
            .get(max_preview_lines.saturating_sub(1))
            .cloned()
            .unwrap_or_default();
        // Truncate the last line and add ellipsis.
        let max_chars = usable_width.saturating_sub(1);
        if last.chars().count() > max_chars {
            last = last.chars().take(max_chars).collect();
        }
        last.push('…');
        out.push(last);
        out
    };

    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            " Approval ",
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ));

    // Build the content lines. The "Preview: " prefix goes on the
    // first preview line; subsequent lines are plain.
    let mut lines: Vec<Line> = Vec::with_capacity(2 + fixed_inner + display_lines.len());
    lines.push(Line::from(vec![
        Span::styled("Tool: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&approval.tool_name, Style::default().add_modifier(Modifier::BOLD)),
    ]));
    for (i, pl) in display_lines.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::DarkGray)),
                Span::raw(pl.clone()),
            ]));
        } else {
            lines.push(Line::from(Span::raw(pl.clone())));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("Always rule: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&approval.always_rule, Style::default().fg(theme::ACCENT)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[y] Allow  ", Style::default().fg(theme::SUCCESS)),
        Span::styled("[a] Always  ", Style::default().fg(theme::ACCENT)),
        Span::styled("[n] Deny", Style::default().fg(theme::ERROR)),
    ]));

    // No Wrap — lines are already pre-wrapped to fit.
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, modal_area);
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

/// Marker prefix for a per-tool result line — mirrors scrollback's
/// `RESULT_MARKER` (`↳ `) so an overlay result reads as a reply rather
/// than another invocation, matching the main transcript exactly.
const RESULT_MARKER: &str = "↳ ";

/// Max visual lines of a `ToolResult`'s `output` we render before
/// collapsing the rest into a "… N more lines" line — same cap as
/// scrollback's `MAX_RESULT_LINES`, kept in sync for visual parity.
const MAX_RESULT_LINES: usize = 5;

/// Draw the agent output overlay — a near-fullscreen popup showing
/// the selected agent's transcript (text + tool calls + tool results).
///
/// Reuses the scrollback's per-variant styling rather than a flat
/// markdown dump: agent-colored markers, the `↳` result line, and
/// `theme::error` on `is_error`. [`app::agent_transcript`] returns the
/// typed [`TranscriptEntry`] values (for in-process subagents, the
/// `ToolResult` per-tool lines that the previous `Vec<String>` path
/// dropped entirely); [`render_entry_lines`] mirrors the scrollback
/// match arms so the overlay matches the main transcript cell-for-cell.
fn draw_agent_overlay(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use unicode_width::UnicodeWidthStr;

    let Some(overlay) = &app.agent_overlay else {
        return;
    };

    // Look up the agent handle for color.
    let color = app
        .registry
        .find_by_name(&overlay.agent_name)
        .map(|h| theme::agent_color_for(h.color))
        .unwrap_or(theme::MUTED);

    // Collect this agent's transcript as typed entries.
    let entries = crate::commands::code_tui::app::agent_transcript(app, &overlay.agent_name);

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

    // Usable text width inside the bordered overlay: the block's
    // Borders::ALL consumes one column on each side, leaving
    // overlay_width − 2. This is the column budget markdown::render
    // pre-wraps to, so each returned Line is one visual row (code-block
    // lines excepted — they're emitted hard and may overflow, see
    // markdown::render_code_block).
    let usable_width = overlay_width.saturating_sub(2) as usize;

    // Build the content lines, reusing the scrollback's per-variant
    // styling (see [`render_entry_lines`]). A blank separator between
    // entries preserves the prior visual grouping.
    let mut lines: Vec<Line> = Vec::new();
    for entry in &entries {
        lines.extend(render_entry_lines(entry, &overlay.agent_name, color, usable_width));
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no output yet)",
            Style::default().fg(theme::MUTED),
        )));
    }

    // Scroll calculation (same bottom-anchoring as scrollback). The
    // markdown-rendered lines are pre-wrapped to usable_width, so most
    // are exactly one visual row; code-block / blank lines may differ.
    // Count DISPLAY width (not chars) and ceil-divide by usable_width —
    // minimum 1 — so wide glyphs and the non-pre-wrapped lines stay
    // consistent with what Paragraph renders with wrap off.
    let total_visual: usize = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
            if w == 0 {
                1
            } else {
                ((w + usable_width.saturating_sub(1)) / usable_width.max(1)).max(1)
            }
        })
        .sum();
    let inner_height = overlay_height.saturating_sub(2) as usize; // minus borders
    let max_from_top = total_visual.saturating_sub(inner_height);
    let scroll_from_top =
        max_from_top.saturating_sub(overlay.scroll as usize).min(max_from_top);

    // No `.wrap()`: content is already pre-wrapped to usable_width, and
    // leaving wrap off stops ratatui from double-counting (and drifting
    // the scroll position against the row count above).
    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top as u16, 0));
    frame.render_widget(para, overlay_area);
}

/// Draw the in-TUI diff viewer overlay (M7b `/diff`). Cloned from
/// [`draw_agent_overlay`] minus the agent-color/transcript plumbing: the
/// content is the styled unified diff parsed from `app.pending_diff` via
/// [`crate::commands::code_tui::diff::parse_diff`]. Reuses the same
/// centered-rect + `Clear` + rounded `Block` + `Paragraph` + scroll-from-top
/// math so it scrolls identically to the agent overlay.
fn draw_diff_view(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Span;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use unicode_width::UnicodeWidthStr;

    let Some(view) = &app.diff_view else {
        return;
    };

    // Overlay: 80% width, 80% height, centered — same as the agent overlay.
    let overlay_width = (area.width as f32 * 0.8) as u16;
    let overlay_height = (area.height as f32 * 0.8) as u16;
    let overlay_x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let overlay_y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(overlay_x, overlay_y, overlay_width, overlay_height);

    frame.render_widget(Clear, overlay_area);

    // Title: " diff — esc/tab to close " plus the pathspec if one was given.
    let title = match &view.path {
        Some(p) if !p.is_empty() => format!(" diff {p} — esc/tab to close "),
        _ => " diff — esc/tab to close ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme::MUTED))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    // Usable text width inside the bordered overlay (Borders::ALL = 1 col
    // each side). Diff lines are emitted raw (no markdown wrap), so wide
    // lines may overflow — consistent with the agent overlay's code-block
    // behavior.
    let usable_width = overlay_width.saturating_sub(2) as usize;

    let mut lines = crate::commands::code_tui::diff::parse_diff(
        app.pending_diff.as_deref().unwrap_or(""),
    );

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no changes)",
            Style::default().fg(theme::MUTED),
        )));
    }

    // Scroll calculation (same bottom-anchoring as the agent overlay).
    let total_visual: usize = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
            if w == 0 {
                1
            } else {
                ((w + usable_width.saturating_sub(1)) / usable_width.max(1)).max(1)
            }
        })
        .sum();
    let inner_height = overlay_height.saturating_sub(2) as usize; // minus borders
    let max_from_top = total_visual.saturating_sub(inner_height);
    let scroll_from_top = max_from_top
        .saturating_sub(view.scroll as usize)
        .min(max_from_top);

    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top as u16, 0));
    frame.render_widget(para, overlay_area);
}

/// Render a single overlay [`TranscriptEntry`] to styled [`Line`]s,
/// mirroring the scrollback (`scrollback.rs`) match arms so the overlay
/// matches the main transcript cell-for-cell:
/// - `SubagentText` → agent-colored bold name prefix + markdown body.
/// - `SubagentTool` → agent-colored name + `●` marker + bold tool name
///   + muted detail (via `tool_preview`), identical to the scrollback.
/// - `SubagentEnd` → agent-colored name + outcome label
///   (`done`/`failed`/`stopped`), color-coded by outcome.
/// - `ToolResult` → the `↳` result marker, `theme::error` on `is_error`,
///   word-wrapped and capped at [`MAX_RESULT_LINES`] with a "… N more"
///   overflow line.
/// - `System` (background-agent log lines) → dim, one per line.
///
/// `color` is the agent's resolved color (passed in to avoid a per-entry
/// registry lookup). `agent_name` is informational only here since the
/// overlay is already scoped to one agent; the result lines' tool name
/// was already stripped of the `"{agent} · "` prefix by
/// [`app::agent_transcript`].
fn render_entry_lines<'a>(
    entry: &'a TranscriptEntry,
    agent_name: &'a str,
    color: ratatui::style::Color,
    usable_width: usize,
) -> Vec<Line<'a>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    match entry {
        TranscriptEntry::SubagentText { text, .. } => {
            let agent_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            let md_lines = markdown::render(text, usable_width);
            let mut out = Vec::with_capacity(md_lines.len());
            for (i, md_line) in md_lines.into_iter().enumerate() {
                if i == 0 {
                    let mut v = vec![
                        Span::styled(agent_name.to_string(), agent_style),
                        Span::raw(" "),
                    ];
                    v.extend(md_line.spans);
                    out.push(Line::from(v));
                } else {
                    out.push(md_line);
                }
            }
            out
        }
        TranscriptEntry::SubagentTool {
            tool_name, args, ..
        } => {
            // Reuse the shared preview (not re-implemented here) to get
            // the per-tool detail string, then split off the detail so we
            // can color the tool name bold and the detail muted — mirroring
            // the scrollback `SubagentTool` arm exactly.
            let preview =
                crate::commands::code_tool_preview::tool_preview(tool_name, args);
            let detail = preview
                .strip_prefix(tool_name)
                .map(str::trim_start)
                .unwrap_or("");
            let agent_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            let marker_style = Style::default().fg(color);
            let mut spans = vec![
                Span::styled(agent_name.to_string(), agent_style),
                Span::raw(" "),
                Span::styled(theme::glyph::TOOL_MARKER, marker_style),
                Span::raw(" "),
                Span::styled(tool_name.clone(), theme::bold()),
            ];
            if !detail.is_empty() {
                spans.push(Span::styled(format!("({detail})"), theme::muted()));
            }
            vec![Line::from(spans)]
        }
        TranscriptEntry::SubagentEnd { outcome, .. } => {
            let agent_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            let (label, label_style) = match outcome {
                SubagentOutcome::Completed => ("done", theme::success()),
                SubagentOutcome::Failed => ("failed", theme::error()),
                SubagentOutcome::Stopped => ("stopped", theme::muted()),
            };
            vec![Line::from(vec![
                Span::styled(agent_name.to_string(), agent_style),
                Span::raw(" "),
                Span::styled(label, label_style),
            ])]
        }
        TranscriptEntry::ToolResult {
            name,
            output,
            is_error,
        } => {
            let style = if *is_error {
                theme::error()
            } else {
                theme::muted()
            };
            let prefix = format!("{RESULT_MARKER}{name}");
            let prefix_w = prefix.chars().count() + 1; // prefix + ": "
            let body = if output.is_empty() {
                prefix
            } else {
                format!("{prefix}: {output}")
            };
            // Word-wrap to usable_width (M4a pre-wrap parity with scrollback)
            // and cap the visual lines, appending a "… N more" overflow line.
            let wrapped = wrap::word_wrap(&body, usable_width, prefix_w);
            let total = wrapped.len();
            let mut out: Vec<Line> = Vec::with_capacity(wrapped.len().min(MAX_RESULT_LINES) + 1);
            if total <= MAX_RESULT_LINES {
                for chunk in wrapped {
                    out.push(Line::from(Span::styled(chunk, style)));
                }
            } else {
                for chunk in wrapped.into_iter().take(MAX_RESULT_LINES) {
                    out.push(Line::from(Span::styled(chunk, style)));
                }
                let more = total - MAX_RESULT_LINES;
                out.push(Line::from(Span::styled(
                    format!("… {more} more line{}", if more == 1 { "" } else { "s" }),
                    style,
                )));
            }
            out
        }
        // Background-agent log lines: render each raw line dim, matching
        // the scrollback System styling. One TranscriptEntry::System here
        // is exactly one log line (agent_transcript wraps them 1:1), so no
        // extra splitting is needed.
        TranscriptEntry::System(text) => {
            vec![Line::from(Span::styled(text.clone(), theme::muted()))]
        }
        // Other variants aren't produced by agent_transcript; render any
        // stray one as dim text so the match stays exhaustive and never
        // silently drops content.
        _ => vec![Line::from(Span::styled(format!("{entry:?}"), theme::muted()))],
    }
}
