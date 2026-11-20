<analysis>
**Literal Request**: Inspect four package dirs (Cargo.toml, src, LOC, features) and explain each one's purpose plus whether it mirrors omp or is tau-specific.
**Actual Need**: Provenance/origin classification per package so the caller knows which code is upstream-derived vs. tau-original when auditing licensing, porting fidelity, or feature scope.
**Success Looks Like**: Per-package table: absolute paths, LOC, deps/features, one-line purpose, and an explicit omp-mirror vs tau-specific verdict with evidence.
</analysis>

Read-only inspection done. Findings below; note the four packages split cleanly into "third-party upstream + tau patch" (crossterm) and "tau-specific, no omp counterpart" (the other three).

## 1. `/home/whrho/git/oh-my-tau/vendor/crossterm`

| | |
|---|---|
| Version | `crossterm` 0.28.1, MIT, edition 2021, rust-version 1.63.0 |
| Size | 55 `.rs` files, **11,529 LOC** (+ examples, docs, CHANGELOG 36KB) |
| Provenance | `.cargo_vcs_info.json` → upstream sha `492b764f9f132d302903ff6f2f55e96c1b8cd0ec`; `Cargo.toml.orig` is the pristine upstream manifest; `NOTICE.md` calls it "fork of crossterm 0.28" |
| Wiring | **Not** a workspace member. Injected via `[patch.crates-io] crossterm = { path = "vendor/crossterm" }` (`/home/whrho/git/oh-my-tau/Cargo.toml:140-145`) |

Features (upstream set, unchanged): `default = ["bracketed-paste", "windows", "events"]`, plus `event-stream` (→ futures-core), `serde`, `use-dev-tty` (→ filedescriptor, rustix/process), `windows` (winapi + crossterm_winapi). Only consumer: `crates/omp-tui` (`crossterm = { version = "0.28", features = ["event-stream"] }`, alongside ratatui 0.29).

The **only** tau delta found: xterm `modifyOtherKeys` decoding, `vendor/crossterm/src/event/sys/unix/parse.rs:642` — a `27 => KeyCode::Char(code)` arm parsing `CSI 27 ; modifiers ; codepoint ~`. The root manifest comment states why: "adds xterm modifyOtherKeys ... that upstream lacks — needed so ctrl+shift+v works when only the modifyOtherKeys fallback (no kitty keyboard protocol) is engaged (TS keys.ts parity)."

Verdict: **upstream third-party mirror + one tau-specific patch**. The patch is derived from omp's TypeScript `keys.ts` behavior, so the fork exists *for* omp parity, but the crate itself is not an omp port.

## 2. `/home/whrho/git/oh-my-tau/vendor/laya-ort`

| | |
|---|---|
| Size | **537 LOC** (`src/lib.rs` 416, `src/main.rs` 121) — 2 files only |
| Manifest | `publish = false`, own version 0.1.0, MIT; description: "ONNX-runtime inference for the Laya judge (replaces the Python laya_predict daemon)" |
| Deps | `ort 2.0.0-rc.11` (feature `download-binaries`), `tokenizers 0.21`, `serde_json` (preserve_order), `anyhow`, `rayon 1.10`. **No cargo features.** |
| Targets | lib `laya_ort` + bin `laya-judge` |

Purpose: CPU ONNX Runtime inference for the Laya turn judge. `lib.rs` documents that it "Reimplements laya.common.build_sequence / render_options with the `tokenizers` crate so token ids match the Python fast tokenizer exactly, and applies the per-bucket temperature from rl_agent_config.json (same as laya-train-v2/train.py)". Public API: `render_criterion`, `render_options`, `LayaTokenizer`, `Sequence`, `temp_bucket`, `softmax_with_temp`, `Temperatures`, `Judge::{load,judge}` (batched, chunked by an 8192-token budget, feeding ONNX inputs `input_ids`/`attention_mask`/`marker_pos`/`marker_mask`/`qtype` → `logits`). `#![forbid(unsafe_code)]`. The `laya-judge` binary is a JSONL-in/JSONL-out CLI whose output schema matches the Python `laya_predict.py` reference; defaults to `$OMP_JUDGE_CKPT` or `$HOME/.omp-rs/judge/tj/ckpt`.

Verdict: **tau-specific**. Nothing in omp corresponds to it — the reference implementation is Python (`laya-train-v2`), not the omp TypeScript agent.

