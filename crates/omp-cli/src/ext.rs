//! Extension host wiring: spawns `ext-host/host.ts` (Bun), loads the user's
//! `~/.omp/agent/extensions` plus `--extension` paths, bridges the host onto
//! the agent (`transform_context`/`before_tool_call`/`after_tool_call`),
//! registers extension tools, serves host->core requests (`append_entry`,
//! `send_message`, ui frames), and emits omp lifecycle events.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, Subscription};
use pa_agent::agent_loop::{AfterToolCallFn, BeforeToolCallFn, TransformContextFn};
use pa_agent::types::{
    AgentEvent, AgentMessage, AgentTool, AgentToolResult, CustomAgentMessage,
    GetContinuationMessagesContext, ToolResultContent,
};
use serde_json::{json, Value};

use omp_ext::hooks::ExtHooks;
use omp_ext::lifecycle::{
    self, CacheWarmingDecisionEvent, CacheWarmingOutcome, CompactingOutcome, DiscoveredPath,
    InputSource, LifecycleEvent, ResourcesDiscoverOutcome, ResourcesDiscoverReason, SwitchReason,
};
use omp_ext::{ExtHost, HostConfig, HostMessage, ToolInfo};
use omp_providers::auth::{
    on_credential_disabled, CredentialDisabledEvent, CredentialDisabledSubscription,
};

use crate::events::{message_value, ExtUpdateEncoder};
use crate::runtime::Runtime;

/// Default budget for one extension tool call (host default matches).
fn tool_timeout() -> Duration {
    let ms = std::env::var("OMP_EXT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000);
    Duration::from_millis(ms)
}

/// TS `SESSION_STOP_CONTINUATION_CAP` (agent-session.ts): at most this many
/// `session_stop` continuation turns per run; `decision: "block"` is exempt.
const SESSION_STOP_CONTINUATION_CAP: usize = 8;

/// TS `prompts/system/session-stop-blocked.md` (rendered when a `block`
/// decision carries no reason/additionalContext).
const SESSION_STOP_BLOCKED_PROMPT: &str = "A session_stop handler blocked completion without a reason. Resolve the outstanding work before finishing.";

/// Per-run `session_stop` control-hook state (TS `#sessionStopContinuationCount`
/// / `#sessionStopHookActive`).
#[derive(Debug, Default, PartialEq, Eq)]
struct StopState {
    continuations: usize,
    hook_active: bool,
}

/// TS `#sessionStopContinuationContext`: the hidden next-turn message a
/// `session_stop` outcome schedules, if any.
fn stop_continuation_text(outcome: &omp_ext::lifecycle::SessionStopOutcome) -> Option<String> {
    let additional = outcome
        .additional_context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let reason = outcome
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if outcome.block {
        Some(
            reason
                .or(additional)
                .unwrap_or(SESSION_STOP_BLOCKED_PROMPT)
                .to_string(),
        )
    } else if outcome.continue_turn {
        additional.or(reason).map(str::to_string)
    } else {
        None
    }
}

/// Whether a session has any extension to load: explicit `--extension`/`--hook`
/// paths, or an entry the host's discovery would pick up in
/// `~/.omp/agent/extensions` (a `.ts`/`.js` file or a directory with an index).
#[must_use]
pub fn has_extensions(extra: &[PathBuf]) -> bool {
    if !extra.is_empty() {
        return true;
    }
    let Some(dir) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".omp").join("agent").join("extensions"))
    else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            ["index.ts", "index.js"]
                .iter()
                .any(|i| path.join(i).is_file())
        } else {
            path.extension().is_some_and(|e| e == "ts" || e == "js")
        }
    })
}

/// Locate `ext-host/host.ts`: `$OMP_RS_EXT_HOST`, else the workspace's copy
/// found by walking up from the binary's location.
#[must_use]
pub fn host_script() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OMP_RS_EXT_HOST") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    for dir in exe.ancestors() {
        let cand = dir.join("ext-host").join("host.ts");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// FNV-1a: a hash that stays stable across Rust versions (socket names must
/// match between differently built omp-rs binaries sharing one daemon).
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in *part {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Shared extension-host daemon socket (the default mode; one bun process per
/// user serves every omp-rs session, each in its own Worker). `None` selects
/// the per-process private host: `OMP_RS_EXT_HOST=private` (or the legacy
/// `OMP_RS_EXT_SHARED=0`).
///
/// Path: `$OMP_RS_EXT_SOCKET` verbatim, else
/// `$XDG_RUNTIME_DIR/omp-rs/ext-<key>.sock`, else
/// `~/.omp-rs/run/ext-<key>.sock`. `<key>` hashes the host version, the host
/// script path/size/mtime (a changed host never talks to an old daemon) and
/// the explicit extension paths, so different extension sets or builds get
/// their own daemon.
fn shared_socket_path(script: &Path, extra: &[PathBuf]) -> Option<PathBuf> {
    if std::env::var("OMP_RS_EXT_HOST").is_ok_and(|v| v == "private")
        || std::env::var_os("OMP_RS_EXT_SHARED").is_some_and(|v| v == "0")
    {
        return None;
    }
    if let Ok(p) = std::env::var("OMP_RS_EXT_SOCKET") {
        return Some(PathBuf::from(p));
    }
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(x) => PathBuf::from(x).join("omp-rs"),
        None => PathBuf::from(std::env::var_os("HOME")?)
            .join(".omp-rs")
            .join("run"),
    };
    let stamp = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{}:{t}", m.len())
            })
            .unwrap_or_default()
    };
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let script = canon(script);
    let mut sets: Vec<String> = extra
        .iter()
        .map(|p| canon(p).to_string_lossy().into_owned())
        .collect();
    sets.sort();
    let mut parts: Vec<Vec<u8>> = vec![
        env!("CARGO_PKG_VERSION").into(),
        script.to_string_lossy().as_bytes().to_vec(),
        stamp(&script).into_bytes(),
    ];
    if let Some(parent) = script.parent() {
        parts.push(stamp(&parent.join("shard.ts")).into_bytes());
        parts.push(stamp(&parent.join("session.worker.ts")).into_bytes());
    }
    parts.extend(sets.into_iter().map(String::into_bytes));
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Some(dir.join(format!("ext-{:016x}.sock", fnv1a(&refs))))
}

/// Gate decision from the `input` event before a prompt is admitted.
pub enum PromptGate {
    /// Run with (possibly rewritten) text.
    Run(String),
    /// An extension handled the input; skip the prompt.
    Handled,
}

/// Last session ctx sent to the host; kept so a shared-daemon reconnect (or a
/// title change) can re-send the full merged ctx (`host.ts set_ctx` merges
/// into state → `getSessionName()` etc.).
#[derive(Debug, Clone, Default)]
struct SessionCtx {
    session_id: String,
    session_file: Option<PathBuf>,
    #[allow(dead_code)] // ctx.model is the live Model object (see `Live`), not this ref
    model: String,
    session_name: Option<String>,
    /// TS `ctx.hasUI` (see [`ExtMode::has_ui`]).
    has_ui: bool,
    /// TS `ctx.mode`.
    mode: &'static str,
}

/// Which TS mode the host serves (`ctx.mode`, and `hasUI`: TS `hasUI()` is true exactly when a UI
/// context was provided — interactive and non-headless rpc — and false for print/json).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtMode {
    Print,
    Json,
    Rpc,
    Tui,
}

impl ExtMode {
    fn name(self) -> &'static str {
        match self {
            Self::Print => "print",
            Self::Json => "json",
            Self::Rpc => "rpc",
            Self::Tui => "tui",
        }
    }
    fn has_ui(self) -> bool {
        matches!(self, Self::Rpc | Self::Tui)
    }
}

/// `HostResponder` backed by the runtime: dialogs travel as `extension_ui_request` frames (the TUI
/// renders them; an rpc client answers them), `ctx.compact` / `navigateTree` run on the session.
struct RuntimeResponder {
    rt: std::sync::Weak<Runtime>,
    mode: ExtMode,
}

impl RuntimeResponder {
    /// Send one dialog and await the answer object (`None` = cancelled / no UI).
    async fn dialog(
        &self,
        method: &str,
        mut frame: serde_json::Map<String, Value>,
    ) -> Option<Value> {
        if !self.mode.has_ui() {
            return None;
        }
        let rt = self.rt.upgrade()?;
        let id = format!(
            "ext-ui-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        frame.insert("type".into(), json!("extension_ui_request"));
        frame.insert("id".into(), json!(id));
        frame.insert("method".into(), json!(method));
        let waiter = crate::rpc::register_pending_ui_request(&id);
        rt.send(Value::Object(frame));
        let answer = waiter.await.ok()?;
        (answer.get("cancelled").and_then(Value::as_bool) != Some(true)).then_some(answer)
    }

    fn text(answer: Option<Value>) -> Option<String> {
        answer?
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

impl omp_ext::lifecycle::HostResponder for RuntimeResponder {
    fn ui_confirm<'a>(
        &'a self,
        title: &'a str,
        message: &'a str,
    ) -> omp_ext::lifecycle::BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut f = serde_json::Map::new();
            f.insert("title".into(), json!(title));
            f.insert("message".into(), json!(message));
            // TUI answers `{value: bool}`, an rpc client `{confirmed: bool}` (TS `RpcExtensionUIContext`).
            self.dialog("confirm", f).await.is_some_and(|a| {
                a.get("confirmed").and_then(Value::as_bool) == Some(true)
                    || a.get("value").and_then(Value::as_bool) == Some(true)
            })
        })
    }

    fn ui_select<'a>(
        &'a self,
        title: &'a str,
        options: Vec<String>,
    ) -> omp_ext::lifecycle::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let mut f = serde_json::Map::new();
            f.insert("title".into(), json!(title));
            f.insert("options".into(), json!(options));
            Self::text(self.dialog("select", f).await)
        })
    }

    fn ui_input<'a>(
        &'a self,
        title: &'a str,
        placeholder: Option<&'a str>,
    ) -> omp_ext::lifecycle::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let mut f = serde_json::Map::new();
            f.insert("title".into(), json!(title));
            f.insert("placeholder".into(), json!(placeholder));
            Self::text(self.dialog("input", f).await)
        })
    }

    fn ui_editor<'a>(
        &'a self,
        title: &'a str,
        prefill: Option<&'a str>,
    ) -> omp_ext::lifecycle::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let mut f = serde_json::Map::new();
            f.insert("title".into(), json!(title));
            f.insert("prefill".into(), json!(prefill));
            Self::text(self.dialog("editor", f).await)
        })
    }

    fn compact(&self, options: Value) -> omp_ext::lifecycle::BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let Some(rt) = self.rt.upgrade() else {
                return Ok(());
            };
            let instructions = match &options {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o
                    .get("customInstructions")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            };
            rt.compact(instructions.as_deref(), None)
                .await
                .map(|_| ())
                .map_err(|e| e.message)
        })
    }

    fn navigate_tree<'a>(
        &'a self,
        target: &'a str,
        options: Value,
    ) -> omp_ext::lifecycle::BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            // TS: only command contexts (interactive / rpc) carry a real handler.
            let (true, Some(rt)) = (
                matches!(self.mode, ExtMode::Tui | ExtMode::Rpc),
                self.rt.upgrade(),
            ) else {
                return Ok(json!({"cancelled": false}));
            };
            crate::tree::navigate_tree(&rt, target, &options, None)
                .await
                .map(|v| v.unwrap_or_else(|| json!({"cancelled": false})))
                .map_err(|e| e.message)
        })
    }

    fn model_complete(
        &self,
        request: Value,
    ) -> omp_ext::lifecycle::BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move {
            let provider = request
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = request
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if provider.is_empty() || id.is_empty() {
                return Err("model_complete requires provider and model".into());
            }
            let selected = crate::model::select(Some(&format!("{provider}/{id}")))
                .await
                .map_err(|e| format!("model resolution failed: {e:#}"))?;
            let context: pa_types::ai::Context =
                serde_json::from_value(request.get("context").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("invalid completion context: {e}"))?;
            let options_value = request.get("options").cloned().unwrap_or(Value::Null);
            let options = pa_ai::types::StreamOptions {
                api_key: selected.api_key,
                max_tokens: options_value.get("maxTokens").and_then(Value::as_u64),
                session_id: options_value
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                timeout_ms: options_value.get("timeoutMs").and_then(Value::as_u64),
                headers: selected.headers.map(|h| h.into_iter().collect()),
                ..Default::default()
            };
            let response = pa_ai::complete(&selected.model, &context, Some(options))
                .await
                .map_err(|e| format!("model completion failed: {e}"))?;
            serde_json::to_value(response).map_err(|e| format!("completion encoding failed: {e}"))
        })
    }

    fn run_ephemeral_turn(
        &self,
        request: Value,
    ) -> omp_ext::lifecycle::BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move {
            let mut turn = EphemeralTurn::parse(&request)?;
            let rt = self
                .rt
                .upgrade()
                .ok_or_else(|| "runEphemeralTurn: the session has ended".to_string())?;
            let mut messages = crate::features::btw::conversation(&rt)
                .await
                .map_err(|e| format!("runEphemeralTurn: failed to read the conversation: {e}"))?;
            messages.extend(std::mem::take(&mut turn.tail));
            let context = crate::features::btw::side_context(messages);
            turn.check_context_bytes(&context)?;
            let options = crate::features::btw::SideTurnOptions {
                max_tokens: turn.max_tokens,
                thinking_level: crate::adapter::model_thinking_level(
                    rt.agent.thinking_level().await,
                ),
                minimize_effort: turn.minimize_effort,
                session_id: Some(turn.side_session_id(&rt.session_id())),
            };
            let observer = DeltaObserver {
                host: rt.ext_host(),
                stream_id: turn.stream_id.clone(),
            };
            use crate::features::btw::{pump_side_stream, side_stream, SideTurn};
            let stream = side_stream(&rt, &context, &options)
                .await
                .map_err(ephemeral_turn_error)?;
            let reply = pump_side_stream(stream, &observer)
                .await
                .map_err(ephemeral_turn_error)?;
            let (response, failed) = match reply.outcome {
                SideTurn::Done(response) => (response, false),
                SideTurn::Failed(response) => (response, true),
            };
            // TS reports usage where the provider settles the attempt, failed ones included.
            turn.journal(&rt, &response);
            if failed {
                return Err(format!(
                    "runEphemeralTurn: the side turn {}: {}",
                    if response.stop_reason == pa_types::ai::StopReason::Aborted {
                        "was aborted"
                    } else {
                        "failed"
                    },
                    response
                        .error_message
                        .as_deref()
                        .unwrap_or("no error message")
                ));
            }
            // TS builds `replyText` from the accumulated text deltas, so the reply is exactly the text
            // the host was streamed (tau has no obfuscator, so no later expansion can add to it).
            let text = reply.text.trim();
            let reply_text = if turn.dedupe_reply {
                dedupe_ephemeral_reply(text, turn.reply_max_bytes)
            } else {
                text.to_string()
            };
            Ok(ephemeral_result(&reply_text, &response))
        })
    }
}

/// Prefix a side-turn failure the way the request's error messages read.
fn ephemeral_turn_error(error: crate::features::btw::SideTurnError) -> String {
    use crate::features::btw::SideTurnError;
    match error {
        SideTurnError::NoModel(e) => format!("runEphemeralTurn: no model available: {e}"),
        SideTurnError::Request(e) => format!("runEphemeralTurn: request failed: {e}"),
    }
}

/// Delivers one side turn's text to the host's `onTextDelta` (TS awaits each delivery in order, and
/// a rejected delivery rejects the turn).
struct DeltaObserver {
    host: Option<Arc<ExtHost>>,
    /// The host's side-turn id; `None` when the caller passed no `onTextDelta`.
    stream_id: Option<Value>,
}

impl crate::features::btw::SideTurnObserver for DeltaObserver {
    fn text_delta<'a>(
        &'a self,
        delta: &'a str,
    ) -> crate::features::btw::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(stream_id) = &self.stream_id else {
                return Ok(());
            };
            let host = self
                .host
                .as_ref()
                .ok_or_else(|| "the extension host is not loaded".to_string())?;
            host.request(
                "ephemeral_delta",
                json!({"streamId": stream_id, "delta": delta}),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
    }
}

