//! `--mode rpc`: omp's JSON-lines RPC protocol (`modes/rpc`) on stdin/stdout.
//!
//! First frame `ready`; every command is answered with a `response` frame
//! (`{id?, type, command, success, data?|error?, code?}`); agent events,
//! `prompt_result` and `session_settled` frames are interleaved. Commands that
//! depend on subsystems omp-rs lacks answer `success: false` with an omp-style
//! error message (see `unsupported`).
//!
//! Commands are handled one at a time in arrival order on a worker task; the read loop keeps
//! routing `extension_ui_response` frames (and `abort`, while a command is in flight) so a
//! command awaiting a dialog answer cannot deadlock the transport.
//!
//! Interactive slash commands (`/branch`, `/copy`, `/agents`, `/modelpreset`, …) drive TS-shaped
//! `extension_ui_request` dialogs (`select` / `confirm` / `input` / `editor`, withdrawn by a
//! `cancelDialog` side-effect frame) unless the process runs with `--no-ui`; see
//! `slash_extra::dialog`. Host effects without an answer:
//! - `{"type":"extension_ui_request","id":…,"method":"set_editor_text","text":…}` — replace the
//!   client's prompt draft (TS rpc `set_editor_text`).
//! - `{"type":"clipboard_copy","text":…}` — `/copy` picked `text`; the client puts it on its
//!   clipboard (omp-rs extension; the TUI host copies itself).
//!
//! Host tools (`set_host_tools`, TS `RpcHostToolBridge`): each registered tool runs on the client.
//! A call emits `host_tool_call {id, toolCallId, toolName, arguments}`; the client streams
//! `host_tool_update {id, partialResult}` and finishes with `host_tool_result {id, result,
//! isError?}`. An abort emits `host_tool_cancel {id, targetId}`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};

use anyhow::Result;
use pa_agent::abort::{AbortController, AbortSignal};
use pa_agent::agent::{Agent, QueueMode};
use pa_agent::types::{
    AgentTool, AgentToolResult, AgentToolUpdateCallback, ThinkingLevel, ToolResultContent,
};
use pa_agent::BoxFut;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agent::Target;
use crate::paging;
use crate::persist::Persistence;
use crate::runtime::{
    available_models, level_name, model_info, newest_session, parse_level, unsupported, CmdError,
    CmdResult, Runtime,
};
use crate::tools::bash as shell;
use crate::tools::{AskUi, SelectOption};

#[path = "slash.rs"]
pub mod slash;

pub(crate) const MAX_FRAME_BYTES: u64 = 1_048_576;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Resolve a pending ask-UI dialog (`RpcAskUi` waiter) by frame id; no-op
/// when no waiter is registered. Also called from the interactive TUI's
/// bridge (`tui.rs`), which never runs the RPC command loop.
pub fn resolve_pending_ui_request(id: &str, frame: Value) {
    let tx = {
        let mut pending = lock(&PENDING_UI_REQUESTS);
        match pending.get(id) {
            Some((owner, _)) if !answers_session(owner.as_ref(), &frame) => None,
            Some(_) => pending.remove(id),
            None => None,
        }
    };
    if let Some((_, tx)) = tx {
        let _ = tx.send(frame);
    }
}

/// Register a waiter for the `extension_ui_response` that answers frame `id` (interactive
/// `/login` and `/logout` dialogs; resolved by [`resolve_pending_ui_request`]).
pub fn register_pending_ui_request(id: &str) -> tokio::sync::oneshot::Receiver<Value> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    lock(&PENDING_UI_REQUESTS).insert(id.to_string(), (caller_session(), tx));
    rx
}

/// Drop the waiter for frame `id` (its dialog was withdrawn unanswered).
pub fn forget_pending_ui_request(id: &str) {
    lock(&PENDING_UI_REQUESTS).remove(id);
}

/// A pending callback: the session that is owed the answer, and where it goes.
type Pending<T> = (Option<String>, T);

/// Pending RPC ask-UI dialogs (`extension_ui_request` id → waiter and the session that asked).
static PENDING_UI_REQUESTS: LazyLock<
    Mutex<HashMap<String, Pending<tokio::sync::oneshot::Sender<Value>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The routing id of the hosted session the caller runs in (none outside a multi-session host).
pub(crate) fn caller_session() -> Option<String> {
    crate::scope::current().and_then(|scope| crate::scope::read(&scope.routing_id).clone())
}

/// Whether a callback frame names the session that is owed the answer: a host stamps
/// `sessionId` on every frame it routes back, so a callback for another session never
/// resolves this one's pending request.
fn answers_session(pending: Option<&String>, frame: &Value) -> bool {
    match (pending, frame.get("sessionId").and_then(Value::as_str)) {
        (Some(owner), Some(named)) => owner == named,
        _ => true,
    }
}

/// Mirrors the `set_ask_dialog` RPC command (TS `askDialogEnabled`).
static ASK_DIALOG_ENABLED: AtomicBool = AtomicBool::new(false);

/// Disambiguates dialog ids minted in the same nanosecond.
static UI_REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Set once stdin closes: no answer can arrive any more, so new dialogs fail at once.
static UI_REQUESTS_CLOSED: AtomicBool = AtomicBool::new(false);

/// Fail every in-flight and future dialog (stdin closed).
pub(crate) fn close_ui_requests() {
    UI_REQUESTS_CLOSED.store(true, Ordering::SeqCst);
    lock(&PENDING_UI_REQUESTS).clear();
}

/// Removes a dialog's waiter when its request ends or is dropped mid-wait.
struct PendingUiGuard(String);

impl Drop for PendingUiGuard {
    fn drop(&mut self) {
        lock(&PENDING_UI_REQUESTS).remove(&self.0);
    }
}

/// RPC transport for the `ask` tool dialog (TS `RpcExtensionUIContext`):
/// emits `extension_ui_request` frames and resolves from the matching
/// `extension_ui_response` (routed by id in the run loop).
struct RpcAskUi;

impl RpcAskUi {
    /// Send one dialog frame and await its response. `Ok(None)` = the dialog
    /// was cancelled/timed out (TS resolves `undefined`).
    async fn request(method: &str, mut frame: Map<String, Value>) -> Result<Option<Value>> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        let id = format!(
            "ui-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
            UI_REQUEST_SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let timeout_ms = frame.get("timeout").and_then(Value::as_u64);
        frame.insert("id".into(), json!(id));
        frame.insert("type".into(), json!("extension_ui_request"));
        frame.insert("method".into(), json!(method));
        let _pending = PendingUiGuard(id.clone());
        lock(&PENDING_UI_REQUESTS).insert(id.clone(), (caller_session(), tx));
        if UI_REQUESTS_CLOSED.load(Ordering::SeqCst) {
            anyhow::bail!("RPC client disconnected before the dialog was answered");
        }
        // Emit through the runtime's frame channel via a task-local runtime;
        // `RpcAskUi` is only constructed inside the RPC loop.
        RUNTIME_TX.with(|tx| {
            if let Some(tx) = &*tx.borrow() {
                tx.send(Value::Object(frame)).ok();
            }
        });
        let wait = async {
            match rx.await {
                Ok(response) => Ok(Some(response)),
                Err(_) => Err(anyhow::anyhow!("ask dialog dropped")),
            }
        };
        let result = match timeout_ms {
            Some(ms) => {
                match tokio::time::timeout(std::time::Duration::from_millis(ms), wait).await {
                    Ok(r) => r,
                    Err(_) => Ok(Some(json!({"cancelled": true, "timedOut": true}))),
                }
            }
            None => wait.await,
        };
        result
    }
}

/// Internal end-of-stream frame: the stdout writer stops after the frames queued before it.
const FLUSH_MARKER: &str = "__omt_flush";

thread_local! {
    /// Frame channel of the active RPC loop (set in `run_with`).
    static RUNTIME_TX: std::cell::RefCell<Option<mpsc::UnboundedSender<Value>>> =
        const { std::cell::RefCell::new(None) };
}

impl AskUi for RpcAskUi {
    fn has_ask_dialog(&self) -> bool {
        ASK_DIALOG_ENABLED.load(Ordering::SeqCst)
    }

    fn ask(
        &self,
        questions: Value,
        timeout_ms: Option<u64>,
    ) -> BoxFut<'static, anyhow::Result<Option<Value>>> {
        Box::pin(async move {
            let mut frame = Map::new();
            frame.insert("questions".into(), questions);
            if let Some(ms) = timeout_ms {
                frame.insert("timeout".into(), json!(ms));
            }
            // Cancelled responses are passed through: the tool interprets
            // `cancelled`/`timedOut` (TS `parseAskDialogResponse`).
            Self::request("ask", frame).await
        })
    }

    // The gateway's dialog parser only knows `confirm|select|input|editor|question`
    // (omo-nyang `dialogs.ts`) and it never sends `set_ask_dialog`. A client that did
    // send it speaks tau's own `ask` dialog, which is what it can render, so only the
    // others get `question` frames.
    fn has_question_dialog(&self) -> bool {
        !ASK_DIALOG_ENABLED.load(Ordering::SeqCst)
    }

    fn question(
        &self,
        title: String,
        questions: Value,
        timeout_ms: Option<u64>,
    ) -> BoxFut<'static, anyhow::Result<Option<Value>>> {
        Box::pin(async move {
            let mut frame = Map::new();
            frame.insert("title".into(), json!(title));
            frame.insert("questions".into(), questions);
            if let Some(ms) = timeout_ms {
                frame.insert("timeout".into(), json!(ms));
            }
            Self::request("question", frame).await
        })
    }

    fn select(
        &self,
        prompt: String,
        options: Vec<SelectOption>,
        _multi: bool,
        _checked: Vec<usize>,
        _initial_index: Option<usize>,
    ) -> BoxFut<'static, anyhow::Result<Option<String>>> {
        Box::pin(async move {
            let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
            let mut frame = Map::new();
            frame.insert("title".into(), json!(prompt));
            frame.insert("options".into(), json!(labels));
            let details: Vec<Value> = options
                .iter()
                .map(|o| match &o.description {
                    Some(d) if !d.trim().is_empty() => json!({"description": d}),
                    _ => json!({}),
                })
                .collect();
            if details
                .iter()
                .any(|d| !d.as_object().is_none_or(Map::is_empty))
            {
                frame.insert("optionDetails".into(), json!(details));
            }
            Ok(Self::request("select", frame)
                .await?
                .and_then(|v| v.get("value").cloned())
                .and_then(|v| v.as_str().map(str::to_string)))
        })
    }
}

pub(crate) const MAX_REASSEMBLED_BYTES: u64 = 67_108_864;
const CACHE_WARMING_MODES: [&str; 3] = ["off", "streaming", "idle"];

/// RPC protocol version advertised by `get_protocol_info` (the transport `negotiate_protocol`
/// version is separate: that one selects frame chunking).
pub const PROTOCOL_VERSION: u32 = 2;
/// Engine name the OmOMeow gateway records for this host.
pub const ENGINE: &str = "oh-my-tau";

/// Every command `dispatch` answers, in catalog order.
///
/// This is the single source of truth behind `get_protocol_info`; `tests::catalog_matches_dispatch`
/// keeps it in step with the `match kind` arms below.
pub const RPC_COMMANDS: &[&str] = &[
    "negotiate_protocol",
    "get_protocol_info",
    "prompt",
    "steer",
    "follow_up",
    "remove_queued_message",
    "promote_queued_message",
    "abort",
    "abort_and_prompt",
    "interrupt",
    "set_client_info",
    "extension_request",
    "reload",
    "resume_session",
    "new_session",
    "switch_session",
    "open_session",
    "branch",
    "fork",
    "get_state",
    "set_fast_mode",
    "goal",
    "set_ask_dialog",
    "get_available_commands",
    "get_entries",
    "get_tree",
    "set_todos",
    "set_host_tools",
    "set_host_uri_schemes",
    "set_subagent_subscription",
    "set_event_filter",
    "get_subagents",
    "get_subagent_messages",
    "cancel_subagent",
    "steer_subagent",
    "live_start",
    "live_stop",
    "live_mute",
    "set_model",
    "cycle_model",
    "get_available_models",
    "set_thinking_level",
    "cycle_thinking_level",
    "get_available_thinking_levels",
    "set_steering_mode",
    "set_follow_up_mode",
    "set_interrupt_mode",
    "compact",
    "set_auto_compaction",
    "set_cache_warming",
    "set_auto_retry",
    "abort_retry",
    "abort_bash",
    "get_session_stats",
    "export_html",
    "get_branch_messages",
    "get_last_assistant_text",
    "set_session_name",
    "handoff",
    "get_messages",
    "get_messages_page",
    "get_login_providers",
    "login",
    "predict_word",
    "predict_word_feedback",
];

