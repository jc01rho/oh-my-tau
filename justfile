# oh-my-tau workspace command runner — the single source of truth for local gates.
# `just --list` shows every recipe; `just <recipe>` runs one.
# Keep in sync with .github/workflows/ci.yml.

set shell := ["bash", "-euo", "pipefail", "-c"]

# List available recipes.
default:
    @just --list

# ---------------------------------------------------------------------------
# Build & check
# ---------------------------------------------------------------------------

# Typecheck the whole workspace.
check:
    cargo check --workspace --locked

# ---------------------------------------------------------------------------
# Format & lint
# ---------------------------------------------------------------------------

# Check formatting without writing (CI gate).
fmt-check:
    cargo fmt --all -- --check

# Clippy the whole workspace, all targets, non-pedantic warnings denied.
lint:
    cargo clippy --workspace --all-targets --locked -- -A clippy::pedantic -D warnings

# Full pedantic clippy report (informational until pedantic backlog is cleared).
lint-pedantic:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Verify locked dependency licenses and sources against deny.toml.
deny:
    cargo deny --locked check licenses sources bans

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

# Run every workspace test, doc tests included (the CI `test` job).
test:
    cargo test --workspace --locked

# ---------------------------------------------------------------------------
# Parity harnesses
# ---------------------------------------------------------------------------

# Enforce tool-schema parity against tests-tools/ts_tools.json (fails on new regressions).
tools-parity:
    bun scripts/check-tools-parity.ts

# Enforce Rust vs TS golden parity against tests-golden/compare-baseline.txt (fails on new drift).
golden:
    bun scripts/check-golden.ts

# ---------------------------------------------------------------------------
# E2E
# ---------------------------------------------------------------------------

# Parse a real `omt --mode rpc` lane's answers with the OmOMeow gateway's zod schemas.
gateway-schema:
    cargo build -p omp-cli --locked
    cd tests-e2e && bun install --frozen-lockfile
    OMT_BIN=target/debug/omt bun tests-e2e/gateway-schema.ts

# Run the tests-e2e/tui parity scenarios against a fresh release `omt` build.
#
# Real PTY, isolated HOME, local mock provider; the TS `omp` baseline is used
# when it is on PATH, otherwise the Rust client is the default. Extra args pass
# through, e.g. `just tui-e2e --only paste_small --client rs`.
tui-e2e *args:
    CARGO_TARGET_DIR=target/shared cargo build --release -p omp-cli --locked
    client_arg=""; \
    case " {{ args }}" in \
      *--client*) ;; \
      *) command -v omp >/dev/null 2>&1 || client_arg="--client rs" ;; \
    esac; \
    python3 tests-e2e/tui/run.py {{ args }} $client_arg

# ---------------------------------------------------------------------------
# Housekeeping
# ---------------------------------------------------------------------------

# Reproduce every local gate in CI order before pushing.
ci: fmt-check lint deny check test tools-parity golden

# Run all gates including real-PTY TUI E2E (requires python3 + pyte).
ci-all: ci
    just tui-e2e --client rs
