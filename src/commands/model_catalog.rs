//! Model metadata (context window, pricing, vision) from `/v1/models`
//! entries, and the writers that turn it into third-party agent configs.

use serde_json::{json, Map, Value};

use crate::client::ModelEntry;

/// USD per million tokens from a `/v1/models` per-token price (string or
/// number). Negative values (OpenRouter's `-1` "unset") yield `None`.
/// Rounded to 6 decimals so `"0.00000015"` reads back as exactly `0.15`.
fn per_mtok(per_token: &Value) -> Option<f64> {
    let v = match per_token {
        Value::String(s) => s.parse().ok()?,
        v => v.as_f64()?,
    };
    (v >= 0.0).then(|| (v * 1e12).round() / 1e6)
}

impl ModelEntry {
    /// Whether this id may be offered as a chat model. The listing also
    /// carries image, embedding and audio ids, which only lack
    /// `architecture`. An entry with no metadata at all is kept, so a model
    /// the API knows nothing about still surfaces.
    pub fn is_chat(&self) -> bool {
        match &self.architecture {
            Some(a) => a.output_modalities.iter().any(|m| m == "text"),
            None => self.name.is_none() && self.pricing.is_none(),
        }
    }

    /// `None` when the API doesn't say.
    pub fn vision(&self) -> Option<bool> {
        let a = self.architecture.as_ref()?;
        Some(a.input_modalities.iter().any(|m| m == "image"))
    }

    /// USD per million (input, output, cached input) tokens.
    pub fn rates_per_mtok(&self) -> Option<(f64, f64, Option<f64>)> {
        let p = self.pricing.as_ref()?;
        Some((
            per_mtok(p.prompt.as_ref()?)?,
            per_mtok(p.completion.as_ref()?)?,
            p.input_cache_read.as_ref().and_then(per_mtok),
        ))
    }
}

// ── opencode config integration ─────────────────────────────────────────────

/// Context/output limits written when the API supplies no real window.
const OPENCODE_FALLBACK_CONTEXT: u32 = 200_000;
const OPENCODE_FALLBACK_OUTPUT: u32 = 32_768;

/// Build the opencode `provider.libertai.models[<id>]` entry. `meta` is the
/// id's `/v1/models` entry, `None` when it isn't listed. `cost` values are
/// USD per million tokens (opencode's models.dev convention). `reasoning`
/// follows the id: LibertAI serves `<base>-thinking` as the reasoning
/// variant. opencode 1.18+ derives image support from `modalities.input`,
/// not the legacy `attachment` flag, so both are written; models without
/// architecture metadata get neither (opencode's text-only default).
pub fn opencode_model_entry(id: &str, meta: Option<&ModelEntry>) -> Value {
    let context = meta
        .and_then(|m| m.context_length)
        .unwrap_or(OPENCODE_FALLBACK_CONTEXT);

    let mut entry = Map::new();
    entry.insert("name".to_string(), Value::String(id.to_string()));
    entry.insert(
        "reasoning".to_string(),
        Value::Bool(id.ends_with("-thinking")),
    );
    entry.insert(
        "limit".to_string(),
        json!({ "context": context, "output": OPENCODE_FALLBACK_OUTPUT }),
    );
    if let Some(vision) = meta.and_then(ModelEntry::vision) {
        entry.insert("attachment".to_string(), Value::Bool(vision));
        let mut input = vec!["text"];
        if vision {
            input.push("image");
        }
        entry.insert(
            "modalities".to_string(),
            json!({ "input": input, "output": ["text"] }),
        );
    }
    if let Some((input, output, cache_read)) = meta.and_then(ModelEntry::rates_per_mtok) {
        entry.insert(
            "cost".to_string(),
            json!({
                "input": input,
                "output": output,
                "cache_read": cache_read.unwrap_or(0.0),
                "cache_write": 0.0,
            }),
        );
    }
    Value::Object(entry)
}

// ── Presentation helpers (shared by `libertai models`) ─────────────────────

/// `262144` → `"262k"`, `16000` → `"16k"`; sub-1000 windows print raw.
pub fn format_context_window(ctx: u32) -> String {
    if ctx >= 1000 {
        format!("{}k", (ctx + 500) / 1000)
    } else {
        ctx.to_string()
    }
}

