<analysis>
**Literal Request**: Read-only inspection of `crates/ai` and `crates/catalog` (READMEs, Cargo.toml, src filenames), max 8 tool calls, <=45-line summary.
**Actual Need**: A trustworthy mental model of the service spine, middleware ordering, streaming event contract, and the data-driven provider model.
**Success Looks Like**: Named types and files with the composition order, so the caller can navigate the code directly.
</analysis>

**Tower service spine (crates/ai)**
- One erased boundary: `provider/mod.rs` defines `pub type ProviderService = BoxCloneSyncService<Call, Answer, Error>`; per-route services use `layer::stack::RouteProviderService = BoxCloneSyncService<LayerCall<Call>, Answer, Error>`, so every fallback shares one `ExecutionContext` (`layer/mod.rs`, `Arc<ExecutionState>`, clone-cheap).
- `lib.rs`: public edge is statically typed per operation; the provider center is erased exactly once at construction. `registry.rs` holds the immutable `Registry` (ArcSwap `RegistrySnapshot`), built by `RegistryBuilder` from catalog `RouteDef`s via `layer/stack.rs` `RouteStackFactory` / `RouteComposer` / `BuiltinConfig::production(ProductionDependencies)`.
- `router.rs`: `execute_registry_call` = plan then `registry.service().oneshot(call)`. `Router` implements `plan::Planner`; produces `RouteCandidate { model, provider, policy_model: Arc<PolicyModel>, wire_target: WireTarget }`; `first_callable_candidate` leases credentials in candidate-plan order and returns the first callable route (candidates are cloned, never rewritten, so a pinned selector stays intact).

**Fixed-order middleware** (`layer/stack.rs` imports the whole set; `layer/mod.rs` = "Fixed-order request-plane middleware")
account (`AccountPoolLayer`) -> admission -> intent -> operation policy -> budget (`OverallBudgetLayer`, `InferenceLedger`) -> rate -> auth lease -> session -> staging -> encode (`EncodeLayer`, `CredentialApplyLayer`) -> recover (`RecoveryLayer`) -> semantic -> retry (`TransportRetryLayer`) -> observe -> answer (`AnswerLayer`) -> hook (`ProviderErrorLayer`). Supporting modules: `secrets` (bidirectional secret projection/restore), `attempt`, `semantic`, `hook`.

**Routing + recovery**
- Routing is capability-first and exact-selector: `plan::{ExecutionPlan, Planner, negotiate, negotiate_native_option, plan_replay, CapabilityEvidence, FallbackScope}`; ranking/ordering is deterministic in `router.rs`; catalog supplies `PolicyModel` facts only (no raw wire ids), so policy is never inferred from provider/model strings.
- `layer/recover.rs` = "canonical response recovery before semantic output gating", driven by catalog `WirePolicyId`: delegates to `recovery::{dialect/projection, empty, harmony, json (JsonRepairLimits/Stage), reasoning (ReasoningStallGuard), repetition (AttemptRepetitionGuard), thinking, tools (ToolAssembler + schema validation)}`. It also owns `DiscoveryProjector` for normalizing provider discovery pages.
- `layer/retry.rs` = pre-commit, replay-evidence-driven same-route/same-account retry with full-jitter backoff and an ephemeral `RetryNotice`/`RetrySink` countdown (never journaled, never affects replay).

**ChatEvent stream** (`event.rs`)
- `ChatEvent` is the canonical generative stream: `Started(ResponseMeta)`, `BlockStarted{index,kind: BlockKind::{Text,Thinking,ToolCall,Artifact}}`, `TextDelta`, `ThinkingDelta`, `ToolCallStarted`, `ToolArgumentsDelta` (display/telemetry only), `ToolCallReady(ToolCall)` (the sole execution authorization, via `authorized_tool_call()`), `Artifact`, `Usage(UsageUpdate{usage,final_update})`, `WorkflowAction/WorkflowResume/WorkflowCancelled` (live control plane, `is_workflow_control()`), terminal `Completed(Completion{reason: FinishReason})`.
- Wire streams enter as codec `RawEventStream`/`RawEvent` (`codec/`, `transport/{sse,ndjson,eventstream,websocket,frame,encoding}`), get recovered/projected into `ChatEvent`s, and are accounted by `receipt::{ExecutionReceipt, AttemptReceipt, Cost, Usage, RecoveryRecord}`.

