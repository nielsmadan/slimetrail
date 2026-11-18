[private]
default:
    @just --list

setup:
    @cargo fetch --locked
    @just doctor

doctor:
    @cargo --version
    @rustc --version
    @cargo fmt --version
    @cargo clippy --version

install:
    @cargo install --path . --locked --force

uninstall:
    @cargo uninstall slimetrail

test:
    @cargo test --locked --all-targets

lint:
    @cargo clippy --locked --all-targets --all-features -- -D warnings

format:
    @cargo fmt

check:
    @cargo fmt --check
    @just lint
    @just test

build-release:
    @cargo build --release --locked

clean:
    @cargo clean
