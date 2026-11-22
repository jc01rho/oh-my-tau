//! Lifecycle events the core emits itself, with the payload shapes of omp's `ExtensionRunner`
//! (`pi-coding-agent/src/extensibility/{shared-events,extensions/types}.ts`), plus the answers to the
//! host's own requests (`ui_confirm`, `compact`, `navigate_tree`, `set_model`, ...).
//!
//! Optional TS fields that are `undefined` are omitted on the wire (`JSON.stringify`), nullable ones are
//! `null`; field order follows the TS object literals so the serialized event matches byte for byte.
//! `AbortSignal` members (`session_before_compact`, `session_before_tree`, `session_stop`) are not
//! serializable and are never sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{EmitResult, ExtHost, HostMessage};

/// Why a session switch happens (`session_before_switch` / `session_switch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchReason {
    New,
    Resume,
    Fork,
}

/// Branch transition kind (`session_before_branch` / `session_branch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchReason {
    Branch,
    Fork,
    Btw,
}

/// How a prompt-cache refresh reaches the provider (TS `CacheWarmingRentMethod`):
/// `prewarm` sends the request with `max_tokens: 0`, `replay` re-sends it under a
/// one-token output cap and cuts generation off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingRentMethod {
    Prewarm,
    Replay,
}

/// What the warmer does with a decision: `warm` sends a refresh, `compact`
/// bridges the wait with compaction, `stop` lets the entry expire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingAction {
    Warm,
    Compact,
    Stop,
}

impl CacheWarmingAction {
    fn from_wire(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "warm" => Some(Self::Warm),
            "compact" => Some(Self::Compact),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// Payload of `cache_warming_decision` (TS `CacheWarmingDecisionEvent`): the warmer's economics for
/// one decision, plus the action it took. Field order and names follow the TS interface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheWarmingDecisionEvent {
    /// How the refresh would be sent; `None` when the request cannot be refreshed safely.
    pub method: Option<CacheWarmingRentMethod>,
    /// Price of this refresh: a cache read of the prompt, plus the expected billed output for a replay.
    pub warm_cost: f64,
    /// Extra price of the next real request if the entry is lost.
    pub miss_cost: f64,
    /// Price of bridging the wait with compaction; `None` when compaction is not an option.
    pub compact_cost: Option<f64>,
    /// Refresh spend since the last real request.
    pub spent_cost: f64,
    /// Estimated chance that a real request arrives before the entry expires.
    pub continuation_probability: f64,
    /// The warmer's own decision.
    pub action: CacheWarmingAction,
}

/// Outcome of `cache_warming_decision`: the last handler's `action` override, if any.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheWarmingOutcome {
    pub action: Option<CacheWarmingAction>,
}

impl CacheWarmingOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            action: v.get("action").and_then(CacheWarmingAction::from_wire),
        }
    }
}

/// Origin of an `input` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InputSource {
    Interactive,
    Rpc,
    Extension,
}

/// Why auto-compaction started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Threshold,
    Overflow,
    Idle,
    Incomplete,
}

/// Auto-compaction strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CompactionAction {
    #[serde(rename = "context-full")]
    ContextFull,
    #[serde(rename = "remote")]
    Remote,
    #[serde(rename = "handoff")]
    Handoff,
    #[serde(rename = "shake")]
    Shake,
    #[serde(rename = "snapcompact")]
    Snapcompact,
}

/// Why `resources_discover` fires (`ResourcesDiscoverEvent["reason"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourcesDiscoverReason {
    Startup,
    Reload,
}

/// Tool approval mode (`tools/approval.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ApprovalMode {
    #[serde(rename = "always-ask")]
    AlwaysAsk,
    #[serde(rename = "write")]
    Write,
    #[serde(rename = "yolo")]
    Yolo,
}

/// Status of one built-in session mode (`SessionModeState` in omp's `extensibility/extensions/types.ts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModeStatus {
    #[default]
    Inactive,
    Active,
    Paused,
}

/// Plan mode: `planFilePath` is omitted when no plan file is known (TS optional field).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeState {
    pub status: ModeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_file_path: Option<String>,
}

/// Vibe mode status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeModeState {
    pub status: ModeStatus,
}

/// Goal mode: `objective`/`tokenBudget` are omitted when no goal is tracked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalModeState {
    pub status: ModeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
}

/// Loop mode status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopModeState {
    pub status: ModeStatus,
}

/// Detached snapshot of the built-in session modes (TS `SessionModeState`). Field order follows
/// the TS object literal so the serialized event matches byte for byte.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    pub plan: PlanModeState,
    pub vibe: VibeModeState,
    pub goal: GoalModeState,
    #[serde(rename = "loop")]
    pub r#loop: LoopModeState,
}

/// What spawned a subagent (`before_subagent_spawn.invocationKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentInvocationKind {
    Task,
    Eval,
}

/// Stable ids of the interactive plan review's choices (`types.ts` `PlanReviewChoice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanReviewChoice {
    Execute,
    Compact,
    Keep,
    Refine,
    Save,
}

impl PlanReviewChoice {
    /// Parse a wire id (`"execute"`, ...).
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Some(match id {
            "execute" => Self::Execute,
            "compact" => Self::Compact,
            "keep" => Self::Keep,
            "refine" => Self::Refine,
            "save" => Self::Save,
            _ => return None,
        })
    }

    /// Whether the choice approves the plan (the only choices an execution tier applies to).
    #[must_use]
    pub fn approves(self) -> bool {
        matches!(self, Self::Execute | Self::Compact | Self::Keep)
    }
}

/// Who answered a plan review (`plan_review_resolved.by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanReviewBy {
    Local,
    Extension,
}

/// One plan-review choice as `plan_review_requested` reports it (`PlanReviewOption`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanReviewOption {
    pub id: PlanReviewChoice,
    pub label: String,
    pub disabled: bool,
}

