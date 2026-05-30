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
  is intentionally unavailable; `/permissions bypassPermissions` is an
  explicit explanatory action rather than a silent no-op.
- **CLI tool preview lines** — REPL and one-shot renderers now show the
  primary tool arguments (`read src/lib.rs:12+40`, `bash cargo test`,
  `grep pattern in src`) instead of only the tool name.
- **CLI approval diff previews** — file-mutation approval prompts now
  compare proposed `write`/`edit` changes against current files when
  available and show structured `hashline_edit` operation summaries.
- **Opt-in smart approvals** — `smart_approval_enabled = true` asks a
  bounded auxiliary LibertAI model before manual mutating-tool prompts;
  exact `APPROVE` runs, exact `DENY` returns a tool error, and any
  error, malformed answer, or `ESCALATE` falls back to the existing
  approval UI. Auto-approve and auto-deny decisions emit structured
  `smart_approval` tool updates for CLI/desktop audit visibility.
- **CLI `/model` command** — REPL users can inspect the active
  provider/model with `/model status|show|current`, list discovered
  models with `/model list|ls`, cycle with `/model next|cycle` and
  `/model prev|previous|back`, and switch with
  `/model <model|provider/model>` without rebuilding the session.
  Session-local `/scoped-models` patterns filter `/model list` and
  cycling.
- **CLI `/name` command** — REPL users can persist a display name for
  the current session so it appears in later session listings.
- **CLI `/export` command** — REPL users can write the current transcript
  to Markdown with `/export [path]`.
- **CLI `/history` command** — REPL users can inspect recent submitted
  prompts with `/history [count]`.
- **CLI `/history` aliases** — REPL users can use `/history
  list|recent|latest|status` for the default recent-prompt view.
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
- **CLI `/changelog` aliases** — REPL users can use `/changelog
  list|recent|latest|status` for the default recent-commit view.
- **CLI `/reload`, `/login`, and `/logout` commands** — REPL users can
  refresh config/auth state and rebuild the active agent session without
  leaving `libertai code`; `/login status` reports saved LibertAI
  credentials, `/login show <provider>` / `/logout show <provider>`
  inspect the terminal-vs-desktop credential boundary without printing
  secret values, and `/login <provider>` / `/logout <provider>` explain
  the desktop-only provider credential handoff.
- **CLI `/send` and `/send-message` aliases** — REPL users get the
  desktop send-path explanation instead of accidentally sending the slash
  line to the model; `/send status`, `/send targets`, and `/send list`
  mirror the desktop target-inspection vocabulary while noting the terminal's
  single-session limitation.
- **CLI `/doctor` diagnostics** — REPL users get a local health report
  for pi-session state, auth/defaults, smart approvals, remembered
  approvals, hook event counts, project memory plus sidecar/reference
  health, named-agent worktree defaults, custom slash source counts,
  active code-agent skills, MCP native exposure, scheduled prompt
  due/pending counts, git status, and usage.
- **CLI `/resume` command** — REPL users can switch to the latest saved
  session for the current cwd or an explicit session JSONL path.
- **CLI `/fork` command** — REPL users can list forkable user messages
  and fork from the latest, an index, or an entry ID/prefix.
- **CLI `/thinking` command** — REPL users can inspect and set pi's
  thinking level with `/thinking <off|minimal|low|medium|high|xhigh>`
  plus `/think` and `/t` aliases.
- **CLI `/share` command** — REPL users can write a self-contained
  HTML transcript with `/share [path]` for local sharing/review or
  publish it through the authenticated GitHub CLI with
  `/share gist [public|secret] [filename.html]`.
- **CLI `/onboarding` command** — REPL users can generate a local
  Markdown onboarding guide from repo facts, commands, structure, and
  existing AGENTS/CLAUDE guidance, or publish it through the authenticated
  GitHub CLI with `/onboarding gist [public|secret] [filename.md]`.
