pub mod auth;
pub mod cli;
pub mod client;
pub mod commands;
pub mod config;
pub mod update_check;

/// Serialisation for tests that mutate process-global environment variables.
///
/// `std::env::set_var` affects the whole process, not the calling thread, so
/// two tests that set the same variable clobber each other when the harness
/// runs them in parallel. Any test touching `PI_CODING_AGENT_DIR`,
/// `LIBERTAI_API_KEY` or the catalog-URL override must hold this lock for its
/// whole body — including the `remove_var` cleanup at the end, or it will pull
/// the variable out from under a test that is still running.
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        // Recover from poisoning: one failing test should not cascade into
        // every other env-mutating test in the suite.
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