/// An execution tier of the plan review's model slider (`PlanReviewTier`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewTier {
    pub role: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

/// `plan_review_requested` payload (`PlanReviewRequestedEvent` minus `type`), field order as in
/// `interactive-mode.ts` `handlePlanApproval`. Also the value of `ctx.getPendingPlanReview()`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewRequest {
    pub review_id: String,
    pub session_id: String,
    pub title: String,
    pub plan_file_path: String,
    pub plan_content: String,
    pub options: Vec<PlanReviewOption>,
    /// Omitted when fewer than two tiers resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<PlanReviewTier>>,
}

/// An omp extension event. Message/entry payloads are opaque omp JSON (`AgentMessage`, `SessionEntry`, ...).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum LifecycleEvent {
    #[serde(rename = "session_start")]
    SessionStart,
    #[serde(rename = "before_agent_start", rename_all = "camelCase")]
    BeforeAgentStart {
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Value>,
        system_prompt: Vec<String>,
    },
    #[serde(rename = "input")]
    Input {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Value>,
        source: InputSource,
    },
    #[serde(rename = "agent_start")]
    AgentStart,
    #[serde(rename = "agent_end", rename_all = "camelCase")]
    AgentEnd {
        messages: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        will_continue: Option<bool>,
    },
    #[serde(rename = "turn_start", rename_all = "camelCase")]
    TurnStart { turn_index: u64, timestamp: u64 },
    #[serde(rename = "turn_end", rename_all = "camelCase")]
    TurnEnd {
        turn_index: u64,
        message: Value,
        tool_results: Vec<Value>,
    },
    #[serde(rename = "message_start")]
    MessageStart { message: Value },
    #[serde(rename = "message_update", rename_all = "camelCase")]
    MessageUpdate {
        message: Value,
        assistant_message_event: Value,
    },
    #[serde(rename = "message_end")]
    MessageEnd { message: Value },
    #[serde(rename = "tool_execution_start", rename_all = "camelCase")]
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        intent: Option<String>,
    },
    #[serde(rename = "tool_execution_update", rename_all = "camelCase")]
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: Value,
    },
    #[serde(rename = "tool_execution_end", rename_all = "camelCase")]
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: Value,
        is_error: bool,
    },
    #[serde(rename = "session_before_compact", rename_all = "camelCase")]
    SessionBeforeCompact {
        preparation: Value,
        branch_entries: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    #[serde(rename = "session_compact", rename_all = "camelCase")]
    SessionCompact {
        compaction_entry: Value,
        from_extension: bool,
    },
    #[serde(rename = "session_before_switch", rename_all = "camelCase")]
    SessionBeforeSwitch {
        reason: SwitchReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_session_file: Option<String>,
    },
    #[serde(rename = "session_switch", rename_all = "camelCase")]
    SessionSwitch {
        reason: SwitchReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_session_file: Option<String>,
    },
    #[serde(rename = "session_before_branch", rename_all = "camelCase")]
    SessionBeforeBranch {
        reason: BranchReason,
        entry_id: String,
    },
    #[serde(rename = "session_branch", rename_all = "camelCase")]
    SessionBranch {
        reason: BranchReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_session_file: Option<String>,
    },
    #[serde(rename = "session_before_tree")]
    SessionBeforeTree { preparation: Value },
    #[serde(rename = "session_tree", rename_all = "camelCase")]
    SessionTree {
        new_leaf_id: Option<String>,
        old_leaf_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary_entry: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from_extension: Option<bool>,
    },
    /// Main-agent turn about to settle. Fields are `snake_case` in omp.
    #[serde(rename = "session_stop")]
    SessionStop {
        messages: Vec<Value>,
        turn_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_assistant_message: Option<Value>,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_file: Option<String>,
        stop_hook_active: bool,
    },
    /// Finalized assistant message (rewritable: see [`AssistantRewriteOutcome`]).
    #[serde(rename = "assistant_message")]
    AssistantMessage { message: Value },
    #[serde(rename = "auto_compaction_start")]
    AutoCompactionStart {
        reason: CompactionReason,
        action: CompactionAction,
    },
    #[serde(rename = "auto_compaction_end", rename_all = "camelCase")]
    AutoCompactionEnd {
        action: CompactionAction,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        aborted: bool,
        will_retry: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<bool>,
    },
    #[serde(rename = "auto_retry_start", rename_all = "camelCase")]
    AutoRetryStart {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error_message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_id: Option<u64>,
    },
    #[serde(rename = "auto_retry_end", rename_all = "camelCase")]
    AutoRetryEnd {
        success: bool,
        attempt: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_errors: Option<Vec<Value>>,
    },
    #[serde(rename = "retry_fallback_applied")]
    RetryFallbackApplied {
        from: String,
        to: String,
        role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "retry_fallback_succeeded")]
    RetryFallbackSucceeded { model: String, role: String },
    #[serde(rename = "ttsr_triggered")]
    TtsrTriggered { rules: Vec<Value> },
    #[serde(rename = "todo_reminder", rename_all = "camelCase")]
    TodoReminder {
        todos: Vec<Value>,
        attempt: u64,
        max_attempts: u64,
    },
    /// `goal` is `null` when the goal was cleared.
    #[serde(rename = "goal_updated")]
    GoalUpdated {
        goal: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<Value>,
    },
    /// Field order follows `wrapper.ts` (sessionId, toolName, toolCallId, ...).
    #[serde(rename = "tool_approval_requested", rename_all = "camelCase")]
    ToolApprovalRequested {
        session_id: String,
        tool_name: String,
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        approval_mode: ApprovalMode,
    },
    #[serde(rename = "tool_approval_resolved", rename_all = "camelCase")]
    ToolApprovalResolved {
        session_id: String,
        tool_name: String,
        tool_call_id: String,
        approved: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// `!`/`!!` shell command typed by the user.
    #[serde(rename = "user_bash", rename_all = "camelCase")]
    UserBash {
        command: String,
        exclude_from_context: bool,
        cwd: String,
    },
    /// `$`/`$$` Python typed by the user.
    #[serde(rename = "user_python", rename_all = "camelCase")]
    UserPython {
        code: String,
        exclude_from_context: bool,
        cwd: String,
    },
    /// Prompt-cache warming decision, with the warmer's own economics filled in
    /// (`CacheWarmingDecisionEvent`). A handler's `action` overrides it.
    #[serde(rename = "cache_warming_decision", rename_all = "camelCase")]
    CacheWarmingDecision {
        #[serde(skip_serializing_if = "Option::is_none")]
        method: Option<CacheWarmingRentMethod>,
        warm_cost: f64,
        miss_cost: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        compact_cost: Option<f64>,
        spent_cost: f64,
        continuation_probability: f64,
        action: CacheWarmingAction,
    },

    /// The auth store automatically soft-disabled a credential (TS `CredentialDisabledEvent`):
    /// a definitive OAuth refresh failure (`invalid_grant`, `revoked`, ...) or an auth-broker
    /// disable. A user-initiated removal, a logout and the duplicate-credential purge stay
    /// silent, like upstream. `disabled_cause` is the verbatim error kept for forensics.
    #[serde(rename = "credential_disabled", rename_all = "camelCase")]
    CredentialDisabled {
        provider: String,
        disabled_cause: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        credential_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        org_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        org_name: Option<String>,
    },

    /// A built-in session mode changed, from any source: a slash command, a tool, RPC, or session
    /// restore. `previous` is the last state reported through this event (all-inactive at first).
    #[serde(rename = "mode_changed")]
    ModeChanged {
        previous: SessionModeState,
        state: SessionModeState,
    },

    /// A subagent is about to spawn, before its model is resolved.
    #[serde(rename = "before_subagent_spawn", rename_all = "camelCase")]
    BeforeSubagentSpawn {
        /// Agent definition name being spawned.
        agent: String,
        invocation_kind: SubagentInvocationKind,
        /// Pre-expansion role alias; absent for an explicit selector.
        #[serde(skip_serializing_if = "Option::is_none")]
        model_role: Option<String>,
        /// Model patterns the spawn would use, in attempt order.
        patterns: Vec<String>,
        /// Stable per-spawn key, when the spawn has one.
        #[serde(skip_serializing_if = "Option::is_none")]
        spawn_key: Option<String>,
    },

    /// A provider response arrived, before its stream body is read
    /// (`types.ts` `AfterProviderResponseEvent extends ProviderResponseMetadata`). Observe-only:
    /// the runner discards handler results. `metadata` is absent here for the same reason it is
    /// absent upstream: no provider of this core supplies any.
    #[serde(rename = "after_provider_response", rename_all = "camelCase")]
    AfterProviderResponse {
        status: u16,
        headers: std::collections::BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Before compaction summarization: handlers may replace the summary prompt, add context
    /// lines and stash `preserveData` on the entry (the last result wins).
    #[serde(rename = "session.compacting", rename_all = "camelCase")]
    SessionCompacting {
        session_id: String,
        messages: Vec<Value>,
    },

    /// Extensions may contribute skill/prompt/theme paths (`ResourcesDiscoverEvent`); emitted after
    /// `session_start` at startup and on every extension reload.
    #[serde(rename = "resources_discover", rename_all = "camelCase")]
    ResourcesDiscover {
        cwd: String,
        reason: ResourcesDiscoverReason,
    },

    /// The live/model context was cleared in place: the session id, title and
    /// transcript file survive (`/clear`, TS `SessionClearEvent`).
    #[serde(rename = "session_clear")]
    SessionClear,

    /// A JSON-RPC notification received from a connected MCP server, after the
    /// runtime's own handling of the frame (omp `McpNotificationEvent`).
    #[serde(rename = "mcp_notification")]
    McpNotification {
        /// Server name exactly as declared in the MCP config (raw, unsanitized).
        server: String,
        /// JSON-RPC method (`notifications/tools/list_changed`, or server-custom).
        method: String,
        /// JSON-RPC params, opaque to the runtime.
        params: Value,
    },
    /// The plan review opened (a plan awaits approval).
    #[serde(rename = "plan_review_requested")]
    PlanReviewRequested(PlanReviewRequest),
    /// The plan review closed: a choice, or none when dismissed/cancelled.
    #[serde(rename = "plan_review_resolved", rename_all = "camelCase")]
    PlanReviewResolved {
        review_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        choice: Option<PlanReviewChoice>,
        by: PlanReviewBy,
    },
    /// An operator close the host can still refuse (cancellable). The `signal` the TS event
    /// carries is made host-side and aborted through `abort_emit` (see
    /// [`ExtHost::emit_session_before_shutdown`]).
    #[serde(rename = "session_before_shutdown")]
    SessionBeforeShutdown,
    /// A moving session's out-of-folder state: handlers write what must follow the session into
    /// `dir` while its runtime is live (the host hands each handler an `AbortSignal` as `signal`).
    #[serde(rename = "session_move_export", rename_all = "camelCase")]
    SessionMoveExport { move_id: String, dir: String },
    /// A moved session's extension state, delivered once its new runtime started; `redelivered`
    /// when an earlier delivery of the same state did not complete.
    #[serde(rename = "session_move_import", rename_all = "camelCase")]
    SessionMoveImport {
        move_id: String,
        dir: String,
        redelivered: bool,
    },
    /// Process exit (the session-ending event).
    #[serde(rename = "session_shutdown")]
    SessionShutdown,
}

impl LifecycleEvent {
    /// The wire form handed to [`ExtHost::emit`].
    ///
    /// # Panics
    /// Never in practice: every field serializes to JSON.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("lifecycle events are plain JSON")
    }
}

