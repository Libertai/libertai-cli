# libertai-cli — Claude-Code-parity & Hermes-inspired roadmap

Snapshot 2026-05-12.

This is the CLI-side plan. The desktop's parity story is in
`../libertai-code-desktop/docs/claude-code-parity.md`. The companion
handoff doc — what the desktop must do when it bumps this crate — is
in `../libertai-code-desktop/docs/libertai-cli-parity-handoff.md`.

The desktop pulls libertai-cli as a path dep
(`libertai-code-desktop/src-tauri/Cargo.toml:32`), so every item below
flows into the desktop on rebuild. Some items must land in
`pi_agent_rust` so both this crate and the desktop pick them up via the
SDK; those are flagged **(upstream)**.

---

## 0. Already shipped on `integrated-code`

- **AcceptEdits middle permission tier** (`2a86a37`) — `ModeFlag::AcceptEdits`,
  toggled via `Shift+Tab` cycle and `/plan` etc. (`src/commands/code_factory.rs`).
- **`libertai-harness` behavioral skill** (`a13cddf`) — first cut of the
  Claude-Code-style guidance block
  (`src/agent_skills/libertai-harness/SKILL.md`); covers parity-doc Section A
  (terse responses, exploratory framing, `file_path:line`, parallel tool
  use, end-of-turn brevity), Section C (tool posture), Section D (per-tool
  notes), and the auto-memory protocol for when to save `user`,
  `feedback`, `project`, and `reference` memories.
- **`libertai hermes` launcher** (`eeb433f`) — Hermes Agent (Nous Research)
  launched against LibertAI credentials. Not part of `libertai code`, listed
  here so the next refresh of the parity doc doesn't list it as "missing."
- **CLI `/permissions` command** — reports the current native permission
  mode, switches among `default`, `acceptEdits`, and `plan`, clears saved
  allow rules, and documents that native `bypassPermissions`
  is intentionally unavailable.
- **CLI tool preview lines** — REPL and one-shot renderers now show the
  primary tool arguments (`read src/lib.rs:12+40`, `bash cargo test`,
  `grep pattern in src`) instead of only the tool name.
- **CLI approval diff previews** — file-mutation approval prompts now
  compare proposed `write`/`edit` changes against current files when
  available and show structured `hashline_edit` operation summaries.
- **CLI `/model` command** — REPL users can inspect the active
  provider/model and switch with `/model <model|provider/model>` without
  rebuilding the session.
- **CLI `/name` command** — REPL users can persist a display name for
  the current session so it appears in later session listings.
- **CLI `/export` command** — REPL users can write the current transcript
  to Markdown with `/export [path]`.
- **CLI `/history` command** — REPL users can inspect recent submitted
  prompts with `/history [count]`.
- **CLI `/new` alias** — REPL users can start a fresh session with the
  Claude/Pi-style `/new` spelling as well as `/clear`.
- **CLI `/copy` command** — REPL users can copy the latest assistant
  response through the terminal clipboard with OSC 52.
- **CLI `/settings` and `/hotkeys` commands** — REPL users get Pi-style
  aliases for the config summary and keyboard-control reference.
- **CLI `/tree` command** — REPL users can print a bounded project tree
  with noisy dependency/build directories skipped.
- **CLI `/changelog` command** — REPL users can inspect recent git
  commits with `/changelog [count]`.
- **CLI `/reload`, `/login`, and `/logout` commands** — REPL users can
  refresh config/auth state and rebuild the active agent session without
  leaving `libertai code`.
- **CLI `/resume` command** — REPL users can switch to the latest saved
  session for the current cwd or an explicit session JSONL path.
- **CLI `/fork` command** — REPL users can list forkable user messages
  and fork from the latest, an index, or an entry ID/prefix.
- **CLI `/thinking` command** — REPL users can inspect and set pi's
  thinking level with `/thinking <off|minimal|low|medium|high|xhigh>`
  plus `/think` and `/t` aliases.
- **CLI `/share` command** — REPL users can write a self-contained
  HTML transcript with `/share [path]` for local sharing/review.
- **CLI `/compact` command** — REPL users can trigger pi's explicit
  compaction pass from the REPL, with optional user notes threaded
  into the summarization prompt.