- **CLI `/init from-agent merge-lines`** — REPL users can apply the
  latest assistant-proposed fenced `AGENTS.md candidate` by appending
  only new lines inside matching `##` sections, preserving existing
  section text instead of replacing whole headings; `/init from-agent
  preview merge-lines` shows that no-write result before applying.
- **CLI `/compact` command** — REPL users can trigger pi's explicit
  compaction pass from the REPL, with optional user notes threaded
  into the summarization prompt.
- **CLI `/loop` command** — REPL users can queue a bounded foreground
  autonomous follow-up loop with `/loop [turns] [goal]` or `/autoloop`,
  capped at 10 turns.
- **CLI `/auto` command** — REPL users can start a bounded foreground
  continuous-execution mode with `/auto on [turns] [goal]`, inspect it
  with `/auto status`, and cancel idle state with `/auto off`; active
  runs are Ctrl-C stoppable and cap at 25 turns.
- **CLI `/schedule` command** — REPL users can queue in-process
  follow-up prompts with `/schedule in <delay> <prompt>`, inspect them
  with `/schedule list` / `/schedule state` including due/pending
  counts, inspect one queued prompt with `/schedule show <id>`, manually
  queue one immediately with `/schedule run <id>`, and cancel them with
  `/schedule cancel <id>` or `/schedule clear`; `/cron` aliases match the
  desktop composer. Due prompts run
  between REPL turns; durable detached cron remains out of scope here.
- **Bash background execution** — the upstream `bash` tool accepts
  Claude-style `run_in_background: true` for long-running servers and
  watchers, returning immediately with a PID and temp log path.
