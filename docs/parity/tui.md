# TUI Parity — omp-tui vs `pi-tui`

Measured against the read-only reference checkout at `/tmp/oh-my-pi-ro/packages/tui/src/`
(upstream = `omp` main). All counts below are from `wc -l` / `ls` in this worktree
(`.worktrees/phase1-parity`) or that checkout; nothing here is estimated.

Scope: keybindings, overlays, terminal-capability matrix, E2E harness status.
Charts, HTML forms and the standalone `apps/` tree are out of scope for this
document (see `docs/research/18-omp-tui-gap-analysis.md` for those).

| Area | omp-tui | upstream `pi-tui` | Status |
|---|---|---|---|
| Keybinding ids | 70 | 79 | **70/79 — 9 net missing** (11 missing, 2 Rust-only extras) |
| Overlay modules | 12 files / 10 overlays | 76 files | **12/76** |
| Terminal capability matrix | none (static gate in `render.rs`) | `terminal-capabilities.ts` 1,602 lines | **absent** |
| E2E parity harness | `tests-e2e/tui/run.py`, 24 scenarios | — | **22 green, 2 tracked** |

---

## 1. Keybindings — 70/79

Files: `crates/omp-tui/src/keybindings.rs` (620 lines) vs
`/tmp/oh-my-pi-ro/packages/tui/src/keybindings.ts` (351) +
`app-keybindings.ts` (715).

Upstream declares 79 ids: 32 `tui.*` in `keybindings.ts` and 47 `app.*` in
`app-keybindings.ts`. `keybindings.rs` declares 70 — all 32 `tui.*` (exact
match) plus 38 `app.*`.

Mechanism is faithfully ported, not just the table:

- `canonical_key_id` ↔ TS `canonicalKeyId` — modifier order `ctrl,shift,alt,super`,
  base lowercased, `esc`→`escape`, `return`→`enter`, bare uppercase letter →
  `shift+<lower>`.
- `add_key_aliases` ↔ `addKeyAliases` — the 20-entry `SHIFTED_SYMBOL_KEYS` set
  is identical, so a `?` binding still matches when the terminal reports
  `shift+?`.
- `KeybindingsManager::new` ↔ the `KeybindingsManager` class — user overrides
  *replace* defaults (no merge), empty list disables, per-key conflicts among
  user overrides are collected, precomputed alias sets make the hot path a
  set probe.
- `load_user_bindings` ↔ the `keybindings.yml` loader — flat
  `"binding-id": key | [keys] | []`, `OMP_AGENT_DIR` honoured; malformed YAML
  is an error the caller surfaces as a notice rather than a panic.
- `key_id_from_event` ↔ `parseKey` + `canonicalKeyId` fused, including the
  explicit-SHIFT-flag carry (CSI-u / modifyOtherKeys).

### The 11 upstream ids: declared, wiring status (Phase 8b)

All 11 ids are declared in `keybindings.rs` with upstream defaults. Wiring:

| Id | Status | Action |
|---|---|---|
| `app.approval.approve` | WIRED | `ExtensionUi::approve_active`: answers a pending confirm with `true`, or a select with its first Approve/Allow/Yes/Accept option; any other dialog gets a notice instead of an answer. |
| `app.ask.hide` | WIRED | `ExtensionUi::toggle_hidden`: the dialog stays pending but stops rendering and owning keys until the key is pressed again; the next dialog always starts visible. |
| `app.stt.pushToTalk` | WIRED (notice) | With `stt.enabled` on and an empty editor, Space prints `speech-to-text is not available ...`; omp has no STT backend. With `stt.enabled` off Space types normally, as upstream. |
| `app.ask.note` | NOT WIRED | Needs per-option notes in the ask dialog (`AskAnswer` has no note field). |
| `app.btw.inject` | NOT WIRED | No btw panel overlay in the TUI to inject from. |
| `app.links.hint` | NOT WIRED | No link-hint overlay. |
| `app.local.open` | NOT WIRED | omp has no session `local://` store (see `app.rs` image-attach note). |
| `app.session.pin` / `app.session.unpin` | NOT WIRED | The sessions overlay has a `pinned` field but no host pin-store hook. |
| `app.tree.nextBranch` / `app.tree.previousBranch` | NOT WIRED | The tree overlay lists one branch path and has no branch-point navigation. |

### Two ids present here but absent upstream

- `app.tree.foldOrUp` (`ctrl+left`, `alt+left`)
- `app.tree.unfoldOrDown` (`ctrl+right`, `alt+right`)

These are Rust-invented. That makes the arithmetic `79 - 11 + 2 = 70` and it is
also a real divergence: upstream folds the same keys into `app.tree.*Branch`
semantics, so a `keybindings.yml` written against `omp` does not map 1:1, and
`get_resolved_bindings()` cannot be diffed against TS `getResolvedBindings()`
without translating these two ids. Decide which side is canonical before the
tree-overlay port; the cheap answer is to keep both pairs one release and
retire the Rust-only pair once `nextBranch`/`previousBranch` land.

The NOT WIRED rows each need a missing UI component, not just a table entry.

---

## 2. Overlays — 12/76

`crates/omp-tui/src/overlays/` holds 12 files / 12,776 lines: `mod.rs` (the
focus stack) plus 11 modules, of which `settings_defs.rs` (3,818) is data
rather than a focus-owning overlay. **So 10 real overlays**: `command_palette`,
`diff`, `help`, `large_paste`, `models`, `session_info`, `sessions`,
`settings`, `thinking`, `tree`.

