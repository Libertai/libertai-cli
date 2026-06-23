//! Shared terminal RAII guard for the ratatui TUI.
//!
//! Both the main `code` TUI (`app.rs`) and the standalone `agents` TUI
//! (`agent_view.rs`) set up the terminal the same way — `enable_raw_mode`,
//! `EnterAlternateScreen`, build a `Terminal` — and need to undo all of that
//! on every exit path, including panics. [`TerminalGuard`] owns that teardown.
//!
//! The two callers differ in exactly one respect: `app.rs` enables mouse
//! capture during `run_loop` (so its guard must emit `DisableMouseCapture` on
//! drop), while `agent_view.rs` does not. That difference is captured by the
//! `restore_mouse` flag passed to [`TerminalGuard::new`].

use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// RAII guard that restores the terminal on drop — covers early-return
/// and panic paths between `enable_raw_mode` and the end of the TUI loop.
///
/// Tracks which terminal modifications have been applied so far so
/// that if `enable_raw_mode` succeeds but `Terminal::new` fails, we
/// still undo raw mode and the alternate screen.
///
/// The `raw_mode`, `alt_screen`, and `terminal` fields are set by the
/// caller as each setup step succeeds (mirroring the original per-file
/// structs), so they are `pub(crate)` for direct field mutation. Use
/// [`TerminalGuard::new`] to construct the guard and record whether mouse
/// capture was enabled.
pub(crate) struct TerminalGuard {
    pub(crate) raw_mode: bool,
    pub(crate) alt_screen: bool,
    restore_mouse: bool,
    pub(crate) terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
}

impl TerminalGuard {
    /// Construct a guard with no terminal modifications applied yet.
    ///
    /// `restore_mouse`: emit `DisableMouseCapture` on drop iff the caller
    /// enabled mouse capture (`app.rs` yes; `agent_view.rs` no).
    pub(crate) fn new(restore_mouse: bool) -> Self {
        TerminalGuard {
            raw_mode: false,
            alt_screen: false,
            restore_mouse,
            terminal: None,
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = terminal.show_cursor();
            if self.restore_mouse {
                let _ = crossterm::execute!(
                    terminal.backend_mut(),
                    LeaveAlternateScreen,
                    crossterm::event::DisableMouseCapture
                );
            } else {
                let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
            }
        } else if self.alt_screen {
            if self.restore_mouse {
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    LeaveAlternateScreen,
                    crossterm::event::DisableMouseCapture
                );
            } else {
                let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
            }
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}
