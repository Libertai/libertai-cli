# libertai-cli — `libertai code` subcommand

> Scope: Plank 2 of the LiberClaw Code product — our own Claude-Code-class coding CLI, shipped as a subcommand of this existing Rust binary.
>
> Master plan: `~/repos/decursor/docs/plan.md` (will live at `github.com/libertai/liberclaw-code-web`).
>
> Current repo already has `claw`, `claude`, `opencode`, `aider` launchers for third-party tools. `libertai code` is our *own* branded coding agent — not a wrapper of a third-party tool.

## Implementation path (selected 2026-04-24)

After exploring four delivery options, we're shipping v0 via **path D — `pi_agent_rust` linked as a Cargo library dependency** (`github.com/Dicklesworthstone/pi_agent_rust`, a Rust port of pi-mono). No Node runtime bundled, no subprocess, single Rust binary, all existing distribution channels (GH Releases / APT / Homebrew / `cargo install`) keep working unchanged.

**Why not the Node-bundled TS pi originally scoped in this doc?** While writing the scaffold we found (a) `pi_agent_rust` exists, ships a stable `pi::sdk` surface with `create_agent_session`, and is authorized by Mario Zechner, (b) TS pi's provider list is closed and it doesn't honor `OPENAI_BASE_URL`, so we'd still need an upstream PR either way, and (c) `pi_agent_rust`'s `ModelRegistry` already supports custom OpenAI-compatible providers via a `models.json` file — we declare LibertAI there and get routing for free without writing a `Provider` trait impl.

### What shipped in the `integrated-code` branch

- `src/commands/code.rs` — asupersync runtime + `pi::sdk::create_agent_session` + custom streaming renderer (text deltas to stdout, tool events as dim stderr lines). No pi TUI embedded.
- `src/commands/code_models.rs` — writes `~/.pi/agent/models.json` with LibertAI registered under `providers.libertai` (`api: openai-completions`, `baseUrl: <api_base>/v1`). Resolves the path via `pi::config::Config::global_dir()` (honors `$PI_CODING_AGENT_DIR`). Merges, doesn't clobber user-added providers.
- `src/bin/lcode.rs` — `lcode` binary alias dispatching into `Command::Code`.
- `Cargo.toml` — `pi_agent_rust` git dep pinned at tag `v0.1.12` (crates.io only has 0.1.7 as of 2026-04-24) + `asupersync = "=0.3.1"` + `async-trait` + `futures`. License stays `"MIT"` on branch pending a policy call about `pi_agent_rust`'s MIT + OpenAI/Anthropic rider before merge.
- `src/config.rs` — added flat `default_code_provider: String` with `DEFAULT_CODE_PROVIDER = "libertai"`; wired into `config_cmd.rs` get/set/unset.
- `src/update_check.rs` — suppresses the update banner for the `code` subcommand (same treatment as `claw`).

### Carried-over v0 non-goals

- No custom Rust Provider trait impl — pi's built-in `OpenAIProvider` handles the OpenAI-compatible protocol against our `baseUrl`.
- No interactive REPL yet — an ephemeral one-shot renderer for now. Interactive Claude-Code-style bottom-bar UI is tracked as a follow-up task on this same branch.
- No session persistence — `no_session: true` for v0.
- No LiberClaw account, no `--agent` offload, no `status` reconnect — those are v1.

## Context

LiberClaw Code ships in two release trains:

- **v0 MVP** — a terminal CLI with one-shot inference + a local agent loop, any backend (LibertAI by default). No LiberClaw account or remote-agent features.
- **v1** — adds `--agent` offload to a LiberClaw agent running on Aleph Cloud, plus `libertai code status` reconnect. This needs the baal backend sprint (see `baal/docs/liberclaw-code-integration.md`).

The existing libertai-cli already has auth, config, HTTP client with SSE parsing, and launcher-style subcommands. We build on that.

## Direction: pi-based, not a from-scratch Rust tool loop

The recommended direction is to **wrap pi** (the TypeScript agent harness at `github.com/badlogic/pi-mono`) rather than write a tool-loop engine from scratch in Rust.

**Why:**
- pi is a clean multi-package TS monorepo: `pi-ai` (providers), `pi-agent-core` (tool loop), `pi-coding-agent` (CLI). MIT licensed, active, designed for embedding.
- The VSCode extension (Plank 1) plans to depend on `pi-agent-core` too — using the same harness in the CLI means one set of tool semantics, one provider abstraction, one bug-fix surface.
- Writing a Rust tool loop duplicates work the pi team is already doing well.

**Shape:** `libertai code` is a Rust subcommand that shells out to pi's CLI (or pi's `--mode rpc` for structured IO) with our defaults pre-injected. Keeps the Rust binary as the single entry point; preserves the `libertai` umbrella brand.

## v0 MVP — ~1 week

Single feature: `libertai code [prompt]` launches pi with our defaults (LibertAI provider, user's code model, user's config).

### Work items

#### 1. Subcommand scaffold

Add `code` to the existing clap subcommand enum. Follow the pattern of the recently added `claw` launcher (commit `43176a6 claw: add launcher + default_code_model for code-focused agents`).

#### 2. LibertAI provider in pi-ai

