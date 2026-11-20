<analysis>
**Literal Request**: Compare tool definitions and shell builtins across the two oh-my-pi source trees and oh-my-tau's `omp-cli`, producing an exact tool-by-tool parity gap list.
**Actual Need**: A definitive, actionable inventory: which tools/builtins exist on the reference side, which exist on the target side, and exactly what is missing or diverging so the caller knows what to port.
**Success Looks Like**: Absolute paths, a per-tool matrix with counts, schema-level deltas for shared tools, and a prioritized gap list requiring no follow-up search.
</analysis>

# Tool & Builtin Parity Audit: oh-my-pi → oh-my-tau

## 1. Sources of truth

| Side | Registry file | Count |
|---|---|---|
| Reference (visible) | `/tmp/oh-my-pi-ro/packages/coding-agent/src/tools/builtin-names.ts` (`BUILTIN_TOOL_NAMES`) | 29 |
| Reference (hidden) | same file (`HIDDEN_TOOL_NAMES`) | 3 |
| Reference (factories) | `/tmp/oh-my-pi-ro/packages/coding-agent/src/tools/index.ts` (`BUILTIN_TOOLS`, `HIDDEN_TOOLS`) | 29 + 3 |
| Target (visible) | `/home/whrho/git/oh-my-tau/crates/omp-cli/src/tools.rs` (`builtin_tools`) | 18 |
| Target (hidden) | same file (`hidden_tools`, `hidden_tool_by_name`) | 3 |
| Golden schema extract | `/home/whrho/git/oh-my-tau/tests-tools/ts_tools.json` | 21 tools |
| Schema differ | `/home/whrho/git/oh-my-tau/tests-tools/compare.ts` | compares 21 |

`tests-tools/ts_tools.json` covers only 21 of the 32 reference tools — `debug`, `github`, `context_notes`, `new_context`, `security_scan`, `memory_edit`, `retain`, `recall`, `reflect`, `learn`, `manage_skill` have **no golden schema** on the target side at all.

---

## 2. Model-facing tool matrix (32 reference tools)

### Visible builtins (29 reference vs 18 target)

| # | Tool | Reference | Target | Status |
|---|---|---|---|---|
| 1 | `read` | yes | yes | PRESENT (behavior gap) |
| 2 | `bash` | yes | yes | PRESENT (schema gap) |
| 3 | `edit` | yes | yes | PRESENT |
| 4 | `ast_grep` | yes | yes | PRESENT |
| 5 | `ast_edit` | yes | yes | PRESENT |
| 6 | `ask` | yes | yes | **DIVERGENT** (schema) |
| 7 | `debug` | yes | **no** | **MISSING** |
| 8 | `eval` | yes | yes | PRESENT |
| 9 | `github` | yes | **no** | **MISSING** |
| 10 | `glob` | yes | yes | PRESENT (schema gap) |
| 11 | `grep` | yes | yes | PRESENT |
| 12 | `find` | yes | yes | PRESENT (exact) |
| 13 | `lsp` | yes | yes | PRESENT |
| 14 | `checkpoint` | yes | yes | PRESENT |
| 15 | `rewind` | yes | yes | PRESENT |
| 16 | `context_notes` | yes | **no** | **MISSING** |
| 17 | `new_context` | yes | **no** | **MISSING** |
| 18 | `security_scan` | yes | **no** | **MISSING** |
| 19 | `task` | yes | yes | PRESENT |
| 20 | `wait` | yes | yes | PRESENT (exact) |
| 21 | `todo` | yes | yes | **DIVERGENT** (schema) |
| 22 | `web_search` | yes | yes | **DIVERGENT** (schema) |
| 23 | `write` | yes | yes | PRESENT |
| 24 | `memory_edit` | yes | **no** | **MISSING** |
| 25 | `retain` | yes | **no** | **MISSING** |
| 26 | `recall` | yes | **no** | **MISSING** |
| 27 | `reflect` | yes | **no** | **MISSING** |
| 28 | `learn` | yes | **no** | **MISSING** |
| 29 | `manage_skill` | yes | **no** | **MISSING** |

**Visible parity: 18/29 (62%). 11 tools missing entirely.**

