//! `libertai completions <shell>` and the hidden `libertai man` — both
//! render the `Cli` derive from `crate::cli` to stdout so users can pipe
//! them wherever their shell/manpath wants, and so packaging can capture
//! them at build time (deb assets, brew's
//! `generate_completions_from_executable`, packaging/generate-assets.sh).
//!
//! Output goes to stdout only; the update-check banner is stderr-only and
//! skips non-tty stdout, so piped output is always a clean script.

use std::io::Write;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;

/// Print the completion script for `shell` to stdout.
pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = crate::cli::Cli::command();
    // `generate` writes straight to the sink; the binary name must match
    // what's on $PATH for the script's function names to line up.
    clap_complete::generate(shell, &mut cmd, "libertai", &mut std::io::stdout());
    Ok(())
}

/// Print the top-level man page (roff) to stdout.
pub fn man() -> Result<()> {
    let cmd = crate::cli::Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    clap_mangen::Man::new(cmd)
        .render(&mut buf)
        .context("rendering man page")?;
    std::io::stdout()
        .write_all(&buf)
        .context("writing man page to stdout")?;
    Ok(())
}
