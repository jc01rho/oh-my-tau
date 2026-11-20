<analysis>
**Literal Request**: Compare omp's Rust `vcs`/`iso` crates against tau's `omp-sandbox`/`omp-cli` on worktree, git, and isolation concerns; identify what omp solved in Rust that tau still shells out for or lacks.
**Actual Need**: A gap ledger that a caller can use to decide what to port into tau, with exact absolute paths per capability.
**Success Looks Like**: Named omp implementation per capability, named tau counterpart (or its absence), plus concrete porting targets.
</analysis>

<results>
<files>
- /tmp/oh-my-pi-ro/crates/vcs/src/lib.rs - `Repo` enum + `detect`/`detect_for_display`/`is_pure_jj`, `Feature` capability gating; the whole "which VCS owns this dir" answer in Rust
- /tmp/oh-my-pi-ro/crates/vcs/src/types.rs - `GitRepoInfo`, `LinkedWorktree`, `WorktreeEntry`, `WorktreeAddOptions` (`detach`/`clone`/`keep_changes`), `WorktreeClone`, `DiffOptions`, `StatusSummary`, `CommitDetails`
- /tmp/oh-my-pi-ro/crates/vcs/src/git/mod.rs - `GitRepo::discover` (pure FS walk, no subprocess), `linked_worktree`, `is_linked_worktree`, `prefix_of`, `discover_info`
- /tmp/oh-my-pi-ro/crates/vcs/src/git/mutate.rs - `worktree_add` (gitoxide-native registration + checkout + optional CoW clone + `keep_changes` seeding), `worktree_remove`, `worktree_prune`, `clone_holes`, `seed_worktree_changes`, `detach_git_dir` (alternates-based severing), `stage_files`, `unstage`, `commit_create`, `commit_tree`, `checkout`, `create_branch`, `delete_branch`, `checkout_new_branch`, `restore`, `stash_create`, `reset`, `clean`, `read_tree`, `write_tree`, `config_set`, `remote_add`
- /tmp/oh-my-pi-ro/crates/vcs/src/git/patch.rs - `apply_patch`, `can_apply_patch`, `stage_hunks` (partial staging), `cherry_pick`, `stash_push`, `stash_try_pop`, `join_patches`, `validate_hunk_selections`
- /tmp/oh-my-pi-ro/crates/vcs/src/git/read.rs - `head`, `head_sha`, `current_branch`, `default_branch`, `resolve_ref`, `short_id`, `ref_exists`, `tags_at`, `list_branches`, `is_dirty`, `operation_in_progress`, `operation_kind`, `status_porcelain`, `status_summary`, `config_has_local`, `config_get`, `remote_list`, `remote_url`, `worktrees`, `log_subjects`, `stash_subjects`, `log_onelines`, `rev_list_range`, `merge_base`, `rev_list_touching`, `commit_details`, `ls_files`, `ls_tree`, `submodule_paths`, `show_blob`, `lfs_media_dir`, `head_watch_target`
- /tmp/oh-my-pi-ro/crates/vcs/src/git/diff.rs - in-process `diff_text`/`changed_files`/`numstat`/`has_diff`/`diff_tree`/`diff_no_index`/`show_commit` in git patch dialect
- /tmp/oh-my-pi-ro/crates/vcs/src/git/cli.rs - the deliberately tiny, hardened subprocess escape hatch: `run_sync_write`, deadline + SIGTERM→SIGKILL, `COMMAND_TIMEOUT`, `push`/`fetch`/`clone` (credential-bound only)
- /tmp/oh-my-pi-ro/crates/vcs/src/git/filter.rs - filter/attribute handling (present only in the newer ro tree; absent in omp2)
- /tmp/oh-my-pi-ro/crates/vcs/src/jj/mod.rs, /tmp/oh-my-pi-ro/crates/vcs/src/jj/ops.rs - `JjWorkspace` on jj-lib, no subprocess: `working_copy_label`, `head_id`, `status_summary`, `status_porcelain`, `diff_text`, `changed_files`, `numstat`, `log_subjects`, `log_onelines`, `commit_details`, `ls_files`
- /tmp/oh-my-pi-ro/crates/vcs/tests/worktree_clone_carry.rs - the worktree clone/change-carry test the worktree behavior is pinned by
- /tmp/oh-my-pi-ro/crates/iso/src/lib.rs - `BackendKind` (Apfs/Btrfs/Zfs/LinuxReflink/Overlayfs/WindowsBlockClone/Projfs/Rcopy), `IsolationBackend` trait (`probe`/`start`/`clone_tree`/`stop`/`diff`), `resolve`/`clone_candidates`/`auto_order`
- /tmp/oh-my-pi-ro/crates/iso/src/rcopy.rs - cross-platform fallback: `git worktree add --detach` + `seed_dirty_state`, else recursive copy; teardown via `git worktree remove --force` + `rm -rf`
- /tmp/oh-my-pi-ro/crates/iso/src/cow.rs - `clone_file`/`clone_open`/`is_unsupported`/`is_cross_device` over FICLONE / clonefile(2) / FSCTL_DUPLICATE_EXTENTS_TO_FILE
- /tmp/oh-my-pi-ro/crates/iso/src/overlayfs.rs, btrfs.rs, zfs.rs, linux_reflink.rs, apfs.rs, projfs.rs, windows_block_clone.rs - per-filesystem isolation backends
- /tmp/oh-my-pi-ro/crates/iso/src/diff.rs - `Diff`/`FileChange`/`ChangeKind`; git-delegating default diff, tree-walk fallback
- /tmp/oh-my-pi-ro/crates/iso/src/tree.rs - shared tree-copy/canonicalize/destination-prep helpers
- /tmp/oh-my-pi-omp2/crates/vcs/README.md - the design statement: gitoxide primary, git binary only for credential-bound transfers + reftable, jj on jj-lib, discovery as pure FS walk, structured `Error` instead of stderr regexes
- /tmp/oh-my-pi-omp2/crates/vcs/src/git/mutate.rs - older/smaller `worktree_add(&self, path, ref_name, detach: bool)` (no clone, no keep_changes) and `detach_git_dir`
- /tmp/oh-my-pi-omp2/crates/app/src/worktree_cmd.rs - environment-owned worktree discovery/cleanup: `.omp-isolation-owner` markers, `IsolationOwner`/`DurableRecord`, orphan detection by pid, `worktree list/clear`, `prune_git_worktrees`
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/features/worktree.rs - tau's `/wt`: `sanitize_name`, `worktree_path`, `default_worktree_name`, `git()` helper shelling to `git -C`, `list_worktrees` parsing `git worktree list --porcelain`, `/wt new|list|remove|merge|run|status`
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/features/sandbox.rs - tau's `/sandbox` toggle + `wrap_inner` re-exec launcher; Linux-only, write-dirs + net-deny only
- /home/whrho/git/oh-my-tau/crates/omp-sandbox/src/lib.rs - Landlock V9 ruleset + seccomp AF_INET/AF_INET6 deny + `exec_confined`; `KernelStatus` probe
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/sysprompt.rs - tau's own `git worktree root` walk for the system prompt (duplicate discovery logic)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools/checkpoint.rs - `checkpoint`/`rewind` are conversation-context tools only, not filesystem/VCS snapshots
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/status_line.rs, status_line/segments.rs - `GitInfo` branch/staged/unstaged/untracked rendering, fed by shelled git
- /home/whrho/git/oh-my-tau/crates/omp-settings/src/schema.rs, omp-tui/src/overlays/settings_defs.rs - settings `worktree.base|cleanSource|clone|onExit|onStart`, `sandbox.enabled` (declared, largely unimplemented)
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/features/skills.rs - other `Command::new("git")` sites (clone/`git` checks)
</files>