### Hidden tools (3 reference vs 3 target) — full parity

| Tool | Reference | Target | Status |
|---|---|---|---|
| `think` | yes | `crates/omp-cli/src/tools/think.rs` | PRESENT |
| `yield` | yes | `crates/omp-cli/src/tools/yield_tool.rs` | PRESENT |
| `goal` | yes | `crates/omp-cli/src/tools/goal.rs` | PRESENT |

### Non-registry tools (registered outside `BUILTIN_TOOLS`) — all missing

| Tool / name | Reference location | Target | Status |
|---|---|---|---|
| `computer` | `tools/computer.ts` + `tools/computer/prelude-definition.ts` (eval prelude capability) | `/home/whrho/git/oh-my-tau/crates/omp-cli/src/features/browser.rs` has a `/computer` **slash command** only | **MISSING as tool** |
| `browser` | `tools/browser.ts` + `tools/browser/prelude-definition.ts` (eval prelude) | `features/browser.rs` `/browser` slash command only | **MISSING as tool** |
| `generate_image` | `tools/image-gen.ts`, gated by `generateImage.enabled` in `sdk.ts` | none | **MISSING** |
| `tts` | `tools/tts.ts`, gated by `speechgen.enabled` in `sdk.ts` | none | **MISSING** |
| `vibe_spawn`/`vibe_send`/`vibe_wait`/`vibe_kill`/`vibe_list` | `tools/vibe.ts` (`VibeSpawnTool` … `VibeListTool`) | none | **MISSING (5 tools)** |
| `report_tool_issue` | `tools/report-tool-issue.ts`, dispatched via `write xd://report_issue` | none | **MISSING** |
| `search` → `grep` alias | `builtin-names.ts` `LEGACY_BUILTIN_TOOL_NAME_ALIASES` | no alias table found | **MISSING (compat)** |
| `mcp__<server>_<tool>` | `isMCPToolName` | `crates/omp-cli/src/tools/mcp.rs` (`mcp__fake_echo` test) | **PRESENT** |

Note: `fetch.ts` is **not** a tool — it is `read`'s URL fetcher (`executeReadUrl`, `fetchReadUrl`, `materializeReadUrlToFile`). It should not appear as a gap row; it appears as a `read` capability gap instead.

---

## 3. Schema-level deltas for the 18 shared tools

Derived from `tests-tools/ts_tools.json` vs `/home/whrho/git/oh-my-tau/crates/omp-cli/src/tools.rs` and the TS sources.

