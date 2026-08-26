//! `libertai code --acp` — expose the coding agent over the Agent Client
//! Protocol (ACP) so editors drive LibertAI's TEE-backed inference directly.
//!
//! ACP is Zed's editor↔agent standard: line-delimited JSON-RPC 2.0 over
//! stdin/stdout, spoken natively by Zed and (since Dec 2025) the JetBrains
//! IDEs. The protocol server itself lives in the pinned `pi_agent_rust`
//! engine (`pi::acp::run_stdio`); this module is the LibertAI adapter that
//! builds its [`AcpOptions`](pi::acp::AcpOptions) out of exactly the same
//! auth / model / endpoint resolution `libertai code` uses:
//!
//! - [`code_models::ensure_libertai_registered`] writes the `libertai`
//!   provider into pi's `models.json` (base URL from `cfg.api_base`, API key
//!   as the in-memory `env:LIBERTAI_API_KEY` indirection) — the same call
//!   `libertai code` makes via `prepare_agent_environment`.
//! - The resolved provider/model (flags → `default_code_provider` /
//!   `default_code_model`) is stamped onto pi's `Config` as
//!   `default_provider` / `default_model`, which is what pi's
//!   `select_acp_model_entry` keys off when a client calls `session/new`.
//!   Without this, ACP would pick "the first ready model", which on a
//!   machine with other providers registered is not LibertAI.
//! - The full [`ModelRegistry`](pi::models::ModelRegistry) is handed over so
//!   an editor's model picker (`session/set_model`) can switch between every
//!   catalogued LibertAI model at runtime.
//!
//! ## Stdout discipline
//!
//! stdout is the protocol wire. Anything else written there — an update
//! banner, a login prompt, a progress bar, an ANSI reset — desynchronises
//! the client's line framing and the handshake dies. Everything on this
//! path is therefore either silent or stderr-only:
//!
//! - The update-check banner is suppressed twice over: `dispatch` skips
//!   `maybe_notify` outright for stdio-protocol commands (see
//!   [`crate::cli::is_stdio_protocol_command`]), and `update_check`'s own
//!   `print_banner` writes to stderr anyway.
//! - No interactive prompt runs: a missing API key is a hard error out of
//!   `ensure_libertai_registered` (stderr, non-zero exit) rather than a
//!   browser-login flow, and no approval UI is constructed — tool approvals
//!   travel over ACP's own `session/request_permission` requests.
//! - No `tracing` subscriber is installed anywhere in this process (pi
//!   installs one only in its own `main`), so engine-side `tracing::info!`
//!   calls are no-ops rather than stray lines.
//! - `LIBERTAI_ACP=1` is exported for any child process that wants to know
//!   it must stay quiet.
//!
//! The invariant is asserted end-to-end by `tests/probes_acp.rs`, which
//! spawns the real binary, runs an `initialize` handshake, and requires
//! stdout to be nothing but well-formed JSON-RPC.

use anyhow::{Context, Result};

use crate::commands::{code_identity_prompt, code_memory, code_models, code_session};
use crate::config;

/// Set on the process (and inherited by children) while the ACP server is
/// running. Anything tempted to write to stdout can check it.
pub const ACP_MODE_ENV: &str = "LIBERTAI_ACP";

/// Run the ACP server on stdin/stdout until the client disconnects.
///
/// `model` / `provider` mirror `libertai code`'s flags; unset falls back to
/// the config defaults.
pub fn run(model: Option<String>, provider: Option<String>) -> Result<()> {
    let cfg = config::load()?;
    let model = model.unwrap_or_else(|| cfg.default_code_model.clone());
    let provider = provider.unwrap_or_else(|| cfg.default_code_provider.clone());

    // Mark the process before anything else so children inherit it.
    std::env::set_var(ACP_MODE_ENV, "1");

    // Same as `libertai code`: pi reads this once through a OnceLock, so it
    // has to be set before the first provider request is built.
    code_session::ensure_pi_http_timeout(cfg.http_timeout_secs);

    // Brand the engine's base system prompt as LibertAI Code (env-driven,
    // read later by pi's `build_acp_system_prompt`).
    code_identity_prompt::set_brand_env();

    // Register LibertAI in pi's models.json and export LIBERTAI_API_KEY for
    // the `env:` indirection. Errors here (no API key) are fatal and land on
    // stderr — never a prompt, since stdin belongs to the protocol.
    code_models::ensure_libertai_registered(&cfg)
        .context("registering the libertai provider for ACP")?;
    code_memory::ensure_memory_env()?;

    // pi's ACP loop spawns per-prompt work onto the runtime handle it is
    // given, so it needs a real multi-threaded runtime (the current-thread
    // builder used by the one-shot path would starve those tasks while the
    // stdio loop is parked).
    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|e| anyhow::anyhow!("asupersync reactor: {e}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::multi_thread()
        .blocking_threads(1, 2)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!("asupersync runtime: {e}"))?;

    let options = build_acp_options(&provider, &model, runtime.handle())?;

    runtime
        .block_on(pi::acp::run_stdio(options))
        .map_err(anyhow::Error::new)
}

