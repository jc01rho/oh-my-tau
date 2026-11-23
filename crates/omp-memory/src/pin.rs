//! Session-pinned memory projection and turn-change `<memory_notice>` tracking.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::expiry::{render_expired_line, MemoryExpiry};
use crate::external_projection::locale_compare;
use crate::projection::now_ms;
use crate::repo::{MemoryRepo, RepoCommit, RepoError};

pub const PROJECTION_PIN_ENTRY_TYPE: &str = "omo-memory:projection-pin";
pub const MEMORY_NOTICE_CUSTOM_TYPE: &str = "omo-memory:notice";
pub const MEMORY_CHANGES_METADATA_TOKEN: &str =
    "Memory changed after this session's prompt was pinned to";
const MAX_LISTED_PATHS: usize = 20;
const UNKNOWN_DEVICE: &str = "another device";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionPinRecord {
    pub version: u32,
    pub session_id: String,
    pub revision: Option<String>,
    pub noticed_through: Option<String>,
    pub compaction_id: Option<String>,
    pub pinned_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_noticed_through_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppression_generation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionRepinReason {
    FirstTurn,
    Compaction,
    Refresh,
    MissingRevision,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectedChanges {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

impl ProjectedChanges {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionTurn {
    pub revision: Option<String>,
    pub as_of_ms: i64,
    pub expiry_noticed_through_ms: i64,
    pub now_ms: i64,
    pub changes: Option<ProjectedChanges>,
    pub repinned: Option<ProjectionRepinReason>,
    pub sources: Vec<String>,
}

struct ResolvedPin {
    compaction_id: Option<String>,
    state: Option<ProjectionPinRecord>,
    reason: Option<ProjectionRepinReason>,
}

pub struct NativeProjectionPins {
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    live: Mutex<BTreeMap<String, LivePin>>,
    /// Records whose journal append failed: retried verbatim, never re-selected.
    pending: Mutex<BTreeMap<String, ProjectionPinRecord>>,
    refresh_epoch: AtomicU64,
    refresh_requested_at_ms: Mutex<i64>,
}

impl Default for NativeProjectionPins {
    fn default() -> Self {
        Self::new()
    }
}

struct LivePin {
    record: ProjectionPinRecord,
    epoch: u64,
}

impl NativeProjectionPins {
    #[must_use]
    pub fn new() -> Self {
        Self::with_now(Box::new(now_ms))
    }

    #[must_use]
    pub fn with_now(now: Box<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            now,
            live: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(BTreeMap::new()),
            refresh_epoch: AtomicU64::new(0),
            refresh_requested_at_ms: Mutex::new(i64::MIN),
        }
    }

    /// Journal the record, or keep it for a verbatim retry when the append failed.
    fn record_or_defer(
        &self,
        session_id: &str,
        record: ProjectionPinRecord,
        writer: &dyn Fn(&ProjectionPinRecord) -> bool,
    ) {
        if writer(&record) {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(session_id);
            return;
        }
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_owned(), record);
    }

    /// Retry a record whose append failed earlier: the same revision and pinned time, so a
    /// restart restores what this session actually showed rather than the previous pin.
    fn flush_pending(
        &self,
        session_id: &str,
        writer: &dyn Fn(&ProjectionPinRecord) -> bool,
    ) {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned();
        let Some(record) = pending else {
            return;
        };
        if writer(&record) {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(session_id);
        }
    }

    pub fn request_refresh(&self) {
        self.refresh_epoch.fetch_add(1, Ordering::SeqCst);
        let now = (self.now)();
        *self
            .refresh_requested_at_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
    }

    fn store_live(&self, session_id: &str, record: ProjectionPinRecord) {
        let epoch = self.refresh_epoch.load(Ordering::SeqCst);
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_owned(), LivePin { record, epoch });
    }

    fn refresh_requested_at(&self) -> i64 {
        *self
            .refresh_requested_at_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn resolve(
        &self,
        repo: &MemoryRepo,
        session_id: &str,
        branch: &[Value],
    ) -> Result<ResolvedPin, RepoError> {
        let compaction_id = latest_compaction_id(branch);
        let cached = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(|live| LivePin {
                record: live.record.clone(),
                epoch: live.epoch,
            });
        let state = match &cached {
            Some(live) => Some(live.record.clone()),
            None => read_pin_record(branch, session_id),
        };
        let stale_by_refresh = match &cached {
            None => state
                .as_ref()
                .is_some_and(|s| s.pinned_at_ms < self.refresh_requested_at()),
            Some(live) => live.epoch < self.refresh_epoch.load(Ordering::SeqCst),
        };
        let reason = if stale_by_refresh {
            Some(ProjectionRepinReason::Refresh)
        } else {
            repin_reason(repo, state.as_ref(), compaction_id.as_deref())?
        };
        Ok(ResolvedPin {
            compaction_id,
            state,
            reason,
        })
    }

    /// Restore an existing pin on resume without advancing `noticed_through`, or create a
    /// fresh pin if none is valid (new session, fork, compaction, missing revision).
    ///
    /// # Errors
    /// Git lookup failed.
    pub fn ensure_pinned(
        &self,
        repo: &MemoryRepo,
        session_id: &str,
        branch: &[Value],
        record: &dyn Fn(&ProjectionPinRecord) -> bool,
    ) -> Result<ProjectionTurn, RepoError> {
        self.flush_pending(session_id, record);
        let ResolvedPin {
            compaction_id,
            state,
            reason,
        } = self.resolve(repo, session_id, branch)?;
        let now = (self.now)();
        if reason.is_none() {
            if let Some(pinned) = state {
                self.store_live(session_id, pinned.clone());
                return Ok(ProjectionTurn {
                    revision: pinned.revision,
                    as_of_ms: pinned.pinned_at_ms,
                    expiry_noticed_through_ms: pinned
                        .expiry_noticed_through_ms
                        .unwrap_or(pinned.pinned_at_ms),
                    now_ms: now,
                    changes: None,
                    repinned: None,
                    sources: Vec::new(),
                });
            }
        }
        let head = repo.head()?;
        let fresh = ProjectionPinRecord {
            version: 1,
            session_id: session_id.to_owned(),
            revision: head.clone(),
            noticed_through: head.clone(),
            compaction_id,
            pinned_at_ms: now,
            expiry_noticed_through_ms: None,
            suppression_generation: None,
        };
        self.store_live(session_id, fresh.clone());
        self.record_or_defer(session_id, fresh, record);
        Ok(ProjectionTurn {
            revision: head,
            as_of_ms: now,
            expiry_noticed_through_ms: now,
            now_ms: now,
            changes: None,
            repinned: Some(reason.unwrap_or(ProjectionRepinReason::FirstTurn)),
            sources: Vec::new(),
        })
    }

    /// Advance the pin for a new turn, detecting commits or repin conditions since the last turn.
    ///
    /// # Errors
    /// Git lookup failed.
    pub fn advance(
        &self,
        repo: &MemoryRepo,
        session_id: &str,
        branch: &[Value],
        record: &dyn Fn(&ProjectionPinRecord) -> bool,
    ) -> Result<ProjectionTurn, RepoError> {
        self.flush_pending(session_id, record);
        let head = repo.head()?;
        let ResolvedPin {
            compaction_id,
            state,
            reason,
        } = self.resolve(repo, session_id, branch)?;
        let now = (self.now)();
        let pinned = if reason.is_none() { state } else { None };
        let Some(state) = pinned else {
            let fresh = ProjectionPinRecord {
                version: 1,
                session_id: session_id.to_owned(),
                revision: head.clone(),
                noticed_through: head.clone(),
                compaction_id,
                pinned_at_ms: now,
                expiry_noticed_through_ms: None,
                suppression_generation: None,
            };
            self.store_live(session_id, fresh.clone());
            self.record_or_defer(session_id, fresh, record);
            return Ok(ProjectionTurn {
                revision: head,
                as_of_ms: now,
                expiry_noticed_through_ms: now,
                now_ms: now,
                changes: None,
                repinned: Some(reason.unwrap_or(ProjectionRepinReason::FirstTurn)),
                sources: Vec::new(),
            });
        };

        self.store_live(session_id, state.clone());
        if state.noticed_through == head {
            return Ok(ProjectionTurn {
                revision: state.revision,
                as_of_ms: state.pinned_at_ms,
                expiry_noticed_through_ms: state
                    .expiry_noticed_through_ms
                    .unwrap_or(state.pinned_at_ms),
                now_ms: now,
                changes: None,
                repinned: None,
                sources: Vec::new(),
            });
        }

        let (changes, commits) = projected_changes_between(
            repo,
            state.noticed_through.as_deref(),
            head.as_deref(),
            Some(session_id),
        )?;
        let advanced = ProjectionPinRecord {
            noticed_through: head,
            ..state.clone()
        };
        self.store_live(session_id, advanced.clone());
        self.record_or_defer(session_id, advanced, record);
        if changes.is_empty() {
            return Ok(ProjectionTurn {
                revision: state.revision,
                as_of_ms: state.pinned_at_ms,
                expiry_noticed_through_ms: state
                    .expiry_noticed_through_ms
                    .unwrap_or(state.pinned_at_ms),
                now_ms: now,
                changes: None,
                repinned: None,
                sources: Vec::new(),
            });
        }
        let sources = synced_sources(&commits, &changes);
        Ok(ProjectionTurn {
            revision: state.revision,
            as_of_ms: state.pinned_at_ms,
            expiry_noticed_through_ms: state
                .expiry_noticed_through_ms
                .unwrap_or(state.pinned_at_ms),
            now_ms: now,
            changes: Some(changes),
            repinned: None,
            sources,
        })
    }

    pub fn notice_expiries(
        &self,
        session_id: &str,
        through_ms: i64,
        record: &dyn Fn(&ProjectionPinRecord) -> bool,
    ) {
        let cached = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(|live| (live.record.clone(), live.epoch));
        let Some((cached, epoch)) = cached else {
            return;
        };
        let noticed = ProjectionPinRecord {
            expiry_noticed_through_ms: Some(through_ms),
            ..cached
        };
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                session_id.to_owned(),
                LivePin {
                    record: noticed.clone(),
                    epoch,
                },
            );
        self.record_or_defer(session_id, noticed, record);
    }

    /// Clear cached live pin for `session_id` (e.g. on `/clear` or reload).
    pub fn evict(&self, session_id: &str) {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
    }
}

fn repin_reason(
    repo: &MemoryRepo,
    state: Option<&ProjectionPinRecord>,
    compaction_id: Option<&str>,
) -> Result<Option<ProjectionRepinReason>, RepoError> {
    let Some(state) = state else {
        return Ok(Some(ProjectionRepinReason::FirstTurn));
    };
    if state.compaction_id.as_deref() != compaction_id {
        return Ok(Some(ProjectionRepinReason::Compaction));
    }
    // A live pin can lose its commits to a history rewrite and prune just like a journaled one:
    // reusing it would keep compiling a revision that no longer exists.
    for rev in [state.revision.as_deref(), state.noticed_through.as_deref()]
        .into_iter()
        .flatten()
    {
        if !repo.revision_exists(rev)? {
            return Ok(Some(ProjectionRepinReason::MissingRevision));
        }
    }
    Ok(None)
}

/// Read the newest pin record matching `session_id` from raw JSONL branch entries.
#[must_use]
pub fn read_pin_record(branch: &[Value], session_id: &str) -> Option<ProjectionPinRecord> {
    for entry in branch.iter().rev() {
        if entry.get("type").and_then(Value::as_str) != Some("custom") {
            continue;
        }
        if entry.get("customType").and_then(Value::as_str) != Some(PROJECTION_PIN_ENTRY_TYPE) {
            continue;
        }
        let Some(record) = entry.get("data").and_then(parse_pin_record) else {
            continue;
        };
        if record.session_id == session_id {
            return Some(record);
        }
    }
    None
}

fn parse_pin_record(value: &Value) -> Option<ProjectionPinRecord> {
    if !value.is_object() || value.get("version").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let session_id = value.get("sessionId").and_then(Value::as_str)?;
    let revision = nullable_string(value.get("revision"))?;
    let noticed_through = nullable_string(value.get("noticedThrough"))?;
    let compaction_id = nullable_string(value.get("compactionId"))?;
    let pinned_at_ms = parse_ms(value.get("pinnedAtMs")?)?;
    let suppression_generation = match value.get("suppressionGeneration") {
        None => None,
        Some(gen_val) => nullable_string(Some(gen_val))?,
    };
    Some(ProjectionPinRecord {
        version: 1,
        session_id: session_id.to_owned(),
        revision,
        noticed_through,
        compaction_id,
        pinned_at_ms,
        expiry_noticed_through_ms: value.get("expiryNoticedThroughMs").and_then(parse_ms),
        suppression_generation,
    })
}

fn parse_ms(value: &Value) -> Option<i64> {
    if let Some(i) = value.as_i64() {
        return Some(i);
    }
    let f = value.as_f64()?;
    if !f.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(f as i64)
}

fn nullable_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        _ => None,
    }
}

