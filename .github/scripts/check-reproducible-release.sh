#!/usr/bin/env bash
set -euo pipefail

# Run from the workspace root. Only registry/source caches may be shared:
# neither invocation may reuse the other's compiled artifacts.
scratch=$(mktemp -d "${TMPDIR:-/tmp}/rune-reproducible.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

for build in 1 2; do
    CARGO_TARGET_DIR="$scratch/build-$build" cargo xtask release dev

    archives=(target/dist/*/*.tar.gz)
    if [[ ${#archives[@]} != 1 || ! -f "${archives[0]}" ]]; then
        echo 'ERROR: expected exactly one staged release archive' >&2
        exit 1
    fi
    mkdir "$scratch/release-$build"
    cp -- "${archives[0]}" "$scratch/release-$build/archive.tar.gz"
    tar -xzf "$scratch/release-$build/archive.tar.gz" -C "$scratch/release-$build"
done

hash() {
    sha256sum "$1" | cut -d' ' -f1
}

first_binary=$(hash "$scratch/release-1/rune")
second_binary=$(hash "$scratch/release-2/rune")
first_archive=$(hash "$scratch/release-1/archive.tar.gz")
second_archive=$(hash "$scratch/release-2/archive.tar.gz")
echo "binary 1  $first_binary"
echo "binary 2  $second_binary"
echo "archive 1 $first_archive"
echo "archive 2 $second_archive"

if [[ "$first_binary" != "$second_binary" ]]; then
    echo 'ERROR: independently built release binaries have different hashes' >&2
    exit 1
fi
if [[ "$first_archive" != "$second_archive" ]]; then
    echo 'ERROR: independently built release archives have different hashes' >&2
    exit 1
fi
echo 'Independent clean release builds have equal binary and archive hashes.'
