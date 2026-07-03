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
    mention_popup_filtered, slash_palette_filtered, App, Focus, Phase, SubagentOutcome,
    TranscriptEntry,
};
use crate::commands::code_tui::footer;
use crate::commands::code_tui::input;
use crate::commands::code_tui::input_layout;
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

    // (B3 Fix 5 / B4-INPUT-WIDTH) The input bar grows one row per *visual*
    // (soft-wrapped) row of the draft, capped at `MAX_INPUT_ROWS`, so a
    // multi-line or long-wrapping draft is visible instead of clipped or
    // horizontally scrolled. The wrap layout is computed ONCE here from the
    // same width `input::draw` uses, so height and render can never
    // disagree.
    let input_wrap = input_layout::wrap_layout(
        app.textarea.lines(),
        input_layout::input_wrap_width(area.width),
    );
    let input_lines = input_wrap.len() as u16;

    // (B3 Fix 6) Compute the footer's per-component heights ONCE, with a
    // documented degradation priority so the constraint sum can never exceed
    // the footer Rect and silently collapse the input row. `draw_footer`
    // consumes the SAME struct, so the height and the layout never disagree.
    // (B4-INPUT-HINT) Show the keymap hint row while composing multi-line.
    let input_hint = input_lines > 1;

    let footer_layout = compute_footer_layout(
        agent_rows,
        &app.queued,
        todo_rows,
        input_lines,
        input_hint,
        area.height,
    );
    let footer_height = footer_layout.total();

    // (B4-INPUT-SCROLL) Update the input viewport scroll against the SAME
    // `input_h` the layout just allocated, so the cursor is always inside
    // the rows `input::draw` will actually render this frame.
    let (cursor_vrow, _) =
        input_layout::visual_cursor(&input_wrap, app.textarea.lines(), app.textarea.cursor());
    app.input_scroll = input_layout::clamp_input_scroll(
        app.input_scroll,
        cursor_vrow,
        footer_layout.input_h as usize,
        input_wrap.len(),
    );

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
    draw_footer(frame, footer_area, app, &agents, &footer_layout);

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

    // Draw the @-mention file-autocomplete popup with the same stacking
    // rationale as the palette (bottom-anchored, above the footer, input
    // bar visible behind it). Mutually exclusive with the palette.
    if app.mention_popup.is_some() {
        draw_mention_popup(frame, area, app);
    }
}

/// Agent row count for the footer's agents panel, clamped to a third of
/// the terminal height. Computed once in [`draw`] and shared with
/// [`compute_footer_layout`] (to size the footer) and [`draw_footer`]
/// (to lay out + render the panel) so the two never disagree on the
/// `term_height / 3` denominator (tui-bugs #11).
fn agent_rows(agents: &[Arc<AgentHandle>], term_height: u16) -> u16 {
    if agents.is_empty() {
        0
    } else {
        agents.len().min((term_height / 3) as usize) as u16
    }
}

/// (B3 Fix 5) Max rows the input bar grows to before the textarea's own
/// viewport takes over and scrolls to keep the cursor visible.
const MAX_INPUT_ROWS: u16 = 6;

/// The footer's per-component heights, computed once by
/// [`compute_footer_layout`] so [`draw`] (which sizes the footer Rect via
/// [`FooterLayout::total`]) and [`draw_footer`] (which builds the row
/// constraints) consume the SAME numbers and can never disagree on the
/// constraint sum (B3 Fix 6 / tui-bugs #11).
///
/// The rows, top-to-bottom, are: `todo` overlay, agents header, agents
/// panel, spinner, queued previews, status rule, input bar.
struct FooterLayout {
    todo_rows: u16,
    agent_header: u16,
    agent_rows: u16,
    spinner_h: u16,
    queued_rows: u16,
    rule_h: u16,
    /// (B4-INPUT-HINT) Keymap hint row (`⏎ send · \⏎ newline …`) shown
    /// while the draft spans multiple visual rows. Pure chrome — first to
    /// go under height pressure.
    hint_h: u16,
    input_h: u16,
}

