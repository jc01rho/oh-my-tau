//! `/btw`: ask the current model a side question over the live conversation as
//! read-only context, without tools; neither question nor answer enters the
//! session or the agent transcript. TS: session/btw-history.ts,
//! slash-commands/builtin-lifecycle.ts (`btw`).

use std::sync::Arc;

use pa_types::ai::{
    AssistantContentBlock, AssistantMessage, Context, Message, StopReason, UserContent, UserMessage,
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

/// Trimmed concatenation of the reply's text blocks.
pub(crate) fn reply_text(response: &AssistantMessage) -> String {
    let text: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            AssistantContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    text.trim().to_string()
}

/// The live conversation as provider messages (`/btw`'s read-only context).
pub(crate) async fn conversation(rt: &Arc<Runtime>) -> anyhow::Result<Vec<Message>> {
    rt.wait_settled().await;
    convert_messages(rt.agent.messages_snapshot().await.into_vec()).await
}

/// Run one side turn over `messages` on the session's current model, without
/// tools; nothing is appended to the session or the agent transcript.
pub(crate) async fn complete_side_turn(
    rt: &Arc<Runtime>,
    messages: Vec<Message>,
    max_tokens: u64,
) -> Result<SideTurn, SideTurnError> {
    let live = crate::compact::LiveCompleter::from_target(&rt.provider_target())
        .map_err(|e| SideTurnError::NoModel(e.to_string()))?;
    let (api_key, _) = crate::adapter::fresh_stored_key(
        &live.model.provider,
        live.credential_id,
        live.api_key.clone(),
    )
    .await;
    let options = pa_ai::types::SimpleStreamOptions {
        base: pa_ai::types::StreamOptions {
            max_tokens: Some(max_tokens),
            api_key,
            headers: live.headers.clone().map(|h| h.into_iter().collect()),
            // A side turn is a provider request too: extensions see it (upstream `onPayload`).
            on_payload: rt.ext_host().map(crate::ext::ai_payload_hook),
            ..pa_ai::types::StreamOptions::default()
        },
        reasoning: live
            .model
            .reasoning
            .then_some(pa_types::ai::ModelThinkingLevel::Medium),
        thinking_budgets: None,
        disable_reasoning: false,
        tool_choice: None,
    };
    let context = Context {
        system_prompt: Some(BTW_SYSTEM_PROMPT.to_string()),
        messages,
        tools: None,
    };
    match pa_ai::complete_simple(&live.model, &context, Some(options)).await {
        Ok(response)
            if matches!(
                response.stop_reason,
                StopReason::Aborted | StopReason::Error
            ) =>
        {
            Ok(SideTurn::Failed(response))
        }
        Ok(response) => Ok(SideTurn::Done(response)),
        Err(e) => Err(SideTurnError::Request(format!("{e:?}"))),
    }
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
