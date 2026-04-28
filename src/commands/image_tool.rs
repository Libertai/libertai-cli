//! Pi `Tool` impl for the LibertAI image-generation API.
//!
//! Wraps the existing `post_image` client (`client.rs::post_image`) so an
//! agent can generate an image and save it to a workspace-relative
//! filename. Mirrors the chat-pillar `search_tool` shape — built once
//! per session, captures the libertai `Config` + workspace cwd at
//! registry-build time so subsequent on-disk config edits don't change
//! tool behavior mid-session.
//!
//! On filename collision the tool auto-suffixes (`logo-1.png`,
//! `logo-2.png`, …) via the existing `commands::image::numbered_path`
//! helper so an unattended agent never stalls waiting for a `force`
//! decision.
//!
//! The result is a JSON envelope the desktop renderer parses to find
//! `image_path` and render a thumbnail. The image bytes themselves are
//! *not* returned in the tool result — the agent can call `read(path)`
//! to feed them back into context, since pi's built-in `read` natively
//! returns image content blocks for image files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolOutput, ToolUpdate};

use crate::client::{post_image, ImageRequest};
use crate::commands::image::numbered_path;
use crate::config::Config;

const NAME: &str = "generate_image";
const LABEL: &str = "Generate image (LibertAI)";
const DESCRIPTION: &str = "Generate an image from a text prompt via LibertAI's image \
endpoint and save it to a workspace-relative filename. Returns a JSON envelope \
{ text, image_path, mime_type }. The image is NOT inlined in the tool result — \
to actually look at the result, call `read(\"<filename>\")` afterward; pi's \
read tool natively decodes image files so vision-capable models will see them. \
Filename must be a relative path under the working directory; collisions are \
auto-suffixed (logo.png → logo-1.png if logo.png already exists).";

const DEFAULT_SIZE: &str = "1024x1024";
const MAX_N: u32 = 4;

#[derive(Debug, Clone, Deserialize)]
struct ImageInput {
    prompt: String,
    /// Workspace-relative path (e.g. `"art/logo.png"`). Absolute paths
    /// and `..`-traversals are rejected.
    filename: String,
    /// e.g. "1024x1024", "1024x1792", "1792x1024". Default: 1024x1024.
    #[serde(default)]
    size: Option<String>,
    /// Number of images to generate (1..=4). Default 1.
    #[serde(default)]
    n: Option<u32>,
}

pub struct ImageGenTool {
    cfg: Arc<Config>,
    cwd: Arc<PathBuf>,
}

impl ImageGenTool {
    pub fn new(cfg: Arc<Config>, cwd: Arc<PathBuf>) -> Self {
        Self { cfg, cwd }
    }
}

