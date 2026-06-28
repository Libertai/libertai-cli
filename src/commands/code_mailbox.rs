//! The `mailbox` tool — file-based message passing for M4 teammates.
//!
//! In M4 teammates (separate OS processes spawned by a team) need to
//! exchange messages. The transport is the shared filesystem: each
//! teammate has a mailbox directory at
//! `.libertai/teams/<team-name>/mailbox/<teammate-name>/`, and every
//! message is a single JSON file written there by the sender. The
//! [`MailboxTool`] lets a teammate send messages to other teammates and
//! check their own inbox. Polling-based — agents call `check` when
//! ready to act, there is no push notification.
//!
//! Each message file is named `<sent_at_ms>-<from>-<id>.json` so a
//! directory listing is already time-ordered. The [`MailMessage`]
//! schema carries `read` so `check` can return only new mail and flip
//! the flag in place by rewriting the same file (the id, sender, and
//! timestamp never change, so the filename is stable across the rewrite).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use uuid::Uuid;

const NAME: &str = "mailbox";
const LABEL: &str = "Mailbox";
const DESCRIPTION: &str = concat!(
    "Send and receive messages to/from teammates. Use `check` to read ",
    "your inbox (returns unread messages and marks them as read). Use ",
    "`send` to send a message to another teammate by name. Messages are ",
    "delivered instantly via the shared filesystem. Check your inbox ",
    "regularly to coordinate with the team."
);

/// One message in a teammate's mailbox. Stored as a single JSON file
/// (see [`message_file_name`]); `id` is the stable key the `check`
/// operation uses to flip `read` in place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub sent_at_ms: u64,
    pub read: bool,
}

/// The operation a `mailbox` call selects. Kept separate from the raw
/// input string so [`parse_action`] is unit-testable without file I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailAction {
    Check,
    Send,
}

/// Parsed payload of a `mailbox` call. `action` selects the operation;
/// `to`/`subject`/`body` are required for `send` and ignored by `check`.
#[derive(Debug, Deserialize)]
struct MailboxInput {
    action: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// The `mailbox` tool. Bound to one teammate (by name) and one team
/// directory on disk (`.libertai/teams/<team-name>/`); the mailbox
/// subdirectory lives under `team_dir/mailbox/<teammate-name>/`.
pub struct MailboxTool {
    team_dir: PathBuf,
    teammate_name: String,
}

impl MailboxTool {
    pub fn new(team_dir: PathBuf, teammate_name: String) -> Self {
        Self {
            team_dir,
            teammate_name,
        }
    }
}

impl Default for MailboxTool {
    fn default() -> Self {
        Self::new(PathBuf::new(), String::new())
    }
}

#[async_trait]
impl Tool for MailboxTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["check", "send"],
                    "description": "Operation to perform."
                },
                "to": {
                    "type": "string",
                    "description": "Recipient teammate name (for send)."
                },
                "subject": {
                    "type": "string",
                    "description": "Message subject (for send)."
                },
                "body": {
                    "type": "string",
                    "description": "Message body (for send)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: MailboxInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(err_output(&format!("invalid `mailbox` payload: {e}")));
            }
        };

        match parse_action(&parsed.action) {
            Some(MailAction::Check) => Ok(self.do_check()),
            Some(MailAction::Send) => Ok(self.do_send(&parsed)),
            None => Ok(err_output(&format!(
                "unknown `action`: {} (expected `check` or `send`)",
                parsed.action
            ))),
        }
    }

    fn is_read_only(&self) -> bool {
        // `send` writes a JSON file to a teammate's mailbox; `check`
        // mutates `read` flags in place. Both touch the filesystem.
        false
    }
}

