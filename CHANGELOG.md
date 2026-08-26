# Changelog

All notable changes to `libertai-cli` are documented here.

The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/) with 0.x
semantics: the minor number moves for feature batches and user-visible
behaviour changes.

## [Unreleased]

### Added

- **Agent Client Protocol (ACP) mode** — `libertai code --acp` (also `lcode
  --acp`) serves the coding agent to an editor over line-delimited JSON-RPC
  2.0 on stdio, reaching Zed and the JetBrains IDEs. The session is built
  from the same LibertAI auth, provider registration and model resolution as
  a normal `libertai code` run, so an editor session starts on LibertAI's
  confidential (TEE-backed) inference; tool approvals travel over the protocol
  as `session/request_permission`, and the model can be changed mid-session
  via `session/set_model`. The editor's picker lists every model in pi's
  registry, not only LibertAI's, so selecting another configured provider
  routes that session away from LibertAI. ACP sessions are in-memory only:
  they do not appear in `--list-sessions` and cannot be resumed. Setup
  instructions for both editors are in the README.
- **Stdout discipline for stdio-protocol commands** — the startup update
  check is now skipped outright for `libertai mcp` and `libertai code --acp`
  (`cli::is_stdio_protocol_command`), so nothing can put a non-protocol byte
  on the wire. Covered by `tests/probes_acp.rs`, which spawns the real binary,
  runs an `initialize` + `session/new` handshake, and asserts stdout is
  byte-clean JSON-RPC.

## [0.5.0] - 2026-08-26

First release cut from `master` since v0.4.1. Releases v0.4.2 through v0.4.5
were tagged on a side branch that has since been deleted; every substantive fix
from that line (per-platform native TLS, the OpenCode config path, the
catalog-driven OpenCode model list, the OpenCode skills directory, and
`ask --image`) is included here. This is a minor bump rather than a patch bump
because it adds a new top-level subcommand, a scripting engine, and a rewritten
interactive UI.

### Added

#### Plugins

- **`libertai plugin`** — install and manage plugins in the Claude-Code-compatible
  plugin format, from marketplaces you add yourself (git URL or local path).
  Subcommands: `marketplace add|list|remove`, `list`, `audit`, `install`,
  `enable`, `disable`, `remove`, `sign`.
- Plugins can contribute slash commands, agents, skills, hooks and MCP servers;
  enabled plugins are loaded into the extension registries at startup.
- **Trust is explicit.** Executable components (hooks, MCP servers) do not run
  until trusted. `install` reports requested capabilities and prompts before
  activating them; `audit` produces the same report without installing. `--yes`
  answers non-code prompts non-interactively and never auto-trusts code;
  `--trust` is the explicit opt-in. `--scan` / `--no-scan` control the external
  security scan.
- **Publisher identity** — `libertai plugin sign` signs a plugin directory in
  place; plugin digests are symlink-safe, and for GitHub-hosted marketplaces a
  verified commit acts as an additional identity anchor, so installs can require
  a verified publisher rather than trusting a name.
- `/plugin` (alias `/plugins`) in the REPL lists installed plugins.

#### Workflow engine

- **JavaScript workflow engine** for `libertai code` — workflows run in an
  embedded QuickJS sandbox (`rquickjs`) and are exposed to the agent through a
  `Workflow` tool and a `WorkflowRegistry` threaded through the tool factory.
- **Background workflows** that keep running while the session continues, with
  a `<task-notification>` delivered on completion.
- **Live progress tree** for a running workflow, plus an enriched `/workflows`
  command listing registered and in-flight workflows.
- **Per-session tool policy, named agents, and `wf:` labels**, with a schema for
  workflow definitions.
- **Offline engine selftest** and probe coverage, a final log flush on exit, and
  workflow documentation.

#### Agent capabilities

- **Session cron** — `cron_create` / `cron_list` / `cron_delete` tools backed by
  a timer thread, so an agent can schedule work inside a session.
- **Team tasks** — a `team_task` dependency graph with `blocks` / `blockedBy`
  edges and an owner per task.
- **Structured output** — a `structured_output` tool that validates against a
  caller-supplied JSON Schema.
- **Context tools** — `context_status` to inspect context pressure and
  `request_compaction` to trigger compaction deliberately; auto-compaction now
  distinguishes a context-limit stop from an output-cap `Length` stop.
- **Push-based messaging** — a `send_message` tool with a parent inbox poll.
- **Skill tool and latent skill registry** — skill bodies stay out of the system
  prompt until invoked.
- **Deferred MCP tool loading** — `tool_search` loads MCP tool schemas on
  demand instead of putting every schema in the prompt.
- **`PostCompact` hooks** and compaction metrics.

#### `libertai usage`

- **New `libertai usage` subcommand** reporting credit and subscription usage,
  authenticated with a refresh token.
- Refresh-token sessions: `Auth.refresh_token` persisted at login, session
  refresh/revoke against the API, and `libertai logout` now revokes the session
  server-side and scrubs the stored token.
- `libertai status` reports whether a refresh-token session is present.

#### Login

- **Paste-code fallback and QR code** for signing in when the browser is on a
  different machine than the CLI.
- The paste-key option is labelled as inference-only, since it cannot open a
  full session.

#### `libertai ask`

