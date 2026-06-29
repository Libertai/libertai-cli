//! Top-level layout: scrollback (top) + footer (bottom).
//!
//! The footer is pinned to the bottom of the screen; the scrollback
//! fills the remaining space above it. ratatui handles double-buffering,
//! resize, and cursor positioning automatically.

use std::sync::Arc;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::commands::code_team::AgentHandle;
use crate::commands::code_tui::agents_panel;
use crate::commands::code_tui::app::{
    slash_palette_filtered, App, Focus, Phase, SubagentOutcome, TranscriptEntry,
};
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

    // (todo-fix) Pinned task-list overlay height: a header row + one row
    // per item, capped to half the terminal so a huge list never eats the
    // whole screen. `None` (no `todo` call yet, or cleared) → 0 rows.
    let todo_rows = app
        .todo
        .as_ref()
        .map(|items| 1 + items.len() as u16)
        .unwrap_or(0)
        .min(area.height / 2);

    // Compute footer height from the frame area, not a separate syscall.
    let footer_height = compute_footer_height(agent_rows, &app.queued, todo_rows, area.height);

    // Split: scrollback (variable) + footer (fixed).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                // scrollback — takes remaining space
            Constraint::Length(footer_height), // footer — pinned to bottom
        ])
        .split(area);

    let scrollback_area = chunks[0];
    let footer_area = chunks[1];

    // Draw scrollback transcript.
    scrollback::draw(frame, scrollback_area, app);

    // Draw footer (agents + spinner + queued + rule + input).
    draw_footer(frame, footer_area, app, &agents, agent_rows, todo_rows);

    // Draw approval modal overlay if active.
    if app.phase == Phase::Approval {
        draw_approval_modal(frame, area, app);
    }

    // Draw ask-user modal overlay if active.
    if app.phase == Phase::Ask {
        draw_ask_modal(frame, area, app);
    }

    // Draw agent output overlay if active. draw_agent_overlay takes &mut App
    // (it splits the borrow to mutate the overlay's log-read cache); the
    // borrow ends when the call returns, so draw_diff_view can borrow next.
    if app.agent_overlay.is_some() {
        draw_agent_overlay(frame, area, app);
    }

    // Draw diff viewer overlay if active (M7b `/diff`).
    if app.diff_view.is_some() {
        draw_diff_view(frame, area, app);
    }

    // Draw tool-result expand overlay if active (M3/#28 `/output`).
    if app.tool_output_view.is_some() {
        draw_tool_output_view(frame, area, app);
    }

    // Draw the slash-command palette (FEATURE-A) LAST so it sits above the
    // footer — matches Claude Code (the input bar stays visible behind the
    // popup). Bottom-anchored, above the footer with a 2-row gap.
    if app.slash_palette.is_some() {
        draw_slash_palette(frame, area, app);
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
///
/// `todo_rows` is the `todo` overlay height — `None` when no task list is
/// pinned (`app.todo == None`), else `1 + items.len()` (a header row plus
/// one row per item). The overlay sits at the TOP of the footer.
fn compute_footer_height(
    agent_rows: u16,
    queued: &[String],
    todo_rows: u16,
    term_height: u16,
) -> u16 {
    let agent_header = if agent_rows > 0 { 1 } else { 0 };
    let queued_rows = queued.len().min(3) as u16;
    // spinner + queued + rule + input
    let base = 1 + queued_rows + 1 + 1;
    let total = todo_rows + agent_header + agent_rows + base;
    total.min(term_height.saturating_sub(1))
}

/// Draw the footer block: agents panel + spinner + queued + rule + input.
///
/// `agent_rows` is the precomputed panel row count (see [`agent_rows`]),
/// sized off the terminal height — not `area.height`, which is the
/// already-clamped footer area and would disagree with the height
/// computation (tui-bugs #11).
///
/// `todo_rows` is the `todo` overlay height (see [`compute_footer_height`]);
/// `0` means no overlay. The overlay renders FIRST (top of the footer).
fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    agents: &[Arc<AgentHandle>],
    agent_rows: u16,
    todo_rows: u16,
) {
    let agent_header = if agent_rows > 0 { 1 } else { 0 };
    let queued_rows = app.queued.len().min(3) as u16;
    let spinner_h = 1u16;
    let rule_h = 1u16;
    let input_h = 1u16;

    // Build vertical layout for the footer.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints({
            let mut c = Vec::new();
            if todo_rows > 0 {
                c.push(Constraint::Length(todo_rows));
            }
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

    // (todo-fix) Pinned task-list overlay at the top of the footer.
    if todo_rows > 0 {
        if let Some(items) = &app.todo {
            footer::draw_todo(frame, chunks[chunk_idx], items);
        }
        chunk_idx += 1;
    }

    // Agent header + agent rows.
    if agent_header > 0 {
        agents_panel::draw_header(
            frame,
            chunks[chunk_idx],
            agents.len(),
            app.focus == Focus::Agents,
        );
        chunk_idx += 1;
    }
    if agent_rows > 0 {
        let max_rows = agent_rows as usize;
        // Scroll offset so the selected agent is always visible.
        let scroll_offset = app
            .agent_selection
            .saturating_sub(max_rows.saturating_sub(1));
        agents_panel::draw(
            frame,
            chunks[chunk_idx],
            agents,
            max_rows,
            scroll_offset,
            app.agent_selection,
            app.focus == Focus::Agents,
        );
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
        let mut out: Vec<String> = wrapped_preview[..max_preview_lines.saturating_sub(1)].to_vec();
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
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    // Build the content lines. The "Preview: " prefix goes on the
    // first preview line; subsequent lines are plain.
    let mut lines: Vec<Line> = Vec::with_capacity(2 + fixed_inner + display_lines.len());
    lines.push(Line::from(vec![
        Span::styled("Tool: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &approval.tool_name,
            Style::default().add_modifier(Modifier::BOLD),
        ),
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
        Span::styled("[s] Session  ", Style::default().fg(theme::WARNING)),
        Span::styled("[a] Always  ", Style::default().fg(theme::ACCENT)),
        // (M4/#10) Per-call scope choices: p=Prefix, r=GrantRoot, o=Domain.
        // Shown dim so the primary y/s/a/n flow stays visually dominant;
        // the keys record an always-rule at the chosen scope (falling back
        // to the default rule when the call has no candidate for it).
        Span::styled(
            "[p] Prefix  [r] Root  [o] Domain  ",
            Style::default().fg(Color::DarkGray),
        ),
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
    // (R5) Clamp before the `u16` cast so a pathological >65535-option ask
    // modal doesn't wrap `content_lines` and corrupt the layout. Saturating
    // add keeps the +4 padding without overflow.
    let modal_height = (content_lines.min(u16::MAX as usize - 4) as u16)
        .saturating_add(4)
        .min(area.height.saturating_sub(2));
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
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
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
        // Use display width (not char count) so CJK/emoji wide glyphs
        // don't leave the cursor mid-glyph (MED-2).
        let cursor_x = inner.x + 2 + (modal.free_text.width() as u16);
        let cursor_y = inner.y + 2;
        frame.set_cursor_position((cursor_x, cursor_y));
    } else {
        // Options list mode.
        let mut lines = vec![Line::from(vec![
            Span::styled("Q: ", Style::default().fg(theme::MUTED)),
            Span::styled(&q.question, Style::default().add_modifier(Modifier::BOLD)),
        ])];
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
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(theme::MUTED),
        )));

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
        // (R5) Clamp before the `u16` cast so a >65535-line header doesn't
        // wrap and corrupt the list area below. The +1 blank separator uses
        // saturating_add to avoid overflow.
        let header_height = (lines.len().min(u16::MAX as usize - 1) as u16).saturating_add(1);
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
fn draw_agent_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    // (MED-6 + R2 perf) Split the App borrow into disjoint field paths so we
    // can hold `&app.registry` + `&app.transcript` immutably AND
    // `&mut app.agent_overlay` (the log-read cache) at once. The borrow
    // checker rejects this through a single `&App`/`&mut App`, but permits it
    // for disjoint fields accessed as separate paths.
    let registry = &app.registry;
    let transcript = &app.transcript;
    let Some(overlay) = app.agent_overlay.as_mut() else {
        return;
    };

    // Look up the agent handle for color.
    let color = registry
        .find_by_name(&overlay.agent_name)
        .map(|h| theme::agent_color_for(h.color))
        .unwrap_or(theme::MUTED);

    // Collect this agent's transcript as typed entries. Goes through the
    // overlay's mtime/size cache so an unchanged log file is NOT re-read on
    // every redraw tick.
    let entries =
        crate::commands::code_tui::app::agent_transcript_for_overlay(registry, transcript, overlay);

    // Overlay: 80% width, 80% height, centered.
    let overlay_width = (area.width as f32 * 0.8) as u16;
    let overlay_height = (area.height as f32 * 0.8) as u16;
    let overlay_x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let overlay_y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(overlay_x, overlay_y, overlay_width, overlay_height);

    frame.render_widget(Clear, overlay_area);

    // Re-borrow overlay immutably for the (read-only) name + scroll math below
    // — the mutable cache borrow above has ended (entries is owned now).
    let overlay = app
        .agent_overlay
        .as_ref()
        .expect("overlay present (checked above)");

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
        lines.extend(render_entry_lines(
            entry,
            &overlay.agent_name,
            color,
            usable_width,
        ));
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no output yet)",
            Style::default().fg(theme::MUTED),
        )));
    }

    // COUNT MODEL: wrap-off + flat count — matches the scrollback. The
    // Paragraph below renders with `.wrap()` OFF, so ratatui 0.30
    // truncates each input `Line` to exactly ONE visual row (never wraps).
    // The row-count model must agree: every `Line` (empty OR over-wide) is
    // exactly one row.  The previous ceil(width/usable_width) count
    // OVER-COUNTED over-wide lines (drifted scroll vs. render, leaving
    // blank rows above the latest content).  Lines that want to wrap are
    // pre-wrapped upstream (`render_entry_lines` → `markdown::render` /
    // `wrap::word_wrap`), so each chunk is its own `Line` and the flat
    // 1-per-Line count is correct for those too.
    let total_visual: usize = lines.len();
    let inner_height = overlay_height.saturating_sub(2) as usize; // minus borders
    let max_from_top = total_visual.saturating_sub(inner_height);
    let scroll_from_top = max_from_top
        .saturating_sub(overlay.scroll as usize)
        .min(max_from_top);

    // No `.wrap()`: content is already pre-wrapped to usable_width, and
    // leaving wrap off stops ratatui from double-counting (and drifting
    // the scroll position against the row count above).
    // (R4) Saturate the `u16` cast so an overlay taller than 65535 visual
    // rows doesn't wrap the offset (`70000 as u16 == 4464`).
    let scroll_from_top_u16 = scroll_from_top.min(u16::MAX as usize) as u16;
    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top_u16, 0));
    frame.render_widget(para, overlay_area);

    // (R3-OVERLAY-SCROLL-NOCLAMP) Pin `max_scroll` to the real scrollable
    // range so the Up key (handle_agent_overlay_key) clamps against it
    // instead of letting `scroll` run past the top. Done AFTER the render:
    // `lines` (built from `&overlay.agent_name`) borrows the overlay
    // immutably until `render_widget` consumes it, so the mutable write must
    // come after. Saturate to `u16::MAX` for a >65535-row overlay.
    if let Some(ov) = app.agent_overlay.as_mut() {
        ov.max_scroll = max_from_top.min(u16::MAX as usize) as u16;
    }
}