/// Success/failure response frame in omp's key order.
///
/// A failure carries the error code twice, as `code` (omp's spelling) and `errorCode` (the
/// OmOMeow gateway's `MoveHost`/`lane-hold` parsers read `errorCode`). An error without a code
/// falls back to `rpc_failed` so every failure frame is machine-classifiable.
pub(crate) fn response(id: Option<&Value>, command: &str, result: CmdResult) -> Value {
    let mut m = Map::new();
    if let Some(id) = id {
        m.insert("id".into(), id.clone());
    }
    m.insert("type".into(), json!("response"));
    m.insert("command".into(), json!(command));
    match result {
        Ok(data) => {
            m.insert("success".into(), json!(true));
            if let Some(d) = data {
                m.insert("data".into(), d);
            }
        }
        Err(e) => {
            m.insert("success".into(), json!(false));
            m.insert("error".into(), json!(e.message));
            let code = e.code.unwrap_or("rpc_failed");
            m.insert("code".into(), json!(code));
            m.insert("errorCode".into(), json!(code));
        }
    }
    Value::Object(m)
}

fn str_field<'a>(cmd: &'a Value, key: &str) -> Result<&'a str, CmdError> {
    cmd.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CmdError::coded(format!("{key} must be a string"), "invalid_params"))
}

fn bool_field(cmd: &Value, key: &str) -> Result<bool, CmdError> {
    cmd.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| CmdError::coded(format!("{key} must be a boolean"), "invalid_params"))
}

fn queue_mode_field(cmd: &Value) -> Result<QueueMode, CmdError> {
    match cmd.get("mode").and_then(Value::as_str) {
        Some("all") => Ok(QueueMode::All),
        Some("one-at-a-time") => Ok(QueueMode::OneAtATime),
        _ => Err(CmdError::new("mode must be \"all\" or \"one-at-a-time\"")),
    }
}

use omp_providers::login::providers::PROVIDERS as OAUTH_PROVIDERS;

/// `goal` with no goal runtime behind it: the answers omp gives while no goal exists.
fn goal(cmd: &Value) -> CmdResult {
    let empty = || ok(json!({"goal": null, "state": null}));
    match cmd.get("op").and_then(Value::as_str) {
        Some("get" | "pause" | "drop") => empty(),
        Some("create") => {
            if cmd
                .get("objective")
                .and_then(Value::as_str)
                .is_none_or(|o| o.trim().is_empty())
            {
                return Err(CmdError::new("objective is required when op=create"));
            }
            if let Some(b) = cmd.get("token_budget") {
                if !b.as_f64().is_some_and(|n| n.fract() == 0.0 && n > 0.0) {
                    return Err(CmdError::new(
                        "token_budget must be a positive integer when provided",
                    ));
                }
            }
            Err(unsupported("Goal mode"))
        }
        Some("resume") => Err(CmdError::new("No paused goal to resume.")),
        other => Err(CmdError::new(format!(
            "Unknown goal op: {}",
            other.map_or_else(|| "undefined".to_string(), str::to_string)
        ))),
    }
}

/// A client-side tool registered through `set_host_tools` (TS `RpcHostToolDefinition`).
struct HostToolDef {
    name: String,
    label: String,
    description: String,
    parameters: Value,
}

/// omp's `normalizeHostToolDefinitions`: validated, trimmed definitions.
fn host_tool_definitions(cmd: &Value) -> Result<Vec<HostToolDef>, CmdError> {
    let tools = cmd
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| CmdError::new("tools must be an array"))?;
    let mut defs = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .map_or("", str::trim);
        if name.is_empty() {
            return Err(CmdError::new(format!(
                "Host tool at index {index} must provide a non-empty name"
            )));
        }
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .map_or("", str::trim);
        if description.is_empty() {
            return Err(CmdError::new(format!(
                "Host tool \"{name}\" must provide a non-empty description"
            )));
        }
        let Some(parameters) = tool.get("parameters").filter(|p| p.is_object()) else {
            return Err(CmdError::new(format!(
                "Host tool \"{name}\" must provide a JSON Schema object"
            )));
        };
        let label = tool
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(name);
        defs.push(HostToolDef {
            name: name.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            parameters: parameters.clone(),
        });
    }
    Ok(defs)
}

/// Host tool calls awaiting `host_tool_update` / `host_tool_result` frames (call id -> sink and
/// the session that asked).
static PENDING_HOST_TOOLS: LazyLock<Mutex<HashMap<String, Pending<mpsc::UnboundedSender<Value>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Names installed by the last `set_host_tools` outside a host (replaced wholesale by the next
/// one); a hosted session keeps its own list in its scope.
static HOST_TOOL_NAMES: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Set once the client disconnected: active and later host tool calls fail (TS `close`).
static HOST_TOOLS_CLOSED: AtomicBool = AtomicBool::new(false);
static HOST_FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

const HOST_TOOLS_DISCONNECTED: &str =
    "RPC client disconnected before host tool execution completed";

/// Where a host tool emits its `host_tool_call` / `host_tool_cancel` frames.
type FrameSink = Arc<dyn Fn(Value) + Send + Sync>;

fn host_frame_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        HOST_FRAME_SEQ.fetch_add(1, Ordering::SeqCst) + 1
    )
}

/// Route an inbound `host_tool_result` / `host_tool_update` frame to its pending call.
/// `false` for unknown ids or frames whose result lacks a `content` array (TS type guards).
pub(crate) fn route_host_tool_frame(frame: Value) -> bool {
    let is_result = frame.get("type") == Some(&json!("host_tool_result"));
    let key = if is_result { "result" } else { "partialResult" };
    if !frame
        .get(key)
        .and_then(|r| r.get("content"))
        .is_some_and(Value::is_array)
    {
        return false;
    }
    let Some(id) = frame.get("id").and_then(Value::as_str).map(str::to_string) else {
        return false;
    };
    let tx = {
        let mut pending = lock(&PENDING_HOST_TOOLS);
        match pending.get(&id) {
            Some((owner, _)) if !answers_session(owner.as_ref(), &frame) => None,
            Some(_) if is_result => pending.remove(&id),
            other => other.cloned(),
        }
    };
    tx.is_some_and(|(_, tx)| tx.send(frame).is_ok())
}

/// Fail every in-flight and future host tool call (stdin closed).
pub(crate) fn close_host_tools() {
    HOST_TOOLS_CLOSED.store(true, Ordering::SeqCst);
    lock(&PENDING_HOST_TOOLS).clear();
}

/// A `set_host_tools` tool proxied to the RPC client (TS `RpcHostToolAdapter`).
struct HostTool {
    def: HostToolDef,
    sink: FrameSink,
}

impl HostTool {
    async fn call(
        &self,
        tool_call_id: String,
        arguments: Value,
        signal: AbortSignal,
        on_update: AgentToolUpdateCallback,
    ) -> Result<AgentToolResult> {
        let name = &self.def.name;
        if signal.is_aborted() {
            anyhow::bail!("Host tool \"{name}\" was aborted");
        }
        if HOST_TOOLS_CLOSED.load(Ordering::SeqCst) {
            anyhow::bail!(HOST_TOOLS_DISCONNECTED);
        }
        let id = host_frame_id("host-tool");
        let (tx, mut rx) = mpsc::unbounded_channel();
        lock(&PENDING_HOST_TOOLS).insert(id.clone(), (caller_session(), tx));
        (self.sink)(
            json!({"type": "host_tool_call", "id": id, "toolCallId": tool_call_id,
                           "toolName": name, "arguments": arguments}),
        );
        loop {
            let frame = tokio::select! {
                biased;
                () = signal.aborted() => {
                    lock(&PENDING_HOST_TOOLS).remove(&id);
                    (self.sink)(json!({"type": "host_tool_cancel",
                                       "id": host_frame_id("host-tool-cancel"), "targetId": id}));
                    anyhow::bail!("Host tool \"{name}\" was aborted");
                }
                frame = rx.recv() => frame,
            };
            let Some(mut frame) = frame else {
                anyhow::bail!(HOST_TOOLS_DISCONNECTED);
            };
            if frame.get("type") == Some(&json!("host_tool_update")) {
                if let Ok(partial) = serde_json::from_value(frame["partialResult"].take()) {
                    on_update(partial);
                }
                continue;
            }
            let result: AgentToolResult = serde_json::from_value(frame["result"].take())?;
            if frame.get("isError") == Some(&json!(true)) {
                let text = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = text.trim();
                anyhow::bail!(if text.is_empty() {
                    "Host tool execution failed".to_string()
                } else {
                    text.to_string()
                });
            }
            return Ok(result);
        }
    }
}

impl AgentTool for HostTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn parameters(&self) -> &Value {
        &self.def.parameters
    }
    fn label(&self) -> &str {
        &self.def.label
    }
    fn execute(
        self: Arc<Self>,
        tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.call(tool_call_id, params, signal, on_update).await })
    }
}

/// `set_host_tools`: replace the previous host tool set (a same-named tool is shadowed).
async fn set_host_tools(rt: &Arc<Runtime>, cmd: &Value) -> CmdResult {
    let defs = host_tool_definitions(cmd)?;
    let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
    let weak = Arc::downgrade(rt);
    let sink: FrameSink = Arc::new(move |frame| {
        if let Some(rt) = weak.upgrade() {
            rt.send(frame);
        }
    });
    let _tool_guard = rt.lock_tool_list().await;
    let mut tools = rt.agent.tools().await;
    match crate::scope::current() {
        Some(scope) => {
            let mut installed = crate::scope::write(&scope.host_tools);
            tools.retain(|t| !installed.iter().chain(&names).any(|n| n == t.name()));
            installed.clone_from(&names);
        }
        None => {
            let mut installed = lock(&HOST_TOOL_NAMES);
            tools.retain(|t| !installed.iter().chain(&names).any(|n| n == t.name()));
            installed.clone_from(&names);
        }
    }
    tools.extend(defs.into_iter().map(|def| {
        Arc::new(HostTool {
            def,
            sink: sink.clone(),
        }) as Arc<dyn AgentTool>
    }));
    crate::tools::republish_dyn_tools(&tools);
    rt.agent.set_tools(tools).await;
    ok(json!({"toolNames": names}))
}

/// `set_host_uri_schemes`: omp's scheme validation, then the no-bridge answer.
fn set_host_uri_schemes(cmd: &Value) -> CmdResult {
    let schemes = cmd
        .get("schemes")
        .and_then(Value::as_array)
        .ok_or_else(|| CmdError::new("schemes must be an array"))?;
    for raw in schemes {
        let scheme = raw
            .get("scheme")
            .and_then(Value::as_str)
            .map_or_else(String::new, |s| s.trim().to_lowercase());
        if scheme.is_empty() {
            return Err(CmdError::new("Host URI scheme must be a non-empty string"));
        }
        let mut chars = scheme.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-')
            });
        if !valid {
            let shown = raw.get("scheme").and_then(Value::as_str).unwrap_or("");
            return Err(CmdError::new(format!(
                "Host URI scheme contains invalid characters: {shown}"
            )));
        }
        // Built-in schemes (`xd`, `local`, `skill`, ...) are OMP-owned (TS `isBuiltin`).
        if crate::tools::url_registry().get(&scheme).is_some() {
            return Err(CmdError::new(format!(
                "Host URI scheme is reserved by OMP: {scheme}://"
            )));
        }
    }
    if schemes.is_empty() {
        return ok(json!({"schemes": []}));
    }
    Err(unsupported("Host URI schemes"))
}

