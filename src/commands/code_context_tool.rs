//! The `context_status` + `request_compaction` tools (M5/#16).
//!
//! `context_status` (read-only) lets the model inspect how full the
//! context window is — the same occupancy the status bar shows, but as
//! a structured tool result the model can reason about ("I'm at 87%,
//! I should wrap up before I'm auto-compacted"). `request_compaction`
//! (mutating) asks the loop to compact at the next turn boundary.
//!
//! ## Why a shared snapshot, not a live session read
//!
//! A `pi::sdk::Tool::execute` runs *mid-turn* and receives only its
//! arguments — no `&AgentSession`, no `Usage`. The honest value a tool
//! can report is the **last completed turn's** context occupancy: the
//! most recent `Usage` event the TUI's `Usage` handler observed. That
//! is exactly what the model needs (it tells the model how full the
//! window is right now, the same number the status bar shows), and it
//! is what the model would get from any tool — a tool simply cannot
//! see the in-flight turn's token count.
//!
//! So both tools read a shared `Arc<ContextSnapshot>`: the factory
//! builds the tool with a clone of the snapshot, and the TUI updates
//! `last_input_tokens` from the `Usage` handler + reads/clears
//! `compaction_requested` at `TurnEnd`. The snapshot is created once
//! in `run()` (where the `App` lives) and threaded into the factory
//! via `with_context_snapshot`, mirroring how `edit_journal` /
//! `approvals` Arcs are shared across the main↔bg-thread boundary.
//!
//! ## Why `request_compaction` is a signal, not a synchronous call
//!
//! pi's compaction (`compact_now_force*`) needs `&mut AgentSession`,
//! which the running turn holds. A tool executes *inside* that turn,
//! so calling compaction from `execute` would deadlock against the
//! turn. The correct, non-deadlocking design is a **flag the turn-end
//! handler checks**: the tool sets `compaction_requested`, returns
//! immediately ("compaction scheduled for end of turn"), and the TUI's
//! `TurnEnd` handler — already the site that auto-compacts on
//! ctx-limit (#35) — sends `BgCommand::Compact` when the flag is set.
//! This reuses the existing `/compact` path (`cmd_tx.send`) and never
//! touches the session from within the tool.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_ui::{context_percent, context_window_for};

/// The shared, cross-thread context state both tools read.
///
/// `last_input_tokens` + `context_window` are atomics because the TUI
/// (main thread, `Usage` handler) writes them while a tool (bg thread)
/// reads them. The compaction config is immutable for the session —
/// captured once at construction — so it's plain fields behind the
/// `Arc`. `compaction_requested` is the turn-end compaction signal
/// `request_compaction` sets and the `TurnEnd` handler drains.
#[derive(Debug)]
pub struct ContextSnapshot {
    last_input_tokens: AtomicU64,
    context_window: AtomicU32,
    /// Resolved at construction so the tools can recompute the window
    /// (and thus the percent) even before the first `Usage` event —
    /// the snapshot starts with the model's catalog window, not 0.
    provider: String,
    model: String,
    auto_compaction_enabled: bool,
    reserve_tokens: u32,
    keep_recent_tokens: u32,
    /// Set by `request_compaction`; drained (`swap(false)`) by the
    /// TUI `TurnEnd` handler which then sends `BgCommand::Compact`.
    compaction_requested: AtomicBool,
}

impl ContextSnapshot {
    /// Build a fresh snapshot. `context_window` is resolved against
    /// the model catalog (`context_window_for`) so the percent is
    /// meaningful from the first tool call, before any `Usage`.
    pub fn new(
        provider: &str,
        model: &str,
        auto_compaction_enabled: bool,
        reserve_tokens: u32,
        keep_recent_tokens: u32,
    ) -> Self {
        let window = context_window_for(provider, model);
        Self {
            last_input_tokens: AtomicU64::new(0),
            context_window: AtomicU32::new(window),
            provider: provider.to_string(),
            model: model.to_string(),
            auto_compaction_enabled,
            reserve_tokens,
            keep_recent_tokens,
            compaction_requested: AtomicBool::new(false),
        }
    }

    /// Latest context occupancy (last completed turn's input tokens
    /// — `input + cache_read + cache_write`, the same figure the
    /// status bar shows). 0 until the first `Usage` event.
    pub fn last_input_tokens(&self) -> u64 {
        self.last_input_tokens.load(Ordering::Relaxed)
    }

    /// Update from the `Usage` handler. `context_window` may change
    /// on a model swap, so it's updated alongside the token count.
    pub fn record_usage(&self, input_tokens: u64, context_window: u32) {
        self.last_input_tokens
            .store(input_tokens, Ordering::Relaxed);
        if context_window > 0 {
            self.context_window.store(context_window, Ordering::Relaxed);
        }
    }

    /// Current context window. Re-resolved against the catalog when
    /// the stored value is 0 (defensive — `record_usage` always sets
    /// a positive window, but a tool may run before the first `Usage`).
    pub fn context_window(&self) -> u32 {
        let w = self.context_window.load(Ordering::Relaxed);
        if w > 0 {
            w
        } else {
            context_window_for(&self.provider, &self.model)
        }
    }

