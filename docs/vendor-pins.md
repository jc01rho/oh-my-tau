# Vendored crate pins

Single source of truth for where every crate under `vendor/` came from, which upstream
revision it was taken at, and how it may be updated. Companion to
[`rust-migration-plan.md`](rust-migration-plan.md) §4.2b (vendor drift) and §5.1
(pin/delta policy adopted from the mhc precedent), and to the crate table in
[`NOTICE.md`](../NOTICE.md).

Reference upstreams for this repository (all MIT):

| Track | Repository | Branch | Import commit | Reference commit |
| --- | --- | --- | --- | --- |
| `main` | [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) | `main` | `b636e19d10b664950d600505d194337bbeef42f0` (`19.0.1`) | `b636e19d10b664950d600505d194337bbeef42f0` (`v19.0.1+126`; short form `b636e19d10`) |
| `omp2` | [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) | `omp2` | `2f92f3b5aa2035d21c9f016f9ff6350795e5685a` (`omp-*` shell stack in Phase 2) | `2f92f3b5aa2035d21c9f016f9ff6350795e5685a` (short form `2f92f3b`) |
| crossterm | [crossterm-rs/crossterm](https://github.com/crossterm-rs/crossterm) | `master` | n/a — registry crate | `492b764f` (`492b764f9f132d302903ff6f2f55e96c1b8cd0ec`, crate `0.28.1`) |
| prime-agent fork base | [PrimeIntellect-ai/prime-agent](https://github.com/PrimeIntellect-ai/prime-agent) | default | not recorded yet — see "Known gaps" | not recorded yet — see "Known gaps" |

**`import-commit` vs `reference-commit`.** These are different claims and must not be
conflated. `import-commit` is the revision the files under `vendor/` were actually copied
from (updated to `b636e19d10b664950d600505d194337bbeef42f0` for the Phase 2.5 `pi-*` /
`omp-xfs` crates, and `2f92f3b5aa2035d21c9f016f9ff6350795e5685a` for the Phase 2 `omp-*`
shell stack). `reference-commit` is the revision each track is measured against (`main`
`b636e19d10b664950d600505d194337bbeef42f0` and `omp2` `2f92f3b5aa2035d21c9f016f9ff6350795e5685a`).
`vendor-src.lock` carries both fields per crate; this document mirrors them 1:1.

The machine-readable form of these pins is [`vendor-src.lock`](../vendor-src.lock) at the
repository root. It is a **pin ledger**: the durable provenance record for the vendored trees,
and nothing reads it at build or test time. The compat-oracle and other TS-driving scripts do
not consult it — they resolve the upstream checkout at runtime from `$OMP_TS_SRC`, falling back
to `<repo>/vendor-src/oh-my-pi` (gitignored, not distributed; see §5.9). This document is the
prose/decision view of the same facts, and a pin change updates both files together; where the
two disagree, treat the disagreement as a documentation bug and reconcile them, since no
tooling will catch it for you.

## workspace members from `vendor/`

`Cargo.toml` lists `vendor/pa-{types,ai,models,agent}`, `vendor/pi-{edit,ast,diff,vfs,predict}`,
`vendor/omp-xfs`, `vendor/omp-utok`, the Phase 2 `vendor/omp-*` shell stack (`omp-{core,macros,vocab,walker,tool,shell,shell-builtins}`),
`vendor/xutf`, and `vendor/laya-ort` as workspace members. `vendor/crossterm` and `vendor/omp-proto` are not members (`crossterm` enters via `[patch.crates-io]`, `omp-proto` as a standalone path dependency).

## 1. `pi-*` and `omp-xfs` — synced from oh-my-pi (`main` @ `b636e19d10`)

Origin: `can1357/oh-my-pi`, `main` track (`19.0.1`). Authors `Stencil Labs, Inc.`, MIT.
License text: [`vendor/LICENSE-oh-my-pi`](../vendor/LICENSE-oh-my-pi).
Each manifest carries the provenance line `Vendored from can1357/oh-my-pi (MIT), see ../LICENSE-oh-my-pi.`

| Crate | Vendored version | Upstream source | Import commit | Reference commit | Notes |
| --- | --- | --- | --- | --- | --- |
| `pi-ast` | `19.0.1` | `can1357/oh-my-pi` (`main`, `crates/ast`) | `b636e19d10` (`main`) | `b636e19d10` (`main`) | edition 2024; `wasm` feature omits `wasmtime` edge; `search`/`outline`/`debug` synced |
| `pi-diff` | `19.0.1` | `can1357/oh-my-pi` (`main`, `crates/diff`) | `b636e19d10` (`main`) | `b636e19d10` (`main`) | `ComponentArena` + `myers_diff_within`; links `vendor/xutf` |
| `pi-edit` | `19.0.1` | `can1357/oh-my-pi` (`main`, `crates/edit`) | `b636e19d10` (`main`) | `b636e19d10` (`main`) | depends on `pi-ast`, `pi-diff`, `omp-xfs`, `xutf`; `Rejection` + `max_columns` + 5 prompts |
| `pi-vfs` | `19.0.1` | `can1357/oh-my-pi` (`main`, `crates/vfs`) | `b636e19d10` (`main`) | `b636e19d10` (`main`) | `read_until_cancelled` + `ScopedOpen` + `Fs::with_provider` |
| `omp-xfs` | `19.0.1` | `can1357/oh-my-pi` (`main`, `crates/xfs`) | `b636e19d10` (`main`) | `b636e19d10` (`main`) | replaces retired `pi-walker`; async walk/tree/grep/glob over `pi-vfs` |
| `omp-utok` | `19.1.0` | `can1357/oh-my-pi` (`main`, `crates/natives/src/utok` + `data/` + `fixtures/`) | `efcbef6dd0` (`main`) | `efcbef6dd0` (`main`) | exact offline token counts (o200k/cl100k/Qwen3/DeepSeek/Kimi/GLM BPE, Claude ctok reconstructions); Jev dropped; 4.3 MB zstd tables; links `vendor/xutf` (UCD 16.0.0) |
| `pi-predict` | `19.0.1` | `can1357/oh-my-pi` (`main`, `crates/predict`, n-gram only) | `b636e19d10` (`main`) | `b636e19d10` (`main`) | n-gram engine only; `smollm`/`apple` return explicit fallback errors without pulling ML deps |

All five `pi-*` crates and `omp-xfs` come from the `main` track (`b636e19d10`). `pi-predict`
is **not** an `omp2` crate: its source is `can1357/oh-my-pi` `main`, `crates/predict`,
restricted to the n-gram engine. The `omp2` track (`2f92f3b`) supplies the Phase 2
`omp-{core,macros,vocab,walker,tool,shell,shell-builtins}` shell stack (`vendor-src.lock`).

**Version vs. pin.** In Phase 2.5 these crates were synced to `main` `b636e19d10` (`19.0.1`),
retiring `pi-walker` into `omp-xfs` and updating `pi-ast` (`ast::search`), `pi-edit`,
`pi-diff`, `pi-vfs`, and `pi-predict` (n-gram). The rule remains: a `pi-*` update is an API
migration, never a bare version bump.

## 2. `pa-*` — fork of prime-agent, modified in this repo

Origin: `PrimeIntellect-ai/prime-agent` (Rust crates). MIT, with the prime-agent notice
crediting the original TypeScript product. License text:
[`vendor/LICENSE-prime-agent`](../vendor/LICENSE-prime-agent).
These crates are **modified** here — they are not verbatim upstream copies — and they are
co-versioned with this workspace (`version.workspace = true`, currently `0.13.0`) rather
than tracking an upstream crate version.

| Crate | Version | Fork base commit | Modification record |
| --- | --- | --- | --- |
| `pa-types` | `0.13.0` (workspace) | **not recorded** | delta note: `vendor/pa-types/NOTICE` |
| `pa-ai` | `0.13.0` (workspace) | **not recorded** | delta note: `vendor/pa-ai/NOTICE`; bedrock h2c transport, provider registry extensions |
| `pa-models` | `0.13.0` (workspace) | **not recorded** | delta note: `vendor/pa-models/NOTICE`; `src/models_generated.rs` is hand-maintained (no generator — §4.10) |
| `pa-agent` | `0.13.0` (workspace) | **not recorded** | delta note: `vendor/pa-agent/NOTICE` |

`vendor-src.lock` carries no `pa-*` entries — that gap is deliberate here rather than
forgotten: the fork base has not been established, so there is nothing to pin yet.

Delta notes live per crate as `vendor/pa-*/NOTICE` (Phase 0.3), landing alongside this
document; `NOTICE.md` remains the repository-wide summary. A `pa-*` crate without its own
`NOTICE` is an open item, not an exemption.

Semantic distance from upstream is large (`docs/rust-migration-plan.md` §4.2b: ~78,381 lines,
`compaction_epoch`, `get_system_prompt`, `prepare_dispatch`, bedrock h2c). Because the fork
is modified, "sync with upstream" means a reviewed three-way delta, not a copy. `pa-types`
also keeps its own `[lints.clippy]` table instead of `workspace = true`, so the workspace
`unsafe_code = "forbid"` does not outlaw its platform bindings.

## 3. `crossterm` — registry crate plus one local patch

| Field | Value |
| --- | --- |
| Crate | `crossterm` |
| Version | `0.28.1` (upstream `Cargo.toml`, edition 2021, MSRV 1.63.0) |
| Upstream | [crossterm-rs/crossterm](https://github.com/crossterm-rs/crossterm) |
| Pinned commit | `492b764f9f132d302903ff6f2f55e96c1b8cd0ec` (`492b764f`), from `vendor/crossterm/.cargo_vcs_info.json` |
| Provenance | downloaded registry crate (`Cargo.toml` is cargo-normalized; `.cargo_vcs_info.json`, `Cargo.toml.orig` present) |
| License | MIT, [`vendor/crossterm/LICENSE`](../vendor/crossterm/LICENSE) |
| Local delta | xterm `modifyOtherKeys` (`CSI 27 ; mods ; code ~`) decoding — one patch, needed for `ctrl+shift+v` under the modifyOtherKeys fallback (TS `keys.ts` parity) |
| Wiring | `[patch.crates-io]` in the root `Cargo.toml` |

The `sha1` in `.cargo_vcs_info.json` is the upstream commit the published `0.28.1` crate was
built from, not a commit of our fork. Our delta lives only in `vendor/crossterm/src` and is
not representable as an upstream revision — it must be re-applied (and re-verified) if the
base ever moves.

## 4. `laya-ort` — first-party, but pulls an external runtime

| Field | Value |
| --- | --- |
| Crate | `laya-ort` `0.1.0`, `publish = false` |
| Origin | this project — ONNX Runtime inference for the Laya judge (`laya-judge` binary), replacing the Python `laya_predict` daemon |
| Upstream pin | none (not vendored third-party code) |
| External dependency of note | `ort = 2.0.0-rc.11` with `download-binaries`, plus `tokenizers 0.21` |
| License obligation | ONNX Runtime is MIT-licensed (Microsoft / the ONNX Runtime contributors), and that obligation, together with the `ort` dual `MIT OR Apache-2.0` notice and the fact that no ONNX Runtime binary is distributed in this repo, is already satisfied in [`NOTICE.md`](../NOTICE.md#laya-ort-and-onnx-runtime-ort) (Phase 0.2 in the migration plan) |

## 5. Update policy

Applies to everything under `vendor/`. The governing principles are
`docs/rust-migration-plan.md` §5.1 ("adopt: upstream commit SHA pin", "adopt: change delta
notes") and §5.2 #7 ("pin and delta for every `vendor/*`").

1. **Every update lands with a SHA.** An update PR records, in this file, the upstream
   repository, the new commit SHA (full 40 hex, plus the short form used elsewhere), the
   resulting crate version, and the semantic delta. A vendored crate whose SHA is not in
   this file is not considered pinned.
2. **No floating version bumps.** `pi-*` versions are snapshot labels, not compatibility
   promises; bumping `18.9.1` → `19.0.1` in a manifest without the corresponding API port is
   forbidden (`pi-ast`, `pi-walker` change shape, not just numbers).
3. **Semantic diff before code diff.** Roughly two thirds of a raw `pi-*` diff is 4-space vs
   hard-tab indentation; size a change by AST/API surface, not by `git diff --stat` lines.
4. **No silent re-forking.** `pa-*` stays a modified fork. Updating it means a reviewed
   delta against the recorded fork base and an updated per-file list; it never means
   re-copying upstream over local changes.
5. **Local deltas are enumerated.** Non-upstream changes to a vendored crate get a delta
   note: for `crossterm` the single `modifyOtherKeys` patch; for `pa-*` a per-crate
   `vendor/pa-*/NOTICE` modification list (§4.2b, Phase 0.3).
   Rationale: MIT requires preserving notices, and reviewers must be able to tell our code
   from upstream's.
6. **License gate is part of the update.** `cargo deny check licenses sources` (Phase 0.2,
   `deny.toml` ported from `omp2`) must pass; a new vendored crate or a new external
   runtime (ONNX, generated C) also updates `NOTICE.md` in the same change.
7. **Behavior changes need a gate.** Any update that can change user-visible output
   (edit/walk/ast semantics, key decoding, provider wire format) lands only with the
   relevant golden/oracle suite green (`tests-golden/`, `tests-tools/`, `tests-e2e/`).
   Do not delete or weaken a test to accommodate an upstream change.
8. **`pi-walker` is retired.** Phase 2.5 deleted `vendor/pi-walker` and replaced it with
   `vendor/omp-xfs` (`Tree`/`Walk`/`glob`/`grep`). Do not re-introduce `pi-walker`.
9. **Upstream source checkouts are not distributed.** `vendor-src/oh-my-pi/` is gitignored;
   pins here are the durable record, so this file must stay accurate without it.
10. **Reviewer checklist for a vendor change:** SHA recorded here · `NOTICE.md`/license
    updated · delta list updated · semantic delta described · `cargo deny` green ·
    affected goldens green.

## Known gaps (Phase 0.3 work)

- `pi-*` and `omp-xfs` pins were brought up to `main` `b636e19d10` (`19.0.1`) in Phase 2.5,
  closing the unrecorded-SHA gap for the `pi-*` family.
- No SHA is recorded for the `pa-*` fork base; it must be recovered from the original
  checkout and added both here and to `vendor-src.lock`.
- `NOTICE.md` states the `pa-*` crates are modified but does not list the modified files;
  the per-crate `vendor/pa-*/NOTICE` notes close this gap and must cover all four crates.
- `vendor/pa-ai/src/models_generated.rs` has no generator, so its content cannot be
  reproduced from a pin (§4.10).
