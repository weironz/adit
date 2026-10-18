# Disposable Windows guest netem packet-path PoC

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
