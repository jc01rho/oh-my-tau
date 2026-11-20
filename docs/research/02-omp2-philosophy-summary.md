<analysis>
**Literal Request**: Read PHILOSOPHY.md and README.md in full; report philosophy, non-goals, TypeScript rationale, architecture/scope, invariants.
**Actual Need**: A compact quotable briefing the parent can act on without opening the files.
**Success Looks Like**: Exact headings, absolute paths, four points, <=60 lines.
</analysis>

Files read in full (20104 B / 17280 B). Both have a `# PHILOSOPHY` / `## Workspace layout` structure; no literal "Non-goals" heading exists — the non-goals are stated as rejections, flagged as such below.

**(1) Core philosophy** — `/tmp/oh-my-pi-omp2/PHILOSOPHY.md`
- One-liner: "**build a coding harness so good that the model is the only replaceable part.**"
- `## The two bets`: "**The harness is the product.**" (models converge; harness decides how much model you get) and "**Agents are both the builders and the users.**" — "Pick a language your agents can't ruin." "Mediocre-but-uniform beats brilliant-but-divergent."
- `## The eight lessons`: The Stack / The TUI / The Shell / The Architecture / The Inference / The Tools / The Loop / "Version everything the model sees" (`family@rev`, e.g. `edit@hl.3`).
- `## Doctrine`: "Structured truth, projected views."; "Own the whole mechanism or don't ship it."; "Speculation, then effect."; "Honesty over convenience."; "Data over code."; "Delete the weightless machinery."
- `## How to choose` (10 rules: write the call site first; name concepts; variance is data; read prior art; design by simulation; generalize the primitive; re-derive the requirement; prefer ladders to gates; lock it and delete losers; keep an honest ledger).
- `## Standards`: "**Performance is correctness.**", "**Clean cutover, always.**", "**Finished means running.**", "**Craft is visible.**"

**Explicit non-goals (stated as bans, not a heading)**
- No shims/legacy: "No shims, no deprecated aliases, no dual runtimes, no `PI_*` residue" (`## Standards`).
- No wrapping what should be native: shell is an interpreter "not a `/bin/sh -c` wrapper"; JSON repairer is a parser (`slopjson`), "not a regex pass in front of serde".
- No per-entity branching in code: "runtime code never inspects a model name"; no `if model_id.contains(...)`; quirks live in KDL/catalog rows.
- No scaffolding/speculative design: "No scaffolds, no 'rest is trivial', no speculative design reports in place of working code."
- No feature-list-over-product: "we added all this bullshit features and yet neither of these things are working as good as /work/pi out of the box" — out-of-box experience outranks checklists.
- No lock-in: "lets assume that we want to build such a great harness that no one will ever need any plugins at all" (own search/fetch, neutral interfaces).

**(2) Why TypeScript is dropped** — `## The two bets` + `## The eight lessons` §1 "The Stack" + `## Standards`
- "TypeScript hands every codebase its own dialect, Rust rejects the unsound version before it runs." Chosen by *idiom convergence*: agents average the training corpus, so uniform-and-mediocre beats divergent-and-brilliant.
- "**Performance is correctness.** 'ts is slow, rust isn't' — the rewrite exists to delete the throttles, GC workarounds, and UTF-16 defenses that compensated for the runtime, not to port them. 50 ms is a lot."
- Replacement for extensions: "Rust engine; embedded free-threaded Python for extensions (agents write decent Python, it embeds small, and it can introspect itself)" → `crates/py`.

**(3) Target architecture & surface scope** — `/tmp/oh-my-pi-omp2/README.md`
- `## Workspace layout`: all crates under `crates/*`, virtual workspace, resolver 3, packages `omp-` prefixed; grouped Core primitives (`core`, `ar`, `walker`, `edit`, `ast`), AI & Inference (`catalog`, `ai`), Services (`proto`, `rpc`, `observability`, `env`, `envd`, `serve`, `oauth`, `collab`, `memory`), Agent (`journal`, `dom`/`vocab`, `session`, `con`, `cache`, `tool`/`tools`, `agent`, `driver`, `app`, `e2e`, `ext`, `sdk`, `snapcompact`), Shell (`shell`, `shell-builtins`), Interface (`tui`, `chat`, `macros`, `gui`, `desktop`, `webview`, `py`, `audio`), Infrastructure (`secrets`, `sandbox`, `http`).
- `### 1. 5-Layer Crate Topology & Turn Flow`: strictly acyclic layers 1-5; single-writer journal-first loop: `omp-chat` → `Kernel` `flume::Sender<Up>` → `Session` appends `TurnStart` to `.oms` before folding into `omp-dom::Dom` → `CanonicalPromptSource` projects DOM to `omp-ai::Client` over Tower → streamed `ChatEvent` deltas with `slopjson` arg streaming + live `omp-edit` previews → tools run in `omp-envd`, big output spills to CAS (`artifact://sha256/<hex>`).
- `### 2. Operating System Processes`: frontend `omp [chat | print | rpc | acp]`; detached `omp envd` (idle timeout 60s, build-keyed socket); hidden same-binary children in `crates/app/src/main.rs`: `__omp-ext-host`, `__omp-py-worker`, `__omp-eval-child`, `__omp-shell-child`, `--omp-sandbox-child`; plus `omp serve`, `omp-sh`, `omp-memory-embedding-worker`. Two-stage `SIGTERM`→`SIGKILL` to `-pgid`; watchdog `getppid()` every 100ms.
- `### 3. IPC & Protocols` and `### 4. Cardinality & Authority Boundaries`: UDS/Protobuf for client↔envd, gRPC/Tonic for `omp serve`, JSON-RPC 2.0 stdio for IDE↔`omp rpc`, PTY for shells, `.oms` SSE frames for the journal; envd is N-1 per build, one writer per `.oms` (ADR 0004), Python worker ceiling 8.
- Surface: CLI + TUI + native GUI/webview + Python extension SDK + npm shim `npm/pi-coding-agent`; `just test` = nextest + doctests (nextest skips doctests).

