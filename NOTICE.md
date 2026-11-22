# Notices

This file covers third-party code vendored into this repository. oh my tau (omt) itself
is MIT-licensed — see [`LICENSE`](LICENSE).

## oh-my-pi (omp)

omt is a Rust port of [oh-my-pi](https://github.com/can1357/oh-my-pi) ("omp"),
a TypeScript coding agent by Mario Zechner, Can Bölük and Stencil Labs, Inc.
The upstream TypeScript source used as the porting reference is checked out at
`vendor-src/oh-my-pi/` (gitignored; not distributed). Its license text is
vendored at [`vendor/LICENSE-oh-my-pi`](vendor/LICENSE-oh-my-pi) (MIT,
Copyright (c) 2025 Mario Zechner, Copyright (c) 2025-2026 Can Bölük,
Copyright (c) 2026 Stencil Labs, Inc.).

## Vendored crates (`vendor/`)

| Crate | Origin | License |
| --- | --- | --- |
| `pa-agent`, `pa-ai`, `pa-models`, `pa-types` | [PrimeIntellect-ai/prime-agent](https://github.com/PrimeIntellect-ai/prime-agent) — **modified** in this repo | MIT, see [`vendor/LICENSE-prime-agent`](vendor/LICENSE-prime-agent) |
| `pi-ast`, `pi-diff`, `pi-edit`, `pi-predict`, `pi-vfs`, `omp-xfs` | oh-my-pi (`can1357/oh-my-pi`) ref `main` @ `b636e19d10b664950d600505d194337bbeef42f0` (`19.0.1`), adapted per `vendor-src.lock` (`pi-walker` retired into `omp-xfs`) | MIT, see [`vendor/LICENSE-oh-my-pi`](vendor/LICENSE-oh-my-pi) and per-crate `LICENSE` (`omp-xfs`, `pi-ast`) |
| `omp-core`, `omp-macros`, `omp-vocab`, `omp-walker`, `omp-shell`, `omp-shell-builtins` | **adapted from** oh-my-pi (can1357/oh-my-pi) ref `omp2` @ `2f92f3b5aa2035d21c9f016f9ff6350795e5685a`: vendored from that commit with manifests pinned to build outside the upstream workspace, but **adapted, not unmodified verbatim** — several crates carry further upstream lineage with retained notices (`omp-shell`: brush-core/brush-parser; `omp-shell-builtins`: uutils coreutils/findutils/sed and `pi-builtins`/`pi-shell`; `omp-walker`: `pi-walker`; `omp-core`: `serde_json`). See each crate's `LICENSE`/`NOTICE`, listed below | MIT, see [`vendor/LICENSE-oh-my-pi`](vendor/LICENSE-oh-my-pi); per-crate files listed below |
| `omp-utok` | oh-my-pi (`can1357/oh-my-pi`) ref `main` @ `efcbef6dd0a190b82ea8811db3526a97101fbbb3`, `crates/natives` `utok` module extracted into a crate per `vendor-src.lock`; embeds the tokenizer vocabularies listed below | MIT, see [`vendor/LICENSE-oh-my-pi`](vendor/LICENSE-oh-my-pi) and [`vendor/omp-utok/data/LICENSE.ctok`](vendor/omp-utok/data/LICENSE.ctok) |
| `omp-tool`, `omp-proto`, `xutf` | **local shims**, not upstream code — see the note below the table | MIT (this project) |
| `crossterm` | fork of [crossterm](https://github.com/crossterm-rs/crossterm) 0.28 | MIT, see [`vendor/crossterm/LICENSE`](vendor/crossterm/LICENSE) |
| `laya-ort` | part of this project (ONNX Runtime inference for the Laya judge) | MIT, plus the `ort` / ONNX Runtime terms below |

`omp-utok` embeds zstd-compressed rank tables derived from public tokenizer
files: OpenAI tiktoken `o200k_base` / `cl100k_base` (MIT, openai/tiktoken), the
Qwen3.8 `tokenizer.json` (Apache-2.0, Qwen), DeepSeek V3/V4 `tokenizer.json`
(MIT, deepseek-ai), Kimi K2 `tiktoken.model` (Modified MIT, moonshotai) and the
GLM-5 `tokenizer.json` (MIT, zai-org), plus the measured Claude vocabularies of
[sanderland/ctok](https://github.com/sanderland/ctok) v1.0.0 (MIT, see
`vendor/omp-utok/data/LICENSE.ctok`). Packing formats and sources:
[`vendor/omp-utok/data/README.md`](vendor/omp-utok/data/README.md) and
`families.json` next to it.

Per-crate notices:

- [`vendor/pa-agent/NOTICE`](vendor/pa-agent/NOTICE)
- [`vendor/pa-ai/NOTICE`](vendor/pa-ai/NOTICE)
- [`vendor/pa-models/NOTICE`](vendor/pa-models/NOTICE)
- [`vendor/pa-types/NOTICE`](vendor/pa-types/NOTICE)

Each file documents the PrimeIntellect-ai/prime-agent MIT lineage and enumerates the modifications made in this repository.

Provenance files shipped with the adapted `omp-*` crates; read them before
reusing or re-vendoring that code:

- [`vendor/omp-core/LICENSE`](vendor/omp-core/LICENSE) and
  [`vendor/omp-core/NOTICE`](vendor/omp-core/NOTICE) — MIT (Stencil Labs, Inc.).
  The `slopjson` `json!` token-tree muncher
  ([`vendor/omp-core/src/slopjson/macros.rs`](vendor/omp-core/src/slopjson/macros.rs))
  is adapted from the `serde_json` `json!` macro. `serde_json` is dual-licensed
  `MIT OR Apache-2.0`, at the recipient's option; this repository elects the MIT
  terms, whose complete text ships in [`vendor/omp-core/LICENSE`](vendor/omp-core/LICENSE).
  [`vendor/omp-core/NOTICE`](vendor/omp-core/NOTICE) also points at a
  `THIRD-PARTY-NOTICES.txt` for the Apache-2.0 alternative; that file is an upstream
  `omp2` repository-root file and is **not shipped in this repository**, so the
  Apache-2.0 option is not available from the vendored tree — obtain it from
  upstream `serde_json` (https://github.com/serde-rs/json) if you need those terms.
- [`vendor/omp-macros/LICENSE`](vendor/omp-macros/LICENSE) — MIT (Stencil Labs, Inc.).
- `omp-vocab` ships no per-crate license file; its terms are the oh-my-pi MIT text in
  [`vendor/LICENSE-oh-my-pi`](vendor/LICENSE-oh-my-pi).
- [`vendor/omp-walker/LICENSE`](vendor/omp-walker/LICENSE) and
  [`vendor/omp-walker/NOTICE`](vendor/omp-walker/NOTICE) — MIT; continues the earlier
  `pi-walker` implementation (renamed, extended and modified; no separate `pi-walker`
  copyright notice was present in the incorporated snapshot).
- [`vendor/omp-shell/LICENSE`](vendor/omp-shell/LICENSE) and
  [`vendor/omp-shell/README.md`](vendor/omp-shell/README.md) — incorporates and adapts
  brush-core 0.5.0 and brush-parser 0.4.0 (MIT, Copyright (c) 2024 reuben olinsky),
  compiled directly into the engine with local fixes and interactive-shell machinery removed.
- [`vendor/omp-shell-builtins/LICENSE`](vendor/omp-shell-builtins/LICENSE) and
  [`vendor/omp-shell-builtins/NOTICE`](vendor/omp-shell-builtins/NOTICE) — ported from
  `pi-builtins`, retaining per-module notices for uutils coreutils 0.8.0, uutils
  findutils (Google Inc. 2017; Tavian Barnes 2017, 2022; Collabora, Ltd. 2021, 2022),
  uutils sed 0.1.1 (Copyright (c) 2025 Diomidis Spinellis), the `wc` UTF-8 decoder
  (Copyright (c) Simon Sapin and many others), and earlier `pi-shell` implementations.
- [`vendor/omp-xfs/LICENSE`](vendor/omp-xfs/LICENSE) and
  [`vendor/pi-ast/LICENSE`](vendor/pi-ast/LICENSE) — MIT (Stencil Labs, Inc.).

Notes:

- The `pa-*` crates are a modified fork of the prime-agent crates; the prime-agent
  license file additionally credits the original product: "a port of the Prime
  Agent TypeScript product, originally Copyright (c) 2025 Mario Zechner".
- `pi-predict` vendors only the n-gram word-completion engine of oh-my-pi; the
  smollm and apple engines are not included.
- `omp-tool`, `omp-proto` and `xutf` are **not** vendored from oh-my-pi (nor crates.io `xutf`). They keep the
  upstream crate names so the shell stack resolves unchanged, but they are
  first-party MIT code of this repository:
  - `vendor/omp-tool` exposes only the `Diag` diagnostics payload that
    `omp-shell-builtins` consumes, instead of upstream `crates/tool`, which
    drags in `omp-ai`, `omp-catalog` and the proto/gRPC agent mesh.
  - `vendor/omp-proto` is a dependency-free hand-written subset of the wire
    shapes `omp-shell` reads, replacing upstream `crates/proto`'s
    prost/protox build-script codegen. It declares its own `[workspace]`, so it
    enters the graph as a path dependency rather than a workspace member.
  - `vendor/xutf` is a stable-Rust shim replacing crates.io `xutf` (which
    requires nightly `core::simd`) for both the `omp2` shell stack (UTF-8 char
    decoding and display width) and the `19.0.1` `pi-diff`/`pi-edit`/`pi-predict`/`pi-ast`
    stack (`lines`, `lines_str`, `LineEnds`, `Text`, `UnitSet`, `find_iter`, and
    `IntoUnicodeNormalized` via `unicode-normalization`).
  - Note for MPL hygiene: `omp-shell` and `omp-shell-builtins` depend on `im`
    15.1 (MPL-2.0), which brings `bitmaps` and `sized-chunks`. Those are
    unmodified crates.io dependencies, not vendored here, and are allowed
    package-scoped in [`deny.toml`](deny.toml). If those crates are ever forked
    into `vendor/`, that changes their MPL obligations and must be re-reviewed.

## `laya-ort` and ONNX Runtime (`ort`)

`vendor/laya-ort` is first-party code (MIT, part of this project). It is **not** a copy
of a third-party crate; it is an in-repo binary/lib crate that hosts the Laya judge
inference, replacing the former Python `laya_predict` daemon.

The third-party pieces it pulls in at build time are:

- **`ort`** (crate `2.0.0-rc.11` in `Cargo.toml`; `2.0.0-rc.14` resolved in
  `Cargo.lock`) — the Rust bindings for ONNX Runtime, dual-licensed
  `MIT OR Apache-2.0`. It is a normal crates.io dependency, not vendored here, so its
  license texts ship with the crate source in the Cargo registry cache rather than in
  `vendor/`.
- **ONNX Runtime itself** — the native library `ort` binds to, licensed MIT by
  Microsoft / the ONNX Runtime contributors. `laya-ort` enables the
  `ort` `download-binaries` feature, so a prebuilt ONNX Runtime shared library is
  fetched by `ort`'s build script at compile time and is **not distributed in this
  repository**. Only a source build of omt links an ONNX Runtime binary, and that
  binary keeps its own MIT terms and copyright notice, unaffected by omt's license.

`laya-ort`'s remaining direct dependencies are `tokenizers` (Apache-2.0), `serde_json`
(`MIT OR Apache-2.0`), `anyhow` (`MIT OR Apache-2.0`), and `rayon` (`MIT OR Apache-2.0`),
each carrying its own upstream license; none is vendored in this repository, and no model
weights or tokenizer vocabularies are vendored here either.

## `omp-goose` generated dedup kernel

`crates/omp-goose` is first-party code (MIT, part of this project) but contains a
**generated C translation unit** that must be called out because it is compiled by
`build.rs` through the `cc` crate and is not hand-written Rust:

- Kernel source: [`crates/omp-goose/goose/omt_dedup.goose`](crates/omp-goose/goose/omt_dedup.goose),
  written in goose (a memory-safe systems language with no heap allocator).
- Generated output: [`crates/omp-goose/generated/omt_dedup.c`](crates/omp-goose/generated/omt_dedup.c)
  and [`crates/omp-goose/generated/omt_dedup.h`](crates/omp-goose/generated/omt_dedup.h),
  committed to the tree so that building omt does not require the goose toolchain.
  Regenerate with [`crates/omp-goose/regenerate.sh`](crates/omp-goose/regenerate.sh).
- Both the goose source and the generated C are original work of this project and are
  released under this repository's MIT license (see `LICENSE`); they are not derived
  from any third-party project.
- The crate is deliberately isolated so its `unsafe` FFI region stays outside the
  `omp-cli` workspace's `unsafe_code = "forbid"`, the same way `omp-sandbox` is; the
  crate sets `unsafe_code = "allow"` for itself only.
