//! The `structured_output` tool — validate model output against a JSON
//! Schema and return it (M5/#14, per docs/overhaul-plan.md).
//!
//! When a subagent (or the orchestrator) needs to return machine-readable
//! data, it calls `structured_output(schema=<JSON Schema>, data=<value>)`.
//! The tool validates `data` against `schema` with a self-contained
//! validator (the common JSON Schema keywords — no heavy `jsonschema` dep)
//! and either echoes the data back as a non-error result (validated) or
//! returns an `is_error` result naming the violated path(s) so the model
//! retries with corrected data.
//!
//! A per-session failure cap (env `LIBERTAI_STRUCTURED_OUTPUT_RETRIES`,
//! default 5) stops a model from spinning forever on a schema it can't
//! satisfy: after N invalid calls the tool returns a terminal "retries
//! exhausted" error instead of re-validating.
//!
//! Read-only (no side effects — pure validation), so the factory
//! registers it unwrapped, like `todo`/`skill`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use pi::tools::ToolEffects;

const NAME: &str = "structured_output";
const LABEL: &str = "Structured output";

const DEFAULT_RETRIES: u64 = 5;

const DESCRIPTION: &str = concat!(
    "Return a JSON value validated against a JSON Schema. Pass `schema` ",
    "(a JSON Schema object) and `data` (the value to validate). On success ",
    "the validated `data` is echoed back. On failure the result is an error ",
    "naming the violated schema path(s) — fix the data and retry, up to a ",
    "small cap. Use this whenever the parent asked for structured, ",
    "machine-readable output (a findings list, a config object, a record) ",
    "so the shape is guaranteed, not just asserted in prose."
);

#[derive(Debug, Clone, Deserialize)]
struct StructuredOutputInput {
    schema: serde_json::Value,
    data: serde_json::Value,
    #[serde(default)]
    name: Option<String>,
}

pub struct StructuredOutputTool {
    // Per-session failure counter for the retry cap. The factory builds
    // one tool instance per session, so this counts failures across the
    // whole session's structured_output calls.
    failures: Arc<AtomicU64>,
    retry_cap: u64,
}

impl StructuredOutputTool {
    pub fn new() -> Self {
        Self {
            failures: Arc::new(AtomicU64::new(0)),
            retry_cap: structured_output_retry_cap(),
        }
    }

    /// Test-only constructor with an explicit cap (avoids env races in
    /// unit tests that would otherwise need to set
    /// `LIBERTAI_STRUCTURED_OUTPUT_RETRIES` process-globally).
    #[cfg(test)]
    pub fn with_retry_cap(cap: u64) -> Self {
        Self {
            failures: Arc::new(AtomicU64::new(0)),
            retry_cap: cap,
        }
    }
}

impl Default for StructuredOutputTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session retry cap. Reads `LIBERTAI_STRUCTURED_OUTPUT_RETRIES`;
/// empty/invalid falls back to [`DEFAULT_RETRIES`] (so a stray
/// `LIBERTAI_STRUCTURED_OUTPUT_RETRIES=` doesn't silently disable the cap).
pub fn structured_output_retry_cap() -> u64 {
    match std::env::var("LIBERTAI_STRUCTURED_OUTPUT_RETRIES") {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().parse::<u64>().unwrap_or(DEFAULT_RETRIES),
        _ => DEFAULT_RETRIES,
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "schema": {
                    "type": "object",
                    "description": "A JSON Schema describing the expected shape of `data`."
                },
                "data": {
                    "description": "The value to validate against `schema`."
                },
                "name": {
                    "type": "string",
                    "description": "Optional label echoed back with the validated data."
                }
            },
            "required": ["schema", "data"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: StructuredOutputInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!(
                    "invalid `structured_output` payload: {e}"
                )));
            }
        };

        // Retry cap: once the session has accumulated `retry_cap` invalid
        // calls, stop validating — return a terminal error so a model
        // stuck on an unsatisfiable schema doesn't spin forever.
        let prior = self.failures.load(Ordering::Relaxed);
        if prior >= self.retry_cap {
            return Ok(err_output(&format!(
                "structured_output retry cap reached ({}/{cap}). The schema has \
                 failed validation {cap} times this session; stop retrying and \
                 report the mismatch to the parent in prose instead.",
                prior,
                cap = self.retry_cap,
            )));
        }

        match validate(&parsed.data, &parsed.schema) {
            Ok(()) => {
                // Reset the failure counter on a success: the cap is on
                // *consecutive-ish* failures against a schema the model
                // can't satisfy, not a lifetime budget across unrelated
                // successful structured outputs.
                self.failures.store(0, Ordering::Relaxed);
                Ok(ok_output(&parsed.data, parsed.name.as_deref()))
            }
            Err(errors) => {
                let count = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                Ok(validation_error_output(&errors, count, self.retry_cap))
            }
        }
    }

    fn effects(&self) -> ToolEffects {
        // Pure validation; no writes, no network.
        ToolEffects::read()
    }
}

