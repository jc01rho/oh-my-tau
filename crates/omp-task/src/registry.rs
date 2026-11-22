//! Background subagents: the `st_<12hex>` task registry behind the `task`
//! tool's `run_in_background`, `task_send`, `task_output`, `task_cancel` and
//! the subagent RPC commands (OmOMeow gateway contract, design `pieces/12`).
//!
//! Every background task is an `omp-rs --mode rpc` child started through
//! [`TaskRunner::start_with`]: a running child takes steering messages at its
//! next turn boundary and a cancel kills its whole process group. A child
//! exits once its prompt settles, but its session file stays, so a message to
//! a finished task revives it: a new child reopens that session and takes the
//! message as its next prompt (the task's next run epoch). Cancel is terminal.
//!
//! Records persist as `<store>/<task_id>.json` with the field names the
//! gateway's `senpi-task` store readers use (`task_id`, `parent_session_id`,
//! `root_session_id`, `run_stats`, `failure_kind`, `error_message`). A record
//! whose owner process died before its run settled is reconciled to `lost`
//! on the next load: nobody recorded its outcome.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::params::SchemaMode;
use crate::runner::{
    ChildMode, RunControl, RunJoin, RunOutput, RunPhase, RunStatus, StartOptions, SteerError,
    TaskRunner, TaskSpec,
};
use crate::transcript::{render_transcript, Transcript, TranscriptMode};

pub const TASK_ID_PREFIX: &str = "st_";
const TASK_ID_HEX_DIGITS: usize = 12;
/// Settled records older than this are pruned when a store is loaded.
const RETENTION: Duration = Duration::from_secs(14 * 24 * 3600);
/// `error_message` of a record reconciled to [`TaskStatus::Lost`].
pub const LOST_EXPLANATION: &str = "No settlement was recorded: the process that ran this task exited before the task finished, so its outcome is unknown.";
const ERROR_MESSAGE_MAX_CHARS: usize = 2_000;
const KNOWN_TASKS_LISTED: usize = 20;

/// Lifecycle status of a background task (the gateway's task vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting for a concurrency slot; nothing spawned yet.
    Pending,
    /// A child process works on the prompt.
    Running,
    Completed,
    /// The run failed (non-zero exit, rejected prompt, schema violation).
    Error,
    /// Cancelled by the parent; terminal and never revived.
    Cancelled,
    /// The run never recorded a result (its owner process died).
    Lost,
}

impl TaskStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    /// Settling in this status sends the parent a completion notification.
    #[must_use]
    pub fn notifies(self) -> bool {
        matches!(self, Self::Completed | Self::Error | Self::Lost)
    }
}

/// What a revival re-launches (the spawn inputs a run epoch does not carry).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub depth: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
}

/// Token and cost totals of the child session (its `get_session_stats`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub cost_status: String,
    pub tool_calls: u64,
}

impl RunStats {
    #[must_use]
    pub fn from_session_stats(stats: &Value) -> Self {
        let count = |pointer: &str| stats.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input_tokens: count("/tokens/input"),
            output_tokens: count("/tokens/output"),
            cache_read_tokens: count("/tokens/cacheRead"),
            cache_write_tokens: count("/tokens/cacheWrite"),
            cost_usd: stats.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
            cost_status: "reported".to_string(),
            tool_calls: count("/toolCalls"),
        }
    }
}

/// The persisted state of one background task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    /// Caller-chosen handle, unique within the parent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Agent definition the child runs as.
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parent_session_id: String,
    /// The top-level session of the spawning tree.
    pub root_session_id: String,
    pub status: TaskStatus,
    /// 1 for the first run; every revival starts the next epoch.
    pub run_epoch: u64,
    /// Highest epoch whose completion reached the parent.
    #[serde(default)]
    pub notified_epoch: u64,
    /// A settled task the parent released with `task_cancel`: it no longer
    /// counts as an open child.
    #[serde(default)]
    pub dismissed: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    /// Duration of the latest run.
    #[serde(default)]
    pub duration_ms: u64,
    pub cwd: String,
    /// The child's session file (transcript source, reopened on revival).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// The latest run's final answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_response: Option<String>,
    /// The final answer hit the runner's output cap.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// `child_error`, `spawn_failed` or `lost` for a failed task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_stats: Option<RunStats>,
    /// The process that runs (or ran) the task; a load reconciles only the
    /// records of dead owners.
    pub owner_pid: u32,
    #[serde(default)]
    pub spawn: SpawnRecord,
}

/// A record plus what the live run adds to it.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskView {
    /// The record; for a live run `status` reflects its phase.
    pub record: TaskRecord,
    /// A child process runs (or waits for a slot) for the current epoch.
    pub live: bool,
}

/// How a message to a task is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuation {
    /// The run is going: the message steers it at its next turn boundary.
    Steer,
    /// The run completed: a new run reopens its session with the message.
    Revive,
    NotContinuable(Stop),
}

/// Why a task takes no more messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    Errored,
    Lost,
    NoSession,
}

impl Stop {
    /// The model-facing reason, naming the task.
    #[must_use]
    pub fn explain(self, task_id: &str) -> String {
        match self {
            Self::Cancelled => format!("{task_id} was cancelled; cancel is terminal and a cancelled task is never revived. Spawn a new task for the remaining work."),
            Self::Errored => format!("{task_id} ended with an error and is not revived. Read its transcript with task_output, spawn a new task for the remaining work, and task_cancel this one so it stops counting as an open child."),
            Self::Lost => format!("{task_id} was lost (its run never recorded a result) and cannot be revived. Spawn a new task for the remaining work, and task_cancel this one so it stops counting as an open child."),
            Self::NoSession => format!("{task_id} has no session file to reopen, so it cannot be revived. Spawn a new task for the remaining work."),
        }
    }
}