#[async_trait]
impl Tool for ImageGenTool {
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
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Free-text description of the image to generate."
                },
                "filename": {
                    "type": "string",
                    "description": "Workspace-relative output path (e.g. \"art/logo.png\"). Absolute paths or ..-traversals are rejected. On collision the path is auto-suffixed."
                },
                "size": {
                    "type": "string",
                    "description": "Image size like \"1024x1024\", \"1024x1792\", or \"1792x1024\". Defaults to 1024x1024."
                },
                "n": {
                    "type": "integer",
                    "description": "How many images to generate (1..=4). Defaults to 1."
                }
            },
            "required": ["prompt", "filename"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolOutput> {
        let parsed: ImageInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `generate_image` payload: {e}"))),
        };

        let rel = parsed.filename.trim();
        if rel.is_empty() {
            return Ok(err_output("filename is empty"));
        }
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Ok(err_output(&format!(
                "filename must be relative, got {rel}"
            )));
        }
        if rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Ok(err_output(&format!(
                "filename must not contain `..` traversal, got {rel}"
            )));
        }

        let n = parsed.n.unwrap_or(1).clamp(1, MAX_N);
        let size = parsed
            .size
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_SIZE.to_string());

        let req = ImageRequest {
            model: self.cfg.default_image_model.clone(),
            prompt: parsed.prompt.clone(),
            size,
            n,
        };

        // post_image is blocking reqwest. Same caveat as search_tool —
        // pi's runtime accepts the brief block while the request is in
        // flight (typically a few seconds for image generation).
        let resp = match post_image(&self.cfg, &req) {
            Ok(r) => r,
            Err(e) => return Ok(err_output(&format!("image generation failed: {e:#}"))),
        };
        if resp.data.is_empty() {
            return Ok(err_output("server returned no images"));
        }

        // Resolve target path(s). For n>1 we mirror the CLI's
        // `numbered_path(rel, i)` scheme so the agent can predict file
        // names. Then auto-suffix any final collision so unattended runs
        // never fail on existing files.
        let mut written: Vec<WrittenImage> = Vec::with_capacity(resp.data.len());
        for (i, datum) in resp.data.iter().enumerate() {
            let base = if resp.data.len() == 1 {
                rel.to_string()
            } else {
                numbered_path(rel, i)
            };
            let bytes = match STANDARD.decode(&datum.b64_json) {
                Ok(b) => b,
                Err(e) => {
                    return Ok(err_output(&format!("decoding image #{i}: {e}")));
                }
            };
            let absolute = match resolve_writable_path(self.cwd.as_path(), &base) {
                Ok(p) => p,
                Err(e) => return Ok(err_output(&format!("resolving target: {e}"))),
            };
            if let Some(parent) = absolute.parent() {
                if !parent.exists() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return Ok(err_output(&format!(
                            "creating {}: {e}",
                            parent.display()
                        )));
                    }
                }
            }
            let final_path = avoid_collision(&absolute);
            if let Err(e) = std::fs::write(&final_path, &bytes) {
                return Ok(err_output(&format!(
                    "writing {}: {e}",
                    final_path.display()
                )));
            }
            let final_rel = final_path
                .strip_prefix(self.cwd.as_path())
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| base.clone());
            written.push(WrittenImage {
                absolute_path: final_path.to_string_lossy().into_owned(),
                relative_path: final_rel,
                bytes: bytes.len() as u64,
            });
        }

        let summary = if written.len() == 1 {
            format!(
                "saved {} ({} bytes)",
                written[0].relative_path, written[0].bytes
            )
        } else {
            let lines: Vec<String> = written
                .iter()
                .map(|w| format!("  - {} ({} bytes)", w.relative_path, w.bytes))
                .collect();
            format!("saved {} images:\n{}", written.len(), lines.join("\n"))
        };

        let envelope = if written.len() == 1 {
            json!({
                "text": summary,
                "image_path": written[0].absolute_path,
                "mime_type": "image/png",
            })
        } else {
            json!({
                "text": summary,
                "images": written.iter().map(|w| json!({
                    "image_path": w.absolute_path,
                    "mime_type": "image/png",
                })).collect::<Vec<_>>(),
            })
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(envelope.to_string()))],
            details: None,
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool {
        // Writes to disk under cwd. The factory registers this tool
        // outside the ApprovalTool wrapper (mirroring search/fetch), so
        // it's auto-trusted within a session — the file lands in the
        // workspace the user already opened.
        false
    }
}

struct WrittenImage {
    absolute_path: String,
    relative_path: String,
    bytes: u64,
}

fn err_output(msg: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
}

/// Join `rel` onto `cwd` and confirm the canonical-ish result stays
/// inside `cwd`. We don't `canonicalize` — the path doesn't exist yet.
/// Component-walking with `ParentDir` rejection is enough; absolute
/// paths and traversals were already rejected in `execute`.
fn resolve_writable_path(cwd: &Path, rel: &str) -> Result<PathBuf, String> {
    let joined = cwd.join(rel);
    if !joined.starts_with(cwd) {
        return Err(format!("{} escapes workspace root", joined.display()));
    }
    Ok(joined)
}

/// Walk `path-1.ext`, `path-2.ext`, … until we find a name that doesn't
/// exist. Bounded at 1000 attempts — beyond that we just return the
/// last candidate and let the write fail.
fn avoid_collision(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let target_str = target.to_string_lossy();
    for i in 1..1000 {
        let candidate = PathBuf::from(numbered_path(&target_str, i));
        if !candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(numbered_path(&target_str, 999))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn collision_suffixes() {
        let dir = env::temp_dir().join(format!("libertai-image-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("logo.png");
        std::fs::write(&target, b"x").unwrap();
        let next = avoid_collision(&target);
        assert_eq!(next.file_name().unwrap(), "logo-1.png");
        std::fs::remove_dir_all(&dir).ok();
    }
}
