# Disposable Windows guest netem tests

## Direct host-alias VNC test path

`vnc-windows-direct-impaired.yml` uses a separate `direct-host-alias-fake`
guest build and the `direct_*` controllers. The original guestfwd experiment
below is retained as historical evidence; the direct path does not use its
persistent character-device backend or spawn helpers.

```text
Windows production VNC probe -> 127.0.0.1:15912 -> QEMU hostfwd/Slirp
    -> guest TCP byte relay -> 10.0.2.2:15911 host alias
    -> fresh Windows TCP connection -> owned UltraVNC synthetic fixture
```

The guest has `10.0.2.15/32`, only a `10.0.2.2/32` unicast route, no default
route or DNS configuration, and IPv6 disabled. QEMU uses `restrict=off` for
host-alias access; this is not an additional host firewall or transparent TCP
router. The original TCP-termination and aggregate-egress limitations below
still apply. The fixed 90 ms queue setting is not a measured WAN RTT.

The workflow gates execution in this order:

1. Build and hash the current guest, including actual iproute2 rather than
   BusyBox's incompatible IPv6 route reporting.
2. Verify two independently accepted host TCP connections, opposite half-close
   orders and byte-identical 256 KiB tails after EOF.
3. On one owned QEMU, measure baseline echo/bulk, 80 ms delay, 2 Mbit/s rate,
   100% loss, natural kernel recovery, and the combined impairment.
4. Run one normal WAN/drag sample in baseline and impaired modes, then one
   restored lossless/static sample, each with explicit wrong-password rejection.
   A failure prevents matrix execution.
5. Run lossless/wan × static/scroll/drag, with ten normal baseline and ten
   impaired samples per cell, then one restored lossless/static sample. Preserve every failure;
   do not rerun individual samples to select a passing result.

Impaired mode sets guest eth0 aggregate egress to 90 ms, 5 Mbit/s and 0.5%
random loss. Each ten-sample impaired cell requires actual qdisc drops. Every
probe has before/after raw tc and TCP snapshots bound to this source/run,
controller nonce, exact process lifetime, executable hashes and listener owners.
Vendor fixture validation still requires actual full-frame RGB and target input
consumption. Drag includes an explicit button release during the existing steady
window. Read
each run's artifacts for its outcome; defining this workflow
does not establish acceptance.

