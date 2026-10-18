# One Windows cmd-forwarding feasibility experiment

This is deliberately a fake-service gate. It starts neither UltraVNC nor a VNC
client, sends no credentials, and does not change the failed original VNC run.
The exact historical guest comes from run39145306679, artifact12143371403,
source8fc0866307f8d99c4bd52f5a3f888942b6ad45b1. The host helper uses the current
workflow source; both are recorded separately. The original official QEMU
installer/checksum/extraction policy is unchanged.

## Architecture evidence before testing

QEMU11.1.0's `slirp_guestfwd` creates a single `CharFrontend` at initialization
for the non-`cmd:` branch, and supplies no character event callback. A socket
chardev consequently opens its host connection immediately. That is not an
inetd-style fresh host TCP session for each guest connection. Moving the echo
listener before startup only hides the first failure and cannot prove RFB
session lifecycle. The old echo result does not establish fresh backends.

The official `cmd:` branch uses `slirp_add_exec`. In upstream libslirp4.9.1,
`fork_exec` creates native socket handles then passes one as all three stdio
arguments to GLib's `g_spawn_async_with_fds`. libslirp's Windows type is SOCKET;
GLib's Windows helper uses CRT `dup2` on integer file descriptors. Microsoft's
CRT documentation requires actual open-file descriptors and reports EBADF for
invalid descriptors. This is a concrete incompatibility risk, not proof of the
particular packaged Windows DLL build's behavior. No verified statement that
this vendor build supports `cmd:` is made before CI.

References, all primary sources:

- [QEMU11.1.0 slirp_guestfwd](https://github.com/qemu/qemu/blob/v11.1.0/net/slirp.c#L1047)
- [libslirp4.9.1 fork_exec](https://github.com/qemu/libslirp/blob/v4.9.1/src/misc.c)
- [libslirp Windows SOCKET type](https://github.com/qemu/libslirp/blob/v4.9.1/src/libslirp.h)
- [GLib2.84.0 Windows spawn helper](https://github.com/GNOME/glib/blob/2.84.0/glib/gspawn-win32-helper.c)
- [Microsoft CRT dup/dup2](https://learn.microsoft.com/en-us/cpp/c-runtime-library/reference/dup-dup2)

## Single bounded experiment

`vnc-windows-guestfwd-cmd.yml` is one five-minute Windows job. After the existing
pinned QEMU download/extraction, `cmd_probe.py` allows at most60 seconds including
cleanup. It preserves restrict=on and loopback-only host endpoints. It never
retries a failed bridge configuration or changes its transport to achieve green.

The fixed helper connects only to127.0.0.1:15911. It uses `read1` so a short fake
handshake cannot wait for a full buffer; output is raw bytes with explicit flush.
Its45-second lifetime is bounded. No tracebacks or diagnostics may be written
to stdout/stderr because libslirp connects stderr to the data stream too.

The fake server assigns a distinct ID for each actual accepted TCP connection.
Two fresh guest connections must each receive the correct distinct ID and ACK:
first the client actively half-closes and the fake backend must observe EOF
before closing; then the server actively half-closes, and both endpoints must
observe EOF. Exactly two real backend TCP accepts, no extra bytes or errors,
and both closed sessions are required. Original serial/stderr/result evidence
is kept whether the platform succeeds or fails. A failure ends this experiment;
it is not accepted as VNC or weak-network coverage.

## Recorded outcome and stop decision

The single [run39146789704](https://github.com/weironz/adit/actions/runs/39146789704)
on host-helper sourcebebd3d0961eede24de4159b89cfd09dbbaeca7fc failed after14.578s.
The guest booted with e1000/netem ready. The first fake connection reached early
EOF before its public marker; no backend TCP connection was accepted, and neither
the client-first nor server-first EOF assertion ran. QEMU's exact error was
`Slirp: fork_exec: Failed to execute helper program (No such file or directory)`.

This is an observed GLib helper-launch failure of the pinned unpacked QEMU
configuration. It does not prove universal Windows incompatibility or that the
suspected SOCKET-to-CRT-fd failure was reached. No PATH/environment changes,
alternative packaging, retries, mux or real-VNC execution followed. The fake
workflow is now manual-only. The unvalidated real-VNC integration was removed.
See the [raw evidence and independent dual-hash audit](../results/2026-10-19-windows-guestfwd-cmd/independent-audit.json).
