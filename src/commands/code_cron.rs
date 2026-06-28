//! The `cron_create` / `cron_list` / `cron_delete` tools (M5/#17).
//!
//! A session-scoped cron store: the model (or the user via `/schedule`)
//! schedules a prompt to fire at a wall-clock time described by a 5-field
//! cron expression. A background timer thread in the TUI (`run`) wakes
//! periodically, checks the store for due jobs, and injects each due
//! job's prompt as a turn via `Cmd::Prompt` — the same seam a manual
//! `/prompt` submit uses. Recurring jobs advance to their next fire;
//! one-shot jobs are removed after firing.
//!
//! ## Why a shared store, not a live session read
//!
//! As with `context_status` (M5/#16), a `pi::sdk::Tool::execute` runs
//! mid-turn and receives only its arguments — no `&AgentSession`, no
//! handle to the loop. The tools therefore read/write a shared
//! `Arc<CronStore>` (a `Mutex<Vec<CronJob>>` behind the `Arc`): the
//! factory builds each tool with a clone of the store, and the TUI's
//! timer thread reads + advances it on the main thread. There is no
//! cross-thread `&AgentSession` access — firing is `cmd_tx.send(Cmd::Prompt)`,
//! exactly the path a manual submit takes.
//!
//! ## Scope: session, not durable (yet)
//!
//! The overhaul plan ships the session-scoped store first and defers
//! the `.libertai/scheduled_tasks.json` durable backing as a follow-up.
//! So the store lives in memory for the TUI process's lifetime; a
//! scheduled job does not survive a restart. The `CronJob` type is
//! already `Serialize`/`Deserialize` so the durability follow-up is a
//! load/save pair, not a schema change.
//!
//! ## Cron expression
//!
//! Standard 5 fields — `minute hour day-of-month month day-of-week` —
//! supporting `*` (any), `*/N` (every N units), a comma-list (`1,15,30`),
//! and a single value. Ranges (`1-5`) and step-on-range (`1-10/2`) are
//! NOT supported (kept minimal); the parser returns a clear error so the
//! model can retry. Day-of-week uses 0-6 (0 = Sunday). Like real cron,
//! when both day-of-month and day-of-week are restricted (non-`*`), a
//! match on EITHER fires (cron's "OR" rule); when one is `*`, the other
//! dominates.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_mailbox::{now_epoch_ms, short_uuid};

/// Minimum, maximum, and step for a single cron field.
#[derive(Clone, Copy, Debug)]
struct FieldBounds {
    min: u8,
    max: u8,
}

const MINUTE: FieldBounds = FieldBounds { min: 0, max: 59 };
const HOUR: FieldBounds = FieldBounds { min: 0, max: 23 };
const DOM: FieldBounds = FieldBounds { min: 1, max: 31 };
const MONTH: FieldBounds = FieldBounds { min: 1, max: 12 };
const DOW: FieldBounds = FieldBounds { min: 0, max: 6 };

/// One parsed cron field: a set of allowed values. `*` is the full
/// range; `*/N` is every Nth value; a comma-list is the union. A
/// restricted field (not the full range) matters for cron's day-OR rule.
#[derive(Clone, Debug)]
struct CronField {
    values: Vec<u8>,
    /// Whether this field was written as `*` (the unrestricted case).
    /// Used by the day-of-month / day-of-week OR rule: when both are
    /// restricted, a match on either fires; when one is `*`, the other
    /// dominates.
    is_star: bool,
}

