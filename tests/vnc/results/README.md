# VNC fixture results

## 2026-11-12 UltraVNC staged acceptance: compatibility mode passes 5/5

[2026-11-12 UltraVNC staged](2026-11-12-uvnc-unicode-staged/README.md) is the
real-machine answer to the last open item of
[#205](https://github.com/weironz/adit/issues/205): portable UltraVNC 1.8.3.0 in
the VM's **console** session, the production `TEXT` entry, and an actual WinForms
TextBox. With the BMP compatibility mode on, both non-BMP cases reject the whole
commit — `input_delta` press/release/keypress/text_changes all zero — and the
other three consume their exact scalars. Staged counting also explains the old
"24 releases over the lifecycle": every case shows one unrelated key pair in the
**connection** phase, the input phase is exact, and the close phase is always
zero. The default Unicode run kept the vendor's `U+1F60A → U+F60A` truncation as
a failure and lost its first case to a window-readiness flake, which the fixture
now waits 30 s for (readiness only) instead of 10 s.

## 2026-11-11 TightVNC application mode could not listen on the host

[2026-11-11 TightVNC application mode blocked](2026-11-11-tight-app-mode-blocked/README.md)
records an attempt to rerun the staged-ACK compatibility fixture for
[#205](https://github.com/weironz/adit/issues/205) on the development
workstation. Both hash-pinned, signature-checked servers start and never bind a
port there — TightVNC 2.8.90 leaves `127.0.0.1:15911` unbound and the fixture
reports `TightVNC app listener readiness failed`, and the official UltraVNC
1.8.3.0 archive fails the same way with a log that stops before listening. The
account is not elevated and the session is RDP, which is the environment to
change, not the client. No TEXT case ran, so this archive is **not** acceptance
evidence and says nothing about the compatibility policy itself.

## 2026-11-06 glyph ghosting on the remote VNC surface

[2026-11-06 glyph ghosting](2026-11-06-vnc-glyph-ghosting/README.md) proves the
texture sampler was the cause of [#230](https://github.com/weironz/adit/issues/230):
nearest-neighbour copies texels, so at a fractional magnification a one-pixel
stroke lands on two screen pixels in places and the text reads as doubled. The
archived compositor screenshots are measured two ways that need no model of the
renderer — the colour set against the framebuffer's own, and the share of device
columns that are byte-identical copies of their left. The 1:1 control is
pixel-identical between the two binaries.

## Native foreground input acceptance

[2026-10-21 stateless shortcut translation](2026-10-21-stateless-system-keys/README.md)
proves pending German acute + Win+R/Win+E without consuming the local accent,
plus actual English/Chinese layouts, IME, escape and focus regression. It retains
130 wire events and a newly observed extra Ctrl around AltGr; that AltGr run is
explicitly incomplete and does not replace the prior clean pass.

[2026-10-21 Linux Unicode text consumption](2026-10-21-linux-unicode-text/README.md)
records 12/12 actual native Entry model comparisons across x11vnc, TigerVNC and
TurboVNC, including CJK, non-BMP and combining text. Preinput fixture failures and
owned container cleanup remain alongside the successful results.

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