/// Draw the in-TUI diff viewer overlay (M7b `/diff`). Cloned from
/// [`draw_agent_overlay`] minus the agent-color/transcript plumbing: the
/// content is the styled unified diff parsed from `app.pending_diff` via
/// [`crate::commands::code_tui::diff::parse_diff`]. Reuses the same
/// centered-rect + `Clear` + rounded `Block` + `Paragraph` + scroll-from-top
/// math so it scrolls identically to the agent overlay.
fn draw_diff_view(frame: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Span;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

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

    // Diff lines are emitted raw (one `Line` per parsed diff line, no
    // markdown wrap), so the flat count below (`lines.len()`) matches the
    // wrap-off truncating renderer exactly; wide lines truncate rather than
    // wrap, consistent with the agent overlay's pre-wrap-off model.

    let mut lines =
        crate::commands::code_tui::diff::parse_diff(app.pending_diff.as_deref().unwrap_or(""));

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no changes)",
            Style::default().fg(theme::MUTED),
        )));
    }

    // COUNT MODEL: wrap-off + flat count — matches the agent overlay +
    // scrollback. The Paragraph below renders with `.wrap()` OFF, so each
    // input `Line` truncates to exactly ONE visual row. Diff lines are
    // emitted raw (one `Line` per parsed diff line, no markdown wrap), so
    // the flat `lines.len()` count matches the truncating renderer exactly.
    // (Wide diff lines truncate rather than wrap — consistent with the
    // agent overlay's code-block behavior.) The previous ceil(width/
    // usable_width) count OVER-COUNTED wide diff lines and drifted the
    // scroll against the render.
    let total_visual: usize = lines.len();
    let inner_height = overlay_height.saturating_sub(2) as usize; // minus borders
    let max_from_top = total_visual.saturating_sub(inner_height);
    let scroll_from_top = max_from_top
        .saturating_sub(view.scroll as usize)
        .min(max_from_top);

    // (R3-OVERLAY-SCROLL-NOCLAMP) Pin `max_scroll` to the real scrollable
    // range so handle_diff_view_key's Up arm clamps against it. The
    // immutable `view` borrow above ended at `scroll_from_top`, so this
    // fresh mutable borrow is borrow-clean. Saturate to `u16::MAX`.
    if let Some(v) = app.diff_view.as_mut() {
        v.max_scroll = max_from_top.min(u16::MAX as usize) as u16;
    }

    // (R4) Saturate the `u16` cast so a diff taller than 65535 visual rows
    // doesn't wrap the offset.
    let scroll_from_top_u16 = scroll_from_top.min(u16::MAX as usize) as u16;
    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top_u16, 0));
    frame.render_widget(para, overlay_area);
}