impl CronField {
    /// Parse one field against its bounds. Accepts `*`, `*/N`, a single
    /// value, and a comma-list of those. Returns a clear error on a
    /// range, a step-on-range, an out-of-bounds value, or junk.
    fn parse(raw: &str, bounds: FieldBounds) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("empty cron field".to_string());
        }
        if trimmed == "*" {
            return Ok(Self::star(bounds));
        }
        // `*/N` — every Nth value across the full range.
        if let Some(step_str) = trimmed.strip_prefix("*/") {
            let step: u8 = step_str
                .trim()
                .parse()
                .map_err(|_| format!("bad step in `{trimmed}`"))?;
            if step == 0 {
                return Err("step must be > 0".to_string());
            }
            let mut values = Vec::new();
            let mut v = bounds.min;
            while v <= bounds.max {
                values.push(v);
                v = v.saturating_add(step);
            }
            return Ok(Self {
                values,
                is_star: false,
            });
        }
        // Comma-list of single values (each may itself be `*`-style but
        // we keep it to single values here; no nested `*/N` in a list).
        if trimmed.contains(',') {
            let mut values = Vec::new();
            for part in trimmed.split(',') {
                let v: u8 = part
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad value `{part}` in `{trimmed}`"))?;
                if !(bounds.min..=bounds.max).contains(&v) {
                    return Err(format!(
                        "value {v} out of range {}-{}",
                        bounds.min, bounds.max
                    ));
                }
                values.push(v);
            }
            values.sort_unstable();
            values.dedup();
            return Ok(Self {
                values,
                is_star: false,
            });
        }
        // Single value.
        let v: u8 = trimmed
            .parse()
            .map_err(|_| format!("bad value `{trimmed}`"))?;
        if !(bounds.min..=bounds.max).contains(&v) {
            return Err(format!(
                "value {v} out of range {}-{}",
                bounds.min, bounds.max
            ));
        }
        Ok(Self {
            values: vec![v],
            is_star: false,
        })
    }

    fn star(bounds: FieldBounds) -> Self {
        Self {
            values: (bounds.min..=bounds.max).collect(),
            is_star: true,
        }
    }

    /// Whether `v` is allowed by this field.
    fn matches(&self, v: u8) -> bool {
        self.values.contains(&v)
    }
}

/// A parsed 5-field cron expression.
#[derive(Clone, Debug)]
pub(crate) struct CronExpr {
    minute: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
}

impl CronExpr {
    /// Parse `minute hour dom month dow`. Returns a clear, model-facing
    /// error string on any malformed input.
    pub(crate) fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "expected 5 fields (min hour dom month dow), got {}: `{expr}`",
                parts.len()
            ));
        }
        Ok(Self {
            minute: CronField::parse(parts[0], MINUTE)?,
            hour: CronField::parse(parts[1], HOUR)?,
            dom: CronField::parse(parts[2], DOM)?,
            month: CronField::parse(parts[3], MONTH)?,
            dow: CronField::parse(parts[4], DOW)?,
        })
    }

    /// Whether this expression fires at the given (minute, hour,
    /// day-of-month, month, day-of-week). Applies cron's day-OR rule:
    /// when both dom and dow are restricted, a match on either fires;
    /// when one is `*`, the other dominates.
    fn matches(&self, min: u8, hour: u8, dom: u8, month: u8, dow: u8) -> bool {
        if !self.minute.matches(min) {
            return false;
        }
        if !self.hour.matches(hour) {
            return false;
        }
        if !self.month.matches(month) {
            return false;
        }
        // Day-OR rule (Vixie cron): when both day fields are restricted,
        // either matching fires; when one is unrestricted (`*`), the
        // other must match.
        if self.dom.is_star {
            self.dow.matches(dow)
        } else if self.dow.is_star {
            self.dom.matches(dom)
        } else {
            self.dom.matches(dom) || self.dow.matches(dow)
        }
    }

    /// Next wall-clock fire at or after `from_ms` (epoch millis), or
    /// `None` if no fire exists within the search horizon (a fixed
    /// 366-day cap so a pathologically-restrictive expression can't
    /// spin forever — matches real-cron behavior where such jobs
    /// simply never fire).
    pub(crate) fn next_fire_ms(&self, from_ms: u64) -> Option<u64> {
        // Start at the next minute boundary at/after `from_ms`.
        let mut t = from_ms;
        // Align to the top of the minute; if `from_ms` is already on a
        // boundary we still start there (matches "at or after").
        let remainder = t % 60_000;
        if remainder != 0 {
            t += 60_000 - remainder;
        }
        let horizon = from_ms + 366 * 24 * 60 * 60 * 1000;
        while t <= horizon {
            let comps = decompose_ms(t)?;
            if self.matches(comps.min, comps.hour, comps.dom, comps.month, comps.dow) {
                return Some(t);
            }
            t += 60_000;
        }
        None
    }
}

/// Broken-down wall-clock components (UTC) from epoch millis.
struct TimeComps {
    min: u8,
    hour: u8,
    dom: u8,
    month: u8,
    dow: u8,
}

