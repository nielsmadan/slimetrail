#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/.cache/tools/bin"
DOWNLOADS="$ROOT/.cache/tools/downloads"
export PATH="$BIN:$PATH"
LEFTHOOK_VERSION="2.1.15"
CARGO_DENY_VERSION="0.20.2"
CARGO_MACHETE_VERSION="0.9.2"
CARGO_LLVM_COV_VERSION="0.9.1"
RUFF_VERSION="0.16.9"
ACTIONLINT_VERSION="1.7.12"
GITLEAKS_VERSION="8.24.3"

platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Darwin/arm64)
            rust_target="aarch64-apple-darwin"
            lefthook_asset="lefthook_2.1.15_MacOS_arm64"
            lefthook_sha="f05b7a32a6a3a8fd1ac522f73b9cda3759b12a4f2d1307cca9826a05350bafae"
            deny_asset="cargo-deny-0.20.2-aarch64-apple-darwin.tar.gz"
            deny_sha="fe67d82a10d8597a3549364cb733a3f9cc1bfff9031b7ae46384a9f2a72090c3"
            machete_asset="cargo-machete-v0.9.2-aarch64-apple-darwin.tar.gz"
            machete_sha="63e28fee386d82d33f2d12406c857f98e2d4697f3f7df7f71f34dff07fca0fde"
            llvm_cov_asset="cargo-llvm-cov-aarch64-apple-darwin.tar.gz"
            llvm_cov_sha="7e821a6c90c0884f79f759f5d9be50799ade67bb82515186d922f26032ef43b8"
            ruff_asset="ruff-aarch64-apple-darwin.tar.gz"
            ruff_sha="33d35394499094cf6eb90f730dc82f11c0fab05d176378ae4f67de985ecc4146"
            actionlint_asset="actionlint_1.7.12_darwin_arm64.tar.gz"
            actionlint_sha="aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f"
            gitleaks_asset="gitleaks_8.24.3_darwin_arm64.tar.gz"
            gitleaks_sha="b90f13bb8c90ab72083d9b0c842e39dafb82c0e5c3f872f407366b7a58909013"
            ;;
        Darwin/x86_64)
            rust_target="x86_64-apple-darwin"
            lefthook_asset="lefthook_2.1.15_MacOS_x86_64"
            lefthook_sha="1fa16926902ddb4c08c7c88c5a80f48768f611296c97ee9150c421626f1c4e97"
            deny_asset="cargo-deny-0.20.2-x86_64-apple-darwin.tar.gz"
            deny_sha="248da7f581724e470071990c088ffc55c811981715f4cbdb258621fb79f8b7a6"
            machete_asset="cargo-machete-v0.9.2-x86_64-apple-darwin.tar.gz"
            machete_sha="3e8cb936c9b820f84d047a2b5be8b229b2be67bf2467014885376a6faffdcac3"
            llvm_cov_asset="cargo-llvm-cov-x86_64-apple-darwin.tar.gz"
            llvm_cov_sha="ddf949b82a63017441ea76aa068bdd7e2dde8055f47cd3ef736fdbbd633bc33e"
            ruff_asset="ruff-x86_64-apple-darwin.tar.gz"
            ruff_sha="e98ea259a021c87d3a1f8bf18639d2e32dcd45cb2ef1afcb41b13e295de1e2b3"
            actionlint_asset="actionlint_1.7.12_darwin_amd64.tar.gz"
            actionlint_sha="5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644"
            gitleaks_asset="gitleaks_8.24.3_darwin_x64.tar.gz"
            gitleaks_sha="41c44ae8ad1d6eef57d4526ad0fd67d8129eee9a856f55c2b3b9395fd3d9ec0f"
            ;;
        Linux/aarch64|Linux/arm64)
            rust_target="aarch64-unknown-linux-gnu"
            lefthook_asset="lefthook_2.1.15_Linux_aarch64"
            lefthook_sha="f3c881e1b6aa7f0461970173b1d692e0eb026bddfcd051cc87f636bf9a9782e0"
            deny_asset="cargo-deny-0.20.2-aarch64-unknown-linux-musl.tar.gz"
            deny_sha="995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d"
            machete_asset="cargo-machete-v0.9.2-aarch64-unknown-linux-gnu.tar.gz"
            machete_sha="6f96c3e6026a5bdd241b6ae600c6fb86c9197c6e189a894f91371baa01fd10f5"
            llvm_cov_asset="cargo-llvm-cov-aarch64-unknown-linux-gnu.tar.gz"
            llvm_cov_sha="abf5f13c1520f8756d2192bfaaeadb0208f5b7eaa8cadc15bf847befa42b7360"
            ruff_asset="ruff-aarch64-unknown-linux-gnu.tar.gz"
            ruff_sha="a13061e8f471b49c9d2aa284c32dd54a0e5534d702d1a44e1d7f4c875569586d"
            actionlint_asset="actionlint_1.7.12_linux_arm64.tar.gz"
            actionlint_sha="325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6"
            gitleaks_asset="gitleaks_8.24.3_linux_arm64.tar.gz"
            gitleaks_sha="5f2edbe1f49f7b920f9e06e90759947d3c5dfc16f752fb93aaafc17e9d14cf07"
            ;;
        Linux/x86_64|Linux/amd64)
            rust_target="x86_64-unknown-linux-gnu"
            lefthook_asset="lefthook_2.1.15_Linux_x86_64"
            lefthook_sha="df981b477c236546435047fb8c8ee1f6f902e17faebb1b07b6bf2165832e8fe3"
            deny_asset="cargo-deny-0.20.2-x86_64-unknown-linux-musl.tar.gz"
            deny_sha="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f"
            machete_asset="cargo-machete-v0.9.2-x86_64-unknown-linux-musl.tar.gz"
            machete_sha="48200087f54c55aabcd4db4af1e25742b49846c02a1b1bfa134711945b35b2e9"
            llvm_cov_asset="cargo-llvm-cov-x86_64-unknown-linux-gnu.tar.gz"
            llvm_cov_sha="b3f68e625481fed9b16444174f3fa5ebcdbde4a1878803a35eabe2dcefcdc41a"
            ruff_asset="ruff-x86_64-unknown-linux-gnu.tar.gz"
            ruff_sha="1bfbb819b5d4f9af501748862276b60e412d336034d99387691a4d4bce7a6f13"
            actionlint_asset="actionlint_1.7.12_linux_amd64.tar.gz"
            actionlint_sha="8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8"
            gitleaks_asset="gitleaks_8.24.3_linux_x64.tar.gz"
            gitleaks_sha="9991e0b2903da4c8f6122b5c3186448b927a5da4deef1fe45271c3793f4ee29c"
            ;;
        *)
            printf 'error: unsupported platform %s/%s\n' "$os" "$arch" >&2
            exit 1
            ;;
    esac
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

