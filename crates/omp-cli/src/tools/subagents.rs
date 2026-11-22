//! The process's background-task registry ([`omp_task::registry`]) and the
//! parent-session side of the OmOMeow subagent contract:
//!
//! - completion notifications: an idle session is woken with the result, a
//!   running turn takes it at its next turn boundary, a held (Stop) session
//!   gets it on resume;
//! - the session lifecycle record: after every `agent_settled` (and whenever
//!   the count changes) a `session_lifecycle` frame and a `session-lifecycle`
//!   custom entry carry `waitingOn.children`, the open children the gateway
//!   reads before it treats the session as finished (`fleet.ts`
//!   `stillWaiting`, `session-evidence.ts` done gate);
//! - the subagent RPC commands over the same registry.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use omp_task::registry::{
    CancelOutcome, Continuation, RegistryEvent, SendOutcome, SubagentRegistry, TaskStatus, TaskView,
};
use omp_task::TaskRunner;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::runtime::{CmdError, Runtime};

/// Env var naming the top-level session of a spawning tree, stamped on every
/// background child so its own children record the same root.
pub const ROOT_SESSION_ENV: &str = "OMP_TASK_ROOT_SESSION_ID";
const NOTIFY_RESULT_MAX_CHARS: usize = 12_000;

static REGISTRY: OnceLock<Arc<SubagentRegistry>> = OnceLock::new();
static RUNTIME: Mutex<Option<Weak<Runtime>>> = Mutex::new(None);
static LAST_LIFECYCLE: Mutex<Option<(String, &'static str, usize)>> = Mutex::new(None);

/// The process-wide registry; the first call loads this project's store.
pub fn registry() -> Arc<SubagentRegistry> {
    REGISTRY
        .get_or_init(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("omt"));
            let settings = omp_settings::Settings::load(&cwd);
            let slots =
                TaskRunner::resolve_max_concurrency(settings.get_number("task.maxConcurrency"));
            let registry =
                SubagentRegistry::new(TaskRunner::new(binary, slots), Some(store_dir(&cwd)));
            registry.load();
            let weak = Arc::downgrade(&registry);
            registry.set_listener(Arc::new(move |event| {
                if let Some(registry) = weak.upgrade() {
                    on_event(&registry, event);
                }
            }));
            registry
        })
        .clone()
}

fn existing_registry() -> Option<Arc<SubagentRegistry>> {
    REGISTRY.get().cloned()
}

/// The gateway's `senpi-task` fallback store layout under this engine's
/// agent dir: `<agent dir>/projects/<name>-<sha256(realpath cwd)[:12]>/senpi-task/tasks`.
fn store_dir(cwd: &Path) -> PathBuf {
    let real = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let name = real
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || matches!(c, '_' | '.' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let digest = Sha256::digest(real.display().to_string().as_bytes());
    let hash: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    omp_providers::AgentDir::default()
        .0
        .join("projects")
        .join(format!("{name}-{hash}"))
        .join("senpi-task")
        .join("tasks")
}

/// Bind the RPC runtime: background completions are delivered to it and its
/// lifecycle carries the open children. Loads the store right away, so the
/// lost children of a previous process count before the first spawn.
pub(crate) fn attach_runtime(rt: &Arc<Runtime>) {
    *RUNTIME.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(rt));
    drop(registry());
}

fn attached_runtime() -> Option<Arc<Runtime>> {
    RUNTIME
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade)
}

fn is_attached(rt: &Runtime) -> bool {
    attached_runtime().is_some_and(|attached| std::ptr::eq(Arc::as_ptr(&attached), rt))
}

/// The session that owns tasks spawned now: the attached runtime's session,
/// else a per-process id (TUI and print runs keep their tasks to themselves).
pub(crate) fn caller_session() -> String {
    attached_runtime().map_or_else(
        || format!("pid-{}", std::process::id()),
        |rt| rt.session_id(),
    )
}

pub(crate) fn root_session(caller: &str) -> String {
    std::env::var(ROOT_SESSION_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| caller.to_string())
}

fn on_event(registry: &Arc<SubagentRegistry>, event: &RegistryEvent) {
    let Some(rt) = attached_runtime() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let registry = Arc::clone(registry);
    match event {
        RegistryEvent::Settled(view) => {
            let view = TaskView::clone(view);
            handle.spawn(async move {
                deliver(&rt, &registry, &view).await;
                publish_lifecycle(&rt, &registry).await;
            });
        }
        RegistryEvent::Changed { parent_session_id } => {
            if *parent_session_id == rt.session_id() {
                handle.spawn(async move { publish_lifecycle(&rt, &registry).await });
            }
        }
    }
}