- **CLI `/loop` command** — REPL users can queue a bounded foreground
  autonomous follow-up loop with `/loop [turns] [goal]` or `/autoloop`,
  capped at 10 turns.
- **Bash background execution** — the upstream `bash` tool accepts
  Claude-style `run_in_background: true` for long-running servers and
  watchers, returning immediately with a PID and temp log path.
- **CLI `/doctor` command** — REPL users can print a local diagnostic
  report for session state, auth/config, memory/templates/agents, git,
  and usage.
- **CLI `/usage` / `/cost` tool activity** — REPL usage summaries show
  turn counts, token high-water/output totals, and observed per-tool
  call counts/durations for the current session. True per-tool
  token/cost attribution remains deferred.
- **Typed project memory** — `/remember` accepts `user:`, `feedback:`,
  `project:`, `reference:`, or `--type <kind>` prefixes and stores
  categorized bullets in the shared `MEMORY.md` index, with per-entry
  sidecar Markdown files under `memory/<kind>/`; `/memory files` lists
  those sidecars, `/memory import <path>` imports local Markdown/text
  into project memory with source provenance, and `/memory clear` backs
  sidecars up with the index. The built-in harness now tells the model
  when to save durable memories, when to ask first, and what not to
  persist.
- **CLI `/review`, `/security-review`, and `/pr_comments` commands** —
  REPL users can dispatch the same structured review and PR-comment
  prompts already used by the desktop slash palette.
- **CLI `/sandbox` command** — REPL users can inspect the effective
  strict bash sandbox profile with `/sandbox [info]`; `/sandbox reload`
  explains the CLI restart requirement.
- **CLI `/mode` and `/rename` aliases** — REPL users can use the
  desktop/Claude-style spellings for permission-mode switching and
  session naming.
- **CLI `/abort` command** — REPL users get an explicit status message
  pointing to the active-turn Ctrl+C abort path.
- **CLI `/image` and `/attach` commands** — REPL users can attach a
  local PNG/JPEG/GIF/WebP to a multimodal prompt using
  `/image <path> [prompt]` or the `/attach` alias.
- **CLI `/mention` command** — REPL users can include a local UTF-8 text
  file in the next prompt with `/mention <path> [prompt]`.
- **Native notebook tools** — `notebook_read` summarizes local `.ipynb`
  files cell-by-cell, including stream/result/error output previews,
  rich MIME hints, and rich non-image MIME previews for HTML, Markdown,
  JSON, and data-resource table payloads, and emits supported image MIME
  payloads as image blocks; approval-gated `notebook_edit` can replace,
  insert, or delete notebook cells while preserving the rest of the JSON; and
  approval-gated `notebook_execute` runs the system Jupyter CLI in place
  with a bounded timeout before returning an updated summary plus any
  supported image outputs.
- **Skill disable registry** — native sessions skip skill names listed
  in `~/.config/libertai/disabled-skills.toml`, allowing desktop and
  future CLI surfaces to manage built-in, project, and user skills
  without editing `SKILL.md` files.
- **Tool-call loop guardrail** — every registered tool is wrapped by a
  shared guardrail that warns on repeated exact calls / same-tool loops
  and returns a synthetic tool error when a loop crosses the hard-stop
  threshold.
- **Sensitive-path write guardrail** — mutating path tools deny writes
  to likely secret/auth files (`.env*`, `.netrc`, shell startup files,
  SSH keys/config, cloud credential directories, system account files)
  before approval prompts; `LIBERTAI_WRITE_SAFE_ROOT` can further limit
  writes to a subdirectory.
- **Upstream stale-write detection** — `pi_agent_rust` now shares
  read mtimes across built-in read/write/edit/hashline tools; writes
  attach a `_warning` when the target changed after the last read.
- **Upstream read de-duplication** — repeated unchanged reads of the
  same path/range now escalate from a stub, to a warning, to a blocked
  tool result; successful writes invalidate the read-repeat state.
- **Upstream read secret redaction** — `pi_agent_rust` redacts common
  credential prefixes and sensitive key/value fields from text `read`
  output before the content enters the model context.
- **Agent-callable push notifications** — the native `push_notification`
  tool lets an agent request a short user notification through the active
  UI; non-notification clients report a skipped result instead of failing.
- **File-backed output styles** — `/output-style` keeps built-in
  response styles and discovers Markdown styles from project/user
  `.claude/output-styles` and `.libertai/output-styles` roots.
