//! Shared terminal-rendering helpers for the conversational commands
//! (`libertai chat`, `libertai ask`).
//!
//! Markdown → ANSI rendering is delegated to `pi::tui::PiConsole` (the
//! rich_rust-backed renderer the pinned pi_agent_rust crate already ships
//! for its own REPL), so chat output matches `libertai code` styling and
//! inherits rich_rust's NO_COLOR / dumb-terminal / width handling for
//! free. This module adds the two pieces pi does not provide:
//!
//! - capability gates (`styling_enabled`, `markdown_enabled_stdout`) so
//!   *our* accents (header, prompt, errors) also honour NO_COLOR, piped
//!   output, and `TERM=dumb`;
//! - [`MarkdownStream`], a progressive renderer that buffers streamed
//!   SSE deltas and flushes complete markdown blocks (paragraphs, closed
//!   code fences) as they arrive — no cursor-rewind, so it never
//!   flickers and never duplicates content that scrolled off screen.

use std::io::{IsTerminal, Write};

use pi::tui::PiConsole;
use rich_rust::cells::{cell_len, get_character_cell_size};
use rich_rust::renderables::Markdown;
use rich_rust::Console;

/// True when `NO_COLOR` is set to a non-empty value (https://no-color.org).
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// True when the terminal advertises itself as too dumb for styling.
fn dumb_term() -> bool {
    std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false)
}

/// Whether our own ANSI accents (prompt/header/error colors) should be
/// emitted on a stream. `stream_is_tty` is the caller's
/// `IsTerminal::is_terminal()` result for whichever stream it writes to.
pub fn styling_enabled(stream_is_tty: bool) -> bool {
    stream_is_tty && !no_color() && !dumb_term()
}

/// Whether stdout should get markdown-rendered output. NO_COLOR keeps
/// markdown layout but rich_rust drops the ANSI styling for it; a dumb
/// terminal or piped stdout falls back to raw text passthrough.
pub fn markdown_enabled_stdout() -> bool {
    std::io::stdout().is_terminal() && !dumb_term()
}

/// Streaming sink for assistant deltas.
///
/// In raw mode (piped stdout, dumb terminal) every delta is printed
/// immediately, byte-for-byte — identical to the pre-overhaul behavior,
/// so scripts see an unchanged stream. In markdown mode deltas are
/// buffered and flushed block-by-block: a block is complete at a blank
/// line outside a fenced code block, so fences render whole, with
/// syntax highlighting, exactly once.
///
/// GFM pipe tables render through the same path: rich_rust's markdown
/// renderer (pulldown-cmark with `ENABLE_TABLES`) draws them as
/// box-drawn tables, and a table is a contiguous run of `|` lines with
/// no internal blank line, so the block-boundary rule below already
/// guarantees the whole table reaches `render_markdown` in one piece
/// (verified against the pinned pi_agent_rust rev under a PTY).
pub struct MarkdownStream {
    render: bool,
    console: Option<PiConsole>,
    /// Buffered text not yet rendered (markdown mode only).
    pending: String,
    /// Total characters pushed (used by callers to detect empty replies).
    received: bool,
    /// Claude-Code-style turn decoration (`libertai code` only): the
    /// first rendered line of a turn carries an inline marker ("● "),
    /// every later line a two-space hanging indent. `None` for
    /// `libertai chat`/`ask` — their visuals are deliberately unchanged
    /// (no marker, no indent). Never set in raw mode, so piped output
    /// stays plain assistant text.
    decor: Option<TurnDecor>,
}

/// Columns reserved by the turn marker / hanging indent.
const TURN_INDENT: usize = 2;

struct TurnDecor {
    /// Pre-styled marker, exactly [`TURN_INDENT`] columns wide once the
    /// ANSI is stripped (e.g. `"\x1b[1m●\x1b[0m "`).
    marker: String,
    /// The marker has not been emitted for the current text segment yet.
    marker_pending: bool,
}

impl MarkdownStream {
    pub fn new(render: bool) -> Self {
        Self {
            render,
            console: if render { Some(PiConsole::new()) } else { None },
            pending: String::new(),
            received: false,
            decor: None,
        }
    }