impl TaskView {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.record.task_id
    }

    #[must_use]
    pub fn status(&self) -> TaskStatus {
        self.record.status
    }

    /// Whether the parent session still waits on this task: a live run, a
    /// completion not yet delivered, or a dead child (error/lost) the parent
    /// has not released with `task_cancel`.
    #[must_use]
    pub fn is_open(&self) -> bool {
        if self.live {
            return true;
        }
        let record = &self.record;
        if record.dismissed {
            return false;
        }
        match record.status {
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Error | TaskStatus::Lost => {
                true
            }
            TaskStatus::Completed => record.notified_epoch < record.run_epoch,
            TaskStatus::Cancelled => false,
        }
    }

    #[must_use]
    pub fn continuation(&self) -> Continuation {
        match (self.live, self.record.status) {
            (_, TaskStatus::Cancelled) => Continuation::NotContinuable(Stop::Cancelled),
            (true, _) => Continuation::Steer,
            (false, TaskStatus::Completed) if self.record.session_file.is_some() => {
                Continuation::Revive
            }
            (false, TaskStatus::Completed) => Continuation::NotContinuable(Stop::NoSession),
            (false, TaskStatus::Error) => Continuation::NotContinuable(Stop::Errored),
            (false, _) => Continuation::NotContinuable(Stop::Lost),
        }
    }

    /// A settled run whose completion the parent has not received yet.
    #[must_use]
    pub fn notification_pending(&self) -> bool {
        !self.live
            && !self.record.dismissed
            && self.record.status.notifies()
            && self.record.notified_epoch < self.record.run_epoch
    }

    /// The child's transcript, read from its session file; `None` before the
    /// child reported one.
    #[must_use]
    pub fn transcript(&self, mode: TranscriptMode) -> Option<std::io::Result<Transcript>> {
        let file = self.record.session_file.as_deref()?;
        Some(std::fs::read_to_string(file).map(|text| render_transcript(&text, mode)))
    }
}

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// The child spec; its `id` is replaced by the new task id.
    pub spec: TaskSpec,
    pub name: Option<String>,
    pub description: Option<String>,
    pub parent_session_id: String,
    pub root_session_id: String,
    /// Extra child environment, kept for revivals.
    pub env: Vec<(String, String)>,
}

/// Result of resolving a task id or name for a caller.
#[derive(Debug, Clone)]
pub enum Lookup {
    Found(TaskView),
    /// Nothing matched; `known` lists the caller's task names (ids when unnamed).
    NotFound {
        known: Vec<String>,
    },
    /// The id belongs to another session.
    ScopeDenied(TaskView),
}

#[derive(Debug, Clone)]
pub enum SendOutcome {
    Steered(TaskView),
    Revived { view: TaskView, run_epoch: u64 },
    NotContinuable { view: TaskView, reason: String },
    NotFound { known: Vec<String> },
    ScopeDenied(TaskView),
}

#[derive(Debug, Clone)]
pub enum CancelOutcome {
    Cancelled {
        view: TaskView,
        previous: TaskStatus,
    },
    /// Nothing was running; `released` is true when the settled task stopped
    /// counting as an open child.
    Noop {
        view: TaskView,
        released: bool,
    },
    NotFound {
        known: Vec<String>,
    },
    ScopeDenied(TaskView),
}

#[derive(Debug, Clone)]
pub enum RegistryEvent {
    /// A run settled (any status).
    Settled(Box<TaskView>),
    /// Something else about the session's tasks changed.
    Changed { parent_session_id: String },
}

pub type Listener = Arc<dyn Fn(&RegistryEvent) + Send + Sync>;

struct Entry {
    record: TaskRecord,
    control: Option<RunControl>,
}

/// The background-task registry (one per process in production).
pub struct SubagentRegistry {
    runner: TaskRunner,
    store: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, Entry>>,
    listener: RwLock<Option<Listener>>,
    changes: watch::Sender<u64>,
    owner_pid: u32,
}

