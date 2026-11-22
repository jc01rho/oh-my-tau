//! The advisor (TS `session/session-advisors.ts`, `advisor/*`, `/advisor` in
//! `builtin-collaboration.ts`) — the single-advisor core.
//!
//! A second agent on the `advisor` model role watches the primary session. At
//! each eligible primary update (`advisor.reviewMode`, every
//! `advisor.reviewInterval`-th one) it receives the transcript delta —
//! thoughts, tool calls and results included — and may call `advise` with one
//! terse note (`nit` / `concern` / `blocker`). Notes reach the primary as
//! `<advisory>` blocks: steered in while the primary is still working (after
//! a concern interrupts, further non-blockers wait out `advisor.immuneTurns`
//! primary turns), otherwise held for the next prompt — or, at or above
//! `advisor.continueSeverity`, delivered by continuing the run.
//!
//! Not ported: `WATCHDOG.yml` rosters (several named advisors), advisor
//! transcripts on disk with resumed spend, `advisor.syncBacklog` catch-up
//! waits, tool-result eviction, the loop guard / watchdog / emission guard,
//! and the `/advisor configure` editor.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::types::{
    AgentMessage, AgentTool, AgentToolResult, AgentToolUpdateCallback, AssistantContent,
    CustomAgentMessage, Message, ToolResultContent, UserContent,
};
use pa_agent::BoxFut;
use serde_json::{json, Value};

use crate::adapter::{json_round_trip, ProviderTarget};
use crate::rpc::slash::Outcome;
use crate::runtime::Runtime;

const SYSTEM_PROMPT: &str = include_str!("advisor_system.md");
const ADVISE_DESCRIPTION: &str = include_str!("advisor_advise_tool.md");
/// TS `ADVISOR_GUIDANCE`: how the watched agent should weigh a note.
const GUIDANCE: &str = "weigh, don't blindly obey";
/// TS `delta-split.ts` heading for a delta the primary is still working on.
const IN_PROGRESS: &str = "\n\n---\n\n[in progress — more steps follow]";
/// Read-only tools the advisor may verify with (TS default grant).
const ADVISOR_TOOLS: [&str; 4] = ["read", "grep", "find", "glob"];
/// Longest tool-result excerpt sent to the advisor.
const RESULT_EXCERPT: usize = 4_000;

const NO_MODEL: &str = "Advisor setting enabled, but no model is assigned to the 'advisor' role.";
const NO_MODEL_STATUS: &str =
    "Advisor setting is enabled, but no model is assigned to the 'advisor' role.";
const USAGE: &str = "Usage: /advisor [on|off|status|dump [raw]|configure]";

/// Note weight (TS `AdvisorSeverity`), ordered `nit < concern < blocker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Nit,
    Concern,
    Blocker,
}

impl Severity {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "nit" => Some(Self::Nit),
            "concern" => Some(Self::Concern),
            "blocker" => Some(Self::Blocker),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Nit => "nit",
            Self::Concern => "concern",
            Self::Blocker => "blocker",
        }
    }
}

/// One `advise` call (TS `AdvisorNote`); a missing severity is a plain nit.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub note: String,
    pub severity: Option<Severity>,
}

impl Note {
    fn weight(&self) -> Severity {
        self.severity.unwrap_or(Severity::Nit)
    }
}