/// Decompose epoch millis into UTC cron components. Returns `None` only
/// on the (impossible-in-practice) overflow past the horizon.
fn decompose_ms(ms: u64) -> Option<TimeComps> {
    let secs = ms / 1000;
    let minute = ((secs / 60) % 60) as u8;
    let hour = ((secs / 3600) % 24) as u8;
    // Days since epoch → day-of-week (0 = Sunday; 1970-01-01 was a
    // Thursday = 4).
    let days = (secs / 86_400) as i64;
    let dow = ((days % 7 + 4) % 7).rem_euclid(7) as u8;
    // Day-of-month + month via civil-from-days. Cheap and
    // allocation-free.
    let (month, dom) = ymd_from_days(days);
    Some(TimeComps {
        min: minute,
        hour,
        dom,
        month,
        dow,
    })
}

/// Convert days-since-epoch into (month [1-12], day-of-month [1-31]).
/// The year is not needed (cron has no year field). Uses the
/// civil-from-days algorithm (Howard Hinnant) — no `time`/`chrono` dep.
fn ymd_from_days(days: i64) -> (u8, u8) {
    let z = days + 719_468; // days since 0000-03-01
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8; // [1, 31]
    let m = if mp < 10 { (mp + 3) as u8 } else { (mp - 9) as u8 }; // [1, 12]
    (m, d)
}

/// One scheduled job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CronJob {
    /// Stable, short id (so `cron_delete` can name it). `cron-<8hex>`.
    pub(crate) id: String,
    /// The raw 5-field cron expression the caller supplied. Kept raw
    /// so `cron_list` shows what was asked, not our internal parse.
    pub(crate) cron: String,
    /// The prompt to inject as a turn when the job fires.
    pub(crate) prompt: String,
    /// `true` → advance to the next fire after firing (recurring);
    /// `false` → remove after the first fire (one-shot).
    pub(crate) recurring: bool,
    /// Epoch-ms of the next scheduled fire. Updated on create + after
    /// each fire (recurring). The timer thread compares this to now.
    pub(crate) next_fire_ms: u64,
    /// Epoch-ms the job was created (for `cron_list` display ordering).
    pub(crate) created_ms: u64,
}

/// The session-scoped cron store, shared across the tools (bg thread)
/// and the TUI's timer thread (main thread) via an `Arc`.
#[derive(Debug, Default)]
pub struct CronStore {
    jobs: Mutex<Vec<CronJob>>,
}

impl CronStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a job. Validates the cron expression + computes the first
    /// `next_fire_ms` at/after now. Returns the stored job on success,
    /// or an error string (model-facing) on a bad expression / prompt.
    pub(crate) fn create(&self, cron: &str, prompt: &str, recurring: bool) -> Result<CronJob, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("`cron_create` requires a non-empty `prompt`".to_string());
        }
        let expr = CronExpr::parse(cron)?;
        let now = now_epoch_ms();
        let next = expr.next_fire_ms(now).ok_or_else(|| {
            format!("cron expression `{cron}` never fires within the next year")
        })?;
        let job = CronJob {
            id: format!("cron-{}", short_uuid()),
            cron: cron.trim().to_string(),
            prompt: prompt.to_string(),
            recurring,
            next_fire_ms: next,
            created_ms: now,
        };
        self.jobs.lock().expect("cron store poisoned").push(job.clone());
        Ok(job)
    }

    /// Snapshot of all jobs, sorted by next fire (soonest first) —
    /// the order `cron_list` displays and the timer checks.
    pub(crate) fn list(&self) -> Vec<CronJob> {
        let mut jobs = self.jobs.lock().expect("cron store poisoned").clone();
        jobs.sort_by_key(|j| j.next_fire_ms);
        jobs
    }

    /// Remove a job by id. Returns true if a job was removed.
    pub(crate) fn delete(&self, id: &str) -> bool {
        let id = id.trim();
        let mut jobs = self.jobs.lock().expect("cron store poisoned");
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        jobs.len() != before
    }

    /// Drain all jobs whose `next_fire_ms` is at or before `now`,
    /// returning `(id, prompt)` pairs to fire. Recurring jobs are
    /// advanced to their next fire (kept in the store); one-shot jobs
    /// are removed. Called by the TUI timer thread each tick.
    pub(crate) fn drain_due(&self, now: u64) -> Vec<(String, String)> {
        let mut jobs = self.jobs.lock().expect("cron store poisoned");
        let mut due = Vec::new();
        let mut keep = Vec::with_capacity(jobs.len());
        for mut job in jobs.drain(..) {
            if job.next_fire_ms <= now {
                due.push((job.id.clone(), job.prompt.clone()));
                if job.recurring {
                    if let Some(next) = CronExpr::parse(&job.cron)
                        .ok()
                        .and_then(|e| e.next_fire_ms(now + 60_000))
                    {
                        job.next_fire_ms = next;
                        keep.push(job);
                    }
                    // If a recurring job's next fire can't be computed
                    // (never fires again), drop it rather than spin.
                }
                // one-shot: not reinserted → removed.
            } else {
                keep.push(job);
            }
        }
        *jobs = keep;
        due
    }
}