- **CLI status line customization** — `/statusline <template>` persists
  a terminal input-bar template with Claude-style tokens for project,
  path, backend/model, mode, output style, token count, and context use.

**Sprint 0 + 1 (this branch — `sprint-0-1-prompt-axis`):**
- **Sprint 0**: verification harness — `LIBERTAI_DUMP_SYSTEM_PROMPT` +
  `LIBERTAI_DUMP_AND_EXIT` env-var dump in pi
  (`pi/src/sdk.rs`); tier-1/tier-2 probe scaffolding under `tests/probes_*.rs`.
- **Phase 1C / parity E** (env block): `## Git context` injected by
  `pi::app::build_system_prompt` when cwd is a git work tree.
- **Phase 1D / parity G** (plan-mode prompt swap):
  `src/commands/code_mode_prompt.rs` prepends `## Plan mode` guidance to
  `append_system_prompt` when sessions start under `Mode::Plan`, and
  the interactive CLI now prompts to approve the plan and switch back
  to normal mode after a successful plan-mode turn.
- **Parity B expansion** (executing-actions-with-care): skill section
  expanded to parity-doc target depth with reversibility, blast-radius,
  risky-op categories, scope-of-authorization, investigate-before-bypass.
- **Phase 4E / parity F** (memory v1, pulled forward):
  `pi::app::load_project_memory` reads `<PI_PROJECT_MEMORY_DIR>/<encoded-cwd>/MEMORY.md`;
  libertai-cli sets the env to `~/.config/libertai/projects` (overridable
  via `LIBERTAI_HOME`) and ships a `/remember <text>` REPL command in
  `src/commands/code_memory.rs`; the harness now carries the
  Claude-style auto-memory save/avoid/verify protocol for the typed
  memory categories.

After this sprint, parity-doc Sections A–G are all shipped, including
the model-facing memory guidance. Section H (per-subagent prompts)
remains gated on Phase 4D named-agent registry.

`../libertai-code-desktop/docs/claude-code-parity.md` has been refreshed
to move AcceptEdits, `libertai-harness`, desktop `!cmd`, custom slash
commands, and the first slash-routing batch to their current shipped or
partial states.

---

## Open questions (resolve before starting affected phases)

1. **Does pi auto-compact?** Parity doc says "pi auto-compacts via
   context-window threshold but explicit user-driven compaction isn't
   surfaced" (line 59). Our internal inventory of `code_session.rs` says
   `ResolvedCompactionSettings` is never set, and `SessionOptions.thinking`
   is always `None`. Need to grep `pi_agent_rust`'s session module to
   confirm whether auto-compaction is on by SDK default or requires explicit
   config. Gates **Phase 4C**.

2. **Where to land per-tool usage notes** — patch `pi_agent_rust`'s `Tool::description`
   strings (gives the desktop the same notes for free), or append a
   "tool usage notes" appendix to `libertai-harness` (faster, no upstream
   PR). Decide before **Phase 1B**.

---

## Phase 1 — Harness / prompt polish

**Why first**: same model, fatter prompt, better output. Per the parity
doc, the single highest quality lift per word changed. Most-load-bearing
items are 1A and 1B.

### 1A. Expand `libertai-harness` skill

Port the meat of parity-doc Section A into the existing
`src/skills_content/libertai-harness/SKILL.md`. Priority order
(load-bearing items first):

1. *Exploratory-vs-implementation framing* — "questions like 'what could
   we do about X?' get 2–3 sentences with a recommendation and the main
   tradeoff, not implementation."
2. *Don't add features beyond what was asked.*
3. *Don't add error handling for scenarios that can't happen.*
4. *Default to no comments. Only add when the WHY is non-obvious.*
5. *Use `file_path:line_number` references when citing code.*
6. *Match response length to task complexity.*
7. *No emojis unless asked.*
8. *Brief progress updates at key moments, not running commentary of
   internal thinking.*
9. *End-of-turn = one or two sentences, not a recap.*
10. *Make independent tool calls in parallel.*

**Status**: shipped in `src/agent_skills/libertai-harness/SKILL.md`.

### 1B. Per-tool usage notes (upstream)

Pi's tool descriptions are one-liners. Claude Code's Read tool description
alone spells out: absolute-path requirement, 2000-line default cap, PDF
page-range protocol, "do NOT re-read a file you just edited", notebook
handling, image handling.

