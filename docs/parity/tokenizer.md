# Tokenizer parity: `omp-compact` token counts vs the TS `Tokenizer`

Status of local token counting (Phase 4b). Measured from the code in this tree:

- `vendor/omp-utok` - upstream `crates/natives/src/utok` (the encoder behind TS
  `natives.countTokens`), vendored at `efcbef6dd0` (see `vendor-src.lock`)
- `crates/omp-compact/src/tokens.rs` - `Tokenizer` trait, `ByteEstimate`, `FamilyTokenizer`
- `crates/omp-cli/src/compact.rs`, `crates/omp-cli/src/runtime.rs` - compaction wiring
- TS reference: `packages/agent/src/tokenizer.ts`, `packages/catalog/src/model-tokenizer.ts`

## 1. Families

`FamilyTokenizer::new(model.resolved_tokenizer())` picks the encoder from the model's
`ModelTokenizer` (catalog `tokenizer`, else inferred from the model id).

| `ModelTokenizer` | Encoder | Status | Verified against |
| --- | --- | --- | --- |
| `o200k` | `O200kBase` byte-level BPE | exact | python tiktoken 0.14.0 fixtures + tiktoken-rs 0.12 differential (random strings) |
| `cl100k` | `Cl100kBase` byte-level BPE | exact | same as `o200k` |
| `qwen3` | `Qwen3` BPE + NFC | exact | HF tokenizers fixtures (Qwen3.8 `tokenizer.json`) |
| `deepseek-v3` | `DeepSeekV3` BPE, 3-stage split | exact | HF tokenizers fixtures (DeepSeek V4 `tokenizer.json`, base BPE identical V3..V4) |
| `kimi-k2` | `KimiK2` BPE | exact | tiktoken fixtures with the `tokenization_kimi.py` pattern |
| `glm5` | `Glm5` BPE, `ignore_merges` | exact (GLM-5), near-exact (GLM-4.x) | HF tokenizers fixtures |
| `claude-v3` | ctok v3 reconstruction | count-only reconstruction | 493 ctok 1.0.0 reference counts |
| `claude-v47` | ctok v4.7 reconstruction | count-only reconstruction | same corpus |
| `claude-v5` | v4.7 vocabulary, opus-5 frame | count-only reconstruction | same corpus |
| `claude-v5-sonnet` | v4.7 vocabulary, sonnet/fable-5 frame | count-only reconstruction | 63 live `count_tokens` rows |
| unknown (`None`) | `ByteEstimate` `(bytes + 3) >> 2` | estimate | - |

Counts are content tokens (`encode_ordinary`: no special tokens, no message frame), summed
per fragment like TS `countTokens(string[])`. Image blocks keep the family's
`ImageBilling` (OpenAI / DeepSeek patch schemes; unknown-size budgets otherwise).
The scanners classify every scalar against UCD 16.0.0 (`vendor/xutf` pins
`unicode-properties` / `unicode-script`); `omp-utok`'s exhaustive sweep compares each
class with `regex-syntax` on all 0x110000 codepoints.

## 2. Wiring

| Surface | Counter |
| --- | --- |
| Post-turn threshold floor (`estimate_context_tokens`, `Runtime::auto_compaction_due`) | active model's `FamilyTokenizer` |
| Compaction region plan / keep-recent cut (`plan_region`) | active model's `FamilyTokenizer` (`CompactParams::tokenizer`) |
| Summary input budget (`SummaryParams::tokenizer`) | same |
| `tokensAfter` projection (`project_tokens_after`) | same |

## 3. Known deviations from TS

- **Unknown family:** TS counts in `o200k_base`; this port keeps `ByteEstimate`
  (Phase 4b decision).
- **Jev:** the TypeSafe Jev encoding is not vendored (no `ModelTokenizer` variant).
- **Memoization / usage-anchored tail counting:** TS caches native counts per fragment
  (LRU) and per message and counts only the tail after the newest trustworthy usage;
  the Rust threshold check re-counts the whole live context each turn.
- **Other estimators:** `runtime.rs` `estimate_tokens` (context-usage footer, chars/4),
  the `non_message_tokens` prompt overhead (chars/4) and `tools/rlm.rs` still estimate
  by length.
