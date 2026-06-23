//! Shared text-wrapping helpers for the ratatui TUI.
//!
//! Currently exposes [`word_wrap`], a char-budget word wrapper used by the
//! approval-preview modal to pre-wrap its content into explicit lines. This
//! produces an exact line count that matches what is rendered, unlike
//! `Paragraph::wrap` (which uses `WordWrapper` and can emit more lines than a
//! naive char count predicts). The broader pre-wrap unification across the
//! TUI is M4; this module exists so that work can reuse a single helper.

/// Word-wrap `text` to at most `width` chars per line. The first line
/// is shortened by `first_line_indent` to account for a prefix (e.g.
/// "Preview: "). Returns a `Vec<String>` of pre-wrapped lines.
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
            let word_len = word.chars().count();
            if current.is_empty() {
                if word_len > budget {
                    // Word longer than the line — hard-break it.
                    let mut chars = word.chars();
                    let take: String = chars.by_ref().take(budget).collect();
                    result.push(take);
                    let rest: String = chars.collect();
                    if !rest.is_empty() {
                        current_len = rest.chars().count();
                        current = rest;
                    }
                } else {
                    current = word.to_string();
                    current_len = word_len;
                }
            } else if current_len + 1 + word_len > width {
                // Word doesn't fit — flush current line, start new.
                result.push(std::mem::take(&mut current));
                if word_len > width {
                    let mut chars = word.chars();
                    let take: String = chars.by_ref().take(width).collect();
                    result.push(take);
                    let rest: String = chars.collect();
                    if !rest.is_empty() {
                        current_len = rest.chars().count();
                        current = rest;
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