/// Output-token cap of a side turn whose caller set none (the `/btw` ceiling).
const EPHEMERAL_DEFAULT_MAX_TOKENS: u64 = 4096;

/// Reply cap of an ephemeral turn whose caller set none (TS `EPHEMERAL_REPLY_MAX_BYTES`).
const EPHEMERAL_REPLY_MAX_BYTES: u64 = 4096;

/// Longest reply a `model_usage` entry keeps; a longer one is left out, never truncated
/// (TS `COMPLETION_TEXT_MAX_CHARS`).
const EPHEMERAL_COMPLETION_TEXT_MAX_CHARS: usize = 4000;

/// A validated `run_ephemeral_turn` request (TS `EphemeralTurnOptions`, wire form from `host.ts`).
#[derive(Debug)]
struct EphemeralTurn {
    /// `history` copies followed by the prompt; appended after the live conversation.
    tail: Vec<pa_types::ai::Message>,
    max_tokens: u64,
    /// TS `maxContextBytes`: reject the turn when its provider context is larger.
    max_context_bytes: Option<u64>,
    /// TS `minimizeEffort`: run at the model's lowest effort instead of the session's level.
    minimize_effort: bool,
    /// TS `conversationKey`: related side turns share one provider lineage.
    conversation_key: Option<String>,
    /// TS `purpose`: journal the settled attempt as a `model_usage` entry under this name.
    purpose: Option<String>,
    /// TS `dedupeReply` (default `true`): collapse repeated lines and bound the reply.
    dedupe_reply: bool,
    /// TS `replyMaxBytes` (default 4096): byte ceiling of the deduped reply.
    reply_max_bytes: u64,
    /// Host id echoed on every `ephemeral_delta`; `None` when there is no `onTextDelta`.
    stream_id: Option<Value>,
    /// The host's own `onUsage` reporter is in place, so the `purpose` journal would double count.
    usage_callback: bool,
}

impl EphemeralTurn {
    /// Validate before inference, as TS rejects bad options before any provider call.
    fn parse(request: &Value) -> Result<Self, String> {
        let prompt = request
            .get("promptText")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "runEphemeralTurn: promptText must be a non-empty string".to_string())?;
        let positive = |field: &str| -> Result<Option<u64>, String> {
            match request.get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_u64()
                    .filter(|n| (1..=(1u64 << 53) - 1).contains(n))
                    .map(Some)
                    .ok_or_else(|| {
                        format!("runEphemeralTurn: {field} must be a positive safe integer")
                    }),
            }
        };
        let max_tokens = positive("maxTokens")?.unwrap_or(EPHEMERAL_DEFAULT_MAX_TOKENS);
        let max_context_bytes = positive("maxContextBytes")?;
        let reply_max_bytes = positive("replyMaxBytes")?.unwrap_or(EPHEMERAL_REPLY_MAX_BYTES);
        let flag = |field: &str| -> Result<bool, String> {
            match request.get(field) {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(b)) => Ok(*b),
                Some(_) => Err(format!("runEphemeralTurn: {field} must be a boolean")),
            }
        };
        let minimize_effort = flag("minimizeEffort")?;
        // TS `args.dedupeReply === false`: only an explicit `false` turns the dedupe off.
        let dedupe_reply = match request.get("dedupeReply") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("runEphemeralTurn: dedupeReply must be a boolean".into()),
        };
        let usage_callback = flag("usageCallback")?;
        let text = |field: &str| -> Result<Option<String>, String> {
            match request.get(field) {
                None | Some(Value::Null) => Ok(None),
                // An empty string is no key at all (TS reads these with a truthiness test).
                Some(Value::String(s)) if s.is_empty() => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(format!("runEphemeralTurn: {field} must be a string")),
            }
        };
        let conversation_key = text("conversationKey")?;
        let purpose = text("purpose")?;
        let stream_id = match request.get("streamId") {
            None | Some(Value::Null) => None,
            Some(v @ (Value::Number(_) | Value::String(_))) => Some(v.clone()),
            Some(_) => return Err("runEphemeralTurn: streamId must be a number or a string".into()),
        };
        match request.get("tools") {
            None | Some(Value::Null | Value::Bool(false)) => {}
            Some(_) => return Err("runEphemeralTurn: tools may only be false".into()),
        }
        let mut tail: Vec<pa_types::ai::Message> = match request.get("history") {
            None | Some(Value::Null) => Vec::new(),
            Some(h) => serde_json::from_value(h.clone())
                .map_err(|e| format!("runEphemeralTurn: invalid history: {e}"))?,
        };
        let mut content = vec![json!({"type": "text", "text": prompt})];
        match request.get("promptImages") {
            None | Some(Value::Null) => {}
            Some(Value::Array(images)) => content.extend(images.iter().cloned()),
            Some(_) => return Err("runEphemeralTurn: promptImages must be an array".into()),
        }
        let user = serde_json::from_value(json!({
            "role": "user",
            "content": content,
            "timestamp": pa_agent::now_ms().cast_unsigned(),
        }))
        .map_err(|e| format!("runEphemeralTurn: invalid promptImages: {e}"))?;
        tail.push(user);
        Ok(Self {
            tail,
            max_tokens,
            max_context_bytes,
            minimize_effort,
            conversation_key,
            purpose,
            dedupe_reply,
            reply_max_bytes,
            stream_id,
            usage_callback,
        })
    }

    /// TS `maxContextBytes`: the encoded provider context must fit the caller's ceiling.
    fn check_context_bytes(&self, context: &pa_types::ai::Context) -> Result<(), String> {
        let Some(limit) = self.max_context_bytes else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(context)
            .map_err(|e| format!("runEphemeralTurn: failed to encode the context: {e}"))?
            .len() as u64;
        if bytes > limit {
            return Err(format!(
                "runEphemeralTurn: ephemeral turn context exceeds the configured {limit}-byte limit"
            ));
        }
        Ok(())
    }

    /// TS `sideSessionId`: a keyed conversation keeps one provider lineage across its side turns,
    /// an unkeyed one gets a fresh lineage per call.
    fn side_session_id(&self, session: &str) -> String {
        match &self.conversation_key {
            Some(key) => format!("{session}:side:conversation:{key}"),
            None => format!("{session}:side:{}", crate::persist::uuid_v7()),
        }
    }

    /// TS `journalCompletion`: the settled attempt becomes a `model_usage` entry, so a side turn's
    /// spend counts in session totals and `omt stats`. The host's own `onUsage` wins when the
    /// caller passed one, and a failed write is logged rather than failing the turn.
    fn journal(&self, rt: &Arc<Runtime>, response: &pa_types::ai::AssistantMessage) {
        let Some(purpose) = self.journal_purpose() else {
            return;
        };
        let payload = ephemeral_usage_payload(purpose, response);
        if let Err(e) = rt.append_entry("model_usage", payload) {
            eprintln!("omt: ephemeral turn: model usage journal failed: {e}");
        }
    }

    /// The name the settled attempt is journaled under, or `None` when nothing is journaled: a turn
    /// without a `purpose` has no name, and a host that reports usage itself already counted it.
    fn journal_purpose(&self) -> Option<&str> {
        match (&self.purpose, self.usage_callback) {
            (Some(purpose), false) => Some(purpose),
            _ => None,
        }
    }
}

/// The `model_usage` entry one settled side turn journals (TS `journalCompletion` + `completionRecord`).
fn ephemeral_usage_payload(purpose: &str, response: &pa_types::ai::AssistantMessage) -> Value {
    let reply = completion_reply(response);
    let mut completion = serde_json::Map::new();
    // TS counts UTF-16 code units (`reply.length`); longer text is left out, not truncated.
    if reply.encode_utf16().count() <= EPHEMERAL_COMPLETION_TEXT_MAX_CHARS {
        completion.insert("reply".into(), Value::String(reply));
    }
    let mut payload = json!({
        "purpose": purpose,
        "api": response.api,
        "provider": response.provider,
        "model": response.model,
        "usage": response.usage,
        "stopReason": response.stop_reason,
        "completion": Value::Object(completion),
    });
    // The provider's own timing is journaled alongside, as TS does.
    for key in ["duration", "ttft"] {
        if let Some(value) = response.rest.get(key) {
            payload[key] = value.clone();
        }
    }
    if let Some(message) = &response.error_message {
        payload["errorMessage"] = Value::String(message.clone());
    }
    payload
}

/// TS `completionRecord`'s reply: the text blocks, or — for a call answered through a forced tool
/// call, which has no text — its arguments as JSON lines.
fn completion_reply(response: &pa_types::ai::AssistantMessage) -> String {
    let text = crate::features::btw::message_text(response);
    if !text.is_empty() {
        return text;
    }
    let mut reply = String::new();
    for block in &response.content {
        if let pa_types::ai::AssistantContentBlock::ToolCall(call) = block {
            if !reply.is_empty() {
                reply.push('\n');
            }
            reply.push_str(&Value::Object(call.arguments.clone()).to_string());
        }
    }
    reply
}

/// TS `dedupeEphemeralReply`: collapse runs of one repeated line longer than three, then bound the
/// text to `max_bytes` on a UTF-8 character boundary.
fn dedupe_ephemeral_reply(text: &str, max_bytes: u64) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let run = j - i;
        if run > 3 {
            out.push(lines[i].to_string());
            out.push(format!("[…{run}×]"));
        } else {
            out.extend(std::iter::repeat_n(lines[i].to_string(), run));
        }
        i = j;
    }
    let result = out.join("\n");
    if result.len() as u64 <= max_bytes {
        return result;
    }
    let suffix = "\n[…truncated]";
    let bytes = result.as_bytes();
    let mut cut = (max_bytes.saturating_sub(suffix.len() as u64) as usize).min(bytes.len());
    // Back off continuation bytes so a multi-byte character straddling the budget is dropped whole.
    while cut > 0 && bytes[cut] & 0xc0 == 0x80 {
        cut -= 1;
    }
    format!("{}{suffix}", &result[..cut])
}

/// TS `EphemeralTurnResult`; the assistant message never carries tool calls (a side turn has no
/// tools to run, so a provider that emitted one has it dropped).
fn ephemeral_result(reply_text: &str, response: &pa_types::ai::AssistantMessage) -> Value {
    let mut sanitized = response.clone();
    sanitized
        .content
        .retain(|block| !matches!(block, pa_types::ai::AssistantContentBlock::ToolCall(_)));
    json!({
        "replyText": reply_text,
        "assistantMessage": sanitized,
    })
}

/// TS `normalizeCustomMessagePayload` + the `#promptAgent` wrapper: a `before_agent_start` message
/// payload becomes a `role: "custom"` message (attribution `user` unless the payload sets one).
fn injected_message(payload: &Value) -> Option<AgentMessage> {
    let (custom_type, content, display, details, attribution) = match payload {
        Value::String(text) => ("message".to_string(), json!(text), true, None, None),
        Value::Object(o) => (
            o.get("customType")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .to_string(),
            o.get("content").cloned().unwrap_or_else(|| json!("")),
            o.get("display").and_then(Value::as_bool).unwrap_or(false),
            o.get("details").cloned(),
            o.get("attribution")
                .and_then(Value::as_str)
                .filter(|a| matches!(*a, "user" | "agent")),
        ),
        _ => return None,
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let mut payload = json!({
        "customType": custom_type,
        "content": content,
        "display": display,
        "attribution": attribution.unwrap_or("user"),
        "timestamp": timestamp,
    });
    if let Some(d) = details {
        payload["details"] = d;
    }
    Some(AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
        role: "custom".to_string(),
        payload,
    }))
}

/// Live session facts the host ctx exposes synchronously (TS `ctx.isIdle()`,
/// `getContextUsage()`, `hasPendingMessages()`, `sessionManager.getBranch()`, ...). Refreshed by
/// [`Extensions::refresh_live`] at event boundaries; the host-side provider
/// ([`omp_ext::ExtHost::set_ctx_provider`]) rides it on every emit.
#[derive(Default)]
struct Live {
    snap: Mutex<serde_json::Map<String, Value>>,
    /// Branch entries (root -> leaf) not yet sent to the host.
    branch: Mutex<Option<Value>>,
    /// Session file len the cached branch was computed at.
    branch_len: Mutex<Option<(PathBuf, u64)>>,
    /// Some loaded extension reads the branch (`getBranch`/`getEntry`/`getLeafId`/`getEntries`).
    needs_branch: std::sync::atomic::AtomicBool,
}

/// TS `MAX_PENDING_CREDENTIAL_DISABLED` (runner.ts): the backlog kept for the window between the
/// auth store's automatic soft-disable and the session that emits it.
const MAX_PENDING_CREDENTIAL_DISABLED: usize = 32;

/// Bridge from the auth store's process-wide listener to the host (TS `runner.ts`'s
/// `#pendingCredentialDisabled` + `emitCredentialDisabled`): the store tears a credential down
/// before, or long after, a runtime is attached, so events queue here and one drainer task emits
/// them in order from the moment the session is live.
#[derive(Default)]
struct CredentialDisabledFeed {
    queue: Mutex<Vec<CredentialDisabledEvent>>,
    /// Wakes the drainer. `notify_one` keeps a permit, so a push that lands between a drain and
    /// its `await` cannot be lost.
    wake: tokio::sync::Notify,
    /// The drainer runs once per session.
    draining: AtomicBool,
    /// Held for the session's lifetime: dropping it unsubscribes from the store.
    sub: Mutex<Option<CredentialDisabledSubscription>>,
}

