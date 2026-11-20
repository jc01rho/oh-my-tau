<analysis>
**Literal Request**: Read four architecture docs in full; report crate graph, process topology, agent loop shape, extension model, ≤70 lines.
**Actual Need**: Dense, accurate architectural map grounded in the docs plus verified crate facts.
**Success Looks Like**: Caller can proceed to design/debug without re-reading the docs.
</analysis>

Read all four docs plus verified the real crate list and inter-crate deps from every `crates/*/Cargo.toml`. Report:

**1. Crate dependency graph (verified, real names)**
- Foundation: `omp-core` (everything), `omp-proto` (wire), `omp-vocab`, `omp-macros`, `omp-ar`, `omp-journal`, `omp-dom` (core+journal+vocab), `omp-scribe` (core+dom), `omp-snapcompact`, `omp-walker`, `omp-ast` (walker), `omp-audio`, `omp-memory`, `omp-oauth`, `omp-sandbox`, `omp-cache`, `omp-secrets` (cache), `omp-webview`, `omp-ext` (core only).
- Inference stack: `omp-catalog` (con,core) -> `omp-ai` (audio,catalog,con,oauth,proto,secrets) -> `omp-tool` (catalog,ai,proto) -> `omp-tools` (ast,ar,con,edit,memory,py,shell,shell-builtins,tool,macros,walker,ai).
- Session spine: `omp-session` (core,dom,journal,observability,proto,tool) -> `omp-agent` (catalog,journal,con,dom,vocab,session,ai,tool,proto,scribe) -> `omp-driver` -> `omp-app`. `omp-con` depends on `omp-session` (config folds session state).
- Presentation: `omp-tui` (core,macros,snapcompact,vocab) -> `omp-chat` (agent,tui,tools,session,dom,journal,ai,tool,audio,vcs,walker); `omp-gui` (tui,webview); `omp-sdk` (agent,dom,session,catalog,ai,journal,tool).
- Environment: `omp-env` (ar,core,proto) is the thin client; `omp-envd` is the fattest crate (env,py,ext,tools,webview,desktop,session,dom,ai,tool,shell*,sandbox,vcs,docserver). `omp-py` (core,env,tool,scribe) sits under envd. `omp-rpc` (core,proto) / `omp-serve` (session,catalog,ai,journal,proto,tool) are transports; `omp-app` tops everything; `omp-e2e` tops app.
- Note: docs' `crates.md` names `omp-inference`, `omp-shell-engine`, `omp-telemetry`, `omp-grep` are stale - real crates are `omp-ai` (crates/ai), `omp-shell`, `omp-observability`; no omp-grep/omp-inference crate exists.

**2. Process topology (separate OS processes, and why)**
- `omp` main process: app+driver+agent+ai+TUI/GUI. Owns presentation lifetime.
- `omp envd` (`crates/envd/src/lib.rs::connect_or_start`, re-execs `current_exe() envd --root ...`, new process group, `envd.log`): durable project-scoped resource owner (files, exec, documents, policy, MCP, LSP/DAP, eval, extension hosts). Separate because it must outlive the app, own policy enforcement, and be the sole host authority. Document authority runs as a task inside envd, not a separate process. Falls back to in-process env if spawn fails.
- `omp __omp-ext-host`: one per extension host/pool, FD 3 Unix socketpair with 4-byte big-endian length + JSON CONTROL envelopes; isolates untrusted Python, generation-fenced by `ExtHostSupervisor`.
- `omp __omp-py-worker`: placed Python worker, piped stdio, `omp.toolhost.v1` varint protobuf; per-pool isolation with health probes/restart backoff/generation increments.
- `omp __omp-eval-child` (or external Python): NDJSON stdio/loopback TCP, `kill_on_drop` process group, interrupt grace.
- MCP servers (JSON-RPC stdio/HTTP), LSP/DAP adapters (Content-Length JSON-RPC), `omp serve`/auth-broker/auth-gateway Tonic daemons (gRPC over UDS or bearer TCP).
- IPC: app<->envd `omp.env.v1` framed protobuf over UDS/named pipe; envd<->ext-host DATA same env frames, CONTROL JSON; embedding parent<->`omp rpc` NDJSON serde (not protobuf).

