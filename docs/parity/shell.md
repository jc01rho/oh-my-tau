# Shell Execution Parity (Phase 2)

Status legend: `PRESENT` / `PRESENT-GAP` (exists, behavior differs) / `MISSING` (not
constructible on the target).

Sources verified this pass:

| Side | Path | Note |
|---|---|---|
| Integration layer | `crates/omp-cli/src/tools/shell_host.rs` | `CliPathPolicy`, `CliSpawnWrapper`, `SpawnBook`, `split_utility_builtins`, `CliDynHost`, `analyze_script`, `build_embedded_shell`, `execute_shell` |
| Target tool surface | `crates/omp-cli/src/tools/bash.rs` | `run`, `start_background`, `start_service`, `local_bash_argv`, `detached_bash` |
| Target job plumbing | `crates/omp-cli/src/tools/jobs.rs` | `JobRegistry`, `bg_N`, `Service` |
| Host tool snapshot | `crates/omp-cli/src/tools.rs` | `Env::dyn_tools` (`:130`, `:159-161`), populated at `:437` |
| Remote runs | `crates/omp-cli/src/features/ssh.rs` | `remote_argv` / `remote_argv_for` |
| Sandbox | `crates/omp-sandbox/src/lib.rs` + `crates/omp-cli/src/features/sandbox.rs` | `wrap_inner`, Landlock/seccomp |
| Vendored engine | `vendor/omp-shell` | omp2 `crates/shell` |
| Vendored commands | `vendor/omp-shell-builtins` | omp2 `crates/shell-builtins` |
| Declared deps | `crates/omp-cli/Cargo.toml:52-53` (`omp-shell`, `omp-shell-builtins`), `:83` (`nix`, unix only), `:88` (`omp-sandbox`) | |
| Prior survey | `docs/research/12-omp2-shell-and-builtins.md` | 57 builtins + 59 utilities + 8 process |
| Related parity | `docs/parity/tools.md` §4 `bash`, §7 | schema deltas |

**Bottom line: local finite `bash` execution is wired in-process — this is `PARITY`.**
`crates/omp-cli/src/tools/bash.rs::run_with_dyn` (`:356-383`) calls
`super::shell_host::execute_shell` with a `ShellExecRequest`, so `run`, `bash()` and
`execute()` all reach the embedded `omp_shell::Shell` rather than spawning `bash`. `bash()`
threads `env.dyn_tools()` into both the foreground call (`:752`) and `start_background`
(`:741`, `:1040`, `:1078-1086`), so the `dyn` bridge is live on the tool path. `CliPathPolicy`
gained a `/dev/tty` denial on top of the workspace write root, closing the controlling-terminal
regression in-process (`shell_host.rs:184-186`, `:196-215`). `omp-shell`,
`omp-shell-builtins` and `nix` are declared in `crates/omp-cli/Cargo.toml`, and
`Env::dyn_tools` feeds the registry snapshot from `tools.rs:437`.

Two paths stay external **by design, not by omission**: `/ssh` commands keep
external `ssh` (`run_with_dyn` branches to `run_remote_ssh` at `:366-368`), and long-lived
services (`name`/`ready`) keep their own external `Child` supervisor for the `SUPERVISOR_TICK`
daemon lifecycle (`start_service`, `:828-...`), because both need to own a process table the
in-process interpreter cannot. `detached_bash`/`local_bash_argv` survive only for that
service path. See §1 and §9.

---

## 1. Execution model

