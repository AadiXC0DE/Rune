#!/bin/sh
# Checks for the Makefile. Run from the project root: sh tests/visible.sh
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

p=$work/p
mkdir "$p"
cp -R "$root/Makefile" "$root/include" "$root/src" "$p"
cd "$p" || exit 1

make >/dev/null 2>&1
expect "a.out content" "$(cat out/a.out)" "Copyright notice
alpha body"
expect "c.out content" "$(cat out/c.out)" "charlie body"
expect "index lists outputs" "$(cat out/index.txt)" "a.out
b.out
c.out"

second=$(make 2>&1)
case $second in
  *"Nothing to be done"*) ;;
  *) echo "FAIL second make should do nothing, got: $second"; failures=$((failures + 1)) ;;
esac

[ "$failures" -eq 0 ] || exit 1
echo "visible checks passed"
