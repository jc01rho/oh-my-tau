//! `task_send`, `task_output`, `task_cancel`: the model-facing controls of
//! background tasks started by `task` with `run_in_background` (OmOMeow
//! engine contract: parameter names and result wording follow its
//! `senpi-task` control/output tools).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use omp_task::registry::{
    CancelOutcome, Continuation, Lookup, SendOutcome, SubagentRegistry, TaskStatus, TaskView,
};
use omp_task::transcript::{TranscriptMode, DEFAULT_TAIL_LINES};
use pa_agent::types::AgentToolResult;
use serde_json::{json, Value};

use super::subagents::{caller_session, clip, format_duration, registry};
use super::Env;

pub const SEND_DESCRIPTION: &str = "Send a message to a background child task, keyed by `to` (task id `st_...` or name). A running child is steered immediately: the message reaches it at its next turn boundary. A message to a completed child revives that same session for another run. Cancelled, errored and lost children are not revived. `message` may also be {type:'shutdown_request', reason?}, which cancels the child. Only this session's children are reachable unless all_scope is true.";

pub const OUTPUT_DESCRIPTION: &str = "Read one background child task, keyed by task_id or name. task_output always returns immediately: mode='status' (default) returns one record snapshot per observed progress state, then a short no_progress result until the task changes. mode='tail' returns the last tail_lines of the recorded transcript; mode='full' returns the whole transcript (capped, with a head/tail elision marker). Completion notifications already include the final result. READ-ONLY: this never revives, steers, or otherwise touches the child. Only the current session's children are visible.";

pub const CANCEL_DESCRIPTION: &str = "Cancel a running child task and release its resources; the cancelled status is preserved so task_output can still report the outcome. Cancel is terminal and NOT resumable; cancelling a child that is not running is a no-op that reports its unchanged status, except that a child which already died (error or lost) is released: it keeps its result and stops counting as an open child. Use this to end work you no longer need.";

const MISSING_TARGET: &str = "Provide task_id or name to identify the child task.";
const BLOCKING_REMOVED: &str =
    "blocking removed - completion arrives as a notification; use mode:\"tail\" to peek.";
const STATUS_RESULT_MAX_CHARS: usize = 4_000;
const MAX_STATUS_READS: usize = 1_024;

#[must_use]
pub fn send_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "to": {"type": "string", "description": "Child task id (st_...) or name."},
            "message": {"anyOf": [
                {"type": "string", "description": "The instruction or context to deliver."},
                {"type": "object", "properties": {
                    "type": {"type": "string", "const": "shutdown_request"},
                    "reason": {"type": "string", "description": "Optional shutdown request reason."}
                }, "required": ["type"]}
            ]},
            "summary": {"type": "string", "description": "Optional one-line summary of the message."},
            "all_scope": {"type": "boolean", "description": "Allow messaging a child owned by another session. Off by default."}
        },
        "required": ["to"]
    })
}

#[must_use]
pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string", "description": "Task id (st_...) of the child to read. Provide exactly one of task_id or name."},
            "name": {"type": "string", "description": "Canonical task name; required if task_id is omitted."},
            "mode": {"type": "string", "enum": ["status", "tail", "full"], "description": "status (default) = record snapshot; tail = last lines of the transcript; full = whole transcript."},
            "tail_lines": {"type": "integer", "minimum": 1, "description": "Lines to keep in tail mode. Defaults to 60."}
        }
    })
}

#[must_use]
pub fn cancel_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string", "description": "Task id (st_...) of the child to cancel. Provide exactly one of task_id or name."},
            "name": {"type": "string", "description": "Canonical task name; required if task_id is omitted."},
            "reason": {"type": "string", "description": "Optional human-readable reason recorded on the cancelled task."}
        }
    })
}

pub async fn task_send(_env: Arc<Env>, args: Value) -> Result<AgentToolResult> {
    Ok(run_send(&registry(), &caller_session(), &args).await)
}

pub async fn task_output(_env: Arc<Env>, args: Value) -> Result<AgentToolResult> {
    Ok(run_output(&registry(), &caller_session(), &args))
}

