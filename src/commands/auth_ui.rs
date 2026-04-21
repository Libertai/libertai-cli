//! Small interactive helpers shared between the login and keys flows.

use anyhow::{bail, Context, Result};
use dialoguer::console::Term;
use dialoguer::Confirm;
use owo_colors::OwoColorize;

pub fn validate_limit(v: f64) -> Result<()> {
    if !v.is_finite() || v < 0.0 {
        bail!("monthly limit must be a finite non-negative number (got {v})");
    }
    Ok(())
}

pub fn confirm_signing(term: &Term, account_base: &str, message: &str) -> Result<()> {
    let host = url::Url::parse(account_base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| account_base.to_string());
    eprintln!();
    eprintln!(
        "{}",
        "The server is asking you to sign this message:".yellow().bold()
    );
    eprintln!("  host:    {host}");
    eprintln!("  message: {message}");
    eprintln!();
    let ok = Confirm::new()
        .with_prompt("Sign this message with your private key?")
        .default(false)
        .interact_on(term)
        .context("reading signing confirmation")?;
    if !ok {
        bail!("signing cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_limit;

    #[test]
    fn accepts_zero_and_positive() {
        assert!(validate_limit(0.0).is_ok());
        assert!(validate_limit(5.0).is_ok());
        assert!(validate_limit(1_000_000.0).is_ok());
    }

    #[test]
    fn rejects_negative() {
        assert!(validate_limit(-0.01).is_err());
        assert!(validate_limit(-1.0).is_err());
    }

    #[test]
    fn rejects_non_finite() {
        assert!(validate_limit(f64::NAN).is_err());
        assert!(validate_limit(f64::INFINITY).is_err());
        assert!(validate_limit(f64::NEG_INFINITY).is_err());
    }
}
