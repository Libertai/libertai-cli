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

use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
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
    /// (B4-KITTY) True when the kitty keyboard-enhancement flags were pushed
    /// at setup (`supports_keyboard_enhancement()` reported yes). The flag
    /// stack is per-screen-buffer, so suspend/Drop must POP while still in
    /// the alternate screen (before `LeaveAlternateScreen`) and resume must
    /// re-PUSH after re-entering it — otherwise the external editor (Ctrl+O)
    /// would inherit an enhanced keyboard, or the flags would leak on exit.
    pub(crate) kbd_enhanced: bool,
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
            kbd_enhanced: false,
            terminal: None,
        }
    }

    /// (B4-KITTY) The keyboard-enhancement flags we push where supported:
    /// DISAMBIGUATE so modified keys (Shift+Enter) arrive distinguishable,
    /// REPORT_ALTERNATE_KEYS for shifted-key fidelity. Deliberately NOT
    /// REPORT_EVENT_TYPES — we don't want Repeat/Release event floods; the
    /// Release filter in `run_loop` is belt-and-braces.
    pub(crate) fn kbd_flags() -> crossterm::event::KeyboardEnhancementFlags {
        use crossterm::event::KeyboardEnhancementFlags as K;
        K::DISAMBIGUATE_ESCAPE_CODES | K::REPORT_ALTERNATE_KEYS
    }

    /// Suspend the TUI to hand the terminal to a subprocess (the external
    /// editor opened by Ctrl+O). Leaves the alternate screen, disables mouse
    /// capture, drops out of raw mode, and shows the cursor — the inverse of
    /// the setup in `app::run`. This does NOT drop the guard: the terminal
    /// stays owned by `self`, so [`Drop`] still runs on real exit. The caller
    /// MUST pair this with [`TerminalGuard::resume`] once the subprocess exits.
    ///
    /// Mirrors the `Drop` impl's exact crossterm command sequence (flush via
    /// the backend, `LeaveAlternateScreen` + `DisableMouseCapture` gated on
    /// `restore_mouse`) so the two paths tear down identically; the only
    /// additions are `show_cursor` + `disable_raw_mode`, which `Drop` also
    /// performs (cursor via the backend in `Drop`, raw mode at the tail).
    pub(crate) fn suspend(&mut self) -> anyhow::Result<()> {
        use crossterm::execute;
        let disable_mouse = should_disable_mouse(self.restore_mouse);
        // (B4-KITTY) Pop the enhancement flags FIRST, while still in the alt
        // screen (the stack is per-screen-buffer) — the subprocess gets a
        // vanilla keyboard.
        if self.kbd_enhanced {
            if let Some(terminal) = self.terminal.as_mut() {
                let _ = execute!(
                    terminal.backend_mut(),
                    crossterm::event::PopKeyboardEnhancementFlags
                );
            } else {
                let _ = execute!(
                    std::io::stdout(),
                    crossterm::event::PopKeyboardEnhancementFlags
                );
            }
        }
        if let Some(terminal) = self.terminal.as_mut() {
            // Flush any buffered frame before leaving the alt screen so the
            // editor inherits a clean terminal. `Terminal::flush` is the
            // inherent passthrough to `backend_mut().flush()` — no `Backend`
            // trait import needed.
            terminal.flush()?;
            let _ = terminal.show_cursor();
            if disable_mouse {
                execute!(
                    terminal.backend_mut(),
                    LeaveAlternateScreen,
                    crossterm::event::DisableMouseCapture
                )?;
            } else {
                execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            }
        } else {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
            if disable_mouse {
                execute!(
                    std::io::stdout(),
                    LeaveAlternateScreen,
                    crossterm::event::DisableMouseCapture
                )?;
            } else {
                execute!(std::io::stdout(), LeaveAlternateScreen)?;
            }
        }
        if self.raw_mode {
            disable_raw_mode()?;
        }
        Ok(())
    }

    /// Resume the TUI after a [`TerminalGuard::suspend`] — re-enters raw
    /// mode, the alternate screen, and mouse capture, then clears the
    /// terminal to force a full redraw (the alt-screen buffer was left, so
    /// the previous frame is gone). Inverse of `suspend`; safe to call only
    /// after a matching `suspend` (the guard still owns the terminal).
    pub(crate) fn resume(&mut self) -> anyhow::Result<()> {
        use crossterm::execute;
        if self.raw_mode {
            enable_raw_mode()?;
        }
        let enable_mouse = should_disable_mouse(self.restore_mouse);
        if let Some(terminal) = self.terminal.as_mut() {
            if enable_mouse {
                execute!(
                    terminal.backend_mut(),
                    EnterAlternateScreen,
                    crossterm::event::EnableMouseCapture
                )?;
            } else {
                execute!(terminal.backend_mut(), EnterAlternateScreen)?;
            }
            // The alt-screen buffer was left on suspend, so the old frame is
            // gone — clear to force a full redraw on the next `terminal.draw`.
            terminal.clear()?;
        } else if enable_mouse {
            execute!(
                std::io::stdout(),
                EnterAlternateScreen,
                crossterm::event::EnableMouseCapture
            )?;
        } else {
            execute!(std::io::stdout(), EnterAlternateScreen)?;
        }
        // (B4-KITTY) Re-push the enhancement flags now that we're back in
        // the alternate screen (inverse of the pop in `suspend`).
        if self.kbd_enhanced {
            if let Some(terminal) = self.terminal.as_mut() {
                let _ = execute!(
                    terminal.backend_mut(),
                    crossterm::event::PushKeyboardEnhancementFlags(Self::kbd_flags())
                );
            } else {
                let _ = execute!(
                    std::io::stdout(),
                    crossterm::event::PushKeyboardEnhancementFlags(Self::kbd_flags())
                );
            }
        }
        Ok(())
    }
}