fn subagent_id<'a>(cmd: &'a Value, _command: &str) -> Result<&'a str, CmdError> {
    cmd.get("subagentId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CmdError::new("`subagentId` must be a non-empty string."))
}

/// `get_subagent_messages`: a background task's session messages from
/// `fromByte` on.
fn subagent_messages(cmd: &Value) -> CmdResult {
    let from = match cmd.get("fromByte") {
        None | Some(Value::Null) => 0.0,
        Some(from) => from
            .as_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| CmdError::new("fromByte must be a finite number"))?,
    };
    let text = |key: &str| {
        cmd.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // finite, clamped at 0
    let from = from.max(0.0) as u64;
    ok(crate::tools::subagents::rpc_subagent_messages(
        text("subagentId"),
        text("sessionFile"),
        from,
    )?)
}

/// `predict_word*` validation: `text` string, `cursor` an integer UTF-16 offset within it.
fn text_cursor(cmd: &Value) -> Result<(), CmdError> {
    let text = cmd.get("text").and_then(Value::as_str);
    let cursor = cmd.get("cursor").and_then(Value::as_f64);
    if let (Some(t), Some(c)) = (text, cursor) {
        let utf16_len = f64::from(u32::try_from(t.encode_utf16().count()).unwrap_or(u32::MAX));
        if c.fract() == 0.0 && c >= 0.0 && c <= utf16_len {
            return Ok(());
        }
    }
    Err(CmdError::new(
        "cursor must be an integer UTF-16 offset within text",
    ))
}

/// `get_login_providers`: omp's OAuth registry; `authenticated` = a credential source resolves.
async fn login_providers() -> Vec<Value> {
    let mut out = Vec::with_capacity(OAUTH_PROVIDERS.len());
    for (id, name) in OAUTH_PROVIDERS {
        // `!command` keys shell out: resolve on the blocking pool, not the executor.
        let from_models = match pa_ai::models_generated::get_models(id).first() {
            Some(m) => omp_providers::resolve_async(format!("{id}/{}", m.id))
                .await
                .is_ok_and(|r| r.api_key.is_some()),
            None => false,
        };
        let authenticated = from_models || pa_ai::env_api_keys::get_env_api_key(id).is_some();
        out.push(
            json!({"id": id, "name": name, "available": true, "authenticated": authenticated}),
        );
    }
    out
}

/// `images` of a prompt command: `[{type: "image", data, mimeType}]`.
fn images(cmd: &Value) -> Vec<pa_agent::types::ImageContent> {
    cmd.get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| {
            Some(pa_agent::types::ImageContent {
                data: i.get("data")?.as_str()?.to_string(),
                mime_type: i.get("mimeType")?.as_str()?.to_string(),
            })
        })
        .collect()
}

// Result shape mirrors the TS command framing (`ok(...)` / `none()` feed `?`-chains).
#[allow(clippy::unnecessary_wraps)]
fn ok(data: Value) -> CmdResult {
    Ok(Some(data))
}

#[allow(clippy::unnecessary_wraps)]
fn none() -> CmdResult {
    Ok(None)
}

async fn cycle_model(rt: &Runtime) -> CmdResult {
    let info = rt.model_info();
    let models = available_models(None).await;
    if models.len() < 2 {
        return ok(Value::Null);
    }
    let idx = models
        .iter()
        .position(|m| {
            info.get("provider") == Some(&json!(m.provider.clone()))
                && info.get("id") == Some(&json!(m.id))
        })
        .unwrap_or(0);
    let next = &models[(idx + 1) % models.len()];
    let model = rt.cycle_to(&next.provider, &next.id).await?;
    let level = rt.agent.thinking_level().await;
    let ultra = rt.ultra().map(omp_tui::magic::Ultra::name);
    ok(
        json!({"model": model, "thinkingLevel": level_name(level), "ultra": ultra, "isScoped": false}),
    )
}

fn branch_messages(entries: &[Value]) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|e| e.pointer("/message/role").and_then(Value::as_str) == Some("user"))
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?;
            let content = e.pointer("/message/content")?;
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| {
                        (p.get("type")? == "text")
                            .then(|| p.get("text")?.as_str().map(str::to_string))?
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return None,
            };
            Some(json!({"entryId": id, "text": text}))
        })
        .collect()
}

fn tree(entries: &[Value]) -> Vec<Value> {
    fn build(entries: &[Value], parent: Option<&str>, known: &HashSet<&str>) -> Vec<Value> {
        entries
            .iter()
            .filter(|e| {
                let p = e.get("parentId").and_then(Value::as_str);
                match parent {
                    Some(id) => p == Some(id),
                    None => p.is_none_or(|p| !known.contains(p)),
                }
            })
            .map(|e| {
                let id = e.get("id").and_then(Value::as_str);
                json!({"entry": e, "children": build(entries, id, known)})
            })
            .collect()
    }
    let known: HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();
    build(entries, None, &known)
}

async fn run_bash(rt: &Arc<Runtime>, command: &str) -> CmdResult {
    // TS `executeBash`: a `user_bash` handler's result replaces the execution.
    if let Some(host) = rt.ext_host().filter(|h| h.has_handlers("user_bash")) {
        match host
            .emit_user_bash(
                command.to_owned(),
                false,
                rt.cwd.display().to_string(),
                None,
            )
            .await
        {
            Ok(outcome) => {
                if let Some(result) = outcome.result {
                    return ok(result);
                }
            }
            Err(e) => eprintln!("omt: extension user_bash: {e:#}"),
        }
    }
    let controller = AbortController::new();
    let signal = controller.signal();
    rt.set(|s| s.running_bash(Some(controller)));
    let result = shell::execute(&rt.cwd, command, &signal).await;
    rt.set(|s| s.running_bash(None));
    let r = result.map_err(CmdError::from)?;
    let mut v = json!({
        "output": r.output,
        "cancelled": r.cancelled,
        "truncated": r.truncated,
        "totalLines": r.total_lines,
        "totalBytes": r.total_bytes,
        "outputLines": r.output_lines,
        "outputBytes": r.output_bytes,
        "workingDir": rt.cwd.display().to_string(),
    });
    if let Some(code) = r.exit_code {
        v["exitCode"] = json!(code);
    }
    if r.timed_out {
        v["timedOut"] = json!(true);
    }
    ok(v)
}

/// Handle one command (everything but `bash`, which runs in the background).
#[allow(clippy::too_many_lines)]
pub async fn handle(rt: &Arc<Runtime>, cmd: &Value) -> Value {
    let id = cmd.get("id");
    let kind = cmd.get("type").and_then(Value::as_str).unwrap_or("");
    let result = dispatch(rt, cmd, kind).await;
    response(id, kind, result)
}

#[allow(clippy::too_many_lines)]
// The `abort_bash` closure is required: the method-ref form `SettingsView::abort_bash`
// fails the HRTB lifetime check (`&mut SettingsView<'_>`), so clippy's replacement
// does not compile.
#[allow(clippy::redundant_closure, clippy::redundant_closure_for_method_calls)]
/// TS rpc `emitInput(..., "rpc")`: `None` when an extension consumed the input, else the
/// (possibly rewritten) text. No extension host / host failure passes the text through.
async fn input_gate(text: &str) -> Option<String> {
    let Some(ext) = slash::extensions() else {
        return Some(text.to_string());
    };
    match ext
        .input_gate(text, omp_ext::lifecycle::InputSource::Rpc)
        .await
    {
        Ok(crate::ext::PromptGate::Run(t)) => Some(t),
        Ok(crate::ext::PromptGate::Handled) => None,
        Err(e) => {
            eprintln!("omt: extension input: {e:#}");
            Some(text.to_string())
        }
    }
}

