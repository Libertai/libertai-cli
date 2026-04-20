# LibertAI tools available via the `libertai` CLI

This project's `aider` session is launched with access to a configured
`libertai` CLI. You can invoke it to reach capabilities aider itself does not
provide.

## Generate images

```
libertai image "<prompt>" --out <path> [--size WIDTHxHEIGHT] [--n N] [--force]
```

Use this whenever the user asks for a picture, logo, mockup, or visual.
Always pass an explicit `--out` path. `--force` overwrites an existing file.
After success the command prints `wrote <path>` to stderr and the file is on
disk at `<path>`. Do not try to describe what the image looks like — you did
not see the bytes.

## Web search

```
libertai search "<query>" [--max-results N] [--type web|news|images] [--json]
```

Use for fact-checks, current events, version/release lookups, or any
question you are not confident you already know.

- `--max-results 3` for tight fact-checks; default 10 for broader research.
- `--type news` for time-sensitive answers.
- `--type images --json` if you need raw image URLs to download.
- Pipe to `curl -sL "<url>"` to read a full page when snippets aren't
  enough; keep full-page reads to ~3 per question unless the task requires
  deep research.
- If the query returns nothing, say so and suggest a rephrasing — do not
  invent results or URLs.

## Ask / chat with LibertAI models directly

```
libertai ask "<prompt>"                       # one-shot, non-streaming
libertai chat                                 # interactive REPL, Ctrl-D to exit
```

Prefer these over raw `curl` to `api.libertai.io` — the CLI handles auth,
base URL, and response parsing.

## Rules

- These commands are already authenticated; do not read or write
  `~/.config/libertai/config.toml` and do not set `ANTHROPIC_*` or
  `OPENAI_*` env vars yourself.
- Prefer the CLI over shelling out to `curl https://api.libertai.io/…`.
- If a command fails, show the user the exact command and stderr.
