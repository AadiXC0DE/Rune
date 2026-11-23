# Sum the amount column of a sales file by month.
# Columns are found by the names "date" and "amount" in the header line.
# Fields may be quoted; blank lines are skipped.

function split_csv(line, out,    n, i, c, field, inq, len) {
  split("", out)
  n = 0
  field = ""
  inq = 0
  len = length(line)
  for (i = 1; i <= len; i++) {
    c = substr(line, i, 1)
    if (inq) {
      if (c == "\"") {
        if (substr(line, i + 1, 1) == "\"") {
          field = field "\""
          i++
        } else {
          inq = 0
        }
      } else {
        field = field c
      }
    } else if (c == "\"") {
      inq = 1
    } else if (c == ",") {
      out[++n] = field
      field = ""
    } else {
      field = field c
    }
  }
  out[++n] = field
  return n
}

{ sub(/\r$/, "") }

/^[ \t]*$/ { next }

!have_header {
  have_header = 1
  n = split_csv($0, head)
  for (i = 1; i <= n; i++) {
    if (head[i] == "date") dcol = i
    if (head[i] == "amount") acol = i
  }
  if (!dcol || !acol) {
    print "report.awk: the header needs date and amount columns" > "/dev/stderr"
    failed = 1
    exit 1
  }
  next
}

{
  split_csv($0, f)
  month = substr(f[dcol], 1, 7)
  total[month] += f[acol]
}

END {
  if (failed) exit 1
  for (m in total) {
    printf "%s %.2f\n", m, total[m]
  }
}
