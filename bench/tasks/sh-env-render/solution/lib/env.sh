# Functions for render.sh. Tables are lines of KEY=VALUE.

# env_merge FILE...: print the settings of the files that exist. A later file
# overrides an earlier one. Blank lines and lines starting with # are skipped.
env_merge() {
  for f in "$@"; do
    [ -n "$f" ] && [ -f "$f" ] && cat "$f"
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
# other values filled in. ${NAME} and ${NAME:-fallback} are understood; the
# fallback is used when NAME is unset or empty. References may be chained.
env_expand() {
  awk '
    function expand_once(s,    out, start, rest, stop, inner, c, name, fb, use) {
      out = ""
      while ((start = index(s, "${")) > 0) {
        out = out substr(s, 1, start - 1)
        rest = substr(s, start + 2)
        stop = index(rest, "}")
        if (stop == 0) {
          out = out "${"
          s = rest
          continue
        }
        inner = substr(rest, 1, stop - 1)
        c = index(inner, ":-")
        if (c > 0) {
          name = substr(inner, 1, c - 1)
          fb = substr(inner, c + 2)
        } else {
          name = inner
          fb = ""
        }
        use = (name in value) ? value[name] : ""
        out = out ((use == "") ? fb : use)
        s = substr(rest, stop + 1)
      }
      return out s
    }
    {
      i = index($0, "=")
      key = substr($0, 1, i - 1)
      order[++n] = key
      value[key] = substr($0, i + 1)
    }
    END {
      for (pass = 1; pass <= 20; pass++) {
        changed = 0
        for (j = 1; j <= n; j++) {
          k = order[j]
          nv = expand_once(value[k])
          if (nv != value[k]) {
            value[k] = nv
            changed = 1
          }
        }
        if (!changed) break
      }
      for (j = 1; j <= n; j++) print order[j] "=" value[order[j]]
    }
  '
}

# env_render TABLE TEMPLATE: print the template with each {{KEY}} replaced.
# Placeholders with no matching key are left as they are.
env_render() {
  awk '
    NR == FNR {
      i = index($0, "=")
      value[substr($0, 1, i - 1)] = substr($0, i + 1)
      next
    }
    {
      line = $0
      out = ""
      while ((start = index(line, "{{")) > 0) {
        rest = substr(line, start + 2)
        stop = index(rest, "}}")
        if (stop == 0) break
        name = substr(rest, 1, stop - 1)
        if (name in value) {
          out = out substr(line, 1, start - 1) value[name]
        } else {
          out = out substr(line, 1, start - 1) "{{" name "}}"
        }
        line = substr(rest, stop + 2)
      }
      print out line
    }
  ' "$1" "$2"
}
