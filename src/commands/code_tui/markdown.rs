//! Lightweight markdown-to-`Vec<Line>` renderer.
//!
//! No external crate — ratatui-markdown pins ratatui 0.29 which is
//! incompatible with our 0.30. This module handles the common
//! markdown constructs an LLM produces: headings, bold, italic,
//! inline code, code fences, blockquotes, lists, and hr. It returns
//! ratatui `Line`/`Span` values styled via [`theme`].
//!
//! Block-level parsing is line-oriented. Inline parsing is a small
//! state machine over `**`, `*`, `_`, `` ` ``, `~~`, and `[text](url)`.
//!
//! Rendering is pre-wrapped: every logical (paragraph / list /
//! blockquote) line is word-wrapped to `width` here, so the ratatui
//! `Paragraph` widget above wraps NOTHING. Code-block lines are left
//! hard (no soft-wrap) — they may overflow or hard-break at `width`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::commands::code_tui::{theme, wrap};

/// Parse a markdown string into a list of ratatui lines, pre-wrapped
/// to `width` usable columns. `width` is clamped to >= 1.
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut iter = text.lines().peekable();

    while let Some(line) = iter.next() {
        // Fenced code block ```lang ... ```
        if line.trim_start().starts_with("```") {
            let lang = line.trim_start().trim_start_matches('`').trim();
            let mut code_lines = Vec::new();
            for code_line in iter.by_ref() {
                if code_line.trim_start().starts_with("```") {
                    break;
                }
                code_lines.push(code_line);
            }
            lines.extend(render_code_block(&code_lines, lang, width));
            continue;
        }

        // CommonMark tolerates up to 3 leading spaces of indentation
        // before block-structure prefixes (headings, blockquotes, list
        // markers, hr). Strip them; the consumed width is returned for
        // M4b nested-list indent detection (unused here in M4a, where
        // list indent comes from the explicit `list_item` param).
        let (stripped, _leading) = strip_leading_indent(line);

        // Heading # .. ######
        if let Some(s) = stripped.strip_prefix("# ") {
            lines.push(heading(s, 1));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("## ") {
            lines.push(heading(s, 2));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("### ") {
            lines.push(heading(s, 3));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("#### ") {
            lines.push(heading(s, 4));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("##### ") {
            lines.push(heading(s, 5));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("###### ") {
            lines.push(heading(s, 6));
            continue;
        }

        // Horizontal rule
        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.saturating_sub(2)),
                theme::muted(),
            )));
            continue;
        }

        // Blockquote
        if let Some(s) = stripped.strip_prefix("> ") {
            let spans = parse_inline(s);
            lines.extend(wrap_spans(spans, "▎ ", theme::muted(), width));
            continue;
        }

        // Unordered list
        if let Some(s) = stripped.strip_prefix("- ") {
            lines.extend(list_item("  • ", s, 0, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("* ") {
            lines.extend(list_item("  • ", s, 0, width));
            continue;
        }

        // Ordered list (1. 2. ...)
        if let Some(after) = stripped.strip_prefix(|c: char| c.is_ascii_digit()) {
            if let Some(s) = after.strip_prefix(". ") {
                let num: String = stripped
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                lines.extend(list_item(&format!("  {num}. "), s, 0, width));
                continue;
            }
        }

        // Blank line
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        // Normal paragraph
        lines.extend(wrap_spans(parse_inline(stripped), "", theme::primary(), width));
    }

    lines
}

/// Strip up to 3 leading spaces (CommonMark indent limit) and return
/// the trimmed line plus the consumed display width (for M4b nested
/// lists). Lines with 4+ leading spaces are indented code blocks —
/// out of scope for M4a; we treat them as plain paragraphs.
fn strip_leading_indent(line: &str) -> (&str, usize) {
    let mut consumed = 0;
    let mut chars = line.chars();
    let mut count = 0;
    while count < 3 {
        match chars.next() {
            Some(' ') => {
                count += 1;
                consumed += 1;
            }
            _ => break,
        }
    }
    (&line[consumed..], consumed)
}

/// Render inline markdown (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`,
/// `[text](url)`) into styled spans. All spans are `'static` (owned).
fn parse_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::raw(std::mem::take(buf)));
        }
    };

    while let Some(c) = chars.next() {
        match c {
            '`' => {
                flush(&mut buf, &mut spans);
                let mut code = String::new();
                let mut found_close = false;
                while let Some(&next) = chars.peek() {
                    if next == '`' {
                        chars.next();
                        found_close = true;
                        break;
                    }
                    code.push(next);
                    chars.next();
                }
                if found_close {
                    spans.push(Span::styled(code, theme::code()));
                } else {
                    spans.push(Span::raw(format!("`{code}")));
                }
            }
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // consume second '*'
                flush(&mut buf, &mut spans);
                let mut bold = String::new();
                let mut found_close = false;
                while let Some(&next) = chars.peek() {
                    if next == '*' && chars.clone().nth(1) == Some('*') {
                        chars.next();
                        chars.next();
                        found_close = true;
                        break;
                    }
                    bold.push(next);
                    chars.next();
                }
                if found_close {
                    spans.push(Span::styled(bold, theme::bold()));
                } else {
                    spans.push(Span::raw(format!("**{bold}")));
                }
            }
            '*' | '_' => {
                let marker = c;
                flush(&mut buf, &mut spans);
                let mut italic = String::new();
                let mut found_close = false;
                while let Some(&next) = chars.peek() {
                    if next == marker {
                        chars.next();
                        found_close = true;
                        break;
                    }
                    italic.push(next);
                    chars.next();
                }
                if found_close {
                    spans.push(Span::styled(
                        italic,
                        Style::default().add_modifier(Modifier::ITALIC),
                    ));
                } else {
                    spans.push(Span::raw(format!("{marker}{italic}")));
                }
            }
            '~' if chars.peek() == Some(&'~') => {
                chars.next();
                flush(&mut buf, &mut spans);
                let mut strike = String::new();
                let mut found_close = false;
                while let Some(&next) = chars.peek() {
                    if next == '~' && chars.clone().nth(1) == Some('~') {
                        chars.next();
                        chars.next();
                        found_close = true;
                        break;
                    }
                    strike.push(next);
                    chars.next();
                }
                if found_close {
                    spans.push(Span::styled(
                        strike,
                        Style::default().add_modifier(Modifier::CROSSED_OUT),
                    ));
                } else {
                    spans.push(Span::raw(format!("~~{strike}")));
                }
            }
            '[' => {
                flush(&mut buf, &mut spans);
                let mut label = String::new();
                let mut found_label = false;
                while let Some(&next) = chars.peek() {
                    if next == ']' {
                        chars.next();
                        found_label = true;
                        break;
                    }
                    label.push(next);
                    chars.next();
                }
                if found_label && chars.peek() == Some(&'(') {
                    chars.next();
                    let mut url = String::new();
                    while let Some(&next) = chars.peek() {
                        if next == ')' {
                            chars.next();
                            break;
                        }
                        url.push(next);
                        chars.next();
                    }
                    spans.push(Span::styled(
                        format!("{label} ({url})"),
                        theme::accent().add_modifier(Modifier::UNDERLINED),
                    ));
                } else {
                    spans.push(Span::raw(format!("[{label}")));
                }
            }
            _ => buf.push(c),
        }
    }

    flush(&mut buf, &mut spans);
    spans
}