// ---- tools ----

const CREATE_NAME: &str = "cron_create";
const CREATE_LABEL: &str = "Schedule prompt";
const CREATE_DESC: &str = concat!(
    "Schedule a prompt to fire at a wall-clock time given by a 5-field ",
    "cron expression (minute hour day-of-month month day-of-week). ",
    "Supports `*` (any), `*/N` (every N units), a single value, and a ",
    "comma-list (e.g. `*/5 * * * *` = every 5 minutes, ",
    "`0 9 * * 1-5` not supported — use `0 9 * * 1,2,3,4,5`). Day-of-week ",
    "is 0-6 (0=Sunday). When both day-of-month and day-of-week are ",
    "restricted, a match on EITHER fires (cron OR rule). The prompt is ",
    "injected as a turn at the next matching time. `recurring: true` ",
    "repeats; `false` fires once. Jobs live for the session (not ",
    "durable across restart). Use `cron_list` to see them and ",
    "`cron_delete` to cancel."
);

/// `cron_create` — schedule a prompt. Mutating (changes the schedule
/// the timer thread reads), so it goes through `ApprovalTool`.
#[derive(Clone)]
pub struct CronCreateTool {
    store: Arc<CronStore>,
}

impl CronCreateTool {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

impl Default for CronCreateTool {
    fn default() -> Self {
        Self::new(Arc::new(CronStore::new()))
    }
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        CREATE_NAME
    }
    fn label(&self) -> &str {
        CREATE_LABEL
    }
    fn description(&self) -> &str {
        CREATE_DESC
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cron": {
                    "type": "string",
                    "description": "5-field cron expression: minute hour day-of-month month day-of-week."
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt to inject as a turn when the job fires."
                },
                "recurring": {
                    "type": "boolean",
                    "default": true,
                    "description": "true = repeat at every match; false = fire once then remove."
                }
            },
            "required": ["cron", "prompt"]
        })
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        #[derive(Deserialize)]
        #[serde(default)]
        struct In {
            cron: String,
            prompt: String,
            recurring: bool,
        }
        impl Default for In {
            fn default() -> Self {
                Self {
                    cron: String::new(),
                    prompt: String::new(),
                    recurring: true,
                }
            }
        }
        let parsed: In = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err(&format!("invalid `cron_create` payload: {e}"))),
        };
        match self.store.create(&parsed.cron, &parsed.prompt, parsed.recurring) {
            Ok(job) => Ok(text(&format!(
                "Scheduled{}: {} — next fire at epoch-ms {}",
                if parsed.recurring { " (recurring)" } else { "" },
                job.id,
                job.next_fire_ms,
            ))),
            Err(e) => Ok(err(&e)),
        }
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

const LIST_NAME: &str = "cron_list";
const LIST_LABEL: &str = "List scheduled prompts";
const LIST_DESC: &str = concat!(
    "List all scheduled prompts for this session, soonest next-fire ",
    "first. Each entry shows the job id, cron expression, recurring ",
    "flag, next fire (epoch-ms), and the prompt. Read-only."
);

/// `cron_list` — read-only listing.
#[derive(Clone)]
pub struct CronListTool {
    store: Arc<CronStore>,
}

