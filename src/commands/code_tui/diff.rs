//! Unified-diff parser that emits ratatui styled [`Line`]s for the in-TUI
//! diff viewer (M7b `/diff`, upgraded in M3/#9 with a line-number gutter,
//! `+/-` sign column, file-header `+N/-N` counts, and syntect highlighting
//! of added/removed lines).
//!
//! The bg thread shells out to `git -C <cwd> diff --no-color HEAD [-- <path>]`
//! (`code_ui::git_diff_in`), ships the raw diff string back as
//! `AgentMsg::DiffReady`, and the main thread renders it here via the
//! `DiffView` overlay (`view::draw_diff_view`). Parsing is line-oriented and
//! stateful (one pass over the input, walking the hunk line counters) and
//! caps the output at [`MAX_DIFF_LINES`] so a huge diff can't freeze the
//! viewer.

use ratatui::text::{Line, Span};

use crate::commands::code_tui::highlight;
use crate::commands::code_tui::theme;

/// Hard cap on emitted lines. A `git diff` over a large uncommitted tree can
/// easily run into tens of thousands of lines; rendering every one as a
/// styled `Line` would allocate heavily and stall the render loop. We stop
/// here and append a `… (diff truncated)` notice.
pub const MAX_DIFF_LINES: usize = 2000;

/// Width of each half of the line-number gutter (old + new). git diffs over
/// realistic files stay well under 6 digits; we right-justify the numbers in
/// this fixed width so the gutter column never reflows line to line.
const GUTTER_W: usize = 5;