download() {
    local url="$1" destination="$2" expected="$3"
    if [[ ! -f "$destination" ]] || [[ "$(sha256 "$destination")" != "$expected" ]]; then
        curl --fail --location --retry 3 --connect-timeout 10 --max-time 180 --output "$destination.tmp" "$url"
        mv "$destination.tmp" "$destination"
    fi
    local actual
    actual="$(sha256 "$destination")"
    if [[ "$actual" != "$expected" ]]; then
        printf 'error: checksum mismatch for %s\nexpected: %s\nactual:   %s\n' \
            "$destination" "$expected" "$actual" >&2
        exit 1
    fi
}

install_binary() {
    local name="$1" asset="$2" expected="$3" url="$4"
    local archive="$DOWNLOADS/$asset"
    download "$url/$asset" "$archive" "$expected"
    if [[ "$asset" == *.tar.gz ]]; then
        local entry
        entry="$(tar -tzf "$archive" | awk -v name="$name" \
            '!found && ($0 == name || $0 ~ ("/" name "$")) { print; found = 1 }')"
        if [[ -z "$entry" ]]; then
            printf 'error: %s is absent from %s\n' "$name" "$asset" >&2
            exit 1
        fi
        tar -xOzf "$archive" "$entry" >"$BIN/$name.tmp"
    else
        cp "$archive" "$BIN/$name.tmp"
    fi
    chmod 0755 "$BIN/$name.tmp"
    mv "$BIN/$name.tmp" "$BIN/$name"
}