| Tool | Delta |
|---|---|
| `bash` | Target lacks **`notify`** (`{type:"string"}`, regex reporting while running). Reference also varies schema by settings: `bashSchemaBase`, `…WithAsync`, `…WithService`, `…WithAsyncAndService`, plus `…NoPty` variants (`bash.ts:460-519`). Target emits one static schema (`tools.rs:270-290`). `name` is `maxLength:48` vs reference regex `^(?:\s*\|[A-Za-z0-9][A-Za-z0-9._-]{0,47})$`. |
| `read` | Schema identical (`path` only). **Capability gap**: reference dispatches archives (`read-archive.ts`), SQLite (`read-sqlite.ts`), PDF (`read-pdf.ts`), JSON (`read-json.ts`), binary views (`read-binary.ts`), notebook, SVG, image, and `http(s)://` URL reads via `fetch.ts`. Target `read.rs:255-320` handles only `proc://`, internal `scheme://` URLs, dirs, images, and plain UTF-8 text — no archive/sqlite/pdf/json/binary/notebook/HTTP module exists in `crates/omp-cli/src/tools/`. Target `READ_DESCRIPTION` is also truncated (drops the SQLite / archive / PDF / video / URL paragraphs the TS description has). |
| `glob` | Reference `limit?` is `1 <= number.integer <= 200` and accepts an extra `pattern` key (`GLOB_ARGUMENTS`). Target `limit` is an unconstrained `number` and has no `pattern` key (`tools.rs:297-302`). |
| `grep` | Wire schema matches exactly (`pattern`,`path`,`case`,`gitignore`,`skip: number|null`). Reference caps (`DEFAULT_FILE_LIMIT 20`, `MULTI_FILE_PER_FILE_MATCHES 10`, `SINGLE_FILE_MATCHES 50`) — target cap parity unverified. |
| `find` | Exact match (verified by `find_schema_matches_omp` test, `tools.rs:556-575`). |
| `write` | Match (`path`, `content`, required `path`). |
| `edit` | Target supports 5 modes via `vendor/pi-edit` (`Replace`,`Patch`,`ApplyPatch`,`Hashline`,`Sloppy`), each with its own schema (`tools/edit.rs:24-56`). `ts_tools.json` records only the `replace` shape — matches. Reference has extra runtime files `edit/auto-repair.md`, `edit/hashline-compact.md`, `edit/blackbox.ts`, `edit/normalize.ts`, `edit/remote-mirror.ts` with no target counterpart. |
| `ask` | **Major divergence.** Reference `askSchema = {html: string}` — one HTML `<form>` (`ask.ts:75-77`); the `questions[]` shape survives only as `legacyQuestionsHtml` recovery. Target schema is still `{questions: [...]}` (`ts_tools.json` + `tools/ask.rs`). Target is on the deprecated shape. |
| `todo` | **Divergence.** Reference `todoSchema` (`todo.ts:60-69`) = `op`, `list?`, `task?` as **`string \| string[]`**, `phase?`, `items?` — no `reason`. Target schema (`tools/todo.rs:784-796`) = `op`, `list`, `task` as **`string` only**, `phase`, `items`, **plus `reason`**. Two deltas: `task` array form missing; `reason` is target-only. (`ts_tools.json` also lists `reason`, so the golden itself disagrees with current TS source.) |
| `web_search` | **Divergence.** Reference `webSearchSchema` (`web/search/index.ts:53-60`) = `query`, `recency`, `limit`, `max_tokens`, `temperature`, `num_search_results`. Target (`tools/web_search.rs:78-89`) = `query`, `recency`, `limit` only — **3 params missing**. |
| `eval` | Not fully re-derived this session; `eval.rs` has its own `schema()`/`DESCRIPTION`. Treat as **unverified** rather than parity-confirmed. |
| `lsp` | Target `action` enum has 14 values (`diagnostics`…`request`) plus `file`,`line`,`symbol`,`query`,`new_name`,`apply`,`timeout`,`payload` (`tools/lsp.rs:45-57`). Reference schema not diffed this session — **unverified**. |
| `ast_grep` | Target `pat`,`path`,`lang`,`skip` (`tools/ast.rs:37-49`); matches `ts_tools.json`. |
| `ast_edit` | Target `ops`,`paths` (`tools/ast.rs:53+`); matches `ts_tools.json`. |
| `checkpoint` / `rewind` | `{goal}` / `{report}` — match. |
| `wait` | `{type:"object","properties":{}}` — match. |
| `think` / `yield` / `goal` | `{thoughts}` / `{type,data,error}` / `{op,objective,token_budget}` — match `ts_tools.json`. |

### Settings-gate parity (`enabled_by_settings`, `tools.rs:355-376`)

Target gates only: `bash`, `glob`, `grep`, `ast_grep`, `ast_edit`, `ask`, `todo`, `web_search`, `lsp`, `checkpoint`/`rewind`, `find` (enum `off`/`auto`).

Reference `isToolAllowed` (`tools/index.ts:801-880`) additionally gates: `edit` (on `editMode === "none"`), `eval` (backend reachability), `debug`, `github`, `security_scan`, `think`, `context_notes`/`new_context` (`compaction.experimentalContextManagement`), `wait` (`async`/`irc`/`launch`), `retain`/`recall`/`reflect`/`memory_edit` (`memory.backend` = `hindsight`/`mnemopi`), `manage_skill`/`learn` (`autolearn.enabled` + depth 0), `task` (`task.maxRecursionDepth`), `goal`. Reference also auto-includes siblings (checkpoint↔rewind pairing, ast_grep/ast_edit auto-include, memory-tool auto-include) — target has the checkpoint/rewind pairing only implicitly via settings, no auto-include logic.

---

## 4. Shell builtins — 0% parity

### Reference: `crates/builtins` (97 names in `src/extras.rs` `for_each_extra!`)

