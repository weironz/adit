# RDP fixture results

## 2026-11-11 server pointer shape

[2026-11-11 server cursor](2026-11-11-rdp-server-cursor/README.md) proves
[#92](https://github.com/weironz/adit/issues/92): the client now draws the pointer
bitmap the server sends, hides the local OS pointer over the remote surface and
restores it when the pointer leaves, with the hotspot landing on the point that
was driven. The archived screenshots are measured against the exported bitmaps
pixel for pixel, and a read-only `audit.py` recomputes every number from the
pixels rather than trusting the harness.

The authorized acceptance target (the GRD host `192.168.72.20` / `will-opt7010`)
was unreachable from this machine, so the GUI acceptance ran against a local X /
FreeRDP shadow fixture. See the entry's README for what that does and does not
establish.

## 2026-11-13 multi-monitor against a real Windows server

[2026-11-13 Windows multi-monitor](2026-11-13-rdp-windows-multimonitor/README.md)
covers [#233](https://github.com/weironz/adit/issues/233) against a **real**
Windows RDP server — this machine's own `TermService` on `3389`, not a fixture.
Three of the issue's four items are now closed there: the server opens its
virtual desktop from the advertised GCC block, honours a post-connect
DisplayControl layout change, and the layout survives a reconnect through the
profile store.

The one item still open is the pointer coordinates, and the reason is specific:
Windows keeps the cursor on the server side and does not paint it into the
virtual desktop, so no coordinate-bearing pointer update comes back. That item
is covered elsewhere — on a FreeRDP shadow server whose own X11 pointer lands
on the sent coordinates pixel for pixel — but not yet on the same server as the
other three.

The entry's README also records a side effect worth knowing about before
probing this machine: **a Windows RDP connect takes over the physical console**,
and restoring it needs an elevated `tscon 1 /dest:console`.
