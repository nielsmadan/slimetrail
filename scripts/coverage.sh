#!/usr/bin/env bash
set -euo pipefail
cleanup() {
    local status=$?
    trap - EXIT
    cargo llvm-cov clean --workspace --profraw-only >/dev/null 2>&1 || true
    exit "$status"
}
trap cleanup EXIT
cargo llvm-cov clean --workspace
cargo llvm-cov --locked --all-targets --all-features --no-report
mkdir -p .cache/coverage
cargo llvm-cov report --summary-only | tee .cache/coverage/summary.txt
cargo llvm-cov report --html --output-dir .cache/coverage/html
cargo llvm-cov report --lcov --output-path .cache/coverage/lcov.info
