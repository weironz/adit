#!/usr/bin/env bash
# Runs the RDP pointer-coordinate acceptance harness against an authorized host.
#
# The password is piped on stdin, never passed as an argument: argv is
# readable through the process list by anything on the machine.
#
# Usage:
#   ADIT_RDP_FIXTURE_HOST=host:port \
#   ADIT_RDP_FIXTURE_USER=name \
#   scripts/run_pointer_acceptance.sh < password-file
#
# The host must be one the guard in `adit-domain` admits — a private, loopback
# or RFC 6598 shared-address-space address. The harness itself does not
# enforce that, because it is a GUI probe and its own arguments are not a
# security boundary; the guard belongs in one place, and this is it.
set -eu

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

: "${ADIT_RDP_FIXTURE_HOST:?the authorized host, as host:port}"
: "${ADIT_RDP_FIXTURE_USER:?the account name on that host}"

# The helper is a separate workspace (IronRDP's dependency pins cannot
# coexist with russh's), so it has to be built with its own manifest and lands
# in its own target directory.
helper="${ADIT_RDP_HELPER:-$root/crates/adit-rdp/target/release/adit-rdp-host.exe}"
if [[ ! -f "$helper" ]]; then
    echo "the RDP helper is missing: $helper" >&2
    echo "build it with: cargo build --release --manifest-path crates/adit-rdp/Cargo.toml" >&2
    exit 1
fi
export ADIT_RDP_HOST="$helper"

# The helper's pointer probe is an `info` line and the helper's default
# filter is `warn`, so without this the harness would find nothing in the
# log and every step would fail for a reason that has nothing to do with the
# mapping.
export RUST_LOG="${RUST_LOG:-warn,adit_rdp=info}"

cargo build --release --example rdp_pointer_acceptance \
    --features native-rdp-pointer-acceptance

./target/release/examples/rdp_pointer_acceptance.exe