async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult {
    match kind {
        "negotiate_protocol" => match cmd.get("protocolVersion").and_then(Value::as_i64) {
            Some(2) => ok(json!({"protocolVersion": 2})),
            other => Err(CmdError::new(format!(
                "Unsupported RPC protocol version: {}",
                other.map_or_else(|| "undefined".to_string(), |v| v.to_string())
            ))),
        },
        "prompt" => {
            let message = str_field(cmd, "message")?;
            // TS rpc ordered-input dispatch: `input` handlers run before the prompt/slash handling.
            let gated = input_gate(message).await;
            let outcome = match &gated {
                Some(text) => slash::try_dispatch(rt, text).await,
                None => slash::Outcome::ExtensionHandled,
            };
            let message = gated.as_deref().unwrap_or(message);
            match outcome {
                slash::Outcome::Handled { agent_invoked } => {
                    ok(json!({"agentInvoked": agent_invoked}))
                }
                slash::Outcome::ExtensionHandled => {
                    // TS: the extension consumed the prompt inside session.prompt;
                    // the response carries no data and a completed prompt_result
                    // follows the response frame.
                    let task_rt = Arc::clone(rt);
                    let id = cmd.get("id").cloned();
                    tokio::spawn(async move {
                        tokio::task::yield_now().await;
                        let mut f = json!({"type":"prompt_result","agentInvoked":false,
                            "status":"completed","sessionSettled":true});
                        if let Some(id) = id {
                            f["id"] = id;
                        }
                        task_rt.send(f);
                    });
                    none()
                }
                slash::Outcome::Prompt(rewritten) => {
                    rt.prompt(
                        cmd.get("id").cloned(),
                        &rewritten,
                        cmd.get("streamingBehavior").and_then(Value::as_str),
                        images(cmd),
                    )
                    .await?;
                    none()
                }
                slash::Outcome::NotSlash => {
                    rt.prompt(
                        cmd.get("id").cloned(),
                        message,
                        cmd.get("streamingBehavior").and_then(Value::as_str),
                        images(cmd),
                    )
                    .await?;
                    none()
                }
            }
        }
        "steer" | "follow_up" => {
            let message = str_field(cmd, "message")?;
            if let Some(message) = input_gate(message).await {
                if kind == "steer" {
                    rt.steer(&message);
                } else {
                    rt.follow_up(&message);
                }
            }
            none()
        }
        "remove_queued_message" => {
            let message = str_field(cmd, "message")?;
            let queue = cmd.get("queue").and_then(Value::as_str);
            if !matches!(queue, Some("steering" | "followUp")) {
                return Err(CmdError::new("queue must be \"steering\" or \"followUp\""));
            }
            ok(json!({"removed": rt.remove_queued(message, queue.unwrap_or("steering"))}))
        }
        "promote_queued_message" => {
            let message = str_field(cmd, "message")?;
            ok(json!({"promoted": rt.promote_queued(message)}))
        }
        "abort" => {
            rt.abort().await;
            none()
        }
        "interrupt" => {
            match crate::pause::parse_intent(cmd).map_err(CmdError::coded_params)? {
                crate::pause::InterruptIntent::Stop => rt.pause_for_stop().await?,
                crate::pause::InterruptIntent::Abort => rt.abort().await,
            }
            ok(json!({"interrupted": false}))
        }
        "set_client_info" => {
            rt.set_client_info(cmd)?;
            none()
        }
        "extension_request" => {
            let name = cmd
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if name.is_empty() {
                return Err(CmdError::coded(
                    "Extension RPC request name cannot be empty",
                    "invalid_params",
                ));
            }
            let Some(host) = rt.ext_host() else {
                return Err(CmdError::coded(
                    format!("extension_unavailable: no extension host for {name}"),
                    "extension_unavailable",
                ));
            };
            let data = cmd.get("data").cloned().unwrap_or(Value::Null);
            match host
                .request_extension(name, data, std::time::Duration::from_secs(5))
                .await
            {
                Ok(v) => ok(v),
                Err(e) => {
                    let message = format!("{e:#}");
                    let code = if message.contains("timed out") {
                        "extension_timeout"
                    } else if message.contains("Unknown extension RPC request") {
                        "unknown_extension_request"
                    } else {
                        "extension_failed"
                    };
                    Err(CmdError::coded(message, code))
                }
            }
        }
        "reload" => reload_with_ext(rt, crate::rpc::slash::extensions().as_ref()).await,
        "resume_session" => {
            rt.release_pause("resume_session");
            none()
        }
        "abort_and_prompt" => {
            let message = str_field(cmd, "message")?.to_string();
            let images = images(cmd);
            let Some(message) = input_gate(&message).await else {
                return none();
            };
            rt.abort().await;
            let task_rt = rt.clone();
            let id = cmd.get("id").cloned();
            tokio::spawn(async move {
                if let Err(e) = task_rt.prompt(id.clone(), &message, None, images).await {
                    task_rt.send(response(
                        id.as_ref(),
                        "abort_and_prompt",
                        Err(CmdError::new(e.message.clone())),
                    ));
                    let mut f = json!({"type":"prompt_result","agentInvoked":false,"status":"error",
                        "error":{"message":e.message,"retryable":false},"sessionSettled":true});
                    if let Some(id) = id {
                        f["id"] = id;
                    }
                    task_rt.send(f);
                }
            });
            none()
        }
        "new_session" => match rt.new_session(None).await {
            Err(e) if crate::runtime::is_switch_cancelled(&e) => ok(json!({"cancelled": true})),
            other => {
                other?;
                ok(json!({"cancelled": false}))
            }
        },
        "switch_session" => {
            let path = PathBuf::from(str_field(cmd, "sessionPath")?);
            if !path.is_file() {
                return Err(CmdError::new(format!(
                    "Session file not found: {}",
                    path.display()
                )));
            }
            match rt.load_session(&path, None).await {
                Err(e) if crate::runtime::is_switch_cancelled(&e) => ok(json!({"cancelled": true})),
                other => {
                    other?;
                    ok(json!({"cancelled": false}))
                }
            }
        }
        "open_session" => {
            let dir = PathBuf::from(str_field(cmd, "sessionDir")?);
            let resumed = if let Some(path) = newest_session(&dir) {
                rt.load_session(&path, Some(&dir)).await?;
                true
            } else {
                rt.new_session(Some(&dir)).await?;
                false
            };
            ok(json!({
                "cancelled": false,
                "resumed": resumed,
                "sessionId": rt.session_id(),
                "sessionFile": rt.session_file().display().to_string(),
            }))
        }
        "branch" | "fork" => {
            let entry_id = if kind == "branch" {
                Some(str_field(cmd, "entryId")?)
            } else {
                cmd.get("entryId").and_then(Value::as_str)
            };
            if rt.agent.is_streaming().await {
                return Err(CmdError::coded(
                    format!("Cannot {kind} the session while session maintenance or user work is still running"),
                    "session_busy",
                ));
            }
            let result = if kind == "branch" {
                crate::tree::branch(&Arc::clone(rt), entry_id.unwrap_or_default()).await
            } else {
                crate::tree::fork(&Arc::clone(rt), entry_id).await
            };
            rt.publish_session_file();
            result
        }
        "get_state" => ok(rt.state().await),
        "set_fast_mode" => {
            if bool_field(cmd, "enabled")? {
                Err(CmdError::new(
                    "Fast mode is unavailable for the current model.",
                ))
            } else {
                ok(json!({"enabled": false, "active": false}))
            }
        }
        "goal" => goal(cmd),
        "set_ask_dialog" => {
            let enabled =
                rt.set(|s| s.ask_dialog(cmd.get("enabled").and_then(Value::as_bool) == Some(true)));
            ASK_DIALOG_ENABLED.store(
                cmd.get("enabled").and_then(Value::as_bool) == Some(true),
                Ordering::SeqCst,
            );
            ok(json!({"enabled": enabled}))
        }
        "get_available_commands" => ok(json!({"commands": slash::available_commands(rt).await})),
        "get_entries" => {
            let entries = rt.entries().map_err(CmdError::from)?;
            let leaf = entries
                .last()
                .and_then(|e| e.get("id").cloned())
                .unwrap_or(Value::Null);
            let selected = match cmd.get("since").and_then(Value::as_str) {
                None => entries,
                Some(since) => {
                    let idx = entries
                        .iter()
                        .position(|e| e.get("id").and_then(Value::as_str) == Some(since))
                        .ok_or_else(|| {
                            CmdError::coded(format!("Unknown entry id: {since}"), "unknown_since")
                        })?;
                    entries[idx + 1..].to_vec()
                }
            };
            ok(json!({"entries": selected, "leafId": leaf}))
        }
        "get_tree" => {
            let entries = rt.entries().map_err(CmdError::from)?;
            let leaf = entries
                .last()
                .and_then(|e| e.get("id").cloned())
                .unwrap_or(Value::Null);
            ok(json!({"tree": tree(&entries), "leafId": leaf}))
        }
        "set_todos" => {
            let phases = cmd
                .get("phases")
                .cloned()
                .filter(Value::is_array)
                .ok_or_else(|| CmdError::new("phases must be an array"))?;
            rt.set(|s| s.todo_phases(phases));
            ok(json!({"todoPhases": rt.set(|s| s.todo_phases_value())}))
        }
        "set_host_tools" => set_host_tools(rt, cmd).await,
        "set_host_uri_schemes" => set_host_uri_schemes(cmd),
        "set_subagent_subscription" => {
            let level = str_field(cmd, "level")?;
            if !matches!(level, "off" | "progress" | "events") {
                return Err(CmdError::new(
                    "level must be \"off\", \"progress\" or \"events\"",
                ));
            }
            rt.set(|s| s.subagent_level(level));
            ok(json!({"level": level}))
        }
        "set_event_filter" => {
            let events = cmd.get("events");
            let filter = match events {
                Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<HashSet<_>>(),
                ),
                _ => return Err(CmdError::new("events must be an array of strings or null")),
            };
            let delta = match cmd.get("messageUpdates").and_then(Value::as_str) {
                None | Some("full") => false,
                Some("delta") => true,
                Some(_) => {
                    return Err(CmdError::new(
                        "messageUpdates must be \"full\" or \"delta\"",
                    ))
                }
            };
            rt.set(|s| s.event_filter(filter, delta));
            let mut data = json!({"events": events.cloned().unwrap_or(Value::Null)});
            if let Some(m) = cmd.get("messageUpdates") {
                data["messageUpdates"] = m.clone();
            }
            ok(data)
        }
        "get_subagents" => ok(crate::tools::subagents::rpc_get_subagents(&rt.session_id())),
        "get_subagent_messages" => subagent_messages(cmd),
        "cancel_subagent" => {
            let id = subagent_id(cmd, "cancel_subagent")?;
            let reason = cmd.get("reason").and_then(Value::as_str);
            ok(crate::tools::subagents::rpc_cancel_subagent(id, reason)?)
        }
        "steer_subagent" => {
            let id = subagent_id(cmd, "steer_subagent")?;
            let Some(message) = cmd
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
            else {
                return Err(CmdError::new("`message` is required for steer_subagent."));
            };
            ok(crate::tools::subagents::rpc_steer_subagent(id, message).await?)
        }
        "live_start" => Err(unsupported("Live voice")),
        "live_stop" => none(),
        "live_mute" => Err(CmdError::new("No live session is active")),
        "set_model" => {
            let provider = str_field(cmd, "provider")?;
            let model_id = str_field(cmd, "modelId")?;
            ok(rt.set_model(provider, model_id).await?)
        }
        "cycle_model" => cycle_model(rt).await,
        "get_available_models" => {
            let current = {
                let info = rt.model_info();
                info.get("provider")
                    .zip(info.get("id"))
                    .map(|(p, i)| (p.clone(), i.clone()))
            };
            // Same catalogue as the TUI `/model` picker: every chat model of a
            // provider with credentials (bundled catalog, models.db, models.yml).
            let catalog = tokio::task::spawn_blocking(|| {
                crate::subcmd::models::browser_models(&omp_providers::AgentDir::default())
            })
            .await
            .unwrap_or_default();
            let mut models: Vec<Value> = catalog
                .iter()
                .filter(|b| b.available && b.kind.is_empty() && !b.model.id.is_empty())
                .map(|b| model_info(&b.model))
                .collect();
            if let Some((p, i)) = current {
                if !models
                    .iter()
                    .any(|m| m.get("provider") == Some(&p) && m.get("id") == Some(&i))
                {
                    models.insert(0, rt.model_info());
                }
            }
            ok(json!({"models": models}))
        }
        "set_thinking_level" => {
            let level = str_field(cmd, "level")?;
            if let Some(mode) = crate::runtime::parse_ultra_name(level) {
                rt.set_ultra(Some(mode)).await;
                return none();
            }
            let parsed = parse_level(level)
                .or_else(|| (level == "inherit").then_some(ThinkingLevel::Medium));
            let Some(parsed) = parsed else {
                return Err(CmdError::new(format!("Invalid thinking level: {level}")));
            };
            rt.set_thinking_level(parsed).await;
            none()
        }
        "cycle_thinking_level" => {
            let levels = rt.available_thinking_levels();
            if levels.len() < 2 {
                return ok(Value::Null);
            }
            let current = level_name(rt.agent.thinking_level().await);
            let idx = levels.iter().position(|l| *l == current).unwrap_or(0);
            let next = levels[(idx + 1) % levels.len()];
            if let Some(level) = parse_level(next) {
                rt.set_thinking_level(level).await;
            }
            ok(json!({"level": next}))
        }
        "get_available_thinking_levels" => ok(json!({"levels": rt.available_thinking_levels()})),
        "set_steering_mode" => {
            rt.set_queue_modes(Some(queue_mode_field(cmd)?), None);
            none()
        }
        "set_follow_up_mode" => {
            rt.set_queue_modes(None, Some(queue_mode_field(cmd)?));
            none()
        }
        "set_interrupt_mode" => {
            let mode = str_field(cmd, "mode")?;
            if !matches!(mode, "immediate" | "wait") {
                return Err(CmdError::new("mode must be \"immediate\" or \"wait\""));
            }
            rt.set(|s| s.interrupt_mode(mode));
            none()
        }
        "compact" => {
            let custom = cmd.get("customInstructions").and_then(Value::as_str);
            Ok(Some(rt.compact(custom, None).await?))
        }
        "set_auto_compaction" => {
            let enabled = bool_field(cmd, "enabled")?;
            rt.set(|s| s.auto_compaction(enabled));
            none()
        }
        "set_cache_warming" => {
            let mode = cmd.get("mode").and_then(Value::as_str).unwrap_or("");
            if !CACHE_WARMING_MODES.contains(&mode) {
                let shown = cmd.get("mode").map_or_else(
                    || "undefined".to_string(),
                    |m| m.as_str().map_or_else(|| m.to_string(), str::to_string),
                );
                return Err(CmdError::new(format!(
                    "Invalid cache warming mode: {shown}"
                )));
            }
            rt.set(|s| s.cache_warming(mode));
            crate::cache_warmer::set_mode(crate::cache_warmer::Mode::parse(mode));
            ok(json!({"mode": mode}))
        }
        "set_auto_retry" => {
            let enabled = bool_field(cmd, "enabled")?;
            rt.set_auto_retry(enabled);
            none()
        }
        "abort_retry" => {
            rt.abort_retry();
            none()
        }
        "abort_bash" => {
            rt.set(|s| s.abort_bash());
            none()
        }
        "get_session_stats" => ok(rt.session_stats().await),
        "export_html" => {
            if let Some(v) = cmd.get("outputPath") {
                if !v.is_string() {
                    return Err(CmdError::new("outputPath must be a string"));
                }
            }
            Err(unsupported("HTML export"))
        }
        "get_branch_messages" => {
            let entries = rt.entries().map_err(CmdError::from)?;
            ok(json!({"messages": branch_messages(&entries)}))
        }
        "get_last_assistant_text" => {
            ok(json!({"text": rt.last_assistant_text().await.map_or(Value::Null, Value::String)}))
        }
        "set_session_name" => {
            let name = str_field(cmd, "name")?.trim().to_string();
            if name.is_empty() {
                return Err(CmdError::new("Session name cannot be empty"));
            }
            rt.set_session_name(&name, "user").map_err(CmdError::from)?;
            none()
        }
        "handoff" => {
            if rt.agent.is_streaming().await {
                return Err(CmdError::new(
                    "Cannot hand off while a response is in progress",
                ));
            }
            let custom = cmd.get("customInstructions").and_then(Value::as_str);
            rt.compact(custom, Some("handoff")).await?;
            // TS `rpc-mode.ts`: `success(id, "handoff", {})` — an empty object, not null.
            ok(json!({}))
        }
        "get_messages" => ok(json!({"messages": rt.messages().await})),
        "get_messages_page" => {
            if rt.agent.is_streaming().await {
                return Err(CmdError::coded(
                    "Cannot page messages while the session is changing",
                    "session_busy",
                ));
            }
            let messages = rt.messages().await;
            let entries = rt.entries().map_err(CmdError::from)?;
            let leaf = entries
                .last()
                .and_then(|e| e.get("id").and_then(Value::as_str))
                .map(str::to_string);
            let session_id = rt.session_id();
            let snapshot = paging::Snapshot {
                session_id: &session_id,
                leaf_id: leaf.as_deref(),
                message_count: messages.len(),
            };
            let limit = cmd.get("limit").and_then(Value::as_f64);
            let limit = match limit {
                None => None,
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                Some(n) if n.fract() == 0.0 && n >= 0.0 => Some(n as usize),
                Some(_) => Some(0),
            };
            paging::page(
                &messages,
                &snapshot,
                cmd.get("cursor").and_then(Value::as_str),
                limit,
            )
            .map(Some)
            .map_err(|e| CmdError {
                message: e.message,
                code: e.code,
            })
        }
        "get_login_providers" => ok(json!({"providers": login_providers().await})),
        "login" => {
            let provider = str_field(cmd, "providerId")?;
            if !OAUTH_PROVIDERS.iter().any(|(id, _)| *id == provider) {
                return Err(CmdError::new(format!("Unknown OAuth provider: {provider}")));
            }
            Err(CmdError::new(format!(
                "Provider '{provider}' requires interactive prompts which are not supported in RPC mode. Use the terminal UI to log in."
            )))
        }
        "predict_word" => {
            text_cursor(cmd)?;
            ok(json!({"suffix": null}))
        }
        "predict_word_feedback" => {
            text_cursor(cmd)?;
            if cmd.get("suggestion").is_none_or(|v| !v.is_string())
                || cmd.get("accepted").is_none_or(|v| !v.is_boolean())
            {
                return Err(CmdError::new(
                    "suggestion must be a string and accepted a boolean",
                ));
            }
            none()
        }
        "get_protocol_info" => ok(protocol_info(rt)),
        other => Err(CmdError::coded(
            format!("Unknown command: {other}"),
            "unknown_command",
        )),
    }
}

