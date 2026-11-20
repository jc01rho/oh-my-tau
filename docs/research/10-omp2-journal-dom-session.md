<analysis>
**Literal Request**: Inspect four crates (journal, dom, session, vocab) read-only — README, Cargo.toml, src filenames — and summarize the journal-first session model, `.oms` format, fold/projection, rewind/fork, and the contrast with oh-my-tau's JSONL-v3 `omp-session`.
**Actual Need**: An accurate architectural picture of the durable-session stack, enough to write or review docs/design text without opening the code again.
**Success Looks Like**: A compact, factual summary naming the real types/constants (Journal, Entry, Kind, BlobStore, Session::apply, rewind, diff/LifecycleWork, projection) plus the honest boundary of what "fork" means here.
</analysis>

Inspection done (note: I used 10 read-only calls, slightly over the 8 budget; no further exploration).

**Crates and shape**
- `omp-journal` (deps: omp-core, omp-ar, cap-std): src `lib.rs, sse.rs, entry.rs, kind.rs, chain.rs, data.rs, blob.rs, gc.rs, ulid.rs`. Durable history only — "intentionally structural rather than behavioral".
- `omp-dom` (deps: omp-journal for `EntryId` only, omp-vocab): `lib.rs, arena.rs, node.rs, op.rs, selector.rs, snapshot.rs, stream.rs, subscribe.rs, txn.rs`. Authoritative materialized tree.
- `omp-session` (deps: journal, dom, proto, tool, observability): `lib.rs, session.rs, fold.rs, projection.rs, rewind.rs, component.rs, custom_message.rs, exit_diagnostics.rs, late_diagnostics.rs, components/{con,directors,jobs,lifecycle,prompts,todo}.rs`.
- `omp-vocab` (deps: omp-core, serde, strum only): single `lib.rs` — closed `Tag`/`KnownTag`/`PropId`/`PropKey` vocabulary plus `for_each_prop!`/`for_each_component!` shared with tui/macros.

**Journal-first model**
- `Session` is the sole mutable owner of journal + DOM: every live write is preflighted, appended to the journal, then folded from the exact returned `Entry`. Journal is the single authority; DOM never becomes a second state model.
- One `Journal` = one `.oms` file, one writer: exclusive advisory lock on a stable sidecar (not the inode — GC renames) plus a shared directory-namespace lease; GC takes that lease exclusively.
- `Entry { id: EntryId(Ulid), kind: Kind{name, rev}, by, prior, label, data }`. `by` = causing entry (absent at genesis), `prior` = explicit branch parent (absent = previous file entry). `Kind` is `name@rev` with a closed rev-1 vocabulary: `journal@1, turn.start, msg.user, msg.assistant.start, stream, msg.assistant.end, tool.call, tool.update, tool.result, turn.receipt, patch, compaction`.

**`.oms` format**
- Flat file of raw Server-Sent Events frames; a blank line after a frame is the commit boundary. Canonical field order: comment, `event`, `id`, `by`, `prior`, `data`. `data` is single-line JSON, inline cap `DATA_HARD_CAP` = 1 MiB (`sse.rs`).
- Open truncates only bytes after the last commit point (`recovered_tail_bytes`), so a torn tail costs one frame, not the file.
- Large/binary payloads go to `BlobStore`: SHA-256 addressed at `<root>/blobs/<hh>/<hh>/<64hex>`, write to `tmp` + fsync + atomic rename, put-before-journal ordering (a crash can orphan a blob but never leave a dangling reference). GC (`gc.rs`) uses the same age+reachability plan for dry-run and apply, proving traversal bounds before CAS deletion; live chains, child jobs, checkpoints and imported sessions pin their blobs.

**Fold / projection**
- `Session::apply` (`fold.rs`) matches on `(kind.name, kind.rev)` to `fold_*` methods (genesis builds `<meta>` children Todo/Jobs/Directors/Con and `<queues>` Steering/Prompts), then asks each registered `Component` whose `interested(&kind)` is true to apply into a `Draft`; non-empty drafts become DOM ops. `head` advances to the entry id.
- DOM root is fixed `<session><meta/><body/><queues/></session>`; mutations are `Txn` atomic transactions validated against the sequential post-state via a touched-node scratch overlay; handles are monotonic, never reused. Opening replays every historical file-prefix op and `prior` jump, restoring handle and stream-id floors (`handle_floors`, `stream_floors`) so allocator history is reproduced exactly.
- `projection.rs` is a pure DOM→inference conversion (`project_thread`, `project_thread_history`, `project_thread_through`, `compaction_frames`, `file_mentions`), emitting protobuf thread items; the steering `<system-notice>` envelope is applied at projection time so journaled text stays raw and wire bytes are stable.

