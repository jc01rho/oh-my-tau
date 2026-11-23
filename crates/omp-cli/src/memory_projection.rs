//! Session-pinned OmOMeow memory projection controller for `Runtime`.

use std::path::Path;
use std::sync::Mutex;

use omp_memory::expiry::{newly_expired, ExpiryWindow, MemoryExpiry};
use omp_memory::identity::{discover_memory_repo, BoundMemoryRepo, MemoryRepoLookup};
use omp_memory::pin::{
    render_memory_notice, NativeProjectionPins, ProjectionPinRecord, MEMORY_NOTICE_CUSTOM_TYPE,
    PROJECTION_PIN_ENTRY_TYPE,
};
use omp_memory::projection::{
    compile_memory_block_at_revision, mark_memory_block, replace_memory_block, strip_memory_block,
    CompileMemoryBlockOptions,
};
use pa_agent::agent::Agent;
use pa_agent::types::AgentMessage;
use serde_json::{json, Value};

use crate::persist::Persistence;

#[derive(Default)]
struct CachedProjection {
    sentinel_block: Option<String>,
    expiries: Vec<MemoryExpiry>,
}

pub struct MemoryProjectionController {
    bound: Option<BoundMemoryRepo>,
    pins: NativeProjectionPins,
    state: Mutex<CachedProjection>,
}

impl MemoryProjectionController {
    #[must_use]
    pub fn discover(cwd: &Path) -> Self {
        let bound = match discover_memory_repo(cwd) {
            MemoryRepoLookup::Found(repo) => Some(repo),
            MemoryRepoLookup::NotFound { .. } => None,
        };
        Self {
            bound,
            pins: NativeProjectionPins::new(),
            state: Mutex::default(),
        }
    }

    #[must_use]
    pub fn request_recompile(&self) -> Option<String> {
        let bound = self.bound.as_ref()?;
        self.pins.request_refresh();
        Some(bound.agent_id.clone())
    }

    pub async fn sync_session(
        &self,
        agent: &Agent,
        persist: &Persistence,
        session_id: &str,
        branch: &[Value],
    ) {
        let Some(bound) = self.bound.as_ref() else {
            {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.sentinel_block = None;
                guard.expiries.clear();
            }
            let current = agent.system_prompt().await;
            let stripped = strip_memory_block(&current);
            if stripped != current {
                agent.set_system_prompt(stripped).await;
            }
            return;
        };

        let turn = match self
            .pins
            .ensure_pinned(&bound.repo, session_id, branch, &|rec| {
                append_pin_entry(persist, rec);
            }) {
            Ok(turn) => turn,
            Err(err) => {
                eprintln!("omt: memory projection pin failed: {err}");
                return;
            }
        };

        self.compile_and_apply(agent, bound, turn.revision.as_deref(), turn.as_of_ms)
            .await;
    }

    pub async fn apply_cached(&self, agent: &Agent) {
        let sentinel = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sentinel_block
            .clone();
        let current = agent.system_prompt().await;
        let updated = match sentinel.as_deref() {
            Some(block) => replace_memory_block(&current, block).unwrap_or_else(|_| current.clone()),
            None => strip_memory_block(&current),
        };
        if updated != current {
            agent.set_system_prompt(updated).await;
        }
    }

    pub async fn prepare_turn(
        &self,
        agent: &Agent,
        persist: &Persistence,
        session_id: &str,
        branch: &[Value],
    ) -> Option<AgentMessage> {
        let bound = self.bound.as_ref()?;
        let turn = match self
            .pins
            .advance(&bound.repo, session_id, branch, &|rec| {
                append_pin_entry(persist, rec);
            }) {
            Ok(turn) => turn,
            Err(err) => {
                eprintln!("omt: memory turn check failed: {err}");
                return None;
            }
        };

        let needs_compile = turn.repinned.is_some()
            || self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sentinel_block
                .is_none();
        if needs_compile {
            self.compile_and_apply(agent, bound, turn.revision.as_deref(), turn.as_of_ms)
                .await;
        }

        let expiries = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expiries
            .clone();
        let expired = newly_expired(
            &expiries,
            ExpiryWindow {
                pinned_at_ms: turn.as_of_ms,
                noticed_through_ms: turn.expiry_noticed_through_ms,
                now_ms: turn.now_ms,
            },
        );
        if !expired.is_empty() {
            self.pins.notice_expiries(session_id, turn.now_ms, &|rec| {
                append_pin_entry(persist, rec);
            });
        }

        let text = render_memory_notice(&turn, &expired)?;
        Some(crate::notices::message(
            MEMORY_NOTICE_CUSTOM_TYPE,
            &text,
            pa_agent::now_ms(),
        ))
    }

    async fn compile_and_apply(
        &self,
        agent: &Agent,
        bound: &BoundMemoryRepo,
        revision: Option<&str>,
        as_of_ms: i64,
    ) {
        let compiled = match compile_memory_block_at_revision(
            &bound.repo,
            revision,
            &CompileMemoryBlockOptions {
                agent_id: bound.agent_id.clone(),
                projection: None,
            },
            as_of_ms,
        ) {
            Ok(compiled) => compiled,
            Err(err) => {
                eprintln!("omt: memory projection compile failed: {err}");
                return;
            }
        };
        for warning in &compiled.degraded {
            eprintln!("omt: memory projection degraded: {warning}");
        }
        let sentinel = mark_memory_block(&bound.agent_id, &compiled.block);
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.sentinel_block = Some(sentinel.clone());
            guard.expiries = compiled.expiries;
        }
        let current = agent.system_prompt().await;
        if let Ok(updated) = replace_memory_block(&current, &sentinel) {
            if updated != current {
                agent.set_system_prompt(updated).await;
            }
        }
    }
}

fn append_pin_entry(persist: &Persistence, record: &ProjectionPinRecord) {
    let Ok(data) = serde_json::to_value(record) else {
        return;
    };
    if let Err(err) = persist.append_entry(
        "custom",
        json!({
            "customType": PROJECTION_PIN_ENTRY_TYPE,
            "data": data,
        }),
    ) {
        eprintln!("omt: persist memory projection pin: {err:#}");
    }
}

#[must_use]
pub fn dump_system_prompt_block(cwd: &Path) -> Option<String> {
    let MemoryRepoLookup::Found(bound) = discover_memory_repo(cwd) else {
        return None;
    };
    let head = bound.repo.head().ok()?;
    let compiled = compile_memory_block_at_revision(
        &bound.repo,
        head.as_deref(),
        &CompileMemoryBlockOptions {
            agent_id: bound.agent_id.clone(),
            projection: None,
        },
        omp_memory::projection::now_ms(),
    )
    .ok()?;
    Some(mark_memory_block(&bound.agent_id, &compiled.block))
}
