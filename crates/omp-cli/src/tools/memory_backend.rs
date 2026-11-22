//! [`MemoryToolBackend`] over the real `omp-memory` store.
//!
//! [`StoreBackend`] puts the six memory tools on the same durable Mnemopi
//! runtime the session publishes behind `memory://`, instead of the in-process
//! [`InMemoryBackend`](super::memory_tools::InMemoryBackend). [`session_tools`]
//! is the one entry point a session needs: it returns nothing while
//! `memory.backend` is off, and otherwise the gated tool set.
//!
//! The store is narrower than the trait in a few places, and each gap is
//! reported rather than papered over:
//!
//! * **Forgotten** rows are deleted by the store, so a forgotten id reads back
//!   as absent (`get` -> `None`, a second `forget` -> `NotFound`), never as a
//!   `Forgotten` record.
//! * **Invalidated** rows keep their text and carry `superseded_by`; the store
//!   writes the row's own id there when no replacement was named, which maps
//!   back to `replacement_id: None`.
//! * **Scope**: the runtime writes only the project's retain bank, so a
//!   `global` retain is an error. On read a row counts as `Global` when it
//!   lives in the shared default bank.
//! * **Editing**: `update` and `forget` only reach working memory. An episodic
//!   row can be invalidated but not rewritten or deleted, and an extracted
//!   fact is immutable (`NotEditable`).
//! * The store keeps one timestamp, so `updated_at` equals `created_at`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use omp_memory::bank::DEFAULT_SHARED_BANK;
use omp_memory::recall::RecallBounds;
use omp_memory::runtime::{
    EditOperation, EditStatus, MemoryProjection, RuntimeStart, MAX_PROJECTION_BYTES,
};
use omp_memory::store::{MemoryRecord, MemoryTier};
use omp_memory::{MemoryBackend, MemoryRuntime, MnemopiSettings};
use pa_agent::types::AgentTool;
use serde_json::Value;

use super::memory_tools::{
    self, EditOutcome, MemoryDraft, MemoryScope, MemoryStatus, MemoryToolBackend, RecallHit,
    RetainedMemory,
};
use super::Env;

/// Long-term memory tools backed by a live [`MemoryRuntime`].
pub struct StoreBackend {
    runtime: Arc<MemoryRuntime>,
}

impl StoreBackend {
    /// Wrap an already-started runtime.
    #[must_use]
    pub fn new(runtime: Arc<MemoryRuntime>) -> Self {
        Self { runtime }
    }

    /// Start the Mnemopi runtime for `cwd` the way the session does
    /// (`Runtime::publish_memory_urls`): default Mnemopi settings, data under
    /// `data_dir`.
    pub fn open(cwd: &Path, data_dir: &Path, session_id: String) -> Result<Self> {
        let runtime = MemoryRuntime::start(RuntimeStart {
            session_id,
            data_dir: data_dir.to_path_buf(),
            workspace_root: cwd.to_path_buf(),
            canonical_primary_root: None,
            backend: MemoryBackend::Mnemopi,
            mnemopi: MnemopiSettings::default(),
        })
        .map_err(|err| anyhow!("memory store could not be opened: {err}"))?;
        Ok(Self::new(runtime))
    }

    /// Resolve one record by id, whatever its tier or status. An id the store
    /// cannot hold (bad characters, `root`, empty) is simply not found.
    fn find(&self, id: &str) -> Result<Option<MemoryRecord>> {
        match self.runtime.projection(id, 1, MAX_PROJECTION_BYTES) {
            Ok(MemoryProjection::Record { record, .. }) => Ok(Some(record)),
            Ok(_) | Err(omp_memory::Error::InvalidIdentifier) => Ok(None),
            Err(err) => Err(anyhow!("memory lookup failed: {err}")),
        }
    }

    /// Shared gate for update/forget/invalidate: classify the target, then let
    /// the runtime apply the edit (cache and generation bookkeeping included).
    fn edit(
        &self,
        operation: EditOperation,
        id: &str,
        content: Option<&str>,
        importance: Option<f32>,
        replacement_id: Option<&str>,
    ) -> Result<EditOutcome> {
        let Some(record) = self.find(id)? else {
            return Ok(EditOutcome::NotFound);
        };
        if record.superseded_by.is_some() || record.tier == MemoryTier::Fact {
            return Ok(EditOutcome::NotEditable);
        }
        if record.tier == MemoryTier::Episodic && operation != EditOperation::Invalidate {
            bail!(
                "memory {id} is consolidated (episodic): the store can only {} working memory; use invalidate instead",
                operation.as_str()
            );
        }
        let outcome = self
            .runtime
            .edit(
                operation,
                id,
                content,
                importance.map(f64::from),
                replacement_id,
            )
            .map_err(|err| anyhow!("memory {} failed: {err}", operation.as_str()))?;
        Ok(match outcome.status {
            EditStatus::Updated => EditOutcome::Updated,
            EditStatus::Forgotten => EditOutcome::Forgotten,
            EditStatus::Invalidated => EditOutcome::Invalidated,
            EditStatus::NotEditable => EditOutcome::NotEditable,
            EditStatus::NotFound => EditOutcome::NotFound,
        })
    }
}

