//! Shared output discipline for the non-conversational commands.
//!
//! One styling gate for every piece of user-facing styled output: ANSI
//! accents are emitted only when the destination stream is a real
//! terminal, `NO_COLOR` is unset (per <https://no-color.org>), and the
//! terminal is not `TERM=dumb`. stdout styling gates on stdout's
//! TTY-ness and stderr styling on stderr's, so piping one stream never
//! restyles the other.
//!
//! The underlying gate is [`chat_render::styling_enabled`] — re-exported
//! here so `chat`/`ask` and the simpler commands share one definition.
//!
//! [`chat_render::styling_enabled`]: crate::commands::chat_render::styling_enabled

use std::io::IsTerminal;

use owo_colors::OwoColorize;

pub use crate::commands::chat_render::styling_enabled;

/// Whether ANSI accents should be emitted on stdout.
pub fn stdout_styled() -> bool {
    styling_enabled(std::io::stdout().is_terminal())
}

/// Whether ANSI accents should be emitted on stderr.
pub fn stderr_styled() -> bool {
    styling_enabled(std::io::stderr().is_terminal())
}

/// `("\x1b[2m", "\x1b[0m")` when stderr styling is enabled, else two
/// empty strings — for call sites that embed a dim span inside a larger
/// format string (e.g. the `libertai code` stream renderer).
pub fn stderr_dim_pair() -> (&'static str, &'static str) {
    if stderr_styled() {
        ("\x1b[2m", "\x1b[0m")
    } else {
        ("", "")
    }
}

/// Minimal conditional styler: each method returns either the styled
/// string or the plain text, so call sites keep single-expression
/// formatting without a per-line `if styled { … } else { … }` branch.
#[derive(Clone, Copy)]
pub struct Styler {
    enabled: bool,
}

impl Styler {
    /// Styler for output written to stdout.
    pub fn stdout() -> Self {
        Self {
            enabled: stdout_styled(),
        }
    }

    /// Styler for output written to stderr.
    pub fn stderr() -> Self {
        Self {
            enabled: stderr_styled(),
        }
    }

    fn apply(self, s: &str, f: impl FnOnce(&str) -> String) -> String {
        if self.enabled {
            f(s)
        } else {
            s.to_string()
        }
    }

    pub fn bold(self, s: &str) -> String {
        self.apply(s, |t| t.bold().to_string())
    }

    pub fn dimmed(self, s: &str) -> String {
        self.apply(s, |t| t.dimmed().to_string())
    }

    pub fn green(self, s: &str) -> String {
        self.apply(s, |t| t.green().to_string())
    }

    pub fn red(self, s: &str) -> String {
        self.apply(s, |t| t.red().to_string())
    }

    pub fn yellow(self, s: &str) -> String {
        self.apply(s, |t| t.yellow().to_string())
    }

    pub fn yellow_bold(self, s: &str) -> String {
        self.apply(s, |t| t.yellow().bold().to_string())
    }

    pub fn cyan(self, s: &str) -> String {
        self.apply(s, |t| t.cyan().to_string())
    }

    /// Section heading: bold + underline.
    pub fn heading(self, s: &str) -> String {
        self.apply(s, |t| t.bold().underline().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::Styler;

    #[test]
    fn disabled_styler_passes_text_through_unchanged() {
        let st = Styler { enabled: false };
        assert_eq!(st.bold("ID"), "ID");
        assert_eq!(st.dimmed("x"), "x");
        assert_eq!(st.heading("Head"), "Head");
    }

    #[test]
    fn enabled_styler_emits_escape_bytes() {
        let st = Styler { enabled: true };
        assert!(st.bold("ID").contains('\x1b'));
        assert!(st.heading("Head").contains('\x1b'));
    }
}