- **`--image` attachments** — local files are base64-inlined, URLs are passed
  through, and the model catalog gates on vision support.

### Changed

#### TUI overhaul (`libertai code`)

- **Soft-wrap input editor** with exact cursor placement and viewport scrolling.
- **Kitty keyboard protocol** support, giving a real `Shift+Enter` newline, with
  key-release events filtered out.
- **Large-paste collapse** — pasted blocks become `[Pasted text #N +M lines]`
  placeholders instead of flooding the transcript.
- **Backslash-`Enter` line continuation** for terminals without kitty support.
- **Inline `@`-mention file autocomplete**, expanded into attachments at submit
  time.
- **Multi-line keymap hint row** and a footer layout budget that keeps hotkeys
  honest at every terminal width.
- Approval and ask modals: arrow-key navigation, bottom-left popups, and
  reliable visibility.
- Rendering: per-entry markdown render cache, unclosed-fence holdback,
  `syntect` syntax highlighting for fenced code, a diff viewer with gutter, line
  numbers and change counts, and a tool-result exit glyph with an `/output`
  expand overlay.
- Transcript follow-mode, correct rendering of System entries, and general
  render polish.
- Queued-command routing: abort holds the queue, plus `/queue` and `/reload`.
- Paste gating, dispatch ordering, and `Ctrl+C` / `Ctrl+J` footgun fixes.
- The todo tool renders as a pinned overlay routed through the agent message
  channel.

#### Approvals

- Per-call approval scopes: `Prefix`, `GrantRoot`, and `Domain`.
- `Mode::Bypass` behind `--dangerously-skip-permissions`, with a one-time
  consent prompt.
- `bash_command_wrapper` is threaded through to subagents.
- Session-scoped approvals.

#### Agent harness

- Prompt posture batch: act by default, drive to completion, no filler openers,
  investigate before asking, task continuity, lead with the outcome, adjust when
  denied (with prompt-injection flagging), and no echo-chaining. `generate_image`
  is classified as a mutating tool.
- UX: live elapsed time, spinner verbs, a pre-compaction warning, and four hook
  events.
- The double tool-call loop guardrail thresholds (warn/halt) were doubled.

#### OpenCode

- Catalog-driven model entries with filtering, cost, context window, and
  reasoning metadata.
- Bundled skills install into OpenCode's own skills directory rather than
  `~/.claude`.

### Fixed

- **TLS behind intercepting proxies** — the CLI now uses the OS native root
  store, with per-platform `native-tls` selection, so it works behind corporate
  TLS-interception. Restored `webpki-roots`, which `asupersync` 0.3.4 requires.
- The workflow engine's script evaluation, which previously turned every script
  into a `SyntaxError`.
- The subagent skill tool scans the parent working directory rather than the
  worktree.
- Stale sessions are cleared on paste login; non-interactive `usage` fails fast.
- A flaky detached-hook test.

### Build and CI

- `pi_agent_rust` fork synced onto upstream `329c1f9b`; the repo now pins
  `nightly-2026-02-19` in `rust-toolchain.toml` in lockstep with the fork.
- The release workflow installs that pinned toolchain (and cross-compilation
  targets against it) instead of `stable`, which `rust-toolchain.toml` silently
  overrode.
- The release workflow now refuses a tag that is not an ancestor of the default
  branch — the failure mode that produced the v0.4.2..v0.4.5 orphan tags.
- The `.deb` is packaged from the prebuilt Linux binary instead of recompiling.
- Caching via `Swatinem/rust-cache`; the redundant release build in `verify` was
  dropped.
- Dependency bumps: `rquickjs` 0.11 → 0.12 (aligned with the `pi_agent_rust`
  fork so QuickJS compiles once), `tui-textarea-2` 0.11 → 0.12, `dirs` 5 → 6,
  `indicatif` 0.17 → 0.18, `clap_mangen` 0.2 → 0.3, `rand` 0.9 → 0.10,
  `sha3` 0.10 → 0.12, `actions/checkout` 6 → 7, `actions/cache` 5 → 6.

### Documentation

- README rewritten around `libertai code`.

## [0.4.5] - 2026-07-16

Tagged on a release branch that is no longer reachable from `master`.

- OpenCode: bundled skills install to OpenCode's own skills directory.
- `ask --image`: local files base64-inlined, URLs passed through, catalog vision
  gate.

## [0.4.4] - 2026-07-15

- OpenCode: catalog-driven model entries — filter, cost, context, reasoning.

## [0.4.3] - 2026-07-15

- OpenCode config path fix, native TLS, CI caching.

## [0.4.2] - 2026-07-15

- TLS: per-platform native trust store.

## [0.4.1] and earlier

See the [GitHub releases](https://github.com/Libertai/libertai-cli/releases).

[0.5.0]: https://github.com/Libertai/libertai-cli/compare/v0.4.5...v0.5.0
[0.4.5]: https://github.com/Libertai/libertai-cli/releases/tag/v0.4.5
[0.4.4]: https://github.com/Libertai/libertai-cli/releases/tag/v0.4.4
[0.4.3]: https://github.com/Libertai/libertai-cli/releases/tag/v0.4.3
[0.4.2]: https://github.com/Libertai/libertai-cli/releases/tag/v0.4.2
