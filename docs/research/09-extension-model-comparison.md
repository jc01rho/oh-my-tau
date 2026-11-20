I have enough evidence across all three. Note: `/tmp/oh-my-tau` does not exist — the real tree is `/home/whrho/git/oh-my-tau`.

## Comparison: three extension models

| Axis | (1) oh-my-pi `ExtensionAPI` | (2) omp2 embedded Python | (3) oh-my-tau `ext-host` + `omp-ext` + `omp-wasm` |
|---|---|---|---|
| Language | TS/JS module, default factory | Python 3.14t, decorators | TS/JS (compat host) **and** WASM plugins |
| Execution | In-process, same Bun isolate | One killable child process per extension | Bun child process (tier A); `wasmi` sandbox (tier B) |
| Isolation | **None.** Raw timer throw = process `uncaughtException` = session torn down | Full process + own site tree; `SIGKILL`+respawn | Tier A: process, not sandboxed. Tier B: no fs/net at all |
| Cancellation | Race-the-promise; handler keeps running | Kill boundary (ADR 0011) | Tier A: `cancel_tool` aborts via `AbortController`; tier B: fuel/timeout |
| Surface | ~35 `ExtensionAPI` methods, 45+ events, UI/renderers/providers/voice/live | `@omp.device`, `@omp.tool`, `@omp.hook` (5 arms), `@omp.regime`, services, slots, env, journal | `load/emit/list_tools/call_tool/list_commands/call_command` + subset of API; WASM: tools + commands only |
| Trust | None; extension = arbitrary code | Ed25519 signed index, BLAKE3+SHA256, TOFU pins, capability grants, revocations, lock v2 | None (tier A); implicit (tier B has no capabilities) |
| Ordering | Loader discovery order, per-emitter ad-hoc conflict rules | `HookPhase` (PRECHECK/TRANSFORM/REVIEW/APPROVAL/OBSERVE), declared composition rules | Mirrors pi's ordering (`outer:` loops in `host.ts`) |
| Distribution | npm/`.ts` files in `.omp/extensions`, `settings.json`, plugin manifests | Signed wheels via `uv`, `omp.toml` + `omp.lock` | `.ts` files from `~/.omp/agent/extensions`; `.wasm` from `~/.omp/agent/plugins` |

### Model 1 — oh-my-pi (`/tmp/oh-my-pi-ro`)

Load pipeline is `loader.ts` (817 lines) → `runner.ts` (2748) → `wrapper.ts` (618), types in `types.ts` (2577). Modules are imported concurrently by Bun, factories bound sequentially in path order, registration-only until `ExtensionRunner.initialize()`.

The design's own documented weakness is isolation: `docs/extensions.md:472` states extensions run "in-process with no isolation" and a throw from a raw timer "tears down the whole session." `ctx.setInterval`/`ctx.setTimeout` (`managed-timers.ts`) exist purely to contain that. Handler budget is 30s, `session_shutdown` 2s.

Its real asset is breadth: `ctx.runEphemeralTurn`, `ctx.startLive` (WebRTC voice), `ctx.sessionModes`, `ctx.annotations`, `ctx.models`, provider registration with `streamSimple` command-backed providers, transcript presenters, link schemes, and legacy `pi` compat shims (`legacy-pi-ai-shim.ts`, `legacy-pi-tui-shim.ts`, `legacy-typebox.ts`).

### Model 2 — omp2 (`/tmp/oh-my-pi-omp2`)

ADR 0036 is an explicit, argued rejection of model 1: no JS/TS plugin runtime, one embedded runtime for Extensions + `Eval` + `@remote`, and "adding a second extension language requires superseding this record."

Four mechanisms matter for migration:
1. **Manifest authority** — `StaticDeclarations` in `crates/ext/src/config.rs` is signed and sealed before any Python imports. `LifecycleMachine::activate_declared` detects `DeclarationDrift`. This is what makes lazy child spawn possible.
2. **Two sockets** — CONTROL (`toolhost/v1` protobuf on an inherited fd, deliberately not stdio so `print()` cannot corrupt the protocol) and DATA (`env/v1` to envd). CONTROL carries no world access.
3. **Kill boundary** — `ExtHostSupervisor` owns cancellation, respawn backoff, generation fencing, `quarantine`.
4. **Decision vocabulary** — hooks return exactly `Allow`/`Deny`/`Modify`/`Defer`/`RequireApproval`; `docs/py/05-hooks.md` argues pi's 45 events with per-emitter first-wins/last-wins rules are unpredictable by construction.

Status is **Partial** per ADR 0036: `@remote` placement, scoped env handles, and spill diversion remain unproved.

### Model 3 — oh-my-tau (`/home/whrho/git/oh-my-tau`)

Two independent tiers, and conflating them is the main analytical trap.

