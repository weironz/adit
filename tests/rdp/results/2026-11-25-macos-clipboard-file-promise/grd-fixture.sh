#!/bin/sh
set -eu

sudo loginctl enable-linger "$(id -un)"
systemctl --user daemon-reload
systemctl --user start dbus.socket pipewire.socket wireplumber.service
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

certdir="$HOME/.local/share/gnome-remote-desktop"
mkdir -p "$certdir" "$HOME/rdp-file-fixture"
openssl req -quiet -new -newkey rsa:2048 -nodes -x509 -days 1 \
  -subj /CN=localhost -keyout "$certdir/tls.key" -out "$certdir/tls.crt"
grdctl --headless rdp set-tls-key "$certdir/tls.key"
grdctl --headless rdp set-tls-cert "$certdir/tls.crt"
grdctl --headless rdp set-credentials "$(id -un)"
grdctl --headless rdp disable-view-only
grdctl --headless rdp enable
systemctl --user set-environment G_MESSAGES_DEBUG=all
systemd-run --user --unit=adit-mutter --setenv=LIBGL_ALWAYS_SOFTWARE=1 \
  /usr/bin/mutter --headless --wayland --no-x11
timeout 30s gdbus wait --session org.gnome.Mutter.RemoteDesktop
systemctl --user enable --now gnome-remote-desktop-headless.service

python3 - <<'PY'
from pathlib import Path
folder = Path.home() / 'rdp-file-fixture'
(folder / 'remote-pattern.bin').write_bytes(bytes(n % 251 for n in range(1_048_613)))
PY
sha256sum "$HOME/rdp-file-fixture/remote-pattern.bin"
dpkg-query -W gnome-remote-desktop mutter nautilus
ss -ltn '( sport = :3389 )'