/// Success: echo the validated data back, optionally keyed by `name`.
fn ok_output(data: &serde_json::Value, name: Option<&str>) -> ToolExecution {
    let mut text = String::from("Validated structured output:\n\n");
    if let Some(name) = name {
        text.push_str(&format!("name: {name}\n"));
    }
    // Pretty-printed so the model + logs can read it; the data round-trips
    // via JSON so nested values survive intact.
    let pretty = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
    text.push_str("data: ");
    text.push_str(&pretty);
    text.push('\n');
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(serde_json::json!({ "validated": true, "name": name, "data": data })),
        is_error: false,
    }
    .into()
}

/// Validation failure: list the violated paths so the model can fix them.
fn validation_error_output(errors: &[ValidationError], count: u64, cap: u64) -> ToolExecution {
    let mut text = format!(
        "structured_output: data failed schema validation ({count}/{cap} failures this session).\n\n"
    );
    text.push_str("Fix the data and retry. Violations:\n");
    for e in errors {
        text.push_str(&format!("- at `{}`: {}\n", e.path, e.message));
    }
    if count >= cap {
        text.push_str("\nRetry cap reached — stop retrying and report the mismatch in prose.\n");
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(serde_json::json!({
            "validated": false,
            "failures": count,
            "errors": errors.iter().map(|e| serde_json::json!({
                "path": e.path,
                "message": e.message,
            })).collect::<Vec<_>>(),
        })),
        is_error: true,
    }
    .into()
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

// ---- Self-contained JSON Schema validator (common keywords) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationError {
    pub path: String,
    pub message: String,
}

