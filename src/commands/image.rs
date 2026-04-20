use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use indicatif::ProgressBar;
use std::time::Duration;

use crate::client::{post_image, ImageRequest};
use crate::config::load;

pub fn run(
    prompt: String,
    model: Option<String>,
    size: String,
    n: u32,
    out: String,
) -> Result<()> {
    let cfg = load()?;
    let model = model.unwrap_or_else(|| cfg.default_image_model.clone());

    let pb = ProgressBar::new_spinner();
    pb.set_message("generating...");
    pb.enable_steady_tick(Duration::from_millis(100));

    let req = ImageRequest {
        model,
        prompt,
        size,
        n,
    };
    let resp = post_image(&cfg, &req);
    pb.finish_and_clear();
    let resp = resp?;

    for (i, datum) in resp.data.iter().enumerate() {
        let bytes = STANDARD
            .decode(&datum.b64_json)
            .context("decoding b64_json from image response")?;
        let path = if n == 1 {
            out.clone()
        } else {
            numbered_path(&out, i)
        };
        std::fs::write(&path, &bytes).with_context(|| format!("writing {path}"))?;
        eprintln!("wrote {path}");
    }

    Ok(())
}

fn numbered_path(out: &str, i: usize) -> String {
    match out.rfind('.') {
        Some(idx) if idx > 0 => {
            let (stem, ext) = out.split_at(idx);
            format!("{stem}-{i}{ext}")
        }
        _ => format!("{out}-{i}.png"),
    }
}
