#!/usr/bin/env bash
# Builds a fully static musl release archive for one Linux target on a runner
# of that architecture: cargo xtask release runs what it builds to measure it,
# so the target is never cross-compiled here.
#
# Usage: build-static-linux.sh TARGET CHANNEL VERSION
set -euo pipefail
target="$1"
channel="$2"
version="$3"

sudo apt-get update
sudo apt-get install -y musl-tools
rustup target add "$target"
# ring compiles C code, which needs a musl C compiler.
export CC_x86_64_unknown_linux_musl=musl-gcc
export CC_aarch64_unknown_linux_musl=musl-gcc

cargo xtask release "$channel" "$version" "$target"

binary="target/$target/release/rune"
if readelf -d "$binary" | grep -q NEEDED; then
    echo "not static: $binary links shared libraries"
    readelf -d "$binary"
    exit 1
fi
stat -c "$target binary: %s bytes" "$binary"