impl CronListTool {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

impl Default for CronListTool {
    fn default() -> Self {
        Self::new(Arc::new(CronStore::new()))
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        LIST_NAME
    }
    fn label(&self) -> &str {
        LIST_LABEL
    }
    fn description(&self) -> &str {
        LIST_DESC
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let jobs = self.store.list();
        if jobs.is_empty() {
            return Ok(text("No scheduled prompts for this session."));
        }
        let mut out = String::from("Scheduled prompts:\n");
        for j in jobs {
            out.push_str(&format!(
                "- {} `{}` {} next={} created={} : {}\n",
                j.id, j.cron, if j.recurring { "recurring" } else { "one-shot" }, j.next_fire_ms, j.created_ms, j.prompt,
            ));
        }
        Ok(text(out.trim_end()))
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

const DELETE_NAME: &str = "cron_delete";
const DELETE_LABEL: &str = "Cancel scheduled prompt";
const DELETE_DESC: &str = concat!(
    "Cancel a scheduled prompt by its job id (from `cron_list`). ",
    "Returns whether a job was removed."
);

/// `cron_delete` — cancel a job. Mutating.
#[derive(Clone)]
pub struct CronDeleteTool {
    store: Arc<CronStore>,
}

impl CronDeleteTool {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

impl Default for CronDeleteTool {
    fn default() -> Self {
        Self::new(Arc::new(CronStore::new()))
    }
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        DELETE_NAME
    }
    fn label(&self) -> &str {
        DELETE_LABEL
    }
    fn description(&self) -> &str {
        DELETE_DESC
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id from `cron_list`." }
            },
            "required": ["id"]
        })
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        #[derive(Deserialize)]
        struct In {
            id: String,
        }
        let parsed: In = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err(&format!("invalid `cron_delete` payload: {e}"))),
        };
        if self.store.delete(&parsed.id) {
            Ok(text(&format!("Cancelled {}", parsed.id.trim())))
        } else {
            Ok(err(&format!("no scheduled prompt with id `{}`", parsed.id.trim())))
        }
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

// ---- output helpers ----

fn text(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: false,
    }
    .into()
}

