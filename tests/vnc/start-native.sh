#!/bin/sh
set -eu
export DISPLAY=:112
Xtigervnc :112 -noreset -geometry 1200x800 -depth 24 -rfbport 15915 -SecurityTypes None -localhost=0 >/tmp/native-tiger.log 2>&1 &
for attempt in $(seq 1 100); do
    if xsetroot -solid '#17324d' 2>/dev/null; then
        python3 /cursor_target.py >/tmp/native-cursor.log 2>&1 &
        # The grid is a separate raised window and inherits the root cursor.
        for ready in $(seq 1 100); do
            test -S /tmp/adit-vnc-cursor.sock && break
            sleep .05
        done
        python3 /native_target.py >/tmp/native-events.jsonl 2>&1 &
        wait
    fi
    sleep .1
done
exit 1