## 3. `/home/whrho/git/oh-my-tau/crates/omp-judge`

| | |
|---|---|
| Size | **1,700 LOC** across 10 `.rs` files + 2 embedded JSON question sets + a 1.2MB golden fixture |
| Deps | `anyhow`, `serde_json` (workspace), `tracing`, `regex 1`, `omp-session`, `laya-ort` (optional) |
| Features | `default = ["onnx"]`, `onnx = ["dep:laya-ort"]` — comment: off for targets without prebuilt ONNX Runtime (musl), where loading a head errors cleanly |
| Lints | `[lints] workspace = true` (so it inherits `unsafe_code = "forbid"` + the pedantic clippy gate) |

Largest files: `episode.rs` 550, `shadow.rs` 385, `view.rs` 180, `pyfmt.rs` 148, `offline.rs` 128, `scorer.rs` 123, `policy.rs` 73, `onnx.rs` 41, `lib.rs` 18.

What it does (from `lib.rs` doc + module heads):
- `episode` — session entries → episodes, "port of `build_traj_outcome.py`" (one real user message → steps until the next real user message), producing the flat `state` string and objective counters.
- `view` — the judge input, "port of `judge_view.view_sel` (byte-identical)"; `DEFAULT_BUDGET = 2600`, emits `REQUEST / FACTS / FINAL REPLY / SELECTED STEPS`. Verified by `tests/golden.rs::view_sel_matches_python_on_50_episodes` against `tests/fixtures/golden_view_sel.jsonl` (50 synthetic episodes), plus an `#[ignore]`d whole-family check via `OMP_JUDGE_GOLDEN_ALL`.
- `policy` — context-policy head `noop | clm_edit | rlm_offload` over the same ONNX runtime, reading `policy_questions.json`.
- `scorer` — ONNX inference through `laya-ort`; defaults to `$OMP_JUDGE_CKPT` / `$HOME/.omp-rs/judge/tj_sel/ckpt`, threads 2.
- `shadow` — env-gated background worker (`OMP_LAYA_JUDGE=1`, default **off**), bounded `sync_channel` queue of 8 with drop-and-count on overflow, lazy one-time model load on the worker thread, appends JSON lines to `~/.omp/agent/laya_judge.jsonl`; never blocks or fails a turn.
- `offline` — same view builder + scorer applied to historical session `.jsonl` files.
- `onnx` — the feature seam: re-exports `laya_ort::Judge` with `onnx`, else a stub whose `load` bails "this omt build has no ONNX Runtime".

Consumers: `crates/omp-cli` (`default-features = false`, re-enabled by its own `onnx` feature), used at `context_policy.rs:154/284`, `runtime.rs:931-939` (shadow hook), `subcmd/judge_sessions.rs`, `subcmd/stats_judge.rs`.

Verdict: **tau-specific**. It is a Rust port of a Python ML-training repo, not of omp — the only omp-adjacent parts are that it consumes `omp-session` entries and writes into the omp agent dir (`~/.omp/agent/laya_judge.jsonl`) so the two coexist on the same agent directory.

## 4. `/home/whrho/git/oh-my-tau/crates/omp-goose`

| | |
|---|---|
| Size | **145 LOC** Rust (`src/lib.rs` 133, `build.rs` 12) + `goose/omt_dedup.goose` (~200 LOC) + `generated/omt_dedup.c` **4,398 lines** / `.h` 47 lines |
| Deps | build-dependency only: `cc = "1"`. No runtime deps. |
| Features | **none** — instead `[lints.rust] unsafe_code = "allow"`, deliberately kept as its own crate "so the isolated `unsafe` FFI region stays out of the `omp-cli` workspace's `forbid`, exactly like `omp-sandbox`" |
| Wiring | `crates/omp-cli`: `omp-goose = { path = "../omp-goose", optional = true }`, feature `goose = ["dep:omp-goose"]`, **off by default** |