**Decision required** (see open question 3): upstream to `pi_agent_rust`
`Tool::description` strings vs. append as a section in `libertai-harness`.

If upstream: edit `pi_agent_rust/src/tools.rs` per-tool `description()`
methods; PR; bump rev in our `Cargo.toml`.

**Files**: `pi_agent_rust/src/tools.rs` (preferred) or
`src/skills_content/libertai-harness/SKILL.md`.
**Effort**: S–M (1 day).

### 1C. Env block — git status + recent commits

Pi already appends cwd and date. Claude Code also injects `git status -sb`
+ recent `git log --oneline` + git user.

**Best place to land**: `pi_agent_rust::app::build_system_prompt` so the
desktop inherits automatically (preferred). Fallback: a `code_env.rs`
module that pi calls via a hook.

**Files**: `pi_agent_rust/src/app.rs` (preferred upstream) or
`src/commands/code_session.rs` (local override).
**Effort**: S (half-day).

### 1D. Plan-mode prompt addendum

Today plan mode only changes tool behavior. Add a short block prepended
to the system prompt when `ModeFlag::Plan` is active: "you are in plan
mode; describe the intended edits, do not attempt to mutate state;
finish with a numbered plan for the user to approve."

**Files**: `src/commands/code_factory.rs` (where `ModeFlag` lives) +
`src/commands/code_session.rs` (system-prompt assembly).
**Effort**: S (a few hours).

---

## Phase 2 — CLI UX gaps

### 2A. Inline diff renderer in approvals

Shipped first pass for CLI approval prompts: `write` and `edit` compare
against current files when readable, fall back to payload-only previews
for new/unreadable files, and `hashline_edit` summarizes requested
operations before the user approves. Remaining work is colored rendering
and post-exec rendering for exact file-system deltas.

**Files**: `src/commands/code_approvals.rs` (snapshot trigger),
`src/commands/code_diff.rs` (renderer).
**Status**: partial.
**Desktop note**: the desktop already has its own diff viewer
(`js/editor.js` MergeView) — this is CLI-specific UX, not shared.

### 2B. Tool preview line

Shipped for CLI renderers: one-line summaries now print before each tool
call (`read src/main.rs`, `bash cargo build`, `edit src/foo.rs`,
`grep pattern in src`). The formatter is shared by interactive and
one-shot mode and caps long payloads.

**Files**: `src/commands/code_ui.rs` (interactive renderer) +
`src/commands/code.rs` (one-shot renderer), plus
`src/commands/code_tool_preview.rs`.
**Status**: shipped.
**Desktop note**: desktop renders tool calls richly already — this is
CLI-only.

### 2C. Persistent allow-rule storage

Shipped for the CLI: `ApprovalState` can now load and save
`~/.config/libertai/allow-rules.toml` using `[[rules]]` entries with
`{tool, pattern, wildcard, scope = "always"}`. CLI sessions opt into the
store, "always allow" writes the deduped rule set, and `/forget` clears
the saved rules.

**Files**: `src/commands/code_approvals.rs`, `src/config.rs`
(path resolution).
**Status**: shipped for CLI.
**Desktop note**: same storage path; desktop must surface a
"remembered approvals" management UI (see handoff doc item D-2).

### 2D. Surface pi's slash commands in REPL

The libertai-cli REPL handles `/help`, `/plan`, `/clear`, `/exit`,
`/forget`, `/permissions`, `/remember`, `/memory`, `/init`, `/agents`, `/agent`,
`/template`, custom `/<name>` templates, `/status`, `/usage`/`/cost`,
`/config`, `/settings`, `/model`, `/mode`, `/name`, `/rename`, `/new`, `/export`, `/share`, `/history`, `/copy`, `/hotkeys`, `/tree`, `/changelog`, `/reload`, `/resume`, `/fork`, `/compact`, `/doctor`, `/abort`, `/review`, `/security-review`, `/pr_comments`, `/sandbox`, `/thinking`, `/login`, `/logout`, `/output-style`, `/vim`, `/ide`, and `/bug`. Pi defines ~24
(`/compact`, `/resume`, `/fork`, `/export`, `/thinking`, `/theme`,
`/scoped-models`, `/template`, `/share`, `/login`, `/logout`,
`/history`, `/copy`, `/name`, `/hotkeys`, `/changelog`, `/tree`,
`/reload`, `/settings`, `/model`, `/new`, etc.).

