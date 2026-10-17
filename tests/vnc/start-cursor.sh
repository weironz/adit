#!/bin/sh
set -eu
export DISPLAY=:111
Xtigervnc :111 -noreset -geometry 800x600 -depth 24 -rfbport 15912 -SecurityTypes None -localhost=0 >/tmp/cursor-tiger.log 2>&1 &
server=$!
target=''
trap 'kill "$server" ${target:+"$target"} 2>/dev/null || true' EXIT INT TERM
for attempt in $(seq 1 100); do
    if xsetroot -solid '#244466' 2>/dev/null; then
        python3 /cursor_target.py &
        target=$!
        wait "$target"
        exit $?
    fi
    sleep .1
done
echo 'isolated cursor display did not start' >&2
exit 1
