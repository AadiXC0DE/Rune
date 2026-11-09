#!/usr/bin/env bash
# The checks CI no longer runs on every push, run here before one.
#
#   scripts/preflight.sh          format, lint, budget, demo module, tests
#   scripts/preflight.sh --quick  everything but the workspace test suite
#
# On macOS a few PTY tests fail that pass on Linux CI; compare the workspace
# suite's failures with the list in PLAN.md.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL=0

step() { printf '\n== %s\n' "$*"; }

step "format"
cargo fmt --all -- --check

step "lint"
cargo clippy --workspace --all-targets -- -D warnings

step "budget (size, startup, request floor, duplicate dependencies)"
cargo xtask budget

step "demo module matches its source"
cargo xtask verify-web

if command -v cargo-deny >/dev/null 2>&1; then
    step "dependency policy"
    cargo deny check
else
    step "dependency policy skipped: cargo-deny is not installed (CI runs it when dependencies change)"
fi

if [[ "${1:-}" != "--quick" ]]; then
    step "workspace tests"
    cargo test --workspace --no-fail-fast
fi

step "preflight passed"
