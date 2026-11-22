# Slash Command Parity (Phase 1)

Companion to [`tools.md`](./tools.md) and [`tui.md`](./tui.md). Reference is the
read-only checkout at `/tmp/oh-my-pi-ro/packages/coding-agent/src/slash-commands/`
(upstream `omp` main).

Status legend:

- **PORTED** — reserved on the target, dispatches to a real handler with the same
  user-visible effect as the reference.
- **PARTIAL** — reserved and functional, but a named slice of reference behavior is
  absent (or the effect is simulated rather than performed).
- **STUB** — reserved and consumed, body only emits a notice.
- **UNIQUE** — tau-only command, absent from the reference registry; must survive the
  port.
- **MISSING** — ships upstream, has no reserved name on the target, so the name is
  free for an extension or a `commands/*.md` file to claim silently.

Every count below was produced by parsing the sources in this pass, not estimated.

| Side | Artifact | Measured |
|---|---|---|
| Target catalog | `crates/omp-cli/src/builtin_commands.json` | 888 lines, 47 names, 4 aliases, 112 subcommands |
| Target extra catalog | `crates/omp-cli/src/slash_extra.rs` `extra_catalog` | 822 lines, 20 names (+3 aliases) |
| Target dispatch | `crates/omp-cli/src/slash.rs` (3316) + `slash_extra.rs` | `dispatch_builtin` → `dispatch_builtin_more` → `dispatch_ported` → `dispatch_extra` |
| Reference registry | `slash-commands/builtin-registry.ts` concatenating 8 providers | 5259 lines / 14 files, **89 top-level names** |

Reference top-level names per provider (array members of each
`BUILTIN_*_SLASH_COMMANDS`, at declaration indentation):

| Provider | Count | Names |
|---|---|---|
| `builtin-modes.ts` | 21 | `security settings setup plan plan-review vibe goal guided-goal loop queue model switch fast slow skillful extended-context computer ratchet prewalk modelpreset effort` |
| `builtin-session.ts` | 20 | `todo session jobs usage stats changelog hotkeys tools context extensions agents git local hub branch fork tree login logout mcp` |
| `builtin-lifecycle.ts` | 26 | `ssh new fresh clear delete compact shake handoff resume pin unpin btw tan omfg cleanse retry debug memory rename move wt add-dir remove-dir dirs exit restart` |
| `builtin-collaboration.ts` | 11 | `advisor export trace dump share collab join leave browser copy open` |
| `builtin-control.ts` | 6 | `force live record pause forget-approvals quit` |
| `builtin-marketplace.ts` | 3 | `marketplace plugins reload-plugins` |
| `builtin-skills.ts` | 1 | `skills` |
| `builtin-commit.ts` | 1 | `commit` |

Target reserves **67 canonical names** (47 catalog + 20 extra, disjoint) plus
`/models`, `/worktree`, `/plugin`, `/force:` from the catalog and `/q`,
`/rewind`, `/status` from the extra catalog — **74 reserved strings** in total via
`is_reserved_builtin` (`slash.rs:3038`, built from the JSON catalog only) and
`is_extra_command` (`slash_extra.rs:211`). `/clear` dispatches but is reserved
nowhere (see §5).

Totals: **PORTED 43 / PARTIAL 10 / STUB 11 / UNIQUE 3 = 67 reserved**, plus
**MISSING 26** of the 89 upstream names. 63 of the 67 canonical names exist upstream
too; the names that do not are `ultra`, `rlm`, `sandbox`, and `budget` (top-level in
tau, a nested `Adjust the token budget` entry upstream).

Mechanism parity, not just the name table:

- `parse_slash_command` (`slash.rs:37`) ↔ `helpers/parse.ts` `parseSlashCommand` —
  the name/args separator is the earliest whitespace **or** `:`, which is why both
  `/force:bash ...` and `/force bash ...` parse.
- Dispatch order in `try_dispatch` (`slash.rs:125`) is builtins → `dispatch_extra` →
  WASM plugins → extension commands → file templates, matching the reference's
  builtin-before-extension precedence. Reserved names shadow extensions, which is why
  the MISSING rows are a correctness issue and not only a feature gap.
