//! Vibe mode tools (`/vibe`): the director's entire non-read surface.
//!
//! Ported from omp's `packages/coding-agent/src/tools/vibe.ts` and its
//! `vibe/runtime.ts` registry: five thin tools over [`VibeSessionRegistry`]
//! that spawn, message, wait on, kill and list persistent worker sessions
//! ("little CLIs") driven by a `fast` or `good` model tier.
//!
//! Tool names, approval tiers, parameter schemas, result text and the
//! `details` payloads match the reference so the TUI renderers (mini composer
//! for spawn/send, TV wall for wait/list) can be ported without translation.
//!
//! Divergences from omp, all forced by this port's subagent plumbing:
//!
//! - Each turn is one `omt -p` child process through [`omp_task::runner`],
//!   not a keep-alive session with a live steering channel. Conversation
//!   continuity therefore rides on a bounded per-session history folded into
//!   the next turn's prompt ([`compose_prompt`]), and a message sent mid-turn
//!   is queued rather than steered (see [`VibeSendMode`]).
//! - Turn jobs live in the session's [`JobRegistry`] (omp's async job
//!   manager), so `vibe_wait` observes settlement the same way `wait` does
//!   for `bash` jobs. Cancelling a turn records the intent and settles the
//!   job; the child process finishes on its own (the task runner exposes no
//!   per-turn kill handle).
//! - omp's `useless` result flag has no counterpart on [`AgentToolResult`],
//!   so the "nothing to wait for" frames are plain text.
//! - Lifecycle persistence (`vibe-session-lifecycle` custom entries, TTL
//!   parking, JSONL revive) is not ported; the registry is process-local.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use omp_task::params::{SchemaMode, TaskItem};
use omp_task::runner::{RunOutput, RunStatus, TaskRunner, TaskSpec};
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};

use super::jobs::{Job, JobRegistry, JobStatus};
use super::Env;

/// omp `VIBE_TOOL_NAMES`: the ephemeral tools installed while `/vibe` is
/// active. They are deliberately absent from the built-in registry
/// (`tools.rs`'s table) and from `enabled_by_settings`, mirroring
/// `activateVibeTools` / `deactivateVibeTools`.
pub const VIBE_TOOL_NAMES: [&str; 5] = [
    "vibe_spawn",
    "vibe_send",
    "vibe_wait",
    "vibe_kill",
    "vibe_list",
];

/// Most recent killed ids named in `vibe_list`'s trailing line; older ones
/// fold into `+N more` (omp `KILLED_IDS_LISTED`).
pub const KILLED_IDS_LISTED: usize = 8;

/// `vibe_wait` window when the call gives no `timeout` (omp
/// `DEFAULT_WAIT_TIMEOUT_MS`).
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Response cap inside a follow-up prompt; the full output stays in the job.
const RESPONSE_PREVIEW_MAX: usize = 6000;

/// Turns of prior conversation folded into a follow-up prompt.
const HISTORY_TURNS_MAX: usize = 6;

/// Longest single blocking slice inside `vibe_wait` before re-checking the
/// job registry and the abort signal (omp races job promises; here the
/// registry is a snapshot store, so settlement is observed on a slice).
const SETTLE_SLICE_MAX: Duration = Duration::from_millis(250);

/// omp `WAIT_PROGRESS_INTERVAL_MS`: cadence of the interim TV-wall frames.
const WAIT_PROGRESS_INTERVAL_MS: u64 = 500;

// ─── Descriptions (verbatim omp `prompts/tools/vibe-*.md`) ────────────────

/// omp `vibeSpawnDescription`.
pub const VIBE_SPAWN_DESCRIPTION: &str = r"Starts persistent conversational coding-agent worker session (edit, bash, grep, everything).

CLI flavor by task:
- `fast`: low-latency model; mechanical, well-specified work (renames, boilerplate, running tests, data collection).
- `good`: strong model; hard work (design, debugging, multi-file changes, judgment calls).

`prompt`: first session instruction. Worker starts with NO context beyond it; include files, constraints, acceptance criteria.
`name`: optional session label; otherwise generated.

Returns session id immediately. On worker completion, turn result—activity trace + worker response—delivered automatically. Do not wait unless blocked; direct other sessions.

Session persists after turn; remembers whole conversation. Same-workstream follow-up: `vibe_send`; NEVER spawn second session.";

/// omp `vibeSendDescription`.
pub const VIBE_SEND_DESCRIPTION: &str = r"Send a worker session message by id from `vibe_spawn`/`vibe_list`. Session retains full conversation history; refer naturally (`now do the same for the other module`).

Returns immediately with an ack:
- `turn`: worker idle → new turn; result self-delivers when done.
- `steered`: worker mid-turn → message injected into the running turn as live steering.
- `queued`: worker mid-turn and not currently steerable → message runs automatically as next turn.

Use for follow-ups, corrections, scope changes, review requests. NEVER re-explain prior context; session already has it.";

/// omp `vibeWaitDescription`.
pub const VIBE_WAIT_DESCRIPTION: &str = r"Blocks until ONE watched session finishes its current turn, the timeout elapses, or you are interrupted — not until all finish. Re-issue to keep waiting.

Turn results normally deliver themselves; you NEVER need this to receive output. Use it only when you are completely blocked and cannot direct any other session.

- `sessions` — ids to watch. Omit to watch every session with a turn in flight.
- `timeout` — seconds to wait (default 30).

A finished turn's full result (activity trace + response) is returned here and will not be re-delivered separately.";

/// omp `vibeKillDescription`.
pub const VIBE_KILL_DESCRIPTION: &str = r"Terminates worker session: aborts in-flight turn, if any; discards session. Conversation cannot continue; transcript remains readable at `history://<id>`.

Kill stuck, looping, or completed-workstream sessions. Free dead weight → legible roster.";

/// omp `vibeListDescription`.
pub const VIBE_LIST_DESCRIPTION: &str = r"Worker-session roster: id, CLI flavor (`fast`/`good`), state (`starting`/`running`/`idle`/`dead`), model, turn count, queued messages, one-line gist of latest activity. Sessions you killed collapse to one trailing line of ids.

Use to reorient: existing sessions, busy workers, idle workers ready for next instruction.";

// ─── Parameter schemas (byte-equal, as JSON, to omp's ArkType schemas) ─────

/// omp `vibeSpawnSchema`.
#[must_use]
pub fn schema_spawn() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cli": {
                "enum": ["fast", "good"],
                "type": "string",
                "description": "worker flavor: fast = low-latency model for mechanical work; good = strong model for hard work"
            },
            "name": {
                "type": "string",
                "maxLength": 48,
                "description": "optional session name; generated when omitted"
            },
            "prompt": {
                "type": "string",
                "minLength": 1,
                "description": "first instruction; the worker starts with no other context"
            }
        },
        "required": ["cli", "prompt"],
        "additionalProperties": false
    })
}

/// omp `vibeSendSchema`.
#[must_use]
pub fn schema_send() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": {
                "type": "string",
                "minLength": 1,
                "description": "session id from vibe_spawn / vibe_list"
            },
            "message": {
                "type": "string",
                "minLength": 1,
                "description": "message for the session; steers mid-turn, else runs as its next turn"
            }
        },
        "required": ["session", "message"],
        "additionalProperties": false
    })
}

/// omp `vibeWaitSchema`.
#[must_use]
pub fn schema_wait() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessions": {
                "type": "array",
                "items": {"type": "string"},
                "description": "session ids to watch; omit to watch every session with a turn in flight"
            },
            "timeout": {
                "type": "number",
                "exclusiveMinimum": 0,
                "description": "max seconds to wait (default 30)"
            }
        },
        "additionalProperties": false
    })
}

/// omp `vibeKillSchema`.
#[must_use]
pub fn schema_kill() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": {"type": "string", "minLength": 1, "description": "session id to terminate"}
        },
        "required": ["session"],
        "additionalProperties": false
    })
}

/// omp `vibeListSchema` (an empty object: the roster takes no arguments).
#[must_use]
pub fn schema_list() -> Value {
    json!({"type": "object", "properties": {}})
}

// ─── Value types (omp `@oh-my-pi/pi-tui/tools/vibe` interfaces) ───────────

/// The two worker CLI flavors the director drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VibeCli {
    Fast,
    Good,
}

