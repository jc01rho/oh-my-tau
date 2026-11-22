#![allow(dead_code)]
//! Security scan coordinator: the concrete [`SecurityScanBackend`] behind the
//! `security_scan` tool, the canonical `security://` findings store, and the
//! `/security` slash command.
//!
//! Layering:
//!
//! * [`SecurityStore`] — SQLite (WAL) persistence for plans, operations,
//!   findings and Codex Security cloud configurations.
//! * [`SecurityCoordinator`] — validates settings/credentials, builds
//!   immutable plans pinned to a repository snapshot, records operations,
//!   validates findings, and exposes SARIF export.
//! * [`command`] — the `/security` slash command surface.
//!
//! Native actions are always local. Cloud actions are gated on a Codex OAuth
//! token and never fall back to a native scan; when no token is present they
//! fail with [`CLOUD_AUTH_REQUIRED`]. The remote Codex Security calls
//! themselves are host-owned: this module records and reports cloud
//! configuration state, it does not perform the network round-trip.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::rpc::slash::Outcome;
use crate::runtime::Runtime;
use crate::tools::security_scan::{Action, SecurityScanBackend};

/// Error text for every `cloud_*` action when no Codex token is available.
pub const CLOUD_AUTH_REQUIRED: &str = "Codex Security cloud actions require a signed-in ChatGPT OAuth account, \
but no Codex token is present. Sign in with `omp login` (or set `OMP_CODEX_TOKEN`) and retry. \
Native `security_scan` actions (preflight, start, status, cancel, validate) do not need a Codex token \
and are unaffected. Cloud actions never fall back to a native scan.";

/// Error text when `security` is off in settings.
pub const SECURITY_DISABLED: &str =
    "Security is disabled in settings. Enable security before running \
security scans (preflight/start/cloud_* are refused while it is off).";

/// Target kinds accepted by `preflight`.
pub const TARGET_KINDS: [&str; 4] = ["repository", "scoped_path", "ref_diff", "working_tree"];

/// Validation states accepted by `validate`.
pub const VALIDATION_STATES: [&str; 5] =
    ["unvalidated", "validated", "rejected", "partial", "error"];

const MAX_SNAPSHOT_ENTRIES: usize = 20_000;

// ---------------------------------------------------------------------------
// small deterministic helpers
// ---------------------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Stable short digest over ordered parts (used for ids, so a plan/finding is
/// addressable across processes without a uuid dependency).
fn digest(parts: &[String]) -> String {
    format!("{:016x}", fnv1a(parts.join("\u{1f}").as_bytes()))
}

// ---------------------------------------------------------------------------
// records
// ---------------------------------------------------------------------------

/// One immutable scan plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanPlan {
    pub plan_id: String,
    pub created_at: i64,
    pub target_kind: String,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub base_revision: Option<String>,
    pub head_revision: Option<String>,
    pub knowledge_base_paths: Vec<String>,
    pub output_root: Option<String>,
    pub credential_id: Option<i64>,
    /// Digest of the repository snapshot the plan is pinned to.
    pub snapshot: String,
    pub repository_root: String,
}

/// One background scan operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Operation {
    pub operation_id: String,
    pub plan_id: String,
    /// `queued` | `running` | `succeeded` | `failed` | `cancelled`.
    pub state: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub detail: Option<String>,
}

impl Operation {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "succeeded" | "failed" | "cancelled")
    }
}

/// One validation evidence item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub label: String,
    pub explanation: String,
}

/// One finding in the canonical security store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub finding_id: String,
    pub scan_id: String,
    pub source: String,
    pub severity: String,
    pub title: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub validation_status: String,
    pub validation_summary: Option<String>,
    pub validation_evidence: Vec<Evidence>,
    pub updated_at: i64,
}