impl MailboxTool {
    /// `check`: read own inbox, format unread messages, then mark each
    /// returned message as read by rewriting its file with `read: true`.
    fn do_check(&self) -> ToolExecution {
        let dir = mailbox_dir_for(&self.team_dir, &self.teammate_name);
        if !dir.exists() {
            return text_output("No messages.");
        }
        let all = read_mailbox(&dir);
        // Snapshot the unread ids before formatting so we mark exactly
        // what we surface, even if `read_mailbox` ordering shifts later.
        let unread_ids: Vec<String> = all
            .iter()
            .filter(|m| !m.read)
            .map(|m| m.id.clone())
            .collect();
        if unread_ids.is_empty() {
            return text_output("No new messages.");
        }
        let text = format_inbox(&all);
        // Best-effort: a file vanishing between read and mark should
        // not drop the inbox we already promised the agent.
        for id in &unread_ids {
            let _ = mark_read(&dir, id);
        }
        text_output(&text)
    }

    /// `send`: validate fields, build a [`MailMessage`], and write it
    /// into the recipient's mailbox directory.
    fn do_send(&self, parsed: &MailboxInput) -> ToolExecution {
        // Treat missing or empty strings as invalid up front so we
        // never write a message the recipient can't make sense of.
        let to = parsed.to.as_deref().unwrap_or("");
        let subject = parsed.subject.as_deref().unwrap_or("");
        let body = parsed.body.as_deref().unwrap_or("");
        if to.is_empty() || subject.is_empty() || body.is_empty() {
            return err_output("`send` requires non-empty `to`, `subject`, and `body`");
        }

        let msg = MailMessage {
            id: format!("msg-{}", short_uuid()),
            from: self.teammate_name.clone(),
            to: to.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            sent_at_ms: now_epoch_ms(),
            read: false,
        };

        let dir = mailbox_dir_for(&self.team_dir, to);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err_output(&format!("create mailbox dir {}: {e}", dir.display()));
        }
        if let Err(e) = write_message(&dir, &msg) {
            return err_output(&format!("write message: {e}"));
        }
        text_output(&format!("Message sent to {to}: {subject}"))
    }
}

// ---- public free helpers (used by the agent-view badge + tests) ----

/// Count unread messages in a teammate's mailbox directory.
/// Returns 0 if the directory doesn't exist.
pub fn count_unread(mailbox_dir: &Path) -> usize {
    read_mailbox(mailbox_dir).iter().filter(|m| !m.read).count()
}

/// List all messages in a teammate's mailbox, newest-first.
pub fn list_messages(mailbox_dir: &Path) -> Vec<MailMessage> {
    read_mailbox(mailbox_dir)
}

/// Mark a message as read by its id. Rewrites the message file in
/// place (the id, sender, and timestamp are stable, so the filename
/// is unchanged across the rewrite). Bails if no message with the id
/// is present in the mailbox.
pub fn mark_read(mailbox_dir: &Path, message_id: &str) -> Result<()> {
    let mut msg = read_mailbox(mailbox_dir)
        .into_iter()
        .find(|m| m.id == message_id)
        .with_context(|| format!("message not found: {message_id}"))?;
    msg.read = true;
    write_message(mailbox_dir, &msg)
}

// ---- file I/O helpers ----

/// Resolve a teammate's mailbox directory under a team directory:
/// `team_dir/mailbox/<teammate>`. `pub(crate)` so the `send_message`
/// tool (M5/#21) reuses the same path convention.
pub(crate) fn mailbox_dir_for(team_dir: &Path, teammate: &str) -> PathBuf {
    team_dir.join("mailbox").join(teammate)
}

/// Current epoch time in milliseconds. Mirrors the private helper in
/// `code_team_spawn`; falls back to 0 if the clock is before epoch.
pub(crate) fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// First 8 hex chars of a fresh v4 UUID — short enough for a filename
/// collision-resistant within a single mailbox's lifetime.
pub(crate) fn short_uuid() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Filename for a message: `<sent_at_ms>-<from>-<id>.json`. Stable
/// across a `read` flip since `id`, `from`, and `sent_at_ms` never
/// change after the message is written.
fn message_file_name(msg: &MailMessage) -> String {
    format!("{}-{}-{}.json", msg.sent_at_ms, msg.from, msg.id)
}