impl MemoryToolBackend for StoreBackend {
    fn retain(&self, draft: MemoryDraft) -> Result<String> {
        if draft.scope == MemoryScope::Global {
            bail!(
                "global-scope retain is not supported by the memory store: it writes only the project bank; retain with scope=project"
            );
        }
        let outcome = self
            .runtime
            .save(
                &draft.content,
                draft.source,
                f64::from(draft.importance),
                draft.context.as_deref(),
            )
            .map_err(|err| anyhow!("memory retain failed: {err}"))?;
        outcome.id.ok_or_else(|| {
            anyhow!(
                "{}",
                outcome
                    .message
                    .unwrap_or_else(|| omp_memory::INACTIVE_MESSAGE.to_owned())
            )
        })
    }

    fn recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // The tool layer owns the token budget; ask the store for its maximum.
        let bounds = RecallBounds {
            limit,
            token_budget: usize::MAX,
            ..RecallBounds::default()
        };
        let outcome = self
            .runtime
            .search(query, None, bounds)
            .map_err(|err| anyhow!("memory recall failed: {err}"))?;
        Ok(outcome
            .items
            .into_iter()
            .map(|item| RecallHit {
                id: item.memory.id.clone(),
                content: item.memory.content.clone(),
                context: context_of(&item.memory),
                importance: item.memory.importance as f32,
                score: item.score as f32,
                created_at: rfc3339_secs(&item.memory.timestamp),
            })
            .collect())
    }

    fn update(
        &self,
        id: &str,
        content: Option<&str>,
        importance: Option<f32>,
    ) -> Result<EditOutcome> {
        self.edit(EditOperation::Update, id, content, importance, None)
    }

    fn forget(&self, id: &str) -> Result<EditOutcome> {
        self.edit(EditOperation::Forget, id, None, None, None)
    }

    fn invalidate(&self, id: &str, replacement_id: Option<&str>) -> Result<EditOutcome> {
        self.edit(EditOperation::Invalidate, id, None, None, replacement_id)
    }

    fn get(&self, id: &str) -> Result<Option<RetainedMemory>> {
        Ok(self.find(id)?.map(|record| retained(&record)))
    }
}

