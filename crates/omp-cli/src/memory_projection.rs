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
                append_pin_entry(persist, rec)
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
            Some(block) => {
                replace_memory_block(&current, block).unwrap_or_else(|_| current.clone())
            }
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
        let turn = match self.pins.advance(&bound.repo, session_id, branch, &|rec| {
            append_pin_entry(persist, rec)
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
                append_pin_entry(persist, rec)
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

/// Journal one pin record; `false` leaves it with the pin owner to write again.
fn append_pin_entry(persist: &Persistence, record: &ProjectionPinRecord) -> bool {
    let data = match serde_json::to_value(record) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("omt: serialize memory projection pin: {err}");
            return false;
        }
    };
    match persist.append_entry(
        "custom",
        json!({
            "customType": PROJECTION_PIN_ENTRY_TYPE,
            "data": data,
        }),
    ) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("omt: persist memory projection pin: {err:#}");
            false
        }
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    use omp_memory::pin::read_pin_record;
    use omp_memory::repo::MemoryRepo;
    use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};

    use super::*;

    /// `git` in the test memory repository with a fixed author.
    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn commit(repo: &Path, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, text).expect("write memory file");
        }
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "memory fixture"]);
        git(repo, &["rev-parse", "HEAD"])
    }

    fn memory_file(description: &str, body: &str) -> String {
        format!("---\ndescription: {description}\n---\n{body}\n")
    }

    /// A loose git object taken out of the store until `restore`: the read that needs it fails
    /// the way a transient git error does, and works again afterwards.
    struct HiddenObject {
        path: PathBuf,
        backup: PathBuf,
    }

    impl HiddenObject {
        fn new(repo: &Path, oid: &str) -> Self {
            let path = repo.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
            let backup = path.with_extension("hidden");
            std::fs::rename(&path, &backup).expect("a fresh commit's object is loose");
            Self { path, backup }
        }

        fn restore(self) {
            std::fs::rename(self.backup, self.path).expect("restore object");
        }
    }

    fn notice_text(message: AgentMessage) -> String {
        let AgentMessage::Custom(custom) = message else {
            panic!("the memory notice is a custom message");
        };
        custom.payload["content"][0]["text"]
            .as_str()
            .expect("notice text")
            .to_owned()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        agent: Agent,
        persist: Persistence,
        controller: MemoryProjectionController,
        clock: Arc<AtomicI64>,
    }

    impl Fixture {
        /// A memory repository whose first commit holds `files`, and a session bound to it.
        /// The pin clock starts at 1000 ms and only moves through [`Fixture::at`].
        async fn new(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path().join("memory");
            std::fs::create_dir_all(&repo).expect("memory dir");
            git(&repo, &["init", "-q"]);
            git(&repo, &["config", "core.hooksPath", ".git/hooks"]);
            commit(&repo, files);

            let model: pa_types::ai::Model = serde_json::from_value(json!({
                "id": "a", "name": "a", "api": "openai-completions", "provider": "p",
                "baseUrl": "http://127.0.0.1:9", "reasoning": false, "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 100_000, "maxTokens": 4096
            }))
            .expect("model json");
            let agent = Agent::new(AgentOptions {
                initial_state: AgentInitialState {
                    system_prompt: Some("base prompt".into()),
                    model: crate::adapter::json_round_trip(&model),
                    thinking_level: Some(pa_agent::types::ThinkingLevel::Off),
                    tools: Some(Vec::new()),
                    messages: None,
                },
                ..AgentOptions::default()
            });
            let persist = crate::persist::attach_in(
                &agent,
                dir.path(),
                dir.path(),
                None,
                crate::persist::uuid_v7(),
            )
            .await
            .expect("session");

            let clock = Arc::new(AtomicI64::new(1_000));
            let now = Arc::clone(&clock);
            let bound = BoundMemoryRepo {
                repo: MemoryRepo::open(&repo, "agent").expect("open memory repo"),
                root: repo.clone(),
                agent_id: "agent".to_owned(),
                from_env: false,
                used_legacy: false,
            };
            let controller = MemoryProjectionController {
                bound: Some(bound),
                pins: NativeProjectionPins::with_now(Box::new(move || now.load(Ordering::SeqCst))),
                state: Mutex::default(),
            };
            Self {
                _dir: dir,
                repo,
                agent,
                persist,
                controller,
                clock,
            }
        }

        fn at(&self, ms: i64) {
            self.clock.store(ms, Ordering::SeqCst);
        }

        fn journal(&self) -> Vec<Value> {
            std::fs::read_to_string(self.persist.path())
                .expect("session file")
                .lines()
                .map(|line| serde_json::from_str(line).expect("journal line"))
                .collect()
        }

        /// The newest pin record the journal holds: what a restart would restore.
        fn pin(&self) -> ProjectionPinRecord {
            read_pin_record(&self.journal(), &self.persist.session_id()).expect("pin entry")
        }

        async fn sync(&self) {
            self.controller
                .sync_session(
                    &self.agent,
                    &self.persist,
                    &self.persist.session_id(),
                    &self.journal(),
                )
                .await;
        }

        /// One turn start; the `<memory_notice>` text it produced, if any.
        async fn prepare(&self) -> Option<String> {
            self.controller
                .prepare_turn(
                    &self.agent,
                    &self.persist,
                    &self.persist.session_id(),
                    &self.journal(),
                )
                .await
                .map(notice_text)
        }

        async fn prompt(&self) -> String {
            self.agent.system_prompt().await
        }
    }

    #[tokio::test]
    async fn a_failed_repin_compile_is_retried_until_the_new_pin_is_installed() {
        let f = Fixture::new(&[("system/persona.md", &memory_file("Persona", "persona-v1"))]).await;
        f.sync().await;
        assert!(f.prompt().await.contains("persona-v1"));

        commit(
            &f.repo,
            &[("system/persona.md", &memory_file("Persona", "persona-v2"))],
        );
        f.controller.request_recompile();
        let unreadable = HiddenObject::new(&f.repo, &git(&f.repo, &["rev-parse", "HEAD^{tree}"]));
        f.at(2_000);
        f.prepare().await;
        unreadable.restore();
        f.at(3_000);
        f.prepare().await;

        let prompt = f.prompt().await;
        assert!(prompt.contains("persona-v2"), "{prompt}");
        assert!(!prompt.contains("persona-v1"), "{prompt}");
    }

    #[tokio::test]
    async fn a_pending_compile_does_not_borrow_the_previous_pins_expiries() {
        let gone = |expires: &str| format!("---\ndescription: Gone\nexpires: {expires}\n---\ngone-body\n");
        let f = Fixture::new(&[
            ("system/persona.md", &memory_file("Persona", "persona-v1")),
            ("system/gone.md", &gone("1970-01-01T00:00:05Z")),
        ])
        .await;
        f.sync().await;

        // The next pin keeps gone.md alive for a minute, but its tree cannot be read yet.
        commit(
            &f.repo,
            &[
                ("system/persona.md", &memory_file("Persona", "persona-v2")),
                ("system/gone.md", &gone("1970-01-01T00:01:00Z")),
            ],
        );
        f.controller.request_recompile();
        let unreadable = HiddenObject::new(&f.repo, &git(&f.repo, &["rev-parse", "HEAD^{tree}"]));
        f.at(2_000);
        assert_eq!(f.prepare().await, None);
        f.at(6_000);
        let pending = f.prepare().await;
        assert_eq!(pending, None, "the old pin's expiry list was used");
        assert_eq!(
            f.pin().expiry_noticed_through_ms,
            None,
            "the expiry watermark advanced without a matching projection"
        );

        unreadable.restore();
        f.at(7_000);
        assert_eq!(f.prepare().await, None);
        assert!(f.prompt().await.contains("persona-v2"));
    }

    #[tokio::test]
    async fn a_pending_compile_does_not_reinstall_the_previous_projection_on_a_rebuilt_prompt() {
        let f = Fixture::new(&[("system/persona.md", &memory_file("Persona", "persona-v1"))]).await;
        f.sync().await;
        commit(
            &f.repo,
            &[("system/persona.md", &memory_file("Persona", "persona-v2"))],
        );
        f.controller.request_recompile();
        let unreadable = HiddenObject::new(&f.repo, &git(&f.repo, &["rev-parse", "HEAD^{tree}"]));
        f.at(2_000);
        f.prepare().await;

        f.agent.set_system_prompt("rebuilt base").await;
        f.controller.apply_cached(&f.agent).await;
        assert_eq!(f.prompt().await, "rebuilt base");
        unreadable.restore();
    }

    #[tokio::test]
    async fn a_pin_record_that_failed_to_persist_is_written_on_a_later_turn() {
        let f = Fixture::new(&[("system/persona.md", &memory_file("Persona", "persona-v1"))]).await;
        f.sync().await;
        let first = f.pin();

        let moved = commit(
            &f.repo,
            &[("system/persona.md", &memory_file("Persona", "persona-v2"))],
        );
        f.controller.request_recompile();
        f.persist.set_fail_append(true);
        f.at(2_000);
        f.prepare().await;
        f.persist.set_fail_append(false);
        assert_eq!(f.pin(), first, "the failed append reached the journal");

        f.at(3_000);
        f.prepare().await;
        let landed = f.pin();
        assert_eq!(landed.revision.as_deref(), Some(moved.as_str()));
        assert_eq!(landed.pinned_at_ms, 2_000, "the retry re-pinned");
    }

    #[tokio::test]
    async fn a_partial_git_read_is_not_installed_as_the_pinned_projection() {
        let f = Fixture::new(&[
            ("system/persona.md", &memory_file("Persona", "persona-body")),
            ("system/extra.md", &memory_file("Extra", "extra-body")),
        ])
        .await;
        let unreadable = HiddenObject::new(
            &f.repo,
            &git(&f.repo, &["rev-parse", "HEAD:system/extra.md"]),
        );
        f.sync().await;
        let partial = f.prompt().await;
        assert!(
            !partial.contains("persona-body"),
            "a partial projection was installed: {partial}"
        );

        unreadable.restore();
        f.at(2_000);
        f.prepare().await;
        let prompt = f.prompt().await;
        assert!(prompt.contains("persona-body"), "{prompt}");
        assert!(prompt.contains("extra-body"), "{prompt}");
        assert_eq!(f.pin().pinned_at_ms, 1_000, "the retry kept the pin time");
    }
}
