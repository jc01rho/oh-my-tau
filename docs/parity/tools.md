# Tool & Builtin Parity (Phase 1)

Status legend: `PRESENT` (registered + schema equal) / `PRESENT-GAP` (registered, schema or
behavior differs) / `MISSING` (not constructible on the target).

Sources verified this pass:

| Side | File | Count |
|---|---|---|
| Reference registry | `/tmp/oh-my-pi-ro/packages/coding-agent/src/tools/builtin-names.ts` | 29 visible + 3 hidden |
| Reference factories + gates | `/tmp/oh-my-pi-ro/packages/coding-agent/src/tools/index.ts` (`BUILTIN_TOOLS`, `isToolAllowed`) | 29 + 3 |
| Target registry | `crates/omp-cli/src/tools.rs` (`builtin_tools`, `hidden_tools`, `hidden_tool_by_name`) | 18 visible + 3 hidden |
| Target gates | `crates/omp-cli/src/tools.rs` (`enabled_by_settings`) | 11 ids |
| Golden schema extract | `tests-tools/ts_schemas.ts` → `tests-tools/ts_tools.json` | **32 tools** (29 visible + 3 hidden) |
| Golden differ | `tests-tools/compare.ts` (`STRICT_TOOLS`, `NOT_PORTED_YET`) | **21 compared**, 8 strict |

Totals: visible **18/29**, hidden **3/3**, non-registry **1/9** (`mcp__*` present, 8 absent).

Extracted is not the same as compared. `ts_schemas.ts` extracts **all 32** upstream built-ins
(29 visible + 3 hidden, in `BUILTIN_TOOL_NAMES` then `HIDDEN_TOOL_NAMES` order) into
`ts_tools.json`. `compare.ts` then diffs only the **21** that have a Rust counterpart: the 32
minus the 11 names in its `NOT_PORTED_YET` set (`debug`, `github`, `context_notes`,
`new_context`, `security_scan`, `memory_edit`, `retain`, `recall`, `reflect`, `learn`,
`manage_skill`). Those 11 are printed with their TS params and skipped, not silently dropped —
a name that lands in Rust while still listed is reported as "now present in omp-rs, drop from
NOT_PORTED_YET", and a listed name with no golden is reported as stale. Of the 21 compared,
**8** are strict (`todo`, `ask`, `checkpoint`, `rewind`, `wait`, `think`, `yield`, `goal`):
canonical-JSON parameters plus the full description must match. The other 13 get the per-param
diff and a first-line description check. The non-registry tools of §3 (`computer`, `browser`,
`generate_image`, `tts`, the five `vibe_*`, `report_tool_issue`, the `search` alias) are in
neither layer — no golden, no diff.

---

## 1. Visible builtins (29 reference, 18 target)

Order below is `BUILTIN_TOOL_NAMES` order. Target order in `builtin_tools` differs
(`read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask,
todo, web_search, lsp, checkpoint, rewind`) and is asserted by
`tools::tests::tool_names_and_schemas_match_omp`.

