#!/bin/sh
set -eu
# Separate, unpublished fixture. This script owns only display :109 / port 15909.
test ! -e /tmp/.X109-lock
x11vnc -storepasswd adit-ci /tmp/adit-gateway-passwd >/dev/null
Xtigervnc :109 -noreset -geometry 800x600 -depth 24 -rfbport 15909 -PasswordFile /tmp/adit-gateway-passwd -SecurityTypes VncAuth -localhost >/tmp/adit-gateway-tiger.log 2>&1 &
server=$!
echo $$ >/tmp/adit-gateway-fixture.pid
viewer=
trap 'rm -f /tmp/adit-gateway-fixture.pid; test -z "$viewer" || kill "$viewer" 2>/dev/null || true; kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true' EXIT INT TERM
for i in $(seq 1 50); do DISPLAY=:109 xsetroot -solid red 2>/dev/null && break; sleep .1; done
DISPLAY=:109 xev -name AditGatewayInput -geometry 240x180+0+0 -event keyboard -event mouse >/tmp/adit-gateway-events.log 2>&1 &
viewer=$!
for i in $(seq 1 50); do
    window=$(DISPLAY=:109 xdotool search --name '^AditGatewayInput$' 2>/dev/null | head -1)
    if test -n "$window"; then DISPLAY=:109 xdotool windowfocus --sync "$window"; echo READY; break; fi
    sleep .1
done
wait "$server"
