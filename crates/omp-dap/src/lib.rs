//! DAP (Debug Adapter Protocol) support for omp-rs: a port of omp's `src/dap`.
//!
//! - [`config`]: adapter discovery/merging from `defaults.json` + user/project
//!   config files (`dap.json`/`dap.yaml` + hidden variants), root-marker gating,
//!   binary resolution, and launch/attach adapter selection.
//! - [`client`]: DAP transport (stdio pipes, unix socket, TCP) with
//!   `Content-Length` framing, pending-request routing, event dispatch and
//!   reverse requests (`runInTerminal`, `startDebugging`).
//! - [`session`]: the session manager — session tree, breakpoint mutation
//!   queue, stop-outcome waiters, bounded output ring, idle reaping.
//! - [`format`]: the model-facing text renderers for every action result.
//! - [`types`]: the DAP wire model and session records.
//!
//! Cancellation is injected through [`Cancel`] so the crate stays independent of
//! the agent runtime; `omp-cli` implements it for `pa_agent`'s `AbortSignal`.

use std::future::Future;
use std::pin::Pin;

pub mod client;
pub mod config;
pub mod format;
pub mod session;
pub mod types;

/// Cancellation handle threaded through every potentially blocking DAP call.
///
/// Mirrors the TS `AbortSignal` usage in `src/dap`: an aborted signal rejects the
/// in-flight request instead of leaving a pending entry behind.
pub trait Cancel: Send + Sync {
    fn is_cancelled(&self) -> bool;
    /// Resolves when the handle is cancelled.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Never-cancelled handle (TS `undefined` signal).
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverCancel;

impl Cancel for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

/// Wall-clock deadline plus optional caller cancellation.
#[derive(Clone, Copy, Default)]
pub struct Deadline<'a> {
    /// Caller cancellation, when the runtime provides one.
    pub cancel: Option<&'a dyn Cancel>,
    /// Wall-clock budget for the whole call.
    pub timeout: Option<std::time::Duration>,
}

impl std::fmt::Debug for Deadline<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deadline")
            .field("cancel", &self.cancel.map(|_| ".."))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl<'a> Deadline<'a> {
    /// Build a deadline from a cancel handle and a budget.
    #[must_use]
    pub fn new(cancel: Option<&'a dyn Cancel>, timeout: Option<std::time::Duration>) -> Self {
        Self { cancel, timeout }
    }

    /// Milliseconds for the budget, as the DAP client expects.
    #[must_use]
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout
            .map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX))
    }
}

/// Failure modes of the DAP machinery.
///
/// Every variant carries the message the tool surfaces verbatim: the TS client
/// throws plain `Error`s with these exact shapes, and the model reads them.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DapError {
    /// The adapter binary could not be spawned.
    #[error("{0}")]
    Spawn(String),
    /// A DAP request failed (adapter answered `success: false`).
    #[error("{0}")]
    Request(String),
    /// A request or event wait exceeded its budget.
    #[error("{0}")]
    Timeout(String),
    /// The caller aborted the operation.
    #[error("Debug operation aborted")]
    Aborted,
    /// The transport closed (adapter exited, socket dropped).
    #[error("{0}")]
    Transport(String),
    /// The active session does not support the requested capability.
    #[error("{0}")]
    Unsupported(String),
    /// No active debug session.
    #[error("No active debug session. Launch or attach first.")]
    NoSession,
    /// The session state forbids the operation.
    #[error("{0}")]
    State(String),
}

impl DapError {
    /// Whether the failure came from caller cancellation.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted)
    }
}

/// `Result` alias for this crate.
pub type DapResult<T> = Result<T, DapError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn never_cancel_never_cancels() {
        let cancel = NeverCancel;
        assert!(!cancel.is_cancelled());
        drop(cancel.cancelled());
    }

    #[test]
    fn deadline_reports_millis() {
        let deadline = Deadline::new(None, Some(Duration::from_secs(30)));
        assert_eq!(deadline.timeout_ms(), Some(30_000));
        assert_eq!(Deadline::default().timeout_ms(), None);
        assert!(format!("{deadline:?}").contains("30s"));
    }

    #[test]
    fn aborted_error_is_flagged() {
        assert!(DapError::Aborted.is_aborted());
        assert!(!DapError::NoSession.is_aborted());
        assert_eq!(
            DapError::NoSession.to_string(),
            "No active debug session. Launch or attach first."
        );
    }
}
