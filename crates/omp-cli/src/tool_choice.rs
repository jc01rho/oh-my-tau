//! `/force` (TS `AgentSession.setForcedToolChoice`, `utils/tool-choice.ts`,
//! `session/tool-choice-queue.ts`): pin the next model call of the main agent
//! to one active tool, then `none` for the call after it, so the model calls
//! exactly that tool once and then answers.
//!
//! The queue is consumed by the main agent's stream function
//! ([`wrap_stream`], installed by `agent::build_agent`): each call takes the
//! head choice and hands it to the provider request through the
//! [`FORCED`] task-local. A choice whose call fails or is aborted goes back to
//! the head (TS `onRejected` → `requeue`); a forced tool that is no longer
//! active drops the whole sequence (TS `unavailable` → `drop_sequence`).

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use pa_agent::stream::{
    AssistantMessageEvent, LlmContext, ModelStream, StreamFn, StreamRequestOptions,
};
use pa_agent::types::{AssistantMessage, Model as AgentModel, StopReason};
use pa_ai::types::SimpleToolChoice;
use serde_json::Value;

tokio::task_local! {
    /// The forced choice of the stream call running in this scope; read by
    /// `adapter::stream_once` into the provider request.
    pub(crate) static FORCED: SimpleToolChoice;
}

#[derive(Debug)]
struct Queue {
    pending: VecDeque<SimpleToolChoice>,
    in_flight: Option<SimpleToolChoice>,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    pending: VecDeque::new(),
    in_flight: None,
});

fn queue() -> MutexGuard<'static, Queue> {
    QUEUE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// TS `buildNamedToolChoice` reduced to its yes/no: whether the model's api
/// can carry a forced call to one named tool. Google apis only accept
/// `required` (not a named tool) and unknown apis none, so both refuse, as do
/// OpenAI-family models whose compat turns tool choice off.
#[must_use]
pub fn supports_named_choice(model: &Value) -> bool {
    let api = model.get("api").and_then(Value::as_str).unwrap_or_default();
    match api {
        "anthropic-messages" | "bedrock-converse-stream" | "ollama-chat" => true,
        "openai-codex-responses"
        | "openai-responses"
        | "openai-completions"
        | "azure-openai-responses"
        | "openrouter" => {
            let compat = model.get("compat");
            let off =
                |key: &str| compat.and_then(|c| c.get(key)).and_then(Value::as_bool) == Some(false);
            !off("supportsToolChoice") && !off("supportsForcedToolChoice")
        }
        _ => false,
    }
}

/// TS `setForcedToolChoice`: queue `[forced, none]` for the next model calls.
///
/// # Errors
/// The tool is not active, or the model cannot force a named tool (upstream's
/// messages).
pub fn force(tool: &str, active: &[String], model: &Value) -> Result<(), String> {
    if !active.iter().any(|name| name == tool) {
        return Err(format!("Tool \"{tool}\" is not currently active."));
    }
    if !supports_named_choice(model) {
        return Err("Current model does not support forcing a specific tool.".into());
    }
    let mut q = queue();
    q.pending.push_back(SimpleToolChoice::Function {
        name: tool.to_owned(),
    });
    q.pending.push_back(SimpleToolChoice::None);
    Ok(())
}

/// The choice for the call about to start (TS `#nextHardToolChoice`): the
/// queue head, recorded in flight; a forced tool missing from `active` drops
/// its sequence instead.
fn take(active: &[&str]) -> Option<SimpleToolChoice> {
    let mut q = queue();
    let choice = q.pending.pop_front()?;
    if let SimpleToolChoice::Function { name } = &choice {
        if !active.contains(&name.as_str()) {
            if matches!(q.pending.front(), Some(SimpleToolChoice::None)) {
                q.pending.pop_front();
            }
            return None;
        }
    }
    q.in_flight = Some(choice.clone());
    Some(choice)
}

/// Finish the in-flight choice: served (TS `resolve`) or put back at the head
/// for the next call (TS `reject` → `requeue`).
fn settle(served: bool) {
    let mut q = queue();
    if let Some(choice) = q.in_flight.take() {
        if !served {
            q.pending.push_front(choice);
        }
    }
}

/// Drop every queued and in-flight choice (a new session starts clean).
pub fn clear() {
    let mut q = queue();
    q.pending.clear();
    q.in_flight = None;
}

/// Wrap the main agent's stream function so each model call carries the
/// queued choice.
pub fn wrap_stream(base: StreamFn) -> StreamFn {
    std::sync::Arc::new(
        move |model: AgentModel, context: LlmContext, options: StreamRequestOptions| {
            let base = base.clone();
            Box::pin(async move {
                // A prewalk handoff decided at the last turn end lands first.
                crate::features::prewalk::wait_for_handoff().await;
                let active: Vec<&str> = context.tools.iter().map(|t| t.name.as_str()).collect();
                let Some(choice) = take(&active) else {
                    return base(model, context, options).await;
                };
                match FORCED.scope(choice, base(model, context, options)).await {
                    Ok(inner) => Ok(Box::new(Settling {
                        inner,
                        settled: false,
                    }) as Box<dyn ModelStream>),
                    Err(err) => {
                        settle(false);
                        Err(err)
                    }
                }
            })
        },
    )
}

