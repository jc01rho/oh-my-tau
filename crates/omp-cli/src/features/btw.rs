//! `/btw`: ask the current model a side question over the live conversation as
//! read-only context, without tools; neither question nor answer enters the
//! session or the agent transcript. TS: session/btw-history.ts,
//! slash-commands/builtin-lifecycle.ts (`btw`).

use std::future::Future;
use std::sync::Arc;

use pa_types::ai::{
    AssistantContentBlock, AssistantMessage, AssistantMessageEvent, Context, Message,
    ModelThinkingLevel, StopReason, UserContent, UserMessage,
};

use crate::rpc::slash::Outcome;
use crate::runtime::Runtime;

/// Answer ceiling (TS btw answers run to a few paragraphs).
const BTW_MAX_TOKENS: u64 = 4096;

/// Side-question system prompt: answer only, the transcript is context.
const BTW_SYSTEM_PROMPT: &str = "You are answering a side question about an ongoing coding-agent session. The conversation transcript is provided as read-only context; you have no tools and cannot change anything. Answer the question directly and concisely based on the transcript. Do not repeat large transcript verbatim; summarize what matters. Output plain prose only.";

/// Slash-command entry point.
pub async fn command(rt: &Arc<Runtime>, args: &str) -> Outcome {
    let question = args.trim();
    if question.is_empty() {
        rt.send(serde_json::json!({"type": "command_output", "text": "usage: /btw <question>"}));
        return Outcome::Handled {
            agent_invoked: false,
        };
    }
    // Wait for any running turn so the snapshot is a settled conversation.
    rt.wait_settled().await;

    let messages = rt.agent.messages_snapshot().await.into_vec();
    if messages.is_empty() {
        rt.send(serde_json::json!({"type": "command_output", "text": "btw: nothing to ask about yet — the session has no messages."}));
        return Outcome::Handled {
            agent_invoked: false,
        };
    }
    let mut context_messages = match convert_messages(messages).await {
        Ok(m) => m,
        Err(e) => {
            rt.send(serde_json::json!({"type": "command_output", "text": format!("btw: failed to read the conversation context: {e}")}));
            return Outcome::Handled {
                agent_invoked: false,
            };
        }
    };
    context_messages.push(Message::User(UserMessage {
        content: UserContent::Text(question.to_string()),
        timestamp: pa_agent::now_ms().cast_unsigned(),
        rest: serde_json::Map::default(),
    }));

    let answer = match complete_side_turn(rt, context_messages, BTW_MAX_TOKENS).await {
        Ok(SideTurn::Done(response)) => {
            let text = reply_text(&response);
            if text.is_empty() {
                "btw: the model returned an empty answer.".to_string()
            } else {
                format!("btw: {text}")
            }
        }
        Ok(SideTurn::Failed(_)) => "btw: the side question was aborted or failed.".to_string(),
        Err(SideTurnError::NoModel(e)) => format!("btw: no model available: {e}"),
        Err(SideTurnError::Request(e)) => format!("btw: request failed: {e}"),
    };
    // Neither the question nor the answer enters the transcript: we never
    // `set_messages`/`append_messages`, and the outcome is `Handled`, not a
    // prompt.
    rt.send(serde_json::json!({"type": "command_output", "text": answer}));
    Outcome::Handled {
        agent_invoked: false,
    }
}

/// Outcome of one tool-less side turn that reached the provider.
pub(crate) enum SideTurn {
    /// The provider settled with a usable reply.
    Done(AssistantMessage),
    /// The provider settled the turn as aborted or failed.
    Failed(AssistantMessage),
}

/// A side turn that never reached a settled provider reply.
pub(crate) enum SideTurnError {
    NoModel(String),
    Request(String),
}

