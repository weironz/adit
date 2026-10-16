#!/bin/sh
set -eu
# Synthetic one-day CA and leaf are generated inside the disposable container.
openssl req -x509 -newkey rsa:2048 -nodes -keyout /tmp/ca.key -out /tmp/ca.pem -days 1 -subj /CN=Adit-Isolated-VNC-Test-CA >/tmp/cert.log 2>&1
openssl req -newkey rsa:2048 -nodes -keyout /tmp/server.key -out /tmp/server.csr -subj /CN=localhost >>/tmp/cert.log 2>&1
printf 'subjectAltName=DNS:localhost
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
' >/tmp/server.ext
openssl x509 -req -in /tmp/server.csr -CA /tmp/ca.pem -CAkey /tmp/ca.key -CAcreateserial -out /tmp/server.pem -days 1 -sha256 -extfile /tmp/server.ext >>/tmp/cert.log 2>&1
x11vnc -storepasswd adit-ci /tmp/vnc-passwd >/dev/null
Xtigervnc :102 -noreset -geometry 800x600 -depth 24 -rfbport 15905 -PasswordFile /tmp/vnc-passwd -SecurityTypes X509Vnc -X509Cert /tmp/server.pem -X509Key /tmp/server.key -localhost=0 >/tmp/tiger-tls.log 2>&1 &
/opt/TurboVNC/bin/Xvnc :103 -noreset -geometry 800x600 -depth 24 -rfbport 15906 -rfbauth /tmp/vnc-passwd -securitytypes x509vnc -x509cert /tmp/server.pem -x509key /tmp/server.key >/tmp/turbo-tls.log 2>&1 &
for display in 102 103; do
    for i in $(seq 1 50); do DISPLAY=:$display xsetroot -solid red 2>/dev/null && break; sleep .1; done
done
touch /tmp/adit-tls-ready
wait
