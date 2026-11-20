I read this as read-only fork-divergence audit - inventory oh-my-tau's distinctive features and classify each against oh-my-pi TS (`/tmp/oh-my-pi-ro`) and the oh-my-pi Rust rewrite (`/tmp/oh-my-pi-omp2`). I'll stop when every named feature has a verdict: upstream-inherited, tau/prime-agent-ported, or omt-only-and-must-be-preserved.

<analysis>
**Literal Request**: Check `/home/whrho/git/oh-my-tau` for fork-unique / tau-ported features (/ultra, magic keywords, omp-sot, omp-judge/laya-ort, omp-goose, web UI, slash commands) and verify whether they exist in oh-my-pi main or omp2.
**Actual Need**: A preservation list - which behaviours must survive a sync/rebase against upstream oh-my-pi (TS) and the omp2 Rust rewrite, and which come free from upstream.
**Success Looks Like**: Per-feature verdict with absolute paths and evidence, so a rebase can be scoped without re-investigating.
</analysis>

Upstream references used (no `vendor-src/oh-my-pi/` checkout exists in the repo - it is gitignored):
- `/tmp/oh-my-pi-ro` - can1357/oh-my-pi TypeScript, HEAD `b636e19d10`, 2026-11-20
- `/tmp/oh-my-pi-omp2` - can1357/oh-my-pi Rust rewrite, v0.1.0, HEAD `2f92f3b`, 2026-09-04

<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/slash_extra.rs - `/ultra code|deep|long|hyper|off` command: usage text (line 155), `UltraArg` parse (176), `ultra_command` (186), dispatch (395), command registration (370). omt-only.
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/magic.rs - magic keyword table (lines 35-66: ultrathink, orchestrate, workflowz, jevify, ultracode) plus the `Ultra` enum (363-436: Code/Deep/Long/Hyper -> ultracode/ultradeep/ultralong/hypercode, `rlm()`/`clm()` gates). `ultracode` keyword and `Ultra` are omt additions.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/notices.rs - keyword notice injection (`keyword_notice`, line 78) with `requires` gating (148).
- /home/whrho/git/oh-my-tau/crates/omp-cli/prompts/notices/ - ultrathink-notice.md, orchestrate-notice.md, workflow-notice.md, jevify-notice.md, ultracode-notice.md, ultradeep-addendum.md, ultralong-addendum.md.
- /home/whrho/git/oh-my-tau/crates/omp-sot/ - State of Thought controllers (arXiv 2609.16055): controller.rs, embed.rs, sot.rs, state.rs, segment.rs, examples/gsm8k_sot.rs. No upstream counterpart anywhere.
- /home/whrho/git/oh-my-tau/crates/omp-judge/ - Laya turn judge: episode.rs, view.rs (port of `judge_view.view_sel`), policy.rs, scorer.rs, shadow.rs, offline.rs, onnx.rs, policy_questions.json, questions.json.
- /home/whrho/git/oh-my-tau/vendor/laya-ort/ - ONNX Runtime inference for the Laya judge (`laya-judge` bin), replaces the Python `laya_predict` daemon. Part of the project per NOTICE.md.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/subcmd/judge_sessions.rs - `omt judge-sessions` offline scoring entry point; also stats_judge.rs.
- /home/whrho/git/oh-my-tau/crates/omp-goose/ - goose-language dedup kernel: goose/omt_dedup.goose, generated/omt_dedup.c/.h, build.rs (cc), regenerate.sh, src/lib.rs FFI wrapper.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/subcmd/web/ - `omt web` browser UI: mod.rs, agent.rs, sessions.rs, wallpapers.rs, assets.rs + assets/ (app.js, composer.js, dialogs.js, markdown.js, models.js, shell.js, tools.js, transcript.js, app.css, index.html, 6 wallpapers incl. anime-sakura/vaporwave).
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/features/ - fork-ported slash-command backends: plan, worktree, btw, loop_mode, shake, restart, skills, share, ssh, browser, budget, sandbox (mod.rs line 1-15 says wired from `rpc/slash.rs`/`slash_extra.rs`).
- /home/whrho/git/oh-my-tau/crates/omp-sandbox/ - sandbox feature backend (added with wave 1).
- /home/whrho/git/oh-my-tau/crates/omp-wasm/ + /home/whrho/git/oh-my-tau/docs/wasm-plugins.md - WASM (wasmi) plugin host, JSON-over-linear-memory ABI. omt-only.
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/slash.rs - slash dispatch incl. `/fast [on|ultra|off|status]` (911-943) where ultrafast is explicitly refused, and notices for unported TS commands (ratchet, security, handoff, smithery-search).
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/subcmd/daemon.rs - `omt daemon` (wave 1).
- /home/whrho/git/oh-my-tau/vendor/pa-agent, pa-ai, pa-models, pa-types - modified fork of PrimeIntellect-ai/prime-agent (NOTICE.md): the actual tau lineage in this repo.
- /home/whrho/git/oh-my-tau/Cargo.toml - workspace members listing omp-sot/omp-judge/omp-goose/laya-ort as first-class members; vendored crossterm patch for modifyOtherKeys.
- /tmp/oh-my-pi-ro/packages/coding-agent/src/modes/magic-keywords.ts - upstream TS magic keywords: ultrathink, chillax, orchestrate, workflowz, jevify. No ultracode, no Ultra mode.
- /tmp/oh-my-pi-ro/packages/coding-agent/src/slash-commands/builtin-modes.ts - upstream `/fast ... ultra` is the Ultrafast service tier (516), unrelated to `/ultra` modes.
- /tmp/oh-my-pi-omp2/crates/tui/src/components/editor.rs (337-344) - omp2 knows only ultrathink/orchestrate/workflowz gradients; no ultra modes, no sot/judge/goose/laya crates.
</files>

