//! Tool-call loop guardrails for `libertai code`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

const EXACT_WARN_AT: usize = 3;
const EXACT_HALT_AT: usize = 5;
const SAME_TOOL_WARN_AT: usize = 6;
const SAME_TOOL_HALT_AT: usize = 10;
const SAME_RESULT_WARN_AT: usize = 3;
const SAME_RESULT_HALT_AT: usize = 5;
const RECENT_LIMIT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GuardrailDecision {
    Allow,
    Warn(String),
    Halt(String),
}

#[derive(Debug, Clone)]
struct RecentCall {
    tool: String,
    args: String,
}

#[derive(Debug, Default)]
pub struct ToolGuardrailState {
    recent_calls: VecDeque<RecentCall>,
    recent_results: VecDeque<String>,
}

impl ToolGuardrailState {
    pub fn shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    fn before_call(&mut self, tool: &str, input: &Value) -> GuardrailDecision {
        let args = canonical_json(input);
        self.recent_calls.push_back(RecentCall {
            tool: tool.to_string(),
            args: args.clone(),
        });
        trim_deque(&mut self.recent_calls, RECENT_LIMIT);

        let exact = trailing_exact_count(&self.recent_calls, tool, &args);
        if exact >= EXACT_HALT_AT {
            return GuardrailDecision::Halt(format!(
                "tool-call guardrail halted repeated `{tool}` call after {exact} identical attempts; change strategy or ask the user before retrying"
            ));
        }
        if exact >= EXACT_WARN_AT {
            return GuardrailDecision::Warn(format!(
                "tool-call guardrail warning: `{tool}` has been called {exact} times in a row with identical arguments; avoid retry loops unless new information is available"
            ));
        }

        let same_tool = trailing_tool_count(&self.recent_calls, tool);
        if same_tool >= SAME_TOOL_HALT_AT {
            return GuardrailDecision::Halt(format!(
                "tool-call guardrail halted `{tool}` after {same_tool} consecutive calls; summarize what you learned and choose a different tool or ask the user"
            ));
        }
        if same_tool >= SAME_TOOL_WARN_AT {
            return GuardrailDecision::Warn(format!(
                "tool-call guardrail warning: `{tool}` has been called {same_tool} consecutive times; consider summarizing progress or switching tactics"
            ));
        }

        GuardrailDecision::Allow
    }

    fn after_result(&mut self, output: &ToolOutput) -> GuardrailDecision {
        if output.is_error {
            return GuardrailDecision::Allow;
        }
        let hash = output_fingerprint(output);
        if hash.is_empty() {
            return GuardrailDecision::Allow;
        }
        self.recent_results.push_back(hash.clone());
        trim_deque(&mut self.recent_results, RECENT_LIMIT);

        let repeats = trailing_result_count(&self.recent_results, &hash);
        if repeats >= SAME_RESULT_HALT_AT {
            return GuardrailDecision::Halt(format!(
                "tool-call guardrail halted after {repeats} consecutive tools returned the same result; stop retrying and use the existing result"
            ));
        }
        if repeats >= SAME_RESULT_WARN_AT {
            return GuardrailDecision::Warn(format!(
                "tool-call guardrail warning: {repeats} consecutive tools returned the same result; reuse what is already known unless a different query is needed"
            ));
        }

        GuardrailDecision::Allow
    }
}

pub struct GuardrailTool {
    inner: Box<dyn Tool>,
    state: Arc<Mutex<ToolGuardrailState>>,
}

impl GuardrailTool {
    pub fn new(inner: Box<dyn Tool>, state: Arc<Mutex<ToolGuardrailState>>) -> Self {
        Self { inner, state }
    }
}

#[async_trait]
impl Tool for GuardrailTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let before = {
            let mut state = self.state.lock().expect("tool guardrail mutex poisoned");
            state.before_call(self.inner.name(), &input)
        };
        if let GuardrailDecision::Halt(reason) = before {
            return Ok(guardrail_output(&reason, true).into());
        }

        let mut execution = self.inner.execute(tool_call_id, input, on_update).await?;
        if let Some(output) = execution_output_mut(&mut execution) {
            if let GuardrailDecision::Warn(warning) = before {
                prepend_warning(output, &warning);
            }

            let after = {
                let mut state = self.state.lock().expect("tool guardrail mutex poisoned");
                state.after_result(output)
            };
            match after {
                GuardrailDecision::Allow => {}
                GuardrailDecision::Warn(warning) => prepend_warning(output, &warning),
                GuardrailDecision::Halt(reason) => return Ok(guardrail_output(&reason, true).into()),
            }
        }
        Ok(execution)
    }

    async fn resume(
        &self,
        tool_call_id: &str,
        request_id: &str,
        payload: Value,
    ) -> PiResult<ToolExecution> {
        let mut execution = self.inner.resume(tool_call_id, request_id, payload).await?;
        if let Some(output) = execution_output_mut(&mut execution) {
            let after = {
                let mut state = self.state.lock().expect("tool guardrail mutex poisoned");
                state.after_result(output)
            };
            match after {
                GuardrailDecision::Allow => {}
                GuardrailDecision::Warn(warning) => prepend_warning(output, &warning),
                GuardrailDecision::Halt(reason) => return Ok(guardrail_output(&reason, true).into()),
            }
        }
        Ok(execution)
    }
}