- **CLI `/agent --background` command** — REPL users can start a named
  sub-agent task in a detached terminal process, including the
  `/agent --detached` alias, without blocking the active transcript; the
  command prints the child PID and a log path under
  `~/.config/libertai/code-background-agents`. `/agents background`
  lists recorded detached runs, `/agents background show [pid|latest]`
  inspects one run's status/backend/cwd/log/prompt metadata, `/agents
  background log [pid|latest]` tails saved output, and `/agents
  background kill <pid>` stops a running child process.
- **CLI `/doctor` command** — REPL users can print a local diagnostic
  report for session state, auth/config, smart approval status,
  remembered approvals, hook event counts, memory/templates/agents,
  terminal background-agent status counts, git, MCP registry/exposure
  availability, scheduled prompt counts, and usage.
- **CLI `/usage` / `/cost` tool activity** — REPL usage summaries show
  turn counts, token high-water/output totals, and observed per-tool
  call counts/durations for the current session, plus clearly labeled
  estimated per-tool token/cost attribution weighted by observed tool
  duration when model rates are known. `/usage status|show|summary|tools`
  and matching `/cost ...` aliases print the same auditable view. True
  provider-measured per-tool token/cost attribution remains deferred.
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
  prompts already used by the desktop slash palette, capture GitHub PR
  review-thread IDs, and explicitly reply to a review thread with
  `/pr_comments reply <thread_id> <body>` or edit a review comment with
  `/pr_comments edit <comment_id> <body>`. CLI and desktop can resolve
  review threads by ID, and the shared GitHub GraphQL helper also
  supports reopening mistakenly resolved threads with
  `unresolveReviewThread`. CLI and desktop can also submit summary PR
  reviews with `/pr_comments review <event> <body>` using GitHub's
  `addPullRequestReview` mutation, and mark changed PR files viewed or
  unviewed with `/pr_comments viewed <path>` and
  `/pr_comments unviewed <path>` through `markFileAsViewed` /
  `unmarkFileAsViewed`. They can also create line-level pending review
  threads with `/pr_comments thread <path>:<line> <body>` through
  `addPullRequestReviewThread`, stage queued draft threads, and publish
  queued drafts together with a summary review event via
  `/pr_comments drafts submit <approve|comment|request_changes> [body]`.
- **CLI `/sandbox` command** — REPL users can inspect the effective
  strict bash sandbox profile with `/sandbox [info]`; `/sandbox reload`
  explains the CLI restart requirement.
- **CLI `/sandbox` aliases** — REPL users get explicit `/sandbox
  status|state|show|diagnostics|diag` handling for strict-profile
  inspection.
- **CLI `/vim` command** — REPL users get explicit `/vim`,
  `/vim status`, `/vim on`, and `/vim off` handling. `/vim on` enables a
  session-local Vim input mode in the raw REPL bar with insert/normal
  state, Esc, i/a/I/A, h/l/0/$, x, and Enter.
- **CLI `/ide` command** — REPL users get explicit `/ide`,
  `/ide status`, and `/ide open` handling so IDE integration requests
  report the current terminal/desktop handoff instead of falling through
  as agent text.
- **CLI `/bug` command** — REPL users get explicit `/bug report`,
  `/bug template`, and `/bug status` aliases for the diagnostic block so
  bug-report requests do not fall through as agent text.
- **CLI `/copy`, `/hotkeys`, and `/reload` aliases** — REPL users get
  explicit `/copy last|latest|response`, `/hotkeys status|show|list`,
  and `/reload config|session|now` handling so common Claude/Pi-style
  utility subcommands do not fall through as agent text.
- **CLI `/status` aliases** — REPL users get explicit `/status
  show|info|session` handling for session state so status requests do
  not fall through as agent text.
- **CLI `/doctor` aliases** — REPL users get explicit `/doctor
  status|health|diagnostics` handling for local health checks so
  diagnostic requests do not fall through as agent text.
- **CLI `/mode` and `/rename` aliases** — REPL users can use the
  desktop/Claude-style spellings for permission-mode switching and
  session naming.
- **CLI `/abort` command** — REPL users get an explicit status message
  pointing to the active-turn Ctrl+C abort path.
- **CLI `/abort` aliases** — REPL users get explicit `/abort
  status|cancel|stop` handling so abort requests do not fall through as
  agent text.
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
  in `~/.config/libertai/disabled-skills.toml`. Desktop Settings and
  CLI REPL `/skills [list|show <name>|enable <name>|disable <name>]`
  inspect or manage built-in, project, and user skills for future
  sessions without editing `SKILL.md` files.
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
  path, backend/model, mode, output style, token count, and context use;
  `/statusline command <shell>` can instead render the first output line
  from a dynamic shell command.

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
- **Review/verification discipline**: the built-in harness now tells
  the model to treat review requests as findings-first audits, cite
  `file_path:line_number`, avoid modifying files during review-only
  tasks, run checks that exercise the changed behavior, and report
  blocked verification honestly instead of overclaiming from unrelated
  tests.

After this sprint, parity-doc Sections A–G are all shipped, including
the model-facing memory guidance plus the review/verification posture
expected from Claude Code-style coding agents. Section H
(per-subagent prompts) remains gated on Phase 4D named-agent registry.

`../libertai-code-desktop/docs/claude-code-parity.md` has been refreshed
to move AcceptEdits, `libertai-harness`, desktop `!cmd`, custom slash
commands, and the first slash-routing batch to their current shipped or
partial states.

---

## Open questions (resolve before starting affected phases)

1. **Resolved: pi auto-compacts by default.** `pi_agent_rust` has a
   background compaction worker, explicit `/compact`, force-mode SDK
   compaction, lifecycle events, and extension hooks. LibertAI now pins
   a fork commit whose `SessionOptions` exposes per-session compaction
   overrides, and CLI/desktop share `code_auto_compaction_enabled`,
   `code_compaction_reserve_tokens`, and
   `code_compaction_keep_recent_tokens`.

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
11. *Users cannot see most raw tool calls; summarize decisive command
    results instead of pasting logs.*
12. *Don't create planning docs, status reports, TODO files, docstrings,
    or module comments unless the user asked for a durable artifact.*
13. *Review requests are findings-first audits. Do not edit files
    during review-only work.*
14. *Claim completion only from checks that exercise the changed
    behavior; report missing gates honestly.*

**Status**: shipped in `src/agent_skills/libertai-harness/SKILL.md`.

### 1B. Per-tool usage notes (upstream)

Pi's built-in tool descriptions now carry model-facing guidance for
read/edit/write/bash/grep/find/ls/hashline_edit plus background bash
companions, including cwd scoping, truncation behavior, hashline flows,
and destructive-shell caution. `libertai-harness` also keeps concise
per-tool usage notes in the appended prompt.

**Status**: shipped across `pi_agent_rust/src/tools.rs` and
`src/agent_skills/libertai-harness/SKILL.md`; remaining depth belongs
to future tools that do not exist yet.

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
operations before the user approves. Terminal approval prompts color
diff headers, additions, removals, and truncation markers. Successful
path-edit tool results now append the exact file-system delta observed
after execution, so previews and final changes can be compared.

**Files**: `src/commands/code_approvals.rs` (snapshot trigger),
`src/commands/code_diff.rs` (renderer).
**Status**: shipped.
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
`/skills`, `/template`, custom `/<name>` templates, `/status`, `/usage`/`/cost`,
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
**Desktop note**: common and diagnostic slash commands are now wired in
the desktop composer and command palette, including direct Settings-tab
targets for account, backends, defaults, agents, skills, hooks, MCP,
approvals, appearance, sandbox, and advanced. The remaining work here is
deeper Claude-specific semantics and any future decision to delegate
unknown typed `/foo` lines into pi's slash dispatcher instead of handling
them locally.

### 2F. Native `/init`

Shipped: CLI and desktop `/init` call `code_init::init_project`, which
creates `AGENTS.md` when missing and preserves existing files. CLI
`/init <project notes>` can add one user-provided project note to the
generated file without overwriting existing guidance, and CLI
`/init --agent <project notes>` now sends the same Claude-style
model-written initialization prompt through the active session for
inspect/propose/write flows; when existing guidance is present, that
prompt requires a fenced `AGENTS.md candidate`, a merge plan, and a
unified diff before any overwrite request. The initializer also exposes a no-write
candidate generator so desktop can show a merge candidate when
guidance already exists, and CLI `/init` prints that same no-write
candidate with a diff against the existing file and a numbered section
index plus per-section impact labels (new section, unchanged, or added
lines) when it leaves an existing `AGENTS.md` unchanged. The generated file
is deterministic and based on visible repo docs/manifests
(`README.md`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`)
plus common directory names. It now parses README title/summary,
manifest names, exact package script bodies, Go modules, common config
files such as Dockerfile / GitHub Actions / Makefile, and
CONTRIBUTING/EditorConfig guidance. CLI and desktop
`/init from-agent merge-lines` can now apply an assistant-proposed
candidate by appending only new lines inside matching `##` sections,
CLI and desktop `/init from-agent preview append|merge|merge-lines|replace` show
the resulting file before applying, and CLI plus desktop `/init from-agent preview
[append|merge|merge-lines] sections N[,M]` plus `/init from-agent
append|merge|merge-lines sections N[,M]` support numbered-section
review/apply flows alongside the desktop merge modal.
The desktop merge modal and typed slash previews show the same
per-section impact labels beside the selectable generated sections.
Remaining work is richer interactive
review controls around agent-written prose.