pub async fn task_cancel(_env: Arc<Env>, args: Value) -> Result<AgentToolResult> {
    Ok(run_cancel(&registry(), &caller_session(), &args))
}

fn reply(text: String, details: Value) -> AgentToolResult {
    let mut result = AgentToolResult::text(text);
    result.details = details;
    result
}

fn refusal(text: String, details: Value) -> AgentToolResult {
    let mut result = reply(text, details);
    result.is_error = Some(true);
    result
}

fn not_found(target: &str, known: &[String]) -> AgentToolResult {
    let reason = format!("No task '{target}' in this session.");
    let list = if known.is_empty() {
        String::new()
    } else {
        format!(" Known tasks in this session: {}.", known.join(", "))
    };
    refusal(
        format!("{reason}{list}"),
        json!({"kind": "not_found", "reason": reason, "known_tasks": known}),
    )
}

fn target_of(args: &Value) -> Option<&str> {
    ["task_id", "name"]
        .iter()
        .filter_map(|key| args.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|v| !v.is_empty())
}

pub(crate) async fn run_send(
    registry: &Arc<SubagentRegistry>,
    caller: &str,
    args: &Value,
) -> AgentToolResult {
    let Some(to) = args
        .get("to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return refusal(
            "Provide `to`: the child task id (st_...) or name.".to_string(),
            json!({"kind": "invalid_arguments"}),
        );
    };
    if args.get("team_run_id").is_some_and(|v| !v.is_null()) {
        return refusal(
            "Teams are not available in this runtime; task_send reaches background child tasks only.".to_string(),
            json!({"kind": "invalid_arguments"}),
        );
    }
    let all_scope = args
        .get("all_scope")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match args.get("message") {
        Some(Value::String(message)) if !message.trim().is_empty() => {
            send_result(registry.send(to, message, Some(caller), all_scope).await)
        }
        Some(Value::Object(message)) => match message.get("type").and_then(Value::as_str) {
            Some("shutdown_request") => {
                let reason = message
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|r| !r.trim().is_empty())
                    .unwrap_or("shutdown requested");
                cancel_result(registry.cancel(to, Some(reason), Some(caller), all_scope), to)
            }
            Some("shutdown_response") => refusal(
                "shutdown_response answers a team shutdown negotiation; background child tasks have none.".to_string(),
                json!({"kind": "invalid_arguments"}),
            ),
            _ => refusal(
                "message must be a string or {type:'shutdown_request', reason?}.".to_string(),
                json!({"kind": "invalid_arguments"}),
            ),
        },
        _ => refusal(
            "message is required: the instruction or context to deliver.".to_string(),
            json!({"kind": "invalid_arguments"}),
        ),
    }
}

fn send_result(outcome: SendOutcome) -> AgentToolResult {
    match outcome {
        SendOutcome::Steered(view) => reply(
            format!("Delivered to {} as steer.", view.id()),
            json!({"kind": "steered", "task_id": view.id(), "status": view.status().as_str(), "delivered": "steer"}),
        ),
        SendOutcome::Revived {
            view,
            run_epoch,
            undelivered,
        } => {
            let mut text = format!("Revived {} (run epoch {run_epoch}).", view.id());
            if let Some(previous) = &undelivered {
                text.push_str("\nIts previous run's result had not reached you yet:\n");
                text.push_str(&clip(previous, STATUS_RESULT_MAX_CHARS).0);
            }
            reply(
                text,
                json!({"kind": "revived", "task_id": view.id(), "run_epoch": run_epoch, "previous_result": undelivered}),
            )
        }
        SendOutcome::NotContinuable { view, reason } => reply(
            reason.clone(),
            json!({"kind": "not_continuable", "task_id": view.id(), "status": view.status().as_str(), "reason": reason}),
        ),
        SendOutcome::NotFound { known } => not_found_any(&known),
        SendOutcome::ScopeDenied(view) => {
            let reason = format!(
                "Task {} belongs to session {}; pass all_scope to send across sessions.",
                view.id(),
                view.record.parent_session_id
            );
            refusal(
                reason.clone(),
                json!({"kind": "scope_denied", "task_id": view.id(), "owning_session_id": view.record.parent_session_id, "reason": reason}),
            )
        }
    }
}

