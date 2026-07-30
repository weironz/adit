#!/usr/bin/env bash
#
# Build the Linux packages from an already-built release binary.
#
# Shared by ci.yml and release.yml on purpose. The dependency lists below are
# the part of packaging most likely to be wrong, and keeping a copy in each
# workflow guarantees that one of them eventually goes stale.
#
#   usage: installer/build-linux-packages.sh 0.1.60
#
# Produces, in the working directory, for the architecture it is running on:
#   adit_<version>_<amd64|arm64>.deb
#   rpmbuild/RPMS/<x86_64|aarch64>/adit-<version>-1.<x86_64|aarch64>.rpm

set -euo pipefail

# Defaults to the workspace version; release.yml passes it explicitly so the
# package matches the tag even before the bump is committed.
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
[[ -n "$version" ]] || { echo "cannot determine version" >&2; exit 1; }
binary=target/release/adit-app
root=pkgroot

# dpkg and rpm disagree on what to call the same machine, and neither uses
# uname's name for arm64. Both spellings are needed, so derive them together
# rather than letting a caller pass one and get the other wrong.
case "$(uname -m)" in
  x86_64)  deb_arch=amd64; rpm_arch=x86_64 ;;
  aarch64) deb_arch=arm64; rpm_arch=aarch64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

if [[ ! -x "$binary" ]]; then
  echo "no release binary at $binary — run cargo build --release -p adit-app first" >&2
  exit 1
fi

rm -rf "$root" rpmbuild
mkdir -p "$root/DEBIAN" "$root/usr/bin" \
         "$root/usr/share/applications" \
         "$root/usr/share/icons/hicolor/256x256/apps"

install -m755 "$binary" "$root/usr/bin/adit"
install -m644 assets/icon.png "$root/usr/share/icons/hicolor/256x256/apps/adit.png"

cat > "$root/usr/share/applications/adit.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Adit
Comment=SSH / SFTP / RDP terminal client
Exec=adit
Icon=adit
Terminal=false
Categories=Network;RemoteAccess;
DESKTOP

# libxkbcommon-x11-0 is dlopened by winit, so it appears in no ELF header and
# no automatic tool would find it — CI caught it only by running the binary.
# The rest back rfd (GTK), keyring (D-Bus), winit (X11/Wayland) and wgpu (the
# Vulkan loader). The `|` alternatives cover Ubuntu 24.04's t64 renaming.
cat > "$root/DEBIAN/control" <<CONTROL
Package: adit
Version: $version
Section: net
Priority: optional
Architecture: $deb_arch
Maintainer: Adit <noreply@example.com>
Depends: libxkbcommon0, libxkbcommon-x11-0, libwayland-client0,
 libx11-6, libxcursor1, libxrandr2, libxi6,
 libgtk-3-0t64 | libgtk-3-0, libdbus-1-3, libvulkan1
Description: SSH / SFTP / RDP terminal client
 A native terminal client with session groups, tabs, split panes,
 SFTP transfer and port forwarding.
CONTROL

dpkg-deb --build --root-owner-group "$root" "adit_${version}_${deb_arch}.deb"
dpkg-deb --info "adit_${version}_${deb_arch}.deb"

# RPM derives Requires by scanning the ELF, which is worth very little here:
# winit reaches the X11 stack through x11-dl, so libX11, libXcursor, libXrandr
# and libXi are all dlopened and appear in no ELF header. Trusting the automatic
# scan produced a package that installed cleanly and then died on startup with
# "libXcursor.so.1: cannot open shared object file". The list below mirrors the
# Debian one deliberately; the two must stay in step.
cat > adit.spec <<SPEC
%global debug_package %{nil}
Name:      adit
Version:   $version
Release:   1
Summary:   SSH / SFTP / RDP terminal client
License:   MIT
BuildArch: $rpm_arch
Requires:  libxkbcommon, libxkbcommon-x11, libwayland-client
Requires:  libX11, libXcursor, libXrandr, libXi
Requires:  gtk3, dbus-libs, vulkan-loader

%description
A native terminal client with session groups, tabs, split panes,
SFTP transfer and port forwarding.

%install
mkdir -p %{buildroot}
cp -a %{_sourcedir}/$root/usr %{buildroot}/

%files
/usr/bin/adit
/usr/share/applications/adit.desktop
/usr/share/icons/hicolor/256x256/apps/adit.png
SPEC

rpmbuild -bb \
  --define "_topdir $PWD/rpmbuild" \
  --define "_sourcedir $PWD" \
  adit.spec
rpm -qpR "rpmbuild/RPMS/$rpm_arch/adit-"*.rpm
