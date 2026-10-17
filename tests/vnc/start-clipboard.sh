#!/bin/sh
set -eu
# Dedicated clipboard fixture; leave benchmark displays, workloads and ports alone.
export DISPLAY=:113
test ! -e /tmp/.X113-lock
Xvfb :113 -noreset -screen 0 800x600x24 >/tmp/clipboard-xvfb.log 2>&1 &
xserver=$!
server=
trap 'test -z "$server" || kill "$server" 2>/dev/null || true; kill "$xserver" 2>/dev/null || true' EXIT INT TERM
for i in $(seq 1 50); do xsetroot -solid red 2>/dev/null && break; sleep .1; done
xsetroot -solid red
x11vnc -storepasswd adit-ci /tmp/clipboard-vnc-passwd >/dev/null
# Without a window manager x11vnc otherwise defers its X11 selection owner for
# approximately 45 seconds after a client connects. This is fixture preparation,
# not a client timeout or product workaround. Still wait for observable readiness.
X11VNC_AVOID_WINDOWS=never x11vnc -display :113 -rfbport 15913 -rfbauth /tmp/clipboard-vnc-passwd -forever -shared -debug_sel >/tmp/clipboard-x11vnc.log 2>&1 &
server=$!
wait "$server"
