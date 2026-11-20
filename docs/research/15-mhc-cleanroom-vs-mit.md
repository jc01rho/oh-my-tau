<analysis>
**Literal Request**: Read license/provenance files from oh-my-tau and oh-my-pi, compare against mhc's SUL->MIT cleanroom constraints, recommend applicable mechanisms.
**Actual Need**: A decision on what compliance apparatus oh-my-tau must carry, given it is already MIT with MIT-only upstreams - and explicitly what it can skip.
**Success Looks Like**: An evidence-backed apply/skip table plus the gaps found in oh-my-tau today.
</analysis>

## Evidence: what the files actually say

**oh-my-tau (omt)** - `/home/whrho/git/oh-my-tau/`
- `LICENSE`: plain MIT, "Copyright (c) 2026 oh my tau contributors".
- `NOTICE.md`: omt is MIT; it is a Rust port of oh-my-pi (MIT, Zechner / Boeluek / Stencil Labs), reference TS tree at `vendor-src/oh-my-pi/` which is **gitignored, not distributed**; `vendor/LICENSE-oh-my-pi` and `vendor/LICENSE-prime-agent` vendored; vendored crates `pa-*` (prime-agent, **modified**, MIT), `pi-*` (oh-my-pi, **verbatim**, MIT), `crossterm` fork (MIT), `laya-ort` (first-party, MIT).
- `.gitignore`: `vendor-src/` is ignored - upstream TS source is a build/oracle input only, never shipped.
- `Cargo.toml`: `license = "MIT"` at workspace level, inherited by all `crates/*` and `vendor/pa-*`.
- **Absent**: no `deny.toml`, no `about.toml`, no `THIRD-PARTY-NOTICES.txt`, no `provenance.json`, no `*.NOTICE` per-crate files, no SPDX headers (0 files), and `.github/workflows/{ci,release}.yml` contain **no** `cargo-deny`/`cargo-about`/license step.
- Parity apparatus that **does** exist: `tests-golden/` (`capture.ts`, `compare.ts`, `ts/` + `rs/` jsonl pairs for 12 scenarios, `compare-baseline.txt` = recorded shape diffs), `scripts/compat-oracle/` (TS ground truth for `pa-ai` policy resolution and request bodies, consumed by `compat::oracle` / `body_parity_tests` / `transport_parity_tests`), `scripts/sync-catalog.sh` (regenerates compat rules + catalog from a `vendor-src` bump, rules hash feeds the materialization policy).

**oh-my-pi (omp)** - `/tmp/oh-my-pi-omp2/`
- `LICENSE`: MIT, Stencil Labs 2026.
- `THIRD-PARTY-NOTICES.txt`: self-contained release payload; MIT default; **package-scoped exceptions** for MPL-2.0 (`bitmaps`, `im`, `option-ext`, `sized-chunks`), WTFPL (`terminfo`), `tree-sitter-graphql` MIT clarification with a pinned LICENSE SHA-256; **excluded material** (`login/provenance.json` for 90 provider logos, `vendor-schemas/provenance.json` for 21 Cursor/Devin schemas) held out of the MIT grant pending rights review; per-crate `NOTICE` files concatenated.
- `about.toml` (cargo-about 0.8.2): permissive allowlist + per-package license overrides + checksum-pinned clarification file.
- `deny.toml` (cargo-deny 0.20.2): same allowlist, `unused-allowed-license = "deny"`, `unused-license-exception = "deny"`, package+version-scoped exceptions, `unknown-registry`/`unknown-git = "deny"`, single allowlisted git source.

**mhc** - `/home/whrho/.local/share/mhc/versions/2026.11.19-omomeow1/`
- `licenses/SUL-1.0.md`: Sustainable Use License - internal/non-commercial use only, no paid distribution, no notice removal, patent termination clause. **Not an open-source license.**
- `skills/*/CLEANROOM.md`: the cleanroom record format - "Inputs used" (delta spec path + MIT base + pinned upstream commit SHA), "Not read" (explicit statement that no SUL source was opened/grepped/diffed), "Changes vs base", "Requirement checklist" table, license/authorship statement. The `onboarding` record pins mhc commit `13699780` and the lazycodex base commit `75b017473c71d669dd9aed0a10e950e70724b283`.