| # | Tool | Ref default | Target | Status | Diff / target file |
|---|---|---|---|---|---|
| 1 | `read` | on | yes | **PRESENT-GAP** | Schema equal (`{path}` required, `additionalProperties:false`). Behavior + description differ — see §4. `crates/omp-cli/src/tools/read.rs` |
| 2 | `bash` | on | yes | **PRESENT-GAP** | **Phase 1 landed** `notify` and the ref `name` regex (`^(?:\s*\|[A-Za-z0-9][A-Za-z0-9._-]{0,47})$`, `maxLength:48` kept). Remaining: the 6 settings-dependent ref variants collapse to one union schema. `crates/omp-cli/src/tools/bash.rs` |
| 3 | `edit` | on | yes | **PRESENT** | 5 modes via `vendor/pi-edit`; golden records the `replace` shape and matches. `crates/omp-cli/src/tools/edit.rs` |
| 4 | `ast_grep` | **off** | yes | **PRESENT** | `{pat,path,lang,skip}`, equal to golden. Gate `astGrep.enabled` default false. `crates/omp-cli/src/tools/ast.rs` |
| 5 | `ast_edit` | on | yes | **PRESENT** | `{ops,paths}`, equal to golden. `crates/omp-cli/src/tools/ast.rs` |
| 6 | `ask` | on | yes | **PRESENT-GAP** | **Phase 1 landed** `{html: string}` ("one HTML `<form>`") with `parse_html_form` flattening named controls, plus the legacy `questions[]` path as recovery (omp `recoverAskHtml`/`legacyQuestionsHtml`). Remaining: the schema advertises *both* keys with **no** `required` list where omp requires `html`, and dispatch is object-only — a bare HTML string is rejected, not recovered. `crates/omp-cli/src/tools/ask.rs` |
| 7 | `debug` | on | **no** | **MISSING** | DAP client. Ref `tools/debug.ts`. New `crates/omp-cli/src/tools/debug.rs` |
| 8 | `eval` | on | yes | **PRESENT** | Target schema `{language: enum[py,js], code, title, timeout, reset}` required `[language,code]` — matches golden. Gate is `allowEval` in ref, ungated in target. `crates/omp-cli/src/tools/eval.rs:58` |
| 9 | `github` | **off** | **no** | **MISSING** | Ref needs `gh.ts` + `gh-common`/`gh-view`/`gh-pr-diff`/`gh-pr-checkout`/`gh-run-watch`/`gh-search`/`github-cache`/`gh-cache-invalidation`/`glab-mr`. New `crates/omp-cli/src/tools/github.rs` |
| 10 | `glob` | on | yes | **PRESENT-GAP** | **Phase 1 landed** `limit: {"type":"integer","minimum":1,"maximum":200}`, equal to the ref. Remaining: the ref's extra `pattern` key (`GLOB_ARGUMENTS`) is still absent, and `additionalProperties:false` refuses it. `crates/omp-cli/src/tools.rs:300` |
| 11 | `grep` | on | yes | **PRESENT** | Wire schema equal incl. `skip: {anyOf:[number,null]}`; ref caps (`20`/`10`/`50`) unverified. `crates/omp-cli/src/tools/search.rs` |
| 12 | `find` | detected | yes | **PRESENT** | Exact match; asserted by `tools::tests::find_schema_matches_omp`. Ref `defaultState:"detected"` when a native judge model exists; target gates only on `find.enabled != off`. `crates/omp-cli/src/tools/find.rs` |
| 13 | `lsp` | on | yes | **PRESENT** | `{action + file,line,symbol,query,new_name,apply,timeout,payload}`, `additionalProperties:false` — matches golden. Ref gate adds `enableLsp`. `crates/omp-cli/src/tools/lsp.rs` |
| 14 | `checkpoint` | **off** | yes | **PRESENT** | `{goal}`; gate `checkpoint.enabled` default false. Ref also hides at `taskDepth > 0` unless explicitly requested. `crates/omp-cli/src/tools/checkpoint.rs` |
| 15 | `rewind` | **off** | yes | **PRESENT** | `{report}`; same gate as `checkpoint`. `crates/omp-cli/src/tools/checkpoint.rs` |
| 16 | `context_notes` | **off** | **no** | **MISSING** | Ref `tools/context-notes.ts`; gate `compaction.experimentalContextManagement`. |
| 17 | `new_context` | **off** | **no** | **MISSING** | Same gate as `context_notes`. |
| 18 | `security_scan` | **off** | **no** | **MISSING** | Gate `security.enabled`. **Phase 1:** the dangling catalog half is cleaned — `builtin_commands.json` keeps one bare `security` entry described `"Not ported: security scans need the security-scan mode controller"` and lists no subcommands (`slash.md` §3, §5 #2). The tool itself is still unported. |
| 19 | `task` | on | yes | **PRESENT-GAP** | `{context, tasks, model}` required `[context,tasks]`. Golden (`ts_tools.json`) has **no** top-level `model` — its `tasks[]` item is `{task, solutionSpace, name, agent, tools}` — so the target's top-level `model: {"not": {}}` (key advertised, rejected) is **target-only**, not a golden match; selectors are accepted per `tasks[]` item only. Ref gate `canSpawnAtDepth(task.maxRecursionDepth)` absent in target. `crates/omp-cli/src/tools/task.rs` |
| 20 | `wait` | on | yes | **PRESENT-GAP** | Schema `{type:"object",properties:{}}` byte-equal. Runtime: 30-minute `WAIT_MAX`, and a wait wakes only on an owned job/service settling, that cap, or abort — the IRC/peer-message and owed-answer legs have no port (no peer bus), so the description's answer language is unbacked. Ref gate `async` \|\| `irc` \|\| `launch` absent. `crates/omp-cli/src/tools/wait.rs` |
| 21 | `todo` | on | yes | **PRESENT-GAP** | **Phase 1 landed** `task: anyOf[string, array<string>]` with the ref description ("verbatim task content; array targets several (not start)") and `list[].items.minItems:1`. Remaining: target-only `reason` (`block reason`) has no counterpart in `todo.ts:60-69`. `crates/omp-cli/src/tools/todo.rs` |
| 22 | `web_search` | on | yes | **PRESENT** | **Phase 1 landed** all 6 ref params (`query, recency, limit, max_tokens, temperature, num_search_results`); `temperature` is forwarded only when supplied, as `callSearch` does. `crates/omp-cli/src/tools/web_search.rs` |
| 23 | `write` | on | yes | **PRESENT** | **Phase 1 landed** `required: ["path","content"]`; a call without `content` fails (`edit::write`). `crates/omp-cli/src/tools/edit.rs` |
| 24 | `memory_edit` | **off** | **no** | **MISSING** | Ref gate `memory.backend === "mnemopi"`. |
| 25 | `retain` | **off** | **no** | **MISSING** | Ref gate `memory.backend ∈ {hindsight, mnemopi}`. |
| 26 | `recall` | **off** | **no** | **MISSING** | Same gate as `retain`. |
| 27 | `reflect` | **off** | **no** | **MISSING** | Same gate as `retain`. |
| 28 | `learn` | **off** | **no** | **MISSING** | Ref gate `autolearn.enabled` + depth 0 + memory backend. |
| 29 | `manage_skill` | **off** | **no** | **MISSING** | Ref gate `autolearn.enabled`. |

Missing visible tools: `debug`, `github`, `context_notes`, `new_context`, `security_scan`,
`memory_edit`, `retain`, `recall`, `reflect`, `learn`, `manage_skill` (11).

---

## 2. Hidden tools (3 reference, 3 target) — full parity

| Tool | Ref | Target file | Schema | Status |
|---|---|---|---|---|
| `think` | yes | `crates/omp-cli/src/tools/think.rs` | `{thoughts}` req, `additionalProperties:false` | PRESENT |
| `yield` | yes | `crates/omp-cli/src/tools/yield_tool.rs` | `{type,data,error}`, none required, `additionalProperties:false` | PRESENT (schema; no section tracking — §4) |
| `goal` | yes | `crates/omp-cli/src/tools/goal.rs` | `{op, objective, token_budget}` | PRESENT |

Activation order matches the reference plan expression: `think` (setting
`externalThinking` + `think::supports_external_thinking(model)`), then `yield`
(`require_yield_tool`, subagent runs), then `goal` (`goal_mode_active`).
`hidden_tool_by_name` covers all three for explicit `--tools` requests.

---

## 3. Non-registry tools (9 reference, 1 target)

Tools registered outside `BUILTIN_TOOLS`.

| Tool(s) | Reference location | Target | Status |
|---|---|---|---|
| `mcp__<server>_<tool>` | `isMCPToolName` in `builtin-names.ts` | `crates/omp-cli/src/tools/mcp.rs` | **PRESENT** |
| `computer` | `tools/computer.ts` + `tools/computer/prelude-definition.ts` (eval prelude capability) | only a `/computer` slash command in `crates/omp-cli/src/features/browser.rs` | **MISSING as tool** |
| `browser` | `tools/browser.ts` + `tools/browser/prelude-definition.ts` | only a `/browser` slash command in `features/browser.rs` | **MISSING as tool** |
| `generate_image` | `tools/image-gen.ts`, gated `generateImage.enabled` in `sdk.ts` | none | **MISSING** |
| `tts` | `tools/tts.ts`, gated `speechgen.enabled` in `sdk.ts` | none | **MISSING** |
| `vibe_spawn`, `vibe_send`, `vibe_wait`, `vibe_kill`, `vibe_list` | `tools/vibe.ts` (`VibeSpawnTool`…`VibeListTool`) | none | **MISSING (5 tools)** |
| `report_tool_issue` | `tools/report-tool-issue.ts`, dispatched through `write xd://report_issue` | none | **MISSING** |
| `search` → `grep` legacy alias | `LEGACY_BUILTIN_TOOL_NAME_ALIASES` in `builtin-names.ts` | no alias table in `tools.rs` | **MISSING (compat)** |

`fetch.ts` is **not** a tool — it backs `read`'s URL path (`executeReadUrl`,
`materializeReadUrlToFile`). It belongs to the `read` capability gap in §4, not here.

