# libertai

A single-binary CLI for [LibertAI](https://libertai.io). The centerpiece is
`libertai code` — a privacy-first coding agent running open models on
LibertAI's confidential (TEE-backed) inference — plus utility commands
(ask / chat / image / search / fetch) and launchers that pre-wire
third-party agent tools (Claude Code, OpenCode, Aider, Claw, Hermes) to
LibertAI.

## Install

Pick the channel that fits your OS — they all land on the same released binary.

```sh
# One-liner (Linux / macOS / WSL — no sudo, installs to ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/Libertai/libertai-cli/main/packaging/install.sh | sh

# Debian / Ubuntu (system-wide, auto-updates via apt)
curl -fsSL https://apt.libertai.io/install.sh | sudo bash

# macOS (Homebrew)
brew install Libertai/tap/libertai

# Any platform with a Rust toolchain
cargo install libertai-cli

# From source (dev)
git clone https://github.com/Libertai/libertai-cli
cd libertai-cli && cargo install --path .
```

Windows: grab the latest `libertai-windows-x86_64.exe` from
[GitHub Releases](https://github.com/Libertai/libertai-cli/releases/latest).
No native package yet.

The binary is named `libertai`. Cargo / from-source builds also produce an
`lcode` alias binary for the coding agent. The one-liner honours
`LIBERTAI_VERSION` (pin a tag) and `LIBERTAI_INSTALL_DIR` (override the
install dir).

## Updates

`libertai` pings GitHub once every 24h in a background thread and prints a
one-line banner on the next startup if a newer release exists, pointing to
the upgrade command that matches how you installed it (apt / brew / cargo
install / re-run install.sh). No self-replacing `libertai update` subcommand
— updates flow through your system package manager.

Silence the banner with `NO_UPDATE_CHECK=1` or
`libertai config set check_for_updates false`. The check is also skipped
automatically in non-interactive shells and CI.

## Quick start

```sh
libertai login          # pick: [1] browser sign-in  [2] paste API key
libertai code           # the coding agent — interactive REPL in the current repo
libertai ask "explain EIP-191 signing in two sentences"
libertai chat           # streaming REPL, Ctrl-D to exit
libertai image "a lighthouse at dusk" --out dusk.png
libertai claude         # or launch Claude Code against LibertAI instead
```

## The coding agent: `libertai code`

`libertai code` (alias binary: `lcode`, from cargo / source builds) is
LibertAI's first-party coding agent, built on the [pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust)
agent loop. Open models, confidential inference, no training on your code.
Run it bare for the interactive REPL; pass a prompt for a one-shot run that
streams the answer to stdout (turn/tool noise goes to stderr, so it pipes
cleanly):

```sh
libertai code                                   # REPL
libertai code "fix the failing test in src/lib.rs"
lcode --plan "how would you refactor the config module?"
```

For scripting, `--print` / `-p` runs a turn fully headless (no interactive
prompts; prompt from args and/or piped stdin).

What's in the box:

- **Permission modes** — `default`, `acceptEdits`, `plan`; cycle with
  **Shift+Tab** or use `/plan` / `/mode`. Plan mode lets the agent
  read/grep/find/ls but blocks bash, writes, and edits. Tool approvals can
  be persisted as allow-rules (`/permissions` to inspect, `/forget` to
  clear).
- **55+ built-in slash commands** — `/help` lists them: `/model`,
  `/compact`, `/resume`, `/fork`, `/usage`, `/review`, `/memory`,
  `/agents`, `/mcp`, `/hooks`, `/export`, `/share`, … Claude-style custom
  commands are discovered from `.claude/commands/` (project and user) too.
- **Sessions** — every run persists as JSONL. `--continue` resumes the most
  recent session for the cwd, `--resume <path>` a specific one,
  `--list-sessions` (add `--all` for every project) prints them, and
  `/fork` branches from an earlier message. Long sessions auto-compact as
  the context window fills.
- **MCP** — configure servers under `[mcpServers.<name>]` in
  `config.toml`; stdio, streamable HTTP, and legacy SSE transports are
  supported. The agent reaches them through the `mcp_call` /
  `mcp_read_resource` / `mcp_get_prompt` tools; `/mcp` shows server status.
- **Hooks** — Claude-Code-format hooks (`UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, `Stop`, …) configured in `config.toml`; see
  [Config](#config) for the payload contract.
- **Subagents** — the native `task` tool spawns sub-agents; `/agents`
  manages named and background ones, `/agent <name> <task>` runs one.
- **Skills** — agent skills in the
  [agentskills.io](https://agentskills.io/specification) format, loaded
  from `.claude/skills/` and `~/.config/libertai/skills/` (several ship
  built in); user-invocable skills surface as slash commands. `/skills`
  manages them.
- **Sandboxing** — `--sandbox=strict` wraps the bash tool in `bwrap`
  (Linux-only today): no network, read-only system dirs, tmpfs `/tmp`.
  Useful for untrusted models or third-party agent scripts. The default
  `off` runs bash with full host privileges. Also settable via the
  `LIBERTAI_SANDBOX` env var; `libertai sandbox info` prints the resolved
  profile.
- **Claude Code import** — `libertai import claude-code list|show|summarize|import`
  turns an existing Claude Code transcript into a summarized checkpoint you
  can pick up with `libertai code --resume <path>`.

Model and provider default to `default_code_model` /
`default_code_provider` from config; override per-run with `--model` /
`--provider`.

## Commands

| Command | Description |
| --- | --- |
| `libertai login` | Interactive login: browser SSO (recommended) or paste an API key. |
| `libertai logout` | Clear saved credentials (backs up the config to `config.toml.bak.<epoch>`). |
| `libertai status` | Show current auth state and default models. `--json`. |
| `libertai models` | List models available from `/v1/models`. `--json`; `--refresh` re-syncs the model catalog persisted for `libertai code`. |
| `libertai ask <prompt>` | One-shot, non-streaming completion. |
| `libertai chat` | Streaming chat REPL with history. `--system` for a system prompt. |
| `libertai code [prompt]` | The coding agent (see above). `--print/-p`, `--plan`, `--resume`, `--continue`, `--list-sessions` (`--json`), `--sandbox`, `--model`, `--provider`. Alias binary: `lcode`. |
| `libertai search <query>` | Web search via `search.libertai.io`. `--max-results`, `--type web\|news\|images`, `--engines`, `--json`. |
| `libertai fetch <url>` | Fetch a URL and return its cleaned article text (title, content, word count). `--json` for the raw response. |
| `libertai image <prompt>` | Generate and save images. `--n`, `--size`, `--out`, `--model`, `--force`. |
| `libertai keys list\|create\|delete` | Manage your account's API keys. `list --json`. |
| `libertai run -- <cmd>` | Exec any command with LibertAI env vars injected. |
| `libertai claude [args]` | `run` preset for [Claude Code](https://docs.claude.com/en/docs/claude-code). |
| `libertai opencode [args]` | Writes a `libertai` provider into `~/.config/opencode/opencode.json`, sets `LIBERTAI_API_KEY`, then launches OpenCode. |
| `libertai aider [args]` | `run` preset for Aider; auto-passes `--model openai/<default_code_model>`. |
| `libertai claw [args]` | `run` preset for [Claw Code](https://github.com/ultraworkers/claw-code); auto-passes `--model openai/<default_code_model>`. |
| `libertai hermes [args]` | Launch [Hermes Agent](https://hermes-agent.nousresearch.com) with LibertAI credentials injected (env vars). |
| `libertai config show\|path\|set\|unset` | Inspect or edit `~/.config/libertai/config.toml`. |
| `libertai skills install\|list\|uninstall` | Manage the bundled Claude Code skills (image gen, web search). |
| `libertai sandbox info` | Print the resolved bash-sandbox profile for this host. `--json`. |
| `libertai import claude-code …` | Import Claude Code transcripts into resumable `libertai code` sessions. |

### Scripting

The CLI is built to compose with pipes and scripts:

- **`--json`** — `status`, `models`, `keys list`, `code --list-sessions`,
  `search`, `fetch`, `sandbox info`, and `import claude-code list|show`
  emit machine-readable JSON. JSON is the *only* thing written to stdout;
  progress notes and human extras go to stderr.
- **`models --json`** — keeps the `/v1/models` wire fields (`id`,
  `owned_by`) and, when LibertAI's public model catalog is reachable
  (fetched from an Aleph aggregate, cached on disk for 24h), adds a
  `catalog` object per text model: `name`, `hfId`, `contextWindow`,
  `vision`, `reasoning`, `tee`, `functionCalling`, `inputUsdPerMtok`,
  `outputUsdPerMtok`. For alias/deprecated/`-thinking` ids the metadata
  comes from the base entry and `resolvedId` replaces `name`/`hfId`.
  Offline, the `catalog` key is simply absent.
- **Styling** — ANSI colors are emitted only when the destination stream
  is a terminal; piped output is plain text. `NO_COLOR` (per
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

## Config

`~/.config/libertai/config.toml` (permissions `0600`):

```toml
api_base           = "https://api.libertai.io"
account_base       = "https://api.libertai.io"
default_chat_model  = "qwen3.5-122b-a10b"
default_code_model  = "qwen3.6-35b-a3b"
default_code_provider = "libertai"
default_image_model = "z-image-turbo"

[launcher_defaults]
opus_model   = "gemma-4-31b-it"
sonnet_model = "qwen3.6-35b-a3b"
haiku_model  = "qwen3.6-35b-a3b"

# MCP servers for `libertai code` — stdio (command) or HTTP/SSE (url).
[mcpServers.docs]
command = "docs-mcp-server"
args = ["--root", "."]

[mcpServers.remote]
transport = "streamable-http"   # or "sse" for legacy servers
url = "https://example.com/mcp"

[[hooks.UserPromptSubmit]]
command = "scripts/user-prompt-submit.sh"
timeout = 5

[[hooks.PreToolUse]]
matcher = "bash|write|edit"
command = "scripts/pre-tool-use.sh"
timeout = 5
reviewPolicy = "strict"
continueOnBlock = true

[[hooks.PostToolUse]]
matcher = "bash|write|edit"
command = "scripts/post-tool-use.sh"
timeout = 5
async = true

[[hooks.SubagentStop]]
matcher = "task"
command = "scripts/subagent-stop.sh"

[[hooks.SessionStart]]
command = "scripts/session-start.sh"

[[hooks.Stop]]
command = "scripts/stop.sh"

[[hooks.SessionEnd]]
command = "scripts/session-end.sh"

[auth]
api_key = "LTAI_..."
```

`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `SubagentStop`,
`SessionStart`, `Stop`, and `SessionEnd` hooks are command-only in the
native CLI. They receive a JSON payload on stdin. `UserPromptSubmit` hooks run before the prompt
reaches the agent and may add `additionalContext` or block on nonzero exit.
`PreToolUse` hooks may print Claude-style JSON such as
`{"permissionDecision":"deny","permissionDecisionReason":"no writes"}`.
`SubagentStop` hooks run after native `task` tool subagents finish.
Tool hook `matcher` values support case-sensitive exact names, `*` globs,
`|` alternatives, `regex:<pattern>`, and slash-delimited regex patterns
such as `/^(bash|write)$/`.
Claude-style hook metadata such as `source`, `statusMessage`,
`reviewPolicy`, `once`, `asyncRewake`, and `continueOnBlock` round-trips as
named fields; unknown hook metadata is preserved for config-save fidelity.
Set `async = true` (or imported `asyncHook = true`) to launch a command
hook without waiting for completion; async hook output is discarded and
cannot affect prompt/tool decisions.
Native command, HTTP, prompt, agent, and MCP-tool hook handlers are supported
for session hook events.

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
and a models map built from your `default_chat_model` / `default_code_model`
plus the three launcher tiers. Other top-level keys and providers in
`opencode.json` are preserved. `LIBERTAI_API_KEY` is exported from the CLI's
config on each launch. If you don't pass `--model`, the CLI appends
`--model libertai/<default_code_model>`.

### Claw specifics

[Claw Code](https://github.com/ultraworkers/claw-code) reads
`ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` like Claude Code, but its
CLI rejects model names that don't match a known provider prefix with
`invalid_model_syntax`, and the Anthropic path does not strip a routing
prefix before sending the request — so `--model qwen3.5-122b-a10b` and
`--model anthropic/qwen3.5-122b-a10b` both fail against a LibertAI
backend. `libertai claw` works around this by routing via claw's
OpenAI-compatible path: it appends `--model openai/<default_code_model>`
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
| `libertai code` / `lcode` | Native tools (search/fetch/image built in) + agent skills from `.claude/skills/` |
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
  not want this tradeoff, use `libertai code` / `ask` / `chat` / `image`
  directly.
- The `account_base` the account commands talk to is user-configurable — if
  you change it, you are trusting that host.
- HTTPS is enforced for `api_base` and `account_base`; `http://` URLs are
  rejected at config load.
- `libertai code --sandbox=strict` confines the bash tool with `bwrap`, but
  the default mode runs tools with your full host privileges — review what
  you approve.

## Development

```sh
cargo build                      # debug
cargo build --release            # single optimized `libertai` + `lcode`
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

- macOS / Windows backends for `--sandbox=strict` (Linux `bwrap` only today).
- OpenCode MCP bridge (expose image/search as MCP tools in opencode's tool
  list).
- OS keyring storage as an alternative to the TOML file.
