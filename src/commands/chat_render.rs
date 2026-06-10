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
}

impl MarkdownStream {
    pub fn new(render: bool) -> Self {
        Self {
            render,
            console: if render { Some(PiConsole::new()) } else { None },
            pending: String::new(),
            received: false,
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

    fn render_block(&self, block: &str) {
        if block.trim().is_empty() {
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
