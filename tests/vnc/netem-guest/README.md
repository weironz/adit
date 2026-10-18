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
control channel and restricted user-mode networking. Both Windows listeners
bind only to loopback.

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
- fixed 80 ms guest-egress delay with independent echo RTT increase;
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

Official references:

- [QEMU Windows builds](https://www.qemu.org/download/#windows)
- [Pinned build and published checksums](https://qemu.weilnetz.de/w64/)
- [TCG accelerator support](https://www.qemu.org/docs/master/system/introduction.html)
- [User networking, restrict, hostfwd and guestfwd](https://www.qemu.org/docs/master/system/qemu-manpage.html)
- [Alpine rootfs checksum](https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.2-x86_64.tar.gz.sha256)
- [Upstream netem manual and limitations](https://github.com/iproute2/iproute2/blob/main/man/man8/tc-netem.8)