impl VibeCli {
    /// Parse the wire value (`"fast"` / `"good"`).
    ///
    /// # Errors
    /// Any other string, with omp's "expected fast or good" wording.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "fast" => Ok(Self::Fast),
            "good" => Ok(Self::Good),
            other => bail!("Invalid cli {other:?}: expected \"fast\" or \"good\"."),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Good => "good",
        }
    }

    /// Bundled agent type for this flavor (omp `VIBE_CLI_AGENT`): `sonic`
    /// carries the fast/smol model role, `task` the session's strong model.
    #[must_use]
    pub fn agent(self) -> &'static str {
        match self {
            Self::Fast => "sonic",
            Self::Good => "task",
        }
    }
}

/// Worker session lifecycle as shown to the director.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VibeSessionState {
    Starting,
    Running,
    Idle,
    Dead,
}

impl VibeSessionState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Dead => "dead",
        }
    }
}

/// Delivery mode of a `vibe_send` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VibeSendMode {
    /// A new background turn was started (`job_id` set).
    Turn,
    /// The message was injected into a running turn. Kept for payload parity
    /// with omp: this port has no live steering channel, so it is never
    /// produced (a mid-turn message queues instead).
    Steered,
    /// The worker was mid-turn; the message runs automatically as its next turn.
    Queued,
}

/// Live per-session "screen" for rich rendering (the TV wall): what the
/// worker is doing right now plus roster metadata. Every string is already
/// one-line sanitized, so renderers can print them verbatim.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeScreenSnapshot {
    pub id: String,
    pub cli: VibeCli,
    pub state: VibeSessionState,
    /// Terminated by the director (`vibe_kill`); unset for workers that died
    /// on their own.
    pub killed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub turns: u32,
    pub queued: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_intent: Option<String>,
    pub trace: Vec<String>,
    pub output_tail: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    pub last_activity_at: u64,
}

/// Worker identity and job handle returned by a spawn.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeSpawnOutcome {
    pub id: String,
    pub job_id: String,
}

/// Delivery mode and optional turn job for a worker message.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeSendOutcome {
    pub id: String,
    pub mode: VibeSendMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

/// Worker shutdown outcome including any cancelled turn.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeKillOutcome {
    pub id: String,
    pub cancelled_turn: bool,
}

/// One watched turn that settled during (or before) a wait.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeSettledTurn {
    pub id: String,
    pub job_id: String,
    /// `completed`, `failed` or `cancelled` (omp's job status names).
    pub status: String,
    pub result_text: String,
}

/// Settled and still-running worker turns observed by a wait.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VibeWaitOutcome {
    pub settled: Vec<VibeSettledTurn>,
    pub still_running: Vec<String>,
    pub timed_out: bool,
}

/// Owner scope of a vibe roster: omp keys records by owner agent id *and*
/// stable parent session id, so two sessions in one process never see each
/// other's workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VibeOwnerScope {
    pub owner_id: String,
    pub parent_session_id: String,
}

impl VibeOwnerScope {
    #[must_use]
    pub fn new(owner_id: impl Into<String>, parent_session_id: impl Into<String>) -> Self {
        Self {
            owner_id: owner_id.into(),
            parent_session_id: parent_session_id.into(),
        }
    }

    /// Scope of the calling session. omp reads these from its agent registry;
    /// here they come from the process environment, with the working
    /// directory as the last resort so a plain CLI run still gets a stable
    /// scope for its own lifetime.
    #[must_use]
    pub fn for_env(env: &Env) -> Self {
        let owner_id = std::env::var("OMP_AGENT_ID")
            .or_else(|_| std::env::var("PI_AGENT_ID"))
            .unwrap_or_else(|_| "main".to_string());
        let parent_session_id = std::env::var("OMP_SESSION_ID")
            .or_else(|_| std::env::var("PI_SESSION_ID"))
            .or_else(|_| std::env::var("OMP_RS_SESSION_ID"))
            .unwrap_or_else(|_| format!("cwd:{}", env.cwd.display()));
        Self::new(owner_id, parent_session_id)
    }
}

// ─── Internal record ─────────────────────────────────────────────────────

/// One in-flight worker turn.
#[derive(Debug, Clone)]
struct VibeTurn {
    job_id: String,
    message: String,
    started_at_ms: u64,
    /// Completed tool calls of this turn, oldest first (capped).
    trace: Vec<String>,
    tool_count: u32,
}

#[derive(Debug, Clone)]
struct VibeRecord {
    id: String,
    cli: VibeCli,
    owner_id: String,
    parent_session_id: String,
    /// Working directory every turn of this session runs in.
    cwd: PathBuf,
    state: VibeSessionState,
    created_at_ms: u64,
    last_activity_at_ms: u64,
    last_activity: Option<String>,
    resolved_model: Option<String>,
    turn: Option<VibeTurn>,
    /// Job id of the most recently started turn (wait snapshots after settle).
    last_job_id: Option<String>,
    /// Messages queued while a turn was in flight; drained into the next turn.
    queue: Vec<String>,
    turn_count: u32,
    killed: bool,
    /// Prior turns, oldest first: `(instruction, worker response)`.
    history: Vec<(String, String)>,
}

impl VibeRecord {
    fn matches(&self, scope: &VibeOwnerScope) -> bool {
        self.owner_id == scope.owner_id && self.parent_session_id == scope.parent_session_id
    }