/// Draw the tool-result expand overlay (M3/#28 `/output`). Cloned from
/// [`draw_diff_view`] minus the diff parser: the content is the
/// un-compacted `full_output` of the currently-selected `ToolResult` entry,
/// rendered as one plain styled `Line` per source line (no markdown wrap so
/// the flat count matches the wrap-off truncating renderer). Up/Down cycle
/// through the collected ToolResult indices (handled in
/// `handle_tool_output_view_key`); the title reports the tool name + which
/// result is in view.
fn draw_tool_output_view(frame: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Span;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(view) = &app.tool_output_view else {
        return;
    };
    // Resolve the currently-selected entry BEFORE mutably borrowing app to
    // pin max_scroll below (mirrors draw_diff_view's borrow split).
    let entry = view
        .indices
        .get(view.pos)
        .and_then(|i| app.transcript.get(*i));
    let (name, full_output, is_error) = match entry {
        Some(TranscriptEntry::ToolResult {
            name,
            full_output,
            is_error,
            ..
        }) => (name.clone(), full_output.clone(), *is_error),
        _ => (String::new(), String::new(), false),
    };

    let overlay_width = (area.width as f32 * 0.8) as u16;
    let overlay_height = (area.height as f32 * 0.8) as u16;
    let overlay_x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let overlay_y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(overlay_x, overlay_y, overlay_width, overlay_height);

    frame.render_widget(Clear, overlay_area);

    let glyph = if is_error { "✗" } else { "✓" };
    let title = format!(
        " output {glyph} {name} — {pos}/{total} · esc/tab to close · ↑↓ cycle ",
        pos = view.pos.saturating_add(1),
        total = view.indices.len(),
    );
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

    // One `Line` per source line of the full output (newlines preserved by
    // `full_tool_output`); empty → "(no output)". Plain styled spans (no
    // markdown) so the flat count matches the wrap-off truncating renderer.
    let body_style = if is_error {
        theme::error()
    } else {
        theme::muted()
    };
    let mut lines: Vec<Line> = if full_output.is_empty() {
        vec![Line::from(Span::styled("(no output)", body_style))]
    } else {
        full_output
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), body_style)))
            .collect()
    };
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("(no output)", body_style)));
    }

    // COUNT MODEL: wrap-off + flat count (mirrors draw_diff_view). Each
    // input `Line` truncates to exactly one visual row.
    let total_visual: usize = lines.len();
    let inner_height = overlay_height.saturating_sub(2) as usize; // minus borders
    let max_from_top = total_visual.saturating_sub(inner_height);
    let scroll_from_top = max_from_top
        .saturating_sub(view.scroll as usize)
        .min(max_from_top);

    if let Some(v) = app.tool_output_view.as_mut() {
        v.max_scroll = max_from_top.min(u16::MAX as usize) as u16;
    }

    let scroll_from_top_u16 = scroll_from_top.min(u16::MAX as usize) as u16;
    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top_u16, 0));
    frame.render_widget(para, overlay_area);
}