#[must_use]
pub fn latest_compaction_id(branch: &[Value]) -> Option<String> {
    for (index, entry) in branch.iter().enumerate().rev() {
        if entry.get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        return Some(
            entry
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| format!("index:{index}"), str::to_owned),
        );
    }
    None
}

/// Compare projected paths between `base` and `head`, excluding commits stamped with `exclude_session_id`.
///
/// # Errors
/// Git log or tree listing failed.
pub fn projected_changes_between(
    repo: &MemoryRepo,
    base: Option<&str>,
    head: Option<&str>,
    exclude_session_id: Option<&str>,
) -> Result<(ProjectedChanges, Vec<RepoCommit>), RepoError> {
    let Some(head) = head else {
        return Ok((ProjectedChanges::default(), Vec::new()));
    };
    if base == Some(head) {
        return Ok((ProjectedChanges::default(), Vec::new()));
    }
    let range = match base {
        None => head.to_owned(),
        Some(base) => format!("{base}..{head}"),
    };
    let commits = repo.log_commits(&range)?;
    let mut touched = BTreeSet::new();
    for commit in &commits {
        if let Some(exclude) = exclude_session_id {
            if commit.session_id.as_deref() == Some(exclude) {
                continue;
            }
        }
        for path in &commit.paths {
            if is_projected_path(path) {
                touched.insert(path.clone());
            }
        }
    }
    if touched.is_empty() {
        return Ok((ProjectedChanges::default(), commits));
    }

    let before = match base {
        None => Vec::new(),
        Some(base) => repo.ls_tree(base)?,
    };
    let after = repo.ls_tree(head)?;
    let before_set: HashSet<&str> = before.iter().map(String::as_str).collect();
    let after_set: HashSet<&str> = after.iter().map(String::as_str).collect();

    let mut touched: Vec<String> = touched.into_iter().collect();
    touched.sort_by(|a, b| locale_compare(a, b));
    let mut changes = ProjectedChanges::default();
    for path in &touched {
        let existed_before = before_set.contains(path.as_str());
        let exists_after = after_set.contains(path.as_str());
        if !existed_before && exists_after {
            changes.added.push(path.clone());
        } else if existed_before && !exists_after {
            changes.removed.push(path.clone());
        } else if existed_before && exists_after && is_system_body_path(path) {
            changes.updated.push(path.clone());
        }
    }
    Ok((changes, commits))
}