impl SubagentRegistry {
    /// A registry over `runner` (its semaphore bounds the background
    /// children) persisting records under `store` (`None`: memory only).
    #[must_use]
    pub fn new(runner: TaskRunner, store: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            runner,
            store,
            entries: Mutex::new(BTreeMap::new()),
            listener: RwLock::new(None),
            changes: watch::Sender::new(0),
            owner_pid: std::process::id(),
        })
    }

    #[must_use]
    pub fn store(&self) -> Option<&Path> {
        self.store.as_deref()
    }

    /// Install the listener (replacing any earlier one). It runs on the
    /// thread that caused the event, never under the registry lock.
    pub fn set_listener(&self, listener: Listener) {
        *self
            .listener
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(listener);
    }

    /// A receiver that observes every change (a generation counter).
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load the store: records of dead owner processes join the registry
    /// (unsettled ones reconciled to [`TaskStatus::Lost`]); settled records
    /// past retention are pruned. Records of live processes are left alone.
    /// Returns how many records were reconciled to lost.
    pub fn load(&self) -> usize {
        let Some(dir) = self.store.clone() else {
            return 0;
        };
        let Ok(read) = std::fs::read_dir(&dir) else {
            return 0;
        };
        let now = now_ms();
        let retention = u64::try_from(RETENTION.as_millis()).unwrap_or(u64::MAX);
        let mut lost = 0;
        let mut entries = self.lock();
        for item in read.flatten() {
            let path = item.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut record) = serde_json::from_str::<TaskRecord>(&text) else {
                continue;
            };
            if entries.contains_key(&record.task_id)
                || record.owner_pid == self.owner_pid
                || process_alive(record.owner_pid)
            {
                continue;
            }
            if record.status.is_terminal() {
                if now.saturating_sub(record.updated_at_ms) > retention {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            } else {
                record.status = TaskStatus::Lost;
                record.error_message = Some(LOST_EXPLANATION.to_string());
                record.failure_kind = Some("lost".to_string());
                record.ended_at_ms = Some(now);
                record.updated_at_ms = now;
                write_logged(&dir, &record);
                lost += 1;
            }
            entries.insert(
                record.task_id.clone(),
                Entry {
                    record,
                    control: None,
                },
            );
        }
        drop(entries);
        if lost > 0 {
            self.bump();
        }
        lost
    }

    /// Start a background task and return its view right away.
    ///
    /// # Errors
    /// The name is already taken in the parent session.
    pub fn spawn(self: &Arc<Self>, request: SpawnRequest) -> Result<TaskView, String> {
        let SpawnRequest {
            mut spec,
            name,
            description,
            parent_session_id,
            root_session_id,
            env,
        } = request;
        let now = now_ms();
        let mut entries = self.lock();
        if let Some(name) = name.as_deref() {
            let taken = entries.values().any(|e| {
                e.record.parent_session_id == parent_session_id
                    && e.record.name.as_deref() == Some(name)
            });
            if taken {
                return Err(format!(
                    "A task named `{name}` already exists in this session; choose another name or message it with task_send."
                ));
            }
        }
        let task_id = loop {
            let id = new_task_id();
            if !entries.contains_key(&id) {
                break id;
            }
        };
        spec.id = Some(task_id.clone());
        let record = TaskRecord {
            task_id: task_id.clone(),
            name,
            agent: spec.agent.clone(),
            description,
            parent_session_id: parent_session_id.clone(),
            root_session_id,
            status: TaskStatus::Pending,
            run_epoch: 1,
            notified_epoch: 0,
            dismissed: false,
            created_at_ms: now,
            updated_at_ms: now,
            ended_at_ms: None,
            duration_ms: 0,
            cwd: spec.cwd.display().to_string(),
            session_file: None,
            final_response: None,
            truncated: false,
            error_message: None,
            failure_kind: None,
            cancel_reason: None,
            output_tokens: 0,
            run_stats: None,
            owner_pid: self.owner_pid,
            spawn: SpawnRecord {
                model: spec.model.clone(),
                tools: spec.tools.clone(),
                depth: spec.depth,
                extra_args: spec.extra_args.clone(),
                env: env.clone(),
            },
        };
        let opts = StartOptions {
            resume_session: None,
            report_session: true,
            env,
        };
        let (control, join) = self.runner.start_with(spec, ChildMode::Rpc, opts);
        self.write(&record);
        let entry = Entry {
            record,
            control: Some(control.clone()),
        };
        let view = view_of(&entry);
        entries.insert(task_id.clone(), entry);
        drop(entries);
        self.watch(task_id, 1, control, join);
        self.changed(&parent_session_id);
        Ok(view)
    }

    /// Resolve `target` (a task id, or a name within the caller's session)
    /// for `caller` (`None`: every task is visible). Another session's id is
    /// [`Lookup::ScopeDenied`] unless `all_scope`.
    #[must_use]
    pub fn lookup(&self, target: &str, caller: Option<&str>, all_scope: bool) -> Lookup {
        let entries = self.lock();
        let owned = |record: &TaskRecord| {
            caller.is_none_or(|c| record.parent_session_id == c || record.root_session_id == c)
        };
        if let Some(entry) = entries.get(target) {
            let view = view_of(entry);
            return if all_scope || owned(&entry.record) {
                Lookup::Found(view)
            } else {
                Lookup::ScopeDenied(view)
            };
        }
        let by_name = entries
            .values()
            .filter(|e| owned(&e.record) && e.record.name.as_deref() == Some(target))
            .max_by_key(|e| e.record.created_at_ms);
        match by_name {
            Some(entry) => Lookup::Found(view_of(entry)),
            None => Lookup::NotFound {
                known: known_tasks(&entries, caller),
            },
        }
    }

    /// The caller's newest task names (ids for unnamed tasks), oldest first.
    #[must_use]
    pub fn known_tasks(&self, caller: Option<&str>) -> Vec<String> {
        known_tasks(&self.lock(), caller)
    }

    #[must_use]
    pub fn get(&self, task_id: &str) -> Option<TaskView> {
        self.lock().get(task_id).map(view_of)
    }

    /// Tasks spawned by `session` (`None`: all), oldest first.
    #[must_use]
    pub fn list(&self, session: Option<&str>) -> Vec<TaskView> {
        let entries = self.lock();
        let mut views: Vec<TaskView> = entries
            .values()
            .filter(|e| session.is_none_or(|s| e.record.parent_session_id == s))
            .map(view_of)
            .collect();
        views.sort_by_key(|v| v.record.created_at_ms);
        views
    }

    /// Whether `session` ever spawned a background task (in this process or
    /// a dead one whose records were loaded).
    #[must_use]
    pub fn has_children(&self, session: &str) -> bool {
        self.lock()
            .values()
            .any(|e| e.record.parent_session_id == session)
    }

    /// Children `session` still waits on (see [`TaskView::is_open`]).
    #[must_use]
    pub fn open_children(&self, session: &str) -> usize {
        self.lock()
            .values()
            .filter(|e| e.record.parent_session_id == session)
            .filter(|e| view_of(e).is_open())
            .count()
    }

    /// Settled runs of `session`'s children whose completion is undelivered.
    #[must_use]
    pub fn pending_notifications(&self, session: &str) -> Vec<TaskView> {
        let mut views: Vec<TaskView> = self
            .lock()
            .values()
            .filter(|e| e.record.parent_session_id == session)
            .map(view_of)
            .filter(TaskView::notification_pending)
            .collect();
        views.sort_by_key(|v| v.record.ended_at_ms);
        views
    }

    /// Claim the delivery of `task_id`'s epoch-`epoch` completion; `false`
    /// when it is already delivered (or claimed) or the run moved on. A
    /// failed delivery gives the claim back with [`Self::abort_notify`].
    pub fn begin_notify(&self, task_id: &str, epoch: u64) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(task_id) else {
            return false;
        };
        if entry.control.is_some()
            || entry.record.run_epoch != epoch
            || entry.record.notified_epoch >= epoch
        {
            return false;
        }
        entry.record.notified_epoch = epoch;
        self.write(&entry.record);
        let parent = entry.record.parent_session_id.clone();
        drop(entries);
        self.changed(&parent);
        true
    }

    /// Give back a [`Self::begin_notify`] claim whose delivery failed.
    pub fn abort_notify(&self, task_id: &str, epoch: u64) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(task_id) else {
            return;
        };
        if entry.record.notified_epoch != epoch {
            return;
        }
        entry.record.notified_epoch = epoch.saturating_sub(1);
        self.write(&entry.record);
        let parent = entry.record.parent_session_id.clone();
        drop(entries);
        self.changed(&parent);
    }

    /// Deliver `message`: steer a running task, revive a completed one.
    pub async fn send(
        self: &Arc<Self>,
        target: &str,
        message: &str,
        caller: Option<&str>,
        all_scope: bool,
    ) -> SendOutcome {
        let view = match self.lookup(target, caller, all_scope) {
            Lookup::Found(view) => view,
            Lookup::NotFound { known } => return SendOutcome::NotFound { known },
            Lookup::ScopeDenied(view) => return SendOutcome::ScopeDenied(view),
        };
        match view.continuation() {
            Continuation::Steer => {
                let control = self.lock().get(view.id()).and_then(|e| e.control.clone());
                let Some(control) = control else {
                    return self.not_settled_yet(view);
                };
                match control.steer(message).await {
                    Ok(()) => SendOutcome::Steered(self.get(view.id()).unwrap_or(view)),
                    Err(SteerError::NotRunning) => self.not_settled_yet(view),
                    Err(SteerError::Unsupported) => SendOutcome::NotContinuable {
                        reason: format!("{} has no input channel.", view.id()),
                        view,
                    },
                }
            }
            Continuation::Revive => match self.revive(view.id(), message) {
                Ok((view, run_epoch)) => SendOutcome::Revived { view, run_epoch },
                Err(reason) => SendOutcome::NotContinuable { view, reason },
            },
            Continuation::NotContinuable(stop) => SendOutcome::NotContinuable {
                reason: stop.explain(view.id()),
                view,
            },
        }
    }

    fn not_settled_yet(&self, view: TaskView) -> SendOutcome {
        SendOutcome::NotContinuable {
            reason: format!(
                "{} finished its run before the message went out; its completion notification is on the way. Send again after it arrives to revive the task.",
                view.id()
            ),
            view,
        }
    }

    fn revive(self: &Arc<Self>, task_id: &str, message: &str) -> Result<(TaskView, u64), String> {
        let mut entries = self.lock();
        let entry = entries
            .get_mut(task_id)
            .ok_or_else(|| format!("{task_id} is no longer known."))?;
        if entry.control.is_some() || entry.record.status != TaskStatus::Completed {
            return Err(format!(
                "{task_id} is {} and cannot be revived now.",
                view_of(entry).status().as_str()
            ));
        }
        let session = entry
            .record
            .session_file
            .clone()
            .ok_or_else(|| Stop::NoSession.explain(task_id))?;
        if !Path::new(&session).is_file() {
            return Err(format!(
                "{task_id} cannot be revived: its session file {session} is gone."
            ));
        }
        let record = &mut entry.record;
        let epoch = record.run_epoch + 1;
        let spec = TaskSpec {
            id: Some(task_id.to_string()),
            agent: record.agent.clone(),
            prompt: message.to_string(),
            cwd: PathBuf::from(&record.cwd),
            output_schema: None,
            schema_mode: SchemaMode::Permissive,
            model: record.spawn.model.clone(),
            tools: record.spawn.tools.clone(),
            depth: record.spawn.depth,
            extra_args: record.spawn.extra_args.clone(),
        };
        let opts = StartOptions {
            resume_session: Some(PathBuf::from(&session)),
            report_session: false,
            env: record.spawn.env.clone(),
        };
        let (control, join) = self.runner.start_with(spec, ChildMode::Rpc, opts);
        record.run_epoch = epoch;
        record.status = TaskStatus::Pending;
        record.dismissed = false;
        record.final_response = None;
        record.truncated = false;
        record.error_message = None;
        record.failure_kind = None;
        record.ended_at_ms = None;
        record.updated_at_ms = now_ms();
        let parent = record.parent_session_id.clone();
        entry.control = Some(control.clone());
        self.write(&entry.record);
        let view = view_of(entry);
        drop(entries);
        self.watch(task_id.to_string(), epoch, control, join);
        self.changed(&parent);
        Ok((view, epoch))
    }

    /// Cancel a running task (terminal, never revived). A settled task is
    /// left as it is, but an open one (dead or undelivered) is released so
    /// it stops counting as an open child.
    pub fn cancel(
        &self,
        target: &str,
        reason: Option<&str>,
        caller: Option<&str>,
        all_scope: bool,
    ) -> CancelOutcome {
        let view = match self.lookup(target, caller, all_scope) {
            Lookup::Found(view) => view,
            Lookup::NotFound { known } => return CancelOutcome::NotFound { known },
            Lookup::ScopeDenied(view) => return CancelOutcome::ScopeDenied(view),
        };
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(view.id()) else {
            return CancelOutcome::NotFound { known: Vec::new() };
        };
        if let Some(control) = entry.control.clone() {
            let previous = view_of(entry).status();
            if previous == TaskStatus::Cancelled || !control.cancel() {
                // Already cancelled, or the run finished first and is settling.
                return CancelOutcome::Noop {
                    view: view_of(entry),
                    released: false,
                };
            }
            entry.record.status = TaskStatus::Cancelled;
            entry.record.cancel_reason = Some(
                reason
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("cancelled by the parent")
                    .to_string(),
            );
            entry.record.updated_at_ms = now_ms();
            self.write(&entry.record);
            let view = view_of(entry);
            drop(entries);
            self.changed(&view.record.parent_session_id);
            return CancelOutcome::Cancelled { view, previous };
        }
        let released = view_of(entry).is_open();
        if released {
            entry.record.dismissed = true;
            entry.record.updated_at_ms = now_ms();
            self.write(&entry.record);
        }
        let view = view_of(entry);
        drop(entries);
        if released {
            self.changed(&view.record.parent_session_id);
        }
        CancelOutcome::Noop { view, released }
    }

    fn watch(self: &Arc<Self>, task_id: String, epoch: u64, control: RunControl, join: RunJoin) {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let out = join.await;
            registry.settle(&task_id, epoch, &control, &out);
        });
    }

    fn settle(&self, task_id: &str, epoch: u64, control: &RunControl, out: &RunOutput) {
        let now = now_ms();
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(task_id) else {
            return;
        };
        if entry.record.run_epoch != epoch {
            return;
        }
        entry.control = None;
        let record = &mut entry.record;
        record.status = if record.cancel_reason.is_some() {
            TaskStatus::Cancelled
        } else {
            match out.status {
                RunStatus::Completed => TaskStatus::Completed,
                RunStatus::Failed => TaskStatus::Error,
                RunStatus::Cancelled => TaskStatus::Cancelled,
            }
        };
        record.final_response = (!out.output.trim().is_empty()).then(|| out.output.clone());
        record.truncated = out.truncated;
        if record.status == TaskStatus::Error {
            record.error_message = Some(error_summary(&out.stderr));
            record.failure_kind = Some(
                if out.exit_code.is_none() && out.stderr.starts_with("failed to spawn") {
                    "spawn_failed"
                } else {
                    "child_error"
                }
                .to_string(),
            );
        }
        if control.output_tokens() > 0 {
            record.output_tokens = control.output_tokens();
        }
        if let Some(file) = control.session_file() {
            record.session_file = Some(file);
        }
        if let Some(stats) = control.session_stats() {
            record.run_stats = Some(RunStats::from_session_stats(&stats));
        }
        record.duration_ms = u64::try_from(out.duration.as_millis()).unwrap_or(u64::MAX);
        record.ended_at_ms = Some(now);
        record.updated_at_ms = now;
        self.write(&entry.record);
        let view = view_of(entry);
        drop(entries);
        self.emit(&RegistryEvent::Settled(Box::new(view)));
        self.bump();
    }

    fn write(&self, record: &TaskRecord) {
        if let Some(dir) = &self.store {
            write_logged(dir, record);
        }
    }

    fn changed(&self, parent_session_id: &str) {
        self.emit(&RegistryEvent::Changed {
            parent_session_id: parent_session_id.to_string(),
        });
        self.bump();
    }

    fn emit(&self, event: &RegistryEvent) {
        let listener = self
            .listener
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(listener) = listener {
            listener(event);
        }
    }

    fn bump(&self) {
        self.changes.send_modify(|generation| *generation += 1);
    }
}

