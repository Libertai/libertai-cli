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

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::commands::code_tui::theme;

/// Parse a markdown string into a list of ratatui lines.
pub fn render(text: &str) -> Vec<Line<'static>> {
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
            lines.extend(render_code_block(&code_lines, lang));
            continue;
        }

        // Heading # .. ######
        if let Some(stripped) = line.strip_prefix("# ") {
            lines.push(heading(stripped, 1));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("## ") {
            lines.push(heading(stripped, 2));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("### ") {
            lines.push(heading(stripped, 3));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("#### ") {
            lines.push(heading(stripped, 4));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("##### ") {
            lines.push(heading(stripped, 5));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("###### ") {
            lines.push(heading(stripped, 6));
            continue;
        }

        // Horizontal rule
        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            lines.push(Line::from(Span::styled(
                "─".repeat(40),
                theme::muted(),
            )));
            continue;
        }

        // Blockquote
        if let Some(stripped) = line.strip_prefix("> ") {
            let spans = parse_inline(stripped);
            let mut line_spans = vec![Span::styled("▎ ".to_string(), theme::muted())];
            line_spans.extend(spans);
            lines.push(Line::from(line_spans));
            continue;
        }

        // Unordered list
        if let Some(stripped) = line.strip_prefix("- ") {
            lines.push(list_item("  • ", stripped, 0));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("* ") {
            lines.push(list_item("  • ", stripped, 0));
            continue;
        }

        // Ordered list (1. 2. ...)
        if let Some(after) = line.strip_prefix(|c: char| c.is_ascii_digit()) {
            if let Some(stripped) = after.strip_prefix(". ") {
                let num: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
                lines.push(list_item(&format!("  {num}. "), stripped, 0));
                continue;
            }
        }

        // Blank line
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        // Normal paragraph
        lines.push(Line::from(parse_inline(line)));
    }

    lines
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
                    spans.push(Span::styled(
                        code,
                        theme::accent().add_modifier(Modifier::BOLD),
                    ));
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

/// Render a heading line.
fn heading(text: &str, _level: usize) -> Line<'static> {
    let style = theme::bold();
    Line::from(Span::styled(text.to_string(), style))
}

/// Render a list item with the given bullet and indent.
fn list_item(bullet: &str, text: &str, _indent: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(bullet.to_string(), theme::accent())];
    spans.extend(parse_inline(text));
    Line::from(spans)
}

/// Render a code block with dim border lines.
fn render_code_block(code: &[&str], _lang: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "─".repeat(40),
        theme::muted(),
    )));
    for code_line in code {
        lines.push(Line::from(Span::styled(
            format!("  {code_line}"),
            theme::accent(),
        )));
    }
    lines.push(Line::from(Span::styled(
        "─".repeat(40),
        theme::muted(),
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text() {
        let lines = render("hello world");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_heading() {
        let lines = render("# Title");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_bold() {
        let lines = render("**bold**");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_inline_code() {
        let lines = render("use `fmt` module");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_code_block() {
        let lines = render("```rust\nfn main() {}\n```");
        assert_eq!(lines.len(), 3); // top border + code + bottom border
    }

    #[test]
    fn renders_list() {
        let lines = render("- item one\n- item two");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn renders_blockquote() {
        let lines = render("> quoted text");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn renders_hr() {
        let lines = render("---");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn handles_empty_input() {
        let lines = render("");
        assert_eq!(lines.len(), 0);
    }

    #[test]
    fn handles_unclosed_bold() {
        let lines = render("**unclosed");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn handles_unclosed_code() {
        let lines = render("`unclosed");
        assert_eq!(lines.len(), 1);
    }
}
