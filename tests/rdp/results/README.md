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

## 2026-11-14 multi-monitor against a real GNOME Remote Desktop host

[2026-11-14 GRD headless multi-monitor](2026-11-14-rdp-grd-headless-multimonitor/README.md)
covers [#233](https://github.com/weironz/adit/issues/233) against a **real GRD host**
in headless remote-login mode — the same target `mstsc` reaches. Three of the
issue's four items are closed there: the server opens its virtual desktop from
the advertised GCC block (two `1200x800` surfaces at `(0,0)` and `(1200,0)`),
follows a **mid-session** layout change (the client's `2 × 1200x800 →
1 × 1200x800` switch reaches the server, which disposes one virtual monitor
and rebuilds the other at `[0, 0, 1200, 800]`), and the layout survives a
reconnect through the profile store.

The pointer coordinates are covered on the **client** side, deterministically:
the acceptance tool drives the real OS pointer and reads back the coordinate
that reaches the wire, and all four points (each screen's near and far edge)
land exactly where they were aimed.

The **server** half of that item is still open, and the reason is specific to
this host: GRD paints no cursor into the framebuffer (three snapshots at
different pointer positions are pixel-identical), `GrdRdpCursorUpdate` has no
position field, GRD logs no pointer coordinates, and every host-side pointer
oracle is blocked. The host does not produce the observable. That item needs a
physically dual-head server.
