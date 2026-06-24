//! Scrollback transcript — renders the conversation history with
//! markdown formatting and a scrollbar.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::commands::code_tui::app::{App, SubagentOutcome, TranscriptEntry};
use crate::commands::code_tui::{markdown, theme, wrap};

/// Marker prefix for a per-tool result line — distinct from the tool-call
/// `●` so a result reads as a reply rather than another invocation.
const RESULT_MARKER: &str = "↳ ";

/// Max visual lines of a `ToolResult`'s `output` we render before
/// collapsing the rest into a "… N more lines" line. Tuned for a scannable
/// transcript: a tool usually emits a short confirmation; long outputs
/// (big reads, verbose bash) get a short summary instead of a wall.
const MAX_RESULT_LINES: usize = 5;

/// Count the visual rows the wrap-off `Paragraph` renderer will produce for
/// `lines`.
///
/// COUNT MODEL: wrap-off + flat count. `draw` renders the transcript with
/// `.wrap()` OFF, so ratatui 0.30's `LineTruncator` truncates each input
/// `Line` to exactly ONE visual row — it never soft-wraps. The row-count
/// model must therefore agree: every `Line` (empty OR over-wide) is exactly
/// one row, i.e. `lines.len()`.
///
/// The previous model ceil-divided each line's display width by
/// `usable_width`, which OVER-COUNTED over-wide lines (headings, code-block
/// lines, single-line transcript entries): counted rows > rendered rows,
/// inflating `max_from_top` / `scroll_from_top` and leaving blank rows above
/// the latest content plus a misplaced scrollbar thumb at the bottom. The
/// flat count matches the truncating renderer exactly.
///
/// Lines that legitimately want to wrap are PRE-WRAPPED upstream (in `draw`
/// via `markdown::render` — headings, code blocks, paragraphs — and
/// `wrap::word_wrap` for User / `ToolResult` text), so each pre-wrapped
/// chunk is its own `Line` and the flat 1-per-`Line` count is correct for
/// those too. Single-line entries (Tool / SubagentTool / SubagentEnd /
/// System / AutoAllowed / Blank) are intentionally truncated, counted as 1.
pub(crate) fn visual_line_count(lines: &[Line]) -> usize {
    lines.len()
}

