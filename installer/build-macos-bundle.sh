#!/usr/bin/env bash
#
# Build Adit.app and a dmg from an already-built release binary. macOS only —
# sips, iconutil and hdiutil all come from the system.
#
# Shared by ci.yml and release.yml so the bundle layout and Info.plist live in
# one place rather than being copied between workflows.
#
#   usage: installer/build-macos-bundle.sh 0.1.60
#
# Produces, in the working directory:
#   Adit.app/
#   adit_<version>_<arch>_unsigned.dmg
#
# NEITHER IS SIGNED OR NOTARISED. Both need an Apple Developer account and
# certificates, and until those exist Gatekeeper refuses to open the app by
# double-click. Right-click → Open no longer reliably works either; the
# dependable route is `xattr -dr com.apple.quarantine /Applications/Adit.app`.

set -euo pipefail

# Defaults to the workspace version; release.yml passes it explicitly so the
# package matches the tag even before the bump is committed.
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
[[ -n "$version" ]] || { echo "cannot determine version" >&2; exit 1; }
binary=target/release/adit-app
app=Adit.app

# GitHub's macOS runners are Apple Silicon, so an unlabelled dmg looks universal
# while being arm64-only — an Intel Mac downloads it and gets "cannot be opened"
# with nothing saying why. Name the architecture the way rustdesk does.
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64)          arch=x86_64 ;;
  *) echo "unknown macOS architecture $(uname -m)" >&2; exit 1 ;;
esac

if [[ ! -x "$binary" ]]; then
  echo "no release binary at $binary — run cargo build --release -p adit-app first" >&2
  exit 1
fi

# macOS wants an .icns; render one from the single PNG we ship.
rm -rf adit.iconset "$app"
mkdir -p adit.iconset
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" assets/icon.png \
    --out "adit.iconset/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) assets/icon.png \
    --out "adit.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns adit.iconset -o adit.icns

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/adit"
chmod +x "$app/Contents/MacOS/adit"
cp adit.icns "$app/Contents/Resources/adit.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Adit</string>
  <key>CFBundleDisplayName</key><string>Adit</string>
  <key>CFBundleIdentifier</key><string>com.github.weironz.adit</string>
  <key>CFBundleExecutable</key><string>adit</string>
  <key>CFBundleIconFile</key><string>adit.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

plutil -lint "$app/Contents/Info.plist"
test -x "$app/Contents/MacOS/adit"

hdiutil create -volname Adit -srcfolder "$app" -ov -format UDZO \
  "adit_${version}_${arch}_unsigned.dmg"
