//! `/prewalk` and the `--prewalk` / `--no-prewalk` / `--prewalk-into` startup
//! flags (TS `session/prewalk.ts` `PrewalkCoordinator`, `/prewalk` in
//! `builtin-modes.ts`): a one-shot model handoff. The session plans on the
//! current model; the first `edit`/`write` that changes files — once the todo
//! list exists, when the `todo` tool is active — switches to the `into` model
//! before the next model call, and a checklist reminder follows.
//!
//! Ported: arm / disarm / restart with upstream's messages and checks, the
//! deep-plan and continue nudges, the todo gate, the implementation-action
//! test (`touchesNoFiles`, `xd://` write tier), the no-op disarm, startup
//! arming. Not ported: `prewalk.auto` (task classification) and plan-yolo;
//! a delivered plan nudge stays in the transcript (TS scrubs it).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pa_agent::types::{AgentMessage, CustomAgentMessage, ThinkingLevel, ToolResultMessage};
use serde_json::{json, Value};

use crate::rpc::slash::Outcome;
use crate::runtime::Runtime;

const PLAN_PROMPT: &str = include_str!("prewalk_plan.md");
const CONTINUE_PROMPT: &str = "Continue task now; do not end turn here.";
const CHECKLIST_PROMPT: &str = include_str!("prewalk_checklist.md");
const USAGE: &str = "Usage: /prewalk [into] | restart [from] [into] | off";

/// `--prewalk` / `--prewalk-into` / `prewalk.enabled`: the selector a
/// configured arm takes at the session's first completed turn.
static STARTUP_INTO: Mutex<Option<String>> = Mutex::new(None);

/// Held for writing while a turn-end advance decides and performs a switch;
/// the main agent's model calls take it for reading first
/// ([`wait_for_handoff`]), so the call after the implementation action runs
/// on the new model (TS awaits the switch inside the turn-end callback).
static HANDOFF_GATE: std::sync::LazyLock<Arc<tokio::sync::RwLock<()>>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::RwLock::new(())));

/// Wait for a pending turn-end advance (main agent model calls only).
pub async fn wait_for_handoff() {
    drop(HANDOFF_GATE.read().await);
}

/// Arm the session's prewalk at startup (`into` = the target selector).
pub fn arm_at_startup(into: String) {
    *STARTUP_INTO.lock().unwrap_or_else(PoisonError::into_inner) = Some(into);
}

/// A resolved handoff target.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub provider: String,
    pub id: String,
    pub thinking: Option<ThinkingLevel>,
}

impl Target {
    fn label(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

#[derive(Default)]
struct State {
    armed: Option<Target>,
    plan_injected: bool,
    continue_pending: bool,
    todo_seen: bool,
    /// A configured (startup) arm still to resolve at the first turn end.
    startup: Option<String>,
}

/// Reset for a fresh session (TS `resetForNewSession`): the pending arm
/// goes; with `prewalk.enabled` the configured default arms again at the
/// first completed turn.
pub fn reset_for_new_session(rt: &Runtime) {
    let settings = omp_settings::Settings::load(&rt.cwd);
    let startup = (settings.get_bool("prewalk.enabled") == Some(true)).then(|| {
        settings
            .get_string("prewalk.into")
            .unwrap_or_else(|| "@smol".into())
    });
    *rt.prewalk.lock() = State {
        startup,
        ..State::default()
    };
}

/// The session's prewalk, owned by the `Runtime`.
pub struct Prewalk {
    state: Mutex<State>,
}

impl Default for Prewalk {
    fn default() -> Self {
        let startup = STARTUP_INTO
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Self {
            state: Mutex::new(State {
                startup,
                ..State::default()
            }),
        }
    }
}

impl Prewalk {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The armed target, if the one-way switch is pending.
    pub fn armed(&self) -> Option<Target> {
        self.lock().armed.clone()
    }
}

/// TS `resolveSessionModelSelector` over the session's model resolver:
/// `@role`, a bare role, or `provider/id[:level]`.
///
/// # Errors
/// Upstream's messages: unknown model, or no API key for it.
pub async fn resolve_selector(rt: &Runtime, selector: &str) -> Result<Target, String> {
    let reference = selector.trim().trim_start_matches('@');
    let resolved = rt
        .resolve_model_ref(reference)
        .await
        .map_err(|_| format!("Model \"{}\" not found", selector.trim()))?;
    if resolved.api_key.is_none() && resolved.credential_id.is_none() {
        return Err(format!(
            "No API key for {}/{}",
            resolved.provider, resolved.id
        ));
    }
    Ok(Target {
        provider: resolved.provider,
        id: resolved.id,
        thinking: resolved
            .thinking
            .and_then(|t| crate::adapter::json_round_trip(&t)),
    })
}

/// TS `prewalkWouldBeNoop`: the session already runs the target model and
/// thinking level (an unspecified target level keeps the current one).
async fn is_noop(rt: &Runtime, target: &Target) -> bool {
    let current = rt.live_provider_id();
    let same_model = current
        .as_ref()
        .is_some_and(|(p, i)| *p == target.provider && *i == target.id);
    let level = rt.agent.thinking_level().await;
    same_model && target.thinking.is_none_or(|t| t == level)
}

fn nudge(custom_type: &str, text: &str) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage {
        role: "custom".to_string(),
        payload: json!({
            "customType": custom_type,
            "content": text,
            "display": false,
            "attribution": "agent",
            "timestamp": pa_agent::now_ms(),
        }),
    })
}