async fn reload_with_ext(
    rt: &Arc<Runtime>,
    ext: Option<&Arc<crate::ext::Extensions>>,
) -> CmdResult {
    // Extensions reload; the session and any hold survive (the gateway reloads while holding a
    // lane). Failures propagate to the client via `CmdError` without dropping session, hold or
    // the previously published command catalog.
    if let Some(ext) = ext {
        ext.reload().await.map_err(CmdError::from)?;
    }
    let commands = slash::try_dynamic_commands_for(rt, ext)
        .await
        .map_err(CmdError::from)?;
    rt.set_available_commands(commands);
    if !rt.commands_announced() {
        rt.announce_commands();
    }
    ok(json!({"cancelled": false}))
}

/// `get_protocol_info` payload: what the gateway needs to branch on before it drives a session.
fn protocol_info(rt: &Arc<Runtime>) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverVersion": env!("CARGO_PKG_VERSION"),
        "engine": ENGINE,
        "mode": "classic",
        "capabilities": RPC_COMMANDS,
        "commands": RPC_COMMANDS,
        "agentDir": omp_providers::AgentDir::default().0.display().to_string(),
        "sessionId": rt.session_id(),
    })
}

const RPC_CHUNK_PAYLOAD_BYTES: usize = 256 * 1024;
const METADATA_STRING_CAP: usize = 1024;
const STRING_ELISION_RESERVE: usize = 80;
/// `(stringCap, arrayLimit, objectLimit)` per shrink pass, as omp's `SHRINK_PASSES`.
const SHRINK_PASSES: [(usize, usize, usize); 7] = [
    (256 * 1024, 512, 512),
    (64 * 1024, 256, 256),
    (16 * 1024, 128, 128),
    (4 * 1024, 64, 64),
    (1024, 32, 32),
    (256, 8, 16),
    (64, 1, 8),
];

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn shrink_string(s: &str, cap: usize) -> String {
    let len = s.chars().count();
    if len <= cap {
        return s.to_string();
    }
    let head = cap.saturating_sub(STRING_ELISION_RESERVE);
    let kept: String = s.chars().take(head).collect();
    format!("{kept}\n…[{} chars elided for RPC frame]", len - head)
}