Upstream `overlays/` holds 74 `*.ts` files (76 per the gap analysis, which
also counts the `extensions/` subdirectory's 9 files separately —
`extension-dashboard`, `extension-list`, `mcp-runtime`, `inspector-panel`,
`inspector-model`, `live-tool-session`, `state-manager`, …).

Coverage is by *capability*, not by *module*: the Rust side merges several
upstream overlays into one. `/model` → `models.rs` (2,277) absorbs
`model-browser`/`model-picker`/`model-selector`/`model-hub` partially;
`/settings` → `settings.rs` (2,876) + `settings_defs.rs` (3,818);
session browsing → `sessions.rs` (1,154) + `tree.rs` (386). Even crediting
merges generously, roughly 60 upstream overlays have no Rust surface.

### Highest-value gaps (by upstream size)

| Upstream overlay | Lines | Nearest existing Rust state |
|---|---|---|
| `model-hub.ts` | 4,218 | `overlays/models.rs` |
| `plan-review-overlay.ts` | 2,602 | none |
| `agent-hub.ts` + `agents-hub.ts` | 2,694 + 1,862 | `tools/task.rs` |
| `session-selector.ts` | 2,353 | `overlays/sessions.rs` |
| `annotation-overlay.ts` | 2,040 | none |
| `mcp-add-wizard.ts` | 1,912 | `overlays/settings.rs` (MCP page) |
| `usage-dashboard.ts` | 1,875 | `status_line/metrics.rs` |
| `ask-dialog.ts` | 1,561 | `extension_ui.rs` |
| `copy-selector.ts` | 1,244 | `clipboard.rs` |

Also entirely absent: `theme-selector`, `login-dialog`, `oauth-selector`,
`jobs-panel`, `job-tree`, `btw-*` (5 files), `omfg-panel`, `plugin-selector`,
`plugin-settings`, `rewind-selector`, `hook-editor`/`hook-input`/
`hook-selector`, `link-browser`, `queue-mode-selector`, `pause-screen`,
`spotlight-sheet`, `context-viewer*` (3 files), `history-search`,
`error-banner`, `move-overlay`, `reset-usage-selector`, `agent-activity`,
`agent-transcript-viewer`, `running-subagent-badge`, `codex-reset-fireworks`,
and the whole `extensions/` subdirectory.

Ordering note: `model-hub` and `agents-hub` are the two largest gaps that map
onto state which already exists in `overlays/models.rs` and `tools/task.rs`,
so they are the cheapest large wins. `plan-review`, `annotation-overlay` and
`ask-dialog` need new state, not just a new view.

---

## 3. Terminal capability matrix — absent

Upstream: `/tmp/oh-my-pi-ro/packages/tui/src/terminal-capabilities.ts`,
1,602 lines, one module deciding sync output, kitty keyboard support, sixel,
graphics budget, notification channels, hyperlinks, and font support — the
single source of truth every renderer feature consults.

omp-tui has no counterpart. There is no `capabilities.rs`; the logic is
scattered across `render.rs`, `images.rs` (473 lines) and
`chrome/notify.rs` (532), each carrying its own private subset of the
decisions. `render.rs` states the gap in-line (lines 153-154): sync output is a
*static* gate, and "runtime DECRQM adjustment is a P1 item"; today
`render.rs` only records `Multiplexer::detect()` (Herdr/Tmux/Screen/Zellij/
Cmux) and a kitty-keyboard probe.

Consequence: capability decisions cannot be unit-tested in isolation, and each
new renderer feature re-implements the probing it needs. Extracting one
`terminal_capabilities.rs` is the smallest high-leverage TUI change available —
it gates sync output, kitty keys, sixel, image budget, notifications and
hyperlinks at once.

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

1. **Add the 11 missing keybinding ids** (one patch, except
   `app.stt.pushToTalk` which needs the gesture). Decide the
   `foldOrUp`/`unfoldOrDown` divergence in the same patch.
2. **Extract `terminal_capabilities.rs`** from the scattered logic, keeping the
   existing static behaviour as the initial implementation and adding DECRQM
   runtime adjustment behind it. This is the dependency for everything else.
3. **Re-enable `ext_developer` and `new_clears` in the existing `tui-e2e` job**
   in `.github/workflows/ci.yml` before porting more surface — that job already
   runs `tests-e2e/tui/run.py` over the 22 green scenarios; the remaining work is
   closing the dev-role flow (`ext_developer`) and the timing flake
   (`new_clears`) so both can join the `--only` set. Then the whole 24-scenario
   harness is a regression gate instead of a manual claim.
4. **Port overlays by user-facing frequency**, starting with `model-hub` and
   `agents-hub` (existing state to render), then `usage-dashboard` and
   `session-selector` (also existing state). Defer `plan-review`,
   `annotation-overlay` and the `apps/` tree — new state, no regression if
   absent.

## References

- `docs/research/18-omp-tui-gap-analysis.md` — full crate audit, measured LOC,
  charts/HTML-forms/apps gaps, omp2 architectural mismatch.
- `crates/omp-tui/src/keybindings.rs` — 70-id table, canonicalization, loader.
- `crates/omp-tui/src/overlays/mod.rs` — focus stack and `centered()` popup rule.
- `crates/omp-tui/src/render.rs:153` — the static capability gate and the
  DECRQM P1 note.
- `tests-e2e/tui/run.py:786` — `SCENARIOS`.
