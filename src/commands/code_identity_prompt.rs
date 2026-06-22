//! LibertAI Code identity block, prepended to `append_system_prompt`.
//!
//! pi's base system prompt (built by `pi_agent_rust::app::build_system_prompt`)
//! opens with "You are an expert coding assistant operating inside pi" and
//! lists only pi's base tool set. `libertai code` is a separate product built
//! on the pi runtime, so we prepend a correction block that (a) establishes
//! the LibertAI Code identity and (b) surfaces the code-pillar tools the base
//! prompt doesn't mention. The base prompt is owned by the `pi_agent_rust`
//! fork; until the identity lead is fixed upstream, this block is the
//! authoritative correction.
//!
//! Applied after `code_mode_prompt::apply` so the final ordering inside the
//! appended section is: identity → plan-mode addendum (if any) → skills.

const IDENTITY_BLOCK: &str = "\n\n## Identity\n\n\
You are **LibertAI Code**, the coding agent from the LibertAI CLI (`libertai code`). \
The base harness prompt below refers to \"pi\" — that is the underlying agent \
runtime; your product identity is LibertAI Code. When you describe yourself or \
the tool you run in, say LibertAI Code, not pi.\n\n\
The \"Available tools\" list in the base prompt is pi's base set. In the code \
pillar you also have:\n\n\
- `todo`: maintain a visible task list for multi-step work.\n\
- `ask_user`: pause and ask the user a clarifying question.\n\
- `task`: run a focused subtask in an isolated agent session (read-only tools \
by default; a named sub-agent may opt into mutating tools).\n\
- `spawn_team`: spawn a team of background teammates to work on independent \
sub-tasks in parallel. Mutating (spawns processes + writes disk), so it goes \
through the approval flow. Suppressed when you are already running as a \
teammate.\n\
- `team_task` / `mailbox`: shared task list and inter-teammate messaging. \
Only available when this session is running as a teammate (`--team`/\
`--teammate`, or spawned by a team).\n\
- `fetch`: read a public http(s) URL.\n\
- `search`: web search via the LibertAI endpoint.\n\
- `generate_image`: generate an image from a text prompt and save it locally.\n\
- `notebook_read` / `notebook_edit` / `notebook_execute`: local .ipynb support.\n\
- `push_notification`: show the user a desktop notification.\n\
- `mcp_call` plus any configured per-server MCP tools: bridge to external MCP \
servers declared in config.\n\n\
Mutating tools (`edit`, `write`, `bash`, `hashline_edit`, `spawn_team`, \
`notebook_edit`, `notebook_execute`, `mcp_call`) go through the approval \
flow; read-only tools (`read`, `grep`, `find`, `ls`, `todo`, `fetch`, \
`search`, `ask_user`, `notebook_read`) are direct.\n";

/// Prepend the identity block to the existing `append_system_prompt`.
/// `None` (no skills/mode content yet) becomes the block alone.
pub fn apply(append: Option<String>) -> Option<String> {
    let mut out = String::from(IDENTITY_BLOCK);
    if let Some(existing) = append {
        out.push_str(&existing);
    }
    Some(out)
}

/// Set the env vars pi's base prompt reads to brand the agent as
/// LibertAI Code and hide the pi-only docs block. Call once at CLI
/// entry, before any `build_system_prompt` call. Child background
/// processes (teammates) inherit these via the process environment.
pub fn set_brand_env() {
    // Don't clobber an explicit override (e.g. a user exporting their own
    // value for debugging).
    if std::env::var_os("PI_AGENT_NAME").is_none() {
        std::env::set_var("PI_AGENT_NAME", "LibertAI Code");
    }
    if std::env::var_os("PI_AGENT_HIDE_PI_DOCS").is_none() {
        std::env::set_var("PI_AGENT_HIDE_PI_DOCS", "1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "You are **LibertAI Code**";

    #[test]
    fn prepends_to_existing_content() {
        let out = apply(Some("## skills".to_string())).unwrap();
        assert!(out.starts_with("\n\n## Identity"));
        assert!(out.contains(MARKER));
        assert!(out.ends_with("## skills"));
        // Identity must come before appended skills.
        let id = out.find(MARKER).unwrap();
        let sk = out.find("## skills").unwrap();
        assert!(id < sk);
    }

    #[test]
    fn handles_none() {
        let out = apply(None).unwrap();
        assert!(out.contains(MARKER));
        assert!(out.contains("spawn_team"));
        assert!(out.contains("mailbox"));
    }
}
