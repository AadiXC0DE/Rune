#!/usr/bin/env bash
# Proves a static Linux archive runs on distributions it was not built on:
# --version, then one scripted end-to-end run, inside each image.
#
# Usage: test-static-linux.sh ARCHIVE
set -euo pipefail
archive="$1"
scenario="crates/rune/tests/scenarios/fix_one_file.json"
images=(alpine:latest debian:stable-slim)

rm -rf dist
mkdir -p dist
tar -xzf "$archive" -C dist
test -x dist/rune

# Build the scripted provider once, outside the timed part.
cargo build --locked -q -p rune-testkit --example scripted_server

for image in "${images[@]}"; do
    echo "== $image"
    version=$(docker run --rm -v "$PWD/dist:/opt/rune:ro" "$image" /opt/rune/rune --version)
    case "$version" in
        "rune "*) echo "$version" ;;
        *) echo "unexpected --version output: $version"; exit 1 ;;
    esac

    work=$(mktemp -d)
    # The server serves until its standard input closes; a FIFO held open on
    # descriptor 3 is that input.
    fifo="$(mktemp -u)"
    mkfifo "$fifo"
    out="$(mktemp)"
    cargo run --locked -q -p rune-testkit --example scripted_server -- \
        "$scenario" --workspace "$work" < "$fifo" > "$out" &
    server_pid=$!
    exec 3> "$fifo"
    for _ in $(seq 1 300); do
        [ -s "$out" ] && break
        sleep 0.1
    done
    line=$(head -n 1 "$out")
    port=$(printf '%s' "$line" | python3 -c 'import json,sys; print(json.load(sys.stdin)["port"])')
    prompt=$(printf '%s' "$line" | python3 -c 'import json,sys; print(json.load(sys.stdin)["prompt"])')

    status=0
    docker run --rm --network host -v "$work:/work" -w /work \
        -e HOME=/tmp/h -e RUNE_HOME=/tmp/h/state \
        -e RUNE_PROVIDER=chat_completions \
        -e "RUNE_BASE_URL=http://127.0.0.1:$port/v1" \
        -e RUNE_MODEL=scripted -e RUNE_API_KEY_ENV=SCRIPTED_KEY -e SCRIPTED_KEY=x \
        -v "$PWD/dist:/opt/rune:ro" "$image" \
        /opt/rune/rune --permission-mode full-access --allow-unsandboxed \
        ask --json --no-save -- "$prompt" || status=$?

    exec 3>&-
    wait "$server_pid" || true
    rm -f "$fifo" "$out"
    if [ "$status" -ne 0 ]; then
        echo "the scripted run in $image exited $status"
        exit 1
    fi
    cargo run --locked -q -p rune-testkit --example scripted_server -- \
        --check "$scenario" --workspace "$work"
    sudo rm -rf "$work"
done

for image in "${images[@]}"; do
    docker image rm "$image" > /dev/null || true
done
echo "static archive runs on: ${images[*]}"
