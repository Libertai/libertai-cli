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
const ENV_REF: &str = "env:LIBERTAI_API_KEY";

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

#[test]
fn code_dry_run_writes_env_indirection_not_plaintext_key() {
    let home = common::fake_config_home();
    let pi_dir = tempfile::tempdir().expect("pi tempdir");

    libertai_cmd()
        .env("XDG_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .env("PI_CODING_AGENT_DIR", pi_dir.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success();

    let models_json = std::fs::read_to_string(pi_dir.path().join("models.json"))
        .expect("code run registers models.json");
    assert!(
        models_json.contains(ENV_REF),
        "models.json missing `{ENV_REF}` indirection; got:\n{models_json}"
    );
    assert!(
        !models_json.contains(PROBE_KEY),
        "models.json contains the plaintext API key; got:\n{models_json}"
    );
}

#[test]
fn code_dry_run_migrates_legacy_plaintext_models_json() {
    let home = common::fake_config_home();
    let pi_dir = tempfile::tempdir().expect("pi tempdir");
    // A models.json written by an older CLI version: literal libertai key,
    // plus an unrelated provider that must be preserved verbatim.
    std::fs::write(
        pi_dir.path().join("models.json"),
        format!(
            r#"{{
  "providers": {{
    "libertai": {{
      "baseUrl": "https://api.libertai.io/v1",
      "api": "openai-completions",
      "apiKey": "{PROBE_KEY}",
      "authHeader": true,
      "models": [
        {{ "id": "legacy-model", "name": "legacy-model", "api": "openai-completions", "contextWindow": 32768 }}
      ]
    }},
    "otherco": {{ "apiKey": "other-providers-secret" }}
  }}
}}"#
        ),
    )
    .unwrap();

    libertai_cmd()
        .env("XDG_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .env("PI_CODING_AGENT_DIR", pi_dir.path())
        .env("LIBERTAI_DUMP_SYSTEM_PROMPT", "1")
        .env("LIBERTAI_DUMP_AND_EXIT", "1")
        .args(["code", "-p", "probe-ignored"])
        .assert()
        .success();

    let models_json = std::fs::read_to_string(pi_dir.path().join("models.json")).unwrap();
    assert!(
        !models_json.contains(PROBE_KEY),
        "legacy plaintext key not migrated out of models.json; got:\n{models_json}"
    );
    assert!(
        models_json.contains(ENV_REF),
        "models.json missing `{ENV_REF}` after migration; got:\n{models_json}"
    );
    assert!(
        models_json.contains("other-providers-secret"),
        "other providers must survive the libertai merge; got:\n{models_json}"
    );
    assert!(
        models_json.contains("legacy-model"),
        "existing libertai models array must be preserved; got:\n{models_json}"
    );
}

#[test]
fn logout_scrubs_plaintext_key_from_pi_models_json() {
    let home = common::fake_config_home();
    let pi_dir = tempfile::tempdir().expect("pi tempdir");
    std::fs::write(
        pi_dir.path().join("models.json"),
        format!(
            r#"{{
  "providers": {{
    "libertai": {{ "baseUrl": "https://api.libertai.io/v1", "apiKey": "{PROBE_KEY}" }},
    "otherco": {{ "apiKey": "other-providers-secret" }}
  }}
}}"#
        ),
    )
    .unwrap();

    libertai_cmd()
        .env("XDG_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .env("PI_CODING_AGENT_DIR", pi_dir.path())
        .arg("logout")
        .assert()
        .success();

    let models_json = std::fs::read_to_string(pi_dir.path().join("models.json")).unwrap();
    assert!(
        !models_json.contains(PROBE_KEY),
        "logout left the plaintext key in models.json; got:\n{models_json}"
    );
    assert!(
        models_json.contains(ENV_REF),
        "logout should swap the key for `{ENV_REF}`; got:\n{models_json}"
    );
    assert!(
        models_json.contains("other-providers-secret"),
        "logout must not touch other providers; got:\n{models_json}"
    );
}