impl FooterLayout {
    /// Sum of the component heights — the total footer height. Guaranteed
    /// `<= term_height - 1` for any `term_height >= 2` (see
    /// [`compute_footer_layout`]), so the scrollback always keeps its
    /// `Constraint::Min(1)` row.
    fn total(&self) -> u16 {
        self.todo_rows
            + self.agent_header
            + self.agent_rows
            + self.spinner_h
            + self.queued_rows
            + self.rule_h
            + self.hint_h
            + self.input_h
    }
}

/// Compute the footer's per-component heights from the precomputed agent-row
/// count, the queued-message snapshot, the `todo` overlay height, and the
/// draft's line count.
///
/// `term_height` is the frame's area height — not a separate syscall.
/// `agent_rows` is computed once by the caller via [`agent_rows`] and
/// `todo_rows` by the caller (both already clamped to a fraction of the
/// terminal); they're passed in so the height and the layout use one value.
/// `input_lines` is the draft's soft-wrapped *visual* row count
/// (`input_layout::wrap_layout(...).len()`, B4-INPUT-WIDTH), clamped to
/// `[1, MAX_INPUT_ROWS]` here.
///
/// # Degradation priority
///
/// The natural component heights can sum to more than the footer's budget
/// (`term_height - 1` — the scrollback always keeps at least one row). When
/// that happens the components are shrunk in a FIXED priority so the layout
/// always fits the footer Rect and the cassowary solver never silently
/// collapses the input bar: the input bar is sacred (floored at 1), then the
/// status rule, spinner, queued previews, agents panel, and the `todo`
/// overlay shrink first — the `todo` overlay gives way first, the input bar
/// last. The returned layout's [`FooterLayout::total`] is therefore
/// `<= term_height - 1` for any `term_height >= 2`.
fn compute_footer_layout(
    agent_rows: u16,
    queued: &[String],
    todo_rows: u16,
    input_lines: u16,
    input_hint: bool,
    term_height: u16,
) -> FooterLayout {
    // Natural (unclamped-by-budget) component heights.
    let mut todo = todo_rows;
    let mut a_rows = agent_rows;
    let mut a_header: u16 = if agent_rows > 0 { 1 } else { 0 };
    let mut queued_rows = queued.len().min(3) as u16;
    let mut spinner = 1u16;
    let mut rule = 1u16;
    let mut hint: u16 = if input_hint { 1 } else { 0 };
    let mut input = input_lines.clamp(1, MAX_INPUT_ROWS);

    // The footer never fills the whole screen: the scrollback keeps its
    // `Constraint::Min(1)` row, so the footer's budget is `term_height - 1`.
    let budget = term_height.saturating_sub(1);
    let natural = todo + a_header + a_rows + spinner + queued_rows + rule + hint + input;
    let mut overflow = natural.saturating_sub(budget);

    // Shrink in the degradation priority (least-protected first). Each step
    // takes as much as it can up to its floor, then defers to the next.
    // 0. (B4-INPUT-HINT) keymap hint → 0 — pure chrome, first to go.
    let take = hint.min(overflow);
    hint -= take;
    overflow -= take;
    // 1. todo overlay → 0
    let take = todo.min(overflow);
    todo -= take;
    overflow -= take;
    // 2. agents panel → 0; once no rows remain the header is meaningless and
    //    is dropped too (freeing its row).
    if overflow > 0 {
        let take = a_rows.min(overflow);
        a_rows -= take;
        overflow -= take;
    }
    if a_rows == 0 && a_header > 0 {
        // Dropping the now-orphaned header frees one more row (saturating so
        // an already-satisfied overflow stays at 0).
        overflow = overflow.saturating_sub(a_header);
        a_header = 0;
    }
    // 3. queued previews → 0
    if overflow > 0 {
        let take = queued_rows.min(overflow);
        queued_rows -= take;
        overflow -= take;
    }
    // 4. spinner → 0
    if overflow > 0 {
        let take = spinner.min(overflow);
        spinner -= take;
        overflow -= take;
    }
    // 5. status rule → 0
    if overflow > 0 {
        let take = rule.min(overflow);
        rule -= take;
        overflow -= take;
    }
    // 6. input bar — sacred: never below 1 row.
    if overflow > 0 {
        let take = input.saturating_sub(1).min(overflow);
        input -= take;
    }

    FooterLayout {
        todo_rows: todo,
        agent_header: a_header,
        agent_rows: a_rows,
        spinner_h: spinner,
        queued_rows,
        rule_h: rule,
        hint_h: hint,
        input_h: input,
    }
}