/// TS `formatAdvisorBatchContent`: one `<advisory>` element per note.
#[must_use]
pub fn format_batch(notes: &[Note]) -> String {
    notes
        .iter()
        .map(|n| {
            let severity = n
                .severity
                .map(|s| format!(" severity=\"{}\"", s.as_str()))
                .unwrap_or_default();
            format!(
                "<advisory{severity} guidance=\"{GUIDANCE}\">\n{}\n</advisory>",
                escape_xml(&n.note)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The advisor system prompt with the per-update note budget filled in.
#[must_use]
pub fn system_prompt(max_notes: u64) -> String {
    SYSTEM_PROMPT.replace(
        "{{#if max_notes_per_update}}{{max_notes_per_update}}{{else}}4{{/if}}",
        &max_notes.to_string(),
    )
}

/// Settings the review cadence and delivery read live each boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tuning {
    enabled: bool,
    every_turn: bool,
    interval: u64,
    max_notes: u64,
    immune_turns: u64,
    continue_at: Severity,
}

impl Tuning {
    pub(crate) fn load(settings: &omp_settings::Settings) -> Self {
        let count = |id: &str, fallback: u64| {
            settings
                .get_number(id)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map_or(fallback, |v| v as u64)
        };
        Self {
            enabled: settings.get_bool("advisor.enabled").unwrap_or(false),
            every_turn: settings.get_string("advisor.reviewMode").as_deref() != Some("agent-end"),
            interval: count("advisor.reviewInterval", 1).max(1),
            max_notes: count("advisor.maxNotesPerUpdate", 4),
            immune_turns: count("advisor.immuneTurns", 3),
            continue_at: settings
                .get_string("advisor.continueSeverity")
                .and_then(|s| Severity::parse(&s))
                .unwrap_or(Severity::Concern),
        }
    }
}

/// The live advisor agent and the notes its `advise` calls recorded.
pub(crate) struct Live {
    agent: Agent,
    label: String,
    notes: Arc<Mutex<Vec<Note>>>,
}

#[derive(Default)]
struct State {
    /// Session override of `advisor.enabled` (`/advisor on|off`, `--advisor`).
    enabled: Option<bool>,
    live: Option<Arc<Live>>,
    /// Primary transcript messages already sent to the advisor.
    reviewed: usize,
    /// Eligible primary updates seen (the `advisor.reviewInterval` cadence).
    updates: u64,
    /// Primary turns left before a non-blocker may interrupt again.
    cooldown: u64,
    /// A review is running; the next boundary picks up the delta it missed.
    reviewing: bool,
    /// Notes waiting for the primary's next prompt.
    pending: Vec<Note>,
}

/// `--advisor` seen at startup: the session's advisor starts enabled.
static ENABLED_AT_STARTUP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `--advisor`: enable the advisor for this session at startup.
pub fn enable_at_startup() {
    ENABLED_AT_STARTUP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The session's advisor state, owned by the `Runtime`.
pub struct Advisor {
    state: Mutex<State>,
}

impl Default for Advisor {
    fn default() -> Self {
        let enabled = ENABLED_AT_STARTUP
            .load(std::sync::atomic::Ordering::Relaxed)
            .then_some(true);
        Self {
            state: Mutex::new(State {
                enabled,
                ..State::default()
            }),
        }
    }
}

impl Advisor {
    fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut guard: MutexGuard<'_, State> =
            self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

/// A fresh session starts the advisor over (TS `resetSessionState`): the
/// review cursor, cadence, cooldown and held notes go; the override and the
/// advisor agent stay, with the agent's conversation cleared.
pub async fn reset_for_new_session(rt: &Runtime) {
    let live = rt.advisor.with_state(|s| {
        s.reviewed = 0;
        s.updates = 0;
        s.cooldown = 0;
        s.pending.clear();
        s.live.clone()
    });
    if let Some(live) = live {
        live.agent.abort();
        live.agent.wait_for_idle().await;
        live.agent.reset().await;
    }
}

fn enabled(rt: &Runtime, tuning: &Tuning) -> bool {
    rt.advisor
        .with_state(|s| s.enabled)
        .unwrap_or(tuning.enabled)
}

/// Start the advisor agent when enabled and a model resolves for the
/// `advisor` role (TS `#buildAdvisorRuntime`); `None` otherwise.
async fn ensure_live(rt: &Arc<Runtime>, tuning: &Tuning) -> Option<Arc<Live>> {
    if !enabled(rt, tuning) {
        return None;
    }
    if let Some(live) = rt.advisor.with_state(|s| s.live.clone()) {
        return Some(live);
    }
    let resolved = omp_providers::resolve("advisor").ok()?;
    let agent_model: pa_agent::types::Model = json_round_trip(&resolved.model)?;
    let label = format!("{}/{}", resolved.provider, resolved.id);
    let target = Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
        api_key: resolved.api_key.clone(),
        model: resolved.model.clone(),
        service_tier: None,
        headers: (!resolved.headers.is_empty()).then(|| resolved.headers.iter().cloned().collect()),
        credential_id: resolved.credential_id,
    })));
    let thinking = resolved.thinking.and_then(|t| json_round_trip(&t));
    Some(
        install_live(
            rt,
            agent_model,
            label,
            crate::adapter::switchable_stream_fn(target),
            thinking,
            tuning.max_notes,
        )
        .await,
    )
}

/// Build the advisor agent over `stream_fn` (its `advise` tool plus the
/// session's read-only tools) and make it the session's live advisor.
pub(crate) async fn install_live(
    rt: &Arc<Runtime>,
    model: pa_agent::types::Model,
    label: String,
    stream_fn: pa_agent::stream::StreamFn,
    thinking: Option<pa_agent::types::ThinkingLevel>,
    max_notes: u64,
) -> Arc<Live> {
    let notes = Arc::new(Mutex::new(Vec::new()));
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(AdviseTool {
        notes: notes.clone(),
        budget: max_notes,
        schema: advise_schema(),
    })];
    tools.extend(
        rt.agent
            .tools()
            .await
            .into_iter()
            .filter(|t| ADVISOR_TOOLS.contains(&t.name())),
    );
    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some(system_prompt(max_notes)),
            model: Some(model),
            thinking_level: thinking,
            tools: Some(tools),
            messages: None,
        },
        stream_fn: Some(stream_fn),
        ..AgentOptions::default()
    });
    let live = Arc::new(Live {
        agent,
        label,
        notes,
    });
    rt.advisor.with_state(|s| s.live = Some(live.clone()));
    live
}

