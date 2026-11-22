#![allow(dead_code, clippy::too_many_arguments)]
//! Rollover plumbing for omp's experimental context management: the turn-local
//! `new_context` request consumer, the rollover-only compaction entry, and the
//! notebook reminder.
//!
//! Ported from coding-agent:
//! - `session/agent-session.ts#takeExperimentalContextRolloverRequest` — scan a
//!   turn's tool results for `details.requested === true` (also through a
//!   write-device `execute` dispatch), see [`take_experimental_context_rollover_request`].
//! - `session/session-maintenance.ts#runExperimentalContextRollover` — commit a
//!   `compaction` entry whose `summary` is the rollover prompt and whose details
//!   are `{kind: "experimental-context-rollover", version: 1}`, bypassing the
//!   method-order pipeline entirely (no summarizer call), see
//!   [`rollover_compaction`].
//! - `session/session-maintenance.ts#maybeQueueExperimentalNotesReminder` — the
//!   single-shot-per-window notebook nudge, see [`notes_reminder_due`] and
//!   [`notes_reminder_prompt`].
//!
//! Nothing here mutates session state: the runtime owns the flag, the journal
//! append and the reminder queue, and calls these builders for the exact shapes.

use serde_json::{json, Value};

// ─── Entry contract ──────────────────────────────────────────────────────

/// `details.kind` of a rollover compaction entry.
pub const ROLLOVER_KIND: &str = "experimental-context-rollover";
/// `details.version` of a rollover compaction entry.
pub const ROLLOVER_VERSION: u64 = 1;

/// The tools whose presence makes experimental rollover safe
/// (`EXPERIMENTAL_CONTEXT_REQUIRED_TOOLS`): the notebook and the signal.
pub const EXPERIMENTAL_CONTEXT_REQUIRED_TOOLS: [&str; 2] = ["context_notes", "new_context"];

/// The summary a rollover compaction commits
/// (`prompts/system/experimental-context-rollover.md`, rendered).
pub const ROLLOVER_SUMMARY_TEMPLATE: &str =
    include_str!("../../prompts/system/experimental-context-rollover.md");

/// The notebook nudge queued before a rollover
/// (`prompts/system/experimental-context-notes-reminder.md`, rendered).
pub const NOTES_REMINDER_TEMPLATE: &str =
    include_str!("../../prompts/system/experimental-context-notes-reminder.md");

/// omp renders prompts through `prompt.render`, which trims trailing
/// whitespace. Neither template has blank-line runs, so trimming the trailing
/// newline is the whole difference.
fn rendered(template: &str) -> String {
    template.trim_end().to_owned()
}

/// The exact `summary` text a rollover compaction stores.
#[must_use]
pub fn rollover_summary() -> String {
    rendered(ROLLOVER_SUMMARY_TEMPLATE)
}

/// The exact reminder prompt the runtime queues at the next prompt admission.
#[must_use]
pub fn notes_reminder_prompt() -> String {
    rendered(NOTES_REMINDER_TEMPLATE)
}

// ─── new_context request consumption ─────────────────────────────────────

/// How a write-device dispatch reached its tool (`XdevDispatch.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// The device ran the inner tool; its result is in `inner`.
    Execute,
    /// The device only proposed the call.
    Propose,
}

/// The inner result of a write-device dispatch (`XdevDispatch`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WriteDeviceDispatch<'a> {
    /// The tool the device dispatched to.
    pub tool: &'a str,
    /// How the device reached it.
    pub mode: DispatchMode,
    /// The inner tool result's `details`.
    pub inner_details: Option<&'a Value>,
}

/// One settled tool result of the turn, as the request scan sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RolloverToolResult<'a> {
    /// The tool name reported on the result.
    pub tool_name: &'a str,
    /// `toolResult.isError`; errored results never carry a live request.
    pub is_error: bool,
    /// The result's `details`.
    pub details: Option<&'a Value>,
    /// Set when a write device dispatched this call (`writeDeviceDispatch`).
    pub dispatch: Option<WriteDeviceDispatch<'a>>,
}

impl<'a> RolloverToolResult<'a> {
    /// A plain (non-device) result.
    #[must_use]
    pub fn new(tool_name: &'a str, is_error: bool, details: Option<&'a Value>) -> Self {
        Self {
            tool_name,
            is_error,
            details,
            dispatch: None,
        }
    }

    /// A result that reached its tool through a write device.
    #[must_use]
    pub fn dispatched(
        tool_name: &'a str,
        is_error: bool,
        details: Option<&'a Value>,
        dispatch: WriteDeviceDispatch<'a>,
    ) -> Self {
        Self {
            tool_name,
            is_error,
            details,
            dispatch: Some(dispatch),
        }
    }

