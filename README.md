# libertai

A single-binary CLI for [LibertAI](https://libertai.io): log in once, then run
inference, generate images, and launch agent tools (Claude Code, OpenCode,
Aider) pre-wired to talk to LibertAI.

## Install

From source (requires Rust 1.80+):

```sh
git clone https://github.com/Libertai/libertai-cli
cd libertai-cli
cargo install --path .
```

The binary is named `libertai` and installs into `~/.cargo/bin`.

## Quick start

```sh
libertai login          # pick: [1] paste API key  [2] sign with wallet  [3] open console
libertai ask "explain EIP-191 signing in two sentences"
libertai chat           # streaming REPL, Ctrl-D to exit
libertai image "a lighthouse at dusk" --out dusk.png
libertai claude         # launch Claude Code against LibertAI
```

## Commands

| Command | Description |
| --- | --- |
| `libertai login` | Interactive login: API key, wallet signing (Base), or browser fallback. |
| `libertai logout` | Back up the current config to `config.toml.bak.<epoch>`. |
| `libertai status` | Show current auth state and default models. |
| `libertai models` | List models available from `/v1/models`. |
| `libertai ask <prompt>` | One-shot, non-streaming completion. |
| `libertai chat` | Streaming chat REPL with history. `--system` for a system prompt. |
| `libertai search <query>` | Web search via `search.libertai.io`. `--max-results`, `--type web\|news\|images`, `--json`. |
| `libertai image <prompt>` | Generate and save images. `--n`, `--size`, `--out`, `--model`, `--force`. |
| `libertai keys list\|create\|delete` | Manage API keys (requires wallet). |
| `libertai run -- <cmd>` | Exec any command with LibertAI env vars injected. |
| `libertai claude [args]` | `run` preset for [Claude Code](https://docs.claude.com/en/docs/claude-code). |
| `libertai opencode [args]` | Writes a `libertai` provider into `~/.config/opencode/opencode.json`, sets `LIBERTAI_API_KEY`, then launches OpenCode. |
| `libertai aider [args]` | `run` preset for Aider; auto-passes `--model openai/<default>`. |
| `libertai config show\|path\|set\|unset` | Inspect or edit `~/.config/libertai/config.toml`. |
| `libertai skills install\|list\|uninstall` | Manage bundled Claude Code skills (image gen etc). |

## Config

`~/.config/libertai/config.toml` (permissions `0600`):

```toml
api_base           = "https://api.libertai.io"
account_base       = "https://api.libertai.io"
default_chat_model  = "qwen3.5-122b-a10b"
default_image_model = "z-image-turbo"

[launcher_defaults]
opus_model   = "gemma-4-31b-it"
sonnet_model = "qwen3.6-35b-a3b"
haiku_model  = "qwen3.6-35b-a3b"

[auth]
api_key = "LTAI_..."
# wallet_address / chain are only written when you log in via wallet.
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
  ANTHROPIC_DEFAULT_OPUS_MODEL=gemma-4-31b-it \
  ANTHROPIC_DEFAULT_SONNET_MODEL=qwen3.6-35b-a3b \
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

## Agent skills

Out of the box, agent CLIs pointed at LibertAI have no image-generation or
web-search tool. The CLI bundles two Claude Code
[skills](https://code.claude.com/docs/en/skills) that teach the agent how
to call `libertai` for these capabilities:

- **`libertai-image`** — teaches the agent to run `libertai image "<prompt>"
  --out <path>` when the user asks for a picture, logo, mockup, etc.
- **`libertai-search`** — teaches the agent to run `libertai search "<query>"
  [--type news|images]` for fact-checks, current events, and research.

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

Future: OpenCode MCP bridge (exposes image/search as MCP tools so they
appear in opencode's tool list alongside native ones).

## Authentication

Two supported flows today:

1. **API key** — create one at [console.libertai.io](https://console.libertai.io)
   and paste it into `libertai login`.
2. **Wallet signing on Base** — `libertai login` prompts for a hex-encoded
   secp256k1 private key, fetches an auth message from `/auth/message`,
   **shows you the message and host and asks for confirmation before
   signing**, then submits the EIP-191 signature to `/auth/login` and
   creates an inference API key via `/api-keys`.

The private key is never persisted. Only the derived address and chain are
saved, so `keys list/create/delete` can re-prompt for signing when they need a
fresh JWT.

## Security notes

- Credentials live on disk at `~/.config/libertai/config.toml` in plaintext
  (file mode `0600`, parent dir `0700`). OS keyring storage is on the roadmap.
- `libertai run` / `claude` / `opencode` / `aider` **inject the API key into
  the child process's environment**. Any subprocess that reads env vars — and
  any diagnostic tool that can enumerate this process — can see the key. This
  is the only way these third-party tools can authenticate today; if you do
  not want this tradeoff, use `libertai ask` / `chat` / `image` directly.
- The wallet-signing flow shows you the exact message and host before signing
  and requires explicit confirmation. Still: the `account_base` it talks to is
  user-configurable — if you change it, you are trusting that host to issue
  benign signing requests.
- HTTPS is enforced for `api_base` and `account_base`; `http://` URLs are
  rejected at config load.

## Development

```sh
cargo build                      # debug
cargo build --release            # single optimized binary
cargo test                       # config round-trip + masking
./target/release/libertai --help
```

## Roadmap

- Solana wallet signing.
- Browser-based device pairing (console.libertai.io issues a one-time code the
  CLI exchanges for a key — removes the private-key prompt).
- `libertai openclaw` and `libertai hermes` launchers.
- OS keyring storage as an alternative to the TOML file.
