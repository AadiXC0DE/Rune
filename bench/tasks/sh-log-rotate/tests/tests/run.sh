#!/bin/sh
# Grading checks for rotate.sh. Run from the project root: sh tests/run.sh
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

sh "$root/tests/visible.sh" || failures=$((failures + 1))

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

d=$work/one
mkdir "$d"
for n in 01 02 03; do : > "$d/app-2024-$n.log"; done
sh "$root/rotate.sh" "$d" 1
expect "keep count 1" "$(listing "$d")" "app-2024-03.log,"

d=$work/spaces
mkdir "$d"
: > "$d/app 2024 01.log"
: > "$d/app 2024 02.log"
: > "$d/app 2024 03.log"
: > "$d/keep me.txt"
sh "$root/rotate.sh" "$d" 2
expect "names with spaces" "$(listing "$d")" "app 2024 02.log,app 2024 03.log,keep me.txt,"

d=$work/dash
mkdir "$d"
: > "$d/-2024-01.log"
: > "$d/-2024-02.log"
: > "$d/2024-03.log"
sh "$root/rotate.sh" "$d" 1
expect "leading dashes" "$(listing "$d")" "2024-03.log,"

d="$work/dir with space"
mkdir "$d"
: > "$d/a.log"
: > "$d/b.log"
sh "$root/rotate.sh" "$d" 1
expect "directory with a space" "$(listing "$d")" "b.log,"

d=$work/glob
mkdir "$d"
: > "$d/a*.log"
: > "$d/b.log"
: > "$d/c.log"
sh "$root/rotate.sh" "$d" 2
expect "glob characters in names" "$(listing "$d")" "b.log,c.log,"

d=$work/empty
mkdir "$d"
sh "$root/rotate.sh" "$d" 3
expect "no logs at all" "$(listing "$d")" ""

d=$work/dirlog
mkdir "$d" "$d/old.log"
: > "$d/a.log"
: > "$d/b.log"
sh "$root/rotate.sh" "$d" 1
expect "a directory named like a log is left alone" "$(listing "$d")" "b.log,old.log,"

if sh "$root/rotate.sh" "$work/missing" 1 2>/dev/null; then
  echo "FAIL missing directory should fail"
  failures=$((failures + 1))
fi

[ "$failures" -eq 0 ] || exit 1
echo "all checks passed"