impl CredentialDisabledFeed {
    /// Buffer one event; the store's listener may run on any thread, so this stays synchronous.
    fn push(&self, event: CredentialDisabledEvent) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.len() >= MAX_PENDING_CREDENTIAL_DISABLED {
            queue.remove(0);
        }
        queue.push(event);
        drop(queue);
        self.wake.notify_one();
    }

    fn take(&self) -> Vec<CredentialDisabledEvent> {
        std::mem::take(
            &mut *self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Live extension host and its agent-facing hooks.
pub struct Extensions {
    pub host: Arc<ExtHost>,
    hooks: ExtHooks,
    /// Kept alive for the session's lifetime (drop unsubscribes).
    sub: Mutex<Option<Subscription>>,
    turn: Arc<AtomicU64>,
    /// `session_stop` continuation bookkeeping (TS agent-session fields).
    stop: Arc<Mutex<StopState>>,
    /// Session cwd (extension `load` param), for reconnect recovery.
    cwd: PathBuf,
    /// Explicit `--extension` paths, for reconnect recovery.
    extra: Vec<PathBuf>,
    /// Default extensions dir, for reconnect recovery.
    dir: Option<String>,
    /// Last session ctx sent to the host.
    ctx: Mutex<SessionCtx>,
    mode: Mutex<ExtMode>,
    /// `(base prompt, override applied)` of the last `before_agent_start` system-prompt override.
    prompt_override: Mutex<Option<(String, String)>>,
    /// Live per-event ctx facts (see [`Live`]).
    live: Arc<Live>,
    /// Runtime attached via [`Extensions::attach`]; re-served on reconnect.
    rt: Mutex<Option<Arc<Runtime>>>,
    /// Tool calls an extension `tool_call` handler blocked: their result events are delivered to
    /// extensions in TS's order (see `subscribe_lifecycle`).
    blocked_calls: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Print/json runs: TS reaches extensions with `agent_end` only after `session_shutdown`
    /// (its settle path emits it detached), so the messages wait here for [`Self::shutdown`].
    deferred_agent_end: Arc<Mutex<Option<Vec<Value>>>>,
    /// Automatic credential soft-disables on their way to extensions (see
    /// [`CredentialDisabledFeed`]).
    credential_disabled: Arc<CredentialDisabledFeed>,
    /// MCP server notifications (`mcp_notification`), buffered until the session ctx is wired.
    mcp: McpNotificationSink,
}

/// `resources_discover` round trip: emit (only when an extension subscribes, TS `hasHandlers`),
/// then register every returned path with the loader that owns it. Extracted from
/// [`Extensions::resources_discover`] so the fake-host tests can drive it.
async fn discover_resources(host: &ExtHost, cwd: &Path, reason: ResourcesDiscoverReason) {
    if !host.has_handlers("resources_discover") {
        return;
    }
    let wire_cwd = cwd.to_string_lossy().into_owned();
    match host.emit_resources_discover(wire_cwd, reason, None).await {
        Ok(outcome) => apply_discovered_resources(&outcome, cwd),
        Err(e) => eprintln!("omt: extension resources_discover: {e:#}"),
    }
}

/// Register a `resources_discover` outcome with the loaders that own each kind of path: skills feed
/// the system-prompt skill listing, prompt templates become file slash commands, themes join the
/// theme loader's catalog (`ExtensionRunner.emitResourcesDiscover` shapes; the loader split is tau's).
fn apply_discovered_resources(outcome: &ResourcesDiscoverOutcome, cwd: &Path) {
    let resolve = |paths: &[DiscoveredPath]| -> Vec<PathBuf> {
        paths
            .iter()
            .map(|p| resolve_discovered_path(&p.path, cwd))
            .collect()
    };
    crate::sysprompt::set_extension_skill_paths(resolve(&outcome.skill_paths));
    crate::rpc::slash::set_extension_prompt_paths(resolve(&outcome.prompt_paths));
    omp_tui::theme::set_extension_theme_paths(resolve(&outcome.theme_paths));
}

/// Upstream `resolvePath`: `~` expands to the home directory, an absolute path stays as given,
/// anything else resolves against the session cwd.
fn resolve_discovered_path(raw: &str, cwd: &Path) -> PathBuf {
    let expanded = if raw == "~" {
        std::env::var_os("HOME").map_or_else(|| PathBuf::from(raw), PathBuf::from)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map_or_else(|| PathBuf::from(raw), |home| PathBuf::from(home).join(rest))
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// `session_before_compact` / `session.compacting` / `session_compact` emission around runtime
/// compactions.
struct CompactBridge {
    host: Arc<ExtHost>,
}

impl crate::runtime::CompactionHooks for CompactBridge {
    fn before<'a>(
        &'a self,
        preparation: Value,
        branch_entries: Vec<Value>,
        custom_instructions: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if !self.host.has_handlers("session_before_compact") {
                return false;
            }
            match self
                .host
                .emit_session_before_compact(
                    preparation,
                    branch_entries,
                    custom_instructions.map(str::to_owned),
                    None,
                )
                .await
            {
                Ok(outcome) => outcome.cancel,
                Err(e) => {
                    eprintln!("omt: extension session_before_compact: {e:#}");
                    false
                }
            }
        })
    }

    fn compacting<'a>(
        &'a self,
        session_id: &'a str,
        messages: Vec<Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<CompactingOutcome>> + Send + 'a>>
    {
        Box::pin(async move {
            if !self.host.has_handlers("session.compacting") {
                return None;
            }
            match self
                .host
                .emit_session_compacting(session_id.to_owned(), messages, None)
                .await
            {
                Ok(outcome) => Some(outcome),
                Err(e) => {
                    eprintln!("omt: extension session.compacting: {e:#}");
                    None
                }
            }
        })
    }

    fn after<'a>(
        &'a self,
        compaction_entry: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            emit_silent(
                self.host
                    .emit_session_compact(compaction_entry, false, None)
                    .await,
            );
        })
    }
}

impl Live {
    /// ctx merged into the host on each emit: the cheap snapshot, plus the branch once per change.
    fn provide(&self) -> Option<Value> {
        let mut out = self
            .snap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(b) = self
            .branch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            out.insert(
                "leafId".into(),
                b.as_array()
                    .and_then(|a| a.last())
                    .and_then(|e| e.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            out.insert("branch".into(), b);
        }
        (!out.is_empty()).then_some(Value::Object(out))
    }
}

/// Whether any extension source reads the session branch (so the core must feed it).
fn sources_read_branch(dir: Option<&str>, extra: &[PathBuf]) -> bool {
    fn scan(p: &Path, depth: usize) -> bool {
        if p.is_dir() {
            if depth == 0 {
                return false;
            }
            return std::fs::read_dir(p).is_ok_and(|rd| {
                rd.flatten()
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .any(|e| scan(&e.path(), depth - 1))
            });
        }
        p.extension().is_some_and(|e| e == "ts" || e == "js")
            && std::fs::read_to_string(p).is_ok_and(|t| {
                ["getBranch", "getEntries", "getEntry(", "getLeafId"]
                    .iter()
                    .any(|k| t.contains(k))
            })
    }
    dir.is_some_and(|d| scan(Path::new(d), 3)) || extra.iter().any(|p| scan(p, 3))
}

impl Extensions {
    /// Spawn the host and load extensions (`dir` = `~/.omp/agent/extensions`,
    /// `extra` = repeatable `--extension` paths).
    ///
    /// # Errors
    /// Host spawn or `load` transport failure (load errors on individual
    /// extensions are reported by the host and do not fail here).
    pub async fn start(cwd: &Path, extra: &[PathBuf]) -> Result<Arc<Self>> {
        let script = host_script().context("ext-host/host.ts not found (set OMP_RS_EXT_HOST)")?;
        let mut cfg = HostConfig::new(script);
        cfg.trace = std::env::var_os("OMP_EXT_TRACE").map(Into::into);
        // Shared host by default: one bun daemon per user serves all sessions;
        // each session still runs its extensions in an isolated bun Worker
        // (fresh module graph == per-process state). `OMP_RS_EXT_HOST=private`
        // keeps the per-process host.
        cfg.shared_socket = shared_socket_path(&cfg.host_script, extra);
        let host = ExtHost::spawn(&cfg).await?;
        let hooks = ExtHooks::new(host.clone());

        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join(".omp").join("agent").join("extensions"))
            .filter(|d| d.is_dir())
            .map(|d| d.to_string_lossy().into_owned());
        let paths: Vec<PathBuf> = extra.to_vec();
        let loaded = host
            .load(
                if paths.is_empty() { None } else { Some(&paths) },
                dir.as_deref(),
                &cwd.to_string_lossy(),
                Some(json!({"cwd": cwd.to_string_lossy(), "hasUI": false, "branch": []})),
            )
            .await?;
        for err in &loaded.errors {
            eprintln!("omt: extension failed to load: {}: {}", err.path, err.error);
        }

        let ext = Arc::new(Self {
            mcp: McpNotificationSink::new(host.clone()),
            host,
            hooks,
            sub: Mutex::new(None),
            turn: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(Mutex::new(StopState::default())),
            cwd: cwd.to_path_buf(),
            extra: extra.to_vec(),
            dir,
            ctx: Mutex::new(SessionCtx::default()),
            mode: Mutex::new(ExtMode::Print),
            prompt_override: Mutex::new(None),
            live: Arc::new(Live::default()),
            rt: Mutex::new(None),
            blocked_calls: Arc::new(Mutex::new(std::collections::HashSet::new())),
            deferred_agent_end: Arc::new(Mutex::new(None)),
            credential_disabled: Arc::new(CredentialDisabledFeed::default()),
        });
        {
            // The store keeps the events of a session that has not subscribed yet, and hands its
            // backlog to the first subscriber: subscribing here covers both windows.
            let feed = ext.credential_disabled.clone();
            let sub = on_credential_disabled(move |event| feed.push(event.clone()));
            *ext.credential_disabled
                .sub
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sub);
        }