fn not_found_any(known: &[String]) -> AgentToolResult {
    let list = if known.is_empty() {
        String::new()
    } else {
        format!(" Known tasks in this session: {}.", known.join(", "))
    };
    refusal(
        format!("No such task in this session.{list}"),
        json!({"kind": "not_found", "known_tasks": known}),
    )
}

pub(crate) fn run_cancel(
    registry: &SubagentRegistry,
    caller: &str,
    args: &Value,
) -> AgentToolResult {
    let Some(target) = target_of(args) else {
        return refusal(
            MISSING_TARGET.to_string(),
            json!({"kind": "invalid_arguments"}),
        );
    };
    let reason = args.get("reason").and_then(Value::as_str);
    cancel_result(registry.cancel(target, reason, Some(caller), false), target)
}

fn cancel_result(outcome: CancelOutcome, target: &str) -> AgentToolResult {
    match outcome {
        CancelOutcome::Cancelled { view, previous } => reply(
            format!(
                "Cancelled {} (was {}, now {}).",
                view.id(),
                previous.as_str(),
                TaskStatus::Cancelled.as_str()
            ),
            json!({"kind": "cancelled", "task_id": view.id(), "previous_status": previous.as_str(), "status": "cancelled"}),
        ),
        CancelOutcome::Noop {
            view,
            released: true,
        } => reply(
            format!(
                "Released {}: it already ended as {}, keeps its result, and no longer counts as an open child. No change.",
                view.id(),
                view.status().as_str()
            ),
            json!({"kind": "released", "task_id": view.id(), "status": view.status().as_str()}),
        ),
        CancelOutcome::Noop { view, .. } => reply(
            format!("Task {} is {}. No change.", view.id(), view.status().as_str()),
            json!({"kind": "noop", "task_id": view.id(), "status": view.status().as_str()}),
        ),
        CancelOutcome::NotFound { known } => not_found(target, &known),
        // Fail-closed: another session's id reads as unknown here.
        CancelOutcome::ScopeDenied(_) => not_found(target, &[]),
    }
}

/// Per (caller session, task id) fingerprint of the last status read, so an
/// unchanged task is not replayed to the same caller (bounded, oldest out).
static STATUS_READS: Mutex<VecDeque<((String, String), String)>> = Mutex::new(VecDeque::new());

fn unchanged_since_last_read(key: (String, String), fingerprint: String) -> bool {
    let mut reads = STATUS_READS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(index) = reads.iter().position(|(existing, _)| *existing == key) {
        if reads[index].1 == fingerprint {
            return true;
        }
        reads.remove(index);
    }
    reads.push_back((key, fingerprint));
    if reads.len() > MAX_STATUS_READS {
        reads.pop_front();
    }
    false
}

fn fingerprint(view: &TaskView) -> String {
    let record = &view.record;
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        record.status.as_str(),
        record.run_epoch,
        view.live,
        record.dismissed,
        record.output_tokens,
        record.session_file.as_deref().unwrap_or(""),
        record.updated_at_ms
    )
}

pub(crate) fn run_output(
    registry: &SubagentRegistry,
    caller: &str,
    args: &Value,
) -> AgentToolResult {
    if args.get("block").is_some() || args.get("timeout_ms").is_some() {
        return refusal(
            BLOCKING_REMOVED.to_string(),
            json!({"kind": "invalid_arguments"}),
        );
    }
    let Some(target) = target_of(args) else {
        return refusal(
            MISSING_TARGET.to_string(),
            json!({"kind": "invalid_arguments"}),
        );
    };
    let mode = match args.get("mode").and_then(Value::as_str) {
        None | Some("status") => None,
        Some("tail") => {
            let lines = match args.get("tail_lines") {
                None | Some(Value::Null) => DEFAULT_TAIL_LINES,
                Some(value) => match value.as_u64().filter(|n| *n >= 1) {
                    Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
                    None => {
                        return refusal(
                            "tail_lines must be a positive integer.".to_string(),
                            json!({"kind": "invalid_arguments"}),
                        )
                    }
                },
            };
            Some(TranscriptMode::Tail(lines))
        }
        Some("full") => Some(TranscriptMode::Full),
        Some(other) => {
            return refusal(
                format!("mode must be \"status\", \"tail\" or \"full\", got {other:?}."),
                json!({"kind": "invalid_arguments"}),
            )
        }
    };
    let view = match registry.lookup(target, Some(caller), false) {
        Lookup::Found(view) => view,
        Lookup::NotFound { known } => return not_found(target, &known),
        Lookup::ScopeDenied(_) => return not_found(target, &registry.known_tasks(Some(caller))),
    };
    match mode {
        None => {
            let key = (caller.to_string(), view.id().to_string());
            if unchanged_since_last_read(key, fingerprint(&view)) {
                no_progress(&view)
            } else {
                reply(
                    status_text(&view),
                    json!({"kind": "status", "task": status_details(&view)}),
                )
            }
        }
        Some(mode) => transcript_result(&view, mode),
    }
}