probe() {
    local name="$1" remedy="$2"
    shift 2
    if "$@"; then
        return 0
    fi
    printf 'missing or unusable %s; %s\n' "$name" "$remedy" >&2
    return 1
}

doctor() {
    local failed=0
    probe 'Python 3.11+' 'install Python 3.11 or newer' python3 -c 'import sys; sys.exit(sys.version_info < (3, 11))' || failed=1
    probe cargo 'install the Rust toolchain in rust-toolchain.toml' cargo --version || failed=1
    probe rustc 'install the Rust toolchain in rust-toolchain.toml' rustc --version || failed=1
    probe rustfmt 'run rustup component add rustfmt' cargo fmt --version || failed=1
    probe clippy 'run rustup component add clippy' cargo clippy --version || failed=1
    local sysroot host version llvm_remedy
    sysroot="$(rustc --print sysroot)"
    host="$(rustc -vV | sed -n 's/^host: //p')"
    version="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
    llvm_remedy="run rustup component add llvm-tools-preview --toolchain $version"
    probe llvm-cov "$llvm_remedy" "$sysroot/lib/rustlib/$host/bin/llvm-cov" --version || failed=1
    probe llvm-profdata "$llvm_remedy" "$sysroot/lib/rustlib/$host/bin/llvm-profdata" --version || failed=1
    probe timeout 'install coreutils' timeout 1 true || failed=1
    for tool in lefthook cargo-deny cargo-machete ruff actionlint gitleaks; do
        probe "$tool" 'run just setup-tools' "$BIN/$tool" --version || failed=1
    done
    probe cargo-llvm-cov 'run just setup-tools' "$BIN/cargo-llvm-cov" llvm-cov --version || failed=1
    probe hooks 'run just setup' lefthook check-install || failed=1
    return "$failed"
}

platform
cd "$ROOT"
case "${1:-}" in
    setup-tools)
        mkdir -p "$BIN" "$DOWNLOADS"
    install_binary lefthook "$lefthook_asset" "$lefthook_sha" \
        "https://github.com/evilmartians/lefthook/releases/download/v$LEFTHOOK_VERSION"
    install_binary cargo-deny "$deny_asset" "$deny_sha" \
        "https://github.com/EmbarkStudios/cargo-deny/releases/download/$CARGO_DENY_VERSION"
    install_binary cargo-machete "$machete_asset" "$machete_sha" \
        "https://github.com/bnjbvr/cargo-machete/releases/download/v$CARGO_MACHETE_VERSION"
    install_binary cargo-llvm-cov "$llvm_cov_asset" "$llvm_cov_sha" \
        "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v$CARGO_LLVM_COV_VERSION"
    install_binary ruff "$ruff_asset" "$ruff_sha" \
        "https://github.com/astral-sh/ruff/releases/download/$RUFF_VERSION"
    install_binary actionlint "$actionlint_asset" "$actionlint_sha" \
        "https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT_VERSION"
    install_binary gitleaks "$gitleaks_asset" "$gitleaks_sha" \
        "https://github.com/gitleaks/gitleaks/releases/download/v$GITLEAKS_VERSION"

        ;;
    doctor) doctor ;;
    *) printf 'usage: %s {setup-tools|doctor}\n' "$0" >&2; exit 2 ;;
esac
