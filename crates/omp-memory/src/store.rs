//! Multiprocess-safe per-bank SQLite store and rebuildable index generations.
//!
//! Durable rows in `working_memory`, `episodic_memory`, and `facts` are
//! authoritative. `fts_*`, `memory_embeddings`, `triples`, and `memory_links`
//! are derived projections carrying a generation stamp, so any of them can be
//! dropped and rebuilt without touching authoritative content.
//!
//! Connections are opened per operation: handles stay `Send + Sync`, and
//! independent processes coordinate through SQLite WAL plus the busy timeout.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    result,
    sync::atomic::{AtomicU64, Ordering},
    time::{self, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension as _, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    bank::BankId,
    extract::{
        ExtractionRequest, MAX_EXTRACTION_BATCH_BYTES, MAX_EXTRACTION_BATCH_JOBS,
        MAX_EXTRACTION_INPUT_BYTES,
    },
    Error, Result,
};

/// Current memory-bank schema contract.
pub const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT_MS: u64 = 5000;
const MAX_ID_BYTES: usize = 128;
static NEXT_MEMORY_ID: AtomicU64 = AtomicU64::new(1);

/// Authoritative memory tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTier {
    /// Fresh retained memory, eligible for working-memory recall.
    Working,
    /// Consolidated durable episode.
    Episodic,
    /// Read-only extracted fact projection.
    Fact,
}

impl MemoryTier {
    /// Returns the persisted tier spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Episodic => "episodic",
            Self::Fact => "fact",
        }
    }
}

impl std::fmt::Display for MemoryTier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One durable memory row hydrated from a bank.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Stable memory identifier.
    pub id: String,
    /// Owning bank.
    pub bank: BankId,
    /// Authoritative tier.
    pub tier: MemoryTier,
    /// Full untruncated content.
    pub content: String,
    /// Optional source label.
    pub source: Option<String>,
    /// Session that authored the row.
    pub session_id: String,
    /// RFC3339 UTC timestamp.
    pub timestamp: String,
    /// Normalized importance in `[0, 1]`.
    pub importance: f64,
    /// Veracity label.
    pub veracity: String,
    /// Semantic memory kind.
    pub memory_type: String,
    /// Arbitrary structured metadata.
    pub metadata: serde_json::Value,
    /// Superseding memory identifier, when invalidated.
    pub superseded_by: Option<String>,
}

/// Input for one durable working-memory row.
pub struct NewMemory<'a> {
    /// Content stored verbatim.
    pub content: &'a str,
    /// Separate text indexed for lexical/vector recall.
    pub embed_text: Option<&'a str>,
    /// Source label.
    pub source: &'a str,
    /// Authoring session.
    pub session_id: &'a str,
    /// Importance, clamped by the store.
    pub importance: f64,
    /// Veracity label.
    pub veracity: &'a str,
    /// Semantic memory type.
    pub memory_type: &'a str,
    /// Structured metadata.
    pub metadata: &'a serde_json::Value,
    /// Optional caller-stable idempotency key.
    pub stable_id: Option<&'a str>,
}

/// Result of a scoped mutable-memory edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditResult {
    /// A mutable row was changed.
    Changed(MemoryTier),
    /// No row with the supplied id exists in this bank.
    NotFound,
    /// The id exists but this operation cannot mutate its tier.
    Ineligible(MemoryTier),
    /// The id names an immutable extracted fact.
    ImmutableFact,
}

/// Borrowed view of one durable retained transcript window handed to memory
/// extraction and embedding.
pub struct RetainedWindow<'a> {
    /// Session journal identity.
    pub session_id: &'a str,
    /// Durable framed transcript.
    pub transcript: &'a str,
    /// Marker-free embedding text.
    pub embed_text: &'a str,
    /// Bounded user-only text durably queued for extraction.
    pub extraction_text: Option<&'a str>,
    /// Structured metadata including source ids and canonical root.
    pub metadata: &'a serde_json::Value,
    /// Inclusive number of user turns durably covered by the window.
    pub retained_through_user_turn: u64,
}

/// One derived vector row produced against a durable generation.
pub struct VectorEntry<'a> {
    /// Memory identifier.
    pub memory_id: &'a str,
    /// Dense vector in model order.
    pub vector: &'a [f32],
}

/// Stored vector used by the recall voice.
#[derive(Debug)]
pub struct StoredVector {
    /// Memory identifier.
    pub memory_id: String,
    /// Dense vector.
    pub vector: Vec<f32>,
}

/// Generation fence for authoritative data and derived indexes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexGeneration {
    /// Generation of authoritative rows.
    pub durable: u64,
    /// Durable generation represented by vectors.
    pub vector: u64,
    /// Durable generation represented by graph rows.
    pub graph: u64,
}

/// Lexical/temporal candidate returned to recall.
#[derive(Clone, Debug)]
pub struct RankedCandidate {
    /// Hydrated memory.
    pub record: MemoryRecord,
    /// Voice-local normalized score.
    pub score: f64,
}

/// Lightweight bank ownership handle.
///
/// Connections are opened per operation so handles are thread-safe and
/// independent processes coordinate through SQLite WAL and busy timeout.
#[derive(Clone, Debug)]
pub struct BankStore {
    path: PathBuf,
    bank: BankId,
    identity_root: PathBuf,
    working_limit: usize,
    working_ttl_hours: u64,
}

impl BankStore {
    /// Opens or migrates a bank and records its canonical scope identity.
    pub fn open(
        path: impl Into<PathBuf>,
        bank: BankId,
        identity_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let store = Self {
            path: path.into(),
            bank,
            identity_root: identity_root.into(),
            working_limit: 1000,
            working_ttl_hours: 24,
        };
        if let Some(parent) = store.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut connection = store.connection()?;
        store.migrate(&mut connection)?;
        Ok(store)
    }

    /// Applies the normalized transient working-memory eviction policy to this
    /// bank handle.
    pub fn with_working_policy(mut self, limit: usize, ttl_hours: u64) -> Self {
        self.working_limit = limit;
        self.working_ttl_hours = ttl_hours;
        self
    }

    /// Returns the non-secret SQLite path for diagnostics only.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the owning bank.
    pub const fn bank(&self) -> &BankId {
        &self.bank
    }

    /// Returns the canonical identity root recorded for this bank.
    pub fn identity_root(&self) -> &Path {
        &self.identity_root
    }

