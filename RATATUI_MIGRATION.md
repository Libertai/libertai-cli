# Ratatui Migration Plan

## Design Read

Developer tool REPL for engineers, with a clean, dense, modern language.
Think lazygit's density meets Linear's restraint — not flashy, just good.

## Dials

```
DESIGN_VARIANCE: 3   — predictable, functional layout
MOTION_INTENSITY: 4  — subtle spinner, no flashy transitions
VISUAL_DENSITY: 7    — cockpit: dense but breathable, every cell earns its space
```

## Color System (`theme.rs`)

```rust
// Semantic palette — inspired by One Dark + Catppuccin Mocha
// Leave background as terminal default (don't paint over it)

pub struct Theme {
    text_primary:   Color::Reset,       // terminal default
    text_muted:      Color::DarkGray,    // hints, metadata, dim previews
    text_bold:       Color::Reset,       // bold weight for emphasis
    accent:          Color::Cyan,        // brand (matches existing CYAN)
    success:         Color::Green,        // completed, allow
    warning:         Color::Yellow,       // needs input, plan mode
    error:           Color::Red,           // failed, deny
    info:            Color::Blue,           // informational
    agent_colors: [Red, Green, Yellow, Blue, Magenta, Cyan,
                   Rgb(216,144,60), Rgb(220,120,180)],  // orange, pink
}
```

Rules:
- One accent (cyan) — used for `❯`, tool markers, brand
- Semantic colors only for status (green/yellow/red)
- Dim gray for all metadata, previews, dividers
- No gradients, no neon glow, no purple
- Pure black/white banned — use `Reset` (terminal default)

## Glyph System

```
Status icons (single-cell width):
  SPAWNING:    "○"
  WORKING:     "✽"
  NEEDS_INPUT: "⏸"
  IDLE:        "∙"
  COMPLETED:   "✓"
  FAILED:      "✗"
  STOPPED:     "⊘"

Capability markers:
  READ_ONLY:   ""      // no prefix (clean)
  READ_WRITE:  "✎ "   // pencil prefix

Turn markers:
  ASSISTANT:   "●"    // bold, default foreground
  TOOL:        "●"    // cyan
  USER:        "❯"    // bold cyan
  QUEUED:      "›"    // dim

Spinner (braille, 80ms tick):
  ⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏
```

## Layout

```
┌────────────────────────────────────────────────────┐
│                                                     │
│  Scrollback (scrollable transcript)                 │ ← Paragraph + Scrollbar
│                                                     │
│  ● assistant text with markdown rendering...        │ ← bold ● + styled text
│    ● bash(result preview)                          │ ← cyan ● + bold tool + dim args
│                                                     │
│  ❯ user prompt                                     │ ← bold cyan ❯
│                                                     │
│  ● more assistant text...                          │
│                                                     │
├────────────────────────────────────────────────────┤ ← dim ─ divider
│ ── agents (2) ─────────────────────────────────── │ ← dim agent header
│  ✽ reviewer    fix the build        read    12s   │ ← status + name + prompt + tool + time
│  ∙ builder     write tests          edit     4s   │
│  ⠹ thinking 23s · 1.2k tokens                     │ ← braille spinner + dim metadata
│  › queued: fix the tests                           │ ← dim queued preview
│  ── claude-sonnet-4 · Normal · 42% ctx ────────── │ ← dim rule line (centered)
│  ❯ _                                              │ ← bold cyan ❯ + textarea cursor
└────────────────────────────────────────────────────┘
```

Constraint math:
- Footer height = agent_count + 1 (header if agents) + 1 (spinner) + queued_count + 1 (rule) + 1 (input)
- Scrollback = terminal_height - footer_height
- If no agents: omit agent header + rows (footer shrinks)
- If no queued: omit queued previews (footer shrinks)

## Widget Mapping