/// Render a heading line, styled by level via [`theme::heading`].
fn heading(text: &str, level: usize) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), theme::heading(level)))
}

/// Render a list item with the given bullet and indent, pre-wrapped
/// to `width`.
///
/// `indent` is the number of 2-space units to prefix before the bullet
/// (so M4b nested lists can pass their depth). Current call sites pass
/// 0, so behavior is unchanged for now; wiring the param here lets M4b
/// pass a non-zero value without touching this function again. The
/// wrapped prefix is `  `.repeat(indent) + bullet.
fn list_item(bullet: &str, text: &str, _indent: usize, width: usize) -> Vec<Line<'static>> {
    let prefix = format!("{}{}", "  ".repeat(_indent), bullet);
    wrap_spans(parse_inline(text), &prefix, theme::accent(), width)
}

/// Render a code block with dim border lines and a dim header naming
/// the language. Borders are `width`-aware (was hardcoded 40). Code
/// lines are emitted hard — one `Line` each — and are NOT soft-wrapped;
/// a line wider than `width` overflows or hard-breaks at `width`.
fn render_code_block(code: &[&str], lang: &str, width: usize) -> Vec<Line<'static>> {
    let border = "─".repeat(width.saturating_sub(2));
    let mut lines = Vec::new();
    // Dim header naming the language (or "(code)" if empty) above the
    // top border, so fenced blocks read as code at a glance.
    let label = if lang.is_empty() { "(code)" } else { lang };
    lines.push(Line::from(Span::styled(label.to_string(), theme::muted())));
    lines.push(Line::from(Span::styled(border.clone(), theme::muted())));
    for code_line in code {
        lines.push(Line::from(Span::styled(
            format!("  {code_line}"),
            theme::accent(),
        )));
    }
    lines.push(Line::from(Span::styled(border, theme::muted())));
    lines
}