/// TS `arm`: arm an explicit request; an existing arm stays (true when it is
/// the same target), a no-op target disarms with a notice.
async fn arm(rt: &Arc<Runtime>, target: Target) -> bool {
    if let Some(active) = rt.prewalk.armed() {
        rt.emit_notice(
            "info",
            &format!(
                "Prewalk: already armed for {}, waiting for the first edit/write.",
                active.label()
            ),
            Some("prewalk"),
        );
        return active == target;
    }
    if is_noop(rt, &target).await {
        rt.emit_notice(
            "info",
            &format!(
                "Prewalk: target {} already matches the active model and thinking level; nothing to switch.",
                target.label()
            ),
            Some("prewalk"),
        );
        return false;
    }
    rt.emit_notice(
        "info",
        &format!(
            "Prewalk: armed for {} — will switch at the first edit/write once the todo list exists.",
            target.label()
        ),
        Some("prewalk"),
    );
    {
        let mut s = rt.prewalk.lock();
        s.armed = Some(target);
        s.plan_injected = true;
        s.continue_pending = true;
        s.todo_seen = false;
    }
    rt.agent.steer(nudge("prewalk-plan", PLAN_PROMPT));
    true
}

/// TS `disarm`; returns whether an arm was pending.
fn disarm(rt: &Runtime) -> bool {
    let active = {
        let mut s = rt.prewalk.lock();
        let active = s.armed.take();
        s.plan_injected = false;
        s.continue_pending = false;
        s.todo_seen = false;
        s.startup = None;
        active
    };
    if let Some(active) = &active {
        rt.emit_notice(
            "info",
            &format!(
                "Prewalk: disarmed; staying on the active model instead of switching to {}.",
                active.label()
            ),
            Some("prewalk"),
        );
    }
    active.is_some()
}

/// Switch the session to `target` now (TS `setModelTemporary`): the provider
/// slot the next model call reads is replaced before this returns, the model
/// change is journaled and the thinking level applied.
async fn switch_to(rt: &Arc<Runtime>, target: &Target) -> Result<(), String> {
    rt.set_model(&target.provider, &target.id)
        .await
        .map_err(|e| e.message)?;
    if let Some(level) = target.thinking {
        rt.set_thinking_level(level).await;
    }
    Ok(())
}

/// TS `isPrewalkImplementationAction`: a direct `edit`/`write` that touched
/// files, or an `xd://` dispatch through `write` that resolved to the write
/// tier and is not marked read-only.
#[must_use]
pub fn is_implementation_action(result: &ToolResultMessage) -> bool {
    if !matches!(result.tool_name.as_str(), "edit" | "write") || result.is_error {
        return false;
    }
    let Some(details) = result.details.as_ref().filter(|d| d.is_object()) else {
        return true;
    };
    if details.get("touchesNoFiles") == Some(&Value::Bool(true)) {
        return false;
    }
    match details.get("xdev") {
        None | Some(Value::Null | Value::Bool(false)) => true,
        Some(xdev) => {
            xdev.get("tier").and_then(Value::as_str) == Some("write")
                && xdev.get("readOnlyHint") != Some(&Value::Bool(true))
        }
    }
}

