#!/bin/sh
# Sourced by release entry points; all paths are relative to the repository.
set -eu
REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$REPO_ROOT"
export LC_ALL=C

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing build dependency: $1 (see docs/packaging.md)" >&2
        exit 1
    }
}

init_build() {
    for tool in cargo rustup python3 readelf "$CROSS_PREFIX-gcc" "$CROSS_PREFIX-ar" "$CROSS_PREFIX-strip"; do
        require_command "$tool"
    done
    python3 -c 'import yaml' || {
        echo "install scripts/requirements-build.txt before building" >&2
        exit 1
    }
    if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
        require_command git
    fi
    mkdir -p build dist
    STAGE=$(mktemp -d "$REPO_ROOT/build/.stage-$PRODUCT.XXXXXX")
    trap 'rm -rf "$STAGE"' EXIT
    trap 'exit 1' HUP INT TERM
    CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$REPO_ROOT/target}
    mkdir -p "$CARGO_TARGET_DIR"
    CARGO_TARGET_DIR=$(CDPATH= cd -- "$CARGO_TARGET_DIR" && pwd)
    export CARGO_TARGET_DIR
}

build_binary() {
    rustup target add "$RUST_TARGET"
    # Keep host build-script compiler inputs identical across the two targets;
    # changing these env vars between builds needlessly invalidates Cargo caches.
    env CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
        AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
        CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
        AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        cargo build --locked --release --target "$RUST_TARGET" --bin "$BINARY"
    cp "$CARGO_TARGET_DIR/$RUST_TARGET/release/$BINARY" "$1"
    "$CROSS_PREFIX-strip" "$1"
    chmod 755 "$1"
}

publish_stage() {
    # Only replace this product's output after compilation and validation pass.
    rm -rf "build/$PRODUCT"
    mv "$STAGE" "build/$PRODUCT"
    trap - EXIT HUP INT TERM
}