        // MCP managers publish every server notification here, whether they were
        // created before or after this session (omp `sdk.ts` bridges the manager's
        // `addNotificationListener` into each session's runner).
        register_mcp_notifications(ext.mcp.clone());
        ext.live.needs_branch.store(
            sources_read_branch(ext.dir.as_deref(), &ext.extra),
            Ordering::Relaxed,
        );
        {
            let live = ext.live.clone();
            ext.host
                .set_ctx_provider(Some(Arc::new(move |_ty: &str| live.provide())));
        }
        if ext.host.may_reconnect() {
            // Shared-daemon supervisor: on daemon death the host transparently
            // falls back to a private host; re-load extensions, re-send the
            // session ctx, re-emit `session_start` (as TS does on extension
            // reload), and re-serve host->core messages.
            let ext = ext.clone();
            tokio::spawn(async move { ext.supervise_reconnect().await });
        }
        Ok(ext)
    }

    /// `pa-agent` hook points backed by the host.
    #[must_use]
    pub fn hook_fns(&self) -> (TransformContextFn, BeforeToolCallFn, AfterToolCallFn) {
        let inner = self.hooks.before_tool_call();
        let blocked = self.blocked_calls.clone();
        let before: BeforeToolCallFn = Arc::new(move |ctx, signal| {
            let id = ctx.tool_call.id.clone();
            let (fut, blocked) = (inner(ctx, signal), blocked.clone());
            Box::pin(async move {
                let result = fut.await?;
                if result.as_ref().is_some_and(|r| r.block) {
                    blocked
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(id);
                }
                Ok(result)
            })
        });
        let inner_after = self.hooks.after_tool_call();
        let after: AfterToolCallFn = Arc::new(move |ctx, signal| {
            // A call the approval gate refused never ran, so (as in TS) `tool_result` is not emitted.
            if crate::approval::Gate::get().is_some_and(|g| g.take_refused(&ctx.tool_call.id)) {
                return Box::pin(async { Ok(None) });
            }
            inner_after(ctx, signal)
        });
        // While an ultra mode runs CLM, `context-slim` stands aside: the core
        // owns context maintenance then.
        let transform = self
            .hooks
            .transform_context_skipping(Arc::new(crate::context::skip_extensions));
        (transform, before, after)
    }

    /// The real outbound payload at the provider boundary: every
    /// `before_provider_request` handler sees the previous handler's payload, and a
    /// returned payload replaces it, so the rewrite lands in the request that is
    /// actually sent (TS `emitBeforeProviderRequest`). `None` keeps it unchanged.
    pub fn before_provider_request(&self, payload: Value) -> Option<Value> {
        provider_payload(&self.host, payload)
    }

    /// Whether any loaded extension subscribes to `cache_warming_decision`: the warmer emits
    /// nothing otherwise.
    #[must_use]
    pub fn cache_warming_handlers(&self) -> bool {
        self.host.has_handlers("cache_warming_decision")
    }

    /// Emit `cache_warming_decision`; the last handler's `action` overrides the warmer's own.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn cache_warming_decision(
        &self,
        event: &CacheWarmingDecisionEvent,
    ) -> Result<CacheWarmingOutcome> {
        self.host.emit_cache_warming_decision(event, None).await
    }

    /// Extension tools as `AgentTool` wrappers (`list_tools` -> `call_tool`).
    ///
    /// # Errors
    /// Transport failure on `list_tools`.
    pub async fn tools(&self) -> Result<Vec<Arc<dyn AgentTool>>> {
        Ok(self
            .host
            .list_tools()
            .await?
            .into_iter()
            .map(|info| {
                Arc::new(ExtTool {
                    host: self.host.clone(),
                    info,
                }) as Arc<dyn AgentTool>
            })
            .collect())
    }

    /// Merge session info into the host's ctx state. Also sends the stored
    /// `sessionName` (see [`Self::set_session_name`]) so the merged state
    /// stays complete across reconnects.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn set_session_ctx(
        &self,
        session_id: &str,
        session_file: Option<&Path>,
        model: &str,
    ) -> Result<()> {
        {
            let mut ctx = self
                .ctx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ctx.session_id = session_id.to_string();
            ctx.session_file = session_file.map(Path::to_path_buf);
            ctx.model = model.to_string();
        }
        self.send_ctx().await?;
        // The host now carries this session's ctx, so buffered MCP notifications can
        // run their handlers against it (omp drains `#pendingMcpNotifications` once
        // `initialize()` wired the runtime context).
        self.mcp.attach();
        Ok(())
    }

    /// Update the session title in the host's ctx state (`host.ts set_ctx`
    /// merges into state → `getSessionName()`), so extensions like
    /// herdr-pane-names see title changes without re-reading the session file.
    /// `None` clears the title. Safe to call on every title change: the value
    /// is stored and the full merged ctx is re-sent. Before the first
    /// `set_session_ctx` the update is stored only (nothing to merge into).
    ///
    /// # Errors
    /// Transport failure.
    pub async fn set_session_name(&self, name: Option<String>) -> Result<()> {
        let ready = {
            let mut ctx = self
                .ctx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ctx.session_name = name;
            !ctx.session_id.is_empty()
        };
        if ready {
            self.send_ctx().await?;
        }
        Ok(())
    }

    /// Send the stored session ctx (id/file/model/name) to the host.
    ///
    /// # Errors
    /// Transport failure.
    async fn send_ctx(&self) -> Result<()> {
        let body = {
            let ctx = self
                .ctx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            json!({
                "sessionId": ctx.session_id,
                "sessionFile": ctx.session_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
                "sessionName": ctx.session_name,
                "hasUI": ctx.has_ui,
                "mode": ctx.mode,
            })
        };
        self.host.set_ctx(body).await
    }

    /// `session_start` (once per session).
    pub async fn session_start(&self) {
        emit_silent(self.host.emit_session_start(None).await);
    }

    /// `resources_discover`: extensions contribute skill, prompt-template and theme paths. Emitted
    /// at startup once the extensions are loaded (before the session's system prompt is assembled,
    /// so contributed skills reach it) and again on every extension reload.
    ///
    /// Only emitted when an extension subscribes (`has_handlers`), like every other event.
    pub async fn resources_discover(&self, reason: ResourcesDiscoverReason) {
        discover_resources(&self.host, &self.cwd, reason).await;
    }

    /// TS `before_agent_start` (agent-session `#promptAgent`, every mode): handlers may
    /// replace the system prompt and inject messages ahead of the prompt.
    ///
    /// # Errors
    /// Transport failure; handler outcomes are applied best-effort.
    pub async fn before_agent_start(
        &self,
        agent: &Agent,
        prompt: &str,
        images: Option<Value>,
    ) -> Result<Vec<AgentMessage>> {
        // TS keeps the override per turn (`setTurnSystemPromptOverride`, cleared on the next
        // prompt): start every prompt from the base prompt, never from the last override.
        let mut base = agent.system_prompt().await;
        let restore = {
            let mut applied = self
                .prompt_override
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            applied
                .take()
                .filter(|(_, set)| base == *set)
                .map(|(orig, _)| orig)
        };
        if let Some(orig) = restore {
            agent.set_system_prompt(orig.clone()).await;
            base = orig;
        }
        let outcome = self
            .host
            .emit_before_agent_start(prompt.to_string(), images, vec![base.clone()], None)
            .await?;
        if let Some(list) = outcome.system_prompt {
            let joined = list.join("\n\n");
            *self
                .prompt_override
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((base, joined.clone()));
            agent.set_system_prompt(joined).await;
        }
        // Injected messages ride the prompt (TS `#promptAgent`: `[user, ...injected]`), so they get
        // message events and are persisted like any prompt message.
        Ok(outcome
            .messages
            .iter()
            .filter_map(injected_message)
            .collect())
    }

    /// TS `input` gate (interactive input-controller / rpc ordered-input dispatch; never print
    /// mode): handlers may rewrite the text or consume the input.
    ///
    /// # Errors
    /// Transport failure.
    pub async fn input_gate(&self, prompt: &str, source: InputSource) -> Result<PromptGate> {
        let input = self
            .host
            .emit_input(prompt.to_string(), None, source, None)
            .await?;
        if input.handled {
            return Ok(PromptGate::Handled);
        }
        Ok(PromptGate::Run(
            input.text.unwrap_or_else(|| prompt.to_string()),
        ))
    }

    /// Attach to a running runtime: serve host->core notifications
    /// (`append_entry` -> session writer, `send_message` -> queue,
    /// ui notifications -> `extension_ui_request` frames) and subscribe
    /// lifecycle forwarding (`agent_*`, `turn_*`, `message_*`,
    /// `tool_execution_*`, `session_stop`).
    pub fn attach(self: &Arc<Self>, rt: Arc<Runtime>) {
        *self
            .rt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rt.clone());
        self.serve_incoming(&rt);
        rt.set_compaction_hooks(Some(Arc::new(CompactBridge {
            host: self.host.clone(),
        })));
        // `before_agent_start` runs inside every prompt admission, like TS `#promptAgent`.
        let ext = self.clone();
        let weak = Arc::downgrade(&rt);
        rt.set_before_start_hook(Some(Arc::new(
            move |prompt: String, images: Option<Value>| {
                let (ext, weak) = (ext.clone(), weak.clone());
                Box::pin(async move {
                    let Some(rt) = weak.upgrade() else {
                        return Vec::new();
                    };
                    ext.before_agent_start(&rt.agent, &prompt, images)
                        .await
                        .unwrap_or_else(|e| {
                            eprintln!("omt: extension before_agent_start: {e:#}");
                            Vec::new()
                        })
                })
            },
        )));
        let ext = self.clone();
        tokio::spawn(async move {
            ext.refresh_live(&rt, true).await;
            let sub = ext.subscribe_lifecycle(rt).await;
            *ext.sub
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sub);
            // Hold the subscription for the session's lifetime.
            std::future::pending::<()>().await;
        });
    }

    /// Serve host->core messages on the host's current incoming channel.
    /// Called by [`Self::attach`] and again on every reconnect (the fallback
    /// host has a fresh channel; the previous consumer's task ends when its
    /// sender drops).
    fn responder(&self, rt: &Arc<Runtime>) -> Arc<dyn omp_ext::lifecycle::HostResponder> {
        let mode = *self
            .mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::new(RuntimeResponder {
            rt: Arc::downgrade(rt),
            mode,
        })
    }

    /// Declare the serving mode before [`Self::attach`]: sets `ctx.mode` / `hasUI` like TS
    /// (`print`/`json`: no UI; `rpc` and `tui`: a UI context exists).
    fn mode(&self) -> ExtMode {
        *self
            .mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn set_mode(&self, mode: ExtMode) {
        *self
            .mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
        let mut ctx = self
            .ctx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ctx.has_ui = mode.has_ui();
        ctx.mode = mode.name();
    }

    fn serve_incoming(self: &Arc<Self>, rt: &Arc<Runtime>) {
        if let Some(incoming) = self.host.take_incoming() {
            let host = self.host.clone();
            let runtime = rt.clone();
            let mut notifications = lifecycle::serve(&host, incoming, self.responder(rt));
            tokio::spawn(async move {
                while let Some(msg) = notifications.recv().await {
                    handle_notification(&runtime, msg);
                }
            });
        }
    }

    /// Wait for shared-daemon-death reconnects and recover the session on
    /// each: re-load extensions, re-send the session ctx, re-emit
    /// `session_start` (as TS does on extension reload), resume serving
    /// host->core messages. Emits keep flowing afterwards; a failure is
    /// logged and the next prompt retries recovery on the following event.
    async fn supervise_reconnect(self: Arc<Self>) {
        let mut generations = self.host.reconnections();
        loop {
            if generations.changed().await.is_err() {
                return;
            }
            eprintln!("omt: extension host reconnected; reloading extensions and session ctx");
            if let Err(e) = self.recover().await {
                eprintln!("omt: extension host recovery: {e:#}");
            }
        }
    }

    /// Re-establish session state on a transparently reconnected host.
    ///
    /// # Errors
    /// Transport failure on reload or ctx re-send.
    async fn recover(self: &Arc<Self>) -> Result<()> {
        let loaded = self
            .host
            .load(
                if self.extra.is_empty() {
                    None
                } else {
                    Some(&self.extra)
                },
                self.dir.as_deref(),
                &self.cwd.to_string_lossy(),
                Some(json!({"cwd": self.cwd.to_string_lossy(), "hasUI": false, "branch": []})),
            )
            .await?;
        for err in &loaded.errors {
            eprintln!("omt: extension failed to load: {}: {}", err.path, err.error);
        }
        self.send_ctx().await?;
        *self
            .live
            .branch_len
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        emit_silent(self.host.emit_session_start(None).await);
        // A reload re-sends the whole discovered resource set (TS emits `resources_discover` on
        // extension reload); the loaders keep only the newest set.
        self.resources_discover(ResourcesDiscoverReason::Reload)
            .await;
        if let Some(rt) = self
            .rt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            self.serve_incoming(&rt);
        }
        Ok(())
    }

    /// Full per-mode wiring on top of [`Self::attach`]: `ctx.mode`/`hasUI` (TS: UI present in tui
    /// and non-headless rpc), slash-command registration, a switch listener that refreshes the ext
    /// ctx (id/file/model/name) and emits `session_switch` for `/new`, `/resume` and `/fork`, the
    /// title listener, then the initial ctx push and `session_start`. Call it once the startup
    /// resume (if any) is loaded.
    pub async fn attach_session(self: &Arc<Self>, rt: Arc<Runtime>, mode: ExtMode) {
        self.set_mode(mode);
        rt.set_ext_host(Some(self.host.clone()));
        self.attach(rt.clone());
        crate::rpc::slash::set_extensions(self.clone());
        rt.set_name_listener(Some(self.name_listener()));
        let ext = self.clone();
        let weak = Arc::downgrade(&rt);
        rt.set_switch_listener(Some(Arc::new(move |reason, previous| {
            let (ext, weak) = (ext.clone(), weak.clone());
            tokio::spawn(async move {
                let Some(rt) = weak.upgrade() else { return };
                ext.push_session_ctx(&rt).await;
                crate::features::modes::notify(&rt);
                // A branch/fork-into-file move emits `session_branch` from the caller (TS).
                if reason == "branch" {
                    return;
                }
                let reason = match reason {
                    "new" => SwitchReason::New,
                    "fork" => SwitchReason::Fork,
                    _ => SwitchReason::Resume,
                };
                let previous = previous.map(|p| p.to_string_lossy().into_owned());
                emit_silent(ext.host.emit_session_switch(reason, previous, None).await);
            });
        })));
        let host = self.host.clone();
        rt.set_switch_gate(Some(Arc::new(move |reason, target| {
            let host = host.clone();
            Box::pin(async move {
                if !host.has_handlers("session_before_switch") {
                    return false;
                }
                let reason = match reason {
                    "new" => SwitchReason::New,
                    "fork" => SwitchReason::Fork,
                    _ => SwitchReason::Resume,
                };
                let target = target.map(|p| p.to_string_lossy().into_owned());
                match host.emit_session_before_switch(reason, target, None).await {
                    Ok(o) => o.cancel,
                    Err(e) => {
                        eprintln!("omt: extension session_before_switch: {e:#}");
                        false
                    }
                }
            })
        })));
        self.push_session_ctx(&rt).await;
        crate::features::modes::notify(&rt);
        self.session_start().await;
        self.start_credential_disabled_feed();
    }

    /// Refresh the live ctx facts from the runtime. `with_branch` also re-reads the session
    /// branch (only when an extension reads it, and only when the journal grew).
    async fn refresh_live(&self, rt: &Runtime, with_branch: bool) {
        let usage = rt.context_usage_value().await;
        let streaming = rt.agent.is_streaming().await;
        {
            let mut snap = self
                .live
                .snap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.insert("idle".into(), json!(!streaming));
            snap.insert("pending".into(), json!(rt.has_pending_messages()));
            snap.insert("contextUsage".into(), usage.unwrap_or(Value::Null));
            snap.insert("model".into(), rt.model_info());
            snap.insert(
                "asyncJobs".into(),
                crate::tools::session_jobs().map_or(Value::Null, |j| j.async_snapshot(5)),
            );
        }
        if with_branch && self.live.needs_branch.load(Ordering::Relaxed) {
            self.refresh_branch(rt);
        }
    }

    fn refresh_branch(&self, rt: &Runtime) {
        let path = rt.session_file();
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        {
            let mut seen = self
                .live
                .branch_len
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if seen.as_ref().is_some_and(|(p, l)| *p == path && *l == len) {
                return;
            }
            *seen = Some((path, len));
        }
        let Ok(entries) = rt.entries() else { return };
        let branch = omp_session::tree::leaf_id(&entries)
            .map(|leaf| {
                omp_session::tree::branch_path(&entries, leaf)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<Value>>()
            })
            .unwrap_or_default();
        *self
            .live
            .branch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Value::Array(branch));
    }

    /// Send the runtime's current session id/file/model/name to the host.
    async fn push_session_ctx(&self, rt: &Runtime) {
        // A new session invalidates the cached branch.
        *self
            .live
            .branch_len
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.refresh_live(rt, true).await;
        let model = rt.agent.model().await;
        let model_ref = format!("{}/{}", model.provider, model.id);
        if let Err(e) = self
            .set_session_ctx(
                &rt.session_id(),
                rt.live_session_file().as_deref(),
                &model_ref,
            )
            .await
        {
            eprintln!("omt: extension ctx: {e:#}");
        }
        if let Err(e) = self.set_session_name(rt.session_name()).await {
            eprintln!("omt: extension ctx: {e:#}");
        }
    }

    /// Name-change sink: every title change re-sends the ext ctx with the new
    /// `sessionName` so hosts reading `getSessionName()` see it at once.
    pub fn name_listener(self: &Arc<Self>) -> crate::runtime::NameListener {
        let ext = self.clone();
        Arc::new(move |name: &str| {
            let ext = ext.clone();
            let name = name.to_string();
            tokio::spawn(async move {
                let _ = ext.set_session_name(Some(name)).await;
            });
        })
    }

    /// `rpc::run_with` `OnStart` hook form of [`Self::attach`].
    pub fn rpc_on_start(self: &Arc<Self>) -> crate::rpc::OnStart {
        let ext = self.clone();
        Box::new(move |rt: Arc<Runtime>| {
            Box::pin(async move { ext.attach_session(rt, ExtMode::Rpc).await })
        })
    }

    /// Emit `session_shutdown` and stop the host.
    pub async fn shutdown(&self) {
        emit_silent(self.host.emit_session_shutdown(None).await);
        let deferred = self
            .deferred_agent_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(messages) = deferred {
            emit_silent(self.host.emit_agent_end(messages, None, None).await);
        }
        if let Err(e) = self.host.shutdown().await {
            eprintln!("omt: extension host shutdown: {e:#}");
        }
    }

    /// Emit the credential soft-disables the auth store reported before this session attached, then
    /// every later one as it arrives (TS `runner.ts`'s `emitCredentialDisabled`, whose pre-init
    /// buffer is [`CredentialDisabledFeed`]). Runs once per session, after `session_start`.
    fn start_credential_disabled_feed(self: &Arc<Self>) {
        let feed = self.credential_disabled.clone();
        if feed.draining.swap(true, Ordering::Relaxed) {
            return;
        }
        let ext = self.clone();
        tokio::spawn(async move {
            loop {
                flush_credential_disabled(&ext.host, &feed).await;
                feed.wake.notified().await;
            }
        });
    }

    /// Link schemes the loaded extensions claimed with `pi.registerLinkScheme()`, in load order
    /// (first claimer first). The TUI offers exactly these links for `link_activate`.
    #[must_use]
    pub fn link_schemes(&self) -> Vec<String> {
        self.host.link_schemes()
    }

    /// Deliver a click on `url` as `link_activate` to the extension that claimed its scheme.
    /// `false` when no extension owns the link.
    ///
    /// # Errors
    /// Transport failure to the extension host.
    pub async fn activate_link(&self, url: &str) -> Result<bool> {
        self.host.link_activate(url).await
    }
}

/// Emit every buffered credential soft-disable in order (TS `runner.ts`'s
/// `emitCredentialDisabled`, which drains `#pendingCredentialDisabled` once initialized).
async fn flush_credential_disabled(host: &ExtHost, feed: &CredentialDisabledFeed) {
    for event in feed.take() {
        emit_silent(
            host.emit_credential_disabled(
                event.provider,
                event.disabled_cause,
                event.credential_id,
                event.email,
                event.account_id,
                event.org_id,
                event.org_name,
                None,
            )
            .await,
        );
    }
}

fn emit_silent(r: Result<omp_ext::EmitResult>) {
    match r {
        Ok(r) => {
            for err in r.errors {
                eprintln!("omt: extension handler error: {err}");
            }
        }
        Err(e) => eprintln!("omt: extension emit: {e:#}"),
    }
}

fn handle_notification(rt: &Arc<Runtime>, msg: HostMessage) {
    let HostMessage::Notification { method, params } = msg else {
        return;
    };
    match method.as_str() {
        "append_entry" => {
            let entry = json!({
                "extension": params.get("extension").cloned().unwrap_or(Value::Null),
                "customType": params.get("customType").cloned().unwrap_or(Value::Null),
                "data": params.get("data").cloned().unwrap_or(Value::Null),
            });
            if let Err(e) = rt.append_entry("custom", entry) {
                eprintln!("omt: extension append_entry: {e:#}");
            }
        }
        "send_message" | "send_user_message" => {
            let text = params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !text.is_empty() {
                rt.extension_message(&text);
            }
        }
        "log" => {
            if let Some(line) = params.get("message").and_then(Value::as_str) {
                eprintln!("ext: {line}");
            }
        }
        "ui_notify" | "tool_update" | "set_label" => {
            rt.send_extension_frame(
                params.get("extension").and_then(Value::as_str),
                &method,
                &params,
            );
        }
        _ => {}
    }
}

/// Cap on `mcp_notification` frames buffered before the session ctx is wired (omp
/// `MAX_PENDING_MCP_NOTIFICATIONS`); drop-oldest under pressure.
const MAX_PENDING_MCP_NOTIFICATIONS: usize = 100;

/// One session's MCP notification state: the host that runs the handlers, whether the
/// session ctx has been sent yet, and the frames received before it was.
struct McpNotificationState {
    host: Arc<ExtHost>,
    attached: AtomicBool,
    pending: Mutex<VecDeque<LifecycleEvent>>,
}

/// Sink for MCP server notifications: every connected server's frames reach
/// [`Extensions`] here, which emits them as `mcp_notification` (omp `emitMcpNotification`).
#[derive(Clone)]
pub struct McpNotificationSink {
    state: Arc<McpNotificationState>,
}