## Constraint comparison

| Dimension | mhc | oh-my-tau |
| --- | --- | --- |
| Outbound license | SUL-1.0 (source-available, non-commercial) | MIT |
| Upstream license | SUL-1.0 assets + MIT lazycodex base | MIT (oh-my-pi) + MIT (prime-agent) + MIT (crossterm) |
| Contamination risk | High - SUL forbids derivative reuse outside its terms, so re-implementation must be provably independent | **None** - MIT-to-MIT derivation is explicitly permitted with attribution |
| What "cleanroom" buys | Legal defensibility: proves no SUL text was copied | Nothing - there is no restrictive license to be contaminated by |
| Real obligation | Prove independent authorship | Preserve copyright notices + state modifications |

The cleanroom in mhc is not a quality practice, it is a **legal firewall against a non-open-source license**. MIT grants "use, copy, modify, merge, publish, distribute, sublicense" with the sole condition of notice retention, so a direct port is the *licensed* path, not a risk to be engineered around.

## Recommendation

**Apply (keep / adopt):**

1. **Upstream pinning** - adopt, and tighten it. mhc's cleanroom records pin upstream commit SHAs; oh-my-tau currently pins only *versions* (`pi-*` at 18.9.1 in `Cargo.lock`) and describes `vendor-src` as "a newer checkout" with no recorded SHA. Record the exact oh-my-pi commit that `tests-golden/` and `scripts/compat-oracle/` fixtures were captured against, in `NOTICE.md` or a `vendor-src.lock`. Without it, `compare-baseline.txt` diffs are unattributable and `sync-catalog.sh` output is unreproducible. This is the single highest-value mechanism to carry over.
2. **Parity ledger** - already present in substance (`compare-baseline.txt` + compat-oracle fixtures), just not named as a ledger. Promote it: one tracked file listing each scenario/fixture, the upstream SHA it was captured from, and its accepted diff set. mhc's "Requirement checklist" table is the same artifact shape.
3. **Golden tests** - already the strongest part of the repo; keep as-is. `tests-golden/{ts,rs}` + `scripts/compat-oracle` are exactly the right parity harness and need no licensing justification.
4. **Delta specs** - apply in reduced form. mhc uses a delta spec because it cannot read the SUL source; oh-my-tau *can* read upstream, so the analogue is a per-port note stating "what differs from upstream and why" (e.g. `pa-models/README.md` already does this for the catalog client; `pi-predict` vendors only the n-gram engine). Keep these as porting notes, not as cleanroom records.

**Do not apply:**

1. **Cleanroom isolation** - drop entirely. No "Not read" clause, no segregated author, no prohibition on reading `vendor-src/oh-my-pi`. Reading and porting the MIT upstream is the licensed activity; walling it off would only degrade fidelity and defeat the parity harness.
2. **Feature-gating the SUL portion** - drop entirely. There is no SUL code in the dependency graph; there is nothing to gate, quarantine, or exclude from the MIT grant.
3. **`CLEANROOM.md` per skill/component** - drop. Its inputs (delta spec + MIT base + "not read" attestation) have no referent here.

