#!/usr/bin/env bash
#
# Build Adit.app and a dmg from an already-built release binary. macOS only —
# sips, iconutil and hdiutil all come from the system.
#
# Shared by ci.yml and release.yml so the bundle layout and Info.plist live in
# one place rather than being copied between workflows.
#
#   usage: installer/build-macos-bundle.sh 0.1.60 [rust-target-triple]
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
# Empty and unset both fall back, so a caller that only wants to pass the target
# triple can leave the version as ''.
version="${1:-}"
[[ -n "$version" ]] || version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ -n "$version" ]] || { echo "cannot determine version" >&2; exit 1; }
app=Adit.app

# Optional second argument: a Rust target triple, which is how the Intel dmg
# gets built. GitHub's macOS runners are all Apple Silicon now, but Apple's
# clang targets either architecture from either host, so cross-compiling beats
# depending on an Intel runner image continuing to exist.
target="${2:-}"
binary="target/${target:+$target/}release/adit-app"

if [[ ! -x "$binary" ]]; then
  echo "no release binary at $binary — run" >&2
  echo "  cargo build --release -p adit-app${target:+ --target $target}" >&2
  exit 1
fi

# Take the architecture from the binary rather than from uname or from the
# triple above. The dmg used to carry no architecture at all while being
# arm64-only, so an Intel Mac downloaded it and got an unexplained refusal to
# open; labelling it from the bytes it actually ships makes that mismatch
# impossible rather than merely unlikely.
case "$(lipo -archs "$binary")" in
  arm64)  arch=aarch64 ;;
  x86_64) arch=x86_64 ;;
  *) echo "unexpected architecture '$(lipo -archs "$binary")' in $binary" >&2; exit 1 ;;
esac

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
