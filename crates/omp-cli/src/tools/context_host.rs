//! Late-bound [`ContextNotesHost`] for the `context_notes` / `new_context` tools.
//!
//! The agent's tool list is built before the session `Runtime` exists, but the
//! runtime is what implements [`ContextNotesHost`]. [`session_tools`] therefore
//! hands the tools a [`LateHost`] proxy that resolves a process-global weak
//! reference on every call; the runtime publishes itself with [`bind`] once it
//! is constructed. The reference is weak, so the global never keeps a disposed
//! session alive: once the runtime drops, the host reads as
//! [`ContextState::NotOwned`], the same "no journal for this session" answer the
//! tools already give.

use std::sync::{Arc, PoisonError, RwLock, Weak};

use anyhow::{anyhow, Result};
use pa_agent::types::AgentTool;
use serde_json::Value;

use super::context_notes::{context_notes_tool, new_context_tool, ContextNotesHost, ContextState};

const NOT_BOUND: &str = "context host not bound";

static HOST: RwLock<Option<Weak<dyn ContextNotesHost>>> = RwLock::new(None);

/// Publish `host` as the session's context host (weakly; the caller keeps the
/// owning `Arc`). Replaces any previously bound host.
pub fn bind(host: &Arc<dyn ContextNotesHost>) {
    if let Some(scope) = crate::scope::current() {
        *crate::scope::write(&scope.context_host) = Some(Arc::downgrade(host));
        return;
    }
    *HOST.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(host));
}

/// Withdraw the bound host; the tools report [`ContextState::NotOwned`] again.
#[cfg(test)]
pub fn unbind() {
    *HOST.write().unwrap_or_else(PoisonError::into_inner) = None;
}

fn bound() -> Option<Arc<dyn ContextNotesHost>> {
    if let Some(scope) = crate::scope::current() {
        return crate::scope::read(&scope.context_host)
            .as_ref()
            .and_then(Weak::upgrade);
    }
    HOST.read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade)
}

/// Proxy that forwards to whichever host is bound at call time.
struct LateHost;

impl ContextNotesHost for LateHost {
    fn context_state(&self) -> ContextState {
        bound().map_or(ContextState::NotOwned, |host| host.context_state())
    }

    fn session_id(&self) -> String {
        bound().map(|host| host.session_id()).unwrap_or_default()
    }

    fn branch(&self) -> Vec<Value> {
        bound().map(|host| host.branch()).unwrap_or_default()
    }

    fn ensure_on_disk(&self) -> Result<()> {
        bound().ok_or_else(|| anyhow!(NOT_BOUND))?.ensure_on_disk()
    }

    fn append_custom_entry(&self, custom_type: &str, data: Value) -> Result<String> {
        bound()
            .ok_or_else(|| anyhow!(NOT_BOUND))?
            .append_custom_entry(custom_type, data)
    }
}

