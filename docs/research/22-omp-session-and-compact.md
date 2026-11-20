<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-session/Cargo.toml - deps: pa-types, serde_json (float_roundtrip, raw_value), memchr=2.8.3, memmap2=0.9.11, rayon; no dev-deps
- /home/whrho/git/oh-my-tau/crates/omp-session/src/lib.rs - 1583 LOC: load/read/read_values, SessionWriter (create/open/branch_to/append_*/append_seeded_line/update_title_slot), roundtrip checker, JS number formatter, 15 unit tests
- /home/whrho/git/oh-my-tau/crates/omp-session/src/entries.rs - 680 LOC: 16 typed `OmpEntry` kinds with omp key order, TITLE_SLOT_BYTES=256, STRICT_KINDS
- /home/whrho/git/oh-my-tau/crates/omp-session/src/tree.rs - 425 LOC: build_tree, leaf_id, branch_path, entries_for_branch_summary, branch_summary_entry, labels_in_effect
- /home/whrho/git/oh-my-tau/crates/omp-session/src/index.rs - 553 LOC: lazy index (line/offset/len/id/parentId/kind) + load_branch for the active leaf only
- /home/whrho/git/oh-my-tau/crates/omp-session/src/scan.rs - 300 LOC: mmap >1MiB, rayon parallel parse >4MiB, zero-copy head_of
- /home/whrho/git/oh-my-tau/crates/omp-session/tests/tree_parity.rs - 68 LOC golden test vs TS `getTree()` digest
- /home/whrho/git/oh-my-tau/crates/omp-session/tests/fixtures/branched-session.jsonl - real redacted branched journal
- /home/whrho/git/oh-my-tau/crates/omp-session/tests/fixtures/branched-session.expectation.json - TS-produced leafId + flat tree digest
- /home/whrho/git/oh-my-tau/crates/omp-session/examples/roundtrip.rs - 113 LOC round-trip checker example
- /home/whrho/git/oh-my-tau/crates/omp-compact/Cargo.toml - deps omp-session, pa-types, serde_json, anyhow, regex; dev tokio
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/cut.rs - 517 LOC: find_cut_point, is_turn_start_entry, plan_boundary, plan_region, plan_compaction
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/settings.rs - 175 LOC: thresholds, reserve, should_compact
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/summary.rs - 498 LOC: summary/turn-prefix/short-summary requests, window folding, `compact()`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/apply.rs - 168 LOC: compaction_entry, append_compaction, synthetic_compaction_entry, project_tokens_after
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/context.rs - 986 LOC: build_session_context[_path|_owned], Leaf, remote/anthropic/snapcompact replay lanes
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/branch.rs - 261 LOC: branch summary collection/prep/request/finish/entry
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/handoff.rs - 183 LOC: handoff prompt render + compaction commit
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/native.rs - 569 LOC: openai_remote_payload, anthropic_compaction_payload, snapcompact archive/history_blocks replay
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/dialect.rs - 1305 LOC: per-model transcript dialects
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/{convert,messages,files,serialize,prompts,tokens,json}.rs - 265/159/289/185/168/166/100 LOC support layers
- /home/whrho/git/oh-my-tau/crates/omp-compact/prompts/*.md - 12 prompt templates ported from TS
- /home/whrho/git/oh-my-tau/crates/omp-compact/tests/golden.rs - 4 tests vs TS buildSessionContext + convertToLlm + prepareCompaction
- /home/whrho/git/oh-my-tau/crates/omp-compact/tests/{native,units,handoff,dialects}.rs - 5/11/2/2 tests, all TS-golden backed
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/compact.rs - the only consumer wiring compaction (settings load, rebuild_context, LiveCompleter, compact_now, auto_check)
- /home/whrho/git/oh-my-tau/crates/omp-stats/src/gain/snapcompact.rs - only other snapcompact touchpoint: reads `snapcompact-savings.jsonl` for stats
</files>

<answer>
**Size.** omp-session: 3541 src LOC + 68 test LOC + 113 example. omp-compact: 6065 src LOC + 803 test LOC. Total ~10.6k LOC. Consumers: omp-cli (both), omp-judge + omp-tui (session only).

**JSONL v3 parity - high, and deliberately byte-exact.** The crate is a real port, not a sketch. `SESSION_VERSION = 3`; header is `{type:session,version,id,timestamp,cwd}`; line 1 may be the fixed 256-byte `{"type":"title","v":1,...}` slot, patched in place via `write_all_at(0)` (or atomically prepended for pre-titling sessions). Loading mirrors `parseJsonlLenient`: blank lines ignored, non-object records skipped and reported with 1-based line numbers, invalid UTF-8 decoded lossily, mmap above 1 MiB and rayon chunked parse above 4 MiB. Writing mirrors `SessionManager`: 8-hex ids (`crypto.randomUUID().slice(-8)`), 100 collision retries then 16-hex, `parentId` chaining to the current leaf. The strongest parity signal is `entries.rs`: 16 typed entry kinds with omp's exact key order, explicit-null vs absent distinction (`nullable`/`keep_null`), and a hand-written JS `Number#toString` formatter including the round-half-to-even tie fix — so parse->serialize is byte-identical, verified by a `RoundTrip::Exact` corpus covering 17 real omp shapes. Resume is also optimized beyond TS: `append_seeded_line` rewrites only top-level `id`/`parentId`/`firstKeptEntryId`/`providerReplayThroughEntryId` by byte span, never building a `Value` DOM for a seeded branch.

**Compaction logic.** Three-layer split that is cleaner than the TS original: `plan_boundary` works over content-free `BoundaryEntry` heads (so a 40 MB journal never needs a full DOM), `plan_region` does the cut pass over the owned region only, `plan_compaction` is the in-memory composition. Cut selection is a faithful port: `find_valid_cut_points` excludes tool results, the newest group is always kept even if over budget, non-message entries immediately before the cut are pulled in, split turns are detected and their prefix summarized separately, and `keep_recent_tokens` is additionally scaled down by the provider-reported/estimated prompt ratio. Boundary handling covers `reset_boundary` clearing a prior summary, `firstKeptEntryId` re-expansion for local/Anthropic lanes, and `providerReplayThroughEntryId` re-expansion plus the post-snapshot appended interval for remote. `settings.rs` reproduces the 15%-of-window reserve floor, the defaulted-vs-explicit reserve resolution, and the -1 sentinel threshold semantics. `apply.rs` writes the entry and projects `tokensAfter` by rebuilding context with a synthetic compaction entry. `context.rs` rebuilds the model view for all four lanes (local, OpenAI remote replacement history, Anthropic native with `historyRewriteAt` predated to just before the first retained entry, snapcompact frame blocks) plus reset boundaries, mode/model/thinking state, TTSR rules, context notes, dangling-tool-call stripping and error-turn dropping.

**Branching/forking.** Branching is implemented as the TS tree semantics, not as file copying. `tree.rs` ports `SessionEntryIndex.tree` (orphans and self-parents become roots, children sorted by timestamp), `pathTo`/`getBranch` with a seen-set cycle guard, `collectEntriesForBranchSummary` down to the deepest common ancestor, and `branchWithSummary`'s exact key order with `fromId: "root"`. On the writer side `branch_to` retargets the append point without touching the file (durable only once an entry lands), `open(path, leaf)` reopens at an arbitrary tip, and `append_branch_summary` commits the marker. `omp-compact/src/branch.rs` adds the summary request/commit path (2048-token budget, 90% squeeze rule for nested summaries, tool-result truncation, `<files>` upsert). A real branched journal is pinned against a TS-generated `getTree()` digest in `tree_parity.rs`.

**Gaps vs omp.**
1. Snapcompact is read-only. `native.rs` replays `preserveData.snapcompact` (frames, textHead/textTail, 3 MB `FRAME_DATA_BYTES_BUDGET`, legacy crash-risk guard, `elide_data_urls`) but there is no frame capture, no archive production, and no blob-store resolver - every `blob:sha256:` frame replays as an "unavailable" gap marker (the test asserts exactly that). The commit side of snapcompact does not exist.
2. No `shake` module at all; no remote/Anthropic-native compaction request execution (explicitly "harness work, out of scope").
3. Handoff is implemented but unwired: `omp-compact/src/handoff.rs` is complete, yet `/handoff` in omp-cli returns "not wired into the omt runtime".
4. `experimental-context-rollover` compactions are rejected with `ContextError::Unsupported` (rollover request recovery unported).
5. `summary.rs` drops the overflow-halving retry, the transient-error retry of the oneshot, and the remote-compaction endpoint; split-turn summaries run sequentially instead of in parallel.
6. `context.rs` does not port the transcript/display variants of `buildSessionContext`.
7. CLI gating: a `methodOrder` without `soft`/`context-full` silently sets `strategy = "off"`, so any config leading with `remote`/`snapcompact` disables automatic compaction entirely. Default `methodOrder` in `omp-settings` is `["remote","snapcompact","handoff","shake","soft"]`, so the default config does auto-compact only via the `soft` fallback.
8. No fork (session-file copy), no session list/delete, and no `label` entry writer in omp-session - labels are read (`labels_in_effect`) but never written.
9. `entries.rs` does not model `child_usage_attributed` or `session_info`, and `index.rs`'s `EntryKind` omits `session_init`, `model_usage`, `title_change`, `credential_pin` (they fall to `Other`).
10. `tree_parity.rs` references `reports/SessionTree.md`, which does not exist in the repo.
</answer>

<next_steps>
Ready to proceed. If you plan to close the largest gap, start with snapcompact commit-side (frame capture + archive write into `preserveData.snapcompact` + a blob resolver), since the entire replay path already exists and is golden-tested. Second priority is wiring `handoff` into `/handoff` in omp-cli, which is a call-site change with no new logic. The `methodOrder`-without-`soft` behavior is worth confirming as intentional before touching it - it currently makes the shipped default config depend on the `soft` fallback.
</next_steps>
</results>