**Gaps worth closing (independent of mhc, driven by omp's own precedent):**

- `NOTICE.md` says `pa-*` crates are "**modified**". MIT requires that modifications be conveyed and that upstream notices be retained - retention is satisfied via `vendor/LICENSE-prime-agent`, but there is no record of *which* files changed. omp solved this class of problem with per-crate `NOTICE` files; a short `vendor/pa-*/NOTICE` (or a diff summary in `NOTICE.md`) closes it cheaply.
- No `deny.toml` / `about.toml` and no license gate in CI. Given `unsafe_code = "forbid"` and a pedantic-clean policy already exist in `Cargo.toml`, a `deny.toml` ported from omp's (same permissive allowlist, `unused-allowed-license = "deny"`, package-scoped exceptions) is a small, high-leverage addition. omp's file is a directly reusable template.
- `laya-ort` is first-party but ships ONNX runtime bindings; the 1.3 GB weights are gitignored as build artifacts (correct), but the runtime's own license should be named in `NOTICE.md` alongside the crate.

<results>
<files>
- /home/whrho/git/oh-my-tau/LICENSE - MIT, "oh my tau contributors"; the outbound grant, no restrictions beyond notice retention
- /home/whrho/git/oh-my-tau/NOTICE.md - the provenance document: omp port, pa-* modified, pi-* verbatim, crossterm fork, laya-ort; states vendor-src is gitignored and undistributed
- /home/whrho/git/oh-my-tau/.gitignore - confirms `vendor-src/` (upstream TS) and `crates/omp-judge/onnx/*.onnx` are never distributed
- /home/whrho/git/oh-my-tau/Cargo.toml - workspace `license = "MIT"`, `unsafe_code = "forbid"`, pedantic lint gate, crossterm patch; no license tooling config
- /home/whrho/git/oh-my-tau/vendor/LICENSE-oh-my-pi - MIT, Zechner / Boeluek / Stencil Labs
- /home/whrho/git/oh-my-tau/vendor/LICENSE-prime-agent - MIT, Prime Intellect 2025-2026, plus the "port of Prime Agent TypeScript, originally (c) 2025 Mario Zechner" credit
- /home/whrho/git/oh-my-tau/tests-golden/compare.ts - the parity differ (shape-collapse + path->type diff) driving ts/ vs rs/ comparison
- /home/whrho/git/oh-my-tau/tests-golden/compare-baseline.txt - the accepted-diff ledger in practice (347+ lines of recorded ts-only/rs-only shape deltas)
- /home/whrho/git/oh-my-tau/tests-golden/{ts,rs}/ - 12 paired scenario captures (state, prompt, compact, slash, steer_follow_up, ...)
- /home/whrho/git/oh-my-tau/scripts/compat-oracle/README.md - TS ground-truth oracle for pa-ai policy resolution and request bodies; documents the `cargo test -p pa-ai -- compat::oracle body_parity transport_parity` consumers
- /home/whrho/git/oh-my-tau/scripts/sync-catalog.sh - regenerates catalog + compat rules from a vendor-src bump; rules hash feeds the materialization policy
- /home/whrho/git/oh-my-tau/README.md - lines 218-251 document the vendor-src workflow and the MIT/NOTICE licensing posture
- /home/whrho/git/oh-my-tau/.github/workflows/ci.yml - no cargo-deny / cargo-about / license step
- /home/whrho/git/oh-my-tau/.github/workflows/release.yml - no license or notice generation step
- /tmp/oh-my-pi-omp2/LICENSE - MIT, Stencil Labs 2026
- /tmp/oh-my-pi-omp2/THIRD-PARTY-NOTICES.txt - self-contained release payload: MIT default, MPL-2.0/WTFPL package-scoped exceptions, excluded logos + vendor-schemas with provenance.json, per-crate NOTICE concatenation
- /tmp/oh-my-pi-omp2/about.toml - cargo-about 0.8.2 allowlist, per-package MPL-2.0/WTFPL overrides, checksum-pinned tree-sitter-graphql clarification
- /tmp/oh-my-pi-omp2/deny.toml - cargo-deny 0.20.2: allowlist, unused-allowed-license/exception = deny, package+version-scoped exceptions, unknown-registry/unknown-git deny
- /home/whrho/.local/share/mhc/versions/2026.11.19-omomeow1/licenses/SUL-1.0.md - the Sustainable Use License text (internal/non-commercial only, no paid distribution, no notice removal, patent termination)
- /home/whrho/.local/share/mhc/versions/2026.11.19-omomeow1/skills/refactor/CLEANROOM.md - cleanroom record template: inputs (delta spec + MIT base + pinned SHA), "Not read" attestation, changes vs base, requirement checklist
- /home/whrho/.local/share/mhc/versions/2026.11.19-omomeow1/skills/onboarding/CLEANROOM.md - fuller example pinning mhc commit 13699780 and base commit 75b017473c71d669dd9aed0a10e950e70724b283
- /home/whrho/.local/share/mhc/versions/2026.11.19-omomeow1/licenses/THIRD_PARTY_LICENSES.md - mhc's generated dependency license inventory
</files>

<answer>
The two projects face structurally different problems, and copying mhc's cleanroom wholesale would be cargo-culting.

mhc's source is SUL-1.0 - source-available, internal/non-commercial use only, no paid distribution, no notice removal, with a patent-termination clause. Because SUL is not an open-source license, re-implementing any of its code under MIT is legally unsafe unless authorship can be proven independent of the SUL text. That is the entire purpose of the `CLEANROOM.md` records: a delta spec plus a signed "I never opened the SUL source" attestation. Feature-gating the SUL portion is the same instinct - quarantine the encumbered code so the MIT grant stays clean.

oh-my-tau has no such encumbrance. Its LICENSE is MIT, its primary upstream (oh-my-pi) is MIT, its secondary upstream (prime-agent) is MIT, and the crossterm fork is MIT. MIT expressly grants the right to use, copy, modify, merge, publish, distribute and sublicense, conditioned only on retaining the copyright and permission notice. A direct port is therefore the licensed path - the exact opposite of a risk to be engineered around. There is no SUL code anywhere in the graph, so there is nothing to isolate, gate, or exclude.

What oh-my-tau should carry over is the engineering discipline that happens to sit next to mhc's legal discipline:

Keep pinning - and fix it. mhc pins upstream commit SHAs because provenance matters. oh-my-tau pins versions only (`pi-*` at 18.9.1 in Cargo.lock) and describes `vendor-src` as "a newer checkout" with no SHA anywhere in README, NOTICE, scripts, or CI. That is the one real defect. Without a recorded oh-my-pi commit, `compare-baseline.txt` deltas cannot be attributed to an upstream revision and `sync-catalog.sh` output is not reproducible.

Keep the parity ledger and golden tests - they already exist and are the repo's strongest asset. `tests-golden/{ts,rs}` with `compare.ts` plus `scripts/compat-oracle` (feeding `compat::oracle`, `body_parity_tests`, `transport_parity_tests`) is a better parity harness than anything mhc has; it just needs a named ledger file tying each fixture to its upstream SHA.

Keep delta specs, in reduced form - "what differs from upstream and why" as porting notes (`pa-models/README.md` and the pi-predict scoping note already do this), not as cleanroom records.

Drop cleanroom isolation, drop SUL feature-gating, drop per-component CLEANROOM.md. None has a referent.

Separately, and not because of mhc: NOTICE.md says the `pa-*` crates are "modified" but never says which files changed - MIT obliges you to convey modifications, and omp's per-crate NOTICE convention is the cheap fix. And there is no `deny.toml`/`about.toml` and no license gate in either CI workflow, despite the repo already enforcing `unsafe_code = "forbid"` and a pedantic lint bar; omp's `deny.toml` is a drop-in template.
</answer>

<next_steps>
1. Record the exact oh-my-pi commit SHA that `tests-golden/` and `scripts/compat-oracle` fixtures were captured against - in `NOTICE.md` or a new `vendor-src.lock`. This is the only genuine gap the comparison surfaced.
2. Promote `tests-golden/compare-baseline.txt` into an explicit parity ledger: scenario, upstream SHA, accepted diff set.
3. Port `/tmp/oh-my-pi-omp2/deny.toml` into oh-my-tau and wire a `cargo deny check licenses` step into `.github/workflows/ci.yml`.
4. Add `vendor/pa-*/NOTICE` (or a diff summary section in `NOTICE.md`) naming the modifications to the prime-agent fork.
5. Do not add cleanroom records or SUL gating - document the MIT-to-MIT reasoning once (NOTICE.md already nearly does) and move on.
</next_steps>
</results>