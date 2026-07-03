//! Soft-wrap geometry for the input bar.
//!
//! The input editor keeps `tui_textarea::TextArea` as the *model* but the
//! bar renders wrapped visual rows itself (input.rs) and sizes itself by
//! wrapped row count (view.rs). Both sides MUST use this module for every
//! geometry question — width, row layout, cursor mapping, scroll — so the
//! height the layout allocates and the rows the renderer draws can never
//! disagree (B4-INPUT-WIDTH; same lesson as the B3 Fix 6 `FooterLayout`).
//!
//! This is a *character* wrapper, not a word wrapper: cursor↔cell math has
//! to be exact and reversible, which `wrap::word_wrap` (whitespace-lossy)
//! cannot provide. Occupancy is tracked in display columns via
//! `unicode-width`, and a wide glyph (CJK / emoji, 2 cols) is never split
//! across rows.

use unicode_width::UnicodeWidthChar;

/// One visual row of the wrapped input: a char-index slice of logical line
/// `line_idx`. `start_char..end_char` are *char* offsets (tui-textarea's
/// cursor column unit), not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    pub line_idx: usize,
    pub start_char: usize,
    pub end_char: usize,
}

/// Width in display columns available to the editor text inside the input
/// bar: the bar width minus the 2-col `❯ ` prompt gutter (input.rs).
/// Floored at 1 so degenerate terminals still make progress.
pub fn input_wrap_width(bar_width: u16) -> usize {
    (bar_width as usize).saturating_sub(2).max(1)
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Wrap `lines` (the textarea's logical lines) at `width` display columns.
/// Every logical line yields at least one row (an empty line yields one
/// empty row); a line exactly at `width` yields one row, no phantom
/// continuation. The first char of a row is always taken even if it alone
/// exceeds `width` (a 2-col glyph at width 1) so wrapping always makes
/// progress.
pub fn wrap_layout(lines: &[String], width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (line_idx, line) in lines.iter().enumerate() {
        let mut start_char = 0usize;
        let mut row_cols = 0usize;
        let mut row_chars = 0usize;
        let mut total_chars = 0usize;
        for (ci, ch) in line.chars().enumerate() {
            let cw = char_width(ch);
            if row_chars > 0 && row_cols + cw > width {
                rows.push(VisualRow {
                    line_idx,
                    start_char,
                    end_char: ci,
                });
                start_char = ci;
                row_cols = 0;
                row_chars = 0;
            }
            row_cols += cw;
            row_chars += 1;
            total_chars = ci + 1;
        }
        rows.push(VisualRow {
            line_idx,
            start_char,
            end_char: total_chars,
        });
    }
    if rows.is_empty() {
        rows.push(VisualRow {
            line_idx: 0,
            start_char: 0,
            end_char: 0,
        });
    }
    rows
}

/// Slice logical line `line` to the char range of `row`. Char offsets are
/// converted to byte offsets here, in one place.
pub fn row_text<'a>(line: &'a str, row: &VisualRow) -> &'a str {
    let mut it = line.char_indices();
    let start = it.nth(row.start_char).map(|(b, _)| b).unwrap_or(line.len());
    let end = if row.end_char > row.start_char {
        line.char_indices()
            .nth(row.end_char)
            .map(|(b, _)| b)
            .unwrap_or(line.len())
    } else {
        start
    };
    &line[start..end]
}

/// Map tui-textarea's logical cursor `(row, col-in-chars)` to
/// `(visual_row_index, col-in-display-cells)`. A cursor sitting exactly on
/// a wrap boundary belongs to the *following* row (it renders at col 0 of
/// the continuation), except at the very end of the logical line where it
/// stays on the line's last row.
pub fn visual_cursor(
    layout: &[VisualRow],
    lines: &[String],
    cursor: (usize, usize),
) -> (usize, usize) {
    let (crow, ccol) = cursor;
    for (vidx, vr) in layout.iter().enumerate() {
        if vr.line_idx != crow {
            continue;
        }
        let last_row_of_line = layout
            .get(vidx + 1)
            .is_none_or(|next| next.line_idx != crow);
        let owns_cursor = if last_row_of_line {
            ccol >= vr.start_char
        } else {
            ccol >= vr.start_char && ccol < vr.end_char
        };
        if owns_cursor {
            let line = lines.get(crow).map(String::as_str).unwrap_or("");
            let cells: usize = line
                .chars()
                .skip(vr.start_char)
                .take(ccol.saturating_sub(vr.start_char))
                .map(char_width)
                .sum();
            return (vidx, cells);
        }
    }
    // Defensive: cursor row outside the layout (should not happen — the
    // layout is rebuilt from the same lines every frame).
    (0, 0)
}