**Providers as data (`crates/catalog`)**
- Cargo deps are pure data/compile: `kdl`, `toml`, `serde`, `postcard`, `zstd`, `globset`, `arc-swap` — no HTTP, no transport, no Tower. "The crate contains facts rather than executable provider behavior."
- Modules: `provider.rs` (`ProviderDef`, `RouteDef`, `TransportKind`, `CodecProfile`, `EndpointSpec`, auth/discovery spec ids), `model.rs` (`ModelSpec` -> `PolicyModel` router-facing, `WireTarget{route,codec,endpoint,wire_model}` codec-facing only), `capability.rs` (typed bitsets: `OperationBits`, `ModelCapabilities`, per-modality feature bits), `id.rs` (strong `ProviderId/RouteId/CodecId/ModelKey/ClassId/FamilyId/WireModelId/WirePolicyId`), `policy.rs` (wire-lowering/recovery/header policies), `pricing.rs` (integer-only), `resolve.rs` (exact selection, overlays, aliases, fallback chains, `CompiledCatalog`), `compile.rs` (deterministic offline compilation of checked-in oracle TOML with sha2), `cascade.rs` (KDL compat cascade: class/provider/family/revision/model-glob conjunction rules replacing flat enumerated per-model profiles), `discover.rs` (conservative runtime discovery normalization), `contrib.rs` (`OverlayStack`, immutable, ArcSwap generations), `snapshot.rs` (`Catalog`), `runtime.rs`, `selection.rs`, `taxonomy.rs`, `classify.rs`, `thinking.rs`, `compat/`, `settings.rs`.
- Net effect: adding a provider is a catalog record + codec selection, not new Rust provider code; `omp-ai` contains only `provider/{builtin,copilot,http,fake}.rs` plus codecs/transports.

<results>
<files>
- /tmp/oh-my-pi-omp2/crates/ai/src/lib.rs - crate contract: typed operations over one Tower spine, single erasure point
- /tmp/oh-my-pi-omp2/crates/ai/src/provider/mod.rs - `ProviderService` erased boundary definition
- /tmp/oh-my-pi-omp2/crates/ai/src/layer/stack.rs - fixed-order stack construction, `RouteStackFactory`/`RouteComposer`/`BuiltinConfig`
- /tmp/oh-my-pi-omp2/crates/ai/src/layer/mod.rs - `ExecutionContext`, `LayerCall`, layer module list
- /tmp/oh-my-pi-omp2/crates/ai/src/layer/recover.rs - recovery middleware + `DiscoveryProjector`
- /tmp/oh-my-pi-omp2/crates/ai/src/layer/retry.rs - transport retry, `RetryNotice`/`RetrySink`
- /tmp/oh-my-pi-omp2/crates/ai/src/router.rs - capability-first planning, `RouteCandidate`, `first_callable_candidate`
- /tmp/oh-my-pi-omp2/crates/ai/src/registry.rs - immutable registry/snapshot, `RouteUnavailable`
- /tmp/oh-my-pi-omp2/crates/ai/src/event.rs - `ChatEvent` canonical stream
- /tmp/oh-my-pi-omp2/crates/ai/src/call.rs - `Call`/`OperationCall` envelope
- /tmp/oh-my-pi-omp2/crates/catalog/src/model.rs - `PolicyModel` vs `WireTarget` split
- /tmp/oh-my-pi-omp2/crates/catalog/src/provider.rs - `ProviderDef`/`RouteDef`/`TransportKind`
- /tmp/oh-my-pi-omp2/crates/catalog/src/resolve.rs - exact selection, overlays, fallback chains
- /tmp/oh-my-pi-omp2/crates/catalog/src/compile.rs - offline deterministic catalog compilation
- /tmp/oh-my-pi-omp2/crates/catalog/src/cascade.rs - KDL compat cascade rules
</files>

<answer>
`omp-ai` is a single Tower service spine: `Call -> (fixed layer stack) -> Answer`, type-erased once into `ProviderService`/`RouteProviderService`. `Router` plans capability-first from catalog facts, then dispatches via `oneshot`; recovery and retry are just layers in the fixed order, and recovery is selected by catalog `WirePolicyId` rather than provider-specific code. Streaming is a single canonical `ChatEvent` enum whose `ToolCallReady` is the only execution authorization. `omp-catalog` is pure data + compile: `PolicyModel` (router-facing, no wire ids) and `WireTarget` (codec-facing, carries wire model id) are structurally separated, so new providers are added as catalog records/overlays compiled offline, not as new per-provider Rust.
</answer>

<next_steps>
Read `layer/stack.rs` bottom half (`build_execution_stack`) for the literal ordering, and `recovery/projection.rs` + `codec/` for RawEvent -> ChatEvent lowering. Ready to proceed - no follow-up needed.
</next_steps>
</results>