/// Draw the footer block: agents panel + spinner + queued + rule + input.
///
/// `layout` carries the per-component heights computed once by
/// [`compute_footer_layout`] (see [`FooterLayout`]); the row constraints are
/// built directly from it so the constraint sum equals the footer Rect that
/// [`draw`] sized from the SAME struct — the cassowary solver can never
/// shrink a row unpredictably (B3 Fix 6). The `todo` overlay renders FIRST
/// (top of the footer), the input bar LAST (bottom).
fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    agents: &[Arc<AgentHandle>],
    layout: &FooterLayout,
) {
    let FooterLayout {
        todo_rows,
        agent_header,
        agent_rows,
        spinner_h,
        queued_rows,
        rule_h,
        hint_h,
        input_h,
    } = *layout;

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
            if spinner_h > 0 {
                c.push(Constraint::Length(spinner_h));
            }
            for _ in 0..queued_rows {
                c.push(Constraint::Length(1));
            }
            if rule_h > 0 {
                c.push(Constraint::Length(rule_h));
            }
            if hint_h > 0 {
                c.push(Constraint::Length(hint_h));
            }
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

    // Spinner (dropped under extreme height pressure — see the degradation
    // priority in `compute_footer_layout`).
    if spinner_h > 0 {
        footer::draw_spinner(frame, chunks[chunk_idx], app);
        chunk_idx += 1;
    }

    // Queued previews.
    for (i, queued_text) in app.queued.iter().take(queued_rows as usize).enumerate() {
        footer::draw_queued(frame, chunks[chunk_idx + i], queued_text);
    }
    chunk_idx += queued_rows as usize;

    // Rule line (status bar) — also droppable under extreme height pressure.
    if rule_h > 0 {
        footer::draw_rule(frame, chunks[chunk_idx], app);
        chunk_idx += 1;
    }

    // (B4-INPUT-HINT) Keymap hint row above the input while multi-line.
    if hint_h > 0 {
        footer::draw_input_hint(frame, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // Input bar — always present (sacred, floored at 1 row).
    input::draw(frame, chunks[chunk_idx], app);
}

/// The approval-choice controls, one `(label, style-color)` per option.
/// Kept as a function so the draw path and the option-line packer agree on
/// the exact key set — every key here is a live arm of
/// [`crate::commands::code_tui::app::handle_approval_key`]: `y`=Allow,
/// `s`=Session, `a`=Always, `p`=Prefix, `r`=Root(GrantRoot), `o`=Domain,
/// `n`/Esc=Deny. The deny option names Esc explicitly so it's discoverable
/// even when the terminal is too narrow to fit every option on one row (B3
/// Fix 2 — the old single line truncated at the pane width and hid deny).
fn approval_option_tokens() -> Vec<(&'static str, ratatui::style::Color)> {
    vec![
        ("[y] Allow", theme::SUCCESS),
        ("[s] Session", theme::WARNING),
        ("[a] Always", theme::ACCENT),
        // (M4/#10) Per-call scope choices, shown dim so the primary y/s/a/n
        // flow stays visually dominant.
        ("[p] Prefix", theme::MUTED),
        ("[r] Root", theme::MUTED),
        ("[o] Domain", theme::MUTED),
        ("[n]/Esc Deny", theme::ERROR),
    ]
}

/// Pack the approval option tokens into as many rows as needed so every
/// option is always visible (B3 Fix 2). Tokens flow left-to-right separated
/// by two spaces; a token that would overflow `width` starts a new row. A
/// single token wider than `width` still gets its own row (Paragraph
/// truncates it, but the option keys are short so this never bites).
fn pack_approval_options(width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    const SEP: &str = "  ";
    let sep_w = SEP.width();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for (label, color) in approval_option_tokens() {
        let lw = label.width();
        let add = if cur.is_empty() { lw } else { sep_w + lw };
        if !cur.is_empty() && cur_w + add > width {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur_w = 0;
        }
        if !cur.is_empty() {
            cur.push(Span::raw(SEP));
            cur_w += sep_w;
        }
        cur.push(Span::styled(label, Style::default().fg(color)));
        cur_w += lw;
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// Draw the approval modal as a centered popup.
///
/// The modal is a single opaque box: `Clear` wipes the transcript behind it
/// (B3 Fix 1) and a rounded, padded [`Block`] frames the content so nothing
/// bleeds through around the labels. The preview region scrolls (B3 Fix 3 —
/// `approval.scroll`, a top-anchored offset into the wrapped preview) with a
/// "… (N more lines — PageDown)" hint when there's more below, and the
/// options wrap across rows so the deny choice is never truncated off-screen
/// (B3 Fix 2). All the choice/scroll KEY handling lives in
/// `handle_approval_key`; this function is pure rendering + it pins
/// `approval.max_scroll` to the real scrollable range at the end.
fn draw_approval_modal(frame: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

    let Some(approval) = &app.approval else {
        return;
    };

    // Modal size: 70% width, max 80% height, centered. The block reserves 2
    // columns for borders + 2 for horizontal padding, so content wraps to
    // `modal_width - 4`.
    let modal_width = (area.width as f32 * 0.7) as u16;
    let max_modal_height = (area.height as f32 * 0.8) as u16;
    let content_width = modal_width.saturating_sub(4).max(1) as usize;

    // Pre-wrap the preview into explicit lines (word-wrapped to the content
    // width). An exact line count lets us window a scroll region precisely.
    let wrapped_preview = wrap::word_wrap(&approval.preview, content_width, 0);
    let total_preview = wrapped_preview.len();

    // Option rows are wrapping-independent of height — compute them first so
    // the chrome budget knows how many rows they need (they're never clipped:
    // deny must always show).
    let option_lines = pack_approval_options(content_width);
    let option_rows = option_lines.len();

    // Fixed chrome rows: tool (1) + "Preview:" label (1) + always_rule (1)
    // + blank separator (1) + the option rows.
    let chrome_rows = 4 + option_rows;
    let natural_inner = chrome_rows + total_preview;

    // Cap the modal at 80% height. If everything fits, show all preview with
    // no indicator; otherwise reserve one row for the "… more" hint and give
    // the rest to a scrollable preview window (>= 0 rows on a tiny terminal).
    let cap_inner = (max_modal_height as usize).saturating_sub(2);
    let (inner_height, preview_region) = if natural_inner <= cap_inner {
        (natural_inner, total_preview)
    } else {
        let region = cap_inner.saturating_sub(chrome_rows + 1);
        (cap_inner, region)
    };

    // Clamp the scroll offset to the real scrollable range and window the
    // preview. `max_scroll` is pinned back onto the modal after the borrow of
    // `approval` ends so the key handler clamps against the same value.
    let max_scroll = total_preview.saturating_sub(preview_region);
    let scroll = (approval.scroll as usize).min(max_scroll);
    let visible_preview: &[String] = if preview_region == 0 || total_preview == 0 {
        &[]
    } else {
        let end = (scroll + preview_region).min(total_preview);
        &wrapped_preview[scroll..end]
    };
    let more_below = total_preview.saturating_sub(scroll + visible_preview.len());

    let modal_height = (inner_height as u16).saturating_add(2);
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    // (B3 Fix 1) Clear the transcript behind the modal so it reads as one
    // opaque box, then frame it — mirrors the slash palette / mention popup.
    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " approval required ",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    // Build the content lines top-to-bottom.
    let mut lines: Vec<Line> = Vec::with_capacity(chrome_rows + preview_region + 1);
    lines.push(Line::from(vec![
        Span::styled("Tool: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &approval.tool_name,
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "Preview:",
        Style::default().fg(Color::DarkGray),
    )));
    for pl in visible_preview {
        lines.push(Line::from(Span::raw(pl.clone())));
    }
    if more_below > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "… ({more_below} more line{} — PageDown)",
                if more_below == 1 { "" } else { "s" }
            ),
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled("Always rule: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&approval.always_rule, Style::default().fg(theme::ACCENT)),
    ]));
    lines.push(Line::from(""));
    lines.extend(option_lines);

    // No Wrap — lines are already pre-wrapped/packed to fit the content width.
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, modal_area);

    // (B3 Fix 3) Pin `max_scroll` to the real scrollable range so
    // `handle_approval_key`'s scroll arms clamp against it — mirrors the diff
    // viewer's post-draw pin. Saturate the `u16` cast for a pathological
    // >65535-line preview.
    if let Some(a) = app.approval.as_mut() {
        a.max_scroll = max_scroll.min(u16::MAX as usize) as u16;
    }
}

/// Render `text` word-wrapped to `width` display columns as styled owned
/// [`Line`]s: the first line carries `prefix` (styled `prefix_style`),
/// continuation lines are indented by `prefix`'s display width so they align
/// under the body. The wrapped body uses `body_style`. Used by the ask modal
/// (B3 Fix 4) to wrap the question and long options instead of silently
/// clipping them at the modal width.
fn wrapped_labeled_lines(
    prefix: &str,
    prefix_style: ratatui::style::Style,
    text: &str,
    body_style: ratatui::style::Style,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line, Span};
    let prefix_w = prefix.width();
    let body_width = width.saturating_sub(prefix_w).max(1);
    let wrapped = wrap::word_wrap(text, body_width, 0);
    let indent = " ".repeat(prefix_w);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(chunk, body_style),
                ])
            } else {
                Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(chunk, body_style),
                ])
            }
        })
        .collect()
}

