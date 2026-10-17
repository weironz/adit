#!/bin/sh
# Public synthetic credentials for an isolated manual settings test only.
set -eu
export DISPLAY=:115
test ! -e /tmp/.X115-lock
umask 077
x11vnc -storepasswd adit-ci /tmp/vault-manual.passwd >/dev/null
Xtigervnc :115 -noreset -geometry 800x600 -depth 24 -rfbport 15918 \
    -PasswordFile /tmp/vault-manual.passwd -SecurityTypes VncAuth \
    -localhost=0 >/tmp/vault-manual-server.log 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT
trap 'exit 0' TERM INT
for attempt in $(seq 1 20); do
    kill -0 "$server_pid"
    if timeout 1s xsetroot -solid '#244a72' 2>/dev/null; then
        printf '%s\n' 'READY synthetic VncAuth desktop, container port 15918'
        wait "$server_pid"
        exit 0
    fi
    sleep .1
done
cat /tmp/vault-manual-server.log
exit 1