fn known_tasks(entries: &BTreeMap<String, Entry>, caller: Option<&str>) -> Vec<String> {
    let mut mine: Vec<&Entry> = entries
        .values()
        .filter(|e| {
            caller.is_none_or(|c| e.record.parent_session_id == c || e.record.root_session_id == c)
        })
        .collect();
    mine.sort_by_key(|e| e.record.created_at_ms);
    let skip = mine.len().saturating_sub(KNOWN_TASKS_LISTED);
    mine[skip..]
        .iter()
        .map(|e| {
            e.record
                .name
                .clone()
                .unwrap_or_else(|| e.record.task_id.clone())
        })
        .collect()
}

fn view_of(entry: &Entry) -> TaskView {
    let mut record = entry.record.clone();
    if let Some(control) = &entry.control {
        if !record.status.is_terminal() {
            record.status = match control.phase() {
                RunPhase::Queued => TaskStatus::Pending,
                _ => TaskStatus::Running,
            };
        }
        if record.session_file.is_none() {
            record.session_file = control.session_file();
        }
        record.output_tokens = record.output_tokens.max(control.output_tokens());
    }
    TaskView {
        record,
        live: entry.control.is_some(),
    }
}

/// A fresh `st_<12 lowercase hex>` id.
#[must_use]
pub fn new_task_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    // A freshly keyed SipHash over time, sequence and pid: unpredictable
    // across processes, distinct within one.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.write_u64(SEQUENCE.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    format!(
        "{TASK_ID_PREFIX}{:012x}",
        hasher.finish() & 0xffff_ffff_ffff
    )
}

