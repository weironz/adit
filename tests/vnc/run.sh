#!/bin/sh
set -eu
# Disposable credentials on isolated test ports; never a production endpoint.
export DISPLAY=:99
Xvfb :99 -noreset -screen 0 800x600x24 >/tmp/xvfb.log 2>&1 &
for i in $(seq 1 50); do xsetroot -solid red 2>/dev/null && break; sleep .1; done
x11vnc -storepasswd adit-ci /tmp/vnc-passwd >/dev/null
x11vnc -display :99 -rfbport 15900 -rfbauth /tmp/vnc-passwd -localhost -forever -shared >/tmp/x11vnc.log 2>&1 &
# Container listener is exposed only on host loopback by the documented run command.
x11vnc -display :99 -rfbport 15901 -rfbauth /tmp/vnc-passwd -forever -shared >/tmp/x11vnc-direct.log 2>&1 &
xev -name AditVncTest -geometry 200x150+0+0 -event keyboard -event mouse >/tmp/vnc-events.log 2>&1 &
/usr/sbin/sshd -D -e
