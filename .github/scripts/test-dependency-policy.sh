#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT

# A standalone workspace keeps fixture dependencies out of Rune's lockfile.
mkdir -p "$scratch/src" "$scratch/dependency/src"
cat > "$scratch/Cargo.toml" <<'EOF'
[workspace]

[package]
name = "dependency-policy-fixture"
version = "0.0.0"
edition = "2024"
license = "Apache-2.0"

[dependencies]
policy-test-dependency = { path = "dependency", version = "0.0.0" }
EOF
touch "$scratch/src/lib.rs" "$scratch/dependency/src/lib.rs"

write_dependency() {
    cat > "$scratch/dependency/Cargo.toml" <<EOF
[package]
name = "policy-test-dependency"
version = "0.0.0"
edition = "2024"
license = "$1"
EOF
}

check_licenses() {
    cargo deny --manifest-path "$scratch/Cargo.toml" --offline \
        check --config "$repo_root/deny.toml" licenses
}

write_dependency MIT
check_licenses

# The trust-store exception must not allow its license on unrelated crates.
for license in GPL-3.0-only CDLA-Permissive-2.0; do
    write_dependency "$license"
    if check_licenses > "$scratch/denied.log" 2>&1; then
        echo "ERROR: dependency policy accepted a $license dependency" >&2
        exit 1
    fi

    cat "$scratch/denied.log"
    # A metadata or tool failure must not masquerade as a policy rejection.
    grep -F 'rejected: license is not explicitly allowed' "$scratch/denied.log"
    grep -F "$license" "$scratch/denied.log"
    echo "Dependency policy rejected the scratch $license dependency."
done
echo "Dependency policy test passed: MIT accepted, policy violations rejected."