/// Concatenation of the reply's text blocks, untrimmed.
pub(crate) fn message_text(response: &AssistantMessage) -> String {
    response
        .content
        .iter()
        .filter_map(|b| match b {
            AssistantContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Trimmed concatenation of the reply's text blocks.
pub(crate) fn reply_text(response: &AssistantMessage) -> String {
    message_text(response).trim().to_string()
}

/// The live conversation as provider messages (`/btw`'s read-only context).
pub(crate) async fn conversation(rt: &Arc<Runtime>) -> anyhow::Result<Vec<Message>> {
    rt.wait_settled().await;
    convert_messages(rt.agent.messages_snapshot().await.into_vec()).await
}

/// The side turn's provider context: the read-only transcript under the side-question prompt.
pub(crate) fn side_context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: Some(BTW_SYSTEM_PROMPT.to_string()),
        messages,
        tools: None,
    }
}

/// How one tool-less side turn is requested (TS `EphemeralTurnOptions` minus prompt and history).
pub(crate) struct SideTurnOptions {
    /// Output-token cap of the request.
    pub max_tokens: u64,
    /// Thinking level the session runs at; the side turn asks for it unless minimized.
    pub thinking_level: ModelThinkingLevel,
    /// TS `minimizeEffort`: run at the model's lowest effort instead of the session's level.
    pub minimize_effort: bool,
    /// TS `conversationKey`-derived provider session id, kept across related side turns.
    pub session_id: Option<String>,
}

impl SideTurnOptions {
    /// `/btw`'s request: its answer ceiling at the medium effort btw answers have used.
    fn btw(max_tokens: u64) -> Self {
        Self {
            max_tokens,
            thinking_level: ModelThinkingLevel::Medium,
            minimize_effort: false,
            session_id: None,
        }
    }
}

/// Thinking level a side turn asks for, or `None` for a model without reasoning.
///
/// TS `minimizeEffort` runs a side turn at the model's lowest effort. tau carries effort per
/// request (no transport exposes a per-message effort control), so the request names the lowest
/// level the model supports instead of the session's level.
pub(crate) fn side_turn_thinking_level(
    model: &pa_types::ai::Model,
    options: &SideTurnOptions,
) -> Option<ModelThinkingLevel> {
    if !model.reasoning {
        return None;
    }
    if !options.minimize_effort {
        return Some(options.thinking_level);
    }
    Some(
        pa_types::ai::get_supported_thinking_levels(model)
            .into_iter()
            .find(|level| *level != ModelThinkingLevel::Off)
            .unwrap_or(ModelThinkingLevel::Off),
    )
}

/// Start one side turn's provider stream over `context` on the session's current model, without
/// tools; nothing is appended to the session or the agent transcript.
pub(crate) async fn side_stream(
    rt: &Arc<Runtime>,
    context: &Context,
    options: &SideTurnOptions,
) -> Result<pa_ai::AssistantMessageEventStream, SideTurnError> {
    let live = crate::compact::LiveCompleter::from_target(&rt.provider_target())
        .map_err(|e| SideTurnError::NoModel(e.to_string()))?;
    let (api_key, _) = crate::adapter::fresh_stored_key(
        &live.model.provider,
        live.credential_id,
        live.api_key.clone(),
    )
    .await;
    let stream_options = pa_ai::types::SimpleStreamOptions {
        base: pa_ai::types::StreamOptions {
            max_tokens: Some(options.max_tokens),
            api_key,
            headers: live.headers.clone().map(|h| h.into_iter().collect()),
            session_id: options.session_id.clone(),
            ..pa_ai::types::StreamOptions::default()
        },
        reasoning: side_turn_thinking_level(&live.model, options),
        thinking_budgets: None,
        disable_reasoning: false,
        tool_choice: None,
    };
    pa_ai::stream_simple(&live.model, context, Some(stream_options))
        .map_err(|e| SideTurnError::Request(format!("{e:?}")))
}

/// Boxed future used by [`SideTurnObserver`] so the trait stays object safe.
pub(crate) type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Live delivery of one side turn (TS `onTextDelta`).
pub(crate) trait SideTurnObserver: Send + Sync {
    /// Deliver one provider text delta. An error aborts the side turn (TS awaits `onTextDelta` in
    /// order and rejects the turn whose delivery throws).
    fn text_delta<'a>(&'a self, delta: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

/// A side turn nobody watches (`/btw`).
pub(crate) struct NoObserver;

impl SideTurnObserver for NoObserver {
    fn text_delta<'a>(&'a self, _delta: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// One side turn's settled outcome and the text its stream produced.
pub(crate) struct SideTurnReply {
    pub outcome: SideTurn,
    /// Concatenated text deltas (TS `providerReplyText`).
    pub text: String,
}

/// Consume one side turn's stream: every text delta reaches `observer` in the order the provider
/// produced it, then the settled message does.
pub(crate) async fn pump_side_stream(
    mut stream: pa_ai::AssistantMessageEventStream,
    observer: &dyn SideTurnObserver,
) -> Result<SideTurnReply, SideTurnError> {
    let mut text = String::new();
    while let Some(event) = stream.next_event().await {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                text.push_str(&delta);
                observer.text_delta(&delta).await.map_err(|e| {
                    SideTurnError::Request(format!("the text delta was not delivered: {e}"))
                })?;
            }
            AssistantMessageEvent::Done { message, .. } => {
                let outcome =
                    if matches!(message.stop_reason, StopReason::Aborted | StopReason::Error) {
                        SideTurn::Failed(message)
                    } else {
                        SideTurn::Done(message)
                    };
                return Ok(SideTurnReply { outcome, text });
            }
            AssistantMessageEvent::Error { error, .. } => {
                return Ok(SideTurnReply {
                    outcome: SideTurn::Failed(error),
                    text,
                });
            }
            _ => {}
        }
    }
    Err(SideTurnError::Request(
        "the side turn ended without a final message".into(),
    ))
}

/// Run one side turn over `messages` on the session's current model, without
/// tools; nothing is appended to the session or the agent transcript.
pub(crate) async fn complete_side_turn(
    rt: &Arc<Runtime>,
    messages: Vec<Message>,
    max_tokens: u64,
) -> Result<SideTurn, SideTurnError> {
    let context = side_context(messages);
    let stream = side_stream(rt, &context, &SideTurnOptions::btw(max_tokens)).await?;
    pump_side_stream(stream, &NoObserver)
        .await
        .map(|reply| reply.outcome)
}

/// Agent transcript → LLM messages through the same converter the live loop
/// uses (developer/custom-role handling included).
async fn convert_messages(
    messages: Vec<pa_agent::types::AgentMessage>,
) -> anyhow::Result<Vec<Message>> {
    // `convert_to_llm` yields `pa_agent::types::Message`; the provider-facing
    // `Context` carries the wire-shape `pa_types::ai::Message`, so convert each
    // message back through its serialization (lossless: same JSON contract).
    let converted = crate::agent::convert_to_llm()(messages).await?;
    converted
        .into_iter()
        .map(|m| serde_json::from_value::<Message>(serde_json::to_value(&m)?).map_err(Into::into))
        .collect()
}