In-process utilities: `ast-grep`, `sg`, `awk`, `b2sum`, `base32`, `base64`, `basename`, `cat`, `chmod`, `cksum`, `cmp`, `comm`, `cp`, `combine`, `cut`, `date`, `dd`, `df`, `diff`, `direnv`, `dirname`, `du`, `entr`, `env`, `errno`, `fd`, `file`, `find`, `flock`, `fold`, `fswatch`, `grep`, `rg`, `head`, `hostname`, `id`, `ifne`, `isutf8`, `jq`, `ln`, `ls`, `md5`, `md5sum`, `mkdir`, `mktemp`, `mv`, `nl`, `nproc`, `od`, `open`, `xdg-open`, `start`, `paste`, `printenv`, `readlink`, `realpath`, `rm`, `rmdir`, `rsync`, `say`, `sd`, `sed`, `seq`, `sha1sum`, `sha224sum`, `sha256sum`, `sha384sum`, `sha512sum`, `shasum`, `sort`, `sponge`, `stat`, `strings`, `tac`, `tail`, `tar`, `tee`, `touch`, `tr`, `truncate`, `ts`, `uname`, `uniq`, `uptime`, `watchexec`, `wc`, `which`, `whoami`, `xargs`, `yes`.

Process builtins (7): `pgrep`, `pidwait`, `pkill`, `lsof`, `ps`, `sleep`, `top`.

Gate switches: `PI_DISABLE_UUTILS_BUILTINS`, `PI_DISABLE_UUTILS_DESTRUCTIVE`, `PI_DISABLE_RM_BUILTIN`, `PI_DISABLE_MV_BUILTIN` (`src/extras.rs:93-133`).

### Reference: `crates/shell`

- bash 5.3 engine with zsh dialect (`src/bash/`), 61 command builtins in `src/bash/vm/builtins/doc_table.rs`: `.` `:` `[` `alias` `bg` `bind` `break` `builtin` `caller` `cd` `command` `compgen` `complete` `compopt` `continue` `declare` `dirs` `disown` `echo` `enable` `eval` `exec` `exit` `export` `false` `fc` `fg` `getopts` `hash` `help` `history` `jobs` `kill` `let` `local` `logout` `mapfile` `popd` `printf` `pushd` `pwd` `read` `readarray` `readonly` `return` `set` `shift` `shopt` `source` `suspend` `test` `times` `trap` `true` `type` `typeset` `ulimit` `umask` `unalias` `unset` `wait`, plus 16 reserved-word/help-only entries (`!` `%` `(( … ))` `[[ … ]]` `case` `coproc` `for` `for ((` `function` `if` `select` `time` `until` `variables` `while` `{ … }`).
- zsh `CORE_BUILTINS` (~60 names) in `src/bash/vm/builtins/zsh_module.rs:109-115`; 193 ZLE widgets in `src/bash/vm/zle/widgets/mod.rs`.
- Extra host callouts added by `src/tools/mod.rs:70-80` (`builtins()`): **`timeout`, `urlmap`, `nohup`** (+ the 97 extras), and the `git` guard/worktree layers (`src/tools/git.rs`), `minimizer` (~60 TOML defs + 22 filter modules), `taps`, `cancel`, `proctree`, `vfs`, persistent `Session`, and the `obash` standalone binary.

### Target: `/home/whrho/git/oh-my-tau`

- **No `builtins` crate, no `shell` crate.** Workspace members are `crates/{omp-auth-broker, omp-cli, omp-collab, omp-compact, omp-eval, omp-ext, omp-goose, omp-judge, omp-lsp, omp-mcp, omp-providers, omp-sandbox, omp-session, omp-settings, omp-sot, omp-stats, omp-task, omp-tui, omp-urls, omp-wasm}` — nothing named `builtins`/`shell`.
- No dependency on `omp-builtins` / `omp-shell` / `omp-vfs` / `uutils` anywhere in the workspace (`grep` over all `*.toml`/`*.rs` returned nothing but comment text).
- No coreutils implementations, no minimizer, no persistent shell session, no `obash`.
- `bash` is a **plain subprocess**: `tokio::process::Command::new("bash").arg("-c").arg(script)` or `setsid --wait bash -c` (`crates/omp-cli/src/tools/bash.rs:121-155`, `:580-586`). The tool's own doc header states this explicitly: *"Non-persistent shell (omp embeds a brush-based shell; see the port report)"* (`bash.rs:3`).
- **Doc drift**: the reference shell is a bash 5.3 port with a zsh dialect, not brush. The `brush` attribution in `bash.rs:3` is wrong and should be corrected when the port is done.

