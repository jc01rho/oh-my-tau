<analysis>
**Literal Request**: Compare `/tmp/oh-my-pi-ro/packages/mnemopi` + `/tmp/oh-my-pi-ro/docs/memory.md` + `/tmp/oh-my-pi-omp2/crates/memory` against `/home/whrho/git/oh-my-tau` (memory/context files/rules) and detail the exact gap.
**Actual Need**: A precise, evidence-backed inventory of what the omt Rust port lacks in the memory subsystem and the context-file/rule discovery subsystem, keyed to files and line ranges, so someone can scope a porting plan without re-reading three codebases.
**Success Looks Like**: Every reference surface named with the omt path that (a) fakes it, (b) declares but ignores it, or (c) does not exist at all - plus what does exist so nothing is re-built.
</analysis>

<results>
<files>
Reference (read-only inputs):
- /tmp/oh-my-pi-ro/packages/mnemopi - TS memory engine: 67 files, 20970 lines src, 81 tests, `Mnemopi` facade + `BeamMemory` + MCP tools + fastembed/ONNX.
- /tmp/oh-my-pi-ro/docs/memory.md - 189 lines: 5-backend model (`off|local|hindsight|mnemopi|sharpshooter`), `memory://` URLs, `/memory` subcommands, local pipeline config table.
- /tmp/oh-my-pi-ro/docs/context-files.md - 328 lines: discovery provider registry, priorities, shadowing, `@` imports, `RULES.md` sticky rules, `contextFiles.extra`, `disabledProviders`/`disabledExtensions`.
- /tmp/oh-my-pi-ro/docs/mnemosyne-memory-backend.md - 247 lines: mnemopi settings table, scoping, recall previews, ranking weights, shutdown durability.
- /tmp/oh-my-pi-omp2/crates/memory - Rust rewrite: 6428 lines, `bank/cache/config/diagnose/extract/link/recall/remote/retain/runtime/session/store` + `embedding/{protocol,supervisor,worker}` + `bin/omp-memory-embedding-worker`.
- /tmp/oh-my-pi-omp2/crates/tools/src/memory.rs (597), `memory_edit.rs` (289), `learn.rs` (307)
- /tmp/oh-my-pi-omp2/crates/envd/src/memory.rs (173), `tool_url/memory.rs` (229) - `memory://` resolver
- /tmp/oh-my-pi-ro/packages/coding-agent/src/{memories,memory-backend,mnemopi,hindsight,autolearn,sharpshooter} - 10880 lines of wrapper/backends.

omt (target) - what exists:
- /home/whrho/git/oh-my-tau/crates/omp-settings/src/schema.rs - declares the whole memory key surface (74 ids) as dead config.
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/overlays/settings_defs.rs - 30 memory-tab UI rows (lines 1849-2129+), `contextFiles.extra` row at 1559, "Rules (TTSR)" group at 1840.
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/overlays/settings.rs - lines 734-736 wire the `hindsightActive` / `mnemopiActive` / `autolearnActive` row conditions to `memory.backend` / `autolearn.enabled`.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/slash.rs - lines 757-776: `/memory` stub returning canned strings.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/builtin_commands.json - lines 702-780: advertises `view|stats|diagnose|queue|sync|clear|reset|enqueue|rebuild|mm list|show|refresh|history|seed|delete|reload`.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tools.rs - lines 200-355 builtin registry (no memory tools); 395-420 `hidden_tools` (think/yield/goal only).
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/sysprompt.rs - context-file discovery + prompt assembly.
- /home/whrho/git/oh-my-tau/crates/omp-cli/prompts/project-prompt.md - `<repo-rules>` / `<dir-context>` / `<workspace-tree>` template.
- /home/whrho/git/oh-my-tau/crates/omp-urls/src/schemes/ - scheme providers, no `memory.rs`; `rule.rs` returns an empty rulebook.
- /home/whrho/git/oh-my-tau/crates/omp-settings/src/settings.rs - lines 308-330: `disabled_extensions` / `disabled_providers` / `enabled_providers` accessors with zero callers.
- /home/whrho/git/oh-my-tau/crates/omp-compact/src/summary.rs - line 85 `extra_context` field, never populated by the CLI.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/context.rs + `context_policy.rs` - CLM `context_edit` slots (the only working "memory").
</files>

