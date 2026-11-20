<analysis>
**Literal Request**: Diff six vendored `pi-*` crates at `/home/whrho/git/oh-my-tau/vendor/` against `/tmp/oh-my-pi-ro/crates/` counterparts and report exact divergence.
**Actual Need**: Decide whether the vendored copies are stale/missing features, and how much work a resync would be (which call sites break).
**Success Looks Like**: Per-crate verdict (version lag, missing modules, changed public API) with absolute paths, so the caller can scope a port without re-investigating.
</analysis>

All diffs below were run in this session: `diff -rq` for file presence, `diff -r -w -B -u` for whitespace-insensitive semantics, plus `grep` on pub item lists and symbol presence.

# Headline

| Crate | Vendor version | RO version | Raw diff | Semantic diff (ignore whitespace) | Vendor LOC | RO LOC |
|---|---|---|---|---|---|---|
| pi-ast | 18.9.1 | 19.0.1 | 7277 | **3533** | 3591 | 9194 |
| pi-diff | 18.9.1 | 19.0.1 | 1671 | **643** | 921 | 1192 |
| pi-edit | 18.9.1 | 19.0.1 | 41451 | **10301** | 22493 | 35191 |
| pi-predict | 18.6.1 | 19.0.1 | 6405 | **429** | 4157 | 8291 |
| pi-vfs | 18.9.1 | 19.0.1 | 9753 | **1471** | 6045 | 7625 |
| pi-walker | 18.9.1 | **no counterpart** | n/a | n/a | 5737 | (xfs) 21015 |

RO HEAD is `v19.0.1-126-gb636e19d10`. Two structural facts dominate every pair:

1. **Reformatting, not divergence.** The vendored copies are 4-space-indented; RO uses `hard_tabs = true, tab_spaces = 3, max_width = 100` (`/tmp/oh-my-pi-ro/rustfmt.toml`). This inflates raw diffs 3-4x. Do not trust raw `diff` counts here.
2. **Crate renaming + workspace inheritance.** Every vendored crate is `pi-*` with a standalone `Cargo.toml` (edition 2024, literal versions, `description = "Vendored from can1357/oh-my-pi (MIT)"`). RO is `omp-*` with `version/edition/license/authors/homepage/repository.workspace = true`, `[lints] workspace = true`, and an added `xutf.workspace = true` dependency. There is no `pi-walker` in RO at all.

# Per-crate divergence

**pi-ast (18.9.1 -> 19.0.1): largest API break.**
RO-only files: `src/debug.rs` (139 LOC: `dump_tree`, `dump_sexp`, `dump_pattern`), `src/outline.rs` (543 LOC: `outline`, `Item`, `Member`, `SymbolType`, `EntryRole`), `src/search/` (2030 LOC: `find`, `find_source`, `rewrite_source`, `Discover`, `Candidate`, `Query`, `Finder`, `Page`, `Rewrite`, `Change`, `FileEdit`, `SourceEdit`), `src/testing.rs` (167), plus `BUILD.bazel`, `LICENSE`.
`ops.rs` was gutted: its remaining pub items are exactly `supported_lang_list`, `resolve_supported_lang`, `resolve_language`, `compile_pattern`, `apply_edits`. Gone entirely from the whole crate (0 occurrences): `AstMatch`, `AstMatchStrictness`, `MatchedFile`, `CompiledRewrite`, `compile_search_patterns`, `compile_rewrite_rules`, `collect_matches`, `collect_matched_files`, `has_glob_syntax`, `is_supported_file`. Their replacement is `ast::search`, and it is **not signature-compatible**: `rewrite_source(source: &str, language: SupportLang, rewrite: &Rewrite<'_>, cancel: &CancellationToken) -> Result<SourceEdit, Error>`, with matching via `find(fs, candidates, query)`.
Other semantic adds: `parse_cache` gains `MAX_RETAINED_BYTES = 64 MiB`, `MAX_ENTRY_SOURCE_BYTES`, and `parse_cached(code, lang, path)`; `summary` gains `parse_code_status` and `summarize_code(SummaryOptions) -> SummaryResult`; Cargo gains `default = ["wasm"]`, `wasm = ["tree-sitter/wasm", "dep:semver"]`.
Impact on tau: `/home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/ast.rs` imports `collect_matches, compile_rewrite_rules, compile_search_patterns, AstMatch` from `pi_ast::ops` (line 21-24) and calls the old shapes at lines 214, 222, 416, 433, 453. This is a rewrite of that tool, not a rename.