/// Take the trailing run of `s` whose cumulative display width is `<=
/// budget`, taking whole code points so a wide glyph is never split. Used to
/// tail-scroll the ask modal's free-text input so a long answer stays inside
/// the box with the cursor visible (B3 Fix 4).
fn tail_by_width(s: &str, budget: usize) -> String {
    let mut w = 0usize;
    let mut start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        w += cw;
        start = idx;
    }
    s[start..].to_string()
}

/// Draw the ask-user modal as a centered popup.
///
/// (B3 Fix 4) The question and each option are word-wrapped to the modal
/// width — grown up to the 80%-height cap to fit — instead of being silently
/// clipped. In free-text mode the input tail-scrolls so a long answer stays
/// inside the box and the terminal cursor is clamped within the border.
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

    let modal_width = (area.width as f32 * 0.7) as u16;
    // Content wraps to the inner width (borders consume 2 columns).
    let content_width = modal_width.saturating_sub(2).max(1) as usize;
    let max_modal_height = area.height.saturating_sub(2);

    let q_style = Style::default().add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(theme::MUTED);

    if modal.free_text_mode {
        // Free-text input mode. Wrap the question, then reserve rows for the
        // blank / input / blank / hint.
        let mut lines: Vec<Line> =
            wrapped_labeled_lines("Q: ", label_style, &q.question, q_style, content_width);
        let q_lines = lines.len();

        // Tail-scroll the free-text so the visible tail + "❯ " prefix fit the
        // content width, keeping the cursor inside the box.
        let input_avail = content_width.saturating_sub(2);
        let full_w = modal.free_text.width();
        let (visible, visible_w) = if full_w <= input_avail {
            (modal.free_text.clone(), full_w)
        } else {
            let t = tail_by_width(&modal.free_text, input_avail);
            let w = t.width();
            (t, w)
        };

        let hint_lines = wrap::word_wrap("[enter] submit  [esc] cancel", content_width, 0);
        let hint_count = hint_lines.len();

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("❯ ", theme::bold_accent()),
            Span::raw(visible),
        ]));
        lines.push(Line::from(""));
        for h in hint_lines {
            lines.push(Line::from(Span::styled(
                h,
                Style::default().fg(theme::MUTED),
            )));
        }

        let content_rows = q_lines + 3 + hint_count;
        let modal_height = (content_rows as u16)
            .saturating_add(2)
            .min(max_modal_height.max(3));
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
        frame.render_widget(Paragraph::new(lines), inner);

        // Clamp the terminal cursor inside the box. The input row is at
        // `q_lines + 1` (after the wrapped question + one blank); if the modal
        // was height-capped so that row is off-screen, keep the cursor on the
        // last inner row.
        let input_row = (q_lines as u16).saturating_add(1);
        let cursor_y = inner
            .y
            .saturating_add(input_row)
            .min(inner.y + inner.height.saturating_sub(1));
        let cursor_x = inner
            .x
            .saturating_add(2)
            .saturating_add(visible_w as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, cursor_y));
    } else {
        // Options list mode. Wrap the question + header + hint.
        let mut header_lines: Vec<Line> =
            wrapped_labeled_lines("Q: ", label_style, &q.question, q_style, content_width);
        if !q.header.is_empty() {
            header_lines.extend(wrapped_labeled_lines(
                "",
                Style::default(),
                &q.header,
                Style::default().fg(theme::ACCENT),
                content_width,
            ));
        }
        let hint = if q.multi_select {
            "↑↓ move · space toggle · enter confirm · esc cancel"
        } else {
            "↑↓ move · 1-9 pick · enter confirm · esc cancel"
        };
        for h in wrap::word_wrap(hint, content_width, 0) {
            header_lines.push(Line::from(Span::styled(
                h,
                Style::default().fg(theme::MUTED),
            )));
        }

        // Build option items, wrapping any that overflow the content width so
        // nothing is clipped. Options that fit keep the rich marker/label/desc
        // styling; long ones wrap with the marker on the first row and an
        // indented, muted continuation.
        let mut option_rows = 0usize;
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
                let combined = match &opt.description {
                    Some(d) => format!("{} — {d}", opt.label),
                    None => opt.label.clone(),
                };
                let item_lines: Vec<Line> = if combined.width() + 2 <= content_width {
                    // Fits on one row — keep the pretty label/desc distinction.
                    let mut spans = vec![
                        Span::styled(marker, Style::default().fg(theme::ACCENT)),
                        Span::raw(opt.label.clone()),
                    ];
                    if let Some(desc) = &opt.description {
                        spans.push(Span::styled(
                            format!(" — {desc}"),
                            Style::default().fg(theme::MUTED),
                        ));
                    }
                    vec![Line::from(spans)]
                } else {
                    wrapped_labeled_lines(
                        marker,
                        Style::default().fg(theme::ACCENT),
                        &combined,
                        Style::default(),
                        content_width,
                    )
                };
                option_rows += item_lines.len();
                ListItem::new(item_lines)
            })
            .collect();

        // Header height (wrapped rows + a blank separator before the list).
        let header_height = header_lines.len().saturating_add(1);
        let content_rows = header_height + option_rows;
        let modal_height = (content_rows.min(u16::MAX as usize - 2) as u16)
            .saturating_add(2)
            .min(max_modal_height.max(3));
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

        // Clamp the header area to the inner height so the list area never has
        // a negative/zero height on a tiny terminal.
        let header_h = (header_height as u16).min(inner.height);
        let header_area = Rect {
            height: header_h,
            ..inner
        };
        frame.render_widget(Paragraph::new(header_lines), header_area);

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
    // screen. An empty filtered list still renders a bordered "no matches" box
    // — the clamp floor of 1 keeps a content row for that hint (a bare
    // `.min(7)` gave an empty list ZERO content rows, so the hint line was
    // built but never drawn).
    let palette_width = ((area.width as f32) * 0.8) as u16;
    let visible_rows = entries.len().clamp(1, 7);
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

