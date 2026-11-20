<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/host.rs - 1362 LOC. JSON-RPC transport to the Bun extension host: spawn/connect, PendingTable, per-connection `Conn` generations, writer/reader tasks, reconnect + shared-daemon fallback, `emit`/`emit_nowait`/`emit_skipping`, tool/command/flags/provider RPCs.
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/hooks.rs - 247 LOC. Maps host events onto pa-agent hooks: `transform_context`, `before_tool_call` (block/input rewrite/additionalContext), `after_tool_call` (content/details/isError rewrite).
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/lifecycle.rs - 897 LOC. Typed `LifecycleEvent` enum (~25 variants: session_start, before_agent_start, input, turn_*, message_*, tool_execution_*, session_before_compact/switch/branch/tree, session_stop, ...), `emit_*` helpers, `HostResponder` trait + `answer`/`serve` loop for host-initiated requests.
- /home/whrho/git/oh-my-tau/crates/omp-ext/src/lib.rs - 16 LOC. Public surface: `ExtHost`, `HostConfig`, `EmitResult`, `ToolInfo`, `CommandInfo`, `hooks`, `lifecycle`.
- /home/whrho/git/oh-my-tau/crates/omp-ext/tests/host.rs - 857 LOC, 12 tokio tests (handler failure survival, shared-daemon isolation/death/concurrency, tools+commands, skipExtensions, pa-agent hook mapping, message_update delta isolation, nowait barriers, big frames).
- /home/whrho/git/oh-my-tau/crates/omp-ext/tests/lifecycle.rs - 351 LOC, 4 tests; /home/whrho/git/oh-my-tau/crates/omp-ext/tests/lifecycle_more.rs - 282 LOC, 2 tests (wire-shape assertions for every lifecycle event, HostResponder answers).
- /home/whrho/git/oh-my-tau/crates/omp-ext/tests/common/mod.rs - 81 LOC. Gate: skips with a stderr notice when `bun` or the `@oh-my-pi/pi-coding-agent` package is absent.
- /home/whrho/git/oh-my-tau/crates/omp-ext/tests/fixtures/{fixture,lifecycle,mutating,nowait_blocking,context-slim,broken,nofactory}.ts - TS extension fixtures written against the real `ExtensionAPI` surface.
- /home/whrho/git/oh-my-tau/ext-host/host.ts - 1014 LOC. The actual TS host: discovers/loads `.ts|.js` extensions, builds the `api` object, per-event handler aggregation, per-handler timeout, nowait barrier, `skipExtensions`, `message_update` delta chains.
- /home/whrho/git/oh-my-tau/ext-host/shard.ts - 190 LOC. Opt-in shared daemon (`OMP_RS_EXT_SHARED=1`): one Bun process, one Worker per session over a unix socket.
- /home/whrho/git/oh-my-tau/ext-host/session.worker.ts - 102 LOC. Per-session Worker; redirects `homedir`/`cwd`/spawn env to the owning session.
- /home/whrho/git/oh-my-tau/ext-host/legacy-typebox.ts - 176 LOC. Vendored `api.typebox` subset (lazy-loaded).
- /home/whrho/git/oh-my-tau/crates/omp-wasm/src/host.rs - 381 LOC. `wasmi` runtime: `WasmModule::load` (compile + `omt_manifest`), `call` on a dedicated OS thread with fuel (1e10), 256 MiB memory cap, 30 s wall timeout, `omt.log` capture, trap->clean error mapping.
- /home/whrho/git/oh-my-tau/crates/omp-wasm/src/manifest.rs - 47 LOC. `Manifest`/`ToolDef`/`CommandDef` serde contract.
- /home/whrho/git/oh-my-tau/crates/omp-wasm/src/registry.rs - 65 LOC. `discover(cwd)` over `~/.omp/agent/plugins/*.wasm` + `<cwd>/.omp/plugins/*.wasm`, broken plugins recorded as failures, never fatal.
- /home/whrho/git/oh-my-tau/crates/omp-wasm/tests/abi.rs - 226 LOC, 8 tests via hand-written `wat` modules (manifest parse, tool/command round trip, fuel exhaustion, trap, missing export, invalid wasm, discovery failures).
- /home/whrho/git/oh-my-tau/examples/wasm-plugin-hello/src/lib.rs - 161 LOC. `#![no_std]` cdylib reference plugin exporting `memory`, `omt_alloc`, `omt_manifest`, `omt_call`, importing `omt.log`.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/ext.rs - 1981 LOC. Where omp-ext is actually wired: `RuntimeResponder` implements `HostResponder` (compact/navigate_tree/ui_confirm/set_model...), event fan-out from the agent loop.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/wasm.rs - 258 LOC. Bridges omp-wasm into the agent: `wasm_tools` -> `<plugin>_<tool>` `AgentTool`, `/plugins` listing, `/<plugin>:<command>` dispatch, cached registry per cwd.
- /home/whrho/git/oh-my-tau/docs/wasm-plugins.md - 112 LOC. The WASM ABI + discovery + limits contract.
</files>

<answer>
There are **two entirely separate extension mechanisms**, and only one of them is Rust-native.

**1. TS extensions via a Bun sidecar (`omp-ext`) — the primary, omp-compatible path**

Extensions are ordinary omp `ExtensionAPI` `.ts`/`.js` files loaded **unmodified**. Rust never interprets them; `omp-ext` is a client of an external process:

- Transport: newline-delimited JSON-RPC 2.0 over the child's stdio (`bun ext-host/host.ts`), or over a unix socket when the shared daemon is enabled (`HostConfig.shared_socket` / `OMP_RS_EXT_SHARED=1`). `host.rs` owns writer coalescing, pending-waiter tables, connection generations, and a 5-attempt shared->private fallback on daemon death.
- The TS side (`ext-host/host.ts`) does all the real work: `discover()` scans `~/.omp/agent/extensions` (or an explicit path list) for `*.ts|*.js` files or dirs with `index.ts`; `loadOne` dynamic-imports the file and calls its **default-exported factory** with an `api` object. That api is the extension contract: `on(event, handler)`, `registerTool`, `registerCommand`, `registerFlag`/`getFlag`, `registerProvider`/`unregisterProvider`, `registerMessageRenderer`, `sendMessage`/`sendUserMessage`/`appendEntry`, `exec`, `setLabel`, `setModel`/`setSessionName`/`setThinkingLevel` (RPC-backed), lazy `api.typebox` / `api.zod`, `logger`, and an `events` bus. Several UI-ish registers are deliberate no-ops in headless mode.
- Handler aggregation lives host-side per event type: `tool_call` is **fail-closed** (a throwing/timing-out handler becomes `{block:true}`, later handlers are skipped), `input` is first-`handled`-wins, `tool_result`/`context`/`before_agent_start` fold sequentially with later handlers seeing earlier mutations, `message_update` uses per-encoder delta chains with a `resync` request back to the core. Fire-and-forget `emit(nowait)` runs strictly serially behind a barrier so wire order == execution order.
- Rust consumes the aggregate through typed surfaces: `hooks.rs` maps `context`/`tool_call`/`tool_result` onto `pa-agent`'s `TransformContextFn`/`BeforeToolCallFn`/`AfterToolCallFn`; `lifecycle.rs` models ~25 events as a serde-tagged enum with `emit_*` helpers, plus `HostResponder` for host->core requests (`compact`, `navigate_tree`, `ui_confirm`, model/session setters). `omp-cli/src/ext.rs` (1981 LOC) is the glue that drives all of it.
- Cost: needs `bun` at runtime, and each extension runs with full process privileges (filesystem, network, child processes). The shared daemon (`shard.ts` + `session.worker.ts`) amortizes Bun startup: one process, one Worker isolate per session, with per-session env/cwd/HOME patched in.

**2. WASM plugins (`omp-wasm`) — the sandboxed, dependency-free path**

- A plugin is a single `.wasm` implementing a JSON-over-linear-memory ABI: exports `memory`, `omt_alloc(len)->ptr`, `omt_manifest()->(ptr<<32)|len`, `omt_call(ptr,len)->(ptr<<32)|len`; the only import is `omt.log(ptr,len)`. No filesystem, no network.
- Rust is the host, using `wasmi` (pure-Rust interpreter): module compiled once at load, manifest parsed at load, and **each call instantiates a fresh store** so no state leaks between calls. Calls run on a dedicated OS thread with fuel metering (1e10), a 256 MiB memory cap, and a 30 s wall-clock timeout; traps map to clean messages ("plugin fuel exhausted", "plugin trapped: ...").
- Discovery is `~/.omp/agent/plugins/*.wasm` + `<cwd>/.omp/plugins/*.wasm`; broken plugins are logged into `Discovery::failures` and never fatal.
- `omp-cli/src/tools/wasm.rs` exposes each manifest tool as `<plugin>_<tool>` (same registry shape MCP tools use) and each command as `/<plugin>:<command>`, with `/plugins` for listing. Registry is cached per cwd behind a `Mutex`.
- Capability trade-off: WASM plugins get **no** hooks/lifecycle events at all — only tool calls and slash commands. They cannot intercept `tool_call`, rewrite context, or register providers. They also cannot use `serde_json` (the reference example hand-rolls JSON scanning in `no_std`).

**Examples**
`examples/` contains exactly one crate: `wasm-plugin-hello` (standalone, `crate-type=["cdylib"]`, `#![no_std]`, `panic="abort"`, not a workspace member), built with `cargo build --target wasm32-unknown-unknown --release` and dropped into `.omp/plugins/`. There is **no** TS example crate — TS extension examples exist only as test fixtures under `crates/omp-ext/tests/fixtures/`, notably `fixture.ts` (33 LOC) which is a compact tour of the whole API surface.

**LOC / test summary**
| Area | LOC | Tests |
|---|---|---|
| omp-ext src (host 1362, lifecycle 897, hooks 247, lib 16) | 2522 | — |
| omp-ext tests (host 857, lifecycle 351, lifecycle_more 282, common 81) | 1571 | 18 tokio tests, all driving a real `bun` host; auto-skip if bun/omp pkg missing |
| ext-host (host.ts 1014, shard 190, worker 102, legacy-typebox 176) | 1482 | none (covered indirectly by omp-ext tests) |
| omp-wasm src (host 381, registry 65, manifest 47, lib 17) | 510 | — |
| omp-wasm tests | 226 | 8 tests, synthesized `wat` modules |
| examples/wasm-plugin-hello | 161 | none |
| omp-cli glue (ext.rs 1981, tools/wasm.rs 258) | 2239 | covered via omp-ext/omp-wasm suites |
</answer>

<next_steps>
If you are adding a capability (hook, context rewrite, provider registration), it must go through the TS path — `omp-ext/src/lifecycle.rs` for the Rust event type and `ext-host/host.ts` `emit()` for the aggregation branch. If you are adding a tool that must be sandboxed and Bun-free, it belongs in `omp-wasm` plus `omp-cli/src/tools/wasm.rs`; you will need a matching branch in `docs/wasm-plugins.md` since that ABI is a published contract. If you are auditing the TS host's parity with upstream omp, `ext-host/host.ts` `emit()` (line ~603) and `makeApi` (line ~268) are the two places where parity actually lives. Note that the omp-ext suite silently skips on a machine without `bun` — a green run is not evidence the TS path works there.
</next_steps>
</results>