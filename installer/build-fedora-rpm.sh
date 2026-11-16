#!/usr/bin/env bash
#
# Build the rpm inside a Fedora container for that Fedora release, then install
# and start it in a second, clean container of the same release.
#
# An rpm takes its library requirements from the system it was linked on. One
# built on Ubuntu 24.04 required libfuse3.so.3, which Fedora 44+ (fuse3 3.18,
# libfuse3.so.4) does not provide, so it could not be installed there. Hence
# one rpm per Fedora release, each built against that release.
#
#   usage: installer/build-fedora-rpm.sh 44 [version]
#
# Produces rpmbuild/RPMS/x86_64/adit-<version>-1.fc<release>.x86_64.rpm.

set -euo pipefail

if [[ ${1:-} == --in-container ]]; then
  shift
  dnf -y -q install gcc gcc-c++ make pkgconf-pkg-config rpm-build rustup \
    libxkbcommon-devel wayland-devel libX11-devel libXcursor-devel \
    libXrandr-devel libXi-devel gtk3-devel dbus-devel fuse3-devel
  # rust-toolchain.toml pins the compiler; rustup installs that one on first use.
  rustup-init -y -q --profile minimal --default-toolchain none
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
  rustc --version
  cargo build --release -p adit-app --locked
  bash installer/build-linux-packages.sh rpm "$@"
  # The mounted checkout belongs to the runner; root-owned outputs would break
  # its cleanup and later steps.
  chown -R "$HOST_UID:$HOST_GID" target rpmbuild pkgroot adit.spec
  exit 0
fi

fedora=${1:?usage: build-fedora-rpm.sh <fedora-release> [version]}
shift
[[ $fedora =~ ^[0-9]+$ ]] || { echo "Fedora release must be a number: $fedora" >&2; exit 2; }

rm -rf rpmbuild
# The OAuth client IDs are baked in at compile time (option_env! in adit-sync)
# and release.yml sets them only on the host; -e NAME forwards each one if set.
docker run --rm -v "$PWD:/w" -w /w -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -e ADIT_SYNC_GOOGLE_CLIENT_ID -e ADIT_SYNC_GOOGLE_CLIENT_SECRET \
  -e ADIT_SYNC_ONEDRIVE_CLIENT_ID -e ADIT_SYNC_DROPBOX_CLIENT_ID -e ADIT_SYNC_GITHUB_CLIENT_ID \
  "fedora:$fedora" bash installer/build-fedora-rpm.sh --in-container "$@"

shopt -s nullglob
rpms=(rpmbuild/RPMS/x86_64/adit-*.fc"$fedora".x86_64.rpm)
((${#rpms[@]} == 1)) || { echo "expected one .fc$fedora rpm, found ${#rpms[@]}" >&2; exit 1; }
echo "built ${rpms[0]}"

# A bare image has none of the -devel packages, so dnf must satisfy Requires on
# its own; a missing or unsatisfiable entry fails here, not on a user's machine.
docker run --rm -v "$PWD:/w" -w /w "fedora:$fedora" bash -eu -c '
  dnf -y -q install "./$1" xorg-x11-server-Xvfb >/dev/null
  xvfb-run -a --server-args="-screen 0 1280x800x24" adit > /tmp/rpm-smoke.log 2>&1 &
  pid=$!
  sleep 25
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    echo "installed adit stayed up on Fedora $2"
  else
    cat /tmp/rpm-smoke.log
    echo "::error::the installed adit exited on its own on Fedora $2; Requires is incomplete or wrong"
    exit 1
  fi
' smoke "${rpms[0]}" "$fedora"