<answer>
The gap is not "tau lacks sandboxing" — tau's `omp-sandbox` is real, working Landlock+seccomp and omp's `iso` crate is not a sandbox at all. The gap is everything *around* it: omp replaced its whole git/status/diff/worktree layer with an in-process Rust VCS, while tau still shells out for every git operation and has no isolation PAL, no CoW cloning, and no jj support.

**1. Git access: omp in-process, tau subprocess-only.**
- omp: `omp-vcs` is a single Rust `Repo` enum (`Git`/`Jj`) dispatching ~60 operations; git runs on gitoxide, discovery is a pure filesystem walk (`GitRepo::discover`, handles `.git` pointer files, `commondir`, reftable detection) cheap enough for synchronous render paths. The git binary survives only for credential-bound push/fetch/clone and reftable repos, behind a hardened `cli.rs` (stripped `GIT_DIR` family, non-interactive env, bounded capture, deadline + SIGTERM→SIGKILL). Errors are structured `Error` variants, never stderr regexes.
- tau: a single `git()` helper in `features/worktree.rs` runs `git -C <dir> …` and string-matches stderr; `sysprompt.rs` has its own separate `.git`-walk; status-line git state comes from shelled git; `skills.rs` has two more `Command::new("git")` sites. No gitoxide, no git2 anywhere in the workspace (`Cargo.toml`/`crates/*/Cargo.toml` have no `gix`/`git2`). Every read (branch, dirty counts, log, diff, status) is a fork+exec.