fn status_details(view: &TaskView) -> Value {
    let record = &view.record;
    json!({
        "task_id": record.task_id,
        "name": record.name,
        "agent": record.agent,
        "status": record.status.as_str(),
        "live": view.live,
        "open": view.is_open(),
        "run_epoch": record.run_epoch,
        "duration_ms": record.duration_ms,
        "output_tokens": record.output_tokens,
        "session_file": record.session_file,
        "error": record.error_message,
    })
}

fn status_text(view: &TaskView) -> String {
    let record = &view.record;
    let mut text = format!(
        "{} [{}] agent:{}",
        record.task_id,
        record.status.as_str(),
        record.agent
    );
    if let Some(name) = &record.name {
        text.push_str(&format!(" name:{name}"));
    }
    if record.run_epoch > 1 {
        text.push_str(&format!(" epoch:{}", record.run_epoch));
    }
    if !view.live && record.status.is_terminal() {
        text.push_str(&format!(
            " duration:{}",
            format_duration(record.duration_ms)
        ));
    }
    if record.output_tokens > 0 {
        text.push_str(&format!(" tokens:{}", record.output_tokens));
    }
    if let Some(file) = &record.session_file {
        text.push_str(&format!("\nsession: {file}"));
    }
    match record.status {
        TaskStatus::Completed => {
            if let Some(result) = &record.final_response {
                text.push_str(&format!(
                    "\nresult: {}",
                    clip(result, STATUS_RESULT_MAX_CHARS).0
                ));
            }
            if let Some(note) = &record.error_message {
                text.push_str(&format!("\nnote: {note}"));
            }
        }
        TaskStatus::Cancelled => {
            if let Some(reason) = &record.cancel_reason {
                text.push_str(&format!("\ncancelled: {reason}"));
            }
        }
        TaskStatus::Lost => {
            text.push_str(&format!(
                "\nlost: {}",
                record
                    .error_message
                    .as_deref()
                    .unwrap_or("no settlement was recorded")
            ));
        }
        TaskStatus::Error => {
            if let Some(error) = &record.error_message {
                text.push_str(&format!("\nerror: {error}"));
            }
        }
        TaskStatus::Pending | TaskStatus::Running => {}
    }
    if !view.live && view.is_open() && matches!(record.status, TaskStatus::Error | TaskStatus::Lost)
    {
        text.push_str("\nThis dead child counts as an open child until task_cancel releases it.");
    }
    match view.continuation() {
        Continuation::Steer => text.push_str(&format!(
            "\nnext: task_send({{ to: \"{}\", message: \"...\" }}) steers it now.",
            record.task_id
        )),
        Continuation::Revive => text.push_str(&format!(
            "\nnext: task_send({{ to: \"{}\", message: \"...\" }}) revives it for another run.",
            record.task_id
        )),
        Continuation::NotContinuable(_) => {}
    }
    text
}

fn no_progress(view: &TaskView) -> AgentToolResult {
    let reason = if view.live || view.notification_pending() {
        format!(
            "Task {} has not changed since the last status read. Await its completion notification; use mode:\"tail\" only for explicit transcript diagnosis.",
            view.id()
        )
    } else {
        format!(
            "Task {} is {}. It will not send a completion notification; use mode:\"tail\" only for explicit transcript diagnosis.",
            view.id(),
            view.status().as_str()
        )
    };
    reply(
        reason.clone(),
        json!({"kind": "no_progress", "task_id": view.id(), "status": view.status().as_str(), "reason": reason}),
    )
}