**Tier A — compat host.** `ext-host/host.ts` is a Bun script that loads *unmodified* omp extensions and speaks newline-delimited JSON-RPC; `crates/omp-ext` is the Rust client (`ExtHost`, `HostConfig`, `hooks.rs` mapping to `pa_agent` hook points). This buys near-zero migration cost for pi extensions — `crates/omp-ext/tests/fixtures/fixture.ts` is written against the real omp API.

But compatibility is not fidelity. In `host.ts`, `registerShortcut`, `registerMessageRenderer`, `registerAssistantThinkingRenderer`, `registerComposerShape`, `registerFileWriteFallback`, `registerFileDeleteFallback`, `getServiceTiers`, `setServiceTier`, and ~10 `ui.*` methods are `noop`; `runEphemeralTurn` throws `unsupported(...)`. An extension loads, reports success, and silently does nothing. There is also a bespoke `message_update` delta/patch codec (`ExtUpdateEncoder` in `crates/omp-cli/src/events.rs`) whose inverse is `rebuildMessageUpdate` — real engineering, but a private protocol with no spec outside those two files.

Cost: **Bun is a runtime dependency**. README: "Optional: bun, needed only to run omp extensions."

**Tier B — WASM.** `crates/omp-wasm` is a genuinely different model: `memory`/`omt_alloc`/`omt_manifest`/`omt_call` over JSON in linear memory, one import (`omt.log`), no filesystem or network. `wasmi` fuel 10^10, 256 MiB, 30s wall timeout on a dedicated OS thread. But the manifest (`manifest.rs`) admits only `tools` and `commands` — **no hooks, no context transforms, no policy, no UI**. It is a plugin model, not an extension model.

## Migration trade-offs

**pi/omp → omp2 is a rewrite, not a port.** ADR 0036 states it plainly: "pi's TypeScript extensions are not portable; the ecosystem restarts in Python." Concretely you must translate `pi.on("tool_call")` → `@omp.hook`, `registerTool` → `@omp.device`/`@omp.tool`, `registerCommand` → `@omp.command`, and `sendMessage(deliverAs:)` → journal/context-patch surfaces. You also gain work pi never required: an `omp.toml`, a dependency lock, and a capability declaration you must get right or `DeclarationDrift` degrades the extension. In exchange you get real fault containment, deterministic hook ordering, and install-time trust — the three things model 1 documents as unfixed.

**pi → tau tier A is cheap and lossy.** Near-zero source changes, but you inherit a compatibility surface that is a subset of the original with silent no-ops. For a policy/context/tool extension it works; for a UI-heavy or provider-registering extension you will discover the gaps at runtime, not at load. You also take a Bun dependency and a private delta wire format.

**pi → tau tier B is a different product.** You get a hard sandbox and a static binary, and you give up hooks entirely. Only viable for self-contained tools whose inputs and outputs are JSON — which excludes most of the catalogue.

**omp2 ↔ tau are not migration targets for each other.** omp2's Python surface is orders of magnitude larger (18 doc files, ~38,800 lines) than tau's WASM ABI, and tau has no Python runtime at all.

**Cross-cutting judgment.** These three are not points on one axis; they answer different questions. Model 1 optimizes for authoring power and ecosystem continuity and pays in isolation. Model 2 optimizes for containment, determinism, and supply-chain trust and pays in ecosystem reset plus binary/RSS cost. Model 3 optimizes for binary size and a static single-binary deployment, and pays by splitting into a lossy compat shim plus a capability-poor sandbox — the one place where the trade-off is hard to defend is that tier A's no-ops make degradation invisible to both author and user.

If the goal is a durable extension platform, model 2's design is the only one that closes all three holes model 1 documents. If the goal is running today's pi extensions on a Rust core with minimal work, model 3 tier A is the only option that exists — just budget for auditing every extension against the no-op list in `host.ts`.