**2. Worktrees: omp has a native worktree engine, tau has `git worktree add` calls.**
- omp `worktree_add` does branch-existence and already-checked-out validation, registers the worktree admin dir, checks out the tree via gitoxide, and optionally *CoW-clones* the source checkout (`WorktreeClone::Off|Auto|Prefer(BackendKind)`) with `clone_holes` excluding the target and sibling worktrees. `keep_changes` carries staged+unstaged+untracked state into the new worktree by copying paths and installing a rebased source index (stat-cache refresh included) — the `/wt` "start where I am" behavior. `worktree_remove`/`worktree_prune` and `detach_git_dir` (severs a linked worktree from its parent via `objects/info/alternates`, no object copy) exist too.
- tau: `/wt new` is literally `git worktree add <path> -b omt/<name>`; `/wt list` parses `git worktree list --porcelain` line prefixes; `/wt remove` shells `git worktree remove [--force]`; `/wt merge` shells `git add -A` + `git commit` + `git merge` and re-derives conflicts from `git diff --diff-filter=U`. No clone, no change-carry, no detach, no prune, no worktree metadata beyond `git`'s own.
- Note the version delta: omp2's `worktree_add` is the older 3-arg `(path, ref_name, detach)`; the `ro` tree is the newer one with clone/keep_changes. The `ro` tree is the target to port.

**3. Isolation: omp has a cross-platform PAL, tau has none.**
- omp `omp-iso`: `IsolationBackend` trait with `probe`/`start`/`stop`/`clone_tree`/`diff`, eight backends (APFS clonefile, btrfs snapshot, ZFS clone, Linux FICLONE reflink, overlayfs with fuse fallback, Windows block clone, ProjFS, rcopy), a `resolve`/`clone_candidates`/`auto_order` preference resolver with fallback and `fell_back` reporting, `cow::clone_file`/`clone_open` as the shared primitive, and a `diff` that delegates to git for git trees and tree-walks `(size, mtime)` otherwise.
- tau: nothing. No `omp-iso` equivalent crate; no CoW/reflink/overlay/APFS/btrfs/ZFS code anywhere in `crates/`. Subagent isolation and worktree cloning settings (`worktree.clone`, `isolation.backend` in `settings_defs.rs`) are *declared in the TUI settings schema* but have no implementation behind them.

