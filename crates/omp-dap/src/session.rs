//! The DAP session manager: session tree, breakpoint bookkeeping, stop-outcome
//! waiters, a bounded output ring and idle reaping.
//!
//! One [`DapSessionManager`] owns every live debug session of a process. Sessions
//! are addressed by id; the manager keeps an `active_session_id` that the
//! tool-facing actions use when the caller does not name a session explicitly.
//! `startDebugging` reverse requests create child sessions whose parent keeps a
//! link, so a tree of adapters (e.g. a js-debug style launcher) stays navigable.
//!
//! The manager is transport-agnostic beyond [`DapClient`]: every request goes
//! through [`DapClient::send_request`], and the stop/exit/terminated events that
//! decide a `continue`/step outcome are observed through per-session
//! subscriptions that are installed *before* the resuming request is sent. That
//! ordering matters: an adapter may emit `stopped` in the same read chunk as the
//! `continue` response, so a waiter registered afterwards would miss it and the
//! call would sit until its budget expired.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::sync::Notify;

use crate::client::{DapClient, DapSpawnOptions};
use crate::types::{
    DapAccessType, DapAttachSessionOptions, DapBreakpoint, DapBreakpointRecord, DapBreakpointMutation,
    DapCapabilities, DapCapability, DapContinueOutcome, DapDataBreakpoint, DapDataBreakpointInfoResponse,
    DapDataBreakpointRecord, DapDisassembledInstruction, DapEvaluateResponse, DapFunctionBreakpoint,
    DapFunctionBreakpointRecord, DapInstructionBreakpoint, DapInstructionBreakpointRecord,
    DapLaunchSessionOptions, DapModule, DapOutputSnapshot, DapReadMemoryResponse, DapResolvedAdapter,
    DapRunState, DapScope, DapSessionStatus, DapSessionSummary, DapSource, DapSourceBreakpoint,
    DapStackFrame, DapStopLocation, DapThread, DapTypedBreakpointMutation, DapVariable,
    DapWriteMemoryResponse,
};
use crate::{Cancel, DapError, DapResult};

/// Bytes of adapter `output` kept per session before the oldest bytes are dropped.
pub const OUTPUT_RING_CAPACITY: usize = 128 * 1024;

/// How long a `continue`/step waits for its stop before reporting `running`.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the `configurationDone` handshake waits for the first stop.
pub const DEFAULT_CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an adapter may sit untouched before the reaper disconnects it.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Budget for the bookkeeping requests the manager issues on its own behalf.
const BOOKKEEPING_TIMEOUT: Duration = Duration::from_secs(10);

/// Wall-clock stamp used for `launchedAt`/`lastUsedAt`.
fn timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format_unix_millis(millis)
}

/// Render epoch milliseconds as an RFC 3339 UTC stamp without pulling in `chrono`.
///
/// The manager only ever needs a monotonic, sortable, human-readable stamp, so a
/// hand-rolled civil-date conversion is enough and keeps the dependency set flat.
#[must_use]
pub fn format_unix_millis(millis: u128) -> String {
    let secs = u64::try_from(millis / 1000).unwrap_or(u64::MAX);
    let millis_part = u32::try_from(millis % 1000).unwrap_or_default();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's `civil_from_days`.
    let z = i64::from_ne_bytes(i64::to_ne_bytes(i64::try_from(days).unwrap_or(0))) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis_part:03}Z")
}

/// Milliseconds since the Unix epoch, for idle accounting.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

// =============================================================================
// Output ring
// =============================================================================

/// A byte ring holding the tail of a session's `output` events.
///
/// The ring never grows past [`OUTPUT_RING_CAPACITY`]: once full, the oldest
/// bytes are dropped and `truncated` latches so the model can tell that what it
/// reads is the tail and not the whole stream.
#[derive(Debug)]
pub struct OutputRing {
    buffer: Vec<u8>,
    start: usize,
    truncated: bool,
}

impl Default for OutputRing {
    fn default() -> Self {
        Self::with_capacity(OUTPUT_RING_CAPACITY)
    }
}

impl OutputRing {
    /// Build a ring with an explicit capacity (tests use small ones).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
            start: 0,
            truncated: false,
        }
    }

    /// Append `bytes`, dropping the oldest bytes when the capacity is exceeded.
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.buffer.extend_from_slice(bytes);
        let capacity = OUTPUT_RING_CAPACITY;
        if self.buffer.len() > capacity {
            let excess = self.buffer.len() - capacity;
            self.buffer.drain(..excess);
            self.truncated = true;
        }
        self.start = 0;
    }

    /// Bytes currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the ring holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Whether any bytes were dropped.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The held bytes as lossy UTF-8.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.buffer[self.start..]).into_owned()
    }

    /// Clear the ring (used by `output` with `clear`).
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.start = 0;
        self.truncated = false;
    }
}

// =============================================================================
// Stop outcome plumbing
// =============================================================================

/// A waiter installed before a resume request, resolved by the event handler.
#[derive(Debug)]
struct StopWaiter {
    /// Settled once; later events for the same resume are ignored.
    resolved: AtomicBool,
    /// The first `stopped`/`exited`/`terminated` body seen, if any.
    outcome: Mutex<Option<StopSignal>>,
    notify: Notify,
}

/// What a resume produced.
#[derive(Debug, Clone)]
struct StopSignal {
    /// `stopped`, `exited` or `terminated`.
    event: String,
    body: Option<Value>,
}

impl StopWaiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            resolved: AtomicBool::new(false),
            outcome: Mutex::new(None),
            notify: Notify::new(),
        })
    }

    /// Record `signal` if this waiter has not settled yet.
    fn settle(&self, signal: StopSignal) {
        if self.resolved.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.outcome.lock() = Some(signal);
        self.notify.notify_waiters();
    }

    fn is_resolved(&self) -> bool {
        self.resolved.load(Ordering::SeqCst)
    }

    fn take(&self) -> Option<StopSignal> {
        self.outcome.lock().clone()
    }
}

/// A prepared stop outcome: subscribe, then send, then [`Self::await_outcome`].
///
/// The two-phase shape exists so the caller can install the waiter *before* the
/// resuming request leaves the process. See the module docs for why.
#[derive(Debug)]
pub struct PreparedStopOutcome {
    session_id: String,
    waiter: Arc<StopWaiter>,
    started: Instant,
    timeout: Duration,
}

