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
| Golden schema extract | `tests-tools/ts_tools.json` | 21 tools |
| Golden differ | `tests-tools/compare.ts` (`STRICT_TOOLS`) | 21 tools, 8 strict |

Totals: visible **18/29**, hidden **3/3**, non-registry **1/9** (`mcp__*` present, 8 absent).
`tests-tools/ts_tools.json` holds no golden for `debug`, `github`, `context_notes`,
`new_context`, `security_scan`, `memory_edit`, `retain`, `recall`, `reflect`, `learn`,
`manage_skill`, `computer`, `browser`, `generate_image`, `tts`, `vibe_*`,
`report_tool_issue` — 11 visible + 7 non-registry tools are unguarded by the differ.

---

## 1. Visible builtins (29 reference, 18 target)

Order below is `BUILTIN_TOOL_NAMES` order. Target order in `builtin_tools` differs
(`read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask,
todo, web_search, lsp, checkpoint, rewind`) and is asserted by
`tools::tests::tool_names_and_schemas_match_omp`.

| # | Tool | Ref default | Target | Status | Diff / target file |
|---|---|---|---|---|---|
| 1 | `read` | on | yes | **PRESENT-GAP** | Schema equal (`{path}` required, `additionalProperties:false`). Behavior + description differ — see §4. `crates/omp-cli/src/tools/read.rs` |
| 2 | `bash` | on | yes | **PRESENT-GAP** | Missing `notify`; 6 settings-dependent ref variants collapsed to 1; `name` is `maxLength:48` not the ref regex. `crates/omp-cli/src/tools/bash.rs` |
| 3 | `edit` | on | yes | **PRESENT** | 5 modes via `vendor/pi-edit`; golden records the `replace` shape and matches. `crates/omp-cli/src/tools/edit.rs` |
| 4 | `ast_grep` | **off** | yes | **PRESENT** | `{pat,path,lang,skip}`, equal to golden. Gate `astGrep.enabled` default false. `crates/omp-cli/src/tools/ast.rs` |
| 5 | `ast_edit` | on | yes | **PRESENT** | `{ops,paths}`, equal to golden. `crates/omp-cli/src/tools/ast.rs` |
| 6 | `ask` | on | yes | **PRESENT-GAP (divergent)** | Ref schema is `{html: string}` ("one HTML `<form>`"); target is still `{questions:[…]}`. Ref keeps `questions` only via `recoverAskHtml`/`legacyQuestionsHtml`. `crates/omp-cli/src/tools/ask.rs:788` |
| 7 | `debug` | on | **no** | **MISSING** | DAP client. Ref `tools/debug.ts`. New `crates/omp-cli/src/tools/debug.rs` |
| 8 | `eval` | on | yes | **PRESENT** | Target schema `{language: enum[py,js], code, title, timeout, reset}` required `[language,code]` — matches golden. Gate is `allowEval` in ref, ungated in target. `crates/omp-cli/src/tools/eval.rs:58` |
| 9 | `github` | **off** | **no** | **MISSING** | Ref needs `gh.ts` + `gh-common`/`gh-view`/`gh-pr-diff`/`gh-pr-checkout`/`gh-run-watch`/`gh-search`/`github-cache`/`gh-cache-invalidation`/`glab-mr`. New `crates/omp-cli/src/tools/github.rs` |
| 10 | `glob` | on | yes | **PRESENT-GAP** | Ref `limit` is `1 <= integer <= 200`; target `{"type":"number"}` unconstrained. Ref also accepts an extra `pattern` key (`GLOB_ARGUMENTS`) that target's `additionalProperties:false` refuses. `crates/omp-cli/src/tools.rs:297` |
| 11 | `grep` | on | yes | **PRESENT** | Wire schema equal incl. `skip: {anyOf:[number,null]}`; ref caps (`20`/`10`/`50`) unverified. `crates/omp-cli/src/tools/search.rs` |
| 12 | `find` | detected | yes | **PRESENT** | Exact match; asserted by `tools::tests::find_schema_matches_omp`. Ref `defaultState:"detected"` when a native judge model exists; target gates only on `find.enabled != off`. `crates/omp-cli/src/tools/find.rs` |
| 13 | `lsp` | on | yes | **PRESENT** | `{action + file,line,symbol,query,new_name,apply,timeout,payload}`, `additionalProperties:false` — matches golden. Ref gate adds `enableLsp`. `crates/omp-cli/src/tools/lsp.rs` |
| 14 | `checkpoint` | **off** | yes | **PRESENT** | `{goal}`; gate `checkpoint.enabled` default false. Ref also hides at `taskDepth > 0` unless explicitly requested. `crates/omp-cli/src/tools/checkpoint.rs` |
| 15 | `rewind` | **off** | yes | **PRESENT** | `{report}`; same gate as `checkpoint`. `crates/omp-cli/src/tools/checkpoint.rs` |
| 16 | `context_notes` | **off** | **no** | **MISSING** | Ref `tools/context-notes.ts`; gate `compaction.experimentalContextManagement`. |
| 17 | `new_context` | **off** | **no** | **MISSING** | Same gate as `context_notes`. |
| 18 | `security_scan` | **off** | **no** | **MISSING** | Gate `security.enabled`. **Phase 1:** the dangling catalog half is cleaned — `builtin_commands.json` keeps one bare `security` entry described `"Not ported: security scans need the security-scan mode controller"` and lists no subcommands (`slash.md` §3, §5 #2). The tool itself is still unported. |
| 19 | `task` | on | yes | **PRESENT** | `{context, tasks, model}` required `[context,tasks]` — matches golden. Ref gate `canSpawnAtDepth(task.maxRecursionDepth)` absent in target. `crates/omp-cli/src/tools/task.rs` |
| 20 | `wait` | on | yes | **PRESENT** | `{type:"object",properties:{}}` — matches. Ref gate: `async` \|\| `irc` \|\| `launch`; target ungated. `crates/omp-cli/src/tools/wait.rs` |
| 21 | `todo` | on | yes | **PRESENT-GAP (divergent)** | Ref `task?: string \| string[]`, no `reason`. Target `task: {"type":"string"}` plus target-only `reason`. `ts_tools.json` lists `reason` and `task` as `string`, so the golden itself is stale vs `todo.ts:60-69`. `crates/omp-cli/src/tools/todo.rs:784` |
| 22 | `web_search` | on | yes | **PRESENT-GAP (divergent)** | Ref has 6 params (`query, recency, limit, max_tokens, temperature, num_search_results`); target omits the last 3. `crates/omp-cli/src/tools/web_search.rs:78` |
| 23 | `write` | on | yes | **PRESENT** | `{path, content}` required `[path]`. `crates/omp-cli/src/tools/edit.rs` |
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
| `yield` | yes | `crates/omp-cli/src/tools/yield_tool.rs` | `{type,data,error}`, none required, `additionalProperties:false` | PRESENT |
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

- **`bash`** — target properties: `command, timeout, cwd, pty, async, name, ready{log,port,host,timeout}`, required `[command]`.
  Reference adds `notify?: string` and emits six schema variants (`bashSchemaBase`,
  `…WithAsync`, `…WithService`, `…WithAsyncAndService`, plus `…NoPty` forms; `bash.ts:460-519`).
  `name` is `{"type":"string","maxLength":48}` vs ref regex `^(?:\s*\|[A-Za-z0-9][A-Za-z0-9._-]{0,47})$`.
- **`glob`** — target `limit` is an unconstrained `number`; ref is `1 <= number.integer <= 200`.
  Target rejects `pattern`; ref accepts it and scopes each `;` entry under each `path`
  entry (`GLOB_ARGUMENTS`, `globSearchPath`).
- **`ask`** — target `{questions: array(minItems 1) of {id,question,options[{label,description?}],multi?,recommended?}}`;
  ref `{html: string}` described "one HTML `<form>`". Legacy `questions` survives only as recovery.
- **`todo`** — target `task: {"type":"string","description":"exact task text"}` plus
  `reason: {"type":"string","description":"block reason"}`; ref `task?: string | string[]`
  ("array targets several (not start)") and no `reason`. Golden `ts_tools.json` agrees with
  the target, so the golden must be regenerated before tightening `compare.ts`.
- **`web_search`** — target `{query, recency(enum day|week|month|year), limit}` required `[query]`;
  ref also has `max_tokens`, `temperature`, `num_search_results` (all `number?`).
- **`read`** — schema identical (single `path`, required). Behavior: ref dispatches
  `read-archive.ts`, `read-sqlite.ts`, `read-pdf.ts`, `read-json.ts`, `read-binary.ts`,
  `read-selector.ts`, `read-format.ts`, `read-summary.ts`, `fetch.ts`; target handles only
  `proc://`, internal `scheme://` URLs, directories, images, and plain UTF-8 text. Target
  `READ_DESCRIPTION` drops the SQLite / archive / PDF / video / URL paragraphs the TS
  description carries (`ts_tools.json` `read.description` is the full text).
- **`eval`** — target `{language, code, title, timeout, reset}`; golden match. Ref additionally
  gates on `allowEval` (backend reachability).
- **`lsp`** — target action enum of 14 values with `additionalProperties:false`; golden match.
- Unchanged and equal: `edit` (replace shape), `write`, `find`, `grep`, `ast_grep`, `ast_edit`,
  `checkpoint`, `rewind`, `wait`, `task`, `think`, `yield`, `goal`.

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

Still open and *not* dangling declarations: the `bash` schema deltas in §4 (`notify`, six
reference variants collapsed to one, `name` regex vs `maxLength:48`) and `pty` being accepted
by the schema but ignored in code (`shell.md` §3) are behavior gaps with real handlers behind
them.

---

## 8. Target files to create or extend

| Work | File |
|---|---|
| `read` behavior parity | `crates/omp-cli/src/tools/read.rs` + new `read_archive.rs`, `read_sqlite.rs`, `read_pdf.rs`, `read_json.rs`, `read_binary.rs`, `fetch.rs` |
| `bash` `notify` + schema variants | `crates/omp-cli/src/tools/bash.rs` |
| `ask` `{html}` | `crates/omp-cli/src/tools/ask.rs` |
| `todo` `task: string\|string[]`, drop or keep `reason` | `crates/omp-cli/src/tools/todo.rs` |
| `web_search` 3 extra params | `crates/omp-cli/src/tools/web_search.rs` |
| `glob` `limit` bound + `pattern` key | `crates/omp-cli/src/tools.rs` (`GLOB` entry) |
| `retain`/`recall`/`reflect`/`memory_edit`/`learn`/`manage_skill` | new `memory.rs`, `learn.rs`, `manage_skill.rs` |
| `context_notes`/`new_context` | new `context_notes.rs`, `new_context.rs` |
| `github` | new `github.rs` + `gh_common.rs`, `gh_view.rs`, `gh_search.rs`, `gh_pr_diff.rs`, `gh_pr_checkout.rs`, `gh_run_watch.rs` |
| `debug` | new `debug.rs` (DAP client) |
| `security_scan` | new `security_scan.rs` |
| `computer`/`browser` tools + preludes | `crates/omp-cli/src/tools/computer.rs`, `browser.rs`, `crates/omp-eval/src/prelude_browser.js` |
| `generate_image`, `tts`, `vibe_*`, `report_tool_issue` | new modules under `crates/omp-cli/src/tools/` |
| `search`→`grep` alias + auto-include rules | `crates/omp-cli/src/tools.rs` (`normalize`/plan function) |
| Golden coverage for the 11 missing visible tools | `tests-tools/ts_tools.json` + `STRICT_TOOLS` in `tests-tools/compare.ts` |