fn is_projected_path(path: &str) -> bool {
    if path.starts_with("skills/") {
        return false;
    }
    !path.starts_with("system/") || is_system_body_path(path)
}

fn is_system_body_path(path: &str) -> bool {
    path.starts_with("system/") && path.ends_with(".md")
}

#[must_use]
pub fn render_projected_changes_line(
    changes: &ProjectedChanges,
    pinned_revision: Option<&str>,
    sources: &[String],
) -> String {
    let groups = [
        ("added", &changes.added),
        ("updated", &changes.updated),
        ("removed", &changes.removed),
    ];
    let mut budget = MAX_LISTED_PATHS;
    let mut parts = Vec::new();
    for (verb, paths) in groups {
        if paths.is_empty() {
            continue;
        }
        let shown: Vec<String> = paths
            .iter()
            .take(budget)
            .map(|path| crate::projection::redact_secret_like_material(path))
            .collect();
        budget = budget.saturating_sub(shown.len());
        let hidden = paths.len() - shown.len();
        let mut listed = shown;
        if hidden > 0 {
            listed.push(format!("{hidden} more"));
        }
        parts.push(format!("{verb} {}", listed.join(", ")));
    }
    let pin = match pinned_revision {
        Some(revision) => revision.chars().take(7).collect::<String>(),
        None => "an empty memory repo".to_owned(),
    };
    let synced = if sources.is_empty() {
        String::new()
    } else {
        format!(", synced from {}", sources.join(", "))
    };
    format!(
        "- {MEMORY_CHANGES_METADATA_TOKEN} {pin}: {}{synced}. The prompt keeps the pinned copy; read a file when it matters now.",
        parts.join("; ")
    )
}

