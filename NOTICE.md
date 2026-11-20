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
| `pi-ast`, `pi-diff`, `pi-edit`, `pi-predict`, `pi-vfs`, `pi-walker` | oh-my-pi (can1357/oh-my-pi), vendored verbatim per each crate's `Cargo.toml` | MIT, see [`vendor/LICENSE-oh-my-pi`](vendor/LICENSE-oh-my-pi) |
| `crossterm` | fork of [crossterm](https://github.com/crossterm-rs/crossterm) 0.28 | MIT, see [`vendor/crossterm/LICENSE`](vendor/crossterm/LICENSE) |
| `laya-ort` | part of this project (ONNX Runtime inference for the Laya judge) | MIT, plus the `ort` / ONNX Runtime terms below |

Per-crate notices:

- [`vendor/pa-agent/NOTICE`](vendor/pa-agent/NOTICE)
- [`vendor/pa-ai/NOTICE`](vendor/pa-ai/NOTICE)
- [`vendor/pa-models/NOTICE`](vendor/pa-models/NOTICE)
- [`vendor/pa-types/NOTICE`](vendor/pa-types/NOTICE)

Each file documents the PrimeIntellect-ai/prime-agent MIT lineage and enumerates the modifications made in this repository.

Notes:

- The `pa-*` crates are a modified fork of the prime-agent crates; the prime-agent
  license file additionally credits the original product: "a port of the Prime
  Agent TypeScript product, originally Copyright (c) 2025 Mario Zechner".
- `pi-predict` vendors only the n-gram word-completion engine of oh-my-pi; the
  smollm and apple engines are not included.

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