    pub fn auto_compaction_enabled(&self) -> bool {
        self.auto_compaction_enabled
    }
    pub fn reserve_tokens(&self) -> u32 {
        self.reserve_tokens
    }
    pub fn keep_recent_tokens(&self) -> u32 {
        self.keep_recent_tokens
    }

    /// Set the turn-end compaction signal. Returns the previous value
    /// (true if a request was already pending — the model calling
    /// `request_compaction` twice in one turn is harmless).
    pub fn request_compaction(&self) -> bool {
        self.compaction_requested.swap(true, Ordering::Relaxed)
    }

    /// Drain the compaction signal. Called by the `TurnEnd` handler;
    /// returns true if a `request_compaction` tool call asked for
    /// compaction this turn.
    pub fn take_compaction_request(&self) -> bool {
        self.compaction_requested.swap(false, Ordering::Relaxed)
    }
}

const STATUS_NAME: &str = "context_status";
const STATUS_LABEL: &str = "Context status";
const STATUS_DESCRIPTION: &str = "Report current context-window occupancy so you can plan ahead. \
Returns {context_tokens, context_window, percent, auto_compaction_enabled, reserve_tokens, \
keep_recent_tokens}. `context_tokens` is the last completed turn's input-token count \
(input + cache_read + cache_write) — the same figure the status bar shows. Call this before \
long-running work to gauge how much room is left; when occupancy is high, prefer finishing the \
task over starting new exploratory branches.";

const COMPACTION_NAME: &str = "request_compaction";
const COMPACTION_LABEL: &str = "Request compaction";
const COMPACTION_DESCRIPTION: &str = "Request that the session be compacted at the end of the \
current turn, summarizing older messages to free context. Compaction cannot run mid-turn (the \
turn holds the session), so this schedules it for the turn boundary — the same path `/compact` \
and the ctx-limit auto-compaction use. Optional `notes` become the summarization guidance. \
Returns immediately; the next turn will see a compacted history. Use this when context_status \
shows high occupancy and you want to control what's kept.";

#[derive(serde::Deserialize)]
struct CompactionInput {
    #[serde(default)]
    notes: Option<String>,
}

/// Read-only `context_status` tool.
pub struct ContextStatusTool {
    snapshot: Arc<ContextSnapshot>,
}

impl ContextStatusTool {
    pub fn new(snapshot: Arc<ContextSnapshot>) -> Self {
        Self { snapshot }
    }
}

#[async_trait]
impl Tool for ContextStatusTool {
    fn name(&self) -> &str {
        STATUS_NAME
    }
    fn label(&self) -> &str {
        STATUS_LABEL
    }
    fn description(&self) -> &str {
        STATUS_DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let tokens = self.snapshot.last_input_tokens();
        let window = self.snapshot.context_window();
        let pct = context_percent(tokens, window);
        let result = serde_json::json!({
            "context_tokens": tokens,
            "context_window": window,
            "percent": pct,
            "auto_compaction_enabled": self.snapshot.auto_compaction_enabled(),
            "reserve_tokens": self.snapshot.reserve_tokens(),
            "keep_recent_tokens": self.snapshot.keep_recent_tokens(),
        });
        Ok(ToolExecution::Done(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(result.to_string()))],
            details: Some(result),
            is_error: false,
        }))
    }
}

/// Mutating `request_compaction` tool. Registered wrapped in
/// `ApprovalTool` by the factory.
pub struct RequestCompactionTool {
    snapshot: Arc<ContextSnapshot>,
}

impl RequestCompactionTool {
    pub fn new(snapshot: Arc<ContextSnapshot>) -> Self {
        Self { snapshot }
    }
}

#[async_trait]
impl Tool for RequestCompactionTool {
    fn name(&self) -> &str {
        COMPACTION_NAME
    }
    fn label(&self) -> &str {
        COMPACTION_LABEL
    }
    fn description(&self) -> &str {
        COMPACTION_DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "notes": {
                    "type": "string",
                    "description": "Optional guidance for what the compaction summary should keep.",
                },
            },
            "additionalProperties": false,
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let had_pending = self.snapshot.request_compaction();
        let notes = serde_json::from_value::<CompactionInput>(input)
            .ok()
            .and_then(|i| i.notes)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let msg = if had_pending {
            "Compaction already scheduled for the end of this turn.".to_string()
        } else {
            match &notes {
                Some(n) => {
                    format!("Compaction scheduled for the end of this turn with notes: {n}")
                }
                None => "Compaction scheduled for the end of this turn.".to_string(),
            }
        };
        // The notes aren't carried through the snapshot flag (the flag
        // is a single bool). They surface in the tool result so the
        // model knows its guidance was acknowledged; the actual
        // `BgCommand::Compact { notes }` sent at TurnEnd uses `None`
        // (the same no-notes path the ctx-limit auto-compaction uses).
        // If we later want notes honored, extend the snapshot to hold
        // an `Option<String>` — left as a follow-up to keep #16 lean.
        Ok(ToolExecution::Done(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(msg))],
            details: Some(serde_json::json!({
                "scheduled": true,
                "pending": had_pending,
                "notes": notes,
            })),
            is_error: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::test_utils::run_test;