    /// Returns authoritative and derived index generations.
    pub fn generations(&self) -> Result<IndexGeneration> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT durable_generation, vector_generation, graph_generation FROM \
                 index_generations WHERE singleton = 1",
                [],
                |row| {
                    Ok(IndexGeneration {
                        durable: row.get(0)?,
                        vector: row.get(1)?,
                        graph: row.get(2)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    /// Saves one durable working-memory row and invalidates derived
    /// generations.
    pub fn save(&self, input: NewMemory<'_>) -> Result<String> {
        self.save_batch(&[input])?
            .into_iter()
            .next()
            .ok_or(Error::InvalidIdentifier)
    }

    /// Saves a bounded group of working memories atomically.
    ///
    /// A serialization, lock, or SQLite failure rolls back every item, so a
    /// successful tool receipt can never describe a partially retained batch.
    pub fn save_batch(&self, inputs: &[NewMemory<'_>]) -> Result<Vec<String>> {
        if inputs.is_empty() {
            return Err(Error::InvalidIdentifier);
        }
        for input in inputs {
            if input.content.trim().is_empty() || !input.importance.is_finite() {
                return Err(Error::InvalidIdentifier);
            }
        }
        let timestamp = utc_timestamp();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut ids = Vec::with_capacity(inputs.len());
        let mut changed_sessions = HashSet::new();
        {
            let mut duplicate = transaction.prepare(
                "SELECT id FROM working_memory WHERE content = ?1 AND session_id = ?2 AND \
                 superseded_by IS NULL ORDER BY rowid LIMIT 1",
            )?;
            let mut refresh = transaction.prepare(
                "UPDATE working_memory SET importance = MAX(importance, ?2), timestamp = ?3, \
                 source = ?4, embed_text = COALESCE(?5, embed_text), metadata_json = ?6, veracity \
                 = CASE WHEN ?7 != 'unknown' THEN ?7 ELSE veracity END, memory_type = \
                 COALESCE(?8, memory_type) WHERE id = ?1",
            )?;
            let mut insert = transaction.prepare(
                "INSERT OR IGNORE INTO working_memory\n(id, content, embed_text, source, timestamp, \
                 session_id, importance, metadata_json, veracity, memory_type, scope, \
                 channel_id)\nVALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'bank', ?11)",
            )?;
            for input in inputs {
                let content = input.content.trim();
                let metadata = serde_json::to_string(input.metadata)?;
                if input.stable_id.is_none() {
                    let existing = duplicate
                        .query_row(params![content, input.session_id], |row| {
                            row.get::<_, String>(0)
                        })
                        .optional()?;
                    if let Some(existing) = existing {
                        refresh.execute(params![
                            existing.as_str(),
                            input.importance.clamp(0.0, 1.0),
                            timestamp,
                            input.source,
                            input.embed_text,
                            metadata,
                            input.veracity,
                            input.memory_type,
                        ])?;
                        changed_sessions.insert(input.session_id.to_owned());
                        ids.push(existing);
                        continue;
                    }
                }
                let id = match input.stable_id {
                    Some(stable) if valid_memory_id(stable) => stable.to_owned(),
                    _ => new_memory_id(self.bank.as_str(), input.session_id, content),
                };
                if insert.execute(params![
                    id.as_str(),
                    content,
                    input.embed_text,
                    input.source,
                    timestamp,
                    input.session_id,
                    input.importance.clamp(0.0, 1.0),
                    metadata,
                    input.veracity,
                    input.memory_type,
                    self.bank.as_str(),
                ])? != 0
                {
                    changed_sessions.insert(input.session_id.to_owned());
                }
                ids.push(id);
            }
        }
        if !changed_sessions.is_empty() {
            for session_id in changed_sessions {
                prune_working_transaction(
                    &transaction,
                    session_id.as_str(),
                    self.working_limit,
                    self.working_ttl_hours,
                )?;
            }
            bump_durable(&transaction)?;
        }
        transaction.commit()?;
        Ok(ids)
    }

    /// Replaces working-memory content and/or importance.
    pub fn update_working(
        &self,
        id: &str,
        content: Option<&str>,
        importance: Option<f64>,
    ) -> Result<EditResult> {
        if !valid_memory_id(id) {
            return Err(Error::InvalidIdentifier);
        }
        if content.is_none() && importance.is_none() {
            return Err(Error::InvalidIdentifier);
        }
        let content = content.map(str::trim);
        if content == Some("") || importance.is_some_and(|value| !value.is_finite()) {
            return Err(Error::InvalidIdentifier);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_fact(&transaction, id)? {
            return Ok(EditResult::ImmutableFact);
        }
        let changed = transaction.execute(
            "UPDATE working_memory SET content = COALESCE(?2, content), embed_text = COALESCE(?2, \
             embed_text), importance = COALESCE(?3, importance) WHERE id = ?1",
            params![id, content, importance.map(|value| value.clamp(0.0, 1.0))],
        )?;
        if changed == 0 {
            return ineligible(&transaction, id);
        }
        transaction.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
        transaction.execute(
            "DELETE FROM memory_links WHERE source_memory_id = ?1 OR target_memory_id = ?1",
            [id],
        )?;
        bump_durable(&transaction)?;
        transaction.commit()?;
        Ok(EditResult::Changed(MemoryTier::Working))
    }

    /// Permanently deletes one working-memory row.
    pub fn forget_working(&self, id: &str) -> Result<EditResult> {
        if !valid_memory_id(id) {
            return Err(Error::InvalidIdentifier);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_fact(&transaction, id)? {
            return Ok(EditResult::ImmutableFact);
        }
        let changed = transaction.execute("DELETE FROM working_memory WHERE id = ?1", [id])?;
        if changed == 0 {
            return ineligible(&transaction, id);
        }
        transaction.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
        transaction.execute(
            "DELETE FROM memory_links WHERE source_memory_id = ?1 OR target_memory_id = ?1",
            [id],
        )?;
        bump_durable(&transaction)?;
        transaction.commit()?;
        Ok(EditResult::Changed(MemoryTier::Working))
    }

    /// Softly supersedes one working or episodic memory.
    pub fn invalidate(&self, id: &str, replacement_id: Option<&str>) -> Result<EditResult> {
        if !valid_memory_id(id) {
            return Err(Error::InvalidIdentifier);
        }
        if replacement_id.is_some_and(|replacement| !valid_memory_id(replacement)) {
            return Err(Error::InvalidIdentifier);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_fact(&transaction, id)? {
            return Ok(EditResult::ImmutableFact);
        }
        let working = transaction.execute(
            "UPDATE working_memory SET superseded_by = COALESCE(?2, id) WHERE id = ?1",
            params![id, replacement_id],
        )?;
        let tier = if working != 0 {
            MemoryTier::Working
        } else if transaction.execute(
            "UPDATE episodic_memory SET superseded_by = COALESCE(?2, id) WHERE id = ?1",
            params![id, replacement_id],
        )? != 0
        {
            MemoryTier::Episodic
        } else {
            return Ok(EditResult::NotFound);
        };
        bump_durable(&transaction)?;
        transaction.commit()?;
        Ok(EditResult::Changed(tier))
    }

    /// Atomically saves a retained transcript suffix and advances its restart
    /// cursor.
    pub fn retain_window(&self, window: RetainedWindow<'_>) -> Result<Option<String>> {
        let current = self.retention_cursor(window.session_id)?;
        if window.retained_through_user_turn <= current {
            return Ok(None);
        }
        let stable_material = format!(
            "{}:{}:{}",
            window.session_id,
            window.retained_through_user_turn,
            sha256_hex(window.transcript.as_bytes())
        );
        let id = new_memory_id(self.bank.as_str(), window.session_id, &stable_material);
        let timestamp = utc_timestamp();
        let metadata = serde_json::to_string(window.metadata)?;
        if window
            .extraction_text
            .is_some_and(|text| text.len() > MAX_EXTRACTION_INPUT_BYTES)
        {
            return Err(Error::InputTooLarge);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor = transaction
            .query_row(
                "SELECT retained_user_turn FROM retention_cursors WHERE session_id = ?1",
                [window.session_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()?
            .unwrap_or(0);
        if window.retained_through_user_turn <= cursor {
            return Ok(None);
        }
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO working_memory\n(id, content, embed_text, source, timestamp, \
             session_id, importance, metadata_json, veracity, memory_type, scope, \
             channel_id)\nVALUES (?1, ?2, ?3, 'coding-agent-transcript', ?4, ?5, 0.65, ?6, \
             'unknown', 'episode', 'bank', ?7)",
            params![
                id.as_str(),
                window.transcript,
                window.embed_text,
                timestamp,
                window.session_id,
                metadata,
                self.bank.as_str()
            ],
        )?;
        if let Some(input) = window.extraction_text {
            transaction.execute(
                "INSERT OR IGNORE INTO extraction_jobs(source_memory_id, session_id, input, \
                 created_at) VALUES (?1, ?2, ?3, ?4)",
                params![id.as_str(), window.session_id, input, timestamp],
            )?;
        }
        if inserted != 0 {
            prune_working_transaction(
                &transaction,
                window.session_id,
                self.working_limit,
                self.working_ttl_hours,
            )?;
        }
        transaction.execute(
            "INSERT INTO retention_cursors(session_id, retained_user_turn, updated_at) VALUES (?1, \
             ?2, ?3)\nON CONFLICT(session_id) DO UPDATE SET retained_user_turn = \
             MAX(retained_user_turn, excluded.retained_user_turn), updated_at = excluded.updated_at",
            params![window.session_id, window.retained_through_user_turn, timestamp],
        )?;
        bump_durable(&transaction)?;
        transaction.commit()?;
        Ok(Some(id))
    }

    /// Restores the highest durably retained user-turn cursor.
    pub fn retention_cursor(&self, session_id: &str) -> Result<u64> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT retained_user_turn FROM retention_cursors WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Reads the oldest durable extraction jobs under count and aggregate-byte
    /// bounds. Jobs remain queued until [`Self::complete_extraction`] commits.
    pub fn pending_extractions(&self, max_jobs: usize) -> Result<Vec<ExtractionRequest>> {
        let max_jobs = max_jobs.min(MAX_EXTRACTION_BATCH_JOBS);
        if max_jobs == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT input, session_id, source_memory_id FROM extraction_jobs ORDER BY rowid LIMIT \
             ?1",
        )?;
        let rows = statement.query_map([max_jobs], |row| {
            Ok(ExtractionRequest {
                input: row.get::<_, String>(0)?,
                session_id: row.get::<_, String>(1)?,
                source_id: row.get::<_, String>(2)?,
            })
        })?;
        let mut requests = Vec::with_capacity(max_jobs);
        let mut bytes = 0usize;
        for request in rows {
            let request = request?;
            let next_bytes = bytes.saturating_add(request.input.len());
            if !requests.is_empty() && next_bytes > MAX_EXTRACTION_BATCH_BYTES {
                break;
            }
            bytes = next_bytes;
            requests.push(request);
        }
        Ok(requests)
    }

    /// Counts durable extraction jobs for bounded worker drain decisions.
    pub fn pending_extraction_count(&self) -> Result<usize> {
        let connection = self.connection()?;
        connection
            .query_row("SELECT COUNT(*) FROM extraction_jobs", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Promotes unconsolidated working rows to episodic memory in one
    /// crash-consistent transaction.
    pub fn consolidate(&self, session_id: Option<&str>) -> Result<usize> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let predicate = if session_id.is_some() {
            "WHERE session_id = ?1"
        } else {
            ""
        };
        let sql = format!(
            "INSERT OR IGNORE INTO episodic_memory\n(id, content, source, timestamp, session_id, \
             importance, metadata_json, veracity, memory_type, scope, channel_id, \
             created_at)\nSELECT id, content, source, timestamp, session_id, importance, \
             metadata_json, veracity, memory_type, scope, channel_id, created_at\nFROM \
             working_memory {predicate}"
        );
        let promoted = match session_id {
            Some(session) => transaction.execute(&sql, [session])?,
            None => transaction.execute(&sql, [])?,
        };
        let delete_sql = format!("DELETE FROM working_memory {predicate}");
        match session_id {
            Some(session) => transaction.execute(&delete_sql, [session])?,
            None => transaction.execute(&delete_sql, [])?,
        };
        if promoted > 0 {
            bump_durable(&transaction)?;
        }
        transaction.commit()?;
        Ok(promoted)
    }

    /// Replaces the complete vector projection when `expected` still equals
    /// durable generation.
    pub fn replace_vectors(
        &self,
        expected: u64,
        model: &str,
        entries: &[VectorEntry<'_>],
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if durable_generation(&transaction)? != expected {
            return Err(Error::StaleGeneration);
        }
        transaction.execute("DELETE FROM memory_embeddings", [])?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO memory_embeddings(memory_id, vector_blob, dimensions, model, \
                 generation) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for entry in entries {
                insert.execute(params![
                    entry.memory_id,
                    encode_vector(entry.vector),
                    entry.vector.len(),
                    model,
                    expected,
                ])?;
            }
        }
        transaction.execute(
            "UPDATE index_generations SET vector_generation = ?1 WHERE singleton = 1",
            [expected],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads vectors only when their generation exactly represents durable
    /// rows.
    pub fn vectors(&self) -> Result<Vec<StoredVector>> {
        let generations = self.generations()?;
        if generations.vector != generations.durable {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT memory_id, vector_blob, dimensions FROM memory_embeddings WHERE generation = \
             ?1 LIMIT 50000",
        )?;
        let rows = statement.query_map([generations.vector], |row| {
            let memory_id = row.get::<_, String>(0)?;
            let bytes = row.get::<_, Vec<u8>>(1)?;
            let dimensions = row.get::<_, usize>(2)?;
            Ok((memory_id, bytes, dimensions))
        })?;
        let mut output = Vec::new();
        for row in rows {
            let (memory_id, bytes, dimensions) = row?;
            if let Some(vector) = decode_vector(&bytes, dimensions) {
                output.push(StoredVector { memory_id, vector });
            }
        }
        Ok(output)
    }

    /// Saves immutable model-extracted facts and invalidates derived
    /// graph/vector generations.
    pub fn save_extracted_facts(&self, facts: &[NewFact<'_>]) -> Result<usize> {
        self.persist_extracted_facts(facts, None)
    }

    /// Atomically saves an extraction result and acknowledges its durable job.
    ///
    /// A successful empty or fully rejected completion still removes the job;
    /// inference failures never call this method and therefore remain
    /// retryable.
    pub fn complete_extraction(
        &self,
        source_memory_id: &str,
        facts: &[NewFact<'_>],
    ) -> Result<usize> {
        self.persist_extracted_facts(facts, Some(source_memory_id))
    }

    fn persist_extracted_facts(
        &self,
        facts: &[NewFact<'_>],
        completed_source: Option<&str>,
    ) -> Result<usize> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted = 0usize;
        {
            let mut statement = transaction.prepare(
                "INSERT OR IGNORE INTO facts(fact_id, session_id, subject, predicate, object, \
                 timestamp, source_memory_id, confidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for fact in facts {
                inserted += statement.execute(params![
                    fact.fact_id,
                    fact.session_id,
                    fact.subject,
                    fact.predicate,
                    fact.object,
                    fact.timestamp,
                    fact.source_memory_id,
                    fact.confidence.clamp(0.0, 1.0),
                ])?;
            }
        }
        if let Some(source_memory_id) = completed_source {
            transaction.execute(
                "DELETE FROM extraction_jobs WHERE source_memory_id = ?1",
                [source_memory_id],
            )?;
        }
        if inserted > 0 {
            bump_durable(&transaction)?;
        }
        transaction.commit()?;
        Ok(inserted)
    }

    /// Replaces rebuildable graph triples and links under a generation fence.
    pub fn replace_graph(
        &self,
        expected: u64,
        triples: &[GraphTriple<'_>],
        links: &[MemoryLink<'_>],
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if durable_generation(&transaction)? != expected {
            return Err(Error::StaleGeneration);
        }
        transaction.execute("DELETE FROM triples", [])?;
        transaction.execute("DELETE FROM memory_links", [])?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO triples(subject, predicate, object, source_memory_id, confidence, \
                 generation) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for triple in triples {
                insert.execute(params![
                    triple.subject,
                    triple.predicate,
                    triple.object,
                    triple.source_memory_id,
                    triple.confidence.clamp(0.0, 1.0),
                    expected
                ])?;
            }
        }
        {
            let mut insert = transaction.prepare(
                "INSERT INTO memory_links(source_memory_id, target_memory_id, relation, weight, \
                 generation) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for link in links {
                insert.execute(params![
                    link.source_memory_id,
                    link.target_memory_id,
                    link.relation,
                    link.weight.clamp(0.0, 1.0),
                    expected
                ])?;
            }
        }
        transaction.execute(
            "UPDATE index_generations SET graph_generation = ?1 WHERE singleton = 1",
            [expected],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// FTS-ranked working-memory candidates.
    pub fn search_working(&self, query: &str, limit: usize) -> Result<Vec<RankedCandidate>> {
        self.search_fts(
            "fts_working",
            "working_memory",
            MemoryTier::Working,
            query,
            limit,
        )
    }

    /// FTS-ranked episodic candidates with a mild importance merge.
    pub fn search_episodic(&self, query: &str, limit: usize) -> Result<Vec<RankedCandidate>> {
        self.search_fts(
            "fts_episodes",
            "episodic_memory",
            MemoryTier::Episodic,
            query,
            limit,
        )
    }

    /// FTS-ranked immutable extracted facts.
    pub fn search_facts(&self, query: &str, limit: usize) -> Result<Vec<RankedCandidate>> {
        let Some(fts_query) = lexical_query(query) else {
            return Ok(Vec::new());
        };
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT f.fact_id, trim(f.subject || ' ' || f.predicate || ' ' || f.object), \
             'mnemopi-extraction', f.session_id, COALESCE(f.timestamp, ''), f.confidence, \
             'extracted', 'fact', '{}', NULL, bm25(fts_facts) FROM fts_facts JOIN facts f ON \
             f.fact_id = fts_facts.fact_id WHERE fts_facts MATCH ?1 ORDER BY bm25(fts_facts), \
             f.fact_id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![fts_query, limit.clamp(1, 100)], |row| {
            let rank = row.get::<_, f64>(10)?;
            let record = row_to_record(row, &self.bank, MemoryTier::Fact)?;
            let lexical = 1.0 / (1.0 + rank.abs());
            Ok(RankedCandidate {
                score: f64::mul_add(record.importance, 0.2, lexical * 0.8).clamp(0.0, 1.0),
                record,
            })
        })?;
        rows.collect::<result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Recent working-memory candidates used when the query is temporal.
    pub fn recent_working(&self, limit: usize) -> Result<Vec<RankedCandidate>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, content, source, session_id, timestamp, importance, veracity, \
             memory_type, metadata_json, superseded_by\nFROM working_memory WHERE superseded_by IS \
             NULL ORDER BY CASE WHEN timestamp NOT GLOB '*[^0-9]*' THEN CAST(timestamp AS \
             INTEGER) ELSE COALESCE(unixepoch(timestamp) * 1000, 0) END DESC, id LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.clamp(1, 100)], |row| {
            row_to_record(row, &self.bank, MemoryTier::Working).map(|record| RankedCandidate {
                score: record.importance,
                record,
            })
        })?;
        rows.collect::<result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Graph candidates whose triples match query terms, including one-hop
    /// links.
    pub fn graph_candidates(&self, terms: &[String], limit: usize) -> Result<Vec<RankedCandidate>> {
        let generations = self.generations()?;
        if generations.graph != generations.durable || terms.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut scores = HashMap::<String, f64>::new();
        let mut triple_query = connection.prepare(
            "SELECT source_memory_id, confidence FROM triples\nWHERE generation = ?1 AND \
             (lower(subject) LIKE ?2 OR lower(predicate) LIKE ?2 OR lower(object) LIKE ?2)\nLIMIT \
             256",
        )?;
        for term in terms {
            let pattern = format!("%{}%", term.to_lowercase());
            let rows = triple_query.query_map(params![generations.graph, pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?;
            for row in rows {
                let (id, score) = row?;
                scores
                    .entry(id)
                    .and_modify(|current| *current = current.max(score))
                    .or_insert(score);
            }
        }
        let seeds = scores.keys().cloned().collect::<Vec<_>>();
        let mut link_query = connection.prepare(
            "SELECT target_memory_id, weight FROM memory_links WHERE generation = ?1 AND \
             source_memory_id = ?2 LIMIT 64",
        )?;
        for seed in seeds {
            let rows = link_query.query_map(params![generations.graph, seed.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?;
            for row in rows {
                let (id, weight) = row?;
                scores.entry(id).or_insert(weight * 0.5);
            }
        }
        let mut output = Vec::new();
        for (id, score) in scores {
            if let Some(record) = self.get(id.as_str())? {
                output.push(RankedCandidate { record, score });
            }
        }
        output.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.record.id.cmp(&right.record.id))
        });
        output.truncate(limit.clamp(1, 100));
        Ok(output)
    }

    /// Gets a full memory by id across working, episodic, and extracted fact
    /// projections.
    pub fn get(&self, id: &str) -> Result<Option<MemoryRecord>> {
        if !valid_memory_id(id) {
            return Err(Error::InvalidIdentifier);
        }
        let connection = self.connection()?;
        for (table, tier) in [
            ("working_memory", MemoryTier::Working),
            ("episodic_memory", MemoryTier::Episodic),
        ] {
            let sql = format!(
                "SELECT id, content, source, session_id, timestamp, importance, veracity, \
                 memory_type, metadata_json, superseded_by FROM {table} WHERE id = ?1"
            );
            let record = connection
                .query_row(&sql, [id], |row| row_to_record(row, &self.bank, tier))
                .optional()?;
            if let Some(record) = record {
                return Ok(Some(record));
            }
        }
        let fact = connection
            .query_row(
                "SELECT fact_id, subject || ' ' || predicate || ' ' || object, \
                 'mnemopi-extraction', session_id, COALESCE(timestamp, ''), confidence, \
                 'extracted', 'fact', '{}', NULL FROM facts WHERE fact_id = ?1",
                [id],
                |row| row_to_record(row, &self.bank, MemoryTier::Fact),
            )
            .optional()?;
        Ok(fact)
    }

    /// Lists bounded full records in deterministic newest-first order.
    pub fn list(&self, limit: usize) -> Result<Vec<MemoryRecord>> {
        self.list_page(0, limit.clamp(1, 1000))
    }

    /// Lists newest records without materializing content beyond the aggregate
    /// projection bound.
    pub fn list_bounded(
        &self,
        limit: usize,
        max_bytes: usize,
    ) -> Result<(Vec<MemoryRecord>, bool)> {
        let limit = limit.clamp(1, 1000);
        let max_bytes = max_bytes.clamp(1, 4 * 1024 * 1024);
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, content, source, session_id, timestamp, importance, veracity, \
             memory_type, metadata_json, superseded_by, tier_name, length(content) FROM (\nSELECT \
             id, content, source, session_id, timestamp, importance, veracity, memory_type, \
             metadata_json, superseded_by, 'working' AS tier_name FROM working_memory\nUNION \
             ALL\nSELECT id, content, source, session_id, timestamp, importance, veracity, \
             memory_type, metadata_json, superseded_by, 'episodic' AS tier_name FROM \
             episodic_memory\n) ORDER BY CASE WHEN timestamp NOT GLOB '*[^0-9]*' THEN \
             CAST(timestamp AS INTEGER) ELSE COALESCE(unixepoch(timestamp) * 1000, 0) END DESC, id \
             LIMIT ?1",
        )?;
        let mut rows = statement.query([limit.saturating_add(1)])?;
        let mut records = Vec::with_capacity(limit.min(100));
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            if records.len() == limit {
                return Ok((records, true));
            }
            let content_bytes = usize::try_from(row.get::<_, i64>(11)?).unwrap_or(usize::MAX);
            let Some(next) = bytes.checked_add(content_bytes) else {
                return Ok((records, true));
            };
            if next > max_bytes {
                return Ok((records, true));
            }
            let tier = tier_from_name(row.get::<_, String>(10)?.as_str());
            records.push(row_to_record(row, &self.bank, tier)?);
            bytes = next;
        }
        Ok((records, false))
    }

    /// Loads one deterministic page without imposing the resolver's 1000-row
    /// display ceiling.
    pub fn list_page(&self, offset: usize, limit: usize) -> Result<Vec<MemoryRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, content, source, session_id, timestamp, importance, veracity, \
             memory_type, metadata_json, superseded_by, tier_name FROM (\nSELECT rowid, id, \
             content, source, session_id, timestamp, importance, veracity, memory_type, \
             metadata_json, superseded_by, 'working' AS tier_name FROM working_memory\nUNION \
             ALL\nSELECT rowid, id, content, source, session_id, timestamp, importance, veracity, \
             memory_type, metadata_json, superseded_by, 'episodic' AS tier_name FROM \
             episodic_memory\n) ORDER BY CASE WHEN timestamp NOT GLOB '*[^0-9]*' THEN \
             CAST(timestamp AS INTEGER) ELSE COALESCE(unixepoch(timestamp) * 1000, 0) END DESC, \
             rowid DESC, id LIMIT ?1 OFFSET ?2",
        )?;
        let rows = statement.query_map(params![limit.max(1), offset], |row| {
            let tier = tier_from_name(row.get::<_, String>(10)?.as_str());
            row_to_record(row, &self.bank, tier)
        })?;
        rows.collect::<result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Loads one deterministic page of unsuperseded rows for a complete vector
    /// generation rebuild.
    pub fn list_live_page(&self, offset: usize, limit: usize) -> Result<Vec<MemoryRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, content, source, session_id, timestamp, importance, veracity, \
             memory_type, metadata_json, superseded_by, tier_name FROM (\nSELECT id, content, \
             source, session_id, timestamp, importance, veracity, memory_type, metadata_json, \
             superseded_by, 'working' AS tier_name FROM working_memory WHERE superseded_by IS \
             NULL\nUNION ALL\nSELECT id, content, source, session_id, timestamp, importance, \
             veracity, memory_type, metadata_json, superseded_by, 'episodic' AS tier_name FROM \
             episodic_memory WHERE superseded_by IS NULL\n) ORDER BY CASE WHEN timestamp NOT GLOB \
             '*[^0-9]*' THEN CAST(timestamp AS INTEGER) ELSE COALESCE(unixepoch(timestamp) * 1000, \
             0) END DESC, id LIMIT ?1 OFFSET ?2",
        )?;
        let rows = statement.query_map(params![limit.max(1), offset], |row| {
            let tier = tier_from_name(row.get::<_, String>(10)?.as_str());
            row_to_record(row, &self.bank, tier)
        })?;
        rows.collect::<result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Counts durable rows and graph triples.
    pub fn counts(&self) -> Result<StoreCounts> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM working_memory), (SELECT COUNT(*) FROM \
                 episodic_memory), (SELECT COUNT(*) FROM facts), (SELECT COUNT(*) FROM triples)",
                [],
                |row| {
                    Ok(StoreCounts {
                        working: row.get(0)?,
                        episodic: row.get(1)?,
                        facts: row.get(2)?,
                        triples: row.get(3)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    /// Runs SQLite integrity and FTS/vector generation health checks.
    pub fn integrity(&self) -> Result<IntegrityReport> {
        let connection = self.connection()?;
        let integrity =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
        let generations = self.generations()?;
        let vector_rows =
            connection.query_row("SELECT COUNT(*) FROM memory_embeddings", [], |row| {
                row.get(0)
            })?;
        let graph_rows =
            connection.query_row("SELECT COUNT(*) FROM triples", [], |row| row.get(0))?;
        Ok(IntegrityReport {
            integrity,
            generations,
            vector_rows,
            graph_rows,
            vector_current: generations.vector == generations.durable,
            graph_current: generations.graph == generations.durable,
        })
    }

    /// Deletes all bank data inside one exclusive transaction and advances
    /// generations atomically.
    pub fn clear(&self) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        for table in [
            "working_memory",
            "episodic_memory",
            "facts",
            "triples",
            "memory_links",
            "memory_embeddings",
            "extraction_jobs",
            "retention_cursors",
            "scope_adoptions",
        ] {
            transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
        bump_durable(&transaction)?;
        let durable = durable_generation(&transaction)?;
        transaction.execute(
            "UPDATE index_generations SET vector_generation = ?1, graph_generation = ?1 WHERE \
             singleton = 1",
            [durable],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persists a safely adopted legacy bank for this exact canonical identity.
    pub fn persist_adoption(&self, bank: &BankId) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR IGNORE INTO scope_adoptions(identity_root, bank) VALUES (?1, ?2)",
            params![self.identity_root.to_string_lossy(), bank.as_str()],
        )?;
        Ok(())
    }

    /// Loads prior legacy-bank adoptions for this canonical identity.
    pub fn adopted_banks(&self) -> Result<Vec<BankId>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT bank FROM scope_adoptions WHERE identity_root = ?1 ORDER BY bank LIMIT 64",
        )?;
        let rows = statement.query_map([self.identity_root.to_string_lossy().as_ref()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut banks = Vec::new();
        for row in rows {
            if let Ok(bank) = BankId::configured(&row?) {
                banks.push(bank);
            }
        }
        Ok(banks)
    }

    fn search_fts(
        &self,
        fts: &str,
        table: &str,
        tier: MemoryTier,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RankedCandidate>> {
        let Some(fts_query) = lexical_query(query) else {
            return Ok(Vec::new());
        };
        let connection = self.connection()?;
        let sql = format!(
            "SELECT m.id, m.content, m.source, m.session_id, m.timestamp, m.importance, \
             m.veracity, m.memory_type, m.metadata_json, m.superseded_by, bm25({fts})\nFROM {fts} \
             JOIN {table} m ON m.id = {fts}.id\nWHERE {fts} MATCH ?1 AND m.superseded_by IS NULL \
             ORDER BY bm25({fts}), m.id LIMIT ?2"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params![fts_query, limit.clamp(1, 100)], |row| {
            let rank = row.get::<_, f64>(10)?;
            let record = row_to_record(row, &self.bank, tier)?;
            let lexical = 1.0 / (1.0 + rank.abs());
            Ok(RankedCandidate {
                score: f64::mul_add(record.importance, 0.2, lexical * 0.8).clamp(0.0, 1.0),
                record,
            })
        })?;
        rows.collect::<result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Opens one short-lived connection with WAL and the documented pragmas.
    pub fn connection(&self) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(&self.path, flags)?;
        connection.busy_timeout(time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
        )?;
        Ok(connection)
    }

    fn migrate(&self, connection: &mut Connection) -> Result<()> {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        let version =
            transaction.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Sqlite(rusqlite::Error::InvalidQuery));
        }
        if version < SCHEMA_VERSION {
            migrate_authoritative_tables(&transaction)?;
            transaction.execute_batch(RESET_REBUILDABLE_INDEXES)?;
        }
        transaction.execute_batch(SCHEMA)?;
        normalize_authoritative_rows(&transaction)?;
        if version < SCHEMA_VERSION {
            transaction.execute_batch(REBUILD_SEARCH_INDEXES)?;
            transaction.execute(
                "UPDATE index_generations SET durable_generation = durable_generation + 1, \
                 vector_generation = 0, graph_generation = 0 WHERE singleton = 1",
                [],
            )?;
        }
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.execute(
            "INSERT INTO bank_scope(singleton, identity_root, bank) VALUES (1, ?1, ?2)\nON \
             CONFLICT(singleton) DO UPDATE SET identity_root = excluded.identity_root, bank = \
             excluded.bank",
            params![self.identity_root.to_string_lossy(), self.bank.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

/// Immutable extracted fact projection.
pub struct NewFact<'a> {
    /// Stable fact identifier.
    pub fact_id: &'a str,
    /// Authoring session.
    pub session_id: &'a str,
    /// Subject.
    pub subject: &'a str,
    /// Predicate.
    pub predicate: &'a str,
    /// Object.
    pub object: &'a str,
    /// Optional source timestamp.
    pub timestamp: Option<&'a str>,
    /// Durable source memory.
    pub source_memory_id: &'a str,
    /// Extraction confidence.
    pub confidence: f64,
}

/// Rebuildable graph fact.
pub struct GraphTriple<'a> {
    /// Subject.
    pub subject: &'a str,
    /// Predicate.
    pub predicate: &'a str,
    /// Object.
    pub object: &'a str,
    /// Durable source memory.
    pub source_memory_id: &'a str,
    /// Extraction confidence.
    pub confidence: f64,
}

/// Rebuildable associative link.
pub struct MemoryLink<'a> {
    /// Source memory.
    pub source_memory_id: &'a str,
    /// Target memory.
    pub target_memory_id: &'a str,
    /// Relation label.
    pub relation: &'a str,
    /// Link weight.
    pub weight: f64,
}

/// Store row counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoreCounts {
    /// Working rows.
    pub working: u64,
    /// Episodic rows.
    pub episodic: u64,
    /// Extracted facts.
    pub facts: u64,
    /// Graph triples.
    pub triples: u64,
}

/// Bank integrity and index health.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntegrityReport {
    /// SQLite integrity result (`ok` when healthy).
    pub integrity: String,
    /// Current generations.
    pub generations: IndexGeneration,
    /// Vector rows.
    pub vector_rows: u64,
    /// Graph rows.
    pub graph_rows: u64,
    /// Whether vectors represent the durable generation.
    pub vector_current: bool,
    /// Whether graph rows represent the durable generation.
    pub graph_current: bool,
}

fn is_fact(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<bool> {
    Ok(transaction
        .query_row("SELECT 1 FROM facts WHERE fact_id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some())
}

fn ineligible(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<EditResult> {
    let episodic = transaction
        .query_row("SELECT 1 FROM episodic_memory WHERE id = ?1", [id], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    Ok(if episodic {
        EditResult::Ineligible(MemoryTier::Episodic)
    } else {
        EditResult::NotFound
    })
}

fn tier_from_name(name: &str) -> MemoryTier {
    if name == "working" {
        MemoryTier::Working
    } else {
        MemoryTier::Episodic
    }
}

fn row_to_record(
    row: &rusqlite::Row<'_>,
    bank: &BankId,
    tier: MemoryTier,
) -> rusqlite::Result<MemoryRecord> {
    let metadata = row
        .get::<_, Option<String>>(8)?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null);
    Ok(MemoryRecord {
        id: row.get::<_, String>(0)?,
        bank: bank.clone(),
        tier,
        content: row.get::<_, String>(1)?,
        source: row.get::<_, Option<String>>(2)?,
        session_id: row.get::<_, String>(3)?,
        timestamp: row.get::<_, String>(4)?,
        importance: row.get(5)?,
        veracity: row.get::<_, String>(6)?,
        memory_type: row.get::<_, String>(7)?,
        metadata,
        superseded_by: row.get::<_, Option<String>>(9)?,
    })
}

fn bump_durable(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute(
        "UPDATE index_generations SET durable_generation = durable_generation + 1 WHERE singleton \
         = 1",
        [],
    )?;
    Ok(())
}

fn durable_generation(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<u64> {
    transaction.query_row(
        "SELECT durable_generation FROM index_generations WHERE singleton = 1",
        [],
        |row| row.get(0),
    )
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(bytes: &[u8], dimensions: usize) -> Option<Vec<f32>> {
    if dimensions == 0 || bytes.len() != dimensions.checked_mul(size_of::<f32>())? {
        return None;
    }
    let mut vector = Vec::with_capacity(dimensions);
    #[allow(clippy::chunks_exact_to_as_chunks)]
    for chunk in bytes.chunks_exact(size_of::<f32>()) {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(chunk);
        let value = f32::from_le_bytes(raw);
        if !value.is_finite() {
            return None;
        }
        vector.push(value);
    }
    Some(vector)
}

/// Builds a bounded FTS5 OR-query from free text.
pub fn lexical_query(query: &str) -> Option<String> {
    let mut terms = query
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .filter(|term| term.chars().count() >= 2)
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .take(24)
        .collect::<Vec<_>>();
    terms.sort_unstable();
    terms.dedup();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

fn valid_memory_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn new_memory_id(bank: &str, session: &str, content: &str) -> String {
    let serial = NEXT_MEMORY_ID.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let material = format!("{bank}\0{session}\0{content}\0{now}\0{serial}");
    let hex = sha256_hex(material.as_bytes());
    format!("mem_{}", &hex[..24])
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

/// Returns the current UTC instant as RFC3339 with millisecond precision.
pub fn utc_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let millis = duration.subsec_millis();
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = remainder / 3_600;
    let minute = (remainder % 3_600) / 60;
    let second = remainder % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Converts days since the Unix epoch into a proleptic Gregorian civil date.
///
/// Howard Hinnant's `civil_from_days`, valid for the whole `i64` day range used
/// here. Kept local so the crate needs no date dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn unix_millis() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis())
}

fn prune_working_transaction(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    limit: usize,
    ttl_hours: u64,
) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }
    let ttl_millis = u128::from(ttl_hours)
        .saturating_mul(60)
        .saturating_mul(60)
        .saturating_mul(1000);
    let cutoff = unix_millis()?.saturating_sub(ttl_millis).to_string();
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    transaction.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS working_eviction_ids (
            id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         DELETE FROM working_eviction_ids;",
    )?;
    transaction.execute(
        "INSERT INTO working_eviction_ids(id)
         SELECT id FROM working_memory
         WHERE session_id = ?1
           AND lower(COALESCE(source, '')) NOT IN ('imported', 'import')
           AND (
             CASE WHEN timestamp NOT GLOB '*[^0-9]*' THEN CAST(timestamp AS INTEGER)
                  ELSE COALESCE(unixepoch(timestamp) * 1000, 0) END < CAST(?2 AS INTEGER)
             OR id NOT IN (
               SELECT id FROM working_memory
               WHERE session_id = ?1
                 AND lower(COALESCE(source, '')) NOT IN ('imported', 'import')
               ORDER BY CASE WHEN timestamp NOT GLOB '*[^0-9]*' THEN CAST(timestamp AS INTEGER)
                             ELSE COALESCE(unixepoch(timestamp) * 1000, 0) END DESC, rowid DESC
               LIMIT ?3
             )
           )",
        params![session_id, cutoff, limit],
    )?;
    transaction.execute_batch(
        "DELETE FROM memory_embeddings
           WHERE memory_id IN (SELECT id FROM working_eviction_ids);
         DELETE FROM extraction_jobs
           WHERE source_memory_id IN (SELECT id FROM working_eviction_ids);
         DELETE FROM facts
           WHERE source_memory_id IN (SELECT id FROM working_eviction_ids);
         DELETE FROM triples
           WHERE source_memory_id IN (SELECT id FROM working_eviction_ids);",
    )?;
    transaction.execute(
        "DELETE FROM memory_links
         WHERE source_memory_id IN (SELECT id FROM working_eviction_ids)
            OR target_memory_id IN (SELECT id FROM working_eviction_ids)",
        [],
    )?;
    let removed = transaction.execute(
        "DELETE FROM working_memory WHERE id IN (SELECT id FROM working_eviction_ids)",
        [],
    )?;
    if removed != 0 {
        transaction.execute(
            "UPDATE retention_cursors
             SET retained_user_turn = COALESCE((
               SELECT MAX(retained_through) FROM (
                 SELECT CAST(json_extract(metadata_json, '$.retained_through_user_turn') AS INTEGER)
                          AS retained_through
                   FROM working_memory
                  WHERE session_id = ?1 AND source = 'coding-agent-transcript'
                 UNION ALL
                 SELECT CAST(json_extract(metadata_json, '$.retained_through_user_turn') AS INTEGER)
                          AS retained_through
                   FROM episodic_memory
                  WHERE session_id = ?1 AND source = 'coding-agent-transcript'
               )
             ), 0)
             WHERE session_id = ?1",
            [session_id],
        )?;
    }
    transaction.execute("DELETE FROM working_eviction_ids", [])?;
    Ok(removed)
}

const RESET_REBUILDABLE_INDEXES: &str = r"
DROP TRIGGER IF EXISTS wm_ai;
DROP TRIGGER IF EXISTS wm_ad;
DROP TRIGGER IF EXISTS wm_au;
DROP TRIGGER IF EXISTS em_ai;
DROP TRIGGER IF EXISTS em_ad;
DROP TRIGGER IF EXISTS em_au;
DROP TRIGGER IF EXISTS facts_ai;
DROP TRIGGER IF EXISTS facts_ad;
DROP TRIGGER IF EXISTS facts_au;
DROP TABLE IF EXISTS fts_working;
DROP TABLE IF EXISTS fts_episodes;
DROP TABLE IF EXISTS fts_facts;
DROP TABLE IF EXISTS memory_embeddings;
DROP TABLE IF EXISTS triples;
DROP TABLE IF EXISTS memory_links;
";

const REBUILD_SEARCH_INDEXES: &str = r"
INSERT INTO fts_working(id, content)
SELECT id, COALESCE(embed_text, content) FROM working_memory;
INSERT INTO fts_episodes(id, content)
SELECT id, content FROM episodic_memory;
INSERT INTO fts_facts(fact_id, content)
SELECT fact_id, trim(subject || ' ' || predicate || ' ' || object) FROM facts;
";

/// Authoritative schema contract, version [`SCHEMA_VERSION`].
const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS bank_scope (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    identity_root TEXT NOT NULL,
    bank TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS index_generations (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    durable_generation INTEGER NOT NULL DEFAULT 0,
    vector_generation INTEGER NOT NULL DEFAULT 0,
    graph_generation INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO index_generations(singleton) VALUES (1);
CREATE TABLE IF NOT EXISTS working_memory (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    embed_text TEXT,
    source TEXT,
    timestamp TEXT NOT NULL,
    session_id TEXT NOT NULL,
    importance REAL NOT NULL DEFAULT 0.5 CHECK (importance >= 0 AND importance <= 1),
    metadata_json TEXT,
    veracity TEXT NOT NULL DEFAULT 'unknown',
    memory_type TEXT NOT NULL DEFAULT 'unknown',
    superseded_by TEXT,
    scope TEXT NOT NULL DEFAULT 'bank',
    channel_id TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_wm_session ON working_memory(session_id);
CREATE INDEX IF NOT EXISTS idx_wm_timestamp ON working_memory(timestamp);
CREATE INDEX IF NOT EXISTS idx_wm_live ON working_memory(superseded_by);
CREATE TABLE IF NOT EXISTS episodic_memory (
    rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT UNIQUE NOT NULL,
    content TEXT NOT NULL,
    source TEXT,
    timestamp TEXT NOT NULL,
    session_id TEXT NOT NULL,
    importance REAL NOT NULL DEFAULT 0.5 CHECK (importance >= 0 AND importance <= 1),
    metadata_json TEXT,
    veracity TEXT NOT NULL DEFAULT 'unknown',
    memory_type TEXT NOT NULL DEFAULT 'unknown',
    superseded_by TEXT,
    scope TEXT NOT NULL DEFAULT 'bank',
    channel_id TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_em_session ON episodic_memory(session_id);
CREATE INDEX IF NOT EXISTS idx_em_timestamp ON episodic_memory(timestamp);
CREATE INDEX IF NOT EXISTS idx_em_live ON episodic_memory(superseded_by);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_working USING fts5(id UNINDEXED, content);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_episodes USING fts5(id UNINDEXED, content);
CREATE TRIGGER IF NOT EXISTS wm_ai AFTER INSERT ON working_memory BEGIN
    INSERT INTO fts_working(id, content) VALUES (new.id, COALESCE(new.embed_text, new.content));
END;
CREATE TRIGGER IF NOT EXISTS wm_ad AFTER DELETE ON working_memory BEGIN
    DELETE FROM fts_working WHERE id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS wm_au AFTER UPDATE OF content, embed_text ON working_memory BEGIN
    DELETE FROM fts_working WHERE id = old.id;
    INSERT INTO fts_working(id, content) VALUES (new.id, COALESCE(new.embed_text, new.content));
END;
CREATE TRIGGER IF NOT EXISTS em_ai AFTER INSERT ON episodic_memory BEGIN
    INSERT INTO fts_episodes(id, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS em_ad AFTER DELETE ON episodic_memory BEGIN
    DELETE FROM fts_episodes WHERE id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS em_au AFTER UPDATE OF content ON episodic_memory BEGIN
    DELETE FROM fts_episodes WHERE id = old.id;
    INSERT INTO fts_episodes(id, content) VALUES (new.id, new.content);
END;
CREATE TABLE IF NOT EXISTS facts (
    fact_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL,
    timestamp TEXT,
    source_memory_id TEXT,
    confidence REAL NOT NULL DEFAULT 1.0
);
CREATE INDEX IF NOT EXISTS idx_facts_source ON facts(source_memory_id);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_facts USING fts5(fact_id UNINDEXED, content);
CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
    INSERT INTO fts_facts(fact_id, content)
    VALUES (new.fact_id, trim(new.subject || ' ' || new.predicate || ' ' || new.object));
END;
CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
    DELETE FROM fts_facts WHERE fact_id = old.fact_id;
END;
CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE OF subject, predicate, object ON facts BEGIN
    DELETE FROM fts_facts WHERE fact_id = old.fact_id;
    INSERT INTO fts_facts(fact_id, content)
    VALUES (new.fact_id, trim(new.subject || ' ' || new.predicate || ' ' || new.object));
END;
CREATE TABLE IF NOT EXISTS extraction_jobs (
    source_memory_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    input TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS memory_embeddings (
    memory_id TEXT PRIMARY KEY,
    vector_blob BLOB NOT NULL,
    dimensions INTEGER NOT NULL,
    model TEXT NOT NULL,
    generation INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS triples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    subject TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL,
    source_memory_id TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    generation INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_triples_subject ON triples(subject);
CREATE INDEX IF NOT EXISTS idx_triples_object ON triples(object);
CREATE TABLE IF NOT EXISTS memory_links (
    source_memory_id TEXT NOT NULL,
    target_memory_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    weight REAL NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY(source_memory_id, target_memory_id, relation)
);
CREATE TABLE IF NOT EXISTS retention_cursors (
    session_id TEXT PRIMARY KEY,
    retained_user_turn INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS scope_adoptions (
    identity_root TEXT NOT NULL,
    bank TEXT NOT NULL,
    PRIMARY KEY(identity_root, bank)
);
";

fn migrate_authoritative_tables(transaction: &rusqlite::Transaction<'_>) -> Result<()> {
    for (table, columns) in [
        (
            "working_memory",
            &[
                ("embed_text", "TEXT"),
                ("source", "TEXT"),
                ("timestamp", "TEXT"),
                ("session_id", "TEXT DEFAULT 'default'"),
                ("importance", "REAL DEFAULT 0.5"),
                ("metadata_json", "TEXT"),
                ("veracity", "TEXT DEFAULT 'unknown'"),
                ("memory_type", "TEXT DEFAULT 'unknown'"),
                ("superseded_by", "TEXT"),
                ("scope", "TEXT DEFAULT 'bank'"),
                ("channel_id", "TEXT"),
                ("created_at", "TEXT DEFAULT ''"),
            ][..],
        ),
        (
            "episodic_memory",
            &[
                ("source", "TEXT"),
                ("timestamp", "TEXT"),
                ("session_id", "TEXT DEFAULT 'default'"),
                ("importance", "REAL DEFAULT 0.5"),
                ("metadata_json", "TEXT"),
                ("veracity", "TEXT DEFAULT 'unknown'"),
                ("memory_type", "TEXT DEFAULT 'unknown'"),
                ("superseded_by", "TEXT"),
                ("scope", "TEXT DEFAULT 'bank'"),
                ("channel_id", "TEXT"),
                ("created_at", "TEXT DEFAULT ''"),
            ][..],
        ),
    ] {
        if !table_exists(transaction, table)? {
            continue;
        }
        for (column, definition) in columns {
            add_column_if_missing(transaction, table, column, definition)?;
        }
    }
    if table_exists(transaction, "facts")? {
        add_column_if_missing(transaction, "facts", "source_memory_id", "TEXT")?;
        if column_exists(transaction, "facts", "source_msg_id")? {
            transaction.execute(
                "UPDATE facts SET source_memory_id = COALESCE(source_memory_id, source_msg_id)",
                [],
            )?;
        }
    }
    Ok(())
}

fn normalize_authoritative_rows(transaction: &rusqlite::Transaction<'_>) -> Result<()> {
    for table in ["working_memory", "episodic_memory"] {
        transaction.execute(
            &format!(
                "UPDATE {table} SET timestamp = COALESCE(NULLIF(timestamp, ''), NULLIF(created_at, \
                 ''), CURRENT_TIMESTAMP), session_id = COALESCE(session_id, 'default'), importance \
                 = MIN(1.0, MAX(0.0, COALESCE(importance, 0.5))), veracity = COALESCE(veracity, \
                 'unknown'), memory_type = COALESCE(memory_type, 'unknown'), scope = \
                 COALESCE(scope, 'bank')"
            ),
            [],
        )?;
    }
    transaction.execute(
        "UPDATE facts SET session_id = COALESCE(session_id, 'default'), confidence = MIN(1.0, \
         MAX(0.0, COALESCE(confidence, 1.0)))",
        [],
    )?;
    Ok(())
}

fn table_exists(transaction: &rusqlite::Transaction<'_>, table: &str) -> Result<bool> {
    transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map(|opt| opt.is_some())
        .map_err(Into::into)
}

fn column_exists(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn add_column_if_missing(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    if !column_exists(transaction, table, column)? {
        transaction.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::test_support::temp_root;

    fn store_with(limit: usize, ttl_hours: u64) -> BankStore {
        let root = temp_root("store");
        BankStore::open(
            root.join("mnemopi.db"),
            BankId::configured("test").expect("bank"),
            root,
        )
        .expect("store")
        .with_working_policy(limit, ttl_hours)
    }

    fn store() -> BankStore {
        store_with(10, 24)
    }

    fn save(store: &BankStore, id: &str) {
        save_content(store, id, id);
    }

    fn save_content(store: &BankStore, id: &str, content: &str) {
        store
            .save(NewMemory {
                content,
                embed_text: Some(content),
                source: "test",
                session_id: "session",
                importance: 0.5,
                veracity: "user",
                memory_type: "scratch",
                metadata: &serde_json::Value::Null,
                stable_id: Some(id),
            })
            .expect("save memory");
    }

    fn table_names(store: &BankStore) -> Vec<String> {
        let connection = store.connection().expect("connection");
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<result::Result<Vec<_>, _>>()
            .expect("rows")
    }

    #[test]
    fn schema_v3_creates_every_authoritative_and_derived_table() {
        let store = store();
        let names = table_names(&store);
        for expected in [
            "bank_scope",
            "index_generations",
            "working_memory",
            "episodic_memory",
            "fts_working",
            "fts_episodes",
            "facts",
            "fts_facts",
            "extraction_jobs",
            "memory_embeddings",
            "triples",
            "memory_links",
            "retention_cursors",
            "scope_adoptions",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing table {expected}"
            );
        }
        let connection = store.connection().expect("connection");
        let version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 3);
        let scope = connection
            .query_row(
                "SELECT identity_root, bank FROM bank_scope WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("bank scope row");
        assert_eq!(scope.1, "test");
        assert_eq!(
            store.generations().expect("generations").durable,
            1,
            "first migration advances the durable generation once"
        );
    }

    #[test]
    fn reopening_an_existing_bank_does_not_rerun_migration() {
        let root = temp_root("reopen");
        let path = root.join("mnemopi.db");
        let bank = BankId::configured("test").expect("bank");
        let first = BankStore::open(&path, bank.clone(), root.clone()).expect("open");
        save(&first, "stable");
        let durable = first.generations().expect("generations").durable;
        let second = BankStore::open(&path, bank, root).expect("reopen");
        assert_eq!(second.generations().expect("generations").durable, durable);
        assert_eq!(second.counts().expect("counts").working, 1);
    }

    #[test]
    fn fts5_search_ranks_and_skips_superseded_rows() {
        let store = store();
        save_content(
            &store,
            "mem-rust",
            "User strongly prefers the Rust programming language",
        );
        save_content(&store, "mem-python", "User once mentioned Python packaging");
        save_content(&store, "mem-other", "Unrelated note about lunch");

        let hits = store.search_working("Rust programming", 8).expect("search");
        assert_eq!(hits.len(), 1, "only the Rust row mentions both terms");
        assert_eq!(hits[0].record.id, "mem-rust");
        assert!(hits[0].score > 0.0);

        let python = store.search_working("Python", 8).expect("search");
        assert_eq!(python.len(), 1);
        assert_eq!(python[0].record.id, "mem-python");

        assert!(store
            .search_working("zzzznomatch", 8)
            .expect("search")
            .is_empty());
        assert!(
            store.search_working("a", 8).expect("search").is_empty(),
            "single-character terms produce no FTS query"
        );

        store.invalidate("mem-rust", None).expect("invalidate");
        assert!(
            store
                .search_working("Rust programming", 8)
                .expect("search")
                .is_empty(),
            "superseded rows leave the FTS recall path"
        );
    }

    #[test]
    fn fts5_index_follows_content_updates_and_deletes() {
        let store = store();
        save_content(&store, "mem-a", "original alpha content");
        assert_eq!(store.search_working("alpha", 8).expect("search").len(), 1);

        store
            .update_working("mem-a", Some("replaced beta content"), None)
            .expect("update");
        assert!(store.search_working("alpha", 8).expect("search").is_empty());
        assert_eq!(store.search_working("beta", 8).expect("search").len(), 1);

        store.forget_working("mem-a").expect("forget");
        assert!(store.search_working("beta", 8).expect("search").is_empty());
    }

    #[test]
    fn episodic_search_and_fact_search_use_their_own_fts_tables() {
        let store = store();
        save_content(
            &store,
            "mem-episode",
            "Retained episode about SQLite WAL mode",
        );
        assert_eq!(store.consolidate(None).expect("consolidate"), 1);
        assert_eq!(store.counts().expect("counts").working, 0);
        assert_eq!(store.counts().expect("counts").episodic, 1);
        let episodic = store.search_episodic("SQLite WAL", 8).expect("search");
        assert_eq!(episodic.len(), 1);
        assert_eq!(episodic[0].record.tier, MemoryTier::Episodic);

        store
            .save_extracted_facts(&[NewFact {
                fact_id: "fact-1",
                session_id: "session",
                subject: "User",
                predicate: "prefers",
                object: "Rust",
                timestamp: None,
                source_memory_id: "mem-episode",
                confidence: 0.9,
            }])
            .expect("save fact");
        let facts = store.search_facts("User Rust", 8).expect("search");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].record.id, "fact-1");
        assert_eq!(facts[0].record.tier, MemoryTier::Fact);
        assert_eq!(facts[0].record.importance, 0.9);
    }

    #[test]
    fn retain_window_advances_the_cursor_idempotently() {
        let store = store();
        let metadata = serde_json::json!({"retained_through_user_turn": 2});
        let window = || RetainedWindow {
            session_id: "session",
            transcript: "[role: user]\nremember the WAL pragma\n[user:end]",
            embed_text: "remember the WAL pragma",
            extraction_text: None,
            metadata: &metadata,
            retained_through_user_turn: 2,
        };
        let id = store
            .retain_window(window())
            .expect("retain")
            .expect("stored");
        assert!(store
            .retain_window(window())
            .expect("retain again")
            .is_none());
        assert_eq!(store.retention_cursor("session").expect("cursor"), 2);
        assert_eq!(store.counts().expect("counts").working, 1);
        let record = store.get(&id).expect("lookup").expect("record");
        assert_eq!(record.source.as_deref(), Some("coding-agent-transcript"));
        assert_eq!(record.memory_type, "episode");

        let next_metadata = serde_json::json!({"retained_through_user_turn": 4});
        assert!(store
            .retain_window(RetainedWindow {
                session_id: "session",
                transcript: "[role: user]\nand the busy timeout\n[user:end]",
                embed_text: "and the busy timeout",
                extraction_text: Some("[role: user]\nand the busy timeout\n[user:end]"),
                metadata: &next_metadata,
                retained_through_user_turn: 4,
            })
            .expect("retain")
            .is_some());
        assert_eq!(store.retention_cursor("session").expect("cursor"), 4);
        assert_eq!(store.pending_extraction_count().expect("pending"), 1);
    }

    #[test]
    fn forget_removes_working_rows_and_rejects_episodic_and_fact_rows() {
        let store = store();
        save(&store, "mem-working");
        assert_eq!(
            store.forget_working("mem-working").expect("forget"),
            EditResult::Changed(MemoryTier::Working)
        );
        assert!(store.get("mem-working").expect("lookup").is_none());
        assert_eq!(
            store.forget_working("mem-working").expect("forget"),
            EditResult::NotFound
        );

        save(&store, "mem-episodic");
        store.consolidate(None).expect("consolidate");
        assert_eq!(
            store.forget_working("mem-episodic").expect("forget"),
            EditResult::Ineligible(MemoryTier::Episodic)
        );
        assert!(store.get("mem-episodic").expect("lookup").is_some());

        store
            .save_extracted_facts(&[NewFact {
                fact_id: "fact-immutable",
                session_id: "session",
                subject: "User",
                predicate: "prefers",
                object: "Rust",
                timestamp: None,
                source_memory_id: "mem-episodic",
                confidence: 1.0,
            }])
            .expect("save fact");
        assert_eq!(
            store.forget_working("fact-immutable").expect("forget"),
            EditResult::ImmutableFact
        );
        assert_eq!(
            store
                .update_working("fact-immutable", Some("x"), None)
                .expect("update"),
            EditResult::ImmutableFact
        );
    }

    #[test]
    fn invalidate_supersedes_working_and_episodic_rows_softly() {
        let store = store();
        save(&store, "mem-old");
        assert_eq!(
            store
                .invalidate("mem-old", Some("mem-new"))
                .expect("invalidate"),
            EditResult::Changed(MemoryTier::Working)
        );
        let record = store.get("mem-old").expect("lookup").expect("record");
        assert_eq!(record.superseded_by.as_deref(), Some("mem-new"));

        save(&store, "mem-episode");
        store.consolidate(None).expect("consolidate");
        assert_eq!(
            store.invalidate("mem-episode", None).expect("invalidate"),
            EditResult::Changed(MemoryTier::Episodic)
        );
        let episode = store.get("mem-episode").expect("lookup").expect("record");
        assert_eq!(episode.superseded_by.as_deref(), Some("mem-episode"));

        assert_eq!(
            store.invalidate("mem-missing", None).expect("invalidate"),
            EditResult::NotFound
        );
        assert!(store.invalidate("../escape", None).is_err());
    }

    #[test]
    fn update_changes_content_and_importance_and_bumps_the_generation() {
        let store = store();
        save(&store, "mem-edit");
        let before = store.generations().expect("generations").durable;
        assert_eq!(
            store
                .update_working("mem-edit", Some("revised content"), Some(0.9))
                .expect("update"),
            EditResult::Changed(MemoryTier::Working)
        );
        let record = store.get("mem-edit").expect("lookup").expect("record");
        assert_eq!(record.content, "revised content");
        assert!((record.importance - 0.9).abs() < f64::EPSILON);
        assert!(store.generations().expect("generations").durable > before);
        assert!(store.update_working("mem-edit", None, None).is_err());
    }

    #[test]
    fn vector_projection_is_generation_fenced_and_dropped_when_stale() {
        let store = store();
        save(&store, "mem-vector");
        let generation = store.generations().expect("generations").durable;
        store
            .replace_vectors(
                generation,
                "test-model",
                &[VectorEntry {
                    memory_id: "mem-vector",
                    vector: &[1.0, 0.0, 0.5],
                }],
            )
            .expect("replace");
        let vectors = store.vectors().expect("vectors");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].memory_id, "mem-vector");
        assert_eq!(vectors[0].vector, vec![1.0, 0.0, 0.5]);

        save(&store, "mem-newer");
        assert!(
            store.vectors().expect("vectors").is_empty(),
            "a durable write invalidates the vector generation"
        );
        let stale = generation;
        assert!(matches!(
            store.replace_vectors(stale, "test-model", &[]),
            Err(Error::StaleGeneration)
        ));
    }

    #[test]
    fn working_memory_count_eviction_is_session_scoped() {
        let store = store_with(2, 24);
        save(&store, "oldest");
        save(&store, "middle");
        save(&store, "newest");

        assert!(store.get("oldest").expect("lookup").is_none());
        assert!(store.get("middle").expect("lookup").is_some());
        assert!(store.get("newest").expect("lookup").is_some());
        assert_eq!(store.counts().expect("counts").working, 2);
    }

    #[test]
    fn atomic_batch_deduplicates_session_content_and_refreshes_rank_fields() {
        let store = store();
        let metadata = serde_json::json!({"context": "newer"});
        let inputs = [
            NewMemory {
                content: "User prefers Rust",
                embed_text: Some("User prefers Rust"),
                source: "retain",
                session_id: "session",
                importance: 0.4,
                veracity: "user",
                memory_type: "fact",
                metadata: &serde_json::Value::Null,
                stable_id: None,
            },
            NewMemory {
                content: "User prefers Rust",
                embed_text: Some("User prefers Rust"),
                source: "retain",
                session_id: "session",
                importance: 0.9,
                veracity: "user",
                memory_type: "fact",
                metadata: &metadata,
                stable_id: None,
            },
        ];
        let ids = store.save_batch(&inputs).expect("atomic save");
        assert_eq!(ids[0], ids[1]);
        assert_eq!(store.counts().expect("counts").working, 1);
        let record = store
            .get(&ids[0])
            .expect("lookup")
            .expect("deduplicated row");
        assert!((record.importance - 0.9).abs() < f64::EPSILON);
        assert!(record.timestamp.ends_with('Z'));
        assert_eq!(record.metadata["context"], "newer");
    }

    #[test]
    fn legacy_schema_migrates_authority_and_rebuilds_search_indexes() {
        let root: PathBuf = temp_root("legacy");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("mnemopi.db");
        let connection = Connection::open(&path).expect("legacy database");
        connection
            .execute_batch(
                "CREATE TABLE working_memory (
                    id TEXT PRIMARY KEY,
                    content TEXT NOT NULL,
                    source TEXT,
                    timestamp TEXT,
                    session_id TEXT,
                    importance REAL,
                    metadata_json TEXT
                );
                CREATE TABLE facts (
                    fact_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    predicate TEXT NOT NULL,
                    object TEXT NOT NULL,
                    timestamp TEXT,
                    source_msg_id TEXT,
                    confidence REAL
                );
                CREATE TABLE memory_embeddings (
                    memory_id TEXT PRIMARY KEY,
                    embedding_json TEXT NOT NULL,
                    model TEXT
                );
                CREATE TABLE triples (
                    id INTEGER PRIMARY KEY,
                    subject TEXT,
                    predicate TEXT,
                    object TEXT,
                    valid_from TEXT
                );
                INSERT INTO working_memory
                    (id, content, source, timestamp, session_id, importance, metadata_json)
                VALUES
                    ('legacy-memory', 'Rust is preferred', 'retain',
                     '2026-09-04T12:00:00Z', 'session', 0.8, '{}');
                INSERT INTO facts
                    (fact_id, session_id, subject, predicate, object, timestamp, source_msg_id,
                     confidence)
                VALUES
                    ('legacy-fact', 'session', 'User', 'prefers', 'Rust',
                     '2026-09-04T12:00:00Z', 'legacy-memory', 0.9);
                PRAGMA user_version = 0;",
            )
            .expect("legacy schema");
        drop(connection);

        let store = BankStore::open(&path, BankId::configured("default").expect("bank"), root)
            .expect("migration");
        assert_eq!(
            store
                .get("legacy-memory")
                .expect("lookup")
                .expect("memory")
                .content,
            "Rust is preferred"
        );
        let facts = store.search_facts("User Rust", 8).expect("fact search");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].record.id, "legacy-fact");
        let connection = store.connection().expect("connection");
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("version"),
            SCHEMA_VERSION
        );
        let source = connection
            .query_row(
                "SELECT source_memory_id FROM facts WHERE fact_id = 'legacy-fact'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("source migration");
        assert_eq!(source, "legacy-memory");
    }

    #[test]
    fn newer_schema_versions_are_rejected_instead_of_downgraded() {
        let root = temp_root("future");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("mnemopi.db");
        let connection = Connection::open(&path).expect("database");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("version");
        drop(connection);
        assert!(
            BankStore::open(&path, BankId::configured("test").expect("bank"), root).is_err(),
            "a future schema must never be opened by an older build"
        );
    }

    #[test]
    fn clear_empties_every_table_and_advances_generations() {
        let store = store();
        save(&store, "mem-clear");
        store.consolidate(None).expect("consolidate");
        store.clear().expect("clear");
        let counts = store.counts().expect("counts");
        assert_eq!(counts, StoreCounts::default());
        let generations = store.generations().expect("generations");
        assert_eq!(generations.vector, generations.durable);
        assert_eq!(generations.graph, generations.durable);
        assert_eq!(store.integrity().expect("integrity").integrity, "ok");
    }

    #[test]
    fn list_bounded_reports_truncation_at_the_byte_budget() {
        let store = store();
        save_content(&store, "mem-1", &"a".repeat(100));
        save_content(&store, "mem-2", &"b".repeat(100));
        let (records, truncated) = store.list_bounded(10, 150).expect("list");
        assert_eq!(records.len(), 1);
        assert!(truncated);
        let (all, not_truncated) = store.list_bounded(10, 4096).expect("list");
        assert_eq!(all.len(), 2);
        assert!(!not_truncated);
    }

    #[test]
    fn timestamps_render_as_sortable_rfc3339_utc() {
        let timestamp = utc_timestamp();
        assert_eq!(timestamp.len(), 24, "fixed-width millisecond RFC3339");
        assert!(timestamp.ends_with('Z'));
        assert!(timestamp.as_bytes()[4] == b'-');
        let connection = Connection::open_in_memory().expect("memory database");
        let parsed = connection
            .query_row("SELECT unixepoch(?1)", [timestamp.as_str()], |row| {
                row.get::<_, i64>(0)
            })
            .expect("sqlite parses our timestamps");
        assert!(
            parsed > 1_700_000_000,
            "timestamp must be a plausible epoch second"
        );
    }

    #[test]
    fn civil_date_conversion_matches_known_epochs() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
