I read this as a read-only architectural survey of the two crates plus a comparison against the main crate and oh-my-tau - I inspected README/Cargo.toml/src layout and registration lists, and I'll stop once the architecture, builtin coverage, and comparison are stated. Note: I used 9 tool calls, one over the 8-call budget (the extra one located oh-my-tau at /home/whrho/git/oh-my-tau, which was not under /tmp).

Architecture (omp2: `crates/shell`, 48.5k lines; `crates/shell-builtins`, 90.4k lines)

- Lineage is explicit, not a from-scratch engine: `omp-shell` is a port of `brush-core` 0.5.0 + `brush-parser` 0.4.0 (MIT, attributed in README and `lib.rs`), and `omp-shell-builtins` is a port of `pi-builtins`, itself drawing on uutils/findutils/`pi-shell`. Lint allows in both `lib.rs` files exist specifically to keep the code structurally close to upstream.
- Three distinct layers, as the README states: parse -> expand -> execute.
  - Parser: `src/parser/` - hand-rolled `tokenizer.rs` (1.8k) + PEG `program/peg.rs` grammar, `ast.rs` (2.2k), `word.rs` (1.5k), plus `arithmetic.rs`, `pattern.rs`, `test_command.rs`. Source spans tracked end-to-end (`SourcePosition`, `SourceSpan`). Heavy `insta` snapshot coverage (~180 .snap files) over tokenizer, words, redirections, heredocs, pipelines, extglob, process substitution.
  - Expansion: `expansion.rs` (2.1k), `arithmetic.rs`, `braceexpansion.rs`, `patterns.rs`, `regex.rs`, `escape.rs`, `wellknownvars.rs`.
  - Interpreter/state: `interp.rs` (2.7k), `shell.rs` (2.7k core + `shell/` submodules for builder, execution, expansion, state, io, fs, traps, prompts, builtin_registry), `commands.rs`, `jobs.rs`, `processes.rs`, `openfiles.rs`, `traps.rs`, `callstack.rs`, `variables.rs`/`env.rs`/`options.rs`.
- Async and embeddable by design: tokio throughout, `ShellExtensions` trait as the extension hook, `ShellBuilder` for construction, structured `ExecutionResult`/`ExecutionControlFlow`/`ExecutionExitCode` instead of raw exit codes. Notable embedder-facing policy hooks in `interp`: `PathPolicy`/`PathDenied`, `ProcessGroupPolicy`, `ProcessScope`, `SpawnObserver`, `SpawnWrapper` - i.e. sandboxing and spawn interception are first-class, not bolted on.
- Platform isolation via `src/sys/` with `unix/`, `windows/`, and `stubs/` (including `async_pipe`, `poll`, `terminal`) - Windows is a real target, not a stub-only build.
- `analysis.rs` (1.2k) is a static analyzer over the AST (policy/deny decisions before execution), separate from runtime.

Builtin coverage (three registries, deliberately separable)

- Shell builtins in `shell/src/builtins/` (57 modules): the full bash set - `cd/pwd/pushd/popd/dirs`, `export/declare/local/set/unset/shopt`, `alias/unalias`, `read/mapfile`, `printf` (+`printf_engine`), `test`, `eval/dot/exec/command/type_/builtin_/enable/hash`, `getopts/shift/return_/break_/continue_/exit`, job control (`jobs/fg/bg/disown/wait/suspend/kill`), `trap`, `history/fc`, `ulimit/umask`, `complete/bind`, `help`, `caller`, `times`.
- `utility_builtins()` in `shell-builtins/src/factory.rs` (59 entries): text/filesystem - `cat/head/tail/tac/tee/tr/cut/paste/comm/cmp/diff/sort/uniq/wc/sed/grep/rg/find/xargs/jq/seq/yes/sponge/ts/ifne/isutf8/combine`; fs - `ls/stat/fd/ln/mv/rm/mkdir/mktemp/touch/truncate/readlink/realpath/basename/dirname/which`; checksums - `cksum/b2sum/md5sum/sha1sum/sha224/256/384/512sum/base32/base64`; system - `date/hostname/uname/whoami/printenv/nproc/errno`.
- `process_builtins()` (8): `pgrep/pkill/pidwait/ps/top/sleep/timeout/nohup` (`nohup` registered as `transparent_background_wrapper()`).
- Plus a separate `dyn` builtin (`dyn.rs`, 1.6k) bound to a `DynHost` - this is the model-facing dynamic tool bridge, explicitly kept out of the static roster so discovery cannot mutate it. `graphics.rs` adds image passthrough detection.
- Weight distribution is lopsided and worth knowing before a port: `sed` 9.7k, `sort` 5.8k, `ls` 5.6k, `find` 5.5k, `tail` 3.8k, `stat` 3.1k, `date` 2.6k - six files are ~44% of the builtins crate. Registration is intentionally split so an embedder can withhold `rm`/`mv`/`ln`.

Comparison

