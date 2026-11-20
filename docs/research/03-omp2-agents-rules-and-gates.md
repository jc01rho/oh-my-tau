<analysis>
**Literal Request**: Read AGENTS.md + CONTRIBUTING.md; extract engineering rules, porting discipline, crate boundary rules, testing/QA gates, banned patterns, with section citations.
**Actual Need**: An enforceable rulebook to apply while writing/porting code in this repo, including the enforcement mechanism (which tool actually rejects a violation) so the caller can self-check before submitting.
**Success Looks Like**: Categorized rules, each citing file + section (and line where useful), plus the drift between docs and code that would otherwise mislead the caller.
</analysis>

Read both files, then traced every rule to its enforcement point (`clippy.toml`, `justfile`, `Cargo.toml` lints, `tools/lintx`, `.config/nextest.toml`, `.github/workflows/ci.yml`, `scripts/check-spec.rs`, `docs/architecture/crates.md`).

## Key finding first

`CONTRIBUTING.md` contains **zero** engineering rules. Its only section is `## Contribution licensing` (MIT inbound = outbound, no DCO, no CLA, third-party material must keep notices). Every rule you asked about lives in `/tmp/oh-my-pi-omp2/AGENTS.md` ("Repository Guidelines"), with rationale in `PHILOSOPHY.md` and crate ownership in `docs/architecture/crates.md`.

---

## 1. Porting discipline — "observable behavior, not TS shape"

`AGENTS.md` header (L1-6): "Rust rewrite of `pi`: port observable behavior, not TS shape."

`### Porting (from pi)` (AGENTS.md L441-456), 5 numbered rules:
1. **Read pi's impl in extreme detail first** — including `crates/natives`, compat shims, support detection, tests. "Missed behaviors (editor keys, paste/drag-drop, resize-settling, truncation) = user-reported bugs within hours."
2. **Copy pi's tests; drop TS-shaped compensations** — throttles, GC workarounds, UTF-16 defenses ("ts is slow, rust isn't"). "Port behavior, not shape: reimplement where the shape is wrong (mermaid, slopjson, brush parser); never wrap what should be native."
3. **Generalize while porting** — themed, charset-aware, prop-driven engine primitives, not one-example checkmarks.
4. **Match pi where good** (editor UX, telemetry, statusline semantics, alt-buffer resize); **exceed where weak** (renderer contract, error taxonomy, providers-as-data).
5. **Close pi's gaps** (missing builtins, slash-arg completion) while in the area.

`### Locked Deviations from pi` (AGENTS.md L458-500) — the hard inverse: `"pi does X" is NEVER an argument`; regressing to pi shape "= defect, not parity." Never port back:
- Extensions/eval: embedded free-threaded CPython only; no JS/TS plugin runtime, no multi-language eval.
- Shell: in-process bash parser/interpreter + builtin coreutils; **NEVER** shell out to `/bin/bash` or resolve via `$PATH`.
- File edits: envd document authority (versioned CAS + fuzzy 3-way rebase), never pi's direct disk read/write.
- Tools: minimal fixed wire roster; MCP/optional capabilities ride `dyn` builtin devices; **NEVER** pi's discoverable `loadMode`/dynamic schema mutation. Versioned `name@rev` identities, single-stream lifecycle (ArgFeed → speculative preview → commit → typed verdict), never renderCall/execute/renderResult callbacks. Every tool schema carries the `i` intent param.
- Inference: providers-as-data (catalog/KDL); typed serde/prost wire structs — no `json!`/untyped `Value` traversal; provider frames decode to canonical semantic events; no vendor server-side tools.
- Prompts: scribe templates with Frozen/Stable/Dynamic/Volatile bands; volatile facts (date, cwd, mounts) **NEVER** in a stable prefix.
- Control plane: agent loop is a generic hook surface; hardcoded per-feature outcome tracking (TTSR-style) prohibited.
- Runtime: tokio + rayon only (custom executor crates prohibited); local audio/ML via candle, never C/C++ binding graphs.