/// Draw the @-mention file-autocomplete popup. Mirrors
/// [`draw_slash_palette`]'s geometry exactly (80% width, bottom-anchored
/// with a 2-row gap, capped at 7 content rows, windowed scrolling, the
/// selected row ACCENT+BOLD+REVERSED) — single-column path rows instead of
/// the name/description pair, directories distinguished by their trailing
/// `/`. Reads [`mention_popup_filtered`] — the same list the key handler
/// used for this frame, so the two never disagree on the row set.
fn draw_mention_popup(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(popup) = &app.mention_popup else {
        return;
    };

    let entries = mention_popup_filtered(app);

    let popup_width = ((area.width as f32) * 0.8) as u16;
    // Clamp floor 1 (not `.min(7)`): an empty filtered list keeps one
    // content row so the "(no matching files)" hint actually renders.
    let visible_rows = entries.len().clamp(1, 7);
    let popup_height = visible_rows.saturating_add(2) as u16;
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.height.saturating_sub(popup_height).saturating_sub(2);
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = " @files — esc to close ";
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
    let selected = popup.selected.min(total.saturating_sub(1));

    let visible = visible_rows;
    let half = visible / 2;
    let win_start = if total <= visible {
        0
    } else {
        selected.saturating_sub(half).min(total - visible)
    };

    let selected_style = Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::REVERSED);
    let dir_style = Style::default().fg(theme::MUTED);
    let file_style = Style::default().fg(theme::PRIMARY);

    let mut lines: Vec<Line> = Vec::new();
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no matching files)",
            Style::default().fg(theme::MUTED),
        )));
    } else {
        for (i, path) in entries.iter().enumerate() {
            if i < win_start || i >= win_start + visible {
                continue;
            }
            let style = if i == selected {
                selected_style
            } else if path.ends_with('/') {
                dir_style
            } else {
                file_style
            };
            lines.push(Line::from(Span::styled(path.clone(), style)));
        }
    }

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, popup_area);
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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    // ---- Fix 5 / Fix 6: footer layout ------------------------------------

    /// (Fix 5) The input row grows one row per draft line, capped at
    /// `MAX_INPUT_ROWS`, and never drops below 1.
    #[test]
    fn footer_input_row_grows_with_draft_lines() {
        let empty: Vec<String> = Vec::new();
        let tall = 60u16; // ample room — no degradation
        assert_eq!(
            compute_footer_layout(0, &empty, 0, 1, false, tall).input_h,
            1
        );
        assert_eq!(
            compute_footer_layout(0, &empty, 0, 4, false, tall).input_h,
            4
        );
        assert_eq!(
            compute_footer_layout(0, &empty, 0, 10, false, tall).input_h,
            MAX_INPUT_ROWS,
            "draft taller than the cap clamps to MAX_INPUT_ROWS"
        );
        assert_eq!(
            compute_footer_layout(0, &empty, 0, 0, false, tall).input_h,
            1,
            "an empty draft still reserves one input row"
        );
    }

    /// (B4-INPUT-HINT) The hint row appears when requested with room to
    /// spare, and is the FIRST thing dropped under height pressure — before
    /// the todo overlay, and long before the input row.
    #[test]
    fn footer_hint_row_drops_first_under_pressure() {
        let empty: Vec<String> = Vec::new();
        // Room to spare: hint present.
        let fl = compute_footer_layout(0, &empty, 0, 3, true, 60);
        assert_eq!(fl.hint_h, 1);
        assert_eq!(fl.input_h, 3);
        // Tight height: hint gives way before the todo overlay does.
        // Natural: todo 3 + spinner 1 + rule 1 + hint 1 + input 2 = 8,
        // budget 7 → overflow 1 → hint drops, todo survives.
        let fl = compute_footer_layout(0, &empty, 3, 2, true, 8);
        assert_eq!(fl.hint_h, 0, "hint is pure chrome — first to go");
        assert_eq!(fl.todo_rows, 3, "todo survives while only hint drops");
        assert_eq!(fl.input_h, 2, "input untouched");
        assert!(fl.total() <= 7);
    }

    /// (Fix 6) At a tiny terminal height with agents + a pinned todo + queued
    /// previews, the constraint sum must fit `term_height - 1` AND the input
    /// row must survive (>= 1) — the regression that let the cassowary solver
    /// collapse the input row.
    #[test]
    fn footer_layout_fits_and_keeps_input_at_tiny_height() {
        let queued = vec!["a".to_string(), "b".to_string()];
        let term_height = 10u16;
        let fl = compute_footer_layout(3, &queued, 4, 1, true, term_height);
        assert!(fl.input_h >= 1, "input row must never collapse below 1");
        assert!(
            fl.total() <= term_height - 1,
            "footer total {} must leave the scrollback its Min(1) row (budget {})",
            fl.total(),
            term_height - 1
        );
    }

    /// (Fix 6) Under extreme pressure (everything maxed, 4-row terminal) the
    /// input row still survives and the total still fits the budget — the
    /// chrome (todo, agents, queued, spinner, rule) gives way first.
    #[test]
    fn footer_layout_input_survives_extreme_pressure() {
        let queued: Vec<String> = (0..4).map(|i| i.to_string()).collect();
        let term_height = 4u16;
        let fl = compute_footer_layout(20, &queued, 20, 6, true, term_height);
        assert!(fl.input_h >= 1, "input row is sacred");
        assert!(
            fl.total() <= term_height - 1,
            "total {} must fit budget {}",
            fl.total(),
            term_height - 1
        );
        // The chrome collapsed first: todo + agents are gone.
        assert_eq!(fl.todo_rows, 0);
        assert_eq!(fl.agent_rows, 0);
        assert_eq!(
            fl.agent_header, 0,
            "orphaned agent header dropped with its rows"
        );
    }

    // ---- Fix 2: approval option wrapping ---------------------------------

    /// (Fix 2) At a narrow width the options wrap across rows and every
    /// option — including the deny choice, which names Esc — stays visible.
    #[test]
    fn approval_options_wrap_and_keep_deny_visible() {
        let narrow = pack_approval_options(40);
        assert!(
            narrow.len() > 1,
            "narrow width must wrap options across rows, got {}",
            narrow.len()
        );
        let joined: String = narrow
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            joined.contains("Deny"),
            "deny option must be present: {joined:?}"
        );
        assert!(
            joined.contains("Esc"),
            "Esc-to-deny must be named: {joined:?}"
        );
        for (i, l) in narrow.iter().enumerate() {
            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 40, "row {i} width {w} must fit 40 cols");
        }
    }

    /// (Fix 2) A wide terminal keeps all options on a single row.
    #[test]
    fn approval_options_single_row_when_wide() {
        let wide = pack_approval_options(200);
        assert_eq!(wide.len(), 1, "all options fit one row at width 200");
        let joined: String = wide[0].spans.iter().map(|s| s.content.as_ref()).collect();
        for token in ["[y] Allow", "[s] Session", "[a] Always", "[n]/Esc Deny"] {
            assert!(joined.contains(token), "missing {token} in {joined:?}");
        }
    }

    // ---- Fix 4: ask modal wrap helpers -----------------------------------

    /// (Fix 4) `wrapped_labeled_lines` wraps the body across rows, keeps the
    /// prefix on the first row + an aligned indent on continuations, and
    /// every rendered row fits the width.
    #[test]
    fn wrapped_labeled_lines_wraps_indents_and_fits() {
        use ratatui::style::Style;
        let text = "one two three four five six seven eight nine ten";
        let lines = wrapped_labeled_lines("Q: ", Style::default(), text, Style::default(), 12);
        assert!(lines.len() > 1, "long text must wrap");
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            first.starts_with("Q: "),
            "prefix on the first row: {first:?}"
        );
        let second: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            second.starts_with("   "),
            "continuation indented by the prefix width: {second:?}"
        );
        for (i, l) in lines.iter().enumerate() {
            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 12, "row {i} width {w} must fit 12 cols");
        }
    }

    /// (Fix 4) `tail_by_width` returns the trailing run that fits the budget,
    /// taking whole (possibly wide) glyphs.
    #[test]
    fn tail_by_width_keeps_trailing_fit() {
        assert_eq!(tail_by_width("abcdefgh", 3), "fgh");
        assert_eq!(
            tail_by_width("abc", 10),
            "abc",
            "shorter than budget returns whole"
        );
        // 4 full-width CJK chars (8 cols); budget 4 keeps the last two.
        let s = "中文测试";
        let t = tail_by_width(s, 4);
        assert_eq!(t, "测试");
        assert!(t.width() <= 4);
    }
}
