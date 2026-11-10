# RDP fixture results

## 2026-11-11 server pointer shape

[2026-11-11 server cursor](2026-11-11-rdp-server-cursor/README.md) proves
[#92](https://github.com/weironz/adit/issues/92): the client now draws the pointer
bitmap the server sends, hides the local OS pointer over the remote surface and
restores it when the pointer leaves, with the hotspot landing on the point that
was driven. The archived screenshots are measured against the exported bitmaps
pixel for pixel, and a read-only `audit.py` recomputes every number from the
pixels rather than trusting the harness.

The authorized acceptance target (the GRD host `192.168.72.10`) was unreachable
from this machine, so the GUI acceptance ran against a local X / FreeRDP shadow
fixture. See the entry's README for what that does and does not establish.