Supporting doctrine in `PHILOSOPHY.md`: `## Doctrine` (L78) "Own the whole mechanism or don't ship it"; `## Standards` (L259) "Clean cutover, always… port knowledge and fixtures, not structure"; `## Lineage` (L303) "Port behavior, not shape; match Pi where it is good, exceed it where the contract was the disease."

## 2. Crate boundary rules

`## Architecture` (AGENTS.md L8-67) plus `docs/architecture/crates.md` `## Ownership rules` (L71-79):

1. **Driver composes; app presents.** `crates/driver` is the reusable DI boundary (`compose_kernel` = production construction boundary); `crates/app` adapts composition to commands/presentation.
2. **`crates/driver` + `crates/app` = the production stack; other libraries NEVER build a second production stack** (AGENTS.md L16-17, L162-166; crates.md L63-69 "never assemble a competing agent/session stack").
3. `crates/app` "NEVER owns environment-host, extension-host, or Python-worker internals" (L165).
4. **Envd owns host resources + policy enforcement; `omp-env` is only its typed client and "owns no host resources"** (L39-44; crates.md L76). "Host changes go to `omp-envd`; client protocol APIs go to `omp-env`."
5. **`omp-tool` defines contracts, `omp-tools` implements — never inverted** (L137-138; crates.md L78).
6. **Catalog owns compatibility facts; inference owns translation and correction** (crates.md L77).
7. **Session is the only session-state authority**: appends to journal first, folds the exact returned entry; replay uses the same fold; `omp-chat` is an actor over `Session::subscribe()` and "never owns controller state" (L27-30; crates.md L73-75).
8. **Transport crates project canonical state; they do not invent a second dialect or store** (crates.md L79; L135-137 "transport = provider wire protocol ≠ dialect = thread rendering to the LLM; NEVER conflate").
9. Public daemon commands and same-binary child roles are dispatched by app, but daemon implementation belongs in its host crate (`omp-envd`), never in app presentation internals (L138-141).
10. `crates/tui`/`chat`/`gui` "None owns agent/provider policy" (L48-50).
11. Hard dependency gate: `[workspace.metadata.omp.dependency-lints.omp-agent]` in root `Cargo.toml` L319-323 — `allowed-world-boundary = ["omp-env"]`, `denied-direct = ["omp-envd", "omp-shell", "omp-walker"]`, enforced by `scripts/check-spec.rs` (`check_agent_dependencies`, L296-310, run as the `runtime_spec` CI job). Comment cites "D2: the agent reaches the world through omp-env; implementation crates stay behind env/v1."
12. Crate naming/packaging (L119-134): members `crates/*`, virtual workspace, resolver 3; dirs unprefixed (`crates/demo`); package names `omp-` prefixed; every member needs real `description`, workspace `license`/`authors`/`homepage`/`repository`, a README, and inherits `version/edition/lints` from workspace. Taxonomy: domain prefix after `omp-` (`omp-llm-*`, `omp-shell*`).

## 3. Engineering rules (Conventions, AGENTS.md L83-185)