fn execution_output_mut(execution: &mut ToolExecution) -> Option<&mut ToolOutput> {
    match execution {
        ToolExecution::Done(output) => Some(output),
        ToolExecution::Paused { .. } => None,
    }
}

fn prepend_warning(output: &mut ToolOutput, warning: &str) {
    output
        .content
        .insert(0, ContentBlock::Text(TextContent::new(format!("{warning}\n"))));
}

fn guardrail_output(reason: &str, is_error: bool) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(reason.to_string()))],
        details: Some(json!({ "guardrail": "tool_loop" })),
        is_error,
    }
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(v) => serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(items) => {
            let inner = items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            let inner = entries
                .into_iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    format!("{key}:{}", canonical_json(value))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}

fn output_fingerprint(output: &ToolOutput) -> String {
    let text = output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    canonical_text(&text)
}

fn canonical_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trailing_exact_count(calls: &VecDeque<RecentCall>, tool: &str, args: &str) -> usize {
    calls
        .iter()
        .rev()
        .take_while(|call| call.tool == tool && call.args == args)
        .count()
}

fn trailing_tool_count(calls: &VecDeque<RecentCall>, tool: &str) -> usize {
    calls.iter().rev().take_while(|call| call.tool == tool).count()
}

fn trailing_result_count(results: &VecDeque<String>, hash: &str) -> usize {
    results.iter().rev().take_while(|item| *item == hash).count()
}

fn trim_deque<T>(deque: &mut VecDeque<T>, max_len: usize) {
    while deque.len() > max_len {
        deque.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str) -> Value {
        json!({ "path": path })
    }

    #[test]
    fn canonical_json_sorts_object_keys() {
        let a = json!({ "b": 2, "a": [true, null] });
        let b = json!({ "a": [true, null], "b": 2 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn warns_then_halts_repeated_exact_calls() {
        let mut state = ToolGuardrailState::default();
        assert_eq!(state.before_call("read", &input("a")), GuardrailDecision::Allow);
        assert_eq!(state.before_call("read", &input("a")), GuardrailDecision::Allow);
        assert!(matches!(
            state.before_call("read", &input("a")),
            GuardrailDecision::Warn(_)
        ));
        assert!(matches!(
            state.before_call("read", &input("a")),
            GuardrailDecision::Warn(_)
        ));
        assert!(matches!(
            state.before_call("read", &input("a")),
            GuardrailDecision::Halt(_)
        ));
    }

    #[test]
    fn same_tool_repeats_ignore_interleaved_tools() {
        let mut state = ToolGuardrailState::default();
        for i in 0..5 {
            assert_eq!(
                state.before_call("grep", &json!({ "pattern": i })),
                GuardrailDecision::Allow
            );
        }
        assert!(matches!(
            state.before_call("grep", &json!({ "pattern": 5 })),
            GuardrailDecision::Warn(_)
        ));
        assert_eq!(
            state.before_call("read", &input("x")),
            GuardrailDecision::Allow
        );
        assert_eq!(
            state.before_call("grep", &json!({ "pattern": 6 })),
            GuardrailDecision::Allow
        );
    }

    #[test]
    fn warns_then_halts_repeated_results() {
        let mut state = ToolGuardrailState::default();
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("same\n result"))],
            details: None,
            is_error: false,
        };
        assert_eq!(state.after_result(&output), GuardrailDecision::Allow);
        assert_eq!(state.after_result(&output), GuardrailDecision::Allow);
        assert!(matches!(
            state.after_result(&output),
            GuardrailDecision::Warn(_)
        ));
        assert!(matches!(
            state.after_result(&output),
            GuardrailDecision::Warn(_)
        ));
        assert!(matches!(
            state.after_result(&output),
            GuardrailDecision::Halt(_)
        ));
    }
}