    /// Markdown stream whose rendered output gets the Claude-Code turn
    /// treatment: `marker` inline with the first rendered line, a
    /// two-space hanging indent under it for every later line. The
    /// marker must occupy [`TURN_INDENT`] columns once ANSI is
    /// stripped. In raw mode (`render == false`) this is identical to
    /// [`MarkdownStream::new`] — no marker/indent games on piped output.
    pub fn with_turn_marker(render: bool, marker: String) -> Self {
        let mut stream = Self::new(render);
        if render {
            stream.decor = Some(TurnDecor {
                marker,
                marker_pending: false,
            });
        }
        stream
    }

    /// True when output goes through the markdown renderer (TTY mode).
    pub fn renders_markdown(&self) -> bool {
        self.render
    }

    /// Arm the turn marker: the next rendered non-blank line gets the
    /// inline marker. Called by `libertai code` at the start of each
    /// assistant text segment (a turn can have several, split by tool
    /// calls). No-op unless built via [`Self::with_turn_marker`].
    pub fn begin_marked_block(&mut self) {
        if let Some(decor) = &mut self.decor {
            decor.marker_pending = true;
        }
    }

    /// True once at least one delta has arrived.
    pub fn saw_output(&self) -> bool {
        self.received
    }

    /// Feed one streamed delta.
    pub fn push(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.received = true;
        if !self.render {
            print!("{delta}");
            std::io::stdout().flush().ok();
            return;
        }
        self.pending.push_str(delta);
        if let Some(cut) = complete_block_end(&self.pending) {
            let block: String = self.pending.drain(..cut).collect();
            self.render_block(&block);
        }
    }

    /// Flush whatever remains. In raw mode this prints the same trailing
    /// newline the old REPL always emitted after a response.
    pub fn finish(&mut self) {
        if !self.render {
            println!();
            return;
        }
        let rest = std::mem::take(&mut self.pending);
        if !rest.trim().is_empty() {
            self.render_block(&rest);
        }
    }

    /// Render any buffered partial block *now*, without ending the
    /// stream. Used by `libertai code` to flush assistant prose before
    /// out-of-band chrome (tool markers, approval prompts) prints, so
    /// the text always appears above the event that interrupted it.
    /// Raw mode is a no-op — deltas were already printed verbatim.
    pub fn flush_pending(&mut self) {
        if !self.render {
            return;
        }
        let rest = std::mem::take(&mut self.pending);
        if !rest.trim().is_empty() {
            self.render_block(&rest);
        }
    }

    fn render_block(&mut self, block: &str) {
        if block.trim().is_empty() {
            return;
        }
        if let Some(decor) = &mut self.decor {
            // Decorated path (`libertai code`): render to a string at a
            // width reduced by the indent, then re-emit each line with
            // the marker / hanging indent so wrapped lines still fit.
            let width = stdout_render_width().saturating_sub(TURN_INDENT).max(20);
            let rendered = render_markdown_ansi(block, width);
            let out = decorate_block(&rendered, &decor.marker, &mut decor.marker_pending, width);
            print!("{out}");
            // Trailing blank line: same block separator the plain path
            // gets from its `println!()` below.
            println!();
            std::io::stdout().flush().ok();
            return;
        }
        if let Some(console) = &self.console {
            console.render_markdown(block);
            // render_markdown guarantees one trailing newline; add the
            // blank line that separated this block from the next in the
            // source so progressive output keeps document spacing.
            println!();
            std::io::stdout().flush().ok();
        }
    }
}

