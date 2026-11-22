# Git memory repository tools (`memory`, `memory_apply_patch`)

These two tools give the OmOMeow gateway the same memory surface it gets from
the mhc engine. They are unrelated to the Mnemopi store behind `memory.backend`
(`recall`, `retain`, `reflect`, `memory_edit`).

## Binding

| Variable | Meaning | Default |
| --- | --- | --- |
| `MEMORY_DIR` | Root of a git work tree of `.md` blocks | tools are not advertised |
| `AGENT_ID` | Commit identity: author name `<id>`, email `<id>@omo.local` | `omt` |

`memory.backend` does not gate them. A `MEMORY_DIR` that is not a git repository
logs a warning and leaves the tools out.

## Contract

- `memory` takes `command` (`create`, `str_replace`, `insert`, `delete`,
  `rename`, `update_description`) and a required `reason`; `memory_apply_patch`
  takes `reason` and a codex-style `input` (`*** Begin Patch` ... `*** End Patch`).
- Paths are repository-relative (`system/human.md`; `.md` is added when omitted)
  or absolute inside the repository. `..`, symlink escapes, `.git`, `~/` and
  foreign absolute paths are rejected.
- A block is a `.md` file with YAML frontmatter. `description` is required (one
  line, at most 1024 characters). `read_only: "true"` (or `true`) blocks every
  modification, including delete and rename. Edits keep unknown header keys such
  as `expires`.
- One call is one commit with `reason` as the message. Success returns
  `Memory <command> committed locally (<sha7>).`, or `... committed (<sha7>);
  harness will sync after the turn.` when a git remote exists;
  `memory_apply_patch` returns `memory_apply_patch committed ...`.
- A write is refused while the work tree has uncommitted changes. A call that
  fails after touching the tree (including a rejecting git hook) restores it.
- `memory_apply_patch` validates every operation in memory first and writes
  nothing unless all of them pass; a move onto an existing block is refused.
- Writes are serialised by an advisory lock in the git directory.
