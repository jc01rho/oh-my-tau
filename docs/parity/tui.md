# TUI Parity — omp-tui vs `pi-tui`

Measured against upstream `can1357/oh-my-pi` main @ `efcbef6` (2026-11-22),
`packages/tui/src/` (overlays, keybindings, hotkeys sheet) and
`packages/coding-agent/src/modes/controllers/` (how they open). Counts are
`wc -l` / `ls` of that checkout and of this tree; nothing is estimated.

Scope: keybindings, overlays and dialogs, terminal-capability matrix, E2E
harness status. Charts, HTML forms and the standalone `apps/` tree are out of
scope (see `docs/research/18-omp-tui-gap-analysis.md`).

| Area | omp-tui | upstream `pi-tui` | Status |
|---|---|---|---|
| Keybinding ids | 79 (32 `tui.*` + 47 `app.*`) | 79 | **79/79 declared**; 14 `app.*` ids wired, the editor chords hard-wired (§1) |
| Overlays / dialogs | 10 overlays + extension dialogs | 83 files (74 + `extensions/` 9), 54,924 lines | **8 ported, 15 partial, 60 absent** (§2) |
| Terminal capability matrix | `terminal_capabilities.rs` (Phase 8) | `terminal-capabilities.ts` | **ported** (§3) |
| E2E parity harness | `tests-e2e/tui/run.py`, 24 scenarios | — | **22 green, 2 tracked** (§4) |

---

## 1. Keybindings — 79/79 declared

`crates/omp-tui/src/keybindings.rs` declares exactly the upstream ids: 32
`tui.*` (`keybindings.ts`) and 47 `app.*` (`app-keybindings.ts`), with
upstream defaults. Phase 8c retired the two Rust-only ids
`app.tree.foldOrUp` / `app.tree.unfoldOrDown` (never wired; upstream removed
fold/unfold — tui CHANGELOG — because `/tree` is always fully expanded and the
branch-jump ids own those moves), so a `keybindings.yml` written for `omp`
now maps 1:1.

Mechanism is ported, not just the table: `canonical_key_id` ↔
`canonicalKeyId`, `add_key_aliases` ↔ `addKeyAliases` (same 20 shifted
symbols), `KeybindingsManager` (user overrides replace defaults, `[]`
disables, conflict detection, precomputed alias sets), the `keybindings.yml`
loader (`OMP_AGENT_DIR` aware), `key_id_from_event` ↔ `parseKey` +
`canonicalKeyId`, and since Phase 8c `format_key_hint` / `format_key_hints`
↔ `formatKeyHint` / `formatKeyHints` (theme `key.*` symbols, ⌃⌥⇧⌘ on
macOS, `Ctrl+` elsewhere) plus `display_string` ↔ `getDisplayString`.

### Wired `app.*` ids

| Id | Where | Behavior |
|---|---|---|
| `app.interrupt`, `app.clear`, `app.exit` | `app.rs` | Esc ladder (double-Esc opens the rewind column), Ctrl+C clear/double-tap exit, Ctrl+D exit |
| `app.thinking.cycle` | `app.rs` | cycle the effort level |
| `app.message.dequeue` | `app.rs` | pop the last queued message into the editor |
| `app.tools.expand` | `app.rs` | toggle tool output; an open approval prompt takes it for its details first, `/tree` takes it as its filter cycle (TS input-controller) |
| `app.clipboard.pasteImage` / `pasteTextRaw` | `app.rs` | clipboard paste (smart / raw) |
| `app.approval.approve` | `app.rs` → `ExtensionUi::approve_active_at` | a tool-approval prompt approves only once armed (500 ms after it appears — the default chords double as the follow-up chord); a confirm answers yes; a select picks its Approve/Allow/Yes/Accept option |
| `app.ask.hide` | `ExtensionUi::toggle_hidden` | the pending dialog stops rendering/owning keys until pressed again (Phase 8b) |
| `app.session.tree` | `app.rs` (editor) | opens `/tree`; unbound by default like upstream |
| `app.tree.previousBranch` / `nextBranch` | `overlays/tree.rs` | jump to the first visible row of the adjacent branch (wraps); unbound by default like upstream, shown in the `/tree` hints once bound |
| `app.stt.pushToTalk` | `app.rs` | notice only (`stt.enabled` on, empty editor): omp has no STT backend (Phase 8b) |

