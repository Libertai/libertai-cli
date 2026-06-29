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

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::commands::code_tui::{theme, wrap};

/// Maximum nesting depth for recursive inline parsing. Emphasis captures
/// (`**bold**`, `*italic*`/`_italic_`, `~~strike~~`) recurse into
/// [`parse_inline_depth`]; a pathological input (e.g. deeply nested
/// `**...**`) could otherwise recurse without bound. At this limit we
/// stop recursing and emit the remaining captured text as a single plain
/// span (no further inline parsing, no style layering). 32 is far above
/// any realistic nesting the LLM emits, so normal markup is unaffected.
const MAX_INLINE_DEPTH: usize = 32;

/// Process-global OSC-8 hyperlink capability flag.
///
/// OSC-8 (`\x1b]8;;url\x1b\\label\x1b]8;;\x1b\\`) is emitted unconditionally
/// by default (matching the pre-detection behavior), but some terminals
/// mangle the escape and render it as garbage. The flag is set ONCE at TUI
/// startup via [`probe_osc8_capability`] (an env-var heuristic — there is no
/// hermetic, deterministic in-process terminal-capability query available
/// from within a render fn, which must stay pure / I/O-free for
/// testability), and may be flipped at runtime via
/// [`set_osc8_enabled`] (e.g. tests, or a future `/osc8` slash command).
///
/// Modeled on the `VIM_INPUT_ENABLED` flag in `code_ui.rs`: a plain
/// `AtomicBool` plus `pub(crate)` get/set fns, so the render path stays pure
/// (it only reads the flag) while the startup probe / tests can write it.
static OSC8_ENABLED: AtomicBool = AtomicBool::new(true);

/// Read the process-global OSC-8 capability flag. Used by the inline link
/// renderer to decide whether to emit the OSC-8 escape or fall back to the
/// underlined label only.
pub(crate) fn osc8_enabled() -> bool {
    OSC8_ENABLED.load(Ordering::SeqCst)
}

