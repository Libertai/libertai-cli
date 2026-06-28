//! Syntax highlighting for code blocks + diff lines (finding #6).
//!
//! `syntect` is already compiled into the build via `rich_rust` (default
//! features: bundled `SyntaxSet` + `ThemeSet`, `onig` backend). We declare
//! it as a direct dep here and expose a tiny lazy-initialized highlighter
//! that maps a syntect-styled line into ratatui `Span`s.
//!
//! The `SyntaxSet` / `ThemeSet` are loaded ONCE behind `once_cell::Lazy`
//! so the (small) parse cost per code line is the only per-frame work —
//! and after the render cache (finding #3) settled blocks aren't
//! re-parsed at all.
//!
//! Coloring maps syntect's `Color` to ratatui `Color` per-style-scope;
//! themes use a dark background so we pick a theme whose highlights read
//! on the terminal's background.

use once_cell::sync::Lazy;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Bundled syntax set (all common languages, newline-aware so multi-line
/// strings/comments highlight across lines within a block).
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(|| SyntaxSet::load_defaults_newlines());

/// Bundled theme set. We use `base16-ocean.dark` for its muted,
/// terminal-friendly palette; fall back to the first theme if missing.
static THEME: Lazy<&'static Theme> = Lazy::new(|| {
    static THEME_SET: Lazy<ThemeSet> = Lazy::new(|| ThemeSet::load_defaults());
    THEME_SET
        .themes
        .get("base16-ocean.dark")
        .or_else(|| THEME_SET.themes.values().next())
        .expect("syntect ships at least one default theme")
});

/// Map a syntect `Color` to a ratatui `Color`. syntect uses 8-bit RGBA;
/// ratatui `Color::Rgb` is the exact match.
fn map_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Convert a syntect style to a ratatui style.
fn map_style(s: SyStyle) -> Style {
    let mut style = Style::default().fg(map_color(s.foreground));
    // `FontStyle` is a bitflags struct; check the named flags.
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// Resolve a language hint (the fenced-block lang string, e.g. "rs",
/// "python", "ts") to a syntect syntax reference. Returns `None` for an
/// unknown/empty language — callers fall back to a plain (unstyled)
/// render in that case.
fn syntax_for_lang(lang: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    let lang = lang.trim();
    if lang.is_empty() {
        return None;
    }
    SYNTAX_SET.find_syntax_by_token(lang)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))
}

/// Highlight a single source `line` for the given language into a
/// `Vec<Span>`. Returns `None` when the language is unknown or the line
/// can't be highlighted — the caller should emit it as a plain span.
///
/// `highlighter` is held by the caller across a block so multi-line
/// constructs (block comments, triple-quoted strings) keep state line
/// to line. Build one with [`highlighter_for_lang`].
pub(crate) fn highlight_line(
    highlighter: &mut HighlightLines<'static>,
    line: &str,
) -> Option<Vec<Span<'static>>> {
    // syntect can panic on pathological input; treat any error as
    // "not highlightable" and fall back to plain.
    let events = highlighter.highlight_line(line, &SYNTAX_SET).ok()?;
    let mut spans = Vec::with_capacity(events.len());
    for (style, text) in events {
        spans.push(Span::styled(text.to_string(), map_style(style)));
    }
    Some(spans)
}

/// Build a line-stateful highlighter for a language. Returns `None` if
/// the language isn't recognized (caller renders plain).
pub(crate) fn highlighter_for_lang(
    lang: &str,
) -> Option<HighlightLines<'static>> {
    let syntax = syntax_for_lang(lang)?;
    Some(HighlightLines::new(syntax, &THEME))
}

/// The plain (un-highlighted) fallback style for a code line — matches
/// the accent color `render_code_block` used before highlighting.
pub(crate) fn plain_code_style() -> Style {
    crate::commands::code_tui::theme::accent()
}