- `load_file_commands` (`slash.rs:176`) ports `loadSlashCommands`: `commands/*.md`
  under user + project dirs, `description` / `argument-hint` frontmatter, first body
  line as fallback, and `expand_file_command` reproduces `expandSlashCommand`
  (`$ARGUMENTS`, `$ARGS`, `$1..$9`, bare args appended).
- Two guards exist but do not cover the STUB rows: `slash.rs` asserts the catalog
  never advertises a command with no handler, and `slash_extra.rs`
  `test_extra_catalog_names_all_handled` asserts every extra name has an arm. A stub
  satisfies both by returning a notice.

---

## 1. Master mapping (every command)

One row per **reserved** name (67). The 26 upstream names with no reserved name
follow in §4; §1 + §4 together cover the full 89-name upstream surface plus the 4
tau-only names. `Handler` is the function that actually runs.

| # | Command | Class | Handler | Note |
|---|---|---|---|---|
| 1 | `/model` (`/models`) | PORTED | `dispatch_builtin` → resolver | `provider/id`, unique id prefix, `:level`. Upstream also takes `@role` and prefers the `--models` scope. |
| 2 | `/switch` | PORTED | `dispatch_builtin` | Session-only model switch. |
| 3 | `/effort` | PARTIAL | `dispatch_builtin` | 8 levels `off..max,auto` set the thinking level; no preset/role interplay. |
| 4 | `/fast` | PARTIAL | `fast_command` (`slash.rs:928`) | `on`→`priority` tier, `ultra`→ultrafast, `off`, `status`. Tier only, no request-level priority headers. |
| 5 | `/slow` | PARTIAL | `slow_command` (`slash.rs:968`) | flex tier on OpenAI/Google; Anthropic low priority explicitly unsupported (`SLOW_UNSUPPORTED`). |
| 6 | `/skillful` | PARTIAL | `setting_status_toggle` | Flips the setting + prints status; prompt-side skill listing effect unverified. |
| 7 | `/extended-context` | PARTIAL | `setting_status_toggle` | Same shape as `/skillful`, on `extendedContext`. |
| 8 | `/computer` | PORTED | `features::browser::computer_command` | `on`/`off`/`status`. |
| 9 | `/security` | PARTIAL | `features::security::command` | Rust verbs over the SQLite store and coordinator: `status plans plan preflight start cancel findings validate sarif cloud help`. The catalog lists upstream's 11 verbs, but `scan scans show import export compare disposition` have no handler yet (8b-2 ledger). |
| 10 | `/ratchet` | STUB | `notice` (`slash.rs:880`) | Needs the eval-harness + hill-climb runner. |
| 11 | `/prewalk` | STUB | `notice` (`slash.rs:881`) | Startup-only feature in omt (`--prewalk*` flags); `/prewalk restart` unsupported. |
| 12 | `/modelpreset` | PORTED | `modelpreset_command` | `list save switch delete`. |
| 13 | `/advisor` | PARTIAL | `features::advisor` | 8b-2: single-advisor core — a second `Agent` on the `advisor` role (upstream system prompt, `advise` tool, the session's read-only tools) reviews primary updates per `reviewMode`/`reviewInterval`; notes return as `<advisory>` blocks (steer while running with the `immuneTurns` cooldown, held for the next prompt, or a continuation at `continueSeverity`); `on/off/toggle/status/dump [raw]` drive that state with upstream's messages, `--advisor` enables it. Missing: `WATCHDOG.yml` multi-advisor rosters, advisor transcripts on disk and resumed spend, `syncBacklog` waits, tool-result eviction, watchdog/loop/emission guards, `configure` editor. |
| 14 | `/export` | PORTED | `dispatch_builtin` | HTML export, `--themes`, `--copy`/`clipboard`. |
| 15 | `/trace` | PORTED | `trace_command` (`slash.rs:2048`) | Prints the dashboard URL for this session. |
| 16 | `/dump` | PORTED | `dispatch_builtin` / `_more` | `all` → zip: transcript + LLM request JSON + one file per subagent. |
| 17 | `/share` | PORTED | `features::share::command` | Encrypted link or secret gist. |
| 18 | `/browser` | PORTED | `features::browser::command` | `headless` / `visible`. |
| 19 | `/todo` | PARTIAL | `todo_command` (`slash.rs:1425`) | All 11 verbs ported (`phasesToMarkdown`, `findPhaseFuzzy`, `findTaskFuzzy`, `normalize_in_progress`, `commitTodos`); `edit` is TUI-only outside the TUI. |
| 20 | `/session` | PARTIAL | `dispatch_builtin` | `info` only; the usage line names `delete` / `pin`, which live in `/delete` and `/pin`. |
| 21 | `/jobs` | PORTED | `slash_extra::interactive::jobs_command` | `full` for untruncated command lines. |
| 22 | `/usage` | PARTIAL | `usage_command` (`slash.rs:1663`) | `show`/`reset`; anthropic OAuth limits + reset credits. `reset` cannot select `provider/credential-id`. |
| 23 | `/stats` | PORTED | `stats_command` (`slash.rs:2029`) | In-process dashboard, `--port`/`--host`. |
| 24 | `/changelog` | PORTED | `changelog_command` (`slash.rs:2153`) | `full` / `last [N]`; `CHANGELOG.md` beside the executable. |
| 25 | `/tools` | PORTED | `dispatch_builtin_more` | Lists `dumpTools` state. |
| 26 | `/context` | PORTED | `dispatch_builtin_more` | `context_report` over `contextUsage`. |
| 27 | `/mcp` | PARTIAL | `mcp_command` (`slash.rs:2586`) | 17 subcommands incl. `smithery-*`, edits `mcp.json` preserving unknown fields; no `reconnectMcpServer` host hook, so `reauth`/`reconnect` degrade to restart guidance. |
| 28 | `/ssh` | PORTED | `features::ssh::command` | `add list remove help`. |
| 29 | `/fresh` | PORTED | `dispatch_builtin` | Resets provider stream state; transcript untouched. |
| 30 | `/compact` | PORTED | `dispatch_builtin` | `soft`, `remote`, `snapcompact`, focus text. |
| 31 | `/shake` | PORTED | `features::shake::command` | `elide` (default), `images`, `thinking`. |
| 32 | `/handoff` | PORTED | `handoff_command` → `rt.compact(focus, "handoff")` | Handoff document oneshot (`omp_compact::handoff`) then compact in place, with the streaming guard. Runs in the foreground (TS backgrounds it). |
| 33 | `/pin` | PORTED | `pin_command` | Pin session in the resume list. |
| 34 | `/retry` | PORTED | `dispatch_builtin` | Re-runs the last failed turn. |
| 35 | `/memory` | PORTED | `features::memory::handle_memory_command` | 8b-2: every verb runs on the session's shared engine (`tools::memory_backend::shared_runtime`, the one the memory tools retain through, configured `mnemopi.*` settings); `clear`/`reset` republish the `memory://` book. `mm` answers the ACP notice like TS's `handle`. |
| 36 | `/rename` | PORTED | `dispatch_builtin` | Rename, or generate a title. |
| 37 | `/move` | PARTIAL | `move_command` | Validates the path and streaming state for real, then reports the relocation gap instead of re-scoping the session. |
| 38 | `/wt` (`/worktree`) | PORTED | `features::worktree::command` | Move session into a new worktree, changes included. |
| 39 | `/add-dir` | STUB | `notice` (`slash.rs:890`) | No per-session workspace-dir store. |
| 40 | `/remove-dir` | STUB | `notice` (same arm) | Same. |
| 41 | `/dirs` | STUB | `notice` (same arm) | Cannot even list roots. |
| 42 | `/marketplace` | PORTED | `features::skills::marketplace_command` | 10 subcommands. |
| 43 | `/plugins` (`/plugin`) | PORTED | `plugins_command` | `list enable disable`. |
| 44 | `/reload-plugins` | PARTIAL | `Runtime::reload_plugins` | 8b-2: WASM plugin registry re-discovered and its tools swapped into the live tool list, base system prompt reassembled (skills re-read), slash catalog (WASM commands, `commands/*.md`) refreshed and re-announced; reports counts and load errors. Extension hosts and MCP connections are not reloaded live (the report says to /restart); `/plugins enable|disable` and marketplace installs do not trigger it. |
| 45 | `/force` (`/force:`) | PORTED | `tool_choice::force` + `tool_choice::wrap_stream` | 8b-2: TS `setForcedToolChoice`: active-tool and model checks (TS `buildNamedToolChoice`), `[forced, none]` queued and carried by the main agent's next model calls as `SimpleToolChoice`; failed/aborted calls requeue, an inactive tool drops the sequence, `/new` clears it. |
| 46 | `/budget` | PORTED | `features::budget::command` | `status`, `$amount`, `<n>k`, `off`. Top-level only in tau; upstream nests it. |
| 47 | `/sandbox` | UNIQUE | `features::sandbox::command` | Landlock + seccomp; `status/on/off/net on/net off`. |
| 48 | `/rlm` | UNIQUE | `rlm_command` (`slash_extra.rs:69`) | `[--max-calls=N] <question> @file \| -- <text>`; loads the file as `context` in the eval kernel, delegates chunks via `llm_query`. |
| 49 | `/ultra` | UNIQUE | `ultra_command` (`slash_extra.rs:186`) | `[off\|<mode>]`, `rt.set_ultra`, `omp_tui::magic::Ultra`. Distinct from `/fast ultra`. |
| 50 | `/new` | PORTED | `slash_extra` | Start a new session. |
| 51 | `/quit` (`/q`) | PORTED | `slash_extra` | `/q` is a tau-only alias. |
| 52 | `/copy` | PORTED | `interactive::copy_command` | `code\|cmd\|link` picker. |
| 53 | `/fork` | PORTED | `slash_extra` | Fork from a previous message. |
| 54 | `/branch` (`/rewind`) | PORTED | `interactive::branch_command` | Rewind keeping the old path as a branch. |
| 55 | `/plan` | PORTED | `features::plan::command` | Toggle plan mode. |
| 56 | `/goal` | STUB | `slash_extra` | Answers that goal mode is not enabled: omp-rs registers the hidden `goal` tool without a goal runtime (`tools/goal.rs` `tool()` → every op fails), so goal mode itself is unported. Blocks `/guided-goal`. |
| 57 | `/loop` | PORTED | `features::loop_mode::command` | |
| 58 | `/queue` | PORTED | `slash_extra` | |
| 59 | `/skills` | PORTED | `features::skills::command` | `search install installed update`. |
| 60 | `/agents` | PORTED | `interactive::agents_command` | |
| 61 | `/extensions` (`/status`) | PORTED | `interactive::extensions_command` | `/status` is a tau alias for the same handler. |
| 62 | `/login` | PORTED | `slash_extra` | |
| 63 | `/logout` | PORTED | `slash_extra` | |
| 64 | `/debug` | PORTED | `interactive::debug_command` | |
| 65 | `/restart` | PORTED | `features::restart::command` | |
| 66 | `/btw` | PORTED | `features::btw::command` | |
| 67 | `/collab` | PARTIAL | `collab_command` (`slash_extra.rs:593`) | `list`/`status`/`stop` only; upstream's `join`/`leave`/`open` are separate top-level commands here marked MISSING. |

`/clear` is dispatchable (`dispatch_builtin` replaces the transcript) but appears in
no catalog, so it is neither advertised nor reserved — see §5.

---

## 2. (b) tau-unique — must be preserved

Wave-1 provenance: `docs/research/14-tau-unique-features.md:54`. These carry
tau-specific machinery no upstream command reproduces, so a rebase must not delete
them on a name match.

| Command | Why preserved |
|---|---|
| `/ultra [off\|<mode>]` | Tau's own magic tier selector stored on the Runtime (`rt.ultra()`, `rt.set_ultra()`); `/fast ultra` is the *provider* ultrafast tier and is a different lever. |
| `/rlm [...]` | Opt-in recursive-context mode over the eval kernel; no reference counterpart at any layer. |
| `/sandbox [...]` | Front-end for `crates/omp-sandbox/` (Landlock + seccomp), a wave-1 fork crate that also owns one of the two `unsafe_code` allowances in the workspace. |
| `/q` (alias) | Fork convenience alias on `/quit`; upstream ships `/quit` only. |

Wave-1 commands the research doc lists (`/plan`, `/wt`, `/btw`, `/loop`, `/shake`,
`/restart`, `/skills`, `/marketplace`, `/share`, `/ssh`, `/browser`, `/computer`,
`/budget`) are tau-origin but now *also* exist upstream, so they are classed PORTED.
They remain fork-owned surfaces: implementations live in
`crates/omp-cli/src/features/*.rs`, and a rebase must not swap them for upstream
internals wholesale.

---

## 3. (c) STUB — reserved, consumed, no behavior

All 11 are honest about the gap except the four flagged below.

| Command | Target message / reason | Severity |
|---|---|---|
| `/security` | `"/security needs the security-scan mode controller (plan/run/compare), which is not ported to omt."` | honest. **Phase 1:** `builtin_commands.json` now holds a single bare `security` entry, `"description": "Not ported: security scans need the security-scan mode controller"`, with **no subcommand list** — the 11 phantom subcommands (`plan\|scan\|status\|cancel\|scans\|show\|import\|export\|validate\|compare\|disposition`) are gone. The `security_scan` tool is still absent (`tools.md` §1 #18); porting still means both together (§6 #6). |
| `/ratchet` | `"/ratchet needs the ratchet eval-harness subsystem (eval preludes + hill-climb runner), which is not ported to omt."` | honest |
| `/handoff` | ~~not wired~~ — **ported** (handoff document + compact in place, §1 #32) | resolved |
| `/prewalk` | `"Prewalk is a startup-time feature in omt (the --prewalk/--no-prewalk/--prewalk-into flags); arming or restarting it mid-session is not supported."` | honest |
| `/add-dir` | `"Multi-root workspace directories are not tracked per-session in omt; …"` | honest |
| `/remove-dir` | same notice | honest |
| `/dirs` | same notice | honest |
| `/memory` | ~~canned strings~~ — **ported** on the session's shared engine (§1 #35) | resolved (8b-2) |
| `/advisor` | ~~always inactive~~ — **partial**: a real advisor agent reviews and advises (§1 #13) | resolved (8b-2) |
| `/force` | ~~pins nothing~~ — **ported**: the next model call is forced (§1 #45) | resolved (8b-2) |
| `/reload-plugins` | ~~claimed a reload~~ — **partial**: WASM tools, prompt/skills and the slash catalog reload live; extensions/MCP need /restart (§1 #44) | resolved (8b-2) |

The four bold rows are worse than the explicit gap stubs: `/memory`, `/force` and
`/reload-plugins` claim the operation happened, and `/marketplace` / `/plugins`
mutations cannot rely on `/reload-plugins` to refresh state.

---

## 4. (c) MISSING — 26 upstream names with no reserved name

**Phase 8b:** all 26 are now reserved. `clear`, `unpin` and `hotkeys` were
already in `builtin_commands.json`; the other 23 were added there and answer
with an explicit not-implemented notice (`UNPORTED_RESERVED` in `slash.rs`,
test `every_upstream_builtin_name_is_reserved`). The table below is kept as the
list of behaviour still to port.

Exhaustive complement of §1: the full set difference between the 89 upstream
top-level names and the 74 reserved strings on the target.

Not in the catalog and not in `extra_catalog`, therefore not reserved: an extension
or a `commands/*.md` file can claim any of these today.

| Command | Upstream provider | Note |
|---|---|---|
| `/settings` | modes | **TUI-only like TS**: omp-tui opens `overlays/settings.rs`; text-mode dispatch says it needs the TUI (8b-2). |
| `/setup` | modes | First-run wizard. |
| `/plan-review` | modes | Review gate on plan mode; tau has `/plan` only. |
| `/vibe` | modes | |
| `/guided-goal` | modes | **Not ported (8b-2)**: it interviews the user and finishes with `goal create`, but omp-rs goal mode is unported (the hidden `goal` tool has no runtime, §1 #56), so the kickoff could not complete. |
| `/hotkeys` | session | Keybinding help overlay; blocked on the 9 missing binding ids (`tui.md` §1). |
| `/git` | session | In tau, `/branch` and `/fork` are *conversation* forks — port `/git` subcommand-form only. |
| `/local` | session | **Ported (8b-2), TUI-only like TS `handleTui`**: picker over the session's `local://` files or `/local <name>`, opened in `$VISUAL`/`$EDITOR` with the TUI stopped (`app.local.open` runs the same flow); text-mode dispatch says it needs the TUI. |
| `/hub` | session | `/git hub`. |
| `/tree` | session | **TUI-only like TS**: `overlays/tree.rs` (8c); text-mode dispatch says it needs the TUI (8b-2). |
| `/resume` | lifecycle | **TUI-only like TS**: `overlays/sessions.rs` with pins (8b-2); text-mode dispatch says it needs the TUI. |
| `/unpin` | lifecycle | `/pin` exists, unpin-by-id does not. |
| `/delete` | lifecycle | **Ported (8b-2)**: `Runtime::delete_session` starts a new session, then deletes the old session file and its artifacts dir (TS `newSession({ drop: true })`); "Nothing to delete (in-memory session)". |
| `/exit` | lifecycle | **TUI-only like TS**: the TUI quits; text-mode dispatch says it needs the TUI (8b-2). |
| `/clear` | lifecycle | **Dispatches in tau** but is unadvertised/unreserved — see §5. |
| `/commit` | commit | Dedicated commit flow. |
| `/live` | control | |
| `/record` | control | Session recording. |
| `/pause` | control | |
| `/forget-approvals` | control | |
| `/join` | collaboration | Sub-command of `/collab` in tau's model, absent there anyway. |
| `/leave` | collaboration | Same. |
| `/open` | collaboration | **Ported (8b-2), TUI-only like TS**: opens the last http(s) link of the latest assistant message (`open`/`xdg-open`/`start`); `link`/`url` accepted, other args print the usage. The usage hint names `/copy link` instead of TS's block drill keys (no copy selector). |
| `/tan`, `/omfg`, `/cleanse` | lifecycle | Easter-egg names; presence in the reference array is confirmed, semantics unread. |

`/providers` did **not** appear at top level in this extraction (it is nested under
`/settings`/`/setup`), so it is not counted here despite appearing in a crude
name-grep.

---

## 5. Catalog hygiene bugs

Three defects the mapping exposed, each independent of feature work. #2 is fixed in
Phase 1; #1 and #3 remain open.

1. **`/clear` is a shadow command.** `dispatch_builtin` handles it, `builtin_commands.json`
   does not list it, so it never appears in `available_commands_update`, never appears
   in autocomplete, and — because `is_reserved_builtin` reads only the JSON — can be
   shadowed by an extension while the builtin silently wins dispatch. Add it to the
   catalog rather than adding a handler.
2. ~~**`/security` advertises 11 subcommands and a matching absent tool.**~~ **RESOLVED
   (Phase 1).** The entry no longer lists subcommands: `builtin_commands.json` carries
   only `"description": "Not ported: security scans need the security-scan mode
   controller"`, so the catalog no longer promises a surface the target cannot build.
   The command stays reserved and STUB (§3), which is the honest state until the mode
   controller and `security_scan` are ported together.
3. **`/budget` and `/sandbox` are the only two of 47 catalog entries without
   `"source": "builtin"`.** Consumers that key on `source` see them as non-builtin.

---

## 6. Follow-ups, cheapest first

1. Add `clear` to `builtin_commands.json` (advertise + reserve).
2. Add `"source": "builtin"` to `budget` and `sandbox`.
3. Reserve the §4 names as STUB notices — 26 names, one `notice` arm each, extends
   `is_reserved_builtin`'s protection to the whole upstream surface.
4. Make `/memory`, `/advisor`, `/force`, `/reload-plugins` report unavailability the
   way `/ratchet` does, then decide which of the four is worth implementing.
5. Port `/unpin` (pairs with the existing `/pin`) and `/hotkeys` (after the binding
   gaps in `tui.md` §1).
6. Decide `/security`: the catalog half is done in Phase 1 — the 11 advertised
   subcommands are removed and the entry is a one-line unported notice (§3, §5 #2).
   What remains is the feature itself: port the mode controller and the `security_scan`
   tool together, or drop the reserved entry.