### 2E. `!` shell prefix in REPL

Prefix-`!` lines run a shell command in the current cwd and render the
captured output locally (Claude Code muscle memory). `!!` repeats the
previous shell escape in the same session. The CLI REPL now matches the
desktop composer escape and reuses the same bash wrapper argv when
`--sandbox=strict` is active.

**Files**: `src/commands/code_ui.rs`.
**Status**: shipped for local `!cmd` and `!!` repeat. Captured
stdout/stderr/exit summaries are now attached to the next text prompt
so the agent can reason from quick local shell checks without rerunning
them.
**Desktop note**: shipped on desktop, including `!!` repeat and
next-prompt shell-output context.

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

Shipped as an opt-in CLI/SDK feature: when `smart_approval_enabled =
true`, a flagged mutating tool call first asks
`smart_approval_model` with `max_tokens=16` for
APPROVE/DENY/ESCALATE. APPROVE runs without the manual prompt, DENY
returns a tool error, and ESCALATE/errors/malformed responses fall back
to the existing UI. The auxiliary request is capped to a 10-second
timeout and inherits the normal LibertAI API config. APPROVE and DENY
decisions emit a structured `smart_approval` tool update before the
tool runs or the denial result returns.
(`/tmp/hermes-agent/tools/approval.py:841-885`)