/// Outcome of `before_agent_start`: custom messages to inject and an optional replaced system prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeAgentStartOutcome {
    pub messages: Vec<Value>,
    pub system_prompt: Option<Vec<String>>,
}

/// Outcome of `input`: `handled` stops normal flow; `text`/`images` replace the submitted input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputOutcome {
    pub handled: bool,
    pub text: Option<String>,
    pub images: Option<Value>,
}

/// Outcome of a cancellable `session_before_*` event; `result` is the last handler result
/// (e.g. `{compaction}` for `session_before_compact`, `{summary}` for `session_before_tree`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CancelOutcome {
    pub cancel: bool,
    pub result: Value,
}

/// Outcome of `session.compacting` (TS `SessionCompactingResult`): extra summary context, a
/// summarization-prompt override, and data the committed `compaction` entry keeps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactingOutcome {
    /// Lines appended to the summary request (`<additional-context>`).
    pub context: Vec<String>,
    /// Replaces the default summarization prompt.
    pub prompt: Option<String>,
    /// Stored on the `compaction` entry as `preserveData`.
    pub preserve_data: Option<Value>,
}

impl CompactingOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            context: v
                .get("context")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            prompt: str_of(v, "prompt"),
            preserve_data: v.get("preserveData").filter(|d| d.is_object()).cloned(),
        }
    }
}

