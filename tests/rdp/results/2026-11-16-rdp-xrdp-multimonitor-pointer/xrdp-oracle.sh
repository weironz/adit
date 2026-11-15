#!/usr/bin/env bash
# Prints where the xrdp session's own X server puts its pointer, as `X Y`.
# xorgxrdp *is* the server: the pointer it reports is the one RDP input moved,
# not a copy reconstructed by the client.
#   xrdp-oracle.sh pointer | xrandr
set -eu
user="${XRDP_ORACLE_USER:-aditrdp}"
display=$(pgrep -u "$user" -a Xorg | grep -o ' :[0-9]\+' | head -1 | tr -d ' ')
[ -n "$display" ] || { echo "no xorgxrdp display for $user" >&2; exit 1; }
run() { sudo -u "$user" env DISPLAY="$display" XAUTHORITY="/home/$user/.Xauthority" "$@"; }
case "${1:-pointer}" in
    pointer) eval "$(run xdotool getmouselocation --shell)"; echo "$X $Y" ;;
    xrandr)  echo "display=$display"; run xrandr --listmonitors; run xdpyinfo | grep dimensions ;;
esac
