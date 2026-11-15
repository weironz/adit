#!/usr/bin/env bash
# One layout run against this machine's own TermService, as the local test
# account. Usage: run.sh NAME MONITORS TARGET... < password-file
# Any session the account left behind is logged off first, so the server
# builds the virtual monitors for *this* run's layout instead of resizing an
# old session.
set -eu
here="$(cd "$(dirname "$0")" && pwd -W)"
root="$(cd "$here/../../../.." && pwd -W)"
name=$1 monitors=$2; shift 2
user="${ADIT_ORACLE_USER:-aditptr}"
for id in $(query session 2>/dev/null | awk -v u="$user" '$0 ~ u {for(i=1;i<=NF;i++) if ($i ~ /^[0-9]+$/) {print $i; break}}'); do
    logoff "$id" || true
done
sleep 3
rm -f "${ADIT_ORACLE_DIR:-C:/adit-oracle}/latest.json"
python "$root/tests/rdp/results/2026-11-16-rdp-xrdp-multimonitor-pointer/drive.py" \
    --probe "$root/target/release/examples/rdp_probe.exe" \
    --helper "$root/crates/adit-rdp/target/release/adit-rdp-host.exe" \
    --host 127.0.0.2:3389 --user "$user" --domain . --monitors "$monitors" \
    --oracle "python \"$here/oracle.py\" pointer" \
    --layout-oracle "python \"$here/oracle.py\" topology" \
    --out "$here/$name.jsonl" --settle-timeout 3 "$@" 2>&1 | tee "$here/$name.txt"