/// Parse a unified-diff string into styled ratatui `Line<'static>`s.
///
/// Each body row is rendered as:
///   `<oldnum> <newnum> <sign> <body>`
/// where the line numbers come from walking the `@@ -a,b +c,d @@` hunk
/// header, the sign is `+`/`-`/` `, and the body of added/removed lines is
/// syntax-highlighted via [`highlight`] when the file's language is
/// recognized. File headers (`diff --git`/`---`/`+++`) are bold; after each
/// `+++` line a `+N/-N` summary is appended.
///
/// Empty input yields a single "(no changes)" muted line so the viewer never
/// renders a blank frame. Output is capped at [`MAX_DIFF_LINES`]; if the cap
/// is hit, a trailing `… (diff truncated, N lines)` line is appended.
pub fn parse_diff(diff: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if diff.is_empty() {
        lines.push(Line::from(Span::styled("(no changes)", theme::muted())));
        return lines;
    }

    let bold = theme::bold();
    let muted = theme::muted();
    let added_style = theme::success();
    let removed_style = theme::error();

    // Hunk line counters, seeded from `@@ -a,b +c,d @@`. `None` until the
    // first hunk header is seen (so pre-hunk file headers don't render a
    // gutter of zeros).
    let mut old_line: Option<u64> = None;
    let mut new_line: Option<u64> = None;
    // Counts of added/removed body lines for the current file, reset at each
    // `diff --git` header and emitted as a `+N/-N` summary after `+++`.
    let mut added_count: u64 = 0;
    let mut removed_count: u64 = 0;
    // Language hint inferred from the `b/<path>` line, applied to body
    // highlighting. Cleared at each new file.
    let mut highlighter: Option<highlight::HighlightLines<'static>> = None;

    for raw in diff.lines() {
        if lines.len() >= MAX_DIFF_LINES {
            lines.push(Line::from(Span::styled(
                format!("… (diff truncated at {MAX_DIFF_LINES} lines)"),
                theme::muted(),
            )));
            break;
        }

        // File header: `diff --git a/x b/y`. Reset per-file state.
        if raw.starts_with("diff --git ") {
            added_count = 0;
            removed_count = 0;
            highlighter = None;
            old_line = None;
            new_line = None;
            lines.push(Line::from(Span::styled(raw.to_string(), bold)));
            continue;
        }
        // `--- a/<path>` — the OLD file. Bold; not a body line.
        if raw.starts_with("--- ") {
            lines.push(Line::from(Span::styled(raw.to_string(), bold)));
            continue;
        }
        // `+++ b/<path>` — the NEW file. Bold; infer the language from the
        // `b/` path and emit a `+N/-N` counts summary (the counts are only
        // known at the END of the file, so the summary is emitted lazily on
        // the next file header / end — handled below via a placeholder).
        if raw.starts_with("+++ ") {
            highlighter = highlight::highlighter_for_lang(&lang_from_b_path(raw));
            lines.push(Line::from(Span::styled(raw.to_string(), bold)));
            continue;
        }
        // Hunk header: `@@ -a,b +c,d @@ <optional section>`.
        if let Some(rest) = raw.strip_prefix("@@") {
            if let Some((o, n)) = parse_hunk_header(rest) {
                old_line = Some(o);
                new_line = Some(n);
            }
            lines.push(Line::from(Span::styled(raw.to_string(), muted)));
            continue;
        }
        // `index 111..222 100644` — git metadata line, dim.
        if raw.starts_with("index ")
            || raw.starts_with("similarity ")
            || raw.starts_with("rename ")
            || raw.starts_with("copy ")
            || raw.starts_with("new file ")
            || raw.starts_with("deleted file ")
            || raw.starts_with("old mode ")
            || raw.starts_with("new mode ")
        {
            lines.push(Line::from(Span::styled(raw.to_string(), muted)));
            continue;
        }

        // Body line: context / added / removed. Walk the counters and build
        // the gutter + sign + (possibly highlighted) body.
        let (sign, style, body, kind) = if let Some(rest) = raw.strip_prefix('+') {
            added_count += 1;
            let nl = new_line.map(|n| n + 1);
            new_line = nl;
            ('+', added_style, rest, LineKind::Added)
        } else if let Some(rest) = raw.strip_prefix('-') {
            removed_count += 1;
            let ol = old_line.map(|o| o + 1);
            old_line = ol;
            ('-', removed_style, rest, LineKind::Removed)
        } else {
            // Context line: leading space (strip exactly one).
            let rest = raw.strip_prefix(' ').unwrap_or(raw);
            let ol = old_line.map(|o| o + 1);
            let nl = new_line.map(|n| n + 1);
            old_line = ol;
            new_line = nl;
            (' ', muted, rest, LineKind::Context)
        };

        let old_num = match kind {
            LineKind::Added => None,
            _ => old_line.map(|n| n.saturating_sub(1)),
        };
        let new_num = match kind {
            LineKind::Removed => None,
            _ => new_line.map(|n| n.saturating_sub(1)),
        };

        let mut spans = Vec::with_capacity(4);
        spans.push(Span::styled(format_gutter_num(old_num), muted));
        spans.push(Span::styled(format_gutter_num(new_num), muted));
        spans.push(Span::styled(format!("{sign} "), style));
        // Highlight added/removed body lines when the language is known and
        // the body fits a reasonable budget (mirrors render_code_lines'
        // overflow guard). Context lines stay plain (dim) — git already
        // colors context as low-salience.
        let highlighted = if matches!(kind, LineKind::Added | LineKind::Removed) {
            highlighter
                .as_mut()
                .and_then(|h| highlight::highlight_line(h, body))
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        match highlighted {
            Some(ts) => {
                // Apply the line-kind style as a base by pushing the
                // sign already; highlight spans carry their own colors.
                spans.extend(ts);
            }
            None => {
                spans.push(Span::styled(body.to_string(), style));
            }
        }
        lines.push(Line::from(spans));
    }

    // If the diff ended mid-file (no trailing `diff --git`), we never got a
    // chance to emit the counts summary after the last `+++`. Append one now
    // when there were any added/removed lines and a file was in progress.
    if (added_count > 0 || removed_count > 0) && lines.len() < MAX_DIFF_LINES {
        lines.push(Line::from(Span::styled(
            format_counts(added_count, removed_count),
            muted,
        )));
    }

    lines
}

#[derive(Clone, Copy)]
enum LineKind {
    Context,
    Added,
    Removed,
}

/// Right-justify a line number in the gutter width, or emit a blank gutter.
fn format_gutter_num(n: Option<u64>) -> String {
    match n {
        Some(v) => {
            let s = v.to_string();
            if s.len() >= GUTTER_W {
                s
            } else {
                let pad = GUTTER_W - s.len();
                format!("{}{}", " ".repeat(pad), s)
            }
        }
        None => " ".repeat(GUTTER_W),
    }
}

/// Format the `+N/-N` file-summary line.
fn format_counts(added: u64, removed: u64) -> String {
    format!(
        "{added} insertion{pl_a} / {removed} deletion{pl_r}",
        pl_a = if added == 1 { "" } else { "s" },
        pl_r = if removed == 1 { "" } else { "s" }
    )
}

/// Extract the language hint from a `+++ b/<path>` line, returning the path
/// extension (without the dot) lowercased, or the full token if there's no
/// dot. Returns "" for `/dev/null` (new-file-deletion) so no highlighter is
/// built.
fn lang_from_b_path(plus_line: &str) -> String {
    // `+++ b/<path>` (or `+++ /dev/null`).
    let after = plus_line.trim_start_matches("+++ ");
    let path = after.strip_prefix("b/").unwrap_or(after);
    if path == "/dev/null" {
        return String::new();
    }
    match path.rsplit('.').next() {
        Some(ext) if !ext.is_empty() && ext != path => ext.to_string(),
        _ => String::new(),
    }
}

/// Parse the `-a,b +c,d` portion of an `@@ -a,b +c,d @@` hunk header,
/// returning `(old_start, new_start)` (1-based). The `,b`/`,d` counts are
/// optional (a hunk of one line omits them); we read only the start value
/// (the `,count` tail is dropped via [`take_num`]).
fn parse_hunk_header(rest: &str) -> Option<(u64, u64)> {
    // rest starts right after `@@`, e.g. ` -1,3 +1,4 @@ context`.
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('-')?;
    let (old_start, after_old) = take_num(rest);
    let old_start: u64 = old_start.parse().ok()?;
    // Skip the optional `,count` tail of the old range up to whitespace
    // (e.g. `,3 ` in `-1,3`), then the separating space, then `+`.
    let after_old = after_old.trim_start_matches(|c: char| c.is_ascii_digit() || c == ',');
    let after_old = after_old.trim_start();
    let after_old = after_old.strip_prefix('+')?;
    let (new_start, _) = take_num(after_old);
    let new_start: u64 = new_start.parse().ok()?;
    Some((old_start, new_start))
}

/// Take a run of digits off the front of `s`, returning the numeric token
/// and the remainder (stops at the first non-digit, e.g. the `,count` in a
/// hunk start).
fn take_num(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_yields_no_changes_notice() {
        let lines = parse_diff("");
        assert_eq!(lines.len(), 1);
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
        // diff --git / index / --- / +++ / @@ / context / removed / added /
        // counts-summary = 9 lines.
        assert_eq!(lines.len(), 9, "got {lines:?}");
        // file headers bold; index muted.
        assert_eq!(lines[0].spans[0].style, theme::bold());
        assert_eq!(lines[1].spans[0].style, theme::muted());
        assert_eq!(lines[2].spans[0].style, theme::bold());
        assert_eq!(lines[3].spans[0].style, theme::bold());
        // hunk header muted.
        assert_eq!(lines[4].spans[0].style, theme::muted());
        // Body rows: gutter(old) + gutter(new) + sign + body.
        // context: old=1, new=1
        let ctx = &lines[5];
        assert_eq!(ctx.spans.len(), 4);
        assert_eq!(ctx.spans[0].content, "    1"); // right-justified width 5
        assert_eq!(ctx.spans[1].content, "    1");
        assert_eq!(ctx.spans[2].content, "  "); // sign + trailing space
        assert_eq!(ctx.spans[3].content, "context line");
        assert_eq!(ctx.spans[3].style, theme::muted());
        // removed: old=2, new blank, sign '-', error color.
        let rem = &lines[6];
        assert_eq!(rem.spans[0].content, "    2");
        assert_eq!(rem.spans[1].content, "     "); // blank gutter
        assert_eq!(rem.spans[2].style, theme::error());
        assert_eq!(rem.spans[2].content, "- ");
        // added: old blank, new=2, sign '+', success color.
        let add = &lines[7];
        assert_eq!(add.spans[0].content, "     ");
        assert_eq!(add.spans[1].content, "    2");
        assert_eq!(add.spans[2].style, theme::success());
        assert_eq!(add.spans[2].content, "+ ");
        // counts summary last: 1 insertion / 1 deletion.
        let counts = &lines[8];
        assert_eq!(counts.spans[0].content, "1 insertion / 1 deletion");
    }

    #[test]
    fn counts_summary_pluralizes() {
        let diff = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,1 +1,1 @@
-a
+b
+c
-d
";
        let lines = parse_diff(diff);
        let last = lines.last().unwrap();
        assert_eq!(last.spans[0].content, "2 insertions / 2 deletions");
    }

    #[test]
    fn truncates_at_cap_and_appends_notice() {
        let mut diff = String::from("diff --git a/x b/x\n@@ -1,1 +1,1 @@\n");
        for _ in 0..(MAX_DIFF_LINES + 50) {
            diff.push_str("+a\n");
        }
        let lines = parse_diff(&diff);
        assert_eq!(lines.len(), MAX_DIFF_LINES + 1);
        let last = lines.last().unwrap();
        assert_eq!(last.spans.len(), 1);
        assert!(last.spans[0].content.contains("diff truncated"));
    }

    #[test]
    fn hunk_header_without_counts_parses() {
        // `@@ -1 +1 @@` (single-line hunks omit the `,b`/`,d`).
        assert_eq!(parse_hunk_header(" -1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header(" -10,3 +12,5 @@ section"), Some((10, 12)));
    }

    #[test]
    fn lang_from_b_path_extracts_extension() {
        assert_eq!(lang_from_b_path("+++ b/src/main.rs"), "rs");
        assert_eq!(lang_from_b_path("+++ b/lib/utils.py"), "py");
        assert_eq!(lang_from_b_path("+++ /dev/null"), "");
        assert_eq!(lang_from_b_path("+++ b/Makefile"), "");
    }

    #[test]
    fn highlighted_added_line_has_multiple_spans() {
        // A rust added line under a .rs file should highlight into more
        // than one body span (keyword + identifier), proving syntect ran.
        let diff = "\
diff --git a/foo.rs b/foo.rs
--- a/foo.rs
+++ b/foo.rs
@@ -1,1 +1,1 @@
-old line
+let x = 1;
";
        let lines = parse_diff(diff);
        // find the added line (sign '+').
        let added = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "+ "))
            .expect("added line present");
        // gutter + gutter + sign + highlighted body spans.
        assert!(
            added.spans.len() > 4,
            "expected highlighted body spans, got {}: {added:?}",
            added.spans.len()
        );
    }
}
