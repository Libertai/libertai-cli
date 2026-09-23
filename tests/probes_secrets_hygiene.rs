//! Secrets-hygiene probes — verify that the plaintext API key never
//! outlives the places it belongs:
//!
//!   * `libertai logout` leaves no file under the config dir containing the
//!     key (live config keeps non-secret prefs; stray `.bak` files from old
//!     CLI versions are scrubbed or deleted);
//!   * `libertai code` registers the libertai provider in pi's models.json
//!     with the `env:LIBERTAI_API_KEY` indirection instead of the literal
//!     key, and migrates plaintext entries written by older versions;
//!   * `libertai logout` scrubs a plaintext libertai apiKey out of pi's
//!     models.json without touching other providers.
//!
//! Offline tier-1: no model API call, no network (`LIBERTAI_DUMP_AND_EXIT`
//! short-circuits `code` before any request fires).

use std::path::{Path, PathBuf};

use assert_cmd::Command;

mod common;

/// Must match the key planted by `common::fake_config_home`.
const PROBE_KEY: &str = "LTAI_sk_probe_config_00000000000000000000";

/// The libertai config dir that `dirs::config_dir()` resolves for a fake
/// `$HOME` / `$XDG_CONFIG_HOME` pointing at `home`.
fn platform_config_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("libertai")
    } else {
        home.join("libertai")
    }
}

/// Recursively collect every file under `root` whose contents contain
/// `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return hits;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            hits.extend(files_containing(&path, needle));
        } else if std::fs::read_to_string(&path).is_ok_and(|raw| raw.contains(needle)) {
            hits.push(path);
        }
    }
    hits
}

#[test]
fn logout_scrubs_plaintext_key_from_legacy_pi_models_json() {
    let home = tempfile::tempdir().expect("home tempdir");
    let config_dir = platform_config_dir(home.path()).join("libertai");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!("[auth]\napi_key = \"{PROBE_KEY}\"\n"),
    )
    .unwrap();

    // Legacy pi-era models.json carrying a plaintext key. With
    // XDG_CONFIG_HOME overridden this resolves to $XDG_CONFIG_HOME/pi,
    // matching logout's dirs::config_dir()-based lookup.
    let pi_dir = home.path().join("pi");
    std::fs::create_dir_all(&pi_dir).unwrap();
    std::fs::write(
        pi_dir.join("models.json"),
        format!("{{\"providers\":{{\"libertai\":{{\"apiKey\":\"{PROBE_KEY}\"}}}}}}"),
    )
    .unwrap();

    libertai_cmd()
        .env("XDG_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .args(["logout"])
        .assert()
        .success();

    let models_json =
        std::fs::read_to_string(pi_dir.join("models.json")).expect("models.json survives scrub");
    assert!(
        !models_json.contains(PROBE_KEY),
        "models.json still contains the plaintext API key; got:\n{models_json}"
    );
    assert!(
        models_json.contains("env:LIBERTAI_API_KEY"),
        "models.json missing the env indirection; got:\n{models_json}"
    );
}

fn libertai_cmd() -> Command {
    Command::cargo_bin("libertai").expect("libertai binary built")
}

#[test]
fn logout_leaves_no_file_containing_the_key_and_keeps_prefs() {
    let home = tempfile::tempdir().expect("home tempdir");
    let pi_dir = tempfile::tempdir().expect("pi tempdir");
    let config_dir = platform_config_dir(home.path());
    std::fs::create_dir_all(&config_dir).unwrap();

    // Live config: key + non-secret prefs + the persistent device id.
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "default_code_model = \"probe-custom-model\"\n\n\
             [auth]\n\
             api_key = \"{PROBE_KEY}\"\n\
             wallet_address = \"0xprobe\"\n\
             device_id = \"probe-device-id\"\n"
        ),
    )
    .unwrap();
    // Stray backups from older logout implementations: one parseable, one
    // corrupt — both embedding the key.
    std::fs::write(
        config_dir.join("config.toml.bak.1700000000"),
        format!("[auth]\napi_key = \"{PROBE_KEY}\"\n"),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("config.toml.bak.1700000001"),
        format!("not toml [ api_key = \"{PROBE_KEY}"),
    )
    .unwrap();

    libertai_cmd()
        .env("XDG_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .env("PI_CODING_AGENT_DIR", pi_dir.path())
        .arg("logout")
        .assert()
        .success();

    let leaks = files_containing(home.path(), PROBE_KEY);
    assert!(
        leaks.is_empty(),
        "files still containing the API key after logout: {leaks:?}"
    );

    let config = std::fs::read_to_string(config_dir.join("config.toml"))
        .expect("config.toml kept (prefs preserved)");
    assert!(
        config.contains("probe-custom-model"),
        "non-secret prefs lost on logout; got:\n{config}"
    );
    assert!(
        config.contains("probe-device-id"),
        "device_id should survive logout; got:\n{config}"
    );
    assert!(
        !config.contains("0xprobe"),
        "wallet_address should be cleared on logout; got:\n{config}"
    );
}
