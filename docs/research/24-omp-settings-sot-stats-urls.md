<analysis>
**Literal Request**: Inspect four crates' Cargo.toml/src/tests/LOC and describe what each handles plus partial/stub areas.
**Actual Need**: A crate-by-crate capability map with explicit gaps, so the caller knows what is finished, what is deliberately out of scope, and what is degraded in standalone use.
**Success Looks Like**: Per-crate purpose, module inventory, LOC/test counts, and a concrete "partial/stub" list with file:line evidence.
</analysis>

All four crates are complete ports (no `todo!`/`unimplemented!`/`FIXME` anywhere); the "partial" areas are deliberate scope reductions documented in module headers, plus one genuinely weak function in `omp-settings`.

## omp-settings — 4,951 LOC (src 4,778 / tests 190), 8 tests
Typed, layered view over `config.yml`: TS registry defaults < global `~/.omp/agent/config.yml` (or `OMP_PROFILE` profile dir) < project `.omp/config.yml`/`config.yaml` < env vars. Unknown keys preserved verbatim and reported.

- `src/schema.rs` (3,933 LOC, generated, "do not edit by hand") — 548 `SettingDef` entries (526 TS `register({...})` + 19 panel-only + 3 omp-rs gates), sorted for binary search; each carries `id`, `ty`, `env`, `default_yaml`, `credential`.
- `src/settings.rs` (773 LOC) — `Settings::load/load_from`, `get*` accessors, typed helpers (`model_presets`, `model_roles`, `cycle_order`, `disabled_*`, `images_urls_*`, `tier()`), `write_global` (cfg:// save semantics: type-validated, null = unset, unknown keys preserved), deep-merge with TS `null`-falls-through semantics, `parse_env_value`, unknown-key collection.
- `src/lib.rs` (55) — `SettingType`, `lookup()`.
- Consumers: `omp-cli`, `omp-providers`, `omp-tui`, `omp-urls`.

**Partial area (real defect, not just scope):** `Settings::provenance` (`settings.rs:210-229`) is half-implemented — `Provenance::Global` is never returned (no grep hit anywhere in src or tests), and any merged value present is reported as `Provenance::Project` regardless of whether it came from global config or defaults ("merged value present; layer detail below" is a comment, not code). `tests/settings.rs:133` only does `let _ = Provenance::Default; // provenance API smoke` — no test covers Global/Project/Env discrimination.

## omp-sot — 2,340 LOC (src 1,756 / example 584), 33 tests (all inline `#[cfg(test)]`, no `tests/` dir)
"State of Thought" limited-access controllers per arXiv 2609.16055 App. D — pure data structures, deterministic, no I/O except the injected backend.

- `state.rs` (444) — endogenous 4-D state `m_t=[δ,v,c,H]`, Eq. 13-19, `Normalizer`, `Trajectory`.
- `controller.rs` (317) — training-free operators `TrainingFreeEvidence` (Eq. 25 state-compatibility scoring) / `TrainingFreeStop` (Eq. 27 trailing-window).
- `embed.rs` (299) — SoT-Embed: `SentenceEncoder` trait, `Pca` fit/residual, `NearestPool`.
- `segment.rs` (149) — conservative Unicode sentence splitter (abbreviation/decimal/ellipsis aware).
- `sot.rs` (507) — `Backend`/`StateTracker` traits, `EmbeddingTracker`, `run()` (Algorithm 1), `StopReason`.
- `examples/gsm8k_sot.rs` (584) — GSM8K smoke bench vs local Ollama (vanilla/cot/sot).

**Partial by design:** the paper's *learned* controller (582 params, hidden-state access) is not implemented — only the two limited-access variants. In SoT-Embed, δ and H are documented **proxies** (PCA residual, k-NN cosine distance) for coordinates the paper leaves open. `budget` counts reasoning units, not the paper's token budget (Table 11: 1536); token budgeting is delegated to `Backend`. The example explicitly "approximates" the paper's deployment scaffolding with CLI flags and ships no datasets.

## omp-stats — 21,067 LOC (.rs), 185 tests
Port of `@oh-my-pi/omp-stats`: session-JSONL ingest into sqlite, hourly rollups, dashboard JSON API, and the local dashboard HTTP server. One module per TS file, all feature-gated along the TS import graph (`default = ["server"]` pulls in gain/frustration/live/trace/port_conflict), so each module builds alone.

Largest modules: `types.rs` 1,116; `db/ingest.rs` 680; `trace/scan.rs` 681; `port_conflict.rs` 656; `frustration/run.rs` 644; `trace/mod.rs` 616; `frustration/tests.rs` 583; `taxonomy/classify.rs` 567; `parser/entries.rs` 561; `db/schema.rs` 454; `aggregator/sync.rs` 422; `parser/session.rs` 422; `db/schema_backfills.rs` 423; `trace/list.rs` 424; `usage_windows/mod.rs` 398; `catalog/mod.rs` 384.

Module groups: `parser` (lenient JSONL scanning, wyhash checkpoints, JS `Date.parse`/`String.length` semantics, service-tier helpers) → `db` (global sqlite handle, schema + migrations/backfills, file offsets, ingest) → `rollup` (trigger-driven dirty-hour refresh, union reads) → `db_query`/`aggregator` → `server` (hyper 1 router, SSE `/api/events`, statics, port-conflict reuse/reclaim). Plus `taxonomy`, `user_metrics`, `catalog` (embedded pricing JSON), `gain` (snapcompact savings), `frustration` (paid LLM judge), `usage_windows` (subscription windows), `live` (notify watcher), `trace` (span trees), `paths`, `testutil`.

**Partial / degraded areas:**
- `client/` is *not* ported — the prebuilt React bundle (`index.js` 610 KB, `index.css`, `index.html`, `favicon.ico`) is served byte-for-byte (`lib.rs:3-5`, `server/statics.rs`).
- `smokeTestSyncWorker` intentionally not ported (no Bun workers; parsing runs on scoped threads) — `aggregator/mod.rs:4-6`.
- Frustration judge: standalone `omp-stats` registers no `StatsJudgeProvider`, so dashboard reports `judgeAvailable: false`, unavailable estimate, and 503 on start (`frustration.rs:9-11`, `frustration/run.rs:106-112`).
- Usage windows: no broker registered in-crate (no HTTP client) → falls back to local agent.db with no fleet tokens; `omp-cli` registers the real broker (`usage_windows/mod.rs:24-25`).
- Windows: no stable inode identity — canonical-path hash used as `ino` stand-in, `dev=0`; renamed files re-parse from scratch (`parser/session.rs:157-161`).
- `port_conflict.rs:337` `process_alive` is a hand-rolled `kill(pid,0)` stand-in (per-OS cfg).
- Runtime substitutions, not behavior gaps: `fs.watch` → `notify` thread + polling fallback, timers → epoch-cancelled threads, async memo → process-global `Mutex`, listeners → `tokio::sync::broadcast`.

## omp-urls — 4,687 LOC (src 4,481 / test 206), 57 tests
Internal URL layer: parse, scheme specs, provider trait, process-global registry. Port of TS `src/internal-urls`.

- `parse.rs` (230) — `InternalUrl`, `parse_internal_url`, `extract_uri_scheme`.
- `spec.rs` (120) — `SchemeBacking` (File/Virtual/Remote/Device), `SchemeSelectors`, `WriteScope`, `WriteVia`, `SchemeSpec`, `InternalResource`, `UrlCompletion`.
- `lib.rs` (140) — `UrlProvider` trait (read/write/locate/locate_create/locate_sync/complete/prompt_doc, all with defaults), `UrlCtx`, global session file.
- `registry.rs` (317) — `Registry`, `UrlTarget` (File vs Resource routing), write path.
- `selectors.rs` (152) — `:N`, `:N-M`, `:N+M` line selectors.
- 11 schemes: `skill` 406, `history` 428, `cfg` 545, `mcp` 357, `omp` 381, `local` 343, `agent` 229, `rule` 175, `artifact` 168, `core` 168, `xd` 148. `schemes::register_all` wires skill/rule/omp/cfg/mcp/xd + core.
- Depends on `omp-settings` (cfg:// reads the layered view) and `omp-providers`. Consumed by `omp-cli`.

**Partial areas:**
- All schemes are **read-only**: no provider overrides `fn write` (grep-verified), so the trait default bail applies everywhere. `cfg://` explicitly documents that TS's approval-gated writes "are not ported" — `schemes/cfg.rs:8-10`; `CFG_SAVE_SEGMENT` is "parsed, never written here".
- Capability flags are hand-maintained: `impl_write`/`impl_locate` are manual `bool`s because Rust can't probe method presence (`lib.rs:76-84`), so a provider adding `write` must remember to flip the flag — `Registry::register` only cross-checks that one.

<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-settings/src/settings.rs - layered store; contains the one real stub (`provenance`, lines 210-229)
- /home/whrho/git/oh-my-tau/crates/omp-settings/src/schema.rs - generated 548-entry registry table
- /home/whrho/git/oh-my-tau/crates/omp-settings/tests/settings.rs - 8 behavior tests; line 133 only smoke-tests Provenance
- /home/whrho/git/oh-my-tau/crates/omp-sot/src/sot.rs - Algorithm 1 closed loop, Backend/StateTracker traits
- /home/whrho/git/oh-my-tau/crates/omp-sot/src/embed.rs - SoT-Embed proxies (PCA residual, k-NN distance) replacing δ/H
- /home/whrho/git/oh-my-tau/crates/omp-sot/examples/gsm8k_sot.rs - GSM8K/Ollama bench, "approximates" paper scaffolding
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/lib.rs - feature-gate map; states client is not ported
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/aggregator/mod.rs - notes smokeTestSyncWorker intentionally not ported
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/frustration.rs - judge provider unregistered in standalone (503, judgeAvailable:false)
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/usage_windows/mod.rs - broker unregistered in-crate; omp-cli supplies it
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/parser/session.rs - Windows inode stand-in (path hash), lines 157-161
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/port_conflict.rs - hand-rolled process_alive, line 337
- /home/whrho/git/oh-my-tau/crates/omp-stats/client/index.js - prebuilt React bundle served as-is (610 KB)
- /home/whrho/git/oh-my-tau/crates/omp-urls/src/lib.rs - UrlProvider trait with default read-only write; manual impl_write/impl_locate flags
- /home/whrho/git/oh-my-tau/crates/omp-urls/src/schemes/cfg.rs - explicitly read-only; TS approval-gated writes not ported
- /home/whrho/git/oh-my-tau/crates/omp-urls/src/registry.rs - File-vs-Resource routing and write rejection
- /home/whrho/git/oh-my-tau/crates/omp-urls/tests/schemes_fixture.rs - parity tests for artifact/core/history/local schemes
</files>

<answer>
- **omp-settings** (4,951 LOC / 8 tests): the typed `config.yml` model — generated 548-setting registry plus a 4-layer merge (TS defaults < global < project < env) with unknown-key preservation and `write_global` for cfg:// saves. Only genuine defect in all four crates: `Settings::provenance` never distinguishes Global from Project/Default (any present merged value → `Project`), and no test exercises it.
- **omp-sot** (2,340 LOC / 33 tests): the arXiv 2609.16055 State-of-Thought limited-access variants — training-free operators plus SoT-Embed (encoder + PCA + k-NN proxies for the paper's δ/H), driven by `sot::run`. The learned 582-param controller is out of scope by design; unit-budget rather than token-budget; no `tests/` dir (inline tests only); the GSM8K example is a smoke harness that approximates the paper's prompts.
- **omp-stats** (21,067 LOC / 185 tests): full TS port of the stats pipeline — JSONL parser → sqlite ingest → trigger-driven hourly rollups → range queries → dashboard JSON API and hyper server. Nothing is unimplemented; the gaps are host-injected: the React client is a checked-in bundle, the frustration judge and usage broker are unregistered standalone (503 / no fleet tokens; omp-cli supplies both), Windows lacks real inode identity, and `fs.watch`/Bun workers/async memo are replaced by notify threads, scoped threads, and a global Mutex.
- **omp-urls** (4,687 LOC / 57 tests): the internal-URL router — 11 read-only schemes (skill/rule/omp/cfg/mcp/xd/core + local/artifact/agent/history), line selectors, File-vs-Resource target routing, global registry. Writes are entirely unported (cfg:// approval-gated saves are explicitly excluded), and capability reporting relies on hand-maintained `impl_write`/`impl_locate` booleans.
</answer>

<next_steps>
If the goal is hardening: fix `Settings::provenance` (track per-layer origin during merge instead of inferring from the merged map) and add a Global/Project/Env test — that is the only functional hole. If the goal is porting parity: the missing pieces are cfg:// writes in omp-urls, the omp-stats React client, and the learned SoT controller — all three are documented as intentional omissions, so confirm scope before starting any of them.
</next_steps>
</results>