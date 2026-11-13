#!/bin/sh
# Remove all but the newest KEEP files named *.log in DIR.
# File names carry their date, so sorting by name puts the oldest first.
# usage: rotate.sh DIR KEEP

dir=$1
keep=$2
cd $dir || exit 1

files=$(ls *.log | sort)
count=$(echo "$files" | wc -l)
remove=$((count - keep - 1))
i=0
for f in $files; do
  if [ $i -lt $remove ]; then
    rm $f
  fi
  i=$((i + 1))
done