---

## 4. Shared-tool schema deltas (exact)

Derived from `tests-tools/ts_tools.json` + the TS sources vs `tools.rs` / per-tool modules.
**Phase 1 landed** the schema alignments marked _landed_; each bullet keeps the delta that is
still open, and the runtime-only gaps (behavior no schema can show) are called out separately.

Runtime deltas that survive the schema work: `wait` blocks 30 minutes max and never waits on an
answer (no peer bus), `yield` validates `type` sections but tracks none (no accumulation, so a
materialized `type:"result"` cannot fail the way omp's "none submitted" check does), `ask` still
accepts the `questions` fallback the reference only recovers into HTML, `todo` still carries
`reason`, and `task` restricts `model` to per-item selectors.

- **`bash`** — properties `command, timeout, cwd, pty, async, name, notify, ready{log,port,host,timeout}`,
  required `[command]`. **Landed:** `notify: {"type":"string"}` and the ref `name` contract
  (`maxLength:48` + `pattern: ^(?:\s*|[A-Za-z0-9][A-Za-z0-9._-]{0,47})$`). **Open:** the reference
  emits six schema variants (`bashSchemaBase`, `…WithAsync`, `…WithService`, `…WithAsyncAndService`,
  plus `…NoPty` forms; `bash.ts:460-519`); the target always advertises the union, and `pty` is
  accepted but ignored (`shell.md` §3).
- **`glob`** — **Landed:** `limit: {"type":"integer","minimum":1,"maximum":200}`, matching
  `GLOB_ARGUMENTS`. **Open:** the ref's `pattern` key is absent and `additionalProperties:false`
  refuses it, so `globSearchPath` scoping has no counterpart.
- **`ask`** — **Landed:** `{html: string}` ("one HTML `<form>`") as the primary shape,
  `parse_html_form` flattening of named `<input>/<select>/<textarea>` controls, and the pre-HTML
  `questions[]` shape still accepted as recovery (`recoverAskHtml`/`legacyQuestionsHtml`).
  **Open:** the schema advertises both `html` and `questions` with **no** `required` where omp
  requires `html`; dispatch is object-only, so an argument that is a bare HTML string is rejected
  rather than recovered.
- **`todo`** — **Landed:** `task: anyOf[{string}, {array of string}]` with the ref description
  ("verbatim task content; array targets several (not start)"), array entries splitting like the
  string form (`splitTaskList`, `todoTaskTargets`), and `list[].items.minItems:1`.
  **Open:** `reason: {"type":"string","description":"block reason"}` is target-only; the ref has
  no such key (`todo.ts:60-69`).
- **`web_search`** — **Landed:** all six ref params — `query`, `recency`
  (enum `day|week|month|year`), `limit`, `max_tokens`, `temperature`, `num_search_results` — required
  `[query]`. `temperature` is forwarded only when the caller supplies one, matching `callSearch`.
- **`write`** — **Landed:** `required: ["path","content"]`; `edit::write` bails with "`content` is
  required to write …" when it is missing. `additionalProperties:false` retained.
- **`wait`** — schema byte-equal (empty `properties`, no `required`). **Open (runtime only):**
  `WAIT_MAX` is 30 minutes; a wait wakes on an owned job/service settling, that cap, or tool-call
  abort. omp's IRC/peer-message and owed-answer legs have no port (no peer bus), so the description
  still promises waiting on an answer that nothing can deliver.
- **`yield`** — schema canonical-JSON equal (`{type: string|array, data, error}`, none required).
  **Open (runtime only):** no section tracking — `type: ["<field>"]` sections and the data-less
  `type:"result"` finalizer are accepted and echoed into `details`, but nothing accumulates, so the
  omp "invalid when none were submitted" ladder has no state to fail on.
- **`goal`** — schema equal (`{op, objective, token_budget}`, required `[op]`, six-value op enum).
- **`read`** — schema identical (single `path`, required). Behavior: ref dispatches
  `read-archive.ts`, `read-sqlite.ts`, `read-pdf.ts`, `read-json.ts`, `read-binary.ts`,
  `read-selector.ts`, `read-format.ts`, `read-summary.ts`, `fetch.ts`; target handles only
  `proc://`, internal `scheme://` URLs, directories, images, and plain UTF-8 text. Target
  `READ_DESCRIPTION` drops the SQLite / archive / PDF / video / URL paragraphs the TS
  description carries (`ts_tools.json` `read.description` is the full text).
- **`eval`** — target `{language, code, title, timeout, reset}`; golden match. Ref additionally
  gates on `allowEval` (backend reachability).
- **`lsp`** — target action enum of 14 values with `additionalProperties:false`; golden match.
- **`task`** — `{context, tasks, model}` required `[context,tasks]`. The golden schema (`ts_tools.json`)
  carries no top-level `model` key at all (items are `{task, solutionSpace, name, agent, tools}`), so
  this is **not** a match: the target's top-level `model: {"not": {}}` is **target-only** — it
  advertises the key and refuses it — while model selectors are accepted only inside `tasks[]` items.
- Unchanged and equal: `edit` (replace shape), `find`, `grep`, `ast_grep`, `ast_edit`, `checkpoint`,
  `rewind`, `think`.

---

## 5. Eval preludes

Target files: `crates/omp-eval/src/prelude.js`, `prelude_agents.js`, `prelude_combinators.js`,
`prelude_txn.js`.

| Global | Reference (`src/eval/js/shared/prelude.txt`) | Target | Status |
|---|---|---|---|
| `read`, `write`, `env`, `tool`, `output`, `log`, `phase`, `print` | yes | yes (`prelude.js`) | PRESENT |
| `sh` | yes | no | **MISSING** |
| `console` | yes | no | **MISSING** |
| `agent`, `wait`, `judge`, `judgeBatch`, `workpool`, `budget`, `completion` | yes | yes (`prelude_agents.js`) | PRESENT |
| `AgentHandle`, `CompletionHandle`, `JudgmentBatch`, `JudgmentItem`, `WorkPool` | yes | yes (+ target-only `PendingHandle`) | PRESENT |
| `llm_query`, `llm_stats`, `rlm_context` (+ `prelude_combinators.js`, `prelude_txn.js`) | no | yes | TARGET-ONLY (RLM) |
| `browser`, `computer` preludes | yes (`tools/browser/prelude-definition.ts`, `tools/computer/prelude-definition.ts`) | no | **MISSING** |

---

## 6. Settings gates

Target `enabled_by_settings` (`crates/omp-cli/src/tools.rs`) — 11 ids, everything else returns `true`:

| Setting | Target tools | Default |
|---|---|---|
| `bash.enabled` | `bash` | on |
| `glob.enabled` | `glob` | on |
| `grep.enabled` | `grep` | on |
| `astGrep.enabled` | `ast_grep` | **off** |
| `astEdit.enabled` | `ast_edit` | on |
| `ask.enabled` | `ask` | on |
| `todo.enabled` | `todo` | on |
| `web_search.enabled` | `web_search` | on |
| `lsp.enabled` | `lsp` | on |
| `checkpoint.enabled` | `checkpoint`, `rewind` | **off** |
| `find.enabled` | `find` (`off` hides; `auto`/anything else keeps) | on when not `off` |

Reference `isToolAllowed` (`tools/index.ts:801-880`) gates **22** names. Present in target:
`bash`, `glob`, `grep`, `find`, `ast_grep`, `ast_edit`, `ask`, `todo`, `web_search`, `lsp`,
`checkpoint`/`rewind`. Absent from target:

| Ref gate | Setting / condition | Tools |
|---|---|---|
| edit mode | `cfgEditMode === "none"` hides `edit`, and `bash` stays on when edit is disabled | `edit`, `bash` |
| `allowEval` | eval backend reachable | `eval` |
| `cfgDebugEnabled` | `debug.enabled` | `debug` |
| `cfgGithubEnabled` | `github.enabled` | `github` |
| `cfgSecurityEnabled` | `security.enabled` | `security_scan` |
| `externalThinkingActive` | `externalThinking` + model support | `think` |
| `cfgCompactionExperimentalContextManagement` | experimental context management | `context_notes`, `new_context` |
| `async` \|\| `irc` \|\| `launch` | any of the three | `wait` |
| `cfgMemoryBackend ∈ {hindsight, mnemopi}` | memory backend | `retain`, `recall`, `reflect` |
| `cfgMemoryBackend === "mnemopi"` | memory backend | `memory_edit` |
| `cfgAutolearnEnabled` + depth 0 | autolearn | `manage_skill` |
| `cfgAutolearnEnabled` + depth 0 + backend ∈ {hindsight, mnemopi, local} | autolearn | `learn` |
| `canSpawnAtDepth(task.maxRecursionDepth)` | recursion depth | `task` |
| `goal.enabled` + goal-mode state | goal record exists/enabled/dropped | `goal` |
| `todo`: `!includeYield \|\| prewalkArmed` | yield/prewalk interaction | `todo` |
| `checkpoint`: `taskDepth === 0 \|\| requestedTools` | depth | `checkpoint`, `rewind` |

Also absent on the target: the reference's auto-include/sibling rules — `includeYield` pushes
`yield` into `requestedTools`; `ast_grep`/`ast_edit` and the memory tools auto-include their
siblings; `checkpoint`/`rewind` pair.

---

## 7. Dangling declarations — resolved in Phase 1

Four declarations advertised capability the target cannot construct. All four are cleaned
up in this worktree; the table records what was wrong and what changed.

| Declaration | Was | Phase 1 state |
|---|---|---|
| `crates/omp-cli/src/tools/task.rs:25` — `READ_ONLY_TOOLS` | Whitelisted `recall`, `reflect`, `retain`, `memory_edit`, none constructible via `builtin_tools`/`hidden_tool_by_name`, so an agent's tool set could read as read-only on names that cannot resolve. | **RESOLVED** — the four memory ids are dropped; the const lists only registered tools (`read, wait, grep, glob, find, web_search, ast_grep, yield, ask, todo, checkpoint, rewind`) with a comment deferring re-add to the Phase 3 memory crate. |
| `crates/omp-cli/src/builtin_commands.json` — `security` slash command | Shipped `plan\|scan\|status\|cancel\|scans\|show\|import\|export\|validate\|compare\|disposition` with no backing `security_scan` tool. | **RESOLVED** — one `security` entry, `"description": "Not ported: security scans need the security-scan mode controller"`, no subcommand list. See `slash.md` §3 and §5 #2. |
| `crates/omp-tui/src/overlays/settings_defs.rs` — `github` tool setting | A `github` tool setting row with no `github` tool (§1 #9). | **RESOLVED** — the row is gone; the only remaining `github`/`gh` mentions are `share`'s gist option and worktree descriptions, neither of which advertises a tool. |
| `crates/omp-cli/src/tools/bash.rs:3` — doc header | Called the reference shell "brush-based", which reads as a defect if the reference is `oh-my-pi` main's `crates/shell` (bash 5.3 + zsh dialect, not brush-based). | **RESOLVED — comment kept, correction withdrawn.** Per `docs/parity/shell.md` §6, the omp2 reference `crates/shell` **is** a `brush-core` 0.5.0 / `brush-parser` 0.4.0 port (its own `README.md`, echoed by `docs/research/12`), so the header is accurate for the reference this port targets. The earlier note was mis-scoped, not a code defect. |

Still open and *not* dangling declarations: the `bash` deltas in §4 (six reference variants collapsed
to one union schema, `pty` accepted but ignored in code — `shell.md` §3) and the runtime gaps in
`wait` (30-minute cap, no answer/peer leg) and `yield` (no section tracking) are behavior gaps with
real handlers behind them, not advertised capability without a tool.

---

## 8. Target files — Phase 1 landed vs remaining

Landed in this worktree (the schema half of each delta):

| Work | File | State |
|---|---|---|
| `write` required `[path,content]` | `crates/omp-cli/src/tools.rs` (`write` entry) | **LANDED** |
| `glob` `limit` 1..200 integer | `crates/omp-cli/src/tools.rs:300` | **LANDED** |
| `web_search` all 6 params | `crates/omp-cli/src/tools/web_search.rs` | **LANDED** |
| `todo` `task: string\|string[]` | `crates/omp-cli/src/tools/todo.rs` | **LANDED** |
| `ask` `{html}` + legacy recovery | `crates/omp-cli/src/tools/ask.rs` | **LANDED** |
| `bash` `notify` + `name` regex | `crates/omp-cli/src/tools/bash.rs`, `tools.rs` | **LANDED** |
| `wait` / `yield` / `goal` schema parity | `wait.rs`, `yield_tool.rs`, `goal.rs` | **LANDED** |

Still open:

| Work | File |
|---|---|
| `bash` six schema variants; `pty` actually honored | `crates/omp-cli/src/tools/bash.rs` |
| `wait` answer/peer-message leg (needs a peer bus) | `crates/omp-cli/src/tools/wait.rs` |
| `yield` section accumulation + "none submitted" finalizer | `crates/omp-cli/src/tools/yield_tool.rs` |
| `ask` `required: ["html"]`, bare-string recovery | `crates/omp-cli/src/tools/ask.rs` |
| `todo` drop or justify target-only `reason` | `crates/omp-cli/src/tools/todo.rs` |
| `task` top-level `model` (currently `"not": {}`) | `crates/omp-cli/src/tools/task.rs` |
| `glob` `pattern` key + `GLOB_ARGUMENTS` scoping | `crates/omp-cli/src/tools.rs` (`glob` entry) |
| `read` behavior parity | `crates/omp-cli/src/tools/read.rs` + new `read_archive.rs`, `read_sqlite.rs`, `read_pdf.rs`, `read_json.rs`, `read_binary.rs`, `fetch.rs` |
| `retain`/`recall`/`reflect`/`memory_edit`/`learn`/`manage_skill` | new `memory.rs`, `learn.rs`, `manage_skill.rs` |
| `context_notes`/`new_context` | new `context_notes.rs`, `new_context.rs` |
| `github` | new `github.rs` + `gh_common.rs`, `gh_view.rs`, `gh_search.rs`, `gh_pr_diff.rs`, `gh_pr_checkout.rs`, `gh_run_watch.rs` |
| `debug` | new `debug.rs` (DAP client) |
| `security_scan` | new `security_scan.rs` |
| `computer`/`browser` tools + preludes | `crates/omp-cli/src/tools/computer.rs`, `browser.rs`, `crates/omp-eval/src/prelude_browser.js` |
| `generate_image`, `tts`, `vibe_*`, `report_tool_issue` | new modules under `crates/omp-cli/src/tools/` |
| `search`→`grep` alias + auto-include rules | `crates/omp-cli/src/tools.rs` (`normalize`/plan function) |
| Golden coverage: shrink `NOT_PORTED_YET` as each visible tool lands (32 extracted, 21 compared today) | `tests-tools/compare.ts` (`NOT_PORTED_YET`, `STRICT_TOOLS`) |
