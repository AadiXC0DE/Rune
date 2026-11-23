#!/bin/sh
# Grading checks for report.sh. Run from the project root: sh tests/run.sh
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

cat > "$work/quoted.csv" <<'EOF'
date,"Item, long name",qty,amount
2024-03-01,"Widget, large",3,19.99
2024-03-15,"Say ""hi"", gadget",1,5.01
2024-04-01,plain,2,0.50
EOF
expect "quoted header and quoted fields" "$(sh "$root/report.sh" "$work/quoted.csv")" "2024-03 25.00
2024-04 0.50"

printf 'date,item,qty,amount\n\n2024-05-01,a,1,1.00\n\n   \n2024-05-02,b,1,2.00\n\n' > "$work/blank.csv"
expect "blank lines are skipped" "$(sh "$root/report.sh" "$work/blank.csv")" "2024-05 3.00"

printf '\ndate,item,qty,amount\n2024-05-01,a,1,1.00\n' > "$work/leading.csv"
expect "blank line before the header" "$(sh "$root/report.sh" "$work/leading.csv")" "2024-05 1.00"

cat > "$work/order.csv" <<'EOF'
amount,region,date,qty
10.00,north,2024-06-01,5
2.50,south,2024-06-09,1
7.00,north,2024-07-01,9
EOF
expect "column order changed" "$(sh "$root/report.sh" "$work/order.csv")" "2024-06 12.50
2024-07 7.00"

printf 'date,item,qty,amount\r\n2024-08-01,a,1,4.00\r\n2024-08-02,b,1,1.00\r\n' > "$work/crlf.csv"
expect "carriage returns" "$(sh "$root/report.sh" "$work/crlf.csv")" "2024-08 5.00"

printf 'when,item,qty,total\n2024-08-01,a,1,4.00\n' > "$work/nocol.csv"
if sh "$root/report.sh" "$work/nocol.csv" >/dev/null 2>&1; then
  echo "FAIL a file without date and amount columns should fail"
  failures=$((failures + 1))
fi

mkdir "$work/dir with space"
cp "$root/report.sh" "$root/report.awk" "$work/dir with space/"
expect "script run from another directory" "$(cd / && sh "$work/dir with space/report.sh" "$work/blank.csv")" "2024-05 3.00"

[ "$failures" -eq 0 ] || exit 1
echo "all checks passed"