/// Hand one settled run to its parent: a prompt with `steer` behavior wakes
/// an idle session and queues into a running turn. A held session keeps it
/// pending until [`flush_pending`].
async fn deliver(rt: &Arc<Runtime>, registry: &SubagentRegistry, view: &TaskView) {
    if !view.notification_pending()
        || view.record.parent_session_id != rt.session_id()
        || rt.is_paused()
    {
        return;
    }
    let epoch = view.record.run_epoch;
    if !registry.begin_notify(view.id(), epoch) {
        return;
    }
    if let Err(e) = rt
        .prompt(None, &notification_text(view), Some("steer"), Vec::new())
        .await
    {
        registry.abort_notify(view.id(), epoch);
        eprintln!(
            "omt: background task {} completion not delivered: {}",
            view.id(),
            e.message
        );
    }
}

/// Deliver every pending completion of the attached session (after a resume
/// or a settle).
pub(crate) fn flush_pending() {
    let (Some(rt), Some(registry)) = (attached_runtime(), existing_registry()) else {
        return;
    };
    let pending = registry.pending_notifications(&rt.session_id());
    if pending.is_empty() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        for view in pending {
            deliver(&rt, &registry, &view).await;
        }
        publish_lifecycle(&rt, &registry).await;
    });
}

fn lifecycle_state(streaming: bool, children: usize) -> &'static str {
    if streaming {
        "running"
    } else if children > 0 {
        "waiting_children"
    } else {
        "done"
    }
}

/// The lifecycle snapshot the gateway parses (`waitingOn.children`,
/// `waitingOn.background`, `waitingOn.human`).
pub(crate) fn lifecycle_value(state: &str, children: usize) -> Value {
    json!({
        "state": state,
        "waitingOn": {"children": children, "background": [], "human": []},
    })
}

fn emit_lifecycle(rt: &Runtime, session: &str, state: &'static str, children: usize, force: bool) {
    {
        let mut last = LAST_LIFECYCLE
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (session.to_string(), state, children);
        if !force && last.as_ref() == Some(&key) {
            return;
        }
        *last = Some(key);
    }
    let lifecycle = lifecycle_value(state, children);
    rt.send(json!({"type": "session_lifecycle", "sessionId": session, "lifecycle": lifecycle}));
    rt.journal_custom("session-lifecycle", lifecycle);
}

async fn publish_lifecycle(rt: &Runtime, registry: &SubagentRegistry) {
    let session = rt.session_id();
    if !registry.has_children(&session) {
        return;
    }
    let streaming = rt.agent.is_streaming().await;
    let children = registry.open_children(&session);
    emit_lifecycle(
        rt,
        &session,
        lifecycle_state(streaming, children),
        children,
        false,
    );
}

/// Right after `agent_settled`: the lifecycle record that tells the gateway
/// whether the settled session still waits on children (always sent, since
/// the gateway's fleet runner reads the first record after each settle),
/// then any completion still pending.
pub(crate) fn on_session_settled(rt: &Runtime) {
    if !is_attached(rt) {
        return;
    }
    let Some(registry) = existing_registry() else {
        return;
    };
    let session = rt.session_id();
    if !registry.has_children(&session) {
        return;
    }
    let children = registry.open_children(&session);
    emit_lifecycle(
        rt,
        &session,
        lifecycle_state(false, children),
        children,
        true,
    );
    flush_pending();
}

/// The `<task-notification>` a parent receives when a background run settles.
pub(crate) fn notification_text(view: &TaskView) -> String {
    let record = &view.record;
    let mut head = String::from("task completion");
    if let Some(name) = &record.name {
        head.push_str(&format!(" name:{name}"));
    }
    head.push_str(&format!(
        " id:{} agent:{} status:{} duration:{}",
        record.task_id,
        record.agent,
        record.status.as_str(),
        format_duration(record.duration_ms)
    ));
    if record.run_epoch > 1 {
        head.push_str(&format!(" epoch:{}", record.run_epoch));
    }
    if record.output_tokens > 0 {
        head.push_str(&format!(" tokens:{}", record.output_tokens));
    }
    let mut lines = vec![head];
    match record.status {
        TaskStatus::Completed => {
            if let Some(result) = &record.final_response {
                let (shown, cut) = clip(result, NOTIFY_RESULT_MAX_CHARS);
                lines.push(format!("result:\"{shown}\""));
                if cut || record.truncated {
                    lines.push("result_note:truncated; task_output with mode:\"full\" shows the whole transcript".to_string());
                }
            }
            lines.push(format!(
                "next:Use task_send({{ to: \"{}\", message: \"...\" }}) to continue it.",
                record.task_id
            ));
        }
        TaskStatus::Error | TaskStatus::Lost => {
            if let Some(error) = &record.error_message {
                lines.push(format!("error:\"{error}\""));
            }
            lines.push(format!(
                "next:It is not revived. Read task_output for its transcript, redo the missing piece, then task_cancel {} so it stops counting as an open child.",
                record.task_id
            ));
        }
        _ => {}
    }
    format!(
        "<task-notification>\n{}\n</task-notification>",
        lines.join("\n")
    )
}