/// A forced call's stream: settles the choice once the call ends — served
/// unless the provider reported an error or the call was aborted/closed.
struct Settling {
    inner: Box<dyn ModelStream>,
    settled: bool,
}

impl Settling {
    fn finish(&mut self, served: bool) {
        if !self.settled {
            self.settled = true;
            settle(served);
        }
    }
}

impl ModelStream for Settling {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        self.inner.next_event()
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(async move {
            let result = self.inner.result().await;
            let served = matches!(&result, Ok(message)
                if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted));
            self.finish(served);
            result
        })
    }

    fn close(&mut self) {
        self.inner.close();
        self.finish(false);
    }
}

impl Drop for Settling {
    fn drop(&mut self) {
        self.finish(false);
    }
}

/// Serializes the tests that drive the process-wide queue (async tests hold
/// it across awaits, so it is a tokio mutex).
#[cfg(test)]
pub(crate) fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Vec<String> {
        vec!["bash".into(), "read".into()]
    }

    #[test]
    fn named_choice_support_follows_the_api() {
        assert!(supports_named_choice(&json!({"api": "anthropic-messages"})));
        assert!(supports_named_choice(&json!({"api": "openai-completions"})));
        assert!(!supports_named_choice(
            &json!({"api": "openai-completions", "compat": {"supportsForcedToolChoice": false}})
        ));
        assert!(!supports_named_choice(
            &json!({"api": "google-generative-ai"})
        ));
        assert!(!supports_named_choice(&json!({"api": "mystery"})));
    }

    #[test]
    fn force_queues_forced_then_none_and_requeues_failures() {
        let _guard = test_lock().blocking_lock();
        clear();
        let model = json!({"api": "anthropic-messages"});
        assert_eq!(
            force("write", &tools(), &model).unwrap_err(),
            "Tool \"write\" is not currently active."
        );
        assert_eq!(
            force("bash", &tools(), &json!({"api": "google-vertex"})).unwrap_err(),
            "Current model does not support forcing a specific tool."
        );
        force("bash", &tools(), &model).unwrap();
        let forced = SimpleToolChoice::Function {
            name: "bash".into(),
        };
        assert_eq!(take(&["bash", "read"]), Some(forced.clone()));
        settle(false);
        assert_eq!(
            take(&["bash", "read"]),
            Some(forced),
            "a failed call is retried"
        );
        settle(true);
        assert_eq!(take(&["bash", "read"]), Some(SimpleToolChoice::None));
        settle(true);
        assert_eq!(take(&["bash", "read"]), None);
    }

    struct Fake {
        fail: bool,
    }

    impl ModelStream for Fake {
        fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
            Box::pin(async { None })
        }
        fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    return Err(anyhow::anyhow!("transport failed"));
                }
                Ok(serde_json::from_value(json!({
                    "role": "assistant", "content": [], "api": "anthropic-messages",
                    "provider": "p", "model": "m", "stopReason": "stop", "timestamp": 0,
                    "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0,
                        "cacheWrite": 0, "total": 0}}
                }))?)
            })
        }
    }

    #[tokio::test]
    async fn wrapped_calls_see_the_queued_choice_in_order() {
        let _guard = test_lock().lock().await;
        clear();
        force("bash", &tools(), &json!({"api": "anthropic-messages"})).unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fail_next = std::sync::Arc::new(Mutex::new(true));
        let (seen_in, fail_in) = (seen.clone(), fail_next.clone());
        let base: StreamFn = std::sync::Arc::new(move |_model, _context, _options| {
            let (seen, fail) = (seen_in.clone(), fail_in.clone());
            Box::pin(async move {
                seen.lock()
                    .unwrap()
                    .push(FORCED.try_with(Clone::clone).ok());
                let fail = std::mem::replace(&mut *fail.lock().unwrap(), false);
                Ok(Box::new(Fake { fail }) as Box<dyn ModelStream>)
            })
        });
        let wrapped = wrap_stream(base);
        let model: AgentModel = serde_json::from_value(json!({
            "id": "m", "name": "m", "api": "anthropic-messages", "provider": "p",
            "baseUrl": "", "reasoning": false, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap();
        let context = LlmContext {
            tools: vec![pa_agent::stream::ToolDefinition {
                name: "bash".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }],
            ..LlmContext::default()
        };
        for _ in 0..4 {
            let mut stream = wrapped(
                model.clone(),
                context.clone(),
                StreamRequestOptions::default(),
            )
            .await
            .unwrap();
            let _ = stream.result().await;
        }
        let forced = SimpleToolChoice::Function {
            name: "bash".into(),
        };
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Some(forced.clone()),
                Some(forced),
                Some(SimpleToolChoice::None),
                None
            ],
            "a failed forced call is retried forced; then none; then free"
        );
    }

    #[test]
    fn an_inactive_forced_tool_drops_its_sequence() {
        let _guard = test_lock().blocking_lock();
        clear();
        force("bash", &tools(), &json!({"api": "openai-responses"})).unwrap();
        assert_eq!(take(&["read"]), None);
        assert_eq!(
            take(&["bash", "read"]),
            None,
            "the trailing none went with it"
        );
    }
}