fn synced_sources(commits: &[RepoCommit], changes: &ProjectedChanges) -> Vec<String> {
    let listed: BTreeSet<&str> = changes
        .added
        .iter()
        .chain(&changes.updated)
        .chain(&changes.removed)
        .take(MAX_LISTED_PATHS)
        .map(String::as_str)
        .collect();
    let mut has_sync = false;
    for commit in commits {
        if !commit.sync_principals.is_empty()
            && commit
                .paths
                .iter()
                .any(|path| listed.contains(path.as_str()))
        {
            has_sync = true;
            break;
        }
    }
    if has_sync {
        vec![UNKNOWN_DEVICE.to_owned()]
    } else {
        Vec::new()
    }
}

/// Render the late `<memory_notice>` message, or `None` when nothing changed or expired.
#[must_use]
pub fn render_memory_notice(turn: &ProjectionTurn, expired: &[MemoryExpiry]) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(changes) = &turn.changes {
        lines.push(render_projected_changes_line(
            changes,
            turn.revision.as_deref(),
            &turn.sources,
        ));
    }
    if !expired.is_empty() {
        lines.push(render_expired_line(expired));
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push("<memory_notice>".to_owned());
    out.extend(lines);
    out.push("</memory_notice>".to_owned());
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::test_support::init_repo;

    fn commit_file(repo: &MemoryRepo, path: &str, body: &str) {
        let full = repo.root().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, format!("---\ndescription: {path}\n---\n{body}\n")).unwrap();
        repo.commit_paths(&[path.to_owned()], &format!("update {path}"))
            .unwrap();
    }

    fn pin_entry(record: &ProjectionPinRecord) -> Value {
        json!({
            "type": "custom",
            "id": "pin-1",
            "customType": PROJECTION_PIN_ENTRY_TYPE,
            "data": serde_json::to_value(record).unwrap(),
        })
    }

    #[test]
    fn pin_keeps_revision_and_announces_changes_once() {
        let root = init_repo("pin-changes");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "first");
        let pins = NativeProjectionPins::with_now(Box::new(|| 100));
        let recorded = RefCell::new(Vec::new());
        let first = pins
            .advance(&repo, "s1", &[], &|r| {
                recorded.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        assert_eq!(first.repinned, Some(ProjectionRepinReason::FirstTurn));
        assert!(first.changes.is_none());

        commit_file(&repo, "reference/new.md", "new");
        commit_file(&repo, "system/persona.md", "second");
        let second = pins
            .advance(&repo, "s1", &[], &|r| {
                recorded.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        assert_eq!(second.revision, first.revision);
        assert_eq!(second.repinned, None);
        let notice = render_memory_notice(&second, &[]).unwrap();
        assert!(notice.contains(MEMORY_CHANGES_METADATA_TOKEN));
        assert!(notice.contains("added reference/new.md"));
        assert!(notice.contains("updated system/persona.md"));

        let third = pins
            .advance(&repo, "s1", &[], &|r| {
                recorded.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        assert!(third.changes.is_none());
    }

    #[test]
    fn resume_reuses_pin_without_advancing_noticed_through_until_turn() {
        let root = init_repo("pin-resume");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "first");
        let original = NativeProjectionPins::with_now(Box::new(|| 42));
        let recorded = RefCell::new(Vec::new());
        let first = original
            .ensure_pinned(&repo, "s1", &[], &|r| {
                recorded.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        commit_file(&repo, "reference/later.md", "later");

        let resumed = NativeProjectionPins::with_now(Box::new(|| 99));
        let branch = [pin_entry(&recorded.borrow()[0])];
        let restored = resumed
            .ensure_pinned(&repo, "s1", &branch, &|_| true)
            .unwrap();
        assert_eq!(restored.revision, first.revision);
        assert_eq!(restored.as_of_ms, 42);
        assert_eq!(restored.repinned, None);

        let turn = resumed.advance(&repo, "s1", &branch, &|_| true).unwrap();
        assert_eq!(turn.revision, first.revision);
        assert_eq!(turn.changes.unwrap().added, vec!["reference/later.md"]);
    }

    #[test]
    fn compaction_recompile_and_vanished_revision_repin_to_head() {
        let root = init_repo("pin-repin");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "v1");
        let pins = NativeProjectionPins::with_now(Box::new(|| 1));
        let records = RefCell::new(Vec::new());
        pins.advance(&repo, "s1", &[], &|r| {
            records.borrow_mut().push(r.clone());
            true
        })
            .unwrap();
        commit_file(&repo, "system/persona.md", "v2");

        let compact_branch = [json!({"type": "compaction", "id": "c1"})];
        let after_compact = pins
            .advance(&repo, "s1", &compact_branch, &|r| {
                records.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        assert_eq!(
            after_compact.repinned,
            Some(ProjectionRepinReason::Compaction)
        );
        assert_eq!(after_compact.revision, repo.head().unwrap());

        commit_file(&repo, "system/persona.md", "v3");
        pins.request_refresh();
        let after_refresh = pins
            .advance(&repo, "s1", &compact_branch, &|r| {
                records.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        assert_eq!(after_refresh.repinned, Some(ProjectionRepinReason::Refresh));
        assert_eq!(after_refresh.revision, repo.head().unwrap());

        let vanished = ProjectionPinRecord {
            version: 1,
            session_id: "s2".into(),
            revision: Some("0123456789abcdef0123456789abcdef01234567".into()),
            noticed_through: Some("0123456789abcdef0123456789abcdef01234567".into()),
            compaction_id: None,
            pinned_at_ms: 0,
            expiry_noticed_through_ms: None,
            suppression_generation: None,
        };
        let fresh_pins = NativeProjectionPins::new();
        let after_missing = fresh_pins
            .advance(&repo, "s2", &[pin_entry(&vanished)], &|_| true)
            .unwrap();
        assert_eq!(
            after_missing.repinned,
            Some(ProjectionRepinReason::MissingRevision)
        );
        assert_eq!(after_missing.revision, repo.head().unwrap());
    }

    #[test]
    fn notice_line_caps_at_twenty_paths() {
        let changes = ProjectedChanges {
            added: (0..25).map(|i| format!("reference/{i}.md")).collect(),
            updated: Vec::new(),
            removed: Vec::new(),
        };
        let line = render_projected_changes_line(&changes, Some("abcdef0123456789"), &[]);
        assert!(line.starts_with(
            "- Memory changed after this session's prompt was pinned to abcdef0: added reference/0.md"
        ));
        assert!(line.contains("reference/19.md"));
        assert!(!line.contains("reference/20.md"));
        assert!(line.contains("5 more"));
    }

    /// Drop a commit's loose object, the way a history rewrite followed by a prune does.
    fn prune_commit(repo: &MemoryRepo, sha: &str) {
        let object = repo
            .root()
            .join(".git/objects")
            .join(&sha[..2])
            .join(&sha[2..]);
        std::fs::remove_file(object).unwrap();
    }

    #[test]
    fn live_pin_whose_commit_was_pruned_repins_to_head() {
        let root = init_repo("pin-live-pruned-revision");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "v1");
        let pinned = repo.head().unwrap().unwrap();
        let pins = NativeProjectionPins::with_now(Box::new(|| 1));
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        commit_file(&repo, "system/persona.md", "v2");
        // The notice cursor reaches the second commit while the pin stays on the first.
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        prune_commit(&repo, &pinned);

        let turn = pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        assert_eq!(turn.repinned, Some(ProjectionRepinReason::MissingRevision));
        assert_eq!(turn.revision, repo.head().unwrap());
    }

    #[test]
    fn live_notice_cursor_that_was_pruned_repins_instead_of_failing_the_range_lookup() {
        let root = init_repo("pin-live-pruned-cursor");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "v1");
        let pins = NativeProjectionPins::with_now(Box::new(|| 1));
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        commit_file(&repo, "system/persona.md", "v2");
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        let cursor = repo.head().unwrap().unwrap();
        commit_file(&repo, "system/persona.md", "v3");
        prune_commit(&repo, &cursor);

        let turn = pins
            .advance(&repo, "s1", &[], &|_| true)
            .expect("a pruned cursor repins instead of failing");
        assert_eq!(turn.repinned, Some(ProjectionRepinReason::MissingRevision));
        assert_eq!(turn.revision, repo.head().unwrap());
    }

    #[test]
    fn live_pin_with_both_commits_present_is_kept() {
        let root = init_repo("pin-live-intact");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "v1");
        let pinned = repo.head().unwrap();
        let pins = NativeProjectionPins::with_now(Box::new(|| 1));
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        commit_file(&repo, "reference/new.md", "new");

        let turn = pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        assert_eq!(turn.repinned, None);
        assert_eq!(turn.revision, pinned);
        assert!(turn.changes.is_some());
    }

    #[test]
    fn failed_pin_write_is_retried_verbatim_without_repinning() {
        let root = init_repo("pin-retry-write");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "v1");
        let pins = NativeProjectionPins::with_now(Box::new(|| 1));
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();
        pins.request_refresh();
        let written = RefCell::new(Vec::new());
        let failing = |r: &ProjectionPinRecord| {
            written.borrow_mut().push(r.clone());
            false
        };
        let selected = pins.advance(&repo, "s1", &[], &failing).unwrap();
        assert_eq!(selected.repinned, Some(ProjectionRepinReason::Refresh));
        // The repo moves on while the record is still unwritten.
        commit_file(&repo, "system/persona.md", "v2");

        let records = RefCell::new(Vec::new());
        let retried = pins
            .advance(&repo, "s1", &[], &|r| {
                records.borrow_mut().push(r.clone());
                true
            })
            .unwrap();
        assert_eq!(
            records.borrow().first(),
            written.borrow().first(),
            "the retry writes the same record"
        );
        assert_eq!(retried.repinned, None, "the retry is not a repin");
        assert_eq!(retried.revision, selected.revision);
        assert_eq!(retried.as_of_ms, selected.as_of_ms);
        assert_eq!(retried.now_ms, selected.now_ms);
        assert_ne!(retried.revision, repo.head().unwrap());
    }

    #[test]
    fn retried_pin_write_stops_once_it_lands() {
        let root = init_repo("pin-retry-once");
        let repo = MemoryRepo::open(&root, "pin-agent").unwrap();
        commit_file(&repo, "system/persona.md", "v1");
        let pins = NativeProjectionPins::with_now(Box::new(|| 1));
        pins.advance(&repo, "s1", &[], &|_| false).unwrap();
        pins.advance(&repo, "s1", &[], &|_| true).unwrap();

        let after = RefCell::new(0usize);
        pins.advance(&repo, "s1", &[], &|_| {
            *after.borrow_mut() += 1;
            true
        })
        .unwrap();
        assert_eq!(*after.borrow(), 0, "a landed record is not rewritten");
    }
}
