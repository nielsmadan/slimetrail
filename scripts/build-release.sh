#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
export CARGO_TARGET_DIR="$repo_root/target"
dist="$repo_root/dist"
stage=""

cleanup() {
    if [ -n "$stage" ] && [ -d "$stage" ]; then
        rm -rf "$stage"
    fi
}
trap cleanup EXIT

ALL_TARGETS=(
    aarch64-apple-darwin
    x86_64-apple-darwin
    aarch64-unknown-linux-gnu
    x86_64-unknown-linux-gnu
)

targets=("${ALL_TARGETS[@]}")
if [ "$#" -gt 0 ]; then
    targets=("$@")
fi

for target in "${targets[@]}"; do
    supported=false
    for candidate in "${ALL_TARGETS[@]}"; do
        if [ "$target" = "$candidate" ]; then
            supported=true
            break
        fi
    done
    if [ "$supported" = false ]; then
        echo "unsupported release target: $target" >&2
        exit 1
    fi
done

if command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$@"; }
else
    sha256() { sha256sum "$@"; }
fi

built_path() {
    echo "$repo_root/target/$1/release/slimetrail"
}

installed="$(rustup target list --installed)"
for target in "${targets[@]}"; do
    if ! grep -qx "$target" <<<"$installed"; then
        echo "target $target is not installed — run: rustup target add $target" >&2
        exit 1
    fi
done

stage="$(mktemp -d "$repo_root/.dist-stage.XXXXXX")"

for target in "${targets[@]}"; do
    echo "Building $target..."
    timeout 900 cargo build --locked --release --target "$target"
done

has() {
    [[ " ${targets[*]} " == *" $1 "* ]]
}

if has aarch64-apple-darwin && has x86_64-apple-darwin; then
    echo "Combining the macOS targets into a universal binary..."
    lipo -create -output "$stage/slimetrail-macos-universal" \
        "$(built_path aarch64-apple-darwin)" \
        "$(built_path x86_64-apple-darwin)"
    lipo -info "$stage/slimetrail-macos-universal"
fi

if has aarch64-unknown-linux-gnu; then
    cp "$(built_path aarch64-unknown-linux-gnu)" "$stage/slimetrail-linux-aarch64"
fi

if has x86_64-unknown-linux-gnu; then
    cp "$(built_path x86_64-unknown-linux-gnu)" "$stage/slimetrail-linux-x86_64"
fi

shopt -s nullglob
produced=("$stage"/slimetrail-*)
if [ "${#produced[@]}" -eq 0 ]; then
    echo "no artifacts produced from: ${targets[*]}" >&2
    echo "a macOS artifact needs both apple-darwin targets (they are lipo'd together)" >&2
    exit 1
fi

chmod +x "${produced[@]}"

cd "$stage"
sha256 slimetrail-* >SHA256SUMS

old_dist="$repo_root/.dist-old.$$"
if [ -e "$dist" ]; then
    mv "$dist" "$old_dist"
fi
if mv "$stage" "$dist"; then
    stage=""
    rm -rf "$old_dist"
elif [ -e "$old_dist" ]; then
    mv "$old_dist" "$dist"
    exit 1
else
    exit 1
fi

echo ""
echo "=== $dist ==="
cat "$dist/SHA256SUMS"