    fn screen(&self) -> VibeScreenSnapshot {
        VibeScreenSnapshot {
            id: self.id.clone(),
            cli: self.cli,
            state: self.state,
            killed: self.killed,
            model: self.resolved_model.clone(),
            turns: self.turn_count,
            queued: self.queue.len(),
            turn_started_at: self.turn.as_ref().map(|turn| turn.started_at_ms),
            turn_message: self
                .turn
                .as_ref()
                .map(|turn| one_line_label(&turn.message, 80)),
            current_tool: None,
            current_tool_args: None,
            last_intent: None,
            trace: self
                .turn
                .as_ref()
                .map(|turn| {
                    turn.trace
                        .iter()
                        .rev()
                        .take(6)
                        .rev()
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            output_tail: Vec::new(),
            last_activity: self.last_activity.clone(),
            last_activity_at: self.last_activity_at_ms,
        }
    }
}

#[derive(Default)]
struct RegistryInner {
    records: HashMap<String, VibeRecord>,
    next_session: u64,
    /// Monotonic stamp handed to each record so two sessions created in the
    /// same millisecond still sort in spawn order.
    next_stamp: u64,
}

/// Process-global registry of vibe worker sessions, scoped by owner agent id
/// and stable parent session id (omp `VibeSessionRegistry`).
pub struct VibeSessionRegistry {
    inner: Mutex<RegistryInner>,
    /// Turn jobs, shared with the session's [`JobRegistry`] so `wait`,
    /// `/jobs` and `proc://` see them.
    runner: TaskRunner,
    /// Job ids whose turn was cancelled by `vibe_kill`. Insert-only for the
    /// life of the process: a cancelled id is never reused.
    cancelled_jobs: Mutex<HashSet<String>>,
    /// Wakes blocked waits when a turn settles.
    settle: tokio::sync::Notify,
}

static GLOBAL: Mutex<Option<Arc<VibeSessionRegistry>>> = Mutex::new(None);

impl VibeSessionRegistry {
    /// A registry whose turns spawn this executable's subagent runner.
    #[must_use]
    pub fn new() -> Self {
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("omt"));
        Self::with_runner(TaskRunner::new(
            binary,
            TaskRunner::default_max_concurrency(),
        ))
    }

    /// A registry over a caller-supplied runner (tests, embedded callers).
    #[must_use]
    pub fn with_runner(runner: TaskRunner) -> Self {
        Self {
            inner: Mutex::new(RegistryInner::default()),
            runner,
            cancelled_jobs: Mutex::new(HashSet::new()),
            settle: tokio::sync::Notify::new(),
        }
    }

    /// The process-global registry (omp `VibeSessionRegistry.global()`).
    #[must_use]
    pub fn global() -> Arc<Self> {
        let mut slot = GLOBAL.lock();
        if let Some(existing) = slot.as_ref() {
            return Arc::clone(existing);
        }
        let registry = Arc::new(Self::new());
        *slot = Some(Arc::clone(&registry));
        registry
    }

    /// Reset the global registry. Test-only (omp `resetGlobalForTests`).
    pub fn reset_global_for_tests() {
        *GLOBAL.lock() = None;
    }

    fn key(scope: &VibeOwnerScope, id: &str) -> String {
        format!(
            "{}\u{0}{}\u{0}{}",
            scope.owner_id, scope.parent_session_id, id
        )
    }

    /// Ids of every live worker in `scope`, in spawn order.
    #[must_use]
    pub fn list_ids(&self, scope: &VibeOwnerScope) -> Vec<String> {
        let inner = self.inner.lock();
        let mut records: Vec<&VibeRecord> = inner
            .records
            .values()
            .filter(|record| record.matches(scope) && record.state != VibeSessionState::Dead)
            .collect();
        records.sort_by_key(|record| record.created_at_ms);
        records.iter().map(|record| record.id.clone()).collect()
    }

    /// Live screen snapshots for rich rendering (the TV wall): one entry per
    /// session in creation order. `ids` narrows the wall to exactly those
    /// sessions; otherwise every session in scope is returned (killed ones
    /// included — [`wall`] decides what to hide).
    #[must_use]
    pub fn screens(
        &self,
        scope: &VibeOwnerScope,
        ids: Option<&[String]>,
    ) -> Vec<VibeScreenSnapshot> {
        let wanted: Option<HashSet<&str>> = ids
            .filter(|ids| !ids.is_empty())
            .map(|ids| ids.iter().map(|id| id.trim()).collect());
        let inner = self.inner.lock();
        let mut records: Vec<&VibeRecord> = inner
            .records
            .values()
            .filter(|record| record.matches(scope))
            .filter(|record| {
                wanted
                    .as_ref()
                    .is_none_or(|set| set.contains(record.id.as_str()))
            })
            .collect();
        // Stable TV-wall ordering: spawn order, not activity order.
        records.sort_by_key(|record| record.created_at_ms);
        records.iter().map(|record| record.screen()).collect()
    }

    /// Spawn a persistent worker session and start its first turn in the
    /// background.
    ///
    /// # Errors
    /// An empty prompt, or a turn that cannot be scheduled.
    pub fn spawn(
        self: &Arc<Self>,
        scope: &VibeOwnerScope,
        cwd: &Path,
        jobs: &JobRegistry,
        args: SpawnArgs,
    ) -> Result<VibeSpawnOutcome> {
        if args.prompt.trim().is_empty() {
            bail!("prompt must not be empty.");
        }
        let (key, id) = {
            let mut inner = self.inner.lock();
            let id = allocate_id(&mut inner, scope, args.cli, args.name.as_deref());
            let now = stamp(&mut inner);
            inner.records.insert(
                Self::key(scope, &id),
                VibeRecord {
                    id: id.clone(),
                    cli: args.cli,
                    owner_id: scope.owner_id.clone(),
                    parent_session_id: scope.parent_session_id.clone(),
                    cwd: cwd.to_path_buf(),
                    state: VibeSessionState::Starting,
                    created_at_ms: now,
                    last_activity_at_ms: now,
                    last_activity: Some("spawned".to_string()),
                    resolved_model: None,
                    turn: None,
                    last_job_id: None,
                    queue: Vec::new(),
                    turn_count: 0,
                    killed: false,
                    history: Vec::new(),
                },
            );
            (Self::key(scope, &id), id)
        };
        match self.start_turn(jobs, &key, args.prompt) {
            Ok(job_id) => Ok(VibeSpawnOutcome { id, job_id }),
            Err(error) => {
                let _ = self.inner.lock().records.remove(&key);
                Err(error)
            }
        }
    }

    /// Send a message to a worker: a new background turn when the worker is
    /// idle, otherwise queued for the next turn.
    ///
    /// # Errors
    /// An unknown or dead session, or an empty message.
    pub fn send(
        self: &Arc<Self>,
        scope: &VibeOwnerScope,
        jobs: &JobRegistry,
        session: &str,
        message: &str,
    ) -> Result<VibeSendOutcome> {
        let message = message.trim();
        if message.is_empty() {
            bail!("Message must not be empty.");
        }
        let key = Self::key(scope, session.trim());
        let (id, mid_turn) = {
            let mut inner = self.inner.lock();
            if !inner.records.contains_key(&key) {
                let roster = list_ids_locked(&inner, scope);
                bail!("{}", unknown_session_error(session, &roster));
            }
            let record = inner
                .records
                .get_mut(&key)
                .expect("record existence was checked above");
            if record.state == VibeSessionState::Dead {
                bail!(
                    "Vibe session \"{}\" is dead. Spawn a new one with vibe_spawn.",
                    record.id
                );
            }
            let mid_turn = record.turn.is_some();
            if mid_turn {
                record.queue.push(message.to_string());
                record.last_activity = Some("message queued".to_string());
                record.last_activity_at_ms = now_ms();
            }
            (record.id.clone(), mid_turn)
        };
        if mid_turn {
            return Ok(VibeSendOutcome {
                id,
                mode: VibeSendMode::Queued,
                job_id: None,
            });
        }
        let job_id = self.start_turn(jobs, &key, message.to_string())?;
        Ok(VibeSendOutcome {
            id,
            mode: VibeSendMode::Turn,
            job_id: Some(job_id),
        })
    }

    /// Terminate one worker: settle its in-flight turn job as cancelled,
    /// drop queued messages and mark the session dead.
    ///
    /// # Errors
    /// An unknown session id.
    pub fn kill(
        &self,
        scope: &VibeOwnerScope,
        jobs: &JobRegistry,
        session: &str,
    ) -> Result<VibeKillOutcome> {
        let key = Self::key(scope, session.trim());
        let (id, turn_job) = {
            let mut inner = self.inner.lock();
            if !inner.records.contains_key(&key) {
                let roster = list_ids_locked(&inner, scope);
                bail!("{}", unknown_session_error(session, &roster));
            }
            let record = inner
                .records
                .get_mut(&key)
                .expect("record existence was checked above");
            let turn_job = record.turn.as_ref().map(|turn| turn.job_id.clone());
            record.killed = true;
            record.queue.clear();
            record.turn = None;
            record.state = VibeSessionState::Dead;
            record.last_activity = Some("killed".to_string());
            record.last_activity_at_ms = now_ms();
            (record.id.clone(), turn_job)
        };
        let cancelled_turn = turn_job.is_some_and(|job_id| {
            let running = jobs
                .get(&job_id)
                .is_some_and(|job| job.status == JobStatus::Running);
            if running {
                self.cancelled_jobs.lock().insert(job_id.clone());
                jobs.update(&job_id, |job| {
                    job.status = JobStatus::Failed;
                    job.error = Some("turn cancelled".to_string());
                    job.ended = Some(job.started.elapsed());
                });
                self.settle.notify_waiters();
            }
            running
        });
        Ok(VibeKillOutcome { id, cancelled_turn })
    }

    /// Block until one watched session's in-flight turn settles, the timeout
    /// elapses, or `signal` aborts (omp `hub` wait semantics). Named sessions
    /// are watched regardless of state; the no-args form watches every
    /// session with a turn actually in flight.
    ///
    /// `on_tick` runs on each blocking slice so the caller can emit interim
    /// TV-wall frames while the wait blocks.
    ///
    /// # Errors
    /// An unknown session id in `sessions`.
    pub async fn wait(
        &self,
        scope: &VibeOwnerScope,
        jobs: &JobRegistry,
        sessions: Option<&[String]>,
        timeout: Option<Duration>,
        signal: &AbortSignal,
        on_tick: &mut (dyn FnMut() + Send),
    ) -> Result<VibeWaitOutcome> {
        let snapshots: Vec<(String, String)> = self
            .watched(scope, sessions)?
            .into_iter()
            .filter_map(|record| {
                record
                    .turn
                    .as_ref()
                    .map(|turn| (record.id.clone(), turn.job_id.clone()))
                    .or_else(|| {
                        record
                            .last_job_id
                            .clone()
                            .map(|job| (record.id.clone(), job))
                    })
            })
            .collect();
        let timeout_ms = timeout.map_or(DEFAULT_WAIT_TIMEOUT_MS, |t| {
            u64::try_from(t.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        loop {
            if !collect_settled(jobs, self, &snapshots).is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if signal.is_aborted() {
                break;
            }
            on_tick();
            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice = remaining
                .min(SETTLE_SLICE_MAX)
                .max(Duration::from_millis(1));
            let _ = tokio::time::timeout(slice, self.settle.notified()).await;
        }
        let settled = collect_settled(jobs, self, &snapshots);
        // Current in-flight state, independent of the snapshot: a session
        // whose watched turn settled may already be mid follow-up turn.
        let still_running: Vec<String> = self
            .watched(scope, sessions)?
            .into_iter()
            .filter(|record| record.turn.is_some())
            .map(|record| record.id)
            .collect();
        Ok(VibeWaitOutcome {
            timed_out: timed_out && settled.is_empty(),
            settled,
            still_running,
        })
    }

    /// Watched records, cloned out of the registry (named ids are resolved
    /// strictly; the no-args form keeps sessions with a turn in flight).
    fn watched(
        &self,
        scope: &VibeOwnerScope,
        sessions: Option<&[String]>,
    ) -> Result<Vec<VibeRecord>> {
        let inner = self.inner.lock();
        match sessions.filter(|ids| !ids.is_empty()) {
            Some(ids) => {
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    let Some(record) = inner.records.get(&Self::key(scope, id.trim())) else {
                        let roster = list_ids_locked(&inner, scope);
                        bail!("{}", unknown_session_error(id, &roster));
                    };
                    out.push(record.clone());
                }
                Ok(out)
            }
            None => Ok(inner
                .records
                .values()
                .filter(|record| record.matches(scope) && record.turn.is_some())
                .cloned()
                .collect()),
        }
    }

    /// Register one background job that runs a single worker turn.
    fn start_turn(
        self: &Arc<Self>,
        jobs: &JobRegistry,
        key: &str,
        message: String,
    ) -> Result<String> {
        let (id, cli, cwd, prompt) = {
            let mut inner = self.inner.lock();
            let Some(record) = inner.records.get_mut(key) else {
                bail!("Unknown vibe session.");
            };
            if record.state == VibeSessionState::Dead {
                bail!(
                    "Vibe session \"{}\" is dead. Spawn a new one with vibe_spawn.",
                    record.id
                );
            }
            let prompt = compose_prompt(record, &message);
            record.state = VibeSessionState::Running;
            record.last_activity = Some("turn started".to_string());
            record.last_activity_at_ms = now_ms();
            (record.id.clone(), record.cli, record.cwd.clone(), prompt)
        };
        let spec = turn_spec(&id, cli, &cwd, prompt)?;
        let job_id = jobs.insert(Job {
            command: format!("vibe {} {}", cli.as_str(), id),
            started: Instant::now(),
            status: JobStatus::Running,
            output: Arc::new(StdMutex::new(String::new())),
            result: None,
            error: None,
            ended: None,
        });
        {
            let mut inner = self.inner.lock();
            if let Some(record) = inner.records.get_mut(key) {
                record.turn = Some(VibeTurn {
                    job_id: job_id.clone(),
                    message,
                    started_at_ms: now_ms(),
                    trace: Vec::new(),
                    tool_count: 0,
                });
                record.last_job_id = Some(job_id.clone());
            }
        }
        let registry = Arc::clone(self);
        let jobs = jobs.clone();
        let key = key.to_string();
        let job = job_id.clone();
        tokio::spawn(async move {
            let output = registry.runner.spawn(spec).await;
            registry.finish_turn(&jobs, &key, &job, output);
        });
        Ok(job_id)
    }

    /// Settle one turn: publish the job result, fold the exchange into the
    /// session history, then start any queued follow-up turn.
    fn finish_turn(
        self: &Arc<Self>,
        jobs: &JobRegistry,
        key: &str,
        job_id: &str,
        output: RunOutput,
    ) {
        let completed = output.status == RunStatus::Completed;
        let body = response_text(&output);
        let failure = failure_text(&output);
        let elapsed = output.duration;
        jobs.update(job_id, |job| {
            job.status = if completed {
                JobStatus::Completed
            } else {
                JobStatus::Failed
            };
            if completed {
                job.result = Some(body.clone());
            } else {
                job.error = Some(failure);
            }
            job.ended = Some(elapsed);
        });
        let next_message = {
            let mut inner = self.inner.lock();
            let Some(record) = inner.records.get_mut(key) else {
                return;
            };
            let instruction = record
                .turn
                .as_ref()
                .filter(|turn| turn.job_id == job_id)
                .map_or_else(|| "turn".to_string(), |turn| turn.message.clone());
            if record
                .turn
                .as_ref()
                .is_some_and(|turn| turn.job_id == job_id)
            {
                record.turn = None;
            }
            record.turn_count += 1;
            record.history.push((instruction, body));
            if record.history.len() > HISTORY_TURNS_MAX {
                let excess = record.history.len() - HISTORY_TURNS_MAX;
                record.history.drain(..excess);
            }
            record.last_job_id = Some(job_id.to_string());
            record.last_activity_at_ms = now_ms();
            record.last_activity = Some(format!(
                "turn {} {}",
                record.turn_count,
                if completed { "completed" } else { "failed" }
            ));
            if record.killed || record.state == VibeSessionState::Dead {
                record.state = VibeSessionState::Dead;
                None
            } else {
                record.state = VibeSessionState::Idle;
                if record.queue.is_empty() {
                    None
                } else {
                    Some(record.queue.remove(0))
                }
            }
        };
        self.settle.notify_waiters();
        if let Some(message) = next_message {
            let _ = self.start_turn(jobs, key, message);
        }
    }

    #[cfg(test)]
    fn register_record_for_tests(
        &self,
        scope: &VibeOwnerScope,
        id: &str,
        cli: VibeCli,
        state: VibeSessionState,
        killed: bool,
        job_id: Option<&str>,
    ) {
        let turn = job_id.map(|job_id| VibeTurn {
            job_id: job_id.to_string(),
            message: "test turn".to_string(),
            started_at_ms: now_ms(),
            trace: Vec::new(),
            tool_count: 0,
        });
        let mut inner = self.inner.lock();
        let now = stamp(&mut inner);
        inner.records.insert(
            Self::key(scope, id),
            VibeRecord {
                id: id.to_string(),
                cli,
                owner_id: scope.owner_id.clone(),
                parent_session_id: scope.parent_session_id.clone(),
                cwd: PathBuf::from("."),
                state,
                created_at_ms: now,
                last_activity_at_ms: now,
                last_activity: None,
                resolved_model: None,
                last_job_id: job_id.map(str::to_string),
                turn,
                queue: Vec::new(),
                turn_count: 0,
                killed,
                history: Vec::new(),
            },
        );
    }
}

impl Default for VibeSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Registry helpers ────────────────────────────────────────────────────

/// Arguments of one `vibe_spawn` call.
#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub cli: VibeCli,
    pub name: Option<String>,
    pub prompt: String,
}

/// Live ids of `scope` (the roster named in an unknown-session error).
fn list_ids_locked(inner: &RegistryInner, scope: &VibeOwnerScope) -> Vec<String> {
    inner
        .records
        .values()
        .filter(|record| record.matches(scope) && record.state != VibeSessionState::Dead)
        .map(|record| record.id.clone())
        .collect()
}

/// Reserve a unique session id: the sanitized `name` when given, else a
/// generated one; a collision gains a numeric suffix.
fn allocate_id(
    inner: &mut RegistryInner,
    scope: &VibeOwnerScope,
    cli: VibeCli,
    name: Option<&str>,
) -> String {
    let base = name.and_then(sanitize_session_name).unwrap_or_else(|| {
        inner.next_session += 1;
        format!("vibe-{}-{}", cli.as_str(), inner.next_session)
    });
    let mut candidate = base.clone();
    let mut suffix = 2u32;
    while inner
        .records
        .contains_key(&VibeSessionRegistry::key(scope, &candidate))
    {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// omp `#record`'s error text, including the active roster when there is one.
fn unknown_session_error(id: &str, roster: &[String]) -> String {
    if roster.is_empty() {
        format!("Unknown vibe session \"{id}\". No sessions — spawn one with vibe_spawn.")
    } else {
        format!(
            "Unknown vibe session \"{id}\". Active sessions: {}",
            roster.join(", ")
        )
    }
}

/// omp `sanitizeAgentId`: keep `[A-Za-z0-9_]`, fold every other run into a
/// single `-`, lowercase, trim separators, cap at 48 chars. `None` when
/// nothing survives (the caller generates a name instead).
#[must_use]
pub fn sanitize_session_name(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut pending_sep = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return None;
    }
    let capped: String = trimmed.chars().take(48).collect();
    Some(capped.trim_end_matches('-').to_string())
}

/// Compose a turn prompt: the instruction alone for a first turn, otherwise
/// the instruction after a bounded digest of the session's prior turns (this
/// port has no keep-alive session, so continuity rides on the prompt).
fn compose_prompt(record: &VibeRecord, message: &str) -> String {
    if record.history.is_empty() {
        return message.to_string();
    }
    let start = record.history.len().saturating_sub(HISTORY_TURNS_MAX);
    let mut out = String::from(
        "You are continuing a persistent worker session. Earlier turns of this \
         conversation are below; treat them as your own prior work.\n\n## Session so far\n\n",
    );
    for (index, (instruction, response)) in record.history[start..].iter().enumerate() {
        let turn = start + index + 1;
        out.push_str(&format!("### Turn {turn} — instruction\n{instruction}\n\n"));
        out.push_str(&format!(
            "### Turn {turn} — your response\n{}\n\n",
            truncate_chars(response, RESPONSE_PREVIEW_MAX)
        ));
    }
    out.push_str("## Next instruction\n\n");
    out.push_str(message);
    out
}

/// The [`TaskSpec`] for one worker turn: the flavor's bundled agent, the
/// session's working directory, no schema and no tool restriction.
fn turn_spec(id: &str, cli: VibeCli, cwd: &Path, prompt: String) -> Result<TaskSpec> {
    let item = TaskItem {
        name: Some(id.to_string()),
        agent: cli.agent().to_string(),
        task: prompt,
        solution_space: "one turn of a persistent worker session".to_string(),
        model: None,
        output_schema: None,
        schema_mode: SchemaMode::Permissive,
        tools: None,
    };
    item.validate()
        .map_err(|error| anyhow::anyhow!("vibe turn rejected: {error}"))?;
    Ok(TaskSpec::from_item(&item, None, cwd))
}

/// A settled turn's worker response, capped and annotated like omp's
/// `<response>` body.
fn response_text(output: &RunOutput) -> String {
    let text = output.output.trim();
    if text.is_empty() {
        return "(no output)".to_string();
    }
    if output.truncated {
        format!("{text}\n\n[output truncated]")
    } else {
        text.to_string()
    }
}

/// Why a turn failed: the child's stderr, else its exit status.
fn failure_text(output: &RunOutput) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    match output.exit_code {
        Some(code) => format!("turn failed (exit {code})"),
        None => "turn failed".to_string(),
    }
}

/// Watched turns whose job is no longer running, in watch order.
fn collect_settled(
    jobs: &JobRegistry,
    registry: &VibeSessionRegistry,
    snapshots: &[(String, String)],
) -> Vec<VibeSettledTurn> {
    let cancelled = registry.cancelled_jobs.lock();
    snapshots
        .iter()
        .filter_map(|(id, job_id)| {
            let job = jobs.get(job_id)?;
            let status = if cancelled.contains(job_id) {
                "cancelled".to_string()
            } else {
                match job.status {
                    JobStatus::Running => return None,
                    JobStatus::Completed => "completed".to_string(),
                    JobStatus::Failed => "failed".to_string(),
                }
            };
            Some(VibeSettledTurn {
                id: id.clone(),
                job_id: job_id.clone(),
                status,
                result_text: job
                    .result
                    .or(job.error)
                    .unwrap_or_else(|| "(no output)".to_string()),
            })
        })
        .collect()
}

// ─── TV wall (omp `wallOf`) ──────────────────────────────────────────────

/// The wall plus the ids it hid. Named `ids` show exactly those sessions.
/// Otherwise sessions the director killed are left off the wall and reported
/// by id, so a director that kills finished workers does not re-render every
/// one on each status check. Workers that died on their own stay visible.
/// `keep` retains ids already on the wall (a wait never drops a card
/// mid-wait).
#[must_use]
pub fn wall(
    registry: &VibeSessionRegistry,
    scope: &VibeOwnerScope,
    ids: Option<&[String]>,
    keep: Option<&HashSet<String>>,
) -> (Vec<VibeScreenSnapshot>, Vec<String>) {
    let all = registry.screens(scope, ids);
    if ids.is_some_and(|ids| !ids.is_empty()) {
        return (all, Vec::new());
    }
    let mut screens = Vec::with_capacity(all.len());
    let mut hidden_killed = Vec::new();
    for screen in all {
        if screen.killed && !keep.is_some_and(|keep| keep.contains(&screen.id)) {
            hidden_killed.push(screen.id);
        } else {
            screens.push(screen);
        }
    }
    (screens, hidden_killed)
}

/// One roster line of `vibe_list`.
#[must_use]
pub fn roster_line(screen: &VibeScreenSnapshot) -> String {
    let mut parts = vec![
        format!(
            "- `{}` [{}] {}",
            screen.id,
            screen.cli.as_str(),
            screen.state.as_str()
        ),
        format!(
            "{} turn{}",
            screen.turns,
            if screen.turns == 1 { "" } else { "s" }
        ),
    ];
    if screen.queued > 0 {
        parts.push(format!("{} queued", screen.queued));
    }
    if let Some(model) = &screen.model {
        parts.push(model.clone());
    }
    if let Some(last) = &screen.last_activity {
        parts.push(format!("last: {last}"));
    }
    parts.join(" · ")
}

/// omp `VibeListTool`'s body: roster lines plus the folded killed tail.
#[must_use]
pub fn list_text(screens: &[VibeScreenSnapshot], hidden_killed: &[String]) -> String {
    let mut lines: Vec<String> = screens.iter().map(roster_line).collect();
    if lines.is_empty() {
        lines.push(if hidden_killed.is_empty() {
            "No vibe sessions. Spawn one with vibe_spawn.".to_string()
        } else {
            "No live vibe sessions.".to_string()
        });
    }
    if !hidden_killed.is_empty() {
        let start = hidden_killed.len().saturating_sub(KILLED_IDS_LISTED);
        let mut recent: Vec<String> = hidden_killed[start..]
            .iter()
            .map(|id| format!("`{id}`"))
            .collect();
        if start > 0 {
            recent.push(format!("+{start} more"));
        }
        lines.push(format!(
            "Killed ({} killed, transcripts at history://<id>): {}",
            hidden_killed.len(),
            recent.join(", ")
        ));
    }
    lines.join("\n")
}

/// omp `VibeSendTool`'s ack line.
#[must_use]
pub fn send_ack(outcome: &VibeSendOutcome) -> String {
    match outcome.mode {
        VibeSendMode::Turn => format!(
            "Started a new turn on `{}` (job `{}`). Its result will be delivered when the turn finishes.",
            outcome.id,
            outcome.job_id.as_deref().unwrap_or("?")
        ),
        VibeSendMode::Steered => format!(
            "Steered `{}` mid-turn — the running turn sees your message at its next step.",
            outcome.id
        ),
        VibeSendMode::Queued => format!(
            "`{}` is mid-turn; your message is queued and runs automatically as the next turn.",
            outcome.id
        ),
    }
}

/// omp `oneLineLabel`: collapse whitespace to single spaces and cap at `max`
/// chars with an ellipsis.
#[must_use]
pub fn one_line_label(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    truncate_chars(&flat, max)
}

/// Truncate on a char boundary, appending an ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Strictly increasing creation stamp: the wall clock, but never behind an
/// earlier record, so same-millisecond inserts keep their spawn order.
fn stamp(inner: &mut RegistryInner) -> u64 {
    let last = inner
        .records
        .values()
        .map(|record| record.created_at_ms)
        .max()
        .unwrap_or(0);
    inner.next_stamp = now_ms().max(inner.next_stamp).max(last + 1);
    inner.next_stamp
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis()),
    )
    .unwrap_or(0)
}

