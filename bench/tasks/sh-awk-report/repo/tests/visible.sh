#!/bin/sh
# Checks for report.sh. Run from the project root: sh tests/visible.sh
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

cat > "$work/sales.csv" <<'EOF'
date,item,qty,amount
2024-01-05,widget,3,30.00
2024-01-20,gadget,1,12.50
2024-02-02,widget,10,100.00
2024-01-31,gizmo,2,7.25
EOF
expect "totals by month" "$(sh "$root/report.sh" "$work/sales.csv")" "2024-01 49.75
2024-02 100.00"

printf 'date,item,qty,amount\n' > "$work/empty.csv"
expect "header only" "$(sh "$root/report.sh" "$work/empty.csv")" ""

[ "$failures" -eq 0 ] || exit 1
echo "visible checks passed"
