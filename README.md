# libertai

A single Rust binary for [LibertAI](https://libertai.io). Its main job is
**`libertai code`** — LibertAI's own terminal coding agent, running open
models on confidential (TEE-backed) inference.

The same binary also ships utility commands (`ask` / `chat` / `search` /
`fetch` / `image`), an MCP server, and launchers that point *other* people's
coding agents (Claude Code, OpenCode, Aider, …) at LibertAI's backend.

```sh
libertai login
libertai code            # start the agent in the current repo
```

## Install

Pick the channel that fits your OS. The one-liner, Debian bootstrap, and
Homebrew install the released binary; the Rust commands build the same source.

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

The released binary is named `libertai`. The one-liner honours
`LIBERTAI_VERSION` (pin a tag) and `LIBERTAI_INSTALL_DIR` (override the
install dir).

There is a second binary, **`lcode`** — a short alias for `libertai code`
covering the most-used flags. It is only installed by the two Cargo routes
above (`cargo install` builds every `[[bin]]`); the release archives, the
`.deb`, and the brew formula ship `libertai` alone. Everything below that
says `libertai code` works as `lcode` too when you have it.

### Updates

`libertai` pings GitHub once every 24h in a background thread and prints a
one-line banner on the next startup if a newer release exists, pointing to
the upgrade command that matches how you installed it (Debian bootstrap /
brew / Cargo-from-git / re-run install.sh). There is no self-replacing
`libertai update` subcommand.

Silence the banner with `NO_UPDATE_CHECK=1` or
`libertai config set check_for_updates false`. The check is also skipped
automatically in non-interactive shells and CI.

## Quick start

```sh
libertai login                       # [1] browser sign-in  or  [2] paste an API key
cd ~/code/my-project
libertai code                        # interactive agent session in this repo
```

That drops you into a full-screen TUI: type a prompt, the agent reads and
greps the repo, proposes edits and shell commands, and asks before each
mutating call. `Shift+Tab` cycles the permission mode, `/help` lists the
slash commands, `Ctrl+D` exits.

Other ways in:

```sh
libertai code "add a --dry-run flag to the exporter"   # one-shot turn, then exit
libertai code -p "list the public API of src/lib.rs"    # headless: stdout only
libertai code --plan                                    # start read-only
libertai code --continue                                # resume this repo's last session
libertai code --bg "port the tests to the new fixture"  # detached run
libertai agents                                         # dashboard of running sessions
```

## `libertai code`

A coding agent built on [`pi_agent_rust`](https://github.com/Dicklesworthstone/pi_agent_rust)
as a linked library — no Node runtime, no subprocess, one binary. It defaults
to `glm-5.2-thinking` on the `libertai` provider; both are configurable and
switchable mid-session with `/model`.

### Tools

The agent gets pi's built-in tools plus LibertAI's own:

| Group | Tools |
| --- | --- |
| Files | `read`, `write`, `edit`, `hashline_edit`, `ls`, `find`, `grep` |
| Shell | `bash`, `bash_output`, `kill_bash` |
| Notebooks | `notebook_read`, `notebook_edit`, `notebook_execute` |
| Web | `search`, `fetch` (search needs an API key; `fetch` is a plain HTTP GET) |
| Images | `generate_image` |
| Planning | `todo`, `context_status`, `request_compaction` |
| Delegation | `task` (subagent), `spawn_team`, `workflow`, `team_task`, `mailbox`, `send_message` |
| Scheduling | `cron_create`, `cron_list`, `cron_delete` |
| Interaction | `ask_user`, `push_notification` |
| Extensibility | `skill`, `mcp_call`, `mcp_read_resource`, `mcp_get_prompt`, `tool_search` |
| Contracts | `structured_output` |

Every mutating tool is wrapped in an approval layer, then in loop guardrails
(which catch a model repeating the same failing call) and path-safety checks
(which refuse writes to sensitive paths outside the safe root).

### Permission modes and approvals

Four modes, cycled with `Shift+Tab` or set with `/mode` — or up front with
`--mode normal|accept-edits|plan` (`--plan` is shorthand for the last):

- **normal** — read-only tools run freely; `bash`, `write`, `edit`, and
  `hashline_edit` prompt with a preview. You can allow once, allow always
  (a rule saved to disk; clear it with `/forget`), or deny with a reason
  that is fed back to the model so it can course-correct.
- **accept-edits** — file edits auto-apply; `bash` still prompts.
- **plan** — every mutating tool is auto-denied. The agent can only read,
  grep, find, and list. Good for "tell me what you'd do" turns.
- **bypass** — `--dangerously-skip-permissions` (or
  `LIBERTAI_DANGEROUSLY_SKIP_PERMISSIONS=1`) auto-allows everything. This
  is genuinely dangerous: the model can run any command and rewrite any
  file with no gate. It is refused outright in `--print`, `--bg`, and
  background teammates unless you have already accepted the risk once in
  an interactive session (a consent sentinel is written then).

An optional second-opinion layer (`libertai config set
smart_approval_enabled true`) asks a small fast model to pre-judge each
approval; only a clean verdict skips the prompt, anything ambiguous still
escalates to you. Off by default.

`/undo` reverts the most recent edit the agent made.

### Sandbox

`--sandbox off|strict|auto` (or `LIBERTAI_SANDBOX`) controls the `bash` tool:

- `off` (default) — bash runs with your full host privileges.
- `strict` — bash is wrapped in `bwrap` with **no network**, read-only
  system directories, a tmpfs `/tmp`, the current directory read-write, and
  a cleared environment (only `PATH`, `HOME`, `TERM`, `LANG` are set back).
  Linux only today; macOS and Windows are deliberate follow-ups. If you ask
  for `strict` on a host that cannot deliver it, the session refuses to
  start rather than silently running unsandboxed.
- `auto` — resolves per pillar; on the CLI that is currently the same as
  `off`.

`libertai sandbox info [--json]` prints the resolved profile for your host —
which bin/lib/config paths would be bound, which are missing, where `bwrap`
is, and the inside-sandbox `PATH`. Use it when something the model wants to
run isn't reachable under `--sandbox=strict`.

### Sessions

Sessions persist as JSONL under pi's session store
(`~/.pi/agent/sessions/<encoded-cwd>/`, overridable with `PI_SESSIONS_DIR`).

```sh
libertai code --continue                 # most recent session for this cwd
libertai code --resume                    # interactive picker of recent sessions
libertai code --resume path/to.jsonl      # a specific one
libertai code --list-sessions [--all] [--json]
```

`--resume` / `--continue` compose with `--print`, so you can run one more
headless turn against a saved conversation.

Long conversations auto-compact (on by default; tune with
`code_compaction_*` config keys, or disable with
`code_auto_compaction_enabled = false`). `/compact` forces it now, optionally
with notes on what to preserve. The agent can also inspect its own context
budget with `context_status` and ask for a compaction with
`request_compaction`.

### Project context, memory, and skills

- **`AGENTS.md` / `CLAUDE.md`** — walked from the current directory up
  through its ancestors and injected into the system prompt.
- **Per-project memory** — dated notes kept at
  `~/.config/libertai/projects/<encoded-cwd>/MEMORY.md`, loaded into the
  prompt. `/memory` shows the current file.
- **Skills** ([agentskills.io](https://agentskills.io/specification)
  format — a directory with a `SKILL.md`) are discovered from
  `.claude/skills/`, `.libertai/skills/`, and `.agents/skills/` in the
  project, plus `~/.claude/skills/` and `~/.config/libertai/skills/`.
  Names and descriptions go into the prompt; the `skill` tool loads a
  body on demand. `/skills` lists what's active.
- **Custom slash commands** — Markdown templates in `.claude/commands/`,
  `.libertai/commands/`, `.liberclaw/commands/` (project) or the same
  under `~` / `~/.config/libertai/` (user). Namespaced by subdirectory
  (`/project/my-command`). `/reload` re-discovers them.
- **Named sub-agents** — Markdown definitions in `.claude/agents/`,
  `.libertai/agents/`, `~/.claude/agents/`, `~/.config/libertai/agents/`.
  Their `tools:` / `model:` frontmatter is honoured. Run a whole session
  as one with `libertai code --agent <name>`.

### Hooks

Shell commands, HTTP endpoints, stdio MCP tools, and prompt handlers can be
attached to session events via a `[hooks]` table in the config file. The
event names match Claude Code's: `UserPromptSubmit`, `PreToolUse`,
`PostToolUse`, `PostToolUseFailure`, `PostToolBatch`, `SubagentStart`,
`SubagentStop`, `PreCompact`, `PostCompact`, `SessionStart`, `Stop`,
`SessionEnd`, `Notification`, `TeammateSpawn`, `TaskComplete`,
`TeamComplete`.

A `PreToolUse` hook can deny a tool call; a `UserPromptSubmit` hook can
block a prompt or append context to it. `/hooks` shows the configuration
and per-event diagnostics.

### MCP clients

Servers declared under `[mcpServers.<name>]` in the config file (stdio or
Streamable HTTP) are exposed to the agent. Below a threshold their tools are
registered individually as `mcp__<server>__<tool>`; above it the agent
instead gets a generic `mcp_call` bridge plus a `tool_search` tool, so a
large MCP surface doesn't bloat the system prompt. `/mcp` shows status and
can probe servers for live diagnostics.

(This is separate from `libertai mcp`, which makes *this* CLI an MCP server —
see [MCP server](#mcp-server).)

### Delegation: subagents, teams, workflows

- **`task`** runs a focused subtask in an isolated child session. Read-only
  by default (`read`/`grep`/`find`/`ls`); a named sub-agent's `tools:`
  frontmatter can opt into mutating tools, in which case the child defaults
  to worktree isolation and still goes through approvals. Recursion is
  capped at depth 3.
- **`spawn_team`** starts several background teammates on related sub-tasks,
  sharing a task list (`team_task`) and inter-agent messaging (`mailbox` /
  `send_message`) under `.libertai/teams/<team>/`. `/team` does the same
  from the prompt; run a teammate by hand with `--team <name> --teammate <who>`.
- **`workflow`** runs a JavaScript orchestration script in an embedded
  QuickJS sandbox, with host functions `agent()`, `parallel()`,
  `pipeline()`, `phase()`, and `log()`. Phases show up live in the TUI's
  workflow tree and in `/workflows`. An `agent()` call can take a JSON
  Schema and resolve with a validated object instead of prose. Wall-clock
  capped (default 300s, `LIBERTAI_WORKFLOW_TIMEOUT_SECS`).
- **`cron_create` / `cron_list` / `cron_delete`** (also `/schedule`)
  schedule a prompt against a 5-field cron expression; a timer thread in
  the TUI injects it as a turn when due. Session-scoped today — jobs do not
  survive a restart.

### Background runs and the agents dashboard

```sh
libertai code --bg "migrate the config loader" --name config-migration
libertai agents
```

`--bg` spawns a detached session, prints a run id, and returns to the
shell. `libertai agents` is one screen for everything in flight —
background runs, sessions backgrounded with `/agent`, and teams: what's running,
what needs input, what's done. You can dispatch new sessions from its
input, peek at output without attaching, and attach when one needs you.
`libertai agents --json` gives the same listing as machine-readable data
and exits without a TUI. Run records live in
`~/.config/libertai/code-background-agents/runs.jsonl`.

### Headless use

`--print` / `-p` runs a single turn with no TUI and no interactive prompts.
Assistant text streams to stdout; turn and tool noise goes to stderr. Any
tool call not already covered by a saved allow rule is auto-denied rather
than prompting, so scripts never hang. The prompt comes from the trailing
args, from piped stdin, or both (stdin becomes context above the args).

```sh
libertai code -p "summarize what changed in the last commit"
git diff | libertai code -p "review this diff for correctness bugs"
libertai code --continue -p "now write the changelog entry"
```

The `structured_output` tool lets a headless run return schema-validated
JSON instead of prose: the model calls it with a JSON Schema and a value,
and the tool validates before accepting (with a per-session retry cap so a
model cannot spin forever on a schema it can't satisfy).

### TUI

Full-screen [ratatui](https://ratatui.rs) interface: streaming markdown with
syntax-highlighted code blocks, inline diffs for proposed edits, an approval
modal, a live agents panel, a workflow tree, and a scrollable transcript.

Slash commands (`/help` lists them, `/` opens a filterable palette):

```
/exit /quit /help /clear /new /mode /permissions /plan /model /skills /memory
/review /security-review /mention /ide /hotkeys /theme /vim /bug /hooks /mcp
/forget /undo /notify /notifications /usage /cost /doctor /compact /changelog
/tree /diff /output /commit /pr_comments /pr-comments /copy /status /statusline
/statusline-command /output-style /history /queue /reload /team /agent /agents
/workflows /image /attach /schedule
```

`/image` and `/attach` are advertised but not yet implemented in the TUI —
they print a "not yet supported" line rather than silently failing.

`!<cmd>` runs a shell command inline, `!!` repeats the last one, and `@` at
a word boundary autocompletes a file whose contents attach to the prompt.
`/hotkeys` prints the full key map; the essentials are `Shift+Tab` (mode),
`Tab` (agents panel focus), `PageUp`/`PageDown` (scroll), `Alt+Enter` or
`\`+`Enter` (newline), `Ctrl+O` (edit the prompt in `$EDITOR`), `Esc` (stop
the running turn), `Ctrl+C` (clear the line / interrupt), `Ctrl+D` (exit).

### Coming from Claude Code

`libertai import claude-code` reads Claude Code's own transcripts from
`~/.claude/projects/`:

```sh
libertai import claude-code list [--all] [--json]      # discover sessions
libertai import claude-code show <uuid|path>            # preview as plain text
libertai import claude-code summarize <uuid|path>       # /compact-style summary
libertai import claude-code import <uuid|path>          # write a resumable session
```

`import` summarises the Claude Code session with your configured chat model
and writes a new session file whose first entry is that summary as a
compaction checkpoint. It prints the path — open it with
`libertai code --resume <path>` or pick it from the session picker.

## Commands

| Command | Description |
| --- | --- |
| `libertai code [prompt]` | **The coding agent.** No prompt → interactive TUI; a prompt → one-shot turn. `--model`, `--provider`, `--plan`, `--mode`, `--resume`, `--continue`, `--list-sessions [--all] [--json]`, `--sandbox`, `--print/-p`, `--bg`, `--name`, `--agent`, `--team`/`--teammate`, `--dangerously-skip-permissions`. |
| `lcode [prompt]` | Alias binary for `libertai code` (Cargo installs only). Covers all of the above except `--bg`/`--name`/`--agent`/`--team`/`--teammate`. |
| `libertai agents` | Dashboard for background agent sessions and teams. `--cwd`, `--json`, `--model`, `--permission-mode`, `--agent`. |
| `libertai sandbox info` | Print the resolved strict-sandbox profile for this host. `--json`. |
| `libertai import claude-code list\|show\|summarize\|import` | Bring a Claude Code transcript into a resumable LibertAI session. |
| `libertai login` | Interactive login: browser SSO (recommended) or paste an API key. |
| `libertai logout` | Clear saved credentials (backs up the config to `config.toml.bak.<epoch>`). |
| `libertai status` | Current auth state and default models. `--json`. |
| `libertai usage` | Plan tier, rolling allowance windows (5h + weekly), and prepaid credits. `--json`. |
| `libertai models` | List models from `/v1/models`. `--json`; `--refresh` re-syncs the persisted catalog so new models become selectable in `libertai code`'s `/model`. |
| `libertai keys list\|create\|delete` | Manage your account's API keys. `list --json`. |
| `libertai ask <prompt>` | One-shot, non-streaming completion. `--model`, `--image` (repeatable; local file or URL, needs a vision model). |
| `libertai chat` | Streaming chat REPL with history. `--model`, `--system`. |
| `libertai search <query>` | Web search via `search.libertai.io`. `--max-results`, `--type web\|news\|images`, `--engines`, `--json`. |
| `libertai fetch <url>` | Fetch a URL and return its cleaned article text. `--json`. |
| `libertai image <prompt>` | Generate and save images. `--n`, `--size`, `--out`, `--model`, `--force`. |
| `libertai mcp` | Run an MCP server over stdio exposing `web_search` + `fetch_page` — see [MCP server](#mcp-server). |
| `libertai config show\|path\|set\|unset` | Inspect or edit `~/.config/libertai/config.toml`. |
| `libertai skills list\|install\|uninstall` | Manage the bundled skills that teach *third-party* agents to call `libertai`. |
| `libertai plugin …` | Install and manage plugins from marketplaces (Claude-Code-compatible format) — see [Plugins](#plugins). |
| `libertai completions <shell>` | Print a bash/zsh/fish/… completion script to stdout. |
| `libertai run -- <cmd>` | Exec any command with LibertAI env vars injected. |
| `libertai claude\|opencode\|aider\|claw\|hermes [args]` | Launch someone else's agent against LibertAI — see [below](#running-other-agents-on-libertai). |

### Scripting

- **`--json`** — `status`, `usage`, `models`, `keys list`, `search`, `fetch`,
  `agents`, `sandbox info`, `code --list-sessions`, and `import claude-code
  list` / `show` emit machine-readable JSON. JSON is the *only* thing
  written to stdout; progress notes and human extras go to stderr.
- **`models --json`** — keeps the `/v1/models` wire fields (`id`,
  `owned_by`) and, when LibertAI's public model catalog is reachable
  (fetched from an Aleph aggregate, cached on disk for 24h), adds a
  `catalog` object per text model: `name`, `hfId`, `contextWindow`,
  `vision`, `reasoning`, `tee`, `functionCalling`, `inputUsdPerMtok`,
  `outputUsdPerMtok`. For alias/deprecated/`-thinking` ids the metadata
  comes from the base entry and `resolvedId` replaces `name`/`hfId`.
  Offline, the `catalog` key is simply absent.
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

## Running other agents on LibertAI

Separate from `libertai code`: if you already live in someone else's coding
agent, these presets point it at LibertAI's backend so you keep the UX you
know but run open models on confidential inference. They are launchers, not
the product — the agent, its prompt, and its behaviour are entirely the
third party's.

```sh
libertai claude                         # Claude Code
libertai claude --opus glm-5.2          # override a single tier
libertai opencode --model libertai/glm-5.2
libertai aider
libertai claw
libertai hermes
libertai run -- <any command>           # generic env-var injection
```

| Launcher | What it does |
| --- | --- |
| `libertai claude` | Sets `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`, remaps the opus/sonnet/haiku tiers to LibertAI models, pins the main and subagent models, disables telemetry and non-essential traffic, then execs `claude`. `--model`, `--opus`, `--sonnet`, `--haiku`. |
| `libertai opencode` | Writes an idempotent `provider.libertai` block into `~/.config/opencode/opencode.json` (pointing at `<api_base>/v1`, key via `{env:LIBERTAI_API_KEY}`), exports the key, and appends `--model libertai/<default_code_model>` unless you passed your own. Other keys and providers in that file are preserved. |
| `libertai aider` | `run` preset for [Aider](https://aider.chat); auto-passes `--model openai/<default_code_model>` and `--read ~/.config/libertai/aider-instructions.md`. |
| `libertai claw` | `run` preset for [Claw Code](https://github.com/ultraworkers/claw-code); auto-passes `--model openai/<default_code_model>` — claw's Anthropic path doesn't strip a routing prefix, so the OpenAI-compatible route is the one that works against a LibertAI backend. |
| `libertai hermes` | Injects LibertAI credentials plus `LIBERTAI_MODEL` and execs [Hermes Agent](https://hermes-agent.nousresearch.com). |
| `libertai run -- <cmd>` | The generic form: sets `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_API_BASE`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN` (and `LIBERTAI_MODEL` with `--model`), then execs. |

`libertai claude`'s three tiers come from `launcher_defaults.*` in the config;
the other four launchers all default to `default_code_model`.

### Bundled skills for third-party agents

Third-party agents pointed at LibertAI have no image-generation or web-search
tool of their own (`libertai code` has these natively). The CLI bundles two
skills that teach them to shell out to `libertai`:

- **`libertai-image`** — run `libertai image "<prompt>" --out <path>` when
  the user asks for a picture, logo, mockup, etc.
- **`libertai-search`** — run `libertai search "<query>" [--type news|images]`
  for fact-checks and research, and `libertai fetch "<url>"` to read a page.

`libertai claude` installs them into `~/.claude/skills/` on first launch;
`libertai opencode` installs them into `~/.config/opencode/skills/` so it
never touches your Claude setup. Both are non-destructive — existing files
are left alone so customisations survive. Aider has no skill system, so
`libertai aider` writes `~/.config/libertai/aider-instructions.md` and passes
`--read <that file>`. Claw has no skill reader today, so the image/search
skills aren't available inside a claw session.

Manual control:

```sh
libertai skills list                 # show what's bundled
libertai skills install              # force-refresh into ~/.claude/skills/
libertai skills install --project    # into ./.claude/skills/ for this repo
libertai skills uninstall
```

## Plugins

`libertai code` loads plugins in the Claude-Code-compatible plugin format.
A plugin can contribute slash commands, agents, skills, hooks and MCP servers.
Plugins come from *marketplaces* — git repos or local directories you add
yourself; nothing is enabled by default.

```sh
libertai plugin marketplace add <url-or-path>   # register a source
libertai plugin list                            # installed plugins + state
libertai plugin audit <name>                    # capabilities + scan, no install
libertai plugin install <name>[@marketplace]    # install (prompts before trusting code)
libertai plugin enable|disable <name>
libertai plugin remove <name>
```

Plugin code does not run until you trust it. `install` reports the
capabilities a plugin requests and prompts before activating anything that
executes — its hooks and MCP servers. `--trust` skips that prompt and
`--yes` answers the non-code prompts non-interactively; `--yes` alone never
auto-trusts executable components. `audit` shows the same report without
installing, and `--scan` / `--no-scan` control the external security scan.

Publishers can sign a plugin directory with `libertai plugin sign <path>`
(key from the argument or `$LIBERTAI_SIGNING_KEY`). Signatures and, where a
marketplace is hosted on GitHub, verified-commit metadata act as identity
anchors, so you can require signed plugins rather than trusting a name.

Inside the REPL, `/plugin` (alias `/plugins`) lists installed plugins;
managing them is done from the `libertai plugin` CLI.

## MCP server

`libertai mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio, exposing two tools backed by LibertAI's search API:
`web_search` (multi-engine web/news/images/academic search with snippets,
URLs, and cross-engine consensus info) and `fetch_page` (fetch a URL as
cleaned plain text). Any MCP client can use them — point it at the installed
binary and you're done. Auth reuses your CLI credentials (`libertai login`)
or a `LIBERTAI_API_KEY` env var; without a key the tools answer with setup
instructions instead of failing.

**Claude Code**

```sh
claude mcp add libertai -- libertai mcp
```

**Generic JSON config** (Claude Desktop `claude_desktop_config.json` and most
other clients):

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

(The `env` block is only needed if you haven't run `libertai login` on that
machine.)

## Config

`~/.config/libertai/config.toml` (permissions `0600`, parent dir `0700`).
Fields that match the built-in default are omitted from the saved file, so a
key you never touched tracks future upgrades automatically.

```toml
api_base            = "https://api.libertai.io"
account_base        = "https://api.libertai.io"
default_chat_model  = "glm-5.2"
default_code_model  = "glm-5.2-thinking"
default_code_provider = "libertai"
default_image_model = "z-image-turbo"
http_timeout_secs   = 600
check_for_updates   = true

# `libertai code` behaviour
smart_approval_enabled            = false
smart_approval_model              = "glm-5.2"
code_auto_compaction_enabled      = true
code_turn_notifications           = false
status_line_template              = ""

# tiers for the third-party launchers
[launcher_defaults]
opus_model   = "glm-5.2"
sonnet_model = "glm-5.2"
haiku_model  = "qwen3.6-35b-a3b"

# MCP servers exposed to `libertai code`
[mcpServers.filesystem]
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "/srv/data"]

# session hooks
[[hooks.PreToolUse]]
matcher = "bash"
command = "./scripts/audit-bash.sh"

[auth]
api_key = "LTAI_..."
```

Set and reset values with:

```sh
libertai config set default_code_model glm-5.2-thinking
libertai config set launcher_defaults.opus_model gemma-4-31b-it
libertai config unset default_chat_model
libertai config unset launcher_defaults          # all three launcher tiers
libertai config unset all                        # every non-auth field
```

## Authentication

`libertai login` offers two flows:

1. **Browser sign-in (recommended)** — opens the console, you sign in (email,
   wallet, or OAuth) and approve; the CLI gets a device key (90-day expiry,
   re-run to renew). Standard OAuth loopback + PKCE.
2. **API key** — paste a key from [console.libertai.io](https://console.libertai.io).

Set `LIBERTAI_CONSOLE_URL` to use a non-default console.

## Security notes

- Credentials live on disk at `~/.config/libertai/config.toml` in plaintext
  (file mode `0600`, parent dir `0700`). OS keyring storage is on the
  roadmap.
- The **launchers** (`run` / `claude` / `opencode` / `aider` / `claw` /
  `hermes`) inject the API key into the child process's environment. Any
  subprocess that reads env vars, and any diagnostic tool that can enumerate
  that process, can see the key. That is the only way those third-party
  tools can authenticate today; if you don't want the tradeoff, use
  `libertai code` or `ask` / `chat` / `image` directly.
- `libertai code` does not spawn a separate agent binary, but it does export
  `LIBERTAI_API_KEY` into **its own** process environment so the embedded pi
  model registry can resolve it (`~/.pi/agent/models.json` stores only the
  `env:LIBERTAI_API_KEY` indirection, never the secret). Every command the
  `bash` tool runs under `--sandbox=off` inherits that environment.
  `--sandbox=strict` runs bwrap with `--clearenv`, so the sandboxed shell
  sees only `PATH`, `HOME` (pointed at the cwd), `TERM`, and `LANG`.
- The agent runs shell commands on your machine. Default `--sandbox=off`
  means `bash` has your privileges; `--sandbox=strict` (Linux) confines it.
  `--dangerously-skip-permissions` removes the approval gate entirely —
  treat it as "I have read the diff of everything this session will do".
- MCP servers and hooks you configure execute arbitrary local commands with
  your privileges. Only configure ones you trust.
- The `account_base` the account commands talk to is user-configurable — if
  you change it, you are trusting that host.
- HTTPS is enforced for `api_base` and `account_base`; `http://` URLs are
  rejected at config load.

## Development

```sh
cargo build                      # debug
cargo build --release            # optimized `libertai` + `lcode` binaries
cargo test                       # 1255 tests, offline + deterministic
./target/release/libertai --help
```

The suite includes black-box probes (`tests/probes_*.rs`, via `assert_cmd`)
that spawn the built binary and assert on its stdout/stderr, plus an offline
workflow-engine selftest that runs a script through the real QuickJS engine
with no LLM, session, or terminal. Tier-2 LLM-judge probes call LibertAI's
free chat model to evaluate output-shape properties; they are off by default
so plain `cargo test` stays offline. Run them with
`cargo test --features tier2-probes -- --include-ignored`.

Shell completions and the man page under `packaging/` are generated from the
same clap definitions (`packaging/generate-assets.sh`) and pinned by
`tests/probes_completions.rs` — regenerate them after changing the CLI
surface.

### Design and planning docs

`docs/` holds internal planning material rather than user documentation, and
each file is a dated snapshot rather than a live description of the code:

| File | What it is |
| --- | --- |
| `docs/distribution.md` | Release/update system design plus a one-time manual-setup checklist (2026-06-16). The checklist is not kept in sync with what has since been provisioned — the apt repo, brew tap, and release workflow referenced in [Install](#install) are live. |
| `docs/liberclaw-code-subcommand.md` | Why `libertai code` is built on `pi_agent_rust` (decision record, 2026-04-24). |
| `docs/crack-the-code-comparison.md` | Source-level gap analysis vs Claude Code and Codex (2026-06-28). |
| `docs/overhaul-plan.md` | The milestone plan derived from that comparison. |
| `docs/parity-roadmap.md` | Item-by-item Claude Code parity tracking (2026-05-31). |
| `RATATUI_MIGRATION.md` | The plan for the ratatui TUI rewrite. Shipped; carries a banner listing what has drifted since. |

## Roadmap

- OS keyring storage as an alternative to the plaintext TOML file.
- `--sandbox=strict` on macOS (`sandbox-exec`) and Windows.
- Durable cron jobs (`.libertai/scheduled_tasks.json`) so scheduled prompts
  survive a restart.
- Image/file attachment in the TUI (`/image`, `/attach`).
- Ship `lcode` in the release archives, `.deb`, and brew formula (today it
  only reaches users who install via Cargo).
- OpenCode MCP bridge (expose image/search as MCP tools in opencode's tool
  list).