/// Build [`AcpOptions`](pi::acp::AcpOptions) for `provider`/`model`.
///
/// Everything the protocol server needs — pi config with the LibertAI
/// defaults stamped in, auth storage, the model registry and the ready-model
/// list — is resolved in one place so the wiring can be asserted in a test.
fn build_acp_options(
    provider: &str,
    model: &str,
    runtime_handle: asupersync::runtime::RuntimeHandle,
) -> Result<pi::acp::AcpOptions> {
    let mut pi_config = pi::config::Config::load().map_err(anyhow::Error::new)?;
    // What pi's `select_acp_model_entry` reads at `session/new`.
    pi_config.default_provider = Some(provider.to_string());
    pi_config.default_model = Some(model.to_string());

    let global_dir = pi::config::Config::global_dir();
    let auth = pi::auth::AuthStorage::load(pi::config::Config::auth_path())
        .map_err(anyhow::Error::new)
        .context("loading pi auth storage")?;
    let models_path = pi::models::default_models_path(&global_dir);
    let model_registry = pi::models::ModelRegistry::load(&auth, Some(models_path));
    if let Some(error) = model_registry.error() {
        // stderr only — stdout is the protocol wire.
        eprintln!("libertai: models.json warning: {error}");
    }
    let available_models = model_registry.get_available();
    if available_models.is_empty() {
        anyhow::bail!(
            "no models are available to the ACP server — run `libertai login` \
             (and `libertai models --refresh`) first"
        );
    }
    if !available_models
        .iter()
        .any(|entry| entry.model.id.eq_ignore_ascii_case(model))
    {
        eprintln!(
            "libertai: model `{model}` is not in the registry; the ACP server will fall back \
             to the provider default. Run `libertai models --refresh` to pick it up."
        );
    }

    Ok(pi::acp::AcpOptions {
        config: pi_config,
        available_models,
        model_registry,
        auth,
        runtime_handle,
        // ACP sessions stay in-memory, matching upstream `pi --acp` with no
        // `--session-dir`: a session's cwd arrives per `session/new`, so a
        // single process-wide directory would file every editor session
        // under whichever project the editor happened to launch us from.
        session_dir: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the adapter: an editor connecting over ACP must get
    /// LibertAI, not whatever other provider happens to be registered first.
    /// This checks the wiring end-to-end through the pinned pi rev — register
    /// libertai, build the options, and confirm pi's own `session/new` model
    /// preference (`config.default_provider` / `default_model`) points at the
    /// LibertAI entry with a resolvable endpoint.
    ///
    /// Shares `test_env::lock()` with `code_models`'s registration test: both
    /// set `PI_CODING_AGENT_DIR` / `LIBERTAI_API_KEY`, which are process-wide.
    #[test]
    fn acp_options_default_to_the_libertai_provider_and_model() {
        let _env = crate::test_env::lock();
        const KEY: &str = "LTAI_sk_unit_probe_acp_00000000000000";
        let pi_dir = tempfile::tempdir().expect("pi tempdir");
        std::env::set_var("PI_CODING_AGENT_DIR", pi_dir.path());
        // Hermetic: no catalog fetch.
        std::env::set_var(crate::commands::model_catalog::CATALOG_URL_ENV, "off");

        let mut cfg = config::Config::default();
        cfg.auth.api_key = Some(KEY.to_string());
        code_models::ensure_libertai_registered(&cfg).expect("registration succeeds");

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let options = build_acp_options(
            &cfg.default_code_provider,
            &cfg.default_code_model,
            runtime.handle(),
        )
        .expect("acp options build");

        assert_eq!(
            options.config.default_provider.as_deref(),
            Some(cfg.default_code_provider.as_str())
        );
        assert_eq!(
            options.config.default_model.as_deref(),
            Some(cfg.default_code_model.as_str())
        );
        let entry = options
            .available_models
            .iter()
            .find(|e| e.model.id.eq_ignore_ascii_case(&cfg.default_code_model))
            .expect("libertai default model is available to ACP");
        assert_eq!(entry.model.provider, cfg.default_code_provider);
        assert_eq!(
            entry.api_key.as_deref(),
            Some(KEY),
            "ACP would start without a usable LibertAI key"
        );

        std::env::remove_var("PI_CODING_AGENT_DIR");
        std::env::remove_var(code_models::LIBERTAI_API_KEY_ENV);
        std::env::remove_var(crate::commands::model_catalog::CATALOG_URL_ENV);
    }
}
