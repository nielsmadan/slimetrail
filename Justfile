export PATH := justfile_directory() / ".cache/tools/bin" + ":" + env_var("PATH")

[private]
default:
    @just --list

setup:
    @just setup-tools
    @timeout 900 cargo fetch --locked
    @lefthook install
    @just doctor

setup-tools:
    @timeout 900 bash scripts/tooling.sh setup-tools

doctor:
    @bash scripts/tooling.sh doctor

install:
    @timeout 900 cargo install --path . --locked --force

uninstall:
    @cargo uninstall slimetrail

test:
    @timeout 900 cargo test --locked --all-targets --all-features
    @timeout 300 python3 -B -m unittest discover -s scripts -p 'test_*.py'

lint: lint-rust lint-python

lint-rust:
    @timeout 900 cargo clippy --locked --all-targets --all-features -- -D warnings

lint-python:
    @ruff check scripts

format:
    @cargo fmt
    @ruff format scripts

format-check: format-check-rust format-check-python

format-check-rust:
    @cargo fmt --check

format-check-python:
    @ruff format --check scripts

check: format-check lint test check-secrets

dependency-policy:
    @timeout 300 cargo deny --locked check

unused-deps:
    @timeout 300 cargo machete

check-workflows:
    @actionlint

check-secrets:
    @timeout 300 python3 scripts/check_secrets.py worktree

check-staged-secrets:
    @timeout 300 python3 scripts/check_secrets.py staged

check-secret-history:
    @timeout 300 python3 scripts/check_secrets.py history

check-all: check dependency-policy unused-deps check-workflows build-docs

build-docs:
    @RUSTDOCFLAGS="-D warnings" timeout 900 cargo doc --locked --no-deps

coverage:
    @timeout 900 bash scripts/coverage.sh

build:
    @timeout 900 cargo build --release --locked

[positional-arguments]
build-release *targets:
    @timeout 900 bash scripts/build-release.sh "$@"

[positional-arguments]
release *args:
    @curtains "$@"

bench: build
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .cache/benchmarks
    timeout 900 python3 scripts/benchmark.py | tee .cache/benchmarks/synthetic.jsonl
    python3 scripts/check_benchmark.py .cache/benchmarks/synthetic.jsonl

clean:
    @cargo clean
    @rm -rf dist
