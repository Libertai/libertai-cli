//! Terminal primitives shared by the `libertai code` UI modules.
//!
//! Lives here so both the REPL's input bar (`code_ui.rs`) and the
//! approval micro-prompt (`code_approvals.rs`) use the same RAII
//! guard — otherwise a panic during an approval prompt would leak raw
//! mode and leave the user's terminal broken.

use anyhow::Result;
use crossterm::terminal;

/// RAII guard that enables raw mode on construction and disables it
/// on drop (including the panic-unwind path).
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode().map_err(|e| anyhow::anyhow!("enable_raw_mode: {e}"))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}