/// Read every `*.json` file in a mailbox, parse each as a
/// [`MailMessage`], and return them newest-first. A missing or
/// unreadable directory yields an empty vec; a single corrupt file is
/// skipped rather than failing the whole read.
fn read_mailbox(dir: &Path) -> Vec<MailMessage> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out, // missing or unreadable → empty
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<MailMessage>(&content) {
            Ok(m) => out.push(m),
            // Skip corrupt files so one bad message can't blank the inbox.
            Err(_) => continue,
        }
    }
    // Newest first by timestamp.
    out.sort_by_key(|m| std::cmp::Reverse(m.sent_at_ms));
    out
}

/// Serialize a message to JSON and write it into `dir` under its
/// [`message_file_name`]. Overwrites an existing file with the same
/// name (used by [`mark_read`] to flip `read` in place).
/// `pub(crate)` so the `send_message` tool (M5/#21) reuses it.
pub(crate) fn write_message(dir: &Path, msg: &MailMessage) -> Result<()> {
    let json =
        serde_json::to_string(msg).with_context(|| format!("serialize message {}", msg.id))?;
    let path = dir.join(message_file_name(msg));
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

// ---- pure formatting/parsing helpers (unit-tested, no file I/O) ----

/// Render a slice of messages as the text returned by `check`. Only
/// unread messages (`read == false`) are shown; read ones are skipped
/// so a teammate calling `check` twice doesn't see the same mail.
/// An empty (or all-read) inbox renders as `"No new messages."`.
fn format_inbox(messages: &[MailMessage]) -> String {
    let unread: Vec<&MailMessage> = messages.iter().filter(|m| !m.read).collect();
    if unread.is_empty() {
        return "No new messages.".to_string();
    }
    let mut out = format!("Inbox ({} unread):\n", unread.len());
    for m in unread {
        // A blank line separates the header and each block.
        out.push('\n');
        out.push_str(&format!("From: {}\n", m.from));
        out.push_str(&format!("Subject: {}\n", m.subject));
        out.push_str(&format!("Body: {}\n", m.body));
    }
    out
}

/// Parse the `action` field (trimmed, case-sensitive). Returns `None`
/// for anything other than `check`/`send` so [`MailboxTool::execute`]
/// can surface a clean error.
fn parse_action(s: &str) -> Option<MailAction> {
    match s.trim() {
        "check" => Some(MailAction::Check),
        "send" => Some(MailAction::Send),
        _ => None,
    }
}

// ---- tool output constructors ----

fn text_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: false,
    }
    .into()
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(from: &str, subject: &str, body: &str, read: bool) -> MailMessage {
        MailMessage {
            id: format!(
                "msg-{}{}{}{}",
                from.len(),
                subject.len(),
                body.len(),
                u8::from(read)
            ),
            from: from.to_string(),
            to: "me".to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            sent_at_ms: 0,
            read,
        }
    }

    // ---- message_file_name ----

    #[test]
    fn message_file_name_format_matches_spec() {
        let m = MailMessage {
            id: "msg-a1b2c3d4".to_string(),
            from: "alice".to_string(),
            to: "bob".to_string(),
            subject: "Parser refactored".to_string(),
            body: "body".to_string(),
            sent_at_ms: 1719062400000,
            read: false,
        };
        assert_eq!(
            message_file_name(&m),
            "1719062400000-alice-msg-a1b2c3d4.json"
        );
    }

    #[test]
    fn message_file_name_is_stable_across_read_flip() {
        // The filename only depends on sent_at_ms/from/id, none of
        // which change when `read` flips — so mark_read overwrites
        // the same path instead of orphaning the old file.
        let mut m = MailMessage {
            id: "msg-deadbeef".to_string(),
            from: "carol".to_string(),
            to: "me".to_string(),
            subject: "s".to_string(),
            body: "b".to_string(),
            sent_at_ms: 42,
            read: false,
        };
        let before = message_file_name(&m);
        m.read = true;
        let after = message_file_name(&m);
        assert_eq!(before, after);
    }

    // ---- format_inbox ----

    #[test]
    fn format_inbox_empty_says_no_new_messages() {
        assert_eq!(format_inbox(&[]), "No new messages.");
    }

    #[test]
    fn format_inbox_all_read_says_no_new_messages() {
        let messages = [
            msg("alice", "Already seen", "old", true),
            msg("bob", "Also read", "older", true),
        ];
        assert_eq!(format_inbox(&messages), "No new messages.");
    }

    #[test]
    fn format_inbox_skips_read_messages() {
        let messages = [
            msg("alice", "Already seen", "old", true),
            msg("bob", "Also read", "older", true),
        ];
        let out = format_inbox(&messages);
        // No "Inbox" header when there's nothing unread to show.
        assert!(!out.contains("Inbox"));
        assert!(!out.contains("From: alice"));
        assert!(!out.contains("From: bob"));
    }

    #[test]
    fn format_inbox_mixed_shows_only_unread() {
        let messages = [
            msg(
                "alice",
                "Parser refactored",
                "The parser is now using the new AST. Ready for you to wire the events.",
                false,
            ),
            msg("bob", "Already read", "stale", true),
            msg(
                "carol",
                "Task claimed",
                "I've claimed the benchmarking task.",
                false,
            ),
        ];
        let out = format_inbox(&messages);
        assert!(
            out.starts_with("Inbox (2 unread):\n"),
            "header wrong: {out:?}"
        );
        // First unread block (alice).
        assert!(out.contains("From: alice\n"));
        assert!(out.contains("Subject: Parser refactored\n"));
        assert!(
            out.contains(
                "Body: The parser is now using the new AST. Ready for you to wire the events.\n"
            ),
            "alice body missing: {out:?}"
        );
        // Second unread block (carol).
        assert!(out.contains("From: carol\n"));
        assert!(out.contains("Subject: Task claimed\n"));
        assert!(out.contains("Body: I've claimed the benchmarking task.\n"));
        // The read message must not appear.
        assert!(!out.contains("From: bob"), "read message leaked: {out:?}");
        assert!(!out.contains("Already read"));
    }

    #[test]
    fn format_inbox_multiple_unread_all_shown() {
        let messages = [
            msg("alice", "one", "body-one", false),
            msg("bob", "two", "body-two", false),
            msg("carol", "three", "body-three", false),
        ];
        let out = format_inbox(&messages);
        assert!(
            out.starts_with("Inbox (3 unread):\n"),
            "count wrong: {out:?}"
        );
        // All three blocks present, in slice order.
        assert!(out.contains("From: alice\n"));
        assert!(out.contains("From: bob\n"));
        assert!(out.contains("From: carol\n"));
        assert!(out.contains("Subject: one\n"));
        assert!(out.contains("Subject: two\n"));
        assert!(out.contains("Subject: three\n"));
        assert!(out.contains("Body: body-one\n"));
        assert!(out.contains("Body: body-two\n"));
        assert!(out.contains("Body: body-three\n"));
        // Blocks are separated by a blank line, so "From:" appears
        // exactly once per message.
        assert_eq!(out.matches("From: ").count(), 3);
    }

    // ---- parse_action ----

    #[test]
    fn parse_action_known_values() {
        assert_eq!(parse_action("check"), Some(MailAction::Check));
        assert_eq!(parse_action("send"), Some(MailAction::Send));
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_action("  check "), Some(MailAction::Check));
        assert_eq!(parse_action("\tsend\n"), Some(MailAction::Send));
    }

    #[test]
    fn parse_action_rejects_invalid() {
        assert_eq!(parse_action("delete"), None);
        assert_eq!(parse_action(""), None);
        assert_eq!(parse_action("CHECK"), None, "case-sensitive");
        assert_eq!(parse_action("read"), None);
    }

    // ---- short_uuid ----

    #[test]
    fn short_uuid_is_eight_hex_chars() {
        for _ in 0..32 {
            let s = short_uuid();
            assert_eq!(s.len(), 8, "wrong length: {s}");
            assert!(
                s.chars().all(|c| c.is_ascii_hexdigit()),
                "non-hex char in: {s}"
            );
        }
    }
}