Contribute a `@mariozechner/pi-ai` provider for LibertAI (OpenAI-compatible, Bearer auth, base URL `api.libertai.io/v1`). Required by both the CLI and the VSCode extension, so PR upstream to pi where both planks can consume the published version.

Fallback if upstream PR is slow: ship our own pi plugin locally in `packaging/` and register at launch.

#### 3. Config wiring

Extend `~/.config/libertai/config.toml` with a `[code]` section:
- `default_code_model` — already exists (commit `03af07a config: wire default_code_model into config set/unset/status`)
- `default_provider` — LibertAI / OpenAI / Anthropic / Ollama / llama.cpp
- pi launch flags or env overrides

#### 4. Packaging / release

Distribute pi alongside the Rust binary. Options:
- Bundle Node prebuilt + pi in a release tarball (clean UX, ~50 MB bloat)
- Require system Node + `npm i -g @mariozechner/pi-coding-agent` at install time
- First-run auto-install with user consent

**Recommend:** bundled prebuilt for macOS/Linux/Windows to keep "one download, works" UX.

### v0 non-goals

- No LiberClaw account integration.
- No `--agent` offload.
- No Rust LiberClaw client.
- No `libertai code status`.
- No hand-off.

## v1 — adds hand-off (~1-1.5 weeks additional)

Ships after v0 is stable and the baal v1 sprint is done.

### Additional work items

#### 5. LiberClaw OAuth + LibertAI-key issuance

**Why:** LiberClaw subscribers should be able to run `libertai code` against their plan's inference allowance without separately pasting a pay-per-use key from `console.libertai.io`. A single sign-in issues them a LibertAI API key scoped to their LiberClaw tier.

**Flow (device-code preferred for terminal UX):**
- `libertai login` (reuse/extend the existing login command) prints a URL + 8-char code, user visits `console.liberclaw.ai/device`, enters code, authorizes.
- CLI polls baal for completion, receives access + refresh tokens, stores in `~/.config/libertai/config.toml` (mode 0600).
- CLI calls `POST /api/v1/users/me/libertai-key` on baal → receives `{api_key, expires_at}`, stores alongside tokens.
- Subsequent `libertai code` invocations use the issued key as the LibertAI provider key in pi-ai config.
- On 401 or near-expiry, refresh transparently.

PKCE (loopback listener) is supported as a fallback for users with a browser available — but device code is the default for SSH/tmux use.

**Config:** the `[auth]` section in `~/.config/libertai/config.toml` gets a `liberclaw` subsection mirroring the existing `libertai` one, plus a `[code].inference_source = "liberclaw-plan" | "libertai-direct"` toggle.

#### 6. Rust LiberClaw client

Generate a thin client from baal's OpenAPI (`/api/v1/openapi.json`). Use `progenitor` or hand-roll with `reqwest`. Cover: list agents, get agent status, start hand-off, get run status, SSE subscribe, issue/refresh LibertAI key.

#### 7. `--agent` hand-off

`libertai code --agent [agent_id_or_name]` offloads the current session to a LiberClaw agent via `POST /api/v1/agents/{id}/handoff`. Uses the hand-off schema from `liberclaw-code/packages/handoff-schema`.

#### 8. `libertai code status`

Reconnect command: `libertai code status [run_id]` subscribes to a running agent's SSE stream and renders progress in the terminal. Same agent-side protocol as the VSCode extension uses.

#### 9. Extended config

Add to `[code]` section:
- Default hand-off target (reuse-existing vs. spawn-new)
- Skip-permissions policy for `--agent` runs

## Dependency order

**v0:**
1. Lock pi subprocess shape (1 day)
2. LibertAI provider PR to pi-ai (parallel with below; local plugin as fallback)
3. Subcommand scaffold + pi launch + config wiring
4. Packaging / release pipeline extension

**v1 (after baal sprint):**
5. Rust LiberClaw client from OpenAPI
6. `--agent` hand-off wiring
7. `status` reconnect path

## Open questions

1. **Node runtime bundling.** Bundle Node with the release, or require system Node? Bundled is cleaner UX but ~50 MB bloat per platform.
2. **Auth sharing with the existing `claw` launcher.** Do we reuse the same session / API key storage, or scope tokens per subcommand?
3. **Telemetry.** pi may have telemetry upstream. Confirm defaults; ensure it's off by default in our wrapper regardless.
4. **Rust client codegen tool** (v1 only). `progenitor` vs. hand-roll. Progenitor is OpenAPI-native but adds a proc-macro dep.

## Files to touch

**v0:**
- `src/commands/` — new `code.rs` subcommand module
- `src/config.rs` — extend `[code]` section
- `packaging/` — pi bundling logic
- `~/.config/libertai/config.toml` — schema update

**v1 (additional):**
- `src/client.rs` — extend, or new `src/liberclaw_client.rs`
- `Cargo.toml` — add LiberClaw client deps

## Non-goals

- No custom Rust tool loop (pi handles it).
- No replacement of the existing `claw`/`aider`/`opencode`/`claude` launchers — they stay; `libertai code` is additive, our own-brand option.
- No npm publishing of the Rust binary; stays a Rust-native release.