fn stop(rt: &Runtime) {
    let live = rt.advisor.with_state(|s| s.live.take());
    if let Some(live) = live {
        live.agent.abort();
    }
}

/// The primary finished a turn (TS `onPrimaryTurnEnd`): advance the cadence
/// and, when this update is scheduled, review its delta in the background.
/// `terminal` = the turn ended without tool calls, so the run is yielding.
pub fn on_primary_turn_end(rt: &Arc<Runtime>, terminal: bool) {
    let tuning = Tuning::load(&omp_settings::Settings::load(&rt.cwd));
    let scheduled = rt.advisor.with_state(|s| {
        s.cooldown = s.cooldown.saturating_sub(1);
        let eligible = tuning.every_turn || terminal;
        if eligible {
            s.updates += 1;
        }
        eligible && s.updates % tuning.interval == 0 && !s.reviewing
    });
    if !scheduled || !enabled(rt, &tuning) {
        return;
    }
    let rt = rt.clone();
    tokio::spawn(async move {
        let Some(live) = ensure_live(&rt, &tuning).await else {
            return;
        };
        review(&rt, &live, &tuning, terminal).await;
    });
}

pub(crate) async fn review(rt: &Arc<Runtime>, live: &Arc<Live>, tuning: &Tuning, terminal: bool) {
    let messages = rt.agent.messages_snapshot().await.into_vec();
    let Some(from) = rt.advisor.with_state(|s| {
        if s.reviewing {
            return None;
        }
        s.reviewing = true;
        let from = s.reviewed.min(messages.len());
        s.reviewed = messages.len();
        Some(from)
    }) else {
        return;
    };
    let mut delta = render_delta(&messages[from..]);
    if delta.trim().is_empty() {
        rt.advisor.with_state(|s| s.reviewing = false);
        return;
    }
    if !terminal {
        delta.push_str(IN_PROGRESS);
    }
    live.notes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    if live
        .agent
        .prompt_until_accepted(delta.as_str())
        .await
        .is_ok()
    {
        live.agent.wait_for_idle().await;
    }
    let notes = std::mem::take(&mut *live.notes.lock().unwrap_or_else(PoisonError::into_inner));
    rt.advisor.with_state(|s| s.reviewing = false);
    deliver(rt, notes, tuning, terminal).await;
}