    /// The details that may carry `requested`: the result's own for
    /// `new_context`, else the inner result's when a device *executed*
    /// `new_context` (TS picks `dispatch.inner` for that case only).
    fn request_details(&self) -> Option<&'a Value> {
        if self.tool_name == "new_context" {
            return self.details;
        }
        match self.dispatch {
            Some(WriteDeviceDispatch {
                tool: "new_context",
                mode: DispatchMode::Execute,
                inner_details,
            }) => inner_details,
            _ => None,
        }
    }
}

/// `details.requested === true` on a JSON record (strict boolean, not truthy).
#[must_use]
pub fn is_rollover_request(details: Option<&Value>) -> bool {
    details
        .and_then(Value::as_object)
        .and_then(|d| d.get("requested"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// `takeExperimentalContextRolloverRequest`: true when any non-errored result of
/// the turn is a `new_context` request, directly or through a device dispatch.
#[must_use]
pub fn take_experimental_context_rollover_request(results: &[RolloverToolResult<'_>]) -> bool {
    results
        .iter()
        .filter(|r| !r.is_error)
        .any(|r| is_rollover_request(r.request_details()))
}

/// `hasExperimentalContextRolloverTools`: every required tool is both enabled
/// and actually registered. A request is dropped, never downgraded to summary
/// compaction, when this goes false between the tool result and dispatch.
#[must_use]
pub fn has_experimental_context_rollover_tools(enabled: &[&str], registered: &[&str]) -> bool {
    EXPERIMENTAL_CONTEXT_REQUIRED_TOOLS
        .iter()
        .all(|name| enabled.contains(name) && registered.contains(name))
}

/// The request scan the runtime runs, gated exactly like TS: an explicit request
/// survives only while experimental context management is on and its tools are
/// still there.
#[must_use]
pub fn accepted_rollover_request(
    experimental_enabled: bool,
    enabled_tools: &[&str],
    registered_tools: &[&str],
    results: &[RolloverToolResult<'_>],
) -> bool {
    experimental_enabled
        && has_experimental_context_rollover_tools(enabled_tools, registered_tools)
        && take_experimental_context_rollover_request(results)
}

// ─── Rollover-only compaction entry ──────────────────────────────────────

/// The `compaction` entry a rollover commits. `short_summary` is always absent
/// and no summarizer ran: the summary is the static rollover prompt, and the
/// notebook (already re-injected by `omp_compact::context`) carries the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloverCompaction {
    /// Committed summary text (see [`rollover_summary`]).
    pub summary: String,
    /// First entry the model keeps verbatim (`prepareCompaction`'s choice).
    pub first_kept_entry_id: Option<String>,
    /// Token count before the rollover.
    pub tokens_before: i64,
}

impl RolloverCompaction {
    /// A rollover over `first_kept_entry_id`, keeping `tokens_before` tokens of history.
    #[must_use]
    pub fn new(first_kept_entry_id: Option<String>, tokens_before: i64) -> Self {
        Self {
            summary: rollover_summary(),
            first_kept_entry_id,
            tokens_before,
        }
    }

    /// `details: {kind: "experimental-context-rollover", version: 1}`.
    #[must_use]
    pub fn details(&self) -> Value {
        json!({"kind": ROLLOVER_KIND, "version": ROLLOVER_VERSION})
    }

    /// The journal payload for the `compaction` entry. `firstKeptEntryId` is
    /// omitted when absent, like an optional TS field.
    #[must_use]
    pub fn to_entry_payload(&self) -> Value {
        let mut payload = json!({
            "summary": self.summary,
            "tokensBefore": self.tokens_before,
            "details": self.details(),
        });
        if let Some(id) = &self.first_kept_entry_id {
            payload["firstKeptEntryId"] = Value::String(id.clone());
        }
        payload
    }
}

/// `rollover_compaction` over a plain first-kept id.
#[must_use]
pub fn rollover_compaction(
    first_kept_entry_id: Option<&str>,
    tokens_before: i64,
) -> RolloverCompaction {
    RolloverCompaction::new(first_kept_entry_id.map(str::to_owned), tokens_before)
}

/// Whether an entry is a rollover compaction (not a summarized one).
#[must_use]
pub fn is_rollover_compaction(entry: &Value) -> bool {
    entry
        .get("details")
        .and_then(|d| d.get("kind"))
        .and_then(Value::as_str)
        == Some(ROLLOVER_KIND)
}

// ─── Notebook reminder ───────────────────────────────────────────────────

/// `boundaryId`: the id of the last `compaction`/`reset_boundary`, else the
/// first entry's id, else `"empty"`. One reminder per boundary, so a rollover
/// (which commits a compaction) re-arms it.
#[must_use]
pub fn notes_reminder_boundary_id(branch: &[Value]) -> String {
    let last_boundary = branch.iter().rev().find(|entry| {
        matches!(
            entry.get("type").and_then(Value::as_str),
            Some("compaction" | "reset_boundary")
        )
    });
    let id = last_boundary
        .or_else(|| branch.first())
        .and_then(|entry| entry.get("id").and_then(Value::as_str));
    id.unwrap_or("empty").to_owned()
}

/// Whether the notebook nudge is due: experimental rollover is usable, nothing
/// else owns compaction, the window is still under the threshold but inside the
/// speculation lead, and this boundary has not been reminded yet.
#[must_use]
pub fn notes_reminder_due(
    experimental_enabled: bool,
    rollover_tools_present: bool,
    compaction_enabled: bool,
    compacting: bool,
    context_tokens: u64,
    threshold_tokens: u64,
    speculation_lead_tokens: u64,
    boundary_id: &str,
    last_reminded_boundary_id: Option<&str>,
) -> bool {
    if !experimental_enabled || !rollover_tools_present {
        return false;
    }
    if !compaction_enabled || compacting {
        return false;
    }
    if context_tokens >= threshold_tokens {
        return false;
    }
    if threshold_tokens - context_tokens > speculation_lead_tokens {
        return false;
    }
    last_reminded_boundary_id != Some(boundary_id)
}

/// The reminder to queue, if [`notes_reminder_due`] allows one.
#[must_use]
pub fn notes_reminder_to_queue(
    boundary_id: &str,
    last_reminded_boundary_id: Option<&str>,
    due: bool,
) -> Option<(String, String)> {
    (due && last_reminded_boundary_id != Some(boundary_id))
        .then(|| (boundary_id.to_owned(), notes_reminder_prompt()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested() -> Value {
        json!({"requested": true})
    }

    #[test]
    fn request_is_found_on_a_direct_new_context_result() {
        let details = requested();
        let results = [RolloverToolResult::new(
            "new_context",
            false,
            Some(&details),
        )];
        assert!(take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn errored_results_never_carry_a_request() {
        let details = requested();
        let results = [RolloverToolResult::new("new_context", true, Some(&details))];
        assert!(!take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn other_tools_and_non_boolean_flags_do_not_count() {
        let truthy = json!({"requested": "true"});
        let other = json!({"requested": true});
        let results = [
            RolloverToolResult::new("context_notes", false, Some(&other)),
            RolloverToolResult::new("bash", false, Some(&truthy)),
            RolloverToolResult::new("new_context", false, None),
        ];
        assert!(!take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn a_device_execute_dispatch_reaches_the_request() {
        let inner = requested();
        let dispatched = json!({"dispatched": true});
        let results = [RolloverToolResult::dispatched(
            "write",
            false,
            Some(&dispatched),
            WriteDeviceDispatch {
                tool: "new_context",
                mode: DispatchMode::Execute,
                inner_details: Some(&inner),
            },
        )];
        assert!(take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn a_device_propose_dispatch_does_not() {
        let inner = requested();
        let results = [RolloverToolResult::dispatched(
            "write",
            false,
            None,
            WriteDeviceDispatch {
                tool: "new_context",
                mode: DispatchMode::Propose,
                inner_details: Some(&inner),
            },
        )];
        assert!(!take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn a_device_dispatch_to_another_tool_does_not() {
        let inner = requested();
        let results = [RolloverToolResult::dispatched(
            "write",
            false,
            None,
            WriteDeviceDispatch {
                tool: "edit",
                mode: DispatchMode::Execute,
                inner_details: Some(&inner),
            },
        )];
        assert!(!take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn any_request_in_the_turn_wins() {
        let details = requested();
        let results = [
            RolloverToolResult::new("read", false, None),
            RolloverToolResult::new("new_context", false, Some(&details)),
        ];
        assert!(take_experimental_context_rollover_request(&results));
    }

    #[test]
    fn a_vanished_capability_drops_the_request() {
        let details = requested();
        let results = [RolloverToolResult::new(
            "new_context",
            false,
            Some(&details),
        )];
        let both = ["context_notes", "new_context"];
        assert!(accepted_rollover_request(true, &both, &both, &results));
        // Setting turned off between the result and dispatch.
        assert!(!accepted_rollover_request(false, &both, &both, &results));
        // `new_context` no longer registered.
        assert!(!accepted_rollover_request(
            true,
            &both,
            &["context_notes"],
            &results
        ));
        // `context_notes` no longer enabled.
        assert!(!accepted_rollover_request(
            true,
            &["new_context"],
            &both,
            &results
        ));
    }

    #[test]
    fn required_tools_are_the_notebook_pair() {
        assert_eq!(
            EXPERIMENTAL_CONTEXT_REQUIRED_TOOLS,
            ["context_notes", "new_context"]
        );
        assert!(!has_experimental_context_rollover_tools(&[], &[]));
    }

    #[test]
    fn rollover_compaction_payload_is_the_ported_shape() {
        let compaction = rollover_compaction(Some("e42"), 91_000);
        let payload = compaction.to_entry_payload();
        assert_eq!(payload["summary"], rollover_summary());
        assert_eq!(payload["firstKeptEntryId"], "e42");
        assert_eq!(payload["tokensBefore"], 91_000);
        assert_eq!(
            payload["details"],
            json!({"kind": "experimental-context-rollover", "version": 1})
        );
        assert!(payload.get("shortSummary").is_none());
        assert!(is_rollover_compaction(&payload));
    }

    #[test]
    fn rollover_compaction_omits_an_absent_first_kept_id() {
        let payload = rollover_compaction(None, 5).to_entry_payload();
        assert!(payload.get("firstKeptEntryId").is_none());
        assert_eq!(payload["tokensBefore"], 5);
    }

    #[test]
    fn summary_is_the_rendered_rollover_prompt() {
        let summary = rollover_summary();
        assert!(summary.starts_with("<context-window-rollover>"));
        assert!(summary.ends_with("</context-window-rollover>"));
        assert!(!summary.ends_with('\n'), "prompt.render trims the newline");
        assert!(summary.contains("history://current/full"));
        // A summarized compaction is not a rollover.
        assert!(!is_rollover_compaction(
            &json!({"summary": "s", "details": {"kind": "snapcompact"}})
        ));
        assert!(!is_rollover_compaction(&json!({"summary": "s"})));
    }

    #[test]
    fn reminder_prompt_is_the_rendered_nudge() {
        let prompt = notes_reminder_prompt();
        assert!(prompt.starts_with("<context-window-reminder>"));
        assert!(prompt.ends_with("</context-window-reminder>"));
        assert!(prompt.contains("16 KiB"));
        assert!(prompt.contains("context_notes"));
        assert!(!prompt.ends_with('\n'));
    }

    #[test]
    fn boundary_id_prefers_the_last_compaction_or_reset() {
        let branch = [
            json!({"type": "message", "id": "a"}),
            json!({"type": "compaction", "id": "c1"}),
            json!({"type": "message", "id": "b"}),
            json!({"type": "reset_boundary", "id": "r1"}),
        ];
        assert_eq!(notes_reminder_boundary_id(&branch), "r1");
        let compacted_only = [
            json!({"type": "message", "id": "a"}),
            json!({"type": "compaction", "id": "c1"}),
            json!({"type": "message", "id": "b"}),
        ];
        assert_eq!(notes_reminder_boundary_id(&compacted_only), "c1");
        let plain = [json!({"type": "message", "id": "a"})];
        assert_eq!(notes_reminder_boundary_id(&plain), "a");
        assert_eq!(notes_reminder_boundary_id(&[]), "empty");
    }

    #[test]
    fn reminder_is_due_only_inside_the_speculation_lead() {
        // threshold 100k, context 90k, lead 20k -> due.
        let due = |context_tokens: u64, boundary: Option<&str>| {
            notes_reminder_due(
                true,
                true,
                true,
                false,
                context_tokens,
                100_000,
                20_000,
                "c1",
                boundary,
            )
        };
        assert!(due(90_000, None));
        assert!(due(80_000, None), "exactly the lead is inside it");
        assert!(!due(79_999, None), "outside the lead");
        assert!(!due(100_000, None), "at the threshold compaction owns it");
        assert!(
            !due(90_000, Some("c1")),
            "already reminded for this boundary"
        );
        assert!(due(90_000, Some("c0")), "a new boundary re-arms it");
    }

    #[test]
    fn reminder_is_never_due_without_experimental_rollover() {
        let base = |experimental: bool, tools: bool, enabled: bool, compacting: bool| {
            notes_reminder_due(
                experimental,
                tools,
                enabled,
                compacting,
                95_000,
                100_000,
                20_000,
                "c1",
                None,
            )
        };
        assert!(base(true, true, true, false));
        assert!(!base(false, true, true, false));
        assert!(!base(true, false, true, false));
        assert!(!base(true, true, false, false));
        assert!(!base(true, true, true, true));
    }

    #[test]
    fn queued_reminder_records_its_boundary_once() {
        let (boundary, prompt) = notes_reminder_to_queue("c1", None, true).unwrap();
        assert_eq!(boundary, "c1");
        assert_eq!(prompt, notes_reminder_prompt());
        assert!(notes_reminder_to_queue("c1", Some("c1"), true).is_none());
        assert!(notes_reminder_to_queue("c1", None, false).is_none());
    }
}
