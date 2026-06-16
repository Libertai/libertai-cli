# LibertAI MCP server — LLM installation guide

This file lets an AI agent (e.g. the Cline marketplace installer) set up the
LibertAI MCP server unassisted. The server is the `mcp` subcommand of the
`libertai` CLI binary — there is no separate package to install.

It exposes two tools over stdio:

- `web_search(query, search_type?, engines?, max_results?)` — multi-engine
  web / news / images / academic search via LibertAI's search API.
- `fetch_page(url)` — fetch a URL and return its cleaned plain-text content.

## Step 1: Install the `libertai` binary

Pick the first method that fits the platform:

```sh
# Linux / macOS / WSL — no sudo, installs to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/Libertai/libertai-cli/master/packaging/install.sh | sh

# Debian / Ubuntu (system-wide)
curl -fsSL https://apt.libertai.io/install.sh | sudo bash

# macOS (Homebrew)
brew install Libertai/tap/libertai

# Any platform with a Rust toolchain
cargo install --git https://github.com/Libertai/libertai-cli --branch master --locked
```

Windows: download `libertai-windows-x86_64.exe` from
<https://github.com/Libertai/libertai-cli/releases/latest>, rename it to
`libertai.exe`, and place it on the `PATH`.

Verify: `libertai --version` prints a version string. If the shell cannot
find it, the one-liner installs to `~/.local/bin` — use the absolute path
`~/.local/bin/libertai` in the MCP config below instead.

## Step 2: Get an API key

The `web_search` tool needs a LibertAI API key (`fetch_page` works without
one). Either:

1. **Browser sign-in (preferred when a human is present):** run
   `libertai login` and follow the browser flow. The key is stored in
   `~/.config/libertai/config.toml`; no env var is needed afterwards.
2. **Console:** create a key at <https://console.libertai.io> (Sign in →
   API keys → create), or — if already logged in on this machine — run
   `libertai keys create <name>`. Keys look like `LTAI_...`. Pass the key
   to the server via the `LIBERTAI_API_KEY` environment variable in the
   MCP config.

If no key is configured, the server still starts and responds; `web_search`
returns setup instructions instead of results. So it is safe to finish the
MCP configuration first and add the key later.

## Step 3: Configure the MCP client

The server command is `libertai` with the single argument `mcp`
(stdio transport).

**Cline** — add to `cline_mcp_settings.json`:

```json
{
  "mcpServers": {
    "libertai": {
      "command": "libertai",
      "args": ["mcp"],
      "env": { "LIBERTAI_API_KEY": "LTAI_..." },
      "disabled": false,
      "autoApprove": ["web_search", "fetch_page"]
    }
  }
}
```

Omit the `env` block if `libertai login` was used in step 2.

**Claude Code:**

```sh
claude mcp add libertai -- libertai mcp
```

**Claude Desktop / Cursor / generic clients:**

```json
{ "mcpServers": { "libertai": { "command": "libertai", "args": ["mcp"] } } }
```

## Step 4: Verify

Send an initialize request manually:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"verify","version":"0"}}}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | libertai mcp
```

Expected: two JSON lines — an `initialize` result with
`"serverInfo":{"name":"libertai", ...}` and a `tools/list` result naming
`web_search` and `fetch_page`. In the MCP client, the `libertai` server
should show both tools; a `web_search` call for "latest rust release"
returns numbered results with titles, URLs, and snippets.

Docs: <https://docs.libertai.io>
