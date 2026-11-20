<analysis>
**Literal Request**: Inventory /home/whrho/git/oh-my-tau/crates/omp-cli — Cargo.toml, every src/**/*.rs path + LOC, main.rs, commands, RPC mode, web server, daemon — and enumerate implemented subcommands, slash commands, tools, modes.
**Actual Need**: A navigable map of the CLI surface with absolute paths so the caller can jump straight to any handler without re-searching.
**Success Looks Like**: Absolute paths + LOC table, plus complete lists of subcommands, slash commands, tools, and run modes each tied to its defining file/line.
</analysis>

## Crate facts

`/home/whrho/git/oh-my-tau/crates/omp-cli/Cargo.toml`
- package `omp-cli`, binary **`omt`** (`path = "src/main.rs"`), version/edition/license from workspace.
- Features: `default = ["onnx"]`, `onnx = ["omp-judge/onnx"]`, `goose = ["dep:omp-goose"]` (off by default).
- Linux-only dep: `omp-sandbox` (Landlock/seccomp; `omt` re-execs itself as `omt __sandbox-exec`).
- Heavy internal deps: pa-agent/pa-ai/pa-types, omp-session, omp-ext, omp-compact, omp-task, omp-eval, omp-wasm, omp-mcp, omp-lsp, omp-judge, omp-providers, omp-settings, omp-tui, omp-stats, omp-collab, omp-urls, vendored pi-ast/pi-edit/pi-walker.
- Web stack: hyper 1 + hyper-util + http-body-util + tokio-tungstenite 0.28.
- **Total: 70,125 LOC across 88 `src/**/*.rs` files** (plus `src/builtin_commands.json`, 19,873 bytes; `src/subcmd/web/assets/*` JS/CSS).

## LOC by file (all `src/**/*.rs`, absolute under `/home/whrho/git/oh-my-tau/crates/omp-cli/`)

| File | LOC | | File | LOC |
|---|---|---|---|---|
| src/runtime.rs | 6028 | | src/tools/rlm.rs | 1644 |
| src/slash.rs | 3318 | | src/compact.rs | 1651 |
| src/tools/eval_agents.rs | 3200 | | src/tools/lsp.rs | 2152 |
| src/tools/web_search.rs | 2050 | | src/features/skills.rs | 2064 |
| src/recovery.rs | 2022 | | src/ext.rs | 1981 |
| src/rpc.rs | 1747 | | src/events.rs | 1429 |
| src/tools/bash.rs | 1364 | | src/context.rs | 1403 |
| src/sysprompt.rs | 1263 | | src/tools/read.rs | 1276 |
| src/approval.rs | 1233 | | src/tools/search.rs | 1139 |
| src/tools/ask.rs | 1102 | | src/subcmd/models.rs | 1094 |
| src/tools/eval_bridge.rs | 1009 | | src/tui.rs | 982 |
| src/cli.rs | 916 | | src/tools/todo.rs | 910 |
| src/features/worktree.rs | 901 | | src/slash_extra/interactive.rs | 899 |
| src/tools/task.rs | 892 | | src/main.rs | 868 |
| src/slash_extra.rs | 824 | | src/features/ssh.rs | 763 |
| src/title.rs | 758 | | src/tools/util.rs | 735 |
| src/adapter.rs | 707 | | src/tools/eval_journal.rs | 707 |
| src/herdr.rs | 688 | | src/tools/ast.rs | 686 |
| src/persist.rs | 636 | | src/model.rs | 625 |
| src/tools/rlm_combinators.rs | 591 | | src/subcmd/stats.rs | 581 |
| src/tools.rs | 579 | | src/tree.rs | 578 |
| src/subcmd/web/mod.rs | 534 | | src/context_policy.rs | 531 |
| src/tools/eval.rs | 497 | | src/features/browser.rs | 495 |
| src/tools/relevance.rs | 494 | | src/subcmd/daemon.rs | 491 |
| src/images.rs | 484 | | src/tools/goal.rs | 462 |
| src/tools/jobs.rs | 448 | | src/tools/checkpoint.rs | 430 |
| src/tools/find.rs | 423 | | src/tools/edit.rs | 409 |
| src/subcmd/export.rs | 409 | | src/subcmd/gc.rs | 362 |
| src/features/plan.rs | 372 | | src/dedup.rs | 367 |
| src/subcmd/login.rs | 348 | | src/features/loop_mode.rs | 345 |
| src/tools/eval_tests.rs | 341 | | src/features/budget.rs | 329 |
| src/features/shake.rs | 321 | | src/tools/mcp.rs | 305 |
| src/subcmd/web/agent.rs | 304 | | src/features/sandbox.rs | 291 |
| src/tools/wasm.rs | 258 | | src/paging.rs | 249 |
| src/slash_extra/dialog.rs | 245 | | src/slash_extra/login.rs | 232 |
| src/notices.rs | 331 | | src/agent.rs | 307 |
| src/tools/yield_tool.rs | 306 | | src/subcmd/web/sessions.rs | 202 |
| src/subcmd/collab.rs | 203 | | src/features/restart.rs | 198 |
| src/methodology.rs | 158 | | src/tools/tree.rs | 158 |
| src/tools/wait.rs | 153 | | src/subcmd/stats_judge.rs | 152 |
| src/subcmd/config.rs | 142 | | src/tools/think.rs | 130 |
| src/subcmd/serve_collab.rs | 120 | | src/subcmd/usage_broker.rs | 103 |
| src/subcmd/mod.rs | 67 | | src/subcmd/judge_sessions.rs | 65 |
| src/subcmd/web/assets.rs | 37 | | src/features/mod.rs | 15 |
| src/subcmd/web/wallpapers.rs | 161 | | src/features/btw.rs | 135 |

