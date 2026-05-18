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
  notes).
- **`libertai hermes` launcher** (`eeb433f`) — Hermes Agent (Nous Research)
  launched against LibertAI credentials. Not part of `libertai code`, listed
  here so the next refresh of the parity doc doesn't list it as "missing."

**Sprint 0 + 1 (this branch — `sprint-0-1-prompt-axis`):**
- **Sprint 0**: verification harness — `LIBERTAI_DUMP_SYSTEM_PROMPT` +
  `LIBERTAI_DUMP_AND_EXIT` env-var dump in pi
  (`pi/src/sdk.rs`); tier-1/tier-2 probe scaffolding under `tests/probes_*.rs`.
- **Phase 1C / parity E** (env block): `## Git context` injected by
  `pi::app::build_system_prompt` when cwd is a git work tree.
- **Phase 1D / parity G** (plan-mode prompt swap):
  `src/commands/code_mode_prompt.rs` prepends `## Plan mode` guidance to
  `append_system_prompt` when sessions start under `Mode::Plan`.
- **Parity B expansion** (executing-actions-with-care): skill section
  expanded to parity-doc target depth with reversibility, blast-radius,
  risky-op categories, scope-of-authorization, investigate-before-bypass.
- **Phase 4E / parity F** (memory v1, pulled forward):
  `pi::app::load_project_memory` reads `<PI_PROJECT_MEMORY_DIR>/<encoded-cwd>/MEMORY.md`;
  libertai-cli sets the env to `~/.config/libertai/projects` (overridable
  via `LIBERTAI_HOME`) and ships a `/remember <text>` REPL command in
  `src/commands/code_memory.rs`.

After this sprint, parity-doc Sections A–G are all shipped. Section H
(per-subagent prompts) remains gated on Phase 4D named-agent registry.