**(4) Performance & quality invariants**
- Perf: 267 s → 90 ms per-session render; tool-roster sweep 86.2 s → 36.6 s; 1.3x forced-call cache bill; "Per-frame or per-token allocation is a bug regardless of profiler evidence"; "Measure, then argue."
- Rust rules (`## Rust, specifically`): memory ladder borrow → `SmallVec` → `[T; N]` → `Str` → rope → `Vec`/`String` last; "Fix the type, never box the lint away" + compile-time `size_of` asserts; "Errors are types, never strings" (`thiserror`, "NEVER USE FORMATTERS WITH ERROR TYPES"); derive both directions from one table; standard traits (`tower::Service`) over bespoke; nightly pinned deliberately; `flume` + `parking_lot`, single decoded mailbox per actor, structural cancellation; every dependency defends its seat.
- Quality gates: `PLAN.md` P0-P7 ledger, `just test`/`just e2e`, `AGENTS.md` mandatory conventions, clean cutover per change, "port knowledge and fixtures, not structure."

<results>
<files>
- /tmp/oh-my-pi-omp2/PHILOSOPHY.md - philosophy, two bets, eight lessons, doctrine, 10 decision rules, Rust taste, standards, method, lineage
- /tmp/oh-my-pi-omp2/README.md - workspace/crate map, 5-layer topology, turn flow, process tree, IPC table, cardinality table, build/test recipes
- /tmp/oh-my-pi-omp2/AGENTS.md - mandatory dependency/allocation/async/TUI rules referenced by README "Conventions" (not read in this pass)
- /tmp/oh-my-pi-omp2/PLAN.md - referenced P0-P7 verification gate ledger (path named in README, file not present in top-level listing)
- /tmp/oh-my-pi-omp2/crates/app/src/main.rs - hidden child entry-point dispatch for envd/ext-host/py-worker/eval-child/shell-child
- /tmp/oh-my-pi-omp2/crates/py - the free-threaded CPython embedding that replaces the TypeScript extension surface
- /tmp/oh-my-pi-omp2/npm/pi-coding-agent - npm shim retained as the only JS-adjacent surface
</files>

<answer>
omp2 is a ground-up Rust rewrite of the Pi fork with one thesis: "build a coding harness so good that the model is the only replaceable part." Two bets drive it - the harness (renderer, shell, document layer, inference spine) is the product, and agents are both builders and users, so language choice is made by idiom convergence. That is why TypeScript is dropped: it "hands every codebase its own dialect" while "Rust rejects the unsound version before it runs," and "ts is slow, rust isn't" - the rewrite exists to delete GC workarounds and UTF-16 defenses, not port them. Extensions move to embedded free-threaded Python (`crates/py`); JS survives only as an npm shim. Non-goals are stated as bans rather than a heading: no shims/deprecated aliases/dual runtimes/`PI_*` residue, no wrapping what should be native, no per-model branching in code, no scaffolds or speculative design reports, no lock-in via vendor tools. Architecture is a 5-layer acyclic crate workspace with a single-writer journal-first turn loop (`omp-chat` → `Kernel` → `.oms` journal → `omp-dom::Dom` → `omp-ai::Client` → `omp-envd` tools → `omp-tui`), one `envd` daemon per project build, hidden same-binary child processes for extension host, Python workers, eval, and shells, plus `omp serve` gRPC and `omp-sh`. Surfaces: CLI `omp [chat|print|rpc|acp]`, TUI, native GUI/webview, Python SDK, ACP/JSON-RPC for IDEs. Invariants: performance is correctness (50 ms is a lot; 267 s → 90 ms render), clean cutover with no dual paths, "finished means running," honest ledgers with losing benchmark rows shown, and typed errors/typed verdicts instead of model-facing strings.
</answer>

<next_steps>
If the parent needs the enforceable rules (allocation ladder, async/mailbox rules, TUI rendering bans), read /tmp/oh-my-pi-omp2/AGENTS.md (38 KB) - README defers to it as mandatory. If it needs the verification gates or current completion state, look for PLAN.md and .plan/qa/ (README names them; PLAN.md was absent from the top-level listing, so confirm the actual path before citing it). Ready to proceed otherwise.
</next_steps>
</results>