// ─── Tools ───────────────────────────────────────────────────────────────

fn text_result(text: String, details: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![pa_agent::types::ToolResultContent::text(text)],
        details,
        terminate: None,
        is_error: None,
    }
}

/// The `op`/`screens`/`hiddenKilled` details every vibe result carries.
fn base_details(op: &str, screens: &[VibeScreenSnapshot], hidden_killed: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("op".to_string(), json!(op));
    map.insert(
        "screens".to_string(),
        serde_json::to_value(screens).unwrap_or(Value::Null),
    );
    if !hidden_killed.is_empty() {
        map.insert(
            "hiddenKilled".to_string(),
            serde_json::to_value(hidden_killed).unwrap_or(Value::Null),
        );
    }
    Value::Object(map)
}

fn insert_detail(details: &mut Value, key: &str, value: Value) {
    if let Some(map) = details.as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

/// `vibe_spawn`: start a persistent fast/good worker session.
pub struct VibeSpawnTool {
    env: Arc<Env>,
    schema: Value,
}

impl VibeSpawnTool {
    #[must_use]
    pub fn new(env: Arc<Env>) -> Self {
        Self {
            env,
            schema: schema_spawn(),
        }
    }

    fn run(&self, params: Value) -> Result<AgentToolResult> {
        let cli = VibeCli::parse(
            params
                .get("cli")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if prompt.trim().is_empty() {
            bail!("prompt must not be empty.");
        }
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        if let Some(name) = &name {
            if name.chars().count() > 48 {
                bail!("name must be at most 48 characters.");
            }
        }
        let scope = VibeOwnerScope::for_env(&self.env);
        let registry = VibeSessionRegistry::global();
        let outcome = registry.spawn(
            &scope,
            &self.env.cwd,
            &self.env.jobs,
            SpawnArgs { cli, name, prompt },
        )?;
        let (screens, hidden) = wall(&registry, &scope, None, None);
        let mut details = base_details("spawn", &screens, &hidden);
        insert_detail(
            &mut details,
            "spawned",
            json!({"id": outcome.id.clone(), "cli": cli.as_str(), "jobId": outcome.job_id.clone()}),
        );
        Ok(text_result(
            format!(
                "Spawned {} session `{}` (turn job `{}`). The turn result will be delivered when it \
                 finishes — keep directing other sessions meanwhile. Continue this one with \
                 vibe_send `{}`.",
                cli.as_str(),
                outcome.id,
                outcome.job_id,
                outcome.id
            ),
            details,
        ))
    }
}

impl AgentTool for VibeSpawnTool {
    fn name(&self) -> &'static str {
        "vibe_spawn"
    }
    fn description(&self) -> &str {
        VIBE_SPAWN_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Vibe Spawn"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.run(params) })
    }
}

/// `vibe_send`: message a worker session (steer or next turn).
pub struct VibeSendTool {
    env: Arc<Env>,
    schema: Value,
}

impl VibeSendTool {
    #[must_use]
    pub fn new(env: Arc<Env>) -> Self {
        Self {
            env,
            schema: schema_send(),
        }
    }

    fn run(&self, params: Value) -> Result<AgentToolResult> {
        let session = params
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if session.trim().is_empty() {
            bail!("session must not be empty.");
        }
        if message.trim().is_empty() {
            bail!("Message must not be empty.");
        }
        let scope = VibeOwnerScope::for_env(&self.env);
        let registry = VibeSessionRegistry::global();
        let outcome = registry.send(&scope, &self.env.jobs, &session, &message)?;
        let (screens, hidden) = wall(&registry, &scope, None, None);
        let mut details = base_details("send", &screens, &hidden);
        insert_detail(
            &mut details,
            "send",
            serde_json::to_value(&outcome).unwrap_or(Value::Null),
        );
        Ok(text_result(send_ack(&outcome), details))
    }
}

impl AgentTool for VibeSendTool {
    fn name(&self) -> &'static str {
        "vibe_send"
    }
    fn description(&self) -> &str {
        VIBE_SEND_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Vibe Send"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.run(params) })
    }
}