The two items in `../libertai-code-desktop/docs/claude-code-parity.md`
already noted as stale (AcceptEdits rows; `libertai-harness` "Suggested
shape") still need to be moved to the "shipped" side on the next
refresh of that doc.

---

## Open questions (resolve before starting affected phases)

1. **Does pi auto-compact?** Parity doc says "pi auto-compacts via
   context-window threshold but explicit user-driven compaction isn't
   surfaced" (line 59). Our internal inventory of `code_session.rs` says
   `ResolvedCompactionSettings` is never set, and `SessionOptions.thinking`
   is always `None`. Need to grep `pi_agent_rust`'s session module to
   confirm whether auto-compaction is on by SDK default or requires explicit
   config. Gates **Phase 4C**.

2. **Are persistent allow-rules really session-only?** Parity doc mentions
   `/forget` wipes allow-memory. Inventory said `ApprovalState` is
   session-scoped. Confirm whether anything persists to disk today before
   designing storage. Gates **Phase 2C**.

3. **Where to land per-tool usage notes** — patch `pi_agent_rust`'s `Tool::description`
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

**Files**: `src/skills_content/libertai-harness/SKILL.md`.
**Effort**: S (half-day). Pure prompt edits; lint-test by running a
sample session and checking output shape.

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

Today, `write`/`edit`/`hashline_edit` approvals show only path + byte
count (`src/commands/code_approvals.rs`). Borrow Hermes's
snapshot-then-diff pattern (`/tmp/hermes-agent/agent/display.py:90-566`):
snapshot the file at approval-prompt time, after approval+execution
compute a unified diff, render colored ±/context with a cap of 6 files
and 80 lines per file. Excess summarized as "… N lines omitted".

**Files**: `src/commands/code_approvals.rs` (snapshot trigger),
new `src/commands/code_diff.rs` (renderer).
**Effort**: M (2 days).
**Desktop note**: the desktop already has its own diff viewer
(`js/editor.js` MergeView) — this is CLI-specific UX, not shared.

### 2B. Tool preview line

One-line summary printed before each tool call (`read_file src/main.rs`,
`bash cargo build`, `edit src/foo.rs +12 -3`). Match Hermes's
per-tool primary-arg map (`agent/display.py:170-276`).

**Files**: `src/commands/code_ui.rs` (interactive renderer) +
`src/commands/code.rs` (one-shot renderer).
**Effort**: S (half-day).
**Desktop note**: desktop renders tool calls richly already — this is
CLI-only.

### 2C. Persistent allow-rule storage

`ApprovalState` is session-scoped (gated on open question 2). Add
on-disk `~/.config/libertai/allow-rules.toml`: array of
`{tool, pattern, scope: always|session}`. Load at session start, save on
"always" choice.

**Files**: `src/commands/code_approvals.rs`, `src/config.rs`
(path resolution).
**Effort**: S (1 day).
**Desktop note**: same storage path; desktop must surface a
"remembered approvals" management UI (see handoff doc item D-2).

### 2D. Surface pi's slash commands in REPL

The libertai-cli REPL handles `/help`, `/plan`, `/clear`, `/exit`,
`/forget` only. Pi defines ~24 (`/compact`, `/resume`, `/fork`,
`/export`, `/thinking`, `/theme`, `/scoped-models`, `/template`,
`/share`, `/login`, `/logout`, `/history`, `/copy`, `/name`, `/hotkeys`,
`/changelog`, `/tree`, `/reload`, `/settings`, `/model`, `/new`, etc.).

Route typed `/foo` through pi's slash dispatcher when not in our
local-command set. Add help routing so `/help` includes pi's commands.

**Files**: `src/commands/code_ui.rs` (input parsing → pi dispatcher),
`src/commands/code.rs` (one-shot mode flag handling).
**Effort**: M (1 day, mostly plumbing).
**Desktop note**: partly shipped on desktop already
(`/compact /thinking /reload /export /theme` per desktop commit
`fcff279`); the remaining palette commands and the dispatcher
plumbing for CLI REPL are still TODO.

### 2E. `!` shell prefix in REPL

Prefix-`!` lines run a shell command and inject the output as a
synthetic user-prompt addendum (Claude Code muscle memory). Pi's REPL
already parses `!`/`!!` per the parity doc — verify it reaches our
REPL too; if not, wire it.

**Files**: `src/commands/code_ui.rs`.
**Effort**: S (a few hours).
**Desktop note**: shipped on desktop (composer commit `7029b1b`).
The CLI REPL parity is still TODO.

---

## Phase 3 — Safety / efficiency primitives

Mostly Hermes-inspired. Each is small and independent.

### 3A. Read-dedup with mtime invalidation (upstream)

Three-tier "unchanged-stub → already-read-warning → blocked" on
repeated reads of the same `(path, offset, limit)` with unchanged
mtime. Cache invalidated on every successful write
(`/tmp/hermes-agent/tools/file_tools.py:487-654`).

**Files**: `pi_agent_rust/src/tools/read.rs` + write/edit invalidation
hooks.
**Effort**: S (1 day).

### 3B. Stale-write detection (upstream)

Per-task `read_timestamps` map; before write, check if mtime
changed since last read; append `_warning` to the result.
Cross-agent variant (task-tool concurrency) deferred.

**Files**: `pi_agent_rust/src/tools/write.rs` + `edit.rs`.
**Effort**: S (1 day).

### 3C. Sensitive-path write deny list + LIBERTAI_WRITE_SAFE_ROOT

Static deny list: SSH keys, `.bashrc`, `.netrc`, `.env*`,
`/etc/passwd`, AWS/GCP cred dirs (`/tmp/hermes-agent/agent/file_safety.py:19-90`).
Plus opt-in `LIBERTAI_WRITE_SAFE_ROOT` env to sandbox writes to a workspace.

**Files**: `src/commands/code_factory.rs` (wrap write/edit before
registering) or `pi_agent_rust/src/tools/write.rs` (upstream, cleaner).
**Effort**: S (half-day).

### 3D. Secret redaction on file reads (upstream)

Known-prefix detector (`sk-`, `ghp_`, `xox*`, `AIza*`, etc.) plus
sensitive query-param/body-key names. Applied to `read_file` output
before it enters the model's context. Cribbed from
`/tmp/hermes-agent/agent/redact.py`.

**Files**: `pi_agent_rust/src/tools/read.rs` + a new
`pi_agent_rust/src/redact.rs`.
**Effort**: S (1 day).

### 3E. Tool-call guardrail / loop detector

Per-turn controller that hashes `(tool_name, canonical_args)` and tracks
exact-call repeats, same-name repeats, and idempotent-result repeats;
emits warnings at thresholds, hard-stops at higher thresholds
(`/tmp/hermes-agent/agent/tool_guardrails.py`). Hand the "halt"
decision back to the agent loop as a synthetic tool result.

**Files**: new `src/commands/code_guardrail.rs`,
hook in `src/commands/code_factory.rs` (wrap every tool).
**Effort**: S (1 day).

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

Today's `task` tool spawns a generic read-only subagent
(`src/commands/code_task.rs`, hard-coded to `read/grep/find/ls`). Add a
subagent_type arg + a `~/.config/libertai/agents/<name>.md` registry,
each agent file carrying frontmatter for `tools:`, `model:`,
`system_prompt:`, `description:`. Matches Claude Code's `.claude/agents/`.

**Files**: `src/commands/code_task.rs`,
new `src/commands/code_agent_registry.rs`.
**Effort**: M-L (3 days).
**Desktop note**: handoff doc D-4 — desktop needs a "manage agents" UI.

### 4E. Memory v1

Single `~/.config/libertai/projects/<cwd-hash>/MEMORY.md` per project,
loaded alongside `AGENTS.md`. Add a `/remember <text>` slash command
that appends a dated bullet. Full four-type memory system (user /
feedback / project / reference) deferred — minimal version covers 80%
of value per parity doc section F.

**Files**: new `src/commands/code_memory.rs`,
`pi_agent_rust/src/app.rs` (system-prompt assembly hook).
**Effort**: S-M (1-2 days).

### 4F. Custom slash commands

`.libertai/commands/<name>.md` (project) and
`~/.config/libertai/commands/<name>.md` (user). Each file's frontmatter
defines `description:`, `args:`; body is a prompt template substituted
with args. Matches Claude Code's `.claude/commands/`.

**Files**: new `src/commands/code_slash_registry.rs`,
`src/commands/code_ui.rs` (dispatcher).
**Effort**: M (1.5 days).
**Desktop note**: shipped on desktop (commit `ee861ec`) but using
`.liberclaw/commands/` rather than `.libertai/commands/`. **Decision
required** before the CLI side lands: unify on one path, or support
both as fallbacks.

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
| 3A read-dedup | ✓ | — | Tool-level invariant. |
| 3B stale-write | ✓ | — | Tool-level invariant. |
| 3C sensitive-path deny | ✓ (preferred) | fallback | Cleaner upstream. |
| 3D secret redaction | ✓ | — | Tool-level invariant. |
| 3E loop detector | — | ✓ | Wraps tools at factory level. |
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