/// Route a review's notes (TS `#routeAdvice`): steer while the primary works
/// (non-blockers wait out the cooldown a concern started), else hold them for
/// the next prompt, continuing the run for notes at `continueSeverity`.
async fn deliver(rt: &Arc<Runtime>, notes: Vec<Note>, tuning: &Tuning, terminal: bool) {
    if notes.is_empty() {
        return;
    }
    if !terminal && rt.agent.is_streaming().await {
        let (now, later): (Vec<Note>, Vec<Note>) = rt.advisor.with_state(|s| {
            notes
                .into_iter()
                .partition(|n| n.weight() == Severity::Blocker || s.cooldown == 0)
        });
        rt.advisor.with_state(|s| {
            s.pending.extend(later);
            if now.iter().any(|n| n.weight() >= Severity::Concern) {
                s.cooldown = tuning.immune_turns;
            }
        });
        if !now.is_empty() {
            rt.agent.steer(advisory_message(&now));
        }
        return;
    }
    let (urgent, rest): (Vec<Note>, Vec<Note>) = notes
        .into_iter()
        .partition(|n| n.weight() >= tuning.continue_at);
    rt.advisor.with_state(|s| s.pending.extend(rest));
    if urgent.is_empty() {
        return;
    }
    let mut batch = take_pending(rt);
    batch.extend(urgent);
    let message = advisory_message(&batch);
    if rt.agent.is_streaming().await {
        rt.agent.steer(message);
    } else if let Err(e) = rt.agent.prompt_until_accepted(message).await {
        tracing::warn!("advisor continuation failed: {e:#}");
    }
}

fn take_pending(rt: &Runtime) -> Vec<Note> {
    rt.advisor.with_state(|s| std::mem::take(&mut s.pending))
}

/// Notes held for the primary's next prompt, as the message that carries
/// them ahead of it.
pub fn take_pending_message(rt: &Runtime) -> Option<AgentMessage> {
    let notes = take_pending(rt);
    (!notes.is_empty()).then(|| advisory_message(&notes))
}

/// The `<advisory>` batch as a custom message the model reads (TS advisor
/// notes ride as `custom` messages with the batch as their content).
fn advisory_message(notes: &[Note]) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage {
        role: "developer".to_string(),
        payload: json!({
            "customType": "advisor",
            "content": [{"type": "text", "text": format_batch(notes)}],
            "display": true,
            "attribution": "agent",
            "timestamp": pa_agent::now_ms(),
        }),
    })
}

/// The primary transcript slice the advisor reviews, as markdown: user and
/// assistant text, thinking, tool calls with their arguments, and tool
/// results (long ones excerpted with an `elided` marker).
#[must_use]
pub fn render_delta(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            AgentMessage::Standard(Message::User(user)) => {
                let text = match &user.content {
                    UserContent::Text(t) => t.clone(),
                    UserContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            pa_agent::types::UserPart::Text(t) => Some(t.text.as_str()),
                            pa_agent::types::UserPart::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                out.push_str(&format!("## User\n\n{text}\n\n"));
            }
            AgentMessage::Standard(Message::Assistant(assistant)) => {
                out.push_str("## Agent\n\n");
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) => out.push_str(&format!("{}\n\n", t.text)),
                        AssistantContent::Thinking(t) => {
                            out.push_str(&format!("_Thinking:_ {}\n\n", t.thinking));
                        }
                        AssistantContent::ToolCall(call) => out.push_str(&format!(
                            "**Tool call** `{}` {}\n\n",
                            call.name, call.arguments
                        )),
                    }
                }
            }
            AgentMessage::Standard(Message::ToolResult(result)) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let shown = if text.chars().count() > RESULT_EXCERPT {
                    let head: String = text.chars().take(RESULT_EXCERPT).collect();
                    format!("{head}\n[… elided …]")
                } else {
                    text
                };
                let error = if result.is_error { " (error)" } else { "" };
                out.push_str(&format!(
                    "**Tool result** `{}`{error}\n\n{shown}\n\n",
                    result.tool_name
                ));
            }
            _ => {}
        }
    }
    out
}

fn advise_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "note": {
                "type": "string",
                "description": "One concrete piece of advice for the agent you are watching. Terse, specific, actionable."
            },
            "severity": {
                "type": "string",
                "enum": ["nit", "concern", "blocker"],
                "description": "How strongly to weigh this. Omit for a plain nit."
            }
        },
        "required": ["note"]
    })
}