/// Whether `value` has the background task id shape (`st_<12 lowercase hex>`).
#[must_use]
pub fn is_task_id(value: &str) -> bool {
    value.strip_prefix(TASK_ID_PREFIX).is_some_and(|hex| {
        hex.len() == TASK_ID_HEX_DIGITS
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn error_summary(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        return "the child run failed without an error message".to_string();
    }
    match text.char_indices().nth(ERROR_MESSAGE_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn write_logged(dir: &Path, record: &TaskRecord) {
    if let Err(e) = write_record(dir, record) {
        tracing::warn!(task = %record.task_id, "background task record not saved: {e}");
    }
}

/// Atomic write: a temp file renamed over `<task_id>.json`.
fn write_record(dir: &Path, record: &TaskRecord) -> std::io::Result<()> {
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
    let temp = dir.join(format!(
        ".{}.{}.{}.tmp",
        record.task_id,
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, dir.join(format!("{}.json", record.task_id)))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    if raw <= 0 {
        return false;
    }
    // Signal 0 probes existence; EPERM still means the process exists.
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// Fake `--mode rpc` child. It answers `get_state`/`switch_session`,
    /// appends user/assistant messages to its session file, and settles each
    /// prompt as `answer:<msg>`; a prompt containing `hold` settles only
    /// after a steer (`steered:<steer>`), one containing `fail` settles as an
    /// error.
    const FAKE_RPC: &str = r#"
[ "$1" = "--mode" ] && [ "$2" = "rpc" ] || { echo "bad args: $*" >&2; exit 2; }
dir=$(dirname "$0")
sess="$dir/session-$$.jsonl"
answer=""
holding=""
field() { printf '%s\n' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }
note() { printf '{"type":"message","message":{"role":"%s","content":[{"type":"text","text":"%s"}]}}\n' "$1" "$2" >> "$sess"; }
settle() {
  note assistant "$answer"
  if [ "$1" = error ]; then
    echo '{"type":"prompt_result","status":"error","error":{"message":"boom"},"sessionSettled":true}'
  else
    echo '{"type":"prompt_result","status":"completed","sessionSettled":true}'
  fi
}
while IFS= read -r line; do
  case "$line" in
    *'"type":"get_state"'*)
      printf '{"type":"response","command":"get_state","success":true,"data":{"sessionFile":"%s"}}\n' "$sess" ;;
    *'"type":"switch_session"'*)
      sess=$(field "$line" sessionPath)
      echo '{"type":"response","command":"switch_session","success":true,"data":{"cancelled":false}}' ;;
    *'"type":"prompt"'*)
      msg=$(field "$line" message)
      note user "$msg"
      echo '{"type":"response","command":"prompt","success":true,"id":"omp-task-prompt"}'
      case "$msg" in
        *hold*) holding=1 ;;
        *fail*) answer="failed:$msg"; settle error ;;
        *) answer="answer:$msg"; settle completed ;;
      esac ;;
    *'"type":"steer"'*)
      msg=$(field "$line" message)
      note user "$msg"
      if [ -n "$holding" ]; then holding=""; answer="steered:$msg"; settle completed; fi ;;
    *'"type":"get_last_assistant_text"'*)
      printf '{"type":"response","command":"get_last_assistant_text","success":true,"data":{"text":"%s"}}\n' "$answer" ;;
    *'"type":"get_session_stats"'*)
      printf '{"type":"response","command":"get_session_stats","success":true,"data":{"tokens":{"input":11,"output":7,"cacheRead":3,"cacheWrite":2},"cost":0.25,"toolCalls":1,"sessionFile":"%s"}}\n' "$sess" ;;
  esac
