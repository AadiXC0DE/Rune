#!/bin/sh
# Fail unless every standard stream and /dev/tty refer to a terminal.
test -t 0 && test -t 1 && test -t 2 || exit 91
test -r /dev/tty || exit 92
stty -echo
printf 'initial:%s\n' "$(stty size)"
read -r line </dev/tty || exit 93
printf 'input:%s\n' "$line"
printf 'resized:%s\n' "$(stty size)"
printf 'stderr-marker\n' >&2
# Leave a child and grandchild that ignore TERM and HUP. The leader accepts TERM.
/bin/sh -c 'trap "" TERM HUP; sleep 30 & echo $! > grandchild.pid; echo $$ > child.pid; touch ready; wait' &
wait
