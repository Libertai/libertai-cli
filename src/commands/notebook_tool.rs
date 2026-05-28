//! Native `.ipynb` tools for `libertai code`.

use std::fs;
use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

const READ_NAME: &str = "notebook_read";
const EDIT_NAME: &str = "notebook_edit";
const MAX_READ_CHARS: usize = 24_000;

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

fn read_notebook(path: &str) -> Result<Value, String> {
    ensure_ipynb(path)?;
    let raw = fs::read_to_string(path).map_err(|e| format!("failed to read {path}: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("failed to parse notebook JSON in {path}: {e}"))
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
        if let Some(first) = outputs.and_then(|items| items.first()) {
            let preview = output_preview(first);
            if !preview.is_empty() {
                out.push_str("First output: ");
                out.push_str(&truncate_chars(&preview, 500));
                out.push('\n');
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
    for key in ["text", "ename", "name"] {
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
        for key in ["text/plain", "text/markdown"] {
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
                        { "output_type": "stream", "name": "stdout", "text": ["hello\n"] }
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
        assert!(summary.contains("Outputs: 1"));
        assert!(summary.contains("First output: hello"));
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