Purpose: FFI wrapper for a dedup kernel written in **goose** (a memory-safe systems language with no heap allocator), generated to C and compiled by `build.rs` via `cc` — so building omt does not require the goose toolchain (`regenerate.sh` documents regeneration). The kernel implements SipHash-1-3 (zero keys, `0xff` str separator, c=1/d=3) over decoded text with an open-addressing first-seen table, exposed as `omt_goose_dedup_classify(n, text_len, text_off, skip, texts, out_kind, out_target)`. The Rust side does one `unsafe extern "C"` call per batch with caller-owned buffers (no memory crosses the boundary) and a `Once`-guarded `goose_init` (goose runtime is single-threaded, matching omt's current_thread tokio runtime). Its documented contract: **byte-identical to the `DefaultHasher`-based reference in `crates/omp-cli/src/dedup.rs`**, which the `goose` feature swaps in via `plan_stubs` vs `plan_stubs_reference` (`dedup.rs:91-108`), with an equivalence test alongside.

Verdict: **tau-specific experiment**. It mirrors tau's own pure-Rust `dedup.rs` (itself a tau cost lever, `COST-CACHE-HYGIENE-20261014` — wire-only collapsing of repeated tool results, ~65% of billed input bytes measured on the bench mock), not anything from omp.

## Cross-cutting

- Workspace `members` (`Cargo.toml:3-16`) lists `vendor/laya-ort` explicitly and `crates/*`; `vendor/crossterm` is excluded from membership and enters only through `[patch.crates-io]`.
- `NOTICE.md` license table confirms the split: `crossterm` = "fork of crossterm 0.28", `laya-ort` = "part of this project"; `omp-judge`/`omp-goose` are first-party crates under the repo's MIT `LICENSE`.
- Neither `docs/` nor `README.md` documents Laya or goose; the only non-code trail is `bench/context/README.md` (`OMP_CONTEXT_POLICY=laya`, `OMP_CONTEXT_LAYA_{CKPT,ONNX,THREADS}`).

<results>
<files>
- /home/whrho/git/oh-my-tau/vendor/crossterm/Cargo.toml - upstream crossterm 0.28.1 manifest (auto-generated); features default/bracketed-paste/events/windows/event-stream/serde/use-dev-tty
- /home/whrho/git/oh-my-tau/vendor/crossterm/Cargo.toml.orig - pristine upstream manifest, proof the crate is an upstream copy
- /home/whrho/git/oh-my-tau/vendor/crossterm/.cargo_vcs_info.json - upstream git sha 492b764f, confirms crates.io provenance
- /home/whrho/git/oh-my-tau/vendor/crossterm/src/event/sys/unix/parse.rs - line 642 is the sole tau patch: xterm modifyOtherKeys (CSI 27;mods;code~) decoding
- /home/whrho/git/oh-my-tau/Cargo.toml - workspace members (vendor/laya-ort, crates/*) and [patch.crates-io] crossterm override at lines 140-145
- /home/whrho/git/oh-my-tau/vendor/laya-ort/Cargo.toml - publish=false, ort 2.0.0-rc.11 + tokenizers 0.21 + rayon; lib laya_ort + bin laya-judge; no features
- /home/whrho/git/oh-my-tau/vendor/laya-ort/src/lib.rs - Judge/LayaTokenizer/temperature-bucket ONNX inference (416 LOC), Python tokenizer parity
- /home/whrho/git/oh-my-tau/vendor/laya-ort/src/main.rs - laya-judge JSONL CLI replacing the Python laya_predict daemon (121 LOC)
- /home/whrho/git/oh-my-tau/crates/omp-judge/Cargo.toml - features default=["onnx"], onnx=["dep:laya-ort"]; deps omp-session + optional laya-ort
- /home/whrho/git/oh-my-tau/crates/omp-judge/src/lib.rs - module map: episode/view/policy/scorer/shadow/offline/onnx
- /home/whrho/git/oh-my-tau/crates/omp-judge/src/view.rs - byte-identical port of Python judge_view.view_sel (DEFAULT_BUDGET 2600)
- /home/whrho/git/oh-my-tau/crates/omp-judge/src/episode.rs - port of build_traj_outcome.py, largest module at 550 LOC
- /home/whrho/git/oh-my-tau/crates/omp-judge/src/shadow.rs - env-gated (OMP_LAYA_JUDGE=1) turn-end background scorer, bounded queue of 8
- /home/whrho/git/oh-my-tau/crates/omp-judge/src/policy.rs - noop|clm_edit|rlm_offload context-policy head
- /home/whrho/git/oh-my-tau/crates/omp-judge/src/onnx.rs - feature seam: real Judge vs stub that bails without ONNX Runtime
- /home/whrho/git/oh-my-tau/crates/omp-judge/tests/golden.rs - 50-episode golden test pinning view_sel to Python output
- /home/whrho/git/oh-my-tau/crates/omp-goose/Cargo.toml - no runtime deps, cc build-dep, unsafe_code = "allow" (isolated FFI region)
- /home/whrho/git/oh-my-tau/crates/omp-goose/src/lib.rs - single unsafe FFI call into the goose-compiled dedup kernel
- /home/whrho/git/oh-my-tau/crates/omp-goose/goose/omt_dedup.goose - goose-language SipHash-1-3 dedup kernel source
- /home/whrho/git/oh-my-tau/crates/omp-goose/generated/omt_dedup.c - committed generated C (4398 lines) actually compiled by build.rs
- /home/whrho/git/oh-my-tau/crates/omp-goose/build.rs - cc::Build of the committed C; rerun-if-changed on the .goose source
- /home/whrho/git/oh-my-tau/crates/omp-cli/Cargo.toml - features onnx=["omp-judge/onnx"] (default) and goose=["dep:omp-goose"] (off by default)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/dedup.rs - the pure-Rust reference the goose kernel must match byte for byte
- /home/whrho/git/oh-my-tau/NOTICE.md - license table confirming crossterm = upstream fork, laya-ort = part of this project
</files>

<answer>
None of the four is a port of oh-my-pi. They fall into two groups.

(1) vendor/crossterm — upstream third-party mirror with exactly one tau patch. It is stock crossterm 0.28.1 (upstream sha 492b764f, pristine Cargo.toml.orig, 11,529 LOC over 55 files), injected through [patch.crates-io] rather than workspace membership, and consumed only by crates/omp-tui (with ratatui 0.29, feature event-stream). The single local change is xterm modifyOtherKeys decoding (CSI 27;mods;code~) at src/event/sys/unix/parse.rs:642, added for TS keys.ts parity so ctrl+shift+v works on terminals without the kitty keyboard protocol. Features are the untouched upstream set: default = bracketed-paste/windows/events, plus event-stream, serde, use-dev-tty.

(2) vendor/laya-ort, crates/omp-judge, crates/omp-goose — all tau-specific, all with no omp counterpart. laya-ort (537 LOC, 2 files) is CPU ONNX Runtime inference for the "Laya" judge, replacing a Python daemon (laya_predict.py); it reimplements Python tokenization/temperature bucketing so token ids match the fast tokenizer exactly, and ships the laya-judge JSONL CLI. omp-judge (1,700 LOC, 10 files) is the Rust port of a Python ML pipeline — build_traj_outcome.py for episodes, judge_view.view_sel for the byte-identical judge view (pinned by a 50-episode golden test) — plus a context-policy head, an opt-in shadow worker writing ~/.omp/agent/laya_judge.jsonl, and offline session scoring; its only omp ties are that it reads omp-session entries and writes into the omp agent dir. omp-goose (145 LOC Rust + 4,398 lines of committed generated C) is a goose-language dedup kernel behind a single unsafe FFI call, gated by omp-cli's non-default goose feature and required to be byte-identical to omp-cli's own pure-Rust dedup.rs (a tau cost lever, not an omp feature).

Feature-flag summary: crossterm default=["bracketed-paste","windows","events"] + event-stream/serde/use-dev-tty; laya-ort has no features; omp-judge default=["onnx"], onnx=["dep:laya-ort"] (disabled for musl, where the stub bails cleanly); omp-goose has no features but allows unsafe_code, while omp-cli gates it behind non-default feature "goose".
</answer>

<next_steps>
If the goal is provenance/licensing audit: only vendor/crossterm needs upstream attribution (already in NOTICE.md and vendor/crossterm/LICENSE); the other three are first-party MIT and can be treated as tau-original. If the goal is porting-fidelity audit: compare omp-judge against the Python sources it names (build_traj_outcome.py, judge_view.py, laya-train-v2/train.py) — those are not in this tree (vendor-src/oh-my-pi is absent), so the only fidelity evidence present is the golden fixture at crates/omp-judge/tests/fixtures/golden_view_sel.jsonl. If the goal is build/feature work: onnx is on by default for omp-cli and must stay off for musl builds (scripts/build-musl.sh), and the goose path is inert unless built with --features goose.
</next_steps>
</results>

Note: this ran slightly over the 15-call budget (20 read-only calls) because the first two waves only established directory shape; no writes were made and no subagents were spawned.