**4. Sandbox: tau is ahead of omp here — don't port, keep.**
`omp-sandbox` (Landlock ABI V9 ruleset, read/execute everywhere + write only in granted dirs, optional seccomp AF_INET/AF_INET6 → EACCES, `exec_confined` self-re-exec so the tool keeps the launcher's pid for kill_on_drop/timeout) has no counterpart in omp's `vcs`/`iso` crates. `iso` is filesystem isolation, not syscall confinement. Tau's gaps in this area are only scope: Linux-only, no macOS seatbelt, no read-path restriction, no per-tool policy — but the enforcement core is sound.

**5. jj: omp has it, tau has nothing.**
`JjWorkspace` on jj-lib (no subprocess) plus `detect`/`detect_for_display`/`is_pure_jj` colocated-workspace disambiguation (equal-root ties → Git for automation, Jj for display; deeper root wins). Tau has zero jj code.

**6. Checkpoint/rewind: homonyms, not equivalents.**
Tau's `checkpoint`/`rewind` are *conversation-context* tools (message-count truncation, report retention). omp's VCS layer offers `stash_create`/`stash_try_pop`/`restore`/`reset`/`clean`/`read_tree`/`write_tree` — the primitives a *filesystem* checkpoint/rollback would need. Tau has no filesystem snapshot/rollback at all (`.wsckpt` is excluded from the workspace as checkpoint *stores*, not a Rust crate).

**7. Worktree lifecycle hygiene: omp has an owner-marker registry, tau has none.**
`/tmp/oh-my-pi-omp2/crates/app/src/worktree_cmd.rs` implements `.omp-isolation-owner` marker files, `DurableRecord` (version/id/root/branch/owner_pid/class/source_root), pid-based orphan detection, `worktree list/clear --all --orphan --dry-run --json`, and `prune_git_worktrees`. Tau's `/wt remove` requires an exact name and has no orphan/GC path (`omp-cli/src/subcmd/gc.rs` exists but is unrelated to worktrees).
</answer>

<next_steps>
Port in this order, largest payoff first:

1. **Stand up an `omp-vcs`-equivalent crate in tau** (suggested `crates/omp-vcs`), copying `git/mod.rs` discovery + `git/read.rs` + `git/diff.rs` first — that alone replaces `repo_root`, `list_worktrees`, `change_summary`, the status-line git feed, and `sysprompt.rs`'s duplicate walk, and removes the `git()` stderr-string helper. Add `gix` to `[workspace.dependencies]`.
2. **Then `git/mutate.rs` worktree engine + `omp-iso`** together, because `worktree_add`'s clone path depends on `omp_iso::clone_candidates`. Port `iso/lib.rs` + `cow.rs` + `rcopy.rs` + `overlayfs.rs` + `linux_reflink.rs` + `btrfs.rs` as the Linux-first subset; defer apfs/projfs/windows_block_clone/zfs until those targets matter. This is what makes `/wt new` cheap and gives the declared-but-dead `worktree.clone` / isolation-backend settings something to bind to.
3. **Wire `keep_changes` into `/wt new`** and add `detach_git_dir` for `/wt remove` — the two behaviors tau's `/wt` most visibly lacks.
4. **Port the owner-marker worktree registry** from `app/src/worktree_cmd.rs` into `omp-cli/src/features/worktree.rs` (or a subcmd) so orphaned worktrees from killed sessions are discoverable and GC-able.
5. **Add jj** (`jj/mod.rs` + `jj/ops.rs` + `detect_for_display`) only if tau intends to support jj users; it is self-contained and additive.
6. **Do not port sandboxing from omp** — `omp-sandbox` is already the stronger implementation. If anything, extend it (macOS seatbelt, read-path denies, per-tool policy) rather than replacing it.

Caveat to verify before step 2: the `ro` and `omp2` trees disagree on `worktree_add`'s signature and on whether `git/filter.rs` exists; treat `/tmp/oh-my-pi-ro` as the newer source of truth (it has `tests/worktree_clone_carry.rs` and the clone/keep_changes machinery) and diff `git/diff.rs` before copying, since `ro` is 2309 lines vs omp2's 1556.
</next_steps>
</results>