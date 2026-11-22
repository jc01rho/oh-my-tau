//! `monitor`: watchers that wake the agent instead of being polled.
//!
//! A `command` watch runs the command as a background job (so `bash_output` and `kill_bash` see it
//! and its process group is killed the usual way) and turns each output line matching `filter`
//! into an event. A `path` watch uses the OS file notifier and fires on `create` or `modify`.
//! Events are collapsed in a short window (identical updates become one line with a count) and
//! injected as one `<system-reminder>` through the session's [`WakeSink`], which starts a turn on
//! an idle session. A watch ends when its command exits, its `timeout_ms` passes (the command is
//! killed), it is killed, or, for a one-shot path watch, after its first event; the ending is
//! reported with a summary. A command watch that wakes the session more often than
//! `wakes_per_hour` is muted until `monitor {action: "rearm"}`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use pa_agent::abort::AbortController;
use pa_agent::types::AgentToolResult;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::time::Instant;

use super::bash::{self, Notify, WatchMsg};
use super::jobs::{JobRegistry, JobStatus};
use super::Env;

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const MAX_WAKES_PER_HOUR: u64 = 3_600;
const DEFAULT_WAKES_PER_HOUR: usize = 120;
/// Events of one window are collected for this long before they wake the session.
const BATCH_WINDOW: Duration = Duration::from_millis(200);
/// Distinct event lines one wake carries; further events are only counted.
const MAX_BATCH_LINES: usize = 50;
const HOUR: Duration = Duration::from_secs(3600);

const DESCRIPTION: &str = "Watch a command's output or a file and be woken when it happens; never poll. `command` + optional `filter` (regex per line) wakes you with each matching line and a summary when the command exits; `timeout_ms` (default 300000, max 3600000) kills it, `persistent: true` has no deadline. `path` + `event` (`create` default, or `modify`) wakes you once. `description` is required. `kill_bash` with the monitor id stops a watch; `action: \"rearm\"` resumes watches muted for waking too often.";

#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["create", "rearm"], "description": "`create` (default) starts a watch; `rearm` resumes muted command watches (`bash_id` limits it to one)."},
            "type": {"type": "string", "enum": ["command", "path"], "description": "Optional; inferred from `command` or `path`."},
            "description": {"type": "string", "description": "What is being watched; quoted in every wake."},
            "command": {"type": "string", "description": "Shell command whose output lines are the events."},
            "path": {"type": "string", "description": "File to watch (its parent directory must exist)."},
            "event": {"type": "string", "enum": ["create", "modify"], "description": "Path watches: `create` fires when the path appears, `modify` when it changes."},
            "filter": {"type": "string", "description": "Command watches: regex; only matching lines are events."},
            "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "description": "Deadline; the watched command is killed when it passes. Ignored when persistent."},
            "persistent": {"type": "boolean", "description": "No deadline; a path watch keeps firing."},
            "wakes_per_hour": {"type": "integer", "minimum": 1, "maximum": MAX_WAKES_PER_HOUR, "description": "Command watches: wake budget per hour before the watch is muted (default 120)."},
            "bash_id": {"type": "string", "description": "`rearm` only: monitor id to resume."},
        },
        "additionalProperties": false,
    })
}

#[must_use]
pub fn description() -> &'static str {
    DESCRIPTION
}

/// Where a session's wakes go. The runtime binds one; without it events are dropped.
pub trait WakeSink: Send + Sync {
    /// Inject `text` (a `<system-reminder>` block) into the session's next turn.
    fn deliver(&self, text: String);
    /// The number of running watches changed.
    fn active(&self, count: usize);
}

struct Entry {
    bash_id: Option<String>,
    cancel: Arc<AbortController>,
    budget: Option<Arc<Budget>>,
    running: bool,
}

#[derive(Default)]
struct HubState {
    next_id: usize,
    items: HashMap<String, Entry>,
}

impl HubState {
    fn running(&self) -> usize {
        self.items.values().filter(|e| e.running).count()
    }
}

/// The session's monitors. Lives inside the [`JobRegistry`], so it survives tool-list rebuilds.
#[derive(Default)]
pub struct Hub {
    state: Mutex<HubState>,
    sink: Mutex<Option<Arc<dyn WakeSink>>>,
}

impl Hub {
    pub fn bind(&self, sink: Arc<dyn WakeSink>) {
        *self.sink.lock() = Some(sink);
    }