/// `(0.15, 0.5)` → `"$0.15 / $0.50"` (USD per million tokens, in / out).
pub fn format_price_per_mtok(input: f64, output: f64) -> String {
    format!("${input:.2} / ${output:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: Value) -> ModelEntry {
        serde_json::from_value(v).expect("valid /v1/models entry")
    }

    fn vision_chat() -> ModelEntry {
        entry(json!({
            "id": "qwen3.6-35b-a3b",
            "object": "model",
            "owned_by": "libertai",
            "name": "Qwen3.6-35B-A3B",
            "context_length": 262144,
            "architecture": {
                "modality": "text+image->text",
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"]
            },
            "pricing": {
                "prompt": "0.00000015",
                "completion": "0.0000005",
                "input_cache_read": "0.00000005"
            }
        }))
    }

    fn text_chat() -> ModelEntry {
        entry(json!({
            "id": "hermes-3-8b-tee",
            "name": "Hermes 3 8B (TEE)",
            "context_length": 16000,
            "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
            "pricing": {"prompt": "0.00000015", "completion": "0.0000006"}
        }))
    }

    #[test]
    fn wire_fields_roundtrip() {
        let v = serde_json::to_value(vision_chat()).unwrap();
        assert_eq!(v["object"], "model");
        assert_eq!(v["architecture"]["modality"], "text+image->text");
        assert_eq!(v["pricing"]["input_cache_read"], "0.00000005");
        assert!(serde_json::to_value(entry(json!({"id": "x"})))
            .unwrap()
            .get("owned_by")
            .is_none());
    }

    #[test]
    fn rates_are_exact_per_mtok() {
        assert_eq!(
            vision_chat().rates_per_mtok(),
            Some((0.15, 0.5, Some(0.05)))
        );
        assert_eq!(text_chat().rates_per_mtok(), Some((0.15, 0.6, None)));
        assert_eq!(entry(json!({"id": "x"})).rates_per_mtok(), None);
        assert_eq!(
            entry(
                json!({"id": "x", "pricing": {"prompt": 0.00000025, "completion": "0.00000175"}})
            )
            .rates_per_mtok(),
            Some((0.25, 1.75, None))
        );
        assert_eq!(
            entry(json!({"id": "x", "pricing": {"prompt": "-1", "completion": "-1"}}))
                .rates_per_mtok(),
            None
        );
    }

    #[test]
    fn odd_pricing_shape_does_not_fail_the_listing() {
        let list: crate::client::ModelList = serde_json::from_value(json!({"data": [
            {"id": "img", "pricing": {"image": 0.005}},
            {"id": "qwen", "pricing": {"prompt": "0.00000015", "completion": "0.0000005"}}
        ]}))
        .expect("listing parses");
        assert_eq!(list.data[0].rates_per_mtok(), None);
        assert_eq!(list.data[1].rates_per_mtok(), Some((0.15, 0.5, None)));
    }

    #[test]
    fn is_chat_drops_only_known_non_text() {
        assert!(vision_chat().is_chat());
        assert!(!entry(json!({"id": "z-image-turbo", "name": "Z-Image Turbo"})).is_chat());
        assert!(!entry(json!({
            "id": "kokoro",
            "architecture": {"input_modalities": ["text"], "output_modalities": ["audio"]}
        }))
        .is_chat());
        // No metadata at all: kept (err to inclusion).
        assert!(entry(json!({"id": "brand-new-model", "owned_by": "libertai"})).is_chat());
    }

    #[test]
    fn opencode_entry_carries_cost_context_and_reasoning() {
        let m = text_chat();
        let e = opencode_model_entry(&m.id, Some(&m));
        assert_eq!(e["name"], "hermes-3-8b-tee");
        assert_eq!(e["reasoning"], false);
        assert_eq!(e["limit"]["context"], 16_000);
        assert_eq!(e["attachment"], false);
        assert_eq!(e["modalities"]["input"], json!(["text"]));
        assert_eq!(e["cost"]["input"], 0.15);
        assert_eq!(e["cost"]["output"], 0.6);
        assert_eq!(e["cost"]["cache_read"], 0.0);
        assert_eq!(e["cost"]["cache_write"], 0.0);
    }

    #[test]
    fn opencode_entry_declares_vision_models_as_attachment_capable() {
        let m = vision_chat();
        let e = opencode_model_entry(&m.id, Some(&m));
        assert_eq!(e["attachment"], true);
        assert_eq!(e["modalities"]["input"], json!(["text", "image"]));
        assert_eq!(e["modalities"]["output"], json!(["text"]));
        assert_eq!(e["cost"]["cache_read"], 0.05);
    }

    #[test]
    fn opencode_entry_reasoning_follows_thinking_suffix() {
        let m = vision_chat();
        let e = opencode_model_entry("qwen3.6-35b-a3b-thinking", Some(&m));
        assert_eq!(e["reasoning"], true);
        assert_eq!(e["limit"]["context"], 262_144);
    }

    #[test]
    fn opencode_entry_unlisted_has_fallback_limits_only() {
        let e = opencode_model_entry("brand-new-model", None);
        assert_eq!(e["limit"]["context"], 200_000);
        assert_eq!(e["reasoning"], false);
        assert!(e.get("cost").is_none());
        assert!(e.get("attachment").is_none());
        assert!(e.get("modalities").is_none());
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_context_window(262_144), "262k");
        assert_eq!(format_context_window(16_000), "16k");
        assert_eq!(format_context_window(512), "512");
        assert_eq!(format_price_per_mtok(0.15, 0.5), "$0.15 / $0.50");
    }
}