/// Render one markdown block to an ANSI string, wrapped at `width`
/// columns. The console is buffer-backed but forced into terminal mode
/// so styling matches what PiConsole would print to a real TTY; rich's
/// detection still honours NO_COLOR (layout kept, colors dropped).
/// Code fences render through rich_rust's `Syntax` (the `full` feature
/// set), tables as box-drawn grids — both width-aware, so the hanging
/// indent applied afterwards never pushes them past the terminal edge.
fn render_markdown_ansi(block: &str, width: usize) -> String {
    let console = Console::builder()
        .force_terminal(true)
        .width(width)
        .file(Box::new(std::io::sink()))
        .build();
    let markdown = Markdown::new(block);
    let segments = markdown.render(width);
    let mut buf: Vec<u8> = Vec::new();
    let _ = console.print_segments_to(&mut buf, &segments);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Re-emit rendered ANSI lines with the turn treatment: while
/// `marker_pending`, the first line with visible content gets `marker`
/// inline; every other content line gets a [`TURN_INDENT`]-space
/// hanging indent. Blank lines stay bare (no trailing spaces). ANSI
/// styling is preserved — prefixes go before the line's first escape.
///
/// `content_width` is the column budget per line *excluding* the
/// indent. rich_rust wraps tables and code at the render width but
/// leaves prose/bullet lines unwrapped (the bare terminal used to
/// soft-wrap those at column 0); [`wrap_ansi_cells`] breaks them here
/// so every continuation row keeps the hanging indent.
fn decorate_block(
    rendered: &str,
    marker: &str,
    marker_pending: &mut bool,
    content_width: usize,
) -> String {
    let mut out = String::with_capacity(rendered.len() + 64);
    let indent = " ".repeat(TURN_INDENT);
    for line in rendered.lines() {
        if strip_ansi(line).trim().is_empty() {
            out.push('\n');
            continue;
        }
        // rich pads rendered lines to the full render width; drop the
        // plain trailing spaces so re-emitted lines don't carry
        // copy-paste junk (styled padding that ends in an escape is
        // left alone — trim_end only sees literal trailing whitespace).
        // Tabs are expanded up front: `print_segments_to` passes them
        // through raw, unicode-width counts them as zero cells, and the
        // terminal would jump up to 8 columns — the exact kind of
        // counted-narrower-than-rendered drift that pushed chip-heavy
        // lines past the terminal edge and broke the hanging indent.
        let line = line.trim_end().replace('\t', "    ");
        for chunk in wrap_ansi_cells(&line, content_width) {
            if *marker_pending {
                *marker_pending = false;
                out.push_str(marker);
            } else {
                out.push_str(&indent);
            }
            out.push_str(&chunk);
            out.push('\n');
        }
    }
    out
}

/// Wrap one rendered line at `max` **display cells**, ANSI-aware:
/// CSI escape sequences are copied through without counting, and glyph
/// widths come from rich's own cell calculus (`rich_rust::cells`) so
/// wide CJK/emoji count 2 and combining marks 0 — char-counted wrapping
/// let wide glyphs push lines past the terminal edge, where the
/// terminal soft-wrapped them at column 0 with no hanging indent.
/// Breaks prefer the last space in the chunk (no more mid-word "pee/r"
/// splits); a single word wider than the budget still hard-breaks at
/// the edge. A style span split across the break loses its color on
/// the continuation row — same trade-off Claude Code's renderer makes.
fn wrap_ansi_cells(line: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![line.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cells = 0usize;
    // `cur` contains at least one visible non-space glyph.
    let mut has_word = false;
    // Byte offset in `cur` just past the most recent space that
    // follows a word — the preferred break point.
    let mut last_space: Option<usize> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            cur.push(c);
            if chars.peek() == Some(&'[') {
                cur.push('[');
                chars.next();
                while let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                    if ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        let w = get_character_cell_size(c);
        if c == ' ' && cells + w > max && cells > 0 {
            // The overflowing char is itself a space: the chunk ends
            // exactly here — emit it whole and drop the separator.
            while cur.ends_with(' ') {
                cur.pop();
            }
            chunks.push(std::mem::take(&mut cur));
            cells = 0;
            has_word = false;
            last_space = None;
            continue;
        }
        while cells + w > max && cells > 0 {
            if let Some(bp) = last_space {
                // Word wrap: emit up to the last space, continue with
                // the partial word (its cells re-measured ANSI-free).
                let rest = cur.split_off(bp);
                while cur.ends_with(' ') {
                    cur.pop();
                }
                chunks.push(std::mem::take(&mut cur));
                cur = rest.trim_start_matches(' ').to_string();
            } else {
                // One unbroken over-wide word: hard break at the edge.
                chunks.push(std::mem::take(&mut cur));
            }
            let visible = strip_ansi(&cur);
            cells = cell_len(&visible);
            has_word = !visible.trim().is_empty();
            last_space = None;
        }
        if c == ' ' && !has_word && !chunks.is_empty() {
            // Drop leading spaces on continuation rows — the hanging
            // indent already provides the alignment.
            continue;
        }
        cur.push(c);
        cells += w;
        if c == ' ' {
            if has_word {
                last_space = Some(cur.len());
            }
        } else if w > 0 {
            has_word = true;
        }
    }
    if !cur.is_empty() || chunks.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Remove ANSI CSI sequences (`ESC [ … <final>`) so layout decisions
/// (blank-line detection, width assertions in tests) see only the
/// visible text.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Terminal width for the decorated render path; falls back to 100
/// columns when the probe fails (mirrors code_ui's
/// `FALLBACK_RENDER_WIDTH`). Probed per block so a resize mid-stream
/// affects the next block.
fn stdout_render_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(cols, _)| cols as usize)
        .filter(|cols| *cols > 0)
        .unwrap_or(100)
}

/// Byte offset just past the last *complete* markdown block in `buf`, or
/// `None` if no block has finished yet.
///
/// A block boundary is a blank line outside a fenced (``` / ~~~) code
/// block. Content inside an open fence is never flushed — the fence
/// must close first so the highlighter sees the whole block.
fn complete_block_end(buf: &str) -> Option<usize> {
    let mut in_fence = false;
    let mut fence_char = '`';
    let mut offset = 0usize;
    let mut last_boundary: Option<usize> = None;

    for line in buf.split_inclusive('\n') {
        offset += line.len();
        // Only fully received lines count; a trailing partial line (no
        // '\n' yet) can still grow.
        if !line.ends_with('\n') {
            break;
        }
        let trimmed = line.trim_start();
        let fence_open = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if fence_open {
            let c = trimmed.chars().next().unwrap_or('`');
            if !in_fence {
                in_fence = true;
                fence_char = c;
            } else if c == fence_char {
                in_fence = false;
            }
            continue;
        }
        if !in_fence && trimmed.is_empty() {
            last_boundary = Some(offset);
        }
    }

    last_boundary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_boundary_in_single_partial_paragraph() {
        assert_eq!(complete_block_end("Hello, streaming wor"), None);
        assert_eq!(complete_block_end("line one\nline two"), None);
    }

    #[test]
    fn blank_line_marks_block_complete() {
        let buf = "First paragraph.\n\nSecond para";
        let cut = complete_block_end(buf).expect("boundary");
        assert_eq!(&buf[..cut], "First paragraph.\n\n");
    }

    #[test]
    fn flushes_up_to_last_blank_line() {
        let buf = "a\n\nb\n\nc still going";
        let cut = complete_block_end(buf).expect("boundary");
        assert_eq!(&buf[..cut], "a\n\nb\n\n");
    }

    #[test]
    fn open_code_fence_is_never_flushed() {
        let buf = "Intro.\n\n```rust\nfn main() {}\n\nmore code\n";
        let cut = complete_block_end(buf).expect("boundary");
        // Only the prose before the fence is complete; the blank line
        // *inside* the open fence is not a boundary.
        assert_eq!(&buf[..cut], "Intro.\n\n");
    }

    #[test]
    fn closed_code_fence_flushes_after_following_blank_line() {
        let buf = "```py\nprint(1)\n```\n\ntail";
        let cut = complete_block_end(buf).expect("boundary");
        assert_eq!(&buf[..cut], "```py\nprint(1)\n```\n\n");
    }

    #[test]
    fn tilde_fences_track_their_own_kind() {
        let buf = "~~~\n```\nstill fenced\n~~~\n\ndone";
        let cut = complete_block_end(buf).expect("boundary");
        assert_eq!(&buf[..cut], "~~~\n```\nstill fenced\n~~~\n\n");
    }

    #[test]
    fn trailing_partial_line_does_not_count() {
        // The final "\n\n" boundary needs both newlines received.
        assert_eq!(complete_block_end("para\n"), None);
    }

    #[test]
    fn pipe_table_stays_one_block() {
        // A GFM table has no internal blank line, so the block boundary
        // lands *after* the whole table — render_markdown sees header,
        // separator, and every row together and can draw the box.
        let buf = "intro\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n| 3 | 4 |\n\ntail";
        let cut = complete_block_end(buf).expect("boundary");
        assert_eq!(
            &buf[..cut],
            "intro\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n| 3 | 4 |\n\n"
        );
    }

    #[test]
    fn partially_streamed_table_is_not_flushed_early() {
        // Mid-table (rows still arriving, no blank line yet) the only
        // complete block is the prose before it.
        let buf = "intro\n\n| a | b |\n|---|---|\n| 1 |";
        let cut = complete_block_end(buf).expect("boundary");
        assert_eq!(&buf[..cut], "intro\n\n");
    }

    #[test]
    fn flush_pending_is_noop_in_raw_mode() {
        let mut s = MarkdownStream::new(false);
        s.push("partial paragraph without newline");
        s.flush_pending();
        assert!(s.pending.is_empty());
    }

    #[test]
    fn flush_pending_drains_buffered_markdown() {
        let mut s = MarkdownStream::new(true);
        s.push("a paragraph still streaming");
        assert!(!s.pending.is_empty());
        s.flush_pending();
        assert!(s.pending.is_empty());
        // After a flush the stream keeps accepting deltas.
        s.push("more\n\n");
        assert!(s.pending.is_empty()); // complete block rendered immediately
    }

    // -- turn marker / hanging indent (the captured render path) --------

    const MARKER: &str = "\x1b[1m●\x1b[0m ";

    #[test]
    fn decorate_block_puts_marker_inline_with_first_line() {
        let mut pending = true;
        let out = decorate_block(
            "Two active experiments:\nsecond line\n",
            MARKER,
            &mut pending,
            80,
        );
        assert_eq!(
            out,
            "\x1b[1m●\x1b[0m Two active experiments:\n  second line\n"
        );
        assert!(!pending, "marker consumed by the first content line");
    }

    #[test]
    fn decorate_block_skips_ansi_only_blank_lines_for_the_marker() {
        // A styled-but-blank first line must not eat the marker; blank
        // lines stay bare (no trailing indent spaces).
        let mut pending = true;
        let out = decorate_block(
            "\x1b[2m   \x1b[0m\nreal content\n",
            MARKER,
            &mut pending,
            80,
        );
        assert_eq!(out, "\n\x1b[1m●\x1b[0m real content\n");
    }

    #[test]
    fn decorate_block_indents_everything_once_marker_is_spent() {
        // Second and later blocks of the same turn: hanging indent only.
        let mut pending = false;
        let out = decorate_block("a\n\nb\n", MARKER, &mut pending, 80);
        assert_eq!(out, "  a\n\n  b\n");
    }

    #[test]
    fn decorate_block_preserves_ansi_on_indented_lines() {
        let mut pending = true;
        let out = decorate_block(
            "\x1b[1mTitle\x1b[0m\n\x1b[36mcyan body\x1b[0m\n",
            MARKER,
            &mut pending,
            80,
        );
        assert_eq!(
            out,
            "\x1b[1m●\x1b[0m \x1b[1mTitle\x1b[0m\n  \x1b[36mcyan body\x1b[0m\n"
        );
    }

    #[test]
    fn decorate_block_hard_wraps_overlong_words_under_the_indent() {
        // rich leaves prose unwrapped; the decorator must break it so
        // every continuation row carries the hanging indent. A single
        // unbroken word still hard-breaks at the edge.
        let mut pending = true;
        let out = decorate_block("abcdefghij\n", MARKER, &mut pending, 4);
        assert_eq!(out, "\x1b[1m●\x1b[0m abcd\n  efgh\n  ij\n");
    }

    #[test]
    fn wrap_ansi_cells_ignores_escape_sequences_when_counting() {
        let chunks = wrap_ansi_cells("\x1b[1mabc\x1b[0mdef", 3);
        assert_eq!(chunks, vec!["\x1b[1mabc\x1b[0m", "def"]);
        // Short lines come back whole, escapes intact.
        assert_eq!(
            wrap_ansi_cells("\x1b[36mok\x1b[0m", 10),
            vec!["\x1b[36mok\x1b[0m"]
        );
    }

    #[test]
    fn wrap_ansi_cells_breaks_at_word_boundaries() {
        assert_eq!(
            wrap_ansi_cells("hello world foo", 11),
            vec!["hello world", "foo"]
        );
        // The break drops the separating space; the partial word
        // continues on the next row with no leading gap.
        assert_eq!(
            wrap_ansi_cells("hello world foo", 10),
            vec!["hello", "world foo"]
        );
        // Leading indentation of the original line is preserved on the
        // first row only.
        assert_eq!(wrap_ansi_cells("  ab cd", 5), vec!["  ab", "cd"]);
    }

    #[test]
    fn wrap_ansi_cells_counts_display_cells_for_wide_glyphs() {
        // CJK glyphs are 2 cells each: four of them must split 2+2 at
        // a 4-cell budget (a char-counted wrap would emit all four and
        // overflow the terminal, breaking the hanging indent at col 0).
        assert_eq!(wrap_ansi_cells("你好世界", 4), vec!["你好", "世界"]);
        // A wide glyph never straddles the boundary.
        assert_eq!(wrap_ansi_cells("a你b", 2), vec!["a", "你", "b"]);
    }

    #[test]
    fn chip_heavy_paragraph_never_exceeds_terminal_width() {
        // Regression for the live-session overflow: a paragraph dense
        // with inline-code chips must come out of the full pipeline
        // (markdown render → decorate) with every line within the
        // terminal width in *display cells* — overflow makes the
        // terminal soft-wrap at column 0, killing the hanging indent.
        let width = 80usize;
        let md = "Found the issue. Look at `joinRoom` in `discovery.ts` — the `roomId` is \
                  never forwarded to the `BrokerClient` constructor, so every `peer` \
                  registers under the broker's `lobby` namespace and a random `suffix` \
                  gets appended to the `peerId` each reconnect via `setRoom`.\n";
        let rendered = render_markdown_ansi(md, width - TURN_INDENT);
        let mut pending = true;
        let out = decorate_block(&rendered, MARKER, &mut pending, width - TURN_INDENT);
        let mut content = 0;
        for line in out.lines() {
            let visible = strip_ansi(line);
            assert!(
                cell_len(&visible) <= width,
                "line exceeds {width} cells ({}): {visible:?}",
                cell_len(&visible)
            );
            if !visible.trim().is_empty() {
                content += 1;
            }
        }
        assert!(content >= 3, "expected a wrapped multi-line paragraph");
        // Word wrap: no break splits an ASCII word in two (every
        // continuation row starts at a word boundary).
        for line in out.lines().skip(1) {
            let visible = strip_ansi(line);
            let body = visible.trim_start();
            assert!(
                !body.starts_with(|c: char| c == ',' || c == '.'),
                "break landed mid-token: {visible:?}"
            );
        }
    }

    #[test]
    fn wide_glyph_paragraph_never_exceeds_terminal_width() {
        let width = 40usize;
        let md = "部署完成 🚀 之后请在两个标签页中验证 `joinRoom` 的同步行为，确认 `peerId` 列表一致。\n";
        let rendered = render_markdown_ansi(md, width - TURN_INDENT);
        let mut pending = true;
        let out = decorate_block(&rendered, MARKER, &mut pending, width - TURN_INDENT);
        for line in out.lines() {
            let visible = strip_ansi(line);
            assert!(
                cell_len(&visible) <= width,
                "wide-glyph line exceeds {width} cells ({}): {visible:?}",
                cell_len(&visible)
            );
        }
    }

    #[test]
    fn tabs_in_rendered_output_are_expanded_before_wrapping() {
        // unicode-width counts '\t' as zero cells but terminals jump up
        // to 8 columns — expansion keeps the count honest.
        let mut pending = false;
        let out = decorate_block("a\tb\n", MARKER, &mut pending, 80);
        assert_eq!(out, "  a    b\n");
        assert!(!out.contains('\t'));
    }

    #[test]
    fn every_block_of_a_turn_keeps_the_hanging_indent() {
        // Regression: in a multi-paragraph turn the marker is spent on
        // block 1; blocks 2..n must still get the indent on every
        // content line (sequential decorate calls share one decor
        // state, exactly like MarkdownStream's block-by-block flushes).
        let width = 78usize;
        let blocks = [
            "Found the issue. Look at `joinRoom` for the bug.\n",
            "The problem: `roomId` is stale before `setRoom` runs.\n",
            "Let me check the browser console output next.\n",
        ];
        let mut pending = true;
        let mut outputs = Vec::new();
        for block in blocks {
            let rendered = render_markdown_ansi(block, width);
            outputs.push(decorate_block(&rendered, MARKER, &mut pending, width));
        }
        // Block 1: first content line carries the marker.
        assert!(
            outputs[0].starts_with(MARKER),
            "missing marker: {:?}",
            outputs[0]
        );
        // Blocks 2 and 3: every content line starts with the indent.
        for out in &outputs[1..] {
            for line in out.lines() {
                let visible = strip_ansi(line);
                if visible.trim().is_empty() {
                    continue;
                }
                assert!(
                    visible.starts_with("  ") && !visible.starts_with("   "),
                    "content line lost the 2-space hanging indent: {visible:?}"
                );
            }
        }
    }

    #[test]
    fn rendered_bullet_list_fits_width_with_hanging_indent() {
        // Render at (width - indent) and re-emit with the 2-space pad:
        // every visible line must fit the original width.
        let width = 40usize;
        let rendered = render_markdown_ansi(
            "- first bullet with some longer text that wraps\n- second bullet\n",
            width - TURN_INDENT,
        );
        let mut pending = true;
        let out = decorate_block(&rendered, MARKER, &mut pending, width - TURN_INDENT);
        assert!(out.contains("first bullet"));
        for line in out.lines() {
            let visible = strip_ansi(line);
            assert!(
                visible.chars().count() <= width,
                "line exceeds {width} cols: {visible:?}"
            );
        }
        // First content line carries the marker, the rest the indent.
        let first = out.lines().find(|l| !strip_ansi(l).trim().is_empty());
        assert!(first.unwrap().starts_with(MARKER));
    }

    #[test]
    fn rendered_table_lines_all_get_the_indent() {
        let rendered = render_markdown_ansi("| a | b |\n|---|---|\n| 1 | 2 |\n", 60 - TURN_INDENT);
        let mut pending = false;
        let out = decorate_block(&rendered, MARKER, &mut pending, 60 - TURN_INDENT);
        let content_lines: Vec<&str> = out
            .lines()
            .filter(|l| !strip_ansi(l).trim().is_empty())
            .collect();
        assert!(!content_lines.is_empty(), "table rendered no lines");
        for line in content_lines {
            assert!(
                strip_ansi(line).starts_with("  "),
                "table line missing hanging indent: {line:?}"
            );
        }
    }

    #[test]
    fn with_turn_marker_in_raw_mode_keeps_piped_output_undecorated() {
        // Raw mode (piped stdout): no decor is installed at all, so the
        // print/piped contracts stay byte-identical plain text.
        let s = MarkdownStream::with_turn_marker(false, MARKER.to_string());
        assert!(s.decor.is_none());
        assert!(!s.renders_markdown());
    }

    #[test]
    fn begin_marked_block_arms_the_marker_only_when_decorated() {
        let mut plain = MarkdownStream::new(true);
        plain.begin_marked_block(); // chat: no marker state, must not panic
        assert!(plain.decor.is_none());

        let mut decorated = MarkdownStream::with_turn_marker(true, MARKER.to_string());
        assert!(!decorated.decor.as_ref().unwrap().marker_pending);
        decorated.begin_marked_block();
        assert!(decorated.decor.as_ref().unwrap().marker_pending);
    }

    #[test]
    fn strip_ansi_removes_csi_sequences_only() {
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[0m plain"), "bold plain");
        assert_eq!(strip_ansi("no ansi"), "no ansi");
        assert_eq!(strip_ansi("\x1b[38;5;208mwide\x1b[0m"), "wide");
    }

    #[test]
    fn raw_mode_buffers_nothing() {
        // Raw mode prints straight through; pending must stay empty so
        // finish() only adds the legacy trailing newline.
        let mut s = MarkdownStream::new(false);
        s.push("hello ");
        s.push("world");
        assert!(s.pending.is_empty());
        assert!(s.saw_output());
    }
}