**Shell/builtin parity: 0 of 97 extras, 0 of 61 bash builtins, 0 of 3 host callouts (`timeout`/`nohup`/`urlmap`), no persistent session, no minimizer.**

---

## 5. Eval prelude parity

| Global | Reference (`src/eval/js/shared/prelude.txt`) | Target (`crates/omp-eval/src/prelude*.js`) |
|---|---|---|
| `read`,`write`,`env`,`tool`,`output`,`log`,`phase`,`print` | yes | yes |
| `sh` | yes | **no** |
| `console` | yes | **no** |
| `agent`,`wait`,`judge`,`judgeBatch`,`workpool`,`budget`,`completion` | yes | yes |
| `AgentHandle`,`CompletionHandle`,`JudgmentBatch`,`JudgmentItem`,`WorkPool` | yes | yes (+ `PendingHandle`) |
| `llm_query`,`llm_stats`,`rlm_context` | no | yes (target-only, RLM) |
| `browser`,`computer` preludes | yes (`tools/browser/prelude-definition.ts`, `tools/computer/prelude-definition.ts`) | **no** |

---

## 6. Dangling references on the target side

`/home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/task.rs:25-42` defines `READ_ONLY_TOOLS` containing `"recall"`, `"reflect"`, `"retain"`, `"memory_edit"`, `"checkpoint"`, `"rewind"`, `"yield"` — but `recall`/`reflect`/`retain`/`memory_edit` are not constructible by `builtin_tools` or `hidden_tool_by_name`. A subagent tool whitelist naming them silently resolves to nothing.

`/home/whrho/git/oh-my-tau/crates/omp-cli/src/builtin_commands.json` declares a `security` slash command with subcommands `plan|scan|status|cancel|scans|show|import|export|validate|compare|disposition` — but no `security_scan` tool and no `SecurityScan` implementation exists in any crate.

`/home/whrho/git/oh-my-tau/crates/omp-tui/src/overlays/settings_defs.rs:2832` advertises a `github` tool setting ("op-based dispatch for repository, issue, pull request, diff, search, checkout, push, and Actions watch workflows") with no implementation behind it.

---