    fn deliver(&self, text: String) {
        let sink = self.sink.lock().clone();
        if let Some(sink) = sink {
            sink.deliver(text);
        }
    }

    fn announce(&self, count: usize) {
        let sink = self.sink.lock().clone();
        if let Some(sink) = sink {
            sink.active(count);
        }
    }

    fn register(&self, entry: Entry) -> String {
        let (id, running) = {
            let mut st = self.state.lock();
            st.next_id += 1;
            let id = format!("mon_{}", st.next_id);
            st.items.insert(id.clone(), entry);
            (id, st.running())
        };
        self.announce(running);
        id
    }

    fn finish(&self, id: &str) {
        let running = {
            let mut st = self.state.lock();
            if let Some(entry) = st.items.get_mut(id) {
                entry.running = false;
            }
            st.running()
        };
        self.announce(running);
    }

    /// Number of running watches.
    #[cfg(test)]
    #[must_use]
    pub fn running(&self) -> usize {
        self.state.lock().running()
    }

    /// Stop one watch by monitor id; `None` when the id is unknown, else whether it was running.
    #[must_use]
    pub fn kill(&self, id: &str) -> Option<bool> {
        let (cancel, running) = {
            let st = self.state.lock();
            let entry = st.items.get(id)?;
            (entry.cancel.clone(), entry.running)
        };
        cancel.abort();
        Some(running)
    }

    /// Stop every running path watch (command watches are jobs, stopped with them); returns how
    /// many were running.
    #[must_use]
    pub fn kill_path_watches(&self) -> usize {
        let cancels: Vec<Arc<AbortController>> = {
            let st = self.state.lock();
            st.items
                .values()
                .filter(|e| e.running && e.bash_id.is_none())
                .map(|e| e.cancel.clone())
                .collect()
        };
        for cancel in &cancels {
            cancel.abort();
        }
        cancels.len()
    }

    /// Resume muted watches (all, or the one whose monitor or job id is `target`); returns their ids.
    fn rearm(&self, target: Option<&str>) -> Vec<String> {
        let st = self.state.lock();
        let mut ids: Vec<String> = st
            .items
            .iter()
            .filter(|(id, e)| {
                e.running
                    && target.is_none_or(|t| t == id.as_str() || e.bash_id.as_deref() == Some(t))
                    && e.budget.as_ref().is_some_and(|b| b.rearm())
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }
}

enum Admit {
    Yes,
    /// The wake that exceeded the budget: it carries the mute notice instead of its lines.
    Trip,
    Muted,
}

/// Wakes per hour a command watch may cause before it is muted.
struct Budget {
    limit: usize,
    state: Mutex<BudgetState>,
}

#[derive(Default)]
struct BudgetState {
    wakes: VecDeque<Instant>,
    muted: bool,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            state: Mutex::default(),
        }
    }

    fn admit(&self, now: Instant) -> Admit {
        let mut st = self.state.lock();
        if st.muted {
            return Admit::Muted;
        }
        while st
            .wakes
            .front()
            .is_some_and(|t| now.duration_since(*t) >= HOUR)
        {
            st.wakes.pop_front();
        }
        if st.wakes.len() >= self.limit {
            st.muted = true;
            return Admit::Trip;
        }
        st.wakes.push_back(now);
        Admit::Yes
    }

    /// Clear the mute; whether the watch was muted.
    fn rearm(&self) -> bool {
        let mut st = self.state.lock();
        st.wakes.clear();
        std::mem::take(&mut st.muted)
    }
}

/// Events of one window: identical lines collapse into one with a count.
#[derive(Default)]
struct Batch {
    rows: Vec<(String, usize)>,
    seen: HashMap<String, usize>,
    total: usize,
    omitted: usize,
}

impl Batch {
    fn push(&mut self, line: String) {
        self.total += 1;
        if let Some(&at) = self.seen.get(&line) {
            self.rows[at].1 += 1;
        } else if self.rows.len() >= MAX_BATCH_LINES {
            self.omitted += 1;
        } else {
            self.seen.insert(line.clone(), self.rows.len());
            self.rows.push((line, 1));
        }
    }

    fn render(&self, description: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .rows
            .iter()
            .map(|(line, n)| {
                if *n > 1 {
                    format!("Monitor event({description}): {line} (x{n})")
                } else {
                    format!("Monitor event({description}): {line}")
                }
            })
            .collect();
        if self.omitted > 0 {
            out.push(format!(
                "Monitor event({description}): … {} more event(s)",
                self.omitted
            ));
        }
        out
    }
}