Tests: `tests/login_e2e.rs`, `tests/print_slash.rs`.

## Run modes (`src/main.rs`, `src/cli.rs`)

`MODE_VALUES` = `text | json | rpc | acp | rpc-ui`; `acp` and `rpc-ui` are **rejected at startup** as unsupported (`main.rs` `async_main`). Actual modes:

1. **Interactive TUI** — default when `mode.is_none() && !print && prompt.is_none() && !fork && stdin/stdout are TTYs` → `tui::run(...)` (`src/tui.rs`). Multi-thread tokio runtime.
2. **Print / headless** — `-p`/`--print` or any positional prompt → `run_print()` in `main.rs`; extension lifecycle hooks + single prompt.
3. **JSON** — `--mode json`: prints the session transcript JSONL to stdout after the run.
4. **RPC** — `--mode rpc` → `rpc::run(agent, target, cwd, persistence)` or `rpc::run_with(..., ext.rpc_on_start())` (`src/rpc.rs`); current-thread tokio runtime; session lifecycle owned by the RPC runtime; `--no-ui` disables client-answered dialogs.
5. **Sandbox launcher** — `omt __sandbox-exec ...` handled before any runtime setup, `omp_sandbox::exec_confined` (`src/features/sandbox.rs`).
6. **`--dump-system-prompt`** — builds and prints the system prompt without a provider.
7. Not supported: `--profile`, `--from-claude`, `--from-codex`, `--goal` (non-interactive).

## Subcommands (`src/subcmd/mod.rs`, dispatched by leading non-flag token before flag parsing)

`known()` / `run()`:

| Subcommand | File | Notes |
|---|---|---|
| `models` | src/subcmd/models.rs | verbs `ls`/`list`, `find`, `refresh`; `--json`, `--kind` |
| `stats` | src/subcmd/stats.rs | `-p/--port`, `--host`, `-j/--json`, `-s/--summary/--sync` |
| `config` | src/subcmd/config.rs | `list`, `get <key>`, `path`; `set`/`reset` accepted then rejected |
| `gc` | src/subcmd/gc.rs | `--apply`, `--json`, `--agent-dir`, `--cold-archive-after-days`, `--retain-newest-global`, `--retain-newest-per-cwd` |
| `export` | src/subcmd/export.rs | `omt export <session.jsonl> [-o out.html]` (HTML exporter) |
| `judge-sessions` | src/subcmd/judge_sessions.rs | `<dir\|file> [--out --limit --ckpt --onnx --threads]` |
| `login` | src/subcmd/login.rs | OAuth login |
| `logout` | src/subcmd/login.rs | OAuth logout |
| `collab` | src/subcmd/collab.rs | `list [--json]`, `link <id\|pid> [--view] [--json]` |
| `serve-collab` | src/subcmd/serve_collab.rs | `[--port --host]` relay server |
| `web` | src/subcmd/web/mod.rs | browser UI, see below |
| `daemon` | src/subcmd/daemon.rs | **Unix only** (`#[cfg(unix)]`), see below |

