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

## Real VNC integration after calibration

The separate `vnc-windows-impaired.yml` workflow first gates one lossless/static
sample, then permits six independent jobs (two quality presets by three workloads,
ten fresh sessions each). Every job calibrates its own **same QEMU instance** with
echo/bulk controls before replacing its owned echo listener with the pinned
UltraVNC portable server. The normal probe connects to QEMU port15912; UltraVNC
continues to listen on15911 under the existing containment checks. A fresh
same-source network evidence file ties the forwarder to its live QEMU PID,
executable path/hash, loopback mappings and actual tc settings.

The VNC condition is guest-egress delay90ms, aggregate5Mbit/s and random loss0.1%.
This is **not** a90ms end-to-end RTT or a per-direction5Mbit/s link. The Slirp and
relay TCP termination limits above still apply. Quality `wan` is an independent
VNC pixel-encoding preset, not a network profile. Before and after each actual
VNC probe the owning controller records raw tc/SNMP snapshots; the final summary
separately reports configured loss and observed qdisc drops. Zero drops are
reported as **loss not observed**, never changed to a higher-loss passing run.

Existing15-second complete-RGB and actual Tk consumption requirements remain.
Each job first runs the same quality/workload and sample count with guest netem
removed (ten matched controls in full jobs, one in the narrow gate), then the
impaired phase. Normal probes additionally need the entire requested steady window, an actual
Connected final state, zero disconnects and no connection error. Drag consumption
is the existing end-of-probe target assertion; it does not establish that release
happened inside the steady timing window. A wrong password must explicitly fail
VncAuth rather than time out. After every requested impaired sample finishes,
all relay connections and qdisc queues must drain before netem is removed.
Raw `/proc/net/tcp` snapshots also require all non-LISTEN/non-TIME_WAIT guest
sockets (including calibration SYN_RECV) to disappear within12 seconds before
the next condition; no negative-control retransmits may contaminate VNC drops. A
fresh real VNC full-RGB sample and explicit wrong-auth rejection then verify the
restored baseline. Both the impaired phase and restored phase retain original
failures and cleanup outcomes. This is still SessionManager/RFB proof, not GPU,
physical input, transparent Windows TCP congestion or Windows OS sleep acceptance.
