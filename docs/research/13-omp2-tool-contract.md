<analysis>
**Literal Request**: Inspect only `crates/tool` and `crates/tools` (README, Cargo.toml, src names) and summarize the revisioned tool contract vs oh-my-tau's tool layer in <=45 lines.
**Actual Need**: A crisp statement of what makes this contract different (intent param `i`, versioned identity, single-stream lifecycle) so it can be contrasted with a conventional tool layer without re-reading the code.
**Success Looks Like**: Named types/files, the exact mechanism of each of the three properties, and an explicit note where tau evidence is unavailable.
</analysis>

Summary (scoped to the two crates; `grep` for `tau` across both returned 0 matches, so the tau side below is stated as contrast, not as inspected tau source).

REVISIONED CONTRACT — /tmp/oh-my-pi-omp2/crates/tool (`omp-tool`, contracts only, no execution)

1. Intent param `i`. `inject_protocol_schema` / `schema::<T>()` force `i` to be the first required property of every model-facing schema ("Short present-participle intent for this call"), plus optional `notrunc`. `decode_params` strips `i` and `notrunc` before deserializing the domain `Params`, so they stay in canonical invocation arguments for journaling/dispatch policy but never pollute per-tool parameter types. Schema is draft-2020-12, subschemas inlined, `$schema`/`title` removed.
2. Versioned identity. `Rev { family: Str, n: u16 }` parses `family.n` or bare `n`; `ToolIdentity { rev, name姹 }`; `姉`; `Tool姫` name is durable姉` of `Tool姞普-durable domain姫/` in a姤[92- `姉[0. `姘ImperativeToolSpec姉吻` tool.姉IMUrevised姉、` tool.姉塞 { registry =姘我atomSpec.t姉用时 alt text `姉 pustaka` with姉碎 revolvedTool姒法-carboxRepresent姫户名 repr(self姫 chip at half.姘 intrinsically flawed. No姉. `ToolSpec` pins `name`, `rev`, `description`, `schema: Bytes`, `Constraint`, `Effects`, and `projection_code: [u8;32]` (`native_projection_code(crate, version, module_source)`). Thread items carry `TOOL_REV_PROP = "omp/tool-rev"`. The `Tool` trait keeps `Params/Update/Payload/Fault` until `Registry::register`; the registry erases once, keeps every revision only for pure prompt/lift code (`lift(from: &Rev, RecordedCall) -> Option<LiftedCall>`, `ProjectedCall`/姒噪), and advertises only the live revision. Projection results are cached by `ProjectionKey` (identity + caps hash + projection hash).
4. Single-stream lifecycle. `Tool::call(IncomingParams) -> impl Stream<Item = Ev<Update, Payload, Fault>>`. `IncomingParams` pulls raw fragments through `omp_core::slopjson` linearly; commitment is an explicit frame, dropped feeds abort, interrupts are observed (`Interrupt`, `InterruptibleParams`), and unpulled JSON keys are never validated. `Ev` = Update | Diag | Args(ArgIssue) | Ab�����������������������������������������������������������������������������������������������������姉. Erasure is a total state machine (`ErasedEv`, `ErasedOutcome`); a stream that ends without a terminal event is synthesized as `Abort::MissingOutcome`. `ExecutionMode` (Parallel/Sequential) is declared per revision; `Effects` (documents/exec/inference/desktop/subagents) is a deny-all-by-default envelope with `narrow()` subset checking; `Constraint` covers None/Schema/Grammar with `Fallback` on unsupported routes; `Presentation` (Slot/Device/Hidden) and `ToolsPolicy` (Auto/DeviceOnly/ToolOnly) control advertisement; `ToolRoute` (Native/Remote/Worker) separates execution locus; `Claims`/`Precedence`/`ShadowClaim` resolve name contests deterministically; `HostToolSpec.rev` is optional for foreign RPC hosts.
5. /tmp/oh-my-pi-omp2/crates/tool/src: `device_path.rs`, `diag.rs`, `incoming.rs`, `registry.rs`, `render.rs`, `spec_generated.rs` (generated `operation_spec`, `phase_legality_matrix`, `runtime_duration_metadata`), `lib.rs` (2244 lines: schema injection, `Rev`, `Effects`, `Tool`, `ArgSpec`/repair, `Part`/`OutputProjection`, `CallOutcome`). Tests: `tests/contracts.rs`, `tests/schema.rs`.

EXECUTOR SIDE — /tmp/oh-my-pi-omp2/crates/tools (`omp-tools`, resource-owning, depends on `omp-tool`)

6. Each tool "owns or borrows an environment-side resource through a concrete generic adapter"; "streaming arguments may prepare leases and previews, but only the explicit commitment frame authorizes effects"; "durable payloads remain dialect-neutral truth; model-facing parts are deterministic projections produced by the tool revision that created them."
7. `src/lib.rs` declares 32 `BuiltinToolIdentity` entries (read, web_search, memory trio, edit, write, grep, glob, bash, eval, todo, task, ask, hub, github, debug, checkpoint, ast_grep, ast_edit, rewind, think, goal, yield, image_gen, tts, report_issue, learn, manage_skill, computer, security_scan; hidden flag for think/goal/yield/report_issue/learn/manage_skill), a `view!` macro (`omp_macros::view`) and `register_builtin_renderers`.
8. Module map is one file/folder per capability: `ask, ast_edit, ast_grep, auto_background, browser, checkpoint, computer, debug(+debug_render), device, device_ctl, edit/{apply_patch,observer,projection,replace}, eval/{kernel,idle_timeout}, gallery, github, glob, goal, grep, hub, learn, lsp/{actions,checkers,diagnostics,navigation,refactor,render}, manage_skill, memory, memory_edit, output_schema, path, read/{archive,conflicts,dirtree,format,image,json_query,markit/*,mutation,notebook,pdf,profile,resolver,selector,web/*}, render/{agentic,ast,codeintel,edit,exec,fs,hub,interaction,misc,paths,search,truncate,view,web}, review, security_scan, settings, shell, shell_intercept, shell_uri, staging, task, think, todo, web_search, write, yield_tool`. `build.rs` + `url-vocab.json` feed generated specs; `tests/it/*` covers documents/edit/eval/glob/grep/markit/pdf/read/shell/web/write.

CONTRAST WITH oh-my-tau's tool layer (inferred; tau sources are outside the inspected scope)

9. Tau-style layers typically expose `{name, description, input_schema}` plus a request/response handler (`fn(input) -> Result<output>`): identity is the name alone, there is no `i`, no `notrunc`, no revision, no `projection_code`, and no separate durable-payload vs model-projection split.
10. The three contract deltas here: (a) every call carries a journaled intent string `i` and an output policy `notrunc` injected centrally rather than by each tool; (b) `(name, family.n)` is the durable identity, historical revisions survive only as prompt/lift code, and foreign hosts may omit `rev`; (c) invocation is one linear streaming pull with an explicit commitment frame, typed `Args`/`Aborted`/`Diag` events, interrupt observation, and a synthesized `MissingOutcome` abort — not a batch JSON argument object handed to a handler. Effects are declared per revision and narrowed per invocation, and advertisement is policy-driven (Slot/Device/Hidden, Auto/DeviceOnly/ToolOnly) rather than a fixed tool list.

<results>
<files>
- /tmp/oh-my-pi-omp2/crates/tool/README.md - one-paragraph statement of the typed/revisioned boundary, erasure-once, single-stream pull, no executors
- /tmp/oh-my-pi-omp2/crates/tool/src/lib.rs - `inject_protocol_schema`/`decode_params` (intent `i`), `Rev`/`ToolIdentity`/`ToolSpec`/`projection_code`, `Tool` trait, `Ev`/`CallOutcome`/`Effects`/`Constraint`
- /tmp/oh-my-pi-omp2/crates/tool/src/incoming.rs - streaming argument pull, commitment, interrupt observation
- /tmp/oh-my-pi-omp2/crates/tool/src/registry.rs - one-time type erasure, live-only advertisement, lift composition, projection cache, claims/precedence, routes
- /tmp/oh-my-pi-omp2/crates/tool/src/spec_generated.rs - generated operation/phase/authority metadata
- /tmp/oh-my-pi-omp2/crates/tools/README.md - commitment-gated effects, dialect-neutral durable payloads, revision-owned projections
- /tmp/oh-my-pi-omp2/crates/tools/src/lib.rs - 32 builtin identities, module map, `view!` macro, renderer registration
- /tmp/oh-my-pi-omp2/crates/tools/Cargo.toml - depends on `omp-tool` plus resource crates (rusqlite, pyo3, image, pdf-inspector, omp-shell)
</files>

<answer>
The revisioned contract is: intent `i` (+ `notrunc`) injected into every schema and stripped before domain decoding; identity is `(name, family.n)` with historical revisions retained only for projection/lift; invocation is a single linear argument stream where effects require an explicit commitment frame and every path terminates in a typed `CallOutcome`. `omp-tool` holds only these contracts plus deterministic lowering; `omp-tools` supplies resource-owning executors behind that boundary. No `tau` reference exists in either crate, so the tau comparison above is a contrast with a conventional name+handler tool layer, not an inspected tau implementation.
</answer>

<next_steps>
If a real tau-vs-omp diff is required, the tau crate must be named explicitly (it is not under these two paths). Otherwise this summary is sufficient to write the comparison document.
</next_steps>
</results>