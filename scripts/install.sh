#!/bin/sh
# Installs the rune binary from a published release, after checking it against
# the release's SHA256SUMS.
#
#   curl -fsSL https://rune.heyaadi.com/install.sh | sh
#   sh install.sh --version 0.2.0 --dir ~/bin
#
# Never uses sudo: the binary goes to a directory you can write.
set -eu

base_url=${RUNE_INSTALL_BASE_URL:-https://github.com/AadiXC0DE/Rune/releases}
version=
dir=${RUNE_INSTALL_DIR:-}

fail() {
    printf 'rune: %s\n' "$1" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Installs rune from a published release, after checking its checksum.

Usage: install.sh [--version <version>] [--dir <path>]

  --version <version>  install this release (default: the latest)
  --dir <path>         install into this directory (default: $RUNE_INSTALL_DIR,
                       then $HOME/.local/bin)
  --help               show this help
EOF
}

while [ $# -gt 0 ]; do
    case $1 in
        --version)
            [ $# -ge 2 ] || fail "--version needs a value"
            version=$2
            shift 2
            ;;
        --version=*)
            version=${1#--version=}
            shift
            ;;
        --dir)
            [ $# -ge 2 ] || fail "--dir needs a value"
            dir=$2
            shift 2
            ;;
        --dir=*)
            dir=${1#--dir=}
            shift
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        *)
            fail "unknown option $1; see install.sh --help"
            ;;
    esac
done

if [ -z "$dir" ]; then
    [ -n "${HOME:-}" ] || fail "set HOME or pass --dir"
    dir=$HOME/.local/bin
fi

os=${RUNE_INSTALL_OS:-$(uname -s)}
arch=${RUNE_INSTALL_ARCH:-$(uname -m)}
case $os in
    Darwin)
        case $arch in
            arm64 | aarch64) target=aarch64-apple-darwin ;;
            x86_64)
                # A shell under Rosetta reports x86_64 on an Apple silicon Mac.
                if [ -z "${RUNE_INSTALL_ARCH:-}" ] &&
                    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                    target=aarch64-apple-darwin
                else
                    target=x86_64-apple-darwin
                fi
                ;;
            *) target= ;;
        esac
        ;;
    Linux)
        case $arch in
            x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
            aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
            *) target= ;;
        esac
        ;;
    *) target= ;;
esac
[ -n "$target" ] || fail "no release for $os $arch; see https://github.com/AadiXC0DE/Rune#install"

# The default address is fetched over HTTPS only, so a redirect cannot move the
# download to plain HTTP. A test server set through RUNE_INSTALL_BASE_URL is
# loopback HTTP.
if [ -n "${RUNE_INSTALL_BASE_URL:-}" ]; then
    curl_secure=
    wget_secure=
else
    curl_secure="--proto =https --tlsv1.2"
    wget_secure=--https-only
fi

if command -v curl >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    fetch() { curl -fsSL $curl_secure -o "$2" "$1"; }
    # shellcheck disable=SC2086
    resolve() { curl -fsSL $curl_secure -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    fetch() { wget -q $wget_secure -O "$2" "$1"; }
    # shellcheck disable=SC2086
    resolve() {
        wget -q $wget_secure -S -O /dev/null "$1" 2>&1 |
            sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r'
    }
else
    fail "curl or wget is required"
fi

if [ -z "$version" ]; then
    latest=$(resolve "$base_url/latest") || fail "could not find the latest release at $base_url/latest"
    version=${latest##*/}
    [ -n "$version" ] || fail "could not find the latest release at $base_url/latest"
fi
version=${version#v}

archive=rune-$version-$target.tar.gz
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fetch "$base_url/download/v$version/$archive" "$work/$archive" ||
    fail "could not download $archive for version $version"
fetch "$base_url/download/v$version/SHA256SUMS" "$work/SHA256SUMS" ||
    fail "could not download SHA256SUMS for version $version"

expected=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; exit }' "$work/SHA256SUMS")
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work/$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$work/$archive" | awk '{ print $1 }')
else
    fail "sha256sum or shasum is required to check the download"
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    fail "checksum mismatch for $archive; nothing was installed"
fi

mkdir -p "$work/unpacked"
tar -xzf "$work/$archive" -C "$work/unpacked" || fail "could not unpack $archive"
[ -f "$work/unpacked/rune" ] || fail "$archive holds no rune binary; nothing was installed"

writable() { mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; }
writable || fail "cannot write to $dir; choose a directory you own with --dir or RUNE_INSTALL_DIR"
staged=$dir/.rune.new.$$
cp "$work/unpacked/rune" "$staged" ||
    fail "cannot write to $dir; choose a directory you own with --dir or RUNE_INSTALL_DIR"
chmod 755 "$staged"
mv -f "$staged" "$dir/rune"

printf 'installed rune %s to %s/rune\n' "$version" "$dir"
case ":${PATH:-}:" in
    *":$dir:"*) ;;
    *) printf "add %s to PATH, for example: echo 'export PATH=\"%s:\$PATH\"' >> ~/.profile\n" "$dir" "$dir" ;;
esac
printf 'next: rune connect\n'
printf 'uninstall: rune uninstall\n'
