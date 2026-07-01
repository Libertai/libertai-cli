//! Scrollback transcript — renders the conversation history with
//! markdown formatting and a scrollbar.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::commands::chat_render::strip_ansi;
use crate::commands::code_team::AgentStatus;
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

/// Cap on the number of distinct rendered texts held in the cache.
/// Past this, the oldest entries are evicted (FIFO via insertion order
/// of `IndexMap`). 256 distinct assistant/subagent blocks is well above a
/// normal session's settled entries; the live still-growing entry
/// bypasses the cache, so this only covers completed blocks.
const RENDER_CACHE_CAP: usize = 256;

/// Cache of rendered-markdown `Vec<Line>` for settled transcript text
/// (finding #3). Keyed on the entry's text; the value also records the
/// width it was rendered at so a width change invalidates it.
///
/// Uses `IndexMap` so we can evict the oldest entry when the cap is hit
/// (a plain `HashMap` has no insertion order). The render is pure given
/// (text, width), so the cache is sound; the only invalidation
/// triggers are a width change and ring-buffer eviction (the latter
/// handled by the cap + natural churn as new entries displace old).
pub struct RenderCache {
    entries: indexmap::IndexMap<String, (usize, Vec<Line<'static>>)>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            entries: indexmap::IndexMap::new(),
        }
    }

    /// Return the cached `Vec<Line>` for `text` if it was rendered at
    /// exactly `width`; otherwise render, store, and return it. The live
    /// still-growing entry should NOT call this — it re-renders each
    /// frame and only enters the cache once it settles.
    pub fn get_or_render(&mut self, text: &str, width: usize) -> Vec<Line<'static>> {
        if let Some((cached_w, lines)) = self.entries.get(text) {
            if *cached_w == width {
                return lines.clone();
            }
        }
        let lines = markdown::render(text, width);
        self.entries
            .insert(text.to_string(), (width, lines.clone()));
        // FIFO eviction at the cap so the cache can't grow unboundedly
        // across a long session with many distinct settled blocks.
        while self.entries.len() > RENDER_CACHE_CAP {
            self.entries.shift_remove_index(0);
        }
        lines
    }

    /// Drop everything (e.g. on `/clear`).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for RenderCache {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Render a plain (non-markdown) transcript string into styled `Line`s the
