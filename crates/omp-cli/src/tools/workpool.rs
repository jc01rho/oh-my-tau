//! `workpool`: engine-owned keyed work queues (OmOMeow engine contract,
//! design `pieces/15`, `micro/impl-13`, `micro/test-10`).
//!
//! A pool runs pushed items on background workers: tasks of the process
//! registry ([`super::subagents`]) that the pool owns, so their runs never
//! notify the parent one by one. Items are keyed: pushing a key again with
//! the same input returns its durable id, a different input is refused.
//! A worker reports per key with `workpool` `yield` (data or an error) into
//! the pool's durable store; a run that ends without one leaves the item
//! `item_missing_yield`. `fresh` gives every item a new worker; `keep_alive`
//! (the default) revives an idle worker's session for the next item.
//! `close` or `cancel` ends intake; once every item is terminal the parent
//! receives exactly one aggregate, and a failed delivery is recorded as
//! uncertain and never retried automatically.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use anyhow::Result;
use omp_task::params::SchemaMode;
use omp_task::registry::{SpawnRequest, SubagentRegistry, TaskStatus, TaskView};
use omp_task::{TaskRunner, TaskSpec};
use pa_agent::types::AgentToolResult;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::Env;

pub const DESCRIPTION: &str = "Create and inspect engine-owned keyed work queues. Push returns durable IDs without waiting for capacity. Omitted mode uses the approved keep_alive default; fresh remains selectable. Inspect reads durable keyed data or errors; close delivers one aggregate; uncertain delivery is never retried automatically.";

/// Env var naming a worker run's assignment file; set on every worker, it
/// also tells the `workpool` tool that this session is a worker.
pub const ASSIGNMENT_ENV: &str = "OMP_WORKPOOL_ASSIGNMENT";
const TURN_MESSAGE: &str = "Process the admitted workpool assignments above. Use workpool yield for each key; do not spawn or schedule tasks.";

#[must_use]
pub fn schema() -> Value {
    let nonempty = json!({"type": "string", "minLength": 1, "pattern": "\\S"});
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "op": {"type": "string", "enum": ["create", "push", "close", "inspect", "cancel", "yield"]},
            "name": nonempty,
            "agent": {"oneOf": [
                {"type": "object", "additionalProperties": false, "required": ["category", "prompt"],
                 "properties": {"category": nonempty, "prompt": nonempty, "model": nonempty}},
                {"type": "object", "additionalProperties": false, "required": ["subagent_type", "prompt"],
                 "properties": {"subagent_type": nonempty, "prompt": nonempty, "model": nonempty}}
            ]},
            "mode": {"type": "string", "enum": ["fresh", "keep_alive"]},
            "tools": {"type": "array", "items": nonempty},
            "pool_id": {"type": "string", "pattern": "^wp_[0-9a-f]{32}$"},
            "items": {"type": "array", "items": {"type": "object", "additionalProperties": false,
                "required": ["key", "input"], "properties": {"key": nonempty, "input": {}}}},
            "results": {"type": "array", "items": {}}
        }
    })
}

/// A refusal with the engine's error vocabulary (`invalid_input`,
/// `pool_not_found`, `duplicate_key_conflict`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpError {
    pub code: &'static str,
    pub message: String,
}

fn wp_error(code: &'static str, message: impl Into<String>) -> WpError {
    WpError {
        code,
        message: message.into(),
    }
}

