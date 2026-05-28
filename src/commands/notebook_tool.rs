//! Native `.ipynb` tools for `libertai code`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

const READ_NAME: &str = "notebook_read";
const EDIT_NAME: &str = "notebook_edit";
const EXECUTE_NAME: &str = "notebook_execute";
const MAX_READ_CHARS: usize = 24_000;
const DEFAULT_EXECUTE_TIMEOUT_SECS: u64 = 120;
const MAX_EXECUTE_TIMEOUT_SECS: u64 = 900;

#[derive(Debug, Clone, Deserialize)]
struct NotebookReadInput {
    path: String,
    cell_index: Option<usize>,
    max_chars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EditMode {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Clone, Deserialize)]
struct NotebookEditInput {
    path: String,
    cell_index: usize,
    mode: Option<EditMode>,
    source: Option<String>,
    cell_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NotebookExecuteInput {
    path: String,
    timeout_seconds: Option<u64>,
    max_chars: Option<usize>,
}

pub struct NotebookReadTool;

impl NotebookReadTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NotebookReadTool {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NotebookEditTool;

impl NotebookEditTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NotebookEditTool {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NotebookExecuteTool;

impl NotebookExecuteTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NotebookExecuteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for NotebookReadTool {
    fn name(&self) -> &str {
        READ_NAME
    }

    fn label(&self) -> &str {
        "Read Jupyter notebook"
    }

    fn description(&self) -> &str {
        "Read a local .ipynb notebook and return a compact cell-by-cell summary. \
Use this instead of raw JSON reads when inspecting notebooks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to a local .ipynb file." },
                "cell_index": { "type": "integer", "minimum": 0, "description": "Optional zero-based cell index to read." },
                "max_chars": { "type": "integer", "minimum": 1000, "maximum": 50000, "description": "Optional output cap; defaults to 24000 characters." }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: NotebookReadInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `notebook_read` payload: {e}"))),
        };

        let max_chars = parsed.max_chars.unwrap_or(MAX_READ_CHARS).clamp(1_000, 50_000);
        let notebook = match read_notebook(&parsed.path) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&e)),
        };
        let summary = match summarize_notebook(&parsed.path, &notebook, parsed.cell_index, max_chars) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&e)),
        };
        Ok(text_output(&summary, false))
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        EDIT_NAME
    }

    fn label(&self) -> &str {
        "Edit Jupyter notebook"
    }

    fn description(&self) -> &str {
        "Edit a local .ipynb notebook cell. Supports replacing, inserting, or deleting \
cells while preserving the rest of the notebook JSON."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to a local .ipynb file." },
                "cell_index": { "type": "integer", "minimum": 0, "description": "Zero-based cell index. Insert mode inserts before this index; index equal to cell count appends." },
                "mode": { "type": "string", "enum": ["replace", "insert", "delete"], "description": "Edit mode. Defaults to replace." },
                "source": { "type": "string", "description": "New cell source for replace/insert mode." },
                "cell_type": { "type": "string", "enum": ["code", "markdown", "raw"], "description": "Cell type for replace/insert mode. Defaults to the existing cell type on replace, or code on insert." }
            },
            "required": ["path", "cell_index"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: NotebookEditInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `notebook_edit` payload: {e}"))),
        };

        let result = match edit_notebook_file(&parsed) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&e)),
        };
        Ok(text_output(&result, false))
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

#[async_trait]
impl Tool for NotebookExecuteTool {
    fn name(&self) -> &str {
        EXECUTE_NAME
    }

    fn label(&self) -> &str {
        "Execute Jupyter notebook"
    }

    fn description(&self) -> &str {
        "Execute a local .ipynb notebook in place using the system `jupyter` CLI, \
then return a compact cell/output summary. This mutates notebook outputs and should be approval-gated."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to a local .ipynb file to execute in place." },
                "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 900, "description": "Execution timeout. Defaults to 120 seconds; capped at 900." },
                "max_chars": { "type": "integer", "minimum": 1000, "maximum": 50000, "description": "Optional summary output cap after execution; defaults to 24000 characters." }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: NotebookExecuteInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `notebook_execute` payload: {e}"))),
        };
        let max_chars = parsed.max_chars.unwrap_or(MAX_READ_CHARS).clamp(1_000, 50_000);
        let timeout = parsed
            .timeout_seconds
            .unwrap_or(DEFAULT_EXECUTE_TIMEOUT_SECS)
            .clamp(1, MAX_EXECUTE_TIMEOUT_SECS);