Route typed `/foo` through pi's slash dispatcher when not in our
local-command set. Add help routing so `/help` includes pi's commands.

**Files**: `src/commands/code_ui.rs` (input parsing → pi dispatcher),
`src/commands/code.rs` (one-shot mode flag handling).
**Effort**: M (1 day, mostly plumbing).
**Desktop note**: partly shipped on desktop already
(`/compact /thinking /reload /export /theme` per desktop commit
`fcff279`, later extended with `/status /config /output-style /vim
/ide /bug`); the remaining palette commands and shared dispatcher
plumbing are still TODO.

### 2F. Native `/init`

Shipped: CLI and desktop `/init` call `code_init::init_project`, which
creates `AGENTS.md` when missing and preserves existing files. The
generated file is deterministic and based on visible repo manifests
(`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`) plus common
directory names. It now parses manifest names, package scripts, Go
modules, and common config files such as Dockerfile / GitHub Actions /
Makefile. Remaining work is optional editing/merge UI and any
model-assisted project-specific prose we later decide is worth the
extra turn.

### 2E. `!` shell prefix in REPL

Prefix-`!` lines run a shell command in the current cwd and render the
captured output locally (Claude Code muscle memory). The CLI REPL now
matches the desktop composer escape and reuses the same bash wrapper
argv when `--sandbox=strict` is active.

**Files**: `src/commands/code_ui.rs`.
**Status**: shipped for local `!cmd`; `!!` repeat and agent-history
injection remain optional follow-ups.
**Desktop note**: shipped on desktop (composer commit `7029b1b`).

---

## Phase 3 — Safety / efficiency primitives

Mostly Hermes-inspired. Each is small and independent.

### 3A. Read-dedup with mtime invalidation (upstream)

Shipped upstream in the LibertAI fork: repeated reads of the same
`(path, offset, limit, hashline)` with unchanged mtime escalate through
an unchanged-result stub, an `_warning`, then a blocked tool result.
Successful `write`, `edit`, and `hashline_edit` calls invalidate the
path's read-repeat state.

**Files**: `pi_agent_rust/src/tools/read.rs` + write/edit invalidation
hooks.

### 3B. Stale-write detection (upstream)

Shipped upstream in the LibertAI fork: the built-in tool registry shares
file read mtimes across `read`, `write`, `edit`, and `hashline_edit`.
Before mutating an already-read file, write tools compare the current
mtime and attach `_warning` if the file changed after the last read.
Cross-agent concurrency remains deferred.

**Files**: `pi_agent_rust/src/tools/write.rs` + `edit.rs`.

### 3C. Sensitive-path write deny list + LIBERTAI_WRITE_SAFE_ROOT

Shipped locally: `code_path_safety::PathSafetyTool` wraps mutating path
tools and denies SSH keys/config, `.bashrc`/profile files, `.netrc`,
`.env*`, `/etc/passwd`/`/etc/shadow`, and AWS/GCP/Azure credential
directories before the approval UI. `LIBERTAI_WRITE_SAFE_ROOT` further
restricts writes to a chosen workspace subdirectory.

**Files**: `src/commands/code_factory.rs` (wrap write/edit before
registering) or `pi_agent_rust/src/tools/write.rs` (upstream, cleaner).

### 3D. Secret redaction on file reads (upstream)

Shipped upstream in the LibertAI fork: text `read` output is redacted
after line numbering/truncation and before it is returned to the model.
The detector covers common credential prefixes (`sk-`, `ghp_`,
`github_pat_`, `xox*`, `AIza*`, `AKIA`/`ASIA`) plus sensitive
assignment/query keys such as `api_key`, `token`, `password`,
`client_secret`, and `private_key`. Redaction counts are reported in
tool `details`.

**Files**: `pi_agent_rust/src/tools/read.rs` + a new
`pi_agent_rust/src/redact.rs`.

### 3E. Tool-call guardrail / loop detector

Shipped: the CLI factory wraps every registered tool in
`code_guardrail::GuardrailTool`. It hashes `(tool_name, canonical_args)`,
tracks same-tool loops and repeated idempotent results, injects warnings
at soft thresholds, and returns a synthetic tool error at hard-stop
thresholds.