/// TS `advanceAtTurnEnd`, run at each completed primary turn: resolve a
/// startup arm, then nudge toward the plan, keep the run going, and switch at
/// the first implementation action once the todo gate is open.
pub fn on_turn_end(rt: &Arc<Runtime>, tool_results: &[ToolResultMessage]) {
    let startup = rt.prewalk.lock().startup.take();
    let armed = rt.prewalk.armed();
    if startup.is_none() && armed.is_none() {
        return;
    }
    let tool_results = tool_results.to_vec();
    let rt = rt.clone();
    let gate = HANDOFF_GATE.clone().try_write_owned().ok();
    tokio::spawn(async move {
        let _gate = gate;
        if let Some(selector) = startup {
            match resolve_selector(&rt, &selector).await {
                Ok(target) if rt.prewalk.armed().is_none() && !is_noop(&rt, &target).await => {
                    rt.emit_notice(
                        "info",
                        &format!(
                            "Prewalk: armed for {} — will switch at the first edit/write once the todo list exists.",
                            target.label()
                        ),
                        Some("prewalk"),
                    );
                    let mut s = rt.prewalk.lock();
                    s.armed = Some(target);
                }
                Ok(_) => {}
                Err(e) => rt.emit_notice("warning", &format!("Prewalk: {e}"), Some("prewalk")),
            }
        }
        advance(&rt, &tool_results).await;
    });
}

pub(crate) async fn advance(rt: &Arc<Runtime>, tool_results: &[ToolResultMessage]) {
    let Some(target) = rt.prewalk.armed() else {
        return;
    };
    if is_noop(rt, &target).await {
        rt.prewalk.lock().armed = None;
        rt.emit_notice(
            "info",
            &format!(
                "Prewalk: target {} already matches the active model and thinking level; nothing to switch.",
                target.label()
            ),
            Some("prewalk"),
        );
        return;
    }
    let todo_active = rt.agent.tools().await.iter().any(|t| t.name() == "todo");
    let action = {
        let mut s = rt.prewalk.lock();
        if tool_results
            .iter()
            .any(|r| r.tool_name == "todo" && !r.is_error)
        {
            s.todo_seen = true;
        }
        if s.plan_injected && !tool_results.is_empty() {
            s.continue_pending = true;
        } else if s.continue_pending {
            s.continue_pending = false;
            rt.agent.steer(nudge("prewalk-continue", CONTINUE_PROMPT));
        }
        let gate_open = s.todo_seen || !todo_active;
        let action = gate_open
            .then(|| tool_results.iter().find(|r| is_implementation_action(r)))
            .flatten()
            .map(|r| r.tool_name.clone());
        if action.is_none() && !s.plan_injected {
            s.plan_injected = true;
            s.continue_pending = true;
            rt.agent.steer(nudge("prewalk-plan", PLAN_PROMPT));
            drop(s);
            rt.emit_notice(
                "info",
                "Prewalk: injected deep-plan nudge.",
                Some("prewalk"),
            );
            return;
        }
        action
    };
    let Some(tool) = action else {
        return;
    };
    *rt.prewalk.lock() = State::default();
    match switch_to(rt, &target).await {
        Ok(()) => {
            rt.emit_notice(
                "info",
                &format!(
                    "Prewalk: switched to {} after first {tool} call.",
                    target.label()
                ),
                Some("prewalk"),
            );
            rt.agent.steer(nudge("prewalk-checklist", CHECKLIST_PROMPT));
        }
        Err(e) => rt.emit_notice(
            "warning",
            &format!("Prewalk: could not switch to {}: {e}", target.label()),
            Some("prewalk"),
        ),
    }
}

