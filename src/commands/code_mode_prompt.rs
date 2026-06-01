//! Plan-mode system-prompt addendum.
//!
//! When `libertai code` starts a session under `Mode::Plan`, this
//! block is prepended to the pillar's `append_system_prompt` so the
//! model produces a numbered plan rather than free-form text or
//! mutations. Tool gating still blocks writes/edits/bash at the
//! `ApprovalTool` layer; this block is the **prompt** side of plan
//! mode, separate from the tool-gating side.
//!
//! Limitation: the addendum is added once at session creation. Toggling
//! plan mode mid-session via Shift+Tab changes tool behavior but does
//! NOT re-revise the system prompt — pi doesn't currently expose a way
//! to rewrite the prompt of a live session.

use crate::commands::code_factory::Mode;

pub const PLAN_MODE_ADDENDUM: &str = "\n\n## Plan mode\n\n\
You are in plan mode. Your job this turn is to produce a clear, \
actionable plan for the user to approve — not to execute mutations.\n\n\
- Use the read-only tools (read, grep, find, ls) freely to understand \
the codebase before writing the plan.\n\
- Do NOT call write, edit, hashline_edit, or bash mutations — those \
are blocked anyway, but don't waste turns attempting them.\n\
- Finish with a numbered list of concrete steps under a `### Plan` \
heading. Each step names the file(s) it touches and the change in \
one line. The user will approve, redirect, or hand off to a normal \
session for execution.\n\
- If the request is exploratory or open-ended, the plan can be \
shorter and present 2-3 alternatives with tradeoffs rather than one \
sequence.\n";

/// Prepend the plan-mode addendum to the existing `append_system_prompt`
/// when the session starts under `Mode::Plan`. Leaves it untouched in
/// other modes.
pub fn apply(append: Option<String>, mode: Mode) -> Option<String> {
    if mode != Mode::Plan {
        return append;
    }
    let mut out = String::from(PLAN_MODE_ADDENDUM);
    if let Some(existing) = append {
        out.push_str(&existing);
    }
    Some(out)
}