/// The `context_notes` + `new_context` tools over the late-bound host, or none
/// when `compaction.experimentalContextManagement` leaves them disabled.
#[must_use]
pub fn session_tools(settings: &omp_settings::Settings) -> Vec<Arc<dyn AgentTool>> {
    if !super::enabled_by_settings(settings, "context_notes") {
        return Vec::new();
    }
    let late: Arc<dyn ContextNotesHost> = Arc::new(LateHost);
    vec![context_notes_tool(late.clone()), new_context_tool(late)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// `HOST` is process-global, so tests that touch it run one at a time and
    /// start from an unbound state.
    static GUARD: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        let guard = GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        unbind();
        guard
    }

    #[derive(Default)]
    struct FakeHost {
        appends: Mutex<Vec<(String, Value)>>,
        disk_calls: Mutex<usize>,
    }

    impl ContextNotesHost for FakeHost {
        fn context_state(&self) -> ContextState {
            ContextState::Ready
        }
        fn session_id(&self) -> String {
            "fake-session".into()
        }
        fn branch(&self) -> Vec<Value> {
            vec![serde_json::json!({"id": "leaf-1"})]
        }
        fn ensure_on_disk(&self) -> Result<()> {
            *self.disk_calls.lock().unwrap() += 1;
            Ok(())
        }
        fn append_custom_entry(&self, custom_type: &str, data: Value) -> Result<String> {
            self.appends
                .lock()
                .unwrap()
                .push((custom_type.to_string(), data));
            Ok("entry-1".into())
        }
    }

    fn settings_with(yaml: &str) -> (tempfile::TempDir, omp_settings::Settings) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yml");
        std::fs::write(&cfg, yaml).unwrap();
        let settings = omp_settings::Settings::load_from(Some(&cfg), dir.path());
        (dir, settings)
    }

    const ENABLED: &str = "compaction:\n  experimentalContextManagement: true\n";

    #[test]
    fn unbound_proxy_reports_not_owned_and_empty() {
        let _guard = exclusive();
        let late = LateHost;
        assert_eq!(late.context_state(), ContextState::NotOwned);
        assert_eq!(late.session_id(), "");
        assert!(late.branch().is_empty());
        assert_eq!(
            late.ensure_on_disk().unwrap_err().to_string(),
            "context host not bound"
        );
        assert_eq!(
            late.append_custom_entry("t", Value::Null)
                .unwrap_err()
                .to_string(),
            "context host not bound"
        );
    }

    #[test]
    fn bound_host_receives_delegated_calls() {
        let _guard = exclusive();
        let fake = Arc::new(FakeHost::default());
        let host: Arc<dyn ContextNotesHost> = fake.clone();
        bind(&host);

        let late = LateHost;
        assert_eq!(late.context_state(), ContextState::Ready);
        assert_eq!(late.session_id(), "fake-session");
        assert_eq!(late.branch(), vec![serde_json::json!({"id": "leaf-1"})]);
        late.ensure_on_disk().unwrap();
        assert_eq!(*fake.disk_calls.lock().unwrap(), 1);
        let id = late
            .append_custom_entry("custom", serde_json::json!({"k": 1}))
            .unwrap();
        assert_eq!(id, "entry-1");
        assert_eq!(
            *fake.appends.lock().unwrap(),
            vec![("custom".to_string(), serde_json::json!({"k": 1}))]
        );

        unbind();
        assert_eq!(late.context_state(), ContextState::NotOwned);
    }

    #[test]
    fn dropped_host_reads_as_not_owned() {
        let _guard = exclusive();
        let host: Arc<dyn ContextNotesHost> = Arc::new(FakeHost::default());
        bind(&host);
        assert_eq!(LateHost.context_state(), ContextState::Ready);
        drop(host);
        assert_eq!(LateHost.context_state(), ContextState::NotOwned);
        assert!(LateHost.append_custom_entry("t", Value::Null).is_err());
    }

    #[test]
    fn session_tools_are_named_and_gated_by_setting() {
        let _guard = exclusive();
        let (_dir, on) = settings_with(ENABLED);
        let tools = session_tools(&on);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["context_notes", "new_context"]);

        let (_dir, off) = settings_with("compaction:\n  experimentalContextManagement: false\n");
        assert!(session_tools(&off).is_empty());
        let (_dir, unset) = settings_with("find:\n  enabled: \"on\"\n");
        assert!(session_tools(&unset).is_empty());
    }

    #[test]
    fn tools_built_before_bind_use_the_host_bound_later() {
        let _guard = exclusive();
        let (_dir, settings) = settings_with(ENABLED);
        let tools = session_tools(&settings);
        let new_context = tools
            .iter()
            .find(|t| t.name() == "new_context")
            .cloned()
            .unwrap();

        let run = |tool: Arc<dyn AgentTool>| {
            futures::executor::block_on(tool.execute(
                "call".into(),
                serde_json::json!({}),
                pa_agent::abort::AbortSignal::default(),
                Arc::new(|_| {}),
            ))
        };
        let unbound = run(new_context.clone()).unwrap_err().to_string();
        assert_eq!(
            unbound,
            "Experimental context management is unavailable for this session."
        );

        let host: Arc<dyn ContextNotesHost> = Arc::new(FakeHost::default());
        bind(&host);
        assert!(run(new_context).is_ok());
    }
}