impl PreparedStopOutcome {
    /// Session the outcome belongs to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// How long this outcome has been waiting.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Wait for the resume to settle, mapping it onto a [`DapContinueOutcome`].
    ///
    /// `cancel` aborts the wait; the budget is the one the outcome was prepared
    /// with. A timeout is not an error: the target is simply still running, and
    /// the returned outcome says so with `timed_out: true`.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Aborted`] when the caller cancelled and
    /// [`DapError::NoSession`] when the session disappeared meanwhile.
    pub async fn await_outcome(
        &self,
        manager: &DapSessionManager,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        let state = manager
            .session_state(&self.session_id)
            .ok_or(DapError::NoSession)?;

        let deadline = Instant::now() + self.timeout;
        loop {
            if cancel.is_some_and(Cancel::is_cancelled) {
                return Err(DapError::Aborted);
            }
            if self.waiter.is_resolved() {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let notified = self.waiter.notify.notified();
            // Re-check between arming and awaiting: `settle` may have run while
            // the future was being constructed.
            if self.waiter.is_resolved() {
                break;
            }
            let cancel_future = cancel_future(cancel);
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(remaining) => break,
                () = cancel_future => return Err(DapError::Aborted),
            }
        }

        let signal = self.waiter.take();
        let timed_out = signal.is_none();
        if let Some(signal) = signal {
            state.apply_stop_signal(&signal.event, signal.body.as_ref());
        }
        let run_state = match state.status() {
            DapSessionStatus::Running if timed_out => DapRunState::Running,
            DapSessionStatus::Terminated => DapRunState::Terminated,
            DapSessionStatus::Running => DapRunState::Running,
            _ if timed_out => DapRunState::Running,
            _ => DapRunState::Stopped,
        };
        if timed_out {
            state.set_status(DapSessionStatus::Running);
        }
        let snapshot = manager.build_summary(&state);
        Ok(DapContinueOutcome {
            snapshot,
            state: run_state,
            timed_out,
        })
    }
}

/// Await a cancel handle, or never when there is none.
async fn cancel_future(cancel: Option<&dyn Cancel>) {
    match cancel {
        Some(cancel) => cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

// =============================================================================
// Session state
// =============================================================================

/// Breakpoints a session holds, keyed by absolute source path.
#[derive(Debug, Default)]
struct BreakpointTable {
    sources: HashMap<String, Vec<DapBreakpointRecord>>,
    functions: Vec<DapFunctionBreakpointRecord>,
    instructions: Vec<DapInstructionBreakpointRecord>,
    data: Vec<DapDataBreakpointRecord>,
}

impl BreakpointTable {
    fn source_count(&self) -> usize {
        self.sources.values().map(Vec::len).sum()
    }
}

/// Everything the manager knows about one live session.
///
/// All mutable fields sit behind one lock so a snapshot is internally
/// consistent; the client is separate because it is itself thread-safe and
/// awaits requests while holding no lock.
pub struct DapSessionState {
    id: String,
    adapter: String,
    cwd: String,
    program: Option<String>,
    client: Arc<DapClient>,
    launched_at: String,
    parent_session_id: Option<String>,
    children: Mutex<Vec<String>>,
    inner: Mutex<SessionInner>,
    waiter: Mutex<Option<Arc<StopWaiter>>>,
}

/// The lock-protected half of a session.
#[derive(Debug, Default)]
struct SessionInner {
    status: DapSessionStatus,
    last_used_millis: u64,
    location: DapStopLocation,
    breakpoints: BreakpointTable,
    output: OutputRing,
    exit_code: Option<i64>,
    needs_configuration_done: bool,
    configuration_done_sent: bool,
    capabilities: DapCapabilities,
}

impl Default for DapSessionStatus {
    fn default() -> Self {
        Self::Launching
    }
}

impl std::fmt::Debug for DapSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DapSessionState")
            .field("id", &self.id)
            .field("adapter", &self.adapter)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl DapSessionState {
    fn new(
        id: String,
        client: Arc<DapClient>,
        cwd: String,
        program: Option<String>,
        parent_session_id: Option<String>,
    ) -> Self {
        let now = now_millis();
        Self {
            id,
            adapter: client.adapter_name().to_string(),
            cwd,
            program,
            client,
            launched_at: timestamp(),
            parent_session_id,
            children: Mutex::new(Vec::new()),
            inner: Mutex::new(SessionInner {
                status: DapSessionStatus::Launching,
                last_used_millis: now,
                ..SessionInner::default()
            }),
            waiter: Mutex::new(None),
        }
    }

    /// Session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Adapter name.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// The live client (for callers that need a capability probe of their own).
    #[must_use]
    pub fn client(&self) -> &Arc<DapClient> {
        &self.client
    }

    /// Current lifecycle status.
    #[must_use]
    pub fn status(&self) -> DapSessionStatus {
        self.inner.lock().status
    }

    fn set_status(&self, status: DapSessionStatus) {
        self.inner.lock().status = status;
    }

    fn touch(&self) {
        self.inner.lock().last_used_millis = now_millis();
    }

    fn last_used_millis(&self) -> u64 {
        self.inner.lock().last_used_millis
    }

    fn capabilities(&self) -> DapCapabilities {
        self.inner.lock().capabilities.clone()
    }

    fn set_capabilities(&self, capabilities: DapCapabilities) {
        self.inner.lock().capabilities = capabilities;
    }

    /// Whether the session still accepts requests.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.client.is_alive() && self.status() != DapSessionStatus::Terminated
    }

    /// Append adapter output to the ring.
    fn push_output(&self, bytes: &[u8]) {
        self.inner.lock().output.push(bytes);
    }

    /// The captured output of this session.
    #[must_use]
    pub fn output(&self) -> String {
        self.inner.lock().output.to_string_lossy()
    }

    fn set_exit_code(&self, code: Option<i64>) {
        self.inner.lock().exit_code = code;
    }

    fn location(&self) -> DapStopLocation {
        self.inner.lock().location.clone()
    }

    fn set_location(&self, location: DapStopLocation) {
        self.inner.lock().location = location;
    }

    fn clear_location(&self) {
        self.inner.lock().location = DapStopLocation::default();
    }

    fn configuration_done_sent(&self) -> bool {
        self.inner.lock().configuration_done_sent
    }

    fn mark_configuration_done(&self) {
        let mut inner = self.inner.lock();
        inner.configuration_done_sent = true;
        inner.needs_configuration_done = false;
    }

    fn children(&self) -> Vec<String> {
        self.children.lock().clone()
    }

    fn add_child(&self, child: String) {
        let mut children = self.children.lock();
        if !children.contains(&child) {
            children.push(child);
        }
    }

    fn remove_child(&self, child: &str) {
        self.children.lock().retain(|id| id != child);
    }

    /// Record a resume waiter so events can settle it.
    fn install_waiter(&self, waiter: Arc<StopWaiter>) {
        *self.waiter.lock() = Some(waiter);
    }

    fn waiter(&self) -> Option<Arc<StopWaiter>> {
        self.waiter.lock().clone()
    }

    /// Fold one `stopped`/`exited`/`terminated` event into the session state.
    fn apply_stop_signal(&self, event: &str, body: Option<&Value>) {
        match event {
            "stopped" => {
                let parsed: crate::types::DapStoppedEventBody = body
                    .and_then(|b| serde_json::from_value(b.clone()).ok())
                    .unwrap_or_default();
                let mut location = DapStopLocation {
                    thread_id: parsed.thread_id,
                    reason: Some(parsed.reason.clone()),
                    description: parsed.description.clone(),
                    text: parsed.text.clone(),
                    ..DapStopLocation::default()
                };
                if location.thread_id.is_none() {
                    location.thread_id = self.location().thread_id;
                }
                self.set_location(location);
                self.set_status(DapSessionStatus::Stopped);
            }
            "continued" => {
                self.clear_location();
                self.set_status(DapSessionStatus::Running);
            }
            "exited" => {
                let parsed: crate::types::DapExitedEventBody = body
                    .and_then(|b| serde_json::from_value(b.clone()).ok())
                    .unwrap_or_default();
                if let Some(code) = parsed.exit_code {
                    self.set_exit_code(Some(code));
                }
                self.set_status(DapSessionStatus::Terminated);
            }
            "terminated" => {
                self.set_status(DapSessionStatus::Terminated);
            }
            _ => {}
        }
    }

    /// Whether the adapter advertised `capability`.
    fn require_capability(&self, capability: DapCapability) -> DapResult<()> {
        if self.capabilities().supports(capability) {
            return Ok(());
        }
        Err(DapError::Unsupported(format!(
            "The active debug adapter does not support {}.",
            capability.description()
        )))
    }
}

// =============================================================================
// Manager
// =============================================================================

/// The process-wide session registry.
pub struct DapSessionManager {
    sessions: Mutex<HashMap<String, Arc<DapSessionState>>>,
    order: Mutex<Vec<String>>,
    active_session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    stop_timeout: Duration,
    configuration_timeout: Duration,
    idle_timeout: Duration,
}

/// The manager every tool action uses.
pub static SHARED: LazyLock<DapSessionManager> = LazyLock::new(DapSessionManager::new);

impl Default for DapSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DapSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DapSessionManager")
            .field("sessions", &self.sessions.lock().len())
            .field("active", &*self.active_session_id.lock())
            .finish_non_exhaustive()
    }
}