/// `/prewalk [into] | restart [from] [into] | off` (TS `handle`).
pub async fn command(rt: &Arc<Runtime>, args: &str) -> Outcome {
    let words: Vec<&str> = args.split_whitespace().collect();
    let keyword = words.first().map(|w| w.to_lowercase());
    let restart = keyword.as_deref() == Some("restart");
    let off = keyword.as_deref() == Some("off");
    let rest: &[&str] = if restart || off { &words[1..] } else { &words };
    let limit = if restart {
        2
    } else if off {
        0
    } else {
        1
    };
    let text = if rest.len() > limit {
        USAGE.to_string()
    } else if off {
        if disarm(rt) {
            "Prewalk off: canceled this session's pending handoff; keeping the active model."
                .to_string()
        } else {
            "Prewalk already off for this session; keeping the active model.".to_string()
        }
    } else {
        let settings = omp_settings::Settings::load(&rt.cwd);
        let into = (if restart { rest.get(1) } else { rest.first() })
            .map(ToString::to_string)
            .or_else(|| settings.get_string("prewalk.into"))
            .unwrap_or_else(|| "@smol".to_string());
        match resolve_selector(rt, &into).await {
            Err(e) => e,
            Ok(target) => {
                let target_label = format!("{into} ({})", target.label());
                if restart {
                    let from = rest
                        .first()
                        .map(ToString::to_string)
                        .or_else(|| settings.get_string("prewalk.from"))
                        .unwrap_or_else(|| "@default".to_string());
                    match resolve_selector(rt, &from).await {
                        Err(e) => e,
                        Ok(source) => {
                            restart_with(rt, &from, source, &into, target, &target_label).await
                        }
                    }
                } else if arm(rt, target.clone()).await {
                    format!(
                        "Prewalk on: switching to {} at the next edit/write (todo-gated).",
                        target.label()
                    )
                } else {
                    String::new()
                }
            }
        }
    };
    if !text.is_empty() {
        rt.send(json!({"type": "command_output", "text": text}));
    }
    Outcome::Handled {
        agent_invoked: false,
    }
}

/// TS `restartPrewalk`: return to the planning model and re-arm, unless a
/// different arm is pending (that arm's notice is the answer).
async fn restart_with(
    rt: &Arc<Runtime>,
    from: &str,
    source: Target,
    into: &str,
    target: Target,
    target_label: &str,
) -> String {
    if rt.prewalk.armed().is_some_and(|active| active != target) {
        arm(rt, target).await;
        return String::new();
    }
    if let Err(e) = switch_to(rt, &source).await {
        return format!("Prewalk: could not restore {}: {e}", source.label());
    }
    let source_label = format!("{from} ({})", source.label());
    let armed = rt.prewalk.armed().is_some() || arm(rt, target).await;
    if armed {
        format!(
            "Prewalk restarted: using {source_label} for planning, then switching to {target_label} at the next edit/write (todo-gated)."
        )
    } else {
        format!(
            "Prewalk reset: using {source_label}; {into} resolves to the same model and thinking level, so no handoff was armed."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(tool: &str, details: Value, is_error: bool) -> ToolResultMessage {
        serde_json::from_value(json!({
            "role": "toolResult", "toolCallId": "t", "toolName": tool,
            "content": [], "details": details, "isError": is_error, "timestamp": 0
        }))
        .unwrap()
    }

    #[test]
    fn implementation_actions_follow_upstream() {
        assert!(is_implementation_action(&result(
            "edit",
            Value::Null,
            false
        )));
        assert!(is_implementation_action(&result(
            "write",
            json!({"path": "a"}),
            false
        )));
        assert!(!is_implementation_action(&result(
            "write",
            json!({"touchesNoFiles": true}),
            false
        )));
        assert!(!is_implementation_action(&result(
            "read",
            Value::Null,
            false
        )));
        assert!(!is_implementation_action(&result(
            "edit",
            Value::Null,
            true
        )));
        assert!(is_implementation_action(&result(
            "write",
            json!({"xdev": {"tier": "write"}}),
            false
        )));
        assert!(!is_implementation_action(&result(
            "write",
            json!({"xdev": {"tier": "read"}}),
            false
        )));
        assert!(!is_implementation_action(&result(
            "write",
            json!({"xdev": {"tier": "write", "readOnlyHint": true}}),
            false
        )));
    }

    #[test]
    fn nudges_are_hidden_custom_messages() {
        let value = serde_json::to_value(nudge("prewalk-plan", PLAN_PROMPT)).unwrap();
        assert_eq!(value["customType"], "prewalk-plan");
        assert_eq!(value["display"], false);
        assert!(value["content"].as_str().unwrap().starts_with("STOP:"));
    }
}