<results>
<files>
- /tmp/oh-my-pi-ro/docs/extensions.md - authoritative spec for the pi ExtensionAPI: 35+ methods, 45+ events, in-process no-isolation warning at line 472
- /tmp/oh-my-pi-ro/docs/extension-loading.md - discovery paths, load order, plugin manifest resolution
- /tmp/oh-my-pi-ro/packages/coding-agent/src/extensibility/extensions/types.ts - canonical ExtensionAPI/event type unions (2577 lines)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/extensibility/extensions/runner.ts - event dispatch and per-emitter conflict semantics (2748 lines)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/extensibility/extensions/loader.ts - module import + factory binding
- /tmp/oh-my-pi-ro/packages/coding-agent/src/extensibility/extensions/managed-timers.ts - the isolation workaround for in-process timers
- /tmp/oh-my-pi-omp2/docs/adr/0036-python-for-extensions.md - the decision record: one embedded CPython, no JS runtime, killable children, explicit "not portable" cost
- /tmp/oh-my-pi-omp2/docs/architecture/extensions.md - signed-distribution model, lifecycle state machine, two-socket topology, key-file table
- /tmp/oh-my-pi-omp2/docs/py/00-overview.md - process model, CONTROL/DATA split, latency classes, per-extension keying rationale
- /tmp/oh-my-pi-omp2/docs/py/05-hooks.md - the 5-arm HookDecision vocabulary and why pi's 45-event ordering is rejected
- /tmp/oh-my-pi-omp2/docs/py/01-devices.md - @omp.device/@omp.tool declaration surface and dyn catalog
- /tmp/oh-my-pi-omp2/docs/py/15-regimes.md - durable middleware model
- /tmp/oh-my-pi-omp2/crates/py/python/omp/__init__.py - the frozen Python API root; importing it performs no I/O
- /tmp/oh-my-pi-omp2/crates/py/src/lib.rs - embedded CPython 3.14t engine, frozen stdlib, isolated config
- /tmp/oh-my-pi-omp2/crates/ext/src/config.rs - DeploymentManifest/StaticDeclarations, the signed authority boundary
- /tmp/oh-my-pi-omp2/crates/envd/src/exthost/lifecycle.rs - LifecycleMachine, DeclarationDrift detection, activation triggers
- /tmp/oh-my-pi-omp2/crates/envd/src/worker.rs - ExtHostSupervisor: spawn, cancellation, respawn, quarantine
- /home/whrho/git/oh-my-tau/ext-host/host.ts - Bun compat host; the no-op list and unsupported() surface are the migration risk
- /home/whrho/git/oh-my-tau/ext-host/shard.ts - optional shared-daemon mode, one Bun process with a Worker per session
- /home/whrho/git/oh-my-tau/ext-host/session.worker.ts - per-session isolate, env/cwd redirection
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/host.rs - Rust JSON-RPC client, HostConfig, reconnect logic
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/hooks.rs - maps host event aggregation onto pa-agent hook points
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/lifecycle.rs - core-emitted lifecycle events matching pi payload shapes byte-for-byte
- /home/whrho/git/oh-my-tau/crates/omp-ext/tests/fixtures/fixture.ts - proof that unmodified omp extensions load
- /home/whrho/git/oh-my-tau/crates/omp-wasm/src/host.rs - wasmi runtime, fuel/memory/wall-timeout limits
- /home/whrho/git/oh-my-tau/crates/omp-wasm/src/manifest.rs - WASM manifest: tools and commands only, no hooks
- /home/whrho/git/oh-my-tau/crates/omp-wasm/src/registry.rs - discovery from ~/.omp/agent/plugins and <cwd>/.omp/plugins
- /home/whrho/git/oh-my-tau/docs/wasm-plugins.md - the ABI contract and stated limits
- /home/whrho/git/oh-my-tau/examples/wasm-plugin-hello/src/lib.rs - reference #![no_std] plugin
</files>

<answer>
The three models are not points on one axis. oh-my-pi is a maximally capable in-process TS API that documents its own missing isolation. omp2 deliberately deletes that model: one embedded free-threaded CPython 3.14t, one killable child per extension, a signed manifest that is authoritative before any code runs, and a 5-arm typed decision vocabulary replacing 45 ad-hoc events. oh-my-tau does not choose — it ships a lossy Bun compat host for unmodified pi extensions plus a capability-poor WASM sandbox that can only declare tools and commands.

Migration cost is asymmetric. pi→omp2 is a full rewrite with new obligations (omp.toml, lock, capability declarations) that buys fault containment, deterministic ordering, and install-time trust. pi→tau tier A is near-zero source change but silently degrades: roughly a dozen API methods are noop and runEphemeralTurn throws, so extensions report success while doing nothing. pi→tau tier B is not a migration at all — no hooks, no context, no policy, JSON in and out only. omp2 and tau are not interchangeable: tau has no Python runtime and omp2's surface is two orders of magnitude larger.

Note on the task premise: /tmp/oh-my-tau does not exist. The tree is at /home/whrho/git/oh-my-tau.
</answer>

<next_steps>
If the decision is which model to build on: pick model 2 if you need containment and supply-chain trust and can absorb the ecosystem reset — its ADR explicitly prices that in. Pick model 3 tier A only if running existing pi extensions on a Rust core with minimal effort is the binding constraint, and audit every extension against the noop list in /home/whrho/git/oh-my-tau/ext-host/host.ts:170-242 before shipping.

If you need per-extension migration estimates, the next concrete step is diffing the event names each extension uses against omp2's catalog in /tmp/oh-my-pi-omp2/docs/py/05-hooks.md and tau's handled branches in host.ts `emit()`. Ready to proceed on that if wanted.
</next_steps>
</results>