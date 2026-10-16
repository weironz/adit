# VNC fixture results

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
