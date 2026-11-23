#!/bin/sh
# Checks for rotate.sh. Run from the project root: sh tests/visible.sh
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

listing() {
  (cd "$1" && for f in *; do [ -e "$f" ] && printf '%s,' "$f"; done)
  echo
}

expect() {
  if [ "$2" != "$3" ]; then
    echo "FAIL $1: got '$2', want '$3'"
    failures=$((failures + 1))
  fi
}

d=$work/a
mkdir "$d"
for n in 01 02 03 04; do : > "$d/app-2024-$n.log"; done
sh "$root/rotate.sh" "$d" 2
expect "keeps the newest two" "$(listing "$d")" "app-2024-03.log,app-2024-04.log,"

d=$work/b
mkdir "$d"
: > "$d/app-2024-01.log"
sh "$root/rotate.sh" "$d" 5
expect "fewer files than the limit" "$(listing "$d")" "app-2024-01.log,"

d=$work/c
mkdir "$d"
: > "$d/app-2024-01.log"
: > "$d/notes.txt"
sh "$root/rotate.sh" "$d" 0
expect "other files stay" "$(listing "$d")" "notes.txt,"

[ "$failures" -eq 0 ] || exit 1
echo "visible checks passed"