impl McpNotificationSink {
    fn new(host: Arc<ExtHost>) -> Self {
        Self {
            state: Arc::new(McpNotificationState {
                host,
                attached: AtomicBool::new(false),
                pending: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// One JSON-RPC notification from `server`, after the runtime handled it. Buffered
    /// while the session ctx is not wired yet; emitted at once afterwards.
    pub fn publish(&self, server: String, method: String, params: Value) {
        let event = LifecycleEvent::McpNotification {
            server,
            method,
            params,
        };
        if !self.state.attached.load(Ordering::Relaxed) {
            let mut pending = self
                .state
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() >= MAX_PENDING_MCP_NOTIFICATIONS {
                pending.pop_front();
            }
            pending.push_back(event);
            return;
        }
        self.emit(event);
    }

    /// Session ctx wired: emit the frames that arrived earlier, in arrival order.
    fn attach(&self) {
        if self.state.attached.swap(true, Ordering::Relaxed) {
            return;
        }
        let pending: Vec<LifecycleEvent> = self
            .state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for event in pending {
            self.emit(event);
        }
    }

    /// Observe-only emit: the host runs the handlers serially in wire order and no
    /// result is read back (`ext-host/host.ts` "everything else: observe").
    fn emit(&self, event: LifecycleEvent) {
        notify(&self.state.host, "mcp_notification", || event);
    }
}

/// Sessions forwarding MCP notifications. Weak, so a dropped session unregisters itself.
static MCP_NOTIFICATION_SINKS: Mutex<Vec<std::sync::Weak<McpNotificationState>>> =
    Mutex::new(Vec::new());

fn register_mcp_notifications(sink: McpNotificationSink) {
    let mut sinks = MCP_NOTIFICATION_SINKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sinks.retain(|weak| weak.strong_count() > 0);
    sinks.push(Arc::downgrade(&sink.state));
}

/// Publish one server notification to every live extension session, like omp's
/// `sdk.ts` bridge from `MCPManager.addNotificationListener` into each session's runner.
pub fn publish_mcp_notification(notification: &omp_mcp::ServerNotification) {
    let sinks: Vec<Arc<McpNotificationState>> = {
        let mut list = MCP_NOTIFICATION_SINKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        list.retain(|weak| weak.strong_count() > 0);
        list.iter().filter_map(std::sync::Weak::upgrade).collect()
    };
    for state in sinks {
        McpNotificationSink { state }.publish(
            notification.server.clone(),
            notification.method.clone(),
            notification.params.clone(),
        );
    }
}

impl Extensions {
    /// Forward agent events to host lifecycle handlers. Handler errors are
    /// logged, never fatal to the run.
    async fn subscribe_lifecycle(self: &Arc<Self>, rt: Arc<Runtime>) -> Subscription {
        // `session_stop` runs from the loop's continuation hook: it fires when
        // the agent would otherwise stop, so `decision: "block"` /
        // `continue: true` can hand back a hidden next-turn message and the
        // run continues instead of settling (TS `#emitSessionStopEvent`).
        let hook_host = self.host.clone();
        let hook_stop = self.stop.clone();
        let hook_session_id = rt.session_id();
        let hook_session_file = rt.session_file().to_string_lossy().into_owned();
        let hook_turn = self.turn.clone();
        rt.agent.set_continuation_hook(Some(Arc::new(
            move |ctx: GetContinuationMessagesContext, _signal: AbortSignal| {
                let host = hook_host.clone();
                let stop = hook_stop.clone();
                let session_id = hook_session_id.clone();
                let session_file = hook_session_file.clone();
                let turn = hook_turn.clone();
                Box::pin(async move {
                    Ok(session_stop_continuation(
                        &host,
                        &stop,
                        &session_id,
                        &session_file,
                        &turn,
                        &ctx.new_messages,
                    )
                    .await)
                })
            },
        )));

        let host = self.host.clone();
        let turn = self.turn.clone();
        let stop = self.stop.clone();
        // One encoder per subscription: the agent delivers events to this
        // listener in order, and `forward_event` enqueues each emit while
        // holding the encoder, so encode order == wire order.
        let updates = Arc::new(Mutex::new(ExtUpdateEncoder::default()));
        // TS delivers `tool_execution_end` to extensions after the same call's tool-result
        // `message_start` (its session event path awaits before emitting the end), so the end is
        // held back one event. For a call an extension blocked the start is late too: the whole
        // `message_start` / `tool_execution_start` / `tool_execution_end` burst of an immediate
        // result reaches extensions as message_start, start, end.
        let held: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let blocked_calls = self.blocked_calls.clone();
        let deferred_end = self.deferred_agent_end.clone();
        let ext = self.clone();
        let weak = Arc::downgrade(&rt);
        rt.agent
            .subscribe(move |event, _| {
                let (ext, weak) = (ext.clone(), weak.clone());
                let host = host.clone();
                let turn = turn.clone();
                let stop = stop.clone();
                let updates = updates.clone();
                let held = held.clone();
                let blocked_calls = blocked_calls.clone();
                let deferred_end = deferred_end.clone();
                Box::pin(async move {
                    if let Some(rt) = weak.upgrade() {
                        let boundary = matches!(
                            event,
                            AgentEvent::AgentStart | AgentEvent::TurnStart | AgentEvent::AgentEnd { .. }
                        );
                        if boundary
                            || matches!(
                                event,
                                AgentEvent::MessageEnd { .. }
                                    | AgentEvent::TurnEnd { .. }
                                    | AgentEvent::ToolExecutionEnd { .. }
                            )
                        {
                            ext.refresh_live(&rt, boundary).await;
                        }
                    }
                    let mut pending: Vec<AgentEvent> = std::mem::take(
                        &mut *held.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                    if let AgentEvent::ToolExecutionEnd { tool_call_id: end_id, .. } = &event {
                        if pending.iter().any(|e| matches!(e, AgentEvent::ToolExecutionStart { tool_call_id, .. } if tool_call_id == end_id)) {
                            pending.push(event);
                            *held.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = pending;
                            return Ok(());
                        }
                    }
                    // A run's input messages (prompt, injected, queued) reach extensions as all
                    // starts first, then all ends: TS lets `message_end` lag `message_start`.
                    let input_role = |m: &AgentMessage| matches!(m.role(), "user" | "custom" | "developer");
                    if !pending.is_empty()
                        && pending.iter().all(|e| matches!(e, AgentEvent::MessageEnd { message } if input_role(message)))
                        && matches!(&event, AgentEvent::MessageStart { message } if input_role(message))
                    {
                        forward_event(&host, &turn, &stop, &updates, &event);
                        *held.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = pending;
                        return Ok(());
                    }
                    if !pending.is_empty() {
                        let owns_result = |id: &str| {
                            pending.iter().any(|e| {
                                matches!(e, AgentEvent::ToolExecutionStart { tool_call_id, .. }
                                    | AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == id)
                            })
                        };
                        let result_start = matches!(
                            &event,
                            AgentEvent::MessageStart { message: AgentMessage::Standard(pa_agent::types::Message::ToolResult(r)) }
                                if owns_result(&r.tool_call_id)
                        );
                        if result_start {
                            forward_event(&host, &turn, &stop, &updates, &event);
                        }
                        for e in &pending {
                            forward_event(&host, &turn, &stop, &updates, e);
                        }
                        if result_start {
                            return Ok(());
                        }
                    }
                    if let AgentEvent::AgentEnd { messages } = &event {
                        if matches!(ext.mode(), ExtMode::Print | ExtMode::Json) {
                            *deferred_end.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(messages.iter().map(message_value).collect());
                            return Ok(());
                        }
                    }
                    let delay = match &event {
                        AgentEvent::ToolExecutionEnd { .. } => true,
                        AgentEvent::MessageEnd { message } => input_role(message),
                        AgentEvent::ToolExecutionStart { tool_call_id, .. } => blocked_calls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .contains(tool_call_id),
                        _ => false,
                    };
                    if delay {
                        held.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
                        return Ok(());
                    }
                    forward_event(&host, &turn, &stop, &updates, &event);
                    Ok(())
                })
            })
            .await
    }

    /// [`Self::before_provider_request`] as the loop's payload hook
    /// (`StreamRequestOptions::on_payload`): the provider calls it with the payload it
    /// is about to send.
    pub fn payload_hook(self: &Arc<Self>) -> pa_agent::stream::OnPayloadHook {
        let ext = Arc::clone(self);
        Arc::new(move |payload: Value, _model: &pa_agent::types::Model| {
            ext.before_provider_request(payload)
        })
    }
}

/// [`Extensions::before_provider_request`] for the requests that build their own
/// `SimpleStreamOptions` instead of going through the loop (compaction, side turns);
/// upstream wires the same hook there.
pub fn ai_payload_hook(host: Arc<ExtHost>) -> pa_ai::types::OnPayloadHook {
    Arc::new(move |payload: Value, _model: &pa_types::ai::Model| provider_payload(&host, payload))
}

/// One `before_provider_request` round trip: nothing when no extension subscribes
/// (`has_handlers`), the handlers' chained replacement otherwise. A transport failure
/// leaves the payload alone and is reported, like a failing handler in TS.
fn provider_payload(host: &Arc<ExtHost>, payload: Value) -> Option<Value> {
    if !host.has_handlers("before_provider_request") {
        return None;
    }
    let event = json!({"type": "before_provider_request", "payload": payload});
    match host.emit_blocking(event, None) {
        Ok(result) => result.result.get("payload").cloned(),
        Err(error) => {
            eprintln!("omt: before_provider_request: {error:#}");
            None
        }
    }
}

/// Emit `session_stop` from the would-stop point and translate its outcome
/// into a hidden continuation message (TS `#emitSessionStopEvent` +
/// `#queueHiddenNextTurnMessage`). `decision: "block"` stops the run from
/// settling with the blocked prompt; `continue: true` schedules one more turn
/// carrying `additionalContext`/`reason`.
async fn session_stop_continuation(
    host: &ExtHost,
    stop: &Mutex<StopState>,
    session_id: &str,
    session_file: &str,
    turn: &AtomicU64,
    new_messages: &[AgentMessage],
) -> Vec<AgentMessage> {
    if !host.has_handlers("session_stop") {
        // Same end state as an empty outcome below, without serializing the run's messages.
        *stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = StopState::default();
        return Vec::new();
    }
    let (hook_active, continuations) = {
        let s = stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (s.hook_active, s.continuations)
    };
    let event = LifecycleEvent::SessionStop {
        messages: new_messages.iter().map(message_value).collect(),
        turn_id: turn.load(Ordering::SeqCst),
        last_assistant_message: new_messages
            .iter()
            .rev()
            .find(|m| m.role() == "assistant")
            .map(message_value),
        session_id: session_id.to_string(),
        session_file: Some(session_file.to_string()),
        stop_hook_active: hook_active,
    };
    let outcome = match host.emit_session_stop(event, None).await {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("omt: extension session_stop: {e:#}");
            return Vec::new();
        }
    };
    let Some(text) = stop_continuation_text(&outcome) else {
        *stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = StopState::default();
        return Vec::new();
    };
    {
        let mut s = stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !outcome.block {
            if continuations >= SESSION_STOP_CONTINUATION_CAP {
                eprintln!("omt: session_stop continuation cap reached");
                *s = StopState::default();
                return Vec::new();
            }
            s.continuations += 1;
        }
        s.hook_active = true;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    vec![AgentMessage::Custom(CustomAgentMessage {
        role: "developer".to_string(),
        payload: json!({
            "customType": "session-stop-continuation",
            "content": [{"type": "text", "text": text}],
            "display": false,
            "attribution": "agent",
            "timestamp": timestamp,
        }),
    })]
}

/// Queue one notification-only lifecycle event (fire-and-forget, ordered).
///
/// Every event forwarded here is observe-only in the host (`ext-host/host.ts`
/// `emit` falls through to its "everything else: observe" branch: no handler
/// return value is read), so the turn loop need not wait for the handlers.
/// The wire order is the call order and the host runs these emits strictly
/// serially, and ahead of any later awaited `emit`/`call_tool`. `build` runs
/// only when some extension subscribes to `ty`, so unsubscribed events cost no
/// serialization at all.
fn notify(host: &ExtHost, ty: &str, build: impl FnOnce() -> LifecycleEvent) {
    if !host.has_handlers(ty) {
        return;
    }
    if let Err(e) = host.emit_nowait(&build().to_value(), None, emit_silent) {
        eprintln!("omt: extension emit: {e:#}");
    }
}

/// The session stream's provider-response observation hook: TS `sdk.ts` wires
/// `SimpleStreamOptions.onResponse` to `emitAfterProviderResponse`, so an extension sees every
/// provider response once the HTTP response has landed and before its stream body is read.
/// Observe-only (the runner discards handler results), so the emit rides [`notify`] instead of
/// blocking the provider hook, which is sync by contract.
#[must_use]
pub fn after_provider_response_hook(host: Arc<ExtHost>) -> pa_agent::stream::OnResponseHook {
    Arc::new(
        move |response: pa_agent::stream::ProviderResponse, _model: &pa_agent::types::Model| {
            // TS `normalizeProviderResponse` is handed the provider's request id
            // (`request-id` for Anthropic, `x-request-id` elsewhere); this core's
            // `ProviderResponse` carries headers only, so it is read back from them exactly as
            // the providers do.
            let request_id = response
                .headers
                .get("request-id")
                .or_else(|| response.headers.get("x-request-id"))
                .cloned();
            notify(&host, "after_provider_response", || {
                LifecycleEvent::AfterProviderResponse {
                    status: response.status,
                    headers: response.headers,
                    request_id,
                }
            });
        },
    )
}

/// Compose [`after_provider_response_hook`] onto `base`: a response hook the request already
/// carried runs first, then the extension observation (TS `agent-loop.ts` wraps the user's
/// `onResponse` the same way).
#[must_use]
pub fn with_response_hook(
    base: pa_agent::stream::StreamFn,
    hook: pa_agent::stream::OnResponseHook,
) -> pa_agent::stream::StreamFn {
    Arc::new(move |model, context, mut options| {
        let hook = hook.clone();
        let composed: pa_agent::stream::OnResponseHook = match options.on_response.take() {
            Some(existing) => Arc::new(move |response, model| {
                existing(response.clone(), model);
                hook(response, model);
            }),
            None => hook,
        };
        options.on_response = Some(composed);
        base(model, context, options)
    })
}

/// Queue one `message_update` as a delta frame against the message the host
/// rebuilt from the previous frame (`ExtUpdateEncoder`; `ext-host/host.ts`
/// `rebuildMessageUpdate` restores the full event before any handler runs).
fn notify_update(
    host: &ExtHost,
    updates: &Mutex<ExtUpdateEncoder>,
    message: &AgentMessage,
    ame: &pa_agent::stream::AssistantMessageEvent,
) {
    let mut enc = updates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !host.has_handlers("message_update") {
        // Nothing reaches the host: its chain (if any) is stale from here on.
        enc.invalidate();
        return;
    }
    enc.set_generation(host.generation());
    let frame = enc.encode(message, ame);
    let chain = ExtUpdateEncoder::chain_id(&frame);
    let resync = enc.resync_flag();
    let sent = host.emit_nowait(&frame, None, move |r| {
        // `{resync: true}`: the host could not apply the frame (it skipped the
        // handlers); a failed emit leaves the host state unknown. Either way
        // the next update of this chain goes out full; a late answer about an
        // older chain leaves a newer one alone.
        let needs_full = match &r {
            Ok(out) => out.result.get("resync").and_then(Value::as_bool) == Some(true),
            Err(_) => true,
        };
        if let (true, Some(chain)) = (needs_full, chain) {
            resync.fetch_max(chain, Ordering::AcqRel);
        }
        emit_silent(r);
    });
    if let Err(e) = sent {
        enc.invalidate();
        eprintln!("omt: extension emit: {e:#}");
    }
}

#[allow(clippy::too_many_lines)] // one arm per `AgentEvent` variant; splitting adds indirection.
fn forward_event(
    host: &ExtHost,
    turn: &AtomicU64,
    stop: &Mutex<StopState>,
    updates: &Mutex<ExtUpdateEncoder>,
    event: &AgentEvent,
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    match event {
        AgentEvent::AgentStart => {
            // A new run resets the session_stop continuation bookkeeping
            // (TS `#resetSessionStopContinuationState` on generation change).
            *stop
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = StopState::default();
            notify(host, "agent_start", || LifecycleEvent::AgentStart);
        }
        AgentEvent::AgentEnd { messages } => {
            // `session_stop` itself is emitted from the continuation hook
            // (see `subscribe_lifecycle`), where its block/continue outcome can
            // still keep the run alive.
            notify(host, "agent_end", || LifecycleEvent::AgentEnd {
                messages: messages.iter().map(message_value).collect(),
                will_continue: None,
            });
            if host.has_handlers("agent_settled") {
                if let Err(e) =
                    host.emit_nowait(&json!({"type":"agent_settled"}), None, emit_silent)
                {
                    eprintln!("omt: extension agent_settled: {e:#}");
                }
            }
        }
        AgentEvent::TurnStart => {
            let index = turn.fetch_add(1, Ordering::SeqCst) + 1;
            notify(host, "turn_start", || LifecycleEvent::TurnStart {
                turn_index: index,
                timestamp: now_ms,
            });
        }
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            notify(host, "turn_end", || LifecycleEvent::TurnEnd {
                turn_index: turn.load(Ordering::SeqCst),
                message: message_value(message),
                tool_results: tool_results
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<Vec<Value>, _>>()
                    .unwrap_or_default(),
            });
        }
        AgentEvent::MessageStart { message } => {
            // Sync point: a new message always starts a fresh delta chain.
            updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .message_boundary();
            notify(host, "message_start", || LifecycleEvent::MessageStart {
                message: message_value(message),
            });
        }
        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => notify_update(host, updates, message, assistant_message_event),
        AgentEvent::MessageEnd { message } => {
            updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .message_boundary();
            notify(host, "message_end", || LifecycleEvent::MessageEnd {
                message: message_value(message),
            });
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            notify(host, "tool_execution_start", || {
                LifecycleEvent::ToolExecutionStart {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    intent: None,
                }
            });
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => {
            notify(host, "tool_execution_update", || {
                LifecycleEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    partial_result: serde_json::to_value(partial_result).unwrap_or(Value::Null),
                }
            });
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            notify(host, "tool_execution_end", || {
                LifecycleEvent::ToolExecutionEnd {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: serde_json::to_value(result).unwrap_or(Value::Null),
                    is_error: *is_error,
                }
            });
        }
    }
}

/// An extension-declared tool proxied to the host's `call_tool`.
struct ExtTool {
    host: Arc<ExtHost>,
    info: ToolInfo,
}

impl AgentTool for ExtTool {
    fn name(&self) -> &str {
        &self.info.name
    }
    fn description(&self) -> &str {
        self.info.description.as_deref().unwrap_or("")
    }
    fn parameters(&self) -> &Value {
        &self.info.parameters
    }
    fn label(&self) -> &str {
        self.info.label.as_deref().unwrap_or(&self.info.name)
    }
    fn execute(
        self: Arc<Self>,
        tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: pa_agent::types::AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move {
            let host = self.host.clone();
            let call = host.call_tool(&self.info.name, &tool_call_id, params, None);
            let value = tokio::select! {
                biased;
                // A caller abort must not leave the handler running in the
                // host: cancel it there too (`cancel_tool`), like an
                // AbortSignal reaching a TS extension tool.
                () = signal.aborted() => {
                    let _ = host.cancel_tool(&tool_call_id).await;
                    bail!("extension tool '{}' cancelled", self.info.name);
                }
                res = tokio::time::timeout(tool_timeout(), call) => res
                    .map_err(|_| anyhow::anyhow!("extension tool '{}' timed out", self.info.name))??,
            };
            let content: Vec<ToolResultContent> = value
                .get("content")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            let is_error = value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let details = value.get("details").cloned().unwrap_or(Value::Null);
            if is_error {
                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(anyhow::anyhow!(if text.is_empty() {
                    format!("extension tool '{}' returned an error", self.info.name)
                } else {
                    text
                }));
            }
            Ok(AgentToolResult {
                content,
                details,
                terminate: None,
                is_error: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_agent::abort::AbortController;

    async fn sh_host(
        dir: &std::path::Path,
        body: &str,
        env: Vec<(String, String)>,
    ) -> Arc<ExtHost> {
        let script = dir.join("fake-host.sh");
        std::fs::write(&script, body).unwrap();
        ExtHost::spawn(&HostConfig {
            bun: "/bin/sh".into(),
            host_script: script,
            trace: None,
            handler_timeout_ms: None,
            env,
            shared_socket: None,
        })
        .await
        .unwrap()
    }

    /// A fake host that appends every request line to `$LOG` and answers each request: `load`
    /// with no extensions loaded, everything else with an empty emit outcome. Reading the log
    /// after an awaited request is deterministic: the host runs queued emits in wire order.
    const LOG_HOST: &str = concat!(
        "while IFS= read -r line; do\n",
        "  printf '%s\\n' \"$line\" >> \"$LOG\"\n",
        "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
        "  case \"$line\" in\n",
        "    *'\"method\":\"load\"'*) result='{\"loaded\":[],\"errors\":[]}';;\n",
        "    *) result='{\"result\":null,\"handlers\":[],\"errors\":[]}';;\n",
        "  esac\n",
        "  if [ -n \"$id\" ]; then\n",
        "    printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":%s}\\n' \"$id\" \"$result\"\n",
        "  fi\n",
        "done\n",
    );

    /// The observation hook the session stream installs (`sdk.ts` `onResponse` →
    /// `emitAfterProviderResponse`) hands the provider's status, headers and request id to the
    /// host once the response lands.
    #[tokio::test]
    async fn after_provider_response_reaches_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let host = sh_host(
            dir.path(),
            LOG_HOST,
            vec![("LOG".into(), log.display().to_string())],
        )
        .await;
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("request-id".to_string(), "req_abc".to_string());
        headers.insert("content-type".to_string(), "text/event-stream".to_string());
        let hook = after_provider_response_hook(host.clone());
        hook(
            pa_agent::stream::ProviderResponse {
                status: 429,
                headers,
            },
            &pa_agent::types::Model::unknown(),
        );
        // Barrier: the host answers requests in order, so its reply to this awaited emit proves
        // the queued observation above already reached it.
        host.emit(json!({"type": "session_shutdown"}), None)
            .await
            .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        let line = logged
            .lines()
            .find(|line| line.contains("after_provider_response"))
            .expect("after_provider_response reached the host");
        let value: Value = serde_json::from_str(line).unwrap();
        let event = &value["params"]["event"];
        assert_eq!(event["type"], json!("after_provider_response"));
        assert_eq!(event["status"], json!(429));
        assert_eq!(event["headers"]["content-type"], json!("text/event-stream"));
        assert_eq!(event["headers"]["request-id"], json!("req_abc"));
        assert_eq!(event["requestId"], json!("req_abc"));
        host.shutdown().await.unwrap();
    }

    /// With no `after_provider_response` subscriber the hook stays silent (`hasHandlers` gate).
    #[tokio::test]
    async fn after_provider_response_skipped_without_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let host = sh_host(
            dir.path(),
            LOG_HOST,
            vec![("LOG".into(), log.display().to_string())],
        )
        .await;
        // `load` with no extensions: nothing subscribes, so the gate is closed.
        host.load(None, None, "/tmp", None).await.unwrap();
        assert!(!host.has_handlers("after_provider_response"));
        let hook = after_provider_response_hook(host.clone());
        hook(
            pa_agent::stream::ProviderResponse {
                status: 200,
                headers: std::collections::BTreeMap::default(),
            },
            &pa_agent::types::Model::unknown(),
        );
        host.emit(json!({"type": "session_shutdown"}), None)
            .await
            .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(!logged.contains("after_provider_response"), "{logged}");
        host.shutdown().await.unwrap();
    }

    /// The session stream composes the observation onto the response hook a request already
    /// carried, and both run (TS `agent-loop.ts` wraps the caller's `onResponse` the same way).
    #[tokio::test]
    async fn session_stream_composes_provider_response_observation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let host = sh_host(
            dir.path(),
            LOG_HOST,
            vec![("LOG".into(), log.display().to_string())],
        )
        .await;
        let caller_hook_calls = Arc::new(AtomicU64::new(0));
        let seen = caller_hook_calls.clone();
        // Stands in for the provider: fires the request's response hook exactly where `pa-ai`
        // does, right after the HTTP response and before the body is read.
        let base: pa_agent::stream::StreamFn = Arc::new(move |_model, _context, options| {
            Box::pin(async move {
                if let Some(hook) = &options.on_response {
                    let mut headers = std::collections::BTreeMap::new();
                    headers.insert("x-request-id".to_string(), "req_xyz".to_string());
                    hook(
                        pa_agent::stream::ProviderResponse {
                            status: 503,
                            headers,
                        },
                        &pa_agent::types::Model::unknown(),
                    );
                }
                let no_body: anyhow::Result<Box<dyn pa_agent::stream::ModelStream>> = Err(
                    anyhow::anyhow!("no body: the hook is what this test observes"),
                );
                no_body
            })
        });
        let stream = with_response_hook(base, after_provider_response_hook(host.clone()));
        let options = pa_agent::stream::StreamRequestOptions {
            on_response: Some(Arc::new(move |_response, _model| {
                seen.fetch_add(1, Ordering::SeqCst);
            })),
            ..pa_agent::stream::StreamRequestOptions::default()
        };
        let _ = stream(
            pa_agent::types::Model::unknown(),
            pa_agent::stream::LlmContext::default(),
            options,
        )
        .await;
        assert_eq!(
            caller_hook_calls.load(Ordering::SeqCst),
            1,
            "the caller's response hook still runs"
        );
        host.emit(json!({"type": "session_shutdown"}), None)
            .await
            .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        let line = logged
            .lines()
            .find(|line| line.contains("after_provider_response"))
            .expect("observation reached the host");
        let value: Value = serde_json::from_str(line).unwrap();
        let event = &value["params"]["event"];
        assert_eq!(event["status"], json!(503));
        assert_eq!(event["requestId"], json!("req_xyz"));
        host.shutdown().await.unwrap();
    }

    /// Answers every request with a `session_stop` outcome of
    /// `continue: true, additionalContext: "keep going"`.
    const CONTINUE_HOST: &str = concat!(
        "while IFS= read -r line; do\n",
        "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
        "  if [ -n \"$id\" ]; then\n",
        "    printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"result\":{\"continue\":true,\"additionalContext\":\"keep going\"},\"handlers\":[],\"errors\":[]}}\n' \"$id\"\n",
        "  fi\n",
        "done\n",
    );

    /// `run_ephemeral_turn` contract: options are validated before inference
    /// (TS `EphemeralTurnOptions`), the prompt follows the detached history.
    #[test]
    fn ext_ephemeral_turn_validates_options_before_inference() {
        let err = |v: Value| EphemeralTurn::parse(&v).unwrap_err();
        assert!(err(json!({})).contains("promptText"));
        assert!(err(json!({"promptText": "  "})).contains("promptText"));
        assert!(err(json!({"promptText": "q", "maxTokens": 0})).contains("maxTokens"));
        assert!(err(json!({"promptText": "q", "maxTokens": 1.5})).contains("maxTokens"));
        assert!(
            err(json!({"promptText": "q", "maxTokens": 9_007_199_254_740_992_u64}))
                .contains("maxTokens")
        );
        assert!(err(json!({"promptText": "q", "tools": true})).contains("tools"));
        assert!(err(json!({"promptText": "q", "history": "x"})).contains("history"));
        assert!(err(json!({"promptText": "q", "promptImages": {}})).contains("promptImages"));
        assert!(err(json!({"promptText": "q", "maxContextBytes": 0})).contains("maxContextBytes"));
        assert!(
            err(json!({"promptText": "q", "maxContextBytes": 9_007_199_254_740_992_u64}))
                .contains("maxContextBytes")
        );
        assert!(err(json!({"promptText": "q", "replyMaxBytes": 1.5})).contains("replyMaxBytes"));
        assert!(err(json!({"promptText": "q", "minimizeEffort": "yes"})).contains("minimizeEffort"));
        assert!(err(json!({"promptText": "q", "dedupeReply": 1})).contains("dedupeReply"));
        assert!(err(json!({"promptText": "q", "conversationKey": 3})).contains("conversationKey"));
        assert!(err(json!({"promptText": "q", "purpose": {}})).contains("purpose"));
        assert!(err(json!({"promptText": "q", "streamId": true})).contains("streamId"));

        let bare = EphemeralTurn::parse(&json!({"promptText": "why?", "tools": false})).unwrap();
        assert_eq!(bare.max_tokens, EPHEMERAL_DEFAULT_MAX_TOKENS);
        assert_eq!(bare.tail.len(), 1);
        assert_eq!(bare.max_context_bytes, None);
        assert!(
            !bare.minimize_effort,
            "effort is the session's unless minimized"
        );
        assert_eq!(bare.conversation_key, None);
        assert_eq!(bare.purpose, None);
        assert!(
            bare.dedupe_reply,
            "a reply is deduped unless the caller opts out"
        );
        assert_eq!(bare.reply_max_bytes, EPHEMERAL_REPLY_MAX_BYTES);
        assert_eq!(bare.stream_id, None, "no onTextDelta, no stream");

        let options = EphemeralTurn::parse(&json!({
            "promptText": "q",
            "maxContextBytes": 1024,
            "minimizeEffort": true,
            "conversationKey": "topic",
            "purpose": "recap",
            "dedupeReply": false,
            "replyMaxBytes": 64,
            "streamId": 7,
            "usageCallback": true,
        }))
        .unwrap();
        assert_eq!(options.max_context_bytes, Some(1024));
        assert!(options.minimize_effort);
        assert_eq!(options.conversation_key.as_deref(), Some("topic"));
        assert_eq!(options.purpose.as_deref(), Some("recap"));
        assert!(!options.dedupe_reply);
        assert_eq!(options.reply_max_bytes, 64);
        assert_eq!(options.stream_id, Some(json!(7)));
        assert!(options.usage_callback);

        let full = EphemeralTurn::parse(&json!({
            "promptText": "and now?",
            "maxTokens": 64,
            "history": [
                {"role": "user", "content": "first", "timestamp": 1},
            ],
            "promptImages": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}],
        }))
        .unwrap();
        assert_eq!(full.max_tokens, 64);
        assert_eq!(full.tail.len(), 2, "history first, prompt last");
        let prompt = serde_json::to_value(&full.tail[1]).unwrap();
        assert_eq!(prompt["role"], "user");
        assert_eq!(
            prompt["content"][0],
            json!({"type": "text", "text": "and now?"})
        );
        assert_eq!(prompt["content"][1]["mimeType"], "image/png");
        let first = serde_json::to_value(&full.tail[0]).unwrap();
        assert_eq!(first["content"], "first");
    }

    /// `EphemeralTurnResult`: `replyText` is the trimmed text of the reply's text blocks.
    #[test]
    fn ext_ephemeral_turn_result_shape() {
        let response: pa_types::ai::AssistantMessage = serde_json::from_value(json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": " it is "},
                {"type": "text", "text": "fine "},
            ],
            "api": "anthropic-messages",
            "provider": "anthropic",
            "model": "m",
            "usage": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": "stop",
            "timestamp": 5,
        }))
        .unwrap();
        let result = ephemeral_result("it is fine", &response);
        assert_eq!(result["replyText"], "it is fine");
        assert_eq!(result["assistantMessage"]["model"], "m");
        assert_eq!(result["assistantMessage"]["stopReason"], "stop");
    }

    /// TS `dedupeEphemeralReply`: runs of one repeated line longer than three collapse, and an
    /// over-budget reply is cut on a UTF-8 boundary and marked.
    #[test]
    fn ext_ephemeral_dedupe_reply_collapses_repeats_and_bounds_bytes() {
        assert_eq!(dedupe_ephemeral_reply("", 4096), "");
        assert_eq!(dedupe_ephemeral_reply("a\nb\nb\nb", 4096), "a\nb\nb\nb");
        assert_eq!(
            dedupe_ephemeral_reply("a\nb\nb\nb\nb\nc", 4096),
            "a\nb\n[…4×]\nc"
        );
        let long = "가".repeat(10);
        let bounded = dedupe_ephemeral_reply(&long, 20);
        assert!(bounded.ends_with("\n[…truncated]"));
        let kept = bounded.trim_end_matches("\n[…truncated]");
        assert_eq!(
            kept, "가",
            "a character straddling the budget is dropped whole"
        );
        assert!(bounded.len() <= 20);
    }

    /// TS `journalCompletion`: a side turn with a `purpose` journals its settled attempt as a
    /// `model_usage` entry, keeping the reply only while it is small and a forced tool call's
    /// arguments when there is no text.
    #[test]
    fn ext_ephemeral_purpose_journals_the_settled_attempt() {
        let payload = ephemeral_usage_payload("recap", &streamed_message("hi"));
        assert_eq!(payload["purpose"], "recap");
        assert_eq!(payload["model"], "a");
        assert_eq!(payload["stopReason"], "stop");
        assert_eq!(payload["completion"]["reply"], "hi");
        assert!(payload.get("errorMessage").is_none());

        // A reply past the journal's ceiling is left out, never truncated.
        let long = ephemeral_usage_payload("recap", &streamed_message(&"x".repeat(4001)));
        assert!(long["completion"].get("reply").is_none());
        let at_limit = ephemeral_usage_payload("recap", &streamed_message(&"x".repeat(4000)));
        assert_eq!(
            at_limit["completion"]["reply"].as_str().unwrap().len(),
            4000
        );

        // A call answered through a forced tool call has no text: its arguments are the reply.
        let forced: pa_types::ai::AssistantMessage = serde_json::from_value(json!({
            "content": [{"type": "toolCall", "id": "t", "name": "answer",
                         "arguments": {"text": "it is fine"}}],
            "api": "openai-completions", "provider": "p", "model": "a",
            "usage": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": "error", "errorMessage": "boom", "timestamp": 5
        }))
        .unwrap();
        let failed = ephemeral_usage_payload("recap", &forced);
        assert_eq!(failed["completion"]["reply"], "{\"text\":\"it is fine\"}");
        assert_eq!(failed["errorMessage"], "boom");
    }

    /// A `purpose` names the journal entry, and a host that reports usage itself (`onUsage`) already
    /// counted the attempt, so the core journals nothing then (TS `args.onUsage ?? journalCompletion`).
    #[test]
    fn ext_ephemeral_journals_only_an_unreported_purpose() {
        let parse = |extra: Value| {
            let mut request = json!({"promptText": "q"});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            EphemeralTurn::parse(&request).unwrap()
        };
        assert_eq!(
            parse(json!({"purpose": "recap"})).journal_purpose(),
            Some("recap")
        );
        assert_eq!(
            parse(json!({"purpose": "recap", "usageCallback": true})).journal_purpose(),
            None,
            "the host's own onUsage reporter wins"
        );
        assert_eq!(
            parse(json!({"usageCallback": true})).journal_purpose(),
            None
        );
        assert_eq!(parse(json!({})).journal_purpose(), None);
    }

    /// TS `maxContextBytes`: the encoded provider context must fit the caller's ceiling.
    #[test]
    fn ext_ephemeral_turn_context_ceiling_rejects_a_large_context() {
        let messages = vec![serde_json::from_value(
            json!({"role": "user", "content": "hello", "timestamp": 1}),
        )
        .unwrap()];
        let context = crate::features::btw::side_context(messages);
        let turn =
            EphemeralTurn::parse(&json!({"promptText": "q", "maxContextBytes": 16})).unwrap();
        assert!(turn
            .check_context_bytes(&context)
            .unwrap_err()
            .contains("16-byte limit"));
        let roomy =
            EphemeralTurn::parse(&json!({"promptText": "q", "maxContextBytes": 4096})).unwrap();
        assert!(roomy.check_context_bytes(&context).is_ok());
        let unlimited = EphemeralTurn::parse(&json!({"promptText": "q"})).unwrap();
        assert!(unlimited.check_context_bytes(&context).is_ok());
    }

    /// TS `sideSessionId`: a `conversationKey` keeps one provider lineage; without one every side
    /// turn gets its own.
    #[test]
    fn ext_ephemeral_side_session_id_keys_a_conversation() {
        let keyed =
            EphemeralTurn::parse(&json!({"promptText": "q", "conversationKey": "topic"})).unwrap();
        assert_eq!(keyed.side_session_id("s1"), "s1:side:conversation:topic");
        assert_eq!(
            keyed.side_session_id("s1"),
            keyed.side_session_id("s1"),
            "a keyed lineage is stable across calls"
        );
        let unkeyed = EphemeralTurn::parse(&json!({"promptText": "q"})).unwrap();
        let first = unkeyed.side_session_id("s1");
        assert!(first.starts_with("s1:side:") && !first.contains("conversation"));
        assert_ne!(first, unkeyed.side_session_id("s1"));
    }

    /// TS `minimizeEffort` runs a side turn at the model's lowest effort; without it the turn asks
    /// for the session's level, and a model without a thinking surface asks for nothing.
    #[test]
    fn ext_ephemeral_minimize_effort_lowers_the_requested_level() {
        use pa_types::ai::ModelThinkingLevel;
        let model: pa_types::ai::Model = serde_json::from_value(json!({
            "id": "a", "name": "a", "api": "openai-completions", "provider": "p",
            "baseUrl": "http://127.0.0.1:9", "reasoning": true, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 100_000, "maxTokens": 4096
        }))
        .unwrap();
        let mut options = crate::features::btw::SideTurnOptions {
            max_tokens: 64,
            thinking_level: ModelThinkingLevel::High,
            minimize_effort: false,
            session_id: None,
        };
        assert_eq!(
            crate::features::btw::side_turn_thinking_level(&model, &options),
            Some(ModelThinkingLevel::High)
        );
        options.minimize_effort = true;
        assert_eq!(
            crate::features::btw::side_turn_thinking_level(&model, &options),
            Some(ModelThinkingLevel::Minimal)
        );
        let mut plain = model;
        plain.reasoning = false;
        assert_eq!(
            crate::features::btw::side_turn_thinking_level(&plain, &options),
            None
        );
    }

    /// Collects the deltas one side turn delivers.
    struct Recording(std::sync::Mutex<Vec<String>>);

    impl crate::features::btw::SideTurnObserver for Recording {
        fn text_delta<'a>(
            &'a self,
            delta: &'a str,
        ) -> crate::features::btw::BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.0.lock().unwrap().push(delta.to_string());
                Ok(())
            })
        }
    }

    /// Refuses every delivery (a host whose `onTextDelta` threw).
    struct Refusing;

    impl crate::features::btw::SideTurnObserver for Refusing {
        fn text_delta<'a>(
            &'a self,
            _delta: &'a str,
        ) -> crate::features::btw::BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Err("the callback threw".into()) })
        }
    }

    fn streamed_message(text: &str) -> pa_types::ai::AssistantMessage {
        serde_json::from_value(json!({
            "content": [{"type": "text", "text": text}],
            "api": "openai-completions", "provider": "p", "model": "a",
            "usage": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": "stop", "timestamp": 5
        }))
        .unwrap()
    }

    /// Every text delta reaches the observer in the provider's order, and the delivered deltas are
    /// exactly the reply's text (TS `onTextDelta` + `providerReplyText`).
    #[tokio::test]
    async fn ext_ephemeral_stream_delivers_every_delta_in_order() {
        use pa_types::ai::{AssistantMessageEvent, DoneStopReason};
        let (writer, stream) = pa_ai::AssistantMessageEventStream::new();
        let partial = streamed_message("");
        let deltas = ["Hel", "lo, ", "world"];
        for delta in deltas {
            writer.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: delta.into(),
                partial: partial.clone(),
            });
        }
        writer.push(AssistantMessageEvent::Done {
            reason: DoneStopReason::Stop,
            message: streamed_message("Hello, world"),
        });
        let seen = Recording(std::sync::Mutex::new(Vec::new()));
        let Ok(reply) = crate::features::btw::pump_side_stream(stream, &seen).await else {
            panic!("the stream settles");
        };
        let seen = seen.0.into_inner().unwrap();
        assert_eq!(
            seen,
            deltas.iter().map(|d| d.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(seen.concat(), reply.text);
        assert!(matches!(
            reply.outcome,
            crate::features::btw::SideTurn::Done(_)
        ));
    }

    /// A delta the observer refuses aborts the side turn (TS: the awaited `onTextDelta` rejects).
    #[tokio::test]
    async fn ext_ephemeral_stream_stops_when_a_delta_is_not_delivered() {
        use pa_types::ai::AssistantMessageEvent;
        let (writer, stream) = pa_ai::AssistantMessageEventStream::new();
        writer.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".into(),
            partial: streamed_message("x"),
        });
        let Err(err) = crate::features::btw::pump_side_stream(stream, &Refusing).await else {
            panic!("a refused delivery fails the turn");
        };
        assert!(
            matches!(err, crate::features::btw::SideTurnError::Request(m) if m.contains("not delivered"))
        );
    }

    /// A core without a side-turn implementation answers with an explicit error,
    /// never a silent empty reply.
    #[tokio::test]
    async fn ext_ephemeral_turn_headless_core_rejects_explicitly() {
        let reply = omp_ext::lifecycle::answer(
            &omp_ext::lifecycle::HeadlessResponder,
            "run_ephemeral_turn",
            &json!({"promptText": "q"}),
        )
        .await;
        assert_eq!(
            reply.unwrap_err(),
            "runEphemeralTurn is not supported by the core"
        );
        let gone = RuntimeResponder {
            rt: std::sync::Weak::new(),
            mode: ExtMode::Print,
        };
        let reply =
            omp_ext::lifecycle::answer(&gone, "run_ephemeral_turn", &json!({"promptText": "q"}))
                .await;
        assert!(reply.unwrap_err().contains("session has ended"));
        let reply = omp_ext::lifecycle::answer(&gone, "run_ephemeral_turn", &json!({})).await;
        assert!(reply.unwrap_err().contains("promptText"));
    }

    #[test]
    fn stop_continuation_text_matches_ts_rules() {
        use omp_ext::lifecycle::SessionStopOutcome;
        let block_default = SessionStopOutcome {
            block: true,
            ..Default::default()
        };
        assert_eq!(
            stop_continuation_text(&block_default).as_deref(),
            Some(SESSION_STOP_BLOCKED_PROMPT)
        );
        let block_reason = SessionStopOutcome {
            block: true,
            continue_turn: false,
            reason: Some("not done".into()),
            additional_context: Some("ctx".into()),
        };
        assert_eq!(
            stop_continuation_text(&block_reason).as_deref(),
            Some("not done")
        );
        let cont = SessionStopOutcome {
            block: false,
            continue_turn: true,
            additional_context: Some("ctx".into()),
            reason: Some("why".into()),
        };
        assert_eq!(stop_continuation_text(&cont).as_deref(), Some("ctx"));
        // `continue: true` without context schedules nothing (runner.ts).
        let bare = SessionStopOutcome {
            block: false,
            continue_turn: true,
            additional_context: None,
            reason: None,
        };
        assert_eq!(stop_continuation_text(&bare), None);
        let none = SessionStopOutcome::default();
        assert_eq!(stop_continuation_text(&none), None);
    }

    /// `continue: true` schedules exactly one hidden continuation turn and
    /// flips `stop_hook_active` for the next emit (`ReviewWave3`).
    #[tokio::test]
    async fn session_stop_continue_schedules_hidden_turn() {
        let dir = tempfile::tempdir().unwrap();
        let host = sh_host(dir.path(), CONTINUE_HOST, Vec::new()).await;
        let stop = Mutex::new(StopState::default());
        let turn = AtomicU64::new(2);
        let messages = session_stop_continuation(&host, &stop, "s1", "/s.jsonl", &turn, &[]).await;
        assert_eq!(messages.len(), 1);
        let AgentMessage::Custom(custom) = &messages[0] else {
            panic!("continuation must be a custom message");
        };
        assert_eq!(custom.role, "developer", "convert_to_llm surfaces it");
        assert_eq!(custom.payload["customType"], "session-stop-continuation");
        assert_eq!(
            custom.payload["content"][0]["text"].as_str(),
            Some("keep going")
        );
        assert!(custom.payload["display"].as_bool() == Some(false));
        {
            let st = stop.lock().unwrap();
            assert!(st.hook_active && st.continuations == 1);
        }
        host.shutdown().await.unwrap();
    }

    /// The continuation cap (8) applies to `continue` outcomes; block is exempt.
    #[tokio::test]
    async fn session_stop_continuation_cap_resets_state() {
        let dir = tempfile::tempdir().unwrap();
        let host = sh_host(dir.path(), CONTINUE_HOST, Vec::new()).await;
        let stop = Mutex::new(StopState::default());
        let turn = AtomicU64::new(0);
        for i in 0..SESSION_STOP_CONTINUATION_CAP {
            let messages =
                session_stop_continuation(&host, &stop, "s1", "/s.jsonl", &turn, &[]).await;
            assert_eq!(messages.len(), 1, "continuation {i} scheduled");
        }
        let messages = session_stop_continuation(&host, &stop, "s1", "/s.jsonl", &turn, &[]).await;
        assert!(messages.is_empty(), "cap reached: no further continuation");
        assert_eq!(
            *stop.lock().unwrap(),
            StopState::default(),
            "cap resets the bookkeeping"
        );
        host.shutdown().await.unwrap();
    }

    /// An already-aborted signal cancels the host-side handler via
    /// `cancel_tool` instead of leaving it running (`ReviewWave3`).
    #[tokio::test]
    async fn aborted_signal_cancels_host_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cancelled");
        let host = sh_host(
            dir.path(),
            concat!(
                "while IFS= read -r line; do\n",
                "  case \"$line\" in\n",
                "    *'\"method\":\"cancel_tool\"'*)\n",
                "      touch \"$MARKER\"\n",
                "      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":true}\n' \"$id\";;\n",
                "  esac\n",
                "done\n",
            ),
            vec![("MARKER".into(), marker.display().to_string())],
        )
        .await;
        let tool = Arc::new(ExtTool {
            host: host.clone(),
            info: ToolInfo {
                name: "slow".into(),
                label: None,
                description: None,
                parameters: json!({}),
                extension: "test".into(),
            },
        });
        let controller = AbortController::new();
        let signal = controller.signal();
        controller.abort();
        let result = tool
            .execute(
                "call-1".into(),
                json!({}),
                signal,
                Arc::new(|_| {}) as pa_agent::types::AgentToolUpdateCallback,
            )
            .await;
        let err = result.expect_err("aborted call must fail");
        assert!(err.to_string().contains("cancelled"), "{err:#}");
        assert!(marker.exists(), "host never received cancel_tool");
        host.shutdown().await.unwrap();
    }

    #[test]
    fn injected_message_follows_ts_custom_message_normalization() {
        // Object payload: role custom, explicit fields kept, attribution defaults to "user".
        let m = injected_message(
            &json!({"customType": "memory-recall", "content": "hi", "display": false}),
        )
        .expect("object payload");
        let AgentMessage::Custom(c) = m else {
            panic!("custom message")
        };
        assert_eq!(c.role, "custom");
        assert_eq!(c.payload["customType"], "memory-recall");
        assert_eq!(c.payload["content"], "hi");
        assert_eq!(c.payload["display"], false);
        assert_eq!(c.payload["attribution"], "user");
        // Explicit attribution wins; array content stays an array.
        let m = injected_message(&json!({"customType": "x", "content": [{"type": "text", "text": "t"}], "attribution": "agent"}))
            .expect("explicit attribution");
        let AgentMessage::Custom(c) = m else {
            panic!("custom message")
        };
        assert_eq!(c.payload["attribution"], "agent");
        assert!(c.payload["content"].is_array());
        // A bare string becomes a displayed default-type message; other shapes are dropped.
        assert!(injected_message(&json!("note")).is_some());
        assert!(injected_message(&json!(7)).is_none());
    }

    /// `credential_disabled`: events the auth store reported before the session was live wait in
    /// the feed and reach the host, in the omp payload shape, on the next flush.
    #[tokio::test]
    async fn credential_disabled_feed_flushes_to_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("emits.log");
        let host = sh_host(
            dir.path(),
            concat!(
                "while IFS= read -r line; do\n",
                "  case \"$line\" in\n",
                "    *'\"method\":\"emit\"'*)\n",
                "      printf '%s\\n' \"$line\" >> \"$LOG\"\n",
                "      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"result\":null,\"handlers\":[],\"errors\":[]}}\\n' \"$id\";;\n",
                "  esac\n",
                "done\n",
            ),
            vec![("LOG".into(), log.display().to_string())],
        )
        .await;
        let feed = CredentialDisabledFeed::default();
        feed.push(CredentialDisabledEvent {
            provider: "anthropic".into(),
            disabled_cause: "oauth refresh failed: invalid_grant".into(),
            credential_id: Some(7),
            email: Some("gone@example.com".into()),
            account_id: Some("acct".into()),
            org_id: None,
            org_name: None,
        });
        flush_credential_disabled(&host, &feed).await;
        assert!(feed.take().is_empty(), "the flush drains the queue");

        let written = std::fs::read_to_string(&log).unwrap();
        let request: Value = serde_json::from_str(written.lines().next().unwrap()).unwrap();
        assert_eq!(request["method"], "emit");
        let event = &request["params"]["event"];
        assert_eq!(event["type"], "credential_disabled");
        assert_eq!(event["provider"], "anthropic");
        assert_eq!(
            event["disabledCause"],
            "oauth refresh failed: invalid_grant"
        );
        assert_eq!(event["credentialId"], 7);
        assert_eq!(event["email"], "gone@example.com");
        assert_eq!(event["accountId"], "acct");
        assert!(
            event.get("orgId").is_none(),
            "absent identity fields are omitted"
        );
        host.shutdown().await.unwrap();
    }

    /// Fake host for the payload hook: logs every request line to `$OMP_TEST_LOG`,
    /// answers `load` with the subscribers in `$OMP_TEST_HANDLERS` and `emit` with a
    /// replacement payload (what `host.ts` returns after the handler chain).
    const PAYLOAD_HOST: &str = concat!(
        "while IFS= read -r line; do\n",
        "  printf '%s\\n' \"$line\" >> \"$OMP_TEST_LOG\"\n",
        "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
        "  case \"$line\" in\n",
        "    *'\"method\":\"load\"'*)\n",
        "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"loaded\":[{\"path\":\"/e.ts\",\"name\":\"e\",\"handlers\":%s}],\"errors\":[]}}\\n' \"$id\" \"$OMP_TEST_HANDLERS\"\n",
        "      ;;\n",
        "    *'\"method\":\"emit\"'*)\n",
        "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"result\":{\"payload\":{\"model\":\"rewritten\",\"n\":2}},\"handlers\":[],\"errors\":[]}}\\n' \"$id\"\n",
        "      ;;\n",
        "  esac\n",
        "done\n",
    );

    async fn payload_host(dir: &std::path::Path, handlers: &str) -> Arc<ExtHost> {
        let log = dir.join("requests.log");
        sh_host(
            dir,
            PAYLOAD_HOST,
            vec![
                ("OMP_TEST_LOG".into(), log.to_string_lossy().into_owned()),
                ("OMP_TEST_HANDLERS".into(), handlers.into()),
            ],
        )
        .await
    }

    /// An [`Extensions`] over an already-spawned (fake) host.
    fn sh_ext(host: Arc<ExtHost>, cwd: &std::path::Path) -> Arc<Extensions> {
        Arc::new(Extensions {
            mcp: McpNotificationSink::new(host.clone()),
            hooks: ExtHooks::new(host.clone()),
            host,
            sub: Mutex::new(None),
            turn: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(Mutex::new(StopState::default())),
            cwd: cwd.to_path_buf(),
            extra: Vec::new(),
            dir: None,
            ctx: Mutex::new(SessionCtx::default()),
            mode: Mutex::new(ExtMode::Print),
            prompt_override: Mutex::new(None),
            live: Arc::new(Live::default()),
            rt: Mutex::new(None),
            blocked_calls: Arc::new(Mutex::new(std::collections::HashSet::new())),
            deferred_agent_end: Arc::new(Mutex::new(None)),
            credential_disabled: Arc::new(CredentialDisabledFeed::default()),
        })
    }

    fn agent_model() -> pa_agent::types::Model {
        serde_json::from_value(json!({
            "id": "a", "name": "a", "api": "openai-completions", "provider": "p",
            "baseUrl": "http://127.0.0.1:9"
        }))
        .expect("agent model json")
    }

    /// The provider's real payload reaches `before_provider_request`, and the payload
    /// the handlers leave behind is what the request goes out with.
    #[tokio::test]
    async fn before_provider_request_sends_the_real_payload_and_applies_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let host = payload_host(dir.path(), r#"{"before_provider_request":1}"#).await;
        let ext = sh_ext(host, dir.path());

        let sent = json!({
            "model": "orig",
            "messages": [{"role": "user", "text": "real-payload-marker"}]
        });
        let out = ext
            .before_provider_request(sent)
            .expect("the handler's replacement payload");
        assert_eq!(out, json!({"model": "rewritten", "n": 2}));

        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(
            seen.contains(r#""type":"before_provider_request""#),
            "the event carries the TS name: {seen}"
        );
        assert!(
            seen.contains("real-payload-marker"),
            "the real payload crosses, not an empty one: {seen}"
        );
    }

    /// Hooks chain the way the loop composes them: the extension hook sees the earlier
    /// hook's payload and its own replacement is the final one.
    #[tokio::test]
    async fn before_provider_request_chains_onto_an_earlier_hook() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let host = payload_host(dir.path(), r#"{"before_provider_request":1}"#).await;
        let ext = sh_ext(host, dir.path());

        let earlier: pa_agent::stream::OnPayloadHook = Arc::new(|mut payload: Value, _model| {
            payload["earlier"] = json!(true);
            Some(payload)
        });
        let hook =
            crate::adapter::chain_agent_payload_hooks(Some(earlier), Some(ext.payload_hook()))
                .expect("both hooks present");

        let out = hook(json!({"model": "orig"}), &agent_model()).expect("replacement");
        assert_eq!(out, json!({"model": "rewritten", "n": 2}));

        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(
            seen.contains(r#""earlier":true"#),
            "the extension hook saw the earlier hook's payload: {seen}"
        );
    }

    /// No subscriber: no round trip and no payload change (`has_handlers`).
    #[tokio::test]
    async fn before_provider_request_skips_the_round_trip_without_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let host = payload_host(dir.path(), r#"{"session_start":1}"#).await;
        host.load(None, None, "/w", None).await.unwrap();
        let ext = sh_ext(host, dir.path());

        assert!(ext
            .before_provider_request(json!({"model": "orig"}))
            .is_none());
        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(
            !seen.contains(r#""method":"emit""#),
            "no emit without a subscriber: {seen}"
        );
    }

    /// Fake host that records every request line it receives into `$OUT` and
    /// answers each one with an empty emit result (order == wire order).
    const RECORD_HOST: &str = concat!(
        "while IFS= read -r line; do\n",
        "  printf '%s\\n' \"$line\" >> \"$OUT\"\n",
        "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
        "  if [ -n \"$id\" ]; then\n",
        "    printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"result\":null,\"handlers\":[],\"errors\":[]}}\\n' \"$id\"\n",
        "  fi\n",
        "done\n",
    );

    /// `Extensions` over a fake host (no `start`: no bun, no extension load).
    fn test_extensions(host: Arc<ExtHost>) -> Arc<Extensions> {
        Arc::new(Extensions {
            mcp: McpNotificationSink::new(host.clone()),
            credential_disabled: Arc::new(CredentialDisabledFeed::default()),
            host: host.clone(),
            hooks: ExtHooks::new(host),
            sub: Mutex::new(None),
            turn: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(Mutex::new(StopState::default())),
            cwd: PathBuf::from("."),
            extra: Vec::new(),
            dir: None,
            ctx: Mutex::new(SessionCtx::default()),
            mode: Mutex::new(ExtMode::Print),
            prompt_override: Mutex::new(None),
            live: Arc::new(Live::default()),
            rt: Mutex::new(None),
            blocked_calls: Arc::new(Mutex::new(std::collections::HashSet::new())),
            deferred_agent_end: Arc::new(Mutex::new(None)),
        })
    }

    /// Every recorded `emit` frame of `ty`, in wire order.
    fn emitted_events(path: &std::path::Path, ty: &str) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| v["method"] == "emit" && v["params"]["event"]["type"] == ty)
            .map(|v| v["params"]["event"].clone())
            .collect()
    }

    /// MCP notifications received before the session ctx is wired are buffered and
    /// then emitted in arrival order with the omp payload shape (omp
    /// `emitMcpNotification` + `#pendingMcpNotifications` drain).
    #[tokio::test]
    async fn mcp_notification_buffers_until_session_ctx_then_emits() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("frames.jsonl");
        let host = sh_host(
            dir.path(),
            RECORD_HOST,
            vec![("OUT".into(), out.display().to_string())],
        )
        .await;
        let ext = test_extensions(host.clone());
        let sink = ext.mcp.clone();
        sink.publish(
            "srv".into(),
            "notifications/tools/list_changed".into(),
            json!({}),
        );
        sink.publish("srv".into(), "custom/progress".into(), json!({"pct": 7}));
        assert!(
            emitted_events(&out, "mcp_notification").is_empty(),
            "nothing reaches the host before the session ctx is wired"
        );

        // Wiring the session ctx drains the buffer; the awaited `session_start`
        // afterwards is the ordering barrier (the host runs queued emits first).
        ext.set_session_ctx("s1", Some(std::path::Path::new("/s.jsonl")), "p/m")
            .await
            .unwrap();
        ext.session_start().await;
        let frames = emitted_events(&out, "mcp_notification");
        assert_eq!(frames.len(), 2, "both buffered frames: {frames:?}");
        assert_eq!(frames[0]["server"], json!("srv"));
        assert_eq!(
            frames[0]["method"],
            json!("notifications/tools/list_changed")
        );
        assert_eq!(frames[0]["params"], json!({}));
        assert_eq!(frames[1]["method"], json!("custom/progress"));
        assert_eq!(frames[1]["params"], json!({"pct": 7}));

        // Attached: a new frame goes out at once.
        sink.publish("other".into(), "custom/late".into(), Value::Null);
        ext.session_start().await;
        let frames = emitted_events(&out, "mcp_notification");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2]["server"], json!("other"));
        assert_eq!(frames[2]["method"], json!("custom/late"));
        host.shutdown().await.unwrap();
    }

    /// The pre-attach buffer is bounded at the omp cap (100), drop-oldest.
    #[tokio::test]
    async fn mcp_notification_buffer_drops_oldest_at_cap() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("frames.jsonl");
        let host = sh_host(
            dir.path(),
            RECORD_HOST,
            vec![("OUT".into(), out.display().to_string())],
        )
        .await;
        let ext = test_extensions(host.clone());
        let sink = ext.mcp.clone();
        for i in 0..MAX_PENDING_MCP_NOTIFICATIONS + 5 {
            sink.publish("srv".into(), "custom/tick".into(), json!({"i": i}));
        }
        ext.set_session_ctx("s1", None, "p/m").await.unwrap();
        ext.session_start().await;
        let frames = emitted_events(&out, "mcp_notification");
        assert_eq!(frames.len(), MAX_PENDING_MCP_NOTIFICATIONS);
        assert_eq!(frames[0]["params"], json!({"i": 5}), "oldest dropped");
        host.shutdown().await.unwrap();
    }

    /// The manager-side bridge: one published notification reaches every live
    /// session's sink (omp `sdk.ts` `addNotificationListener` -> runner).
    #[tokio::test]
    async fn publish_mcp_notification_reaches_registered_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("frames.jsonl");
        let host = sh_host(
            dir.path(),
            RECORD_HOST,
            vec![("OUT".into(), out.display().to_string())],
        )
        .await;
        let ext = test_extensions(host.clone());
        ext.set_session_ctx("s1", None, "p/m").await.unwrap();
        register_mcp_notifications(ext.mcp.clone());

        publish_mcp_notification(&omp_mcp::ServerNotification {
            server: "Fake Server.v1".into(),
            method: "notifications/custom/ping".into(),
            params: json!({"detail": "custom"}),
        });
        ext.session_start().await;

        let frames = emitted_events(&out, "mcp_notification");
        assert_eq!(frames.len(), 1, "frames: {frames:?}");
        assert_eq!(frames[0]["server"], json!("Fake Server.v1"));
        assert_eq!(frames[0]["method"], json!("notifications/custom/ping"));
        assert_eq!(frames[0]["params"], json!({"detail": "custom"}));
        host.shutdown().await.unwrap();
    }

    #[test]
    fn extension_mode_matches_ts_has_ui() {
        // TS `hasUI()` is true exactly when a UI context was provided: tui and non-headless rpc.
        assert!(ExtMode::Tui.has_ui() && ExtMode::Rpc.has_ui());
        assert!(!ExtMode::Print.has_ui() && !ExtMode::Json.has_ui());
        assert_eq!(ExtMode::Tui.name(), "tui");
    }

    /// Answers every request with the `resources_discover` result read from `$PATHS` (one line) and
    /// records each received request line in `$LOG`.
    const DISCOVER_HOST: &str = concat!(
        "while IFS= read -r line; do\n",
        "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
        "  if [ -n \"$id\" ]; then\n",
        "    printf '%s\\n' \"$line\" >> \"$LOG\"\n",
        "    printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"result\":%s,\"handlers\":[],\"errors\":[]}}\\n' \"$id\" \"$(cat \"$PATHS\")\"\n",
        "  fi\n",
        "done\n",
    );

    /// The wire event carries `{type, cwd, reason}`, and the returned paths reach the loaders that
    /// own them (skills, prompt templates, themes); a reload replaces the whole set.
    #[tokio::test]
    async fn resources_discover_emits_and_applies_paths() {
        let _guard = crate::sysprompt::extension_registry_lock().lock().await;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join("skills/one")).unwrap();
        std::fs::create_dir_all(cwd.join("prompts")).unwrap();
        std::fs::create_dir_all(cwd.join("themes")).unwrap();
        std::fs::write(
            cwd.join("skills/one/SKILL.md"),
            "---\nname: one\ndescription: From an extension.\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(cwd.join("prompts/hello.md"), "Say hello\n").unwrap();
        std::fs::write(cwd.join("themes/extheme.json"), "{}").unwrap();

        let payload = dir.path().join("payload.json");
        let log = dir.path().join("events.log");
        let write_payload = |v: &str| std::fs::write(&payload, v).unwrap();
        write_payload(concat!(
            r#"{"skillPaths":[{"path":"skills/one","extensionPath":"/ext"}],"#,
            r#""promptPaths":[{"path":"prompts","extensionPath":"/ext"}],"#,
            r#""themePaths":[{"path":"themes/extheme.json","extensionPath":"/ext"}]}"#,
        ));
        let host = sh_host(
            dir.path(),
            DISCOVER_HOST,
            vec![
                ("PATHS".into(), payload.display().to_string()),
                ("LOG".into(), log.display().to_string()),
            ],
        )
        .await;

        discover_resources(&host, &cwd, ResourcesDiscoverReason::Startup).await;
        assert_eq!(
            crate::sysprompt::extension_skill_paths(),
            vec![cwd.join("skills/one")]
        );
        assert_eq!(
            crate::rpc::slash::extension_prompt_paths(),
            vec![cwd.join("prompts")]
        );
        assert_eq!(
            omp_tui::theme::extension_theme_paths(),
            vec![cwd.join("themes/extheme.json")]
        );
        // The paths really reach each loader.
        let empty_agent = dir.path().join("agent");
        let skills = crate::sysprompt::load_skills(&empty_agent, &cwd, true);
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["one"]
        );
        assert!(
            crate::rpc::slash::load_file_commands(&cwd)
                .iter()
                .any(|c| c.name == "hello"),
            "extension prompt template becomes a slash command"
        );
        assert!(
            omp_tui::theme::available_themes()
                .iter()
                .any(|t| t.name == "extheme"),
            "extension theme joins the catalog"
        );

        // A reload re-sends the whole set: the previous paths are dropped.
        write_payload(r#"{"skillPaths":[{"path":"/elsewhere/skills","extensionPath":"/ext"}]}"#);
        discover_resources(&host, &cwd, ResourcesDiscoverReason::Reload).await;
        assert_eq!(
            crate::sysprompt::extension_skill_paths(),
            vec![PathBuf::from("/elsewhere/skills")]
        );
        assert!(crate::rpc::slash::extension_prompt_paths().is_empty());
        assert!(omp_tui::theme::extension_theme_paths().is_empty());

        // Wire shape: upstream `ResourcesDiscoverEvent` with the reason it fired for.
        let events = std::fs::read_to_string(&log).unwrap();
        let cwd_wire = cwd.to_string_lossy();
        assert!(
            events.contains(&format!(
                r#"{{"type":"resources_discover","cwd":"{cwd_wire}","reason":"startup"}}"#
            )),
            "startup event: {events}"
        );
        assert!(
            events.contains(r#""reason":"reload""#),
            "reload event: {events}"
        );

        crate::sysprompt::set_extension_skill_paths(Vec::new());
        host.shutdown().await.unwrap();
    }

    /// `resources_discover` paths resolve like upstream `resolvePath`: an absolute path stays,
    /// anything else resolves against the session cwd, `~` expands to the home directory.
    #[test]
    fn discovered_paths_resolve_against_cwd() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            resolve_discovered_path("/abs/skills", cwd),
            PathBuf::from("/abs/skills")
        );
        assert_eq!(
            resolve_discovered_path("skills/a", cwd),
            cwd.join("skills/a")
        );
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(
            resolve_discovered_path("~/ext/themes", cwd),
            PathBuf::from(home).join("ext/themes")
        );
    }
}