impl DapSessionManager {
    /// A manager with the default budgets.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            active_session_id: Mutex::new(None),
            next_id: AtomicU64::new(0),
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            configuration_timeout: DEFAULT_CONFIGURATION_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// Override the stop budget (tests use short budgets).
    #[must_use]
    pub fn with_stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = timeout;
        self
    }

    /// Override the `configurationDone` budget.
    #[must_use]
    pub fn with_configuration_timeout(mut self, timeout: Duration) -> Self {
        self.configuration_timeout = timeout;
        self
    }

    /// Override the idle budget used by [`Self::reap_idle`].
    #[must_use]
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// The id of the session actions default to.
    #[must_use]
    pub fn active_session_id(&self) -> Option<String> {
        self.active_session_id.lock().clone()
    }

    /// Every live session, in creation order.
    #[must_use]
    pub fn sessions(&self) -> Vec<Arc<DapSessionState>> {
        let order = self.order.lock().clone();
        let sessions = self.sessions.lock();
        order
            .iter()
            .filter_map(|id| sessions.get(id).cloned())
            .collect()
    }

    /// Session summaries in creation order, for `sessions`.
    #[must_use]
    pub fn session_summaries(&self) -> Vec<DapSessionSummary> {
        self.sessions().iter().map(|s| self.build_summary(s)).collect()
    }

    /// One session by id.
    #[must_use]
    pub fn session_state(&self, id: &str) -> Option<Arc<DapSessionState>> {
        self.sessions.lock().get(id).cloned()
    }

    /// The active session state, if any.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::NoSession`] when nothing is active.
    pub fn active_session(&self) -> DapResult<Arc<DapSessionState>> {
        let id = self.active_session_id().ok_or(DapError::NoSession)?;
        self.session_state(&id).ok_or(DapError::NoSession)
    }

    /// Resolve an optional session id to a live session.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::NoSession`] when the id is unknown and
    /// [`DapError::State`] when the session has terminated.
    pub fn resolve_session(&self, id: Option<&str>) -> DapResult<Arc<DapSessionState>> {
        let state = match id {
            Some(id) => self.session_state(id).ok_or_else(|| {
                DapError::State(format!("No debug session with id {id}."))
            })?,
            None => self.active_session()?,
        };
        if state.status() == DapSessionStatus::Terminated {
            return Err(DapError::State(format!(
                "Debug session {} has terminated.",
                state.id()
            )));
        }
        Ok(state)
    }

    /// Make `id` the session actions default to.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::State`] when the id is unknown.
    pub fn select_session(&self, id: &str) -> DapResult<DapSessionSummary> {
        let state = self
            .session_state(id)
            .ok_or_else(|| DapError::State(format!("No debug session with id {id}.")))?;
        *self.active_session_id.lock() = Some(state.id().to_string());
        state.touch();
        Ok(self.build_summary(&state))
    }

    /// Register a freshly spawned session.
    fn register(&self, state: Arc<DapSessionState>) {
        let id = state.id().to_string();
        self.sessions.lock().insert(id.clone(), Arc::clone(&state));
        self.order.lock().push(id.clone());
        *self.active_session_id.lock() = Some(id);
    }

    fn next_session_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        format!("session-{n}")
    }

    /// Snapshot one session.
    #[must_use]
    pub fn build_summary(&self, state: &DapSessionState) -> DapSessionSummary {
        let inner = state.inner.lock();
        let location = inner.location.clone();
        let children = state.children();
        DapSessionSummary {
            id: state.id.clone(),
            adapter: state.adapter.clone(),
            cwd: state.cwd.clone(),
            program: state.program.clone(),
            status: inner.status,
            launched_at: state.launched_at.clone(),
            last_used_at: format_unix_millis(u128::from(inner.last_used_millis)),
            thread_id: location.thread_id,
            frame_id: location.frame_id,
            stop_reason: location.reason.clone(),
            stop_description: location.description.clone(),
            frame_name: location.frame_name.clone(),
            instruction_pointer_reference: location.instruction_pointer_reference.clone(),
            source: location.source.clone(),
            line: location.line,
            column: location.column,
            breakpoint_files: inner.breakpoints.sources.len(),
            breakpoint_count: inner.breakpoints.source_count(),
            function_breakpoint_count: inner.breakpoints.functions.len(),
            output_bytes: u64::try_from(inner.output.len()).unwrap_or(u64::MAX),
            output_truncated: inner.output.truncated(),
            exit_code: inner.exit_code,
            needs_configuration_done: inner.needs_configuration_done,
            parent_session_id: state.parent_session_id.clone(),
            child_session_ids: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    }

    /// Snapshot the active session.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::NoSession`] when nothing is active.
    pub fn status(&self) -> DapResult<DapSessionSummary> {
        let state = self.active_session()?;
        Ok(self.build_summary(&state))
    }

    /// The captured output of one session.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::NoSession`] when no session resolves.
    pub fn output(&self, id: Option<&str>) -> DapResult<DapOutputSnapshot> {
        let state = self.resolve_session(id)?;
        state.touch();
        Ok(DapOutputSnapshot {
            snapshot: self.build_summary(&state),
            output: state.output(),
        })
    }

    // -------------------------------------------------------------------------
    // Launch / attach
    // -------------------------------------------------------------------------

    /// Spawn an adapter, run the handshake, and launch the program.
    ///
    /// The stop waiter is installed before `launch` is sent: an adapter is free
    /// to emit `stopped` in the same read chunk as the `launch` response.
    ///
    /// # Errors
    ///
    /// Propagates spawn, handshake and request failures.
    pub async fn launch(
        &self,
        options: DapLaunchSessionOptions,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        let adapter = options.adapter.clone();
        let cwd = std::path::PathBuf::from(&options.cwd);
        let client = DapClient::spawn(DapSpawnOptions {
            adapter: adapter.clone(),
            cwd: cwd.clone(),
            socket_ready_timeout: None,
        })
        .await?;

        let id = self.next_session_id();
        let state = Arc::new(DapSessionState::new(
            id.clone(),
            Arc::clone(&client),
            options.cwd.clone(),
            Some(options.program.clone()),
            None,
        ));
        self.attach_event_handlers(&state);
        self.register(Arc::clone(&state));

        let deadline = crate::Deadline::new(cancel, Some(BOOKKEEPING_TIMEOUT));
        let capabilities = self
            .handshake(&state, &adapter, deadline)
            .await
            .inspect_err(|_| {
                self.forget(&id);
            })?;
        state.set_capabilities(capabilities.clone());

        let mut arguments = adapter.launch_defaults.clone();
        for (key, value) in options.extra_launch_arguments {
            arguments.insert(key, value);
        }
        arguments.insert("program".to_string(), json!(options.program));
        arguments.insert("cwd".to_string(), json!(options.cwd));
        if let Some(args) = options.args {
            arguments.insert("args".to_string(), json!(args));
        }
        let arguments = Value::Object(arguments);

        let waiter = StopWaiter::new();
        state.install_waiter(Arc::clone(&waiter));
        state.set_status(DapSessionStatus::Configuring);
        client
            .send_request("launch", Some(arguments), cancel, Some(BOOKKEEPING_TIMEOUT))
            .await?;

        self.complete_configuration(&state, &capabilities, cancel).await?;
        let prepared = PreparedStopOutcome {
            session_id: id,
            waiter,
            started: Instant::now(),
            timeout: self.configuration_timeout,
        };
        prepared.await_outcome(self, cancel).await
    }

    /// Spawn an adapter and attach it to a running process or port.
    ///
    /// # Errors
    ///
    /// Propagates spawn, handshake and request failures.
    pub async fn attach(
        &self,
        options: DapAttachSessionOptions,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        let adapter = options.adapter.clone();
        let cwd = std::path::PathBuf::from(&options.cwd);
        let client = DapClient::spawn(DapSpawnOptions {
            adapter: adapter.clone(),
            cwd,
            socket_ready_timeout: None,
        })
        .await?;

        let id = self.next_session_id();
        let state = Arc::new(DapSessionState::new(
            id.clone(),
            Arc::clone(&client),
            options.cwd.clone(),
            None,
            None,
        ));
        self.attach_event_handlers(&state);
        self.register(Arc::clone(&state));

        let deadline = crate::Deadline::new(cancel, Some(BOOKKEEPING_TIMEOUT));
        let capabilities = self
            .handshake(&state, &adapter, deadline)
            .await
            .inspect_err(|_| {
                self.forget(&id);
            })?;
        state.set_capabilities(capabilities.clone());

        let mut arguments = adapter.attach_defaults.clone();
        if let Some(pid) = options.pid {
            arguments.insert("processId".to_string(), json!(pid));
        }
        if let Some(port) = options.port {
            arguments.insert("port".to_string(), json!(port));
            arguments.insert(
                "host".to_string(),
                json!(options.host.clone().unwrap_or_else(|| "127.0.0.1".to_string())),
            );
        }
        if let Some(cwd) = Some(options.cwd.clone()) {
            arguments.insert("cwd".to_string(), json!(cwd));
        }

        let waiter = StopWaiter::new();
        state.install_waiter(Arc::clone(&waiter));
        state.set_status(DapSessionStatus::Configuring);
        if !adapter.skip_attach_request() {
            client
                .send_request(
                    "attach",
                    Some(Value::Object(arguments)),
                    cancel,
                    Some(BOOKKEEPING_TIMEOUT),
                )
                .await?;
        }

        self.complete_configuration(&state, &capabilities, cancel).await?;
        let prepared = PreparedStopOutcome {
            session_id: id,
            waiter,
            started: Instant::now(),
            timeout: self.configuration_timeout,
        };
        prepared.await_outcome(self, cancel).await
    }

    /// Spawn the adapter, run `initialize` and wait for `initialized`.
    async fn handshake(
        &self,
        state: &Arc<DapSessionState>,
        adapter: &DapResolvedAdapter,
        deadline: crate::Deadline<'_>,
    ) -> DapResult<DapCapabilities> {
        let arguments = json!({
            "clientID": "omp",
            "clientName": "omp",
            "adapterID": adapter.name,
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "supportsVariableType": true,
            "supportsVariablePaging": true,
            "supportsRunInTerminalRequest": true,
            "supportsStartDebuggingRequest": true,
        });
        let capabilities = state
            .client
            .initialize(arguments, deadline.cancel, deadline.timeout)
            .await?;
        state.set_capabilities(capabilities.clone());
        Ok(capabilities)
    }

    /// Send `configurationDone` when the adapter asked for the handshake.
    async fn complete_configuration(
        &self,
        state: &Arc<DapSessionState>,
        capabilities: &DapCapabilities,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<()> {
        if !capabilities.needs_configuration_done() || state.configuration_done_sent() {
            state.set_status(DapSessionStatus::Configuring);
            return Ok(());
        }
        state.client
            .send_request("configurationDone", None, cancel, Some(BOOKKEEPING_TIMEOUT))
            .await?;
        state.mark_configuration_done();
        Ok(())
    }

    /// Install the per-session event handlers (output ring, stop bookkeeping).
    fn attach_event_handlers(&self, state: &Arc<DapSessionState>) {
        let weak = Arc::downgrade(state);
        let _output = state.client.on_event("output", move |_event, body| {
            let Some(state) = weak.upgrade() else { return };
            let parsed: crate::types::DapOutputEventBody = body
                .and_then(|b| serde_json::from_value(b.clone()).ok())
                .unwrap_or_default();
            state.push_output(parsed.output.as_bytes());
        });

        let weak = Arc::downgrade(state);
        let _stopped = state.client.on_event("stopped", move |event, body| {
            let Some(state) = weak.upgrade() else { return };
            state.apply_stop_signal(event, body);
            if let Some(waiter) = state.waiter() {
                waiter.settle(StopSignal {
                    event: event.to_string(),
                    body: body.cloned(),
                });
            }
        });

        let weak = Arc::downgrade(state);
        let _exited = state.client.on_event("exited", move |event, body| {
            let Some(state) = weak.upgrade() else { return };
            state.apply_stop_signal(event, body);
            if let Some(waiter) = state.waiter() {
                waiter.settle(StopSignal {
                    event: event.to_string(),
                    body: body.cloned(),
                });
            }
        });

        let weak = Arc::downgrade(state);
        let _terminated = state.client.on_event("terminated", move |event, body| {
            let Some(state) = weak.upgrade() else { return };
            state.apply_stop_signal(event, body);
            if let Some(waiter) = state.waiter() {
                waiter.settle(StopSignal {
                    event: event.to_string(),
                    body: body.cloned(),
                });
            }
        });

        let weak = Arc::downgrade(state);
        let _continued = state.client.on_event("continued", move |event, body| {
            let Some(state) = weak.upgrade() else { return };
            state.apply_stop_signal(event, body);
        });

        // A child session is one the adapter asked us to start (`startDebugging`).
        let weak = Arc::downgrade(state);
        // The reverse handler must be `'static`, so it cannot borrow `self`; child
        // sessions are registered on the process-wide manager.
        let manager: &'static DapSessionManager = &SHARED;
        state.client.on_reverse_request("startDebugging", move |args| {
            let weak = weak.clone();
            async move {
                let Some(parent) = weak.upgrade() else {
                    return Err(DapError::State("parent session is gone".to_string()));
                };
                manager.start_child_session(&parent, args).await
            }
        });

        let weak = Arc::downgrade(state);
        state.client.on_reverse_request("runInTerminal", move |args| {
            let weak = weak.clone();
            async move {
                let Some(state) = weak.upgrade() else {
                    return Err(DapError::State("session is gone".to_string()));
                };
                // The adapter's terminal request is answered with no process: the
                // target inherits our stdio, which is what every adapter we ship
                // tolerates when no terminal client is available.
                let _ = args;
                let _ = state;
                Ok(json!({}))
            }
        });
    }

    /// Spawn the child session an adapter's `startDebugging` request asks for.
    async fn start_child_session(
        &self,
        parent: &Arc<DapSessionState>,
        args: Value,
    ) -> DapResult<Value> {
        let arguments: crate::types::DapStartDebuggingArguments =
            serde_json::from_value(args).unwrap_or_default();
        let port = parent.client.port();
        let cwd = std::path::PathBuf::from(&parent.cwd);
        let adapter = parent.client.adapter().clone();
        let client = match port {
            Some(port) => DapClient::connect(adapter, cwd, "127.0.0.1", port).await?,
            None => {
                return Err(DapError::Unsupported(
                    "The adapter asked for a child session but exposes no TCP port.".to_string(),
                ))
            }
        };

        let id = self.next_session_id();
        let state = Arc::new(DapSessionState::new(
            id.clone(),
            client,
            parent.cwd.clone(),
            None,
            Some(parent.id().to_string()),
        ));
        self.attach_event_handlers(&state);
        self.register(Arc::clone(&state));
        parent.add_child(id);

        let deadline = crate::Deadline::new(None, Some(BOOKKEEPING_TIMEOUT));
        let capabilities = self
            .handshake(&state, &state.client.adapter().clone(), deadline)
            .await?;
        state.set_capabilities(capabilities);

        let request = if arguments.request.is_empty() {
            "launch".to_string()
        } else {
            arguments.request
        };
        state
            .client
            .send_request(
                &request,
                Some(Value::Object(arguments.configuration)),
                None,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(json!({}))
    }

    // -------------------------------------------------------------------------
    // Teardown
    // -------------------------------------------------------------------------

    /// Drop a session from the registry (used when a handshake fails).
    fn forget(&self, id: &str) {
        self.sessions.lock().remove(id);
        self.order.lock().retain(|existing| existing != id);
        let mut active = self.active_session_id.lock();
        if active.as_deref() == Some(id) {
            *active = self.order.lock().last().cloned();
        }
    }

    /// Disconnect a session and forget it.
    ///
    /// The adapter gets a best-effort `disconnect`; failures are ignored because
    /// the process is being torn down either way.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::NoSession`] when no session resolves.
    pub async fn disconnect(
        &self,
        id: Option<&str>,
        terminate_debuggee: bool,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapSessionSummary> {
        let state = match id {
            Some(id) => self
                .session_state(id)
                .ok_or_else(|| DapError::State(format!("No debug session with id {id}.")))?,
            None => self.active_session()?,
        };
        let snapshot = self.build_summary(&state);
        if state.is_alive() {
            let _ = state
                .client
                .send_request(
                    "disconnect",
                    Some(json!({ "terminateDebuggee": terminate_debuggee })),
                    cancel,
                    Some(BOOKKEEPING_TIMEOUT),
                )
                .await;
        }
        state.set_status(DapSessionStatus::Terminated);
        state.client.dispose().await;

        for child in state.children() {
            if let Some(child) = self.session_state(&child) {
                if child.is_alive() {
                    let _ = child.client.dispose().await;
                }
                child.set_status(DapSessionStatus::Terminated);
                self.forget(&child.id);
            }
        }
        if let Some(parent) = state.parent_session_id.clone() {
            if let Some(parent) = self.session_state(&parent) {
                parent.remove_child(state.id());
            }
        }
        self.forget(&state.id());
        Ok(snapshot)
    }

    /// Disconnect every session untouched for longer than `idle_timeout`.
    ///
    /// Returns the ids that were reaped. Sessions that are still running are
    /// left alone: an idle clock is not evidence that a debuggee is abandoned.
    pub async fn reap_idle(&self) -> Vec<String> {
        let cutoff = now_millis().saturating_sub(u64::try_from(self.idle_timeout.as_millis()).unwrap_or(u64::MAX));
        let stale: Vec<String> = self
            .sessions()
            .iter()
            .filter(|state| state.last_used_millis() < cutoff)
            .filter(|state| matches!(state.status(), DapSessionStatus::Stopped | DapSessionStatus::Terminated))
            .map(|state| state.id().to_string())
            .collect();
        for id in &stale {
            let _ = self.disconnect(Some(id), false, None).await;
        }
        stale
    }

    // -------------------------------------------------------------------------
    // Breakpoints
    // -------------------------------------------------------------------------

    /// Set the source breakpoints of one file, replacing the previous set.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::State`] when the session cannot accept breakpoints.
    pub async fn set_breakpoints(
        &self,
        id: Option<&str>,
        source_path: &str,
        breakpoints: &[DapSourceBreakpoint],
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapBreakpointMutation> {
        let state = self.resolve_session(id)?;
        state.touch();
        let absolute = absolute_path(source_path, &state.cwd);
        let requested: Vec<Value> = breakpoints
            .iter()
            .map(|breakpoint| {
                let mut entry = Map::new();
                entry.insert("line".to_string(), json!(breakpoint.line));
                if let Some(column) = breakpoint.column {
                    entry.insert("column".to_string(), json!(column));
                }
                if let Some(condition) = &breakpoint.condition {
                    entry.insert("condition".to_string(), json!(condition));
                }
                if let Some(hit_condition) = &breakpoint.hit_condition {
                    entry.insert("hitCondition".to_string(), json!(hit_condition));
                }
                if let Some(log_message) = &breakpoint.log_message {
                    entry.insert("logMessage".to_string(), json!(log_message));
                }
                Value::Object(entry)
            })
            .collect();
        let body = state
            .client
            .send_request(
                "setBreakpoints",
                Some(json!({
                    "source": { "path": absolute },
                    "breakpoints": requested,
                    "sourceModified": false,
                })),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        let records = parse_source_breakpoints(&body);
        state
            .inner
            .lock()
            .breakpoints
            .sources
            .insert(absolute.clone(), records.clone());
        Ok(DapBreakpointMutation {
            snapshot: self.build_summary(&state),
            breakpoints: records,
            source_path: absolute,
        })
    }

    /// Set function breakpoints, replacing the previous set.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn set_function_breakpoints(
        &self,
        id: Option<&str>,
        breakpoints: &[DapFunctionBreakpoint],
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapTypedBreakpointMutation<DapFunctionBreakpointRecord>> {
        let state = self.resolve_session(id)?;
        state.touch();
        let requested: Vec<Value> = breakpoints
            .iter()
            .map(|breakpoint| {
                let mut entry = Map::new();
                entry.insert("name".to_string(), json!(breakpoint.name));
                if let Some(condition) = &breakpoint.condition {
                    entry.insert("condition".to_string(), json!(condition));
                }
                if let Some(hit_condition) = &breakpoint.hit_condition {
                    entry.insert("hitCondition".to_string(), json!(hit_condition));
                }
                Value::Object(entry)
            })
            .collect();
        let body = state
            .client
            .send_request(
                "setFunctionBreakpoints",
                Some(json!({ "breakpoints": requested })),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        let records: Vec<DapFunctionBreakpointRecord> = body
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let parsed: DapBreakpoint =
                            serde_json::from_value(entry.clone()).unwrap_or_default();
                        let requested = breakpoints.get(index);
                        DapFunctionBreakpointRecord {
                            id: parsed.id,
                            verified: parsed.verified,
                            name: requested.map_or_else(String::new, |b| b.name.clone()),
                            condition: requested.and_then(|b| b.condition.clone()),
                            message: parsed.message,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        state.inner.lock().breakpoints.functions = records.clone();
        Ok(DapTypedBreakpointMutation {
            snapshot: self.build_summary(&state),
            breakpoints: records,
        })
    }

    /// Set instruction breakpoints, replacing the previous set.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn set_instruction_breakpoints(
        &self,
        id: Option<&str>,
        breakpoints: &[DapInstructionBreakpoint],
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapTypedBreakpointMutation<DapInstructionBreakpointRecord>> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::InstructionBreakpoints)?;
        state.touch();
        let requested: Vec<Value> = breakpoints
            .iter()
            .map(|breakpoint| {
                let mut entry = Map::new();
                entry.insert(
                    "instructionReference".to_string(),
                    json!(breakpoint.instruction_reference),
                );
                if let Some(offset) = breakpoint.offset {
                    entry.insert("offset".to_string(), json!(offset));
                }
                if let Some(condition) = &breakpoint.condition {
                    entry.insert("condition".to_string(), json!(condition));
                }
                if let Some(hit_condition) = &breakpoint.hit_condition {
                    entry.insert("hitCondition".to_string(), json!(hit_condition));
                }
                Value::Object(entry)
            })
            .collect();
        let body = state
            .client
            .send_request(
                "setInstructionBreakpoints",
                Some(json!({ "breakpoints": requested })),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        let records: Vec<DapInstructionBreakpointRecord> = body
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let parsed: DapBreakpoint =
                            serde_json::from_value(entry.clone()).unwrap_or_default();
                        let requested = breakpoints.get(index);
                        DapInstructionBreakpointRecord {
                            id: parsed.id,
                            verified: parsed.verified,
                            instruction_reference: requested
                                .map_or_else(String::new, |b| b.instruction_reference.clone()),
                            offset: requested.and_then(|b| b.offset),
                            condition: requested.and_then(|b| b.condition.clone()),
                            hit_condition: requested.and_then(|b| b.hit_condition.clone()),
                            message: parsed.message,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        state.inner.lock().breakpoints.instructions = records.clone();
        Ok(DapTypedBreakpointMutation {
            snapshot: self.build_summary(&state),
            breakpoints: records,
        })
    }

    /// Resolve what `dataBreakpointInfo` reports for a variable.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn data_breakpoint_info(
        &self,
        id: Option<&str>,
        name: &str,
        variables_reference: Option<i64>,
        frame_id: Option<i64>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapDataBreakpointInfoResponse> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::DataBreakpoints)?;
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("name".to_string(), json!(name));
        if let Some(reference) = variables_reference {
            arguments.insert("variablesReference".to_string(), json!(reference));
        }
        if let Some(frame_id) = frame_id {
            arguments.insert("frameId".to_string(), json!(frame_id));
        }
        let body = state
            .client
            .send_request(
                "dataBreakpointInfo",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(serde_json::from_value(body).unwrap_or_default())
    }

    /// Set data breakpoints, replacing the previous set.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn set_data_breakpoints(
        &self,
        id: Option<&str>,
        breakpoints: &[DapDataBreakpoint],
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapTypedBreakpointMutation<DapDataBreakpointRecord>> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::DataBreakpoints)?;
        state.touch();
        let requested: Vec<Value> = breakpoints
            .iter()
            .map(|breakpoint| {
                let mut entry = Map::new();
                entry.insert("dataId".to_string(), json!(breakpoint.data_id));
                if let Some(access_type) = breakpoint.access_type {
                    entry.insert("accessType".to_string(), json!(access_type.as_str()));
                }
                if let Some(condition) = &breakpoint.condition {
                    entry.insert("condition".to_string(), json!(condition));
                }
                if let Some(hit_condition) = &breakpoint.hit_condition {
                    entry.insert("hitCondition".to_string(), json!(hit_condition));
                }
                Value::Object(entry)
            })
            .collect();
        let body = state
            .client
            .send_request(
                "setDataBreakpoints",
                Some(json!({ "breakpoints": requested })),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        let records: Vec<DapDataBreakpointRecord> = body
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let parsed: DapBreakpoint =
                            serde_json::from_value(entry.clone()).unwrap_or_default();
                        let requested = breakpoints.get(index);
                        DapDataBreakpointRecord {
                            id: parsed.id,
                            verified: parsed.verified,
                            data_id: requested.map_or_else(String::new, |b| b.data_id.clone()),
                            access_type: requested.and_then(|b| b.access_type),
                            condition: requested.and_then(|b| b.condition.clone()),
                            hit_condition: requested.and_then(|b| b.hit_condition.clone()),
                            message: parsed.message,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        state.inner.lock().breakpoints.data = records.clone();
        Ok(DapTypedBreakpointMutation {
            snapshot: self.build_summary(&state),
            breakpoints: records,
        })
    }

    // -------------------------------------------------------------------------
    // Execution control
    // -------------------------------------------------------------------------

    /// Install a waiter, send `command`, and await the resulting stop.
    async fn resume(
        &self,
        state: &Arc<DapSessionState>,
        command: &str,
        arguments: Option<Value>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        state.touch();
        state.clear_location();
        state.set_status(DapSessionStatus::Running);
        let waiter = StopWaiter::new();
        // Subscribe before sending: the stop may arrive with the response.
        state.install_waiter(Arc::clone(&waiter));
        state
            .client
            .send_request(command, arguments, cancel, Some(BOOKKEEPING_TIMEOUT))
            .await?;
        let prepared = PreparedStopOutcome {
            session_id: state.id().to_string(),
            waiter,
            started: Instant::now(),
            timeout: self.stop_timeout,
        };
        prepared.await_outcome(self, cancel).await
    }

    /// Continue a stopped thread.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn continue_exec(
        &self,
        id: Option<&str>,
        thread_id: Option<i64>,
        single_thread: bool,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        let state = self.resolve_session(id)?;
        let thread = thread_id.or_else(|| state.location().thread_id);
        let Some(thread) = thread else {
            return Err(DapError::State(
                "No stopped thread to continue. Launch or attach first.".to_string(),
            ));
        };
        let arguments = json!({ "threadId": thread, "singleThread": single_thread });
        self.resume(&state, "continue", Some(arguments), cancel).await
    }

    /// Pause a running thread.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn pause(
        &self,
        id: Option<&str>,
        thread_id: Option<i64>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapSessionSummary> {
        let state = self.resolve_session(id)?;
        let thread = thread_id.or_else(|| state.location().thread_id);
        let mut arguments = Map::new();
        if let Some(thread) = thread {
            arguments.insert("threadId".to_string(), json!(thread));
        }
        state
            .client
            .send_request(
                "pause",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        state.touch();
        Ok(self.build_summary(&state))
    }

    /// Step over.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn next(
        &self,
        id: Option<&str>,
        thread_id: Option<i64>,
        granularity: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        self.step(id, "next", thread_id, granularity, cancel).await
    }

    /// Step into.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn step_in(
        &self,
        id: Option<&str>,
        thread_id: Option<i64>,
        granularity: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        self.step(id, "stepIn", thread_id, granularity, cancel).await
    }

    /// Step out.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn step_out(
        &self,
        id: Option<&str>,
        thread_id: Option<i64>,
        granularity: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        self.step(id, "stepOut", thread_id, granularity, cancel).await
    }

    async fn step(
        &self,
        id: Option<&str>,
        command: &str,
        thread_id: Option<i64>,
        granularity: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapContinueOutcome> {
        let state = self.resolve_session(id)?;
        let thread = thread_id.or_else(|| state.location().thread_id);
        let Some(thread) = thread else {
            return Err(DapError::State(
                "No stopped thread to step. Launch or attach first.".to_string(),
            ));
        };
        let mut arguments = Map::new();
        arguments.insert("threadId".to_string(), json!(thread));
        if let Some(granularity) = granularity {
            if state
                .capabilities()
                .supports_stepping_granularity
                .unwrap_or(false)
            {
                arguments.insert("granularity".to_string(), json!(granularity));
            }
        }
        self.resume(&state, command, Some(Value::Object(arguments)), cancel)
            .await
    }

    // -------------------------------------------------------------------------
    // Inspection
    // -------------------------------------------------------------------------

    /// List the target's threads.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn threads(
        &self,
        id: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapThread>> {
        let state = self.resolve_session(id)?;
        state.touch();
        let body = state
            .client
            .send_request("threads", None, cancel, Some(BOOKKEEPING_TIMEOUT))
            .await?;
        Ok(body
            .get("threads")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Read the stack of a thread.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::State`] when no thread is known.
    pub async fn stack_trace(
        &self,
        id: Option<&str>,
        thread_id: Option<i64>,
        start_frame: Option<i64>,
        levels: Option<i64>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapStackFrame>> {
        let state = self.resolve_session(id)?;
        let thread = thread_id.or_else(|| state.location().thread_id);
        let Some(thread) = thread else {
            return Err(DapError::State(
                "No thread to inspect. Launch or attach first.".to_string(),
            ));
        };
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("threadId".to_string(), json!(thread));
        arguments.insert("startFrame".to_string(), json!(start_frame.unwrap_or(0)));
        if let Some(levels) = levels {
            arguments.insert("levels".to_string(), json!(levels));
        }
        let body = state
            .client
            .send_request(
                "stackTrace",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        let frames: Vec<DapStackFrame> = body
            .get("stackFrames")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(frame) = frames.first() {
            let mut location = state.location();
            location.frame_id = Some(frame.id);
            location.frame_name = Some(frame.name.clone());
            location.instruction_pointer_reference = frame.instruction_pointer_reference.clone();
            location.source = frame.source.clone();
            location.line = Some(frame.line);
            location.column = Some(frame.column);
            location.thread_id = Some(thread);
            state.set_location(location);
        }
        Ok(frames)
    }

    /// Read the scopes of one frame.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::State`] when no frame is known.
    pub async fn scopes(
        &self,
        id: Option<&str>,
        frame_id: Option<i64>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapScope>> {
        let state = self.resolve_session(id)?;
        let frame = frame_id.or_else(|| state.location().frame_id);
        let Some(frame) = frame else {
            return Err(DapError::State(
                "No stack frame selected. Pause the target first.".to_string(),
            ));
        };
        state.touch();
        let body = state
            .client
            .send_request(
                "scopes",
                Some(json!({ "frameId": frame })),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(body
            .get("scopes")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Read the variables of one reference.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn variables(
        &self,
        id: Option<&str>,
        variables_reference: i64,
        start: Option<i64>,
        count: Option<i64>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapVariable>> {
        let state = self.resolve_session(id)?;
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("variablesReference".to_string(), json!(variables_reference));
        if let Some(start) = start {
            arguments.insert("start".to_string(), json!(start));
        }
        if let Some(count) = count {
            arguments.insert("count".to_string(), json!(count));
        }
        let body = state
            .client
            .send_request(
                "variables",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(body
            .get("variables")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Evaluate an expression in the current frame.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn evaluate(
        &self,
        id: Option<&str>,
        expression: &str,
        frame_id: Option<i64>,
        context: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapEvaluateResponse> {
        let state = self.resolve_session(id)?;
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("expression".to_string(), json!(expression));
        if let Some(frame) = frame_id.or_else(|| state.location().frame_id) {
            arguments.insert("frameId".to_string(), json!(frame));
        }
        if let Some(context) = context {
            arguments.insert("context".to_string(), json!(context));
        }
        let body = state
            .client
            .send_request(
                "evaluate",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(serde_json::from_value(body).unwrap_or_default())
    }

    /// Disassemble a memory range.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn disassemble(
        &self,
        id: Option<&str>,
        memory_reference: &str,
        offset: Option<i64>,
        instruction_offset: Option<i64>,
        instruction_count: i64,
        resolve_symbols: bool,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapDisassembledInstruction>> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::Disassemble)?;
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("memoryReference".to_string(), json!(memory_reference));
        arguments.insert("instructionCount".to_string(), json!(instruction_count));
        arguments.insert("resolveSymbols".to_string(), json!(resolve_symbols));
        if let Some(offset) = offset {
            arguments.insert("offset".to_string(), json!(offset));
        }
        if let Some(instruction_offset) = instruction_offset {
            arguments.insert("instructionOffset".to_string(), json!(instruction_offset));
        }
        let body = state
            .client
            .send_request(
                "disassemble",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(body
            .get("instructions")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Read target memory.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn read_memory(
        &self,
        id: Option<&str>,
        memory_reference: &str,
        offset: Option<i64>,
        count: i64,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapReadMemoryResponse> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::ReadMemory)?;
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("memoryReference".to_string(), json!(memory_reference));
        arguments.insert("count".to_string(), json!(count));
        if let Some(offset) = offset {
            arguments.insert("offset".to_string(), json!(offset));
        }
        let body = state
            .client
            .send_request(
                "readMemory",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(serde_json::from_value(body).unwrap_or_default())
    }

    /// Write target memory. `data` is base64, as the protocol requires.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn write_memory(
        &self,
        id: Option<&str>,
        memory_reference: &str,
        offset: Option<i64>,
        data: &str,
        allow_partial: bool,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<DapWriteMemoryResponse> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::WriteMemory)?;
        state.touch();
        let mut arguments = Map::new();
        arguments.insert("memoryReference".to_string(), json!(memory_reference));
        arguments.insert("data".to_string(), json!(data));
        arguments.insert("allowPartial".to_string(), json!(allow_partial));
        if let Some(offset) = offset {
            arguments.insert("offset".to_string(), json!(offset));
        }
        let body = state
            .client
            .send_request(
                "writeMemory",
                Some(Value::Object(arguments)),
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(serde_json::from_value(body).unwrap_or_default())
    }

    /// List the sources the adapter knows about.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn loaded_sources(
        &self,
        id: Option<&str>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapSource>> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::LoadedSources)?;
        state.touch();
        let body = state
            .client
            .send_request("loadedSources", None, cancel, Some(BOOKKEEPING_TIMEOUT))
            .await?;
        Ok(body
            .get("sources")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// List the target's modules.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Unsupported`] when the adapter lacks the capability.
    pub async fn modules(
        &self,
        id: Option<&str>,
        start_module: Option<i64>,
        module_count: Option<i64>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Vec<DapModule>> {
        let state = self.resolve_session(id)?;
        state.require_capability(DapCapability::Modules)?;
        state.touch();
        let mut arguments = Map::new();
        if let Some(start) = start_module {
            arguments.insert("startModule".to_string(), json!(start));
        }
        if let Some(count) = module_count {
            arguments.insert("moduleCount".to_string(), json!(count));
        }
        let body = state
            .client
            .send_request(
                "modules",
                if arguments.is_empty() {
                    None
                } else {
                    Some(Value::Object(arguments))
                },
                cancel,
                Some(BOOKKEEPING_TIMEOUT),
            )
            .await?;
        Ok(body
            .get("modules")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| serde_json::from_value(entry.clone()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Send an arbitrary request through the session.
    ///
    /// # Errors
    ///
    /// Propagates the adapter's refusal.
    pub async fn custom_request(
        &self,
        id: Option<&str>,
        command: &str,
        arguments: Option<Value>,
        cancel: Option<&dyn Cancel>,
    ) -> DapResult<Value> {
        let state = self.resolve_session(id)?;
        state.touch();
        state
            .client
            .send_request(command, arguments, cancel, Some(BOOKKEEPING_TIMEOUT))
            .await
    }
}

/// Resolve `path` against `cwd` unless it is already absolute.
fn absolute_path(path: &str, cwd: &str) -> String {
    let candidate = std::path::Path::new(path);
    if candidate.is_absolute() {
        return candidate.display().to_string();
    }
    std::path::Path::new(cwd)
        .join(candidate)
        .display()
        .to_string()
}

/// Turn a `setBreakpoints` reply into the records the session keeps.
fn parse_source_breakpoints(body: &Value) -> Vec<DapBreakpointRecord> {
    body.get("breakpoints")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    let parsed: DapBreakpoint =
                        serde_json::from_value(entry.clone()).unwrap_or_default();
                    DapBreakpointRecord {
                        id: parsed.id,
                        verified: parsed.verified,
                        line: parsed.line.unwrap_or_default(),
                        condition: None,
                        message: parsed.message,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Access mode of a data breakpoint from its tool spelling.
#[must_use]
pub fn parse_access_type(value: &str) -> Option<DapAccessType> {
    DapAccessType::parse(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_ring_keeps_the_tail_and_latches_truncation() {
        let mut ring = OutputRing::with_capacity(8);
        ring.push(b"hello");
        assert!(!ring.truncated());
        assert_eq!(ring.to_string_lossy(), "hello");

        // `push` enforces the crate-wide capacity, so drive the boundary with a
        // payload larger than `OUTPUT_RING_CAPACITY` instead of the small one.
        let big = "x".repeat(OUTPUT_RING_CAPACITY + 16);
        let mut ring = OutputRing::default();
        ring.push(big.as_bytes());
        assert_eq!(ring.len(), OUTPUT_RING_CAPACITY);
        assert!(ring.truncated());
    }

    #[test]
    fn output_ring_clear_resets_truncation() {
        let mut ring = OutputRing::default();
        ring.push(&vec![b'a'; OUTPUT_RING_CAPACITY + 1]);
        assert!(ring.truncated());
        ring.clear();
        assert!(ring.is_empty());
        assert!(!ring.truncated());
    }

    #[test]
    fn timestamp_renders_rfc3339() {
        // 2024-01-02T03:04:05.006Z
        assert_eq!(
            format_unix_millis(1_704_164_645_006),
            "2024-01-02T03:04:05.006Z"
        );
        assert_eq!(format_unix_millis(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn absolute_path_resolves_against_cwd() {
        assert_eq!(absolute_path("/a/b.c", "/tmp"), "/a/b.c");
        assert_eq!(absolute_path("b.c", "/tmp"), "/tmp/b.c");
    }

    #[test]
    fn parse_source_breakpoints_reads_the_reply() {
        let body = json!({
            "breakpoints": [
                { "id": 1, "verified": true, "line": 7 },
                { "verified": false, "line": 9, "message": "no code" },
            ]
        });
        let records = parse_source_breakpoints(&body);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, Some(1));
        assert!(records[0].verified);
        assert_eq!(records[0].line, 7);
        assert_eq!(records[1].message.as_deref(), Some("no code"));
    }

    #[test]
    fn waiter_settles_once() {
        let waiter = StopWaiter::new();
        assert!(!waiter.is_resolved());
        waiter.settle(StopSignal {
            event: "stopped".to_string(),
            body: None,
        });
        assert!(waiter.is_resolved());
        waiter.settle(StopSignal {
            event: "terminated".to_string(),
            body: None,
        });
        assert_eq!(waiter.take().map(|s| s.event), Some("stopped".to_string()));
    }

    #[tokio::test]
    async fn manager_tracks_active_session_selection() {
        let manager = DapSessionManager::new();
        assert!(manager.active_session_id().is_none());
        assert!(manager.status().is_err());
        assert!(matches!(
            manager.resolve_session(None),
            Err(DapError::NoSession)
        ));
        assert!(matches!(
            manager.select_session("nope"),
            Err(DapError::State(_))
        ));
        assert!(manager.reap_idle().await.is_empty());
    }

    #[test]
    fn access_type_parsing_matches_the_tool_spelling() {
        assert_eq!(parse_access_type("read"), Some(DapAccessType::Read));
        assert_eq!(parse_access_type("write"), Some(DapAccessType::Write));
        assert_eq!(parse_access_type("readWrite"), Some(DapAccessType::ReadWrite));
        assert_eq!(parse_access_type("READ"), None);
    }

    #[test]
    fn summaries_render_empty_state() {
        let summary = DapSessionSummary::default();
        assert!(summary.location().is_none());
        assert!(!summary.needs_configuration_done);
        assert_eq!(summary.breakpoint_files, 0);
    }
}