## Slash commands

Three tiers, dispatched in `slash::try_dispatch` (`src/slash.rs:125`): builtin catalog → `slash_extra` → wasm plugin → extension → file command → `NotSlash`.

**Tier 1 — `src/builtin_commands.json`, 47 entries** (reserved names; each reaches a real handler in `dispatch_builtin` / `dispatch_builtin_more` / `dispatch_ported` or an honest "not ported" notice; TUI-only ones emit "/X is only available in the interactive TUI"):
`security, model (alias models), switch, fast, slow, skillful, extended-context, computer, ratchet, prewalk, modelpreset, effort, advisor, export, trace, dump, share, browser, todo, session, jobs, usage, stats, changelog, tools, context, mcp, ssh, fresh, compact, shake, handoff, pin, retry, memory, rename, move, wt (alias worktree), add-dir, remove-dir, dirs, marketplace, plugins (alias plugin), reload-plugins, force (alias force:), budget, sandbox`

Handlers with real logic in `src/slash.rs`: `model/models`, `switch`, `effort`, `fresh`, `compact` (soft/remote/snapcompact modes), `clear`, `export`, `retry`, `rename`, `session`, `tools`, `context`, `jobs`, `advisor`, `memory`, `reload-plugins`, `force`, `dump`, plus `fast`, `slow`, `todo`, `usage`, `stats`, `changelog`, `trace`, `mcp`, `modelpreset`, `pin`, `skillful`, `extended-context`, `handoff`, `move`, `plugins`. Feature-backed (`src/features/*`): `computer`/`browser`, `share`, `ssh`, `shake`, `wt`/`worktree`, `marketplace` (skills.rs), `budget`, `sandbox`. Deliberate stubs: `ratchet`, `prewalk`, `security`, `add-dir`/`remove-dir`/`dirs`.

**Tier 2 — `src/slash_extra.rs` `extra_catalog()` + `dispatch_extra()`, 20 entries**:
`rlm, new, quit (alias q), copy, fork, branch (alias rewind), plan, goal, loop, queue, skills, agents, extensions (alias status), login, logout, debug, restart, btw, ultra, collab`
(`is_extra_command` lists 22 names including aliases; catalog↔handler coverage is enforced by a unit test.)

**Tier 3 — dynamic**: `commands/*.md` file commands from `~/.omp/agent/commands` and `<cwd>/.omp/commands` (`src/slash.rs` `load_file_commands`), extension commands, and wasm plugin commands (`src/tools/wasm.rs::command_catalog`).

**TUI-local (`crates/omp-tui/src/app.rs`)** — handled by the app, not the runtime: `exit, help, hotkeys, settings, model, switch, effort, resume, tree, theme, session, usage, clear` (`P0_BUILTINS` + `LOCAL_COMMANDS`); bare-confirm set `exit, clear, compact, export`.

## Tools (`src/tools.rs::builtin_tools`, order asserted by test)

**18 visible built-ins**: `read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask, todo, web_search, lsp, checkpoint, rewind`.

- `read` `src/tools/read.rs`; `bash` `src/tools/bash.rs` (pty/async/name/ready); `edit`+`write` `src/tools/edit.rs` (pi-edit, hashline/replace); `glob`+`grep` `src/tools/search.rs`; `find` `src/tools/find.rs`; `task` `src/tools/task.rs`; `wait` `src/tools/wait.rs`; `eval` `src/tools/eval.rs`; `ast_grep`/`ast_edit` `src/tools/ast.rs`; `ask` `src/tools/ask.rs`; `todo` `src/tools/todo.rs`; `web_search` `src/tools/web_search.rs`; `lsp` `src/tools/lsp.rs`; `checkpoint`/`rewind` `src/tools/checkpoint.rs`.
- **3 hidden tools** (`hidden_tools` / `hidden_tool_by_name`): `think` (needs `externalThinking` + capable model), `yield` (subagent runs), `goal` (goal mode active).
- **MCP tools** are added dynamically as `mcp__<server>` (`src/tools/mcp.rs`); wasm plugin tools via `src/tools/wasm.rs`.
- Settings gating in `enabled_by_settings`: `find.enabled` (enum, `off` hides), `bash/glob/grep/astGrep/astEdit/ask/todo/web_search/lsp/checkpoint.enabled`; `ast_grep`, `checkpoint`, `rewind` default **off**.
- Supporting machinery: `eval_bridge` (`tool.<name>` dispatch from eval cells), `eval_agents`, `eval_journal`, `eval_tests`, `rlm`, `rlm_combinators`, `relevance` (query-ranked spill views), `jobs` (`proc://` + background bash), `util`.