| Component | Current (manual) | ratatui widget |
|-----------|-----------------|----------------|
| Scrollback transcript | `MarkdownStream` + `println!` | `Paragraph` + `Scrollbar` (tui-scrollbar) |
| Markdown rendering | `MarkdownStream` (hand-rolled) | `ratatui-markdown` crate |
| Input bar | `read_line` (416 lines) + `repaint` (154 lines) | `tui-textarea` widget |
| Spinner | `SpinnerCore::draw` (200 lines) | `Span` with braille char rotation |
| Agent panel | `agent_footer_line` + manual rows | `List` widget with `ListItem` |
| Rule line (status bar) | `rule_chip` (manual padding) | `Paragraph` centered with `─` fill |
| Divider | manual `─` strings | `Block` with `Borders::TOP` or `Paragraph` with line fill |
| Approval modal | `code_term.rs` prompt (raw mode) | `Clear` + `Block::Rounded` popup |
| Ask user menu | `ask_user` (raw mode) | `List` in centered popup |
| Agent view TUI | `code_agent_view.rs` (1112 lines) | `List` + `Paragraph` (log peek) |

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Main Thread (ratatui event loop, 20ms tick)         │
│                                                      │
│  ┌─────────────┐  ┌──────────┐  ┌─────────────────┐  │
│  │  App State  │←─│  Events  │  │  Terminal::draw │  │
│  │  (Model)    │  │  (Keys,  │  │  (View)         │  │
│  │            │  │   Resize, │  │                 │  │
│  │            │  │   Mouse,  │  │                 │  │
│  │            │  │   Timer)  │  │                 │  │
│  └──────┬─────┘  └──────────┘  └─────────────────┘  │
│         │                                            │
│    mpsc │  AgentEvent  ←────  Background Thread      │
│    mpsc │  Cmd ────────→     (asupersync runtime +  │
│         │                     pi AgentSessionHandle) │
└─────────┴────────────────────────────────────────────┘
```

- Main thread: ratatui's `Terminal::draw()` + crossterm event poll (20ms tick)
- Background thread: `asupersync` runtime + `handle.prompt()` with event callback
- Bridge: `std::sync::mpsc` channels
  - `AgentEvent` channel (bg -> main): text deltas, tool markers, turn end, usage
  - `Command` channel (main -> bg): submit prompt, abort, queued messages
- Approval modals: ratatui popup overlay, response via oneshot channel

## App State Machine

```
┌─────────┐  Enter submit  ┌──────────┐  AgentEnd  ┌─────────┐
│  Idle   │──────────────→│ Streaming │──────────→│  Idle   │
│ (input  │               │ (footer   │            │         │
│  bar)   │←──────────────│  active)  │←─────────┤         │
└─────────┘  turn done    └──────────┘   error/   └─────────┘
     │                                    abort
     │ /command -> dispatch
     ↓
┌──────────┐
│ Command  │ -> result printed to scrollback -> back to Idle
│ Overlay  │
└──────────┘
     │ approval needed
     ↓
┌──────────┐
│ Approval │ -> Allow/Deny/AlwaysAllow -> back to Streaming
│ Modal    │
└──────────┘
```

## Module Structure

```
src/commands/code_tui/
├── mod.rs          — module registration
├── app.rs          — App struct (state machine), event loop, channels
├── view.rs         — ratatui layout: top (scrollback) + bottom (footer)
├── scrollback.rs   — conversation history widget (Paragraph + scroll)
├── input.rs        — input bar (tui-textarea + mode chip + rule line)
├── footer.rs       — spinner + queued previews + agent panel
├── agents_panel.rs — agent list widget (status icons, prompt preview)
├── approvals.rs    — inline approval/ask_user overlays (ratatui modal)
├── markdown.rs     — markdown rendering adapter (ratatui-markdown)
├── agent_view.rs   — standalone `libertai agents` TUI (ratatui rewrite)
└── theme.rs        — colors, styles, AgentColor -> ratatui Color mapping
```

## Dependencies

```toml
ratatui = { version = "0.30", features = ["all-widgets", "macros"] }
tui-textarea = "0.7"
ratatui-markdown = { version = "0.3", default-features = false, features = ["markdown"] }
tui-scrollbar = "0.2"
```

Already available: `crossterm` (events, terminal, cursor)

## Migration Scope

- Migrate: REPL (`code_ui.rs` rendering layer), approval UI (`code_term.rs`), agent view (`code_agent_view.rs`)
- Keep old: One-shot `code` command (`TurnRenderer` on stderr), `--print` mode
- Keep unchanged: Slash command logic, hooks, session persistence, config, factory, team/mailbox tools

## Execution Order

1. Add deps, verify build
2. Create `code_tui/` module skeleton with stubs
3. `theme.rs` -> `app.rs` -> `view.rs` -> `scrollback.rs` -> `footer.rs` -> `input.rs` -> `agents_panel.rs` -> `approvals.rs` -> `markdown.rs` -> `agent_view.rs`
4. Wire `run_interactive` -> `code_tui::app::run()`
5. Wire `RatatuiApprovalUi` into factory
6. Wire `libertai agents` -> `code_tui::agent_view::run()`
7. Strip old rendering code from `code_ui.rs`, `code_term.rs`, `code_agent_view.rs`
8. Fix tests, clippy, commit, push

## What Gets Simpler

- No manual cursor positioning, erase sequences, or scroll regions
- No hide()/show()/suspend/resume dance
- No drawn_rows tracking, no last_term_height, no DECSTBM
- No relative vs. sticky render paths
- No ticker thread (ratatui's event loop tick replaces it)
- No MoveToPreviousLine/Clear arithmetic in repaint
- Resize, scroll, alt-screen, double-buffering all handled by ratatui

## Visual Polish Checklist

- [x] Coherent semantic color palette (not ad-hoc ANSI)
- [x] Consistent glyph system (one weight, one style)
- [x] Clear visual hierarchy (bold/normal/dim)
- [x] Dense but breathable layout (every cell earns its space)
- [x] Subtle dividers (dim --, not heavy boxes)
- [x] Smooth spinner (braille, 80ms)
- [x] Syntax-highlighted code blocks
- [x] Proper scrollbar for transcript
- [x] Clean approval modals (rounded popup, not screen takeover)
- [x] No AI-purple, no neon, no gradients
- [x] One accent color (cyan), locked across all components
- [x] Agent colors rotate consistently (same mapping everywhere)