Editor chords (`tui.editor.*`, follow-up `ctrl+enter`/`ctrl+q`, newline
`shift+enter`/`ctrl+j`, undo `ctrl+-`) are hard-wired in
`editor/dispatch.rs`, so user overrides of those ids do not apply yet.

### Declared but not wired

`app.ask.note` (needs per-option notes in the ask tool, §2), `app.btw.inject`
(no btw panel), `app.links.hint` (no link browser), `app.local.open` (no
`local://` store), `app.session.pin` / `unpin` (no pin store),
`app.suspend`, `app.display.reset`, `app.model.cycleForward` /
`cycleBackward`, `app.model.select`, `app.model.selectTemporary`,
`app.plan.toggle`, `app.history.search`, `app.tools.toggleVisibility`,
`app.thinking.toggle`, `app.editor.external`, `app.retry`,
`app.clipboard.copyLine` / `copyPrompt`, `app.live.toggle`,
`app.agents.hub` / `app.session.observe`, `app.stt.toggle`, and the
session-picker ids (`togglePath`, `toggleSort`, `rename`, `delete`,
`deleteNoninvasive`, `new`, `fork`, `resume`) outside what
`overlays/sessions.rs` hard-wires.

The `?` / `/help` / `/hotkeys` sheet (`overlays/help.rs`, port of
`hotkeys-markdown.ts`) is built from the effective bindings: user overrides
show, an unbound action reads `Disabled`, and it lists only the shortcuts
above that omp-rs implements — the TS rows for unwired actions are left out
instead of advertised (the old MVP sheet claimed `ctrl+k` opens the command
palette and PgUp/PgDn scroll the chat; neither is true).

---

## 2. Overlays and dialogs — 8 ported, 15 partial, 60 absent of 83

### Phase 8c

