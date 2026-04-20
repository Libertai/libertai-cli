use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use indicatif::ProgressBar;
use std::path::Path;
use std::time::Duration;

use crate::client::{post_image, ImageRequest};
use crate::config::load;

pub fn run(
    prompt: String,
    model: Option<String>,
    size: String,
    n: u32,
    out: String,
    force: bool,
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

    if resp.data.is_empty() {
        bail!("server returned no images");
    }

    let paths: Vec<String> = if resp.data.len() == 1 {
        vec![out.clone()]
    } else {
        (0..resp.data.len()).map(|i| numbered_path(&out, i)).collect()
    };

    // Refuse to clobber existing files unless --force.
    if !force {
        for p in &paths {
            if Path::new(p).exists() {
                bail!("{p} already exists — pass --force to overwrite");
            }
        }
    }

    for (datum, path) in resp.data.iter().zip(paths.iter()) {
        let bytes = STANDARD
            .decode(&datum.b64_json)
            .context("decoding b64_json from image response")?;
        std::fs::write(path, &bytes).with_context(|| format!("writing {path}"))?;
        eprintln!("wrote {path}");
    }

    Ok(())
}

pub(crate) fn numbered_path(out: &str, i: usize) -> String {
    match out.rfind('.') {
        Some(idx) if idx > 0 => {
            let (stem, ext) = out.split_at(idx);
            format!("{stem}-{i}{ext}")
        }
        _ => format!("{out}-{i}.png"),
    }
}

#[cfg(test)]
mod tests {
    use super::numbered_path;

    #[test]
    fn numbered_inserts_before_extension() {
        assert_eq!(numbered_path("foo.png", 0), "foo-0.png");
        assert_eq!(numbered_path("out/dir/a.jpeg", 3), "out/dir/a-3.jpeg");
    }

    #[test]
    fn numbered_appends_png_when_no_extension() {
        assert_eq!(numbered_path("foo", 1), "foo-1.png");
    }

    #[test]
    fn numbered_dotfile_gets_png_suffix() {
        // A leading-dot name like ".png" has the extension at index 0; we treat
        // it as "no extension" and append -N.png.
        assert_eq!(numbered_path(".png", 0), ".png-0.png");
    }
}
