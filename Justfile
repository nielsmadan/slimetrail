check:
    cargo fmt --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked --all-targets

release:
    cargo build --release --locked

install prefix:
    cargo install --path . --locked --root {{quote(prefix)}}
