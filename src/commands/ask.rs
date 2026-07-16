//! `libertai ask` — one-shot prompt, non-streaming.
//!
//! Output contract: when stdout is a terminal the answer is rendered as
//! markdown (rich_rust via pi's console — honours NO_COLOR and terminal
//! width); when stdout is piped/redirected the raw model text is printed
//! unchanged so `libertai ask ... | jq`-style scripting keeps working.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;

use crate::client::{post_chat_blocking, ChatMessage, ChatRequest, ContentPart, MessageContent};
use crate::commands::chat_render::markdown_enabled_stdout;
use crate::config::load;

/// Turn an `--image` argument into an OpenAI `image_url` value: remote URLs
/// pass through untouched (fetching is the server's job), local paths are
/// inlined as base64 data URLs with the mime taken from the extension.
fn image_content_url(arg: &str) -> Result<String> {
    if arg.starts_with("http://") || arg.starts_with("https://") {
        return Ok(arg.to_string());
    }
    let path = std::path::Path::new(arg);
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => bail!("unsupported image type: {arg} (expected .png, .jpg, .jpeg, .gif or .webp)"),
    };
    let bytes = std::fs::read(path).with_context(|| format!("reading image {arg}"))?;
    Ok(format!(
        "data:{mime};base64,{}",
        BASE64_STANDARD.encode(bytes)
    ))
}

/// User message content: plain text when no images (keeps the historical
/// wire shape), otherwise a parts array — text first, then images in the
/// order they were given on the command line.
fn build_user_content(prompt: String, images: &[String]) -> Result<MessageContent> {
    if images.is_empty() {
        return Ok(MessageContent::Text(prompt));
    }
    let mut parts = vec![ContentPart::text(prompt)];
    for img in images {
        parts.push(ContentPart::image_url(image_content_url(img)?));
    }
    Ok(MessageContent::Parts(parts))
}

/// Fail fast when the catalog says the model has no vision capability.
/// `None` (catalog missing or model unknown) sends anyway — the API is the
/// authority and will reject if it must.
fn check_vision_support(model: &str, vision: Option<bool>) -> Result<()> {
    if vision == Some(false) {
        bail!(
            "model {model} does not support vision (images); \
             pick a vision-capable model with `libertai models`"
        );
    }
    Ok(())
}

fn catalog_vision(model: &str) -> Option<bool> {
    let catalog = crate::commands::model_catalog::load()?;
    let caps = catalog.find_text(model)?.text_capabilities()?;
    Some(caps.vision)
}

pub fn run(prompt: String, model: Option<String>, images: Vec<String>) -> Result<()> {
    let cfg = load()?;
    let model = model.unwrap_or_else(|| cfg.default_chat_model.clone());

    if !images.is_empty() {
        check_vision_support(&model, catalog_vision(&model))?;
    }

    let req = ChatRequest {
        model,
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: build_user_content(prompt, &images)?,
        }],
        stream: Some(false),
        max_tokens: None,
    };

    let resp = post_chat_blocking(&cfg, &req)?;
    let body: serde_json::Value = resp
        .json()
        .context("parsing /v1/chat/completions response")?;

    let content = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .context("response missing choices[0].message.content")?;

    if markdown_enabled_stdout() {
        // TTY: pretty markdown. render_markdown guarantees a trailing
        // newline of its own.
        pi::tui::PiConsole::new().render_markdown(content);
    } else {
        print!("{}", raw_output(content));
    }
    Ok(())
}

/// Piped/non-TTY form of the answer: the model text byte-for-byte with
/// exactly one trailing newline appended when missing — never any ANSI.
/// This is the scriptability contract pinned by `tests/probes_chat_ask.rs`
/// and the unit tests below.
fn raw_output(content: &str) -> String {
    if content.ends_with('\n') {
        content.to_string()
    } else {
        format!("{content}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::{build_user_content, image_content_url, raw_output};
    use crate::client::MessageContent;

    #[test]
    fn image_content_url_passes_remote_urls_through() {
        assert_eq!(
            image_content_url("https://example.com/cat.jpg").unwrap(),
            "https://example.com/cat.jpg"
        );
        assert_eq!(
            image_content_url("http://example.com/cat.jpg").unwrap(),
            "http://example.com/cat.jpg"
        );
    }

    #[test]
    fn image_content_url_encodes_local_file_as_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).unwrap();
        let url = image_content_url(path.to_str().unwrap()).unwrap();
        assert_eq!(url, "data:image/png;base64,iVBORw==");
    }

    #[test]
    fn image_content_url_maps_jpg_extension_to_jpeg_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.JPG");
        std::fs::write(&path, [0xff, 0xd8]).unwrap();
        let url = image_content_url(path.to_str().unwrap()).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "got: {url}");
    }

    #[test]
    fn image_content_url_rejects_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"hi").unwrap();
        let err = image_content_url(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("unsupported image type"),
            "got: {err}"
        );
    }

    #[test]
    fn image_content_url_errors_on_missing_file() {
        let err = image_content_url("/nonexistent/cat.png").unwrap_err();
        assert!(err.to_string().contains("cat.png"), "got: {err}");
    }

    #[test]
    fn vision_gate_rejects_model_known_to_lack_vision() {
        let err = super::check_vision_support("some-text-model", Some(false)).unwrap_err();
        assert!(err.to_string().contains("some-text-model"), "got: {err}");
        assert!(err.to_string().contains("vision"), "got: {err}");
    }

    #[test]
    fn vision_gate_allows_vision_models_and_unknown_models() {
        super::check_vision_support("qwen-vl", Some(true)).unwrap();
        // Catalog missing or model unknown: let the API decide.
        super::check_vision_support("mystery-model", None).unwrap();
    }

    #[test]
    fn build_user_content_without_images_stays_plain_text() {
        let content = build_user_content("hi".to_string(), &[]).unwrap();
        assert!(matches!(content, MessageContent::Text(ref s) if s == "hi"));
    }

    #[test]
    fn build_user_content_with_images_is_text_part_then_images_in_order() {
        let content = build_user_content(
            "compare".to_string(),
            &[
                "https://a.example/1.png".to_string(),
                "https://a.example/2.png".to_string(),
            ],
        )
        .unwrap();
        let v = serde_json::to_value(&content).unwrap();
        assert_eq!(
            v,
            serde_json::json!([
                {"type": "text", "text": "compare"},
                {"type": "image_url", "image_url": {"url": "https://a.example/1.png"}},
                {"type": "image_url", "image_url": {"url": "https://a.example/2.png"}},
            ])
        );
    }

    #[test]
    fn raw_output_passes_markdown_through_unchanged() {
        let md = "# Title\n\nSome **bold** text\n\n```rust\nfn main() {}\n```";
        let out = raw_output(md);
        assert_eq!(out, format!("{md}\n"));
        assert!(!out.contains('\u{1b}'), "raw output must not contain ANSI");
    }

    #[test]
    fn raw_output_does_not_double_trailing_newline() {
        assert_eq!(raw_output("done\n"), "done\n");
    }
}
