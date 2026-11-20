# Shell Execution Parity (Phase 1)

Status legend: `PRESENT` / `PRESENT-GAP` (exists, behavior differs) / `MISSING` (not
constructible on the target).

Sources verified this pass:

| Side | Path | Size / count |
|---|---|---|
| Target execution path | `crates/omp-cli/src/tools/bash.rs` | 1364 lines (incl. tests) |
| Target job plumbing | `crates/omp-cli/src/tools/jobs.rs` | 448 lines |
| Reference engine | `/tmp/oh-my-pi-omp2/crates/shell` (`README.md`) | brush-core 0.5.0 + brush-parser 0.4.0 port |
| Reference commands | `/tmp/oh-my-pi-omp2/crates/shell-builtins` (`README.md`) | 57 builtins + 59 utilities + 8 process builtins + `dyn` |
| Prior survey | `docs/research/12-omp2-shell-and-builtins.md` | 51 lines |
| Related parity | `docs/parity/tools.md` §4 `bash`, §7 dangling | schema deltas |

**Bottom line: the target has no shell. It spawns `bash -c` per call.** Every capability
below the process boundary — parsing, expansion, pipelines, redirections, builtins, job
control, cwd/env scoping — is delegated to whatever `/bin/bash` is installed. The
reference owns all of it in-process.

---

## 1. Execution model

| Dimension | Target (`crates/omp-cli/src/tools/bash.rs`) | Reference (`crates/shell`) | Status |
|---|---|---|---|
| Evaluation | `tokio::process::Command` → `bash -c <script>` (`run`, `detached_bash`, `local_bash_argv`) | `Shell`/`ShellBuilder` interpreting a parsed AST in-process | **MISSING** (in-process engine) |
| Persistence | None. State dies with the child; each call is a fresh shell | `Shell` holds env, variables, options, jobs, traps, callstack, history across calls | **MISSING** |
| Session isolation | `setsid --wait bash` so the child is its own session leader, no controlling tty (`detached_bash`, test `commands_have_no_controlling_terminal`) | `ProcessGroupPolicy` / `ProcessScope` hooks inside the interpreter | **PRESENT-GAP** (process-level only; no policy object) |
| Parser / AST | None. Bytes go to `bash` | `parser/`: hand-rolled tokenizer + PEG `program/peg.rs`, `ast.rs`, `word.rs`, source spans | **MISSING** |
| Expansion | Delegated | `expansion.rs`, `braceexpansion.rs`, `arithmetic.rs`, `patterns.rs`, `regex.rs` | **MISSING** |
| Static analysis | None — command string is never inspected before spawn | `analysis.rs` (1.2k): AST policy/deny pass before execution | **MISSING** |
| Env / cwd | `current_dir(cwd)` + inherited process env; `stdin` null, `stderr` null | Explicit shell state; exported env per command via `host.rs` | **PRESENT-GAP** |
| Cancellation | `AbortSignal` → `child.kill()`; `kill_on_drop(true)` | `ExecutionControlFlow` + cancellation plumbed through `DynHost` | **PRESENT-GAP** |
| Sandboxing | `features::sandbox::wrap_inner` re-execs through `omt __sandbox-exec` (Landlock/seccomp) | `PathPolicy`/`PathDenied`, `SpawnWrapper`, `SpawnObserver` in `interp` | **PRESENT-GAP** (OS-level only) |

### What the target actually does per call

1. Build script `exec 2>&1\n{command}` and spawn `setsid --wait bash -c <script>` (or `ssh
   <target> -- bash -lc` under `/ssh`, or `omt __sandbox-exec` under sandbox).
2. Read stdout in 8 KiB chunks inside a `tokio::select!` over read / deadline / abort.
   Retains head+tail windows (`RETAIN_BYTES = 8 * DEFAULT_MAX_BYTES * 2`), dropping the middle.
3. Bound the result: relevance (BM25) view when a query exists, else `sink_window` middle
   elision, then `enforce_inline_byte_cap`; anything cut is saved as `artifact://<id>`.
4. Format omp text: body (`(no output)` when empty), `Wall time: N.NN seconds`, optional
   `[Command timed out after N seconds]` / `[Command aborted]`, and
   `Command exited with code N` on non-zero.
5. Non-zero exit becomes a `bail!` (error result); success attaches `details`
   (`wallTimeMs`, `timeoutSeconds` or `timeoutDisabled`, `requestedTimeoutSeconds`).

Stderr is merged by the shell, not by the tool: `stderr(Stdio::null())` is safe only because
the script begins with `exec 2>&1`.

---

## 2. Builtins and utilities

The reference splits its command surface into three independently selectable registries in
`crates/shell-builtins/src/factory.rs`; none of them have a target counterpart.