/// Draw the scrollback transcript.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Reserve a 1-column right margin for the scrollbar so it doesn't
    // clobber the last column of wrapped text.  The remaining width is
    // the usable column budget that markdown::render pre-wraps to, and
    // that the scroll-row count below divides by.
    let para_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    let usable_width = para_area.width as usize;

    // Build lines from transcript entries.
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.transcript {
        match entry {
            TranscriptEntry::User(text) => {
                // Pre-wrap the user prompt to usable_width so a long
                // prompt does not overflow once Paragraph no longer
                // wraps.  The `❯` marker + " " prefixes only the first
                // wrapped line; continuation lines are raw text.
                let marker = theme::glyph::USER_PROMPT;
                let prefix_w = marker.width() + 1; // glyph + space
                let wrapped = wrap::word_wrap(text, usable_width, prefix_w);
                for (i, chunk) in wrapped.into_iter().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(marker, theme::bold_accent()),
                            Span::raw(" "),
                            Span::styled(chunk, theme::bold()),
                        ]));
                    } else {
                        lines.push(Line::from(vec![Span::styled(chunk, theme::bold())]));
                    }
                }
            }
            TranscriptEntry::Assistant(text) => {
                // Render markdown: headings, bold, italic, code, lists, etc.
                // The `●` marker goes on the first rendered line.  render
                // pre-wraps each logical line to usable_width, so each
                // returned Line is one visual row.
                let md_lines = markdown::render(text, usable_width);
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
                let md_lines = markdown::render(text, usable_width);
                for (i, md_line) in md_lines.into_iter().enumerate() {
                    if i == 0 {
                        let mut v = vec![
                            Span::styled(
                                agent_name.clone(),
                                ratatui::style::Style::default()
                                    .fg(color)
                                    .add_modifier(ratatui::style::Modifier::BOLD),
                            ),
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
                args,
            } => {
                let color = app
                    .registry
                    .find_by_name(agent_name)
                    .map(|h| theme::agent_color_for(h.color))
                    .unwrap_or(theme::MUTED);
                // Reuse the shared preview (not re-implemented here) to get
                // the per-tool detail string, e.g. "bash cargo test --lib".
                // `tool_preview` already prefixes the tool name; split off
                // the detail so we can color the tool name like the main
                // `Tool` arm (bold) and the detail muted.
                let preview = crate::commands::code_tool_preview::tool_preview(tool_name, args);
                let detail = preview
                    .strip_prefix(tool_name)
                    .map(str::trim_start)
                    .unwrap_or("");
                let agent_style = ratatui::style::Style::default()
                    .fg(color)
                    .add_modifier(ratatui::style::Modifier::BOLD);
                let marker_style = ratatui::style::Style::default().fg(color);
                // Mirror the main `Tool` arm: marker + bold tool name +
                // muted (detail), but agent-colored (marker + agent name).
                let mut spans = vec![
                    Span::styled(agent_name.clone(), agent_style),
                    Span::raw(" "),
                    Span::styled(theme::glyph::TOOL_MARKER, marker_style),
                    Span::raw(" "),
                    Span::styled(tool_name, theme::bold()),
                ];
                if !detail.is_empty() {
                    spans.push(Span::styled(format!("({detail})"), theme::muted()));
                }
                lines.push(Line::from(spans));
            }
            TranscriptEntry::SubagentEnd {
                agent_name,
                outcome,
            } => {
                let color = app
                    .registry
                    .find_by_name(agent_name)
                    .map(|h| theme::agent_color_for(h.color))
                    .unwrap_or(theme::MUTED);
                let agent_style = ratatui::style::Style::default()
                    .fg(color)
                    .add_modifier(ratatui::style::Modifier::BOLD);
                // Distinct colors per outcome: Completed → success "done",
                // Failed → error "failed", Stopped → muted "stopped".
                let (label, label_style) = match outcome {
                    SubagentOutcome::Completed => ("done", theme::success()),
                    SubagentOutcome::Failed => ("failed", theme::error()),
                    SubagentOutcome::Stopped => ("stopped", theme::muted()),
                };
                lines.push(Line::from(vec![
                    Span::styled(agent_name.clone(), agent_style),
                    Span::raw(" "),
                    Span::styled(label, label_style),
                ]));
            }
            // M5a: a dim per-tool result line, prefixed with `↳ <name>` and
            // color-coded by `is_error`. The `output` is already compacted
            // by `app::render_tool_output` (newlines collapsed, capped at
            // 200 chars), so we word-wrap it to `usable_width` and cap the
            // visual lines at [`MAX_RESULT_LINES`], appending a
            // "… N more lines" line when it overflows.
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
                let prefix_w = prefix.width() + 2; // prefix + ": " (2 display cols)
                let body = if output.is_empty() {
                    prefix
                } else {
                    format!("{prefix}: {output}")
                };
                // Word-wrap to `usable_width` so a long result doesn't
                // overflow once Paragraph no longer wraps (M4a pre-wrap).
                let wrapped = wrap::word_wrap(&body, usable_width, prefix_w);
                let total = wrapped.len();
                if total <= MAX_RESULT_LINES {
                    for chunk in wrapped {
                        lines.push(Line::from(Span::styled(chunk, style)));
                    }
                } else {
                    for chunk in wrapped.into_iter().take(MAX_RESULT_LINES) {
                        lines.push(Line::from(Span::styled(chunk, style)));
                    }
                    let more = total - MAX_RESULT_LINES;
                    lines.push(Line::from(Span::styled(
                        format!("… {more} more line{}", if more == 1 { "" } else { "s" }),
                        style,
                    )));
                }
            }
            TranscriptEntry::AutoAllowed(text) => {
                lines.push(Line::from(Span::styled(text, theme::muted())));
            }
            TranscriptEntry::System(text) => {
                lines.push(Line::from(Span::styled(text, theme::muted())));
            }
            // (MED-9) Errors render in the error color, not dim — mirrors the
            // ToolResult `is_error` styling above so failures are visible.
            TranscriptEntry::Error(text) => {
                lines.push(Line::from(Span::styled(text, theme::error())));
            }
            TranscriptEntry::Blank => {
                lines.push(Line::from(""));
            }
        }
    }

    // COUNT MODEL: wrap-off + flat count.  Paragraph below renders with
    // `.wrap()` OFF, so ratatui 0.30's LineTruncator truncates each input
    // `Line` to exactly ONE visual row regardless of its display width —
    // it never soft-wraps.  The row-count model MUST therefore agree: every
    // `Line` (empty OR over-wide) contributes exactly one visual row.  The
    // previous ceil(width/usable_width) count OVER-COUNTED over-wide lines
    // (headings, code-block lines, single-line transcript entries): counted
    // rows > rendered rows, inflating max_from_top / scroll_from_top and
    // leaving blank rows above the latest content + a misplaced scrollbar
    // thumb at the bottom.  The flat count below matches the truncating
    // renderer exactly.
    //
    // Lines that legitimately want to wrap are PRE-WRAPPED upstream so they
    // never reach the truncator over-wide: `markdown::render` (headings via
    // `heading()`, code blocks via `render_code_block`, paragraphs via
    // `wrap_spans`) and the `wrap::word_wrap` calls above for User /
    // ToolResult text.  Each pre-wrapped chunk is its own `Line`, so the
    // flat 1-per-Line count is also correct for those.  The remaining
    // single-Line entries (Tool / SubagentTool / SubagentEnd / System /
    // AutoAllowed / Blank) are intentionally truncated, counted as 1 —
    // matching the render.
    let total_visual_lines: usize = visual_line_count(&lines);

    // `app.scroll` is "offset from bottom" (0 = latest).  But
    // `Paragraph::scroll()` expects "offset from top" in *visual* lines.
    //   scroll_from_top = max(0, total_visual_lines − viewport − scroll_from_bottom)
    let viewport = area.height as usize;
    let max_from_top = total_visual_lines.saturating_sub(viewport);
    let scroll_from_top = max_from_top
        .saturating_sub(app.scroll as usize)
        .min(max_from_top);

    // Render with scroll.  No `.wrap()`: content is already pre-wrapped to
    // usable_width, and leaving wrap off stops ratatui from double-counting
    // (and thus drifting the scroll position against the row count above).
    // (R4) Saturate before the `u16` cast: a scrollback taller than 65535
    // visual lines would otherwise wrap the offset (`70000 as u16 == 4464`)
    // and render the wrong slice. `app.scroll` is itself `u16`, so clamping
    // to `u16::MAX` loses nothing the renderer could express.
    let scroll_from_top_u16 = scroll_from_top.min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(lines).scroll((scroll_from_top_u16, 0));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code_tui::markdown;
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;

    /// Build a transcript-shaped `lines` Vec the way `draw` does — a
    /// markdown-rendered assistant heading (pre-wrapped) followed by a
    /// single-line `Tool` entry — and assert the flat row count matches the
    /// wrap-off renderer's row count (one row per `Line`). This is the
    /// HIGH-1 invariant: the count model must agree with the truncating
    /// renderer so scroll-to-bottom leaves no phantom blank rows.
    #[test]
    fn flat_count_matches_wrap_off_renderer() {
        // Narrow width so a long heading must pre-wrap to several rows.
        let usable_width = 20usize;

        // A long heading with spaces — pre-wraps to several rows at width 20
        // (each row fits the budget, so the wrap-off renderer does NOT
        // truncate any of them). markdown::render strips the "# " prefix and
        // word-wraps the body.
        let heading_text = format!("# {}", "word ".repeat(17).trim_end());
        let md_lines = markdown::render(&heading_text, usable_width);
        assert!(
            md_lines.len() > 1,
            "long heading must pre-wrap to >1 row at width 20, got {}",
            md_lines.len()
        );
        for (i, line) in md_lines.iter().enumerate() {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                w <= usable_width,
                "heading row {i} ({w} cols) must fit width {usable_width} so the \
                 wrap-off renderer does not truncate it"
            );
        }
        let heading_rows = md_lines.len();

        // A long single-line Tool entry — NOT pre-wrapped, so it overflows
        // usable_width. The wrap-off renderer truncates it to ONE row; the
        // flat count must count it as 1 (NOT ceil(width/usable_width)).
        let tool_line = Line::from(vec![
            Span::styled(theme::glyph::TOOL_MARKER, theme::accent()),
            Span::raw(" "),
            Span::styled("bash", theme::bold()),
            Span::styled(
                "(cargo test --lib --features very-long-flag-name)",
                theme::muted(),
            ),
        ]);
        let tool_w: usize = tool_line.spans.iter().map(|s| s.content.width()).sum();
        assert!(
            tool_w > usable_width,
            "tool line ({tool_w} cols) must overflow width {usable_width} to exercise the \
             over-wide single-line case"
        );

        let mut lines: Vec<Line> = Vec::new();
        lines.extend(md_lines);
        lines.push(tool_line);

        // The flat count is one row per Line: heading rows + 1 tool row.
        let expected = heading_rows + 1;
        let total = visual_line_count(&lines);
        assert_eq!(
            total, expected,
            "flat count must equal rendered rows (1 per Line); the old ceil-division \
             model would over-count the over-wide tool line to \
             ceil({tool_w}/{usable_width}) extra rows and drift the scroll"
        );
        // Explicitly: the over-wide tool line is counted as 1, not its
        // ceil-division (the HIGH-1 regression).
        let old_tool_rows = ((tool_w + usable_width - 1) / usable_width).max(1);
        assert!(
            old_tool_rows > 1,
            "sanity: old model over-counts the tool line to {old_tool_rows} rows"
        );
        assert_eq!(
            total - heading_rows,
            1,
            "the over-wide tool line contributes exactly 1 row under the flat model, \
             not {old_tool_rows}"
        );

        // Scrolling to the bottom must leave no phantom blank rows: with a
        // viewport >= total, max_from_top is 0 and scroll_from_top clamps
        // to 0 — the latest content sits flush at the bottom with no blank
        // rows above it (the bug the old over-count produced).
        let viewport = total; // viewport exactly fits all rows
        let max_from_top = total.saturating_sub(viewport);
        assert_eq!(
            max_from_top, 0,
            "max_from_top is 0 when viewport fits all rows"
        );
        let scroll_from_top = max_from_top.saturating_sub(0).min(max_from_top);
        assert_eq!(
            scroll_from_top, 0,
            "scroll_from_top clamps to 0 at the bottom"
        );
    }

    /// A blank/empty `Line` is still one visual row (the renderer reserves
    /// a row for it). The flat count must NOT collapse empty lines to 0.
    #[test]
    fn empty_line_is_one_row() {
        let lines = vec![Line::from(""), Line::from(""), Line::from("x")];
        assert_eq!(visual_line_count(&lines), 3);
    }

    /// An over-wide single line is counted as ONE row, matching the
    /// truncating renderer — not ceil-divided. This is the core regression
    /// guard: the old model counted this as 4 rows (61/20).
    #[test]
    fn over_wide_single_line_counts_as_one_row() {
        let usable_width = 20usize;
        let line = Line::from(Span::raw("x".repeat(61)));
        assert_eq!(visual_line_count(std::slice::from_ref(&line)), 1);
        // Sanity: this is the case the old ceil-division got wrong.
        let old_count = ((61 + usable_width - 1) / usable_width).max(1);
        assert_eq!(old_count, 4, "old model over-counted to 4, flat count is 1");
    }
}