done
"#;

    fn fake_binary(dir: &Path) -> PathBuf {
        let path = dir.join("fake-omp-rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{FAKE_RPC}").unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn registry(dir: &Path, slots: usize) -> Arc<SubagentRegistry> {
        SubagentRegistry::new(
            TaskRunner::new(fake_binary(dir), slots),
            Some(dir.join("tasks")),
        )
    }

    fn request(dir: &Path, prompt: &str, name: Option<&str>, parent: &str) -> SpawnRequest {
        SpawnRequest {
            spec: TaskSpec {
                id: None,
                agent: "task".to_string(),
                prompt: prompt.to_string(),
                cwd: dir.to_path_buf(),
                output_schema: None,
                schema_mode: SchemaMode::Permissive,
                model: None,
                tools: None,
                depth: 1,
                extra_args: Vec::new(),
            },
            name: name.map(str::to_string),
            description: None,
            parent_session_id: parent.to_string(),
            root_session_id: parent.to_string(),
            env: Vec::new(),
        }
    }

    /// Wait (bounded) until `cond` holds; `rx` was subscribed before the
    /// action that should make it true.
    async fn wait_until(
        reg: &SubagentRegistry,
        rx: &mut watch::Receiver<u64>,
        cond: impl Fn(&SubagentRegistry) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !cond(reg) {
                rx.changed().await.expect("registry alive");
            }
        })
        .await
        .expect("condition reached within 30s");
    }

    fn settled(reg: &SubagentRegistry, id: &str, epoch: u64) -> bool {
        reg.get(id)
            .is_some_and(|v| !v.live && v.record.run_epoch == epoch && v.status().is_terminal())
    }

    #[test]
    fn task_ids_are_st_plus_twelve_hex() {
        let id = new_task_id();
        assert!(is_task_id(&id), "{id}");
        assert_ne!(new_task_id(), id);
        for bad in [
            "st_",
            "st_12345678",
            "st_ABCDEF012345",
            "xx_0123456789ab",
            "st_0123456789abc",
        ] {
            assert!(!is_task_id(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn background_run_completes_with_record_and_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path(), 4);
        let mut rx = reg.subscribe();
        let view = reg
            .spawn(request(dir.path(), "count", Some("Counter"), "parent"))
            .unwrap();
        assert!(is_task_id(view.id()));
        assert!(view.live && view.is_open());
        let id = view.id().to_string();
        wait_until(&reg, &mut rx, |r| settled(r, &id, 1)).await;

        let done = reg.get(&id).unwrap();
        assert_eq!(done.status(), TaskStatus::Completed);
        assert_eq!(done.record.final_response.as_deref(), Some("answer:count"));
        assert_eq!(done.record.output_tokens, 7);
        let stats = done.record.run_stats.clone().unwrap();
        assert_eq!((stats.input_tokens, stats.cache_read_tokens), (11, 3));
        assert!((stats.cost_usd - 0.25).abs() < f64::EPSILON);
        assert!(
            done.is_open(),
            "an undelivered completion keeps the child open"
        );
        assert!(done.notification_pending());
        assert_eq!(reg.pending_notifications("parent").len(), 1);

        let transcript = done.transcript(TranscriptMode::Full).unwrap().unwrap();
        assert_eq!(transcript.text, "user: count\nassistant: answer:count");

        let saved: TaskRecord = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("tasks").join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.status, TaskStatus::Completed);
        assert_eq!(saved.parent_session_id, "parent");
        assert_eq!(saved.name.as_deref(), Some("Counter"));

        assert!(reg.begin_notify(&id, 1));
        assert!(!reg.begin_notify(&id, 1), "a delivery is claimed once");
        assert!(!reg.get(&id).unwrap().is_open());
        assert_eq!(reg.open_children("parent"), 0);
        reg.abort_notify(&id, 1);
        assert_eq!(
            reg.open_children("parent"),
            1,
            "a failed delivery is pending again"
        );
    }

    #[tokio::test]
    async fn steer_reaches_a_running_child() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path(), 4);
        let mut rx = reg.subscribe();
        let id = reg
            .spawn(request(dir.path(), "hold on", None, "parent"))
            .unwrap()
            .id()
            .to_string();
        match reg.send(&id, "redirect", Some("parent"), false).await {
            SendOutcome::Steered(view) => assert_eq!(view.id(), id),
            other => panic!("expected a steer, got {other:?}"),
        }
        wait_until(&reg, &mut rx, |r| settled(r, &id, 1)).await;
        assert_eq!(
            reg.get(&id).unwrap().record.final_response.as_deref(),
            Some("steered:redirect")
        );
    }

    #[tokio::test]
    async fn message_to_a_completed_task_revives_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path(), 4);
        let mut rx = reg.subscribe();
        let id = reg
            .spawn(request(dir.path(), "first", Some("Worker"), "parent"))
            .unwrap()
            .id()
            .to_string();
        wait_until(&reg, &mut rx, |r| settled(r, &id, 1)).await;
        let session = reg.get(&id).unwrap().record.session_file.unwrap();

        let outcome = reg.send("Worker", "again", Some("parent"), false).await;
        let SendOutcome::Revived { run_epoch, view } = outcome else {
            panic!("expected a revival, got {outcome:?}");
        };
        assert_eq!(run_epoch, 2);
        assert!(view.live);
        wait_until(&reg, &mut rx, |r| settled(r, &id, 2)).await;
        let done = reg.get(&id).unwrap();
        assert_eq!(done.status(), TaskStatus::Completed);
        assert_eq!(done.record.final_response.as_deref(), Some("answer:again"));
        assert_eq!(done.record.session_file.as_deref(), Some(session.as_str()));
        let transcript = done.transcript(TranscriptMode::Full).unwrap().unwrap();
        assert_eq!(
            transcript.text,
            "user: first\nassistant: answer:first\nuser: again\nassistant: answer:again"
        );
    }

    #[tokio::test]
    async fn cancel_is_terminal_for_running_and_queued_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path(), 1);
        let mut rx = reg.subscribe();
        let running = reg
            .spawn(request(dir.path(), "hold A", None, "parent"))
            .unwrap()
            .id()
            .to_string();
        let queued = reg
            .spawn(request(dir.path(), "hold B", None, "parent"))
            .unwrap()
            .id()
            .to_string();
        assert_eq!(reg.get(&queued).unwrap().status(), TaskStatus::Pending);

        let CancelOutcome::Cancelled { previous, view } =
            reg.cancel(&queued, Some("not needed"), Some("parent"), false)
        else {
            panic!("queued task cancels");
        };
        assert_eq!(previous, TaskStatus::Pending);
        assert_eq!(view.status(), TaskStatus::Cancelled);
        let CancelOutcome::Cancelled { .. } = reg.cancel(&running, None, Some("parent"), false)
        else {
            panic!("running task cancels");
        };
        wait_until(&reg, &mut rx, |r| {
            settled(r, &running, 1) && settled(r, &queued, 1)
        })
        .await;
        let queued_view = reg.get(&queued).unwrap();
        assert_eq!(queued_view.status(), TaskStatus::Cancelled);
        assert_eq!(
            queued_view.record.session_file, None,
            "a queued cancel never spawns"
        );
        assert_eq!(
            queued_view.record.cancel_reason.as_deref(),
            Some("not needed")
        );
        assert_eq!(reg.get(&running).unwrap().status(), TaskStatus::Cancelled);
        assert_eq!(reg.open_children("parent"), 0);
        assert!(
            reg.pending_notifications("parent").is_empty(),
            "a cancel notifies nobody"
        );

        let CancelOutcome::Noop { released, .. } =
            reg.cancel(&running, None, Some("parent"), false)
        else {
            panic!("cancelling a cancelled task is a no-op");
        };
        assert!(!released);
        let SendOutcome::NotContinuable { reason, .. } =
            reg.send(&running, "more", Some("parent"), false).await
        else {
            panic!("a cancelled task takes no messages");
        };
        assert!(reason.contains("never revived"), "{reason}");
    }

    #[tokio::test]
    async fn a_failed_child_stays_open_until_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path(), 4);
        let mut rx = reg.subscribe();
        let id = reg
            .spawn(request(dir.path(), "fail now", None, "parent"))
            .unwrap()
            .id()
            .to_string();
        wait_until(&reg, &mut rx, |r| settled(r, &id, 1)).await;
        let view = reg.get(&id).unwrap();
        assert_eq!(view.status(), TaskStatus::Error);
        assert_eq!(view.record.failure_kind.as_deref(), Some("child_error"));
        assert!(view
            .record
            .error_message
            .as_deref()
            .unwrap()
            .contains("boom"));
        assert!(reg.begin_notify(&id, 1));
        assert_eq!(
            reg.open_children("parent"),
            1,
            "a dead child blocks the parent"
        );
        let SendOutcome::NotContinuable { .. } =
            reg.send(&id, "retry", Some("parent"), false).await
        else {
            panic!("an errored task is not revived");
        };
        let CancelOutcome::Noop { released, view } = reg.cancel(&id, None, Some("parent"), false)
        else {
            panic!("cancel of a settled task is a no-op");
        };
        assert!(released);
        assert_eq!(view.status(), TaskStatus::Error, "the result is kept");
        assert_eq!(reg.open_children("parent"), 0);
    }

    #[tokio::test]
    async fn lookups_are_scoped_to_the_caller_session() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path(), 4);
        let mut rx = reg.subscribe();
        let id = reg
            .spawn(request(dir.path(), "x", Some("Mine"), "a"))
            .unwrap()
            .id()
            .to_string();
        assert!(reg
            .spawn(request(dir.path(), "y", Some("Mine"), "a"))
            .unwrap_err()
            .contains("already exists"));
        assert!(matches!(reg.lookup("Mine", Some("a"), false), Lookup::Found(v) if v.id() == id));
        assert!(matches!(
            reg.lookup(&id, Some("b"), false),
            Lookup::ScopeDenied(_)
        ));
        assert!(matches!(reg.lookup(&id, Some("b"), true), Lookup::Found(_)));
        assert!(matches!(reg.lookup(&id, None, false), Lookup::Found(_)));
        match reg.lookup("Other", Some("a"), false) {
            Lookup::NotFound { known } => assert_eq!(known, ["Mine"]),
            other => panic!("{other:?}"),
        }
        match reg.lookup("Mine", Some("b"), false) {
            Lookup::NotFound { known } => assert!(known.is_empty()),
            other => panic!("{other:?}"),
        }
        wait_until(&reg, &mut rx, |r| settled(r, &id, 1)).await;
    }

    #[test]
    fn load_reconciles_dead_owners_to_lost_and_skips_live_ones() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("tasks");
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let record = |id: &str, pid: u32, status: TaskStatus| TaskRecord {
            task_id: id.to_string(),
            name: None,
            agent: "task".to_string(),
            description: None,
            parent_session_id: "parent".to_string(),
            root_session_id: "parent".to_string(),
            status,
            run_epoch: 1,
            notified_epoch: 0,
            dismissed: false,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            ended_at_ms: None,
            duration_ms: 0,
            cwd: "/".to_string(),
            session_file: None,
            final_response: None,
            truncated: false,
            error_message: None,
            failure_kind: None,
            cancel_reason: None,
            output_tokens: 0,
            run_stats: None,
            owner_pid: pid,
            spawn: SpawnRecord::default(),
        };
        write_record(
            &store,
            &record("st_00000000dead", dead_pid, TaskStatus::Running),
        )
        .unwrap();
        let mut old = record("st_000000000old", dead_pid, TaskStatus::Completed);
        old.updated_at_ms = 0;
        write_record(&store, &old).unwrap();
        // pid 1 is always alive: a live owner's record is not ours to touch.
        write_record(&store, &record("st_000000000ive", 1, TaskStatus::Running)).unwrap();

        let reg = SubagentRegistry::new(TaskRunner::new("/nonexistent", 1), Some(store.clone()));
        assert_eq!(reg.load(), 1);
        let lost = reg.get("st_00000000dead").unwrap();
        assert_eq!(lost.status(), TaskStatus::Lost);
        assert_eq!(lost.record.error_message.as_deref(), Some(LOST_EXPLANATION));
        assert!(
            lost.is_open(),
            "a lost child blocks its parent until released"
        );
        assert_eq!(
            lost.continuation(),
            Continuation::NotContinuable(Stop::Lost)
        );
        assert!(reg.get("st_000000000ive").is_none());
        assert!(reg.get("st_000000000old").is_none());
        assert!(
            !store.join("st_000000000old.json").exists(),
            "expired record pruned"
        );
        let saved: TaskRecord = serde_json::from_str(
            &std::fs::read_to_string(store.join("st_00000000dead.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.status, TaskStatus::Lost);

        let CancelOutcome::Noop { released, .. } =
            reg.cancel("st_00000000dead", None, Some("parent"), false)
        else {
            panic!("a lost task is released, not cancelled");
        };
        assert!(released);
        assert_eq!(reg.open_children("parent"), 0);
    }
}