/// `vibe_wait`: block until a worker session finishes its turn.
pub struct VibeWaitTool {
    env: Arc<Env>,
    schema: Value,
}

impl VibeWaitTool {
    #[must_use]
    pub fn new(env: Arc<Env>) -> Self {
        Self {
            env,
            schema: schema_wait(),
        }
    }

    fn details(
        registry: &VibeSessionRegistry,
        scope: &VibeOwnerScope,
        sessions: Option<&[String]>,
        keep: &HashSet<String>,
        wait: Option<Value>,
    ) -> Value {
        let (screens, hidden) = wall(registry, scope, sessions, Some(keep));
        let mut details = base_details("wait", &screens, &hidden);
        if let Some(wait) = wait {
            insert_detail(&mut details, "wait", wait);
        }
        details
    }

    async fn run(
        &self,
        params: Value,
        signal: AbortSignal,
        on_update: AgentToolUpdateCallback,
    ) -> Result<AgentToolResult> {
        let sessions: Option<Vec<String>> = params
            .get("sessions")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<String>>()
            })
            .filter(|ids| !ids.is_empty());
        let timeout = params
            .get("timeout")
            .and_then(Value::as_f64)
            .filter(|seconds| *seconds > 0.0)
            .map(Duration::from_secs_f64);
        let scope = VibeOwnerScope::for_env(&self.env);
        let registry = VibeSessionRegistry::global();
        // Cards on the wall when the wait starts stay for its duration, so a
        // worker killed mid-wait cannot vanish from the frame that settles it.
        let (initial, _) = wall(&registry, &scope, sessions.as_deref(), None);
        let mut keep: HashSet<String> = initial.iter().map(|screen| screen.id.clone()).collect();
        let mut last_frame: Option<Instant> = None;
        let mut on_tick = || {
            let now = Instant::now();
            if last_frame.is_some_and(|at| {
                now.duration_since(at) < Duration::from_millis(WAIT_PROGRESS_INTERVAL_MS)
            }) {
                return;
            }
            last_frame = Some(now);
            on_update(AgentToolResult {
                content: vec![pa_agent::types::ToolResultContent::text("")],
                details: Self::details(
                    &registry,
                    &scope,
                    sessions.as_deref(),
                    &keep,
                    Some(json!({
                        "settled": [],
                        "stillRunning": [],
                        "timedOut": false,
                        "waiting": true
                    })),
                ),
                terminate: None,
                is_error: None,
            });
        };
        let outcome = registry
            .wait(
                &scope,
                &self.env.jobs,
                sessions.as_deref(),
                timeout,
                &signal,
                &mut on_tick,
            )
            .await?;
        drop(on_tick);
        // A worker that died as its turn settled keeps its card for the footer.
        for entry in &outcome.settled {
            keep.insert(entry.id.clone());
        }
        let details = Self::details(
            &registry,
            &scope,
            sessions.as_deref(),
            &keep,
            Some(json!({
                "settled": outcome.settled.iter()
                    .map(|entry| json!({"id": entry.id, "jobId": entry.job_id, "status": entry.status}))
                    .collect::<Vec<_>>(),
                "stillRunning": outcome.still_running,
                "timedOut": outcome.timed_out,
            })),
        );
        if outcome.settled.is_empty() && outcome.still_running.is_empty() {
            return Ok(text_result(
                "No turns in flight to wait for.".to_string(),
                details,
            ));
        }
        let mut lines: Vec<String> = Vec::new();
        for entry in &outcome.settled {
            lines.push(format!("## `{}` — {}", entry.id, entry.status));
            lines.push(entry.result_text.clone());
            lines.push(String::new());
        }
        if !outcome.still_running.is_empty() {
            lines.push(format!(
                "Still running: {}.",
                outcome
                    .still_running
                    .iter()
                    .map(|id| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if outcome.timed_out {
            lines.push(
                "Wait window elapsed before any turn settled — re-issue vibe_wait to keep waiting."
                    .to_string(),
            );
        }
        Ok(text_result(
            lines.join("\n").trim_end().to_string(),
            details,
        ))
    }
}

impl AgentTool for VibeWaitTool {
    fn name(&self) -> &'static str {
        "vibe_wait"
    }
    fn description(&self) -> &str {
        VIBE_WAIT_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Vibe Wait"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.run(params, signal, on_update).await })
    }
}

/// `vibe_kill`: terminate a worker session.
pub struct VibeKillTool {
    env: Arc<Env>,
    schema: Value,
}

impl VibeKillTool {
    #[must_use]
    pub fn new(env: Arc<Env>) -> Self {
        Self {
            env,
            schema: schema_kill(),
        }
    }

    fn run(&self, params: Value) -> Result<AgentToolResult> {
        let session = params
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if session.trim().is_empty() {
            bail!("session must not be empty.");
        }
        let scope = VibeOwnerScope::for_env(&self.env);
        let registry = VibeSessionRegistry::global();
        let outcome = registry.kill(&scope, &self.env.jobs, &session)?;
        let (screens, hidden) = wall(&registry, &scope, None, None);
        let mut details = base_details("kill", &screens, &hidden);
        insert_detail(
            &mut details,
            "killed",
            serde_json::to_value(&outcome).unwrap_or(Value::Null),
        );
        let cancel_note = if outcome.cancelled_turn {
            " Its in-flight turn was cancelled."
        } else {
            ""
        };
        Ok(text_result(
            format!(
                "Killed session `{}`.{} Transcript remains at history://{}.",
                outcome.id, cancel_note, outcome.id
            ),
            details,
        ))
    }
}

impl AgentTool for VibeKillTool {
    fn name(&self) -> &'static str {
        "vibe_kill"
    }
    fn description(&self) -> &str {
        VIBE_KILL_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Vibe Kill"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.run(params) })
    }
}