**pi-diff (18.9.1 -> 19.0.1): internal rewrite, memory fix.**
`Component { count, prev: Option<Rc<Self>> }` is replaced by `ComponentArena` (`ComponentNode { count: u32, prev: u32, refs: u32 }`, `NO_COMPONENT = u32::MAX`, intrusive free-list recycling). `PathState` becomes `Copy { old_pos: isize, last: u32 }`; `extract_common` takes `&mut ComponentArena`. Stated goal in the new doc comment: peak memory proportional to live frontier instead of all explored states. Also drops the local LF/UTF-16 splitting in favour of `xutf::lines::<xutf::Utf16>(text, xutf::LineEnds::Lf)`. Public surface looks unchanged; 438+/188-.

**pi-edit (18.9.1 -> 19.0.1): most semantic churn.**
RO-only: `tests/` (8054 LOC, 14 files + fixtures), `BUILD.bazel`. Adds `error::Rejection` enum (`StaleTag, UnknownTag, OtherFileTag, MissingTag, HeaderOnly, UnseenLines, OutOfRange, AmbiguousBoundary, NoChange, FileNotFound`) and `EditError::Rejected { reason, message }`, re-exported as `Rejection` from `lib.rs`. `HashlineEngine` gains `max_columns`; `patcher` gains `header_only_error`; `files.rs` gains `provide_recovery`, `take_recovery_misses`, `forget_answers`, `enforce_write`; `path_policy` gains `RecoveryGlob`; `store` gains `MAX_TAGGED_FILE_BYTES = 64 MiB`, `fork()`, `record_edit()`. All five prompt files were rewritten (hashline.md 27 lines, patch.md 31, apply_patch.md 21, replace.md 16, sloppy.md 4) - hashline now uses explicit `<ops>/<rules>/<example>` sections and adds column-clamping language. Symbol-level spot check of what tau uses (`EditStore`, `PathPolicy`, `canonical_key`, `seen_lines_from_body`, `normalize_to_lf`, `strip_bom`, `FileOp`, `ApplyOutcome`, `FileOutcome`, `EditMode`, `description`) shows all still present in RO, so tau's `omp-cli`/`omp-tui` edit call sites survive a bump; the behavioural deltas are `max_columns` and `Rejection`.

**pi-predict (18.6.1 -> 19.0.1): two whole engines missing.**
The vendored `Cargo.toml` even says so: "Ngram word-completion engine only; the smollm and apple engines are not vendored." RO adds `src/smollm/` (4257 LOC: `config, cpu, gguf, metal, search, session, tokenizer, preamble.txt`) and `src/apple/` (381 LOC: `appkit`), plus `scripts/build-web-prior.ts`. `Method` gains `SmolLm` and `Apple`; `Method::parse` maps `"smollm" -> SmolLm`, `"apple" -> Apple` where the vendor collapses both to `Ngram`; `open()` dispatches to `smollm::open`/`apple::open`. New deps: `candle-core`/`candle-nn` (metal), `flume`, `objc2 =0.6.4`, `objc2-app-kit`, `objc2-foundation`. The 429-line semantic diff in ngram/ is pure refactor (`ceil_char_boundary`, `array_windows`, reflow) - no behaviour change. tau's `/home/whrho/git/oh-my-tau/crates/omp-tui/src/word_completion/host.rs:34` has `_ => Some(Method::Ngram)`, so it silently degrades today.

**pi-vfs (18.9.1 -> 19.0.1): cancellation + scoped-open additions.**
RO adds `POLL_SLICE: Duration = 100 ms`, `reads_can_block`, `fd_reads_can_block`, `readable_within`, `wait_readable`, `File::is_cancelled`, `File::read_until_cancelled`; a whole `Scope`-based open path in `fs.rs` (`plan_scoped_open`, `native_open_scoped`, `native_open_scoped_async`, `native_read`, `may_access`, FIFO release/`release_attempt`/`helper_exited`); `Fs::with_provider`, `Fs::write`, `Fs::symlink`; `path::msys_drive` (Windows MSYS paths); `src/tests.rs`. Also `windows-sys` feature set widened and `native/windows.rs` reworked.

**pi-walker (18.9.1): dissolved, no 19.0.1 equivalent.**
`/tmp/oh-my-pi-ro/crates/walker` does not exist and the workspace `members` list has no walker entry. Traversal now lives in `crates/xfs/src/walk/` (`arena.rs, cache.rs, mod.rs, parallel.rs, tree.rs, tests.rs` = 4769 LOC) inside `omp-xfs`, which also absorbs glob, grep, ignore, pattern and watch (21015 LOC total). The API is a different design: vendor exports `WalkRequest, WalkOptions, WalkDetail, WalkOrder, WalkFilter, EntryVisitor, WalkBackend, WalkControl, WalkDecision, CollectedEntry, CachePolicy, parallel_for_each_init`; xfs exports `Tree, TreeOptions, TreeEntry, Walk, Options, Order, Follow, Batch, Event, Worker, Stop, StopOnDrop, Collection`. tau uses the vendor shape at `/home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/find.rs:12` (`WalkDetail, WalkRequest`), `search.rs:18`, and `search.rs:637` (`parallel_for_each_init`). A port means rewriting those call sites against `xfs::walk`, not bumping a version.