<results>
<files>
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/builtin-names.ts - canonical reference registry: 29 visible `BUILTIN_TOOL_NAMES`, 3 `HIDDEN_TOOL_NAMES`, help/default-state table, `search`→`grep` legacy alias
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/index.ts - `BUILTIN_TOOLS`/`HIDDEN_TOOLS` factory maps (lines 628-668) and the full `isToolAllowed` settings-gate matrix (lines 801-880)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools.rs - target registry: 18 visible tools in `builtin_tools` (lines 262-355), `hidden_tools` (think/yield/goal), `enabled_by_settings` gate list (lines 355-376)
- /home/whrho/git/oh-my-tau/tests-tools/ts_tools.json - golden schema extract for only 21 of 32 reference tools; the parity test surface
- /home/whrho/git/oh-my-tau/tests-tools/compare.ts - schema differ; enumerates exactly which tools are compared and how (STRICT_TOOLS set at line 22)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/edit.rs - 5 edit-mode schemas (Replace/Patch/ApplyPatch/Hashline/Sloppy), lines 24-56
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/read.rs - read dispatch (lines 255-320); no archive/sqlite/pdf/json/binary/notebook/HTTP path
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/bash.rs - bash is `bash -c` subprocess, not an embedded shell (lines 121-155); doc header wrongly says "brush-based" (line 3)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/todo.rs - todo schema with target-only `reason` and `task: string` (lines 784-796)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/web_search.rs - web_search schema missing max_tokens/temperature/num_search_results (lines 78-89)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/ask.rs - ask still on legacy `{questions: [...]}` shape
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/task.rs - `READ_ONLY_TOOLS` whitelists recall/reflect/retain/memory_edit which are not constructible (lines 25-42)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/builtin_commands.json - 47 slash commands incl. `security` subcommands with no backing tool
- /tmp/oh-my-pi-ro/crates/builtins/src/extras.rs - the 97-name extras table (`for_each_extra!`) and the 4 disable switches
- /tmp/oh-my-pi-ro/crates/builtins/Cargo.toml - per-utility feature gates and the `utils` aggregate feature
- /tmp/oh-my-pi-ro/crates/shell/src/bash/vm/builtins/doc_table.rs - bash 5.3's 61 command builtins + 16 reserved-word entries
- /tmp/oh-my-pi-ro/crates/shell/src/bash/vm/builtins/zsh_module.rs - `CORE_BUILTINS` zsh dialect list (lines 109-115)
- /tmp/oh-my-pi-ro/crates/shell/src/tools/mod.rs - `builtins()` adds `timeout`, `urlmap`, `nohup` on top of the extras (lines 70-80)
- /tmp/oh-my-pi-ro/crates/shell/src/lib.rs - layer map: bash engine, sys, vfs host, persistent Session, minimizer, taps, cancel, proctree, obash
- /tmp/oh-my-pi-ro/crates/shell/src/minimizer/ - ~60 TOML defs + 22 filter modules; no target counterpart
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/bash.ts - `notify` param and the 6 schema variants (lines 460-519)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/ask.ts - `askSchema = {html: string}` (lines 75-77); `questions` only as legacy recovery
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/todo.ts - `task?: string | string[]`, no `reason` (lines 60-69)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/web/search/index.ts - `webSearchSchema` with 6 params (lines 53-60)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/glob.ts - `limit?` as `1 <= integer <= 200`, extra `pattern` key (lines 37-47)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/sdk.ts - settings-gated custom tools `generate_image` and `tts` (lines 4117-4126); xdev mounting (lines 4018-4352)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/tools/vibe.ts - 5 vibe tools: `vibe_spawn`/`vibe_send`/`vibe_wait`/`vibe_kill`/`vibe_list` (lines 87-281)
- /tmp/oh-my-pi-ro/packages/coding-agent/src/eval/js/shared/prelude.txt - reference eval globals incl. `sh` and `console`
- /home/whrho/git/oh-my-tau/crates/omp-eval/src/prelude.js - target eval globals; has `print`, lacks `sh`/`console`
- /home/whrho/git/oh-my-tau/crates/omp-eval/src/prelude_agents.js - agent/judge/workpool/budget/completion globals (matches reference) plus target-only `llm_query`/`llm_stats`/`rlm_context`
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/overlays/settings_defs.rs - advertises a `github` tool setting with no implementation (line 2832)
</files>

<answer>
Tool parity is 18/29 visible builtins (62%) plus 3/3 hidden tools. Eleven visible tools are missing outright: `debug`, `github`, `context_notes`, `new_context`, `security_scan`, `memory_edit`, `retain`, `recall`, `reflect`, `learn`, `manage_skill`. Seven additional reference tools that live outside the `BUILTIN_TOOLS` registry are also missing: `computer` and `browser` (eval-prelude capabilities), `generate_image` and `tts` (settings-gated custom tools), the five `vibe_*` tools, and `report_tool_issue` (an `xd://report_issue` device behind `write`). The `search`→`grep` legacy alias has no target equivalent.

For the 18 shared tools, schemas match exactly for `find`, `write`, `wait`, `checkpoint`, `rewind`, `ast_grep`, `ast_edit`, `think`, `yield`, `goal`, and `grep`'s wire shape. Four are genuinely divergent: `ask` (target still on the deprecated `{questions:[…]}` shape; reference is `{html: string}`), `todo` (target `task` is `string` not `string|string[]`, and target adds a `reason` field the reference schema does not have), `web_search` (target omits `max_tokens`, `temperature`, `num_search_results`), and `bash` (target omits `notify` and collapses six settings-dependent schema variants into one static schema; `name` is `maxLength:48` instead of the reference's character-class regex). `glob`'s `limit` is unconstrained on the target and it rejects the extra `pattern` key. `read`'s schema is identical but its behavior is far narrower: the reference dispatches archives, SQLite, PDF, JSON, binary views, notebooks, SVG, and `http(s)://` URLs through `read-archive.ts`/`read-sqlite.ts`/`read-pdf.ts`/`read-json.ts`/`read-binary.ts`/`fetch.ts`, none of which exist under `crates/omp-cli/src/tools/`. `eval` and `lsp` schemas were not fully diffed and should be treated as unverified rather than passing.