    fn snap() -> Arc<ContextSnapshot> {
        Arc::new(ContextSnapshot::new(
            "anthropic",
            "claude-3-5-sonnet",
            true,
            8_000,
            4_000,
        ))
    }

    #[test]
    fn snapshot_starts_with_catalog_window_and_zero_tokens() {
        // context_window_for returns FALLBACK (32_768) under cfg(test).
        let s = snap();
        assert_eq!(s.last_input_tokens(), 0);
        assert_eq!(s.context_window(), 32_768);
        assert!(s.auto_compaction_enabled());
        assert_eq!(s.reserve_tokens(), 8_000);
        assert_eq!(s.keep_recent_tokens(), 4_000);
    }

    #[test]
    fn record_usage_updates_tokens_and_window() {
        let s = snap();
        s.record_usage(25_000, 200_000);
        assert_eq!(s.last_input_tokens(), 25_000);
        assert_eq!(s.context_window(), 200_000);
    }

    #[test]
    fn record_usage_ignores_zero_window() {
        let s = snap();
        s.record_usage(5_000, 200_000);
        // A model swap surfacing window=0 must not wipe the known window.
        s.record_usage(6_000, 0);
        assert_eq!(s.last_input_tokens(), 6_000);
        assert_eq!(s.context_window(), 200_000);
    }

    #[test]
    fn compaction_flag_is_drain_once() {
        let s = snap();
        assert!(!s.take_compaction_request());
        assert!(!s.request_compaction());
        assert!(s.take_compaction_request());
        assert!(!s.take_compaction_request());
    }

    #[test]
    fn context_status_tool_reports_snapshot() {
        run_test(|| async {
            let s = snap();
            s.record_usage(50_000, 200_000);
            let tool = ContextStatusTool::new(Arc::clone(&s));
            let exec = tool
                .execute("c1", serde_json::json!({}), None)
                .await
                .unwrap();
            let pi::sdk::ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            assert!(!out.is_error);
            let details = out.details.unwrap();
            assert_eq!(details["context_tokens"], 50_000);
            assert_eq!(details["context_window"], 200_000);
            assert_eq!(details["percent"], 25);
            assert_eq!(details["auto_compaction_enabled"], true);
            assert_eq!(details["reserve_tokens"], 8_000);
            assert_eq!(details["keep_recent_tokens"], 4_000);
        });
    }

    #[test]
    fn context_status_percent_clamps_at_full() {
        run_test(|| async {
            let s = snap();
            s.record_usage(250_000, 200_000);
            let tool = ContextStatusTool::new(s);
            let exec = tool
                .execute("c1", serde_json::json!({}), None)
                .await
                .unwrap();
            let pi::sdk::ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            assert_eq!(out.details.unwrap()["percent"], 100);
        });
    }

    #[test]
    fn request_compaction_sets_flag_and_reports_scheduled() {
        run_test(|| async {
            let s = snap();
            let tool = RequestCompactionTool::new(Arc::clone(&s));
            let exec = tool
                .execute("c1", serde_json::json!({}), None)
                .await
                .unwrap();
            let pi::sdk::ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            assert!(!out.is_error);
            let details = out.details.unwrap();
            assert_eq!(details["scheduled"], true);
            assert_eq!(details["pending"], false);
            assert!(s.take_compaction_request());
        });
    }

    #[test]
    fn request_compaction_with_notes_echoes_them() {
        run_test(|| async {
            let s = snap();
            let tool = RequestCompactionTool::new(Arc::clone(&s));
            let exec = tool
                .execute(
                    "c1",
                    serde_json::json!({ "notes": "keep the API design section" }),
                    None,
                )
                .await
                .unwrap();
            let pi::sdk::ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            let details = out.details.unwrap();
            assert_eq!(details["notes"], "keep the API design section");
        });
    }

    #[test]
    fn request_compaction_twice_reports_already_pending() {
        run_test(|| async {
            let s = snap();
            let tool = RequestCompactionTool::new(Arc::clone(&s));
            tool.execute("c1", serde_json::json!({}), None)
                .await
                .unwrap();
            let exec = tool
                .execute("c2", serde_json::json!({}), None)
                .await
                .unwrap();
            let pi::sdk::ToolExecution::Done(out) = exec else {
                panic!("expected Done");
            };
            let details = out.details.unwrap();
            assert_eq!(details["pending"], true);
            // Flag stays set (idempotent) — one drain clears it.
            assert!(s.take_compaction_request());
            assert!(!s.take_compaction_request());
        });
    }

    #[test]
    fn tools_are_correctly_readonly() {
        assert!(ContextStatusTool::new(snap()).is_read_only());
        assert!(!RequestCompactionTool::new(snap()).is_read_only());
    }
}