fn transcript_result(view: &TaskView, mode: TranscriptMode) -> AgentToolResult {
    let head = format!("{} [{}]", view.id(), view.status().as_str());
    match view.transcript(mode) {
        None => reply(
            format!("{head} has no transcript yet: the child has not reported its session file."),
            json!({"kind": "transcript", "task_id": view.id(), "transcript": "", "truncated": false}),
        ),
        Some(Err(e)) => refusal(
            format!("{head} transcript unreadable: {e}"),
            json!({"kind": "transcript_error", "task_id": view.id()}),
        ),
        Some(Ok(transcript)) => reply(
            format!("{head} transcript via session file:\n{}", transcript.text),
            json!({
                "kind": "transcript",
                "task_id": view.id(),
                "mode": if mode == TranscriptMode::Full { "full" } else { "tail" },
                "truncated": transcript.truncated,
                "total_lines": transcript.total_lines,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::task::spawn_background;
    use omp_task::params::SchemaMode;
    use omp_task::{TaskRunner, TaskSpec};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Fake `--mode rpc` child: reports a session file, logs each prompt and
    /// answer into it, answers `answer:<msg>` (a `hold` prompt waits for a
    /// steer, a `fail` prompt settles as an error).
    const FAKE_RPC: &str = r#"
[ "$1" = "--mode" ] && [ "$2" = "rpc" ] || exit 2
sess="$(dirname "$0")/session-$$.jsonl"
answer=""; holding=""
field() { printf '%s\n' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }
note() { printf '{"type":"message","message":{"role":"%s","content":[{"type":"text","text":"%s"}]}}\n' "$1" "$2" >> "$sess"; }
settle() {
  note assistant "$answer"
  if [ "$1" = error ]; then echo '{"type":"prompt_result","status":"error","error":"boom","sessionSettled":true}'
  else echo '{"type":"prompt_result","status":"completed","sessionSettled":true}'; fi
}
while IFS= read -r line; do
  case "$line" in
    *'"type":"get_state"'*) printf '{"type":"response","command":"get_state","success":true,"data":{"sessionFile":"%s"}}\n' "$sess" ;;
    *'"type":"switch_session"'*) sess=$(field "$line" sessionPath); echo '{"type":"response","command":"switch_session","success":true,"data":{"cancelled":false}}' ;;
    *'"type":"prompt"'*)
      msg=$(field "$line" message); note user "$msg"
      echo '{"type":"response","command":"prompt","success":true}'
      case "$msg" in *hold*) holding=1 ;; *fail*) answer="failed"; settle error ;; *) answer="answer:$msg"; settle ok ;; esac ;;
    *'"type":"steer"'*) msg=$(field "$line" message); note user "$msg"
      if [ -n "$holding" ]; then holding=""; answer="steered:$msg"; settle ok; fi ;;
    *'"type":"get_last_assistant_text"'*) printf '{"type":"response","command":"get_last_assistant_text","success":true,"data":{"text":"%s"}}\n' "$answer" ;;
    *'"type":"get_session_stats"'*) echo '{"type":"response","command":"get_session_stats","success":true,"data":{"tokens":{"output":5},"cost":0.1}}' ;;
  esac