**3. Agent loop shape** (`crates/agent/src/loop.rs`)
- `Agent<C: TurnClient>` composes TurnClient, `omp_env::EnvClient`, `AgentState` (`Arc<AgentSnapshot>` via `tokio::watch`, resampled per turn), `Journal`, `EventBus`, `Mailbox`, `JobBoard`, abort `watch<u64>`, `InvocationHookBus`, `ControlMailbox`, `Arbiter`.
- Flow: `submit` -> sample abort gen, restore jobs, drain mailbox, `before_agent_start` hook, `Journal::append_turn_input`, emit `agent_start` -> `run_turn`: sample snapshot, verify toolset hash, render prompt/reseed-or-delta, `Journal::start_turn` (durable TurnStart) -> `drive_session`: `TurnClient::turn(Full|Delta)` yields `TurnEvent`s; text/thinking become presentation deltas; tool `PartStart` resolves `ToolIdentity`, checks frozen enabled tools, resolves ADMISSION, opens `SpeculativeCall` (`InvokeTool`), each `PartDelta` relays `ArgText` (unauthorized).
- Commit: `committed_calls` reconciles stream fragments with canonical args; append Outcome+TurnReceipt (`append_arbiter_outcome`), record invocation facts, `authorize_tool_batch` (only for call ids in the receipt), effect tokens + `EffectsAuthorized`, BATCH regime, `AgentPhase::ToolBatch`; `ToolBatch::drive_interruptible` runs concurrently, results in model order; `CallOutcome::{Ok,Faulted,ArgsRejected,Aborted}`, `ToolFinished` event -> next turn's tool-result input.
- Phases: `Idle`, `Projecting`, `Turning`, `ToolBatch` (Cancelling is explanatory only). Cancellation has two channels: ordered `Mailbox` (Immediate/TurnBoundary/Idle classes, FIFO, cancellation-safe backlog) and level-triggered abort generation watch; 500 ms `INTERRUPT_GRACE`, `Abort::EffectsUnknown` after authorization.
- Regimes resolved at 9 fixed points: CONTEXT, TOOL_CHOICE, PRE_MODEL, STREAM, ADMISSION, BATCH, TURN_END, SETTLE, IDLE. Durability is journal-first; EventBus fans out immutable `Arc<AgentEvent>` (lossless or lossy-accounted).

**4. Extension model**
- Signed Python distributions; `DeploymentManifest`/`StaticDeclarations` (crates/ext) declare tools, hooks, services, providers, regimes, UI, telemetry, prompt slots, credentials, secrets, workers, placement, agents, LSP, DAP before any Python runs. Discovery/admission never import the entry module; runtime registration cannot widen the signed surface (`DeclarationDrift`).
- Trust chain: `SignedIndex` Ed25519 -> per-artifact b3+sha256+publisher signature -> TOFU `KeysFile` (rotation only via signed `KeyRotation`) -> exact `GrantsFile` consent -> revocations -> `LockFile` v2 (Python `==3.14.*`, ABI `cp314t`, first-index, digest set) -> content-addressed site generation.
- Runtime: `ExtHostSupervisor::spawn` -> `LifecycleMachine` Declared -> Frozen -> Verified -> Active (or Degraded); activation classes Static/FirstReach/BeforeFirstPrompt/BeforeUiInput; `crates/py` statically links free-threaded CPython 3.14t with frozen stdlib and one authorized site dir.
- Surfaces: `@omp.device`/`@omp.tool` (typed `CallOutcome`), `@omp.hook` (HookGate -> envd `HookControlFactory::compose`, phase/order/name sorted), `@omp.regime` (durable middleware, `ctx` effects + at most one `next_`), `@omp.prelude` (eval helper stub). Tool calls use RAII `WorkerInvocation`; drop requests cancellation, supervisor owns escalation/quarantine/reload.

<results>
<files>
- /tmp/oh-my-pi-omp2/docs/architecture/agent-loop.md - durable turn loop, speculation/commit, mailbox + abort, hooks, regimes, events
- /tmp/oh-my-pi-omp2/docs/architecture/crates.md - production spine (app->driver->agent->session->journal/dom) and crate responsibility tables
- /tmp/oh-my-pi-omp2/docs/architecture/processes.md - process topology, spawn/supervision, IPC framing, protobuf package map
- /tmp/oh-my-pi-omp2/docs/architecture/extensions.md - manifest authority, trust/lock, supervisor lifecycle, Python surfaces
- /tmp/oh-my-pi-omp2/crates/envd - the fat host crate (documents, tools, py, ext, MCP, LSP/DAP, eval)
- /tmp/oh-my-pi-omp2/crates/agent/src/loop.rs - `Agent<C: TurnClient>`, `drive_session`, N-turn settlement
- /tmp/oh-my-pi-omp2/crates/envd/src/worker.rs - ExtHostSupervisor, toolhost protocol, lifecycle fencing
- /tmp/oh-my-pi-omp2/crates/envd/src/lib.rs - `connect_or_start` project daemon spawn and embedded fallback
</files>

<answer>
One composition authority: app presents, driver composes, agent executes turns, session owns journal+DOM. Everything resource-owning (files, exec, docs, Python, policy) lives behind `omp envd` in a separate, longer-lived process; Python extensions and placed workers are further isolated as re-entered child processes of the same binary with generation-fenced supervisors. The agent loop is a journal-first state machine over four phases that speculates tool calls during streaming but only authorizes effects after the inference receipt is durable, with two independent cancel channels (ordered mailbox vs. abort generation). Extensions are signed manifests sealed at FREEZE, executed by envd-owned supervised Python hosts, and hook into the loop only through delegated gates and fixed regime points.
</answer>

<next_steps>
For crate-level work start at crates/agent/src/loop.rs then crates/envd/src/server.rs; for extension work read docs/py/00-overview.md, 05-hooks.md, 15-regimes.md. Treat crates.md's omp-inference/omp-shell-engine/omp-telemetry/omp-grep names as stale (real: omp-ai, omp-shell, omp-observability; no omp-grep crate) before quoting them anywhere.
</next_steps>
</results>