**Files**: `src/commands/code_approvals.rs`,
`src/commands/code_aux.rs`, `src/config.rs`.
**Status**: shipped for CLI/native sessions; desktop visibility polish
remains a handoff item.

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

### 4C. Compaction Wiring + SUMMARY_PREFIX Framing

Shipped: `pi_agent_rust` SDK sessions accept per-session compaction
overrides; `libertai-cli` stores shared code compaction settings and
threads them into one-shot, REPL, and Task subagent sessions; desktop
Advanced settings mirrors those fields and native sessions inherit them.
The compaction summary injected into model context is now framed as
reference-only background, with the recent conversation called out as
the authoritative active task.

**Files**: `pi_agent_rust/src/session.rs`,
`src/commands/code_session.rs`.

### 4D. Named sub-agent registry

Shipped: the `task` tool accepts `subagent_type` and discovers
Claude-compatible `.claude/agents/<name>.md`, project
`.libertai/agents/<name>.md`, user `~/.claude/agents`, and user
`~/.config/libertai/agents`. Agent files carry frontmatter for
`description:`, `tools:`, and `model:` plus a body system prompt;
`tools:` / `allowed-tools:` accepts inline values or YAML block lists.
CLI `/agents` lists discovered definitions, `/agent <name> <task>`
routes through the active agent with an instruction to call the `task`
tool for that named sub-agent, and `/agent --background <name> <task>`
or `/agent --detached <name> <task>` starts a detached terminal child
process with PID/log reporting while the current REPL remains usable.
`/agents background` lists recorded detached runs with
total/running/exited/unknown status counts, `/agents background
show [pid|latest]` inspects one run's status/backend/cwd/log/prompt
metadata, `/agents background log [pid|latest]` tails their saved output,
and `/agents background kill <pid>` stops a running child process;
`/agents background prune` removes non-running records from the durable
list. CLI
`/agents show <name>` inspects a definition with source path, metadata,
and prompt preview, and `/agents create [--worktree] <name>
[description]` scaffolds project-local `.libertai/agents/<name>.md`
definitions. Worktree isolation now uses a detached git worktree when
possible and a copied temp workspace snapshot outside git. Remaining work
is pi-level child event streaming and durable scheduling controls for
detached agents.

**Files**: `src/commands/code_agents.rs`, `src/commands/code_task.rs`,
`src/commands/code_ui.rs`.

### 4E. Memory v1

Single `~/.config/libertai/projects/<cwd-hash>/MEMORY.md` per project,
loaded alongside `AGENTS.md`. Add a typed `/remember <kind>: <text>`
slash command that appends a categorized dated bullet. CLI `/memory`
can inspect typed counts, show the resolved file/path, open it in
`$VISUAL`/`$EDITOR`, list per-entry memory sidecars, inspect one sidecar
with `/memory file <number|path>`, and clear it with a backup; desktop `/memory` can
inspect and edit it, list per-entry memory sidecars, and show one
sidecar with `/memory file <number|path>`. CLI and desktop `/memory references` verify
`[reference]` bullets by marking external URLs and checking local path
targets against the session cwd. Broader memory curation remains deferred.

