//! Output-style registry for `libertai code`.
//!
//! Claude Code supports filesystem-defined response styles. We keep
//! built-ins for portability and also discover Markdown files
//! from project/user style directories.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    pub name: String,
    pub description: String,
    pub instruction: String,
}

const BUILTINS: &[(&str, &str)] = &[
    ("default", "Use the normal project response style."),
    (
        "concise",
        "Be concise. Prefer short, direct answers and only include detail needed to act.",
    ),
    (
        "explanatory",
        "Explain reasoning and tradeoffs clearly before giving final steps.",
    ),
    (
        "review",
        "Use code-review style: findings first, then assumptions, then a short summary.",
    ),
];

pub fn builtin_styles() -> Vec<OutputStyle> {
    BUILTINS
        .iter()
        .map(|(name, instruction)| OutputStyle {
            name: (*name).to_string(),
            description: (*instruction).to_string(),
            instruction: (*instruction).to_string(),
        })
        .collect()
}

pub fn load_styles(cwd: Option<&Path>) -> Vec<OutputStyle> {
    let mut out: BTreeMap<String, OutputStyle> = builtin_styles()
        .into_iter()
        .map(|style| (style.name.clone(), style))
        .collect();
    for dir in style_dirs(cwd) {
        load_dir(&dir, &mut out);
    }
    out.into_values().collect()
}

pub fn find_style(key: &str, cwd: Option<&Path>) -> Option<OutputStyle> {
    let key = normalize_name(key)?;
    load_styles(cwd)
        .into_iter()
        .find(|style| style.name.eq_ignore_ascii_case(&key))
}

pub fn apply_output_style(style: Option<&str>, prompt: &str, cwd: Option<&Path>) -> String {
    let Some(style) = style else {
        return prompt.to_string();
    };
    let Some(style) = find_style(style, cwd) else {
        return prompt.to_string();
    };
    if style.name == "default" {
        return prompt.to_string();
    }
    format!(
        "{prompt}\n\n[Session output style: {}. {}]",
        style.name, style.instruction
    )
}

fn style_dirs(cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".claude/output-styles"));
        dirs.push(home.join(".config/libertai/output-styles"));
    }
    if let Some(cwd) = cwd {
        dirs.push(cwd.join(".claude/output-styles"));
        dirs.push(cwd.join(".libertai/output-styles"));
    }
    dirs
}

fn load_dir(dir: &Path, out: &mut BTreeMap<String, OutputStyle>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).and_then(normalize_name) else {
            continue;
        };
        let (description, instruction) = parse_style_file(&text);
        out.insert(
            name.clone(),
            OutputStyle {
                name,
                description,
                instruction,
            },
        );
    }
}

fn parse_style_file(text: &str) -> (String, String) {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let frontmatter = &rest[..end];
            let body = rest[end + "\n---".len()..].trim();
            let description = frontmatter
                .lines()
                .find_map(|line| line.trim().strip_prefix("description:"))
                .map(|v| v.trim().trim_matches('"').to_string())
                .filter(|v| !v.is_empty());
            let instruction = if body.is_empty() { trimmed } else { body };
            return (
                description.unwrap_or_else(|| first_line(instruction)),
                instruction.to_string(),
            );
        }
    }
    (first_line(trimmed), trimmed.to_string())
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Custom output style")
        .to_string()
}

fn normalize_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_description_and_body() {
        let (description, instruction) =
            parse_style_file("---\ndescription: Findings first\n---\nLead with issues.");
        assert_eq!(description, "Findings first");
        assert_eq!(instruction, "Lead with issues.");
    }

    #[test]
    fn loads_project_output_style_files() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".claude/output-styles");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("terse.md"),
            "---\ndescription: Very short\n---\nAnswer in one paragraph.",
        )
        .unwrap();

        let style = find_style("terse", Some(temp.path())).unwrap();
        assert_eq!(style.description, "Very short");
        assert_eq!(style.instruction, "Answer in one paragraph.");
    }
}