/// Store the process-global OSC-8 capability flag. Used by the startup probe
/// ([`probe_osc8_capability`]) and tests. `Relaxed` is sufficient: the render
/// path reads on its own cadence and does not require acquire/release
/// synchronization (mirroring `set_vim_input_enabled`).
pub(crate) fn set_osc8_enabled(enabled: bool) {
    OSC8_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Probe terminal OSC-8 capability once at startup and set the flag
/// accordingly. There is no hermetic, deterministic in-process query for
/// OSC-8 support (querying the terminal would require a synchronous
/// read-response round-trip, which is I/O and is unavailable inside the pure
/// render path), so this uses a simple env-var heuristic:
///
/// - `LIBERTAI_OSC8=0` / `=false` / `=off` -> DISABLE OSC-8 (label-only).
/// - `LIBERTAI_OSC8=1` / `=true` / `=on`  -> ENABLE OSC-8 (the default).
/// - Unset -> leave the flag at its compile-time default (ENABLED), matching
///   the pre-detection behavior.
///
/// Call this once near TUI startup (after `TerminalGuard::new`). It performs
/// no terminal I/O — only an env-var read — so it is hermetic and
/// deterministic. Tests flip the flag directly via [`set_osc8_enabled`]
/// rather than mutating the process environment.
pub(crate) fn probe_osc8_capability() {
    // Unset (Err) -> keep the default (enabled); only a recognized value
    // flips the flag. `if let` avoids a single-pattern `match` arm.
    if let Ok(v) = std::env::var("LIBERTAI_OSC8") {
        let lower = v.trim().to_ascii_lowercase();
        match lower.as_str() {
            "0" | "false" | "off" | "no" => set_osc8_enabled(false),
            "1" | "true" | "on" | "yes" => set_osc8_enabled(true),
            _ => {} // unknown value -> keep default (enabled)
        }
    }
}

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
            let mut closed = false;
            for code_line in iter.by_ref() {
                if code_line.trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                code_lines.push(code_line);
            }
            // Holdback (finding #3): an UNCLOSED fence means the block
            // is still streaming in. Render its content as plain
            // preformatted text — no border, no label — so the live
            // frame doesn't snap between "bordered block" and
            // "borderless tail" as tokens arrive. Once the closing ```
            // lands the next frame renders the full bordered block.
            if closed {
                lines.extend(render_code_block(&code_lines, lang, width));
            } else {
                lines.extend(render_code_block_open(&code_lines, lang, width));
            }
            continue;
        }

        // CommonMark tolerates up to 3 leading spaces of indentation
        // before block-structure prefixes (headings, blockquotes, list
        // markers, hr). Strip them; the consumed width is returned as
        // `leading` and used by M4b nested-list indent detection below
        // (depth = leading / 2).
        let (stripped, leading) = strip_leading_indent(line);

        // Heading # .. ######
        if let Some(s) = stripped.strip_prefix("# ") {
            lines.extend(heading(s, 1, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("## ") {
            lines.extend(heading(s, 2, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("### ") {
            lines.extend(heading(s, 3, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("#### ") {
            lines.extend(heading(s, 4, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("##### ") {
            lines.extend(heading(s, 5, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("###### ") {
            lines.extend(heading(s, 6, width));
            continue;
        }

        // Horizontal rule
        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
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

        // Nested-list indent: read leading-space depth from the ORIGINAL
        // line (strip_leading_indent trims up to 3, returning the raw
        // count as `leading`). CommonMark's indent step is 2-4; we use 2,
        // so depth = leading_spaces / 2. Two leading spaces -> one level.
        let depth = leading / 2;

        // Task-list checkboxes — checked BEFORE the generic `-`/`*` list
        // branches so `- [x]` / `- [ ]` get a checkbox glyph instead of a
        // bullet. `[x]`/`[X]` -> ✓ (success), `[ ]` -> ☐ (muted).
        if let Some(rest) = stripped.strip_prefix("- [") {
            if let Some(checked) = rest.chars().next() {
                let after = &rest[checked.len_utf8()..];
                if let Some(s) = after.strip_prefix("] ") {
                    let (glyph, style) = match checked {
                        'x' | 'X' => (theme::glyph::COMPLETED, theme::success()),
                        _ => (theme::glyph::UNCHECKED, theme::muted()),
                    };
                    let prefix = format!("{}{}  ", "  ".repeat(depth), glyph);
                    lines.extend(wrap_spans(parse_inline(s), &prefix, style, width));
                    continue;
                }
            }
        }
        if let Some(rest) = stripped.strip_prefix("* [") {
            if let Some(checked) = rest.chars().next() {
                let after = &rest[checked.len_utf8()..];
                if let Some(s) = after.strip_prefix("] ") {
                    let (glyph, style) = match checked {
                        'x' | 'X' => (theme::glyph::COMPLETED, theme::success()),
                        _ => (theme::glyph::UNCHECKED, theme::muted()),
                    };
                    let prefix = format!("{}{}  ", "  ".repeat(depth), glyph);
                    lines.extend(wrap_spans(parse_inline(s), &prefix, style, width));
                    continue;
                }
            }
        }

        // Unordered list — pass the nested-list depth through.
        if let Some(s) = stripped.strip_prefix("- ") {
            lines.extend(list_item("  • ", s, depth, width));
            continue;
        }
        if let Some(s) = stripped.strip_prefix("* ") {
            lines.extend(list_item("  • ", s, depth, width));
            continue;
        }

        // Ordered list (1. 2. ...) — pass the nested-list depth through.
        // Collect the leading run of digits first: `str::strip_prefix` with
        // a char predicate strips only ONE digit, so a multi-digit marker
        // (`12. item`) would otherwise fail the `. ` check and fall through
        // to the paragraph branch.
        let digits: String = stripped
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            if let Some(s) = stripped[digits.len()..].strip_prefix(". ") {
                lines.extend(list_item(&format!("  {digits}. "), s, depth, width));
                continue;
            }
        }

        // GFM table. A header row `^\s*\|.*\|\s*$` must be followed by a
        // separator row whose cells are dashes/colons only (`---`, `:---`,
        // `---:`, `:---:`). Detected AFTER the list/HR/blockquote checks
        // and BEFORE the paragraph fallback; consumes following rows of the
        // same pipe-delimited shape via the peekable iterator.
        if is_table_header(stripped) {
            if let Some(&next) = iter.peek() {
                if is_table_separator(next) {
                    let header = stripped;
                    let sep = next.to_string();
                    iter.next(); // consume separator
                    let mut rows = vec![header.to_string(), sep];
                    while let Some(&row) = iter.peek() {
                        if is_table_row(row) {
                            rows.push(row.to_string());
                            iter.next();
                        } else {
                            break;
                        }
                    }
                    lines.extend(render_table(&rows, width));
                    continue;
                }
            }
        }

        // Blank line
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        // Normal paragraph
        lines.extend(wrap_spans(
            parse_inline(stripped),
            "",
            theme::primary(),
            width,
        ));
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

// ---- GFM table detection + rendering (M4b) ----
//
// A GFM table is a header row `^\s*\|.*\|\s*$` followed by a separator
// row whose cells are dashes/colons only. We render it as aligned text
// (one `Line` per row, cells joined by ` | `) rather than the ratatui
// `Table` widget, which is awkward to slot into a `Vec<Line>` stream.

/// True if `line` looks like a table header row: optional leading
/// whitespace, a leading `|`, content, and a trailing `|`.
fn is_table_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() >= 2
}

/// True if `line` is a table separator row: every cell (split on
/// unescaped `|`, then trimmed) is non-empty and made of only `-` and
/// `:` (e.g. `---`, `:---`, `---:`, `:---:`). Surrounding spaces in a
/// cell are allowed (`| --- | --- |` is valid GFM).
fn is_table_separator(line: &str) -> bool {
    if !is_table_header(line) {
        return false;
    }
    let cells = split_table_row(line.trim());
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| {
        let t = c.trim();
        !t.is_empty() && t.chars().all(|ch| ch == '-' || ch == ':') && t.contains('-')
    })
}

/// True if `line` has the pipe-delimited row shape (used to gather
/// body rows after the separator). Less strict than the separator:
/// only the outer-pipe shape is required.
fn is_table_row(line: &str) -> bool {
    is_table_header(line)
}

/// Split a table row on unescaped `|`. A backslash-escaped pipe
/// (`\|`) becomes a literal `|` in the cell; the backslash is dropped.
fn split_table_row(row: &str) -> Vec<String> {
    // Strip the leading and trailing delimiters, then split on
    // unescaped pipes. A trailing escaped backslash before the final
    // pipe is preserved.
    let t = row.trim();
    let inner = if t.starts_with('|') && t.ends_with('|') && t.len() >= 2 {
        &t[1..t.len() - 1]
    } else {
        t
    };
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '|' {
                    chars.next();
                    cur.push('|');
                    continue;
                }
            }
            cur.push('\\');
        } else if c == '|' {
            cells.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    cells.push(cur);
    cells
}

/// Per-column alignment inferred from a separator cell.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableAlign {
    Left,
    Right,
    Center,
}

/// Render a parsed GFM table (rows[0] = header, rows[1] = separator,
/// rest = body) as aligned text `Line`s. Each cell is `parse_inline`d
/// so inline formatting works inside cells. Columns are padded to the
/// max display width per column and aligned per the separator. The last
/// column is truncated with `…` if the whole table overflows `width`.
fn render_table(rows: &[String], width: usize) -> Vec<Line<'static>> {
    if rows.len() < 2 {
        return Vec::new();
    }
    let mut header_cells = split_table_row(&rows[0]);
    let ncol = header_cells.len().max(1);
    // GFM trims leading/trailing whitespace in each cell.
    for c in header_cells.iter_mut() {
        *c = c.trim().to_string();
    }

    // Alignments from the separator row (trim each cell — GFM allows
    // spaces around the dashes, e.g. `| --- | :---: | ---: |`).
    let sep_cells = split_table_row(&rows[1]);
    let aligns: Vec<TableAlign> = (0..ncol)
        .map(|i| {
            let c = sep_cells.get(i).map(|s| s.trim()).unwrap_or("");
            let starts = c.starts_with(':');
            let ends = c.ends_with(':');
            match (starts, ends) {
                (true, true) => TableAlign::Center,
                (false, true) => TableAlign::Right,
                _ => TableAlign::Left,
            }
        })
        .collect();

    // Split every row into `ncol` cells (padding short rows with "").
    // GFM trims leading/trailing whitespace in each cell, so trim here.
    let mut all_rows: Vec<Vec<String>> = Vec::new();
    all_rows.push(header_cells);
    for r in &rows[2..] {
        let mut cells = split_table_row(r);
        for c in cells.iter_mut() {
            *c = c.trim().to_string();
        }
        while cells.len() < ncol {
            cells.push(String::new());
        }
        cells.truncate(ncol);
        all_rows.push(cells);
    }

    // Render each cell to inline spans and measure display widths.
    let rendered: Vec<Vec<Vec<Span<'static>>>> = all_rows
        .iter()
        .map(|row| {
            (0..ncol)
                .map(|i| parse_inline(row.get(i).map(|s| s.as_str()).unwrap_or("")))
                .collect()
        })
        .collect();
    let cell_text: Vec<Vec<String>> = rendered
        .iter()
        .map(|row| {
            row.iter()
                .map(|spans| spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect()
        })
        .collect();
    let mut col_widths: Vec<usize> = (0..ncol)
        .map(|i| {
            cell_text
                .iter()
                .map(|row| row.get(i).map(|s| s.width()).unwrap_or(0))
                .max()
                .unwrap_or(0)
        })
        .collect();

    // Total = sum(col widths) + (ncol-1)*3 (" | " separators). If that
    // overflows `width`, shrink the last column to fit (min 1). If the
    // table STILL overflows (a non-last column is the wide one), iteratively
    // shave the widest column by one until the total fits — guaranteeing no
    // row overflows `width` regardless of which column is the long one.
    let sep_w = 3 * ncol.saturating_sub(1);
    let total: usize = col_widths.iter().sum::<usize>() + sep_w;
    if total > width {
        let used: usize = col_widths.iter().sum::<usize>();
        let budget = width.saturating_sub(sep_w).max(ncol);
        if used > budget {
            let last = col_widths.len().saturating_sub(1);
            let others: usize = col_widths[..last].iter().sum();
            col_widths[last] = budget.saturating_sub(others).max(1);
        }
        // The last-column shrink alone may not be enough when a non-last
        // column is the wide one. Keep shaving the current widest column
        // (min 1 each) until the rendered width fits.
        while col_widths.iter().sum::<usize>() + sep_w > width {
            let (mi, _) = col_widths
                .iter()
                .enumerate()
                .max_by_key(|(_, &w)| w)
                .expect("non-empty col_widths");
            if col_widths[mi] <= 1 {
                // Every column is already at the floor (min 1); the table
                // is irreducibly wider than `width` (tiny width). Stop so
                // we don't spin; cells will truncate to "…" in table_line.
                break;
            }
            col_widths[mi] -= 1;
        }
    }

    let mut out = Vec::with_capacity(all_rows.len() + 1);

    // Header row.
    out.push(table_line(
        &rendered[0],
        &cell_text[0],
        &col_widths,
        &aligns,
    ));

    // Divider: one `─` run per column, sized to the column width, joined
    // by `─┼─` so it reads as a table border under the header.
    let divider: Vec<Span<'static>> = (0..ncol)
        .map(|i| Span::styled("─".repeat(col_widths[i]), theme::muted()))
        .collect();
    let mut div_spans = Vec::with_capacity(divider.len() * 2);
    for (i, d) in divider.into_iter().enumerate() {
        if i > 0 {
            div_spans.push(Span::styled("─┼─".to_string(), theme::muted()));
        }
        div_spans.push(d);
    }
    out.push(Line::from(div_spans));

    // Body rows.
    for ri in 1..rendered.len() {
        out.push(table_line(
            &rendered[ri],
            &cell_text[ri],
            &col_widths,
            &aligns,
        ));
    }

    out
}

/// Build one aligned table row `Line` from pre-rendered cell spans.
/// Cells are padded to `col_widths` per `aligns`; the last column is
/// truncated with `…` if its text overflows its allotted width.
fn table_line(
    cells: &[Vec<Span<'static>>],
    texts: &[String],
    col_widths: &[usize],
    aligns: &[TableAlign],
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let rows = col_widths
        .iter()
        .zip(texts.iter().map(|s| s.as_str()))
        .zip(cells.iter().cloned())
        .zip(aligns.iter().copied());
    for (i, (((&cw, text), cell_spans), align)) in rows.enumerate() {
        if i > 0 {
            spans.push(Span::raw(" | "));
        }
        let tw = text.width();

        if tw > cw {
            // Truncate an overflowing cell with an ellipsis so the row
            // stays within its column budget. Take chars by display
            // width (not char count) to respect wide glyphs, reserving
            // one column for the trailing `…`.
            let budget = cw.saturating_sub(1);
            let mut taken = String::new();
            let mut w = 0usize;
            for ch in text.chars() {
                let cw_ch = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if w + cw_ch > budget {
                    break;
                }
                taken.push(ch);
                w += cw_ch;
            }
            if cw == 0 {
                // No room at all — emit nothing.
            } else {
                // budget = cw-1 leaves one column for `…`.
                spans.push(Span::styled(format!("{taken}…"), theme::muted()));
            }
        } else {
            let pad = cw.saturating_sub(tw);
            match align {
                TableAlign::Right => {
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.extend(cell_spans);
                }
                TableAlign::Center => {
                    let left = pad / 2;
                    let right = pad - left;
                    spans.push(Span::raw(" ".repeat(left)));
                    spans.extend(cell_spans);
                    spans.push(Span::raw(" ".repeat(right)));
                }
                TableAlign::Left => {
                    spans.extend(cell_spans);
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
        }
    }
    Line::from(spans)
}

/// Peek the next two items of a `Peekable` iterator without consuming
/// them. Returns `(next, next+1)`. Replaces the `chars.clone().nth(1)`
/// smell with a named, intent-revealing helper. Generic over the inner
/// iterator so it works with any `Peekable<I: Clone + Iterator>` whose
/// items are also `Clone` (the blanket `Clone` impl on `Peekable`
/// requires both). We clone the dereferenced iterator — NOT the shared
/// `&Peekable` reference — so the clone is a real value copy.
fn peek2<I>(chars: &std::iter::Peekable<I>) -> (Option<I::Item>, Option<I::Item>)
where
    I: Clone + Iterator,
    I::Item: Clone,
{
    // Dereference so we clone the `Peekable<I>` VALUE (the blanket
    // `Clone for Peekable<I>` impl, which needs `I: Clone + I::Item:
    // Clone`), not the `&Peekable<I>` reference (which would only ever
    // produce another shared reference).
    let mut clone: std::iter::Peekable<I> = (*chars).clone();
    let first = clone.next();
    let second = clone.next();
    (first, second)
}

/// Layer an outer style onto each of a set of inner spans. Used by the
/// recursive inline parser: e.g. `**bold *italic* bold**` recurses on the
/// captured `bold *italic* bold` text, then layers `theme::bold()` (a
/// modifier-only style) onto each returned span via `patch_style`, so the
/// inner italic span keeps its own italic AND gains bold.
fn layer(spans: Vec<Span<'static>>, outer: Style) -> Vec<Span<'static>> {
    spans.into_iter().map(|s| s.patch_style(outer)).collect()
}

/// Render inline markdown (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`,
/// `[text](url)`) into styled spans. All spans are `'static` (owned). Thin
/// wrapper over [`parse_inline_depth`] starting at depth 0; see that fn for
/// the recursion + depth-guard contract.
fn parse_inline(text: &str) -> Vec<Span<'static>> {
    parse_inline_depth(text, 0)
}

/// Recursive inline parser with a depth guard.
///
/// Emphasis is recursive: the captured inner text of `**bold**` / `*italic*` /
/// `~~strike~~` is itself parsed (via [`parse_inline_depth`]), and the outer
/// emphasis style is layered onto the inner spans via [`layer`] (using
/// `Span::patch_style`, which unions modifiers while preserving inner
/// fg/bg). So `**bold *italic* bold**` italicizes the inner word AND keeps
/// it bold, and `**see [x](u)**` renders a clickable link that is also bold.
///
/// `depth` tracks the current recursion depth. To prevent stack overflow on
/// pathological/malicious deeply-nested input (e.g. `***...***` x N), once
/// `depth` reaches [`MAX_INLINE_DEPTH`] the emphasis captures stop recursing
/// and instead emit their captured inner text as a single plain
/// `Span::raw` (no further inline parsing, no style layering). Normal
/// nesting is far below the limit, so well-formed markup is unaffected.
fn parse_inline_depth(text: &str, depth: usize) -> Vec<Span<'static>> {
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
            '\\' => {
                // Escape: next char is literal (e.g. `\*`, `\|`). If there
                // is no next char, emit the backslash verbatim.
                if let Some(&next) = chars.peek() {
                    chars.next();
                    buf.push(next);
                } else {
                    buf.push('\\');
                }
            }
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
            '*' if peek2(&chars).0 == Some('*') => {
                // `**` opens bold. peek2's first slot is the char
                // immediately after the consumed `*`; if it's another
                // `*` we have a bold opener.
                chars.next(); // consume second '*'
                flush(&mut buf, &mut spans);
                let mut bold = String::new();
                let mut found_close = false;
                while let Some(&next) = chars.peek() {
                    let (a, b) = peek2(&chars);
                    if a == Some('*') && b == Some('*') {
                        chars.next();
                        chars.next();
                        found_close = true;
                        break;
                    }
                    bold.push(next);
                    chars.next();
                }
                if found_close {
                    if depth + 1 > MAX_INLINE_DEPTH {
                        // Depth guard: stop recursing, emit the captured
                        // inner text as a single plain span.
                        spans.push(Span::raw(bold));
                    } else {
                        spans.extend(layer(parse_inline_depth(&bold, depth + 1), theme::bold()));
                    }
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
                    if depth + 1 > MAX_INLINE_DEPTH {
                        // Depth guard: stop recursing, emit the captured
                        // inner text as a single plain span.
                        spans.push(Span::raw(italic));
                    } else {
                        spans.extend(layer(
                            parse_inline_depth(&italic, depth + 1),
                            Style::default().add_modifier(Modifier::ITALIC),
                        ));
                    }
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
                    let (a, b) = peek2(&chars);
                    if a == Some('~') && b == Some('~') {
                        chars.next();
                        chars.next();
                        found_close = true;
                        break;
                    }
                    strike.push(next);
                    chars.next();
                }
                if found_close {
                    if depth + 1 > MAX_INLINE_DEPTH {
                        // Depth guard: stop recursing, emit the captured
                        // inner text as a single plain span.
                        spans.push(Span::raw(strike));
                    } else {
                        spans.extend(layer(
                            parse_inline_depth(&strike, depth + 1),
                            Style::default().add_modifier(Modifier::CROSSED_OUT),
                        ));
                    }
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
                    // OSC 8 hyperlink: terminals that support it make the
                    // label clickable; others render just the label. Fall
                    // back to the underlined label only if the URL is very
                    // long (>200), contains control chars, OR the terminal
                    // does not advertise OSC-8 support
                    // ([`osc8_enabled`] — a capability flag probed once at
                    // startup; some terminals mangle the OSC-8 escape into
                    // visible garbage).
                    let style = theme::accent().add_modifier(Modifier::UNDERLINED);
                    if !osc8_enabled() || url.len() > 200 || url.chars().any(|c| c.is_control()) {
                        spans.push(Span::styled(label, style));
                    } else {
                        let content = format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\");
                        spans.push(Span::styled(content, style));
                    }
                } else if found_label {
                    // `]` was consumed but no `(` followed: emit the full
                    // literal `[label]` (the closing bracket too), so a
                    // non-link bracketed span round-trips verbatim.
                    spans.push(Span::raw(format!("[{label}]")));
                } else {
                    // No closing `]` at all: nothing was consumed beyond
                    // the label text, so emit `[label` verbatim.
                    spans.push(Span::raw(format!("[{label}")));
                }
            }
            _ => buf.push(c),
        }
    }

    flush(&mut buf, &mut spans);
    spans
}

/// Render a heading, styled by level via [`theme::heading`], pre-wrapped to
/// `width`. A heading wider than `width` (e.g. a 61-col `#` heading at width
/// 20) is word-wrapped to `width` and emitted as one `Line` per visual row,
/// each carrying the heading style — so it is NOT truncated by the wrap-off
/// Paragraph renderer and the flat 1-row-per-`Line` count stays correct.
/// `text` is the heading text with the `#`/`##`/… prefix already stripped.
fn heading(text: &str, level: usize, width: usize) -> Vec<Line<'static>> {
    let style = theme::heading(level);
    let width = width.max(1);
    // word_wrap to `width` (no first-line indent — the `#` prefix was
    // already removed, so the whole budget is available on every line).
    let wrapped = wrap::word_wrap(text, width, 0);
    wrapped
        .into_iter()
        .map(|chunk| Line::from(Span::styled(chunk, style)))
        .collect()
}

/// Render a list item with the given bullet and indent, pre-wrapped
/// to `width`.
///
/// `indent` is the number of 2-space units to prefix before the bullet.
/// M4b wires the nested-list depth here (top-level items pass 0; a
/// 2-space-indented `-` passes 1, etc.). The wrapped prefix is
/// `  `.repeat(indent) + bullet.
fn list_item(bullet: &str, text: &str, indent: usize, width: usize) -> Vec<Line<'static>> {
    let prefix = format!("{}{}", "  ".repeat(indent), bullet);
    wrap_spans(parse_inline(text), &prefix, theme::accent(), width)
}

/// Render a code block with dim border lines and a dim header naming
/// the language. Borders are `width`-aware (was hardcoded 40). Code
/// lines are pre-wrapped to `width` (with the 2-space indent budget) and
/// emitted as one `Line` per visual row, each carrying the `theme::accent`
/// style. Pre-wrapping (rather than emitting hard unwrapped lines) keeps the
/// wrap-off Paragraph renderer from truncating a wide code line and keeps the
/// flat 1-row-per-`Line` count correct: `word_wrap` emits one chunk per visual
/// row, and each chunk becomes its own `Line`.
fn render_code_block(code: &[&str], lang: &str, width: usize) -> Vec<Line<'static>> {
    let border = "─".repeat(width.saturating_sub(2));
    let mut lines = Vec::new();
    // Dim header naming the language (or "(code)" if empty) above the
    // top border, so fenced blocks read as code at a glance.
    let label = if lang.is_empty() { "(code)" } else { lang };
    lines.push(Line::from(Span::styled(label.to_string(), theme::muted())));
    lines.push(Line::from(Span::styled(border.clone(), theme::muted())));
    lines.extend(render_code_lines(code, lang, width));
    lines.push(Line::from(Span::styled(border, theme::muted())));
    lines
}

/// Render an UNCLOSED code fence — content still streaming in (finding
/// #3). No borders: just the dim language label and the raw code lines
/// indented, so the live frame doesn't snap between a bordered block
/// and a borderless tail as tokens arrive. Once the closing ``` lands,
/// the next render switches to the full bordered `render_code_block`.
fn render_code_block_open(code: &[&str], lang: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let label = if lang.is_empty() { "(code)" } else { lang };
    lines.push(Line::from(Span::styled(label.to_string(), theme::muted())));
    lines.extend(render_code_lines(code, lang, width));
    lines
}

/// Render the body of a code block (the indented code lines, between
/// the borders). Highlights each line via syntect when the language is
/// recognized AND the line fits the code budget (finding #6); falls
/// back to the uniform-accent word-wrapped style when it isn't, when
/// highlighting fails, or when the line would overflow `width` (long
/// lines keep the existing soft-wrap so the wrap-off Paragraph renderer
/// doesn't truncate them).
fn render_code_lines(code: &[&str], lang: &str, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let mut out = Vec::new();
    let plain = crate::commands::code_tui::highlight::plain_code_style();
    let mut highlighter = crate::commands::code_tui::highlight::highlighter_for_lang(lang);
    // Code body budget: width minus the 2-space indent.
    let body_budget = width.saturating_sub(2).max(1);
    for code_line in code {
        // Only highlight when the line fits the budget — otherwise emit
        // the plain word-wrapped form so a long line still wraps (the
        // wrap-off renderer would otherwise truncate a single over-wide
        // highlighted Line and the row-count model would drift).
        let fits = code_line.width() <= body_budget;
        let highlighted = if fits {
            highlighter
                .as_mut()
                .and_then(|h| crate::commands::code_tui::highlight::highlight_line(h, code_line))
        } else {
            None
        };
        match highlighted {
            Some(spans) if !spans.is_empty() => {
                let mut v = vec![Span::raw("  ")];
                v.extend(spans);
                out.push(Line::from(v));
            }
            _ => {
                let wrapped = wrap::word_wrap(code_line, width, 2);
                for chunk in wrapped {
                    out.push(Line::from(Span::styled(format!("  {chunk}"), plain)));
                }
            }
        }
    }
    out
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
    // (R3-OSC8-WIDTH) Measure the VISIBLE width, not the raw escaped-string
    // width. An OSC-8 hyperlink span carries the URL twice inside ESC
    // sequences (`\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\`); each ESC byte
    // counts as 1 display column under `UnicodeWidthStr`, so a 10-col label
    // with a 60-char URL measures ~84 cols and spuriously overflows, sending
    // the line into the wrap branch where `split_at_width` slices INTO the
    // URL escape and produces a broken OSC-8. Strip control chars for the
    // width decision so it keys off what the user actually sees. The raw
    // `content` (escape intact) is still what's emitted on the fits branch.
    let visible_w = visible_width(&content);

    // Fits (or no width budget for content) — emit the styled line
    // exactly as before, behavior-preserving for short inputs.
    if prefix_w + visible_w <= width {
        let mut line_spans = Vec::with_capacity(spans.len() + 1);
        if !prefix.is_empty() {
            line_spans.push(Span::styled(prefix.to_string(), prefix_style));
        }
        line_spans.extend(spans);
        return vec![Line::from(line_spans)];
    }

    // Overflow — the VISIBLE content doesn't fit. If the line carries an
    // OSC-8 escape, wrapping the raw escaped string would slice into the URL
    // escape (broken hyperlink). Fall back to label-only rendering (mirror
    // the >200-char-url fallback in the link parser): strip the OSC-8
    // wrapper and wrap just the visible label, so the escape is never split.
    let content_for_wrap = if content.contains("\u{1b}]8;;") {
        // (R4-OSC8-OVERFLOW-STRIP) Use the multi-link-aware stripper so that
        // text PRECEDING a link (e.g. 'see [hi](url)') and MULTIPLE links in
        // one line are both reduced to their visible labels. The singular
        // `strip_osc8` only handled a LEADING link; with preceding text its
        // `strip_prefix(open)` failed and fell to the control-strip fallback,
        // which removes only ESC bytes and leaks the raw OSC-8 framing
        // (']8;;', '\') + URL into the wrapped output. `strip_all_osc8_labels`
        // scans for the opener anywhere in the string and walks every link.
        strip_all_osc8_labels(&content)
    } else {
        content.clone()
    };
    let usable = width.saturating_sub(prefix_w).max(1);
    let wrapped = wrap::word_wrap(&content_for_wrap, usable, 0);
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

/// (R3-OSC8-WIDTH) Display width of `s` measured against what the terminal
/// actually DRAWS — i.e. the visible text, not the raw escaped string. OSC-8
/// hyperlink spans embed ESC (`\x1b`) and other control bytes around the
/// label AND carry the URL twice inside the escape; `UnicodeWidthStr` counts
/// every byte as 1 column, so the raw width massively overstates the visible
/// width (a 2-col label with a 60-char URL measures ~84 cols) and breaks the
/// wrap decision. This projects to visible cells by:
///   1. Replacing each OSC-8 wrapper with just its visible label (the URL +
///      escape framing are terminal-consumed, never drawn).
///   2. Stripping any remaining C0 control chars.
fn visible_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    let projected = strip_all_osc8_labels(s);
    let stripped: String = projected.chars().filter(|c| !c.is_control()).collect();
    stripped.width()
}

/// (R3-OSC8-WIDTH) Replace every OSC-8 hyperlink wrapper in `s` with just its
/// visible label, leaving surrounding text intact. An OSC-8 span is
/// `\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\`; the terminal draws only
/// `{label}`. Handles multiple links in one string (e.g. a paragraph with
/// several links). Anything that doesn't match the OSC-8 shape is left as-is
/// (the trailing control-strip in [`visible_width`] cleans up stray ESCs).
fn strip_all_osc8_labels(s: &str) -> String {
    let open = "\u{1b}]8;;";
    let st = "\u{1b}\\";
    let close = "\u{1b}]8;;\u{1b}\\";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(open) {
        // Emit any text before the opener.
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        // Skip the URL up to the first ST, then take the label up to the close.
        match after_open.split_once(st) {
            Some((_, label_and_close)) => {
                // (R4-OSC8-MULTILINK) Find the FIRST closer after THIS link's
                // label, not the last one in the whole string. A paragraph with
                // 2+ links is shaped `...<link1>\x1b]8;;\x1b\\...<link2>\x1b]8;;\x1b\\`;
                // `strip_suffix(close)` matches the LAST closer and swallows
                // link2's URL + framing as link1's "label" (visible_width then
                // returned 53 for a true width of 11). `find(close)` gives the
                // first occurrence, which is this link's own closer.
                if let Some(idx) = label_and_close.find(close) {
                    let before_close = &label_and_close[..idx];
                    out.push_str(before_close);
                    rest = &label_and_close[idx + close.len()..];
                } else {
                    // Malformed (no close) — take up to the next ESC as label,
                    // drop the rest.
                    let (label, _) = label_and_close
                        .split_once('\u{1b}')
                        .unwrap_or((label_and_close, ""));
                    out.push_str(label);
                    rest = "";
                }
            }
            None => {
                // No ST after the opener — malformed; emit nothing for the escape.
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use std::sync::Mutex;

    // The OSC-8 capability flag (`OSC8_ENABLED`) and the `LIBERTAI_OSC8` env
    // var it reads are PROCESS-GLOBAL. Rust's default test runner executes
    // tests in parallel threads within ONE process, so the OSC-8 flag/env
    // tests below — which flip the global and then assert on flag-dependent
    // `render` output or on the flag value itself — would race and stomp each
    // other's state without serialization. This guard mutex serializes ONLY
    // the OSC-8 flag/env tests (each acquires it for its full duration); all
    // other markdown tests run concurrently as before. No new deps.
    static OSC8_TEST_GUARD: Mutex<()> = Mutex::new(());
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
        let lines = render("word ".repeat(20).trim_end(), 20);
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

    /// A recognized-language code block renders highlighted spans (at
    /// least one span with a non-default foreground color) for a line
    /// that fits the budget (finding #6).
    #[test]
    fn code_block_highlights_recognized_language() {
        // `let x = 1;` is valid rust and fits a wide budget, so the
        // highlighted branch fires and emits per-token spans with syntect
        // foreground colors. The plain fallback emits exactly ONE span
        // (the whole line) in the accent color; highlighting emits MORE
        // than one span (keyword `let`, identifier, etc.).
        let src = "```rust\nlet x = 1;\n```";
        let lines = render(src, 80);
        // header + border + 1 code line + border.
        assert_eq!(lines.len(), 4, "got {lines:?}");
        let code_line = &lines[2];
        assert!(
            code_line.spans.len() > 2,
            "highlighted code line should have >2 spans (indent + tokens), \
             got {}: {code_line:?}",
            code_line.spans.len()
        );
        // At least one span carries a non-default foreground (the
        // keyword/identifier colors from the theme).
        let any_colored = code_line
            .spans
            .iter()
            .any(|s| s.style.fg.is_some() && s.style.fg != Some(ratatui::style::Color::Reset));
        assert!(any_colored, "expected at least one colored span");
    }

    /// An unrecognized language falls back to the plain (single-span)
    /// render — no highlighting, no panic (finding #6).
    #[test]
    fn code_block_unknown_language_falls_back_plain() {
        let src = "```totally-not-a-language\nsome text\n```";
        let lines = render(src, 80);
        let code_line = &lines[2];
        // Plain fallback: a single span holding "  some text" (indent +
        // text in the accent color).
        assert_eq!(
            code_line.spans.len(),
            1,
            "unknown lang should render as one plain span (indent + text)"
        );
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
        assert!(
            narrow.len() > 1,
            "long word should hard-break to >1 line at width 40"
        );
        let wide = render(&long_word, 200);
        assert_eq!(wide.len(), 1, "long word fits on 1 line at width 200");
    }

    #[test]
    fn hr_width_follows_render_width() {
        // HR content is width-2 `─` glyphs, not the old hardcoded 40.
        let lines = render("---", 60);
        assert_eq!(lines.len(), 1, "hr renders a single line");
        let content_width: usize = lines[0].spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(
            content_width, 58,
            "hr content width should be width-2 (58), got {content_width}"
        );
    }

    #[test]
    fn code_block_soft_wraps_to_width() {
        // A fenced code block whose code line is wider than `width` is
        // PRE-WRAPPED to `width` (with the 2-space indent budget) so the
        // wrap-off Paragraph renderer does not truncate it and the flat
        // 1-row-per-`Line` count stays correct. word_wrap now LOOPS the
        // hard-break until every chunk fits the budget
        // (R3-WORDWRAP-BREAK-ONCE): a 50-char no-space word at width 20
        // (first-line budget 18) splits 18 + 20 + 12 -> 3 code Lines, none
        // overflowing.
        let long_code = "x".repeat(50);
        let src = format!("```rust\n{long_code}\n```");
        let lines = render(&src, 20);
        // word_wrap gets a single line (one code line), so every chunk uses
        // that line's budget = 20 - 2 (indent) = 18. The loop now splits the
        // 50-x run 18 + 18 + 14 -> 3 code Lines (previously [18, 32] with the
        // 32-col remainder overflowing the wrap-off truncator).
        // header + top border + 3 code lines + bottom border == 6 lines.
        assert_eq!(
            lines.len(),
            6,
            "code block should be header + border + 3 wrapped code + border, got {}",
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
        // The first wrapped code line carries the indent + first 18 x's
        // (fits width 20, so the truncator does not cut it).
        let first: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(first, format!("  {}", "x".repeat(18)));
        assert_eq!(first.width(), 20);
        // (R3-WORDWRAP-BREAK-ONCE) The second chunk is another 18-x segment
        // (the line budget is constant 18 within a single code line), and the
        // third is the final 14-x remainder — so every chunk fits the budget
        // and no remainder overflows the truncator.
        let second: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(second, format!("  {}", "x".repeat(18)));
        let third: String = lines[4].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(third, format!("  {}", "x".repeat(14)));
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

    // ---- M4b: tables, nested lists, task lists, recursive inline, OSC-8 ----

    #[test]
    fn renders_gfm_table() {
        let src = "| a | b |\n| --- | --- |\n| 1 | 2 |\n";
        let lines = render(src, 80);
        // header + divider + 1 body row == 3 lines.
        assert_eq!(lines.len(), 3, "table renders header + divider + body");
        // Header and body rows join cells with " | "; the divider (line
        // index 1) uses a border glyph instead, so only check the data
        // rows (indices 0 and 2).
        for &i in &[0usize, 2] {
            let text: String = lines[i].spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.contains(" | "),
                "data row {i} should join cells with ' | ', got {text:?}"
            );
        }
        // Divider line contains the column junction glyph.
        let div: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            div.contains('┼'),
            "divider has a column junction glyph: {div:?}"
        );
    }

    #[test]
    fn table_requires_separator() {
        // No separator row after the header -> NOT a table; the header
        // line renders as a paragraph instead (no divider, no alignment).
        let src = "| a | b |\nnot a separator\n";
        let lines = render(src, 80);
        // Two paragraph lines (no divider line).
        assert_eq!(
            lines.len(),
            2,
            "no separator -> 2 paragraph lines, no divider"
        );
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !text.contains('┼'),
                "no table divider should be rendered without a separator: {text:?}"
            );
        }
    }

    #[test]
    fn table_alignment_from_separator() {
        // Right-aligned second column: cells should be right-padded.
        let src = "| name | count |\n| :--- | ---: |\n| a | 1 |\n| ab | 22 |\n";
        let lines = render(src, 80);
        // header + divider + 2 body rows
        assert_eq!(lines.len(), 4);
        // The body rows right-align the "count" column to the max width (2).
        let row3: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        // "22" is already width 2 (max), so no leading pad; "1" gets a
        // leading space (" 1") to align. Check the second body row's last
        // cell text contains "22" and the first body row contains " 1".
        let row2: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            row2.contains(" 1"),
            "right-aligned short cell is space-padded: {row2:?}"
        );
        assert!(
            row3.contains("22"),
            "right-aligned max-width cell: {row3:?}"
        );
    }

    #[test]
    fn table_clips_when_too_wide() {
        // A 3-col table whose natural width exceeds 20 columns: the last
        // column is truncated with an ellipsis so the row stays in budget.
        let src = "| aaaa | bbbb | cccccccccccccccc |\n| --- | --- | --- |\n| 1 | 2 | zzzzzzzzzzzzzzzzzzzzz |\n";
        let lines = render(src, 20);
        // header + divider + 1 body row
        assert_eq!(lines.len(), 3);
        let body: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            body.contains('…') || body.width() <= 20,
            "overflowing table clips the last column, got {body:?} (w={})",
            body.width()
        );
    }

    #[test]
    fn nested_list_indents() {
        let src = "- top\n  - nested\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 2);
        let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let nested: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // The nested item prefix has more leading space than the top one.
        let top_lead = top.len() - top.trim_start().len();
        let nested_lead = nested.len() - nested.trim_start().len();
        assert!(
            nested_lead > top_lead,
            "nested list item is indented further: top_lead={top_lead} nested_lead={nested_lead} ({nested:?})"
        );
    }

    #[test]
    fn task_list_checked_uses_success_glyph() {
        let lines = render("- [x] done\n- [ ] todo\n", 80);
        assert_eq!(lines.len(), 2);
        let done: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let todo: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            done.contains(theme::glyph::COMPLETED),
            "checked item shows ✓: {done:?}"
        );
        assert!(
            todo.contains(theme::glyph::UNCHECKED),
            "unchecked item shows ☐: {todo:?}"
        );
        // The checked glyph span is styled success; unchecked is muted.
        assert!(
            lines[0].spans.iter().any(|s| s.style == theme::success() && s.content.contains(theme::glyph::COMPLETED)),
            "✓ span is theme::success()"
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.style == theme::muted() && s.content.contains(theme::glyph::UNCHECKED)),
            "☐ span is theme::muted()"
        );
    }

    #[test]
    fn recursive_inline_bold_italic() {
        // `**bold *italic* bold**` -> the inner italic word keeps italic
        // AND gains bold (layered modifiers).
        let lines = render("**bold *italic* bold**", 80);
        assert_eq!(lines.len(), 1);
        let italic_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "italic")
            .expect("inner italic span present");
        let mods = italic_span.style.add_modifier;
        assert!(mods.contains(Modifier::ITALIC), "inner word is italic");
        assert!(
            mods.contains(Modifier::BOLD),
            "inner word is ALSO bold (layered)"
        );
    }

    #[test]
    fn recursive_inline_link_in_bold() {
        // Asserts the OSC-8 escape IS emitted (finds a span with `\x1b`),
        // so the flag must be true. Serialize against the flag/env tests.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // `**see [x](u)**` -> the link span is bold AND underlined+accent.
        let lines = render("**see [x](u)**", 80);
        assert_eq!(lines.len(), 1);
        // The label "x" is emitted as a span inside the bold layer.
        let link_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains('x') && s.content.contains('\x1b'))
            .expect("OSC-8 link span present inside bold");
        let mods = link_span.style.add_modifier;
        assert!(mods.contains(Modifier::BOLD), "link is bold (outer layer)");
        assert!(mods.contains(Modifier::UNDERLINED), "link is underlined");
    }

    #[test]
    fn osc8_hyperlink_emitted() {
        // Asserts the OSC-8 escape IS emitted, which requires the capability
        // flag at its default (true). Serialize against the flag/env tests that
        // flip the process-global flag so they can't turn it off mid-render.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // `[x](u)` -> span content contains the OSC-8 escape sequence.
        let lines = render("[label](https://example.com)", 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\x1b]8;;https://example.com\x1b\\"),
            "OSC-8 opener present: {text:?}"
        );
        assert!(
            text.contains("\x1b]8;;\x1b\\"),
            "OSC-8 closer present: {text:?}"
        );
        assert!(text.contains("label"), "label is in the span: {text:?}");
        // The span is underlined + accent.
        let link = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("\x1b]8;;"))
            .expect("link span");
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(link.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn osc8_long_url_falls_back_to_label_only() {
        // A >200-char URL must NOT embed OSC-8 (control-sequence risk);
        // it renders the underlined label only.
        let mut long = "https://example.com/".to_string();
        long.push_str(&"x".repeat(200));
        let src = format!("[label]({long})");
        let lines = render(&src, 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("\x1b]8;;"), "long URL omits OSC-8: {text:?}");
        assert!(text.contains("label"), "label still shown");
    }

    #[test]
    fn osc8_link_wraps_on_visible_width_without_breaking_escape() {
        // (R3-OSC8-WIDTH) An OSC-8 link whose VISIBLE label fits the width
        // but whose escaped-string width (each ESC = 1 col) far exceeds it
        // must NOT enter the wrap branch — the raw width overstates visible
        // width and `split_at_width` would slice INTO the URL escape. The
        // line stays one Line with the OSC-8 escape intact.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        let url = format!("https://example.com/{}", "x".repeat(60)); // 80-char URL (<200 -> OSC-8 emitted)
        let src = format!("[hi]({url})");
        // Visible content is "hi" (2 cols); escaped width is ~84. At width
        // 30 the visible content fits, so the line is NOT wrapped.
        let lines = render(&src, 30);
        assert_eq!(
            lines.len(),
            1,
            "visible-fitting link must not wrap: got {} lines",
            lines.len()
        );
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // The OSC-8 escape is intact: the opener (`\x1b]8;;{url}\x1b\\`) and
        // the closer (`\x1b]8;;\x1b\\`) are both present exactly once and
        // un-sliced. (The opener-with-URL is the load-bearing check: if
        // split_at_width had sliced into the escape, the URL terminator `\x1b\\`
        // would be missing or the opener split across a wrap boundary.)
        let opener = format!("\u{1b}]8;;{url}\u{1b}\\");
        assert_eq!(
            text.matches(&opener).count(),
            1,
            "OSC-8 opener (with full URL) present exactly once: {text:?}"
        );
        assert_eq!(
            text.matches("\u{1b}]8;;\u{1b}\\").count(),
            1,
            "OSC-8 closer present exactly once: {text:?}"
        );
        assert!(text.contains("hi"), "visible label present: {text:?}");
    }

    #[test]
    fn osc8_link_visible_overflow_falls_back_to_label_only() {
        // (R3-OSC8-WIDTH) When the VISIBLE label itself exceeds the width,
        // the line must wrap on the LABEL only — never slice the OSC-8
        // escape. Here a 30-col label at width 10 wraps to label chunks; the
        // output must contain NO OSC-8 escape (only the wrapped label text).
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        let url = format!("https://example.com/{}", "x".repeat(60));
        let label = "a".repeat(30);
        let src = format!("[{label}]({url})");
        let lines = render(&src, 10);
        // Every line is label-only (no OSC-8 escape sequence anywhere).
        for (i, line) in lines.iter().enumerate() {
            let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !t.contains("\x1b]8;;"),
                "line {i} must be label-only (no OSC-8) when visible label overflows: {t:?}"
            );
        }
        // The full label is recoverable from the wrapped chunks.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert_eq!(
            joined.chars().filter(|c| *c == 'a').count(),
            30,
            "all 30 label cols preserved"
        );
    }

    #[test]
    fn osc8_multi_link_strip_all_does_not_swallow_second_link() {
        // (R4-OSC8-MULTILINK) A paragraph with TWO OSC-8 links must have
        // `strip_all_osc8_labels` return BOTH labels joined by the visible
        // text, NOT swallow link2's URL+framing as link1's "label". The
        // regression used `strip_suffix(close)` which matched the LAST closer
        // in the whole string; for 'see [a](u1) and [b](u2)' it returned a
        // 53-wide blob (URL + framing + label2) instead of the 11-wide
        // 'see a and b'. `find(close)` matches the FIRST closer (this link's
        // own), leaving link2 to be handled on the next loop iteration.
        // Build the escaped string by hand (the renderer emits this shape):
        //   \x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\   per link.
        let open = "\u{1b}]8;;";
        let st = "\u{1b}\\";
        let close = "\u{1b}]8;;\u{1b}\\";
        let link1 = format!("{open}u1{st}a{close}");
        let link2 = format!("{open}u2{st}b{close}");
        let escaped = format!("see {link1} and {link2}");
        let stripped = strip_all_osc8_labels(&escaped);
        assert_eq!(
            stripped, "see a and b",
            "two links strip to their labels + visible text, link2 not swallowed: {stripped:?}"
        );
        // visible_width keys off the TRUE visible width (control-stripped),
        // so it must be 11 ('see a and b'), NOT the 53 from the regression.
        assert_eq!(
            visible_width(&escaped),
            11,
            "visible_width is the true visible width (11), not 53: got {}",
            visible_width(&escaped)
        );
    }

    #[test]
    fn osc8_overflow_with_preceding_text_does_not_leak_framing() {
        // (R4-OSC8-OVERFLOW-STRIP) When text PRECEDES the link
        // ('see [hi](url)') and the visible content overflows a narrow width,
        // the wrap branch must reduce to the visible label only — NO raw
        // OSC-8 framing (']8;;', bare '\') and NO raw URL may leak into the
        // wrapped output. The regression called the singular `strip_osc8`
        // which only handled a LEADING link; its `strip_prefix(open)` failed
        // on 'see ...' and the control-strip fallback leaked ']8;;h' etc.
        // The fix routes the overflow branch through `strip_all_osc8_labels`,
        // which scans for the opener anywhere and strips every link.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        let lines = render("see [hi](https://example.com/aaaa)", 5);
        // No line carries raw OSC-8 framing or a raw URL.
        for (i, line) in lines.iter().enumerate() {
            let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !t.contains("]8;;"),
                "line {i} leaked OSC-8 framing ']8;;': {t:?}"
            );
            // A bare '\' from the ST terminator (`\x1b\\`) is framing, not
            // visible text — it must not survive the strip. (The label 'hi'
            // contains no backslash.)
            assert!(
                !t.contains('\\'),
                "line {i} leaked a bare '\\' (ST terminator): {t:?}"
            );
            assert!(
                !t.contains("https://example.com/aaaa"),
                "line {i} leaked the raw URL: {t:?}"
            );
        }
        // The link label 'hi' still renders (visible text is preserved).
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            joined.contains("hi"),
            "the link label 'hi' renders: {joined:?}"
        );
        assert!(
            joined.contains("see"),
            "the preceding text 'see' renders: {joined:?}"
        );
    }

    #[test]
    fn unclosed_link_emits_label_fallback() {
        // `[label`(no closing) -> the `[{label}` literal fallback.
        let lines = render("[label no close", 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[label"),
            "unclosed link falls back to literal: {text:?}"
        );
    }

    // ---- M4b adversarial suite (independent review) ----
    //
    // Each test is written to FAIL if its construct is broken, then mentally
    // verified against the (fixed) implementation. The verifier runs them
    // for real.

    // (1) Tables ----------------------------------------------------------

    #[test]
    fn table_header_and_body_cell_counts() {
        // A 3-row table (header + sep + 1 body) must render header + divider
        // + body == 3 Lines. Splitting each data row's rendered text on
        // " | " yields (ncol - 1) separators, i.e. ncol cells = ncol-1+1.
        let src = "| name | value | note |\n| --- | --- | --- |\n| a | 1 | x |\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3, "header + divider + 1 body row");
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let body: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            header.split(" | ").count(),
            3,
            "header has 3 cells: {header:?}"
        );
        assert_eq!(body.split(" | ").count(), 3, "body has 3 cells: {body:?}");
        // The header text is exactly the trimmed cells in order.
        assert_eq!(
            header.split(" | ").collect::<Vec<_>>(),
            ["name", "value", "note"]
        );
    }

    #[test]
    fn table_columns_are_width_aligned() {
        // Every cell in a column must pad out to the column's max width, so
        // the rendered " | " separators land in the same column on every row.
        let src = "| a | bbbb |\n| --- | --- |\n| aa | b |\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3);
        // Column widths: col0 = max("a","aa") = 2; col1 = max("bbbb","b") = 4.
        for &i in &[0usize, 2] {
            let row: String = lines[i].spans.iter().map(|s| s.content.as_ref()).collect();
            let cells: Vec<&str> = row.split(" | ").collect();
            assert_eq!(cells.len(), 2);
            // col0 cell width == 2 (padded), col1 cell width == 4 (padded).
            assert_eq!(cells[0].width(), 2, "row {i} col0 width 2: {cells:?}");
            assert_eq!(cells[1].width(), 4, "row {i} col1 width 4: {cells:?}");
        }
        // Sanity: header cells align with body cells at the same columns.
        let h: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let b: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(h.find('|'), b.find('|'), "separator columns align");
    }

    #[test]
    fn table_center_and_right_align_from_separator() {
        // `:---:` centers, `---:` right-aligns. Use enough width difference
        // that the alignment is visible (pad >= 2 for centering symmetry).
        // col0 max width = 4 ("name"); body col0 "ab" (2) -> center pad 2 ->
        // " ab " (1 + 1). col1 max = 5 ("count"); body col1 "1" (1) ->
        // right pad 4 -> "    1".
        let src = "| name | count |\n| :---: | ---: |\n| ab | 1 |\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3);
        let body: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        let cells: Vec<&str> = body.split(" | ").collect();
        assert_eq!(cells.len(), 2);
        // Centered "ab" -> leading AND trailing space present.
        assert!(
            cells[0].starts_with(' ') && cells[0].ends_with(' '),
            "col0 centered: {cells:?}"
        );
        assert_eq!(cells[0].trim(), "ab");
        // Right-aligned "1" -> leading pad, no trailing pad.
        assert!(
            cells[1].starts_with(' ') && !cells[1].ends_with(' '),
            "col1 right-aligned: {cells:?}"
        );
        assert_eq!(cells[1].trim(), "1");
    }

    #[test]
    fn table_escaped_pipe_is_literal() {
        // `\|` inside a cell is a literal pipe, NOT a column split. So
        // `| a \| b | c |` has TWO cells: "a | b" and "c". We verify this
        // via the SPAN structure, not by splitting the flattened row text —
        // the rendered cell "a | b" contains a literal `|`, so splitting
        // the flattened row on " | " would (correctly) conflate it with a
        // column separator and wrongly report 3 cells. The renderer emits
        // the column join as a separate `Span::raw(" | ")` between cells,
        // so grouping spans by those join spans reconstructs the true cells.
        let src = "| h1 | h2 |\n| --- | --- |\n| a \\| b | c |\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3);
        // Group the body row's spans into cells, splitting on the ` | `
        // separator spans (content == " | "). The escaped pipe lives
        // inside a cell span and is never a ` | ` separator span, so it
        // stays in its cell.
        let mut cells: Vec<String> = Vec::new();
        let mut cur = String::new();
        for s in &lines[2].spans {
            if s.content.as_ref() == " | " {
                cells.push(std::mem::take(&mut cur));
            } else {
                cur.push_str(s.content.as_ref());
            }
        }
        cells.push(cur);
        assert_eq!(
            cells.len(),
            2,
            "escaped pipe does not split a column: {cells:?}"
        );
        assert_eq!(cells[0], "a | b", "escaped pipe renders as literal '|'");
        // The last cell may carry a trailing alignment pad; trim it.
        assert_eq!(
            cells[1].trim_end(),
            "c",
            "second cell is 'c' (padded to col width): {cells:?}"
        );
    }

    #[test]
    fn table_clips_wide_non_last_column() {
        // A table whose WIDE column is the FIRST one (not the last) must
        // still be clipped to the render width — the old code only shrunk
        // the last column, leaving a wide first column overflowing.
        let src = "| aaaaaaaaaaaaaaaa | b |\n| --- | --- |\n| aaaaaaaaaaaaaaaa | b |\n";
        let lines = render(src, 15);
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            // The divider uses `─`/`┼` (each 1 cell) so its width matches
            // the data rows; both must stay within `width`.
            assert!(w <= 15, "line {i} overflows width 15: w={w}");
        }
        // The wide first column was truncated with an ellipsis.
        let body: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            body.contains('…'),
            "wide first column is truncated: {body:?}"
        );
    }

    #[test]
    fn table_missing_separator_is_not_a_table() {
        // A header-shaped row NOT followed by a separator must render as
        // ordinary paragraphs — no divider, no alignment, no " | " join
        // reformatting (the raw text is preserved as a paragraph).
        let src = "| a | b |\njust text\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 2, "two paragraph lines, no divider");
        let second: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(second, "just text", "second line is the paragraph verbatim");
        for line in &lines {
            let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(!t.contains('┼'), "no table divider: {t:?}");
        }
    }

    #[test]
    fn table_single_column() {
        // A one-column table still renders header + divider + body.
        let src = "| h |\n| --- |\n| d |\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let body: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        // No " | " separator in a single-column table.
        assert_eq!(header, "h");
        assert_eq!(body, "d");
        // The divider is a single `─` run (no junction glyph).
        let div: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !div.contains('┼'),
            "single-col table has no junction: {div:?}"
        );
    }

    #[test]
    fn table_row_with_trailing_spaces() {
        // GFM trims trailing whitespace in cells, so trailing spaces must
        // not throw off column-width alignment.
        let src = "| a | bb   |\n| --- | --- |\n| a | bb   |\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3);
        for &i in &[0usize, 2] {
            let row: String = lines[i].spans.iter().map(|s| s.content.as_ref()).collect();
            let cells: Vec<&str> = row.split(" | ").collect();
            assert_eq!(cells.len(), 2);
            assert_eq!(
                cells[1], "bb",
                "row {i} col1 trimmed of trailing space: {cells:?}"
            );
        }
    }

    // (2) Nested lists ----------------------------------------------------

    #[test]
    fn nested_unordered_list_indent_differs() {
        // Top-level and nested items must have DIFFERENT indent prefixes;
        // the nested item's leading-space count strictly exceeds the top's.
        let src = "- top\n  - nested\n- top2\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 3);
        let lead_of = |line: &Line| -> usize {
            let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            t.len() - t.trim_start().len()
        };
        let top = lead_of(&lines[0]);
        let nested = lead_of(&lines[1]);
        let top2 = lead_of(&lines[2]);
        assert!(nested > top, "nested indent > top: {top} vs {nested}");
        assert_eq!(top, top2, "both top-level items share indent");
    }

    #[test]
    fn nested_ordered_list_indent_differs() {
        // Ordered nested list: `1.` top, `  1.` nested — the nested item's
        // prefix is indented more than the top item's.
        let src = "1. top\n  1. nested\n";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 2);
        let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let nested: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(top.contains("1."), "top shows its number: {top:?}");
        assert!(nested.contains("1."), "nested shows its number: {nested:?}");
        // Both items carry the ordered-list "  N. " prefix, but the nested
        // one is indented further (depth 1 adds another "  ").
        let top_lead = top.len() - top.trim_start().len();
        let nested_lead = nested.len() - nested.trim_start().len();
        assert!(
            nested_lead > top_lead,
            "nested ordered indent > top: {top_lead} vs {nested_lead}"
        );
        // The nested item is indented at least 4 columns (depth-1 prefix is
        // "  " + "  1. " = 4 leading spaces) vs the top's 2.
        assert!(nested_lead >= 4, "nested indent >= 4: {nested_lead}");
    }

    #[test]
    fn multi_digit_ordered_list_renders_as_list() {
        // `12. item` must render as an ordered list item (with the bullet
        // prefix), NOT fall through to a plain paragraph. The old
        // strip_prefix(predicate) stripped only one digit, so `12.` failed
        // the `. ` check after stripping `1`.
        let lines = render("12. item", 80);
        assert_eq!(lines.len(), 1, "one list line");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // The list-item prefix is "  12. " — the number is preserved and
        // the line is NOT the raw paragraph "12. item".
        assert!(text.contains("12."), "number preserved: {text:?}");
        assert!(
            lines[0].spans.iter().any(|s| s.content.contains("12.")),
            "12. appears in a prefix span, not a raw paragraph: {text:?}"
        );
    }

    // (3) Checkboxes ------------------------------------------------------

    #[test]
    fn checkbox_checked_prefix_is_check_glyph() {
        let lines = render("- [x] done", 80);
        assert_eq!(lines.len(), 1);
        // The prefix span content is exactly "<glyph>  " (glyph + 2 spaces).
        let prefix = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains(theme::glyph::COMPLETED))
            .expect("✓ prefix span");
        assert!(
            prefix.content.starts_with(theme::glyph::COMPLETED),
            "prefix begins with ✓: {:?}",
            prefix.content
        );
        assert_eq!(prefix.style, theme::success(), "✓ span is success-styled");
    }

    #[test]
    fn checkbox_unchecked_prefix_is_box_glyph() {
        let lines = render("- [ ] todo", 80);
        assert_eq!(lines.len(), 1);
        let prefix = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains(theme::glyph::UNCHECKED))
            .expect("☐ prefix span");
        assert!(
            prefix.content.starts_with(theme::glyph::UNCHECKED),
            "prefix begins with ☐: {:?}",
            prefix.content
        );
        assert_eq!(prefix.style, theme::muted(), "☐ span is muted");
    }

    #[test]
    fn checkbox_uppercase_x_is_checked() {
        // `- [X]` is also a checked item (GFM allows both).
        let lines = render("- [X] done", 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains(theme::glyph::COMPLETED), "[X] -> ✓: {text:?}");
    }

    #[test]
    fn list_without_checkbox_uses_bullet() {
        // A plain `- item` (no `[ ]`/`[x]`) still uses the bullet glyph.
        let lines = render("- item", 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('•'), "plain list uses bullet: {text:?}");
        assert!(
            !text.contains(theme::glyph::COMPLETED),
            "no check glyph: {text:?}"
        );
        assert!(
            !text.contains(theme::glyph::UNCHECKED),
            "no box glyph: {text:?}"
        );
    }

    // (4) Recursive inline ------------------------------------------------

    #[test]
    fn recursive_bold_italic_inner_has_both_modifiers() {
        // `**bold *italic* bold**` -> inner "italic" is italic AND bold.
        let lines = render("**bold *italic* bold**", 80);
        assert_eq!(lines.len(), 1);
        let italic = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "italic")
            .expect("inner italic span");
        let mods = italic.style.add_modifier;
        assert!(mods.contains(Modifier::ITALIC), "inner is italic");
        assert!(
            mods.contains(Modifier::BOLD),
            "inner is ALSO bold (layered)"
        );
        // No literal `*` delimiters leaked into any span content.
        for s in &lines[0].spans {
            assert!(!s.content.contains('*'), "no literal '*': {:?}", s.content);
        }
    }

    #[test]
    fn recursive_link_in_bold_is_bold_and_underlined() {
        // Asserts the OSC-8 escape IS emitted (finds a span with `\x1b]8;;`),
        // so the flag must be true. Serialize against the flag/env tests.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // `**a [b](u) c**` -> the link label "b" is bold AND underlined.
        let lines = render("**a [b](u) c**", 80);
        assert_eq!(lines.len(), 1);
        // The link span contains the OSC-8 escape and the label "b".
        let link = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("\x1b]8;;") && s.content.contains('b'))
            .expect("OSC-8 link span inside bold");
        let mods = link.style.add_modifier;
        assert!(mods.contains(Modifier::BOLD), "link is bold (outer layer)");
        assert!(mods.contains(Modifier::UNDERLINED), "link is underlined");
        assert_eq!(link.style.fg, Some(Color::Cyan), "link keeps accent fg");
    }

    #[test]
    fn unclosed_bold_with_inner_italic_is_safe() {
        // `**unclosed *italic*` — the outer bold is unclosed. Sensible
        // behavior: emit the whole thing as a literal (no styling), which
        // means the inner italic does NOT render as italic. The contract
        // we test: one line, no panic, and the literal text survives.
        let lines = render("**unclosed *italic*", 80);
        assert_eq!(lines.len(), 1, "single line, no panic");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("italic"),
            "inner word text survives: {text:?}"
        );
        // Because bold was unclosed, no span carries the BOLD modifier.
        assert!(
            !lines[0]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "unclosed bold does not apply BOLD: {text:?}"
        );
    }

    #[test]
    fn deeply_nested_inline_does_not_panic() {
        // Deeply nested / mismatched delimiters must not panic or loop.
        let deep = "**a *b _c_ b* a**".repeat(5);
        let lines = render(&deep, 80);
        // At width 80 this wraps to a handful of lines; the key contract is
        // that it terminates and produces at least one line.
        assert!(!lines.is_empty(), "deeply nested inline terminates");
        // And an inner-most word "c" survives somewhere in the output.
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(all.contains('c'), "inner-most content survives: {all:?}");
    }

    #[test]
    fn empty_bold_and_italic_delimiters_are_safe() {
        // Edge inputs that must not infinite-loop or panic: bare `**`, `*`,
        // `***`, `~~`, nested empty `** **`.
        for src in &["**", "*", "***", "~~", "** **", "* *", "``", "**``**"] {
            let lines = render(src, 80);
            assert!(lines.len() >= 1, "renders >=1 line for {src:?}");
        }
    }

    // (5) OSC-8 hyperlinks ------------------------------------------------

    #[test]
    fn osc8_span_contains_opener_label_and_closer() {
        // Asserts the OSC-8 escape IS emitted (flag must be true). Serialize
        // against the flag/env tests. See `osc8_hyperlink_emitted`.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // `[x](http://e.com)` -> one span's content contains the OSC-8
        // opener `\x1b]8;;http://e.com\x1b\\`, the label "x", AND the
        // closer `\x1b]8;;\x1b\\`.
        let lines = render("[x](http://e.com)", 80);
        assert_eq!(lines.len(), 1);
        let span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("\x1b]8;;"))
            .expect("OSC-8 span");
        let c = span.content.as_ref();
        assert!(
            c.contains("\x1b]8;;http://e.com\x1b\\"),
            "opener present: {c:?}"
        );
        assert!(c.contains("\x1b]8;;\x1b\\"), "closer present: {c:?}");
        assert!(c.contains('x'), "label 'x' present: {c:?}");
    }

    #[test]
    fn osc8_long_url_omits_escape() {
        // A URL longer than 200 chars must fall back to label-only (no
        // OSC-8 escape anywhere in the rendered content).
        let mut long = "http://e.com/".to_string();
        long.push_str(&"z".repeat(200));
        let src = format!("[lbl]({long})");
        let lines = render(&src, 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("\x1b]8;;"), "long URL omits OSC-8: {text:?}");
        assert!(text.contains("lbl"), "label still shown: {text:?}");
    }

    #[test]
    fn osc8_malformed_open_bracket_emits_literal() {
        // `[x` with no closing `]` emits the literal "[x".
        let lines = render("[x", 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "[x", "malformed link is literal: {text:?}");
        assert!(
            !text.contains("\x1b]8;;"),
            "no OSC-8 for malformed link: {text:?}"
        );
    }

    #[test]
    fn bracketed_non_link_emits_full_literal() {
        // `[label]` (closing bracket but no `(`) must round-trip the FULL
        // `[label]` including the closing `]` — the old code dropped the
        // consumed `]`.
        let lines = render("[label] not a link", 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[label]"),
            "closing bracket preserved: {text:?}"
        );
        assert!(
            !text.contains("\x1b]8;;"),
            "no OSC-8 for non-link: {text:?}"
        );
    }

    #[test]
    fn osc8_url_with_control_char_falls_back_to_label() {
        // A URL containing a control char must not embed OSC-8 (control
        // chars inside the escape could corrupt the terminal).
        let src = "[lbl](http://e.com/\u{0007})";
        let lines = render(src, 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("\x1b]8;;"),
            "control-char URL omits OSC-8: {text:?}"
        );
        assert!(text.contains("lbl"), "label shown: {text:?}");
    }

    // ---- #17: OSC-8 capability detection (flag-gated emission) ----

    #[test]
    fn osc8_disabled_omits_escape_label_only() {
        // Serialize against the other OSC-8 flag/env tests: the capability
        // flag and `LIBERTAI_OSC8` env var are process-global, so a concurrent
        // flag/env test would flip the global between our `set` and `render`,
        // breaking the assertion. Hold the guard for the whole test.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // With the capability flag OFF, `[label](url)` must fall back to the
        // underlined label only — NO OSC-8 escape anywhere. This is the
        // contract terminals that mangle OSC-8 rely on. The flag is a
        // process-global AtomicBool (like VIM_INPUT_ENABLED), so flip it and
        // restore the prior value on the way out so this test does not
        // perturb the global state for other tests.
        let prior = osc8_enabled();
        set_osc8_enabled(false);
        let lines = render("[label](https://example.com)", 80);
        set_osc8_enabled(prior);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("\x1b]8;;"),
            "OSC-8 disabled -> no escape emitted: {text:?}"
        );
        assert!(text.contains("label"), "label still shown: {text:?}");
        // The label span is still styled (underlined + accent) — only the
        // escape is suppressed, not the link styling.
        let link = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("label"))
            .expect("label span");
        assert!(
            link.style.add_modifier.contains(Modifier::UNDERLINED),
            "disabled OSC-8 keeps underlined label styling"
        );
    }

    #[test]
    fn osc8_enabled_emits_escape() {
        // Serialize against the other OSC-8 flag/env tests (process-global
        // flag/env). See `osc8_disabled_omits_escape_label_only`.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // With the flag ON (the default), OSC-8 is emitted as before. Guards
        // against a regression that flips the polarity. Restore prior state.
        let prior = osc8_enabled();
        set_osc8_enabled(true);
        let lines = render("[label](https://example.com)", 80);
        set_osc8_enabled(prior);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\x1b]8;;https://example.com\x1b\\"),
            "OSC-8 enabled -> opener present: {text:?}"
        );
        assert!(text.contains("\x1b]8;;\x1b\\"), "closer present: {text:?}");
    }

    #[test]
    fn osc8_disabled_inside_bold_is_label_only() {
        // Serialize against the other OSC-8 flag/env tests (process-global
        // flag/env). See `osc8_disabled_omits_escape_label_only`.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // The flag gate is read at the link render site, so a link nested in
        // bold (`**[x](u)**`) also suppresses the escape when disabled — the
        // outer bold layer is still applied to the (label-only) span.
        let prior = osc8_enabled();
        set_osc8_enabled(false);
        let lines = render("**[x](https://e.com)**", 80);
        set_osc8_enabled(prior);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("\x1b]8;;"), "no OSC-8 under bold: {text:?}");
        assert!(text.contains('x'), "label survives: {text:?}");
        // The label span keeps the layered BOLD modifier from the outer
        // emphasis — only the escape is suppressed.
        let label = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains('x'))
            .expect("label span");
        assert!(
            label.style.add_modifier.contains(Modifier::BOLD),
            "bold layer still applied to label-only link"
        );
    }

    #[test]
    fn osc8_probe_env_var_disables() {
        // Serialize against the other OSC-8 flag/env tests: the env var and
        // the flag it writes are process-global, so a concurrent flag/env test
        // would race the `probe`/assertion. Hold the guard for the whole test.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // `probe_osc8_capability` honors `LIBERTAI_OSC8=0` by turning the flag
        // OFF. Uses a child env snapshot so the real process env is restored
        // after the test (env vars are process-global).
        let prior_flag = osc8_enabled();
        let prior_env = std::env::var("LIBERTAI_OSC8").ok();
        std::env::set_var("LIBERTAI_OSC8", "0");
        probe_osc8_capability();
        assert!(!osc8_enabled(), "LIBERTAI_OSC8=0 disables the flag");
        // Restore env + flag for other tests.
        match prior_env {
            Some(v) => std::env::set_var("LIBERTAI_OSC8", v),
            None => std::env::remove_var("LIBERTAI_OSC8"),
        }
        set_osc8_enabled(prior_flag);
    }

    #[test]
    fn osc8_probe_env_var_unknown_keeps_default() {
        // Serialize against the other OSC-8 flag/env tests (process-global
        // env/flag). See `osc8_probe_env_var_disables`.
        let _guard = OSC8_TEST_GUARD.lock().unwrap();
        // An unrecognized `LIBERTAI_OSC8` value must NOT change the flag — it
        // stays at its current setting (the default-on behavior).
        let prior_flag = osc8_enabled();
        set_osc8_enabled(true); // establish a known baseline
        let prior_env = std::env::var("LIBERTAI_OSC8").ok();
        std::env::set_var("LIBERTAI_OSC8", "maybe");
        probe_osc8_capability();
        assert!(osc8_enabled(), "unknown env value keeps the flag as-is");
        match prior_env {
            Some(v) => std::env::set_var("LIBERTAI_OSC8", v),
            None => std::env::remove_var("LIBERTAI_OSC8"),
        }
        set_osc8_enabled(prior_flag);
    }

    // ---- #18: recursive inline parse depth guard ----

    #[test]
    fn pathological_inline_run_terminates() {
        // The pathological input from the bug report: a long run of `**`
        // markers around some text. Whether or not the greedy parser
        // interprets it as deep nesting, the contract is that `render`
        // TERMINATES (no infinite loop, no stack overflow, no panic) and the
        // inner text survives. The depth guard ensures bounded recursion.
        const N: usize = 64; // well above MAX_INLINE_DEPTH (32)
        let mut src = String::new();
        for _ in 0..N {
            src.push_str("**");
        }
        src.push_str("inner");
        for _ in 0..N {
            src.push_str("**");
        }
        let lines = render(&src, 200);
        assert!(!lines.is_empty(), "pathological run terminates");
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(all.contains("inner"), "inner text survives: {all:?}");
    }

    #[test]
    fn deeply_nested_mixed_markers_terminates() {
        // A unit that nests all three emphasis markers (bold > italic >
        // strike), repeated many times, exercises every recursion site
        // (bold, italic, strike) at scale. It must terminate and not
        // overflow; the inner-most text survives.
        let unit = "**a *b ~~c~~ b* a**";
        let deep = unit.repeat(40);
        let lines = render(&deep, 200);
        assert!(!lines.is_empty(), "deeply nested mixed markers terminate");
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(all.contains('c'), "inner-most content survives: {all:?}");
    }

    #[test]
    fn normal_nesting_is_unaffected_by_depth_guard() {
        // Normal, shallow nesting (`**bold *italic* bold**`, depth 2) is
        // far below MAX_INLINE_DEPTH (32) and must still layer modifiers as
        // before — the depth guard must NOT fire here. Guards against an
        // off-by-one that would short-circuit legitimate nesting.
        let lines = render("**bold *italic* bold**", 80);
        assert_eq!(lines.len(), 1);
        let italic = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "italic")
            .expect("inner italic span");
        let mods = italic.style.add_modifier;
        assert!(mods.contains(Modifier::ITALIC), "inner is italic");
        assert!(
            mods.contains(Modifier::BOLD),
            "inner is ALSO bold (layered)"
        );
        // No literal delimiters leaked (the parse completed normally).
        for s in &lines[0].spans {
            assert!(
                !s.content.contains('*'),
                "no literal '*' leaked: {:?}",
                s.content
            );
        }
    }

    #[test]
    fn depth_guard_fires_at_limit_emits_plain() {
        // At the depth limit the guard FIRES: a `**bold**` whose parse is
        // invoked at depth == MAX_INLINE_DEPTH must emit its captured inner
        // text as a single PLAIN span (no BOLD modifier, no further
        // parsing). We call the private `parse_inline_depth` directly at the
        // limit (tests live in the same module) so the guard path is
        // exercised deterministically regardless of how the greedy parser
        // interprets a markup nest.
        let spans = parse_inline_depth("**core**", MAX_INLINE_DEPTH);
        // The captured "core" is present, and at least one span holding it
        // carries NO BOLD modifier (the guard emitted it plain).
        let core_plain = spans
            .iter()
            .any(|s| s.content.contains("core") && !s.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            core_plain,
            "depth guard emits 'core' as a plain (non-bold) span: {:?}",
            spans
        );
    }

    #[test]
    fn depth_guard_just_below_limit_still_parses() {
        // One level BELOW the limit, the guard does NOT fire: `**bold**`
        // invoked at depth MAX_INLINE_DEPTH - 1 still recurses one more
        // level and applies the BOLD modifier. Guards the off-by-one in the
        // other direction (the limit must not trigger too early).
        let spans = parse_inline_depth("**core**", MAX_INLINE_DEPTH - 1);
        // `depth + 1 == MAX_INLINE_DEPTH`, which is NOT > MAX_INLINE_DEPTH,
        // so it recurses and layers BOLD. The "core" span is bold.
        let core_bold = spans
            .iter()
            .any(|s| s.content.contains("core") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            core_bold,
            "just below the limit, 'core' is still bold (guard does not fire): {:?}",
            spans
        );
    }

    /// The literal spec input: 40 levels of nested `***...***` (triple-star,
    /// interleaving bold `**` and italic `*`). `parse_inline` must TERMINATE
    /// (return within the depth limit) and the inner-most tail text must
    /// survive — no panic, no stack overflow, no infinite loop. The depth
    /// guard bounds the recursion regardless of how the greedy parser
    /// interprets the `***` run. (The deterministic "tail renders plain"
    /// contract is pinned separately by `depth_guard_fires_at_limit_emits_plain`,
    /// which calls `parse_inline_depth` directly at the limit.)
    #[test]
    fn forty_deep_triple_star_nest_terminates() {
        const DEPTH: usize = 40; // well above MAX_INLINE_DEPTH (32)
                                 // Interleaved bold (`**`) + italic (`*`) openers, then text, then the
                                 // matching closers in reverse. `***` = bold-then-italic opener.
        let mut src = String::new();
        for _ in 0..DEPTH {
            src.push_str("***");
        }
        src.push_str("tail");
        for _ in 0..DEPTH {
            src.push_str("***");
        }
        let lines = render(&src, 200);
        assert!(!lines.is_empty(), "40-deep *** nest terminates (no panic)");
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            all.contains("tail"),
            "inner-most tail text survives: {all:?}"
        );
    }
}