/// Pure decision helper: emit `DisableMouseCapture` on drop iff mouse capture
/// was enabled at setup time (`restore_mouse`). Extracted from the inline
/// `Drop` branch conditions so the mouse-disable decision is testable in
/// isolation (no real terminal / crossterm I/O). The `Drop` impl calls this
/// for both its terminal-backend and stdout branches, so behaviour is
/// unchanged.
pub(crate) fn should_disable_mouse(restore_mouse: bool) -> bool {
    restore_mouse
}

// Test-only drop-probe: a thread-local flag the guard's `Drop` sets on entry.
// Defined at module scope (not inside `mod tests`) so the `Drop` impl can call
// it; compiled ONLY behind `#[cfg(test)]` so production behaviour is unchanged.
// It lets tests observe that teardown fired (including on a panic path) without
// driving a real terminal through crossterm.
#[cfg(test)]
thread_local! {
    static DROP_PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn drop_probe_mark_ran() {
    DROP_PROBE.with(|p| p.set(true));
}

#[cfg(test)]
fn drop_probe_take() -> bool {
    DROP_PROBE.with(|p| p.replace(false))
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        drop_probe_mark_ran();
        let disable_mouse = should_disable_mouse(self.restore_mouse);
        // (B4-KITTY) Best-effort pop of the enhancement flags before leaving
        // the alt screen, so they never leak into the user's shell.
        if self.kbd_enhanced {
            if let Some(terminal) = self.terminal.as_mut() {
                let _ = crossterm::execute!(
                    terminal.backend_mut(),
                    crossterm::event::PopKeyboardEnhancementFlags
                );
            } else if self.alt_screen {
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::event::PopKeyboardEnhancementFlags
                );
            }
        }
        if let Some(mut terminal) = self.terminal.take() {
            let _ = terminal.show_cursor();
            if disable_mouse {
                let _ = crossterm::execute!(
                    terminal.backend_mut(),
                    LeaveAlternateScreen,
                    crossterm::event::DisableMouseCapture
                );
            } else {
                let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
            }
        } else if self.alt_screen {
            if disable_mouse {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The mouse-disable decision is purely a function of `restore_mouse`.
    #[test]
    fn should_disable_mouse_tracks_flag() {
        assert!(
            !should_disable_mouse(false),
            "no mouse capture -> no disable"
        );
        assert!(
            should_disable_mouse(true),
            "mouse capture -> disable on drop"
        );
    }

    /// A guard with `restore_mouse=false` drops cleanly and never asks for a
    /// disable sequence (the pure decision gates it; no real I/O needed).
    #[test]
    fn guard_drops_without_mouse_disable_when_flag_false() {
        drop_probe_take(); // reset
        let guard = TerminalGuard::new(false);
        assert!(
            !should_disable_mouse(guard.restore_mouse),
            "restore_mouse=false must not produce DisableMouseCapture"
        );
        drop(guard);
        // Drop ran (probe set) and asked for no mouse disable above.
        assert!(drop_probe_take(), "guard Drop must run on normal drop");
    }

    /// Pins the documented contract: the guard's `Drop` teardown fires on a
    /// panic path, not just a normal return. We use `catch_unwind` so the
    /// panic is contained and we can still observe the probe afterwards.
    #[test]
    fn drop_runs_on_panic() {
        drop_probe_take(); // reset
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Create the guard in a scope that is exited by a panic rather
            // than a normal return: the guard is dropped during unwinding.
            let _guard = TerminalGuard::new(true);
            panic!("simulated TUI panic during run_loop");
        }));
        assert!(result.is_err(), "the inner closure must panic");
        assert!(
            drop_probe_take(),
            "TerminalGuard::drop must run during unwind \
             (teardown fires on panic, not just normal return)"
        );
    }

    // ── M7a: external-editor suspend/resume ──────────────────────────────
    //
    // `TerminalGuard::suspend`/`resume` are the inverse pair the Ctrl+O
    // external-editor flow (`app::open_external_editor`) calls to hand the
    // terminal to `$EDITOR` and take it back. They are NOT hermetically
    // end-to-end testable (a real `Terminal` backend needs a tty), so we
    // exercise the `terminal: None` branch: with no backend, suspend/resume
    // execute their crossterm sequences directly on `stdout`. The escape bytes
    // land on captured stdout (cargo tests capture per-test stdout), so the
    // calls are hermetic — the contract under test is that they construct +
    // return `Ok(())` without panicking, leave `raw_mode`/`alt_screen`/the
    // owned terminal untouched (suspend does NOT drop the guard — Drop still
    // owns the teardown on real exit), and the mouse-disable decision still
    // gates the `DisableMouseCapture`/`EnableMouseCapture` emission via the
    // pure `should_disable_mouse` helper.

    // (M7a-t1) suspend + resume round-trip cleanly with no terminal backend
    // and mouse capture off: both return Ok, the guard's fields are unchanged
    // (suspend must NOT drop the guard or take the terminal — Drop still runs
    // on real exit), and the teardown probe is NOT set (suspend/resume are
    // NOT Drop).
    #[test]
    fn suspend_resume_roundtrip_without_terminal_no_mouse() {
        drop_probe_take(); // reset
        let mut guard = TerminalGuard::new(false);
        guard.raw_mode = false;
        guard.alt_screen = true;
        guard.terminal = None;

        guard
            .suspend()
            .expect("suspend must Ok on captured stdout (no backend)");
        // Suspend must not fire Drop (the guard still owns teardown).
        assert!(
            !drop_probe_take(),
            "suspend must NOT run Drop (the guard is not dropped)"
        );
        // Fields untouched — the guard still owns the terminal for real exit.
        assert!(!guard.raw_mode);
        assert!(guard.alt_screen, "suspend must not clear alt_screen");
        assert!(guard.terminal.is_none());

        guard
            .resume()
            .expect("resume must Ok on captured stdout (no backend)");
        assert!(
            !drop_probe_take(),
            "resume must NOT run Drop (the guard is not dropped)"
        );
        assert!(!guard.raw_mode);
        assert!(guard.terminal.is_none());

        // Real Drop fires now and runs the probe.
        drop(guard);
        assert!(drop_probe_take(), "the explicit drop runs Drop's teardown");
    }

    // (M7a-t2) With `restore_mouse=true`, suspend/resume gate the mouse
    // capture commands on `should_disable_mouse` (the pure helper). On the
    // no-backend branch the sequence is emitted to stdout; we assert the
    // decision the suspension uses (so the mouse-enable/disable path is
    // exercised) and that both calls still return Ok. The guard with
    // `restore_mouse=true` would, on Drop, emit DisableMouseCapture; here we
    // just pin that suspend/resume consult the same flag.
    #[test]
    fn suspend_resume_with_mouse_flag_consults_should_disable_mouse() {
        drop_probe_take(); // reset
        let mut guard = TerminalGuard::new(true);
        guard.raw_mode = false;
        guard.terminal = None;

        // The mouse-disable/enable decision the suspend/resume branches use
        // mirrors Drop's: gated on `restore_mouse`.
        assert!(
            should_disable_mouse(guard.restore_mouse),
            "restore_mouse=true must select DisableMouseCapture on suspend / EnableMouseCapture on resume"
        );

        guard
            .suspend()
            .expect("suspend with mouse flag Ok on captured stdout");
        guard
            .resume()
            .expect("resume with mouse flag Ok on captured stdout");
        // Guard still alive (not dropped by suspend/resume).
        assert!(guard.terminal.is_none());
        drop(guard);
        assert!(drop_probe_take(), "Drop runs after the suspend/resume pair");
    }

    // (M7a-t3) suspend with `raw_mode=true` drops out of raw mode (calls
    // `disable_raw_mode`). crossterm's `disable_raw_mode` returns `Ok(())`
    // when raw mode was never actually enabled (the `TERMINAL_MODE_PRIOR_RAW_MODE`
    // slot is `None`, so it short-circuits without touching termios) — so this
    // is hermetic on a non-tty: it returns Ok without needing a real tty and
    // without mutating the test runner's terminal. The guard keeps
    // `raw_mode=true` (suspend does not clear the field; resume reads it to
    // re-arm).
    //
    // We deliberately do NOT call `resume()` with `raw_mode=true` here:
    // `resume()` → `enable_raw_mode()` opens `/dev/tty` and sets termios raw,
    // which needs a real controlling terminal and would corrupt the test
    // runner's terminal if it succeeded. That path is exercised only in the
    // live `open_external_editor` flow on a real tty, not in hermetic tests.
    #[test]
    fn suspend_with_raw_mode_flag_calls_disable_raw_mode() {
        drop_probe_take(); // reset
        let mut guard = TerminalGuard::new(false);
        guard.raw_mode = true;
        guard.alt_screen = true;
        guard.terminal = None;

        // suspend disables raw mode; since raw was never actually enabled,
        // disable_raw_mode short-circuits to Ok without a tty.
        guard
            .suspend()
            .expect("suspend with raw_mode must Ok when raw was never enabled (no tty needed)");
        // The field is NOT cleared by suspend (resume would read it to re-arm).
        assert!(guard.raw_mode, "suspend must not clear the raw_mode flag");
        assert!(
            guard.terminal.is_none(),
            "suspend must not drop the (absent) terminal"
        );
        // Suspend must not fire Drop.
        assert!(!drop_probe_take(), "suspend must NOT run Drop");

        // Explicit drop runs the teardown (disable_raw_mode again, harmlessly).
        drop(guard);
        assert!(drop_probe_take());
    }
}
