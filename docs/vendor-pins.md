# Vendored crate pins

Single source of truth for where every crate under `vendor/` came from, which upstream
revision it was taken at, and how it may be updated. Companion to
[`rust-migration-plan.md`](rust-migration-plan.md) §4.2b (vendor drift) and §5.1
(pin/delta policy adopted from the mhc precedent), and to the crate table in
[`NOTICE.md`](../NOTICE.md).

Reference upstreams for this repository (all MIT):

| Track | Repository | Branch | Import commit | Reference commit |
| --- | --- | --- | --- | --- |
| `main` | [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) | `main` | unrecorded (`18.9.1` / `18.6.1` import) | `b636e19d10b664950d600505d194337bbeef42f0` (`v19.0.1+126`; short form `b636e19d10`) |
| `omp2` | [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) | `omp2` | none — no crate imported from this track | `2f92f3b5aa2035d21c9f016f9ff6350795e5685a` (short form `2f92f3b`) |
| crossterm | [crossterm-rs/crossterm](https://github.com/crossterm-rs/crossterm) | `master` | n/a — registry crate | `492b764f` (`492b764f9f132d302903ff6f2f55e96c1b8cd0ec`, crate `0.28.1`) |
| prime-agent fork base | [PrimeIntellect-ai/prime-agent](https://github.com/PrimeIntellect-ai/prime-agent) | default | not recorded yet — see "Known gaps" | not recorded yet — see "Known gaps" |

**`import-commit` vs `reference-commit`.** These are different claims and must not be
conflated. `import-commit` is the revision the files under `vendor/` were actually copied
from; for every `pi-*` crate it is **unrecorded** (the initial import was taken at crate
versions `18.9.1` / `pi-predict` `18.6.1` and the SHA was never captured). `reference-commit`
is the revision the vendored tree is measured against and meant to be updated toward — `main`
`b636e19d10b664950d600505d194337bbeef42f0` for the `pi-*` crates, and `omp2`
`2f92f3b5aa2035d21c9f016f9ff6350795e5685a` as the `omp2` reference track (which supplies no
vendored crate). `vendor-src.lock` carries both fields per crate; this document mirrors them
1:1.

The machine-readable form of these pins is [`vendor-src.lock`](../vendor-src.lock) at the
repository root. It is a **pin ledger**: the durable provenance record for the vendored trees,
and nothing reads it at build or test time. The compat-oracle and other TS-driving scripts do
not consult it — they resolve the upstream checkout at runtime from `$OMP_TS_SRC`, falling back
to `<repo>/vendor-src/oh-my-pi` (gitignored, not distributed; see §5.9). This document is the
prose/decision view of the same facts, and a pin change updates both files together; where the
two disagree, treat the disagreement as a documentation bug and reconcile them, since no
tooling will catch it for you.

## workspace members from `vendor/`

`Cargo.toml` lists `vendor/pa-{types,ai,models,agent}`, `vendor/pi-{edit,ast,diff,walker,vfs,predict}`
and `vendor/laya-ort` as workspace members. `vendor/crossterm` is not a member: it enters the
graph through `[patch.crates-io] crossterm = { path = "vendor/crossterm" }`.

## 1. `pi-*` — vendored verbatim from oh-my-pi

Origin: `can1357/oh-my-pi`, `main` track. Authors `Stencil Labs, Inc.`, MIT.
License text: [`vendor/LICENSE-oh-my-pi`](../vendor/LICENSE-oh-my-pi).
Each manifest carries the provenance line `Vendored from can1357/oh-my-pi (MIT), see ../LICENSE-oh-my-pi.`

| Crate | Vendored version | Upstream source | Import commit | Reference commit | Notes |
| --- | --- | --- | --- | --- | --- |
| `pi-ast` | `18.9.1` | `can1357/oh-my-pi` (`main`, `crates/ast`) | unrecorded (`18.9.1`) | `b636e19d10` (`main`) | edition 2024; tree-sitter grammar set pinned per-crate in the manifest |
| `pi-diff` | `18.9.1` | `can1357/oh-my-pi` (`main`, `crates/diff`) | unrecorded (`18.9.1`) | `b636e19d10` (`main`) | dependency-free manifest |
| `pi-edit` | `18.9.1` | `can1357/oh-my-pi` (`main`, `crates/edit`) | unrecorded (`18.9.1`) | `b636e19d10` (`main`) | depends on `pi-ast`, `pi-diff`, `pi-walker` by path |
| `pi-vfs` | `18.9.1` | `can1357/oh-my-pi` (`main`, `crates/vfs`) | unrecorded (`18.9.1`) | `b636e19d10` (`main`) | |
| `pi-walker` | `18.9.1` | `can1357/oh-my-pi` (`main`, `crates/walker` -> retired into `crates/xfs::walk`) | unrecorded (`18.9.1`) | `b636e19d10` (`main`) | deleted upstream and folded into `crates/xfs::walk`; see §4.2b |
| `pi-predict` | `18.6.1` | `can1357/oh-my-pi` (`main`, `crates/predict`, n-gram only) | unrecorded (`18.6.1`) | `b636e19d10` (`main`) | n-gram engine only; the `smollm` and `apple` engines are not vendored |

All six `pi-*` crates come from the `main` track. `pi-predict` is **not** an `omp2` crate: its
source is `can1357/oh-my-pi` `main`, `crates/predict`, restricted to the n-gram engine. The
`omp2` track (`2f92f3b`) is a reference-only pin — it contributes no vendored crate.

**Version vs. pin.** The manifests still say `18.9.1` (`pi-predict` `18.6.1`) while the
designated reference `main` head is `19.0.1+126`. This is expected and is *not* evidence that
the `reference-commit` is wrong: the vendored trees were copied at the earlier crate
versions, and that copy point (`import-commit`) was never recorded as a SHA — for every
`pi-*` crate it reads `unrecorded (v18.9.1)` / `unrecorded (v18.6.1)` in `vendor-src.lock`.
So read `import-commit` as "where these files actually came from (unknown SHA)", the crate
version as "what the copied files say", and `reference-commit` as "the revision drift is
measured against and updates target" (`b636e19d10`). The drift against that reference is
exactly what §4.2b measures (`pi-walker` deleted, `pi-ast` API replaced by `ast::search`,
`pi-edit` ~10.3k lines). Either way the rule stands: a `pi-*` update is an API migration,
never a version bump.

## 2. `pa-*` — fork of prime-agent, modified in this repo

Origin: `PrimeIntellect-ai/prime-agent` (Rust crates). MIT, with the prime-agent notice
crediting the original TypeScript product. License text:
[`vendor/LICENSE-prime-agent`](../vendor/LICENSE-prime-agent).
These crates are **modified** here — they are not verbatim upstream copies — and they are
co-versioned with this workspace (`version.workspace = true`, currently `0.12.0`) rather
than tracking an upstream crate version.

| Crate | Version | Fork base commit | Modification record |
| --- | --- | --- | --- |
| `pa-types` | `0.12.0` (workspace) | **not recorded** | delta note: `vendor/pa-types/NOTICE` |
| `pa-ai` | `0.12.0` (workspace) | **not recorded** | delta note: `vendor/pa-ai/NOTICE`; bedrock h2c transport, provider registry extensions |
| `pa-models` | `0.12.0` (workspace) | **not recorded** | delta note: `vendor/pa-models/NOTICE`; `src/models_generated.rs` is hand-maintained (no generator — §4.10) |
| `pa-agent` | `0.12.0` (workspace) | **not recorded** | delta note: `vendor/pa-agent/NOTICE` |

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
8. **`pi-walker` is frozen.** Upstream deleted it in favour of `crates/xfs::walk`. Do not
   modernize the vendored copy; the path forward is replacing it with an `xfs`-style
   `Tree`/`Walk`/`Batch` implementation (§4.2b, Phase 3).
9. **Upstream source checkouts are not distributed.** `vendor-src/oh-my-pi/` is gitignored;
   pins here are the durable record, so this file must stay accurate without it.
10. **Reviewer checklist for a vendor change:** SHA recorded here · `NOTICE.md`/license
    updated · delta list updated · semantic delta described · `cargo deny` green ·
    affected goldens green.

## Known gaps (Phase 0.3 work)

- Every `pi-*` crate has `reference-commit = b636e19d10` but an **unrecorded**
  `import-commit` (`18.9.1`, `pi-predict` `18.6.1`) — this is why the crate versions do not
  self-report `19.0.1`. Recover the import SHA from the original checkouts and record it in
  both this file and `vendor-src.lock`, or keep the unrecorded marker deliberately.
- No SHA is recorded for the `pa-*` fork base; it must be recovered from the original
  checkout and added both here and to `vendor-src.lock`.
- `NOTICE.md` states the `pa-*` crates are modified but does not list the modified files;
  the per-crate `vendor/pa-*/NOTICE` notes close this gap and must cover all four crates.
- `vendor/pa-ai/src/models_generated.rs` has no generator, so its content cannot be
  reproduced from a pin (§4.10).
