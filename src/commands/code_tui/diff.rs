//! Unified-diff parser that emits ratatui styled [`Line`]s for the in-TUI
//! diff viewer (M7b `/diff`).
//!
//! The bg thread shells out to `git -C <cwd> diff --no-color HEAD [-- <path>]`
//! (`code_ui::git_diff_in`), ships the raw diff string back as
//! `AgentMsg::DiffReady`, and the main thread renders it here via the
//! `DiffView` overlay (`view::draw_diff_view`). Parsing is line-oriented and
//! stateless — one pass over the input — and caps the output at
//! [`MAX_DIFF_LINES`] so a huge diff can't freeze the viewer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::commands::code_tui::theme;

/// Hard cap on emitted lines. A `git diff` over a large uncommitted tree can
/// easily run into tens of thousands of lines; rendering every one as a
/// styled `Line` would allocate heavily and stall the render loop. We stop
/// here and append a `… (diff truncated)` notice.
pub const MAX_DIFF_LINES: usize = 2000;

/// Parse a unified-diff string into styled ratatui `Line<'static>`s.
///
/// Classification (per git `--no-color` unified-diff conventions):
/// - `diff --git ` / `+++` / `---` → file headers (bold).
/// - `@@ ` → hunk headers (muted/dim).
/// - `+` (not `+++`) → added line (success / green).
/// - `-` (not `---`) → removed line (error / red).
/// - ` ` (leading space) → context (dim).
/// - anything else → dim.
///
/// Empty input yields a single "(no changes)" muted line so the viewer never
/// renders a blank frame. Output is capped at [`MAX_DIFF_LINES`]; if the cap
/// is hit, a trailing `… (diff truncated, N lines)` line is appended.
pub fn parse_diff(diff: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if diff.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no changes)",
            theme::muted(),
        )));
        return lines;
    }

    let bold = theme::bold();
    let muted = theme::muted();
    let added = theme::success();
    let removed = theme::error();

    for raw in diff.lines() {
        if lines.len() >= MAX_DIFF_LINES {
            lines.push(Line::from(Span::styled(
                format!("… (diff truncated at {MAX_DIFF_LINES} lines)"),
                theme::muted(),
            )));
            break;
        }
        // Stage the line + a chosen style, then own the content into a
        // `Span::styled` so the returned `Line<'static>` borrows nothing
        // from the input `&str`.
        let (content, style): (String, Style) = if raw.starts_with("diff --git ")
            || raw.starts_with("+++")
            || raw.starts_with("---")
        {
            // file headers — bold.
            (raw.to_string(), bold)
        } else if raw.starts_with("@@ ") {
            // hunk header — muted.
            (raw.to_string(), muted)
        } else if raw.starts_with('+') {
            (raw.to_string(), added)
        } else if raw.starts_with('-') {
            (raw.to_string(), removed)
        } else {
            // context (` ` prefix) and anything else — muted.
            (raw.to_string(), muted)
        };
        lines.push(Line::from(Span::styled(content, style)));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_yields_no_changes_notice() {
        let lines = parse_diff("");
        assert_eq!(lines.len(), 1);
        // The "(no changes)" span is the only content.
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "(no changes)");
    }

    #[test]
    fn classifies_each_diff_line_kind() {
        let diff = "\
diff --git a/foo.rs b/foo.rs
index 111..222 100644
--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,4 @@
 context line
-removed line
+added line
";
        let lines = parse_diff(diff);
        // 8 input lines, none truncated.
        assert_eq!(lines.len(), 8);
        // file headers (diff --git, ---, +++) — bold (only BOLD modifier,
        // default fg, so equals theme::bold()).
        assert_eq!(lines[0].spans[0].style, theme::bold());
        assert_eq!(lines[2].spans[0].style, theme::bold());
        assert_eq!(lines[3].spans[0].style, theme::bold());
        // hunk header — muted.
        assert_eq!(lines[4].spans[0].style, theme::muted());
        // context — muted.
        assert_eq!(lines[5].spans[0].style, theme::muted());
        // removed — error.
        assert_eq!(lines[6].spans[0].style, theme::error());
        // added — success.
        assert_eq!(lines[7].spans[0].style, theme::success());
    }

    #[test]
    fn truncates_at_cap_and_appends_notice() {
        // Build a diff of MAX_DIFF_LINES + a few added lines so the cap
        // triggers.
        let mut diff = String::from("diff --git a/x b/x\n@@ -1,1 +1,1 @@\n");
        for _ in 0..(MAX_DIFF_LINES + 50) {
            diff.push_str("+a\n");
        }
        let lines = parse_diff(&diff);
        // The cap triggers after MAX_DIFF_LINES, then appends one truncation
        // notice line, so total == MAX_DIFF_LINES + 1.
        assert_eq!(lines.len(), MAX_DIFF_LINES + 1);
        let last = lines.last().unwrap();
        assert_eq!(last.spans.len(), 1);
        assert!(last.spans[0].content.contains("diff truncated"));
    }
}