Shell and builtin parity is zero. `crates/builtins` ships 97 in-process names (90 utilities including `ast-grep`/`rg`/`awk`/`jq`/`sed`/`fd`/`rsync`/`tar`/`watchexec`, plus 7 process builtins `pgrep`/`pidwait`/`pkill`/`lsof`/`ps`/`sleep`/`top`); `crates/shell` ships a bash 5.3 engine with a zsh dialect, 61 command builtins, 193 ZLE widgets, a persistent session, a ~60-def output minimizer, and the `timeout`/`nohup`/`urlmap` host callouts. oh-my-tau has no `builtins` or `shell` crate, no dependency on any of them, and its `bash` tool simply spawns the host's `bash -c`. The tool's own doc comment attributes the reference shell to `brush`, which is factually wrong and will mislead the port.

Settings-gate coverage is also narrower: the target gates 11 tool ids, while the reference gates 22 including `edit` (on `editMode === "none"`), `eval`, `debug`, `github`, `security_scan`, `think`, `context_notes`/`new_context`, `wait`, the four memory tools, `manage_skill`/`learn`, and `task` depth. Three dangling declarations exist on the target side: `task.rs` whitelists four memory tools that cannot be constructed, `builtin_commands.json` advertises a `security` slash command with 11 subcommands and no backing `security_scan` tool, and `settings_defs.rs` advertises a `github` tool setting with no implementation.
</answer>

<next_steps>
Priority order for closing the gaps:

1. **Delete the dangling declarations first** (`task.rs` `READ_ONLY_TOOLS` memory entries, the `security` slash command, the `github` setting) — otherwise the tool whitelist and the settings UI lie about what exists.
2. **Fix the three cheap schema divergences** and add them to `tests-tools/ts_tools.json` so `compare.ts` guards them: `bash.notify`, `web_search` `max_tokens`/`temperature`/`num_search_results`, `todo` `task: string|string[]` (and decide whether target-only `reason` is intentional).
3. **Rewrite `ask` to the `{html: string}` form**, keeping `questions` only as legacy recovery, matching `ask.ts:75-93`.
4. **Add the four `memory.*` tools** (`retain`/`recall`/`reflect`/`memory_edit`) plus `learn` and `manage_skill` — these are the largest coherent missing family and already have a settings schema (`memory.backend`, `autolearn.enabled`) to gate them.
5. **Add `context_notes`/`new_context`** (gated by `compaction.experimentalContextManagement`) — small tools, and they pair with the existing `read`/`grep` auto-include rule.
6. **Add `github` and `debug`** — both are large; `github` has 8 supporting modules in the reference (`gh-common`, `gh-pr-checkout`, `gh-pr-diff`, `gh-run-watch`, `gh-search`, `gh-view`, `github-cache`, `gh-cache-invalidation`), and `debug` needs a DAP client.
7. **Broaden `read`** to the reference dispatch (archives, SQLite, PDF, JSON, binary, notebook, HTTP URL fetch) and restore the full `READ_DESCRIPTION` — the schema already matches, so only behavior and description change.
8. **Port `crates/builtins` then `crates/shell`** as a separate, much larger track. The bash engine, persistent session, minimizer, and 97 extras are the single biggest structural difference; nothing else on the target side substitutes for them. Correct the `brush` attribution in `bash.rs:3` while doing it.
9. **Add the `search`→`grep` alias and the reference auto-include/sibling rules** to `tools.rs` so explicit `--tools` lists behave identically.
10. **Extract golden schemas for the 11 missing tools** into `tests-tools/ts_tools.json` and extend `compare.ts`'s tool list before implementing, so the ports land against a reference rather than from memory.
</next_steps>
</results>