/// way the rest of `draw` expects: pre-wrapped, one visual row per `Line`,
/// no ANSI, no collapsed newlines.
///
/// System entries (slash-command output — `/tree`, `/help`, `/status`,
/// `/doctor`, `/changelog`, `/usage`) arrive as a single multi-line string,
/// often carrying raw ANSI (e.g. `\x1b[1m` bold from `render_project_tree`).
/// The old `Line::from(Span::styled(text, …))` path put ALL of that in one
/// `Line`: the wrap-off renderer collapsed every embedded `\n` and truncated
/// the result to the pane width, and the raw `\x1b[…m` bytes leaked into the
/// buffer as visible garbage. This helper fixes that by:
///   - stripping ANSI CSI sequences (reusing [`strip_ansi`]) so no escape
///     bytes reach the renderer,
///   - splitting on `\n` so each source line is its own row,
///   - emitting lines that already fit `width` VERBATIM (preserving internal
///     whitespace so preformatted output — tree branches, aligned columns —
///     keeps its alignment), and word-wrapping only the over-wide ones so
///     they aren't silently truncated. A wrapped line keeps its leading
///     indentation on the first chunk.
///
/// Not cached (unlike markdown `Assistant`/`SubagentText` blocks): the work is
/// a cheap strip + split, matching the inline word-wrap the `User`/`ToolResult`
/// arms already do each frame. The flat 1-row-per-`Line` count model holds —
/// every emitted `Line` fits `width`.
fn plain_lines(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let stripped = strip_ansi(text);
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw in stripped.lines() {
        if raw.width() <= width {
            out.push(Line::from(Span::styled(raw.to_string(), style)));
            continue;
        }
        // Over-wide source line: word-wrap it, preserving its leading indent
        // on the first chunk so wrapped preformatted text stays aligned.
        let indent: String = raw.chars().take_while(|c| c.is_whitespace()).collect();
        let content = &raw[indent.len()..];
        for (i, chunk) in wrap::word_wrap(content, width, indent.width())
            .into_iter()
            .enumerate()
        {
            if i == 0 {
                out.push(Line::from(Span::styled(format!("{indent}{chunk}"), style)));
            } else {
                out.push(Line::from(Span::styled(chunk, style)));
            }
        }
    }
    // An empty System string (or one that is only a trailing newline) still
    // occupies one blank row, matching the old single-`Line` behaviour.
    if out.is_empty() {
        out.push(Line::from(Span::styled(String::new(), style)));
    }
    out
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

    // The still-growing assistant entry bypasses the render cache: its
    // text changes every frame, so caching it would thrash + serve a
    // stale (shorter) render. Only the LAST Assistant entry can be live
    // (TextDelta appends to it), and only while we're streaming; once
    // the turn ends the entry is settled and gets cached on the next
    // draw. Finding the last Assistant index once is O(n) and cheap
    // relative to the per-frame re-render it avoids.
    let live_assistant_idx =
        if matches!(app.phase, crate::commands::code_tui::app::Phase::Streaming) {
            app.transcript
                .iter()
                .rposition(|e| matches!(e, TranscriptEntry::Assistant(_)))
        } else {
            None
        };

    for (entry_idx, entry) in app.transcript.iter().enumerate() {
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
                //
                // Cache the render for SETTLED entries (finding #3) so we
                // don't re-parse every prior assistant block each frame;
                // the live still-growing entry bypasses the cache.
                let md_lines = if live_assistant_idx == Some(entry_idx) {
                    markdown::render(text, usable_width)
                } else {
                    app.render_cache.get_or_render(text, usable_width)
                };
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
                let handle = app.registry.find_by_name(agent_name);
                let color = handle
                    .as_ref()
                    .map(|h| theme::agent_color_for(h.color))
                    .unwrap_or(theme::MUTED);
                // Cache settled subagent text (finding #3); bypass while
                // the agent is still actively streaming (Working/Spawning)
                // so a growing block doesn't serve a stale shorter render.
                let md_lines = match handle.as_ref().map(|h| h.status()) {
                    Some(AgentStatus::Working) | Some(AgentStatus::Spawning) => {
                        markdown::render(text, usable_width)
                    }
                    _ => app.render_cache.get_or_render(text, usable_width),
                };
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
                full_output: _,
                is_error,
            } => {
                let style = if *is_error {
                    theme::error()
                } else {
                    theme::muted()
                };
                // (M3/#28) Exit glyph: `✗` for an errored tool, `✓` for a
                // successful one — a one-glance pass/fail signal before the
                // tool name. Both render in the line's style (error=red,
                // success=green-ish via muted here; the glyph itself is the
                // cue, the body stays scannable-dim).
                let glyph = if *is_error { "✗" } else { "✓" };
                let prefix = format!("{RESULT_MARKER}{glyph} {name}");
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
            // System entries carry multi-line, sometimes ANSI-decorated text
            // (slash commands like `/tree`, `/help`, `/status`, `/doctor`).
            // Render them through `plain_lines`: strip ANSI, split on '\n',
            // and wrap over-wide lines — otherwise the whole block collapses
            // to one truncated row with raw `\x1b[…m` bytes leaking in.
            TranscriptEntry::System(text) => {
                lines.extend(plain_lines(text, theme::muted(), usable_width));
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

    /// The render cache returns identical `Line`s for the same text at
    /// the same width, and re-renders when the width changes (finding #3).
    #[test]
    fn render_cache_hits_same_width_misses_on_change() {
        let mut cache = RenderCache::new();
        let text = "# Hello\nworld";
        let first = cache.get_or_render(text, 40);
        let second = cache.get_or_render(text, 40);
        // Same width → same number of lines (cache hit, no re-render).
        assert_eq!(first.len(), second.len());
        // A different width invalidates → may produce a different row count.
        let _wide = cache.get_or_render(text, 80);
        // Cache grew by exactly one distinct-text entry.
        // (Re-render at the new width replaces the same key, not adds.)
        assert_eq!(cache.entries.len(), 1);
    }

    /// The render cache evicts the oldest entry past the cap so a long
    /// session can't grow it unboundedly (finding #3).
    #[test]
    fn render_cache_evicts_at_cap() {
        let mut cache = RenderCache::new();
        for i in 0..(RENDER_CACHE_CAP + 5) {
            let _ = cache.get_or_render(&format!("# entry {i}"), 40);
        }
        assert_eq!(cache.entries.len(), RENDER_CACHE_CAP);
        // The first 5 evicted; entry 5 (index 0 now) is the oldest kept.
        let oldest = cache.entries.keys().next().unwrap();
        assert!(oldest.contains("entry 5"));
    }

    /// An unclosed code fence renders WITHOUT borders (holdback),
    /// while the same content closed renders WITH borders (finding #3).
    #[test]
    fn unclosed_code_fence_renders_without_borders() {
        let w = 40usize;
        // Closed fence → has border rows (the `─` repeat).
        let closed = markdown::render("```rs\nlet x = 1;\n```\n", w);
        let closed_has_border = closed
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('─')));
        assert!(closed_has_border, "closed fence should render a border");

        // Unclosed fence → no border rows, just the label + code.
        let open = markdown::render("```rs\nlet x = 1;\n", w);
        let open_has_border = open
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('─')));
        assert!(!open_has_border, "open fence must NOT render a border");
        // Still shows the language label.
        assert!(
            open.iter()
                .any(|l| l.spans.iter().any(|s| s.content == "rs")),
            "open fence still shows the lang label"
        );
    }

    /// Concatenate a `Line`'s span contents into a plain `String`.
    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// (Fix 1) A multi-line System string renders as SEPARATE rows — the old
    /// single-`Line` path collapsed every `\n` into one truncated row.
    #[test]
    fn plain_lines_splits_on_newline() {
        let out = plain_lines("alpha\nbeta\ngamma", theme::muted(), 80);
        assert_eq!(out.len(), 3, "one row per source line");
        assert_eq!(line_text(&out[0]), "alpha");
        assert_eq!(line_text(&out[1]), "beta");
        assert_eq!(line_text(&out[2]), "gamma");
    }

    /// (Fix 1) ANSI CSI escapes are stripped, not leaked: a `/tree`-style
    /// entry (`\x1b[1m…\x1b[0m` bold + `|-- ` connectors + newlines) renders
    /// as clean rows with NO `\x1b`/`[1m` bytes and NO newline collapse.
    #[test]
    fn plain_lines_strips_ansi_and_preserves_tree_rows() {
        let tree = "\x1b[1mmention-demo/\x1b[0m\n|-- docs/\n|   `-- README.md\n`-- src/";
        let out = plain_lines(tree, theme::muted(), 80);
        assert_eq!(out.len(), 4, "four tree rows, not one collapsed line");
        // No escape bytes or CSI remnants leak into any rendered row.
        for l in &out {
            let t = line_text(l);
            assert!(!t.contains('\x1b'), "no ESC byte should leak: {t:?}");
            assert!(!t.contains("[1m"), "no CSI remnant should leak: {t:?}");
        }
        assert_eq!(line_text(&out[0]), "mention-demo/");
        // Internal whitespace of a fitting line is preserved verbatim so the
        // tree stays aligned (word-wrap would have collapsed `|   `).
        assert_eq!(line_text(&out[2]), "|   `-- README.md");
    }

    /// (Fix 1) An over-wide source line is word-wrapped to `width` (each row
    /// fits), rather than truncated to one row by the wrap-off renderer.
    #[test]
    fn plain_lines_wraps_over_wide_line() {
        let width = 20usize;
        let long = "word ".repeat(12); // ~60 cols, one source line
        let out = plain_lines(&long, theme::muted(), width);
        assert!(out.len() > 1, "long line must wrap to multiple rows");
        for l in &out {
            assert!(
                line_text(l).width() <= width,
                "each wrapped row must fit width {width}"
            );
        }
    }

    /// (Fix 1) A wrapped over-wide line keeps its leading indentation on the
    /// first chunk (requirement d).
    #[test]
    fn plain_lines_preserves_leading_indent_on_wrap() {
        let width = 12usize;
        let out = plain_lines("    alpha beta gamma delta", theme::muted(), width);
        assert!(out.len() > 1, "must wrap");
        assert!(
            line_text(&out[0]).starts_with("    "),
            "first wrapped chunk keeps the 4-space indent: {:?}",
            line_text(&out[0])
        );
    }

    /// (Fix 1) An empty System string still occupies exactly one (blank) row,
    /// matching the old single-`Line` behaviour so counts don't shift.
    #[test]
    fn plain_lines_empty_is_one_blank_row() {
        let out = plain_lines("", theme::muted(), 40);
        assert_eq!(out.len(), 1);
        assert_eq!(line_text(&out[0]), "");
    }
}
