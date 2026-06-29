//! `libertai usage` — plan tier, rolling allowance windows, and prepaid
//! credits. Authenticates by refreshing the stored session token (no browser
//! popup on the happy path); falls back to a one-time browser sign-in.

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use std::io::IsTerminal;

use crate::client::{
    get_subscription, refresh_session, ClassifiedError, ErrorClass, Subscription, TokenPair,
};
use crate::commands::login::{browser_sso_access_token, open_url};
use crate::commands::output::Styler;
use crate::config::{self, Config};

pub fn run(json: bool) -> Result<()> {
    let mut cfg = config::load()?;
    let access = acquire_access_token(&mut cfg)?;
    let sub = get_subscription(&cfg, &access)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&sub)?);
        return Ok(());
    }
    print_human(&sub, Utc::now());
    Ok(())
}

/// Refresh-first auth: rotate the stored refresh token into a fresh access
/// token (persisting the rotation), else fall back to a browser sign-in that
/// captures a new refresh token for next time. Persists `cfg` on success.
fn acquire_access_token(cfg: &mut Config) -> Result<String> {
    if let Some(rtok) = cfg.auth.refresh_token.clone() {
        match refresh_session(cfg, &rtok) {
            Ok(pair) => {
                cfg.auth.refresh_token = Some(pair.refresh_token);
                config::save(cfg)?;
                return Ok(pair.access_token);
            }
            Err(_) => {
                // Expired/revoked/rotated-away — drop it and sign in fresh.
                cfg.auth.refresh_token = None;
                config::save(cfg)?;
            }
        }
    }

    // Fix 2: fail fast in non-interactive/CI contexts instead of hanging up to 5min.
    if !std::io::stderr().is_terminal() {
        return Err(ClassifiedError::classified(
            ErrorClass::Auth,
            "not logged in (no active session) — run `libertai login` to sign in",
        ));
    }

    let st = Styler::stderr();
    eprintln!(
        "{} Checking usage needs a quick sign-in confirmation in your browser.",
        st.yellow("!")
    );
    let pair = browser_sso_access_token(cfg, "LibertAI CLI (usage)", |url| {
        eprintln!("Opening your browser to sign in…");
        eprintln!("If it doesn't open, visit:\n  {url}");
        let _ = open_url(url);
    })?;
    // Fix 5: destructure instead of clone.
    let TokenPair {
        access_token,
        refresh_token,
    } = pair;
    cfg.auth.refresh_token = Some(refresh_token);
    config::save(cfg)?;
    Ok(access_token)
}

fn percent(used: Option<f64>, limit: Option<f64>) -> u32 {
    match (used, limit) {
        (Some(u), Some(l)) if l > 0.0 => ((u / l) * 100.0).round().clamp(0.0, 100.0) as u32,
        _ => 0,
    }
}

/// 16-cell bar colored by usage: >=90% red, >=75% amber(yellow), else green.
fn bar(pct: u32, st: &Styler) -> String {
    const WIDTH: u32 = 16;
    let filled = (pct * WIDTH / 100).min(WIDTH);
    let s = format!(
        "{}{}",
        "█".repeat(filled as usize),
        "░".repeat((WIDTH - filled) as usize)
    );
    if pct >= 90 {
        st.red(&s)
    } else if pct >= 75 {
        st.yellow(&s)
    } else {
        st.green(&s)
    }
}

/// Format an integer with comma grouping, e.g. 1040 → "1,040", 999 → "999".
fn group_thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let grouped: String = digits
        .chars()
        .rev()
        .enumerate()
        .flat_map(|(i, c)| {
            if i > 0 && i % 3 == 0 {
                Some(',').into_iter().chain(Some(c))
            } else {
                None.into_iter().chain(Some(c))
            }
        })
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if n < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn amount(v: Option<f64>) -> String {
    // Allowance units are whole-ish; show no decimals, with thousands separators.
    group_thousands(v.unwrap_or(0.0).round() as i64)
}

fn money(v: Option<f64>) -> String {
    format!("${:.2}", v.unwrap_or(0.0))
}

/// Relative reset for the short window, e.g. "Resets in 1h 2m" / "Resets in 5m".
fn reset_in_label(resets_at: Option<&str>, now: DateTime<Utc>) -> String {
    let Some(ts) = resets_at else {
        return String::new();
    };
    let Ok(reset) = DateTime::parse_from_rfc3339(ts) else {
        return String::new();
    };
    let diff = reset.with_timezone(&Utc) - now;
    if diff.num_seconds() <= 0 {
        return "Resets now".to_string();
    }
    let mins = diff.num_minutes();
    let (h, m) = (mins / 60, mins % 60);
    if h >= 1 {
        format!("Resets in {h}h {m}m")
    } else {
        // Sub-hour: show at least 1m so "45s away" reads "Resets in 1m" not "0m".
        let m = mins.max(1);
        format!("Resets in {m}m")
    }
}