/// `vibe_list`: list worker sessions and their states.
pub struct VibeListTool {
    env: Arc<Env>,
    schema: Value,
}

impl VibeListTool {
    #[must_use]
    pub fn new(env: Arc<Env>) -> Self {
        Self {
            env,
            schema: schema_list(),
        }
    }

    fn run(&self) -> AgentToolResult {
        let scope = VibeOwnerScope::for_env(&self.env);
        let registry = VibeSessionRegistry::global();
        let (screens, hidden) = wall(&registry, &scope, None, None);
        let details = base_details("list", &screens, &hidden);
        text_result(list_text(&screens, &hidden), details)
    }
}

impl AgentTool for VibeListTool {
    fn name(&self) -> &'static str {
        "vibe_list"
    }
    fn description(&self) -> &str {
        VIBE_LIST_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Vibe List"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        _params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { Ok(self.run()) })
    }
}

/// The ephemeral tools installed while `/vibe` mode is active (omp
/// `createVibeTools`), in omp's order.
#[must_use]
pub fn create_vibe_tools(env: Arc<Env>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(VibeSpawnTool::new(Arc::clone(&env))),
        Arc::new(VibeSendTool::new(Arc::clone(&env))),
        Arc::new(VibeWaitTool::new(Arc::clone(&env))),
        Arc::new(VibeKillTool::new(Arc::clone(&env))),
        Arc::new(VibeListTool::new(env)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> VibeOwnerScope {
        VibeOwnerScope::new("owner", "session")
    }

    fn registry() -> VibeSessionRegistry {
        VibeSessionRegistry::with_runner(TaskRunner::new("vibe-test-child", 1))
    }

    #[test]
    fn tool_names_match_omp() {
        assert_eq!(
            VIBE_TOOL_NAMES,
            [
                "vibe_spawn",
                "vibe_send",
                "vibe_wait",
                "vibe_kill",
                "vibe_list"
            ]
        );
    }

    #[test]
    fn spawn_schema_matches_omp() {
        let schema = schema_spawn();
        assert_eq!(schema["required"], json!(["cli", "prompt"]));
        let props = &schema["properties"];
        assert_eq!(props["cli"]["enum"], json!(["fast", "good"]));
        assert_eq!(props["name"]["maxLength"], json!(48));
        assert_eq!(props["prompt"]["minLength"], json!(1));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn send_wait_kill_list_schemas_match_omp() {
        let send = schema_send();
        assert_eq!(send["required"], json!(["session", "message"]));
        assert_eq!(send["properties"]["message"]["minLength"], json!(1));

        let wait = schema_wait();
        assert_eq!(wait["properties"]["timeout"]["exclusiveMinimum"], json!(0));
        assert_eq!(
            wait["properties"]["sessions"]["items"],
            json!({"type": "string"})
        );
        assert!(wait.get("required").is_none(), "{wait}");

        let kill = schema_kill();
        assert_eq!(kill["required"], json!(["session"]));

        assert_eq!(schema_list(), json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn descriptions_match_omp_prompts() {
        assert_eq!(
            VIBE_SPAWN_DESCRIPTION.lines().next(),
            Some("Starts persistent conversational coding-agent worker session (edit, bash, grep, everything).")
        );
        assert!(VIBE_SPAWN_DESCRIPTION.contains("- `fast`: low-latency model"));
        assert!(VIBE_SEND_DESCRIPTION.contains("- `steered`: worker mid-turn"));
        assert!(VIBE_WAIT_DESCRIPTION.contains("not until all finish"));
        assert!(VIBE_KILL_DESCRIPTION.contains("history://<id>"));
        assert!(VIBE_LIST_DESCRIPTION.contains("Sessions you killed collapse"));
        for text in [
            VIBE_SPAWN_DESCRIPTION,
            VIBE_SEND_DESCRIPTION,
            VIBE_WAIT_DESCRIPTION,
            VIBE_KILL_DESCRIPTION,
            VIBE_LIST_DESCRIPTION,
        ] {
            assert_eq!(text, text.trim(), "no leading/trailing blank lines");
        }
    }

    #[test]
    fn cli_flavors_parse_and_map_to_bundled_agents() {
        assert_eq!(VibeCli::parse("fast").unwrap(), VibeCli::Fast);
        assert_eq!(VibeCli::parse(" good ").unwrap(), VibeCli::Good);
        assert_eq!(VibeCli::Fast.agent(), "sonic");
        assert_eq!(VibeCli::Good.agent(), "task");
        let error = VibeCli::parse("huge").unwrap_err().to_string();
        assert!(error.contains("expected \"fast\" or \"good\""), "{error}");
    }

    #[test]
    fn session_names_are_sanitized_like_omp() {
        assert_eq!(
            sanitize_session_name("  Parser Cleanup! ").as_deref(),
            Some("parser-cleanup")
        );
        assert_eq!(sanitize_session_name("a__b").as_deref(), Some("a__b"));
        assert_eq!(sanitize_session_name("!!!"), None);
        assert_eq!(sanitize_session_name(""), None);
        let long = "x".repeat(80);
        assert_eq!(sanitize_session_name(&long).unwrap().chars().count(), 48);
    }

    #[test]
    fn wall_hides_killed_sessions_unless_kept_or_named() {
        let registry = registry();
        let scope = scope();
        registry.register_record_for_tests(
            &scope,
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Idle,
            false,
            None,
        );
        registry.register_record_for_tests(
            &scope,
            "beta",
            VibeCli::Good,
            VibeSessionState::Dead,
            true,
            None,
        );

        let (screens, hidden) = wall(&registry, &scope, None, None);
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].id, "alpha");
        assert_eq!(hidden, vec!["beta".to_string()]);

        // A card already on the wall stays for the duration of a wait.
        let keep: HashSet<String> = ["beta".to_string()].into_iter().collect();
        let (screens, hidden) = wall(&registry, &scope, None, Some(&keep));
        assert_eq!(screens.len(), 2);
        assert!(hidden.is_empty());

        // Named ids show exactly those sessions, killed included.
        let named = ["beta".to_string()];
        let (screens, hidden) = wall(&registry, &scope, Some(&named), None);
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].id, "beta");
        assert!(hidden.is_empty());
    }

    #[test]
    fn screens_follow_spawn_order() {
        let registry = registry();
        let scope = scope();
        registry.register_record_for_tests(
            &scope,
            "first",
            VibeCli::Fast,
            VibeSessionState::Idle,
            false,
            None,
        );
        registry.register_record_for_tests(
            &scope,
            "second",
            VibeCli::Good,
            VibeSessionState::Idle,
            false,
            None,
        );
        let ids: Vec<String> = registry
            .screens(&scope, None)
            .into_iter()
            .map(|screen| screen.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "first");
        assert_eq!(registry.list_ids(&scope).len(), 2);
    }

    #[test]
    fn scopes_do_not_leak_between_sessions() {
        let registry = registry();
        registry.register_record_for_tests(
            &scope(),
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Idle,
            false,
            None,
        );
        let other = VibeOwnerScope::new("owner", "other-session");
        assert!(registry.screens(&other, None).is_empty());
        assert!(registry.list_ids(&other).is_empty());
        let error = registry
            .kill(&other, &JobRegistry::default(), "alpha")
            .unwrap_err()
            .to_string();
        assert!(error.contains("No sessions"), "{error}");
    }

    #[test]
    fn list_text_reports_roster_and_folds_killed_ids() {
        let screen = VibeScreenSnapshot {
            id: "alpha".to_string(),
            cli: VibeCli::Fast,
            state: VibeSessionState::Running,
            killed: false,
            model: Some("zai/glm-5.3-flash".to_string()),
            turns: 2,
            queued: 1,
            turn_started_at: Some(1),
            turn_message: Some("do it".to_string()),
            current_tool: None,
            current_tool_args: None,
            last_intent: None,
            trace: Vec::new(),
            output_tail: Vec::new(),
            last_activity: Some("running tests".to_string()),
            last_activity_at: 2,
        };
        assert_eq!(
            roster_line(&screen),
            "- `alpha` [fast] running · 2 turns · 1 queued · zai/glm-5.3-flash · last: running tests"
        );
        let mut single = screen.clone();
        single.turns = 1;
        single.queued = 0;
        single.model = None;
        single.last_activity = None;
        assert_eq!(roster_line(&single), "- `alpha` [fast] running · 1 turn");

        assert_eq!(
            list_text(&[], &[]),
            "No vibe sessions. Spawn one with vibe_spawn."
        );
        // The header alone does not swallow the folded killed tail (omp
        // appends it after the empty-roster line).
        assert_eq!(
            list_text(&[], &["dead".to_string()]),
            "No live vibe sessions.\nKilled (1 killed, transcripts at history://<id>): `dead`"
        );

        let killed: Vec<String> = (0..10).map(|n| format!("k{n}")).collect();
        let text = list_text(&[screen], &killed);
        assert!(text.contains("Killed (10 killed"), "{text}");
        assert!(text.contains("`k2`"), "newest ids listed: {text}");
        assert!(!text.contains("`k1`"), "older ids folded: {text}");
        assert!(text.contains("+2 more"), "{text}");
    }

    #[test]
    fn send_ack_covers_every_mode() {
        let turn = VibeSendOutcome {
            id: "alpha".to_string(),
            mode: VibeSendMode::Turn,
            job_id: Some("bg_3".to_string()),
        };
        assert_eq!(
            send_ack(&turn),
            "Started a new turn on `alpha` (job `bg_3`). Its result will be delivered when the turn finishes."
        );
        let steered = VibeSendOutcome {
            id: "alpha".to_string(),
            mode: VibeSendMode::Steered,
            job_id: None,
        };
        assert!(send_ack(&steered).contains("Steered `alpha` mid-turn"));
        let queued = VibeSendOutcome {
            id: "alpha".to_string(),
            mode: VibeSendMode::Queued,
            job_id: None,
        };
        assert!(send_ack(&queued).contains("queued and runs automatically"));
    }

    #[test]
    fn unknown_session_error_names_the_roster() {
        let bare = unknown_session_error("ghost", &[]);
        assert!(bare.contains("Unknown vibe session \"ghost\"."), "{bare}");
        assert!(bare.contains("spawn one with vibe_spawn"), "{bare}");
        let with_roster =
            unknown_session_error("ghost", &["alpha".to_string(), "beta".to_string()]);
        assert!(
            with_roster.contains("Active sessions: alpha, beta"),
            "{with_roster}"
        );
    }

    #[test]
    fn send_to_dead_session_errors_before_scheduling() {
        let registry = Arc::new(registry());
        let scope = scope();
        registry.register_record_for_tests(
            &scope,
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Dead,
            true,
            None,
        );
        let error = registry
            .send(&scope, &JobRegistry::default(), "alpha", "more")
            .unwrap_err()
            .to_string();
        assert!(error.contains("is dead"), "{error}");
        let empty = registry
            .send(&scope, &JobRegistry::default(), "alpha", "   ")
            .unwrap_err()
            .to_string();
        assert!(empty.contains("must not be empty"), "{empty}");
    }

    #[test]
    fn send_queues_mid_turn_messages() {
        let registry = Arc::new(registry());
        let scope = scope();
        registry.register_record_for_tests(
            &scope,
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Running,
            false,
            Some("bg_1"),
        );
        let outcome = registry
            .send(
                &scope,
                &JobRegistry::default(),
                "alpha",
                "also rename the tests",
            )
            .unwrap();
        assert_eq!(outcome.mode, VibeSendMode::Queued);
        assert_eq!(outcome.job_id, None);
        let screen = &registry.screens(&scope, None)[0];
        assert_eq!(screen.queued, 1);
        assert_eq!(screen.state, VibeSessionState::Running);
    }

    /// Attach a real registry job to a test record so `kill`/`collect_settled`
    /// observe the same job id the registry holds.
    fn attach_job(
        registry: &VibeSessionRegistry,
        scope: &VibeOwnerScope,
        id: &str,
        jobs: &JobRegistry,
    ) -> String {
        let job_id = jobs.insert(Job {
            command: format!("vibe fast {id}"),
            started: Instant::now(),
            status: JobStatus::Running,
            output: Arc::new(StdMutex::new(String::new())),
            result: None,
            error: None,
            ended: None,
        });
        let mut inner = registry.inner.lock();
        let record = inner
            .records
            .get_mut(&VibeSessionRegistry::key(scope, id))
            .expect("record");
        record.turn = Some(VibeTurn {
            job_id: job_id.clone(),
            message: "work".to_string(),
            started_at_ms: now_ms(),
            trace: Vec::new(),
            tool_count: 0,
        });
        job_id
    }

    #[test]
    fn kill_marks_dead_and_cancels_a_running_turn() {
        let registry = registry();
        let scope = scope();
        let jobs = JobRegistry::default();
        registry.register_record_for_tests(
            &scope,
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Running,
            false,
            Some("bg_7"),
        );
        let job_id = attach_job(&registry, &scope, "alpha", &jobs);
        let outcome = registry.kill(&scope, &jobs, "alpha").unwrap();
        assert_eq!(outcome.id, "alpha");
        assert!(outcome.cancelled_turn);
        assert!(jobs.get(&job_id).unwrap().status == JobStatus::Failed);
        assert_eq!(
            jobs.get(&job_id).unwrap().error.as_deref(),
            Some("turn cancelled")
        );
        let screen = &registry.screens(&scope, None)[0];
        assert_eq!(screen.state, VibeSessionState::Dead);
        assert!(screen.killed);
        assert!(registry.list_ids(&scope).is_empty());
    }

    #[test]
    fn killed_ids_report_cancelled_status() {
        let registry = registry();
        let scope = scope();
        let jobs = JobRegistry::default();
        registry.register_record_for_tests(
            &scope,
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Running,
            false,
            Some("bg_9"),
        );
        let job_id = attach_job(&registry, &scope, "alpha", &jobs);
        registry.kill(&scope, &jobs, "alpha").unwrap();
        let settled = collect_settled(&jobs, &registry, &[("alpha".to_string(), job_id)]);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].status, "cancelled");
        assert_eq!(settled[0].result_text, "turn cancelled");
    }

    #[test]
    fn one_line_labels_collapse_whitespace_and_cap() {
        assert_eq!(one_line_label("  fix\n\tthe   bug ", 80), "fix the bug");
        let long = "word ".repeat(40);
        let label = one_line_label(&long, 20);
        assert_eq!(label.chars().count(), 20);
        assert!(label.ends_with('…'));
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("0123456789", 5).chars().count(), 5);
    }

    #[test]
    fn follow_up_prompts_carry_the_prior_turns() {
        let registry = registry();
        let scope = scope();
        registry.register_record_for_tests(
            &scope,
            "alpha",
            VibeCli::Fast,
            VibeSessionState::Idle,
            false,
            None,
        );
        let record = {
            let mut inner = registry.inner.lock();
            let record = inner
                .records
                .get_mut(&VibeSessionRegistry::key(&scope, "alpha"))
                .expect("record");
            record.history.push((
                "rename parse to decode".to_string(),
                "renamed in 4 files".to_string(),
            ));
            record.clone()
        };
        let prompt = compose_prompt(&record, "now the tests");
        assert!(prompt.contains("### Turn 1 — instruction"), "{prompt}");
        assert!(prompt.contains("rename parse to decode"), "{prompt}");
        assert!(prompt.contains("renamed in 4 files"), "{prompt}");
        assert!(
            prompt.ends_with("## Next instruction\n\nnow the tests"),
            "{prompt}"
        );

        let fresh = VibeRecord {
            history: Vec::new(),
            ..record
        };
        assert_eq!(compose_prompt(&fresh, "first"), "first");
    }

    #[test]
    fn turn_specs_use_the_flavor_agent() {
        let spec = turn_spec(
            "alpha",
            VibeCli::Fast,
            Path::new("/tmp"),
            "work".to_string(),
        )
        .unwrap();
        assert_eq!(spec.agent, "sonic");
        assert_eq!(spec.prompt, "work");
        assert_eq!(spec.cwd, PathBuf::from("/tmp"));
        assert!(spec.tools.is_none());
        let good = turn_spec("beta", VibeCli::Good, Path::new("/tmp"), "work".to_string()).unwrap();
        assert_eq!(good.agent, "task");
    }

    #[test]
    fn generated_ids_avoid_collisions() {
        let registry = registry();
        let scope = scope();
        registry.register_record_for_tests(
            &scope,
            "dup",
            VibeCli::Fast,
            VibeSessionState::Idle,
            false,
            None,
        );
        let mut inner = registry.inner.lock();
        assert_eq!(
            allocate_id(&mut inner, &scope, VibeCli::Fast, Some("dup")),
            "dup-2"
        );
        assert_eq!(
            allocate_id(&mut inner, &scope, VibeCli::Fast, None),
            "vibe-fast-1"
        );
    }
}
