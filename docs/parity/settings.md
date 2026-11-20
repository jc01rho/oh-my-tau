# Settings parity: `omp-settings` vs the TS registry

Status of the `config.yml` settings surface in this tree. Everything below is measured
from the code in `crates/omp-settings` plus every literal setting-id reference in
`crates/**/*.rs` (excluding `crates/omp-settings/src/` itself, which is the registry).

Sources read for this document:

- `crates/omp-settings/src/schema.rs` - generated registry (548 `SettingDef` entries)
- `crates/omp-settings/src/settings.rs` - layered store, accessors, `write_global`
- `crates/omp-tui/src/overlays/settings_defs.rs` - generated settings-panel rows (379)
- `crates/omp-tui/src/app.rs`, `crates/omp-tui/src/port.rs`, `crates/omp-cli/src/tui.rs` - live apply + persist path
- `docs/research/06-memory-context-gap.md` - memory/context subsystem gap
- `docs/research/24-omp-settings-sot-stats-urls.md` - crate-by-crate capability map

## 1. How the store works

`Settings::load(cwd)` (`settings.rs`) merges four layers, later winning:

1. TS defaults - every `default_yaml` in the registry, parsed once per process
   (`defaults_layer()` behind a `OnceLock`).
2. Global `~/.omp/agent/config.yml` (or the `OMP_PROFILE` profile's agent dir).
3. Project `<cwd>/.omp/config.yml`, then `.omp/config.yaml`.
4. Environment overrides - per-setting `env:` names from the registry.

Merge semantics: objects merge recursively, `null` in an upper layer means "unset"
(lower layer stands), unknown keys are preserved verbatim and reported via
`unknown_keys()`. `write_global()` is the only writer: it type-checks against the
registry `ty`, treats JSON `null` as unset, and rewrites the global file in place
preserving unknown keys. The TUI persists through it (`port.rs:186`,
`omp-cli/src/tui.rs:658`) and then hot-applies a short allowlist
(`app.rs:1981 apply_setting_live`, 20-odd ids: theme, thinking level, status line,
composer, display prefs, magic keywords).

Recognising a setting is therefore *not* the same as honouring it: the store will
return a value for all 548 ids, while a consumer has to read it to have any effect.

## 2. Declared inventory

| Measure | Count |
| --- | --- |
| `SettingDef` entries (`schema.rs`) | **548** |
| ...with an `env:` override | 35 |
| ...marked `credential: true` | 9 |
| Panel rows (`settings_defs.rs`) | 379 |
| Registry ids with no panel row | 169 |
| Panel ids not in the registry | 0 |

Registry provenance: `schema.rs` header states it is generated from omp TS v18.6.1
`register({...})` definitions (`config/` + `*/settings.ts`) and must not be edited by
hand. `settings_defs.rs` is likewise generated from the TS panel
(`createSettingsHost + getAllSettingDefs`, 379 rows in TS panel order). Both are
portable as *data*; neither carries behavioural parity on its own.

Value types: `Boolean`, `Number`, `String`, `Enum(values)`, `Array`, `Record`. An
`Enum` with an empty value list means "choices derived at runtime" (voices, devices,
status-line presets); `write_global` and `parse_env_value` accept any non-blank string
for those.

### Env overrides (35)

Applied last, so a set variable beats both config files. Parsing mirrors TS
`parseFlag`: booleans accept `1/y/true/yes/on` (any case) as true and any other
non-blank text as false, empty is unset; numbers must parse; enums must be listed;
strings/arrays/records must be non-blank.

Families: `hindsight.*` (17), `mnemopi.*`, `edit.*` (`PI_EDIT_FUZZY`,
`PI_EDIT_FUZZY_THRESHOLD`, `PI_EDIT_VARIANT`), `eval.js/py` (`PI_JS`, `PI_PY`),
`auth.broker.*`, `searxng.*`, `dev.autoqa` (`PI_AUTO_QA`), `images.urls.*`.

### Credentials (9)

`auth.broker.token`, `advisor.enabled`, `dev.autoqaPush.token`, `hindsight.apiToken`,
`images.urls.credentials`, `mnemopi.embeddingApiKey`, `mnemopi.llmApiKey`,
`searxng.basicPassword`, `searxng.token`. These are the ids the `cfg://` view redacts
(`omp-urls/src/schemes/cfg.rs`).

(`advisor.enabled` being flagged `credential: true` looks like a generator artefact -
it is a plain boolean toggle - and should be confirmed against the TS source.)

## 3. Consumption status

Three tiers, measured by what appears outside `crates/omp-settings/src/`:

| Tier | Meaning | Count |
| --- | --- | --- |
| **A - read** | an accessor call with the literal dotted id: `get/get_bool/get_string/get_number/get_string_array/get_record/provenance("id")` | **59** |
| **B - mention-only** | the id appears as a string elsewhere (panel row, message text, log, test, tool-name table) but no accessor read | **345** |
| **C - absent** | no occurrence anywhere outside the registry | **144** |

Tier B is not consumption. Panel rows with no accessor read belong to tier B, so
editing such a setting persists a value nothing consumes. The tier also holds UI-only
labels such as `hindsightActive` / `mnemopiActive` / `autolearnActive`, and the four
phantom memory tool names in `omp-tui/src/tools/task.rs`. Tier A is the only set with
observable behaviour.

Settings-loading crates: `omp-cli` (19 files), `omp-tui` (5), `omp-providers` (2),
`omp-urls` (1), `omp-collab` (1).

Groups with the most tier-A reads: `compaction` (8), `statusLine` (7), `retry` (6),
`task` (3), `tui` (3), `todo` (3), `tools` (2), `lsp` (2), `skills` (2).

## 4. Dead groups

The 144 tier-C ids, by top-level group (declared / absent):

| Group | Declared | Absent | Notes |
| --- | --- | --- | --- |
| `hindsight` | 26 | 17 | recall/retain tuning, bank mission/prefix, debug; no HTTP client exists |
| `memories` | 16 | **16** | entire local pipeline config, incl. `memories.enabled` |
| `skills` | 13 | 10 | only `skills.enabled` + `skills.registryUrl` are read |
| `gc` | 9 | **9** | whole group: archive/blobs/wal/stale/retention |
| `task` | 25 | 6 | `task.agentAdvisor`, `agentModelOverrides`, `agentIdleTtlMs`, `agentPrewalk`, `agentServiceTierOverrides`, `agentCompactionThresholdOverrides` |
| `mnemopi` | 23 | 6 | `debug`, `injectionTokenLimit`, `recallLimit`, `recallContextTurns`, `recallMaxQueryChars`, `retainEveryNTurns` |
| `commit` | 6 | **6** | whole group: cache, ttl, map/reduce budget |
| `thinkingBudgets` | 6 | **6** | minimal/low/medium/high/xhigh/max |
| `read` | 10 | 5 | all five `read.summarize.*` |
| `searxng` | 8 | 5 | `categories`, `engines`, `language`, `safesearch`, `basicUsername` |
| `shellMinimizer` | 7 | 5 | `except`, `only`, `legacyFilters`, `maxCaptureBytes`, `settingsPath` |
| `providers` | 22 | 3 | `anthropic.slowMode`, `ollama-cloud.maxConcurrency`, `openai-codex.codeModeDirectTools` |
| `tui` | 18 | 3 | `maxInlineImage{Columns,Rows,Images}` |
| `images` | 14 | 3 | `urls.options`, `urls.sshRemotePort`, `urls.ttlHours` |
| `claudeResets` / `codexResets` | 4 each | 3 each | only `autoRedeem` is surfaced |
| `compaction` | 19 | 2 | `remoteEndpoint`, `v2RetainedMessageBudget` |
| `bash` | 8 | 2 | `autoBackground.thresholdMs`, `direnvLoadTimeoutMs` |
| `model` | 6 | 2 | `toolCallLoopGuard.threshold`, `.exemptTools` |
| `mcp` | 5 | 2 | `notificationDebounceMs`, `startupTimeoutMs` |
| `computer` | 4 | 2 | `maxWidth`, `maxHeight` |
| `github` | 4 | 2 | `cache.hardTtlSec`, `cache.softTtlSec` |
| `sharpshooter` | 3 | 2 | `injectionTokenLimit`, `intervalMinutes` |
| singles | 1 each | 1 each | `disabledExtensions`, `disabledProviders`, `enabledModels`, `enabledProviders`, `extensionHandlers.toolCallTimeoutMs`, `modelProviderOrder`, `modelTags`, `setupVersion`, `shellPath` |
| plus | | | `async.maxJobs`, `auth.accountPolicies`, `autolearn.minToolCalls`, `bashInterceptor.patterns`, `branchSummary.reserveTokens`, `dev.autoqaConsent`, `edit.modelVariants`, `eval.autoBackground.thresholdMs`, `exa.searchDelayMs`, `goal.continuationModes`, `statusLine.segmentOptions`, `stream.redactPatterns`, `stt.language`, `tools.xdevInlineDevices`, `ttsr.disabledRules` |

Whole-group deaths worth flagging because a panel row still advertises them: `gc.*`
(no UI row either), `commit.*` (no UI row), `thinkingBudgets.*` (no UI row),
`memories.*` (no UI row - the memory tab shows the backends, not the local pipeline),
`task.agent*` overrides (no UI row).

## 5. Memory domain: 74 declared, 0 consumed

The memory surface is the clearest case of registry-only parity.

| Group | Declared | Read by an accessor | Absent from the tree |
| --- | --- | --- | --- |
| `memory` | 1 (`memory.backend`, 5 values) | 0 | 0 (mentioned in UI + `/memory` stub text) |
| `memories` | 16 | 0 | 16 |
| `mnemopi` | 23 | 0 | 6 |
| `hindsight` | 26 | 0 | 17 |
| `sharpshooter` | 3 | 0 | 2 |
| `autolearn` | 3 | 0 | 1 |
| `contextFiles` | 1 (`contextFiles.extra`) | 0 | 0 |
| `contextPromotion` | 1 | 0 | 0 |
| **Total** | **74** | **0** | **42** |

The remaining 32 are tier B: the 30 memory-tab panel rows persist them, and
`omp-tui/src/overlays/settings.rs` uses `memory.backend` / `autolearn.enabled` only to
decide which rows are visible. Selecting `memory.backend: mnemopi` writes a value no
code path observes; `contextFiles.extra` is persisted and ignored;
`disabledProviders` / `disabledExtensions` / `enabledProviders` / `enabledModels` have
typed accessors in `settings.rs` with zero callers.

Five memory ids present in the TS registry are missing here entirely:
`hindsight.globalScope`, `mnemopi.embeddingInputType`,
`mnemopi.embeddingQueryPrefix`, `mnemopi.recallRelevance`,
`sharpshooter.minPromptLength`. There is also no `memories.enabled: true ->
memory.backend: local` migration, so a legacy config silently keeps the `off` backend.
See `docs/research/06-memory-context-gap.md` for the subsystem behind these keys.

## 6. Provenance tracking status

`Provenance` is declared as `{Default, Global, Project, Env}` and exposed as
`Settings::provenance(id) -> Option<Provenance>`, documented as "env beats project
beats global beats default".

Status: all four layers are now tracked. The merge records the supplying layer per
id in a parallel `HashMap<&str, Provenance>` instead of inferring it from the merged
map, so:

- **Env** - reported when the registry `env:` variable is set and parses; it still
  wins over every file layer.
- **Project** - reported only for ids actually supplied by the project config file.
- **Global** - reported only for ids actually supplied by the global config file.
- **Default** - reported for ids that resolved from the registry default table
  rather than from any configured layer.
- `None` - only for ids that are neither configured nor defaulted.

Precedence stays env > project > global > default, and the per-layer merging is
unchanged, so a set-but-unparsable value still resolves the way it did before.

Consumers:

- `omp-urls/src/schemes/cfg.rs:92` renders `provenance_label(...)` as the `cfg://`
  `source:` line (`cfg.rs:271`); each row now names the layer it really came from.
- `omp-providers/src/fallback.rs:90` filters "only user-supplied (non-Default) keys";
  the filter is live again, so a defaulted key is rejected as intended.
- `omp-cli/src/compact.rs:93` and `omp-cli/src/sysprompt.rs:55` branch on
  "Default-provenance key leaves the default standing"; that branch now fires for
  genuinely unset keys.
- The settings panel only ever writes the global layer, so it does not depend on the
  API (`overlays/settings.rs:741`); its writes now report `Global`.

Test coverage: `crates/omp-settings/tests/settings.rs` now exercises the API per layer
in Phase 1 - `provenance_distinguishes_all_four_layers`,
`provenance_reports_default_for_untouched_settings`,
`provenance_record_fallback_keeps_lower_layer` and
`provenance_named_empty_preset_contributes` - so a regression in the merge
bookkeeping fails a test rather than surfacing as a wrong `cfg://` label.

## 7. Other behaviours confirmed

- `unknown_keys()` reports dotted paths that are not an exact id, not a group prefix of
  an id, and not inside a `Record` subtree. It reports unknown *leaves* of unknown
  containers rather than the container, and is sorted.
- `write_global` validates shape against `ty`; `null` unsets and prunes empty parents;
  unknown keys survive the rewrite.
- `modelPresets`, `modelRoles`, `modelTags`, `cycleOrder`, `enabledModels`,
  `enabledProviders`, `disabledExtensions`, `disabledProviders`,
  `workspace.additionalDirectories`, `stream.serverUrl`, `stream.redactPatterns`,
  `images.urls.*`, `tier.*` have typed helpers in `settings.rs`. Of these only
  `model_presets()`/`model_roles()` (via `omp-cli/src/slash.rs`), `cycle_order()`,
  `tier()`, `workspace_additional_directories()`, `stream_server_url()`,
  `images_urls_enabled()`/`images_urls_backends()` have callers. The four
  `enabled*/disabled*` allowlist helpers and `stream_redact_patterns()`
  (`settings.rs:361`) have no production callers (tested only in settings tests).
- `snapcompact.*` (3 ids) and `context.policy/*`, `tools.format`, `read.toolResultPreview`
  appear as strings inside `omp-compact` / `omp-cli` but are never read through the
  store, so they sit in tier B.

## 8. Work items, in order

1. ~~Fix `provenance()` to track the supplying layer during merge; add Global /
   Project / Env / Default tests.~~ Done in Phase 1: the merge records the supplying
   layer for all four layers (Env / Project / Global / Default) and regression tests
   cover each one. This unblocks correct `cfg://` labels and the two previously dead
   "is it user-supplied" branches (`fallback.rs`, `compact.rs`, `sysprompt.rs`).
2. Delete or implement the four phantom accessor groups: `disabledProviders`,
   `disabledExtensions`, `enabledProviders`, `enabledModels`, plus `contextFiles.extra`.
3. Decide per dead group: implement, or drop the key and its panel row. Highest-value
   candidates are `thinkingBudgets.*` (model sampling), `commit.*` and `gc.*` (whole
   groups with no consumer at all), `read.summarize.*`, `searxng.*` (5 of 8 inert),
   `shellMinimizer.*`.
4. Add the 5 missing memory ids while touching `schema.rs`, and wire `memory.backend`
   to a real dispatcher so the 30-row memory tab stops being write-only.
5. Mark tier B explicitly in the generated panel data so the UI can distinguish
   "persisted and honoured" from "persisted only".
