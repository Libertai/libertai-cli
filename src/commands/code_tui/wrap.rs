//! Shared text-wrapping helpers for the ratatui TUI.
//!
//! Currently exposes [`word_wrap`], a display-width word wrapper used by the
//! approval-preview modal (and the scrollback transcript) to pre-wrap its
//! content into explicit lines. This produces an exact line count that
//! matches what is rendered, unlike `Paragraph::wrap` (which uses
//! `WordWrapper` and can emit more lines than a naive char count predicts).
//!
//! Occupancy is tracked in *display columns* (via `unicode_width`) rather than
//! code points, so wide glyphs (CJK / emoji, 2 columns each) wrap at the same
//! screen boundary the renderer uses. This keeps the pre-wrapped line count
//! in sync with `scrollback::draw`'s visual-row count and avoids the
//! scroll-drift M4a closed. The broader pre-wrap unification across the TUI
//! is M4; this module exists so that work can reuse a single helper.

use unicode_width::UnicodeWidthStr;

/// Word-wrap `text` to at most `width` *display columns* per line. The first
/// line is shortened by `first_line_indent` (also in display columns) to
/// account for a prefix (e.g. "Preview: "). Returns a `Vec<String>` of
/// pre-wrapped lines.
pub fn word_wrap(text: &str, width: usize, first_line_indent: usize) -> Vec<String> {
    let width = width.max(1);
    let mut result: Vec<String> = Vec::new();
    let mut first_line_budget = width.saturating_sub(first_line_indent).max(1);

    for (line_idx, raw_line) in text.lines().enumerate() {
        let budget = if line_idx == 0 {
            first_line_budget
        } else {
            width
        };
        first_line_budget = width; // only the very first line is shortened

        if raw_line.is_empty() {
            result.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_len = 0usize;
        for word in raw_line.split_whitespace() {
            let word_len = word.width();
            if current.is_empty() {
                if word_len > budget {
                    // Word wider than the line — hard-break it at the
                    // display-column budget (taking whole code points whose
                    // cumulative width fits, so we never split a wide glyph).
                    let (take, rest) = split_at_width(word, budget);
                    result.push(take);
                    if !rest.is_empty() {
                        current = rest;
                        current_len = current.width();
                    }
                } else {
                    current = word.to_string();
                    current_len = word_len;
                }
            } else if current_len + 1 + word_len > width {
                // Word doesn't fit — flush current line, start new.
                result.push(std::mem::take(&mut current));
                if word_len > width {
                    let (take, rest) = split_at_width(word, width);
                    result.push(take);
                    if !rest.is_empty() {
                        current = rest;
                        current_len = current.width();
                    }
                } else {
                    current = word.to_string();
                    current_len = word_len;
                }
            } else {
                current.push(' ');
                current.push_str(word);
                current_len += 1 + word_len;
            }
        }
        if !current.is_empty() {
            result.push(current);
        }
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Split `s` into a prefix whose cumulative display width is `<= budget` and
/// the remainder, taking whole code points so a wide glyph (CJK / emoji) is
/// never split across a line boundary. Used to hard-break a single long word
/// at the column budget.
///
/// The first code point is always taken even if it alone exceeds `budget`,
/// so the wrapper always makes progress on a non-empty word (a single wide
/// glyph wider than the line still becomes its own line — same behaviour the
/// old char-count `take(budget)` had for a 1-char-longer-than-width word).
/// After that first char, each subsequent code point is taken greedily as long
/// as adding it would not overflow `budget`; zero-width code points (combining
/// marks, ZWJ) are always taken since they don't grow the column count.
fn split_at_width(s: &str, budget: usize) -> (String, String) {
    let mut take_w = 0usize;
    let mut split_idx = s.len();
    let mut first = true;
    for (idx, ch) in s.char_indices() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if !first && take_w + ch_w > budget {
            split_idx = idx;
            break;
        }
        take_w += ch_w;
        split_idx = idx + ch.len_utf8();
        first = false;
    }
    (s[..split_idx].to_string(), s[split_idx..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// Assert every line of `wrapped` has display width <= `width`.
    fn assert_within_width(wrapped: &[String], width: usize) {
        for (i, line) in wrapped.iter().enumerate() {
            assert!(
                line.width() <= width,
                "line {i} ({:?}) is {} cols wide, exceeds budget {width}",
                line,
                line.width()
            );
        }
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        let out = word_wrap("", 10, 0);
        assert_eq!(out, vec!["".to_string()]);
    }

    #[test]
    fn ascii_fits_on_one_line() {
        let out = word_wrap("hello world", 80, 0);
        assert_eq!(out, vec!["hello world".to_string()]);
    }

    #[test]
    fn ascii_wraps_on_word_boundary() {
        // "alpha beta gamma" at width 11 fits "alpha beta" (10) + space +
        // gamma would be 16, so gamma wraps.
        let out = word_wrap("alpha beta gamma", 11, 0);
        assert_eq!(out[0], "alpha beta");
        assert_eq!(out[1], "gamma");
        assert_within_width(&out, 11);
    }

    #[test]
    fn first_line_indent_shortens_budget() {
        // width 12, indent 6 -> first line budget 6.
        let out = word_wrap("abcdefghij", 12, 6);
        // First line holds at most 6 cols: "abcdef", remainder "ghij".
        assert_eq!(out[0], "abcdef");
        assert_eq!(out[1], "ghij");
        assert_within_width(&out, 12);
    }

    #[test]
    fn long_word_hard_breaks_ascii_once() {
        // A word longer than the line is hard-broken ONCE at the column
        // budget; the remainder is left as its own (possibly overflowing)
        // line — this matches the original char-count wrapper's behaviour
        // exactly. Only the first segment is guaranteed within budget.
        let out = word_wrap("supercalifragilistic", 5, 0);
        assert_eq!(out[0], "super");
        assert_eq!(out[1], "califragilistic");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].width(), 5);
    }

    #[test]
    fn cjk_full_width_fits_at_display_boundary() {
        // 5 full-width CJK chars == 10 display columns. At width 10 they
        // fit on exactly one line (char-count is also 5, so the old code
        // happened to agree here — but the point is the budget is columns).
        let s = "中文测试一";
        assert_eq!(s.width(), 10);
        let out = word_wrap(s, 10, 0);
        assert_eq!(out.len(), 1, "5 full-width chars (10 cols) fit width 10");
        assert_eq!(out[0], s);
        assert_within_width(&out, 10);
    }

    #[test]
    fn cjk_full_width_wraps_at_display_boundary() {
        // 6 full-width CJK chars == 12 display columns. At width 10 only 5
        // fit per line; the 6th must wrap. The old char-count code would
        // see 6 chars <= 10 and keep them all on one line (12 cols wide),
        // overflowing the screen and re-opening the M4a scroll drift.
        let s = "中文测试一二";
        assert_eq!(s.width(), 12);
        let out = word_wrap(s, 10, 0);
        assert_eq!(out.len(), 2, "must wrap to two 5-glyph / 10-col lines");
        assert_eq!(out[0], "中文测试一");
        assert_eq!(out[1], "二");
        assert_within_width(&out, 10);
    }

    #[test]
    fn emoji_wraps_at_display_boundary() {
        // Each emoji is 2 display columns. width 6 fits 3 emoji per line.
        let s = "😀😃😄😁😆😅"; // 6 emoji, 12 cols
        assert_eq!(s.width(), 12);
        let out = word_wrap(s, 6, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "😀😃😄");
        assert_eq!(out[1], "😁😆😅");
        assert_within_width(&out, 6);
    }

    #[test]
    fn mixed_ascii_cjk_uses_display_width() {
        // "ab中文" = 2 + 2*2 = 6 cols. At width 5: "ab中" (4 cols) then
        // "文" — the space-less hard-break keys off display width.
        let s = "ab中文";
        assert_eq!(s.width(), 6);
        let out = word_wrap(s, 5, 0);
        assert_eq!(out[0], "ab中");
        assert_eq!(out[1], "文");
        assert_within_width(&out, 5);
    }

    #[test]
    fn cjk_with_spaces_wraps_on_word_boundary() {
        // "中文 测试" -> "中文" (4 cols) + space + "测试" (4 cols) = 9.
        // width 8: "中文" (4) + 1 + 4 = 9 > 8, so "测试" wraps.
        let out = word_wrap("中文 测试", 8, 0);
        assert_eq!(out[0], "中文");
        assert_eq!(out[1], "测试");
        assert_within_width(&out, 8);
    }
}