/// The advisor's `advise` tool (TS `advise-tool.ts`): records one note; past
/// the per-update budget only blockers are accepted.
struct AdviseTool {
    notes: Arc<Mutex<Vec<Note>>>,
    budget: u64,
    schema: Value,
}

impl AgentTool for AdviseTool {
    fn name(&self) -> &'static str {
        "advise"
    }
    fn description(&self) -> &str {
        ADVISE_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let note = params
                .get("note")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| anyhow::anyhow!("advise needs a non-empty note"))?
                .to_string();
            let severity = params
                .get("severity")
                .and_then(Value::as_str)
                .and_then(Severity::parse);
            let text = {
                let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
                let spent = notes
                    .iter()
                    .filter(|n| n.weight() != Severity::Blocker)
                    .count() as u64;
                if severity != Some(Severity::Blocker) && spent >= self.budget {
                    "Note budget for this update is spent; only blockers are delivered."
                } else {
                    notes.push(Note { note, severity });
                    "Noted."
                }
            };
            Ok(AgentToolResult {
                content: vec![ToolResultContent::text(text)],
                details: Value::Null,
                terminate: None,
                is_error: None,
            })
        })
    }
}

/// `/advisor [on|off|status|dump [raw]|configure]` (TS `handle`).
pub async fn command(rt: &Arc<Runtime>, args: &str) -> Outcome {
    let settings = omp_settings::Settings::load(&rt.cwd);
    let tuning = Tuning::load(&settings);
    let mut words = args.split_whitespace();
    let verb = words.next().unwrap_or("").to_lowercase();
    let rest = words.collect::<Vec<_>>().join(" ").to_lowercase();
    let text = match verb.as_str() {
        "" | "toggle" | "on" | "off" => {
            let on = match verb.as_str() {
                "on" => true,
                "off" => false,
                _ => !enabled(rt, &tuning),
            };
            rt.advisor.with_state(|s| s.enabled = Some(on));
            if on {
                if ensure_live(rt, &tuning).await.is_some() {
                    "Advisor enabled.".to_string()
                } else {
                    NO_MODEL.to_string()
                }
            } else {
                stop(rt);
                "Advisor disabled.".to_string()
            }
        }
        "status" => status(rt, &tuning).await,
        "dump" => match rt.advisor.with_state(|s| s.live.clone()) {
            Some(live) => dump(&live, rest == "raw").await,
            None => "Advisor is not active for this session.".to_string(),
        },
        "configure" => {
            "/advisor configure opens an interactive editor and is only available in the interactive TUI."
                .to_string()
        }
        _ => USAGE.to_string(),
    };
    rt.send(json!({"type": "command_output", "text": text}));
    Outcome::Handled {
        agent_invoked: false,
    }
}

