//! Durable, default-off Mnemopi memory runtime.
//!
//! The crate never discovers Git roots or reads application settings directly.
//! The caller supplies canonical repository identity and composes the selected
//! backend. Durable SQLite rows are authoritative; every recall index
//! (FTS, vectors, graph) is derived and rebuildable.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::struct_field_names,
    clippy::struct_excessive_bools
)]

pub mod bank;
pub mod cache;
pub mod config;
pub mod diagnose;
pub mod extract;
pub mod link;
pub mod recall;
pub mod retain;
pub mod runtime;
pub mod session;
pub mod store;

use std::{io, result};

pub use bank::{BankId, BankScope, BankScopeInput};
pub use config::{AutolearnSettings, BankScoping, MemoryBackend, MemorySettings, MnemopiSettings};
pub use runtime::{Capabilities, MemoryRuntime, RuntimeRegistry};

/// Mnemopi unavailable status used consistently by every surface.
pub const INACTIVE_MESSAGE: &str = "Memory is off. Set memory.backend = \"mnemopi\" to enable it.";

/// Typed memory failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// SQLite operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// JSON framing or metadata decoding failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The operation requires a live Mnemopi backend.
    #[error("memory backend is inactive")]
    Inactive,
    /// A requested bank name, memory id, or URL resource was invalid.
    #[error("memory identifier is invalid")]
    InvalidIdentifier,
    /// Input exceeded a documented protocol or projection bound.
    #[error("memory input exceeds its bounded limit")]
    InputTooLarge,
    /// A generation-fenced write raced a newer durable generation.
    #[error("memory index generation changed during rebuild")]
    StaleGeneration,
    /// Recall projection exceeded its explicit byte or token bound.
    #[error("memory projection exceeds its bounded output limit")]
    ProjectionTooLarge,
    /// A bounded background write did not settle inside its budget.
    #[error("memory background write did not settle")]
    ShutdownTimeout,
}

/// Crate-local result alias.
pub type Result<T, E = Error> = result::Result<T, E>;

/// Unique scratch directory for one unit test.
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static SERIAL: AtomicU64 = AtomicU64::new(0);

    /// Returns a fresh, process-unique scratch directory path.
    pub fn temp_root(tag: &str) -> PathBuf {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "omp-memory-{tag}-{}-{nanos}-{serial}",
            std::process::id()
        ))
    }
}