fn shrink_value(
    v: &Value,
    (string_cap, array_limit, object_limit): (usize, usize, usize),
) -> Value {
    let pass = (string_cap, array_limit, object_limit);
    match v {
        Value::String(s) => Value::String(shrink_string(s, string_cap)),
        Value::Array(a) => {
            let keep = a.len().min(array_limit);
            let mut out: Vec<Value> = a.iter().take(keep).map(|e| shrink_value(e, pass)).collect();
            if keep < a.len() {
                out.push(json!(format!(
                    "…[{} more items elided for RPC frame]",
                    a.len() - keep
                )));
            }
            Value::Array(out)
        }
        Value::Object(o) => {
            let keep = o.len().min(object_limit);
            let mut out = Map::new();
            for (k, e) in o.iter().take(keep) {
                out.insert(k.clone(), shrink_value(e, pass));
            }
            if keep < o.len() {
                out.insert("rpcFrameElidedKeys".into(), json!(o.len() - keep));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `overflowFrame`: what replaces a frame that cannot fit the transport at all.
fn overflow_frame(frame: &Value) -> Value {
    let cap = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(|s| shrink_string(s, METADATA_STRING_CAP))
    };
    match frame.get("type").and_then(Value::as_str) {
        Some("response") => {
            let mut m = Map::new();
            if let Some(id) = cap(frame.get("id")) {
                m.insert("id".into(), json!(id));
            }
            m.insert("type".into(), json!("response"));
            m.insert(
                "command".into(),
                json!(cap(frame.get("command")).unwrap_or_else(|| "unknown".into())),
            );
            m.insert("success".into(), json!(false));
            m.insert("code".into(), json!("rpc_failed"));
            m.insert("errorCode".into(), json!("rpc_failed"));
            m.insert(
                "error".into(),
                json!("RPC response exceeded the transport limit"),
            );
            // A hosted client routes the failure by session; a classic frame stays untagged.
            if let Some(session) = frame.get("sessionId").and_then(Value::as_str) {
                m.insert("sessionId".into(), json!(session));
            }
            Value::Object(m)
        }
        Some("agent_end") => json!({
            "type": "agent_end",
            "messages": [],
            "messageCount": frame.get("messageCount").and_then(Value::as_u64).unwrap_or(0),
        }),
        original => {
            let mut m = Map::new();
            m.insert("type".into(), json!("rpc_frame_error"));
            if let Some(t) = original {
                m.insert(
                    "originalType".into(),
                    json!(shrink_string(t, METADATA_STRING_CAP)),
                );
            }
            m.insert(
                "error".into(),
                json!("RPC frame exceeded the transport limit"),
            );
            Value::Object(m)
        }
    }
}

/// Port of omp's `RpcFrameEncoder`: keeps every physical line within `MAX_FRAME_BYTES`
/// (protocol v1: shrink / compact / replace; v2: `rpc_chunk` sequences).
/// A multi-session host writes every hosted session's frames through one encoder: the messages a
/// client already received are tracked per `sessionId` (none on a single-session connection).
struct FrameEncoder {
    version: u8,
    streamed: HashMap<Option<String>, Vec<Value>>,
    chunks: u64,
}

fn stream_of(frame: &Value) -> Option<String> {
    frame
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

impl FrameEncoder {
    fn new() -> Self {
        Self {
            version: 1,
            streamed: HashMap::new(),
            chunks: 0,
        }
    }

    fn fits(json: &str) -> bool {
        (json.len() as u64) < MAX_FRAME_BYTES
    }

    /// `agent_end` minus the messages the client already received as `message_end` frames.
    fn compact_terminal(&self, frame: &Value) -> Value {
        let Some(messages) = frame
            .get("messages")
            .and_then(Value::as_array)
            .filter(|_| frame.get("type") == Some(&json!("agent_end")))
        else {
            return frame.clone();
        };
        let sent = self
            .streamed
            .get(&stream_of(frame))
            .map_or(&[][..], Vec::as_slice);
        let mut streamed = 0;
        while streamed < sent.len().min(messages.len()) && sent[streamed] == messages[streamed] {
            streamed += 1;
        }
        let mut out = frame.clone();
        out["messages"] = json!(messages[streamed..]);
        out["messageCount"] = json!(messages.len());
        out
    }

    fn encode_v1(&self, frame: &Value, json: &str) -> String {
        if Self::fits(json) {
            return json.to_string();
        }
        if frame.get("type") == Some(&json!("response")) {
            return overflow_frame(frame).to_string();
        }
        let compacted = self.compact_terminal(frame);
        let json = compacted.to_string();
        if Self::fits(&json) {
            return json;
        }
        for pass in SHRINK_PASSES {
            let shrunk = shrink_value(&compacted, pass).to_string();
            if Self::fits(&shrunk) {
                return shrunk;
            }
        }
        overflow_frame(&compacted).to_string()
    }

    /// Physical JSONL lines (without newlines) for one logical frame.
    fn encode(&mut self, frame: &Value) -> Vec<String> {
        let kind = frame.get("type").and_then(Value::as_str);
        let trace = std::env::var_os("OMP_RS_TURN_TRACE").is_some();
        if trace {
            crate::runtime::turn_trace!(
                "[turn-trace] encode kind={kind:?} rss={}kB",
                crate::runtime::rss_kb()
            );
        }
        if kind == Some("agent_start") {
            self.streamed.remove(&stream_of(frame));
        }
        let json = frame.to_string();
        if trace {
            crate::runtime::turn_trace!(
                "[turn-trace] encoded kind={kind:?} json={}B rss={}kB",
                json.len(),
                crate::runtime::rss_kb()
            );
        }
        let lines = if self.version == 2 && !Self::fits(&json) {
            let compacted = self.compact_terminal(frame);
            let cjson = compacted.to_string();
            if Self::fits(&cjson) {
                vec![cjson]
            } else if cjson.len() as u64 > MAX_REASSEMBLED_BYTES {
                vec![overflow_frame(&compacted).to_string()]
            } else {
                self.chunks += 1;
                let id = format!("rpc-{}", self.chunks);
                let bytes = cjson.as_bytes();
                let count = bytes.len().div_ceil(RPC_CHUNK_PAYLOAD_BYTES);
                bytes
                    .chunks(RPC_CHUNK_PAYLOAD_BYTES)
                    .enumerate()
                    .map(|(index, part)| {
                        json!({"type": "rpc_chunk", "chunkId": id, "index": index, "count": count,
                               "byteLength": bytes.len(), "data": base64(part)})
                        .to_string()
                    })
                    .collect()
            }
        } else {
            vec![self.encode_v1(frame, &json)]
        };
        match kind {
            Some("message_end") => {
                if let Some(m) = frame.get("message") {
                    self.streamed
                        .entry(stream_of(frame))
                        .or_default()
                        .push(m.clone());
                }
            }
            Some("agent_end") if frame.get("willContinue") != Some(&json!(true)) => {
                self.streamed.remove(&stream_of(frame));
            }
            _ => {}
        }
        if kind == Some("response")
            && frame.get("command") == Some(&json!("negotiate_protocol"))
            && frame.get("success") == Some(&json!(true))
        {
            self.version = 2;
        }
        lines
    }
}

/// Hook run once the runtime exists, before the `ready` frame (extension host start-up).
pub type OnStart = Box<
    dyn FnOnce(Arc<Runtime>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send,
>;

/// Run the RPC loop until stdin closes.
///
/// # Errors
/// Fails on stdin read errors.
pub async fn run(
    agent: Agent,
    target: Target,
    cwd: PathBuf,
    persist: Persistence,
    resume: Option<PathBuf>,
) -> Result<()> {
    run_with(agent, target, cwd, persist, resume, None).await
}

/// The stdout writer of one RPC connection: encodes every frame sent on `rx` until the
/// [`FLUSH_MARKER`] (or a closed stdout).
pub(crate) fn spawn_writer(mut rx: mpsc::UnboundedReceiver<Value>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        let mut encoder = FrameEncoder::new();
        while let Some(v) = rx.recv().await {
            if v.get(FLUSH_MARKER).is_some() {
                break;
            }
            let mut buf = String::new();
            for line in encoder.encode(&v) {
                buf.push_str(&line);
                buf.push('\n');
            }
            if out.write_all(buf.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    })
}

/// Flush every frame queued on `tx` so far (FIFO before the marker) and stop the writer; the
/// timeout only bounds a stalled stdout.
pub(crate) async fn finish_writer(
    tx: &mpsc::UnboundedSender<Value>,
    mut writer: tokio::task::JoinHandle<()>,
) {
    let _ = tx.send(json!({ FLUSH_MARKER: true }));
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

/// A parsed stdin line, or the `parse` failure frame it earns.
pub(crate) fn parse_command(line: &str) -> Result<Value, Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(response(
            None,
            "parse",
            Err(CmdError::new(
                "Failed to parse command: expected a JSON object",
            )),
        )),
        Err(e) => Err(response(
            None,
            "parse",
            Err(CmdError::new(format!("Failed to parse command: {e}"))),
        )),
    }
}

/// How [`start_session`] brings a session up.
pub(crate) struct StartOptions {
    /// A session file (a new one at that path when missing) bound before the hook and ready.
    pub resume: Option<PathBuf>,
    /// Runs once the runtime exists (extension host start-up).
    pub on_start: Option<OnStart>,
    /// The client answers dialogs (TS `hasUI`).
    pub has_ui: bool,
    /// Announce the connection with a `ready` frame (a single-session process).
    pub ready: bool,
}

/// Start one RPC session's runtime on the frame channel `frames`: approval binding, the `ask`
/// dialog transport, the start-up hook, then the `ready` frame and the command catalog.
pub(crate) async fn start_session(
    agent: Arc<Agent>,
    target: Target,
    cwd: PathBuf,
    persist: Persistence,
    frames: mpsc::UnboundedSender<Value>,
    options: StartOptions,
) -> Result<Arc<Runtime>> {
    let StartOptions {
        resume,
        on_start,
        has_ui,
        ready,
    } = options;
    RUNTIME_TX.with(|slot| *slot.borrow_mut() = Some(frames.clone()));
    let rt = Runtime::start(agent, target, cwd, persist, frames).await;
    if let Some(path) = resume {
        rt.load_session(&path, None)
            .await
            .map_err(|e| anyhow::anyhow!("failed to load session: {e:#}"))?;
    }
    if let Some(gate) = crate::approval::Gate::get() {
        gate.bind(&rt, has_ui);
    }
    // Background tasks report to this session (completions, lifecycle).
    crate::tools::subagents::attach_runtime(&rt);

    // The `ask` tool gets the RPC dialog transport (TS: rich `askDialog` once
    // the client sends `set_ask_dialog`, `select`/`editor` frames otherwise).
    {
        let mut tools = rt.agent.tools().await;
        if let Some(slot) = tools.iter_mut().find(|t| t.name() == "ask") {
            *slot = crate::tools::tool_with_ui(Arc::new(RpcAskUi));
        }
        if let Some(slot) = tools.iter_mut().find(|t| t.name() == "ask_user_question") {
            // `waitForAnswer: false` returns at once and delivers the answer later
            // as a user message (a follow-up when a turn is running, a new turn when
            // idle), but only into the session that asked.
            let weak_rt = Arc::downgrade(&rt);
            let follow_up: crate::tools::FollowUpSink = Arc::new(move || {
                let asked_in = weak_rt.upgrade().map(|rt| rt.switch_generation());
                let weak_rt = weak_rt.clone();
                Box::new(move |message: String| {
                    let Some(rt) = weak_rt.upgrade() else { return };
                    let Some(asked_in) = asked_in else { return };
                    tokio::spawn(async move {
                        match rt.prompt_in_generation(asked_in, &message, Some("followUp")).await {
                            Some(Err(e)) => rt.emit_notice(
                                "warning",
                                &format!(
                                    "ask_user_question answer was not delivered: {}",
                                    e.message
                                ),
                                Some("ask_user_question"),
                            ),
                            Some(Ok(())) => {}
                            None => rt.emit_notice(
                                "warning",
                                "ask_user_question answer dropped: the session changed while it was pending",
                                Some("ask_user_question"),
                            ),
                        }
                    });
                })
            });
            *slot = crate::tools::ask_user_question_with_ui(Arc::new(RpcAskUi), Some(follow_up));
        }
        crate::tools::republish_dyn_tools(&tools);
        rt.agent.set_tools(tools).await;
    }
    if let Some(hook) = on_start {
        hook(rt.clone()).await;
    }
    if ready {
        rt.send(json!({
            "type": "ready",
            "protocolVersion": 1,
            "supportedProtocolVersions": [1, 2],
            "maxFrameBytes": MAX_FRAME_BYTES,
            "maxReassembledFrameBytes": MAX_REASSEMBLED_BYTES,
        }));
    }
    rt.announce_commands();
    let dynamic = slash::dynamic_commands(&rt).await;
    if !dynamic.is_empty() {
        rt.set_available_commands(dynamic);
        rt.announce_commands();
    }
    Ok(rt)
}

/// Serve one session's commands, in arrival order, until `commands` closes; then let the accepted
/// work finish. `inflight` counts the commands accepted and not yet answered.
pub(crate) async fn serve_session(
    rt: Arc<Runtime>,
    mut commands: mpsc::UnboundedReceiver<Value>,
    inflight: Arc<tokio::sync::watch::Sender<usize>>,
) {
    let mut background = tokio::task::JoinSet::new();
    // Commands run one at a time, in arrival order, on a worker task, so this loop keeps
    // routing `extension_ui_response` (and `abort`) while a command awaits a dialog answer
    // (interactive slash commands, extension `ui.select`): handling inline deadlocked.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Value>();
    let worker = {
        let (rt, inflight) = (rt.clone(), inflight.clone());
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                let reply = handle(&rt, &cmd).await;
                rt.send(reply);
                inflight.send_modify(|n| *n = n.saturating_sub(1));
            }
        })
    };
    while let Some(cmd) = commands.recv().await {
        let kind = cmd.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "extension_ui_response" {
            // Resolve a pending ask-UI dialog (see `RpcAskUi`).
            if let Some(id) = cmd.get("id").and_then(Value::as_str) {
                resolve_pending_ui_request(id, cmd.clone());
            }
            continue;
        }
        if matches!(kind, "host_tool_result" | "host_tool_update") {
            route_host_tool_frame(cmd);
            continue;
        }
        if kind == "host_uri_result" {
            continue; // this server never opens `host_uri_request`s (no host URI bridge)
        }
        if kind == "bash" {
            let (rt, inflight) = (rt.clone(), inflight.clone());
            background.spawn(async move {
                let result = match str_field(&cmd, "command") {
                    Ok(c) => run_bash(&rt, c).await,
                    Err(e) => Err(e),
                };
                rt.send(response(cmd.get("id"), "bash", result));
                inflight.send_modify(|n| *n = n.saturating_sub(1));
            });
            continue;
        }
        // The router counted this command before handing it over; one in flight is this one.
        if matches!(kind, "abort" | "interrupt") && *inflight.borrow() > 1 {
            // A command is in flight (possibly waiting on a dialog): abort or hold the run now
            // instead of queueing behind it — `interrupt` awaits `wait_for_idle`. This one is
            // answered here, so it is released here too.
            let reply = handle(&rt, &cmd).await;
            rt.send(reply);
            inflight.send_modify(|n| *n = n.saturating_sub(1));
            continue;
        }
        let _ = cmd_tx.send(cmd);
    }
    drop(cmd_tx);
    let _ = worker.await;
    while background.join_next().await.is_some() {}
    rt.agent.wait_for_idle().await;
    // `prompt_result` / `session_settled` land a tick after the final `agent_end`.
    rt.wait_settled().await;
}

/// [`run`] with a start-up hook (see [`OnStart`]).
///
/// # Errors
/// Fails on stdin read errors.
pub async fn run_with(
    agent: Agent,
    target: Target,
    cwd: PathBuf,
    persist: Persistence,
    resume: Option<PathBuf>,
    on_start: Option<OnStart>,
) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let writer = spawn_writer(rx);
    UI_REQUESTS_CLOSED.store(false, Ordering::SeqCst);
    let rt = start_session(
        Arc::new(agent),
        target,
        cwd,
        persist,
        tx.clone(),
        StartOptions {
            resume,
            on_start,
            has_ui: true,
            ready: true,
        },
    )
    .await?;
    let (commands, queued) = mpsc::unbounded_channel::<Value>();
    let inflight = Arc::new(tokio::sync::watch::channel(0usize).0);
    let serving = tokio::spawn(serve_session(rt.clone(), queued, inflight));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match parse_command(&line) {
                Ok(cmd) => {
                    let _ = commands.send(cmd);
                }
                Err(frame) => rt.send(frame),
            }
        }
        anyhow::Ok(())
    };
    let read = read.await;
    // stdin closed: let accepted work finish and flush its frames. Host tool calls can no
    // longer be answered, so fail them instead of waiting on the agent forever.
    close_host_tools();
    close_ui_requests();
    drop(commands);
    let _ = serving.await;
    drop(rt);
    finish_writer(&tx, writer).await;
    read
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_frames_carry_code_and_error_code() {
        let frame = response(
            Some(&json!("e1")),
            "interrupt",
            Err(CmdError::coded(
                "no interactive client",
                "stop_requires_interactive_client",
            )),
        );
        assert_eq!(frame["success"], json!(false));
        assert_eq!(frame["code"], json!("stop_requires_interactive_client"));
        assert_eq!(
            frame["errorCode"],
            json!("stop_requires_interactive_client")
        );
        assert!(frame.get("data").is_none());
    }

    #[test]
    fn uncoded_failures_fall_back_to_rpc_failed() {
        let frame = response(None, "get_state", Err(CmdError::new("boom")));
        assert_eq!(frame["code"], json!("rpc_failed"));
        assert_eq!(frame["errorCode"], json!("rpc_failed"));
    }

    fn dispatch_command_patterns(source: &str) -> Vec<String> {
        let dispatch_src = source
            .split("async fn dispatch")
            .nth(1)
            .expect("dispatch fn");
        let after_match = dispatch_src
            .split("match kind {")
            .nth(1)
            .expect("dispatch match kind");
        let bytes = after_match.as_bytes();
        let mut cmds = Vec::new();
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        let mut line_comment = false;
        let mut block_comment = false;
        let mut collecting_pattern = true;
        let mut pattern = String::new();
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            let next = bytes.get(i + 1).copied();
            if line_comment {
                if b == b'\n' {
                    line_comment = false;
                }
                i += 1;
                continue;
            }
            if block_comment {
                if b == b'*' && next == Some(b'/') {
                    block_comment = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if in_str {
                if collecting_pattern && depth == 0 {
                    pattern.push(b as char);
                }
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            if b == b'/' && next == Some(b'/') {
                line_comment = true;
                i += 2;
                continue;
            }
            if b == b'/' && next == Some(b'*') {
                block_comment = true;
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = true;
                if collecting_pattern && depth == 0 {
                    pattern.push('"');
                }
                i += 1;
                continue;
            }
            if depth == 0 && b == b'=' && next == Some(b'>') {
                let pat = pattern.split(" if ").next().unwrap_or(&pattern);
                let mut rest = pat;
                while let Some(start) = rest.find('"') {
                    let after_open = &rest[start + 1..];
                    let Some(end) = after_open.find('"') else {
                        break;
                    };
                    cmds.push(after_open[..end].to_string());
                    rest = &after_open[end + 1..];
                }
                pattern.clear();
                collecting_pattern = false;
                i += 2;
                continue;
            }
            if b == b'{' {
                depth += 1;
                i += 1;
                continue;
            }
            if b == b'}' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    collecting_pattern = true;
                    pattern.clear();
                }
                i += 1;
                continue;
            }
            if depth == 0 && !collecting_pattern && b == b',' {
                collecting_pattern = true;
                pattern.clear();
                i += 1;
                continue;
            }
            if depth == 0 && collecting_pattern {
                pattern.push(b as char);
            }
            i += 1;
        }
        cmds
    }

    #[test]
    fn catalog_matches_dispatch() {
        // Every advertised command must match `dispatch`'s top-level `match kind` patterns in
        // both directions, so neither `get_protocol_info` nor `dispatch` can drift silently.
        let source = include_str!("rpc.rs");
        let mut catalog = std::collections::BTreeSet::new();
        for command in RPC_COMMANDS {
            assert!(
                catalog.insert((*command).to_string()),
                "duplicate catalog entry: {command}"
            );
        }
        let mut dispatched = std::collections::BTreeSet::new();
        for command in dispatch_command_patterns(source) {
            assert!(
                dispatched.insert(command.clone()),
                "duplicate dispatch arm pattern: {command}"
            );
        }
        let catalog_only: Vec<_> = catalog.difference(&dispatched).cloned().collect();
        let dispatch_only: Vec<_> = dispatched.difference(&catalog).cloned().collect();
        assert!(
            catalog_only.is_empty() && dispatch_only.is_empty(),
            "RPC_COMMANDS vs dispatch mismatch: catalog_only={catalog_only:?}, dispatch_only={dispatch_only:?}"
        );
    }

    #[test]
    fn unknown_command_is_coded() {
        assert_eq!(
            CmdError::coded("Unknown command: nope", "unknown_command").code,
            Some("unknown_command")
        );
    }

    #[test]
    fn protocol_info_advertises_engine_and_commands() {
        assert_eq!(PROTOCOL_VERSION, 2);
        assert_eq!(ENGINE, "oh-my-tau");
        assert!(RPC_COMMANDS.contains(&"get_protocol_info"));
        assert!(RPC_COMMANDS.contains(&"get_state"));
    }

    fn big_response(id: &str) -> Value {
        json!({"id": id, "type": "response", "command": "get_available_models", "success": true,
               "data": {"models": [{"blob": "x".repeat(1_100_000)}]}})
    }

    #[test]
    fn protocol_v1_replaces_an_oversized_response_with_omp_error() {
        let mut enc = FrameEncoder::new();
        let lines = enc.encode(&big_response("g2"));
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(
            v,
            json!({"id": "g2", "type": "response", "command": "get_available_models", "success": false,
                   "code": "rpc_failed", "errorCode": "rpc_failed",
                   "error": "RPC response exceeded the transport limit"})
        );
    }

    #[test]
    fn protocol_v1_shrinks_oversized_events_to_fit() {
        let mut enc = FrameEncoder::new();
        let frame = json!({"type": "tool_execution_end", "toolCallId": "t", "result": {"output": "y".repeat(2_000_000)}});
        let lines = enc.encode(&frame);
        assert!((lines[0].len() as u64) < MAX_FRAME_BYTES);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["type"], "tool_execution_end");
        assert!(v["result"]["output"]
            .as_str()
            .unwrap()
            .contains("chars elided for RPC frame"));
    }

    #[test]
    fn protocol_v2_chunks_reassemble_to_the_original_frame() {
        let mut enc = FrameEncoder::new();
        let negotiated = json!({"id": "n", "type": "response", "command": "negotiate_protocol", "success": true, "data": {"protocolVersion": 2}});
        assert_eq!(enc.encode(&negotiated).len(), 1);
        let frame = big_response("g2");
        let lines = enc.encode(&frame);
        assert!(lines.len() > 1);
        let mut bytes = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let c: Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                (c["type"].clone(), c["index"].clone(), c["count"].clone()),
                (json!("rpc_chunk"), json!(i), json!(lines.len()))
            );
            assert!((line.len() as u64) < MAX_FRAME_BYTES);
            bytes.extend(unbase64(c["data"].as_str().unwrap()));
            assert_eq!(c["byteLength"], frame.to_string().len());
        }
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), frame);
    }

    #[test]
    fn terminal_agent_end_drops_messages_already_streamed() {
        let mut enc = FrameEncoder::new();
        let big = json!({"role": "assistant", "content": [{"type": "text", "text": "z".repeat(700_000)}]});
        let other = json!({"role": "assistant", "content": [{"type": "text", "text": "w".repeat(700_000)}]});
        enc.encode(&json!({"type": "agent_start"}));
        enc.encode(&json!({"type": "message_end", "message": big}));
        let lines =
            enc.encode(&json!({"type": "agent_end", "messages": [big, other], "isTerminal": true}));
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["messageCount"], 2);
        assert_eq!(v["messages"], json!([other]));
    }

    // Bit-extraction casts are exact: the 24-bit accumulator is built from 6-bit sextets.
    #[allow(clippy::cast_possible_truncation)]
    fn unbase64(s: &str) -> Vec<u8> {
        const T: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let vals: Vec<u32> = s
            .bytes()
            .filter(|b| *b != b'=')
            .map(|b| T.find(b as char).unwrap() as u32)
            .collect();
        let mut out = Vec::new();
        for c in vals.chunks(4) {
            let n = c
                .iter()
                .enumerate()
                .fold(0u32, |a, (i, v)| a | (v << (18 - 6 * i)));
            out.push((n >> 16) as u8);
            if c.len() > 2 {
                out.push((n >> 8) as u8);
            }
            if c.len() > 3 {
                out.push(n as u8);
            }
        }
        out
    }

    #[test]
    fn unsupported_commands_follow_omp_validation_and_error_shapes() {
        assert_eq!(
            goal(&json!({"op": "get"})).unwrap(),
            Some(json!({"goal": null, "state": null}))
        );
        assert_eq!(
            goal(&json!({"op": "resume"})).unwrap_err().message,
            "No paused goal to resume."
        );
        assert_eq!(
            goal(&json!({"op": "create"})).unwrap_err().message,
            "objective is required when op=create"
        );
        assert_eq!(
            goal(&json!({"op": "create", "objective": "x", "token_budget": 0}))
                .unwrap_err()
                .message,
            "token_budget must be a positive integer when provided"
        );
        assert_eq!(
            goal(&json!({"op": "nope"})).unwrap_err().message,
            "Unknown goal op: nope"
        );
        assert_eq!(
            host_tool_definitions(
                &json!({"tools": [{"name": " ", "description": "d", "parameters": {}}]})
            )
            .err()
            .unwrap()
            .message,
            "Host tool at index 0 must provide a non-empty name"
        );
        assert_eq!(
            host_tool_definitions(
                &json!({"tools": [{"name": "t", "description": "d", "parameters": []}]})
            )
            .err()
            .unwrap()
            .message,
            "Host tool \"t\" must provide a JSON Schema object"
        );
        assert!(host_tool_definitions(&json!({"tools": []}))
            .unwrap()
            .is_empty());
        assert_eq!(
            set_host_uri_schemes(&json!({"schemes": [{"scheme": "1bad"}]}))
                .unwrap_err()
                .message,
            "Host URI scheme contains invalid characters: 1bad"
        );
        assert_eq!(
            set_host_uri_schemes(&json!({"schemes": [{"scheme": " XD "}]}))
                .unwrap_err()
                .message,
            "Host URI scheme is reserved by OMP: xd://"
        );
        assert_eq!(
            set_host_uri_schemes(&json!({"schemes": []})).unwrap(),
            Some(json!({"schemes": []}))
        );
        assert_eq!(
            subagent_messages(&json!({})).unwrap_err().message,
            "get_subagent_messages requires subagentId or sessionFile"
        );
        assert_eq!(
            subagent_messages(&json!({"subagentId": "s1"}))
                .unwrap_err()
                .message,
            "Unknown subagent or session file unavailable: s1"
        );
        assert!(text_cursor(&json!({"text": "héllo", "cursor": 5})).is_ok());
        assert_eq!(
            text_cursor(&json!({"text": "hi", "cursor": 3}))
                .unwrap_err()
                .message,
            "cursor must be an integer UTF-16 offset within text"
        );
    }

    async fn rpc_test_runtime(
        dir: &std::path::Path,
    ) -> (Arc<Runtime>, mpsc::UnboundedReceiver<Value>) {
        let model: pa_types::ai::Model = serde_json::from_value(json!({
            "id": "a", "name": "a", "api": "openai-completions", "provider": "p",
            "baseUrl": "http://127.0.0.1:9", "reasoning": false, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 100_000, "maxTokens": 4096
        }))
        .unwrap();
        let agent = pa_agent::agent::Agent::new(pa_agent::agent::AgentOptions {
            initial_state: pa_agent::agent::AgentInitialState {
                system_prompt: Some("test".into()),
                model: crate::adapter::json_round_trip(&model),
                thinking_level: Some(pa_agent::types::ThinkingLevel::Off),
                tools: Some(Vec::new()),
                messages: None,
            },
            ..pa_agent::agent::AgentOptions::default()
        });
        let persist = crate::persist::attach_in(&agent, dir, dir, None, crate::persist::uuid_v7())
            .await
            .unwrap();
        let target = Arc::new(std::sync::RwLock::new(Some(
            crate::adapter::ProviderTarget {
                api_key: Some("k".into()),
                model,
                service_tier: None,
                headers: None,
                credential_id: None,
            },
        )));
        let (tx, rx) = mpsc::unbounded_channel();
        let rt = Runtime::start(Arc::new(agent), target, dir.to_path_buf(), persist, tx).await;
        (rt, rx)
    }

    #[tokio::test]
    async fn reload_returns_cmd_error_on_failure_and_refreshes_command_catalog_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let fail_flag = dir.path().join("fail");
        let phase_file = dir.path().join("phase");
        std::fs::write(&phase_file, "1").unwrap();

        let script = dir.path().join("reload-host.sh");
        std::fs::write(
            &script,
            concat!(
                "while IFS= read -r line; do\n",
                "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n",
                "  p=$(cat \"$PHASE_FILE\")\n",
                "  case \"$line\" in\n",
                "    *'\"method\":\"load\"'*)\n",
                "      if [ -f \"$FAIL_FILE\" ]; then\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{\"code\":-32000,\"message\":\"boom reload\"}}\\n' \"$id\"\n",
                "      else\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"loaded\":[],\"handlers\":[],\"errors\":[]}}\\n' \"$id\"\n",
                "      fi;;\n",
                "    *'\"method\":\"list_commands\"'*)\n",
                "      if [ \"$p\" = \"1\" ]; then\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":[{\"name\":\"old_cmd\",\"description\":\"old\",\"extension\":\"ext\"}]}\\n' \"$id\"\n",
                "      elif [ \"$p\" = \"fail_list\" ]; then\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{\"code\":-32001,\"message\":\"boom list_commands\"}}\\n' \"$id\"\n",
                "      else\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":[{\"name\":\"new_cmd\",\"description\":\"new\",\"extension\":\"ext\"}]}\\n' \"$id\"\n",
                "      fi;;\n",
                "    *'\"method\":\"list_tools\"'*)\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":[]}\\n' \"$id\";;\n",
                "    *)\n",
                "      if [ -n \"$id\" ]; then\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{}}\\n' \"$id\"\n",
                "      fi;;\n",
                "  esac\n",
                "done\n",
            ),
        )
        .unwrap();
        let host = omp_ext::ExtHost::spawn(&omp_ext::HostConfig {
            bun: "/bin/sh".into(),
            host_script: script,
            trace: None,
            handler_timeout_ms: None,
            env: vec![
                ("FAIL_FILE".into(), fail_flag.display().to_string()),
                ("PHASE_FILE".into(), phase_file.display().to_string()),
            ],
            shared_socket: None,
        })
        .await
        .unwrap();
        let ext = crate::ext::Extensions::for_test(host, dir.path());
        let (rt, mut rx) = rpc_test_runtime(dir.path()).await;
        rt.set_client_info(&json!({"width": 80, "capabilities": ["interactive_user"]}))
            .unwrap();
        rt.pause_for_stop().await.unwrap();
        let session_before = rt.session_id();
        let initial_cmds = slash::dynamic_commands_for(&rt, Some(&ext)).await;
        rt.set_available_commands(initial_cmds);
        rt.announce_commands();
        while rx.try_recv().is_ok() {}

        // Reload failure must propagate CmdError while keeping session + hold intact.
        std::fs::write(&fail_flag, "1").unwrap();
        let err_frame = response(
            Some(&json!("r1")),
            "reload",
            reload_with_ext(&rt, Some(&ext)).await,
        );
        assert_eq!(err_frame["success"], json!(false));
        assert!(err_frame["error"].as_str().unwrap().contains("boom reload"));
        assert_eq!(rt.session_id(), session_before);
        assert_eq!(rt.state().await["paused"], json!(true));

        // Reload success must recompute slash commands before announcing.
        std::fs::remove_file(&fail_flag).unwrap();
        std::fs::write(&phase_file, "2").unwrap();
        let ok_frame = response(
            Some(&json!("r2")),
            "reload",
            reload_with_ext(&rt, Some(&ext)).await,
        );
        assert_eq!(ok_frame["success"], json!(true));
        let names: Vec<String> = rt
            .available_commands()
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.contains(&"new_cmd".to_string()), "{names:?}");
        assert!(!names.contains(&"old_cmd".to_string()), "{names:?}");
        let announced = std::iter::from_fn(|| rx.try_recv().ok())
            .find(|f| f["type"] == "available_commands_update")
            .expect("available_commands_update emitted");
        let announced_names: Vec<String> = announced["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            announced_names.contains(&"new_cmd".to_string()),
            "{announced_names:?}"
        );
        assert!(
            !announced_names.contains(&"old_cmd".to_string()),
            "{announced_names:?}"
        );

        // If `load` succeeds but `list_commands` fails, return CmdError and keep the existing catalog.
        std::fs::write(&phase_file, "fail_list").unwrap();
        let list_err = response(
            Some(&json!("r3")),
            "reload",
            reload_with_ext(&rt, Some(&ext)).await,
        );
        assert_eq!(list_err["success"], json!(false));
        assert!(list_err["error"]
            .as_str()
            .unwrap()
            .contains("boom list_commands"));
        let kept_names: Vec<String> = rt
            .available_commands()
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            kept_names.contains(&"new_cmd".to_string()),
            "{kept_names:?}"
        );

        ext.host.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod host_tool_tests {
    use super::*;
    use std::time::Duration;

    /// Pending calls and the closed flag are process-global; serialize the tests.
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    const WAIT: Duration = Duration::from_secs(5);

    struct Harness {
        tool: Arc<HostTool>,
        frames: mpsc::UnboundedReceiver<Value>,
        updates: Arc<Mutex<Vec<AgentToolResult>>>,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let (tx, frames) = mpsc::unbounded_channel();
            let def = host_tool_definitions(&json!({"tools": [{"name": name,
                "description": " d ", "parameters": {"type": "object"}}]}))
            .unwrap()
            .pop()
            .unwrap();
            let sink: FrameSink = Arc::new(move |frame| {
                tx.send(frame).unwrap();
            });
            Self {
                tool: Arc::new(HostTool { def, sink }),
                frames,
                updates: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn start(&self, signal: AbortSignal) -> tokio::task::JoinHandle<Result<AgentToolResult>> {
            let updates = self.updates.clone();
            let on_update: AgentToolUpdateCallback = Arc::new(move |r| lock(&updates).push(r));
            let tool = self.tool.clone();
            tokio::spawn(tool.execute("c1".into(), json!({"message": "hi"}), signal, on_update))
        }

        async fn next_frame(&mut self) -> Value {
            tokio::time::timeout(WAIT, self.frames.recv())
                .await
                .expect("frame")
                .expect("open")
        }
    }

    async fn settle(
        call: tokio::task::JoinHandle<Result<AgentToolResult>>,
    ) -> Result<AgentToolResult> {
        tokio::time::timeout(WAIT, call)
            .await
            .expect("settled")
            .unwrap()
    }

    #[test]
    fn definitions_are_trimmed_and_labelled() {
        let defs = host_tool_definitions(&json!({"tools": [
            {"name": " echo ", "description": " says ", "parameters": {}},
            {"name": "b", "label": " Bee ", "description": "d", "parameters": {}}
        ]}))
        .unwrap();
        assert_eq!(
            (defs[0].name.as_str(), defs[0].label.as_str()),
            ("echo", "echo")
        );
        assert_eq!(defs[0].description, "says");
        assert_eq!(defs[1].label, "Bee");
    }

    #[tokio::test]
    async fn call_streams_updates_then_resolves_with_the_result() {
        let _guard = TEST_LOCK.lock().await;
        let mut h = Harness::new("echo");
        let call = h.start(AbortSignal::never());
        let frame = h.next_frame().await;
        assert_eq!(frame["type"], "host_tool_call");
        assert_eq!(frame["toolCallId"], "c1");
        assert_eq!(frame["toolName"], "echo");
        assert_eq!(frame["arguments"], json!({"message": "hi"}));
        let id = frame["id"].as_str().unwrap().to_string();
        assert!(route_host_tool_frame(
            json!({"type": "host_tool_update", "id": id,
            "partialResult": {"content": [{"type": "text", "text": "half"}]}})
        ));
        assert!(route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id,
            "result": {"content": [{"type": "text", "text": "hi"}], "details": {"n": 1}}})
        ));
        let result = settle(call).await.unwrap();
        assert_eq!(result.content, vec![ToolResultContent::text("hi")]);
        assert_eq!(result.details, json!({"n": 1}));
        assert_eq!(
            lock(&h.updates).clone(),
            vec![AgentToolResult::text("half")]
        );
        // Settled: a late result for the same id has nowhere to go.
        assert!(!route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id, "result": {"content": []}})
        ));
    }

    #[tokio::test]
    async fn error_results_reject_with_their_text() {
        let _guard = TEST_LOCK.lock().await;
        let mut h = Harness::new("fail");
        let call = h.start(AbortSignal::never());
        let id = h.next_frame().await["id"].as_str().unwrap().to_string();
        assert!(route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id, "isError": true,
            "result": {"content": [{"type": "text", "text": " boom "}]}})
        ));
        assert_eq!(settle(call).await.unwrap_err().to_string(), "boom");

        let call = h.start(AbortSignal::never());
        let id = h.next_frame().await["id"].as_str().unwrap().to_string();
        assert!(route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id, "isError": true,
            "result": {"content": []}})
        ));
        assert_eq!(
            settle(call).await.unwrap_err().to_string(),
            "Host tool execution failed"
        );
    }

    #[tokio::test]
    async fn malformed_or_unknown_frames_are_not_routed() {
        let _guard = TEST_LOCK.lock().await;
        let mut h = Harness::new("echo");
        let call = h.start(AbortSignal::never());
        let id = h.next_frame().await["id"].as_str().unwrap().to_string();
        assert!(!route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id, "result": {"content": "x"}})
        ));
        assert!(!route_host_tool_frame(
            json!({"type": "host_tool_result", "id": "nope", "result": {"content": []}})
        ));
        assert!(route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id, "result": {"content": []}})
        ));
        assert_eq!(settle(call).await.unwrap().content, vec![]);
    }

    #[tokio::test]
    async fn abort_emits_host_tool_cancel_and_rejects() {
        let _guard = TEST_LOCK.lock().await;
        let mut h = Harness::new("slow");
        let ctl = AbortController::new();
        let call = h.start(ctl.signal());
        let id = h.next_frame().await["id"].as_str().unwrap().to_string();
        ctl.abort();
        let cancel = h.next_frame().await;
        assert_eq!(cancel["type"], "host_tool_cancel");
        assert_eq!(cancel["targetId"], json!(id));
        assert_ne!(cancel["id"], json!(id));
        assert_eq!(
            settle(call).await.unwrap_err().to_string(),
            "Host tool \"slow\" was aborted"
        );
        assert!(!route_host_tool_frame(
            json!({"type": "host_tool_result", "id": id, "result": {"content": []}})
        ));
    }

    #[tokio::test]
    async fn client_disconnect_fails_pending_and_later_calls() {
        let _guard = TEST_LOCK.lock().await;
        let mut h = Harness::new("echo");
        let call = h.start(AbortSignal::never());
        h.next_frame().await;
        close_host_tools();
        let pending = settle(call).await.unwrap_err().to_string();
        let later = settle(h.start(AbortSignal::never()))
            .await
            .unwrap_err()
            .to_string();
        HOST_TOOLS_CLOSED.store(false, Ordering::SeqCst);
        assert_eq!(pending, HOST_TOOLS_DISCONNECTED);
        assert_eq!(later, HOST_TOOLS_DISCONNECTED);
    }
}