| Dimension | Current target | Vendored / integration layer | Status |
|---|---|---|---|
| Local evaluation | **In-process.** `run_with_dyn` → `shell_host::execute_shell` → `Shell::run_string` (`bash.rs:356-383`, `shell_host.rs:848-1021`). No local `bash` spawn | `execute_shell` + `Shell` | **PARITY** |
| Persistence | `Shell` instance per call (fresh interpreter, no carry-over) | `Shell` holds env, variables, options, jobs, traps, callstack, history; `build_embedded_shell` pins `profile(Skip)`, `rc(Skip)`, `working_dir(cwd)`, `do_not_inherit_env(true)`, `external_cmd_leads_session(true)` (`shell_host.rs:758-809`) | **PRESENT** (per-call, matching omp's non-persistent `bash`) |
| Session isolation | `CliPathPolicy` denies `/dev/tty` on read, write, and `open` (`shell_host.rs:184-186`, `:196-215`); external children get `external_cmd_leads_session(true)` → own `setsid` session | same | **PARITY** — pinned by `commands_have_no_controlling_terminal` (in-process `NO_TTY`, and the external child's `sid == pid`) |
| Parser / AST | `vendor/omp-shell/src/parser/` via the interpreter | `Parser::new(.., ParserOptions::default())` in `analyze_script` (`shell_host.rs:739-752`) | **PRESENT** |
| Expansion | `expansion.rs`, `braceexpansion.rs`, `arithmetic.rs`, `patterns.rs`, `regex.rs`, `escape.rs` | same | **PRESENT** |
| Static analysis | `analyze_script` → `omp_shell::analysis::analyze` → `ScriptIr` + `omp_proto::…::BashIr::from(&ir)` (`shell_host.rs:739-752`); called on every `execute_shell` run for IR telemetry (`commands`, `is_read_only`, `writes_outside`, `opaque_count`) and exercised by unit tests | same | **PRESENT** (runs on live path for IR inspection; pre-exec deny policy can layer on top) |
| Env / cwd | `do_not_inherit_env(true)` then `std::env::vars_os()` re-exported as `ShellVariable` (`shell_host.rs:788-804`), but filtered to entries whose key **and** value are UTF-8 (`to_str()`), so non-UTF-8 pairs are dropped rather than re-exported; inherited `PWD`/`OLDPWD` are skipped and `PWD` is re-set from `cwd`; stdin is `openfiles::null()` | same | **PARITY** (env re-export is UTF-8-only) |
| Cancellation | `AbortSignal` → `ExecutionParameters::set_cancel_token` + `SpawnBook::signal_groups(SIGKILL)` (`killpg`) on timeout/abort (`shell_host.rs:887`, `:984`, `:991`); pinned by `cancel_aborts_spawned_child_via_pgid` | same | **PARITY** |
| Sandboxing | `CliPathPolicy` (in-process writes limited to `cwd` plus `/dev/null`) + `CliSpawnWrapper` (external binaries still re-exec through `wrap_inner` → `omt __sandbox-exec`) | `PathPolicy` / `SpawnWrapper`; the external Landlock policy (`features/sandbox.rs`) additionally permits `/tmp` and the agent directory | **PRESENT-GAP** — in-process writes are narrower than the external Landlock policy (no `/tmp`, no agent dir), so sandboxing is not full parity; pinned by `path_policy_enforces_workspace_write_root` and `sandbox_path_policy_blocks_in_process_redirection_outside_root` |

### `execute_shell` shape (`shell_host.rs:848-1021`)

1. `build_embedded_shell(cwd, dyn_tools, include_fs_mutations = true)`.
2. `io::pipe()`; the write end is duplicated onto stderr so both streams share one reader;
   stdin is `omp_shell::openfiles::null()`.
3. `ExecutionParameters` (`shell_host.rs:875-887`) gets `set_fd` for 0/1/2, plus
   `set_path_policy(CliPathPolicy::new(cwd, features::sandbox::enabled()))`,
   `set_spawn_wrapper(CliSpawnWrapper::from_sandbox(cwd))`,
   `set_spawn_observer`/`set_process_scope` (the same `SpawnBook`), and `set_cancel_token`.
4. A `spawn_blocking` reader drains the pipe in 8 KiB chunks into an unbounded channel.
5. One `tokio::select!` races the shell future, chunks, the deadline, and the abort signal;
   timeout or abort sets `outcome`, cancels the token, and `killpg`s every tracked group.
6. Output is retained with a head+tail window (`retain_bytes`, halved on overflow), decoded
   with a UTF-8 carry so split sequences never become U+FFFD, and optionally pushed to an
   `on_chunk` callback and the `Arc<Mutex<String>>` tap used for `proc://`.
7. Returns `ShellExecResult { output, exit_code: Option<i32>, elapsed, outcome }` where
   `outcome` is `Some("timeout")` or `Some("aborted")` and clears `exit_code`.

### What `bash.rs` does per call

1. `run` (`:345-354`) forwards to `run_with_dyn` with `dyn_tools: None`; `bash()` calls
   `run_with_dyn` directly with `env.dyn_tools()` (`:752`). `run_with_dyn` (`:356-383`) first
   builds `script = format!("exec 2>&1\n{command}")` and asks
   `features::ssh::remote_argv(&script)` for a target. When `/ssh` is active, that `script`
   string — the stderr-merge wrapper itself, not merely a probe — is POSIX-quoted and passed to
   `run_remote_ssh` as `ssh <target> -- bash -lc <script>` (`:385-...`, external `ssh`). When no
   target is set, `remote_argv` yields `None`, the wrapped `script` is discarded, and the
   **unwrapped** `command` is what goes into the `ShellExecRequest { cwd, command, timeout,
   signal, dyn_tools, retain_bytes: RETAIN_BYTES, on_chunk, tap }` awaited on `execute_shell`
   (which merges stderr itself by duplicating the pipe write end onto fd 2).
2. `on_chunk` is `notify`-driven: `Box<dyn FnMut(&str) + Send>` feeding `Notify::feed`, so
   `notify` regexes now match on live chunks from the interpreter instead of a reader loop.
3. Bound the result: BM25 relevance view (`tools/relevance.rs`), else `sink_window` middle
   elision, then `enforce_inline_byte_cap`; overflow spills to `artifact://<id>`.
4. Format omp text: body (`(no output)` when empty), `Wall time`, optional timeout/abort
   markers, `Command exited with code N` on non-zero; non-zero exit `bail!`s and success
   attaches `details` (`wallTimeMs`, `timeoutSeconds` or `timeoutDisabled`,
   `requestedTimeoutSeconds`).

`execute` (`:494-515`, the rpc `bash` surface) calls the same `run` with
`DEFAULT_TIMEOUT_SECS` and maps the tuple onto `Executed`, setting `cancelled` from
`outcome == Some("aborted")` and `timed_out` from `Some("timeout")`.

Remote execution stays deliberately external: `/ssh` keeps `ssh <target> -- bash -lc`
(`features/ssh.rs::remote_argv`), because no in-process interpreter can own a remote host's
filesystem or process table. `run_remote_ssh` retains the pre-port reader loop (8 KiB chunks,
`tokio::select!` over read/deadline/abort, head+tail retention, `kill_on_drop(true)`).

`start_service` also keeps its external supervisor by design: it spawns a piped `Child`
(`bash.rs:828-...`, via `wrap_inner`/`detached_bash` + `local_bash_argv`), keeps the `Child`
handle in the supervisor task, drains stdout into a 64 KiB readiness buffer plus the service
log, probes readiness (log regex and/or TCP) and liveness on `SUPERVISOR_TICK = 100 ms`
(`:812`), and kills on registry drop or `kill_requested`. That lifecycle is tied to a real
`Child` and a daemon that outlives the call, so it is not an `execute_shell` candidate —
which is why `detached_bash`/`local_bash_argv`/`setsid_path` still exist.

`start_background` (`bash.rs:1040-...`) registers a `bg_N` job, then spawns a task that calls
`run_with_dyn` with the `env.dyn_tools()` snapshot it was handed (`:1078-1086`), passing the
job's `Arc<Mutex<String>>` as `tap` so `proc://<id>` streams live in-process output.

---

## 2. Builtins and utilities (vendored, now registered)

`vendor/omp-shell-builtins/src/factory.rs` exposes three independently selectable registries
plus the `dyn` bridge; `build_embedded_shell` installs all three.

| Registry | Vendored contents | Wiring | Status |
|---|---|---|---|
| Shell builtins (57 modules, 60 registered names in `vendor/omp-shell/src/builtins/`) | `cd/pwd/pushd/popd/dirs`, `export/declare/typeset/local/readonly/set/unset/shopt`, `alias/unalias`, `read/mapfile/readarray`, `printf` (+`printf_engine`), `test`/`[`, `eval/dot/source/exec/command/type_/builtin_/enable/hash`, `getopts/shift/return_/break_/continue_/exit`, `jobs/fg/bg/disown/wait/suspend/kill`, `trap`, `history/fc`, `ulimit/umask`, `complete/compgen/compopt/bind`, `help`, `caller`, `times`, `echo/colon/true_/false_/let_` | `builtins::default_builtins()` (`shell_host.rs:770`) | **PRESENT** |
| `utility_builtins()` — 59 names | text: `cat/head/tail/tac/tee/tr/cut/paste/comm/cmp/diff/sort/uniq/wc/sed/grep/rg/find/xargs/jq/seq/yes/sponge/ts/ifne/isutf8/combine`; fs: `ls/stat/fd/ln/mv/rm/mkdir/mktemp/touch/truncate/readlink/realpath/basename/dirname/which`; checksums: `cksum/b2sum/md5sum/sha1sum/sha224sum/sha256sum/sha384sum/sha512sum/base32/base64`; system: `date/hostname/uname/whoami/printenv/nproc/errno` (`errno` is `#[cfg(unix)]`) | `split_utility_builtins()` → safe tier always installed (`:771`); `rm`/`mv`/`ln` installed only when `include_fs_mutations` is true (`:777-783`, `execute_shell` passes `true`) | **PRESENT**, with `rm`/`mv`/`ln` behind their own gate |
| `process_builtins()` — 8 names | `pgrep/pkill/pidwait/ps/top/sleep/timeout/nohup`; `nohup` is `transparent_background_wrapper()` so `$!` is the real child PID | installed at `:772-776` | **PRESENT** |
| `dyn_builtin(host)` | `list` / `schema` / `call` against `Arc<dyn DynHost>`; registration is fixed so discovery never mutates the model-facing roster | `builder.builtin("dyn", dyn_builtin(host))` when `Env::dyn_tools()` is non-empty (`:784-786`) | **PRESENT** |

`factory.rs` pushes 67 entries total: 59 utilities + 8 process builtins.

`split_utility_builtins()` (`shell_host.rs:68-79`) is the destructive-command seam: it walks
`utility_builtins()` and returns `(safe, fs_mutation)` with exactly `rm`, `mv`, `ln` in the
second vec, so mutation builtins can be audited, gated, or registered as an independent tier
without touching the rest. The unit test `utility_split_separates_fs_mutations` pins the
three names and asserts `grep` stays safe.

Registry separation remains deliberate:

- `utility_builtins` shadows real system binaries; withholding `rm`/`mv`/`ln` is now
  expressible through `include_fs_mutations` instead of being left to the host image.
- `process_builtins` is a separate tier because process control is a different policy
  decision from text utilities.
- `dyn` stays off the static roster so tool discovery cannot mutate it.

---

## 3. `CliDynHost`: the `dyn` bridge

`CliDynHost` (`shell_host.rs:615-706`) implements `omp_shell_builtins::DynHost` over
`DynToolList = Arc<Vec<Weak<dyn AgentTool>>>`, the snapshot `Env` holds (`tools.rs:130`) and
fills at `tools.rs:437` from the registered tool list — a snapshot of weak handles, not a
reference, so `FnTool` setup creates no cycle.

- `visible_tools()` filters out `bash` **and** `dyn`, so a script cannot recurse into the
  shell or re-enter the bridge.
- `list` maps each tool to `DynDevice { name, description }`.
- `schema` resolves one tool and returns its `parameters()` as the JSON schema, faulting with
  `unknown dyn device: <name>` otherwise.
- `call` builds an `AbortController`, spawns a task that aborts it when the `CancellationToken`
  fires, executes the tool, and converts the result: a single text part that parses as a JSON
  object/array becomes `DynOutput::Json`, other single text becomes `DynOutput::Text`, and
  mixed content becomes `DynOutput::Parts` (images summarized as `[image <mime>, N base64
  chars]`).

`execute_shell_runs_pipes_heredoc_and_dyn_host` exercises `dyn`, `dyn echo_device --msg hi`, and `dyn echo_device -j '{"msg":"hi"}'` (`pong:hi`), and `bash_tool_exposes_registered_tools_via_dyn_bridge` exercises the live `AgentTool` bridge without recursion.

---

## 4. Command surface the tool exposes

Schema (from `crates/omp-cli/src/tools.rs` `bash()`):

| Arg | Target behavior | Notes |
|---|---|---|
| `command` | required; local finite and background commands run **in-process** through `execute_shell` (`omp-shell` interpreter) — only `/ssh` commands and `start_service` daemon supervision are external | parsed by `analyze_script`/`omp-shell` on the in-process path |
| `timeout` | `f64`, clamped `1..=3600`, default `300`; `0` disables | `timeout_arg`; `details.requestedTimeoutSeconds` records overrides |
| `cwd` | `resolve_to_cwd`, defaults to session cwd | also the `Shell` working dir and the `CliPathPolicy` write root |
| `async` | registers `bg_N` in `JobRegistry`, returns `Backgrounded as job bg_N (deadline Ns)`, output via `proc://<id>` | `start_background` → `run` |
| `name` + `ready` | service mode: piped external `Child`, log-regex and/or TCP probe readiness, default 30 s, `127.0.0.1` | `start_service`, 48-char `^[A-Za-z0-9][A-Za-z0-9._-]{0,47}$` |
| `pty` | **accepted in schema, ignored in code** | `bash.rs` has no `pty` branch; `execute_shell` also always pipes |
| `notify` | regex over output; matching lines under `NOTIFY_MAX_LINES = 50`, `NOTIFY_MAX_LINE_CHARS = 400`; bad pattern fails the call | `Notify` in `bash.rs`; `execute_shell`'s `on_chunk` is the matching hook |

`pty` is the one live inconsistency: the description advertises it while every path pipes
stdout and nulls stdin.

`notify` remains **PRESENT-GAP** on cadence: matches are judged per whole line with an
unterminated trailing line carried between reads (`NOTIFY_MAX_PARTIAL_BYTES = 1600`), while
omp emits a follow-up every 200 ms; the target emits one bounded report when the command
settles.

---

## 5. Cancellation and process groups

`SpawnBook` (`shell_host.rs:526-605`) is the target-side bookkeeping type and is wired into
every `execute_shell` run:

- `impl SpawnObserver::on_spawn(pid, pgid)` records each non-zero pid and each distinct
  positive pgid.
- `impl ProcessScope::{may_signal, may_observe}` reports true only for tracked pids/groups,
  so the interpreter's `kill` builtin cannot signal outside the tracked set.
- `signal_groups(signal)` (`#[cfg(unix)]`) calls `nix::sys::signal::killpg` for every tracked
  group, then `nix::sys::signal::kill` for any ungrouped pid. `execute_shell` calls it with
  `SIGKILL` after `cancel_token.cancel()` on timeout or abort. `cancel_aborts_spawned_child_via_pgid`
  asserts `sleep 30` returns `outcome == Some("aborted")` well under 5 s.
- Non-unix builds have no `signal_groups` at all (`#[cfg(unix)]`), so the killpg teardown is
  skipped there; `nix` is a unix-only dependency (`crates/omp-cli/Cargo.toml:81-83`).

Two other mechanisms exist and must not be confused with it:

- **External supervisor teardown**: `crates/omp-task/src/runner/control.rs` spawns with
  `process_group(0)`, captures the pgid before `child.wait()`, and escalates via
  `kill(-pgid, SIGKILL)`.
- **Service supervision**: `bash.rs::start_service` kills its own `Child` on registry drop or
  `kill_requested` inside the `SUPERVISOR_TICK` loop.

Earlier revisions of this doc claimed no `SpawnBook` existed in the tree. That was true before
Phase 2 and is now wrong: `SpawnBook` lives in `crates/omp-cli/src/tools/shell_host.rs` and is
the canonical `-pgid` teardown path for in-process shell runs.

---

## 6. Shims

The omp2 shell stack does not compile against published crates, so three stable shims stand in
for its private dependencies. All three are pure stable Rust, no nightly.

| Shim | Path | Role |
|---|---|---|
| `xutf` | `vendor/xutf` | pure stable-Rust stand-in for the xutf APIs the shell stack consumes |
| `omp-tool` | `vendor/omp-tool` | harness diagnostics (`Diag`) attached to omp tool calls |
| `omp-proto` | `vendor/omp-proto` | hand-written wire shapes; supplies the `BashIr` type `analyze_script` projects `ScriptIr` into (`shell_host.rs:739-752`), reaching the graph as a path dependency |

Provenance is pinned in `vendor-src.lock`; `omp-shell` and `omp-shell-builtins` are workspace
members consumed by `crates/omp-cli` as `omp-shell.workspace = true` / `omp-shell-builtins.workspace = true`
(`Cargo.toml:52-53`).

---

## 7. `CliPathPolicy`: write boundaries

`CliPathPolicy` (`shell_host.rs:136-215`) is the `PathPolicy` impl handed to every run:

- `normalize_for_policy` canonicalizes the path, or its deepest existing ancestor with the
  remaining tail re-appended, so `..` and symlink escapes cannot slip past the prefix test.
- Construction takes `(write_root, sandbox_enabled)`; `execute_shell` passes
  `features::sandbox::enabled()`, so with `/sandbox` off the **workspace write** restriction is
  lifted (writes are then allowed anywhere) — but `/dev/tty` is still rejected in
  `check_read`, `check_write`, and `open`, so the policy is never fully unrestricted.
- `check_read` denies `/dev/tty` and otherwise allows; `check_write` denies `/dev/tty`
  first, allows `/dev/null` unconditionally, and otherwise requires the normalized target to
  stay under `write_root`. `open` rejects `/dev/tty` for every access mode before applying the
  same write rule.
- `open` re-applies the same rule per `OpenRequest` access mode (`Read`, `CreateNew`,
  `Truncate`, `Append`, `ReadWrite`, `Write`) and honors `create_mode` on unix.
- `path_policy_enforces_workspace_write_root` pins `ok.txt` allowed, `/dev/null` allowed,
  `outside.txt` denied, and `../outside.txt` denied.

`CliSpawnWrapper` (`shell_host.rs:489-521`) adapts `features::sandbox::wrap_inner(write_root, &[])`
to `SpawnWrapper::launcher()`, so external binaries run from the in-process shell still
re-exec through `omt __sandbox-exec` with Landlock/seccomp; `env_allowed` admits every key.
Activation is conditional: `wrap_inner` returns a launcher only when `/sandbox` is enabled
**and** the build targets Linux **and** the kernel reports Landlock available
(`KernelStatus::Available`). Otherwise it returns `None`, `from_sandbox` stores no launcher,
`launcher()` yields `None`, and those external binaries run unsandboxed.

---

## 8. Target-side strengths (do not regress)

These are target-only and now sit on top of the in-process path:

- Output bounding: BM25 relevance view (`tools/relevance.rs`), middle elision
  (`sink_window`), inline byte cap, `artifact://` spill with head/tail byte budgets. This is
  `bash.rs` formatting applied to `ShellExecResult.output`, not shell state, so it survived the
  switch unchanged.
- Streaming tap so `proc://<id>` shows live output: `run_with_dyn` passes the job's
  `Arc<Mutex<String>>` as `tap`; `shell_host` trims it with `util::floor_boundary`, and
  `start_background` hands the same tap to `run_with_dyn` (`:1078-1086`).
- `notify` now rides the `on_chunk` callback instead of a reader loop, so matches still arrive
  as chunks land.
- Service supervision: readiness by log regex **and** TCP probe, 64 KiB readiness buffer,
  100 ms tick, kill on registry drop. Stays on the external `Child` path.
- Remote execution (`/ssh` → `ssh <target> -- bash -lc`) stays external.
- Session cleanliness: `CliPathPolicy` denies `/dev/tty` in-process and
  `external_cmd_leads_session(true)` gives external children their own session;
  `commands_have_no_controlling_terminal` pins both halves.

---

## 9. Remaining work

| # | Piece | Where | Remaining gap |
|---|---|---|---|
| 1 | Enforce pre-exec deny policy on `analyze_script` | `crates/omp-cli/src/tools/shell_host.rs::execute_shell` | `analyze_script` runs on every `execute_shell` call and records `ScriptIr`/`BashIr` telemetry, but does not yet reject commands pre-execution |
| 2 | Decide `pty` | `bash.rs` + schema in `tools.rs` | implement or remove from the description; `execute_shell` pipes stdout and nulls stdin today |
| 3 | `SUPERVISOR_TICK` daemon lifecycle | `bash.rs::start_service` | intentionally external; keep the `Child` supervisor and do **not** route it through `execute_shell` |
| 4 | `§8` claim in `docs/parity/tools.md` §7 | docs | the "brush-based" note is accurate for omp2, which is what is vendored — clarify, do not delete |

Done, and no longer work items: local `run`/`execute`/`start_background` wired to
`execute_shell`; registries installed (`default`/`utility`/`process`/`dyn`); `env.dyn_tools()`
threaded to both foreground and background calls; `/dev/tty` denied by `CliPathPolicy`;
`PathPolicy`/`SpawnWrapper`/`SpawnObserver`/`ProcessScope` implemented;
`ExecutionParameters` fd/cancel plumbing; `-pgid` teardown; `rm`/`mv`/`ln` split; module
comment in `bash.rs:1-12` updated to describe the in-process local path.

Test coverage for this state: 38 `#[test]`/`#[tokio::test]` functions in `tools/bash.rs` and 24
in `tools/shell_host.rs` (all passing under `cargo test -p omp-cli --no-default-features --bin omt`). The named cases exist and pin the claims above:
`commands_have_no_controlling_terminal`, `in_process_builtins_grep_wc_and_jq_work_end_to_end`,
`bash_tool_exposes_registered_tools_via_dyn_bridge`, `in_process_cwd_parameter_isolates_shell_working_dir` (bash);
`sandbox_path_policy_blocks_in_process_redirection_outside_root`,
`builtin_registry_without_fs_mutations_omits_rm`,
`execute_shell_runs_pipes_heredoc_and_dyn_host`, `cancel_aborts_spawned_child_via_pgid`,
`analysis_detects_readonly_vs_outside_writes`, `path_policy_enforces_workspace_write_root`,
`utility_split_separates_fs_mutations` (shell_host).

Note on local builds with default `onnx` feature on glibc < 2.38: `ort-sys` references `__isoc23_strtol`; pass `--no-default-features` on older-glibc hosts (CI runs on Ubuntu 24.04 with glibc 2.39 and tests default features).