## RPC mode (`src/rpc.rs`, 55 methods)

`negotiate_protocol, prompt, remove_queued_message, promote_queued_message, abort, abort_and_prompt, new_session, switch_session, open_session, get_state, set_fast_mode, goal, set_ask_dialog, get_available_commands, get_entries, get_tree, set_todos, set_host_tools, set_host_uri_schemes, set_subagent_subscription, set_event_filter, get_subagents, get_subagent_messages, cancel_subagent, steer_subagent, live_start, live_stop, live_mute, set_model, cycle_model, get_available_models, set_thinking_level, cycle_thinking_level, get_available_thinking_levels, set_steering_mode, set_follow_up_mode, set_interrupt_mode, compact, set_auto_compaction, set_cache_warming, set_auto_retry, abort_retry, abort_bash, get_session_stats, export_html, get_branch_messages, get_last_assistant_text, set_session_name, handoff, get_messages, get_messages_page, get_login_providers, login, predict_word, predict_word_feedback`
(`live_start` returns "unsupported: Live voice"; `slash` is `#[path = "slash.rs"] pub mod slash` — same dispatcher as TUI/print.)

## Web server (`src/subcmd/web/`, `omt web`)

Usage: `omt web [--port <port>] [--host <host>] [--cwd <dir>] [--open]`, default `http://127.0.0.1:7467`. Local-only: loopback bind, same-origin + Host checks reject cross-origin.

Routes (`src/subcmd/web/mod.rs::route`):
- `GET /`, `/assets/*` — embedded SPA (`assets.rs` + `assets/` JS/CSS)
- `GET /api/info`, `/api/projects`, `/api/sessions?cwd=`, `/api/fs?path=`
- `GET /api/agents`, `POST /api/agents {cwd, resume?, model?}`, `DELETE /api/agents/<id>` (each agent = a spawned `omt --mode rpc` child, `agent.rs`)
- `GET /api/export?session=<file>` — HTML export
- `GET/POST /api/wallpapers`, `DELETE /api/wallpapers/<name>`, `GET /wallpapers/custom/<name>` (`wallpapers.rs`)
- `GET /ws/<agentId>` — WebSocket bridge carrying RPC frames both ways

## Daemon (`src/subcmd/daemon.rs`, Unix only)

`omt daemon [--socket <path>]` / `omt daemon status` / `omt daemon stop`.
Unix-socket listener at `$XDG_RUNTIME_DIR/omt.sock` else `/tmp/omt-<uid>.sock`, pidfile `<socket>.pid`. Newline-delimited JSON; optional first line `{"type":"hello","cwd":"..."}` selects the child cwd; every other line is forwarded verbatim to a freshly spawned `omt --mode rpc` child (one per connection), child stdout lines piped back. `{"type":"daemon-stop"}` shuts it down.