#[cfg(test)]
mod ask_ui_tests {
    use super::*;

    /// The pending-dialog registry is process-global; serialize the tests.
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn ask_ui_resolves_from_extension_ui_response() {
        let _guard = TEST_LOCK.lock().await;
        let questions = json!([{"id": "q1", "schema": {}}]);
        let ui = RpcAskUi;
        ASK_DIALOG_ENABLED.store(true, Ordering::SeqCst);
        // `RpcAskUi` is a zero-sized handle; build a fresh one inside the task
        // instead of moving the outer `ui` (used for assertions below).
        let ui_task = tokio::spawn(async { AskUi::ask(&RpcAskUi, questions, None).await });
        // Wait for the request frame, then answer it.
        let id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                tokio::task::yield_now().await;
                let pending = lock(&PENDING_UI_REQUESTS);
                if let Some(key) = pending.keys().next().cloned() {
                    return key;
                }
            }
        })
        .await
        .expect("pending dialog registered");
        let response = json!({"type": "extension_ui_response", "id": id,
            "answers": [{"id": "q1", "value": "ok"}]});
        let tx = lock(&PENDING_UI_REQUESTS).remove(&id).expect("waiter");
        let _ = tx.1.send(response);
        let result = ui_task.await.unwrap().unwrap().unwrap();
        assert_eq!(result["answers"][0]["value"], json!("ok"));
        // A client that opted into tau's ask dialog does not get gateway `question` frames.
        assert!(!AskUi::has_question_dialog(&ui));
        ASK_DIALOG_ENABLED.store(false, Ordering::SeqCst);
        assert!(!AskUi::has_ask_dialog(&ui));
        assert!(AskUi::has_question_dialog(&ui));
    }

    #[tokio::test]
    async fn ask_ui_times_out_with_timed_out_frame() {
        let _guard = TEST_LOCK.lock().await;
        let mut frame = Map::new();
        frame.insert("questions".into(), json!([]));
        frame.insert("timeout".into(), json!(50));
        let result = RpcAskUi::request("ask", frame).await.unwrap().unwrap();
        assert_eq!(result["cancelled"], json!(true));
        assert_eq!(result["timedOut"], json!(true));
    }
}
