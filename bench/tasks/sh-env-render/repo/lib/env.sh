# Functions for render.sh. Tables are lines of KEY=VALUE.

# env_merge FILE...: print the settings of the files that exist. A later file
# overrides an earlier one. Blank lines and lines starting with # are skipped.
env_merge() {
  for f in "$@"; do
    [ -f "$f" ] && cat "$f"
  done | awk '
    /^[ \t]*(#|$)/ { next }
    {
      sub(/\r$/, "")
      i = index($0, "=")
      key = substr($0, 1, i - 1)
      if (!(key in value)) order[++n] = key
      value[key] = substr($0, i + 1)
    }
    END { for (j = 1; j <= n; j++) print order[j] "=" value[order[j]] }
  '
}

# env_expand: read a table on standard input and print it with references to
# other values filled in.
env_expand() {
  done_file=$(mktemp)
  while IFS= read -r line; do
    key=${line%%=*}
    value=${line#*=}
    while IFS= read -r prev; do
      pk=${prev%%=*}
      pv=${prev#*=}
      value=$(printf '%s' "$value" | sed "s/\${$pk}/$pv/g")
    done < "$done_file"
    printf '%s=%s\n' "$key" "$value" >> "$done_file"
  done
  cat "$done_file"
  rm -f "$done_file"
}

# env_render TABLE TEMPLATE: print the template with each {{KEY}} replaced.
env_render() {
  out=$(cat "$2")
  while IFS= read -r line; do
    key=${line%%=*}
    value=${line#*=}
    out=$(printf '%s\n' "$out" | sed "s/{{$key}}/$value/g")
  done < "$1"
  printf '%s\n' "$out"
}