<answer>
Verdict table (omt = /home/whrho/git/oh-my-tau, upstream TS = omp, omp2 = Rust rewrite):

1. Magic keywords (`ultrathink`, `orchestrate`, `workflowz`, `jevify`) - INHERITED from omp TS (`/tmp/oh-my-pi-ro/packages/coding-agent/src/modes/magic-keywords.ts`). omt ports them in `crates/omp-tui/src/magic.rs` + `crates/omp-cli/src/notices.rs` + `prompts/notices/*.md`. Note omt DROPS upstream's `chillax` keyword and ADDS `ultracode` - both are divergence points.

2. `/ultra code|deep|long|hyper|off` - NOT upstream. Absent from omp TS (only `ultrafast` service-tier `/fast ultra` exists) and absent from omp2. This is an omt addition (README "Ultra modes"), backed by the `Ultra` enum in `crates/omp-tui/src/magic.rs` and `crates/omp-cli/src/slash_extra.rs`. Its sub-modes reference RLM (`llm_query`, lambda-RLM, SERA rubrics) and CLM (`clm-host`) - neither string appears anywhere in omp TS or omp2, so those are tau/prime-agent-lineage concepts that omt must keep. Must be preserved.

3. `omp-sot` (State of Thought, arXiv 2609.16055) - omt-only. No `sot` crate/dir in omp TS or omp2; no "State of Thought" text upstream. Currently reachable only via `examples/gsm8k_sot.rs` and unit tests - not wired into the runtime (grep for `omp-sot` outside its own crate returns nothing). Preserve as a crate, but it is not load-bearing for CLI behaviour.

4. `omp-judge` + `vendor/laya-ort` (Laya turn judge, ONNX, `judge-sessions`, `OMP_LAYA_JUDGE=1` shadow hook) - omt-only. `laya` appears nowhere in omp TS (its `prompts/judge/*.json` are unrelated TS eval judges: auto-graph, memory-relevance, title-drift, etc.) and omp2 has no judge/laya/onnx crate. The ONNX weights (1.3 GB) are gitignored build artifacts (`crates/omp-judge/onnx/*.onnx`). Must be preserved; note the `onnx` feature is default-on and disabled for musl.