        let report = match execute_notebook_file(&parsed.path, timeout) {
            Ok(report) => report,
            Err(e) => return Ok(err_output(&e)),
        };
        let notebook = match read_notebook(&parsed.path) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&e)),
        };
        let summary = match summarize_notebook(&parsed.path, &notebook, None, max_chars) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&e)),
        };
        Ok(text_output(&format!("{report}\n\n{summary}"), false))
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

fn read_notebook(path: &str) -> Result<Value, String> {
    ensure_ipynb(path)?;
    let raw = fs::read_to_string(path).map_err(|e| format!("failed to read {path}: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("failed to parse notebook JSON in {path}: {e}"))
}

fn execute_notebook_file(path: &str, timeout_seconds: u64) -> Result<String, String> {
    ensure_ipynb(path)?;
    let notebook_path = Path::new(path);
    if !notebook_path.is_file() {
        return Err(format!("notebook does not exist: {path}"));
    }
    let notebook_path = notebook_path
        .canonicalize()
        .map_err(|e| format!("failed to resolve notebook path {path}: {e}"))?;
    let args = jupyter_execute_args(notebook_path.to_string_lossy().as_ref(), timeout_seconds);
    let mut command = Command::new("jupyter");
    command.args(&args);
    if let Some(parent) = notebook_path.parent() {
        command.current_dir(parent);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start `jupyter nbconvert`: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("failed to collect notebook execution output: {e}"))?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !output.status.success() {
                    return Err(format!(
                        "notebook execution failed with status {}\nstdout:\n{}\nstderr:\n{}",
                        output.status,
                        truncate_output(&stdout),
                        truncate_output(&stderr)
                    ));
                }
                return Ok(format!(
                    "executed notebook in place: {path}\nstdout:\n{}\nstderr:\n{}",
                    truncate_output(&stdout),
                    truncate_output(&stderr)
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "notebook execution timed out after {timeout_seconds}s: {path}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("failed while waiting for notebook execution: {e}")),
        }
    }
}

fn jupyter_execute_args(path: &str, timeout_seconds: u64) -> Vec<String> {
    vec![
        "nbconvert".to_string(),
        "--to".to_string(),
        "notebook".to_string(),
        "--execute".to_string(),
        "--inplace".to_string(),
        format!("--ExecutePreprocessor.timeout={timeout_seconds}"),
        path.to_string(),
    ]
}

fn truncate_output(text: &str) -> String {
    const MAX_OUTPUT_CHARS: usize = 4_000;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_OUTPUT_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_OUTPUT_CHARS).collect();
    out.push_str("\n...(truncated)");
    out
}

fn edit_notebook_file(input: &NotebookEditInput) -> Result<String, String> {
    ensure_ipynb(&input.path)?;
    let mut notebook = read_notebook(&input.path)?;
    let mode = input.mode.clone().unwrap_or(EditMode::Replace);
    let cells = notebook
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "notebook is missing a top-level `cells` array".to_string())?;

    match mode {
        EditMode::Replace => {
            if input.cell_index >= cells.len() {
                return Err(format!(
                    "cell_index {} is out of range; notebook has {} cell(s)",
                    input.cell_index,
                    cells.len()
                ));
            }
            let fallback_type = cells[input.cell_index]
                .get("cell_type")
                .and_then(Value::as_str)
                .unwrap_or("code")
                .to_string();
            let cell_type = input.cell_type.as_deref().unwrap_or(&fallback_type);
            let source = input
                .source
                .as_deref()
                .ok_or_else(|| "`source` is required for replace mode".to_string())?;
            replace_cell_source(&mut cells[input.cell_index], cell_type, source)?;
        }
        EditMode::Insert => {
            if input.cell_index > cells.len() {
                return Err(format!(
                    "cell_index {} is out of range for insert; notebook has {} cell(s)",
                    input.cell_index,
                    cells.len()
                ));
            }
            let cell_type = input.cell_type.as_deref().unwrap_or("code");
            let source = input
                .source
                .as_deref()
                .ok_or_else(|| "`source` is required for insert mode".to_string())?;
            cells.insert(input.cell_index, new_cell(cell_type, source)?);
        }
        EditMode::Delete => {
            if input.cell_index >= cells.len() {
                return Err(format!(
                    "cell_index {} is out of range; notebook has {} cell(s)",
                    input.cell_index,
                    cells.len()
                ));
            }
            cells.remove(input.cell_index);
        }
    }

    let out = serde_json::to_string_pretty(&notebook)
        .map_err(|e| format!("failed to serialize notebook: {e}"))?;
    fs::write(&input.path, format!("{out}\n"))
        .map_err(|e| format!("failed to write {}: {e}", input.path))?;

    let mode_label = match mode {
        EditMode::Replace => "replaced",
        EditMode::Insert => "inserted",
        EditMode::Delete => "deleted",
    };
    Ok(format!(
        "{mode_label} cell {} in {}",
        input.cell_index, input.path
    ))
}