**Files**: new `src/commands/code_memory.rs`,
`pi_agent_rust/src/app.rs` (system-prompt assembly hook).
**Effort**: S-M (1-2 days).

### 4F. Custom slash commands

Shipped: CLI and desktop discover project `.claude/commands`,
`.libertai/commands`, and legacy `.liberclaw/commands`, plus user
`~/.claude/commands` and `~/.config/libertai/commands`. Claude-compatible
skill entrypoints under `.claude/skills/<name>/SKILL.md`,
`.libertai/skills/<name>/SKILL.md`, `~/.claude/skills/<name>/SKILL.md`,
and `~/.config/libertai/skills/<name>/SKILL.md` are also invocable as
`/<name>` and override same-named command files. Markdown command files
are discovered recursively; nested paths are shown as namespace metadata,
so `commands/team/audit.md` appears as `/audit` from the `team`
namespace. Each file becomes a prompt template; frontmatter may define
`description:` and `argHint:`. Command and skill entrypoints append
`when_to_use` to the slash description and fall back to the first body
paragraph when `description:` is omitted; combined descriptions are
capped to Claude's 1,536-character listing limit.
`user-invocable: false` entrypoints are hidden from slash invocation.
CLI and desktop support `/template <name> [args]` and direct
`/<name> [args]` dispatch with
Claude-style `$ARGUMENTS`,
`$ARGUMENTS[0]`, `$0` / `$1` positional arguments, implicit
`ARGUMENTS: ...` append for templates without placeholders, named
`arguments:` frontmatter placeholders such as `$path` from inline
strings, inline lists, or simple YAML lists, Claude context variables
`${CLAUDE_SESSION_ID}`, `${CLAUDE_EFFORT}`, and
`${CLAUDE_SKILL_DIR}`, and legacy `{{args}}` substitution.

**Files**: `src/commands/code_slash_registry.rs`, `src/commands/code_ui.rs`.

---

## Phase 5 — Largest items, lowest priority

### 5A. Hooks config schema

