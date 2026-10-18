#!/usr/bin/env bash
set -euo pipefail
[[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_ENVIRONMENT:-} == github-hosted && ${RUNNER_OS:-} == Linux ]] || { echo 'Disposable GitHub Linux runner only' >&2; exit 1; }
src=$(cd -- "$(dirname -- "$0")" && pwd)
out=$(realpath -m -- "$1")
[[ "$out" == "$RUNNER_TEMP"/* && ! -e "$out" ]] || { echo 'Fresh runner-temp output required' >&2; exit 1; }
mkdir -p "$out/root" "$out/apks"
root="$out/root"
python3 - "$src/sources.json" "$out" <<'PY'
import hashlib, json, pathlib, sys, urllib.request
sources=json.loads(pathlib.Path(sys.argv[1]).read_text())
out=pathlib.Path(sys.argv[2]); data=urllib.request.urlopen(sources['rootfs']['url'],timeout=60).read()
assert hashlib.sha256(data).hexdigest()==sources['rootfs']['sha256'], 'Rootfs checksum mismatch'
(out/'rootfs.tar.gz').write_bytes(data)
(out/'sources.json').write_text(json.dumps(sources,indent=2)+'\n')
PY
sudo tar -xzf "$out/rootfs.tar.gz" -C "$root"
sudo cp /etc/resolv.conf "$root/etc/resolv.conf"
python3 - "$src/sources.json" <<'PY' | sudo tee "$root/etc/apk/repositories" >/dev/null
import json,sys
print(json.load(open(sys.argv[1]))['apk_repository'])
PY
mapfile -t packages < <(python3 -c 'import json,sys; print("\n".join(json.load(open(sys.argv[1]))["packages"]))' "$src/sources.json")
# APK checks the official index and package signatures against the keys from
# the hash-pinned Alpine release. No allow-untrusted and no mutable container tag.
sudo timeout 120 chroot "$root" /sbin/apk update
# apk 2 fetch matches package names rather than version constraints. Let add
# solve the exact pins while retaining its signature-verified downloads.
sudo mkdir -p "$root/var/cache/apk"
sudo timeout 120 chroot "$root" /sbin/apk --cache-dir /var/cache/apk add --no-scripts "${packages[@]}"
sudo cp "$root"/var/cache/apk/*.apk "$out/apks/"
sudo chroot "$root" /sbin/apk info -vv | sort > "$out/package-versions.txt"
sudo cp "$root/boot/vmlinuz-virt" "$out/vmlinuz-virt"
kernel_version=$(find "$root/lib/modules" -mindepth 1 -maxdepth 1 -type d -printf '%f\n')
[[ "$kernel_version" != *$'\n'* && -n "$kernel_version" ]]
sudo depmod -b "$root" "$kernel_version"
sudo cp "$src/init" "$root/init"
sudo chmod 755 "$root/init"
sudo cp "$src/guest.py" "$root/guest.py"
# Bootstrap is serial-only. Packages are already installed; guest needs no DNS,
# package network, cloud-init, SSH, login prompts, or persistent disk.
sudo rm -f "$root/etc/resolv.conf"
sudo rm -rf "$root/var/cache/apk" "$root/boot"
(cd "$root" && sudo find . -print0 | sudo cpio --null --quiet -o --format=newc | gzip -1) > "$out/initramfs.cpio.gz"
python3 - "$out" "${GITHUB_SHA:?}" <<'PY'
import hashlib,json,pathlib,sys
out=pathlib.Path(sys.argv[1])
paths=[out/'vmlinuz-virt',out/'initramfs.cpio.gz',*sorted((out/'apks').glob('*.apk'))]
manifest={'source_sha':sys.argv[2],'scope':'Alpine rootfs and signed APKs assembled on Ubuntu runner; package dependencies frozen by artifact hashes, not a reproducible-build claim','files':{str(p.relative_to(out)):hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}}
(out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
PY