**Files**: new `src/commands/code_guardrail.rs`,
hook in `src/commands/code_factory.rs` (wrap every tool).

---

## Phase 4 — Larger subsystems

### 4A. Smart-approval auxiliary LLM tier

When a flagged command would normally interactive-prompt, first ask a
cheap aux LLM with `max_tokens=16` for APPROVE/DENY/ESCALATE. Only
ESCALATE reaches the user. Falls back gracefully if no aux model
configured.
(`/tmp/hermes-agent/tools/approval.py:841-885`)

**Files**: `src/commands/code_approvals.rs`,
new `src/commands/code_aux.rs` (aux client wrapper around our existing
provider config).
**Effort**: M (1.5 days).

### 4B. Background skill-review fork (learning loop)

After every N tool-using iterations, spawn a child `AgentSession` with
the conversation snapshot + a curator prompt; child has access to a
restricted "skill write" tool and can create/patch SKILL.md files under
`~/.config/libertai/skills/`. Triggered async; doesn't block user turn.
(`/tmp/hermes-agent/run_agent.py:4234-4340`, `15419-15441`)

Hermes's curator (weekly consolidation pass) is **deferred** until we
have ~50 agent-created skills.

**Files**: new `src/commands/code_curator.rs`,
`src/commands/code_skills.rs` (writer tool),
`src/commands/code_session.rs` (trigger hook).
**Effort**: M (2-3 days).
**Desktop note**: handoff doc item D-3 — desktop should ship a
"review proposed skills" tray UI before this hits prod, otherwise
skills appear silently.

### 4C. Compaction wiring + SUMMARY_PREFIX framing

Gated on open question 1. Two cases:

- **If pi auto-compacts already**: just override the summary prefix
  to use Hermes's "REFERENCE ONLY / Active Task" framing
  (`/tmp/hermes-agent/agent/context_compressor.py:37-51`).
- **If not**: configure `ResolvedCompactionSettings`, set
  threshold to ~75% of context window, wire the prefix.

**Files**: `pi_agent_rust/src/agent/compaction.rs` (upstream, prefix),
`src/commands/code_session.rs` (settings).
**Effort**: M (1-2 days).

### 4D. Named sub-agent registry

Shipped: the `task` tool accepts `subagent_type` and discovers
Claude-compatible `.claude/agents/<name>.md`, project
`.libertai/agents/<name>.md`, user `~/.claude/agents`, and user
`~/.config/libertai/agents`. Agent files carry frontmatter for
`description:`, `tools:`, and `model:` plus a body system prompt.
CLI `/agents` lists discovered definitions and `/agent <name> <task>`
routes through the active agent with an instruction to call the `task`
tool for that named sub-agent. Worktree isolation now uses a detached
git worktree when possible and a copied temp workspace snapshot outside
git. Remaining work is richer management UI, background execution, and
child event streaming.

**Files**: `src/commands/code_agents.rs`, `src/commands/code_task.rs`,
`src/commands/code_ui.rs`.

### 4E. Memory v1

Single `~/.config/libertai/projects/<cwd-hash>/MEMORY.md` per project,
loaded alongside `AGENTS.md`. Add a typed `/remember <kind>: <text>`
slash command that appends a categorized dated bullet. CLI `/memory`
can inspect typed counts, show the resolved file/path, open it in
`$VISUAL`/`$EDITOR`, and clear it with a backup; desktop `/memory` can
inspect and edit it. CLI and desktop `/memory references` verify
`[reference]` bullets by marking external URLs and checking local path
targets against the session cwd. Full file-per-memory storage is deferred.

**Files**: new `src/commands/code_memory.rs`,
`pi_agent_rust/src/app.rs` (system-prompt assembly hook).
**Effort**: S-M (1-2 days).

### 4F. Custom slash commands

Shipped: CLI and desktop discover project `.claude/commands`,
`.libertai/commands`, and legacy `.liberclaw/commands`, plus user
`~/.claude/commands` and `~/.config/libertai/commands`. Each Markdown
file becomes a prompt template; frontmatter may define `description:`
and `argHint:`. CLI supports `/template <name> [args]` and direct
`/<name> [args]` dispatch with `{{args}}` substitution.