Network changes require zero relay workers, no live kernel TCP states and empty
qdisc queues. Changing modes recreates an already idle qdisc because `replace`
inherits omitted netem options. The 100%-loss negative control can leave a
passive SYN_RECV after the host client aborts. It retains loss and waits up to
90 seconds for actual kernel expiry, checking the same strict idle conditions;
it does not flush a live connection. Normal idle remains bounded to 12 seconds.
[Linux documents the default passive-open timeout as 63 seconds](https://docs.kernel.org/6.12/networking/ip-sysctl.html#tcp-synack-retries-integer).

Only disposable GitHub-hosted Windows runners may invoke these controllers.
After merge, start the manual workflow with
`gh workflow run vnc-windows-direct-impaired.yml --ref main`; ordinary code or
documentation pushes do not start the costly matrix.
QEMU is extracted from the fixed checksum-verified official package; its
installer is not executed. Process, serial, command and whole-context deadlines
are bounded. Kernel diagnostics use a separate serial console and are retained
in `kernel-console.txt`; they cannot be interleaved into strict JSON RPC lines.
Cleanup queries the native Windows TCP table for the four owned
ports (15911–15914); only TIME_WAIT may remain. This does not validate Windows
physical NIC loss, OS sleep, UAC/lock screens, native GUI DPI or physical input.

## Original guestfwd echo/bulk PoC

This is an **echo/bulk feasibility experiment**, not VNC acceptance, a physical
Windows NIC test, or an OS sleep test. It runs only on disposable GitHub-hosted
runners. No local invocation, TAP adapter, capture driver, firewall/routing
change, installer execution, or system protection exception is required.
Any protection/download/extraction/execution failure is terminal. Do not change
packaging or transport to work around quarantine.

The workflow first assembles an Alpine Linux initramfs on Ubuntu. The Alpine
3.22.2 rootfs has a fixed official SHA256. Primary kernel/Python/tc versions are
fixed; APK verifies the official repository/index/package signatures with the
keys in that rootfs. The artifact retains every dependency APK, its exact
version and SHA256, plus kernel/initramfs hashes and source SHA. Dependencies
resolved from the signed repository are artifact-frozen, not an assertion that
future builds reproduce the same bytes. An unavailable fixed package fails.

The Windows job obtains the fixed upstream-listed Weilnetz QEMU build and
checks the official published SHA512 before extracting with 7-Zip. It records
Authenticode status rather than calling an expired certificate valid. The
installer is never executed. QEMU uses TCG (software CPU emulation), a serial
control channel and restricted user-mode networking. All three Windows listeners
bind only to loopback: the two payload endpoints below and the Python-owned
15913 serial character backend to which QEMU connects. The latter is not a
guest management IP service; it carries the guest serial console bytes.

```text
Windows echo client -> 127.0.0.1:15912 -> QEMU hostfwd/Slirp
    -> Linux TCP byte relay :15912 -> guestfwd 10.0.2.100:15911
    -> QEMU host TCP -> Windows echo server 127.0.0.1:15911
```

These are **separately terminated TCP connections**. Slirp and the byte relay
ACK and buffer independently. Netem changes actual packets emitted by **guest
eth0**, affecting Linux/Slirp TCP retransmission/backpressure; it does not drop
packets in the Windows loopback TCP segments. Guest egress is shared by both
relay legs, so the aggregate rate is not a per-direction WAN bandwidth. A
request/reply traverses guest egress twice. Guest ingress is not impaired in
this first PoC. No transparent, end-to-end Windows TCP-loss claim is valid.

The experiment requires all of these before success:

- byte-identical small echo and 256 KiB bulk control;
- fixed 80 ms guest-egress delay with independent connection-plus-echo elapsed increase;
- 2 Mbit/s aggregate guest-egress shaping with a slower measured bulk transfer;
- 100% loss prevents payload delivery and increments actual netem drop counters;
- restoring baseline allows a new byte-identical echo;
- guest `tc -s -j` and `/proc/net/snmp` snapshots before and after phases,
  including the raw failed phase, plus complete serial and QEMU stderr logs.

Boot, serial replies, sockets, package assembly, workflow steps and process
cleanup have deadlines. The workflow keeps original failures as artifacts and
does not retry failing phases. TCG load and timer resolution limit performance
interpretation; the conservative assertions establish a working impairment
path, not reliable WAN latency percentiles. Random loss, symmetric impairment,
VNC integration and full workload matrices require additional evidence.
Each exchange includes a new TCP connection as well as its echo. Keep the cold
first observation and coarse Windows timer readings, including zero readings,
in the raw result; these are not pure payload RTT or submillisecond measurements.

Official references:

- [QEMU Windows builds](https://www.qemu.org/download/#windows)
- [Pinned build and published checksums](https://qemu.weilnetz.de/w64/)
- [TCG accelerator support](https://www.qemu.org/docs/master/system/introduction.html)
- [User networking, restrict, hostfwd and guestfwd](https://www.qemu.org/docs/master/system/qemu-manpage.html)
- [Alpine rootfs checksum](https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.2-x86_64.tar.gz.sha256)
- [Upstream netem manual and limitations](https://github.com/iproute2/iproute2/blob/main/man/man8/tc-netem.8)

## Failed real-VNC integration and stopped cmd experiment

The real-VNC gate [39145306679](https://github.com/weironz/adit/actions/runs/39145306679)
failed before any VNC/authentication sample: the persistent guestfwd socket
backend connected to the not-yet-started listener during QEMU initialization.
More fundamentally, that backend is a single character device, not a fresh
host TCP connection for each new guest connection. The prior successful echo
PoC does not establish independent RFB session lifecycle.

A single fake-only [cmd experiment39146789704](https://github.com/weironz/adit/actions/runs/39146789704)
then booted the guest successfully but failed before the first public marker.
QEMU reported `Slirp: fork_exec: Failed to execute helper program (No such file or directory)`;
there were zero fake backend TCP accepts, so neither EOF lifecycle case ran.
This establishes a helper-launch failure in this unpacked Windows configuration,
not that all Windows cmd forwarding is unsupported or that the suspected
SOCKET/CRT-fd incompatibility was actually exercised.

The unvalidated real-VNC workflow/controller/port hooks have been removed. No
VNC weak-network matrix ran or passed. The working echo PoC remains an echo
packet-path experiment only. The bounded fake tool and
[architecture reference](guestfwd-cmd-reference.md) remain for reproducibility,
but the fake workflow is manual-only and this path is stopped without retries,
PATH changes, mux protocols, guard relaxation or protection exceptions.

[Archived raw cmd failure and independent audit](../results/2026-10-19-windows-guestfwd-cmd/independent-audit.json)
include original download and LF-normalized stored hashes separately. The
real gate failure remains separately preserved in repository result history.

The later isolated dependency investigation found that the fixed QEMU package
omits both GLib spawn helpers. An official MSYS2 package was authenticated
against its signed keyring, and its GLib DLL exactly matched QEMU's bundled DLL.
One fresh fake-only run with the two matching helpers then failed at
`Slirp: fork_exec: Failed to dup() in child process (Bad file descriptor)`.
It accepted **zero** backend connections; neither EOF lifecycle case passed.
This is a different, later failure than the original missing-helper error.
The preparation success is not fake-gate or VNC acceptance. No further retries
or product changes followed. See the [exact raw failure and source](../results/2026-10-19-windows-guestfwd-matched-failure/independent-audit.json),
[package inventory](../results/2026-10-19-windows-qemu-glib-layout/independent-audit.json)
and [dependency authenticity audit](../results/2026-10-19-windows-glib-matching-dependency/independent-audit.json).
