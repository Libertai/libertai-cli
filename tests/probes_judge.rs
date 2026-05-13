//! Tier-2 probe harness — wraps a real LibertAI chat completion as a
//! cheap binary judge. Use for behavioral assertions that don't show
//! up in the assembled prompt: response brevity, parallel tool calls,
//! no-comments-default, etc.
//!
//! All tests in this file are `#[ignore]`d by default. Run with:
//! `cargo test --features tier2-probes -- --include-ignored`.
//!
//! Requires `LIBERTAI_API_KEY` in env. Reads `LIBERTAI_API_BASE`
//! (default: <https://api.libertai.io>) and `LIBERTAI_JUDGE_MODEL`
//! (default: `gemma-3-4b-it`) for routing.
//!
//! The judge model is asked, with `temperature=0` and `max_tokens=8`,
//! whether `payload` matches `criterion`. Any response starting with
//! "yes" (case-insensitive) → true; anything else → false.

#![cfg(feature = "tier2-probes")]

use std::time::Duration;

const DEFAULT_API_BASE: &str = "https://api.libertai.io";
const DEFAULT_JUDGE_MODEL: &str = "hermes-3-8b-tee";

/// Ask the judge model whether `payload` satisfies `criterion`.
///
/// Panics if no API key can be found or the API call fails — tier-2
/// probes are opt-in, so loud failure is the right behavior. Resolves
/// the key from (in order): `LIBERTAI_API_KEY` env, then the user's
/// `~/.config/libertai/config.toml` `api_key` field.
pub fn judge(criterion: &str, payload: &str) -> bool {
    let key = resolve_api_key()
        .expect("tier-2 probes need an API key — set LIBERTAI_API_KEY or run `libertai login`");
    let api_base = std::env::var("LIBERTAI_API_BASE")
        .unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
    let model = std::env::var("LIBERTAI_JUDGE_MODEL")
        .unwrap_or_else(|_| DEFAULT_JUDGE_MODEL.to_string());

    let url = format!("{}/v1/chat/completions", api_base.trim_end_matches('/'));

    let system = "You are a strict binary classifier. Reply with exactly \
                  one word: 'yes' or 'no'. Nothing else. No punctuation, \
                  no explanation.";
    let user = format!(
        "Criterion: {criterion}\n\nPayload:\n---\n{payload}\n---\n\n\
         Does the payload satisfy the criterion? Reply yes or no."
    );

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "temperature": 0.0,
        "max_tokens": 8,
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("build reqwest client");

    let resp = client
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .unwrap_or_else(|e| panic!("judge POST {url} failed: {e}"));

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        panic!("judge POST {url} returned {status}: {text}");
    }

    let v: serde_json::Value = resp.json().expect("judge JSON parse");
    let answer = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    answer.starts_with("yes")
}

fn resolve_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("LIBERTAI_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    let path = dirs::config_dir()?.join("libertai").join("config.toml");
    let raw = std::fs::read_to_string(path).ok()?;
    let v: toml::Value = toml::from_str(&raw).ok()?;
    v.get("auth")?.get("api_key")?.as_str().map(str::to_string)
}

#[test]
#[ignore = "tier-2: hits the LibertAI API; gate with --include-ignored"]
fn judge_smoke_says_yes_to_obvious_match() {
    assert!(
        judge(
            "is the payload the single letter A (uppercase)",
            "A"
        ),
        "judge should say yes for an obvious match"
    );
}

#[test]
#[ignore = "tier-2: hits the LibertAI API; gate with --include-ignored"]
fn judge_smoke_says_no_to_obvious_mismatch() {
    assert!(
        !judge(
            "is the payload the single letter A (uppercase)",
            "Z"
        ),
        "judge should say no for an obvious mismatch"
    );
}