fn err(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

/// Wall-clock "now" in epoch millis, exposed for the timer thread (and
/// tests). Re-exports the mailbox helper so callers don't reach across
/// modules for the clock.
pub(crate) fn now_ms() -> u64 {
    now_epoch_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::test_utils::run_test;

    // ---- CronExpr parsing ----

    #[test]
    fn parses_star_field() {
        let e = CronExpr::parse("* * * * *").unwrap();
        assert!(e.minute.is_star);
        assert!(e.dow.is_star);
    }

    #[test]
    fn rejects_wrong_arity() {
        assert!(CronExpr::parse("* * * *").is_err());
        assert!(CronExpr::parse("* * * * * *").is_err());
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(CronExpr::parse("60 * * * *").is_err());
        assert!(CronExpr::parse("* 24 * * *").is_err());
        assert!(CronExpr::parse("* * 32 * *").is_err());
        assert!(CronExpr::parse("* * * * 7").is_err());
        assert!(CronExpr::parse("* * 0 * *").is_err()); // dom min is 1
    }

    #[test]
    fn rejects_step_on_range_and_ranges() {
        // ranges + step-on-range deliberately unsupported.
        assert!(CronExpr::parse("1-5 * * * *").is_err());
        assert!(CronExpr::parse("1-10/2 * * * *").is_err());
        assert!(CronExpr::parse("*/0 * * * *").is_err());
    }

    #[test]
    fn parses_comma_list() {
        let e = CronExpr::parse("1,15,30 * * * *").unwrap();
        assert_eq!(e.minute.values, vec![1, 15, 30]);
        assert!(!e.minute.is_star);
    }

    #[test]
    fn parses_step() {
        let e = CronExpr::parse("*/15 * * * *").unwrap();
        assert_eq!(e.minute.values, vec![0, 15, 30, 45]);
    }

    // ---- next_fire_ms ----

    #[test]
    fn next_fire_every_minute() {
        // `* * * * *` fires at the next minute boundary.
        let e = CronExpr::parse("* * * * *").unwrap();
        // 1970-01-01 00:00:00 UTC = 0; next at-or-after 0 is 0.
        assert_eq!(e.next_fire_ms(0), Some(0));
        // 30s in → next boundary is 60_000.
        assert_eq!(e.next_fire_ms(30_000), Some(60_000));
    }

    #[test]
    fn next_fire_specific_minute() {
        // `5 * * * *` — at minute 5 of each hour.
        let e = CronExpr::parse("5 * * * *").unwrap();
        // 00:00 → 00:05 = 5*60_000.
        assert_eq!(e.next_fire_ms(0), Some(5 * 60_000));
    }

    #[test]
    fn next_fire_never_in_horizon() {
        // `30 2 31 2 *` — Feb 31 never exists; no fire within a year.
        let e = CronExpr::parse("30 2 31 2 *").unwrap();
        assert_eq!(e.next_fire_ms(0), None);
    }

    #[test]
    fn next_fire_comma_minutes() {
        // `0,30 * * * *` — at 0 and 30 past.
        let e = CronExpr::parse("0,30 * * * *").unwrap();
        // from 00:00:00, next is 00:00 (0), then 00:30.
        assert_eq!(e.next_fire_ms(0), Some(0));
        assert_eq!(e.next_fire_ms(60_000), Some(30 * 60_000));
    }

    // ---- day-of-week + day-OR rule ----

    #[test]
    fn next_fire_dow_sunday() {
        // `0 0 * * 0` — midnight on Sundays (dow=0). 1970-01-01 was
        // Thursday (dow=4); the first Sunday is 1970-01-04.
        let e = CronExpr::parse("0 0 * * 0").unwrap();
        let fire = e.next_fire_ms(0).unwrap();
        // 1970-01-04 00:00 UTC = 3 days * 86400.
        assert_eq!(fire, 3 * 86_400 * 1000);
    }

    #[test]
    fn day_or_rule_both_restricted_fires_on_either() {
        // `0 0 1 * 1` — midnight on the 1st OR a Monday. The 1st of Jan
        // 1970 was Thursday; the first Monday is Jan 5. So the first
        // fire should be Jan 1 (dom match), not Jan 5.
        let e = CronExpr::parse("0 0 1 * 1").unwrap();
        let fire = e.next_fire_ms(0).unwrap();
        // Jan 1 00:00 UTC = 0 (the 1st matches dom).
        assert_eq!(fire, 0);
    }

    // ---- decompose round-trip ----

    #[test]
    fn decompose_epoch_zero_is_thursday() {
        let c = decompose_ms(0).unwrap();
        // 1970-01-01 00:00 UTC, Thursday → dow=4.
        assert_eq!(c.dow, 4);
        assert_eq!(c.dom, 1);
        assert_eq!(c.month, 1);
        assert_eq!(c.hour, 0);
        assert_eq!(c.min, 0);
    }

    #[test]
    fn decompose_known_date() {
        // 2020-03-01 00:00 UTC. Days since epoch:
        // 50 * 365 + leap days... compute via the function instead.
        // Use 1 day after a known Sunday: 2021-01-03 was a Sunday.
        // 2021-01-03 00:00 UTC epoch: 1_609_632_000_000 ms.
        let ms = 1_609_632_000_000u64;
        let c = decompose_ms(ms).unwrap();
        assert_eq!(c.dow, 0); // Sunday
        assert_eq!(c.month, 1);
        assert_eq!(c.dom, 3);
    }

    // ---- CronStore ----

    #[test]
    fn store_create_lists_and_deletes() {
        run_test(|| async {
            let s = Arc::new(CronStore::new());
            let job = s.create("*/5 * * * *", "check CI", true).unwrap();
            assert!(job.id.starts_with("cron-"));
            let listed = s.list();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, job.id);
            assert_eq!(listed[0].prompt, "check CI");
            assert!(listed[0].recurring);
            assert!(s.delete(&job.id));
            assert!(s.list().is_empty());
            assert!(!s.delete(&job.id)); // already gone
        });
    }

    #[test]
    fn store_rejects_bad_expr_and_empty_prompt() {
        run_test(|| async {
            let s = Arc::new(CronStore::new());
            assert!(s.create("not a cron", "x", true).is_err());
            assert!(s.create("* * * * *", "  ", true).is_err());
            // never-fires expr:
            assert!(s.create("0 0 31 2 *", "x", true).is_err());
        });
    }

    #[test]
    fn store_drain_due_fires_one_shot_and_advances_recurring() {
        run_test(|| async {
            let s = Arc::new(CronStore::new());
            // one-shot in the past (next_fire clamped to "now or
            // next minute" by create; force it into the past by
            // creating with a fire that already passed: use a fixed
            // backdated next_fire via direct insert through create
            // then rewrite). Simpler: create a job, then call
            // drain_due with a `now` far in its future.
            let one = s.create("* * * * *", "one-shot", false).unwrap();
            let rec = s.create("* * * * *", "recurring", true).unwrap();
            // Both fire every minute; their next_fire is ~now. Drain
            // with `now` well past them.
            let now = rec.next_fire_ms + 10 * 60_000;
            let due = s.drain_due(now);
            // Both due (next_fire <= now).
            assert_eq!(due.len(), 2);
            let prompts: Vec<_> = due.iter().map(|(_, p)| p.as_str()).collect();
            assert!(prompts.contains(&"one-shot"));
            assert!(prompts.contains(&"recurring"));
            // One-shot removed; recurring kept + advanced past `now`.
            let kept = s.list();
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].id, rec.id);
            assert!(kept[0].next_fire_ms > now);
            let _ = one;
        });
    }

    #[test]
    fn store_drain_due_empty_when_none_due() {
        run_test(|| async {
            let s = Arc::new(CronStore::new());
            let j = s.create("* * * * *", "x", true).unwrap();
            // `now` before its next_fire → nothing due.
            let due = s.drain_due(j.next_fire_ms - 1);
            assert!(due.is_empty());
            assert_eq!(s.list().len(), 1);
        });
    }

    // ---- tool-level ----

    fn exec<T: Tool + Send + 'static>(tool: T, input: serde_json::Value) -> (bool, String) {
        let out: Arc<Mutex<Option<ToolExecution>>> = Arc::new(Mutex::new(None));
        let out_cloned = Arc::clone(&out);
        run_test(move || {
            let out_cloned = Arc::clone(&out_cloned);
            async move {
                let r = tool.execute("c1", input, None).await.unwrap();
                *out_cloned.lock().unwrap() = Some(r);
            }
        });
        let exec_result = out.lock().unwrap().take().unwrap();
        match exec_result {
            ToolExecution::Done(o) => {
                let txt = match o.content.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                (o.is_error, txt)
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn cron_create_tool_schedules_and_lists_and_deletes() {
        let store = Arc::new(CronStore::new());
        let create = || CronCreateTool::new(Arc::clone(&store));
        let list = || CronListTool::new(Arc::clone(&store));
        let delete = || CronDeleteTool::new(Arc::clone(&store));

        let (err, txt) = exec(create(), serde_json::json!({
            "cron": "*/5 * * * *", "prompt": "check CI"
        }));
        assert!(!err, "{txt}");
        assert!(txt.contains("Scheduled"), "{txt}");
        // Grab the id from the list.
        let (err, txt) = exec(list(), serde_json::json!({}));
        assert!(!err);
        assert!(txt.contains("check CI"), "{txt}");
        // The id is `cron-<8hex>`; pull it out of the listing (the
        // second whitespace token, after the leading `-`).
        let id = txt
            .lines()
            .find_map(|l| {
                l.split_whitespace()
                    .find(|s| s.starts_with("cron-"))
                    .map(|s| s.to_string())
            })
            .unwrap();

        let (err, txt) = exec(delete(), serde_json::json!({ "id": id }));
        assert!(!err, "{txt}");
        assert!(txt.contains("Cancelled"), "{txt}");

        let (err, txt) = exec(list(), serde_json::json!({}));
        assert!(!err);
        assert!(txt.contains("No scheduled"), "{txt}");
    }

    #[test]
    fn cron_create_tool_rejects_bad_expr() {
        let store = Arc::new(CronStore::new());
        let create = CronCreateTool::new(store);
        let (err, _txt) = exec(create, serde_json::json!({
            "cron": "garbage", "prompt": "x"
        }));
        assert!(err);
    }

    #[test]
    fn cron_delete_tool_missing_id_is_error() {
        let store = Arc::new(CronStore::new());
        let delete = CronDeleteTool::new(store);
        let (err, txt) = exec(delete, serde_json::json!({ "id": "cron-deadbeef" }));
        assert!(err);
        assert!(txt.contains("no scheduled"), "{txt}");
    }

    #[test]
    fn cron_list_tool_empty_message() {
        let store = Arc::new(CronStore::new());
        let list = CronListTool::new(store);
        let (err, txt) = exec(list, serde_json::json!({}));
        assert!(!err);
        assert!(txt.contains("No scheduled"), "{txt}");
    }
}
