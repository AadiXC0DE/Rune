#!/bin/sh
# Grading checks for the Makefile. Run from the project root: sh tests/run.sh
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

sh "$root/tests/visible.sh" || failures=$((failures + 1))

expect() {
  if [ "$2" != "$3" ]; then
    echo "FAIL $1: got '$2', want '$3'"
    failures=$((failures + 1))
  fi
}

fresh() {
  rm -rf "$work/p"
  mkdir "$work/p"
  cp -R "$root/Makefile" "$root/include" "$root/src" "$work/p"
  cd "$work/p" || exit 1
}

# Make everything look old, then build a marker that is newer than that.
age() {
  touch -t 202001010000 src/* include/* out/*
  touch -t 202101010000 ref
}

rebuilt() {
  (cd out && find . -name '*.out' -newer ../ref | sed 's|^\./||' | sort | tr '\n' ' ')
}

fresh
make >/dev/null 2>&1
age
make -q >/dev/null 2>&1
expect "up to date after a full build (make -q)" "$?" "0"
expect "nothing is rebuilt when nothing changed" "$(make 2>&1 | grep -c '^cat ')" "0"

touch include/common.txt
make >/dev/null 2>&1
expect "touching the shared header rebuilds exactly two outputs" "$(rebuilt)" "a.out b.out "

fresh
make >/dev/null 2>&1
age
touch src/c.txt
make >/dev/null 2>&1
expect "touching one source rebuilds only its output" "$(rebuilt)" "c.out "

fresh
make >/dev/null 2>&1
age
printf 'Changed notice\n' > include/common.txt
make >/dev/null 2>&1
expect "header text reaches a.out" "$(head -1 out/a.out)" "Changed notice"
expect "header text reaches b.out" "$(head -1 out/b.out)" "Changed notice"
expect "c.out ignores the header" "$(cat out/c.out)" "charlie body"
second=$(make 2>&1)
case $second in
  *"Nothing to be done"*) ;;
  *) echo "FAIL second make after a header change should do nothing, got: $second"; failures=$((failures + 1)) ;;
esac
expect "make -q after the rebuild" "$(make -q >/dev/null 2>&1; echo $?)" "0"

fresh
make >/dev/null 2>&1
make clean >/dev/null 2>&1
expect "clean removes the outputs" "$(ls out 2>/dev/null | wc -l | tr -d ' ')" "0"

[ "$failures" -eq 0 ] || exit 1
echo "all checks passed"