/// Outcome of `session_stop`: a handler may request one continuation turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStopOutcome {
    /// Claude/Codex-style `decision: "block"`.
    pub block: bool,
    /// omp-native `continue: true`.
    pub continue_turn: bool,
    pub additional_context: Option<String>,
    pub reason: Option<String>,
}

impl SessionStopOutcome {
    /// Whether the session must run one more turn.
    #[must_use]
    pub fn wants_continuation(&self) -> bool {
        self.block || self.continue_turn
    }
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A `model` handler result: one selector or a list, comma-separated entries flattened and
/// blanks dropped (TS `normalizeModelPatternList`). `None` when nothing is left, which leaves the
/// spawn's own patterns in place.
fn model_patterns(v: &Value) -> Option<Vec<String>> {
    let raw: Vec<&str> = match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    let patterns: Vec<String> = raw
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    (!patterns.is_empty()).then_some(patterns)
}

impl BeforeAgentStartOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            messages: v
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            system_prompt: v.get("systemPrompt").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        }
    }
}

impl InputOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            handled: v.get("handled").and_then(Value::as_bool).unwrap_or(false),
            text: str_of(v, "text"),
            images: v.get("images").cloned(),
        }
    }
}

impl CancelOutcome {
    fn from_result(r: EmitResult) -> Self {
        Self {
            cancel: r.result.get("cancel").and_then(Value::as_bool) == Some(true),
            result: r.result,
        }
    }
}

impl SessionStopOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            block: v.get("decision").and_then(Value::as_str) == Some("block"),
            continue_turn: v.get("continue").and_then(Value::as_bool) == Some(true),
            additional_context: str_of(v, "additionalContext"),
            reason: str_of(v, "reason"),
        }
    }
}

/// Outcome of `before_subagent_spawn`: `block` refuses the spawn, `model` replaces the pattern
/// list, `note` labels the routing decision (surfaced as `resolvedModelRoute`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeSubagentSpawnOutcome {
    pub block: bool,
    pub reason: Option<String>,
    pub model: Option<Vec<String>>,
    pub note: Option<String>,
}

impl BeforeSubagentSpawnOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            block: v.get("block").and_then(Value::as_bool) == Some(true),
            reason: str_of(v, "reason"),
            model: v.get("model").and_then(model_patterns),
            note: str_of(v, "note"),
        }
    }
}

/// Outcome of `assistant_message`: replacement `content` (text-only rewrites, chained) when a handler changed it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantRewriteOutcome {
    pub content: Option<Vec<Value>>,
}

/// Outcome of `user_bash` / `user_python`: the first handler result (`BashResult`/`PythonResult`), if any.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserShellOutcome {
    pub result: Option<Value>,
}

impl AssistantRewriteOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            content: v.get("content").and_then(Value::as_array).cloned(),
        }
    }
}

impl UserShellOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            result: v.get("result").cloned(),
        }
    }
}

/// One path a `resources_discover` handler contributed, tagged with the extension that returned it
/// (the host's `{path, extensionPath}` entries).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredPath {
    /// Path as the handler spelled it (absolute, `~`-relative or cwd-relative).
    pub path: String,
    /// `ext.path` of the contributing extension.
    pub extension_path: String,
}

/// Outcome of `resources_discover`: every handler's paths, in load order (`ExtensionRunner.emitResourcesDiscover`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourcesDiscoverOutcome {
    pub skill_paths: Vec<DiscoveredPath>,
    pub prompt_paths: Vec<DiscoveredPath>,
    pub theme_paths: Vec<DiscoveredPath>,
}

impl ResourcesDiscoverOutcome {
    fn from_value(v: &Value) -> Self {
        Self {
            skill_paths: discovered_paths(v, "skillPaths"),
            prompt_paths: discovered_paths(v, "promptPaths"),
            theme_paths: discovered_paths(v, "themePaths"),
        }
    }
}

