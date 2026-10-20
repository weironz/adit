# VNC fixture results

## Native foreground input acceptance

[2026-10-21 grab/IME/AltGr report](2026-10-21-grab-ime-altgr/README.md) records
actual Windows preedit/Chinese commits with capture enabled, the AltGr auxiliary
Ctrl fix, final-source system-key regression, and owned VM cleanup. It retains
the failed predecessor and the unsupported pending-dead-key boundary. Its
synthetic RFB peer does not replace vendor application Unicode consumption.

[The preceding foreground matrix](2026-10-21-foreground-system-keys-final/README.md)
contains the full left/right modifier, ownership, escape and focus matrix.

## 2026-10-16 fixed-window calibration (partial run)

The two `linux-fixed-window` JSON files preserve raw observations and the exact
source/binary/environment identity. They are **not a completed WAN matrix**.

- 180/180 loopback samples completed: three servers, two quality policies, three
  workloads, ten fresh connections per cell.
- The WAN phase stopped at x11vnc/lossless/static sample 9. Eight samples completed;
  the ninth had painted 453600 of 480000 pixels (94.5%) at the five-second cutoff.
  The earlier completed WAN samples included 4625 ms and 4591 ms first coverage.
- The run records 189 observations including that failure. No failed sample was
  removed. A separate 15-second diagnostic completed first coverage at 4963 ms;
  this does not retroactively change the failed sample.
- The original probe used one five-second total window. Slow initial transfer
  left unequal steady-workload time. The updated runner separates a 15-second
  first-screen deadline from five seconds of steady work **after** full coverage;
  those future results must be stored separately with their own binary identity.
- Status text in the original raw stdout was decoded with replacement characters
  on Windows; numeric metrics and the original JSON evidence are retained.

These are headless debug-build session measurements. They do not establish GPU
presentation FPS, mouse-to-photon latency, reliable P95, or Windows server
compatibility. WAN/JPEG quality must not be assumed to reduce bytes or CPU for
every server/workload. See [fixture method](../README.md).

## Completed steady-window matrix

[2026-10-17 Linux report](2026-10-17-linux-steady-report.md) summarizes all 360
successful observations with per-cell medians/min/max and explicit measurement
limits. The raw JSON and fixed binary identity remain beside it.

## Independent CPU supplement

[2026-10-17 CPU report](2026-10-17-linux-cpu-report.md) summarizes 180 additional
loopback observations with process CPU, first coverage and update bytes. The
original 360-sample matrix has no retroactively generated CPU metrics.

## Windows service and Unicode clipboard smoke acceptance

[Windows TightVNC observations](2026-10-17-windows-tight-acceptance.json) record
real TightVNC 2.8.90 service-mode connection, UAC secure desktop, lock/unlock,
negotiated vendor UTF-8 clipboard in both OS directions, and protected-vault RDP
cold-load authentication. Binary hashes distinguish this patch from earlier
Linux benchmark builds. These smoke observations do not complete the Windows
weak-network matrix or native DPI/IME/keyboard-hook/vault-dialog acceptance.