- **Deps**: all versions in root `[workspace.dependencies]`; members use `{ workspace = true }`, **NEVER pin versions**. `serde_json` always `preserve_order` + `raw_value`.
- **Env vars**: `OMP_*`, never `PI_*`; ported code strips upstream env vars/context/branding — "never aliases."
- **Pre-release cutover**: rename+move (don't copy); compat shims, old names, deprecated aliases **PROHIBITED**; update every caller + remove obsolete exports/tests in the same change.
- **Unicode/ANSI**: `xutf` for ALL UTF-8/16/32, grapheme, display-width, normalization, ANSI/VT. `unicode-normalization` banned; NEVER add `unicode-*`/`utf8-*`/`unicode-segmentation`/`unicode-width`/`ansi_*`/`strip-ansi-escapes`; remove redundant deps rather than wrap.
- **Hashing**: exactly two primitives in `omp-core` — `Hash32` (SHA-256) for content/crypto digests, `{FastHashMap, FastHashSet, FastState, fast_hash64}` (foldhash) for discretionary in-memory hashing. NEVER add `blake3`/`xxhash-*`/`rustc-hash`/`ahash`/`fnv`/`twox-*` for discretionary use. Exceptions = externally fixed algorithms only (git SHA-1, archive CRCs, PKCE/SigV4/JWT, MD5 identicons, SSH fingerprints, pi/Bun behavioral compat, extension trust dual-hash `b3:`+`sha256:`).
- **Style**: pinned nightly, edition 2024; lints in root `[workspace.lints.*]`; `#[allow]` requires `reason`; `cargo fmt` hard tabs / 3-col / width 100 — **NEVER hand-format**.
- **Enum↔string**: hand-written `match self { … => "…" }` tables under any name (`name()`, `as_str()`, `label()`, `Display`) **PROHIBITED, including private enums** → derived strum (`IntoStaticStr`/`Display`/`EnumString`, `serialize_all`, `const_into_str`). Only escape hatch when strum can't express the shape: a local `macro_rules!` emitting both directions from one table (`vocab!`, `crates/observability/src/semconv.rs`). "New bare match table = reviewer-reject; migrate on touch."
- **Errors**: library errors `thiserror`, every variant `#[error("…")]`; hand-written `impl Display`/`impl Error` = reviewer-reject. **Errors NEVER pass through formatters**: string-payload variants, `sf!("…: {error}")`, `format!`, `{error:?}`, `.to_string()` = reviewer-reject → carry typed inner error via `#[source]`/`#[from]` + named fields, render once at the app/miette boundary. App orchestration uses `miette`; classify/redact untrusted provider diagnostics before stderr.
- **Durable state**: append-only transcript journal + blob store; turn state = `AgentSnapshot` + journal projection; **NEVER a parallel mutable source of truth**.
- **Loops**: one `flume` mailbox; priority lifecycle via `tokio::watch`; ownership/cancellation explicit.
- **Docs**: every public symbol documented (`missing_docs = "warn"` in `Cargo.toml` L330).

### Allocation Discipline (CRITICAL, L187-256)
Prefer `&T`/`&str`/`&[T]`; think twice before `String`/`Vec`. Decision table: small/hot/short-lived → `SmallVec` (2.0-alpha two-const-param syntax, L202-210: `SmallVec::<StateEntry, 8>::new()`); hard bound → `[T; N]`; concurrent append log → `AppendVec`; built-once-moved-once → `Vec`; cloned repeatedly → `im::Vector` (only if consumers don't flatten back); strings → `omp_core::Str` (inline ≤23 bytes, heap `Bytes`-backed) built via `StrMut`+`freeze()`/`fmts!`; shared/sliced bytes → `CowBytes`; enum/small-int keys → `SparseMap`/`SparseSet`; clone-heavy maps → `im::HashMap`/`im::OrdMap`. Encoding: `omp_core::encoding` (`hex`/`base64`/`base32`) — "External encoding crates banned outright — no exception." `sf!` three formatting-arm bans (statically-known text; immediate copy into a sink; per-frame/per-key/per-line/per-cell paths). Branded ids: `Id` owned / `Id<str>` borrowed query form; query fns take `&Id<str>`, NEVER `impl AsRef<Id<str>>`; new id newtypes MUST use `string_id!`.

### Type Size Discipline (CRITICAL, L257-288)
`result_large_err|large_enum_variant|large_stack_arrays|large_futures` = measurement, not a request for a pointer. **Boxing to silence a size lint PROHIBITED, reviewer-reject** — `Err(Box<MyError>)`, `Variant(Box<Payload>)`, `Box<SmallVec>`, `field: Box<Struct>`, box-only wrappers, `#[allow(clippy::result_large_err, …)]`. Fix the type: `size_of` → find fat field → shrink (`Arc<[T]>`, `Box<[T]>`, run structs, Arc-backed handle). One exception: foreign fat types (prost message, provider payload) MAY box that one field with a comment. Pin the win with `const _: () = assert!(size_of::<T>() <= N, "…");`.

### Async/Iterator/Codegen Discipline (CRITICAL, L289-398)
Nightly gates are the point: `impl_trait_in_assoc_type` + `type_alias_impl_trait`, `min_specialization`, `const_eval_select`/`core_intrinsics`; each crate MUST gate exactly what it uses, and again in every integration test/example. Async traits unboxed, no per-call allocation — RPITIT when callers never name the future, GAT `type Future<'c>` when nameable. `#[async_trait]`, `BoxFuture`, per-call `Box::pin` quarantined to cold `dyn` I/O boundaries only; per message/frame/token/byte PROHIBITED. Enums before `dyn`. Iterators lazy/borrowed/unboxed, `-> impl Iterator<Item = …> + '_` declaring all capabilities; NEVER `.collect()` an intermediate Vec; `Box<dyn Iterator>` same quarantine as `BoxFuture`. Tower stacks compose at construction; `poll_ready` → `call` MUST run on the SAME instance. Scratch buffers: `BytesMut` clear-vs-`split().freeze()`. **Locks: `parking_lot`, never `std::sync`; `tokio::sync::Mutex` only across `.await`. Channels: `flume`, never `tokio::sync::mpsc`/`std::sync::mpsc`.**

### TUI Rendering Doctrine (CRITICAL, L399-440)
Text parsed ONCE at the boundary (ANSI decomposed via `xutf`), components store zero escapes, ANSI re-emitted exactly once at final materialization. Per-frame `Vec<Line>` buffers = bug. TML degrades like HTML (unknown tag → `CustomElement`, must not fail to raw-text). Props inherit like CSS; colors accept hex/rgb/hsl/lab/oklch/HTML names/gradients. `UiContext` (charset + theme) reaches every component; hardcoded colors + hand-emitted glyphs banned; icons from `icons.tsv`. `dom!`/`layout!` canonical; `write!`/`format!`→`String`→reparse discouraged. Effects are reusable props in core FIRST, never example-local. Examples near-zero boilerplate — example touching kitty image ids/escape dispatch/terminal probing/focus routing/clipboard internals means the engine is missing a primitive. Input = one mailbox of decoded `TerminalEvent`s; resize via watch + `select!`; no polling `read()` loops.

## 4. Banned patterns — consolidated, with the enforcer

| Banned | Enforcer |
|---|---|
| `std::sync::Mutex`/`MutexGuard`, `tokio::sync::mpsc::*` (all 13 types incl. `Permit`, `WeakSender`, error types) | `clippy.toml` `disallowed-types`/`disallowed-methods`; `disallowed_types/methods = "deny"` in `Cargo.toml` L342-343 |
| `tokio::task::block_in_place` | `clippy.toml` |
| `anyhow::anyhow`/`bail`/`ensure`, `anyhow::Error` | `clippy.toml` (use thiserror + miette) |
| `base64::Engine::{encode,decode}`, `glob::glob`/`Pattern`, `chrono::*`, `reqwest::Client`, `secrecy::*`, `smol_str::SmolStr`, `ulid::Ulid`, `cached::proc_macro::cached` | `clippy.toml` `allow-invalid` entries (path resolves only if the removed dep sneaks back) |
| `unicode-*`/`utf8-*`/`strip-ansi-escapes`/encoding crates | AGENTS.md L96-101, L231 + code review |
| `blake3`/`xxhash-*`/`rustc-hash`/`ahash`/`fnv`/`twox-*` for discretionary hashing | AGENTS.md L107 |
| Hand-written enum↔string match tables (incl. private) | AGENTS.md L148-159, reviewer-reject |
| Hand-written `impl Display`/`impl Error` on errors; stringified/format-through errors | AGENTS.md L167-176, reviewer-reject |
| Boxing to silence size lints | AGENTS.md L260-266, reviewer-reject |
| `#[async_trait]`, `BoxFuture`, per-call `Box::pin` on hot paths | AGENTS.md L320-324 |
| `model_id.contains(…)`, `starts_with`, `match_indices`, `eq_ignore_ascii_case`, `strip_prefix`, `==`/`!=` on model-id literals, or arrays of ≥3 model-like ids, in `crates/inference/src` | `tools/lintx` `model-gate` / `model-table` (`tools/lintx/src/lints/model_name.rs` L1-41; run by `just lintx`) |
| Inline `std::module::item` / `tokio::module::item` (except `tokio::fs`) / `crate::`/`super::`/`self::` paths, >2 `::` segments, CamelCase `use … as Alias`, structs where most fields are `Arc`, `Mutex<Arc<…>>` | `tools/lintx` `long-path`/`std-path`/`tokio-path`/`relative-path`/`import-alias`/`arc-struct`/`mutex-arc` (`tools/lintx/src/main.rs` L4-12) |
| Lock-wrapped map/set state (`Mutex<HashMap<…>>` etc.) | `just lint-locked-maps` (warn only) |
| `PI_*` env vars, compat shims, deprecated aliases, dual catalog formats, leftover port fixtures, unread lockfiles, unused feature edges | AGENTS.md L91-95, L499-500 |
| Custom executor crates; C/C++ binding graphs (whisper-rs, llama-cpp) | AGENTS.md L494-496 |
| Tool-local string truncation; vendor server-side tools; dynamic tool schema mutation | AGENTS.md L472-486 |

## 5. Testing & QA gates

`## Testing & QA` (AGENTS.md L618-639):
- Unit tests colocated in `src` where private behavior matters; public contracts + cross-module → `crates/*/tests`.
- `insta` snapshots for shell parser/tokenizer; `proptest` for encoding, zero-copy slicing, transcript replay, round-trip invariants. "Review snapshots; NEVER accept blindly."
- **Test at the owning seam**: `crates/envd` (environment/extension-host/Python-worker), `crates/env` (client/protocol), `crates/driver` (headless/session composition), `crates/app` (CLI/presentation/protocol adapters). "Prefer these seams over mocks of production authority."
- `crates/e2e/tests/p1_doc_race.rs`…`p8_baselines.rs` authoritative for concurrency, cancellation, detached jobs, schema isolation, prefix stability, crash/replay, real-PTY lifecycle, recorded perf; "Bounded waits + RAII-owned processes; preserve both."
- **P8 = non-gating recorder** — "NEVER turn noisy host measurements into an unreviewed hard gate."
- **TUI changes MUST be exercised on a real PTY** via `.omp/tools/tui.ts` (or `OMP_TUI_DEBUG`/`OMP_TTY` hooks): input, resize, clean quit restoration.
- **No numeric coverage target.** Coverage = changed observable behavior defended: branch edges, precedence, state transitions, malformed input, cancellation, recovery. Narrow test → affected crate → relevant E2E proof.

Command gates (`## Commands` L69-82 + `justfile` + CI):
- `justfile` is the source of truth — **use `just`, not raw cargo**. `just --list`.
- `just setup-python` once before anything linking `omp-py`.
- Iterate narrow (`just check-pkg <pkg>`, `just test-pkg <pkg>`), broaden (`check`, `test`, `lint`) after the changed contract passes.
- `just test` = `cargo nextest run --workspace --exclude omp-e2e` **plus** `cargo test --doc …` — nextest does not run doctests and omp has them in 25+ modules; "Adding a nextest call without the doctest half silently drops that coverage" (AGENTS.md L531-536; `.config/nextest.toml` header).
- `just lint` = `fmt-check clippy proto-lint lint-locked-maps`; `just ci` ≈ CI format+rust jobs locally.
- E2E separate/expensive: `just e2e` (`e2e-build|e2e-core|e2e-p7|e2e-p8|e2e-baseline`, plus `e2e-p9`, `e2e-p10`).
- CI (`.github/workflows/ci.yml`): `format` (ubuntu), `runtime_spec` (`cargo -Zscript scripts/check-spec.rs`), `licenses` (`just license-check` = `cargo deny check licenses sources`), then `rust` on **macos-15 arm64** (clippy --workspace, nextest --profile ci --workspace excluding omp-e2e, doctests, `scripts/check-python-examples.py`, P1-P6, P7, P8 contract) and `p7_linux` (ubuntu PTY P7).
- nextest policy (`.config/nextest.toml`): `fail-fast = false`, `retries = 0` in CI ("a flaky acceptance proof is a bug we want to see, not paper over"), slow-timeout 60s/terminate 4; `omp-e2e` gets `threads-required = "num-cpus"`, 120s/5.
- "Done = compiles, wired, exercised" (`### Working Style` L501-512); "Verify by running: TUI changes get real-PTY proof before claiming done." Also: "NEVER revert/`git checkout` user edits."

## 6. Doc-vs-code drift you should know about

1. **`crates/inference` does not exist.** `tools/lintx` `model_name.rs` L41 scopes to `crates/inference/src`, and AGENTS.md L45/L484 says "routing stays in ai" / "lintx-enforced" — but the actual crate is `crates/ai` (`omp-ai`). The `model-gate`/`model-table` lint is currently scoped to a nonexistent directory, so it cannot fire.
2. **Dependency-lint lists disagree.** `Cargo.toml` L319-323: allowed `["omp-env"]`, denied `["omp-envd", "omp-shell", "omp-walker"]`. `scripts/check-spec.rs` L23-25: allowed `["omp-env", "omp-storage"]`, denied `["omp-docserver", "omp-shell", "omp-walker"]`. The CI `runtime_spec` job cross-checks these two lists (`check_policy_list`), so this is a live failure risk.
3. **Proof count drift.** AGENTS.md L50/L629 says "P1-P8"; `crates/e2e/README.md` L13-25 and `justfile` define P1-P10 + `tool_sources` (P9 splits into `p9_isolation.rs` and `p9_extension_control.rs`).
4. **`.plan/` is absent** from this checkout, yet AGENTS.md L52-56 and L460 cite `.plan/quirks`, `.plan/parity-regression-audit.md` as authority-bearing. `PLAN.md` (cited L51-53 as "authoritative plan — locked decisions D1-D8") is also missing.

<results>
<files>
- /tmp/oh-my-pi-omp2/AGENTS.md - the entire rulebook: Architecture, Commands, Conventions, three CRITICAL discipline sections, TUI doctrine, Porting, Locked Deviations, Testing & QA
- /tmp/oh-my-pi-omp2/CONTRIBUTING.md - licensing only (MIT inbound=outbound, no DCO/CLA, third-party notices); contains no engineering rules
- /tmp/oh-my-pi-omp2/PHILOSOPHY.md - rationale behind every AGENTS.md rule; `## Doctrine`, `## How to choose` (10-step decision procedure), `## Rust, specifically`, `## Standards`
- /tmp/oh-my-pi-omp2/docs/architecture/crates.md - canonical crate responsibilities + 7 numbered Ownership rules
- /tmp/oh-my-pi-omp2/clippy.toml - machine-enforced bans: disallowed-methods/macros/types (std::sync::Mutex, tokio mpsc, anyhow, base64, chrono, reqwest, smol_str, ulid, secrecy, glob, cached)
- /tmp/oh-my-pi-omp2/Cargo.toml - `[workspace.lints.*]` (missing_docs warn, disallowed_types/methods deny, allow_attributes_without_reason), `[workspace.metadata.omp.dependency-lints.omp-agent]`, profiles
- /tmp/oh-my-pi-omp2/justfile - source-of-truth commands: check/test/lint/ci/e2e/lintx/license-check
- /tmp/oh-my-pi-omp2/.config/nextest.toml - retries=0 in CI, fail-fast=false, slow-timeout, omp-e2e num-cpus override, doctest caveat
- /tmp/oh-my-pi-omp2/.github/workflows/ci.yml - the authoritative gate: format, runtime_spec, licenses, rust (macos-15 arm64), p7_linux
- /tmp/oh-my-pi-omp2/tools/lintx/src/main.rs - custom lint engine rule list (path/style/arc/mutex/model rules)
- /tmp/oh-my-pi-omp2/tools/lintx/src/lints/model_name.rs - model-gate/model-table definition; scoped to `crates/inference/src` which does not exist
- /tmp/oh-my-pi-omp2/scripts/check-spec.rs - runtime symbol + Python spec + omp-agent dependency-policy checks (CI `runtime_spec` job)
- /tmp/oh-my-pi-omp2/crates/e2e/README.md - harness contract and P1-P10 proof list
- /tmp/oh-my-pi-omp2/deny.toml - license allowlist and source policy behind `just license-check`
- /tmp/oh-my-pi-omp2/rustfmt.toml - hard tabs, tab_spaces=3, max_width=100, edition 2024
- /tmp/oh-my-pi-omp2/rust-toolchain.toml - pinned `nightly-2026-08-08`
- /tmp/oh-my-pi-omp2/crates/core/src/str.rs - `Str` inline cap = 23 bytes, the concrete basis of the string-allocation rule
</files>

<answer>
CONTRIBUTING.md is a licensing document only; the engineering contract you are asking about is AGENTS.md. The five things you asked for, in one line each:

PORTING: read pi's impl exhaustively first, copy its tests, port behavior and drop TS-shaped compensations (throttles, GC/UTF-16 workarounds), reimplement rather than wrap where pi's shape is wrong. "pi does X" is never an argument for anything in the Locked Deviations list — those are owner decisions where regressing to pi shape is a defect, not parity.

CRATE BOUNDARIES: driver composes, app presents; driver+app are the only production stack and no library may build a second one; envd owns host resources while omp-env is only its typed client; omp-tool declares contracts and omp-tools implements; session is the sole state authority (journal-first, fold-exact); catalog owns compat facts, inference owns translation; transport crates project, never invent a dialect. omp-agent's world edge is gated to omp-env with omp-envd/omp-shell/omp-walker denied direct.

ENGINEERING: workspace-pinned deps, OMP_* only, clean cutovers with no shims/aliases, xutf for all Unicode/ANSI, two omp-core hashing primitives, rustfmt-only formatting, strum-derived enum<->string, thiserror+miette with typed sources and no formatter-passed errors, one flume mailbox per loop, parking_lot locks.

BANNED (all reviewer-reject or lint-enforced): std::sync locks, tokio mpsc, anyhow, base64/chrono/reqwest/smol_str/ulid/secrecy/glob/cached direct use, unicode/encoding/hasher crates, hand-written enum<->string tables, hand-written Display/Error impls, boxing to silence size lints, async_trait/BoxFuture on hot paths, model-name conditionals and model-id arrays in inference, inline qualified paths, PI_* residue, custom executors, C/C++ binding graphs.

QA GATES: `just check-pkg`/`test-pkg` while iterating then `just check|test|lint`; `just test` always pairs nextest with `cargo test --doc`; TUI work needs real-PTY proof via `.omp/tools/tui.ts`; e2e P1-P10 are authoritative with bounded waits and RAII ownership, P8 non-gating; CI = format + runtime_spec + licenses on Linux, clippy/nextest/doctests/P1-P8 on macos-15 arm64, plus Linux PTY P7; no numeric coverage target — cover changed observable behavior.

Caveat worth acting on: three live inconsistencies exist — the lintx model-name rules are scoped to `crates/inference/src` which is not in this tree (the crate is `crates/ai`), the omp-agent dependency lists in Cargo.toml disagree with scripts/check-spec.rs (which CI cross-checks), and AGENTS.md says P1-P8 while the e2e README/justfile define P1-P10.
</answer>

<next_steps>
If you are about to write code: run `just check-pkg <crate>` then `just lint` (fmt-check + clippy + proto-lint + lint-locked-maps) and `just test-pkg <crate>`; add `just lintx crates` before submitting since it is not part of `just lint`.

If you are auditing or fixing the repo: reconcile the `crates/inference` -> `crates/ai` lintx scope, and align `[workspace.metadata.omp.dependency-lints.omp-agent]` with `scripts/check-spec.rs` (the `runtime_spec` CI job compares them, so a mismatch fails CI).

If you need the reasoning behind any single rule, PHILOSOPHY.md cites the dated prompt-history quote for each; `docs/adr/0001`-`0036` hold the per-decision records.
</next_steps>
</results>