/// `[{path, extensionPath}]` under `key`; entries without a string `path` are dropped (TS pushes
/// only the paths it was handed, so a malformed entry has no usable path either).
fn discovered_paths(v: &Value, key: &str) -> Vec<DiscoveredPath> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    Some(DiscoveredPath {
                        path: e.get("path").and_then(Value::as_str)?.to_owned(),
                        extension_path: str_of(e, "extensionPath").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

macro_rules! notify_helpers {
    ($($(#[$m:meta])* $name:ident($($arg:ident: $ty:ty),*) => $ev:expr;)*) => {
        $(
            $(#[$m])*
            ///
            /// # Errors
            /// Transport failure; handler failures are reported in the returned [`EmitResult`].
            pub async fn $name(&self, $($arg: $ty,)* ctx: Option<Value>) -> Result<EmitResult> {
                self.emit_event(&$ev, ctx).await
            }
        )*
    };
}

impl ExtHost {
    /// Emit any [`LifecycleEvent`].
    ///
    /// # Errors
    /// Transport failure; handler failures are reported in the returned [`EmitResult`].
    pub async fn emit_event(
        &self,
        event: &LifecycleEvent,
        ctx: Option<Value>,
    ) -> Result<EmitResult> {
        self.emit(event.to_value(), ctx).await
    }

    notify_helpers! {
        /// `session_start`: initial session load.
        emit_session_start() => LifecycleEvent::SessionStart;
        /// `agent_start`: an agent loop starts (once per user prompt).
        emit_agent_start() => LifecycleEvent::AgentStart;
        /// `agent_end`: an agent loop ended; `will_continue` when a continuation is already scheduled.
        emit_agent_end(messages: Vec<Value>, will_continue: Option<bool>) => LifecycleEvent::AgentEnd { messages, will_continue };
        /// `turn_start`.
        emit_turn_start(turn_index: u64, timestamp: u64) => LifecycleEvent::TurnStart { turn_index, timestamp };
        /// `turn_end`.
        emit_turn_end(turn_index: u64, message: Value, tool_results: Vec<Value>) => LifecycleEvent::TurnEnd { turn_index, message, tool_results };
        /// `message_start`.
        emit_message_start(message: Value) => LifecycleEvent::MessageStart { message };
        /// `message_update`.
        emit_message_update(message: Value, assistant_message_event: Value) => LifecycleEvent::MessageUpdate { message, assistant_message_event };
        /// `message_end` (notification only).
        emit_message_end(message: Value) => LifecycleEvent::MessageEnd { message };
        /// `tool_execution_start`.
        emit_tool_execution_start(tool_call_id: String, tool_name: String, args: Value, intent: Option<String>) => LifecycleEvent::ToolExecutionStart { tool_call_id, tool_name, args, intent };
        /// `tool_execution_update`.
        emit_tool_execution_update(tool_call_id: String, tool_name: String, args: Value, partial_result: Value) => LifecycleEvent::ToolExecutionUpdate { tool_call_id, tool_name, args, partial_result };
        /// `tool_execution_end`.
        emit_tool_execution_end(tool_call_id: String, tool_name: String, result: Value, is_error: bool) => LifecycleEvent::ToolExecutionEnd { tool_call_id, tool_name, result, is_error };
        /// `session_compact`: compaction finished.
        emit_session_compact(compaction_entry: Value, from_extension: bool) => LifecycleEvent::SessionCompact { compaction_entry, from_extension };
        /// `session_switch`: after switching to another session.
        emit_session_switch(reason: SwitchReason, previous_session_file: Option<String>) => LifecycleEvent::SessionSwitch { reason, previous_session_file };
        /// `session_branch`: after branching.
        emit_session_branch(reason: BranchReason, previous_session_file: Option<String>) => LifecycleEvent::SessionBranch { reason, previous_session_file };
        /// `session_tree`: after tree navigation.
        emit_session_tree(new_leaf_id: Option<String>, old_leaf_id: Option<String>, summary_entry: Option<Value>, from_extension: Option<bool>) => LifecycleEvent::SessionTree { new_leaf_id, old_leaf_id, summary_entry, from_extension };
        /// `session_shutdown`: process exit (handlers run in parallel, 2 s cap in the host).
        emit_session_shutdown() => LifecycleEvent::SessionShutdown;
        /// `session_move_export`: a failed or timed-out handler is reported in `errors`, and the
        /// move must not go ahead without its state.
        emit_session_move_export(move_id: String, dir: String) => LifecycleEvent::SessionMoveExport { move_id, dir };
        /// `session_move_import`: moved extension state handed to the session's extensions.
        emit_session_move_import(move_id: String, dir: String, redelivered: bool) => LifecycleEvent::SessionMoveImport { move_id, dir, redelivered };
        /// `auto_compaction_start`.
        emit_auto_compaction_start(reason: CompactionReason, action: CompactionAction) => LifecycleEvent::AutoCompactionStart { reason, action };
        /// `auto_compaction_end`.
        emit_auto_compaction_end(action: CompactionAction, result: Option<Value>, aborted: bool, will_retry: bool, error_message: Option<String>, skipped: Option<bool>) => LifecycleEvent::AutoCompactionEnd { action, result, aborted, will_retry, error_message, skipped };
        /// `auto_retry_start`.
        emit_auto_retry_start(attempt: u64, max_attempts: u64, delay_ms: u64, error_message: String, error_id: Option<u64>) => LifecycleEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message, error_id };
        /// `auto_retry_end`.
        emit_auto_retry_end(success: bool, attempt: u64, final_error: Option<String>, retry_errors: Option<Vec<Value>>) => LifecycleEvent::AutoRetryEnd { success, attempt, final_error, retry_errors };
        /// `retry_fallback_applied`.
        emit_retry_fallback_applied(from: String, to: String, role: String, reason: Option<String>) => LifecycleEvent::RetryFallbackApplied { from, to, role, reason };
        /// `retry_fallback_succeeded`.
        emit_retry_fallback_succeeded(model: String, role: String) => LifecycleEvent::RetryFallbackSucceeded { model, role };
        /// `ttsr_triggered`.
        emit_ttsr_triggered(rules: Vec<Value>) => LifecycleEvent::TtsrTriggered { rules };
        /// `todo_reminder`.
        emit_todo_reminder(todos: Vec<Value>, attempt: u64, max_attempts: u64) => LifecycleEvent::TodoReminder { todos, attempt, max_attempts };
        /// `goal_updated`.
        emit_goal_updated(goal: Option<Value>, state: Option<Value>) => LifecycleEvent::GoalUpdated { goal, state };
        /// `tool_approval_requested`.
        emit_tool_approval_requested(session_id: String, tool_name: String, tool_call_id: String, reason: Option<String>, approval_mode: ApprovalMode) => LifecycleEvent::ToolApprovalRequested { session_id, tool_name, tool_call_id, reason, approval_mode };
        /// `tool_approval_resolved`.
        emit_tool_approval_resolved(session_id: String, tool_name: String, tool_call_id: String, approved: bool, reason: Option<String>) => LifecycleEvent::ToolApprovalResolved { session_id, tool_name, tool_call_id, approved, reason };
        /// `credential_disabled`: the auth store tore down a credential on its own.
        #[allow(clippy::too_many_arguments)] // one argument per omp payload field
        emit_credential_disabled(provider: String, disabled_cause: String, credential_id: Option<i64>, email: Option<String>, account_id: Option<String>, org_id: Option<String>, org_name: Option<String>) => LifecycleEvent::CredentialDisabled { provider, disabled_cause, credential_id, email, account_id, org_id, org_name };
        /// `mode_changed`: a built-in session mode changed.
        emit_mode_changed(previous: SessionModeState, state: SessionModeState) => LifecycleEvent::ModeChanged { previous, state };
        /// `after_provider_response`.
        emit_after_provider_response(status: u16, headers: std::collections::BTreeMap<String, String>, request_id: Option<String>) => LifecycleEvent::AfterProviderResponse { status, headers, request_id };
        /// `mcp_notification`: a server→client MCP notification, after the runtime handled it.
        emit_mcp_notification(server: String, method: String, params: Value) => LifecycleEvent::McpNotification { server, method, params };
        /// `session_clear`: the context was reset in place, the session kept.
        emit_session_clear() => LifecycleEvent::SessionClear;
        /// `plan_review_requested`.
        emit_plan_review_requested(request: PlanReviewRequest) => LifecycleEvent::PlanReviewRequested(request);
        /// `plan_review_resolved`.
        emit_plan_review_resolved(review_id: String, session_id: String, choice: Option<PlanReviewChoice>, by: PlanReviewBy) => LifecycleEvent::PlanReviewResolved { review_id, session_id, choice, by };
    }

    /// `before_subagent_spawn`: `block` refuses the spawn; `model` replaces the pattern list.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_before_subagent_spawn(
        &self,
        agent: String,
        invocation_kind: SubagentInvocationKind,
        model_role: Option<String>,
        patterns: Vec<String>,
        spawn_key: Option<String>,
        ctx: Option<Value>,
    ) -> Result<BeforeSubagentSpawnOutcome> {
        let ev = LifecycleEvent::BeforeSubagentSpawn {
            agent,
            invocation_kind,
            model_role,
            patterns,
            spawn_key,
        };
        Ok(BeforeSubagentSpawnOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `session.compacting`: before summarization; the last handler result may override the
    /// prompt, add context lines and set the entry's `preserveData`.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_session_compacting(
        &self,
        session_id: String,
        messages: Vec<Value>,
        ctx: Option<Value>,
    ) -> Result<CompactingOutcome> {
        let ev = LifecycleEvent::SessionCompacting {
            session_id,
            messages,
        };
        Ok(CompactingOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `cache_warming_decision`: a handler may override the warmer's action (the last one wins).
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_cache_warming_decision(
        &self,
        event: &CacheWarmingDecisionEvent,
        ctx: Option<Value>,
    ) -> Result<CacheWarmingOutcome> {
        let ev = LifecycleEvent::CacheWarmingDecision {
            method: event.method,
            warm_cost: event.warm_cost,
            miss_cost: event.miss_cost,
            compact_cost: event.compact_cost,
            spent_cost: event.spent_cost,
            continuation_probability: event.continuation_probability,
            action: event.action,
        };
        Ok(CacheWarmingOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `assistant_message`: a handler may rewrite the text of the finalized message (text blocks only).
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_assistant_message(
        &self,
        message: Value,
        ctx: Option<Value>,
    ) -> Result<AssistantRewriteOutcome> {
        let ev = LifecycleEvent::AssistantMessage { message };
        Ok(AssistantRewriteOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `user_bash`: the first handler result replaces execution.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_user_bash(
        &self,
        command: String,
        exclude_from_context: bool,
        cwd: String,
        ctx: Option<Value>,
    ) -> Result<UserShellOutcome> {
        let ev = LifecycleEvent::UserBash {
            command,
            exclude_from_context,
            cwd,
        };
        Ok(UserShellOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `user_python`: the first handler result replaces execution.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_user_python(
        &self,
        code: String,
        exclude_from_context: bool,
        cwd: String,
        ctx: Option<Value>,
    ) -> Result<UserShellOutcome> {
        let ev = LifecycleEvent::UserPython {
            code,
            exclude_from_context,
            cwd,
        };
        Ok(UserShellOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `before_agent_start`: returns the messages to inject and the (possibly replaced) system prompt.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_before_agent_start(
        &self,
        prompt: String,
        images: Option<Value>,
        system_prompt: Vec<String>,
        ctx: Option<Value>,
    ) -> Result<BeforeAgentStartOutcome> {
        let ev = LifecycleEvent::BeforeAgentStart {
            prompt,
            images,
            system_prompt,
        };
        Ok(BeforeAgentStartOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `input`: returns the transformed text/images or `handled`.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_input(
        &self,
        text: String,
        images: Option<Value>,
        source: InputSource,
        ctx: Option<Value>,
    ) -> Result<InputOutcome> {
        let ev = LifecycleEvent::Input {
            text,
            images,
            source,
        };
        Ok(InputOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `resources_discover`: the skill/prompt/theme paths every handler contributed.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_resources_discover(
        &self,
        cwd: String,
        reason: ResourcesDiscoverReason,
        ctx: Option<Value>,
    ) -> Result<ResourcesDiscoverOutcome> {
        let ev = LifecycleEvent::ResourcesDiscover { cwd, reason };
        Ok(ResourcesDiscoverOutcome::from_value(
            &self.emit_event(&ev, ctx).await?.result,
        ))
    }

    /// `session_before_compact`: cancellable; `result["compaction"]` may carry an extension-made compaction.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_session_before_compact(
        &self,
        preparation: Value,
        branch_entries: Vec<Value>,
        custom_instructions: Option<String>,
        ctx: Option<Value>,
    ) -> Result<CancelOutcome> {
        let ev = LifecycleEvent::SessionBeforeCompact {
            preparation,
            branch_entries,
            custom_instructions,
        };
        Ok(CancelOutcome::from_result(self.emit_event(&ev, ctx).await?))
    }

    /// `session_before_switch`: cancellable.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_session_before_switch(
        &self,
        reason: SwitchReason,
        target_session_file: Option<String>,
        ctx: Option<Value>,
    ) -> Result<CancelOutcome> {
        let ev = LifecycleEvent::SessionBeforeSwitch {
            reason,
            target_session_file,
        };
        Ok(CancelOutcome::from_result(self.emit_event(&ev, ctx).await?))
    }

    /// `session_before_branch`: cancellable.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_session_before_branch(
        &self,
        reason: BranchReason,
        entry_id: String,
        ctx: Option<Value>,
    ) -> Result<CancelOutcome> {
        let ev = LifecycleEvent::SessionBeforeBranch { reason, entry_id };
        Ok(CancelOutcome::from_result(self.emit_event(&ev, ctx).await?))
    }

    /// `session_before_tree`: cancellable; `result["summary"]` may carry a custom summary.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_session_before_tree(
        &self,
        preparation: Value,
        ctx: Option<Value>,
    ) -> Result<CancelOutcome> {
        let ev = LifecycleEvent::SessionBeforeTree { preparation };
        Ok(CancelOutcome::from_result(self.emit_event(&ev, ctx).await?))
    }

    /// `session_stop`: a handler may request one continuation turn.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn emit_session_stop(
        &self,
        event: LifecycleEvent,
        ctx: Option<Value>,
    ) -> Result<SessionStopOutcome> {
        anyhow::ensure!(
            matches!(event, LifecycleEvent::SessionStop { .. }),
            "emit_session_stop needs LifecycleEvent::SessionStop"
        );
        Ok(SessionStopOutcome::from_value(
            &self.emit_event(&event, ctx).await?.result,
        ))
    }

    /// `session_before_shutdown` gate (TS `AgentSession.confirmClose`): `Ok(true)` lets the
    /// close proceed. Without subscribers nothing is emitted. When `aborted` resolves first the
    /// close stops waiting: the host's `event.signal` is aborted, a late handler result is
    /// dropped and the close proceeds; only a `cancel` delivered before that keeps the session.
    ///
    /// # Errors
    /// Transport failure (callers proceed with the close, as TS does on errors).
    pub async fn emit_session_before_shutdown(
        &self,
        aborted: impl Future<Output = ()> + Send,
        ctx: Option<Value>,
    ) -> Result<bool> {
        let Some((abort_id, rx)) =
            self.emit_abortable(&LifecycleEvent::SessionBeforeShutdown.to_value(), ctx)?
        else {
            return Ok(true);
        };
        tokio::select! {
            biased;
            reply = rx => {
                let v = match reply {
                    Ok(Ok(v)) => v,
                    Ok(Err(m)) => anyhow::bail!("emit: {m}"),
                    Err(_) => anyhow::bail!("emit: extension host dropped the request"),
                };
                let r: EmitResult = serde_json::from_value(v)?;
                Ok(!CancelOutcome::from_result(r).cancel)
            }
            () = aborted => {
                self.abort_emit(abort_id);
                Ok(true)
            }
        }
    }
}

/// Boxed future used by [`HostResponder`] so it stays object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Answers the host's requests that need the core. Defaults are omp's `hasUI=false` behaviour
/// (no-op UI context and `ExtensionRunner` default actions).
pub trait HostResponder: Send + Sync {
    /// `ctx.ui.confirm(title, message)`; no UI declines.
    fn ui_confirm<'a>(&'a self, _title: &'a str, _message: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    /// `ctx.ui.select(title, options)`; no UI yields `None`.
    fn ui_select<'a>(
        &'a self,
        _title: &'a str,
        _options: Vec<String>,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
    /// `ctx.ui.input(title, placeholder)`; no UI yields `None`.
    fn ui_input<'a>(
        &'a self,
        _title: &'a str,
        _placeholder: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
    /// `ctx.ui.editor(title, prefill)`; no UI yields `None`.
    fn ui_editor<'a>(
        &'a self,
        _title: &'a str,
        _prefill: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
    /// `ctx.compact(options)`; the default runner action is a no-op.
    fn compact(&self, _options: Value) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// `ctx.navigateTree(target, options)` -> `{cancelled}`; the default runner action reports `{cancelled:false}`.
    fn navigate_tree<'a>(
        &'a self,
        _target: &'a str,
        _options: Value,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async { Ok(json!({"cancelled": false})) })
    }
    /// Complete one auxiliary model request through the core's catalog and auth.
    fn model_complete(&self, _request: Value) -> BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move { Err("model_complete is not supported by the core".into()) })
    }
    /// `ctx.runEphemeralTurn(options)` -> `{replyText, assistantMessage}`: one tool-less side turn
    /// that never enters the session history. The default is an explicit rejection, so an
    /// extension sees an error instead of a silent empty reply.
    fn run_ephemeral_turn(&self, _request: Value) -> BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move { Err("runEphemeralTurn is not supported by the core".into()) })
    }
    /// `pi.setModel(model)`; `false` = model not applied (no UI/credentials to switch with).
    fn set_model(&self, _model: Value) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
    /// `pi.setActiveTools(names)`.
    fn set_active_tools(&self, _names: Vec<String>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// `pi.setSessionName(name)`.
    fn set_session_name(&self, _name: String) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// `ctx.resolvePlanReview(reviewId, choice, {feedback, role})`; `false` when that review is not
    /// open or the answer is refused. Without a plan review (the runner default) always `false`.
    fn resolve_plan_review<'a>(
        &'a self,
        _review_id: &'a str,
        _choice: &'a str,
        _feedback: Option<&'a str>,
        _role: Option<&'a str>,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
}

/// `hasUI=false` answers only.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeadlessResponder;

impl HostResponder for HeadlessResponder {}

/// Compute the reply for one host request.
///
/// # Errors
/// Unknown methods and responder failures become the JSON-RPC error message.
pub async fn answer(
    responder: &dyn HostResponder,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let s = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or_default();
    match method {
        "ui_confirm" => Ok(json!(responder.ui_confirm(s("title"), s("message")).await)),
        "ui_select" => {
            let options = params
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Ok(responder
                .ui_select(s("title"), options)
                .await
                .map_or(Value::Null, Value::String))
        }
        "ui_input" => Ok(responder
            .ui_input(
                s("title"),
                params.get("placeholder").and_then(Value::as_str),
            )
            .await
            .map_or(Value::Null, Value::String)),
        "ui_editor" => Ok(responder
            .ui_editor(s("title"), params.get("prefill").and_then(Value::as_str))
            .await
            .map_or(Value::Null, Value::String)),
        "compact" => responder
            .compact(params.get("options").cloned().unwrap_or(Value::Null))
            .await
            .map(|()| Value::Null),
        "navigate_tree" => {
            responder
                .navigate_tree(
                    s("target"),
                    params.get("options").cloned().unwrap_or(Value::Null),
                )
                .await
        }
        "set_model" => Ok(json!(
            responder
                .set_model(params.get("model").cloned().unwrap_or(Value::Null))
                .await
        )),
        "set_active_tools" => {
            let names = params
                .get("names")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            responder
                .set_active_tools(names)
                .await
                .map(|()| Value::Null)
        }
        "set_session_name" => responder
            .set_session_name(s("name").to_owned())
            .await
            .map(|()| Value::Null),
        "model_complete" => responder.model_complete(params.clone()).await,
        "run_ephemeral_turn" => responder.run_ephemeral_turn(params.clone()).await,
        "resolve_plan_review" => {
            let input = params.get("input");
            let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
            Ok(json!(
                responder
                    .resolve_plan_review(
                        s("reviewId"),
                        s("choice"),
                        field("feedback"),
                        field("role"),
                    )
                    .await
            ))
        }
        other => Err(format!("{other} is not supported by the core")),
    }
}

/// Serve the host's requests with `responder` (each in its own task, so a slow `compact` does not stall
/// the rest) and hand every notification (`append_entry`, `send_message`, ...) to the returned channel.
/// Must run inside a tokio runtime; ends when the host closes.
///
/// `serve` only holds a weak handle: dropping the last [`ExtHost`] handle kills
/// the child process (`kill_on_drop`), after which the reader task closes the
/// message channel and this loop ends. A strong handle here would keep the host
/// (and its bun process) alive forever once the loop is running.
#[must_use]
pub fn serve(
    host: &Arc<ExtHost>,
    mut incoming: mpsc::UnboundedReceiver<HostMessage>,
    responder: Arc<dyn HostResponder>,
) -> mpsc::UnboundedReceiver<HostMessage> {
    let (tx, rx) = mpsc::unbounded_channel();
    let weak = Arc::downgrade(host);
    tokio::spawn(async move {
        while let Some(msg) = incoming.recv().await {
            match msg {
                HostMessage::Request { id, method, params } => {
                    let Some(host) = weak.upgrade() else {
                        break;
                    };
                    let responder = Arc::clone(&responder);
                    tokio::spawn(async move {
                        let reply = answer(responder.as_ref(), &method, &params).await;
                        let _ = host.respond(id, reply).await;
                    });
                }
                note @ HostMessage::Notification { .. } => {
                    if tx.send(note).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `block` needs an explicit `true`; `reason`/`note` are plain strings.
    #[test]
    fn subagent_spawn_outcome_reads_block_and_reason() {
        let blocked = BeforeSubagentSpawnOutcome::from_value(&json!({
            "block": true,
            "reason": "quota",
            "note": "routed"
        }));
        assert!(blocked.block);
        assert_eq!(blocked.reason.as_deref(), Some("quota"));
        assert_eq!(blocked.note.as_deref(), Some("routed"));
        assert_eq!(blocked.model, None);

        let plain = BeforeSubagentSpawnOutcome::from_value(&json!({"block": false}));
        assert!(!plain.block);
        assert_eq!((plain.reason, plain.model, plain.note), (None, None, None));
        assert!(!BeforeSubagentSpawnOutcome::from_value(&json!({"block": "yes"})).block);
    }

    /// `model` is a selector string or an array of them, comma-separated entries flatten, blanks
    /// drop; an empty result means "no replacement".
    #[test]
    fn subagent_spawn_outcome_normalizes_model_patterns() {
        let one = BeforeSubagentSpawnOutcome::from_value(&json!({"model": "a/b:high"}));
        assert_eq!(one.model, Some(vec!["a/b:high".to_string()]));

        let many = BeforeSubagentSpawnOutcome::from_value(&json!({
            "model": ["a/b, c/d", " e/f ", ""]
        }));
        assert_eq!(
            many.model,
            Some(vec![
                "a/b".to_string(),
                "c/d".to_string(),
                "e/f".to_string()
            ])
        );

        for empty in [json!({"model": ""}), json!({"model": []}), json!({})] {
            assert_eq!(
                BeforeSubagentSpawnOutcome::from_value(&empty).model,
                None,
                "{empty}"
            );
        }
    }
}