/// Word-wrap a rendered inline-span line to `width`.
///
/// `prefix` is the leading cell(s) (bullet, blockquote bar, or "" for
/// paragraphs), rendered with `prefix_style`. If the prefix + content
/// fits in `width`, the original styled spans (prefix + `spans`) are
/// emitted as a single `Line`, preserving inline styling. If it
/// overflows, the content text is re-wrapped via [`wrap::word_wrap`]
/// and emitted as one `Line` per chunk; the first chunk carries
/// `prefix` (styled as before) and the rest carry only the wrapped
/// text as raw spans. Inline styling on wrapped continuation lines is
/// lost — acceptable for M4a; richer wrapping is M4b.
fn wrap_spans(
    spans: Vec<Span<'static>>,
    prefix: &str,
    prefix_style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let prefix_w = prefix.width();
    let content: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let content_w = content.width();

    // Fits (or no width budget for content) — emit the styled line
    // exactly as before, behavior-preserving for short inputs.
    if prefix_w + content_w <= width {
        let mut line_spans = Vec::with_capacity(spans.len() + 1);
        if !prefix.is_empty() {
            line_spans.push(Span::styled(prefix.to_string(), prefix_style));
        }
        line_spans.extend(spans);
        return vec![Line::from(line_spans)];
    }

    // Overflow — pre-wrap the content to the remaining budget.
    let usable = width.saturating_sub(prefix_w).max(1);
    let wrapped = wrap::word_wrap(&content, usable, 0);
    let mut out = Vec::with_capacity(wrapped.len());
    for (i, chunk) in wrapped.into_iter().enumerate() {
        if i == 0 && !prefix.is_empty() {
            let mut line_spans = vec![Span::styled(prefix.to_string(), prefix_style)];
            line_spans.push(Span::raw(chunk));
            out.push(Line::from(line_spans));
        } else {
            out.push(Line::from(Span::raw(chunk)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn renders_plain_text() {
        let lines = render("hello world", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_heading() {
        let lines = render("# Title", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_bold() {
        let lines = render("**bold**", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_inline_code() {
        let lines = render("use `fmt` module", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_code_block() {
        let lines = render("```rust\nfn main() {}\n```", 80);
        // header + top border + code + bottom border
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn renders_list() {
        let lines = render("- item one\n- item two", 80);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn renders_blockquote() {
        let lines = render("> quoted text", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_hr() {
        let lines = render("---", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn handles_empty_input() {
        let lines = render("", 80);
        assert_eq!(lines.len(), 0);
    }

    #[test]
    fn handles_unclosed_bold() {
        let lines = render("**unclosed", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn handles_unclosed_code() {
        let lines = render("`unclosed", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn pre_wraps_long_paragraph() {
        let lines = render(
            "word ".repeat(20).trim_end(),
            20,
        );
        // ~5 chars/word * 4 words per 20-col line -> 5 lines
        assert!(lines.len() > 1);
    }

    #[test]
    fn clamps_width_to_one() {
        let lines = render("hello", 0);
        assert!(!lines.is_empty());
    }

    #[test]
    fn heading_uses_level_style() {
        // H1 -> bold accent; just ensure it still renders one line.
        let lines = render("# A\n## B\n### C", 80);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn code_block_header_names_lang() {
        let lines = render("```python\nprint(1)\n```", 80);
        // header "python" + top border + code + bottom border
        assert_eq!(lines.len(), 4);
    }

    // ---- M4a: pre-wrap, heading/code styling tests ----

    #[test]
    fn wraps_long_paragraph_to_many_lines() {
        // A normal sentence with spaces forces *soft* wraps.
        let sentence = "word ".repeat(20).trim_end().to_string();
        // At width 40 the ~99-col sentence wraps to several lines.
        let narrow = render(&sentence, 40);
        assert!(
            narrow.len() > 1,
            "narrow render should wrap to >1 line, got {}",
            narrow.len()
        );
        // At width 200 the whole sentence fits on a single line.
        let wide = render(&sentence, 200);
        assert_eq!(wide.len(), 1, "wide render should fit on 1 line");
    }

    #[test]
    fn hard_breaks_long_word_no_spaces() {
        // A single 200-char word has no soft break point: it must
        // hard-break. At width 40 this produces >1 line; at width 200
        // it fits on one line.
        let long_word = "x".repeat(200);
        let narrow = render(&long_word, 40);
        assert!(narrow.len() > 1, "long word should hard-break to >1 line at width 40");
        let wide = render(&long_word, 200);
        assert_eq!(wide.len(), 1, "long word fits on 1 line at width 200");
    }

    #[test]
    fn hr_width_follows_render_width() {
        // HR content is width-2 `─` glyphs, not the old hardcoded 40.
        let lines = render("---", 60);
        assert_eq!(lines.len(), 1, "hr renders a single line");
        let content_width: usize = lines[0]
            .spans
            .iter()
            .map(|s| s.content.width())
            .sum();
        assert_eq!(
            content_width, 58,
            "hr content width should be width-2 (58), got {content_width}"
        );
    }

    #[test]
    fn code_block_does_not_soft_wrap() {
        // A fenced code block whose code line is wider than `width`
        // must NOT soft-wrap: one Line per code line (it may hard-break
        // or overflow, but never becomes 2 soft-wrapped lines).
        let long_code = "x".repeat(50);
        let src = format!("```rust\n{long_code}\n```");
        let lines = render(&src, 20);
        // header + top border + 1 code line + bottom border == 4 lines.
        assert_eq!(
            lines.len(),
            4,
            "code block should be header + border + 1 code + border, got {}",
            lines.len()
        );
        // The header line names the language and is dim (muted).
        let header = &lines[0];
        assert_eq!(header.spans.len(), 1);
        assert_eq!(header.spans[0].style, theme::muted());
        assert!(
            header.spans[0].content.contains("rust"),
            "header should name the lang 'rust', got {:?}",
            header.spans[0].content
        );
        // The single code line (index 2) is wider than `width` 20 —
        // i.e. it was NOT broken to fit (no soft-wrap).
        let code_line = &lines[2];
        let code_w: usize = code_line.spans.iter().map(|s| s.content.width()).sum();
        assert!(
            code_w > 20,
            "code line should overflow width 20 (no soft-wrap), got {code_w}"
        );
    }

    #[test]
    fn heading_levels_use_distinct_styles() {
        // H1 -> bold_accent (Cyan + BOLD); H3 -> bold_muted (DarkGray +
        // BOLD). The Span styles must differ.
        let h1 = render("# a", 80);
        let h3 = render("### c", 80);
        assert_eq!(h1.len(), 1);
        assert_eq!(h3.len(), 1);
        let h1_style = h1[0].spans[0].style;
        let h3_style = h3[0].spans[0].style;
        assert_ne!(h1_style, h3_style, "h1 and h3 styles must differ");
        assert_eq!(h1_style, theme::bold_accent(), "h1 uses bold_accent");
        assert_eq!(h3_style, theme::bold_muted(), "h3 uses bold_muted");
        // Sanity: the difference is the fg color.
        assert_eq!(h1_style.fg, Some(Color::Cyan));
        assert_eq!(h3_style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn inline_code_uses_code_style_not_accent_bold() {
        // Inline `code` spans must use theme::code() (muted + bold),
        // NOT theme::accent() + BOLD (which is the prompt/bullet style).
        let lines = render("use `fmt`", 80);
        assert_eq!(lines.len(), 1);
        // spans: raw "use " + styled "fmt".
        let spans = &lines[0].spans;
        assert!(
            spans.iter().any(|s| s.content.as_ref() == "fmt"),
            "should contain a 'fmt' code span"
        );
        let code_span = spans
            .iter()
            .find(|s| s.content.as_ref() == "fmt")
            .expect("fmt span present");
        assert_eq!(
            code_span.style,
            theme::code(),
            "inline code uses theme::code()"
        );
        assert_ne!(
            code_span.style,
            theme::accent().add_modifier(Modifier::BOLD),
            "inline code is NOT accent+BOLD"
        );
    }
}