fn invalid_input() -> WpError {
    wp_error("invalid_input", "Invalid workpool arguments.")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Fresh,
    KeepAlive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl AgentSpec {
    fn agent_name(&self) -> String {
        self.subagent_type
            .clone()
            .or_else(|| self.category.clone())
            .unwrap_or_else(|| "task".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolStatus {
    Open,
    Closing,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Queued,
    Assigned,
    Completed,
    Error,
    Cancelled,
}

impl ItemStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Error | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub task_id: String,
    pub run_epoch: u64,
    pub generation: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    pub input: Value,
    pub item_id: String,
    pub status: ItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ItemError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yield_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Busy,
    Idle,
    /// The worker's session takes no more items (fresh mode, a failed run,
    /// or a cancelled pool).
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    pub task_id: String,
    pub status: WorkerStatus,
    pub completed_turns: u32,
    /// The assignment file the worker reads its current item from.
    pub assignment: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Built, not delivered yet (or deferred: no session to take it now).
    Pending,
    Delivering,
    Delivered,
    /// The one delivery attempt failed; never retried automatically.
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregate {
    pub generation: u32,
    pub delivery: DeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One pool, as persisted in `<store>/<pool_id>/pool.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    pub pool_id: String,
    pub name: String,
    pub parent_session_id: String,
    pub root_session_id: String,
    /// Bumped by `cancel`, so a late yield of an earlier generation is ignored.
    pub generation: u32,
    pub mode: Mode,
    pub agent: AgentSpec,
    pub status: PoolStatus,
    pub limit: usize,
    pub cwd: String,
    pub items: Vec<Item>,
    pub workers: Vec<Worker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Aggregate>,
    pub owner_pid: u32,
}

impl Pool {
    fn finished(&self) -> bool {
        self.status != PoolStatus::Open
            && self.items.iter().all(|item| item.status.is_terminal())
            && !self.workers.iter().any(|w| w.status == WorkerStatus::Busy)
    }

    fn view(&self) -> Value {
        let count = |status: ItemStatus| self.items.iter().filter(|i| i.status == status).count();
        json!({
            "pool_id": self.pool_id,
            "name": self.name,
            "mode": self.mode,
            "status": self.status,
            "generation": self.generation,
            "limit": self.limit,
            "counts": {
                "queued": count(ItemStatus::Queued),
                "assigned": count(ItemStatus::Assigned),
                "completed": count(ItemStatus::Completed),
                "error": count(ItemStatus::Error),
                "cancelled": count(ItemStatus::Cancelled),
            },
            "items": self.items.iter().map(|item| {
                let mut row = json!({"key": item.key, "item_id": item.item_id, "status": item.status});
                if let Some(binding) = &item.binding {
                    row["task_id"] = json!(binding.task_id);
                }
                if let Some(data) = &item.data {
                    row["data"] = data.clone();
                }
                if let Some(error) = &item.error {
                    row["error"] = json!(error);
                }
                row
            }).collect::<Vec<_>>(),
            "workers": self.workers.iter().map(|w| json!({
                "task_id": w.task_id, "status": w.status, "completed_turns": w.completed_turns,
            })).collect::<Vec<_>>(),
            "aggregate": self.aggregate,
        })
    }

    /// The one aggregate: every item in push order, data or error.
    fn aggregate_message(&self) -> String {
        let results: Vec<Value> = self
            .items
            .iter()
            .map(|item| match (item.status, &item.data, &item.error) {
                (ItemStatus::Completed, data, _) => {
                    json!({"key": item.key, "data": data.clone().unwrap_or(Value::Null)})
                }
                (_, _, Some(error)) => json!({"key": item.key, "error": error}),
                _ => json!({"key": item.key, "error": {"code": "cancelled", "message": "Pool item did not complete."}}),
            })
            .collect();
        let body = json!({"pool_id": self.pool_id, "name": self.name, "generation": self.generation, "results": results});
        format!(
            "<workpool-aggregate pool_id=\"{}\" name=\"{}\">\n{}\n</workpool-aggregate>",
            self.pool_id,
            self.name,
            serde_json::to_string_pretty(&body).unwrap_or_default()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Delivered,
    /// No session can take it now (held, switched away, no runtime); kept
    /// pending for the next flush.
    Deferred,
    Failed(String),
}

pub type Deliverer =
    Arc<dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Delivery> + Send>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Caller {
    pub session_id: String,
    pub root_session_id: String,
    pub cwd: PathBuf,
}

/// The worker's view of its current assignment (the file behind
/// [`ASSIGNMENT_ENV`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub pool_id: String,
    pub generation: u32,
    pub yields_dir: String,
    pub items: Vec<AssignedItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignedItem {
    pub item_id: String,
    pub key: String,
}

/// A worker's terminal report for one key, as stored in `yields/<item_id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct YieldRecord {
    pool_id: String,
    generation: u32,
    item_id: String,
    key: String,
    result: Value,
    sha256: String,
}

pub struct WorkpoolEngine {
    registry: Arc<SubagentRegistry>,
    store: PathBuf,
    limit: usize,
    pools: Mutex<BTreeMap<String, Pool>>,
    deliverer: Deliverer,
    changes: watch::Sender<u64>,
}

static ENGINE: OnceLock<Arc<WorkpoolEngine>> = OnceLock::new();

/// The process engine (pools under this project's `senpi-task/workpools`).
pub(crate) fn engine() -> Arc<WorkpoolEngine> {
    ENGINE
        .get_or_init(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let settings = omp_settings::Settings::load(&cwd);
            let limit =
                TaskRunner::resolve_max_concurrency(settings.get_number("task.maxConcurrency"));
            let deliverer: Deliverer = Arc::new(|session, message| {
                Box::pin(async move { super::subagents::deliver_message(&session, &message).await })
            });
            WorkpoolEngine::new(
                super::subagents::registry(),
                super::subagents::senpi_task_dir(&cwd).join("workpools"),
                limit,
                deliverer,
            )
        })
        .clone()
}

pub(crate) fn existing_engine() -> Option<Arc<WorkpoolEngine>> {
    ENGINE.get().cloned()
}

impl WorkpoolEngine {
    #[must_use]
    pub fn new(
        registry: Arc<SubagentRegistry>,
        store: PathBuf,
        limit: usize,
        deliverer: Deliverer,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            store,
            limit: limit.max(1),
            pools: Mutex::new(BTreeMap::new()),
            deliverer,
            changes: watch::Sender::new(0),
        })
    }

    /// A registry listener that routes pool workers' settled runs here. In
    /// production the process registry's listener does this through
    /// [`super::subagents`]; tests wire it directly.
    #[cfg(test)]
    #[must_use]
    pub fn settle_forwarder(self: &Arc<Self>) -> omp_task::registry::Listener {
        use omp_task::registry::RegistryEvent;
        let weak = Arc::downgrade(self);
        Arc::new(move |event| {
            let RegistryEvent::Settled(view) = event else {
                return;
            };
            if view.record.owner.is_none() {
                return;
            }
            if let (Some(engine), Ok(handle)) =
                (weak.upgrade(), tokio::runtime::Handle::try_current())
            {
                let view = TaskView::clone(view);
                handle.spawn(async move { engine.on_worker_settled(&view).await });
            }
        })
    }

    #[cfg(test)]
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Pool>> {
        self.pools.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changes.send_modify(|generation| *generation += 1);
    }

    fn pool_dir(&self, pool_id: &str) -> PathBuf {
        self.store.join(pool_id)
    }

    fn persist(&self, pool: &Pool) {
        let dir = self.pool_dir(&pool.pool_id);
        let written = serde_json::to_string_pretty(pool)
            .map_err(std::io::Error::other)
            .and_then(|text| write_atomic(&dir, "pool.json", &text));
        if let Err(e) = written {
            tracing::warn!(pool = %pool.pool_id, "workpool record not saved: {e}");
        }
    }

    /// A persisted pool of this store (one this process no longer holds).
    fn load_pool(&self, pool_id: &str) -> Option<Pool> {
        let text = std::fs::read_to_string(self.pool_dir(pool_id).join("pool.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    #[must_use]
    pub fn has_pools(&self, session: &str) -> bool {
        self.lock().values().any(|p| p.parent_session_id == session)
    }

    /// Pool work `session` still waits on beyond running workers: queued
    /// items of live pools and aggregates not delivered yet.
    #[must_use]
    pub fn open_work(&self, session: &str) -> usize {
        self.lock()
            .values()
            .filter(|p| p.parent_session_id == session)
            .map(|p| {
                let queued = if p.status == PoolStatus::Cancelled {
                    0
                } else {
                    p.items
                        .iter()
                        .filter(|i| i.status == ItemStatus::Queued)
                        .count()
                };
                let undelivered = p.aggregate.as_ref().is_some_and(|a| {
                    matches!(
                        a.delivery,
                        DeliveryState::Pending | DeliveryState::Delivering
                    )
                });
                queued + usize::from(undelivered)
            })
            .sum()
    }

    /// Whether `session` has a finished pool whose aggregate waits for a
    /// session to take it.
    #[must_use]
    pub fn has_deferred_aggregates(&self, session: &str) -> bool {
        self.lock().values().any(|p| {
            p.parent_session_id == session
                && p.aggregate
                    .as_ref()
                    .is_some_and(|a| a.delivery == DeliveryState::Pending)
        })
    }

    pub async fn flush_aggregates(self: &Arc<Self>, session: &str) {
        let ids: Vec<String> = self
            .lock()
            .values()
            .filter(|p| p.parent_session_id == session)
            .map(|p| p.pool_id.clone())
            .collect();
        for id in ids {
            self.maybe_finish(&id).await;
        }
    }

    fn owned<'a>(
        pools: &'a mut BTreeMap<String, Pool>,
        caller: &Caller,
        pool_id: &str,
    ) -> Result<&'a mut Pool, WpError> {
        let pool = pools
            .get_mut(pool_id)
            .ok_or_else(|| wp_error("pool_not_found", "No workpool with this id."))?;
        if pool.parent_session_id != caller.session_id {
            return Err(wp_error(
                "scope_denied",
                "Only the owning parent session may access this pool.",
            ));
        }
        Ok(pool)
    }

    /// `create`: a new open pool (no worker starts before a push).
    ///
    /// # Errors
    /// `tools_unavailable` for eval-defined worker tools, `admission_refused`
    /// past the recursion limit, `pool_name_conflict` for a taken name.
    pub fn create(
        &self,
        caller: &Caller,
        name: Option<String>,
        agent: AgentSpec,
        mode: Option<Mode>,
        tools: Option<&[String]>,
    ) -> Result<Value, WpError> {
        if tools.is_some_and(|t| !t.is_empty()) {
            return Err(wp_error(
                "tools_unavailable",
                "Eval-defined worker tools are unavailable in this runtime.",
            ));
        }
        omp_task::recursion_gate().map_err(|e| wp_error("admission_refused", e))?;
        let mut pools = self.lock();
        let mine = pools
            .values()
            .filter(|p| p.parent_session_id == caller.session_id)
            .count();
        let name = name.unwrap_or_else(|| format!("pool-{}", mine + 1));
        if pools
            .values()
            .any(|p| p.parent_session_id == caller.session_id && p.name == name)
        {
            return Err(wp_error(
                "pool_name_conflict",
                "Pool name is already in use; choose a new explicit name.",
            ));
        }
        let pool = Pool {
            pool_id: new_id("wp_"),
            name,
            parent_session_id: caller.session_id.clone(),
            root_session_id: caller.root_session_id.clone(),
            generation: 1,
            mode: mode.unwrap_or(Mode::KeepAlive),
            agent,
            status: PoolStatus::Open,
            limit: self.limit,
            cwd: caller.cwd.display().to_string(),
            items: Vec::new(),
            workers: Vec::new(),
            aggregate: None,
            owner_pid: std::process::id(),
        };
        self.persist(&pool);
        let view = pool.view();
        pools.insert(pool.pool_id.clone(), pool);
        drop(pools);
        self.bump();
        Ok(view)
    }

    /// `push`: queue keyed items and start workers up to the limit; returns
    /// each item's durable id without waiting for capacity.
    ///
    /// # Errors
    /// `pool_closed`, `duplicate_key_conflict`, `invalid_input`, scope errors.
    pub fn push(
        &self,
        caller: &Caller,
        pool_id: &str,
        items: Vec<(String, Value)>,
    ) -> Result<Value, WpError> {
        let mut pools = self.lock();
        let pool = Self::owned(&mut pools, caller, pool_id)?;
        if pool.status != PoolStatus::Open {
            return Err(wp_error(
                "pool_closed",
                "This pool no longer accepts items.",
            ));
        }
        let receipts = stage_items(&mut pool.items, items)?;
        self.dispatch_locked(pool);
        self.persist(pool);
        drop(pools);
        self.bump();
        Ok(json!({"pool_id": pool_id, "item_ids": receipts}))
    }

    /// # Errors
    /// Unknown pool or another session's pool.
    pub fn inspect(&self, caller: &Caller, pool_id: &str) -> Result<Value, WpError> {
        let mut pools = self.lock();
        match Self::owned(&mut pools, caller, pool_id) {
            Ok(pool) => Ok(pool.view()),
            Err(e) if e.code == "pool_not_found" => {
                drop(pools);
                let pool = self.load_pool(pool_id).ok_or(e)?;
                if pool.parent_session_id != caller.session_id {
                    return Err(wp_error(
                        "scope_denied",
                        "Only the owning parent session may access this pool.",
                    ));
                }
                Ok(pool.view())
            }
            Err(e) => Err(e),
        }
    }

    /// `close`: no more items; the aggregate follows once every item is
    /// terminal.
    ///
    /// # Errors
    /// Unknown pool or another session's pool.
    pub async fn close(self: &Arc<Self>, caller: &Caller, pool_id: &str) -> Result<Value, WpError> {
        {
            let mut pools = self.lock();
            let pool = Self::owned(&mut pools, caller, pool_id)?;
            if pool.status == PoolStatus::Open {
                pool.status = PoolStatus::Closing;
                self.persist(pool);
            }
        }
        self.maybe_finish(pool_id).await;
        self.bump();
        self.inspect(caller, pool_id)
    }

    /// `cancel`: queued and running items end `cancelled`, running workers
    /// are cancelled, and the aggregate (with those errors) follows.
    ///
    /// # Errors
    /// Unknown pool or another session's pool.
    pub async fn cancel(
        self: &Arc<Self>,
        caller: &Caller,
        pool_id: &str,
    ) -> Result<Value, WpError> {
        let busy: Vec<String> = {
            let mut pools = self.lock();
            let pool = Self::owned(&mut pools, caller, pool_id)?;
            if pool.status == PoolStatus::Cancelled {
                return Ok(pool.view());
            }
            pool.status = PoolStatus::Cancelled;
            pool.generation += 1;
            for item in &mut pool.items {
                if matches!(item.status, ItemStatus::Queued | ItemStatus::Assigned) {
                    item.status = ItemStatus::Cancelled;
                    item.error = Some(ItemError {
                        code: "cancelled".to_string(),
                        message: "Pool cancelled.".to_string(),
                    });
                }
            }
            let busy = pool
                .workers
                .iter()
                .filter(|w| w.status == WorkerStatus::Busy)
                .map(|w| w.task_id.clone())
                .collect();
            for worker in &mut pool.workers {
                worker.status = WorkerStatus::Retired;
            }
            self.persist(pool);
            busy
        };
        for task_id in busy {
            self.registry
                .cancel(&task_id, Some("workpool cancelled"), None, true);
        }
        self.maybe_finish(pool_id).await;
        self.bump();
        self.inspect(caller, pool_id)
    }

    /// Take a pool worker's settled run: resolve its assigned items from
    /// their yields, free (or retire) the worker, start queued items and
    /// deliver the aggregate when the pool is finished.
    pub async fn on_worker_settled(self: &Arc<Self>, view: &TaskView) {
        let Some(pool_id) = view.record.owner.clone() else {
            return;
        };
        {
            let mut pools = self.lock();
            let Some(pool) = pools.get_mut(&pool_id) else {
                return;
            };
            let yields = self.pool_dir(&pool_id).join("yields");
            for item in &mut pool.items {
                let bound = item.binding.as_ref().is_some_and(|b| {
                    b.task_id == view.id() && b.run_epoch == view.record.run_epoch
                });
                if bound && item.status == ItemStatus::Assigned {
                    resolve_item(item, &yields, view);
                }
            }
            let keep = pool.mode == Mode::KeepAlive
                && pool.status != PoolStatus::Cancelled
                && view.status() == TaskStatus::Completed;
            if let Some(worker) = pool.workers.iter_mut().find(|w| w.task_id == view.id()) {
                worker.completed_turns += 1;
                worker.status = if keep {
                    WorkerStatus::Idle
                } else {
                    WorkerStatus::Retired
                };
            }
            self.dispatch_locked(pool);
            self.persist(pool);
        }
        self.maybe_finish(&pool_id).await;
        self.bump();
    }

    /// Start queued items while the pool has capacity: revive an idle
    /// keep-alive worker first, else spawn a new worker.
    fn dispatch_locked(&self, pool: &mut Pool) {
        if pool.status == PoolStatus::Cancelled {
            return;
        }
        loop {
            let assigned = pool
                .items
                .iter()
                .filter(|i| i.status == ItemStatus::Assigned)
                .count();
            if assigned >= pool.limit {
                return;
            }
            let Some(index) = pool
                .items
                .iter()
                .position(|i| i.status == ItemStatus::Queued)
            else {
                return;
            };
            let idle = (pool.mode == Mode::KeepAlive)
                .then(|| {
                    pool.workers
                        .iter()
                        .position(|w| w.status == WorkerStatus::Idle)
                })
                .flatten();
            match idle {
                Some(worker) => self.revive_worker(pool, worker, index),
                None => self.spawn_worker(pool, index),
            }
        }
    }

    fn assignment_for(&self, pool: &Pool, index: usize) -> Assignment {
        let item = &pool.items[index];
        Assignment {
            pool_id: pool.pool_id.clone(),
            generation: pool.generation,
            yields_dir: self
                .pool_dir(&pool.pool_id)
                .join("yields")
                .display()
                .to_string(),
            items: vec![AssignedItem {
                item_id: item.item_id.clone(),
                key: item.key.clone(),
            }],
        }
    }

    fn spawn_worker(&self, pool: &mut Pool, index: usize) {
        let assignment = self.assignment_for(pool, index);
        let file = self
            .pool_dir(&pool.pool_id)
            .join("assign")
            .join(format!("{}.json", new_id("wa_")));
        let prompt = format!(
            "{}\n\n{}",
            pool.agent.prompt.trim(),
            assignment_prompt(&pool.items[index], &pool.pool_id)
        );
        let mut spec = TaskSpec {
            id: None,
            agent: pool.agent.agent_name(),
            prompt,
            cwd: PathBuf::from(&pool.cwd),
            output_schema: None,
            schema_mode: SchemaMode::Permissive,
            model: pool.agent.model.clone(),
            tools: None,
            depth: omp_task::current_task_depth() + 1,
            extra_args: Vec::new(),
        };
        super::eval_agents::apply_child_policy(&mut spec);
        super::task::apply_agent_model(&mut spec);
        let item = &mut pool.items[index];
        if let Err(e) = write_assignment(&file, &assignment) {
            item.status = ItemStatus::Error;
            item.error = Some(ItemError {
                code: "spawn_failed".to_string(),
                message: format!("Could not write the worker assignment: {e}"),
            });
            return;
        }
        let request = SpawnRequest {
            spec,
            name: None,
            description: Some(format!("workpool {} worker", pool.name)),
            parent_session_id: pool.parent_session_id.clone(),
            root_session_id: pool.root_session_id.clone(),
            env: vec![
                (
                    super::subagents::ROOT_SESSION_ENV.to_string(),
                    pool.root_session_id.clone(),
                ),
                (ASSIGNMENT_ENV.to_string(), file.display().to_string()),
            ],
            owner: Some(pool.pool_id.clone()),
        };
        match self.registry.spawn(request) {
            Ok(view) => {
                item.status = ItemStatus::Assigned;
                item.binding = Some(Binding {
                    task_id: view.id().to_string(),
                    run_epoch: view.record.run_epoch,
                    generation: pool.generation,
                });
                pool.workers.push(Worker {
                    task_id: view.id().to_string(),
                    status: WorkerStatus::Busy,
                    completed_turns: 0,
                    assignment: file.display().to_string(),
                });
            }
            Err(e) => {
                item.status = ItemStatus::Error;
                item.error = Some(ItemError {
                    code: "spawn_failed".to_string(),
                    message: e,
                });
            }
        }
    }

    fn revive_worker(&self, pool: &mut Pool, worker: usize, index: usize) {
        let assignment = self.assignment_for(pool, index);
        let file = PathBuf::from(&pool.workers[worker].assignment);
        let prompt = assignment_prompt(&pool.items[index], &pool.pool_id);
        let task_id = pool.workers[worker].task_id.clone();
        let revived = write_assignment(&file, &assignment)
            .map_err(|e| e.to_string())
            .and_then(|()| self.registry.revive_task(&task_id, &prompt));
        match revived {
            Ok(run_epoch) => {
                pool.workers[worker].status = WorkerStatus::Busy;
                let item = &mut pool.items[index];
                item.status = ItemStatus::Assigned;
                item.binding = Some(Binding {
                    task_id,
                    run_epoch,
                    generation: pool.generation,
                });
            }
            // The session cannot take another item: retire the worker; the
            // item stays queued for another one.
            Err(_) => pool.workers[worker].status = WorkerStatus::Retired,
        }
    }

    /// Deliver the aggregate once the pool is finished (exactly one attempt
    /// per generation; a deferred one stays pending).
    async fn maybe_finish(self: &Arc<Self>, pool_id: &str) {
        let (session, message, generation) = {
            let mut pools = self.lock();
            let Some(pool) = pools.get_mut(pool_id) else {
                return;
            };
            if !pool.finished() {
                return;
            }
            let attempted = pool.aggregate.as_ref().is_some_and(|a| {
                a.generation == pool.generation && a.delivery != DeliveryState::Pending
            });
            if attempted {
                return;
            }
            pool.aggregate = Some(Aggregate {
                generation: pool.generation,
                delivery: DeliveryState::Delivering,
                error: None,
            });
            self.persist(pool);
            (
                pool.parent_session_id.clone(),
                pool.aggregate_message(),
                pool.generation,
            )
        };
        let outcome = (self.deliverer)(session, message).await;
        {
            let mut pools = self.lock();
            if let Some(pool) = pools.get_mut(pool_id) {
                if let Some(aggregate) = pool
                    .aggregate
                    .as_mut()
                    .filter(|a| a.generation == generation)
                {
                    match outcome {
                        Delivery::Delivered => aggregate.delivery = DeliveryState::Delivered,
                        Delivery::Deferred => aggregate.delivery = DeliveryState::Pending,
                        Delivery::Failed(error) => {
                            aggregate.delivery = DeliveryState::Uncertain;
                            aggregate.error = Some(error);
                        }
                    }
                }
                self.persist(pool);
            }
        }
        self.bump();
    }
}

/// Apply pushed items to `items`: a new key is queued with a fresh id, a
/// known key with the same (canonical) input returns its id, a known key
/// with different input refuses the whole push.
fn stage_items(items: &mut Vec<Item>, pushed: Vec<(String, Value)>) -> Result<Vec<Value>, WpError> {
    let mut staged = items.clone();
    let mut receipts = Vec::with_capacity(pushed.len());
    for (key, input) in pushed {
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(invalid_input());
        }
        let item_id = if let Some(prior) = staged.iter().find(|i| i.key == key) {
            if canonical_json(&prior.input) != canonical_json(&input) {
                return Err(wp_error(
                    "duplicate_key_conflict",
                    "The key already identifies different input.",
                ));
            }
            prior.item_id.clone()
        } else {
            let item_id = new_id("wi_");
            staged.push(Item {
                key: key.clone(),
                input,
                item_id: item_id.clone(),
                status: ItemStatus::Queued,
                binding: None,
                data: None,
                error: None,
                yield_sha256: None,
            });
            item_id
        };
        receipts.push(json!({"key": key, "item_id": item_id}));
    }
    *items = staged;
    Ok(receipts)
}

fn assignment_prompt(item: &Item, pool_id: &str) -> String {
    let input = serde_json::to_string_pretty(&item.input).unwrap_or_default();
    let example = json!({"op": "yield", "results": [{"key": item.key, "data": "<result>"}]});
    format!(
        "<workpool-assignment pool_id=\"{pool_id}\" item_id=\"{}\" key=\"{}\">\n{input}\n</workpool-assignment>\n\n{TURN_MESSAGE}\nYield with workpool {example}, or {{\"key\": ..., \"error\": {{\"code\": ..., \"message\": ...}}}} when the item cannot be done.",
        item.item_id, item.key
    )
}

/// Settle one assigned item from its yield, or record why it has none.
fn resolve_item(item: &mut Item, yields: &Path, worker: &TaskView) {
    let generation = item.binding.as_ref().map_or(0, |b| b.generation);
    let record = std::fs::read_to_string(yields.join(format!("{}.json", item.item_id)))
        .ok()
        .and_then(|text| serde_json::from_str::<YieldRecord>(&text).ok())
        .filter(|y| y.generation == generation && y.key == item.key);
    if let Some(record) = record {
        item.yield_sha256 = Some(record.sha256);
        if let Some(error) = record.result.get("error") {
            item.status = ItemStatus::Error;
            item.error = Some(ItemError {
                code: error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        } else {
            item.status = ItemStatus::Completed;
            item.data = Some(record.result.get("data").cloned().unwrap_or(Value::Null));
        }
        return;
    }
    item.status = ItemStatus::Error;
    item.error = Some(match worker.status() {
        TaskStatus::Completed => ItemError {
            code: "item_missing_yield".to_string(),
            message: "Worker completed without a keyed yield for this item.".to_string(),
        },
        TaskStatus::Cancelled => ItemError {
            code: "cancelled".to_string(),
            message: "The worker was cancelled before it yielded this item.".to_string(),
        },
        status => ItemError {
            code: "item_missing_yield".to_string(),
            message: format!(
                "Worker ended {} without a keyed yield for this item: {}",
                status.as_str(),
                worker
                    .record
                    .error_message
                    .as_deref()
                    .unwrap_or("no error message")
            ),
        },
    });
}

/// `yield` from a worker: record each result for its assigned key.
///
/// # Errors
/// `invalid_input` for a malformed result, `store_corrupt` for an
/// unreadable assignment.
pub(crate) fn worker_yield(assignment: &Path, results: &[Value]) -> Result<Value, WpError> {
    let text = std::fs::read_to_string(assignment).map_err(|e| {
        wp_error(
            "store_corrupt",
            format!("Cannot read the workpool assignment: {e}"),
        )
    })?;
    let assignment: Assignment = serde_json::from_str(&text)
        .map_err(|e| wp_error("store_corrupt", format!("Bad workpool assignment: {e}")))?;
    if !assignment
        .items
        .iter()
        .all(|item| is_item_id(&item.item_id))
    {
        return Err(wp_error(
            "store_corrupt",
            "Bad workpool assignment: malformed item id.",
        ));
    }
    let mut parsed = Vec::with_capacity(results.len());
    for result in results {
        let obj = result.as_object().ok_or_else(invalid_input)?;
        let key = obj
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(invalid_input)?;
        let value = match (obj.get("data"), obj.get("error")) {
            (Some(data), None) if obj.len() == 2 => json!({"key": key, "data": data}),
            (None, Some(error)) if obj.len() == 2 => {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .filter(|c| !c.trim().is_empty());
                let message = error.get("message").and_then(Value::as_str);
                match (code, message) {
                    (Some(code), Some(message)) => {
                        json!({"key": key, "error": {"code": code, "message": message}})
                    }
                    _ => return Err(invalid_input()),
                }
            }
            _ => return Err(invalid_input()),
        };
        parsed.push((key.to_string(), value));
    }
    let dir = PathBuf::from(&assignment.yields_dir);
    let mut accepted = Vec::new();
    let mut refused = Vec::new();
    for (key, value) in parsed {
        let Some(item) = assignment.items.iter().find(|i| i.key == key) else {
            refused.push(json!({"key": key, "code": "worker_unassigned", "message": "Key is not assigned to this worker turn."}));
            continue;
        };
        let sha256 = hex_sha256(&canonical_json(&value));
        let path = dir.join(format!("{}.json", item.item_id));
        if let Some(prior) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<YieldRecord>(&t).ok())
            .filter(|p| p.generation == assignment.generation)
        {
            if prior.sha256 != sha256 {
                refused.push(json!({"key": key, "code": "yield_conflict", "message": "This item already has a different terminal yield."}));
                continue;
            }
        } else {
            let record = YieldRecord {
                pool_id: assignment.pool_id.clone(),
                generation: assignment.generation,
                item_id: item.item_id.clone(),
                key: key.clone(),
                result: value,
                sha256,
            };
            let text = serde_json::to_string(&record)
                .map_err(|e| wp_error("store_corrupt", e.to_string()))?;
            write_atomic(&dir, &format!("{}.json", item.item_id), &text).map_err(|e| {
                wp_error("yield_unavailable", format!("Cannot record the yield: {e}"))
            })?;
        }
        accepted.push(json!({"key": key, "item_id": item.item_id}));
    }
    Ok(json!({"pool_id": assignment.pool_id, "accepted": accepted, "refused": refused}))
}

/// JSON with object keys sorted at every level (key order never makes two
/// inputs differ).
fn canonical_json(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

fn hex_sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `<prefix><32 hex>`: a freshly keyed SipHash pair over time, a sequence
/// and the pid (unpredictable across processes, distinct within one).
fn new_id(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let mut halves = [0u64; 2];
    for half in &mut halves {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.write_u64(SEQUENCE.fetch_add(1, Ordering::Relaxed));
        hasher.write_u32(std::process::id());
        *half = hasher.finish();
    }
    format!("{prefix}{:016x}{:016x}", halves[0], halves[1])
}

fn is_pool_id(value: &str) -> bool {
    value.strip_prefix("wp_").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn write_atomic(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    // Owner-only: pool records and yields carry worker input and output.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(&temp)?, text.as_bytes())?;
    std::fs::rename(&temp, dir.join(name))
}

fn is_item_id(value: &str) -> bool {
    value.strip_prefix("wi_").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn write_assignment(file: &Path, assignment: &Assignment) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let name = file.file_name().map_or_else(
        || "assignment.json".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let text = serde_json::to_string(assignment).map_err(std::io::Error::other)?;
    write_atomic(dir, &name, &text)
}

/// The fields each op accepts (strict: a field of another op is refused).
fn allowed_fields(op: &str) -> Option<&'static [&'static str]> {
    Some(match op {
        "create" => &["op", "name", "agent", "mode", "tools"],
        "push" => &["op", "pool_id", "items"],
        "close" | "inspect" | "cancel" => &["op", "pool_id"],
        "yield" => &["op", "results"],
        _ => return None,
    })
}

fn nonempty_field(obj: &Map<String, Value>, field: &str) -> Result<Option<String>, WpError> {
    match obj.get(field) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => Err(invalid_input()),
    }
}

fn parse_agent(value: Option<&Value>) -> Result<AgentSpec, WpError> {
    let obj = value.and_then(Value::as_object).ok_or_else(invalid_input)?;
    if obj.keys().any(|k| {
        !matches!(
            k.as_str(),
            "category" | "subagent_type" | "prompt" | "model"
        )
    }) {
        return Err(invalid_input());
    }
    let category = nonempty_field(obj, "category")?;
    let subagent_type = nonempty_field(obj, "subagent_type")?;
    if category.is_some() == subagent_type.is_some() {
        return Err(invalid_input());
    }
    Ok(AgentSpec {
        category,
        subagent_type,
        prompt: nonempty_field(obj, "prompt")?.ok_or_else(invalid_input)?,
        model: nonempty_field(obj, "model")?,
    })
}

fn pool_id_of(obj: &Map<String, Value>) -> Result<String, WpError> {
    let id = obj
        .get("pool_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid_input)?;
    if !is_pool_id(id) {
        return Err(wp_error(
            "invalid_pool_id",
            "Expected a wp_ pool identifier.",
        ));
    }
    Ok(id.to_string())
}

/// Run one `workpool` call. `assignment` is the worker's assignment file
/// when this session is a pool worker; the engine is only built for parent
/// operations.
pub(crate) async fn run(
    engine: impl FnOnce() -> Arc<WorkpoolEngine>,
    caller: &Caller,
    assignment: Option<&Path>,
    args: &Value,
) -> Result<Value, WpError> {
    let obj = args.as_object().ok_or_else(invalid_input)?;
    let op = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(invalid_input)?;
    let allowed = allowed_fields(op).ok_or_else(invalid_input)?;
    if obj.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid_input());
    }
    if op == "yield" {
        let Some(assignment) = assignment else {
            return Err(wp_error(
                "yield_unavailable",
                "Only workpool workers yield; this session has no workpool assignment.",
            ));
        };
        let results = obj
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(invalid_input)?;
        return worker_yield(assignment, results);
    }
    if assignment.is_some() {
        return Err(wp_error(
            "scope_denied",
            "Assigned workers may only yield their current assignment.",
        ));
    }
    let engine = engine();
    match op {
        "create" => {
            let mode = match obj.get("mode").and_then(Value::as_str) {
                None if obj.get("mode").is_none() => None,
                Some("fresh") => Some(Mode::Fresh),
                Some("keep_alive") => Some(Mode::KeepAlive),
                _ => return Err(invalid_input()),
            };
            let tools: Option<Vec<String>> = match obj.get("tools") {
                None => None,
                Some(Value::Array(items)) => Some(
                    items
                        .iter()
                        .map(|t| t.as_str().map(str::to_string).ok_or_else(invalid_input))
                        .collect::<Result<_, _>>()?,
                ),
                Some(_) => return Err(invalid_input()),
            };
            engine.create(
                caller,
                nonempty_field(obj, "name")?,
                parse_agent(obj.get("agent"))?,
                mode,
                tools.as_deref(),
            )
        }
        "push" => {
            let pool_id = pool_id_of(obj)?;
            let items = obj
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(invalid_input)?
                .iter()
                .map(|item| {
                    let fields = item.as_object().ok_or_else(invalid_input)?;
                    if fields.len() != 2 {
                        return Err(invalid_input());
                    }
                    let key = fields
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid_input)?;
                    let input = fields.get("input").ok_or_else(invalid_input)?;
                    Ok((key.to_string(), input.clone()))
                })
                .collect::<Result<Vec<_>, WpError>>()?;
            engine.push(caller, &pool_id, items)
        }
        "inspect" => engine.inspect(caller, &pool_id_of(obj)?),
        "close" => engine.close(caller, &pool_id_of(obj)?).await,
        "cancel" => engine.cancel(caller, &pool_id_of(obj)?).await,
        _ => Err(invalid_input()),
    }
}

fn render(outcome: Result<Value, WpError>) -> AgentToolResult {
    match outcome {
        Ok(value) => {
            let mut result =
                AgentToolResult::text(serde_json::to_string_pretty(&value).unwrap_or_default());
            result.details = value;
            result
        }
        Err(e) => {
            let mut result = AgentToolResult::text(format!("{}: {}", e.code, e.message));
            result.details = json!({"code": e.code, "message": e.message});
            result.is_error = Some(true);
            result
        }
    }
}

pub async fn workpool(env: Arc<Env>, args: Value) -> Result<AgentToolResult> {
    let session_id = super::subagents::caller_session();
    let caller = Caller {
        root_session_id: super::subagents::root_session(&session_id),
        session_id,
        cwd: env.cwd.clone(),
    };
    let assignment = std::env::var_os(ASSIGNMENT_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    Ok(render(
        run(engine, &caller, assignment.as_deref(), &args).await,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// Fake `--mode rpc` worker: on each prompt it reads its assignment file
    /// and writes a yield `done:<key>` for it, except that a `skip` key gets
    /// no yield and a `hold` key never settles (until cancelled).
    const FAKE_WORKER: &str = r#"
[ "$1" = "--mode" ] && [ "$2" = "rpc" ] || exit 2
sess="$(dirname "$0")/session-$$.jsonl"
: > "$sess"
holding=""
field() { sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p" "$1"; }
while IFS= read -r line; do
  case "$line" in
    *'"type":"get_state"'*) printf '{"type":"response","command":"get_state","success":true,"data":{"sessionFile":"%s"}}\n' "$sess" ;;
    *'"type":"switch_session"'*) echo '{"type":"response","command":"switch_session","success":true,"data":{"cancelled":false}}' ;;
    *'"type":"prompt"'*)
      echo '{"type":"response","command":"prompt","success":true}'
      a="$OMP_WORKPOOL_ASSIGNMENT"
      item=$(field "$a" item_id); key=$(field "$a" key); pool=$(field "$a" pool_id); dir=$(field "$a" yields_dir)
      gen=$(sed -n 's/.*"generation":\([0-9]*\).*/\1/p' "$a")
      echo "$key" >> "$(dirname "$0")/worked-$$"
      case "$key" in
        *hold*) holding=1 ;;
        *skip*) echo '{"type":"prompt_result","status":"completed","sessionSettled":true}' ;;
        *)
          mkdir -p "$dir"
          printf '{"pool_id":"%s","generation":%s,"item_id":"%s","key":"%s","result":{"key":"%s","data":"done:%s"},"sha256":"x"}' "$pool" "$gen" "$item" "$key" "$key" "$key" > "$dir/$item.json"
          echo '{"type":"prompt_result","status":"completed","sessionSettled":true}' ;;
      esac ;;
    *'"type":"get_last_assistant_text"'*) echo '{"type":"response","command":"get_last_assistant_text","success":true,"data":{"text":"ok"}}' ;;
    *'"type":"get_session_stats"'*) echo '{"type":"response","command":"get_session_stats","success":true,"data":{"tokens":{"output":1},"cost":0}}' ;;
  esac