enum Ending {
    /// The watched command settled.
    Exited(JobStatus),
    /// A one-shot path watch saw its event.
    Fired,
    Timeout,
    Killed,
}

/// One running watch, as its task sees it.
struct Watch {
    hub: Arc<Hub>,
    jobs: JobRegistry,
    id: String,
    description: String,
    /// The watched command's `bg_N` job; `None` for a path watch.
    job_id: Option<String>,
    cancel: Arc<AbortController>,
    budget: Option<Arc<Budget>>,
    deadline: Option<Instant>,
    timeout_ms: u64,
    delivered: usize,
    dropped: usize,
}

impl Watch {
    /// Wake the session with the batch's lines and, when the watch ended, its summary.
    fn flush(&mut self, batch: &mut Batch, ending: Option<&Ending>) {
        let taken = std::mem::take(batch);
        let mut parts: Vec<String> = Vec::new();
        if taken.total > 0 {
            let admit = self
                .budget
                .as_ref()
                .map_or(Admit::Yes, |b| b.admit(Instant::now()));
            match admit {
                Admit::Yes => {
                    parts.extend(taken.render(&self.description));
                    self.delivered += taken.total;
                }
                Admit::Trip => {
                    parts.push(format!(
                        "Monitor {}({}) muted: more than its wake budget in an hour, so later events are dropped. Run monitor({{ action: \"rearm\", bash_id: \"{}\" }}) to resume; completion still wakes this session.",
                        self.id, self.description, self.id
                    ));
                    self.dropped += taken.total;
                }
                Admit::Muted => self.dropped += taken.total,
            }
        }
        if let Some(summary) = ending.and_then(|e| self.summary(e)) {
            parts.push(summary);
        }
        if !parts.is_empty() {
            self.hub.deliver(format!(
                "<system-reminder>\n{}\n</system-reminder>",
                parts.join("\n")
            ));
        }
    }

    fn summary(&self, ending: &Ending) -> Option<String> {
        let reason = match ending {
            Ending::Fired => return None,
            Ending::Exited(JobStatus::Completed) => "the command exited with code 0".to_owned(),
            Ending::Exited(JobStatus::Killed) | Ending::Killed => "it was killed".to_owned(),
            Ending::Exited(_) => self
                .job_id
                .as_deref()
                .and_then(|id| self.jobs.get(id))
                .and_then(|job| job.error)
                .unwrap_or_else(|| "the command failed".to_owned()),
            Ending::Timeout => format!(
                "timed out after {}s; {}",
                self.timeout_ms.div_ceil(1000),
                if self.job_id.is_some() {
                    "the command was killed"
                } else {
                    "the watch was stopped"
                }
            ),
        };
        let dropped = if self.dropped > 0 {
            format!(", {} dropped while muted", self.dropped)
        } else {
            String::new()
        };
        Some(format!(
            "Monitor {}({}) ended: {reason}. {} event(s) delivered{dropped}.",
            self.id, self.description, self.delivered
        ))
    }
}

async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Own one watch from its first event to its ending.
async fn drive(mut watch: Watch, mut rx: UnboundedReceiver<WatchMsg>) {
    let signal = watch.cancel.signal();
    let mut batch = Batch::default();
    let mut flush_at: Option<Instant> = None;
    let ending = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(WatchMsg::Line(line)) => {
                    batch.push(line);
                    flush_at.get_or_insert_with(|| Instant::now() + BATCH_WINDOW);
                }
                // A path watch has no job: its "settled" is the one-shot event.
                Some(WatchMsg::Settled(status)) => {
                    break if watch.job_id.is_some() { Ending::Exited(status) } else { Ending::Fired };
                }
                None => break Ending::Killed,
            },
            () = until(flush_at) => {
                watch.flush(&mut batch, None);
                flush_at = None;
            }
            () = until(watch.deadline) => {
                watch.cancel.abort();
                break Ending::Timeout;
            }
            () = signal.aborted() => break Ending::Killed,
        }
    };
    // Lines that were already queued when the ending won the race still belong to this wake.
    while let Ok(msg) = rx.try_recv() {
        if let WatchMsg::Line(line) = msg {
            batch.push(line);
        }
    }
    watch.flush(&mut batch, Some(&ending));
    watch.hub.finish(&watch.id);
}

