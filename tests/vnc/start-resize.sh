#!/bin/sh
set -eu
x11vnc -storepasswd adit-ci /tmp/vnc-passwd >/dev/null
Xtigervnc :104 -noreset -geometry 800x600 -depth 24 -rfbport 15908 -PasswordFile /tmp/vnc-passwd -SecurityTypes VncAuth -localhost=0 >/tmp/resize-tiger.log 2>&1 &
server=$!
for i in $(seq 1 50); do DISPLAY=:104 xsetroot -solid red 2>/dev/null && break; sleep .1; done
wait "$server"