# Not divergence (checked and cleared)

- `ast/src/language/mod.rs` variant list is byte-identical in content (Just..Zig), only reflowed across lines.
- `predict/src/ngram/{mod,query,model,snapshot,structures,text,tests,web,vocab,hygiene}.rs` are refactors only.
- `EditMode` enum variants (`Replace, Patch, ApplyPatch, Hashline, Sloppy`) and `as_str` wire names are identical.
- `vfs/src/types.rs` and `error.rs` diffs are struct-literal reflow, no removed API.

<results>
<files>
- /home/whrho/git/oh-my-tau/vendor/pi-ast/Cargo.toml - 18.9.1, standalone manifest, no `wasm` feature; counterpart adds `default = ["wasm"]` + `dep:semver`
- /home/whrho/git/oh-my-tau/vendor/pi-ast/src/ops.rs - vendor home of `AstMatch`, `compile_search_patterns`, `collect_matches`, `compile_rewrite_rules`, `collect_matched_files`; all deleted in 19.0.1
- /tmp/oh-my-pi-ro/crates/ast/src/search/mod.rs - replacement API: `find`, `find_source`, `rewrite_source(&str, SupportLang, &Rewrite, &CancellationToken) -> Result<SourceEdit, Error>`, `Discover`, `Candidate`, `Query`
- /tmp/oh-my-pi-ro/crates/ast/src/outline.rs - new module, 543 LOC, absent from vendor
- /tmp/oh-my-pi-ro/crates/ast/src/debug.rs - new module, `dump_tree`/`dump_sexp`/`dump_pattern`, absent from vendor
- /tmp/oh-my-pi-ro/crates/ast/src/testing.rs - RO-only test scaffolding (167 LOC)
- /home/whrho/git/oh-my-tau/vendor/pi-diff/src/lib.rs - `Rc<Component>` chain implementation
- /tmp/oh-my-pi-ro/crates/diff/src/lib.rs - `ComponentArena` + `u32` indices + free-list, `xutf::lines::<Utf16>`; internal only
- /home/whrho/git/oh-my-tau/vendor/pi-edit/src/error.rs - no `Rejection` enum
- /tmp/oh-my-pi-ro/crates/edit/src/error.rs - adds `Rejection` + `EditError::Rejected { reason, message }`
- /tmp/oh-my-pi-ro/crates/edit/src/modes/mod.rs - `HashlineEngine { enforce_seen_lines, max_columns }` (vendor has no `max_columns`)
- /tmp/oh-my-pi-ro/crates/edit/src/store.rs - adds `MAX_TAGGED_FILE_BYTES` (64 MiB), `fork()`, `record_edit()`
- /tmp/oh-my-pi-ro/crates/edit/prompts/hashline.md - rewritten with `<ops>/<rules>/<example>` structure; vendor copy is the older flat prose
- /tmp/oh-my-pi-ro/crates/edit/tests/ - 8054 LOC of integration tests absent from vendor
- /home/whrho/git/oh-my-tau/vendor/pi-predict/src/lib.rs - `Method::{Ngram}` only; `parse("smollm"|"apple") -> Ngram`
- /tmp/oh-my-pi-ro/crates/predict/src/lib.rs - `Method::{Ngram, SmolLm, Apple}` with real dispatch
- /tmp/oh-my-pi-ro/crates/predict/src/smollm/ - 4257 LOC engine (config/cpu/gguf/metal/search/session/tokenizer) absent from vendor
- /tmp/oh-my-pi-ro/crates/predict/src/apple/ - 381 LOC `NSSpellChecker` engine absent from vendor
- /home/whrho/git/oh-my-tau/vendor/pi-vfs/src/file.rs - no cancellation-aware read helpers
- /tmp/oh-my-pi-ro/crates/vfs/src/file.rs - adds `POLL_SLICE`, `reads_can_block`, `fd_reads_can_block`, `readable_within`, `wait_readable`, `File::read_until_cancelled`
- /tmp/oh-my-pi-ro/crates/vfs/src/fs.rs - adds `Scope`-based `plan_scoped_open`/`native_open_scoped`/`native_read`/`may_access`, `Fs::with_provider/write/symlink`
- /tmp/oh-my-pi-ro/crates/vfs/src/path.rs - adds `msys_drive`
- /home/whrho/git/oh-my-tau/vendor/pi-walker/src/lib.rs - 5183 LOC `WalkRequest`/`WalkDetail`/`EntryVisitor` API, no 19.0.1 counterpart
- /home/whrho/git/oh-my-tau/vendor/pi-walker/src/cache.rs - 554 LOC, superseded by xfs walk cache
- /tmp/oh-my-pi-ro/crates/xfs/src/walk/ - new home: `mod.rs` (651), `tree.rs` (1195), `parallel.rs` (1214), `arena.rs` (120), `cache.rs` (231), `tests.rs` (1358)
- /tmp/oh-my-pi-ro/crates/xfs/Cargo.toml - `omp-xfs`, depends on `omp-vfs`; glob/grep/ignore/pattern/watch co-located
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/ast.rs - lines 21-24 and 214/222/416/433/453 bind the deleted `pi_ast::ops` API
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/find.rs - line 12 binds `pi_walker::{WalkDetail, WalkRequest}`
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/search.rs - lines 18 and 637 bind `pi_walker` + `parallel_for_each_init`
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/word_completion/host.rs - line 34 `_ => Some(Method::Ngram)` silently degrades smollm/apple
- /tmp/oh-my-pi-ro/rustfmt.toml - `hard_tabs = true, tab_spaces = 3`; explains the inflated raw diffs
</files>

