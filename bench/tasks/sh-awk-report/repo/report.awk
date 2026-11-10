# Sum the amount column of a sales file by month.
# The first line is a header; the date is in the first column.
BEGIN { FS = "," }

NR == 1 { next }

{
  month = substr($1, 1, 7)
  total[month] += $3
}

END {
  for (m in total) {
    printf "%s %.2f\n", m, total[m]
  }
}