| Registry | Reference contents | Target | Status |
|---|---|---|---|
| Shell builtins (57 modules, `crates/shell/src/builtins/`) | `cd/pwd/pushd/popd/dirs`, `export/declare/local/set/unset/shopt`, `alias/unalias`, `read/mapfile`, `printf` (+`printf_engine`), `test`, `eval/dot/exec/command/type_/builtin_/enable/hash`, `getopts/shift/return_/break_/continue_/exit`, `jobs/fg/bg/disown/wait/suspend/kill`, `trap`, `history/fc`, `ulimit/umask`, `complete/bind`, `help`, `caller`, `times` | supplied by the host `bash` binary, unknown version/patch level | **MISSING** (no owned builtins) |
| `utility_builtins()` — 59 names | text: `cat/head/tail/tac/tee/tr/cut/paste/comm/cmp/diff/sort/uniq/wc/sed/grep/rg/find/xargs/jq/seq/yes/sponge/ts/ifne/isutf8/combine`; fs: `ls/stat/fd/ln/mv/rm/mkdir/mktemp/touch/truncate/readlink/realpath/basename/dirname/which`; checksums: `cksum/b2sum/md5sum/sha1sum/sha224sum/sha256sum/sha384sum/sha512sum/base32/base64`; system: `date/hostname/uname/whoami/printenv/nproc/errno` | supplied by coreutils/uutils on PATH | **MISSING** (unowned, unversioned, unselectable) |
| `process_builtins()` — 8 names | `pgrep/pkill/pidwait/ps/top/sleep/timeout/nohup` (`nohup` = `transparent_background_wrapper()`) | supplied by procps/util-linux on PATH | **MISSING** |
| `dyn` builtin | `shell-builtins/src/dyn.rs` (1.6k), bound to `DynHost`, deliberately kept off the static roster so discovery cannot mutate it | none | **MISSING** |

Consequences that matter beyond porting aesthetics:

- The target cannot withhold destructive commands. The reference registers `rm`/`mv`/`ln` as
  optional entries by design ("embedders may choose different registration policies,
  including withholding destructive utilities", `shell-builtins/README.md`); the target has
  no registry to gate, only the coarse sandbox wrapper.
- The target's behavior is host-dependent: `grep -P`, `sed -i`, `stat -c` vs `-f`, `printf`
  `%b` handling, and `sha*sum` availability all vary by distro image. Nothing pins them.
- `crates/omp-cli/Cargo.toml` has no shell dependency of any kind (no `brush-*`, no
  `uu_*`, no `nix`-based builtin table) — confirmed against the dependency list.

---

## 3. Command surface the tool exposes

Schema (from `crates/omp-cli/src/tools.rs:278` and `bash()`):

| Arg | Target behavior | Notes |
|---|---|---|
| `command` | required; passed verbatim to `bash -c` after `exec 2>&1` | no parse, no validation |
| `timeout` | `f64`, clamped `1..=3600`, default `300`; `0` disables | `timeout_arg`; `details.requestedTimeoutSeconds` records overrides |
| `cwd` | `resolve_to_cwd`, defaults to session cwd | |
| `async` | registers `bg_N` job in `JobRegistry`, returns `Backgrounded as job bg_N (deadline Ns)`, output read via `proc://<id>` | `start_background` |
| `name` + `ready` | service mode: piped spawn, log-regex and/or TCP probe readiness, default 30 s, `127.0.0.1` | `start_service`, 48-char `^[A-Za-z0-9][A-Za-z0-9._-]{0,47}$` |
| `pty` | **accepted in schema, ignored in code** | `bash.rs` has no `pty` branch; grep for `"pty"` finds schema only |
| `notify` | regex over command output; matching lines are collected under the same per-batch bounds omp `OutputNotifier` uses (`NOTIFY_MAX_LINES = 50`, `NOTIFY_MAX_LINE_CHARS = 400`, blank lines never counted, overflow only counted) and reported with the finished result; a bad pattern fails the call | `Notify` in `bash.rs`; see the one-batch note below |

`pty` is the one live inconsistency: description text says "`pty` only interactive" and the
schema advertises it, while `run`/`start_service` always pipe stdout and null stdin.

`notify` is now **PRESENT-GAP** rather than absent: the Phase 1 PR compiles the caller's
regex, matches it against whole lines (an unterminated trailing line is carried between
reads, bounded by `NOTIFY_MAX_PARTIAL_BYTES = 1600` so a huge newline-free line cannot grow
the buffer or force a rescan, and a line longer than the cap is judged once from its prefix
and clamped to 400 chars). The remaining difference is delivery cadence: omp emits a
follow-up every 200 ms with a refilling 30-batch allowance, while the target has no
follow-up channel on the job surface and therefore emits a single bounded report when the
command settles — the timer and the allowance collapse into that one report, so a long
running command's matches arrive only at completion.

---

## 4. Port gaps by layer

Ordered cheapest-first if in-process semantics are wanted.

| Layer | Reference piece | Target substitute | Gap |
|---|---|---|---|
| 1 | `ShellBuilder` + `utility_builtins()` | none — PATH | **MISSING** |
| 2 | `host.rs` `DynHost` adapter (streams, cwd, exported env, cancellation, exit status) | none | **MISSING**; this is the piece that makes in-process builtins behave like spawned ones |
| 3 | `parser` (tokenizer + PEG, source spans, ~180 `insta` snapshots) | none | **MISSING** |
| 4 | `expansion` / `arithmetic` / `braceexpansion` / `patterns` / `regex` | none | **MISSING** |
| 5 | `interp.rs` + `shell/` (execution, state, io, fs, traps, prompts, builtin registry) | none | **MISSING** |
| 6 | `analysis.rs` static AST policy pass | none | **MISSING**; no pre-execution deny capability |
| 7 | `PathPolicy` / `ProcessGroupPolicy` / `SpawnWrapper` / `SpawnObserver` | `features::sandbox::wrap_inner` + `setsid` | **PRESENT-GAP** |
| 8 | `sys/` (`unix`, `windows`, `stubs`: `async_pipe`, `poll`, `terminal`) | none | **MISSING** (no Windows shell path at all) |
| 9 | structured `ExecutionResult` / `ExecutionControlFlow` / `ExecutionExitCode` | `Option<i32>` + `Option<&'static str>` outcome | **PRESENT-GAP** |

Dependency baggage if either reference crate is adopted: both Cargo trees pull `omp-core`,
`omp-proto`, `omp-tool`, `omp-walker` (per `docs/research/12` §next_steps). Adoption is not a
drop-in — those crates must be vendored or accepted.

---

## 5. Target-side strengths (do not regress on a port)

These exist only in the target and would need explicit reimplementation:

- Output bounding: BM25 relevance view (`tools/relevance.rs`), middle elision
  (`sink_window`), inline byte cap, `artifact://` spill with head/tail byte budgets.
- Streaming tap (`Arc<Mutex<String>>`) so `proc://<id>` shows live output, with its own
  retention window.
- Service supervision: readiness by log regex **and** TCP probe (both must hold), 64 KiB
  readiness buffer, 100 ms supervisor tick, kill on registry drop / `kill_requested`.
- Remote execution (`/ssh <host>` → `ssh <target> -- bash -lc`) and sandbox re-exec.
- Session cleanliness: `setsid` guarantees no controlling terminal, tested explicitly.

A brush-based port would replace §1–§4 while these five must be re-attached at the new
boundary.

---

## 6. Corrections to earlier notes

`docs/parity/tools.md` §7 flags `crates/omp-cli/src/tools/bash.rs:3` as wrong for calling the
reference shell "brush-based", reasoning that the reference is `crates/shell` (bash 5.3 with a
zsh dialect). Both are true but about different references:

- **`oh-my-pi` main** `crates/shell` (350k lines) is the bash+zsh product; it is *not*
  brush-based.
- **omp2** `crates/shell` **is** a `brush-core` 0.5.0 / `brush-parser` 0.4.0 port, stated in
  its own `README.md` and echoed by `docs/research/12`.

The `bash.rs` header comment ("omp embeds a brush-based shell; see the port report") is
therefore accurate for omp2. `tools.md` §7 should read as a mis-scoped correction rather than
a code defect. Of the `bash`-relevant deltas in `tools.md` §4, `notify` is no longer open:
`bash.rs` implements it in the Phase 1 PR, so the tools.md row should be re-read as
"implemented, single report instead of 200 ms follow-up batches" rather than "absent". The
six schema variants and `name` regex vs `maxLength:48` still stand unchanged and are out of
scope here.

---

## 7. Files to create or extend

| Work | File |
|---|---|
| Implement or drop `pty` | `crates/omp-cli/src/tools/bash.rs` (`run`, `start_service`, schema in `tools.rs:278`) |
| In-process engine (if adopted) | new `crates/omp-shell`, `crates/omp-shell-builtins` vendored from omp2 |
| `PathPolicy`/`ProcessGroupPolicy` wiring | `crates/omp-sandbox` + `features/sandbox.rs` |
| Static AST policy pass | new, feeding `analysis.rs`-equivalent before `run` |
| Fix §6 claim | `docs/parity/tools.md` §7 |