/// Absolute reset for the weekly window, e.g. "Resets Sun 4:59 PM" (local).
fn reset_at_label(resets_at: Option<&str>) -> String {
    let Some(ts) = resets_at else {
        return String::new();
    };
    let Ok(reset) = DateTime::parse_from_rfc3339(ts) else {
        return String::new();
    };
    format!(
        "Resets {}",
        reset.with_timezone(&Local).format("%a %-I:%M %p")
    )
}

fn print_human(sub: &Subscription, now: DateTime<Utc>) {
    let st = Styler::stdout();
    println!("{}", st.heading("LibertAI usage"));
    println!("  {:<16} {}", st.dimmed("Plan:"), sub.tier);

    let row = |label: &str, used: Option<f64>, limit: Option<f64>, sublabel: String| {
        let pct = percent(used, limit);
        println!(
            "  {:<16} {}  {:>4}  ({} / {})  {}",
            label,
            bar(pct, &st),
            format!("{pct}%"),
            amount(used),
            amount(limit),
            st.dimmed(&sublabel),
        );
    };
    row(
        "Current session",
        sub.window_5h_used,
        sub.window_5h_limit,
        reset_in_label(sub.window_5h_resets_at.as_deref(), now),
    );
    row(
        "Weekly limit",
        sub.weekly_used,
        sub.weekly_limit,
        reset_at_label(sub.weekly_resets_at.as_deref()),
    );
    println!(
        "  {:<16} {}",
        st.dimmed("Usage credits:"),
        money(sub.prepaid_balance)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn percent_clamps_and_handles_zero_limit() {
        assert_eq!(percent(Some(50.0), Some(200.0)), 25);
        assert_eq!(percent(Some(300.0), Some(200.0)), 100); // clamp
        assert_eq!(percent(Some(10.0), Some(0.0)), 0); // no limit
        assert_eq!(percent(None, Some(200.0)), 0);
    }

    #[test]
    fn reset_in_label_is_relative() {
        let now = Utc.with_ymd_and_hms(2026, 6, 29, 12, 0, 0).unwrap();
        let in_1h2m = "2026-06-29T13:02:00Z";
        assert_eq!(reset_in_label(Some(in_1h2m), now), "Resets in 1h 2m");
        let in_5m = "2026-06-29T12:05:00Z";
        assert_eq!(reset_in_label(Some(in_5m), now), "Resets in 5m");
        let past = "2026-06-29T11:00:00Z";
        assert_eq!(reset_in_label(Some(past), now), "Resets now");
        assert_eq!(reset_in_label(None, now), "");
    }

    #[test]
    fn reset_in_label_sub_minute_shows_1m() {
        // 45 seconds ahead: num_minutes() floors to 0, but we clamp to 1.
        let now = Utc.with_ymd_and_hms(2026, 6, 29, 12, 0, 0).unwrap();
        let in_45s = "2026-06-29T12:00:45Z";
        assert_eq!(reset_in_label(Some(in_45s), now), "Resets in 1m");
        // Exactly now (0 seconds) → "Resets now".
        let exact = "2026-06-29T12:00:00Z";
        assert_eq!(reset_in_label(Some(exact), now), "Resets now");
    }

    #[test]
    fn amount_formats_with_commas() {
        assert_eq!(amount(Some(1040.0)), "1,040");
        assert_eq!(amount(Some(2000.0)), "2,000");
        assert_eq!(amount(Some(999.0)), "999");
        assert_eq!(amount(Some(0.0)), "0");
        assert_eq!(amount(None), "0");
        assert_eq!(amount(Some(1_000_000.0)), "1,000,000");
    }

    #[test]
    fn money_label_formats_two_decimals() {
        assert_eq!(money(Some(4.2)), "$4.20");
        assert_eq!(money(None), "$0.00");
    }

    #[test]
    fn json_output_preserves_field_names() {
        let sub = crate::client::Subscription {
            tier: "go".into(),
            has_subscription: true,
            status: Some("active".into()),
            window_5h_used: Some(1.0),
            window_5h_limit: Some(2.0),
            window_5h_resets_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_resets_at: None,
            prepaid_balance: Some(4.2),
        };
        let v = serde_json::to_value(&sub).unwrap();
        assert_eq!(v["tier"], "go");
        assert_eq!(v["window_5h_used"], 1.0);
        assert_eq!(v["prepaid_balance"], 4.2);
    }
}
