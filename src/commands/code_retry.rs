//! TUI-side auto-retry for transient LLM errors.
//!
//! pi_agent_rust only auto-retries on its own RPC and print-mode paths
//! (`run_prompt_with_retry` in `rpc.rs` / `main.rs`). libertai-cli embeds
//! the in-process SDK (`AgentSessionHandle::prompt*` → `run_text`), which
//! otherwise fails fast on the first provider error. This module provides
//! the retry decision logic so every `libertai code` prompt site (TUI
//! bg thread, one-shot print, subagents) can opt in with a small loop:
//!
//! 1. Classify the error with `pi::error::is_retryable_error`
//!    (429/rate-limit/overloaded/5xx/connection resets; NOT context
//!    overflow, NOT auth failures).
//! 2. Sleep with exponential backoff, abortable mid-delay.
//! 3. The caller then strips the failed request's partial output
//!    (`revert_incomplete_response`) and resumes the turn
//!    (`continue_turn_with_abort`) — the retry re-issues only the failed
//!    provider request; no tool re-execution, no re-billing of prior
//!    work (the pi_agent_rust#125 pattern).
//!
//! Knobs mirror pi's `[retry]` config defaults so `libertai code` and a
//! stock pi CLI behave identically out of the box.

use std::time::Duration;

use pi::sdk::Error as PiError;

/// Max retry attempts after the initial failure (pi default).
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Backoff base in ms (pi default).
const BASE_DELAY_MS: u64 = 2_000;
/// Backoff cap in ms (pi default).
const MAX_DELAY_MS: u64 = 60_000;
/// Poll granularity for the abortable backoff sleep (pi uses 50ms).
const SLEEP_POLL_MS: u64 = 50;

/// Exponential backoff: `base * 2^(attempt-1)`, clamped to 60s.
/// `attempt` is 1-based (the retry number about to run).
pub fn retry_delay_ms(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1);
    BASE_DELAY_MS
        .saturating_mul(1u64 << shift.min(6))
        .min(MAX_DELAY_MS)
}

/// Whether a failed prompt should be retried: transient transport
/// errors and retryable HTTP statuses, but never context overflow
/// or auth failures. Thin wrapper over pi's classifier so all call
/// sites classify identically.
pub fn should_retry(err: &PiError) -> bool {
    pi::error::is_retryable_error(&err.to_string(), None, None)
}

/// Retry bookkeeping for one prompt turn.
///
/// The caller drives its own attempt loop (each site has its own
/// closures/event plumbing) and feeds each failure to
/// [`RetryLoop::next_retry`]. The state machine caps the attempt
/// count and computes the backoff delay.
#[derive(Debug)]
pub struct RetryLoop {
    max_retries: u32,
    attempt: u32,
}

impl RetryLoop {
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            attempt: 0,
        }
    }

    pub fn with_default_limit() -> Self {
        Self::new(DEFAULT_MAX_RETRIES)
    }

    /// The retry number the next attempt would be (1-based).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Given the error from the latest attempt, decide what to do:
    /// `Some(delay_ms)` → sleep this long, then run retry number
    /// `attempt + 1`; `None` → give up (non-retryable or exhausted).
    pub fn next_retry(&mut self, err: &PiError) -> Option<u64> {
        if self.attempt >= self.max_retries || !should_retry(err) {
            return None;
        }
        self.attempt += 1;
        Some(retry_delay_ms(self.attempt))
    }
}

/// Backoff sleep that can be aborted mid-delay.
///
/// Polls `is_aborted` every [`SLEEP_POLL_MS`] and bails out as soon
/// as it returns true. Implemented with `thread::sleep` rather than an
/// async timer: the TUI bg thread owns its runtime and one-shot runs
/// have nothing else to poll, and a 50ms-granularity blocking loop is
/// exactly what pi's RPC mode does inside its runtime.
pub fn abortable_sleep_ms(delay_ms: u64, is_aborted: impl Fn() -> bool) -> bool {
    let start = std::time::Instant::now();
    let total = Duration::from_millis(delay_ms);
    while start.elapsed() < total {
        if is_aborted() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(SLEEP_POLL_MS));
    }
    !is_aborted()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_is_exponential_with_cap() {
        assert_eq!(retry_delay_ms(1), 2_000);
        assert_eq!(retry_delay_ms(2), 4_000);
        assert_eq!(retry_delay_ms(3), 8_000);
        assert_eq!(retry_delay_ms(4), 16_000);
        assert_eq!(retry_delay_ms(5), 32_000);
        assert_eq!(retry_delay_ms(6), 60_000);
        assert_eq!(retry_delay_ms(50), 60_000);
    }

    #[test]
    fn should_retry_classifies_transient_errors() {
        assert!(should_retry(&PiError::provider("x", "429 rate limit exceeded")));
        assert!(should_retry(&PiError::provider("x", "503 service unavailable")));
        assert!(should_retry(&PiError::provider(
            "x",
            "connection reset by peer"
        )));
        assert!(!should_retry(&PiError::provider(
            "x",
            "context overflow: 1M tokens"
        )));
        assert!(!should_retry(&PiError::provider("x", "invalid api key")));
    }

    #[test]
    fn loop_stops_after_max_retries() {
        let mut lp = RetryLoop::with_default_limit();
        let err = PiError::provider("x", "503 service unavailable");
        assert_eq!(lp.next_retry(&err), Some(2_000));
        assert_eq!(lp.next_retry(&err), Some(4_000));
        assert_eq!(lp.next_retry(&err), Some(8_000));
        assert_eq!(lp.next_retry(&err), None, "exhausted");
        assert_eq!(lp.attempt(), 3);
    }

    #[test]
    fn loop_stops_on_non_retryable() {
        let mut lp = RetryLoop::with_default_limit();
        let err = PiError::auth("missing key");
        assert_eq!(lp.next_retry(&err), None);
    }

    #[test]
    fn abortable_sleep_bails_immediately() {
        let start = std::time::Instant::now();
        let done = abortable_sleep_ms(60_000, || true);
        assert!(!done);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn abortable_sleep_completes() {
        assert!(abortable_sleep_ms(100, || false));
    }
}
