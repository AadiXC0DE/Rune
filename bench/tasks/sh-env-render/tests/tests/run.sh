#!/bin/sh
# Grading checks for render.sh. Run from the project root: sh tests/run.sh
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

render() {
  sh "$root/render.sh" "$@"
}

tmpl=$root/templates/app.conf.tmpl

# References are resolved against the final values, not the defaults.
printf 'HOST=example.org\n' > "$work/host.env"
expect "defaults refer to overridden values" "$(render "$tmpl" "$work/host.env")" "name = demo
listen = example.org:8080
url = http://example.org:8080/app
logs = /var/log/demo"

# Chained references, written in the opposite order of use.
printf 'A=${B}-a\nB=${C}-b\nC=c\n' > "$work/chain.env"
printf 'a={{A}} b={{B}} c={{C}}\n' > "$work/abc.tmpl"
expect "chained references listed forwards" "$(render "$work/abc.tmpl" "$work/chain.env")" "a=c-b-a b=c-b c=c"

# Defaults with ${NAME:-fallback}.
printf 'SET=yes\nEMPTY=\nA=${SET:-no}\nB=${UNSET:-no}\nC=${EMPTY:-no}\nD=${UNSET:-/usr/local/share}\nE=x${UNSET:-}y\n' > "$work/fb.env"
printf '{{A}} {{B}} {{C}} {{D}} {{E}}\n' > "$work/fb.tmpl"
expect "fallback syntax" "$(render "$work/fb.tmpl" "$work/fb.env")" "yes no no /usr/local/share xy"

# Values with / and & must come through unchanged.
printf 'PREFIX=/srv/a&b\nLOGS=${PREFIX}/logs\nQUERY=a=1&b=2\n' > "$work/sym.env"
printf 'p={{PREFIX}}\nl={{LOGS}}\nq={{QUERY}}\n' > "$work/sym.tmpl"
expect "slash and ampersand" "$(render "$work/sym.tmpl" "$work/sym.env")" "p=/srv/a&b
l=/srv/a&b/logs
q=a=1&b=2"

# A value that contains an equals sign, a backslash and a space.
printf 'RAW=a\\nb = c\n' > "$work/raw.env"
printf 'r={{RAW}}\n' > "$work/raw.tmpl"
expect "backslash and equals kept" "$(render "$work/raw.tmpl" "$work/raw.env")" 'r=a\nb = c'

# No environment file, or one that does not exist.
expect "no environment file argument" "$(render "$tmpl")" "name = demo
listen = localhost:8080
url = http://localhost:8080/app
logs = /var/log/demo"
expect "missing environment file" "$(render "$tmpl" "$work/missing.env")" "name = demo
listen = localhost:8080
url = http://localhost:8080/app
logs = /var/log/demo"

# Comments and blank lines in the environment file, last assignment wins.
printf '# comment\n\nPORT=1\nPORT=2\n' > "$work/dup.env"
printf 'port={{PORT}}\n' > "$work/port.tmpl"
expect "comments, blanks and repeated keys" "$(render "$work/port.tmpl" "$work/dup.env")" "port=2"

# Unknown placeholders stay as they are.
printf 'x={{NOPE}} y={{PORT}}\n' > "$work/unk.tmpl"
expect "unknown placeholder" "$(render "$work/unk.tmpl" "$work/none.env")" "x={{NOPE}} y=8080"

if sh "$root/render.sh" "$work/absent.tmpl" "$work/none.env" >/dev/null 2>&1; then
  echo "FAIL a missing template should fail"
  failures=$((failures + 1))
fi

[ "$failures" -eq 0 ] || exit 1
echo "all checks passed"
