#!/bin/sh
# Remove all but the newest KEEP files named *.log in DIR.
# File names carry their date, so sorting by name puts the oldest first.
# usage: rotate.sh DIR KEEP

if [ $# -ne 2 ]; then
  echo "usage: rotate.sh DIR KEEP" >&2
  exit 2
fi
dir=$1
keep=$2
case $keep in
  '' | *[!0-9]*)
    echo "rotate.sh: KEEP must be a whole number" >&2
    exit 2
    ;;
esac
cd "$dir" || exit 1

LC_ALL=C
export LC_ALL

total=0
for f in ./*.log; do
  [ -f "$f" ] || continue
  total=$((total + 1))
done

remove=$((total - keep))
i=0
for f in ./*.log; do
  [ -f "$f" ] || continue
  [ "$i" -lt "$remove" ] || break
  rm -f -- "$f"
  i=$((i + 1))
done
