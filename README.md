# libertai

A single-binary CLI for [LibertAI](https://libertai.io). It pre-wires
third-party coding agents — [Claude Code](https://docs.claude.com/en/docs/claude-code)
and [OpenCode](https://opencode.ai) — to run against LibertAI's confidential
(TEE-backed) inference on open models, and ships utility commands
(ask / chat / image / search / fetch) plus an MCP server.

## Install

Pick the channel that fits your OS. The one-liner, Debian bootstrap, and
Homebrew install the released binary; the Rust command builds the same source
from the release branch.

```sh
# One-liner (Linux / macOS / WSL — no sudo, installs to ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/Libertai/libertai-cli/master/packaging/install.sh | sh

# Debian / Ubuntu (system-wide)
curl -fsSL https://apt.libertai.io/install.sh | sudo bash

# macOS (Homebrew)
brew install Libertai/tap/libertai

# Any platform with a Rust toolchain
cargo install --git https://github.com/Libertai/libertai-cli --branch master --locked

# From source (dev)
git clone https://github.com/Libertai/libertai-cli
cd libertai-cli && cargo install --path .
```

Windows: grab the latest `libertai-windows-x86_64.exe` from
[GitHub Releases](https://github.com/Libertai/libertai-cli/releases/latest).
No native package yet.

The binary is named `libertai`. The one-liner honours `LIBERTAI_VERSION`
(pin a tag) and `LIBERTAI_INSTALL_DIR` (override the install dir).

## Updates

`libertai` pings GitHub once every 24h in a background thread and prints a
one-line banner on the next startup if a newer release exists, pointing to
the upgrade command that matches how you installed it (Debian bootstrap /
brew / Cargo-from-git / re-run install.sh). No self-replacing `libertai
update` subcommand.

Silence the banner with `NO_UPDATE_CHECK=1` or
`libertai config set check_for_updates false`. The check is also skipped
automatically in non-interactive shells and CI.

## Quick start

```sh
libertai login          # pick: [1] browser sign-in  [2] paste API key
libertai claude         # launch Claude Code against LibertAI
libertai opencode       # launch OpenCode against LibertAI
libertai ask "explain EIP-191 signing in two sentences"
libertai chat           # streaming REPL, Ctrl-D to exit
libertai image "a lighthouse at dusk" --out dusk.png
```

## Launchers: Claude Code & OpenCode

`libertai claude` and `libertai opencode` are the headline commands. Each
launches its respective agent with LibertAI wired in as the backend — open
models, confidential (TEE-backed) inference, no training on your code — so
you keep the agent UX you already know but run it on LibertAI.

```sh
libertai claude                        # Claude Code against LibertAI
libertai claude --opus glm-5.2          # override a single tier
libertai opencode                       # OpenCode against LibertAI
libertai opencode --model libertai/glm-5.2
```

Both:

- Inject your LibertAI API key into the child process so the agent
  authenticates without further setup.
- Map the agent's model tiers (Claude Code's opus / sonnet / haiku, OpenCode's
  default) to LibertAI models from your config (`glm-5.2`, `glm-5.2-thinking`,
  `qwen3.6-35b-a3b`, …). Override per-run with `--model`, or per-tier with
  `--opus` / `--sonnet` / `--haiku` (Claude Code).
- Auto-install the bundled [agent skills](#agent-skills) (image generation,
  web search) non-destructively into `~/.claude/skills/`, so the agent gains
  `libertai image` / `libertai search` / `libertai fetch` capabilities.

Model and provider tiers default to `launcher_defaults.*` /
`default_chat_model` from config; see [How the launchers work](#how-the-launchers-work)
for the exact env-var wiring and the OpenCode provider-entry specifics.

Beyond Claude Code and OpenCode, `libertai run -- <cmd>` is the generic
form that injects LibertAI env vars before any command, and there are
opinionated presets for [Aider](https://aider.chat),
[Claw Code](https://github.com/ultraworkers/claw-code), and
[Hermes Agent](https://hermes-agent.nousresearch.com) too (see the
[Commands](#commands) table).

## Commands

| Command | Description |
| --- | --- |
| `libertai login` | Interactive login: browser SSO (recommended) or paste an API key. |
| `libertai logout` | Clear saved credentials (backs up the config to `config.toml.bak.<epoch>`). |
| `libertai status` | Show current auth state and default models. `--json`. |
| `libertai models` | List models available from `/v1/models`. `--json`; `--refresh` re-syncs the model catalog persisted for the launchers. |
| `libertai ask <prompt>` | One-shot, non-streaming completion. |
| `libertai chat` | Streaming chat REPL with history. `--system` for a system prompt. |
| `libertai search <query>` | Web search via `search.libertai.io`. `--max-results`, `--type web\|news\|images`, `--engines`, `--json`. |
| `libertai fetch <url>` | Fetch a URL and return its cleaned article text (title, content, word count). `--json` for the raw response. |
| `libertai mcp` | Run an MCP server over stdio exposing `web_search` + `fetch_page` to MCP clients (Claude Code, Cursor, Cline, …) — see [MCP server](#mcp-server). |
| `libertai image <prompt>` | Generate and save images. `--n`, `--size`, `--out`, `--model`, `--force`. |
| `libertai keys list\|create\|delete` | Manage your account's API keys. `list --json`. |
| `libertai run -- <cmd>` | Exec any command with LibertAI env vars injected. |
| `libertai claude [args]` | Launch [Claude Code](https://docs.claude.com/en/docs/claude-code) against LibertAI. `--model`, `--opus`, `--sonnet`, `--haiku`. |
| `libertai opencode [args]` | Writes a `libertai` provider into `~/.config/opencode/opencode.json`, sets `LIBERTAI_API_KEY`, then launches [OpenCode](https://opencode.ai). |
| `libertai aider [args]` | `run` preset for Aider; auto-passes `--model openai/<default_chat_model>`. |
| `libertai claw [args]` | `run` preset for [Claw Code](https://github.com/ultraworkers/claw-code); auto-passes `--model openai/<default_chat_model>`. |
| `libertai hermes [args]` | Launch [Hermes Agent](https://hermes-agent.nousresearch.com) with LibertAI credentials injected (env vars). |
| `libertai config show\|path\|set\|unset` | Inspect or edit `~/.config/libertai/config.toml`. |
| `libertai skills install\|list\|uninstall` | Manage the bundled agent skills (image gen, web search). |

### Scripting

The CLI is built to compose with pipes and scripts:

- **`--json`** — `status`, `models`, `keys list`, `search`, `fetch` emit
  machine-readable JSON. JSON is the *only* thing written to stdout; progress
  notes and human extras go to stderr.
- **`models --json`** — keeps the `/v1/models` wire fields (`id`, `owned_by`)
  and, when LibertAI's public model catalog is reachable (fetched from an
  Aleph aggregate, cached on disk for 24h), adds a `catalog` object per text
  model: `name`, `hfId`, `contextWindow`, `vision`, `reasoning`, `tee`,
  `functionCalling`, `inputUsdPerMtok`, `outputUsdPerMtok`. For
  alias/deprecated/`-thinking` ids the metadata comes from the base entry
  and `resolvedId` replaces `name`/`hfId`. Offline, the `catalog` key is
  simply absent.
- **Styling** — ANSI colors are emitted only when the destination stream is
  a terminal; piped output is plain text. `NO_COLOR` (per
  [no-color.org](https://no-color.org)) and `TERM=dumb` disable styling
  everywhere.
- **Exit codes** —

  | Code | Meaning |
  | --- | --- |
  | 0 | success |
  | 1 | generic failure |
  | 2 | usage error (bad flags/arguments) |
  | 3 | auth required or rejected — run `libertai login` |
  | 4 | network/connect failure (backend unreachable, DNS, timeout) |
  | 5 | server-side API error (non-401 4xx/5xx response) |

## MCP server

`libertai mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio, exposing two tools backed by LibertAI's search API:
`web_search` (multi-engine web/news/images/academic search with snippets,
URLs, and cross-engine consensus info) and `fetch_page` (fetch a URL as
cleaned plain text). Any MCP client can use them — point it at the
installed binary and you're done. Auth reuses your CLI credentials
(`libertai login`) or a `LIBERTAI_API_KEY` env var; without a key the
tools answer with setup instructions instead of failing.

**Claude Code**

```sh
claude mcp add libertai -- libertai mcp
```

**Generic JSON config** (Claude Desktop `claude_desktop_config.json` and
most other clients):

```json
{"mcpServers":{"libertai":{"command":"libertai","args":["mcp"]}}}
```

**Cursor** — add to `~/.cursor/mcp.json` (or `.cursor/mcp.json` in a project):

```json
{
  "mcpServers": {
    "libertai": { "command": "libertai", "args": ["mcp"] }
  }
}
```

**Cline** — Settings → MCP Servers → Configure, or edit
`cline_mcp_settings.json`:

```json
{
  "mcpServers": {
    "libertai": {
      "command": "libertai",
      "args": ["mcp"],
      "env": { "LIBERTAI_API_KEY": "LTAI_..." }
    }
  }
}
```

(The `env` block is only needed if you haven't run `libertai login` on
that machine.)

## Config

`~/.config/libertai/config.toml` (permissions `0600`):

```toml
api_base           = "https://api.libertai.io"
account_base       = "https://api.libertai.io"
default_chat_model  = "glm-5.2"
default_image_model = "z-image-turbo"

[launcher_defaults]
opus_model   = "glm-5.2"
sonnet_model = "glm-5.2"
haiku_model  = "qwen3.6-35b-a3b"

[auth]
api_key = "LTAI_..."
```

Set values with:

```sh
libertai config set default_chat_model hermes-3-8b-tee
libertai config set launcher_defaults.opus_model gemma-4-31b-it
```

Reset a key (or everything) back to the built-in default so future default
changes in the CLI propagate to you automatically:

```sh
libertai config unset default_chat_model
libertai config unset launcher_defaults          # all three launcher tiers
libertai config unset all                        # every non-auth field
```

Fields that match the built-in default are omitted from the saved file, so
once reset they track future upgrades.

## How the launchers work

`libertai claude` is equivalent to:

```sh
env \
  ANTHROPIC_BASE_URL=https://api.libertai.io \
  ANTHROPIC_AUTH_TOKEN=$LTAI_API_KEY \
  ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.2 \
  ANTHROPIC_DEFAULT_SONNET_MODEL=glm-5.2 \
  ANTHROPIC_DEFAULT_HAIKU_MODEL=qwen3.6-35b-a3b \
  CLAUDE_CODE_ATTRIBUTION_HEADER=0 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  CLAUDE_CODE_DISABLE_1M_CONTEXT=1 \
  DISABLE_TELEMETRY=1 \
  claude
```

…without the 60 characters of typing. `--model`, `--opus`, `--sonnet`, `--haiku`
override individual tiers.

`libertai run -- <cmd>` is the generic form: it always sets
`OPENAI_API_KEY` / `OPENAI_BASE_URL` / `OPENAI_API_BASE` /
`ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` before exec'ing.

### OpenCode specifics

OpenCode ignores `OPENAI_*` env vars for custom providers and instead
requires a provider entry in `~/.config/opencode/opencode.json`.
`libertai opencode` synthesizes one idempotently — a `provider.libertai`
block pointing at `<api_base>/v1` with `apiKey: "{env:LIBERTAI_API_KEY}"`
and a models map built from your `default_chat_model` plus the three
launcher tiers. Other top-level keys and providers in `opencode.json` are
preserved. `LIBERTAI_API_KEY` is exported from the CLI's config on each
launch. If you don't pass `--model`, the CLI appends
`--model libertai/<default_chat_model>`.

### Claw specifics

[Claw Code](https://github.com/ultraworkers/claw-code) reads
`ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` like Claude Code, but its
CLI rejects model names that don't match a known provider prefix with
`invalid_model_syntax`, and the Anthropic path does not strip a routing
prefix before sending the request — so `--model qwen3.5-122b-a10b` and
`--model anthropic/qwen3.5-122b-a10b` both fail against a LibertAI
backend. `libertai claw` works around this by routing via claw's
OpenAI-compatible path: it appends `--model openai/<default_chat_model>`
(the `openai/` prefix *is* stripped before the request is sent) and
relies on `OPENAI_BASE_URL` / `OPENAI_API_KEY` from the base env. Claw
doesn't ship with a `~/.claude/skills/` reader today, so the image /
search skills aren't automatically available inside a claw session yet.

## Agent skills

Out of the box, agent CLIs pointed at LibertAI have no image-generation or
web-search tool. The CLI bundles two Claude Code
[skills](https://code.claude.com/docs/en/skills) that teach the agent how
to call `libertai` for these capabilities:

- **`libertai-image`** — teaches the agent to run `libertai image "<prompt>"
  --out <path>` when the user asks for a picture, logo, mockup, etc.
- **`libertai-search`** — teaches the agent to run `libertai search "<query>"
  [--type news|images]` for fact-checks, current events, and research, and
  `libertai fetch "<url>"` to read the cleaned text of a specific page.

Because both Claude Code and OpenCode read skills from
`~/.claude/skills/<name>/SKILL.md`, the same bundle works for both. Aider
has no skill system, so `libertai aider` instead generates an
instructions file at `~/.config/libertai/aider-instructions.md` and passes
`--read <that file>` when it exec's `aider`.

`libertai claude` and `libertai opencode` auto-install the bundled skills
non-destructively (existing files are left alone so customisations
survive). Manual control:

```sh
libertai skills list                 # show what's bundled
libertai skills install              # force-refresh into ~/.claude/skills/
libertai skills install --project    # into ./.claude/skills/ for this repo
libertai skills uninstall            # remove
```

| Tool | How libertai capabilities reach it |
| --- | --- |
| Claude Code (`libertai claude`) | `~/.claude/skills/libertai-*` (native) |
| OpenCode (`libertai opencode`) | `~/.claude/skills/libertai-*` (opencode reads Claude's skill format) + `provider.libertai` in `opencode.json` |
| Aider (`libertai aider`) | `~/.config/libertai/aider-instructions.md` loaded via `--read` |

## Authentication

`libertai login` offers two flows:

1. **Browser sign-in (recommended)** — opens the console, you sign in (email,
   wallet, or OAuth) and approve; the CLI gets a device key (90-day expiry, re-run
   to renew). Uses a standard OAuth loopback + PKCE flow.
2. **API key** — paste a key from [console.libertai.io](https://console.libertai.io).

Set `LIBERTAI_CONSOLE_URL` to use a non-default console.

## Security notes

- Credentials live on disk at `~/.config/libertai/config.toml` in plaintext
  (file mode `0600`, parent dir `0700`). OS keyring storage is on the roadmap.
- `libertai run` / `claude` / `opencode` / `aider` **inject the API key into
  the child process's environment**. Any subprocess that reads env vars — and
  any diagnostic tool that can enumerate this process — can see the key. This
  is the only way these third-party tools can authenticate today; if you do
  not want this tradeoff, use `libertai` `ask` / `chat` / `image` directly.
- The `account_base` the account commands talk to is user-configurable — if
  you change it, you are trusting that host.
- HTTPS is enforced for `api_base` and `account_base`; `http://` URLs are
  rejected at config load.

## Development

```sh
cargo build                      # debug
cargo build --release            # single optimized `libertai` binary
cargo test                       # ~500 tests, offline + deterministic
./target/release/libertai --help
```

The test suite includes black-box probes (`tests/probes_*.rs`, via
`assert_cmd`) that spawn the built binary and assert on its stdout/stderr.
Tier-2 LLM-judge probes call LibertAI's free chat model to evaluate
output-shape properties; they are off by default so plain `cargo test`
stays offline. Run them with
`cargo test --features tier2-probes -- --include-ignored`.

## Roadmap

- OS keyring storage as an alternative to the TOML file.
- OpenCode MCP bridge (expose image/search as MCP tools in opencode's tool
  list).