<answer>
**1. There is no memory crate at all.** `crates/` has 20 members (auth-broker, cli, collab, compact, eval, ext, goose, judge, lsp, mcp, providers, sandbox, session, settings, sot, stats, task, tui, urls, wasm). omp2's `crates/memory` (6428 lines, 14 modules, own embedding worker binary, `SCHEMA_VERSION = 3`, SQLite tables `working_memory`/`episodic_memory`/`facts`/`extraction_jobs`/`bank_scope`/`index_generations`, four-voice RRF recall with weights Vector .35 / Graph .25 / Episodic .25 / Working .15) has no counterpart. The TS side (`packages/mnemopi`, 20970 lines, 81 tests) plus its coding-agent wrapper (`memories/`, `memory-backend/`, `mnemopi/`, `hindsight/`, `autolearn/`, `sharpshooter/` - 10880 lines) is likewise absent. No rusqlite or fastembed dependency exists in any crate that would host it (rusqlite appears only in auth-broker/cli/providers/stats/tui for unrelated stores; the only ONNX path is optional `laya-ort` in omp-judge).

**2. Settings are declared but nothing reads them.** `omp-settings/src/schema.rs` carries 74 memory-domain ids. Diffing against the TS registry (79 ids) the port is missing exactly five: `hindsight.globalScope`, `mnemopi.embeddingInputType`, `mnemopi.embeddingQueryPrefix`, `mnemopi.recallRelevance`, `sharpshooter.minPromptLength`. Everything else - `memory.backend` (5 values), 23 `mnemopi.*`, 26 `hindsight.*`, 16 `memories.*`, 3 `sharpshooter.*`, 3 `autolearn.*`, `contextFiles.extra`, `contextPromotion.enabled` - is schema-only: grepping for consumers outside `schema.rs`/`settings_defs.rs` yields three hits total (`overlays/settings.rs:734-736` conditions, `omp-urls/src/schemes/cfg.rs:416-419` credential redaction, `slash.rs:770` message text). 44 of the 74 have no settings-UI row either. So a user can select `memory.backend: mnemopi` in the TUI and no code path observes it.

**3. `/memory` is a stub.** `slash.rs:757-776` returns literal strings: "Memory payload is empty.", "Memory cleared.", "Memory consolidation enqueued.", "Memory consolidation ran.", and "Memory is not available for the off backend (/{verb})." for `queue|stats|diagnose`. There is no backend dispatch, no state, no per-backend branch. `builtin_commands.json` still advertises the full tree including all seven `mm` mental-model verbs (hindsight-only upstream). Also absent: the legacy `memories.enabled: true -> memory.backend: local` migration.

**4. No memory tools.** `tools.rs:200-355` registers read/bash/edit/glob/grep/write/find/task/wait/eval/ast_grep/ast_edit/ask/todo/web_search/lsp/checkpoint/rewind. `recall`, `retain`, `reflect`, `memory_edit`, `learn` do not exist. They survive only as string literals in `tools/task.rs:29-46`'s `READ_ONLY_TOOLS` list (TS parity table), which means the subagent read-only classifier reasons about four phantom tools. `hidden_tools` knows only think/yield/goal.

**5. No `memory://` scheme.** `crates/omp-urls/src/schemes/` contains agent, artifact, cfg, core, history, local, mcp, omp, rule, skill, xd - no `memory.rs`; `schemes/mod.rs:22-28` registers exactly those. So `read memory://root`, `memory://root/MEMORY.md`, `memory://root/learned.md`, `memory://<memory-id>` all fail. omp2 implements it in `envd/src/tool_url/memory.rs` with `MAX_RECORDS = 100` / `MAX_BYTES = 1 MiB` bounds. Related: `rule://` is registered but `RuleProvider::active_rules()` (`schemes/rule.rs:111-115`) returns `Vec::new()` with the comment that omp-rs has no rulebook, so every `rule://<name>` read errors "Unknown rule". The system prompt's `# Internal URLs` section renders `internalUrls: []` (`sysprompt.rs:987`), so the model is never told any scheme exists.

