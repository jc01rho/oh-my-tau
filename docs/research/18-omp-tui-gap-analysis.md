<analysis>
**Literal Request**: Inspect /home/whrho/git/oh-my-tau/crates/omp-tui and /home/whrho/git/oh-my-tau/tests-tui (Cargo.toml, src files, LOC, theme/keybinding/rendering capabilities) and identify what is ported vs missing relative to omp main/omp2.
**Actual Need**: A porting gap ledger — what the Rust TUI already covers, what upstream reference code exists for each missing piece, and where to start.
**Success Looks Like**: Absolute paths, measured LOC, per-area ported/missing verdicts with evidence, and upstream file pointers for each gap.
</analysis>

<results>
<files>
- /home/whrho/git/oh-my-tau/crates/omp-tui/Cargo.toml - the crate under audit; ratatui 0.29 + crossterm 0.28 (vendored patch with modifyOtherKeys), pi-edit/pi-ast/pi-predict vendored deps, tree-sitter 0.25, rusqlite
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/lib.rs - 109 lines; declares 23 modules; documents the architecture (crossterm raw mode + ratatui draw loop, AppEvent in / Action out via RuntimePort)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/app.rs - 4,994 lines, 97 in-crate tests; the central App: event pump, key routing, slash pipeline, render orchestration
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/render.rs - 1,123 lines; inline (normal-screen) renderer with scrollback commit, DEC 2026 sync output, kitty keyboard probe, Multiplexer detection, emergency_restore
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/component.rs - 81 lines; the Component trait (render/handle_key/handle_mouse/wants_focus/take_actions) replacing pi-tui's TuiNode model
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/port.rs - 196 lines; RuntimePort trait (19 methods) + FakePort — the entire agent-facing boundary
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/keybindings.rs - 620 lines; 70 binding ids, TUI_KEYBINDINGS (32) + APP_KEYBINDINGS (38), canonicalKeyId port, keybindings.yml loader, conflict detection
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/theme/mod.rs - 186 lines; only 9 default tokens (accent/text/text_muted/background/success/warning/error/border/selected), glyph() + symbol() lookups
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/theme/loader/schema.rs - 219 lines; REQUIRED_COLORS catalog (69 tokens) + OPTIONAL_COLORS (thinkingMax) + camelCase->snake_case ALIASES
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/theme/loader/builtin/ - 102 built-in theme JSON files (dark/light + 100 named variants)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/theme/loader/symbols_data.rs - 851 lines, 809 symbol keys (upstream symbols.ts has 831)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/images.rs - 473 lines; ImageProtocol detection (Kitty/Sixel/Iterm2), kitty Unicode placeholder + 297 diacritics, ImageBudget; explicitly scopes OUT the commit-watermark/placement-epoch machinery
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/chat/ - 10 files, ~8.3k lines (messages.rs 2,697; markdown.rs 1,583; blocks.rs 1,603; code.rs 869; reveal.rs 664; rail.rs 555; latex.rs 546)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/overlays/ - 12 files, ~11.7k lines (settings_defs.rs 3,838; settings.rs 2,880; models.rs 2,276; sessions.rs 1,154)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/editor/ - 12 files, ~5.7k lines (vim.rs 2,047; mod.rs 1,320; render.rs 437; dispatch.rs 419; buffer.rs 379)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/tools/ - 21 files, ~9.6k lines; tool *view renderers* (bash.rs 1,981; task.rs 2,705; read.rs 997; search.rs 1,002) — not tool implementations
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/status_line.rs + status_line/ - 1,365 + 7 files, ~2.0k lines; segments/metrics/gauge/presets/separators/style/stats
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/extension_ui.rs - 1,529 lines; extension dialog surface
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/autocomplete.rs + autocomplete/fuzzy.rs - 2,242 + 674 lines
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/chrome/ - 10 files, ~2.3k lines (welcome.rs 629, notify.rs 532, title.rs 445, hud.rs 267, osc11.rs 147)
- /home/whrho/git/oh-my-tau/crates/omp-tui/src/word_completion/ - 3 files, 1,094 lines (host.rs 523, mod.rs 385, query.rs 186)
- /home/whrho/git/oh-my-tau/tests-tui/fixtures/themes/ - ONLY fixtures, no test crate: amethyst/amethyst-light/dark/dusk/light/expected.json/config.yml/theme-schema.json + resolve.ts/resolve-theme.ts (TS oracle scripts)
- /home/whrho/git/oh-my-tau/tests-e2e/tui/ - 12 Python files: run.py (parity harness, real PTY, TS-vs-Rust side by side), driver.py, mock_provider.py, probe_menu/paste/ts/typing.py, perf_metrics.py, stream_cadence.py, gen_settings_defs.py, dump_settings_defs.ts
- /home/whrho/git/oh-my-tau/crates/omp-cli/src/tui.rs - 982 lines; the embedding side that implements RuntimePort and calls omp_tui::run
- /tmp/oh-my-pi-ro/packages/tui/ - omp main TUI reference: 465 TS files, 213,123 src LOC + 102,837 test LOC across 386 test files
- /tmp/oh-my-pi-ro/packages/tui/src/theme/ - 18 files, 5,344 lines (theme-class.ts 892, theme.ts 870, symbols.ts 1,560, loader.ts 244, schema.ts 267, shimmer.ts 319, session-color.ts 276, tui-adapters.ts 458)
- /tmp/oh-my-pi-ro/packages/tui/src/keybindings.ts + app-keybindings.ts + keybinding-matchers.ts + keys.ts - 351 + 715 + 136 + 565 = 1,767 lines; 79 binding ids (32 tui.* + 47 app.*)
- /tmp/oh-my-pi-ro/packages/tui/src/terminal-capabilities.ts - 1,602 lines; the capability matrix omp-tui has no counterpart for
- /tmp/oh-my-pi-ro/packages/tui/src/terminal.ts - 3,053 lines; /tmp/oh-my-pi-ro/packages/tui/src/tui.ts - 4,874 lines
- /tmp/oh-my-pi-ro/packages/tui/src/overlays/ - 76 files (vs 12 in Rust)
- /tmp/oh-my-pi-ro/packages/tui/src/components/ - 28 files, 6,234-line editor.ts + 5,289-line markdown.ts
- /tmp/oh-my-pi-ro/packages/tui/src/prompt/ - 34 files (composer.ts 1,324, custom-editor.ts 2,139)
- /tmp/oh-my-pi-ro/packages/tui/src/charts/ - 3 files, 11,267 lines (chart-plan.ts 5,045, chart-svg.ts 4,654, table-data.ts 1,568) — no Rust counterpart at all
- /tmp/oh-my-pi-ro/packages/tui/src/html-form/ - 3 files, 3,189 lines — no Rust counterpart at all
- /tmp/oh-my-pi-ro/packages/tui/src/apps/ - 14 files (git-tui, ps-top, session-picker, debug viewers) — no Rust counterpart
- /tmp/oh-my-pi-ro/packages/tui/src/multiplexers/ - 9 files (Rust folds this into render.rs Multiplexer enum)
- /tmp/oh-my-pi-omp2/crates/tui/ - omp2's from-scratch Rust TUI: ~100 files, 96,899 LOC; retained-mode component tree (Ui/Renderer/slots/dom!) — architecturally incompatible with omp-tui's immediate-mode ratatui approach
- /tmp/oh-my-pi-omp2/crates/tui/src/theme.rs - 844 lines (vs omp-tui's 186-line theme/mod.rs + 219-line schema.rs)
- /home/whrho/git/oh-my-tau-migration-plan/docs/rust-migration-plan.md - the existing gap analysis; §2.1 measures omp-tui at 103 files / 66,466 lines; §4 covers tools/shell/memory/VCS/extension gaps but says almost nothing about TUI surface
- /home/whrho/git/oh-my-tau-migration-plan/docs/research/ - 14 files, 1,286 lines of upstream investigation notes
</files>

<answer>
**Measured state of omp-tui** (all counts from `wc -l`, excluding target/):

| Metric | Value |
|---|---|
| Files in crates/omp-tui/src | 103 (103 matches the migration plan's count) |
| Rust LOC | 66,363 (plan says 66,466 — 103 lines of drift) |
| In-crate `#[test]` | 761 across 30+ files |
| Theme JSON files | 102 builtin + 9 test fixtures |
| Keybinding ids | 70 |
| Theme tokens in default map | 9 |
| Theme tokens in loader schema | 69 required + 1 optional |

**What IS ported** (evidence: module doc comments naming the exact TS source):

1. **Inline renderer with scrollback commit** — `render.rs` ports pi-tui's normal-screen provider paint: `Terminal::insert_before` for committed transcript rows, live region repainted each frame, DEC 2026 sync output bracketing, autowrap-off/on, hardware cursor parked on live-region top row, `emergency_restore()` on panic/signal, `Multiplexer::detect()` (Herdr/Tmux/Screen/Zellij/Cmux) driving the rebuild-scrollback strategy.
2. **Kitty keyboard protocol** — `render.rs` has the full probe (`probe_kitty_keys`), push-flag logic with a ConPTY carve-out (drops flag 4 so Shift+letter survives), and the vendored crossterm adds xterm modifyOtherKeys `CSI 27;mods;code~` decoding that upstream lacks.
3. **Theme system** — 102 builtin themes; loader with hex/256-index/vars resolution; full 69-token required catalog mirroring TS `themeColorsSchema`; camelCase↔snake_case alias table; 809 of 831 symbol keys; appearance detection (OSC 11 probe in `chrome/osc11.rs`, COLORFGBG fallback); session accent colors; 3 symbol presets (unicode/nerd/ascii).
4. **Keybindings** — 70 of 79 upstream ids. Canonicalization (`ctrl,shift,alt,super` ordering, esc→escape, return→enter, bare-uppercase→shift+lower), shifted-symbol aliasing, `keybindings.yml` loader with per-key conflict detection, precomputed alias sets on the hot path. Missing 11: `app.approval.approve`, `app.ask.hide`, `app.ask.note`, `app.btw.inject`, `app.links.hint`, `app.local.open`, `app.session.pin`, `app.session.unpin`, `app.stt.pushToTalk`, `app.tree.nextBranch`, `app.tree.previousBranch`.
5. **Chat/transcript** — markdown, syntax-highlighted code blocks, LaTeX, thinking display, streaming reveal, rail-style tool rows.
6. **Editor** — buffer, undo, kill-ring, history, vim mode (2,047 lines), dispatch, layout, attachments, paste, bracketed paste, submit.
7. **Overlays (12)** — settings (2,880 + 3,838-line defs), models (2,276), sessions (1,154), tree, thinking, diff, help, session_info, command_palette, large_paste.
8. **Status line** — segments, metrics, gauges, presets, separators, style, stats.
9. **Autocomplete + word completion** — 2,242-line autocomplete with 674-line fuzzy matcher; 1,094-line word-completion subsystem.
10. **Images** — kitty Unicode placeholders (297-diacritic table), tmux DCS passthrough, `d=I` purge, per-surface ImageBudget (8 default). Explicitly excludes the commit-watermark/placement-epoch machinery.
11. **Clipboard, mouse (SGR), notifications (OSC 9/777/bell), terminal title, OSC 11, welcome/banner/mascot/HUD chrome.**

**What is MISSING relative to omp main** (`/tmp/oh-my-pi-ro/packages/tui/src/`):

| Area | Upstream | omp-tui | Verdict |
|---|---|---|---|
| Overlays | 76 files | 12 files | ~64 missing: model-hub (4,218), agent-hub (2,694), agents-hub (1,862), plan-review (2,602), usage-dashboard (1,875), mcp-add-wizard (1,912), session-selector (2,353), ask-dialog (1,561), copy-selector (1,244), annotation-overlay (2,040), context-viewer, theme-selector, login-dialog, oauth-selector, jobs-panel, btw-*, omfg-panel, plugin-selector, rewind-selector, hook-editor/input/selector, link-browser, queue-mode-selector, pause-screen, spotlight-sheet, agent-activity/transcript-viewer, extension panels |
| Charts | 3 files / 11,267 lines | 0 | Completely absent — chart-plan (5,045), chart-svg (4,654), table-data (1,568) |
| HTML forms | 3 files / 3,189 lines | 0 | Completely absent — dialog (2,324), parse (582), data (283) |
| Apps | 14 files | 0 | git-tui/diff-pane/sidebar (5,571 total), ps-top, autoresearch-dashboard (1,282), cleanse-board/picker, live-visualizer, shell-visualizer, session-picker, setup-model-picker, debug/* (log-viewer 1,219, protocol-probe, raw-sse, terminal-info, viewer-frame) |
| Components | 28 files (editor 6,234, markdown 5,289) | folded into editor/ and chat/ | Layout primitives absent as a named layer: scroll-view, scroll-viewport, split-pane, stack, row, tab-bar, tree-view, select-list, settings-list, table, sparkline, progress-bar, wizard-step, disclosure, key-value-list, metric, truncated-text, image, box, section, spacer, cancellable-loader |
| Terminal capabilities | terminal-capabilities.ts 1,602 lines | no module (logic scattered in images.rs / render.rs / chrome/notify.rs) | No unified capability matrix; no DECRQM runtime probing (render.rs says "Runtime DECRQM adjustment is a P1 item; this is the P0 static gate") |
| Terminal lifecycle | terminal.ts 3,053 lines | partial in render.rs (1,123) | Resize replay, focus events, stdin buffering, loop watchdog, frame-rate control all absent |
| stdio plumbing | stdin-buffer.ts 1,236, loop-sampler, loop-watchdog, frame-rate, spinner-clock | 0 | Absent |
| Multiplexers | 9 dedicated files | 1 enum in render.rs (5 variants) | No per-mux notification/escape quirks (herdr, orca, wmux missing) |
| Prompt subsystem | 34 files (composer 1,324, custom-editor 2,139, gradient-highlight, magic-keywords, emoji/github-ref/model-mention/internal-url autocomplete, skill-tokens, ordered-pastes, queued-messages, prose-gate, video, tips) | partially in composer.rs (656) + editor/ + magic.rs (627) + magic_paint.rs (212) | Most prompt sub-modules absent: model-mention, github-ref autocomplete, emoji autocomplete, gradient highlight, ordered-pastes, queued-messages as a distinct layer, prose-gate, video |
| Session color / shimmer | session-color.ts 276, shimmer.ts 319 | session_accent.rs 298; shimmer inline in app.rs (`welcome_shimmer`) | Shimmer not a reusable primitive |
| Sixel | sixel.ts 185 | detection only in images.rs | No sixel encoder |
| Media/video | media.ts 106, prompt/video.ts | 0 | Absent |
| Windows input | windows-altgr.ts, windows-input-mode.ts | 0 | Absent (Windows is build-supported but input modes unported) |
| Motion effects / space-hold / STT gesture | motion-effects.ts, space-hold.ts | `app.stt.toggle` binding declared with `&[]` (no gesture) | Binding exists, gesture does not |
| Native render bridge | native/ 18 files | 0 | Not applicable (main's TS-side bridge to pi-natives) |
| Test surface | 386 test files / 102,837 lines | 761 in-crate `#[test]`; tests-tui holds fixtures only | No dedicated test crate; parity harness lives in tests-e2e/tui (Python, not in CI) |

**Missing relative to omp2** (`/tmp/oh-my-pi-omp2/crates/tui/`, 96,899 LOC): this is a different architecture, not a superset. omp2 is retained-mode — `Ui` owns a component tree built by a `dom!` macro, `Renderer::present_plan` is the single slots-to-terminal seam acknowledging `Delivered::All`, `slots` owns transcript block lifecycle + resize replay + staged delivery. omp-tui is immediate-mode ratatui: every widget repaints into a `Frame` each tick and the terminal loop diffs full frames (stated verbatim in `component.rs`). Porting omp2 wholesale would mean rewriting all 66k lines. Specific omp2-only capabilities worth borrowing piecemeal: `theme.rs` (844 lines, oklch/lab/lch color spaces under `src/color/`), `scene/` (532 + 946 geometry + 751 integrator), `shader.rs`, `editcore.rs` (5,320), `spell.rs` (787), `markdown/mermaid.rs` + `graphviz.rs` (1,195), `latex/unicode.rs` (2,435), `components/tests.rs` (2,005-line component conformance suite), `icons.tsv` + build-time catalog generation.

**Key finding for the caller**: the migration plan at `/home/whrho/git/oh-my-tau-migration-plan/docs/rust-migration-plan.md` has a thorough gap analysis for tools (18/29), shell (0 parity), memory (absent), VCS, compaction, and extensions — but the TUI section is essentially silent. §2.1 only reports the LOC. This is a real hole: the TUI is the largest single crate (66k lines, 27% of `crates/`), and it is missing ~64 overlays, all charts, all HTML forms, all standalone apps, and the unified capability matrix.

**Verification posture is also weak**: `tests-tui/` contains only theme fixtures with TS oracle scripts (`resolve.ts`, `resolve-theme.ts`) — there is no Cargo test target there at all. The real parity harness is `/home/whrho/git/oh-my-tau/tests-e2e/tui/run.py` (real PTY, side-by-side TS vs Rust, mock provider, 4 probe scripts), and per the migration plan §2.3 it does not run in CI.
</answer>

<next_steps>
1. **Start with the capability matrix, not overlays.** `terminal-capabilities.ts` (1,602 lines) gates nearly every other feature — sync output, kitty keys, sixel, image budget, notification channels, hyperlink support. omp-tui has the logic scattered across `render.rs`, `images.rs`, and `chrome/notify.rs` with no single source of truth. Extracting one `terminal_capabilities.rs` unblocks the rest and is the smallest high-leverage change.
2. **Prioritize overlays by user-facing frequency.** model-hub (4,218) and agents-hub (2,694+1,862) are the two largest single gaps and map directly onto existing `overlays/models.rs` and `tools/task.rs` state — port those first. usage-dashboard (1,875) and session-selector (2,353) map onto existing `overlays/sessions.rs` and `status_line/metrics.rs`.
3. **Skip charts, HTML forms, and apps for now.** 14,456 lines across 20 files with no existing Rust scaffolding and no user-visible regression if absent. Treat as a separate phase.
4. **Do not port omp2's TUI.** Its retained-mode architecture is incompatible with omp-tui's immediate-mode ratatui loop. Cherry-pick `theme.rs` color spaces (oklch/lab/lch), `components/tests.rs` as a conformance-suite model, and the mermaid/graphviz markdown renderers only.
5. **Wire the parity harness into CI before any porting work.** `tests-e2e/tui/run.py` exists and works; it just is not run. Without it, every porting change is unverified.
6. **Fix the 11 missing keybindings as a single small patch** — they are declared in upstream `app-keybindings.ts` and absent from `keybindings.rs`; several (`app.session.pin`/`unpin`, `app.tree.nextBranch`/`previousBranch`) will be needed by the overlay ports anyway.
</next_steps>
</results>