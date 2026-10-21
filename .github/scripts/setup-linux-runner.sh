#!/usr/bin/env bash
set -euo pipefail

[[ $(uname -sm) == 'Linux x86_64' ]] || { echo 'Linux x64 runner required' >&2; exit 1; }
sudo -n true
# winit dlopens libxkbcommon-x11: compiling alone cannot detect its absence.
packages=(build-essential pkg-config libxkbcommon-dev libxkbcommon-x11-0 libwayland-dev
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgtk-3-dev libdbus-1-dev
  xvfb mesa-vulkan-drivers libgl1-mesa-dri x11vnc tigervnc-standalone-server
  tigervnc-tools x11-xserver-utils x11-utils xdotool xauth python3-tk python3-yaml
  rpm iproute2 curl ca-certificates)
missing=()
for package in "${packages[@]}"; do
  [[ $(dpkg-query -W -f='${Status}' "$package" 2>/dev/null || true) == 'install ok installed' ]] || missing+=("$package")
done
if ((${#missing[@]})); then
  sudo apt-get update
  sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${missing[@]}"
fi
docker info --format 'Docker {{.ServerVersion}}'
python3 -c 'import tkinter; import yaml; print("Python Tk/YAML available")'
pkg-config --exists xkbcommon wayland-client x11 gtk+-3.0 dbus-1
echo 'Linux build, VNC, packaging and Docker prerequisites available'