fn ensure_ipynb(path: &str) -> Result<(), String> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str());
    if ext == Some("ipynb") {
        Ok(())
    } else {
        Err(format!("notebook tools only accept .ipynb files: {path}"))
    }
}

fn summarize_notebook(
    path: &str,
    notebook: &Value,
    cell_index: Option<usize>,
    max_chars: usize,
) -> Result<String, String> {
    let cells = notebook
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| "notebook is missing a top-level `cells` array".to_string())?;

    let mut out = String::new();
    out.push_str(&format!("# {path}\n{} cell(s)\n", cells.len()));

    if let Some(index) = cell_index {
        let cell = cells.get(index).ok_or_else(|| {
            format!(
                "cell_index {index} is out of range; notebook has {} cell(s)",
                cells.len()
            )
        })?;
        push_cell_summary(&mut out, index, cell);
    } else {
        for (index, cell) in cells.iter().enumerate() {
            if out.chars().count() >= max_chars {
                break;
            }
            push_cell_summary(&mut out, index, cell);
        }
    }

    Ok(truncate_chars(&out, max_chars))
}

fn push_cell_summary(out: &mut String, index: usize, cell: &Value) {
    let cell_type = cell
        .get("cell_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    out.push_str(&format!("\n## Cell {index} - {cell_type}\n"));
    let source = source_to_string(cell.get("source"));
    if source.trim().is_empty() {
        out.push_str("[empty source]\n");
    } else {
        out.push_str(source.trim_end());
        out.push('\n');
    }

    if cell_type == "code" {
        let outputs = cell.get("outputs").and_then(Value::as_array);
        let count = outputs.map_or(0, Vec::len);
        out.push_str(&format!("Outputs: {count}\n"));
        if let Some(items) = outputs {
            for (output_index, output) in items.iter().take(5).enumerate() {
                let preview = output_preview(output);
                out.push_str(&format!(
                    "- Output {output_index}: {}",
                    output_label(output)
                ));
                if !preview.is_empty() {
                    out.push_str(" - ");
                    out.push_str(&truncate_chars(&preview, 500));
                }
                out.push('\n');
            }
            if items.len() > 5 {
                out.push_str(&format!("- ... {} more output(s)\n", items.len() - 5));
            }
        }
    }
}

fn source_to_string(source: Option<&Value>) -> String {
    match source {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn output_preview(output: &Value) -> String {
    if let Some(output_type) = output.get("output_type").and_then(Value::as_str) {
        if output_type == "error" {
            let ename = output.get("ename").and_then(Value::as_str).unwrap_or("error");
            let evalue = output.get("evalue").and_then(Value::as_str).unwrap_or("");
            let traceback = output
                .get("traceback")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<String>())
                .unwrap_or_default();
            return [ename, evalue, traceback.as_str()]
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(": ");
        }
    }
    for key in ["text", "ename", "evalue", "name"] {
        if let Some(s) = output.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
        if let Some(items) = output.get(key).and_then(Value::as_array) {
            let text: String = items.iter().filter_map(Value::as_str).collect();
            if !text.is_empty() {
                return text;
            }
        }
    }
    if let Some(data) = output.get("data").and_then(Value::as_object) {
        for key in ["text/plain", "text/markdown", "text/html", "application/json"] {
            if let Some(value) = data.get(key) {
                let text = source_to_string(Some(value));
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    String::new()
}

fn output_label(output: &Value) -> String {
    let output_type = output
        .get("output_type")
        .and_then(Value::as_str)
        .unwrap_or("output");
    match output_type {
        "stream" => {
            let name = output.get("name").and_then(Value::as_str).unwrap_or("stream");
            format!("stream/{name}")
        }
        "display_data" | "execute_result" => {
            let data = output.get("data").and_then(Value::as_object);
            let mimes = data
                .map(|data| {
                    let mut keys = data.keys().cloned().collect::<Vec<_>>();
                    keys.sort();
                    keys.join(", ")
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "no mime data".to_string());
            format!("{output_type} [{mimes}]")
        }
        "error" => {
            let ename = output.get("ename").and_then(Value::as_str).unwrap_or("error");
            format!("error/{ename}")
        }
        other => other.to_string(),
    }
}

fn replace_cell_source(cell: &mut Value, cell_type: &str, source: &str) -> Result<(), String> {
    validate_cell_type(cell_type)?;
    let object = cell
        .as_object_mut()
        .ok_or_else(|| "target cell is not a JSON object".to_string())?;
    object.insert("cell_type".to_string(), Value::String(cell_type.to_string()));
    object.insert("source".to_string(), Value::Array(source_lines(source)));
    object
        .entry("metadata".to_string())
        .or_insert_with(|| json!({}));
    if cell_type == "code" {
        object
            .entry("execution_count".to_string())
            .or_insert(Value::Null);
        object.entry("outputs".to_string()).or_insert_with(|| json!([]));
    }
    Ok(())
}

fn new_cell(cell_type: &str, source: &str) -> Result<Value, String> {
    validate_cell_type(cell_type)?;
    let mut cell = json!({
        "cell_type": cell_type,
        "metadata": {},
        "source": source_lines(source),
    });
    if cell_type == "code" {
        let object = cell.as_object_mut().expect("new cell is an object");
        object.insert("execution_count".to_string(), Value::Null);
        object.insert("outputs".to_string(), json!([]));
    }
    Ok(cell)
}

fn validate_cell_type(cell_type: &str) -> Result<(), String> {
    match cell_type {
        "code" | "markdown" | "raw" => Ok(()),
        other => Err(format!("unsupported cell_type `{other}`; use code, markdown, or raw")),
    }
}

fn source_lines(source: &str) -> Vec<Value> {
    if source.is_empty() {
        return Vec::new();
    }
    source
        .split_inclusive('\n')
        .map(|line| Value::String(line.to_string()))
        .collect()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut head: String = text.chars().take(max_chars).collect();
        head.push_str("\n\n[truncated]");
        head
    }
}

fn text_output(text: &str, is_error: bool) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        details: None,
        is_error,
    }
    .into()
}

fn err_output(msg: &str) -> ToolExecution {
    text_output(msg, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_notebook() -> Value {
        json!({
            "cells": [
                {
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": ["# Title\n", "Some text"]
                },
                {
                    "cell_type": "code",
                    "execution_count": 1,
                    "metadata": {},
                    "outputs": [
                        { "output_type": "stream", "name": "stdout", "text": ["hello\n"] },
                        {
                            "output_type": "execute_result",
                            "execution_count": 1,
                            "data": {
                                "text/plain": ["2"],
                                "text/html": ["<b>2</b>"]
                            },
                            "metadata": {}
                        },
                        {
                            "output_type": "error",
                            "ename": "ValueError",
                            "evalue": "bad value",
                            "traceback": ["ValueError: bad value"]
                        }
                    ],
                    "source": ["print('hello')\n"]
                }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        })
    }

    #[test]
    fn summarize_all_cells() {
        let summary = summarize_notebook("demo.ipynb", &sample_notebook(), None, 10_000).unwrap();
        assert!(summary.contains("# demo.ipynb"));
        assert!(summary.contains("## Cell 0 - markdown"));
        assert!(summary.contains("## Cell 1 - code"));
        assert!(summary.contains("Outputs: 3"));
        assert!(summary.contains("Output 0: stream/stdout"));
        assert!(summary.contains("Output 1: execute_result [text/html, text/plain]"));
        assert!(summary.contains("Output 2: error/ValueError"));
        assert!(summary.contains("ValueError: bad value"));
    }

    #[test]
    fn summarize_single_cell_checks_range() {
        let err = summarize_notebook("demo.ipynb", &sample_notebook(), Some(3), 10_000)
            .unwrap_err();
        assert!(err.contains("out of range"));
    }

    #[test]
    fn replace_cell_source_preserves_cell_object() {
        let mut notebook = sample_notebook();
        let cell = notebook
            .get_mut("cells")
            .and_then(Value::as_array_mut)
            .unwrap()
            .get_mut(1)
            .unwrap();
        replace_cell_source(cell, "code", "x = 1\nx").unwrap();

        assert_eq!(cell.get("cell_type").and_then(Value::as_str), Some("code"));
        assert_eq!(source_to_string(cell.get("source")), "x = 1\nx");
        assert!(cell.get("outputs").is_some());
    }

    #[test]
    fn new_markdown_cell_has_expected_shape() {
        let cell = new_cell("markdown", "hello\nworld").unwrap();
        assert_eq!(cell.get("cell_type").and_then(Value::as_str), Some("markdown"));
        assert_eq!(source_to_string(cell.get("source")), "hello\nworld");
        assert!(cell.get("outputs").is_none());
    }

    #[test]
    fn rejects_non_notebook_paths() {
        let err = ensure_ipynb("notes.json").unwrap_err();
        assert!(err.contains(".ipynb"));
    }

    #[test]
    fn jupyter_execute_args_include_inplace_timeout_and_path() {
        assert_eq!(
            jupyter_execute_args("demo.ipynb", 45),
            vec![
                "nbconvert",
                "--to",
                "notebook",
                "--execute",
                "--inplace",
                "--ExecutePreprocessor.timeout=45",
                "demo.ipynb",
            ]
        );
    }

    #[test]
    fn truncate_output_caps_long_text() {
        let long = "a".repeat(4_100);
        let truncated = truncate_output(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with("...(truncated)"));
    }

    #[test]
    fn edit_notebook_file_replaces_inserts_and_deletes() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "libertai-notebook-test-{}-{}.ipynb",
            std::process::id(),
            "edit"
        ));
        fs::write(&path, serde_json::to_string_pretty(&sample_notebook()).unwrap()).unwrap();

        let path_str = path.to_string_lossy().to_string();
        edit_notebook_file(&NotebookEditInput {
            path: path_str.clone(),
            cell_index: 0,
            mode: Some(EditMode::Replace),
            source: Some("## Updated\n".to_string()),
            cell_type: Some("markdown".to_string()),
        })
        .unwrap();
        edit_notebook_file(&NotebookEditInput {
            path: path_str.clone(),
            cell_index: 1,
            mode: Some(EditMode::Insert),
            source: Some("print(42)\n".to_string()),
            cell_type: Some("code".to_string()),
        })
        .unwrap();
        edit_notebook_file(&NotebookEditInput {
            path: path_str.clone(),
            cell_index: 2,
            mode: Some(EditMode::Delete),
            source: None,
            cell_type: None,
        })
        .unwrap();

        let edited = read_notebook(&path_str).unwrap();
        let cells = edited.get("cells").and_then(Value::as_array).unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(source_to_string(cells[0].get("source")), "## Updated\n");
        assert_eq!(source_to_string(cells[1].get("source")), "print(42)\n");

        let _ = fs::remove_file(path);
    }
}