done
"#;

    struct Harness {
        _dir: tempfile::TempDir,
        engine: Arc<WorkpoolEngine>,
        delivered: Arc<Mutex<Vec<(String, String)>>>,
        caller: Caller,
    }

    fn harness(limit: usize, outcome: Delivery) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-omp-rs");
        let mut f = std::fs::File::create(&bin).unwrap();
        writeln!(f, "#!/bin/sh\n{FAKE_WORKER}").unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        let registry =
            SubagentRegistry::new(TaskRunner::new(bin, 8), Some(dir.path().join("tasks")));
        let delivered: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let sink = Arc::clone(&delivered);
        let deliverer: Deliverer = Arc::new(move |session, message| {
            sink.lock().unwrap().push((session, message));
            let outcome = outcome.clone();
            Box::pin(async move { outcome })
        });
        let engine = WorkpoolEngine::new(
            Arc::clone(&registry),
            dir.path().join("workpools"),
            limit,
            deliverer,
        );
        registry.set_listener(engine.settle_forwarder());
        let caller = Caller {
            session_id: "parent".to_string(),
            root_session_id: "parent".to_string(),
            cwd: dir.path().to_path_buf(),
        };
        Harness {
            _dir: dir,
            engine,
            delivered,
            caller,
        }
    }

    impl Harness {
        async fn call(&self, args: Value) -> Result<Value, WpError> {
            let engine = Arc::clone(&self.engine);
            run(move || engine, &self.caller, None, &args).await
        }

        async fn create(&self, mode: &str) -> String {
            let pool = self
                .call(json!({"op": "create", "agent": {"subagent_type": "task", "prompt": "Handle the item."}, "mode": mode}))
                .await
                .unwrap();
            pool["pool_id"].as_str().unwrap().to_string()
        }

        /// Wait (bounded) until `cond` holds on the pool's inspect view.
        async fn until(&self, pool_id: &str, cond: impl Fn(&Value) -> bool) -> Value {
            let mut rx = self.engine.subscribe();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let view = self.engine.inspect(&self.caller, pool_id).unwrap();
                    if cond(&view) {
                        return view;
                    }
                    rx.changed().await.expect("engine alive");
                }
            })
            .await
            .expect("pool state reached within 30s")
        }
    }

    fn delivered_aggregate(view: &Value) -> bool {
        view["aggregate"]["delivery"] == "delivered"
    }

    #[test]
    fn staging_dedupes_a_key_and_refuses_changed_input() {
        let mut items = Vec::new();
        let first =
            stage_items(&mut items, vec![("a".into(), json!({"x": 1, "y": [1, 2]}))]).unwrap();
        let again = stage_items(
            &mut items,
            vec![(" a ".into(), json!({"y": [1, 2], "x": 1}))],
        )
        .unwrap();
        assert_eq!(
            first[0]["item_id"], again[0]["item_id"],
            "same key + same input = same id"
        );
        assert_eq!(items.len(), 1);
        let conflict = stage_items(
            &mut items,
            vec![("b".into(), json!(1)), ("a".into(), json!({"x": 2}))],
        )
        .unwrap_err();
        assert_eq!(conflict.code, "duplicate_key_conflict");
        assert_eq!(items.len(), 1, "a refused push changes nothing");
        assert_eq!(
            stage_items(&mut items, vec![(" ".into(), json!(1))])
                .unwrap_err()
                .code,
            "invalid_input"
        );
        let id = first[0]["item_id"].as_str().unwrap();
        assert!(id.starts_with("wi_") && id.len() == 35, "{id}");
    }

    #[tokio::test]
    async fn fresh_pool_runs_every_item_and_delivers_one_aggregate() {
        let h = harness(2, Delivery::Delivered);
        let pool = h.create("fresh").await;
        assert!(is_pool_id(&pool), "{pool}");
        let pushed = h
            .call(json!({"op": "push", "pool_id": pool, "items": [{"key": "a", "input": 1}, {"key": "b", "input": 2}, {"key": "c", "input": 3}]}))
            .await
            .unwrap();
        assert_eq!(pushed["item_ids"].as_array().unwrap().len(), 3);
        let early = h.engine.inspect(&h.caller, &pool).unwrap();
        assert!(
            early["counts"]["assigned"].as_u64().unwrap() <= 2,
            "the limit holds: {early}"
        );
        h.call(json!({"op": "close", "pool_id": pool}))
            .await
            .unwrap();
        let done = h.until(&pool, delivered_aggregate).await;
        assert_eq!(done["counts"]["completed"], 3);
        let items = done["items"].as_array().unwrap();
        assert_eq!(items[0]["data"], "done:a");
        assert_eq!(items[2]["data"], "done:c");
        let workers: std::collections::BTreeSet<&str> = items
            .iter()
            .map(|i| i["task_id"].as_str().unwrap())
            .collect();
        assert_eq!(workers.len(), 3, "fresh mode: one worker per item");
        let delivered = h.delivered.lock().unwrap().clone();
        assert_eq!(delivered.len(), 1, "exactly one aggregate");
        assert_eq!(delivered[0].0, "parent");
        assert!(delivered[0].1.contains("\"done:b\""), "{}", delivered[0].1);
        let pushed_late = h
            .call(json!({"op": "push", "pool_id": pool, "items": [{"key": "d", "input": 4}]}))
            .await;
        assert_eq!(pushed_late.unwrap_err().code, "pool_closed");
    }

    #[tokio::test]
    async fn keep_alive_revives_one_worker_for_successive_items() {
        let h = harness(1, Delivery::Delivered);
        let pool = h.create("keep_alive").await;
        h.call(json!({"op": "push", "pool_id": pool, "items": [{"key": "first", "input": "1"}, {"key": "second", "input": "2"}]}))
            .await
            .unwrap();
        h.call(json!({"op": "close", "pool_id": pool}))
            .await
            .unwrap();
        let done = h.until(&pool, delivered_aggregate).await;
        let items = done["items"].as_array().unwrap();
        assert_eq!(
            items[0]["task_id"], items[1]["task_id"],
            "one worker, revived"
        );
        assert_eq!(items[1]["data"], "done:second");
        let worker = &done["workers"][0];
        assert_eq!(worker["completed_turns"], 2);
        assert_eq!(done["workers"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_yields_are_kept_per_key_and_delivery_is_never_retried() {
        let h = harness(2, Delivery::Failed("session gone".to_string()));
        let pool = h.create("fresh").await;
        h.call(json!({"op": "push", "pool_id": pool, "items": [{"key": "fine", "input": 1}, {"key": "skip-me", "input": 2}]}))
            .await
            .unwrap();
        h.call(json!({"op": "close", "pool_id": pool}))
            .await
            .unwrap();
        let done = h
            .until(&pool, |v| v["aggregate"]["delivery"] == "uncertain")
            .await;
        assert_eq!(done["items"][0]["data"], "done:fine");
        assert_eq!(done["items"][1]["status"], "error");
        assert_eq!(done["items"][1]["error"]["code"], "item_missing_yield");
        assert_eq!(done["aggregate"]["error"], "session gone");
        h.engine.flush_aggregates("parent").await;
        assert_eq!(
            h.delivered.lock().unwrap().len(),
            1,
            "an uncertain delivery is not retried"
        );
        assert!(h.delivered.lock().unwrap()[0]
            .1
            .contains("item_missing_yield"));
        assert_eq!(h.engine.open_work("parent"), 0);
    }

    #[tokio::test]
    async fn cancel_ends_queued_and_running_items() {
        let h = harness(1, Delivery::Delivered);
        let pool = h.create("fresh").await;
        h.call(json!({"op": "push", "pool_id": pool, "items": [{"key": "hold", "input": 1}, {"key": "later", "input": 2}]}))
            .await
            .unwrap();
        let busy = h.until(&pool, |v| v["counts"]["assigned"] == 1).await;
        assert_eq!(busy["counts"]["queued"], 1);
        assert_eq!(h.engine.open_work("parent"), 1, "the queued item counts");
        let cancelled = h
            .call(json!({"op": "cancel", "pool_id": pool}))
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["counts"]["cancelled"], 2);
        assert_eq!(cancelled["items"][0]["error"]["code"], "cancelled");
        let done = h.until(&pool, delivered_aggregate).await;
        assert_eq!(done["generation"], 2);
        assert_eq!(h.delivered.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refusals_use_the_engine_vocabulary() {
        let h = harness(2, Delivery::Delivered);
        let pool = h.create("fresh").await;
        let other = Caller {
            session_id: "intruder".to_string(),
            ..h.caller.clone()
        };
        let engine = Arc::clone(&h.engine);
        let denied = run(
            move || engine,
            &other,
            None,
            &json!({"op": "inspect", "pool_id": pool}),
        )
        .await;
        assert_eq!(denied.unwrap_err().code, "scope_denied");
        let bad_id = h.call(json!({"op": "inspect", "pool_id": "wp_nope"})).await;
        assert_eq!(bad_id.unwrap_err().code, "invalid_pool_id");
        let unknown = h
            .call(json!({"op": "inspect", "pool_id": format!("wp_{}", "0".repeat(32))}))
            .await;
        assert_eq!(unknown.unwrap_err().code, "pool_not_found");
        let cross = h
            .call(json!({"op": "inspect", "pool_id": pool, "name": "x"}))
            .await;
        assert_eq!(cross.unwrap_err().code, "invalid_input");
        let both = h
            .call(json!({"op": "create", "agent": {"category": "quick", "subagent_type": "task", "prompt": "p"}}))
            .await;
        assert_eq!(both.unwrap_err().code, "invalid_input");
        let tools = h
            .call(json!({"op": "create", "agent": {"category": "quick", "prompt": "p"}, "tools": ["x"]}))
            .await;
        assert_eq!(tools.unwrap_err().code, "tools_unavailable");
        h.call(
            json!({"op": "create", "name": "named", "agent": {"category": "quick", "prompt": "p"}}),
        )
        .await
        .unwrap();
        let taken = h
            .call(json!({"op": "create", "name": "named", "agent": {"category": "quick", "prompt": "p"}}))
            .await;
        assert_eq!(taken.unwrap_err().code, "pool_name_conflict");
        let no_assignment = h.call(json!({"op": "yield", "results": []})).await;
        assert_eq!(no_assignment.unwrap_err().code, "yield_unavailable");
        let worker_file = h.caller.cwd.join("assignment.json");
        let engine = Arc::clone(&h.engine);
        let as_worker = run(
            move || engine,
            &h.caller,
            Some(&worker_file),
            &json!({"op": "inspect", "pool_id": pool}),
        )
        .await;
        assert_eq!(as_worker.unwrap_err().code, "scope_denied");
    }

    #[test]
    fn worker_yield_records_assigned_keys_and_refuses_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let item_id = format!("wi_{}", "1".repeat(32));
        let assignment = Assignment {
            pool_id: format!("wp_{}", "a".repeat(32)),
            generation: 1,
            yields_dir: dir.path().join("yields").display().to_string(),
            items: vec![AssignedItem {
                item_id: item_id.clone(),
                key: "a".to_string(),
            }],
        };
        let file = dir.path().join("assign.json");
        write_assignment(&file, &assignment).unwrap();
        let out = worker_yield(
            &file,
            &[
                json!({"key": "a", "data": {"n": 1}}),
                json!({"key": "z", "data": 1}),
            ],
        )
        .unwrap();
        assert_eq!(out["accepted"][0]["item_id"], item_id.as_str());
        assert_eq!(out["refused"][0]["code"], "worker_unassigned");
        let same = worker_yield(&file, &[json!({"key": "a", "data": {"n": 1}})]).unwrap();
        assert_eq!(
            same["accepted"].as_array().unwrap().len(),
            1,
            "an identical yield is idempotent"
        );
        let changed = worker_yield(
            &file,
            &[json!({"key": "a", "error": {"code": "x", "message": "m"}})],
        )
        .unwrap();
        assert_eq!(changed["refused"][0]["code"], "yield_conflict");
        let both = worker_yield(
            &file,
            &[json!({"key": "a", "data": 1, "error": {"code": "x", "message": "m"}})],
        );
        assert_eq!(both.unwrap_err().code, "invalid_input");
        let stored: YieldRecord = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(format!("yields/{item_id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.result["data"], json!({"n": 1}));
    }

    #[tokio::test]
    async fn a_persisted_pool_reads_back_after_the_process_lets_go() {
        let h = harness(2, Delivery::Delivered);
        let pool = h.create("fresh").await;
        let fresh = WorkpoolEngine::new(
            SubagentRegistry::new(TaskRunner::new("/nonexistent", 1), None),
            h.engine.store.clone(),
            2,
            Arc::new(|_, _| Box::pin(async { Delivery::Deferred })),
        );
        let view = fresh.inspect(&h.caller, &pool).unwrap();
        assert_eq!(view["pool_id"], pool);
        assert_eq!(view["mode"], "fresh");
    }

    #[test]
    fn schema_is_strict_like_the_engine_contract() {
        let s = schema();
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(
            s["properties"]["op"]["enum"],
            json!(["create", "push", "close", "inspect", "cancel", "yield"])
        );
        assert_eq!(s["properties"]["pool_id"]["pattern"], "^wp_[0-9a-f]{32}$");
        assert_eq!(
            s["properties"]["mode"]["enum"],
            json!(["fresh", "keep_alive"])
        );
        assert_eq!(
            s["properties"]["agent"]["oneOf"].as_array().unwrap().len(),
            2
        );
    }
}
