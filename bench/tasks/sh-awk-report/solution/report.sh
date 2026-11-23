#!/bin/sh
# Print the total amount per month of a sales file, one "YYYY-MM total" line
# per month in date order.
# usage: report.sh FILE

if [ $# -ne 1 ]; then
  echo "usage: report.sh FILE" >&2
  exit 2
fi
dir=$(dirname "$0")
out=$(awk -f "$dir/report.awk" "$1") || exit 1
if [ -n "$out" ]; then
  printf '%s\n' "$out" | sort
fi