/// Validate `data` against `schema`. Returns all accumulated errors
/// (not just the first) so the model can fix everything in one retry.
/// Supports the common JSON Schema keywords the model will reach for:
/// `type`, `properties`, `required`, `items`, `additionalProperties`,
/// `enum`, `const`, `minimum`/`maximum`, `minLength`/`maxLength`,
/// `minItems`/`maxItems`, and `oneOf`/`anyOf`/`allOf` (best-effort).
pub(crate) fn validate(
    data: &serde_json::Value,
    schema: &serde_json::Value,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    validate_at(data, schema, String::new(), &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_at(
    data: &serde_json::Value,
    schema: &serde_json::Value,
    path: String,
    errors: &mut Vec<ValidationError>,
) {
    let Some(obj) = schema.as_object() else {
        // A non-object schema isn't a valid schema; treat it as a const
        // equality check (so `schema: true` validates anything, `false`
        // validates nothing, a literal value must equal `data`).
        match schema {
            serde_json::Value::Bool(true) => return,
            serde_json::Value::Bool(false) => {
                errors.push(ValidationError {
                    path: path.clone(),
                    message: "schema is `false` (rejects everything)".to_string(),
                });
            }
            other => {
                if data != other {
                    errors.push(ValidationError {
                        path: path.clone(),
                        message: format!("expected `{other}` (const schema)"),
                    });
                }
            }
        }
        return;
    };

    // `allOf` — every subschema must pass.
    if let Some(all) = obj.get("allOf").and_then(|v| v.as_array()) {
        for sub in all {
            validate_at(data, sub, path.clone(), errors);
        }
    }
    // `anyOf` — at least one subschema must pass (accumulate no error if
    // one passes; if none pass, report a single anyOf failure).
    if let Some(any) = obj.get("anyOf").and_then(|v| v.as_array()) {
        let any_passes = any.iter().any(|sub| {
            let mut tmp = Vec::new();
            validate_at(data, sub, String::new(), &mut tmp);
            tmp.is_empty()
        });
        if !any_passes {
            errors.push(ValidationError {
                path: path.clone(),
                message: "value matches none of `anyOf` subschemas".to_string(),
            });
        }
    }
    // `oneOf` — exactly one subschema must pass.
    if let Some(one) = obj.get("oneOf").and_then(|v| v.as_array()) {
        let passes = one
            .iter()
            .filter(|sub| {
                let mut tmp = Vec::new();
                validate_at(data, sub, String::new(), &mut tmp);
                tmp.is_empty()
            })
            .count();
        if passes != 1 {
            errors.push(ValidationError {
                path: path.clone(),
                message: format!("`oneOf` matched {passes} subschemas (expected exactly 1)"),
            });
        }
    }
    // `enum`
    if let Some(allowed) = obj.get("enum").and_then(|v| v.as_array()) {
        if !allowed.contains(data) {
            errors.push(ValidationError {
                path: path.clone(),
                message: format!("value not in `enum` {allowed:?}"),
            });
        }
    }
    // `const`
    if let Some(c) = obj.get("const") {
        if data != c {
            errors.push(ValidationError {
                path: path.clone(),
                message: format!("value != `const` {c}"),
            });
        }
    }

    // `type` — may be a single type string or a list.
    if let Some(ty) = obj.get("type") {
        let types: Vec<String> = match ty {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() {
            let matched = types.iter().any(|t| type_matches(data, t));
            if !matched {
                errors.push(ValidationError {
                    path: path.clone(),
                    message: format!("expected type `{}`", types.join(" | ")),
                });
            }
        }
    }

    match data {
        serde_json::Value::Object(map) => {
            // `required` keys.
            if let Some(req) = obj.get("required").and_then(|v| v.as_array()) {
                for r in req {
                    if let Some(name) = r.as_str() {
                        if !map.contains_key(name) {
                            errors.push(ValidationError {
                                path: child_path(&path, name),
                                message: "missing required property".to_string(),
                            });
                        }
                    }
                }
            }
            // `properties` per-key validation.
            if let Some(props) = obj.get("properties").and_then(|v| v.as_object()) {
                for (key, sub) in props {
                    if let Some(val) = map.get(key) {
                        validate_at(val, sub, child_path(&path, key), errors);
                    }
                }
            }
            // `additionalProperties`: when `false`, no keys outside
            // `properties` are allowed; when a schema, validate extras
            // against it.
            if let Some(add) = obj.get("additionalProperties") {
                let props = obj.get("properties").and_then(|v| v.as_object());
                let known: std::collections::HashSet<&str> = props
                    .map(|p| p.keys().map(|k| k.as_str()).collect())
                    .unwrap_or_default();
                for (key, val) in map {
                    if known.contains(key.as_str()) {
                        continue;
                    }
                    match add {
                        serde_json::Value::Bool(false) => {
                            errors.push(ValidationError {
                                path: child_path(&path, key),
                                message: "additional property not allowed".to_string(),
                            });
                        }
                        serde_json::Value::Bool(true) => {}
                        schema => validate_at(val, schema, child_path(&path, key), errors),
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            // `items` validates every element.
            if let Some(items) = obj.get("items") {
                for (i, val) in arr.iter().enumerate() {
                    validate_at(val, items, child_path(&path, &i.to_string()), errors);
                }
            }
            if let Some(n) = obj.get("minItems").and_then(|v| v.as_u64()) {
                if (arr.len() as u64) < n {
                    errors.push(ValidationError {
                        path: path.clone(),
                        message: format!("array length {} < minItems {n}", arr.len()),
                    });
                }
            }
            if let Some(n) = obj.get("maxItems").and_then(|v| v.as_u64()) {
                if (arr.len() as u64) > n {
                    errors.push(ValidationError {
                        path: path.clone(),
                        message: format!("array length {} > maxItems {n}", arr.len()),
                    });
                }
            }
        }
        serde_json::Value::String(s) => {
            if let Some(n) = obj.get("minLength").and_then(|v| v.as_u64()) {
                if (s.chars().count() as u64) < n {
                    errors.push(ValidationError {
                        path: path.clone(),
                        message: format!("string length < minLength {n}"),
                    });
                }
            }
            if let Some(n) = obj.get("maxLength").and_then(|v| v.as_u64()) {
                if (s.chars().count() as u64) > n {
                    errors.push(ValidationError {
                        path: path.clone(),
                        message: format!("string length > maxLength {n}"),
                    });
                }
            }
        }
        serde_json::Value::Number(n) => {
            if let Some(min) = obj.get("minimum").and_then(|v| v.as_f64()) {
                if let Some(v) = n.as_f64() {
                    if v < min {
                        errors.push(ValidationError {
                            path: path.clone(),
                            message: format!("{v} < minimum {min}"),
                        });
                    }
                }
            }
            if let Some(max) = obj.get("maximum").and_then(|v| v.as_f64()) {
                if let Some(v) = n.as_f64() {
                    if v > max {
                        errors.push(ValidationError {
                            path: path.clone(),
                            message: format!("{v} > maximum {max}"),
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

fn type_matches(data: &serde_json::Value, ty: &str) -> bool {
    match ty {
        "null" => data.is_null(),
        "boolean" => data.is_boolean(),
        "object" => data.is_object(),
        "array" => data.is_array(),
        "string" => data.is_string(),
        "number" => data.is_number(),
        // `integer` matches a JSON number with no fractional part. JSON
        // has no integer type; 1.0 is a valid integer per the spec.
        "integer" => data
            .as_number()
            .map(|n| {
                n.is_i64() || n.is_u64() || n.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false)
            })
            .unwrap_or(false),
        _ => true, // unknown type keyword — don't reject (lenient).
    }
}

fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("schema")
    }

    #[test]
    fn validates_object_with_required_and_typed_properties() {
        let s = schema(
            r#"{"type":"object","required":["name","age"],
            "properties":{"name":{"type":"string"},"age":{"type":"integer","minimum":0}}}"#,
        );
        let ok = serde_json::json!({"name":"Ada","age":36});
        assert!(validate(&ok, &s).is_ok());
        let missing = serde_json::json!({"name":"Ada"});
        let errs = validate(&missing, &s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.path == "age" && e.message.contains("missing required")));
    }

    #[test]
    fn reports_wrong_type_with_path() {
        let s = schema(r#"{"type":"object","properties":{"n":{"type":"number"}}}"#);
        let bad = serde_json::json!({"n":"x"});
        let errs = validate(&bad, &s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.path == "n" && e.message.contains("expected type")));
    }

    #[test]
    fn rejects_additional_properties_when_false() {
        let s = schema(
            r#"{"type":"object","properties":{"a":{"type":"string"}},
            "additionalProperties":false}"#,
        );
        assert!(validate(&serde_json::json!({"a":"x"}), &s).is_ok());
        let errs = validate(&serde_json::json!({"a":"x","b":1}), &s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.path == "b" && e.message.contains("additional property")));
    }

    #[test]
    fn validates_array_items_and_bounds() {
        let s = schema(r#"{"type":"array","items":{"type":"integer"},"minItems":1,"maxItems":3}"#);
        assert!(validate(&serde_json::json!([1, 2, 3]), &s).is_ok());
        let errs = validate(&serde_json::json!([1, "x"]), &s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.path == "1" && e.message.contains("expected type")));
        let errs = validate(&serde_json::json!([1, 2, 3, 4]), &s).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("maxItems")));
        let errs = validate(&serde_json::json!([]), &s).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("minItems")));
    }

    #[test]
    fn enum_and_const_checks() {
        let s = schema(r#"{"enum":["a","b","c"]}"#);
        assert!(validate(&serde_json::json!("b"), &s).is_ok());
        assert!(validate(&serde_json::json!("z"), &s).is_err());
        let s = schema(r#"{"const":42}"#);
        assert!(validate(&serde_json::json!(42), &s).is_ok());
        assert!(validate(&serde_json::json!(43), &s).is_err());
    }

    #[test]
    fn string_length_and_number_bounds() {
        let s = schema(r#"{"type":"string","minLength":2,"maxLength":4}"#);
        assert!(validate(&serde_json::json!("abc"), &s).is_ok());
        assert!(validate(&serde_json::json!("a"), &s).is_err());
        assert!(validate(&serde_json::json!("abcde"), &s).is_err());
        let s = schema(r#"{"type":"number","minimum":0,"maximum":10}"#);
        assert!(validate(&serde_json::json!(5), &s).is_ok());
        assert!(validate(&serde_json::json!(-1), &s).is_err());
        assert!(validate(&serde_json::json!(11), &s).is_err());
    }

    #[test]
    fn anyof_oneof_allof() {
        let any = schema(r#"{"anyOf":[{"type":"string"},{"type":"integer"}]}"#);
        assert!(validate(&serde_json::json!("x"), &any).is_ok());
        assert!(validate(&serde_json::json!(1), &any).is_ok());
        assert!(validate(&serde_json::json!(true), &any).is_err());
        let one = schema(r#"{"oneOf":[{"type":"integer"},{"type":"string"}]}"#);
        assert!(validate(&serde_json::json!(1), &one).is_ok());
        // A value matching neither → 0 passes → error.
        assert!(validate(&serde_json::json!(true), &one).is_err());
        let all = schema(r#"{"allOf":[{"type":"integer"},{"minimum":0}]}"#);
        assert!(validate(&serde_json::json!(3), &all).is_ok());
        assert!(validate(&serde_json::json!(-1), &all).is_err());
    }

    #[test]
    fn accepts_integer_when_number_has_no_fraction() {
        let s = schema(r#"{"type":"integer"}"#);
        assert!(validate(&serde_json::json!(7), &s).is_ok());
        assert!(validate(&serde_json::json!(7.5), &s).is_err());
    }

    #[test]
    fn multiple_errors_collected_at_once() {
        let s = schema(
            r#"{"type":"object","required":["a","b"],
            "properties":{"a":{"type":"integer"},"b":{"type":"string"}}}"#,
        );
        let bad = serde_json::json!({"a":"not-int","c":1});
        let errs = validate(&bad, &s).unwrap_err();
        // a wrong type + b missing — both reported, one retry can fix both.
        assert!(errs.iter().any(|e| e.path == "a"));
        assert!(errs
            .iter()
            .any(|e| e.path == "b" && e.message.contains("missing required")));
        assert!(!errs.iter().any(|e| e.path == "c")); // additionalProperties not set → c allowed
    }

    // ---- Tool-level behavior ----

    fn data_text(exec: ToolExecution) -> (bool, String) {
        let ToolExecution::Done(o) = exec else {
            panic!("expected Done");
        };
        let text = o
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        (o.is_error, text)
    }

    #[test]
    fn ok_output_echoes_validated_data() {
        asupersync::test_utils::run_test(|| async {
            let tool = StructuredOutputTool::with_retry_cap(5);
            let exec = tool
                .execute(
                    "c1",
                    serde_json::json!({
                        "schema": {"type":"object","required":["x"],"properties":{"x":{"type":"integer"}}},
                        "data": {"x": 42},
                        "name": "result"
                    }),
                    None,
                )
                .await
                .expect("execute");
            let (is_err, text) = data_text(exec);
            assert!(!is_err);
            assert!(text.contains("Validated structured output"));
            assert!(text.contains("name: result"));
            assert!(text.contains("\"x\": 42"));
        });
    }

    #[test]
    fn invalid_data_returns_error_with_path_and_increments_counter() {
        asupersync::test_utils::run_test(|| async {
            let tool = StructuredOutputTool::with_retry_cap(5);
            let exec = tool
                .execute(
                    "c1",
                    serde_json::json!({
                        "schema": {"type":"object","required":["x"],"properties":{"x":{"type":"integer"}}},
                        "data": {"x": "nope"}
                    }),
                    None,
                )
                .await
                .expect("execute");
            let (is_err, text) = data_text(exec);
            assert!(is_err);
            assert!(text.contains("failed schema validation"));
            assert!(text.contains("at `x`"));
            assert!(text.contains("1/5 failures"));
            assert_eq!(tool.failures.load(Ordering::Relaxed), 1);
        });
    }

    #[test]
    fn success_resets_failure_counter() {
        asupersync::test_utils::run_test(|| async {
            let tool = StructuredOutputTool::with_retry_cap(3);
            // Two failures.
            for _ in 0..2 {
                let _ = tool
                    .execute(
                        "c",
                        serde_json::json!({"schema": {"type":"integer"}, "data": "x"}),
                        None,
                    )
                    .await;
            }
            assert_eq!(tool.failures.load(Ordering::Relaxed), 2);
            // A success resets to 0 — the cap is a consecutive-ish budget.
            let exec = tool
                .execute(
                    "c",
                    serde_json::json!({"schema": {"type":"integer"}, "data": 1}),
                    None,
                )
                .await
                .expect("execute");
            assert!(!data_text(exec).0);
            assert_eq!(tool.failures.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn retry_cap_terminal_error_after_n_failures() {
        asupersync::test_utils::run_test(|| async {
            let tool = StructuredOutputTool::with_retry_cap(2);
            // First two failures validate + increment to 2.
            for _ in 0..2 {
                let _ = tool
                    .execute(
                        "c",
                        serde_json::json!({"schema": {"type":"integer"}, "data": "x"}),
                        None,
                    )
                    .await;
            }
            assert_eq!(tool.failures.load(Ordering::Relaxed), 2);
            // Third call hits the cap → terminal error, no validation.
            let exec = tool
                .execute(
                    "c",
                    serde_json::json!({"schema": {"type":"integer"}, "data": 1}),
                    None,
                )
                .await
                .expect("execute");
            let (is_err, text) = data_text(exec);
            assert!(is_err);
            assert!(text.contains("retry cap reached"));
            // The valid data was NOT accepted (counter unchanged at cap).
            assert_eq!(tool.failures.load(Ordering::Relaxed), 2);
        });
    }

    #[test]
    fn malformed_payload_is_error() {
        asupersync::test_utils::run_test(|| async {
            let tool = StructuredOutputTool::with_retry_cap(5);
            // `schema` must be present; omitting it fails deserialization.
            let exec = tool
                .execute("c", serde_json::json!({"data": 1}), None)
                .await
                .expect("execute");
            let (is_err, text) = data_text(exec);
            assert!(is_err);
            assert!(text.contains("invalid `structured_output` payload"));
        });
    }
}
