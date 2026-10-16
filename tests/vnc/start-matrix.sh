#!/bin/sh
set -eu
export DISPLAY=:99
Xvfb :99 -noreset -screen 0 800x600x24 >/tmp/xvfb.log 2>&1 &
x11vnc -storepasswd adit-ci /tmp/vnc-passwd >/dev/null
for i in $(seq 1 50); do xsetroot -solid red 2>/dev/null && break; sleep .1; done
x11vnc -display :99 -rfbport 15901 -rfbauth /tmp/vnc-passwd -forever -shared >/tmp/x11vnc.log 2>&1 &
Xtigervnc :100 -noreset -geometry 800x600 -depth 24 -rfbport 15902 -PasswordFile /tmp/vnc-passwd -SecurityTypes VncAuth -localhost=0 >/tmp/tiger.log 2>&1 &
/opt/TurboVNC/bin/Xvnc :101 -noreset -geometry 800x600 -depth 24 -rfbport 15903 -rfbauth /tmp/vnc-passwd -securitytypes vnc >/tmp/turbo.log 2>&1 &
for display in 99 100 101; do
    for i in $(seq 1 50); do DISPLAY=:$display xsetroot -solid red 2>/dev/null && break; sleep .1; done
    DISPLAY=:$display python3 /load.py >/tmp/load-$display.log 2>&1 &
done
wait
