//! Per-session slots of a multi-session host (`--multi-session --session-runtime in-process`).
//!
//! A single-session engine keeps its live session's tool env, job registry, eval bridge, session
//! file, extension host, context strategy, sub-call sink and approval binding in process-wide
//! slots. The multi-session host runs every hosted session on its own thread with its own
//! current-thread tokio runtime and installs that session's [`SessionScope`] on the thread first:
//! every task the session spawns (the agent loop, its tool calls) runs on that thread, so each
//! slot's accessor reads the caller's own session. Outside a host no scope is installed and the
//! process-wide slots apply unchanged.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use pa_agent::types::AgentTool;

use crate::tools::JobRegistry;

type Hooks = (
    Option<pa_agent::agent_loop::BeforeToolCallFn>,
    Option<pa_agent::agent_loop::AfterToolCallFn>,
);

#[derive(Default)]
pub struct SessionScope {
    /// The routing id the host names this session by (`rpc-N`).
    pub(crate) routing_id: RwLock<Option<String>>,
    pub(crate) host_tools: RwLock<Vec<String>>,
    pub(crate) session_file: RwLock<Option<PathBuf>>,
    pub(crate) env: Mutex<Option<Weak<crate::tools::Env>>>,
    pub(crate) jobs: Mutex<Option<JobRegistry>>,
    pub(crate) last_jobs: Mutex<Option<JobRegistry>>,
    pub(crate) eval_tools: RwLock<Vec<Arc<dyn AgentTool>>>,
    pub(crate) eval_hooks: RwLock<Hooks>,
    pub(crate) eval_target: RwLock<Option<crate::agent::Target>>,
    pub(crate) extensions: RwLock<Option<Arc<crate::ext::Extensions>>>,
    pub(crate) context_host:
        RwLock<Option<Weak<dyn crate::tools::context_notes::ContextNotesHost>>>,
    pub(crate) context_live: RwLock<Option<(Arc<crate::context::Shared>, bool)>>,
    pub(crate) entry_sink: RwLock<Option<crate::persist::EntrySink>>,
    pub(crate) approval: Mutex<(Option<Weak<crate::runtime::Runtime>>, bool)>,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<SessionScope>>> = const { RefCell::new(None) };
}

/// Make `scope` the session of the calling thread (a hosted session's own thread, before its
/// runtime starts).
pub fn install(scope: Arc<SessionScope>) {
    CURRENT.with(|current| *current.borrow_mut() = Some(scope));
}

/// Put the calling thread back to the session it had before [`install`] (a pooled thread reused
/// after a scoped call).
pub fn restore(previous: Option<Arc<SessionScope>>) {
    CURRENT.with(|current| *current.borrow_mut() = previous);
}

/// [`tokio::task::spawn_blocking`] for a body that reads session slots: the blocking pool's
/// threads carry no scope, so the caller's session is installed for the call and dropped after.
pub async fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let scope = current();
    tokio::task::spawn_blocking(move || {
        let previous = CURRENT.with(|current| current.borrow().clone());
        if let Some(scope) = scope {
            install(scope);
        }
        let out = work();
        restore(previous);
        out
    })
    .await
}

/// The calling thread's hosted session, `None` outside a multi-session host.
#[must_use]
pub fn current() -> Option<Arc<SessionScope>> {
    CURRENT.with(|current| current.borrow().clone())
}

pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `omp-urls` reads the session file through this: the scoped session's when one is installed.
struct UrlScope;

impl omp_urls::SessionFileScope for UrlScope {
    fn get(&self) -> Option<Option<PathBuf>> {
        current().map(|scope| read(&scope.session_file).clone())
    }

    fn set(&self, path: Option<PathBuf>) -> bool {
        current().is_some_and(|scope| {
            *write(&scope.session_file) = path;
            true
        })
    }
}

/// Route `omp-urls`' session file through the calling thread's scope (the host does this once).
pub fn install_url_scope() {
    omp_urls::install_session_file_scope(Box::new(UrlScope));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_thread_sees_its_own_session_and_unscoped_threads_none() {
        install_url_scope();
        let run = |name: &'static str| {
            std::thread::spawn(move || {
                install(Arc::new(SessionScope::default()));
                omp_urls::set_global_session_file(Some(PathBuf::from(name)));
                std::thread::yield_now();
                omp_urls::global_session_file()
            })
        };
        let (a, b) = (run("/a.jsonl"), run("/b.jsonl"));
        assert_eq!(a.join().unwrap(), Some(PathBuf::from("/a.jsonl")));
        assert_eq!(b.join().unwrap(), Some(PathBuf::from("/b.jsonl")));
        assert!(current().is_none());
    }
}