**6. Context-file discovery is a fraction of upstream.** `sysprompt.rs:153-192` implements only standalone `AGENTS.md`/`CLAUDE.md` walk-up: one file per directory depth, hidden dirs skipped, depth-descending sort. Missing versus `docs/context-files.md`:
- native `.omp/AGENTS.md` (user agent dir + nearest non-empty project `.omp/`, priority 100, shadows everything) - not implemented; `agent_dir()` is consulted only for `config.yml`, `SYSTEM.md`, `skills/`.
- user-scope context file `~/.omp/agent/AGENTS.md` - never read; there is no user-scope slot at all.
- `RULES.md` sticky rules (user + project, always-apply, re-discovered on `/clear`/`/new`) - zero implementation; `sysprompt.rs:990-991` hardcodes `"rules": []` and `"alwaysApplyRules": []`.
- `GEMINI.md`, `.github/copilot-instructions.md`, `.claude/CLAUDE.md`, `.codex/AGENTS.md`, `~/.config/opencode/AGENTS.md`, `.agent/`+`.agents/`, `.cursor/rules/*.mdc`, `.windsurf/rules`, `.clinerules` - none.
- provider priority/shadowing/dedup model (native 100 > claude 80 > agents/codex 70 > gemini 60 > opencode 55 > cursor/windsurf 50 > cline 40 > github 30 > vscode 20 > agents-md/claude-md 10 > custom-context 9 > builtin-defaults 1) - replaced by a hardcoded two-name loop.
- `contextFiles.extra` extra basenames - key and UI row exist (`schema.rs:698`, `settings_defs.rs:1559`) but no code reads them; inert.
- `enabledProviders` / `disabledProviders` / `disabledExtensions` gating - accessors exist (`omp-settings/src/settings.rs:308-330`) with zero callers in omp-cli/omp-tui.
- `@path` import expansion (relative-to-importing-file, `~/`, code-fence skipping, 5-hop cap, cycle skip, trailing-punctuation trim) - not implemented anywhere in `sysprompt.rs`.
- paragraph-containment dedup across depths - not implemented (only "one file per depth").
- `<dir-context>` deeper-file pointers - template block exists (`prompts/project-prompt.md:18-24`) but the data is hardcoded `"agentsMdSearch": { "files": [] }` (`sysprompt.rs:985`), so it never renders.
- `<workspace-tree>` - `"workspaceTree": {"rootPath": cwd, "rendered": "", "truncated": false}` (`sysprompt.rs:986`) with `includeWorkspaceTree: true` set later in the same builder, so the block is enabled but always empty.
- subagent context filtering (upstream drops `AGENTS.md` for task subagents but keeps `RULES.md`) - `opts.subagent` is threaded into the template but no filtering logic exists.

**7. The memory pipelines themselves are absent end to end.** Local backend (`memories/index.ts` 1609 lines + `storage.ts` 613 + 9 prompt files under `prompts/memories/`): no stage-1 per-session extraction, no phase-2 consolidation, no `MEMORY.md` / `memory_summary.md` / `learned.md` / generated `skills/`, no lease+heartbeat, no secret redaction, no Memory Guidance injection, no `memories.*` tuning consumers. Hindsight (backend 451 + client 723 + state 759 + mental-models 450 + outbox 389 + bank/content/config/transcript ~1000): nothing - no HTTP client, no durable outbox, no quarantine, no mental models. Sharpshooter (9 files): nothing. Autolearn: 3 setting keys and one UI row, no `learn` tool, no managed-skill authority. Compaction hook: `omp-compact/src/summary.rs:85` has `extra_context` for the upstream `preCompactionContext`, but `omp-cli/src/compact.rs:659-668` constructs `SummaryParams` without ever assigning it, so recalled memory can never enter a compaction. Session entry `memory_recall`: `omp-session/src/entries.rs` handles generic `Custom`/`CustomMessage`, so the record would round-trip, but there is no producer, no first-turn replay, and no drift note.

**8. Documentation.** `docs/` in omt contains only `brand/`, `screenshots/`, `wasm-plugins.md`. No `memory.md`, `context-files.md`, `mnemosyne-memory-backend.md`, or `system-prompt-customization.md`. `README.md` never mentions memory. So there is not even a spec to port against inside the repo.

**9. What does work, and should not be confused with memory.** `crates/omp-cli/src/context.rs` (1403 lines) + `context_policy.rs` (531) implement CLM: the model maintains working memory in named slots via the typed `context_edit` tool, with `append`/`clm`/`clm-host` strategies, a `<context-slots>` marker for the Anthropic cache breakpoint, and `always|rule|laya|schedule` host policies. That is omt's own context-management answer and is unrelated to the omp memory subsystem - do not count it as coverage.
</answer>

<next_steps>
Minimum viable port, in dependency order: (1) add a `crates/omp-memory` modelled on `/tmp/oh-my-pi-omp2/crates/memory` (rusqlite + flume + optional fastembed; durable rows authoritative, indexes rebuildable) rather than the 21k-line TS engine; (2) make `memory.backend` actually resolve and give `/memory` a backend trait with real `view|stats|diagnose|clear|enqueue` (replace `slash.rs:757-776`); (3) register a `memory://` provider in `crates/omp-urls/src/schemes/` and populate `internalUrls` in `sysprompt.rs:987`; (4) add `recall|retain|reflect|memory_edit` (+ gated `learn`) to `tools.rs` so `tools/task.rs:29-46` stops naming phantoms; (5) set `SummaryParams.extra_context` in `compact.rs:659`; (6) separately, replace `context_files()` with a provider registry to get native `.omp/AGENTS.md`, user-scope files, `RULES.md` always-apply, `@` imports, and a consumed `contextFiles.extra` / `disabledProviders` - and fill `agentsMdSearch.files` + `workspaceTree.rendered` so `<dir-context>` and `<workspace-tree>` stop rendering empty. Add the 5 missing setting ids while touching the schema.
</next_steps>
</results>