done
"#;

    fn fake_registry(dir: &Path) -> Arc<SubagentRegistry> {
        let bin = dir.join("fake-omp-rs");
        let mut f = std::fs::File::create(&bin).unwrap();
        writeln!(f, "#!/bin/sh\n{FAKE_RPC}").unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        SubagentRegistry::new(TaskRunner::new(bin, 4), Some(dir.join("tasks")))
    }

    fn spec(dir: &Path, prompt: &str, name: Option<&str>) -> TaskSpec {
        TaskSpec {
            id: name.map(str::to_string),
            agent: "task".to_string(),
            prompt: prompt.to_string(),
            cwd: PathBuf::from(dir),
            output_schema: None,
            schema_mode: SchemaMode::Permissive,
            model: None,
            tools: None,
            depth: 1,
            extra_args: Vec::new(),
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match &result.content[0] {
            pa_agent::types::ToolResultContent::Text(t) => t.text.clone(),
            pa_agent::types::ToolResultContent::Image(_) => String::new(),
        }
    }

    async fn settled(registry: &SubagentRegistry, id: &str, epoch: u64) {
        let mut rx = registry.subscribe();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let done = registry.get(id).is_some_and(|v| {
                    !v.live && v.record.run_epoch == epoch && v.status().is_terminal()
                });
                if done {
                    return;
                }
                rx.changed().await.expect("registry alive");
            }
        })
        .await
        .expect("task settled within 30s");
    }

    #[tokio::test]
    async fn background_spawn_returns_task_ids_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let registry = fake_registry(dir.path());
        let started = std::time::Instant::now();
        let result = spawn_background(
            &registry,
            vec![
                spec(dir.path(), "hold one", Some("Alpha")),
                spec(dir.path(), "two", None),
            ],
            Some("two scouts"),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "spawning never waits for a child"
        );
        let text = text_of(&result);
        assert!(text.starts_with("Started 2 background task(s)"), "{text}");
        let tasks = result.details["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        let first = tasks[0]["task_id"].as_str().unwrap().to_string();
        assert!(omp_task::is_task_id(&first), "{first}");
        assert_eq!(tasks[0]["name"], "Alpha");
        let caller = caller_session();
        let view = registry.get(&first).unwrap();
        assert_eq!(view.record.parent_session_id, caller);
        assert_eq!(view.record.description.as_deref(), Some("two scouts"));
        assert!(view.live);

        let cancelled = run_cancel(&registry, &caller, &json!({"name": "Alpha"}));
        assert!(
            text_of(&cancelled).starts_with(&format!("Cancelled {first} (was ")),
            "{}",
            text_of(&cancelled)
        );
        let second = tasks[1]["task_id"].as_str().unwrap().to_string();
        settled(&registry, &first, 1).await;
        settled(&registry, &second, 1).await;
    }

    #[tokio::test]
    async fn output_status_then_no_progress_then_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let registry = fake_registry(dir.path());
        let caller = "output-session";
        let id = registry
            .spawn(omp_task::SpawnRequest {
                spec: spec(dir.path(), "count", None),
                name: Some("Counter".to_string()),
                description: None,
                parent_session_id: caller.to_string(),
                root_session_id: caller.to_string(),
                env: Vec::new(),
            })
            .unwrap()
            .id()
            .to_string();
        settled(&registry, &id, 1).await;

        let status = text_of(&run_output(&registry, caller, &json!({"task_id": id})));
        assert!(
            status.starts_with(&format!("{id} [completed] agent:task name:Counter")),
            "{status}"
        );
        assert!(status.contains("result: answer:count"), "{status}");
        assert!(status.contains("revives it for another run"), "{status}");
        let again = run_output(&registry, caller, &json!({"name": "Counter"}));
        assert_eq!(again.details["kind"], "no_progress");
        let tail = run_output(
            &registry,
            caller,
            &json!({"name": "Counter", "mode": "tail", "tail_lines": 1}),
        );
        assert_eq!(
            text_of(&tail),
            format!("{id} [completed] transcript via session file:\nassistant: answer:count")
        );
        assert_eq!(tail.details["truncated"], true);
        let full = text_of(&run_output(
            &registry,
            caller,
            &json!({"task_id": id, "mode": "full"}),
        ));
        assert!(
            full.ends_with("user: count\nassistant: answer:count"),
            "{full}"
        );

        let other = run_output(&registry, "someone-else", &json!({"task_id": id}));
        assert_eq!(other.is_error, Some(true));
        assert!(text_of(&other).starts_with(&format!("No task '{id}' in this session.")));
        let blocking = run_output(&registry, caller, &json!({"task_id": id, "block": true}));
        assert_eq!(text_of(&blocking), BLOCKING_REMOVED);
        assert_eq!(
            text_of(&run_output(&registry, caller, &json!({}))),
            MISSING_TARGET
        );
    }

    #[tokio::test]
    async fn send_steers_revives_and_refuses_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let registry = fake_registry(dir.path());
        let caller = "send-session";
        let spawn = |prompt: &str, name: &str| {
            registry
                .spawn(omp_task::SpawnRequest {
                    spec: spec(dir.path(), prompt, None),
                    name: Some(name.to_string()),
                    description: None,
                    parent_session_id: caller.to_string(),
                    root_session_id: caller.to_string(),
                    env: Vec::new(),
                })
                .unwrap()
                .id()
                .to_string()
        };
        let held = spawn("hold please", "Held");
        let steered = run_send(&registry, caller, &json!({"to": "Held", "message": "go"})).await;
        assert_eq!(text_of(&steered), format!("Delivered to {held} as steer."));
        settled(&registry, &held, 1).await;
        assert_eq!(
            registry
                .get(&held)
                .unwrap()
                .record
                .final_response
                .as_deref(),
            Some("steered:go")
        );

        let revived = run_send(&registry, caller, &json!({"to": held, "message": "more"})).await;
        assert!(
            text_of(&revived).starts_with(&format!("Revived {held} (run epoch 2).")),
            "{}",
            text_of(&revived)
        );
        assert_eq!(
            revived.details["previous_result"], "steered:go",
            "the undelivered result goes back with the send"
        );
        settled(&registry, &held, 2).await;
        assert_eq!(
            registry
                .get(&held)
                .unwrap()
                .record
                .final_response
                .as_deref(),
            Some("answer:more")
        );

        let doomed = spawn("hold forever", "Doomed");
        let shutdown = run_send(
            &registry,
            caller,
            &json!({"to": "Doomed", "message": {"type": "shutdown_request", "reason": "done"}}),
        )
        .await;
        assert!(text_of(&shutdown).starts_with(&format!("Cancelled {doomed} (was ")));
        settled(&registry, &doomed, 1).await;
        let refused = run_send(&registry, caller, &json!({"to": doomed, "message": "hi"})).await;
        assert_eq!(refused.details["kind"], "not_continuable");
        let denied = run_send(&registry, "intruder", &json!({"to": held, "message": "hi"})).await;
        assert_eq!(denied.details["kind"], "scope_denied");
        let missing = run_send(&registry, caller, &json!({"to": "Nobody", "message": "hi"})).await;
        assert_eq!(missing.details["known_tasks"], json!(["Held", "Doomed"]));
        let empty = run_send(&registry, caller, &json!({"to": held})).await;
        assert_eq!(empty.is_error, Some(true));
    }

    #[tokio::test]
    async fn cancel_releases_a_dead_child() {
        let dir = tempfile::tempdir().unwrap();
        let registry = fake_registry(dir.path());
        let caller = "cancel-session";
        let id = registry
            .spawn(omp_task::SpawnRequest {
                spec: spec(dir.path(), "fail it", None),
                name: None,
                description: None,
                parent_session_id: caller.to_string(),
                root_session_id: caller.to_string(),
                env: Vec::new(),
            })
            .unwrap()
            .id()
            .to_string();
        settled(&registry, &id, 1).await;
        assert!(registry.begin_notify(&id, 1));
        registry.finish_notify(&id, 1);
        assert_eq!(registry.open_children(caller), 1);
        let released = run_cancel(&registry, caller, &json!({"task_id": id}));
        assert_eq!(released.details["kind"], "released");
        assert_eq!(registry.open_children(caller), 0);
        let again = run_cancel(&registry, caller, &json!({"task_id": id}));
        assert_eq!(text_of(&again), format!("Task {id} is error. No change."));
    }

    #[test]
    fn control_schemas_match_the_engine_contract() {
        let send = send_schema();
        assert_eq!(send["required"], json!(["to"]));
        assert!(send["properties"]["message"]["anyOf"].is_array());
        assert!(send["properties"]["all_scope"].is_object());
        let output = output_schema();
        assert_eq!(
            output["properties"]["mode"]["enum"],
            json!(["status", "tail", "full"])
        );
        assert_eq!(output["properties"]["tail_lines"]["minimum"], 1);
        let cancel = cancel_schema();
        for key in ["task_id", "name", "reason"] {
            assert!(cancel["properties"][key].is_object(), "{key}");
        }
    }
}
