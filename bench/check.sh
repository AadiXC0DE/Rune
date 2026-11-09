#!/bin/sh
# Checks that every task is a fair test: its verify command fails on repo/ as
# shipped and passes once solution/ is copied over it.
#
#   bench/check.sh [tasks-dir]
#
# Needs the toolchains the tasks use: node, python3, go and cargo.
root="${1:-$(dirname "$0")/tasks}"
status=0
for dir in "$root"/*/; do
  id=$(basename "$dir")
  verify=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['verify'])" "$dir/task.json")
  work=$(mktemp -d)
  cp -R "$dir/repo/." "$work/"
  (cd "$work" && sh -c "$verify" >/dev/null 2>&1)
  broken=$?
  cp -R "$dir/solution/." "$work/"
  (cd "$work" && sh -c "$verify" >/dev/null 2>&1)
  fixed=$?
  rm -rf "$work"
  if [ "$broken" -ne 0 ] && [ "$fixed" -eq 0 ]; then
    echo "ok    $id"
  else
    echo "BAD   $id (as shipped exit $broken, solved exit $fixed)"
    status=1
  fi
done
exit $status