- **`/tree` session tree** (`overlays/tree.rs`, TS `tree-selector.ts` over
  `TreeView`). Reads the live session journal through
  `RuntimePort::session_journal` (every entry of the session file and the
  writer's live leaf, so it works after `/resume` and after earlier moves),
  not only this run's `message_end` frames. Active-branch-first ordering,
  linear chains at their head's depth, three-cell `├─`/`└─`/`│` gutter with
  `…` compression, `•` active path, `[label]` badges, per-kind row text
  (`user:`, `assistant:`, `[read: path:3-7]`, `[compaction: 12k tokens]`,
  `[branch summary]: …`, …), five filters (Alt+D/T/U/L/A, Ctrl+O /
  Ctrl+Shift+O, `treeFilterMode` setting), fuzzy search, Alt+↑/↓ turn
  jumps, branch jumps, ←/→ paging, the three empty states. Enter navigates
  (`RuntimePort::navigate_tree` → omp-cli `tree::navigate_tree`),
  Shift+Enter navigates with a branch summary (the runtime's compaction
  summarizer), `branchSummary.enabled` asks "Summarize branch?" first, the
  live leaf answers "Already at this point", Shift+L edits a label
  (`RuntimePort::set_entry_label` → a `label` entry) and reloads in place.
  After a move the host sends `tree_navigated`: the transcript is rebuilt
  for the new branch and a rewound prompt returns to an empty editor.
  Also fixes a latent bug: the old navigator reported its highlighted row as a
  pick on every drain, so any key routed to an overlay after `/tree` had been
  opened could rewind the session.
- **Tool-approval dialog** (`extension_ui/approval.rs`, TS
  `approval-dialog.ts`). The omp-cli gate tags its select `kind:
  "approval"` and sends the untruncated details as `fullPrompt`. Title +
  details capped at 8 clipped rows with a scrollbar (PgUp/PgDn), Ctrl+O swaps
  in every detail wrapped, Approve highlighted, ↑/↓ wrap, Esc denies, the
  approve chord arms after 500 ms. Divergence: Enter is not armed (the
  `approval` E2E scenario presses it as the prompt appears).
- **Keyboard sheet** (`overlays/help.rs`, `hotkeys-markdown.ts`) — §1.
- **Ask dialog** (`extension_ui.rs`, TS `ask-dialog.ts`): per-question
  answers, cursors and toggles; Tab/Shift+Tab/←/→ switch questions, ↑/↓
  cross question edges, answering the last question opens the first
  unanswered one and the form submits once all are answered; title `Ask · n
  of N answered`, question strip (✓ answered, ▸ open, ○ unanswered), ◉/○ and
  ☑/☐ markers, the cursor starts on the recommended option, the Other row
  previews and re-opens a given custom answer.

### Upstream overlay status

| Upstream file | Lines | Status | omp-rs surface / gap |
|---|---:|---|---|
| `advisor-config.ts` | 1678 | ABSENT |  |
| `agent-activity.ts` | 122 | ABSENT |  |
| `agent-hub-projection.ts` | 438 | ABSENT |  |
| `agent-hub-renderer.ts` | 435 | ABSENT |  |
| `agent-hub-types.ts` | 80 | ABSENT |  |
| `agent-hub.ts` | 2700 | ABSENT |  |
| `agent-identity.ts` | 183 | ABSENT |  |
| `agent-transcript-viewer.ts` | 870 | ABSENT |  |
| `agents-hub.ts` | 1862 | ABSENT |  |
| `annotation-overlay.ts` | 2040 | ABSENT | `overlays/diff.rs` ports the older `diff-overlay.ts` (read-only diff); annotations absent |
| `annotation-types.ts` | 104 | ABSENT |  |
| `approval-dialog.ts` | 530 | PORTED | `extension_ui/approval.rs` (8c) — no grants / approve-for-session rows (the omp-cli gate offers Approve/Deny only) and no type-to-deny-and-steer draft |
| `ask-dialog.ts` | 1561 | PARTIAL | `extension_ui.rs` Ask (8c) — question strip, Tab/Shift+Tab/←/→ and edge-crossing ↑/↓ navigation, per-question answers, ◉/○ ☑/☐ markers, Other preview; missing per-option notes (`app.ask.note`, needs omp-cli ask-tool note fields), option previews, Ctrl+O description expand, "Chat about this" |
| `bordered-loader.ts` | 59 | ABSENT |  |
| `btw-history-panel.ts` | 1206 | ABSENT |  |
| `btw-history.ts` | 109 | ABSENT |  |
| `btw-inject.ts` | 91 | ABSENT | `app.btw.inject` unwired |
| `btw-note.ts` | 225 | ABSENT |  |
| `btw-panel.ts` | 283 | ABSENT |  |
| `cleanse-panel.ts` | 260 | ABSENT |  |
| `codex-reset-fireworks.ts` | 437 | ABSENT |  |
| `composer-shape-preview.ts` | 164 | ABSENT |  |
| `composer-shape-registry.ts` | 82 | PORTED | `composer.rs` |
| `context-viewer-format.ts` | 173 | ABSENT |  |
| `context-viewer-overview.ts` | 381 | ABSENT |  |
| `context-viewer.ts` | 729 | ABSENT |  |
| `copy-selector.ts` | 1244 | ABSENT |  |
| `copy-targets.ts` | 189 | ABSENT |  |
| `error-banner.ts` | 94 | PORTED | `chat/blocks.rs` |
| `history-search.ts` | 524 | ABSENT | `app.history.search` unwired |
| `hook-editor.ts` | 476 | PARTIAL | `extension_ui.rs` editor — no Ctrl+G external editor |
| `hook-input.ts` | 194 | PARTIAL | `extension_ui.rs` input — no secret (masked) input |
| `hook-selector.ts` | 935 | PARTIAL | `extension_ui.rs` select/confirm — no radio/checkbox selection markers |
| `hub-frame.ts` | 361 | PORTED | inside `overlays/models.rs` |
| `job-tree.ts` | 177 | ABSENT |  |
| `jobs-panel.ts` | 759 | ABSENT |  |
| `link-browser.ts` | 295 | ABSENT | `app.links.hint` unwired |
| `login-dialog.ts` | 544 | PARTIAL | `/login` through `extension_ui.rs` fullscreen select + input |
| `logout-account-selector.ts` | 222 | ABSENT |  |
| `mcp-add-wizard.ts` | 1912 | ABSENT |  |
| `model-browser.ts` | 2528 | PARTIAL | `overlays/models.rs` — same detail-row gap as model-hub |
| `model-hub.ts` | 4218 | PARTIAL | `overlays/models.rs` — detail rows carry name/ctx/out/price/reasoning/vision/role chips; no new/beta/recommended badges, intelligence, tps/ttft, credit notes or description (omp-rs catalog has no such fields), no over-context compact-then-switch |
| `model-picker.ts` | 735 | PARTIAL | `overlays/models.rs` |
| `model-selector.ts` | 107 | PARTIAL | `overlays/models.rs` (`/switch`) |
| `move-overlay.ts` | 260 | ABSENT |  |
| `oauth-selector.ts` | 699 | ABSENT |  |
| `omfg-panel.ts` | 223 | ABSENT |  |
| `pause-screen.ts` | 440 | ABSENT |  |
| `plan-review-overlay.ts` | 2602 | ABSENT | needs a `plan_review_requested` producer; omp-cli plan mode emits none |
| `plan-save-overlay.ts` | 121 | ABSENT | with plan-review |
| `plan-toc.ts` | 148 | ABSENT | with plan-review |
| `plugin-selector.ts` | 133 | ABSENT |  |
| `plugin-settings.ts` | 1036 | ABSENT |  |
| `queue-mode-selector.ts` | 83 | ABSENT |  |
| `report-panel.ts` | 195 | ABSENT |  |
| `reset-usage-selector.ts` | 408 | ABSENT |  |
| `rewind-selector.ts` | 1199 | PARTIAL | `overlays/tree.rs` Rewind mode — user-prompt column only: no transcript replay, dotted outline, sibling-branch strip or undo |
| `running-subagent-badge.ts` | 30 | ABSENT |  |
| `session-account-selector.ts` | 104 | ABSENT |  |
| `session-info-overlay.ts` | 401 | PORTED | `overlays/session_info.rs` |
| `session-observer-registry.ts` | 297 | ABSENT |  |
| `session-selector.ts` | 2353 | PARTIAL | `overlays/sessions.rs` — no pin/unpin store (`app.session.pin`/`unpin` unwired) |
| `settings-defs.ts` | 368 | PARTIAL | `overlays/settings_defs.rs` |
| `settings-selector.ts` | 2016 | PARTIAL | `overlays/settings.rs` |
| `show-images-selector.ts` | 62 | ABSENT |  |
| `snapcompact-shape-preview.ts` | 243 | ABSENT |  |
| `spotlight-sheet.ts` | 572 | ABSENT |  |
| `stats-notice.ts` | 31 | ABSENT |  |
| `theme-selector.ts` | 97 | PARTIAL | `/settings` theme rows (live preview) and `/theme` — no standalone picker |
| `thinking-selector.ts` | 76 | PORTED | `overlays/thinking.rs` |
| `tree-selector.ts` | 1794 | PORTED | `overlays/tree.rs` Tree mode (8c) — no native preview pane; the summary prompt has no "Summarize with custom prompt" (omp-cli `navigate_tree` takes no custom instructions) |
| `usage-dashboard.ts` | 1890 | ABSENT |  |
| `usage-display.ts` | 126 | PARTIAL | `/usage` → `overlays/session_info.rs` ("Usage") |
| `usage-row.ts` | 268 | PORTED | `chat/blocks.rs` |
| `extensions/display-text.ts` | 39 | ABSENT |  |
| `extensions/extension-dashboard.ts` | 901 | ABSENT |  |
| `extensions/extension-list.ts` | 931 | ABSENT |  |
| `extensions/inspector-model.ts` | 484 | ABSENT |  |
| `extensions/inspector-panel.ts` | 1037 | ABSENT |  |
| `extensions/live-tool-session.ts` | 67 | ABSENT |  |
| `extensions/mcp-runtime.ts` | 289 | ABSENT |  |
| `extensions/state-manager.ts` | 377 | ABSENT |  |
| `extensions/types.ts` | 208 | ABSENT |  |

The keyboard sheet (`hotkeys-markdown.ts`) lives outside `overlays/` and is
ported (§1). Rust-only overlays with no file of their own upstream:
`command_palette.rs`, `large_paste.rs`.

### Remaining, by user impact

1. **Todo HUD** (`interactive-mode.ts` `TodoHudContainer`, the sticky `TODO`
   above the composer with `/todo expand|collapse`): omp-cli emits no
   todo-phase frame for `/todo` edits and the TUI keeps no phase state; needs
   a `todo_update` frame from `slash.rs` `commit_todos` and the todo tool
   settle path, then a chrome block in the live-height budget. `/todo` itself
   prints the markdown list like upstream.
2. **Plan review** (`plan-review-overlay.ts`, `plan-toc.ts`,
   `plan-save-overlay.ts`): no `plan_review_requested` producer in omp-cli.
3. **Agent hub** (`agent-hub*.ts`, `agents-hub.ts`, transcript viewer):
   state exists in `tools/task.rs`, no overlay.
4. **Ask notes** (`app.ask.note`): the omp-cli ask tool parses no note fields
   and renders none into the tool result.
5. **Model detail extras** (badges, intelligence, tps/ttft, description,
   over-context compact-then-switch): need catalog fields and a runtime
   compact-before-switch.
6. **Rewind replay** (`rewind-selector.ts` transcript replay + branch strip),
   **history search**, **copy selector**, **context viewer**, **usage
   dashboard**, **MCP add wizard**, **extensions dashboard** (`extensions/`),
   **btw panels**, **jobs panel**, then the long tail in the table above.

---

## 3. Terminal capability matrix — ported

`crates/omp-tui/src/terminal_capabilities.rs` (Phase 8, PR #15) is the port of
`terminal-capabilities.ts`: one module identifying the host terminal from its
environment and deciding DEC 2026 synchronized output, the kitty keyboard
protocol, the inline image protocol, OSC 8 hyperlinks, the notification
channel and 24-bit color; `render.rs`, `images.rs` and the notification sink
consult it instead of probing the environment themselves.

---

## 4. E2E status — 22 green, 2 tracked

Harness: `tests-e2e/tui/run.py` (967 lines) — real PTY, isolated `HOME`, local
mock provider (`mock_provider.py`), scripted keystrokes via `driver.py`, TS and
Rust clients driven by the same script, screens written side by side, and a
`baseline` verdict when omp itself fails a check (so a harness bug is never
scored against omp-rs).

`SCENARIOS` declares 24 entries:

```
startup, two_prompts, tool, fail, tools, usage, slash, new_clears, login,
exit, model, overlay_model, overlay_settings, settings_edit, composer_shape,
effort, extensions, ext_developer, approval, word_complete, paste_small,
paste_large, paste_clipboard, paste_clipboard_kitty
```

Status: **22 green**. Two scenarios sit outside that count:

- `ext_developer` — the developer-role extension scenario; needs
  `prepare_ext_probe` fixtures and the dev-role flow.
- `new_clears` — `/new` does clear the transcript and the scenario passes
  locally, but it is a timing flake, so CI excludes it from the gate.

Both stay in `SCENARIOS` deliberately: `ext_developer` is the acceptance test
for the work that closes the dev-role flow, and `new_clears` goes back into the
CI `--only` set once the flake is fixed.

Caveats that matter for anyone reading "green" as a guarantee:

1. **Not run in this session.** The harness needs built `omt` and `omp`
   binaries (`--rs target/shared/release/omt`); the status above is the
   recorded harness result, not a re-run.
2. **CI runs a subset, not the whole harness.** `.github/workflows/ci.yml`
   gates the 22 green scenarios by naming them in `--only`; `ext_developer`
   (developer-role gap) and `new_clears` (timing flake) are left out. A
   scenario failing outside that subset is never caught automatically.
3. The coverage is 24 scripted paths against a mock provider — it proves
   scripted parity, not interactive-behaviour parity across overlays. The
   ~60 unported overlays have no scenario at all.

---

## 5. Recommended order

1. **Re-enable `ext_developer` and `new_clears` in the `tui-e2e` job** so the
   whole 24-scenario harness is a regression gate (§4).
2. **Todo HUD**, then **ask notes** (tool + dialog), then the **agent hub** —
   the remaining overlays with the highest everyday use (§2 "Remaining").
3. **Wire the declared app ids** whose behavior already exists in the runtime
   (model cycling, `app.model.select` → the `/model` hub, retry, external
   editor), adding each to the keyboard sheet as it lands.
4. Defer `plan-review`, `annotation-overlay` and the `apps/` tree until
   their producers exist.

## References

- `docs/research/18-omp-tui-gap-analysis.md` — full crate audit, measured LOC,
  charts/HTML-forms/apps gaps, omp2 architectural mismatch.
- `crates/omp-tui/src/keybindings.rs` — 79-id table, canonicalization, loader,
  key-hint formatting.
- `crates/omp-tui/src/overlays/mod.rs` — focus stack and `centered()` popup rule.
- `crates/omp-tui/src/overlays/tree.rs`, `extension_ui/approval.rs`,
  `overlays/help.rs`, `extension_ui.rs` — the Phase 8c overlays.
- `tests-e2e/tui/run.py` — `SCENARIOS`.