/// TS `formatAdvisorStatus` for one advisor.
async fn status(rt: &Runtime, tuning: &Tuning) -> String {
    if !enabled(rt, tuning) {
        return "Advisor is disabled.".to_string();
    }
    let Some(live) = rt.advisor.with_state(|s| s.live.clone()) else {
        return NO_MODEL_STATUS.to_string();
    };
    let messages = live.agent.messages_snapshot().await.into_vec();
    let (mut input, mut output, mut cost, mut context) = (0u64, 0u64, 0f64, 0u64);
    for message in &messages {
        let Some(usage) = serde_json::to_value(message)
            .ok()
            .and_then(|v| v.get("usage").cloned())
        else {
            continue;
        };
        let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        input += n("input");
        output += n("output");
        cost += usage
            .get("cost")
            .and_then(|c| c.get("total"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        context = n("input") + n("cacheRead") + n("output");
    }
    format!(
        "Advisor is enabled ({}). Context: {context} tokens. Spend: {input} input, {output} output, ${cost:.4}.",
        live.label
    )
}

/// TS `formatAdvisorHistoryAsText`: the advisor's own transcript (compact
/// markdown, or with its system prompt and tools when `raw`).
async fn dump(live: &Live, raw: bool) -> String {
    let messages = live.agent.messages_snapshot().await.into_vec();
    let mut text = String::new();
    if raw {
        text.push_str(&format!(
            "# Advisor ({})\n\n## System prompt\n\n{}\n\n## Tools\n\n{}\n\n",
            live.label,
            live.agent.system_prompt().await,
            live.agent
                .tools()
                .await
                .iter()
                .map(|t| format!("- {}", t.name()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    text.push_str(&render_delta(&messages));
    if text.trim().is_empty() {
        "Advisor has not reviewed anything yet.".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(text: &str, severity: Option<Severity>) -> Note {
        Note {
            note: text.into(),
            severity,
        }
    }

    #[test]
    fn batches_render_like_ts() {
        let batch = format_batch(&[
            note("Run the failing test first", Some(Severity::Concern)),
            note("x < y & z", None),
        ]);
        assert_eq!(
            batch,
            "<advisory severity=\"concern\" guidance=\"weigh, don't blindly obey\">\nRun the failing test first\n</advisory>\n<advisory guidance=\"weigh, don't blindly obey\">\nx &lt; y &amp; z\n</advisory>"
        );
    }

    #[test]
    fn system_prompt_carries_the_note_budget() {
        let prompt = system_prompt(2);
        assert!(prompt.contains("max 2 non-blockers/update"));
        assert!(!prompt.contains("{{"));
    }

    #[tokio::test]
    async fn advise_records_notes_within_the_budget_blockers_exempt() {
        let notes = Arc::new(Mutex::new(Vec::new()));
        let tool = Arc::new(AdviseTool {
            notes: notes.clone(),
            budget: 1,
            schema: advise_schema(),
        });
        let call = |p: Value| {
            tool.clone()
                .execute("c".into(), p, AbortSignal::never(), Arc::new(|_| {}))
        };
        call(json!({"note": "first", "severity": "concern"}))
            .await
            .unwrap();
        let over = call(json!({"note": "second"})).await.unwrap();
        let ToolResultContent::Text(t) = &over.content[0] else {
            panic!("text");
        };
        assert!(t.text.starts_with("Note budget"));
        call(json!({"note": "stop now", "severity": "blocker"}))
            .await
            .unwrap();
        assert!(call(json!({"note": "  "})).await.is_err());
        let recorded: Vec<(String, Option<Severity>)> = notes
            .lock()
            .unwrap()
            .iter()
            .map(|n| (n.note.clone(), n.severity))
            .collect();
        assert_eq!(
            recorded,
            vec![
                ("first".into(), Some(Severity::Concern)),
                ("stop now".into(), Some(Severity::Blocker))
            ]
        );
    }

    #[test]
    fn delta_renders_calls_results_and_elides_long_output() {
        let messages: Vec<AgentMessage> = serde_json::from_value(json!([
            {"role": "user", "content": "fix the build", "timestamp": 1},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "check cargo first"},
                {"type": "toolCall", "id": "t1", "name": "bash", "arguments": {"command": "cargo build"}}
            ], "api": "a", "provider": "p", "model": "m", "stopReason": "toolUse", "timestamp": 2,
             "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}},
            {"role": "toolResult", "toolCallId": "t1", "toolName": "bash",
             "content": [{"type": "text", "text": "x".repeat(RESULT_EXCERPT + 10)}],
             "isError": true, "timestamp": 3}
        ]))
        .unwrap();
        let delta = render_delta(&messages);
        assert!(delta.contains("## User\n\nfix the build"));
        assert!(delta.contains("_Thinking:_ check cargo first"));
        assert!(delta.contains("**Tool call** `bash` {\"command\":\"cargo build\"}"));
        assert!(delta.contains("**Tool result** `bash` (error)"));
        assert!(delta.contains("[… elided …]"));
    }

    #[test]
    fn tuning_defaults_follow_upstream() {
        let dir = std::env::temp_dir().join(format!("omp-advisor-tuning-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let settings = omp_settings::Settings::load_from(None, &dir);
        let tuning = Tuning::load(&settings);
        assert_eq!(
            tuning,
            Tuning {
                enabled: false,
                every_turn: true,
                interval: 1,
                max_notes: 4,
                immune_turns: 3,
                continue_at: Severity::Concern,
            }
        );
    }
}