/// Keep-cursor-visible vertical scroll for the input viewport. `prev` is
/// the previous first-visible visual row; returns the new one. Pulls the
/// window up/down just enough to contain `cursor_vrow`, clamped so the
/// window never scrolls past the content.
pub fn clamp_input_scroll(
    prev: usize,
    cursor_vrow: usize,
    height: usize,
    total_rows: usize,
) -> usize {
    let height = height.max(1);
    let max_scroll = total_rows.saturating_sub(height);
    let mut scroll = prev.min(max_scroll);
    if cursor_vrow < scroll {
        scroll = cursor_vrow;
    } else if cursor_vrow >= scroll + height {
        scroll = cursor_vrow + 1 - height;
    }
    scroll.min(max_scroll)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Invariant: every row's slice renders within `width` display columns.
    fn assert_rows_within_width(layout: &[VisualRow], ls: &[String], width: usize) {
        for (i, row) in layout.iter().enumerate() {
            let text = row_text(&ls[row.line_idx], row);
            assert!(
                text.width() <= width,
                "row {i} ({text:?}) is {} cols, exceeds {width}",
                text.width()
            );
        }
    }

    #[test]
    fn empty_buffer_yields_one_empty_row() {
        let ls = lines(&[""]);
        let layout = wrap_layout(&ls, 10);
        assert_eq!(
            layout,
            vec![VisualRow {
                line_idx: 0,
                start_char: 0,
                end_char: 0
            }]
        );
        assert_eq!(visual_cursor(&layout, &ls, (0, 0)), (0, 0));
    }

    #[test]
    fn no_lines_at_all_yields_one_empty_row() {
        let layout = wrap_layout(&[], 10);
        assert_eq!(layout.len(), 1);
    }

    #[test]
    fn short_line_is_one_row() {
        let ls = lines(&["hello"]);
        let layout = wrap_layout(&ls, 10);
        assert_eq!(layout.len(), 1);
        assert_eq!(row_text(&ls[0], &layout[0]), "hello");
    }

    #[test]
    fn exact_width_line_has_no_phantom_row() {
        let ls = lines(&["abcdefghij"]); // exactly 10
        let layout = wrap_layout(&ls, 10);
        assert_eq!(layout.len(), 1);
        // Cursor at end-of-line stays on the last row, at col == width.
        assert_eq!(visual_cursor(&layout, &ls, (0, 10)), (0, 10));
    }

    #[test]
    fn long_line_wraps_by_chars() {
        let ls = lines(&["abcdefghijklm"]); // 13 chars at width 5 → 5,5,3
        let layout = wrap_layout(&ls, 5);
        assert_eq!(layout.len(), 3);
        assert_eq!(row_text(&ls[0], &layout[0]), "abcde");
        assert_eq!(row_text(&ls[0], &layout[1]), "fghij");
        assert_eq!(row_text(&ls[0], &layout[2]), "klm");
        assert_rows_within_width(&layout, &ls, 5);
    }

    #[test]
    fn cursor_on_wrap_boundary_belongs_to_following_row() {
        let ls = lines(&["abcdefghij"]); // width 5 → "abcde" + "fghij"
        let layout = wrap_layout(&ls, 5);
        assert_eq!(layout.len(), 2);
        // col 5 is the boundary: renders at col 0 of row 1.
        assert_eq!(visual_cursor(&layout, &ls, (0, 5)), (1, 0));
        assert_eq!(visual_cursor(&layout, &ls, (0, 4)), (0, 4));
        // End of line: stays on the last row.
        assert_eq!(visual_cursor(&layout, &ls, (0, 10)), (1, 5));
    }

    #[test]
    fn wide_glyphs_never_split() {
        // 6 CJK chars = 12 cols; width 5 fits 2 per row (4 cols).
        let ls = lines(&["中文测试一二"]);
        let layout = wrap_layout(&ls, 5);
        assert_eq!(layout.len(), 3);
        assert_eq!(row_text(&ls[0], &layout[0]), "中文");
        assert_rows_within_width(&layout, &ls, 5);
        // Cursor after the first glyph: 2 display cells.
        assert_eq!(visual_cursor(&layout, &ls, (0, 1)), (0, 2));
        // Cursor on the boundary char (index 2) → row 1 col 0.
        assert_eq!(visual_cursor(&layout, &ls, (0, 2)), (1, 0));
    }

    #[test]
    fn width_one_degenerate_still_progresses() {
        let ls = lines(&["中文"]); // 2-col glyphs at width 1
        let layout = wrap_layout(&ls, 1);
        // One glyph per row (first char always taken).
        assert_eq!(layout.len(), 2);
        // width 0 is floored to 1.
        let layout0 = wrap_layout(&ls, 0);
        assert_eq!(layout0.len(), 2);
    }

    #[test]
    fn multiple_logical_lines_interleave() {
        let ls = lines(&["abcdefgh", "", "xy"]); // width 4: 2 + 1 + 1 rows
        let layout = wrap_layout(&ls, 4);
        assert_eq!(layout.len(), 4);
        assert_eq!(layout[0].line_idx, 0);
        assert_eq!(layout[1].line_idx, 0);
        assert_eq!(
            layout[2],
            VisualRow {
                line_idx: 1,
                start_char: 0,
                end_char: 0
            }
        );
        assert_eq!(layout[3].line_idx, 2);
        // Cursor on the empty middle line.
        assert_eq!(visual_cursor(&layout, &ls, (1, 0)), (2, 0));
        // Cursor on line 2.
        assert_eq!(visual_cursor(&layout, &ls, (2, 1)), (3, 1));
    }

    #[test]
    fn cursor_past_end_of_line_is_defensive() {
        let ls = lines(&["abc"]);
        let layout = wrap_layout(&ls, 10);
        // tui-textarea clamps, but be defensive: col 99 maps to end.
        assert_eq!(visual_cursor(&layout, &ls, (0, 99)), (0, 3));
    }

    #[test]
    fn input_wrap_width_reserves_gutter() {
        assert_eq!(input_wrap_width(80), 78);
        assert_eq!(input_wrap_width(3), 1);
        assert_eq!(input_wrap_width(2), 1);
        assert_eq!(input_wrap_width(0), 1);
    }

    #[test]
    fn scroll_clamps_and_follows_cursor() {
        // 10 rows, window of 3.
        assert_eq!(clamp_input_scroll(0, 0, 3, 10), 0);
        // Cursor below window → pull down.
        assert_eq!(clamp_input_scroll(0, 5, 3, 10), 3);
        // Cursor above window → pull up.
        assert_eq!(clamp_input_scroll(6, 2, 3, 10), 2);
        // Cursor at last row.
        assert_eq!(clamp_input_scroll(0, 9, 3, 10), 7);
        // Stale large prev clamps to max_scroll.
        assert_eq!(clamp_input_scroll(50, 9, 3, 10), 7);
        // Content shorter than window → 0.
        assert_eq!(clamp_input_scroll(4, 1, 6, 3), 0);
        // Zero height floored.
        assert_eq!(clamp_input_scroll(0, 2, 0, 3), 2);
    }

    #[test]
    fn row_text_char_indices_are_exact() {
        let ls = lines(&["a中b文c"]);
        let layout = wrap_layout(&ls, 3); // widths: a=1,中=2 (3) | b=1,文=2 (3) | c
        assert_eq!(layout.len(), 3);
        assert_eq!(row_text(&ls[0], &layout[0]), "a中");
        assert_eq!(row_text(&ls[0], &layout[1]), "b文");
        assert_eq!(row_text(&ls[0], &layout[2]), "c");
    }
}