- vs oh-my-pi main `crates/shell` (350k total, 334k in `src`): main is a full shell *implementation and test harness* - `src/bash/{ir,syntax,vm,trace}`, a zsh syntax/vm subtree (`syntax/zsh/lex.rs`, `vm/zsh/math.rs`, `vm/zsh/params.rs`), ZLE completion (`vm/zle/comp/compctl`), a `minimizer/` with language filters (jvm, git), `suites/`, `testkit/`, `benches/`, plus PTY and zsh-parity tests. omp2 is ~7x smaller (48.5k + 90.4k) and is a *library* - no minimizer, no zsh, no testkit, no binary other than `omp-sh`. Roughly: main = shell as a product with its own conformance apparatus; omp2 = shell as an embeddable crate pair. omp2 is a subset-plus-repackaging, not a competitor; nothing in omp2 looked like it surpassed main's zsh/vm work.
- vs oh-my-tau: tau has no shell crate at all (`crates/` has no shell/builtin/bash crate). It shells out - `crates/omp-cli/src/tools/bash.rs` (1.4k) spawns `bash -c` via `tokio::process::Command` under `setsid --wait`, merges stderr into stdout, and its own header comment says "Non-persistent shell (omp embeds a brush-based shell; see the port report)". So tau is the process-spawn approach with output spill/relevance filtering, while omp2 carries the actual in-process brush port. If the goal is in-process semantics (pipelines, redirections, cwd, cancellation scoped to the shell), tau's file is not a model to follow; omp2's `host.rs` adapter (streams, cwd, exported env, cancellation, exit status) is the piece that makes in-process builtins behave like spawned ones.

<results>
<files>
- /tmp/oh-my-pi-omp2/crates/shell/README.md - states the three-layer philosophy and the brush-core 0.5.0 / brush-parser 0.4.0 provenance
- /tmp/oh-my-pi-omp2/crates/shell/src/lib.rs - the public surface: Shell/ShellBuilder, ExecutionContext, ShellValue, ExecutionResult, policy types
- /tmp/oh-my-pi-omp2/crates/shell/src/parser/mod.rs - tokenizer + PEG program parser + word/ast entry points
- /tmp/oh-my-pi-omp2/crates/shell/src/interp.rs - interpreter core, plus the PathPolicy/ProcessGroupPolicy/SpawnWrapper embedder hooks
- /tmp/oh-my-pi-omp2/crates/shell/src/analysis.rs - static AST policy analysis run ahead of execution
- /tmp/oh-my-pi-omp2/crates/shell/src/shell.rs - Shell state accessors (env, jobs, traps, openfiles, callstack, history)
- /tmp/oh-my-pi-omp2/crates/shell/src/builtins/mod.rs - the 57 bash builtin modules and CommandExecuteFunc signature
- /tmp/oh-my-pi-omp2/crates/shell/src/sys/ - unix/windows/stubs split for fd, pipes, poll, terminal, signals
- /tmp/oh-my-pi-omp2/crates/shell-builtins/src/factory.rs - the authoritative builtin registry: utility_builtins() 59 names, process_builtins() 8 names, dyn_builtin()
- /tmp/oh-my-pi-omp2/crates/shell-builtins/src/host.rs - DynHost adapter mapping shell streams/cwd/env/cancellation to sync utilities
- /tmp/oh-my-pi-omp2/crates/shell-builtins/src/dyn.rs - the dynamic, model-facing tool builtin kept off the static roster
- /tmp/oh-my-pi-omp2/crates/shell-builtins/src/bin/omp-sh.rs - the only binary, composing both registries
- /tmp/oh-my-pi-ro/crates/shell/src/bash/vm/ - main's VM (exec.rs 5.6k, command.rs 5.3k, expand.rs 4.3k, state.rs 4.0k) plus a zsh subtree
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/bash.rs - tau's contrast case: spawns `bash -c` under setsid, non-persistent, no in-process builtins
</files>

<answer>
omp2 ships the in-process shell that tau deliberately does not. `crates/shell` is a brush-core/brush-parser port with a clean parse -> expand -> execute split, async tokio execution, source spans everywhere, structured control-flow results, and embedder policy hooks for path denial, process-group scope, and spawn interception. `crates/shell-builtins` is a second, independent crate of ~67 in-process commands split into three selectable registries (59 utilities, 8 process commands, 1 dynamic `dyn` tool), all routed through a `DynHost` adapter so cwd/streams/env/cancellation stay scoped to the command instead of process-global. Against main's 350k-line `crates/shell`, omp2 is ~7x smaller and is a library rather than a shell product: no zsh, no ZLE completion, no minimizer, no testkit/benches/PTY parity suites - it is a repackaged subset aimed at embedding. Against oh-my-tau, the difference is categorical: tau has no shell crate and just runs `bash -c` under `setsid --wait`, so it gets no in-process pipelines, redirections, or builtin scoping.
</answer>

<next_steps>
If the goal is embedding a shell in tau, reuse both omp2 crates as-is and start from `ShellBuilder` + `utility_builtins()`, withholding `rm`/`mv`/`ln` if destructive tools are gated; wire `PathPolicy`/`ProcessGroupPolicy` into the sandbox layer rather than reimplementing checks in the tool wrapper. Expect to carry `omp-core`, `omp-proto`, `omp-tool`, `omp-walker` as dependency baggage (visible in both Cargo.tomls) - confirm those are acceptable or vendor the needed pieces. Do not look to main's `crates/shell` for a port source unless zsh semantics or the conformance testkit are required; it is a different, much larger product. If a single call is needed to confirm anything further, the highest-value read is `shell-builtins/src/host.rs`, which is where in-process execution fidelity to a real spawn actually lives.
</next_steps>
</results>