5. `omp-goose` (goose-language dedup kernel compiled to C) - omt-only, added in wave 2 (`2dc9ce1`). No `.goose` files or goose runtime in either upstream. Must be preserved (also the sole reason `unsafe_code = "allow"` exists alongside omp-sandbox).

6. Web UI (`omt web`, port 7467, themes/wallpapers/per-session agents) - implemented wholly in `crates/omp-cli/src/subcmd/web/` with bundled assets. omp TS has no matching `web` subcommand (its `packages/collab-web` and `packages/browser-relay` are different things); omp2 has `crates/webview`, `crates/gui`, `crates/desktop` - different architecture. Must be preserved.

7. Slash commands - split into two groups:
   - Inherited/ported from omp TS: /help, /model, /effort, /compact (incl. `snapcompact` mode), /session, /tools, /todo, /mcp, /usage, /stats, /fast, /login, /logout, /fork, /branch, /rewind, /agents, /extensions, etc. `slash.rs` also carries explicit "not ported to omt" notices for TS commands: /ratchet, /security, /handoff, /mcp smithery-search - i.e. those are known upstream gaps, not fork features.
   - Fork additions (commit `95da100` "features wave 1"): /plan, /wt (worktree), /btw, /loop, /shake, /restart, /skills, /marketplace, /share, /ssh, /browser, /computer, /budget, /sandbox, plus `omt daemon` and `compact prompt`. Backends live in `crates/omp-cli/src/features/*.rs` + `crates/omp-sandbox/` + `crates/omp-cli/src/subcmd/daemon.rs`. Must be preserved.

8. Other omt-only additions worth keeping on the list: `crates/omp-wasm` + `docs/wasm-plugins.md` (wasmi plugin host, wave 2), `ext-host/` (Bun ExtensionAPI host running unmodified omp extensions), `crates/omp-collab` + `serve-collab`, `crates/omp-stats` (large, own server/rollup/usage-windows/taxonomy), `crates/omp-auth-broker`, `crates/omp-urls`, `vendor/laya-ort`, and the vendored crossterm patch (xterm modifyOtherKeys for ctrl+shift+v).

9. What omp2 does NOT contain at all: ultra modes, ultracode keyword, sot, judge/laya/onnx, goose, wasm, web UI subcommand, the features/ slash set. omp2 is a separate ground-up Rust rewrite (crates/agent, ai, tui, tool(s), serve, webview, gui, desktop, py, ar, audio, memory, vocab...). So syncing with omp2 gives you nothing of the above for free - every item in (2)-(8) is fork-only and must be re-applied by hand.

Provenance summary: only the magic-keyword mechanism and the bulk of the slash-command surface come from oh-my-pi. The tau/prime-agent lineage is real but narrow - it is the vendored `pa-*` crates (`vendor/pa-agent`, `pa-ai`, `pa-models`, `pa-types`, modified per NOTICE.md) plus the RLM/CLM concepts that `/ultra deep|long|hyper` names. The remaining crates (sot, judge, laya-ort, goose, wasm, web, features/) are original to oh-my-tau.
</answer>

<next_steps>
- Treat items 2-8 as a hard preservation checklist for any rebase onto omp or omp2; nothing there exists upstream.
- Before touching `crates/omp-tui/src/magic.rs`, confirm whether upstream's `chillax` keyword should be added back (it is missing in omt) and whether `ultracode` should stay omt-only.
- If the sync target is omp2, expect zero overlap in crate layout: map omt's `omp-cli`/`omp-tui` onto omp2's `crates/tui`/`crates/tools`/`crates/app`, and carry sot/judge/goose/laya-ort/web/features as new modules.
- `crates/omp-sot` is currently unwired (example-only); decide explicitly whether it is shipped surface or dead weight before spending effort porting it.
- To make future audits cheaper, restore a `vendor-src/oh-my-pi/` checkout (README/golden tests expect it) or pin the two reference trees used here.
</next_steps>
</results>