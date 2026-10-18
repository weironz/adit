#!/usr/bin/env bash
set -euo pipefail
[[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_ENVIRONMENT:-} == github-hosted && ${RUNNER_OS:-} == Linux ]] || { echo 'Disposable GitHub Linux runner only' >&2; exit 1; }
src=$(cd -- "$(dirname -- "$0")" && pwd)
mode=${2:-echo}
case "$mode" in echo|direct-host-alias-fake) ;; *) echo 'Unknown fixed guest build mode' >&2; exit 1 ;; esac
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
# Use the guest's kmod build: Ubuntu depmod can silently produce an empty
# index for Alpine's .ko.gz modules even though the drivers are present.
sudo chroot "$root" /sbin/depmod -a "$kernel_version"
for module in e1000 virtio_net sch_netem; do
  sudo grep -Eq "(^|/)$module[.]ko[.]gz:" "$root/lib/modules/$kernel_version/modules.dep"
done
sudo cp "$root/lib/modules/$kernel_version/modules.dep" "$out/modules.dep"
if [[ "$mode" == direct-host-alias-fake ]]; then
  sudo cp "$src/direct-init" "$root/init"
  sudo cp "$src/direct_guest.py" "$root/guest.py"
else
  sudo cp "$src/init" "$root/init"
  sudo cp "$src/guest.py" "$root/guest.py"
fi
sudo chmod 755 "$root/init"
# Bootstrap is serial-only. Packages are already installed; guest needs no DNS,
# package network, cloud-init, SSH, login prompts, or persistent disk.
sudo rm -f "$root/etc/resolv.conf"
sudo rm -rf "$root/var/cache/apk" "$root/boot"
(cd "$root" && sudo find . -print0 | sudo cpio --null --quiet -o --format=newc | gzip -1) > "$out/initramfs.cpio.gz"
python3 - "$out" "${GITHUB_SHA:?}" "$mode" <<'PY'
import hashlib,json,pathlib,sys
out=pathlib.Path(sys.argv[1])
paths=[out/'vmlinuz-virt',out/'initramfs.cpio.gz',out/'modules.dep',*sorted((out/'apks').glob('*.apk'))]
manifest={'source_sha':sys.argv[2],'scope':'Alpine rootfs and signed APKs assembled on Ubuntu runner; package dependencies frozen by artifact hashes, not a reproducible-build claim','files':{str(p.relative_to(out)):hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}}
if sys.argv[3] == 'direct-host-alias-fake':
    manifest['build_mode']=sys.argv[3]
    manifest['guest_source_files']={name:hashlib.sha256((out/'root'/name).read_bytes()).hexdigest() for name in ('init','guest.py')}
    manifest['network_policy']={'address':'10.0.2.15/32','only_unicast_route':'10.0.2.2/32 dev eth0','default_route':False,'dns_configured':False,'qemu_restrict':False,'note':'Guest route narrowing is not complete host-port isolation. QEMU restrict=off differs from the original echo guest isolation.'}
(out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
PY
