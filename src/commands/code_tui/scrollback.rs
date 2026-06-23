//! Scrollback transcript — renders the conversation history with
//! markdown formatting and a scrollbar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
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
                            Span::styled(agent_name.clone(), ratatui::style::Style::default().fg(color).add_modifier(ratatui::style::Modifier::BOLD)),
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
                let preview =
                    crate::commands::code_tool_preview::tool_preview(tool_name, args);
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
                let prefix_w = prefix.chars().count() + 1; // prefix + ": "
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
            TranscriptEntry::Blank => {
                lines.push(Line::from(""));
            }
        }
    }

    // Paragraph no longer wraps (markdown::render pre-wraps; User text is
    // pre-wrapped above).  We must still count *visual* rows for the scroll
    // calculation: a pre-wrapped markdown Line already fits usable_width so
    // it is exactly one row, but the single-Line entries (Tool / System /
    // AutoAllowed / SubagentTool / SubagentEnd) are NOT pre-wrapped and may
    // still exceed usable_width.  Use DISPLAY width (not char count) so
    // wide CJK / emoji glyphs do not throw the count off, and ceil-divide by
    // usable_width — minimum 1, consistent with the pre-wrapped markdown
    // Lines whose width is <= usable_width (rows == 1).
    let total_visual_lines: usize = lines
        .iter()
        .map(|line| {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            if w == 0 {
                1
            } else {
                ((w + usable_width.saturating_sub(1)) / usable_width.max(1)).max(1)
            }
        })
        .sum();

    // `app.scroll` is "offset from bottom" (0 = latest).  But
    // `Paragraph::scroll()` expects "offset from top" in *visual* lines.
    //   scroll_from_top = max(0, total_visual_lines − viewport − scroll_from_bottom)
    let viewport = area.height as usize;
    let max_from_top = total_visual_lines.saturating_sub(viewport);
    let scroll_from_top =
        max_from_top.saturating_sub(app.scroll as usize).min(max_from_top);

    // Render with scroll.  No `.wrap()`: content is already pre-wrapped to
    // usable_width, and leaving wrap off stops ratatui from double-counting
    // (and thus drifting the scroll position against the row count above).
    let paragraph = Paragraph::new(lines)
        .scroll((scroll_from_top as u16, 0));
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