<answer>
The vendored set is a 18.9.1-era snapshot (predict: 18.6.1) of an upstream now at 19.0.1+126 commits. Roughly two-thirds of every raw diff is indentation: vendor uses 4 spaces, upstream uses hard tabs, so semantic divergence is 3-4x smaller than a plain `diff -r` suggests. All six were also mechanically re-namespaced (`pi-*` -> `omp-*`) with workspace-inherited manifest fields and an added `xutf` dependency; that accounts for the Cargo.toml deltas everywhere.

Real divergence, ranked by migration cost:

1. pi-walker - no counterpart exists. Upstream deleted the crate and folded traversal into `crates/xfs/src/walk/` under a different API (`Tree`/`TreeOptions`/`Walk`/`Batch`/`Event`/`Worker` vs `WalkRequest`/`WalkDetail`/`EntryVisitor`). tau's `find.rs` and `search.rs` need rewriting, not re-pointing.
2. pi-ast - the largest public-API break. `AstMatch`, `compile_search_patterns`, `collect_matches`, `compile_rewrite_rules`, `collect_matched_files` and the old 3-arg `rewrite_source` are gone from the crate entirely; the replacement lives in `ast::search` with a different shape (`Rewrite` + `CancellationToken` -> `SourceEdit`). Upstream also gained three modules vendor lacks: `outline` (543 LOC), `debug` (139), `search` (2030).
3. pi-predict - vendor deliberately ships only the ngram engine (its own Cargo.toml says so); upstream added `smollm` (4257 LOC, candle + GGUF) and `apple` (381 LOC, NSSpellChecker) and real `Method` dispatch. tau currently falls back to `Ngram` for both names.
4. pi-edit - no breaking renames for the symbols tau touches, but behaviour changed: `HashlineEngine` gained `max_columns`, errors gained a typed `Rejection` taxonomy, and all five prompt files were rewritten (hashline is now sectioned `<ops>/<rules>/<example>`). Upstream also carries 8054 LOC of integration tests vendor does not.
5. pi-vfs - additive: 100 ms `POLL_SLICE`, cancellation-aware reads (`read_until_cancelled`, `readable_within`), `Scope`-based scoped opens, `Fs::write`/`symlink`/`with_provider`, `msys_drive`.
6. pi-diff - internal only. `Rc` chain replaced by an arena with intrusive free-list to bound peak memory; public surface unchanged.

Verdict: vendor is stale by one minor version plus 126 commits, but "stale" understates it for ast and walker, where a version bump is actually an API migration. diff and predict/ngram are safe to lift nearly verbatim; vfs and edit need a careful pass; ast and walker need call-site rewrites in `omp-cli`.
</answer>

<next_steps>
Scope the port in dependency order: (1) `pi-vfs` first since `xfs`/`walker` build on it; (2) decide `pi-walker` -> `crates/xfs/src/walk` before touching `omp-cli/src/tools/{find,search}.rs`, since that determines their rewrite; (3) rewrite `omp-cli/src/tools/ast.rs` against `omp_ast::search` (`Rewrite`, `Query`, `Candidate`, `find`, `SourceEdit`) - budget it as a feature port, not a rename; (4) take `pi-diff` and `predict/ngram` as near-drop-in; (5) carry the five rewritten `prompts/*.md` and add `max_columns` plumbing for hashline. If the goal is only to unblock current tau code, items 1-3 are the gating set and 4-6 can wait.
</next_steps>
</results>