/// The deadline of a watch: none when persistent.
fn deadline_for(persistent: bool, timeout_ms: u64) -> Option<Instant> {
    (!persistent).then(|| Instant::now() + Duration::from_millis(timeout_ms))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathEvent {
    Create,
    Modify,
}

impl PathEvent {
    fn verb(self) -> &'static str {
        match self {
            Self::Create => "created",
            Self::Modify => "modified",
        }
    }
}

/// Whether the file notifier's event is the kind of change the watch asked for.
fn event_matches(kind: &EventKind, want: PathEvent) -> bool {
    match (want, kind) {
        (PathEvent::Create, EventKind::Create(_)) => true,
        // A rename onto the path makes it appear.
        (
            PathEvent::Create,
            EventKind::Modify(ModifyKind::Name(
                RenameMode::To | RenameMode::Both | RenameMode::Any,
            )),
        ) => true,
        (PathEvent::Modify, EventKind::Modify(ModifyKind::Name(_))) => false,
        (PathEvent::Modify, EventKind::Modify(_)) => true,
        _ => false,
    }
}

/// Whether `event` is a change to `target` of the wanted kind.
fn hits(event: &notify::Event, target: &std::path::Path, want: PathEvent) -> bool {
    // A both-sided rename lists [from, to]: only `to` is where the file now is.
    let paths: &[PathBuf] = match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            event.paths.get(1..).unwrap_or(&[])
        }
        _ => &event.paths,
    };
    paths.iter().any(|p| p == target) && event_matches(&event.kind, want)
}

fn non_empty<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn bounded(args: &Value, key: &str, max: u64) -> Result<Option<u64>> {
    match args.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => match v.as_u64().filter(|n| (1..=max).contains(n)) {
            Some(n) => Ok(Some(n)),
            None => bail!("{key} must be an integer from 1 to {max}"),
        },
    }
}

/// `monitor { action?, type?, description, command | path, event?, filter?, timeout_ms?,
/// persistent?, wakes_per_hour?, bash_id? }`.
///
/// # Errors
/// Invalid arguments, a path whose parent directory is missing, or a notifier that cannot start.
/// Must run inside the tokio runtime: the watch is a spawned task.
pub fn monitor(env: &Env, args: &Value) -> Result<AgentToolResult> {
    match non_empty(args, "action").unwrap_or("create") {
        "create" => create(env, args),
        "rearm" => {
            let ids = env.jobs.monitors.rearm(non_empty(args, "bash_id"));
            Ok(AgentToolResult::text(if ids.is_empty() {
                "No paused monitors to re-arm.".to_owned()
            } else {
                format!("Re-armed {}.", ids.join(", "))
            }))
        }
        other => bail!("unknown monitor action `{other}`: use create or rearm"),
    }
}