**Files**: `src/commands/code_slash_registry.rs`, `src/commands/code_ui.rs`.

---

## Phase 5 — Largest items, lowest priority

### 5A. Hooks config schema

Expose `pi_agent_rust`'s typed hooks (`on_tool_start`, `on_tool_end`,
`on_stream_event`) via `~/.config/libertai/config.toml`
`[hooks.PreToolUse]` etc. Each hook is a shell command receiving JSON
on stdin; output JSON controls decisions (approve/deny/transform).
Mirrors Claude Code's hook contract.

**Files**: `src/config.rs`,
new `src/commands/code_hooks.rs`,
`pi_agent_rust/src/sdk.rs` (hook dispatcher; may need upstream).
**Effort**: L (4-5 days).

### 5B. MCP support

Spawn user-configured MCP servers (stdio/HTTP/SSE), expose their tools
to the agent. Currently the parity doc reserves Tier C for this
(`../libertai-code-desktop/docs/claude-code-parity.md:43`); nothing
wired.

**Files**: new `src/commands/code_mcp.rs` + transport modules.
**Effort**: L (2+ weeks).

### 5C. Cron / scheduled agents

Out of scope for `libertai code` as a one-shot/REPL tool. Belongs in
a separate `libertai cron` subcommand if at all. Skipping unless user
demand emerges.

---

## Cross-repo landing matrix

| Item | Land in `pi_agent_rust` | Land in `libertai-cli` | Notes |
|---|---|---|---|
| 1A behavioral skill | — | ✓ | Skill content is here. |
| 1B per-tool notes | ✓ (preferred) | fallback | Upstream gives desktop the same notes for free. |
| 1C env / git block | ✓ | — | Append in `build_system_prompt`. |
| 1D plan-mode addendum | — | ✓ | ModeFlag lives here. |
| 2A diff renderer | — | ✓ | CLI-only UX (desktop has MergeView). |
| 2B tool preview line | — | ✓ | CLI-only UX. |
| 2C persistent allow-rules | — | ✓ | But the desktop reads the same file (see handoff). |
| 2D pi slash routing | — | ✓ | Plumbing in REPL. |
| 2E `!` shell prefix | — | ✓ | REPL parser. |
| 3A read-dedup | ✓ | — | Tool-level invariant shipped in LibertAI fork. |
| 3B stale-write | ✓ | — | Tool-level invariant shipped in LibertAI fork. |
| 3C sensitive-path deny | — | ✓ | Local wrapper protects desktop/CLI; upstream still cleaner long-term. |
| 3D secret redaction | ✓ | — | Tool-level invariant shipped in LibertAI fork. |
| 3E loop detector | ✓ | ✓ | Wraps tools at factory level. |
| 4A smart-approval | — | ✓ | Reuses our provider config. |
| 4B skill-review fork | — | ✓ | Spawns child via SDK. |
| 4C compaction prefix | ✓ | — | Upstream the prefix; config local. |
| 4D agent registry | — | ✓ | Local registry directory. |
| 4E memory v1 | partial | ✓ | Pi loads it via hook; libertai-cli writes it. |
| 4F custom slash | — | ✓ | REPL-level. |
| 5A hooks | partial | ✓ | Upstream the dispatcher trait; config local. |
| 5B MCP | partial | ✓ | Could grow to its own crate. |

Upstream items should land as separate PRs against `pi_agent_rust` with
small, focused diffs so the desktop can pick them up by single-rev
bumps.

---

## Suggested sequencing

If we tackle phases roughly in order, the milestones look like:

- **M1 (week 1)**: Phase 1 (1A + 1B + 1C + 1D). Biggest quality lift,
  almost all prompt work.
- **M2 (week 2)**: Phase 2A + 2B + 2D + 2E. Visible CLI UX bump.
- **M3 (week 2-3)**: Phase 3 (3A-3E). Safety bundle — most items
  S effort and independent.
- **M4 (week 3-4)**: Phase 2C + 4A + 4E. Persistent state + smart
  approval + memory.
- **M5 (week 5+)**: Phase 4B + 4D + 4F. Learning loop, agent
  registry, custom slash. Largest user-visible surface change.
- **M6 (later)**: Phase 4C (gated on Q1), Phase 5.

Each milestone is a candidate dep-bump in the desktop (see handoff doc).