<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-cli/Cargo.toml - package/bin definition (`omt`), feature flags (`onnx` default, `goose` opt-in), full dep list
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/main.rs - entry point; HELP text, subcommand pre-dispatch, mode selection (TUI/print/json/rpc), `run_print`, sandbox re-exec, LSP shutdown
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/cli.rs - argv parser: STRING_FLAGS/OPTIONAL_FLAGS/VALUELESS_FLAGS, `MODE_VALUES`, `@file`/image attachment expansion
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/subcmd/mod.rs - `known()`/`run()`: the 12-subcommand registry and dispatch table
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/subcmd/web/mod.rs - web server: USAGE, default port 7467, `route()` with every HTTP path
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/subcmd/daemon.rs - daemon protocol, socket/pidfile paths, `hello` and `daemon-stop` frames
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/rpc.rs - 55 RPC methods, `run`/`run_with`, re-exports `slash.rs`
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/slash.rs - slash dispatcher (`try_dispatch`), `dispatch_builtin`, `dispatch_builtin_more`, `dispatch_ported`, `builtin_command_catalog`, file-command loader
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/builtin_commands.json - the 47-entry builtin slash catalog (names, aliases, descriptions, input hints)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/slash_extra.rs - 20 extra slash commands (`extra_catalog`, `dispatch_extra`), rlm/ultra/collab handlers
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools.rs - builtin tool registry (18 visible), `hidden_tools` (think/yield/goal), settings gating, edit-mode selection
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/features/mod.rs - the 12 feature modules backing slash commands (plan, worktree, btw, loop_mode, shake, restart, skills, share, ssh, browser, budget, sandbox)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/app.rs - TUI-local slash handling (`P0_BUILTINS`, `LOCAL_COMMANDS`, `execute_slash`)
- /home/whrho/git/oh-my-tau/crates/omp-cli/tests/print_slash.rs - print-mode slash command tests
- /home/whrho/git/oh-my-tau/crates/omp-cli/tests/login_e2e.rs - login subcommand e2e test
</files>

<answer>
`omp-cli` builds a single binary, `omt`, at 70,125 LOC / 88 source files.

Surface inventory:
1. Subcommands (12): models, stats, config, gc, export, judge-sessions, login, logout, collab, serve-collab, web, daemon (daemon is `#[cfg(unix)]`). Dispatched in `main.rs::parse_args` via `subcmd::known()`/`subcmd::run()` before flag parsing.
2. Modes (4 real): interactive TUI (TTY default, `src/tui.rs`), print/headless (`-p` or positional prompt, `run_print`), json (`--mode json`, dumps session JSONL), rpc (`--mode rpc`, `src/rpc.rs`, 55 methods). `--mode acp` and `--mode rpc-ui` parse but are explicitly rejected. Plus two special paths: `--dump-system-prompt` and the `omt __sandbox-exec` self-relaunch for Landlock/seccomp.
3. Tools: 18 visible built-ins (`read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask, todo, web_search, lsp, checkpoint, rewind`) with `ast_grep`/`checkpoint`/`rewind` off by default via settings; 3 hidden (`think, yield, goal`); dynamic `mcp__*` tools and wasm plugin tools.
4. Slash commands: 47 builtin catalog entries (JSON, reserved names) + 20 extra (`rlm, new, quit/q, copy, fork, branch/rewind, plan, goal, loop, queue, skills, agents, extensions/status, login, logout, debug, restart, btw, ultra, collab`) + 13 TUI-local (exit, help, hotkeys, settings, model, switch, effort, resume, tree, theme, session, usage, clear) + dynamic file (`commands/*.md`), extension, and wasm commands. Dispatch order: builtin → extra → wasm → extension → file → not-a-slash.
5. Web server: `omt web`, loopback default `127.0.0.1:7467`, SPA + REST (`/api/info|projects|sessions|fs|agents|export|wallpapers`) + `/ws/<agentId>` WebSocket bridge, each session a spawned `omt --mode rpc` child.
6. Daemon: `omt daemon [start|status|stop] [--socket]`, newline-JSON Unix socket at `$XDG_RUNTIME_DIR/omt.sock` (fallback `/tmp/omt-<uid>.sock`), one `--mode rpc` child per connection, `hello` cwd frame and `daemon-stop` frame.
</answer>

<next_steps>
Open `/home/whrho/git/oh-my-tau/crates/omp-cli/src/slash.rs:125` for dispatch order and `:853` for the ported-handler table; `src/tools.rs::builtin_tools` for the exact tool schemas; `src/rpc.rs:595` onward for the RPC method bodies. Note the deliberate gaps if you plan to extend: `acp`/`rpc-ui` modes, `--profile`, `--from-claude/--from-codex`, and the `ratchet`/`prewalk`/`security`/multi-root-dir slash subsystems are parsed or reserved but not implemented.
</next_steps>
</results>