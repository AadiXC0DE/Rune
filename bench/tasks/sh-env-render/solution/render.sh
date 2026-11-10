#!/bin/sh
# Fill a template from defaults.env and an optional environment file.
# Placeholders look like {{NAME}}; values may refer to other values as ${NAME}
# or ${NAME:-fallback}.
# usage: render.sh TEMPLATE [ENVFILE]

here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib/env.sh"

template=$1
envfile=${2:-}

if [ ! -f "$template" ]; then
  echo "render.sh: no such template: $template" >&2
  exit 1
fi

table=$(mktemp)
trap 'rm -f "$table"' EXIT

env_merge "$here/defaults.env" "$envfile" | env_expand > "$table"
env_render "$table" "$template"
