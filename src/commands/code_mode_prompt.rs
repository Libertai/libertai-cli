//! Plan-mode system-prompt addendum.
//!
//! When `libertai code` starts a session under `Mode::Plan`, this
//! block is prepended to the pillar's `append_system_prompt` so the
//! model produces a numbered plan rather than free-form text or
//! mutations. Tool gating still blocks writes/edits/bash at the
//! `ApprovalTool` layer; this block is the **prompt** side of plan
//! mode, separate from the tool-gating side.
//!
//! The addendum is added at session creation when the session starts in
//! plan mode, and the REPL also prefixes each submitted turn with the
//! active-mode guidance so runtime toggles are visible to the model even
//! though pi does not expose live system-prompt rewriting.

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

pub const NORMAL_MODE_TURN_GUIDANCE: &str = "\
Active mode: normal. Mutating tools are available subject to the normal \
approval flow.\n\n";

pub const ACCEPT_EDITS_MODE_TURN_GUIDANCE: &str = "\
Active mode: accept-edits. File edit tools may be auto-approved; bash and \
other broad mutations still follow the normal approval flow.\n\n";

pub const PLAN_MODE_TURN_GUIDANCE: &str = "\
Active mode: plan. Produce a clear, actionable plan and do not attempt \
mutating tools such as bash, write, edit, or hashline_edit.\n\n";

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

/// Prefix a user turn with the current runtime mode. This keeps Shift+Tab
/// and `/plan` changes visible to the model on the next prompt without
/// rebuilding the session or mutating pi's system prompt.
pub fn apply_turn_guidance(prompt: String, mode: Mode) -> String {
    let guidance = match mode {
        Mode::Normal | Mode::Bypass => NORMAL_MODE_TURN_GUIDANCE,
        Mode::AcceptEdits => ACCEPT_EDITS_MODE_TURN_GUIDANCE,
        Mode::Plan => PLAN_MODE_TURN_GUIDANCE,
    };
    format!("{guidance}{prompt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_guidance_reflects_runtime_mode() {
        assert!(apply_turn_guidance("do work".to_string(), Mode::Normal)
            .starts_with("Active mode: normal."));
        assert!(
            apply_turn_guidance("do work".to_string(), Mode::AcceptEdits)
                .starts_with("Active mode: accept-edits.")
        );
        let plan = apply_turn_guidance("make a plan".to_string(), Mode::Plan);
        assert!(plan.starts_with("Active mode: plan."));
        assert!(plan.ends_with("make a plan"));
    }
}