#[allow(clippy::too_many_lines)] // one validation pass, then the command and path starts
fn create(env: &Env, args: &Value) -> Result<AgentToolResult> {
    let command = non_empty(args, "command");
    let path = non_empty(args, "path");
    let wanted = non_empty(args, "type");
    match wanted {
        None | Some("command" | "path") => {}
        Some("rpc") => bail!("monitor type \"rpc\" is unsupported: no peer bus in this build"),
        Some(other) => bail!("unknown monitor type `{other}`: use command or path"),
    }
    let Some(description) = non_empty(args, "description") else {
        bail!("monitor requires description and command or path to start a watcher.");
    };
    match (command, path) {
        (Some(_), Some(_)) => bail!("monitor accepts either command or path, not both."),
        (None, None) => {
            bail!("monitor requires description and command or path to start a watcher.")
        }
        (Some(_), None) if wanted == Some("path") => {
            bail!("type `path` needs `path`, not `command`")
        }
        (None, Some(_)) if wanted == Some("command") => {
            bail!("type `command` needs `command`, not `path`")
        }
        _ => {}
    }
    let timeout_ms = bounded(args, "timeout_ms", MAX_TIMEOUT_MS)?.unwrap_or(DEFAULT_TIMEOUT_MS);
    let persistent = args.get("persistent").and_then(Value::as_bool) == Some(true);
    let wakes = bounded(args, "wakes_per_hour", MAX_WAKES_PER_HOUR)?;
    let hub = env.jobs.monitors.clone();
    let deadline = deadline_for(persistent, timeout_ms);

    if let Some(command) = command {
        if args.get("event").is_some_and(|v| !v.is_null()) {
            bail!("event applies to path monitors.");
        }
        let filter = non_empty(args, "filter").unwrap_or("");
        regex::Regex::new(filter).map_err(|e| anyhow!("Invalid filter regex: {e}"))?;
        let (tx, rx) = unbounded_channel();
        let notify = Arc::new(Notify::streaming(filter, tx)?);
        let job_id = bash::start_watch(env, command.to_owned(), &notify);
        let cancel = env
            .jobs
            .get(&job_id)
            .map(|job| job.cancel)
            .ok_or_else(|| anyhow!("monitor job {job_id} vanished"))?;
        let budget = Arc::new(Budget::new(wakes.map_or(DEFAULT_WAKES_PER_HOUR, |n| {
            usize::try_from(n).unwrap_or(DEFAULT_WAKES_PER_HOUR)
        })));
        let id = hub.register(Entry {
            bash_id: Some(job_id.clone()),
            cancel: cancel.clone(),
            budget: Some(budget.clone()),
            running: true,
        });
        let watch = Watch {
            hub,
            jobs: env.jobs.clone(),
            id: id.clone(),
            description: description.to_owned(),
            job_id: Some(job_id.clone()),
            cancel,
            budget: Some(budget),
            deadline,
            timeout_ms,
            delivered: 0,
            dropped: 0,
        };
        tokio::spawn(drive(watch, rx));
        let limit = if persistent {
            "no deadline".to_owned()
        } else {
            format!("deadline {}s", timeout_ms.div_ceil(1000))
        };
        let mut result = AgentToolResult::text(format!(
            "Monitor {id} started: `{command}` ({limit}, job {job_id}). Matching lines wake your next turn as system reminders; do not poll. Stop it with kill_bash {{bash_id: \"{id}\"}}."
        ));
        result.details = json!({"monitor_id": id, "bash_id": job_id, "type": "command"});
        return Ok(result);
    }

    let raw = path.expect("command or path is present");
    if args.get("filter").is_some_and(|v| !v.is_null()) {
        bail!("Native file monitors do not support filter.");
    }
    if wakes.is_some() {
        bail!("wakes_per_hour applies to command monitors; a file watch is not wake-paced.");
    }
    let want = match non_empty(args, "event").unwrap_or("create") {
        "create" => PathEvent::Create,
        "modify" => PathEvent::Modify,
        other => bail!("unknown event `{other}`: use create or modify"),
    };
    let target = super::util::resolve_to_cwd(raw, &env.cwd);
    let parent = target
        .parent()
        .filter(|p| p.is_dir())
        .ok_or_else(|| anyhow!("parent directory of {raw} does not exist"))?
        .to_path_buf();
    let (tx, rx) = unbounded_channel();
    let watched = target.clone();
    let shown = raw.to_owned();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if hits(&event, &watched, want) {
            let _ = tx.send(WatchMsg::Line(format!("{shown} {}", want.verb())));
            if !persistent {
                let _ = tx.send(WatchMsg::Settled(JobStatus::Completed));
            }
        }
    })
    .map_err(|e| anyhow!("cannot start the file watcher: {e}"))?;
    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .map_err(|e| anyhow!("cannot watch {}: {e}", parent.display()))?;
    let cancel = Arc::new(AbortController::new());
    let id = hub.register(Entry {
        bash_id: None,
        cancel: cancel.clone(),
        budget: None,
        running: true,
    });
    let watch = Watch {
        hub,
        jobs: env.jobs.clone(),
        id: id.clone(),
        description: description.to_owned(),
        job_id: None,
        cancel,
        budget: None,
        deadline,
        timeout_ms,
        delivered: 0,
        dropped: 0,
    };
    // The watcher lives exactly as long as the watch: dropping it stops the notifier.
    tokio::spawn(async move {
        let _watcher = watcher;
        drive(watch, rx).await;
    });
    let mut text = format!(
        "Monitor {id} started: {raw} {} ({}). It wakes your next turn once{}; do not poll. Stop it with kill_bash {{bash_id: \"{id}\"}}.",
        want.verb(),
        if persistent {
            "no deadline".to_owned()
        } else {
            format!("deadline {}s", timeout_ms.div_ceil(1000))
        },
        if persistent { " per change" } else { "" },
    );
    if want == PathEvent::Create && target.exists() {
        text.push_str(" The path already exists: `create` fires when it appears again.");
    }
    let mut result = AgentToolResult::text(text);
    result.details = json!({"monitor_id": id, "type": "path", "path": raw});
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bash_output::kill_bash;
    use tokio::sync::mpsc::UnboundedSender;

    enum Seen {
        Wake(String),
        Active(usize),
    }

    struct Recorder(UnboundedSender<Seen>);

    impl WakeSink for Recorder {
        fn deliver(&self, text: String) {
            let _ = self.0.send(Seen::Wake(text));
        }
        fn active(&self, count: usize) {
            let _ = self.0.send(Seen::Active(count));
        }
    }

    fn env_in(dir: &std::path::Path) -> (Env, UnboundedReceiver<Seen>) {
        let env = Env::new(dir, false);
        let (tx, rx) = unbounded_channel();
        env.jobs.monitors.bind(Arc::new(Recorder(tx)));
        (env, rx)
    }

    fn env() -> (Env, UnboundedReceiver<Seen>) {
        env_in(&std::env::temp_dir())
    }

    async fn next(rx: &mut UnboundedReceiver<Seen>) -> Seen {
        tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("no wake within 30s")
            .expect("sink closed")
    }

    async fn next_wake(rx: &mut UnboundedReceiver<Seen>) -> String {
        loop {
            if let Seen::Wake(text) = next(rx).await {
                return text;
            }
        }
    }

    async fn until_active(rx: &mut UnboundedReceiver<Seen>, want: usize) {
        loop {
            if let Seen::Active(n) = next(rx).await {
                if n == want {
                    return;
                }
            }
        }
    }

    fn body(r: &AgentToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                pa_agent::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                pa_agent::types::ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn err(env: &Env, args: &Value) -> String {
        match monitor(env, args) {
            Ok(r) => panic!("expected an error, got: {}", body(&r)),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn command_watch_wakes_with_filtered_lines_and_a_summary() {
        let (env, mut rx) = env();
        let started = monitor(
            &env,
            &json!({
                "command": "printf 'noise\\nERROR one\\nERROR one\\nok\\nERROR two\\n'",
                "filter": "ERROR",
                "description": "errs",
            }),
        )
        .unwrap();
        assert_eq!(started.details["monitor_id"], "mon_1");
        assert!(started.details["bash_id"]
            .as_str()
            .unwrap()
            .starts_with("bg_"));
        let wake = next_wake(&mut rx).await;
        assert!(wake.starts_with("<system-reminder>\n"), "{wake}");
        assert!(
            wake.contains("Monitor event(errs): ERROR one (x2)"),
            "{wake}"
        );
        assert!(wake.contains("Monitor event(errs): ERROR two"), "{wake}");
        assert!(!wake.contains("noise"), "{wake}");
        assert!(
            wake.contains(
                "Monitor mon_1(errs) ended: the command exited with code 0. 3 event(s) delivered."
            ),
            "{wake}"
        );
        until_active(&mut rx, 0).await;
        assert_eq!(env.jobs.monitors.running(), 0);
    }

    #[tokio::test]
    async fn a_line_wakes_the_session_while_the_command_still_runs_and_kill_ends_it() {
        let (env, mut rx) = env();
        let started = monitor(
            &env,
            &json!({"command": "printf 'READY\\n'; sleep 30", "filter": "READY", "description": "svc"}),
        )
        .unwrap();
        let job_id = started.details["bash_id"].as_str().unwrap().to_owned();
        let wake = next_wake(&mut rx).await;
        assert!(wake.contains("Monitor event(svc): READY"), "{wake}");
        assert!(!wake.contains("ended"), "{wake}");
        assert_eq!(
            env.jobs.get(&job_id).unwrap().status,
            JobStatus::Running,
            "the wake came from a live command"
        );
        let out = body(&kill_bash(&env, &json!({"bash_id": "mon_1"})).unwrap());
        assert_eq!(out, "Killed mon_1.");
        let wake = next_wake(&mut rx).await;
        assert!(
            wake.contains("Monitor mon_1(svc) ended: it was killed. 1 event(s) delivered."),
            "{wake}"
        );
        assert!(env.jobs.get(&job_id).unwrap().cancel.signal().is_aborted());
    }

    #[tokio::test]
    async fn timeout_kills_the_watched_command() {
        let (env, mut rx) = env();
        let started = monitor(
            &env,
            &json!({"command": "sleep 30", "timeout_ms": 300, "description": "slow"}),
        )
        .unwrap();
        let job_id = started.details["bash_id"].as_str().unwrap().to_owned();
        let wake = next_wake(&mut rx).await;
        assert!(
            wake.contains("Monitor mon_1(slow) ended: timed out after 1s; the command was killed. 0 event(s) delivered."),
            "{wake}"
        );
        assert!(env.jobs.get(&job_id).unwrap().cancel.signal().is_aborted());
    }

    #[test]
    fn persistent_has_no_deadline() {
        assert!(deadline_for(true, 1).is_none());
        assert!(deadline_for(false, 1).is_some());
    }

    #[tokio::test]
    async fn path_create_fires_once_and_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (env, mut rx) = env_in(dir.path());
        let started = monitor(
            &env,
            &json!({"path": "out.bin", "event": "create", "description": "artifact"}),
        )
        .unwrap();
        assert!(body(&started).contains("mon_1"), "{}", body(&started));
        until_active(&mut rx, 1).await;
        std::fs::write(dir.path().join("out.bin"), b"x").unwrap();
        let wake = next_wake(&mut rx).await;
        assert!(
            wake.contains("Monitor event(artifact): out.bin created"),
            "{wake}"
        );
        assert!(
            !wake.contains("ended"),
            "a fired watch needs no summary: {wake}"
        );
        until_active(&mut rx, 0).await;
    }

    #[tokio::test]
    async fn path_modify_fires_on_a_change_to_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("log.txt");
        std::fs::write(&file, b"a").unwrap();
        let (env, mut rx) = env_in(dir.path());
        monitor(
            &env,
            &json!({"path": "log.txt", "event": "modify", "description": "log"}),
        )
        .unwrap();
        until_active(&mut rx, 1).await;
        std::fs::write(&file, b"ab").unwrap();
        let wake = next_wake(&mut rx).await;
        assert!(
            wake.contains("Monitor event(log): log.txt modified"),
            "{wake}"
        );
    }

    #[tokio::test]
    async fn path_watch_times_out_with_a_summary_and_can_be_killed() {
        let dir = tempfile::tempdir().unwrap();
        let (env, mut rx) = env_in(dir.path());
        monitor(
            &env,
            &json!({"path": "never", "timeout_ms": 200, "description": "gone"}),
        )
        .unwrap();
        let wake = next_wake(&mut rx).await;
        assert!(
            wake.contains("Monitor mon_1(gone) ended: timed out after 1s; the watch was stopped."),
            "{wake}"
        );

        monitor(
            &env,
            &json!({"path": "also-never", "persistent": true, "description": "kill me"}),
        )
        .unwrap();
        let out = body(&kill_bash(&env, &json!({"all": true})).unwrap());
        assert_eq!(out, "Killed 1 session(s).");
        let wake = next_wake(&mut rx).await;
        assert!(
            wake.contains("Monitor mon_2(kill me) ended: it was killed."),
            "{wake}"
        );
    }

    #[test]
    fn event_kinds_map_to_create_and_modify() {
        use notify::event::{CreateKind, DataChange};
        let create = EventKind::Create(CreateKind::File);
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        let rename_to = EventKind::Modify(ModifyKind::Name(RenameMode::To));
        let rename_from = EventKind::Modify(ModifyKind::Name(RenameMode::From));
        assert!(event_matches(&create, PathEvent::Create));
        assert!(!event_matches(&create, PathEvent::Modify));
        assert!(event_matches(&data, PathEvent::Modify));
        assert!(!event_matches(&data, PathEvent::Create));
        assert!(event_matches(&rename_to, PathEvent::Create));
        assert!(!event_matches(&rename_to, PathEvent::Modify));
        assert!(!event_matches(&rename_from, PathEvent::Create));
        assert!(!event_matches(
            &EventKind::Remove(notify::event::RemoveKind::File),
            PathEvent::Create
        ));
    }

    #[test]
    fn a_rename_away_is_not_a_create() {
        let target = PathBuf::from("/w/a");
        let moved = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/w/a"))
            .add_path(PathBuf::from("/w/b"));
        assert!(!hits(&moved, &target, PathEvent::Create));
        let arrived = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/w/b"))
            .add_path(PathBuf::from("/w/a"));
        assert!(hits(&arrived, &target, PathEvent::Create));
    }

    #[test]
    fn invalid_calls_are_refused_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let (env, _rx) = env_in(dir.path());
        assert!(err(&env, &json!({"command": "true"})).contains("requires description"));
        assert!(err(&env, &json!({"description": "d"})).contains("requires description"));
        assert!(err(
            &env,
            &json!({"description": "d", "command": "true", "path": "p"})
        )
        .contains("not both"));
        assert!(err(
            &env,
            &json!({"description": "d", "type": "rpc", "command": "true"})
        )
        .contains("rpc"));
        assert!(err(
            &env,
            &json!({"description": "d", "command": "true", "filter": "(["})
        )
        .contains("Invalid filter regex"));
        assert!(err(
            &env,
            &json!({"description": "d", "command": "true", "event": "create"})
        )
        .contains("path monitors"));
        assert!(err(
            &env,
            &json!({"description": "d", "path": "p", "filter": "x"})
        )
        .contains("do not support filter"));
        assert!(err(
            &env,
            &json!({"description": "d", "path": "p", "wakes_per_hour": 5})
        )
        .contains("not wake-paced"));
        assert!(err(
            &env,
            &json!({"description": "d", "command": "true", "timeout_ms": 3_600_001})
        )
        .contains("timeout_ms must be"));
        assert!(err(
            &env,
            &json!({"description": "d", "command": "true", "wakes_per_hour": 0})
        )
        .contains("wakes_per_hour must be"));
        assert!(
            err(&env, &json!({"description": "d", "path": "missing/dir/f"}))
                .contains("parent directory")
        );
        assert!(err(&env, &json!({"action": "stop"})).contains("unknown monitor action"));
        assert_eq!(
            env.jobs.monitors.running(),
            0,
            "a refused call starts nothing"
        );
    }

    #[test]
    fn batch_collapses_identical_lines_and_caps_distinct_ones() {
        let mut batch = Batch::default();
        for _ in 0..3 {
            batch.push("same".into());
        }
        for i in 0..(MAX_BATCH_LINES + 2) {
            batch.push(format!("n{i}"));
        }
        let lines = batch.render("d");
        assert_eq!(lines[0], "Monitor event(d): same (x3)");
        assert_eq!(lines.len(), MAX_BATCH_LINES + 1);
        assert_eq!(lines.last().unwrap(), "Monitor event(d): … 3 more event(s)");
    }

    #[test]
    fn budget_mutes_after_the_limit_and_rearm_resumes() {
        let budget = Budget::new(2);
        let now = Instant::now();
        assert!(matches!(budget.admit(now), Admit::Yes));
        assert!(matches!(budget.admit(now), Admit::Yes));
        assert!(matches!(budget.admit(now), Admit::Trip));
        assert!(matches!(budget.admit(now), Admit::Muted));
        assert!(budget.rearm());
        assert!(!budget.rearm(), "nothing left to re-arm");
        assert!(matches!(budget.admit(now), Admit::Yes));
        // Wakes older than an hour no longer count.
        let later = now + HOUR;
        assert!(matches!(budget.admit(later), Admit::Yes));
        assert!(matches!(budget.admit(later), Admit::Yes));
    }

    #[tokio::test]
    async fn a_chatty_watch_is_muted_and_rearmed() {
        let (env, mut rx) = env();
        monitor(
            &env,
            &json!({
                "command": "printf 'a\\n'; sleep 0.5; printf 'b\\n'; sleep 0.5; printf 'c\\n'; sleep 30",
                "description": "chatty",
                "wakes_per_hour": 1,
                "persistent": true,
            }),
        )
        .unwrap();
        let first = next_wake(&mut rx).await;
        assert!(first.contains("Monitor event(chatty): a"), "{first}");
        let second = next_wake(&mut rx).await;
        assert!(second.contains("muted"), "{second}");
        assert!(
            second.contains("action: \"rearm\", bash_id: \"mon_1\""),
            "{second}"
        );
        let out = body(&monitor(&env, &json!({"action": "rearm", "bash_id": "mon_1"})).unwrap());
        assert_eq!(out, "Re-armed mon_1.");
        let out = body(&monitor(&env, &json!({"action": "rearm"})).unwrap());
        assert_eq!(out, "No paused monitors to re-arm.");
        kill_bash(&env, &json!({"all": true})).unwrap();
        let end = next_wake(&mut rx).await;
        assert!(end.contains("ended: it was killed"), "{end}");
        assert!(end.contains("dropped while muted"), "{end}");
    }

    #[test]
    fn the_schema_is_flat() {
        let schema = schema();
        assert_eq!(schema["type"], "object");
        for key in ["anyOf", "oneOf", "allOf"] {
            assert!(schema.get(key).is_none(), "root {key} is not allowed");
        }
        assert!(schema["properties"]["description"].is_object());
    }
}
