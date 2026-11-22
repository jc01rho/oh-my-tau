//! Bank diagnostics and retryable SQLite cleanup.

use std::{
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    store::{BankStore, IndexGeneration, IntegrityReport, StoreCounts},
    Result,
};

const REMOVE_RETRIES: usize = 40;
const REMOVE_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Diagnostics for one scoped bank.
///
/// Deliberately PII-safe: it reports counts, sizes, and index health only. Row
/// content, session ids, and metadata never leave the store through this type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BankDiagnostic {
    /// Bank identifier.
    pub bank: String,
    /// Non-secret database target.
    pub database: PathBuf,
    /// Database byte size.
    pub database_bytes: u64,
    /// Durable row counts.
    pub counts: StoreCounts,
    /// SQLite and derived-index health.
    pub integrity: IntegrityReport,
}

impl BankDiagnostic {
    /// Whether every authoritative and derived index check is healthy.
    pub fn healthy(&self) -> bool {
        healthy(self)
    }
}

/// Collects complete diagnostics for one bank.
pub fn inspect(store: &BankStore) -> Result<BankDiagnostic> {
    let database_bytes = fs::metadata(store.path()).map_or(0, |value| value.len());
    Ok(BankDiagnostic {
        bank: store.bank().as_str().to_owned(),
        database: store.path().to_path_buf(),
        database_bytes,
        counts: store.counts()?,
        integrity: store.integrity()?,
    })
}

/// Whether every authoritative and derived index check is healthy.
pub fn healthy(report: &BankDiagnostic) -> bool {
    report.integrity.integrity == "ok"
        && report.integrity.vector_current
        && report.integrity.graph_current
}

/// Returns generation health without opening raw database paths to callers.
pub const fn generations(report: &BankDiagnostic) -> IndexGeneration {
    report.integrity.generations
}

/// Renders a stable, one-line summary suitable for `/memory diagnose`.
pub fn summary(report: &BankDiagnostic) -> String {
    format!(
        "{}: {} working, {} episodic, {} facts, {} triples, {} bytes, {}",
        report.bank,
        report.counts.working,
        report.counts.episodic,
        report.counts.facts,
        report.counts.triples,
        report.database_bytes,
        if healthy(report) {
            "healthy"
        } else {
            "needs attention"
        }
    )
}

/// Removes a SQLite database together with WAL and shared-memory sidecars.
///
/// Some platforms retain transient locks after the last handle closes.
/// Permission, would-block, and directory-not-empty failures are retried for the
/// one-second window.
pub fn remove_database_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let target = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("{}{suffix}", path.display()))
        };
        remove_with_retry(&target)?;
    }
    Ok(())
}

fn remove_with_retry(path: &Path) -> Result<()> {
    for attempt in 0..=REMOVE_RETRIES {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if attempt < REMOVE_RETRIES
                    && matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::DirectoryNotEmpty
                    ) =>
            {
                thread::sleep(REMOVE_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(crate::Error::Io(io::Error::other(
        "database removal exhausted its retry window",
    )))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{bank::BankId, store::NewMemory, test_support::temp_root};

    fn store() -> BankStore {
        let root = temp_root("diagnose");
        BankStore::open(
            root.join("mnemopi.db"),
            BankId::configured("test").expect("bank"),
            root,
        )
        .expect("store")
    }

    #[test]
    fn inspect_reports_counts_size_and_health_without_content() {
        let store = store();
        store
            .save(NewMemory {
                content: "secret content that must not leak",
                embed_text: None,
                source: "test",
                session_id: "session",
                importance: 0.5,
                veracity: "user",
                memory_type: "fact",
                metadata: &serde_json::json!({"context": "private"}),
                stable_id: Some("mem-1"),
            })
            .expect("save");
        let generation = store.generations().expect("generations").durable;
        store
            .replace_vectors(generation, "test-model", &[])
            .expect("vectors");
        crate::link::reconcile(&store).expect("graph");

        let report = inspect(&store).expect("inspect");
        assert_eq!(report.bank, "test");
        assert_eq!(report.counts.working, 1);
        assert!(report.database_bytes > 0);
        assert_eq!(report.integrity.integrity, "ok");
        assert!(report.integrity.vector_current);
        assert!(report.integrity.graph_current);
        assert!(healthy(&report));

        let rendered = serde_json::to_string(&report).expect("serialize");
        assert!(!rendered.contains("secret content"));
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("session"));
        assert!(summary(&report).contains("healthy"));
        assert_eq!(generations(&report), report.integrity.generations);
    }

    #[test]
    fn a_stale_derived_index_reports_unhealthy() {
        let store = store();
        store
            .save(NewMemory {
                content: "one row",
                embed_text: None,
                source: "test",
                session_id: "session",
                importance: 0.5,
                veracity: "user",
                memory_type: "fact",
                metadata: &serde_json::Value::Null,
                stable_id: Some("mem-1"),
            })
            .expect("save");
        let report = inspect(&store).expect("inspect");
        assert!(
            !report.integrity.vector_current,
            "no vector rebuild has run yet"
        );
        assert!(!healthy(&report));
        assert!(summary(&report).contains("needs attention"));
    }

    #[test]
    fn remove_database_files_deletes_the_database_and_sidecars() {
        let root = temp_root("remove");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("mnemopi.db");
        let store = BankStore::open(
            &path,
            BankId::configured("test").expect("bank"),
            root.clone(),
        )
        .expect("store");
        store
            .save(NewMemory {
                content: "row",
                embed_text: None,
                source: "test",
                session_id: "session",
                importance: 0.5,
                veracity: "user",
                memory_type: "fact",
                metadata: &serde_json::Value::Null,
                stable_id: Some("mem-1"),
            })
            .expect("save");
        assert!(path.exists());
        remove_database_files(&path).expect("remove");
        assert!(!path.exists());
        remove_database_files(&path).expect("removing a missing database is a no-op");
    }
}