/// Optional source context the runtime stores in the row's metadata.
fn context_of(record: &MemoryRecord) -> Option<String> {
    record
        .metadata
        .get("context")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn retained(record: &MemoryRecord) -> RetainedMemory {
    let created_at = rfc3339_secs(&record.timestamp);
    let status = match record.superseded_by.as_deref() {
        None => MemoryStatus::Active,
        // The store writes the row's own id when no replacement was named.
        Some(replacement) if replacement == record.id => MemoryStatus::Invalidated {
            replacement_id: None,
        },
        Some(replacement) => MemoryStatus::Invalidated {
            replacement_id: Some(replacement.to_owned()),
        },
    };
    RetainedMemory {
        id: record.id.clone(),
        content: record.content.clone(),
        context: context_of(record),
        scope: if record.bank.as_str() == DEFAULT_SHARED_BANK {
            MemoryScope::Global
        } else {
            MemoryScope::Project
        },
        importance: record.importance as f32,
        created_at,
        updated_at: created_at,
        status,
    }
}

/// Seconds since the Unix epoch of a store timestamp
/// (`YYYY-MM-DDTHH:MM:SS[.fff]Z`); `0` for anything else.
fn rfc3339_secs(timestamp: &str) -> u64 {
    let parse = || -> Option<u64> {
        let (date, time) = timestamp.trim().split_once('T')?;
        let time = time.strip_suffix('Z')?;
        let mut date = date.splitn(3, '-');
        let year: i64 = date.next()?.parse().ok()?;
        let month: i64 = date.next()?.parse().ok()?;
        let day: i64 = date.next()?.parse().ok()?;
        let mut time = time.split(':');
        let hour: i64 = time.next()?.parse().ok()?;
        let minute: i64 = time.next()?.parse().ok()?;
        let second: i64 = time.next()?.split('.').next()?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let secs = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
        u64::try_from(secs).ok()
    };
    parse().unwrap_or(0)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The memory tool set for one session: empty while `memory.backend` is off or
/// unset (or names an engine the store cannot serve), otherwise the tools
/// gated by `settings` over a store opened under the agent dir's `memory/`.
#[must_use]
pub fn session_tools(
    env: &Arc<Env>,
    cwd: &Path,
    settings: &omp_settings::Settings,
) -> Vec<Arc<dyn AgentTool>> {
    session_tools_in(
        env,
        cwd,
        settings,
        &omp_providers::AgentDir::default().0.join("memory"),
    )
}

/// [`session_tools`] with an explicit memory data directory.
fn session_tools_in(
    env: &Arc<Env>,
    cwd: &Path,
    settings: &omp_settings::Settings,
    data_dir: &Path,
) -> Vec<Arc<dyn AgentTool>> {
    let selected = settings
        .get_string("memory.backend")
        .and_then(|id| MemoryBackend::parse(&id))
        .unwrap_or(MemoryBackend::Off);
    if selected == MemoryBackend::Off {
        return Vec::new();
    }
    let session_id = format!("omt-tools-{}", std::process::id());
    match StoreBackend::open(cwd, data_dir, session_id) {
        Ok(backend) => memory_tools::tools(env, Arc::new(backend), settings),
        Err(err) => {
            tracing::warn!(%err, "memory tools unavailable: store failed to open");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Fixture {
        _cwd: tempfile::TempDir,
        _data: tempfile::TempDir,
        backend: StoreBackend,
    }

    fn fixture() -> Fixture {
        let cwd = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let backend = StoreBackend::open(cwd.path(), data.path(), "test-session".into()).unwrap();
        Fixture {
            _cwd: cwd,
            _data: data,
            backend,
        }
    }

    fn draft(content: &str) -> MemoryDraft {
        MemoryDraft {
            content: content.to_owned(),
            context: Some("ops notes".to_owned()),
            scope: MemoryScope::Project,
            importance: 0.8,
            source: "retain",
        }
    }

    fn settings(backend: Option<&str>) -> omp_settings::Settings {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yml");
        let body = backend.map_or_else(|| json!({}), |id| json!({"memory": {"backend": id}}));
        std::fs::write(&global, body.to_string()).unwrap();
        omp_settings::Settings::load_from(Some(&global), &dir.path().join("project"))
    }

    #[test]
    fn retain_then_recall_finds_it() {
        let f = fixture();
        let id = f
            .backend
            .retain(draft("the staging deploy key rotates quarterly"))
            .unwrap();

        let hits = f.backend.recall("staging deploy key", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].content, "the staging deploy key rotates quarterly");
        assert_eq!(hits[0].context.as_deref(), Some("ops notes"));
        assert!((hits[0].importance - 0.8).abs() < 1e-6);
        assert!(hits[0].created_at > 1_700_000_000);

        let stored = f
            .backend
            .get(&id)
            .unwrap()
            .expect("retained row is readable");
        assert_eq!(stored.status, MemoryStatus::Active);
        assert_eq!(stored.scope, MemoryScope::Project);
        assert_eq!(stored.context.as_deref(), Some("ops notes"));
    }

    #[test]
    fn recall_ignores_blank_queries_and_zero_limits() {
        let f = fixture();
        f.backend
            .retain(draft("the staging deploy key rotates quarterly"))
            .unwrap();
        assert!(f.backend.recall("   ", 5).unwrap().is_empty());
        assert!(f
            .backend
            .recall("staging deploy key", 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn forget_hides_it_from_recall() {
        let f = fixture();
        let id = f
            .backend
            .retain(draft("the staging deploy key rotates quarterly"))
            .unwrap();
        assert_eq!(f.backend.forget(&id).unwrap(), EditOutcome::Forgotten);

        assert!(f
            .backend
            .recall("staging deploy key", 5)
            .unwrap()
            .is_empty());
        // The store deletes the row: it is gone, not marked.
        assert!(f.backend.get(&id).unwrap().is_none());
        assert_eq!(f.backend.forget(&id).unwrap(), EditOutcome::NotFound);
    }

    #[test]
    fn invalidate_hides_it_and_records_the_replacement() {
        let f = fixture();
        let old = f
            .backend
            .retain(draft("the staging deploy key rotates quarterly"))
            .unwrap();
        let new = f
            .backend
            .retain(draft("the staging deploy key rotates monthly"))
            .unwrap();

        assert_eq!(
            f.backend.invalidate(&old, Some(&new)).unwrap(),
            EditOutcome::Invalidated
        );
        let hits = f.backend.recall("staging deploy key", 5).unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            vec![new.as_str()]
        );
        assert_eq!(
            f.backend.get(&old).unwrap().unwrap().status,
            MemoryStatus::Invalidated {
                replacement_id: Some(new)
            }
        );
        // Already invalidated: neither editable nor invalidatable again.
        assert_eq!(
            f.backend.update(&old, Some("x"), None).unwrap(),
            EditOutcome::NotEditable
        );
        assert_eq!(
            f.backend.invalidate(&old, None).unwrap(),
            EditOutcome::NotEditable
        );
    }

    #[test]
    fn invalidate_without_replacement_reads_back_as_none() {
        let f = fixture();
        let id = f
            .backend
            .retain(draft("the staging deploy key rotates quarterly"))
            .unwrap();
        assert_eq!(
            f.backend.invalidate(&id, None).unwrap(),
            EditOutcome::Invalidated
        );
        assert_eq!(
            f.backend.get(&id).unwrap().unwrap().status,
            MemoryStatus::Invalidated {
                replacement_id: None
            }
        );
    }

    #[test]
    fn update_rewrites_content_and_importance() {
        let f = fixture();
        let id = f
            .backend
            .retain(draft("the staging deploy key rotates quarterly"))
            .unwrap();
        assert_eq!(
            f.backend
                .update(
                    &id,
                    Some("the production vault token expires weekly"),
                    Some(0.25)
                )
                .unwrap(),
            EditOutcome::Updated
        );
        let stored = f.backend.get(&id).unwrap().unwrap();
        assert_eq!(stored.content, "the production vault token expires weekly");
        assert!((stored.importance - 0.25).abs() < 1e-6);
        assert_eq!(
            f.backend.recall("production vault token", 5).unwrap().len(),
            1
        );
        assert!(f
            .backend
            .recall("staging deploy key", 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unknown_or_malformed_ids_are_not_found() {
        let f = fixture();
        assert_eq!(
            f.backend.forget("mem_does_not_exist").unwrap(),
            EditOutcome::NotFound
        );
        assert_eq!(
            f.backend.update("a/b", Some("x"), None).unwrap(),
            EditOutcome::NotFound
        );
        assert_eq!(
            f.backend.invalidate("root", None).unwrap(),
            EditOutcome::NotFound
        );
        assert!(f.backend.get("").unwrap().is_none());
    }

    #[test]
    fn global_scope_retain_is_an_explicit_error() {
        let f = fixture();
        let mut global = draft("shared across every project");
        global.scope = MemoryScope::Global;
        let err = f.backend.retain(global).unwrap_err().to_string();
        assert!(err.contains("global-scope"), "{err}");
        assert!(f
            .backend
            .recall("shared across every project", 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn store_timestamps_parse_to_epoch_seconds() {
        assert_eq!(rfc3339_secs("1970-01-01T00:00:00.000Z"), 0);
        assert_eq!(rfc3339_secs("2023-11-14T22:13:20.123Z"), 1_700_000_000);
        assert_eq!(rfc3339_secs("2024-02-29T12:34:00Z"), 1_709_210_040);
        assert_eq!(rfc3339_secs("not a timestamp"), 0);
        assert_eq!(rfc3339_secs(""), 0);
    }

    #[test]
    fn session_tools_are_empty_when_the_backend_is_off_or_unset() {
        let cwd = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let env = Arc::new(Env::with_mode(cwd.path(), pi_edit::EditMode::Replace));
        for configured in [None, Some("off"), Some("local")] {
            let tools = session_tools_in(&env, cwd.path(), &settings(configured), data.path());
            assert!(tools.is_empty(), "{configured:?}");
        }
        // Off never opens the store.
        assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
    }

    #[test]
    fn session_tools_register_the_gated_set_over_the_store() {
        let cwd = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let env = Arc::new(Env::with_mode(cwd.path(), pi_edit::EditMode::Replace));
        let tools = session_tools_in(&env, cwd.path(), &settings(Some("mnemopi")), data.path());
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, vec!["retain", "recall", "reflect", "memory_edit"]);
        assert_ne!(std::fs::read_dir(data.path()).unwrap().count(), 0);
    }

    #[test]
    fn open_failure_leaves_the_session_without_memory_tools() {
        let cwd = tempfile::tempdir().unwrap();
        let blocker = tempfile::NamedTempFile::new().unwrap();
        let env = Arc::new(Env::with_mode(cwd.path(), pi_edit::EditMode::Replace));
        // A data dir that is a regular file cannot hold the store.
        let tools = session_tools_in(&env, cwd.path(), &settings(Some("mnemopi")), blocker.path());
        assert!(tools.is_empty());
    }
}
