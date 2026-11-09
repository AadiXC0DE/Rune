#!/usr/bin/env bash
# The checks CI no longer runs on every push, run here before one.
#
#   scripts/preflight.sh          format, lint, budget, demo module, tests
#   scripts/preflight.sh --quick  everything but the workspace test suite
#
# The workspace suite on macOS has known PTY and MCP harness failures (see
# PLAN.md); compare its failures against that list. CI's Linux and Windows
# jobs remain the judge for those suites.
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