First CLI slices shipped: `~/.config/libertai/config.toml` accepts
`[[hooks.UserPromptSubmit]]`, `[[hooks.PreToolUse]]`, and
`[[hooks.PostToolUse]]`, native task-tool `[[hooks.SubagentStop]]`,
plus lifecycle `[[hooks.SessionStart]]`,
`[[hooks.Stop]]`, `[[hooks.SessionEnd]]`, and agent-requested
`[[hooks.Notification]]` rows. Matching hooks
receive Claude-style JSON payloads on stdin. `UserPromptSubmit` hooks run
before the prompt reaches the agent, can block on nonzero exit, and can
append `additionalContext`; rows with `continueOnBlock = true` report
nonzero exits without blocking the prompt. `PreToolUse` stdout JSON can
`allow`, `ask`, `defer`, `deny`, rewrite `updatedInput`, or attach
`additionalContext` through the existing approval-policy path.
`PostToolUse` hooks run after tool execution and cannot alter the result; `SubagentStop` hooks run
after native `task` tool subagents finish. Lifecycle hooks run around
native CLI sessions/turn stops and warn on nonzero exit; `Notification`
hooks run after the `push_notification` tool requests a user notification.
Tool hook matchers
support case-sensitive exact names, `*` globs, `|` alternatives,
`regex:<pattern>`, and slash-delimited regex patterns that can contain
alternation pipes. Imported matcher arrays and `matchers` aliases also
deserialize to the same pipe-separated matcher form. Tool hook rows can also set handler `if` filters such
as `Bash(rm *)` to match a tool name plus argument glob. Claude-style nested
hook groups with `hooks = [...]` expand into normal CLI hook rows while inheriting
group matcher, filter, timeout, async, continueOnBlock, once,
asyncRewake, shell, source, status, enabled, and unknown metadata
defaults. Rows can set
`async = true` (or imported `asyncHook =
true`) to launch a command or HTTP hook without waiting for completion;
async hook output is discarded and cannot affect prompt/tool decisions.
CLI command hook rows also support separate `args` arrays; args are shell
quoted and appended to the configured command line, Claude-style command
arrays deserialize into the first item as `command` plus the rest as `args`,
and numeric-string `timeout` values deserialize to seconds. CLI HTTP hook rows use
`type = "http"`, `url`, optional `headers`,
`allowedEnvVars`, timeout, and `continueOnBlock`, POST the same JSON
payloads as command hooks, and can return the same JSON decision/context
fields for UserPromptSubmit and PreToolUse. CLI prompt hook rows use
`type = "prompt"`, `prompt`, optional `model`, and the configured LibertAI
chat endpoint, returning the model message as hook output. CLI agent hook
rows use `type = "agent"`, `prompt`, and optional `model` to run an
ephemeral read-only code-agent session with `read`, `grep`, `find`, and
`ls`, returning the child agent's final text as hook output. Hook rows with
`once = true` run at most once per native CLI
session/event/index, and named `source`, `statusMessage`, plus
`asyncRewake` metadata round-trip and display in `/hooks`. Unknown
preserved metadata keys are also listed in `/hooks`. CLI MCP-tool hook rows use
`type = "mcp_tool"` plus Claude-imported `type = "mcp-tool"` aliases,
`server`, `tool`, and optional JSON `input` metadata, launching stdio
servers, POSTing Streamable HTTP servers, or using legacy SSE endpoints
configured under `mcpServers` and calling the named tool through MCP
`initialize` plus `tools/call`.
Unknown/less-common hook fields
are flattened into each hook row and round-trip through TOML config saves.
`/hooks` and `libertai status` report configured runnable hooks, and
`/hooks show <event>` expands one event bucket with per-row matcher,
target, flag, timeout, source, status-message, HTTP header/env counts,
MCP input presence, and preserved metadata keys without printing secret
header or environment values.

Remaining work: any pi-level typed hook dispatcher and persistent/live CLI MCP
connection management.

**Files**: `src/config.rs`,
`src/commands/code_hooks.rs`,
`pi_agent_rust/src/sdk.rs` (hook dispatcher; may need upstream).
**Effort remaining**: M-L.

### 5B. MCP support

Spawn user-configured MCP servers (stdio/HTTP/SSE), expose their tools
to the agent. The CLI now has narrow stdio, Streamable HTTP, and legacy
SSE MCP clients for MCP-tool hook handlers configured through
`mcpServers`, terminal `/mcp probe` can initialize configured stdio,
Streamable HTTP, and legacy SSE servers and list their tools/resources/prompts
for diagnostics, and native CLI sessions now register an approval-gated
generic `mcp_call` tool when `mcpServers` exist. Terminal config can now
also preserve cached `tools = [...]` metadata per MCP server and expose
enabled entries as named `mcp__server__tool` tools, plus cached
`resources = [...]` and `prompts = [...]` metadata through read-only
`mcp_read_resource` and `mcp_get_prompt` tools. Terminal `/mcp status`
now reports native exposure coverage for `mcp_call`, named cached tools,
resource/prompt bridge tools, and resource subscription candidates. It
still does not keep persistent MCP connections. Terminal `/mcp probe --save` and `/mcp refresh`
can refresh discovery caches for future code sessions, and `/mcp reset`
explicitly reports that terminal MCP calls are short-lived while Desktop owns
the richest stdio/HTTP/SSE live registry today. Terminal `/mcp show
<server>` inspects one configured server without exposing secret
env/header values, including transport, target, cache counts, and cached
tools/resources/prompts.

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
