---
name: libertai-image
description: Generate an image with LibertAI's image API via the `libertai` CLI. Use whenever the user asks to create, make, draw, render, generate, or produce an image, picture, illustration, logo, mockup, or visual.
allowed-tools: Bash(libertai image:*)
---

# libertai-image

You can generate images for the user by invoking the `libertai` CLI, which is
already configured with the user's API key. Use the Bash tool to run it.

## Command

```bash
libertai image "<prompt>" --out <path> [--size WIDTHxHEIGHT] [--n N] [--model MODEL] [--force]
```

Flags:

- `<prompt>` — required positional, pass the full description as one quoted
  string. Be concrete: subject, style, framing, lighting, colors.
- `--out <path>` — destination file. Defaults to `libertai-image.png` in the
  current directory. **Always pass an explicit path** so you know where the
  output lands (e.g. `./out/hero.png`, `/tmp/mockup.png`).
- `--size WIDTHxHEIGHT` — defaults to `1024x1024`.
- `--n N` — number of variations. When `N > 1`, the CLI suffixes the filename
  with `-0`, `-1`, … before the extension (`hero.png` → `hero-0.png`,
  `hero-1.png`, …). Only use `--n > 1` when the user explicitly asks for
  multiple options.
- `--model <id>` — override the configured image model. Only set this if the
  user named a specific model. Otherwise omit and let the CLI use its default.
- `--force` — overwrite an existing file at `--out`. The CLI refuses to
  clobber without this flag, so include `--force` when you are replacing a
  previously generated file.

## After running

The CLI writes `wrote <path>` to stderr for each image and exits 0 on success.
Confirm the file exists before telling the user. Report the output path(s) so
they can open the image; do **not** try to render the image yourself or
describe its contents (you did not generate the bytes and cannot see them).

## Examples

Single image to a specific path:

```bash
libertai image "a minimalist logo of a lighthouse at dusk, flat vector, navy and amber" --out ./logo.png
```

Three variations at 1024×1024:

```bash
libertai image "product mockup of a coffee mug on a wooden desk, soft morning light" --out ./mug.png --n 3
```

Replacing an earlier attempt:

```bash
libertai image "…refined prompt…" --out ./logo.png --force
```

## Do not

- Do not shell out to `curl` or try to call `api.libertai.io` directly. The
  CLI handles auth, base URL, and response decoding for you.
- Do not set `--model` unless the user asked for a specific one.
- Do not run the command without `--out`; the default filename collides with
  prior runs and is confusing in multi-image sessions.