pub(crate) fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1_000)
    }
}

pub(crate) fn clip(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((cut, _)) => (format!("{}…", &text[..cut]), true),
        None => (text.to_string(), false),
    }
}

fn subagent_view(view: &TaskView) -> Value {
    let record = &view.record;
    json!({
        "id": record.task_id,
        "name": record.name,
        "agent": record.agent,
        "description": record.description,
        "status": record.status.as_str(),
        "live": view.live,
        "open": view.is_open(),
        "continuation": match view.continuation() {
            Continuation::Steer => "steer",
            Continuation::Revive => "revive",
            Continuation::NotContinuable(_) => "none",
        },
        "runEpoch": record.run_epoch,
        "parentSessionId": record.parent_session_id,
        "rootSessionId": record.root_session_id,
        "sessionFile": record.session_file,
        "createdAt": record.created_at_ms,
        "updatedAt": record.updated_at_ms,
        "endedAt": record.ended_at_ms,
        "durationMs": record.duration_ms,
        "outputTokens": record.output_tokens,
        "error": record.error_message,
    })
}

/// `get_subagents`: the background tasks `session` spawned, oldest first.
pub(crate) fn rpc_get_subagents(session: &str) -> Value {
    let subagents: Vec<Value> = existing_registry()
        .map(|registry| registry.list(Some(session)))
        .unwrap_or_default()
        .iter()
        .map(subagent_view)
        .collect();
    json!({ "subagents": subagents })
}

fn unknown_subagent(id: &str) -> CmdError {
    CmdError::coded(format!("Unknown subagent: {id}"), "subagent_not_found")
}

/// `cancel_subagent`: cancel a running task, or release a settled open one.
///
/// # Errors
/// `subagent_not_found` for an unknown id.
pub(crate) fn rpc_cancel_subagent(id: &str, reason: Option<&str>) -> Result<Value, CmdError> {
    let registry = existing_registry().ok_or_else(|| unknown_subagent(id))?;
    match registry.cancel(id, reason, None, true) {
        CancelOutcome::Cancelled { previous, .. } => Ok(json!({
            "cancelled": true,
            "status": TaskStatus::Cancelled.as_str(),
            "previousStatus": previous.as_str(),
        })),
        CancelOutcome::Noop { view, released } => Ok(json!({
            "cancelled": false,
            "status": view.status().as_str(),
            "released": released,
        })),
        CancelOutcome::NotFound { .. } | CancelOutcome::ScopeDenied(_) => Err(unknown_subagent(id)),
    }
}

/// `steer_subagent`: steer a running task or revive a completed one.
///
/// # Errors
/// `subagent_not_found` for an unknown id, `subagent_not_running` for a
/// task that takes no more messages.
pub(crate) async fn rpc_steer_subagent(id: &str, message: &str) -> Result<Value, CmdError> {
    let registry = existing_registry().ok_or_else(|| unknown_subagent(id))?;
    match registry.send(id, message, None, true).await {
        SendOutcome::Steered(view) => Ok(json!({
            "subagentId": id,
            "delivered": "steer",
            "runEpoch": view.record.run_epoch,
        })),
        SendOutcome::Revived { run_epoch, .. } => Ok(json!({
            "subagentId": id,
            "delivered": "revive",
            "runEpoch": run_epoch,
        })),
        SendOutcome::NotContinuable { reason, .. } => {
            Err(CmdError::coded(reason, "subagent_not_running"))
        }
        SendOutcome::NotFound { .. } | SendOutcome::ScopeDenied(_) => Err(unknown_subagent(id)),
    }
}

