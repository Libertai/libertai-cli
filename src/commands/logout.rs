use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{config_path, set_file_mode_600};

pub fn run() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        eprintln!("already logged out");
        return Ok(());
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("reading system clock")?
        .as_secs();
    let backup = {
        let mut s = path.clone().into_os_string();
        s.push(format!(".bak.{ts}"));
        std::path::PathBuf::from(s)
    };

    std::fs::rename(&path, &backup).with_context(|| {
        format!("renaming {} → {}", path.display(), backup.display())
    })?;
    // Backup can inherit permissive perms if the original file was ever touched
    // by hand; re-chmod to 0600 so the backed-up key material can't leak.
    set_file_mode_600(&backup)
        .with_context(|| format!("chmod 0600 {}", backup.display()))?;

    eprintln!(
        "Logged out. Previous config moved to {}",
        backup.display()
    );
    Ok(())
}