**Rewind / fork**
- `rewind(target)`: snapshot before, `rederive_chain` along `prior` links, restore handle/stream high-water marks, set `pending_prior = target` so the next append opens a divergent branch. History is never destroyed: `chain::live_chain` walks the tail-selected chain, `abandoned` returns the unreachable remainder (GC candidates).
- Lifecycle consequences are a DOM diff, not journal inspection: `rewind::diff(before, after) -> LifecycleWork { terminate, spawn, retained }`, keyed by durable identity (`id`/`cause` for `<subagent>`/`<job>`, `cause` for tool calls still `arguments`/`running`); reminted handles land in `retained`. Replicas converge via `reset` events from the snapshot-plus-event subscription, so no actor reads the journal.
- "Fork" is therefore implicit: there is no fork API in these four crates — a branch is any new entry whose `prior` points at a non-tip ancestor. Literal `fork` identifiers exist only outside scope (`crates/agent/src/loop.rs`, `crates/ai/*`).

**Contrast with oh-my-tau's `omp-session` (JSONL v3)** — inferred from this design; the tau tree was out of scope and not read.
- v3-style: append-only JSONL log, one record per line, session state == linear replay of the file; branching means rewriting/truncating lines or writing a new file.
- Here: SSE framing with a blank-line commit and torn-tail recovery; `prior` links make branches first-class inside one file so rewind never deletes history; blobs dedupe large payloads across sessions instead of inlining them in lines; and the DOM is a separately authoritative, transactionally mutated tree with snapshot + patch/stream/reset event streams, with typed projections feeding inference — the journal carries identity and ordering only, never state.

<results>
<files>
- /tmp/oh-my-pi-omp2/crates/journal/README.md - journal-first doctrine, `.oms` + CAS blobs, GC lease semantics
- /tmp/oh-my-pi-omp2/crates/journal/src/lib.rs - `Journal` single-writer, create/open, torn-tail truncation, `FILE_EXTENSION = "oms"`
- /tmp/oh-my-pi-omp2/crates/journal/src/entry.rs - `Entry`/`EntryDraft`/`EntryId` (ULID), `by`/`prior` fields
- /tmp/oh-my-pi-omp2/crates/journal/src/sse.rs - raw SSE framing, blank-line commit, `DATA_HARD_CAP` 1 MiB
- /tmp/oh-my-pi-omp2/crates/journal/src/kind.rs - closed `name@rev` vocabulary (`journal@1` … `compaction@1`)
- /tmp/oh-my-pi-omp2/crates/journal/src/chain.rs - `live_chain` / `abandoned` branch selection via `prior`
- /tmp/oh-my-pi-omp2/crates/journal/src/blob.rs - SHA-256 CAS layout, put-before-journal, atomic rename
- /tmp/oh-my-pi-omp2/crates/journal/src/data.rs - typed JSON payloads per kind (Genesis, Attachment, FileMentions, …)
- /tmp/oh-my-pi-omp2/crates/dom/README.md - `<session><meta/><body/><queues/></session>`, monotonic handles, txn overlay, snapshots/events
- /tmp/oh-my-pi-omp2/crates/dom/src/{txn,snapshot,op,stream,subscribe,arena,node,selector}.rs - transaction engine, deterministic snapshots, patch/stream-delta/reset events
- /tmp/oh-my-pi-omp2/crates/session/README.md - journal-to-DOM fold, one authority, rewind via `prior`
- /tmp/oh-my-pi-omp2/crates/session/src/session.rs - `Session` create/open/restore, `rewind`, `rederive_chain`, `subscribe`, handle/stream floors
- /tmp/oh-my-pi-omp2/crates/session/src/fold.rs - `apply` dispatch per kind + component `Draft` ops
- /tmp/oh-my-pi-omp2/crates/session/src/projection.rs - pure DOM→inference thread projection, steering envelope
- /tmp/oh-my-pi-omp2/crates/session/src/rewind.rs - `LifecycleWork`, `diff` by durable identity
- /tmp/oh-my-pi-omp2/crates/session/src/components/{lifecycle,jobs,todo,con,prompts,directors}.rs - journal-backed `<meta>` components
- /tmp/oh-my-pi-omp2/crates/vocab/src/lib.rs - closed `Tag`/`PropId` vocabulary and macro row sources
</files>

<answer>
The stack is journal-first and single-authority: one writer appends committed SSE entries to a flat `.oms` file (blank line = commit, torn tail truncated on open, large payloads in SHA-256 CAS blobs), and the DOM is a derived-but-authoritative materialized tree rebuilt by folding those entries. Branching is expressed purely by `prior` links, so rewind re-derives an ancestor chain while preserving handle/stream high-water marks and reports consequences as a snapshot diff (`LifecycleWork`), never by inspecting or rewriting history — a "fork" is just the next entry pointing at a non-tip ancestor. This differs from a JSONL-v3 session log where the file is a linear record stream and replay/truncation is the only branch mechanism.
</answer>

<next_steps>
Ready to proceed — use this as the factual basis for docs/ADR text. If you need the tau-side comparison verified rather than contrasted by inference, that requires reading the oh-my-tau tree, which was outside this task's scope.
</next_steps>
</results>