<analysis>
**Literal Request**: Compare `/tmp/oh-my-pi-ro/packages/snapcompact`, `/tmp/oh-my-pi-omp2/crates/snapcompact`, and `/home/whrho/git/oh-my-tau/crates/omp-compact`; detail missing features, tokenization, handoff/compaction parity.
**Actual Need**: A decision-grade gap list for `omp-compact` (tau) relative to the two reference implementations, with concrete file/symbol pointers so porting work can start immediately.
**Success Looks Like**: Feature matrix, tokenization comparison, handoff/compaction flow parity, and every gap named with the file that holds the reference behavior.
</analysis>

<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/lib.rs - tau's module map + the doc comment that declares snapcompact/handoff "out of scope" (stale relative to shipped modules)
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/cut.rs - `plan_boundary`/`plan_region`/`plan_compaction`, `find_cut_point`, provider-ratio keep-recent adjustment
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/summary.rs - `build_summary_request`, `generate_summary` (window folding), `compact()` orchestration, `CompactionResult`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/dialect.rs - all 10 transcript dialects + `preferred_dialect` class matcher (1305 lines, largest parity surface)
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/serialize.rs - dialect-less `serializeConversation`, 2000-char tool-result truncation, boundary-tag escaping
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/handoff.rs - `render_handoff_prompt`, `generate_handoff_from_context`, `handoff_compaction_entry`, `append_handoff`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/native.rs - replay-only side of provider-native + snapcompact (`get_preserved_archive`, `history_blocks`, `elide_data_urls`)
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/context.rs - `build_session_context`/`_owned`/`_path`, compaction replay lanes, `strip_dangling_tool_calls`, `drop_failed_assistant_turns`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/tokens.rs - `Tokenizer` trait, `ByteEstimate` (`(bytes+3)>>2`), `count_message`, hardcoded `FRAME_TOKEN_ESTIMATE = 5024`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/apply.rs - `compaction_entry`, `append_compaction`, `synthetic_compaction_entry`, `project_tokens_after`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/settings.rs - threshold/reserve arithmetic, `should_compact`, `compaction_context_tokens`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/branch.rs - branch summarization (`BRANCH_SUMMARY_MAX_TOKENS = 2048`, 90% squeeze rule)
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/files.rs - `<files>` block, read-selector stripping, prefix-folded path tree
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/convert.rs - `convert_to_llm`, per-role shaping, `historyRewriteAt`
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/prompts.rs - prompt registry + `format()` post-render formatter
- /home/whrho/git/oh-my-tau/crates/omp-compact/prompts/handoff-document.md - handoff prompt (diverges from omp2's, see below)
- /home/whrho/git/oh-my-tau/crates/omp-compact/tests/handoff.rs - handoff end-to-end test with injected completer + golden prompt fixtures
- /home/whrho/git/oh-my-tau/crates/omp-compact/tests/native.rs - provider-native + snapcompact replay goldens (blob frames replay as gaps)
- /home/whrho/git/oh-my-tau/crates/omp-compact/tests/dialects.rs, tests/golden.rs, tests/units.rs - TS-oracle goldens (24 `#[test]` fns total)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/compact.rs - the harness glue; `methodOrder` without `soft` disables auto-compaction; only `method: "soft"` is ever committed
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/slash.rs - `/handoff` returns "not wired into the omt runtime"; `/compact snapcompact` accepted as a mode token but unimplemented
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/rpc.rs - `"handoff"` RPC returns `unsupported("Handoff")`
- /home/whrho/git/oh-my-tau/crates/omp-settings/src/schema.rs - `snapcompact.shape` / `.systemPrompt` / `.toolResults` settings exist (schema only, no consumer)
- /tmp/oh-my-pi-ro/packages/snapcompact/src/snapcompact.ts - the reference implementation (2721 lines, 120 tests): shapes, billing, normalize, wrap, planArchive foveation, historyBlocks, compact()
- /tmp/oh-my-pi-ro/packages/snapcompact/src/prompts/snapcompact-summary.md - the HISTORY/FILES reading-guide template
- /tmp/oh-my-pi-ro/packages/snapcompact/src/prompts/file-operations.md - `<files>` XML block template
- /tmp/oh-my-pi-ro/packages/snapcompact/test/snapcompact.test.ts - 2164-line test suite (the parity oracle for a port)
- /tmp/oh-my-pi-ro/packages/snapcompact/research/ - ~80 eval/visualization scripts (SQuAD recall evals that chose the shapes)
- /tmp/oh-my-pi-ro/packages/agent/src/tokenizer.ts - the real tokenizer: native utok counts, `countMessage`, `countLatestTurn`, `checkTokenBudget`, image pricing
- /tmp/oh-my-pi-ro/packages/agent/src/compaction/transcript-tokens.ts - provider-usage anchoring (count only the tail after the newest trustworthy usage)
- /tmp/oh-my-pi-ro/packages/agent/src/compaction/utils.ts - `truncateToolResultForSummary` (2000 chars) + `escapeSummaryBoundaryTags`
- /tmp/oh-my-pi-ro/packages/catalog/src/compat/image-tokenization.ts - per-lineage image billing rules (openai-patch / anthropic-patch / deepseek-patch / fixed)
- /tmp/oh-my-pi-ro/docs/handoff-generation-pipeline.md - the authoritative `/handoff` lifecycle doc
- /tmp/oh-my-pi-omp2/crates/snapcompact/src/lib.rs - pure-Rust renderer (1333 lines): BDF/hex/TTF fonts, sent/bw ink, dim spans, line repeat, doc columns, Lanczos resize, PNG encode
- /tmp/oh-my-pi-omp2/crates/snapcompact/src/archive.rs - provider-aware `resolve_shape`, `render_archive` with 0.9 savings admission, `elide_data_urls`
- /tmp/oh-my-pi-omp2/crates/snapcompact/src/fonts/ - 5x8.bdf, 6x12.bdf, 8x13.bdf, unscii-8.hex, Silver.ttf
- /tmp/oh-my-pi-omp2/crates/agent/src/directors/compaction.rs - omp2's compaction director: handoff-document summarizer, byte-based token estimation
- /tmp/oh-my-pi-omp2/crates/agent/prompts/compaction/handoff-document.md - omp2's handoff prompt (shorter, no `<critical>`/`<output>` scaffolding)
- /tmp/oh-my-pi-omp2/crates/journal/src/data.rs - `Compaction` payload with `frames: Vec<Attachment>`, `MAX_SNAPCOMPACT_FRAMES = 80`, `MAX_SNAPCOMPACT_FRAME_BYTES = 3_000_000`
- /tmp/oh-my-pi-omp2/crates/agent/src/prompt/projection.rs - resolves journaled frame blobs; a missing frame is dropped while summary text survives
</files>

<answer>

## The three implementations are not the same kind of thing

- **`/tmp/oh-my-pi-ro/packages/snapcompact` (TS)** — the complete reference: renderer + normalizer + layout planner + archive persistence + full `compact()` pass + ~80 research scripts + 120 tests.
- **`/tmp/oh-my-pi-omp2/crates/snapcompact` (Rust)** — renderer **only**. Its own README states the split: "text normalization, wrapping, and provider request construction remain caller responsibilities." 7 tests, 2042 lines. It is a faithful `text -> PNG` engine plus a shape/billing/`render_archive` layer, and nothing else.
- **`/home/whrho/git/oh-my-tau/crates/omp-compact` (Rust)** — the *summarization and context-rebuild* engine. It has no renderer, no normalizer, no layout planner, and (in the harness) no non-`soft` method.

So "missing features" splits into two independent answers: what tau lacks versus the TS reference, and what tau lacks versus the omp2 Rust crate.

## Feature matrix

| Capability | TS `snapcompact` | omp2 `snapcompact` | tau `omp-compact` |
|---|---|---|---|
| Cut-point selection | in `packages/agent/src/compaction/compaction.ts` | no | **yes** — `cut.rs` (`find_cut_point`, `plan_boundary`, `plan_region`) |
| LLM summary requests | yes | no | **yes** — `summary.rs` (`build_summary_request`, `build_turn_prefix_request`, `build_short_summary_request`) |
| Windowed/folding summarization | yes | no | **yes** — `generate_summary` + `plan_summary_windows` + `clamp_conversation_to_budget` |
| Per-model transcript dialects | yes (`packages/ai` dialect tables) | no | **yes** — `dialect.rs`, all 10 dialects + `preferred_dialect` |
| `<files>` block / path tree | yes | no | **yes** — `files.rs` |
| Compaction entry + commit | yes | no | **yes** — `apply.rs` |
| Context rebuild w/ all lanes | yes | no | **yes** — `context.rs` (local, remote, Anthropic, snapcompact, branch, reset) |
| Handoff generation | yes | yes (as its default summarizer) | **yes as a library** (`handoff.rs`), **no live wiring** |
| Branch summarization | yes | no | **yes** — `branch.rs` |
| Bitmap renderer | yes (native N-API) | **yes** (`lib.rs`) | **no** |
| Text normalization | **yes** (`normalize`, `CHAR_FOLD`, `EMOJI_FOLD`, NFKD) | no (caller's job) | **no** |
| Cell-aware wrapping/pagination | **yes** (`wrap`, `paginateCells`, `docPages`) | partial (`take_frame_end`) | **no** |
| Foveated layout planner | **yes** (`planArchive`, HQ/LQ/HQ, `HQ_EDGE_FRAMES = 3`) | no | **no** |
| Shape selection by model lineage | **yes** (`resolveShape`, `idealShapeVariant`, `MODEL_VARIANTS`) | partial (`resolve_shape`, string `contains` on model id) | **no** |
| Frame billing from catalog rules | **yes** (`frameBilling`, `frameTokens`, `plannedFrameTokens`) | partial (`billed_tokens`, hardcoded formulas) | **no** (hardcoded `FRAME_TOKEN_ESTIMATE = 5024`) |
| Archive persistence | **yes** (`Archive.text/textHead/textTail/totalChars/truncatedChars`) | partial (`Archive` has no text regions, `truncated_chars` always 0) | **replay only** (`native.rs` reads it) |
| `withoutThinking` archive variant | **yes** | no | **no** |
| `viewWithoutFrames` / `viewWithoutThinking` | **yes** | no | **no** |
| `capFrameDataBytes` | **yes** | no | **no** |
| `stripPreservedArchive` | **yes** | no | **no** |
| `renderabilityProbeText` / `scanRenderability` | **yes** | partial (`snapcompact_supported_chars`) | **no** |
| Savings admission (0.9 margin) | via `plannedFrameTokens` | **yes** (`SAVINGS_MARGIN`) | **no** |
| Blob-store frame resolution | **yes** (`resolveFrameData`, `LazyFrameData`) | via journal CAS | **no** — every `blob:sha256:` frame becomes an "unavailable" gap |

## Tokenization — the sharpest divergence

**TS reference** (`/tmp/oh-my-pi-ro/packages/agent/src/tokenizer.ts`):
- Native exact counts via embedded utok (`natives.countTokens`), model-catalog-resolved encoding; unknown families fall back to `o200k_base`, not bytes/4.
- `countTokens(text, mode)` with `approximate` / `upperbound` / `strict` modes; `byteEstimate` = `(Buffer.byteLength + 3) >> 2` only as the approximate fallback and only in tests.
- `countMessage` is memoized per message with a `version` invalidated by prune/shake/strip-images; `excludeEncryptedReasoning` is a separate cache slot.
- `countLatestTurn` charges the surviving final assistant turn's reasoning (Anthropic replays it byte-for-byte).
- `checkTokenBudget` uses byte length as a free upper bound, then pays for an exact count only when ambiguous.
- Image pricing goes through `imageTokens(size, detail)` → the catalog `image-tokenization` cascade (`/tmp/oh-my-pi-ro/packages/catalog/src/compat/image-tokenization.ts`): openai-patch (32px patches × multiplier, per-`detail` sizing), anthropic-patch (28px, `maxEdge`/`maxTokens`), deepseek-patch, or `fixed`.
- `transcript-tokens.ts` anchors on the newest trustworthy assistant `usage` and tokenizes only the tail — long sessions don't pay O(n) per turn.

**omp2 Rust**: no tokenizer at all. `crates/agent/src/directors/compaction.rs` uses `BYTES_PER_TOKEN = 4`, `MESSAGE_OVERHEAD_TOKENS = 4`, `MEDIA_OVERHEAD_TOKENS = 256`, and `div_ceil` byte arithmetic throughout (`estimate_request_tokens`, `estimate_message_tokens`, `estimate_text_tokens`). `render_archive` takes `source_tokens` as a caller-supplied number for its 0.9 admission test.

**tau `omp-compact`** (`src/tokens.rs`):
- `Tokenizer` trait with `count_text` + `count_image`; `ByteEstimate` is `(text.len() as u64 + 3) >> 2` — **always bytes/4, and it is the only implementation**; `omp-cli/src/compact.rs` hardcodes `let tokenizer = ByteEstimate;`.
- `count_message` faithfully ports `Tokenizer.countMessage`'s role dispatch (bashExecution, assistant text/thinking/toolCall/redactedThinking/anthropicServerTool, user/developer/custom/hookMessage/toolResult, branchSummary/compactionSummary with frame blocks) and `exclude_encrypted_reasoning`.
- `count_image` is a **3-bucket guess** on `detail` (low 308 / high 3000 / else 12000) — not pixel-size decoding, not the catalog rule.
- `FRAME_TOKEN_ESTIMATE = 5024` is a bare constant applied to every non-text block in a `compactionSummary`.
- **Missing entirely**: native/exact tokenization, per-model encodings, memoization, `countLatestTurn`, `checkTokenBudget`'s free byte bound is present only as `fits_budget` in `summary.rs`, usage-anchored tail counting, and catalog-driven image billing.

Practical consequence: tau's token arithmetic is uniformly conservative-but-crude. It will over- or under-trigger compaction on non-ASCII-heavy or thinking-heavy sessions, and it cannot price a snapcompact frame per reader lineage even though the archive replay path exists.

## Handoff parity

**What tau has** (`src/handoff.rs`, 183 lines): `render_handoff_prompt` (with handlebars `{{#if additionalFocus}}` semantics replicated by hand), `build_handoff_request`, `generate_handoff_from_context`, `handoff_summary_from_document` (cumulative `<files>` upsert + derived `details`), `handoff_compaction_entry` (wire order `summary, firstKeptEntryId, tokensBefore, tokensAfter, method, details, fromExtension`, `shortSummary` omitted), `append_handoff`, `handoff_result`. `convert.rs` renders `method: "handoff"` summaries through `render_handoff_summary_context`. `tests/handoff.rs` exercises the whole path end-to-end with a golden prompt fixture.

**What tau is missing on handoff**:
1. **No live wiring.** `omp-cli/src/slash.rs:2999` `/handoff` returns "not wired into the omt runtime"; `omp-cli/src/rpc.rs:1022` `"handoff"` returns `unsupported("Handoff")`. The library is complete but unreachable.
2. **No side-request pipeline.** The TS doc (`docs/handoff-generation-pipeline.md`) specifies `agent.buildSideRequestContext`, a pinned base system prompt (so the committed summary never inherits a `before_agent_start` override), secret obfuscation, a unique side `sessionId`, `preferWebsockets: false`, `initiatorOverride: "agent"`, and OTEL instrumentation. tau injects a bare `Completer` and leaves all of that to the caller.
3. **No `toolChoice: "none"` → `"auto"` compatibility retry** on providers that only accept automatic tool choice.
4. **No `compaction.handoffSaveToDisk`** artifact write (`handoff-*.md`).
5. **Prompt drift.** `diff` shows tau's `prompts/handoff-document.md` still has the older `<critical>` + `<output>` structured-template scaffolding; omp2's `crates/agent/prompts/compaction/handoff-document.md` is a shorter rewrite without them. `handoff-summary-context.md` is byte-identical between tau and omp2.
6. **`lib.rs` doc comment is stale** — it says "snapcompact, handoff and shake are out of scope" while `handoff.rs` and `native.rs` both ship.

## Compaction parity

**At parity or better than expected**: cut-point selection including split turns and the non-message-entry pull-in; the provider-ratio keep-recent adjustment (`plan_region`); iterative summary update via `<previous-summary>`; split-turn turn-prefix summary; short summary; `<files>` upsert; `project_tokens_after` via synthetic-entry rebuild; all three provider-native replay lanes (OpenAI remote replacement history, Anthropic native with `historyRewriteAt` predating, snapcompact blocks); `reset_boundary`; context notes; dangling-tool-call stripping; aborted/errored turn dropping (with a 20,000-case randomized equivalence test against the reference implementation).

**Missing versus the TS reference** (`summary.rs` header lists some of this explicitly):
- Remote-compaction endpoint and the commit side of provider-native compaction ("harness work, stays out of scope").
- `experimental-context-rollover` recovery — returns `ContextError::Unsupported` instead of re-asking the discarded turn prefix.
- Overflow-halving retry and transient-error retry on the oneshot (left to the `Completer`).
- Split-turn history and prefix summaries run **sequentially**, not in parallel.
- `convert.rs` gaps it names itself: steering/user-invoked-skill custom messages, image-bearing custom-message splitting, `fileMention`, interrupted-thinking demotion, `xd://` tool-name canonicalisation, `OutputMeta` notice on bash/python text.
- The transcript/display variants of `buildSessionContext` are not ported.

## What a snapcompact port into tau would actually require

In dependency order, each currently absent from `omp-compact`:

1. **A renderer.** Either port `/tmp/oh-my-pi-omp2/crates/snapcompact/src/lib.rs` + `archive.rs` + `fonts/` (5x8.bdf, 6x12.bdf, 8x13.bdf, unscii-8.hex, Silver.ttf), or depend on it. tau has no `fontdue`/`png` dependency anywhere — `omp-compact/Cargo.toml` is only `omp-session`, `pa-types`, `serde_json`, `anyhow`, `regex`. `omp-cli` has the `image` crate, but only for downscaling inbound attachments (`src/images.rs`).
2. **A normalizer.** TS `normalize` (`CHAR_FOLD`, `EMOJI_FOLD`, `COLLAPSIBLE`, `UNRENDERABLE`, `COMBINING_MARKS`, NFKD, newline→`\u2588`) has no counterpart anywhere in tau.
3. **Cell-aware pagination.** `wrap`, `paginateCells`, `docPages`, `charCells`, `usesWideCells` — none present.
4. **A layout planner.** `planArchive`'s text-edges + imaged-middle + HQ/LQ/HQ foveation with `HQ_EDGE_FRAMES = 3` and oldest-middle drop. Nothing analogous exists.
5. **Shape + billing tables.** `SHAPE_VARIANTS` (17 variants) exists in tau *only* as an enum in `omp-settings/src/schema.rs:2865` and as TUI option labels in `omp-tui/src/overlays/settings_defs.rs:1765` — no runtime consumer.
6. **Archive write side.** `native.rs` can *read* `Archive` but nothing *produces* one; `CompactionResult.preserve_data` only ever forwards a previous `snapcompact` slot (`summary.rs:491`).
7. **Replay fidelity upgrades.** `native.rs` lacks `resolveFrameData`, `capFrameDataBytes`, `stripPreservedArchive`, `withoutThinking`, `viewWithoutFrames`, `viewWithoutThinking`, `renderabilityProbeText`, and `totalChars`; every `blob:sha256:` frame degrades to an "unavailable" gap, and `get_preserved_archive` never validates `totalChars`.

## Harness-level gaps (the real blocker)

`/home/whrho/git/oh-my-tau/crates/omp-cli/src/compact.rs` only ever commits `method: "soft"` (`ApplyOptions { method: Some("soft") }`) and hardcodes `ByteEstimate`. Its `settings_from_config` disables automatic compaction outright when `methodOrder` lacks `soft` — so the schema default `["remote","snapcompact","handoff","shake","soft"]` still lands on `soft`, but a user who prefers snapcompact gets nothing. `shake` does exist separately (`omp-cli/src/features/shake.rs`, `/shake`), so the pattern for adding a method is established.

</answer>

<next_steps>
1. Fix the stale claim first: `omp-compact/src/lib.rs`'s header says snapcompact/handoff/shake are out of scope while `handoff.rs` and `native.rs` ship. Correct it to describe the actual split (summarize + rebuild ported; render + commit-side provider compaction not).
2. Decide the renderer question before any snapcompact work: depend on `omp-snapcompact` (port `/tmp/oh-my-pi-omp2/crates/snapcompact` wholesale, including `src/fonts/`) or reimplement. The omp2 crate is already the pure-Rust equivalent of the TS native module, so depending on it is the small path; note it deliberately omits normalization and wrapping, which tau would still have to write.
3. If snapcompact is in scope, port in this order — normalizer, then `wrap`/`paginateCells`/`docPages`, then `planArchive`, then the `Archive` write side, then the `native.rs` replay upgrades (`resolveFrameData`, `withoutThinking`, `capFrameDataBytes`). Each has a direct reference: `/tmp/oh-my-pi-ro/packages/snapcompact/src/snapcompact.ts` at the line numbers in the feature matrix, with `test/snapcompact.test.ts` as the oracle.
4. Wire handoff before snapcompact — it is cheaper and already complete as a library. `handoff.rs` + `tests/handoff.rs` pass today; what is missing is only the caller in `omp-cli/src/slash.rs:3002` and `omp-cli/src/rpc.rs:1022`, plus the side-request details from `docs/handoff-generation-pipeline.md` (pinned system prompt, `toolChoice: none` retry, side `sessionId`).
5. Treat tokenization as its own track: `ByteEstimate` being the only `Tokenizer` makes every threshold decision crude. Add an exact-count implementation behind the existing trait (`count_text` is already the right seam) and replace `count_image`'s 3-bucket guess and the flat `FRAME_TOKEN_ESTIMATE = 5024` with catalog-driven rules — the reference is `packages/catalog/src/compat/image-tokenization.ts` plus `tokenizer.ts`'s `imageTokens`.
6. Reconcile the handoff prompt: tau's `prompts/handoff-document.md` carries older `<critical>`/`<output>` scaffolding that omp2 has dropped. Pick one and regenerate `tests/fixtures/prompts/handoff-document-{none,focus}.txt`.
</next_steps>
</results>