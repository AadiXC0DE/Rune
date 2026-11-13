#!/bin/sh
# Checks for render.sh. Run from the project root: sh tests/visible.sh
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

expect() {
  if [ "$2" != "$3" ]; then
    echo "FAIL $1: got '$2', want '$3'"
    failures=$((failures + 1))
  fi
}

tmpl=$root/templates/app.conf.tmpl

expect "defaults only" "$(sh "$root/render.sh" "$tmpl" "$work/none.env")" "name = demo
listen = localhost:8080
url = http://localhost:8080/app
logs = /var/log/demo"

printf 'PORT=9090\nAPP_NAME=shop\n' > "$work/a.env"
expect "environment file overrides defaults" "$(sh "$root/render.sh" "$tmpl" "$work/a.env")" "name = shop
listen = localhost:9090
url = http://localhost:9090/app
logs = /var/log/shop"

printf 'GREETING=hello\nMESSAGE=${GREETING} there\n' > "$work/b.env"
printf 'say = {{MESSAGE}}\n' > "$work/say.tmpl"
expect "reference to an earlier value" "$(sh "$root/render.sh" "$work/say.tmpl" "$work/b.env")" "say = hello there"

[ "$failures" -eq 0 ] || exit 1
echo "visible checks passed"