/// One Codex Security cloud configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloudConfiguration {
    pub configuration_id: String,
    pub repository_id: String,
    pub repository_url: Option<String>,
    pub environment_id: Option<String>,
    /// `"all"` or a positive integer, rendered as text.
    pub lookback_days: Option<String>,
    /// `configured` | `enabled` | `pulled`.
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// store
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS security_plans (
    plan_id     TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    target_kind TEXT NOT NULL,
    snapshot    TEXT NOT NULL,
    plan_json   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS security_operations (
    operation_id TEXT PRIMARY KEY,
    plan_id      TEXT NOT NULL,
    state        TEXT NOT NULL,
    started_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    detail       TEXT
);
CREATE INDEX IF NOT EXISTS security_operations_plan ON security_operations(plan_id);
CREATE TABLE IF NOT EXISTS security_findings (
    finding_id          TEXT PRIMARY KEY,
    scan_id             TEXT NOT NULL,
    source              TEXT NOT NULL,
    severity            TEXT NOT NULL,
    title               TEXT NOT NULL,
    path                TEXT,
    line                INTEGER,
    validation_status   TEXT NOT NULL,
    validation_summary  TEXT,
    validation_evidence TEXT NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS security_findings_scan ON security_findings(scan_id);
CREATE TABLE IF NOT EXISTS security_cloud_configurations (
    configuration_id TEXT PRIMARY KEY,
    repository_id    TEXT NOT NULL,
    repository_url   TEXT,
    environment_id   TEXT,
    lookback_days    TEXT,
    state            TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
"#;

/// SQLite-backed canonical security store (WAL, one connection behind a mutex).
pub struct SecurityStore {
    conn: Mutex<Connection>,
    location: PathBuf,
}

impl std::fmt::Debug for SecurityStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityStore")
            .field("location", &self.location)
            .finish()
    }
}

impl SecurityStore {
    /// Open (creating if needed) a WAL database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("creating security store directory {}", parent.display())
                })?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening security store {}", path.display()))?;
        Self::prepare(conn, path.to_path_buf())
    }

    /// Ephemeral store, used by tests and by `--dry-run` style hosts.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory security store")?;
        Self::prepare(conn, PathBuf::from(":memory:"))
    }

    fn prepare(conn: Connection, location: PathBuf) -> Result<Self> {
        // WAL is what makes concurrent readers (status polling, `security://`
        // reads) safe while a scan writes.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL on security store")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous on security store")?;
        conn.busy_timeout(Duration::from_secs(5))
            .context("setting busy timeout on security store")?;
        conn.execute_batch(SCHEMA)
            .context("migrating security store schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
            location,
        })
    }

    #[must_use]
    pub fn location(&self) -> &Path {
        &self.location
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| anyhow!("security store mutex poisoned"))?;
        f(&guard)
    }

    // -- plans -------------------------------------------------------------

    pub fn upsert_plan(&self, plan: &ScanPlan) -> Result<()> {
        let payload = serde_json::to_string(plan).context("serializing scan plan")?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO security_plans (plan_id, created_at, target_kind, snapshot, plan_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(plan_id) DO UPDATE SET plan_json = excluded.plan_json",
                params![
                    plan.plan_id,
                    plan.created_at,
                    plan.target_kind,
                    plan.snapshot,
                    payload
                ],
            )?;
            Ok(())
        })
    }

    pub fn plan(&self, plan_id: &str) -> Result<Option<ScanPlan>> {
        self.with_conn(|conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT plan_json FROM security_plans WHERE plan_id = ?1",
                    params![plan_id],
                    |row| row.get(0),
                )
                .optional()?;
            match raw {
                Some(raw) => Ok(Some(
                    serde_json::from_str(&raw).context("decoding stored scan plan")?,
                )),
                None => Ok(None),
            }
        })
    }

    pub fn plans(&self) -> Result<Vec<ScanPlan>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT plan_json FROM security_plans ORDER BY created_at DESC")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                let raw = row?;
                out.push(serde_json::from_str(&raw).context("decoding stored scan plan")?);
            }
            Ok(out)
        })
    }

    // -- operations --------------------------------------------------------

    pub fn insert_operation(&self, op: &Operation) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO security_operations
                     (operation_id, plan_id, state, started_at, finished_at, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    op.operation_id,
                    op.plan_id,
                    op.state,
                    op.started_at,
                    op.finished_at,
                    op.detail
                ],
            )?;
            Ok(())
        })
    }

    pub fn operation(&self, operation_id: &str) -> Result<Option<Operation>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT operation_id, plan_id, state, started_at, finished_at, detail
                 FROM security_operations WHERE operation_id = ?1",
                params![operation_id],
                |row| {
                    Ok(Operation {
                        operation_id: row.get(0)?,
                        plan_id: row.get(1)?,
                        state: row.get(2)?,
                        started_at: row.get(3)?,
                        finished_at: row.get(4)?,
                        detail: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn operations_for_plan(&self, plan_id: &str) -> Result<Vec<Operation>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT operation_id, plan_id, state, started_at, finished_at, detail
                 FROM security_operations WHERE plan_id = ?1 ORDER BY started_at DESC",
            )?;
            let rows = stmt.query_map(params![plan_id], |row| {
                Ok(Operation {
                    operation_id: row.get(0)?,
                    plan_id: row.get(1)?,
                    state: row.get(2)?,
                    started_at: row.get(3)?,
                    finished_at: row.get(4)?,
                    detail: row.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Move an operation to `state`, stamping `finished_at` for terminal states.
    pub fn set_operation_state(
        &self,
        operation_id: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let terminal = matches!(state, "succeeded" | "failed" | "cancelled");
        let finished_at = if terminal { Some(now_unix()) } else { None };
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE security_operations
                 SET state = ?2, finished_at = COALESCE(?3, finished_at), detail = ?4
                 WHERE operation_id = ?1",
                params![operation_id, state, finished_at, detail],
            )?;
            if changed == 0 {
                bail!("unknown security operation `{operation_id}`");
            }
            Ok(())
        })
    }

    // -- findings ----------------------------------------------------------

    /// Insert a finding, or refresh its payload while keeping a prior
    /// validation verdict unless the incoming row carries one.
    pub fn upsert_finding(&self, finding: &Finding) -> Result<()> {
        let evidence = serde_json::to_string(&finding.validation_evidence)
            .context("serializing validation evidence")?;
        self.with_conn(|conn| {
            let existing: Option<(String, Option<String>, String)> = conn
                .query_row(
                    "SELECT validation_status, validation_summary, validation_evidence
                     FROM security_findings WHERE finding_id = ?1",
                    params![finding.finding_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (status, summary, evidence_json) = match existing {
                Some((status, summary, evidence))
                    if finding.validation_status == "unvalidated" && status != "unvalidated" =>
                {
                    (status, summary, evidence)
                }
                _ => (
                    finding.validation_status.clone(),
                    finding.validation_summary.clone(),
                    evidence,
                ),
            };
            conn.execute(
                "INSERT INTO security_findings
                     (finding_id, scan_id, source, severity, title, path, line,
                      validation_status, validation_summary, validation_evidence, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(finding_id) DO UPDATE SET
                     scan_id = excluded.scan_id,
                     source = excluded.source,
                     severity = excluded.severity,
                     title = excluded.title,
                     path = excluded.path,
                     line = excluded.line,
                     validation_status = excluded.validation_status,
                     validation_summary = excluded.validation_summary,
                     validation_evidence = excluded.validation_evidence,
                     updated_at = excluded.updated_at",
                params![
                    finding.finding_id,
                    finding.scan_id,
                    finding.source,
                    finding.severity,
                    finding.title,
                    finding.path,
                    finding.line,
                    status,
                    summary,
                    evidence_json,
                    finding.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn finding(&self, finding_id: &str) -> Result<Option<Finding>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT finding_id, scan_id, source, severity, title, path, line,
                        validation_status, validation_summary, validation_evidence, updated_at
                 FROM security_findings WHERE finding_id = ?1",
                params![finding_id],
                decode_finding,
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn findings(&self, scan_id: Option<&str>) -> Result<Vec<Finding>> {
        self.with_conn(|conn| {
            let (sql, binder): (&str, Option<&str>) = match scan_id {
                Some(_) => (
                    "SELECT finding_id, scan_id, source, severity, title, path, line,
                            validation_status, validation_summary, validation_evidence, updated_at
                     FROM security_findings WHERE scan_id = ?1
                     ORDER BY severity, path, line",
                    scan_id,
                ),
                None => (
                    "SELECT finding_id, scan_id, source, severity, title, path, line,
                            validation_status, validation_summary, validation_evidence, updated_at
                     FROM security_findings ORDER BY scan_id, severity, path, line",
                    None,
                ),
            };
            let mut stmt = conn.prepare(sql)?;
            let mut out = Vec::new();
            match binder {
                Some(scan_id) => {
                    let rows = stmt.query_map(params![scan_id], decode_finding)?;
                    for row in rows {
                        out.push(row?);
                    }
                }
                None => {
                    let rows = stmt.query_map([], decode_finding)?;
                    for row in rows {
                        out.push(row?);
                    }
                }
            }
            Ok(out)
        })
    }

    // -- cloud configurations ---------------------------------------------

    pub fn upsert_cloud_configuration(&self, config: &CloudConfiguration) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO security_cloud_configurations
                     (configuration_id, repository_id, repository_url, environment_id,
                      lookback_days, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(configuration_id) DO UPDATE SET
                     repository_id = excluded.repository_id,
                     repository_url = excluded.repository_url,
                     environment_id = excluded.environment_id,
                     lookback_days = excluded.lookback_days,
                     state = excluded.state,
                     updated_at = excluded.updated_at",
                params![
                    config.configuration_id,
                    config.repository_id,
                    config.repository_url,
                    config.environment_id,
                    config.lookback_days,
                    config.state,
                    config.created_at,
                    config.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn cloud_configuration(
        &self,
        configuration_id: &str,
    ) -> Result<Option<CloudConfiguration>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT configuration_id, repository_id, repository_url, environment_id,
                        lookback_days, state, created_at, updated_at
                 FROM security_cloud_configurations WHERE configuration_id = ?1",
                params![configuration_id],
                |row| {
                    Ok(CloudConfiguration {
                        configuration_id: row.get(0)?,
                        repository_id: row.get(1)?,
                        repository_url: row.get(2)?,
                        environment_id: row.get(3)?,
                        lookback_days: row.get(4)?,
                        state: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn cloud_configurations(&self) -> Result<Vec<CloudConfiguration>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT configuration_id, repository_id, repository_url, environment_id,
                        lookback_days, state, created_at, updated_at
                 FROM security_cloud_configurations ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(CloudConfiguration {
                    configuration_id: row.get(0)?,
                    repository_id: row.get(1)?,
                    repository_url: row.get(2)?,
                    environment_id: row.get(3)?,
                    lookback_days: row.get(4)?,
                    state: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }
}

fn decode_finding(row: &rusqlite::Row<'_>) -> rusqlite::Result<Finding> {
    let evidence_raw: String = row.get(9)?;
    Ok(Finding {
        finding_id: row.get(0)?,
        scan_id: row.get(1)?,
        source: row.get(2)?,
        severity: row.get(3)?,
        title: row.get(4)?,
        path: row.get(5)?,
        line: row.get(6)?,
        validation_status: row.get(7)?,
        validation_summary: row.get(8)?,
        validation_evidence: serde_json::from_str(&evidence_raw).unwrap_or_default(),
        updated_at: row.get(10)?,
    })
}

// ---------------------------------------------------------------------------
// repository snapshot
// ---------------------------------------------------------------------------

/// Digest the repository snapshot a plan is pinned to: relative path, size and
/// mtime for every included file. Deterministic for a given tree state.
pub fn snapshot_digest(root: &Path, include: &[String], exclude: &[String]) -> Result<String> {
    if !root.is_dir() {
        bail!("repository root {} is not a directory", root.display());
    }
    let mut entries: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;

    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            // Unreadable subdirectories are recorded, not fatal.
            Err(_) => continue,
        };
        for entry in read.flatten() {
            visited += 1;
            if visited > MAX_SNAPSHOT_ENTRIES {
                bail!(
                    "repository snapshot exceeds {MAX_SNAPSHOT_ENTRIES} entries under {}",
                    root.display()
                );
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel == ".git" || rel.starts_with(".git/") {
                continue;
            }
            if exclude.iter().any(|pattern| path_matches(&rel, pattern)) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !include.is_empty() && !include.iter().any(|pattern| path_matches(&rel, pattern)) {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push(format!("{rel}:{}:{mtime}", meta.len()));
        }
    }

    entries.sort();
    let mut parts = vec![root.to_string_lossy().to_string()];
    parts.extend(entries);
    Ok(digest(&parts))
}

/// Prefix/glob-lite match: exact, directory prefix, or `*`-suffix wildcard.
fn path_matches(rel: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    if pattern.is_empty() {
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return rel == prefix || rel.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix('/') {
        return rel == prefix || rel.starts_with(&format!("{prefix}/"));
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return rel.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return rel.starts_with(prefix);
    }
    rel == pattern || rel.starts_with(&format!("{pattern}/"))
}

// ---------------------------------------------------------------------------
// coordinator
// ---------------------------------------------------------------------------

/// The concrete security backend: plans, operations, findings, cloud configs.
pub struct SecurityCoordinator {
    store: Arc<SecurityStore>,
    repository_root: PathBuf,
    security_enabled: bool,
    codex_token: Option<String>,
}

impl std::fmt::Debug for SecurityCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityCoordinator")
            .field("store", &self.store)
            .field("repository_root", &self.repository_root)
            .field("security_enabled", &self.security_enabled)
            .field(
                "codex_token",
                &self.codex_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl SecurityCoordinator {
    #[must_use]
    pub fn new(store: Arc<SecurityStore>, repository_root: impl Into<PathBuf>) -> Self {
        Self {
            store,
            repository_root: repository_root.into(),
            security_enabled: true,
            codex_token: None,
        }
    }

    #[must_use]
    pub fn with_security_enabled(mut self, enabled: bool) -> Self {
        self.security_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_codex_token(mut self, token: Option<String>) -> Self {
        self.codex_token = token.filter(|t| !t.trim().is_empty());
        self
    }

    #[must_use]
    pub fn store(&self) -> &Arc<SecurityStore> {
        &self.store
    }

    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.codex_token.is_some()
    }

    fn require_enabled(&self) -> Result<()> {
        if self.security_enabled {
            Ok(())
        } else {
            Err(anyhow!(SECURITY_DISABLED))
        }
    }

    fn require_codex_token(&self) -> Result<&str> {
        match self.codex_token.as_deref() {
            Some(token) => Ok(token),
            None => Err(anyhow!(CLOUD_AUTH_REQUIRED)),
        }
    }

    // -- preflight / start / status / cancel -------------------------------

    /// Build and persist an immutable plan pinned to the current snapshot.
    pub fn preflight(&self, params: &Value) -> Result<Value> {
        self.require_enabled()?;

        let target_kind = params
            .get("target_kind")
            .and_then(Value::as_str)
            .unwrap_or("working_tree")
            .to_string();
        if !TARGET_KINDS.contains(&target_kind.as_str()) {
            bail!("unknown target_kind `{target_kind}`");
        }

        let include_paths = string_list(params, "include_paths");
        let exclude_paths = string_list(params, "exclude_paths");
        let knowledge_base_paths = string_list(params, "knowledge_base_paths");

        if target_kind == "scoped_path" && include_paths.is_empty() {
            bail!("`scoped_path` requires at least one entry in `include_paths`");
        }
        if target_kind == "ref_diff"
            && (params.get("base_revision").is_none() || params.get("head_revision").is_none())
        {
            bail!("`ref_diff` requires both `base_revision` and `head_revision`");
        }

        let base_revision = optional_string(params, "base_revision");
        let head_revision = optional_string(params, "head_revision");
        let output_root = optional_string(params, "output_root");
        let credential_id = params.get("credential_id").and_then(Value::as_i64);
        if params.get("credential_id").is_some() && credential_id.is_none() {
            bail!("`credential_id` must be an integer");
        }

        let snapshot = snapshot_digest(&self.repository_root, &include_paths, &exclude_paths)?;
        let created_at = now_unix();
        let repository_root = self.repository_root.to_string_lossy().to_string();

        let plan_id = format!(
            "plan_{}",
            digest(&[
                target_kind.clone(),
                snapshot.clone(),
                include_paths.join(","),
                exclude_paths.join(","),
                base_revision.clone().unwrap_or_default(),
                head_revision.clone().unwrap_or_default(),
                repository_root.clone(),
            ])
        );

        let plan = ScanPlan {
            plan_id,
            created_at,
            target_kind,
            include_paths,
            exclude_paths,
            base_revision,
            head_revision,
            knowledge_base_paths,
            output_root,
            credential_id,
            snapshot,
            repository_root,
        };
        self.store.upsert_plan(&plan)?;

        Ok(json!({
            "plan_id": plan.plan_id,
            "created_at": plan.created_at,
            "target_kind": plan.target_kind,
            "snapshot": plan.snapshot,
            "repository_root": plan.repository_root,
            "include_paths": plan.include_paths,
            "exclude_paths": plan.exclude_paths,
            "base_revision": plan.base_revision,
            "head_revision": plan.head_revision,
            "knowledge_base_paths": plan.knowledge_base_paths,
            "output_root": plan.output_root,
            "credential_id": plan.credential_id,
        }))
    }

    /// Turn a plan into a background operation. The job runner is host-owned;
    /// this records the operation the host then drives.
    pub fn start(&self, params: &Value) -> Result<Value> {
        self.require_enabled()?;
        let plan_id = required_string(params, "plan_id")?;
        let plan = self
            .store
            .plan(plan_id)?
            .ok_or_else(|| anyhow!("unknown plan_id `{plan_id}`"))?;

        // A plan pinned to a different snapshot than the tree now must be
        // re-planned rather than silently scanned against moved code.
        let current = snapshot_digest(
            &self.repository_root,
            &plan.include_paths,
            &plan.exclude_paths,
        )?;
        if current != plan.snapshot {
            bail!(
                "plan `{plan_id}` is stale: repository snapshot changed since preflight \
                 (planned {}, now {}); run preflight again",
                plan.snapshot,
                current
            );
        }

        let started_at = now_unix();
        let operation_id = format!(
            "op_{}",
            digest(&[
                plan_id.to_string(),
                started_at.to_string(),
                self.store.operations_for_plan(plan_id)?.len().to_string(),
            ])
        );
        let op = Operation {
            operation_id,
            plan_id: plan_id.to_string(),
            state: "queued".to_string(),
            started_at,
            finished_at: None,
            detail: None,
        };
        self.store.insert_operation(&op)?;

        Ok(json!({
            "operation_id": op.operation_id,
            "plan_id": op.plan_id,
            "state": op.state,
            "started_at": op.started_at,
            "scan_id": op.operation_id,
        }))
    }

    pub fn status(&self, params: &Value) -> Result<Value> {
        let operation_id = required_string(params, "operation_id")?;
        let op = self
            .store
            .operation(operation_id)?
            .ok_or_else(|| anyhow!("unknown operation_id `{operation_id}`"))?;
        let findings = self.store.findings(Some(&op.operation_id))?;
        Ok(json!({
            "operation_id": op.operation_id,
            "plan_id": op.plan_id,
            "state": op.state,
            "started_at": op.started_at,
            "finished_at": op.finished_at,
            "detail": op.detail,
            "terminal": op.is_terminal(),
            "finding_count": findings.len(),
        }))
    }

    pub fn cancel(&self, params: &Value) -> Result<Value> {
        let operation_id = required_string(params, "operation_id")?;
        let op = self
            .store
            .operation(operation_id)?
            .ok_or_else(|| anyhow!("unknown operation_id `{operation_id}`"))?;
        if op.is_terminal() {
            bail!(
                "operation `{operation_id}` already finished in state `{}`",
                op.state
            );
        }
        self.store
            .set_operation_state(operation_id, "cancelled", Some("cancelled by user"))?;
        Ok(json!({
            "operation_id": operation_id,
            "state": "cancelled",
        }))
    }

    // -- validate ----------------------------------------------------------

    pub fn validate(&self, params: &Value) -> Result<Value> {
        let finding_id = required_string(params, "finding_id")?;
        let status = required_string(params, "validation_status")?;
        if !VALIDATION_STATES.contains(&status) {
            bail!("unknown validation_status `{status}`");
        }
        let mut finding = self
            .store
            .finding(finding_id)?
            .ok_or_else(|| anyhow!("unknown finding_id `{finding_id}`"))?;

        let evidence = match params.get("validation_evidence") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let label = item
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .ok_or_else(|| {
                            anyhow!("validation_evidence item needs a non-empty `label`")
                        })?;
                    let explanation = item
                        .get("explanation")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    out.push(Evidence {
                        label: label.to_string(),
                        explanation: explanation.to_string(),
                    });
                }
                out
            }
            Some(_) => bail!("`validation_evidence` must be an array"),
        };

        finding.validation_status = status.to_string();
        finding.validation_summary = optional_string(params, "validation_summary");
        finding.validation_evidence = evidence;
        finding.updated_at = now_unix();
        self.store.upsert_finding(&finding)?;

        Ok(json!({
            "finding_id": finding.finding_id,
            "scan_id": finding.scan_id,
            "validation_status": finding.validation_status,
            "validation_summary": finding.validation_summary,
            "validation_evidence": finding.validation_evidence,
            "updated_at": finding.updated_at,
        }))
    }

    // -- cloud -------------------------------------------------------------

    /// List cloud configurations for the selected account.
    pub fn cloud_scans(&self, _params: &Value) -> Result<Value> {
        self.require_enabled()?;
        self.require_codex_token()?;
        let configurations = self.store.cloud_configurations()?;
        Ok(json!({
            "configurations": configurations,
            "count": configurations.len(),
        }))
    }

    /// Create or enable a cloud configuration. Never falls back to a native
    /// scan, and never consumes the native allowance.
    pub fn cloud_start(&self, params: &Value) -> Result<Value> {
        self.require_enabled()?;
        self.require_codex_token()?;

        let repository_id = required_string(params, "repository_id")?;
        let repository_url = required_string(params, "repository_url")?;
        let environment_id = required_string(params, "environment_id")?;
        let lookback_days = match params.get("lookback_days") {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) if raw == "all" => Some("all".to_string()),
            Some(Value::Number(n)) if n.as_i64().is_some_and(|v| v >= 1) => {
                Some(n.as_i64().unwrap_or(1).to_string())
            }
            Some(_) => bail!("`lookback_days` must be a positive integer or \"all\""),
        };

        let now = now_unix();
        let configuration_id = match optional_string(params, "cloud_configuration_id") {
            Some(id) => id,
            None => format!(
                "cloud_{}",
                digest(&[
                    repository_id.to_string(),
                    repository_url.to_string(),
                    environment_id.to_string(),
                ])
            ),
        };
        let created_at = self
            .store
            .cloud_configuration(&configuration_id)?
            .map(|existing| existing.created_at)
            .unwrap_or(now);

        let config = CloudConfiguration {
            configuration_id,
            repository_id: repository_id.to_string(),
            repository_url: Some(repository_url.to_string()),
            environment_id: Some(environment_id.to_string()),
            lookback_days,
            state: "enabled".to_string(),
            created_at,
            updated_at: now,
        };
        self.store.upsert_cloud_configuration(&config)?;

        Ok(json!({
            "cloud_configuration_id": config.configuration_id,
            "state": config.state,
            "repository_id": config.repository_id,
            "repository_url": config.repository_url,
            "environment_id": config.environment_id,
            "lookback_days": config.lookback_days,
            "created_at": config.created_at,
            "updated_at": config.updated_at,
        }))
    }

    /// Cloud progress for one configuration.
    pub fn cloud_status(&self, params: &Value) -> Result<Value> {
        self.require_enabled()?;
        self.require_codex_token()?;
        let configuration_id = required_string(params, "cloud_configuration_id")?;
        let config = self
            .store
            .cloud_configuration(configuration_id)?
            .ok_or_else(|| anyhow!("unknown cloud_configuration_id `{configuration_id}`"))?;
        let findings = self.store.findings(Some(configuration_id))?;
        Ok(json!({
            "cloud_configuration_id": config.configuration_id,
            "state": config.state,
            "repository_id": config.repository_id,
            "environment_id": config.environment_id,
            "finding_count": findings.len(),
            "updated_at": config.updated_at,
        }))
    }

    /// Pull cloud findings into the canonical store, where `security://`
    /// reads them.
    pub fn cloud_pull(&self, params: &Value) -> Result<Value> {
        self.require_enabled()?;
        self.require_codex_token()?;
        let configuration_id = required_string(params, "cloud_configuration_id")?;
        let mut config = self
            .store
            .cloud_configuration(configuration_id)?
            .ok_or_else(|| anyhow!("unknown cloud_configuration_id `{configuration_id}`"))?;

        let pulled = self.store.findings(Some(&config.configuration_id))?;
        config.state = "pulled".to_string();
        config.updated_at = now_unix();
        self.store.upsert_cloud_configuration(&config)?;

        Ok(json!({
            "cloud_configuration_id": config.configuration_id,
            "state": config.state,
            "pulled": pulled.len(),
            "findings": pulled,
            "security_uri_prefix": format!("security://{}", config.configuration_id),
        }))
    }

    // -- findings / SARIF --------------------------------------------------

    pub fn findings(&self, scan_id: Option<&str>) -> Result<Vec<Finding>> {
        self.store.findings(scan_id)
    }

    /// Render findings as SARIF 2.1.0.
    pub fn sarif(&self, scan_id: &str) -> Result<Value> {
        let findings = self.store.findings(Some(scan_id))?;
        Ok(sarif_document(scan_id, &findings))
    }
}

fn sarif_document(scan_id: &str, findings: &[Finding]) -> Value {
    let mut rules: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for finding in findings {
        let rule_id = rule_id_for(finding);
        if !seen.contains(&rule_id) {
            seen.push(rule_id.clone());
            rules.push(json!({
                "id": rule_id,
                "name": finding.title,
                "shortDescription": {"text": finding.title},
                "properties": {
                    "severity": finding.severity,
                    "source": finding.source,
                },
            }));
        }
    }

    let results: Vec<Value> = findings
        .iter()
        .map(|finding| {
            let mut region = serde_json::Map::new();
            region.insert("startLine".into(), json!(finding.line.unwrap_or(1).max(1)));
            let location = match &finding.path {
                Some(path) => json!([{
                    "physicalLocation": {
                        "artifactLocation": {"uri": path},
                        "region": Value::Object(region),
                    }
                }]),
                None => json!([]),
            };
            json!({
                "ruleId": rule_id_for(finding),
                "level": sarif_level(&finding.severity),
                "message": {
                    "text": match &finding.validation_summary {
                        Some(summary) => format!("{} ({summary})", finding.title),
                        None => finding.title.clone(),
                    }
                },
                "locations": location,
                "partialFingerprints": {
                    "ompFindingId": finding.finding_id,
                },
                "properties": {
                    "validationStatus": finding.validation_status,
                    "scanId": finding.scan_id,
                    "source": finding.source,
                },
            })
        })
        .collect();

    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "omp-security",
                    "informationUri": "https://github.com/oh-my-tau",
                    "rules": rules,
                }
            },
            "automationDetails": {"id": scan_id},
            "results": results,
        }]
    })
}

fn rule_id_for(finding: &Finding) -> String {
    let slug: String = finding
        .title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() {
        "finding".to_string()
    } else {
        slug
    };
    format!("omp/{}/{slug}", finding.source)
}

fn sarif_level(severity: &str) -> &'static str {
    match severity.to_ascii_lowercase().as_str() {
        "critical" | "high" => "error",
        "medium" => "warning",
        "low" | "info" | "informational" => "note",
        _ => "warning",
    }
}

// ---------------------------------------------------------------------------
// parameter helpers
// ---------------------------------------------------------------------------

fn required_string<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

fn optional_string(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
}

fn string_list(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// SecurityScanBackend
// ---------------------------------------------------------------------------

impl SecurityScanBackend for SecurityCoordinator {
    fn run(&self, action: Action, params: &Value) -> Result<String> {
        let value = match action {
            Action::Preflight => self.preflight(params)?,
            Action::Start => self.start(params)?,
            Action::Status => self.status(params)?,
            Action::Cancel => self.cancel(params)?,
            Action::Validate => self.validate(params)?,
            Action::CloudScans => self.cloud_scans(params)?,
            Action::CloudStart => self.cloud_start(params)?,
            Action::CloudStatus => self.cloud_status(params)?,
            Action::CloudPull => self.cloud_pull(params)?,
        };
        serde_json::to_string(&value).context("serializing security_scan result")
    }
}

// ---------------------------------------------------------------------------
// `/security`
// ---------------------------------------------------------------------------

pub fn help_text() -> &'static str {
    "\
/security — OMP-native repository security scans

  /security status                     store location, counts, auth state
  /security plans                      list stored plans
  /security plan <plan_id>             show one plan
  /security preflight [target_kind]    plan a scan (repository|scoped_path|ref_diff|working_tree)
  /security start <plan_id>            queue an operation for a plan
  /security cancel <operation_id>      cancel a queued/running operation
  /security findings [scan_id]         list findings (canonical security:// store)
  /security validate <finding_id> <status> [summary]
                                       status: unvalidated|validated|rejected|partial|error
  /security sarif <scan_id>            SARIF 2.1.0 export
  /security cloud                      Codex Security cloud configurations (needs sign-in)
  /security help                       this text

Cloud actions require a signed-in ChatGPT OAuth account and never fall back to a
native scan."
}

/// Store path: `$OMP_SECURITY_DB`, else `<repo>/.omp/security.db`.
fn store_path(repository_root: &Path) -> PathBuf {
    match std::env::var("OMP_SECURITY_DB") {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => repository_root.join(".omp").join("security.db"),
    }
}

fn coordinator_from_env() -> Result<SecurityCoordinator> {
    let repository_root = std::env::current_dir().context("resolving working directory")?;
    let db_path = store_path(&repository_root);
    coordinator_at(repository_root, &db_path)
}

/// Coordinator for `repository_root` backed by the store at `db_path`.
fn coordinator_at(repository_root: PathBuf, db_path: &Path) -> Result<SecurityCoordinator> {
    let store = Arc::new(SecurityStore::open(db_path)?);
    let token = std::env::var("OMP_CODEX_TOKEN")
        .ok()
        .or_else(|| std::env::var("CODEX_TOKEN").ok());
    Ok(SecurityCoordinator::new(store, repository_root).with_codex_token(token))
}

/// Repository containing `cwd`: the nearest ancestor with a `.git` entry,
/// else `cwd` itself (the `/security` command's own fallback).
fn repository_root_of(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// Session tools for the agent loop: the `security_scan` tool over the
/// coordinator of the repository containing `cwd`, or nothing when
/// `security.enabled` is off or the store cannot be opened.
#[must_use]
pub fn session_tools(
    settings: &omp_settings::Settings,
    cwd: &Path,
) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    let repository_root = repository_root_of(cwd);
    let db_path = store_path(&repository_root);
    session_tools_at(settings, repository_root, &db_path)
}

fn session_tools_at(
    settings: &omp_settings::Settings,
    repository_root: PathBuf,
    db_path: &Path,
) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    if !crate::tools::security_scan::enabled(settings) {
        return Vec::new();
    }
    match coordinator_at(repository_root, db_path) {
        Ok(coordinator) => vec![crate::tools::security_scan::tool(Arc::new(coordinator))],
        Err(err) => {
            tracing::warn!("security_scan unavailable: {err:#}");
            Vec::new()
        }
    }
}

/// `/security` entry point, wired from `slash.rs`.
pub async fn command(rt: &Runtime, args: &str) -> Outcome {
    let text = match coordinator_from_env() {
        Ok(coordinator) => match coordinator.run_command(args) {
            Ok(text) => text,
            Err(err) => format!("security: {err:#}"),
        },
        Err(err) => format!("security: {err:#}"),
    };
    rt.send(json!({"type": "command_output", "text": text}));
    Outcome::Handled {
        agent_invoked: false,
    }
}

impl SecurityCoordinator {
    /// Slash-command surface. Returns rendered text; errors are user-facing.
    pub fn run_command(&self, args: &str) -> Result<String> {
        let argv: Vec<&str> = args.split_whitespace().collect();
        let Some(sub) = argv.first().copied() else {
            return Ok(help_text().to_string());
        };
        match sub {
            "help" | "-h" | "--help" => Ok(help_text().to_string()),
            "status" => {
                let plans = self.store.plans()?.len();
                let findings = self.store.findings(None)?.len();
                let cloud = self.store.cloud_configurations()?.len();
                Ok(format!(
                    "security store: {}\nsecurity enabled: {}\ncodex account: {}\nplans: {plans}\nfindings: {findings}\ncloud configurations: {cloud}",
                    self.store.location().display(),
                    if self.security_enabled { "yes" } else { "no" },
                    if self.is_authenticated() {
                        "signed in"
                    } else {
                        "not signed in (cloud actions unavailable)"
                    },
                ))
            }
            "plans" => {
                let plans = self.store.plans()?;
                if plans.is_empty() {
                    return Ok("no plans stored; run `/security preflight`".to_string());
                }
                let mut out = String::new();
                for plan in plans {
                    out.push_str(&format!(
                        "{}  {}  snapshot {}  {}\n",
                        plan.plan_id, plan.target_kind, plan.snapshot, plan.repository_root
                    ));
                }
                Ok(out.trim_end().to_string())
            }
            "plan" => {
                let plan_id = argv
                    .get(1)
                    .ok_or_else(|| anyhow!("usage: /security plan <plan_id>"))?;
                let plan = self
                    .store
                    .plan(plan_id)?
                    .ok_or_else(|| anyhow!("unknown plan_id `{plan_id}`"))?;
                Ok(serde_json::to_string_pretty(&plan)?)
            }
            "preflight" => {
                let mut params = json!({"action": "preflight"});
                if let Some(kind) = argv.get(1) {
                    params["target_kind"] = json!(kind);
                }
                if params.get("target_kind").and_then(Value::as_str) == Some("scoped_path") {
                    bail!("`/security preflight scoped_path` needs include paths; use the security_scan tool");
                }
                let plan = self.preflight(&params)?;
                Ok(format!(
                    "planned {}\n  target: {}\n  snapshot: {}\n  next: /security start {}",
                    plan["plan_id"].as_str().unwrap_or_default(),
                    plan["target_kind"].as_str().unwrap_or_default(),
                    plan["snapshot"].as_str().unwrap_or_default(),
                    plan["plan_id"].as_str().unwrap_or_default(),
                ))
            }
            "start" => {
                let plan_id = argv
                    .get(1)
                    .ok_or_else(|| anyhow!("usage: /security start <plan_id>"))?;
                let started = self.start(&json!({"action": "start", "plan_id": plan_id}))?;
                Ok(format!(
                    "queued {}\n  plan: {}\n  next: /security status {}",
                    started["operation_id"].as_str().unwrap_or_default(),
                    started["plan_id"].as_str().unwrap_or_default(),
                    started["operation_id"].as_str().unwrap_or_default(),
                ))
            }
            "cancel" => {
                let operation_id = argv
                    .get(1)
                    .ok_or_else(|| anyhow!("usage: /security cancel <operation_id>"))?;
                self.cancel(&json!({"action": "cancel", "operation_id": operation_id}))?;
                Ok(format!("cancelled {operation_id}"))
            }
            "findings" => {
                let scan_id = argv.get(1).copied();
                let findings = self.findings(scan_id)?;
                if findings.is_empty() {
                    return Ok("no findings".to_string());
                }
                let mut out = String::new();
                for finding in findings {
                    let location = match (&finding.path, finding.line) {
                        (Some(path), Some(line)) => format!("{path}:{line}"),
                        (Some(path), None) => path.clone(),
                        _ => "-".to_string(),
                    };
                    out.push_str(&format!(
                        "{:<8} {:<12} {}  {}  [{}]\n",
                        finding.severity,
                        finding.validation_status,
                        finding.finding_id,
                        location,
                        finding.title
                    ));
                }
                Ok(out.trim_end().to_string())
            }
            "validate" => {
                let finding_id = argv.get(1).ok_or_else(|| {
                    anyhow!("usage: /security validate <finding_id> <status> [summary]")
                })?;
                let status = argv.get(2).ok_or_else(|| {
                    anyhow!(
                        "usage: /security validate <finding_id> <status> [summary]; status: {}",
                        VALIDATION_STATES.join("|")
                    )
                })?;
                let summary = argv
                    .get(3..)
                    .map(|rest| rest.join(" "))
                    .filter(|s| !s.is_empty());
                let mut params = json!({
                    "action": "validate",
                    "finding_id": finding_id,
                    "validation_status": status,
                });
                if let Some(summary) = summary {
                    params["validation_summary"] = json!(summary);
                }
                let out = self.validate(&params)?;
                Ok(format!(
                    "{} -> {}",
                    out["finding_id"].as_str().unwrap_or_default(),
                    out["validation_status"].as_str().unwrap_or_default()
                ))
            }
            "sarif" => {
                let scan_id = argv
                    .get(1)
                    .ok_or_else(|| anyhow!("usage: /security sarif <scan_id>"))?;
                Ok(serde_json::to_string_pretty(&self.sarif(scan_id)?)?)
            }
            "cloud" => match self.cloud_scans(&json!({"action": "cloud_scans"})) {
                Ok(value) => Ok(serde_json::to_string_pretty(&value)?),
                Err(err) => Ok(format!("security: {err:#}")),
            },
            other => Ok(format!(
                "unknown /security subcommand `{other}`\n\n{}",
                help_text()
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("omp-security-{tag}-{unique}"));
        fs::create_dir_all(dir.join("src")).expect("create scratch dir");
        fs::write(dir.join("src/lib.rs"), "fn main() {}\n").expect("write scratch file");
        fs::write(dir.join("README.md"), "# scratch\n").expect("write scratch file");
        dir
    }

    fn coordinator(tag: &str) -> SecurityCoordinator {
        let store = Arc::new(SecurityStore::open_in_memory().expect("in-memory store"));
        SecurityCoordinator::new(store, scratch_dir(tag))
    }

    fn settings_with(security_enabled: Option<bool>) -> omp_settings::Settings {
        let dir = scratch_dir("settings");
        let cfg = security_enabled.map(|on| {
            let path = dir.join("config.yml");
            fs::write(&path, format!("security:\n  enabled: {on}\n")).expect("write config");
            path
        });
        omp_settings::Settings::load_from(cfg.as_deref(), &dir)
    }

    #[test]
    fn session_tools_empty_when_disabled() {
        let repo = scratch_dir("disabled");
        let db = repo.join("security.db");
        for flag in [None, Some(false)] {
            let tools = session_tools_at(&settings_with(flag), repo.clone(), &db);
            assert!(tools.is_empty(), "flag {flag:?} must register nothing");
        }
        assert!(!db.exists(), "a disabled gate must not open the store");
    }

    #[test]
    fn session_tools_registers_security_scan_when_enabled() {
        let repo = scratch_dir("enabled");
        let db = repo.join("security.db");
        let tools = session_tools_at(&settings_with(Some(true)), repo, &db);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["security_scan"]);
        assert!(db.exists(), "the store is opened at the given path");
    }

    #[test]
    fn session_tools_empty_when_store_cannot_open() {
        let repo = scratch_dir("badstore");
        // A directory is not a database file.
        let tools = session_tools_at(&settings_with(Some(true)), repo.clone(), &repo);
        assert!(tools.is_empty());
    }

    #[test]
    fn repository_root_is_nearest_git_ancestor_else_cwd() {
        let repo = scratch_dir("root");
        fs::create_dir_all(repo.join(".git")).expect("create .git");
        let nested = repo.join("src");
        assert_eq!(repository_root_of(&nested), repo);
        let bare = scratch_dir("noroot");
        assert_eq!(repository_root_of(&bare.join("src")), bare.join("src"));
    }

    fn sample_finding(scan_id: &str) -> Finding {
        Finding {
            finding_id: format!(
                "fnd_{}",
                digest(&[scan_id.to_string(), "src/lib.rs".into()])
            ),
            scan_id: scan_id.to_string(),
            source: "omp-native".to_string(),
            severity: "high".to_string(),
            title: "Unbounded read".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(7),
            validation_status: "unvalidated".to_string(),
            validation_summary: None,
            validation_evidence: Vec::new(),
            updated_at: now_unix(),
        }
    }

    #[test]
    fn store_enables_wal_and_round_trips_plans() {
        let dir = scratch_dir("wal");
        let path = dir.join("security.db");
        let store = SecurityStore::open(&path).expect("open store");
        let mode: String = store
            .with_conn(|conn| {
                Ok(conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?)
            })
            .expect("journal_mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        let plan = ScanPlan {
            plan_id: "plan_test".to_string(),
            created_at: 1,
            target_kind: "repository".to_string(),
            include_paths: vec![],
            exclude_paths: vec!["target/**".to_string()],
            base_revision: None,
            head_revision: None,
            knowledge_base_paths: vec![],
            output_root: None,
            credential_id: Some(3),
            snapshot: "abc".to_string(),
            repository_root: dir.to_string_lossy().to_string(),
        };
        store.upsert_plan(&plan).expect("insert plan");
        assert_eq!(
            store.plan("plan_test").expect("read plan"),
            Some(plan.clone())
        );
        assert_eq!(store.plans().expect("list plans").len(), 1);

        // Re-planning the same tree must not duplicate the row.
        store.upsert_plan(&plan).expect("reinsert plan");
        assert_eq!(store.plans().expect("list plans").len(), 1);
    }

    #[test]
    fn preflight_is_stable_and_pins_snapshot() {
        let coord = coordinator("preflight");
        let first = coord
            .preflight(&json!({"action": "preflight", "target_kind": "repository"}))
            .expect("preflight");
        let second = coord
            .preflight(&json!({"action": "preflight", "target_kind": "repository"}))
            .expect("preflight again");
        assert_eq!(first["plan_id"], second["plan_id"]);
        assert_eq!(first["snapshot"], second["snapshot"]);
        assert_eq!(first["target_kind"], "repository");
        assert!(first["plan_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("plan_"));
    }

    #[test]
    fn preflight_rejects_invalid_target_shapes() {
        let coord = coordinator("invalid");
        assert!(coord
            .preflight(&json!({"action": "preflight", "target_kind": "scoped_path"}))
            .is_err());
        assert!(coord
            .preflight(&json!({"action": "preflight", "target_kind": "ref_diff"}))
            .is_err());
        assert!(coord
            .preflight(&json!({"action": "preflight", "target_kind": "nonsense"}))
            .is_err());
        assert!(coord
            .preflight(&json!({"action": "preflight", "credential_id": "nope"}))
            .is_err());
    }

    #[test]
    fn start_status_cancel_lifecycle() {
        let coord = coordinator("lifecycle");
        let plan = coord
            .preflight(&json!({"action": "preflight"}))
            .expect("preflight");
        let plan_id = plan["plan_id"].as_str().expect("plan id").to_string();

        let started = coord
            .start(&json!({"action": "start", "plan_id": plan_id}))
            .expect("start");
        let operation_id = started["operation_id"]
            .as_str()
            .expect("operation id")
            .to_string();
        assert_eq!(started["state"], "queued");

        let status = coord
            .status(&json!({"action": "status", "operation_id": operation_id}))
            .expect("status");
        assert_eq!(status["state"], "queued");
        assert_eq!(status["terminal"], false);

        coord
            .cancel(&json!({"action": "cancel", "operation_id": operation_id}))
            .expect("cancel");
        let status = coord
            .status(&json!({"action": "status", "operation_id": operation_id}))
            .expect("status");
        assert_eq!(status["state"], "cancelled");
        assert_eq!(status["terminal"], true);

        // Cancelling twice is an error, not a silent success.
        assert!(coord
            .cancel(&json!({"action": "cancel", "operation_id": operation_id}))
            .is_err());
        assert!(coord
            .status(&json!({"action": "status", "operation_id": "op_missing"}))
            .is_err());
    }

    #[test]
    fn start_refuses_a_stale_plan() {
        let coord = coordinator("stale");
        let plan = coord
            .preflight(&json!({"action": "preflight"}))
            .expect("preflight");
        let plan_id = plan["plan_id"].as_str().expect("plan id").to_string();

        fs::write(
            coord.repository_root().join("src/lib.rs"),
            "fn changed() {}\n",
        )
        .expect("mutate tree");

        let err = coord
            .start(&json!({"action": "start", "plan_id": plan_id}))
            .expect_err("stale plan must be refused");
        assert!(format!("{err:#}").contains("stale"));
    }

    #[test]
    fn validate_records_verdict_and_evidence() {
        let coord = coordinator("validate");
        let finding = sample_finding("op_1");
        coord
            .store()
            .upsert_finding(&finding)
            .expect("insert finding");

        let out = coord
            .validate(&json!({
                "action": "validate",
                "finding_id": finding.finding_id,
                "validation_status": "validated",
                "validation_summary": "reachable from request handler",
                "validation_evidence": [
                    {"label": "call path", "explanation": "handler -> read_all"}
                ],
            }))
            .expect("validate");
        assert_eq!(out["validation_status"], "validated");

        let stored = coord
            .store()
            .finding(&finding.finding_id)
            .expect("read finding")
            .expect("finding exists");
        assert_eq!(stored.validation_status, "validated");
        assert_eq!(stored.validation_evidence.len(), 1);
        assert_eq!(stored.validation_evidence[0].label, "call path");

        assert!(coord
            .validate(&json!({
                "action": "validate",
                "finding_id": finding.finding_id,
                "validation_status": "bogus",
            }))
            .is_err());
        assert!(coord
            .validate(&json!({
                "action": "validate",
                "finding_id": "fnd_missing",
                "validation_status": "validated",
            }))
            .is_err());
    }

    #[test]
    fn reimported_finding_keeps_prior_validation() {
        let coord = coordinator("reimport");
        let mut finding = sample_finding("op_2");
        coord.store().upsert_finding(&finding).expect("insert");
        coord
            .validate(&json!({
                "action": "validate",
                "finding_id": finding.finding_id,
                "validation_status": "rejected",
            }))
            .expect("validate");

        // A fresh scan re-reports the same finding as unvalidated.
        finding.validation_status = "unvalidated".to_string();
        finding.updated_at = now_unix();
        coord.store().upsert_finding(&finding).expect("reinsert");

        let stored = coord
            .store()
            .finding(&finding.finding_id)
            .expect("read")
            .expect("exists");
        assert_eq!(stored.validation_status, "rejected");
    }

    #[test]
    fn sarif_document_has_expected_shape() {
        let coord = coordinator("sarif");
        coord
            .store()
            .upsert_finding(&sample_finding("op_3"))
            .expect("insert");
        let sarif = coord.sarif("op_3").expect("sarif");
        assert_eq!(sarif["version"], "2.1.0");
        let run = &sarif["runs"][0];
        assert_eq!(run["tool"]["driver"]["name"], "omp-security");
        assert_eq!(run["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(run["results"][0]["level"], "error");
        assert_eq!(
            run["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/lib.rs"
        );
        assert_eq!(
            run["tool"]["driver"]["rules"].as_array().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn cloud_actions_require_a_codex_token() {
        let coord = coordinator("cloud-unauth");
        for params in [
            json!({"action": "cloud_scans"}),
            json!({
                "action": "cloud_start",
                "repository_id": "r1",
                "repository_url": "https://example.test/r1",
                "environment_id": "e1"
            }),
            json!({"action": "cloud_status", "cloud_configuration_id": "cloud_1"}),
            json!({"action": "cloud_pull", "cloud_configuration_id": "cloud_1"}),
        ] {
            let err = match params["action"].as_str().expect("action") {
                "cloud_scans" => coord.cloud_scans(&params).expect_err("must fail"),
                "cloud_start" => coord.cloud_start(&params).expect_err("must fail"),
                "cloud_status" => coord.cloud_status(&params).expect_err("must fail"),
                _ => coord.cloud_pull(&params).expect_err("must fail"),
            };
            let text = format!("{err:#}");
            assert!(
                text.contains("no Codex token is present"),
                "unexpected error text: {text}"
            );
            assert!(text.contains("omp login"), "must point at sign-in: {text}");
        }
        assert!(!coord.is_authenticated());
    }

    #[test]
    fn cloud_configuration_lifecycle_with_token() {
        let coord = coordinator("cloud-auth").with_codex_token(Some("tok_test".to_string()));
        let started = coord
            .cloud_start(&json!({
                "action": "cloud_start",
                "repository_id": "repo_1",
                "repository_url": "https://example.test/repo_1",
                "environment_id": "env_1",
                "lookback_days": 30,
            }))
            .expect("cloud_start");
        let configuration_id = started["cloud_configuration_id"]
            .as_str()
            .expect("configuration id")
            .to_string();
        assert_eq!(started["state"], "enabled");
        assert_eq!(started["lookback_days"], "30");

        let listed = coord
            .cloud_scans(&json!({"action": "cloud_scans"}))
            .expect("scans");
        assert_eq!(listed["count"], 1);

        let status = coord
            .cloud_status(&json!({
                "action": "cloud_status",
                "cloud_configuration_id": configuration_id,
            }))
            .expect("cloud_status");
        assert_eq!(status["state"], "enabled");

        let pulled = coord
            .cloud_pull(&json!({
                "action": "cloud_pull",
                "cloud_configuration_id": configuration_id,
            }))
            .expect("cloud_pull");
        assert_eq!(pulled["state"], "pulled");
        assert_eq!(pulled["pulled"], 0);

        assert!(coord
            .cloud_start(&json!({
                "action": "cloud_start",
                "repository_id": "repo_1",
                "repository_url": "https://example.test/repo_1",
                "environment_id": "env_1",
                "lookback_days": 0,
            }))
            .is_err());
    }

    #[test]
    fn disabled_security_refuses_every_action() {
        let coord = coordinator("disabled").with_security_enabled(false);
        assert!(coord
            .preflight(&json!({"action": "preflight"}))
            .expect_err("must refuse")
            .to_string()
            .contains("disabled in settings"));
        assert!(coord
            .cloud_scans(&json!({"action": "cloud_scans"}))
            .is_err());
    }

    #[test]
    fn slash_command_surfaces_help_and_auth_state() {
        let coord = coordinator("slash");
        assert!(coord
            .run_command("help")
            .expect("help")
            .contains("/security preflight"));
        assert!(coord.run_command("").expect("empty").contains("OMP-native"));
        let status = coord.run_command("status").expect("status");
        assert!(status.contains("security enabled: yes"));
        assert!(status.contains("not signed in"));
        let cloud = coord.run_command("cloud").expect("cloud");
        assert!(cloud.contains("no Codex token is present"));
        assert!(coord
            .run_command("bogus")
            .expect("unknown")
            .contains("unknown /security subcommand"));
    }

    #[test]
    fn slash_command_plan_start_and_sarif_flow() {
        let coord = coordinator("slash-flow");
        let planned = coord.run_command("preflight").expect("preflight");
        assert!(planned.contains("planned plan_"));

        let plan_id = planned
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("plan id from output")
            .to_string();
        assert!(coord.run_command(&format!("plan {plan_id}")).is_ok());
        assert!(coord
            .run_command(&format!("start {plan_id}"))
            .expect("start")
            .contains("queued"));
        assert!(coord.run_command("sarif op_missing").is_ok());
        assert!(coord
            .run_command("plans")
            .expect("plans")
            .contains(&plan_id));
    }

    #[test]
    fn path_matching_handles_prefixes_and_wildcards() {
        assert!(path_matches("src/lib.rs", "src"));
        assert!(path_matches("src/lib.rs", "src/**"));
        assert!(path_matches("src/lib.rs", "*.rs"));
        assert!(path_matches("target/debug/x", "target/"));
        assert!(!path_matches("src/lib.rs", "tests"));
    }
}