/// `get_subagent_messages`: the message entries of a task's session file
/// from byte `from` on (a partial first line is skipped).
///
/// # Errors
/// An unknown task or session file, or an unreadable file.
pub(crate) fn rpc_subagent_messages(
    id: Option<&str>,
    session_file: Option<&str>,
    from: u64,
) -> Result<Value, CmdError> {
    let registry = existing_registry();
    let view = match (id, session_file) {
        (Some(id), _) => registry.as_ref().and_then(|r| r.get(id)).ok_or_else(|| {
            CmdError::new(format!(
                "Unknown subagent or session file unavailable: {id}"
            ))
        })?,
        (None, Some(file)) => registry
            .as_ref()
            .and_then(|r| {
                r.list(None)
                    .into_iter()
                    .find(|v| v.record.session_file.as_deref() == Some(file))
            })
            .ok_or_else(|| CmdError::new("Unknown subagent session file"))?,
        (None, None) => {
            return Err(CmdError::new(
                "get_subagent_messages requires subagentId or sessionFile",
            ))
        }
    };
    let Some(file) = view.record.session_file.clone() else {
        return Err(CmdError::new(format!(
            "Unknown subagent or session file unavailable: {}",
            view.id()
        )));
    };
    let bytes = std::fs::read(&file)
        .map_err(|e| CmdError::new(format!("Cannot read subagent session file {file}: {e}")))?;
    let len = bytes.len();
    let start = usize::try_from(from).unwrap_or(usize::MAX).min(len);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 && bytes[start - 1] != b'\n' {
        text = text
            .split_once('\n')
            .map_or_else(String::new, |(_, rest)| rest.to_string());
    }
    let messages: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|entry| entry.get("message").cloned())
        .collect();
    Ok(json!({
        "subagentId": view.id(),
        "sessionFile": file,
        "fromByte": start,
        "nextByte": len,
        "messages": messages,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use omp_task::registry::{SpawnRecord, TaskRecord};

    fn view(status: TaskStatus) -> TaskView {
        TaskView {
            record: TaskRecord {
                task_id: "st_0123456789ab".to_string(),
                name: Some("Scout".to_string()),
                agent: "task".to_string(),
                description: None,
                parent_session_id: "p".to_string(),
                root_session_id: "p".to_string(),
                status,
                run_epoch: 2,
                notified_epoch: 1,
                dismissed: false,
                created_at_ms: 0,
                updated_at_ms: 0,
                ended_at_ms: Some(0),
                duration_ms: 12_340,
                cwd: "/".to_string(),
                session_file: Some("/s.jsonl".to_string()),
                final_response: Some("found it".to_string()),
                truncated: false,
                error_message: Some("boom".to_string()),
                failure_kind: None,
                cancel_reason: None,
                output_tokens: 42,
                run_stats: None,
                owner_pid: 1,
                spawn: SpawnRecord::default(),
            },
            live: false,
        }
    }

    #[test]
    fn lifecycle_record_matches_the_gateway_shape() {
        let value = lifecycle_value(lifecycle_state(false, 2), 2);
        assert_eq!(value["state"], "waiting_children");
        assert_eq!(value["waitingOn"]["children"], 2);
        assert_eq!(value["waitingOn"]["background"], json!([]));
        assert_eq!(value["waitingOn"]["human"], json!([]));
        assert_eq!(lifecycle_state(false, 0), "done");
        assert_eq!(lifecycle_state(true, 3), "running");
    }

    #[test]
    fn completion_notification_carries_result_and_next_step() {
        let text = notification_text(&view(TaskStatus::Completed));
        assert!(text.starts_with("<task-notification>\n"), "{text}");
        assert!(text.contains(
            "task completion name:Scout id:st_0123456789ab agent:task status:completed duration:12.3s epoch:2 tokens:42"
        ), "{text}");
        assert!(text.contains("result:\"found it\""));
        assert!(text.contains("task_send({ to: \"st_0123456789ab\""));
        let failed = notification_text(&view(TaskStatus::Error));
        assert!(failed.contains("error:\"boom\""));
        assert!(failed.contains("task_cancel st_0123456789ab"));
        assert!(!failed.contains("result:"));
    }

    #[test]
    fn store_follows_the_senpi_task_project_layout() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("my proj");
        std::fs::create_dir_all(&project).unwrap();
        let store = store_dir(&project);
        let real = std::fs::canonicalize(&project).unwrap();
        let digest = Sha256::digest(real.display().to_string().as_bytes());
        let hash: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        assert!(store.ends_with(format!("projects/my_proj-{hash}/senpi-task/tasks")));
    }

    #[test]
    fn durations_read_like_the_engine() {
        assert_eq!(format_duration(950), "950ms");
        assert_eq!(format_duration(12_340), "12.3s");
        assert_eq!(format_duration(125_000), "2m05s");
    }
}