/// Draw the slash-command palette (FEATURE-A) — a bottom-anchored popup
/// listing the filtered [`app::slash_palette_entries`] for the current
/// textarea prefix. Modeled on [`draw_agent_overlay`]'s centered-rect +
/// `Clear` + rounded `Block` shape, but bottom-anchored (Claude Code style)
/// with a 2-row gap above the footer and a fixed max height of 10 rows so a
/// long command list never swamps the screen. The selected row is styled
/// `theme::ACCENT` + BOLD + REVERSED, matching the agents-panel selection
/// style. The window scrolls so the selected row stays visible when the
/// filtered list exceeds the visible height (max 7 content rows).
fn draw_slash_palette(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(palette) = &app.slash_palette else {
        return;
    };

    let entries = slash_palette_filtered(app);

    // Palette geometry: 80% width, centered; bottom-anchored with a 2-row
    // gap above the footer. Height is the entry count + 2 border rows, capped
    // at 7 content rows (10 total with borders) so a long list never fills the
    // screen. An empty filtered list still renders a bordered "no matches" box.
    let palette_width = ((area.width as f32) * 0.8) as u16;
    let visible_rows = entries.len().min(7);
    let palette_height = visible_rows.saturating_add(2) as u16;
    let palette_x = area.x + (area.width.saturating_sub(palette_width)) / 2;
    let palette_y = area.height.saturating_sub(palette_height).saturating_sub(2);
    let palette_area = Rect::new(palette_x, palette_y, palette_width, palette_height);

    frame.render_widget(Clear, palette_area);

    let title = " /commands — esc to close ";
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

    let total = entries.len();
    let selected = palette.selected.min(total.saturating_sub(1));

    // Window the selected row into the `visible_rows` window so a long list
    // scrolls to keep the selection on-screen. Start = selected - half, then
    // clamp to [0, total - visible] (the legal range of window starts).
    let visible = visible_rows;
    let half = visible / 2;
    let win_start = if total <= visible {
        0
    } else {
        selected.saturating_sub(half).min(total - visible)
    };

    // Build content lines, styling the selected row with ACCENT+BOLD+REVERSED
    // (matching the agents-panel selection). `format!("{:<20}", name)` keeps
    // the name column left-aligned so descriptions line up.
    let selected_style = Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::REVERSED);
    let name_style = Style::default().fg(theme::PRIMARY);
    let desc_style = Style::default().fg(theme::MUTED);

    let mut lines: Vec<Line> = Vec::new();
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no matching commands)",
            Style::default().fg(theme::MUTED),
        )));
    } else {
        for (i, (name, desc)) in entries.iter().enumerate() {
            if i < win_start || i >= win_start + visible {
                continue;
            }
            let style = if i == selected {
                selected_style
            } else {
                name_style
            };
            // The selected row highlights the WHOLE row (name + desc) so the
            // reverse-video bar reads as a single selection, matching Claude
            // Code. Unselected rows color the name PRIMARY and the desc MUTED.
            if i == selected {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<20}", name), style),
                    Span::styled(desc.clone(), style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<20}", name), style),
                    Span::styled(desc.clone(), desc_style),
                ]));
            }
        }
    }

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, palette_area);
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
            let preview = crate::commands::code_tool_preview::tool_preview(tool_name, args);
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
            full_output: _,
            is_error,
        } => {
            let style = if *is_error {
                theme::error()
            } else {
                theme::muted()
            };
            // (M3/#28) Exit glyph — same `✗`/`✓` cue as the scrollback.
            let glyph = if *is_error { "✗" } else { "✓" };
            let prefix = format!("{RESULT_MARKER}{glyph} {name}");
            let prefix_w = prefix.width() + 2; // prefix + ": " (2 display cols)
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
        // (MED-9) Errors render in the error color — mirrors scrollback's
        // `TranscriptEntry::Error` arm (scrollback.rs:268-270) so the overlay
        // stays cell-for-cell parity with the scrollback it claims. The agent
        // transcript never yields `Error` today (the catch-all below is the
        // latent path), but an explicit arm prevents a silent debug-repr
        // render if `agent_transcript` ever surfaces one.
        TranscriptEntry::Error(text) => {
            vec![Line::from(Span::styled(text.clone(), theme::error()))]
        }
        // Other variants aren't produced by agent_transcript; render any
        // stray one as dim text so the match stays exhaustive and never
        // silently drops content.
        _ => vec![Line::from(Span::styled(
            format!("{entry:?}"),
            theme::muted(),
        ))],
    }
}
