# RDP rendering forensics: capture once, replay offline

The scroll-ghosting bug (2026-08) burned three capture/test round-trips before
the tooling below existed, and was closed the same day it did: the final fix
was proven against a captured session *before* it was deployed. When the next
rendering artefact appears, do not iterate live — take **one** capture and do
everything else offline.

A clean run in `mstsc` does **not** exonerate this pipeline: mstsc may
negotiate an entirely different codec (H.264/AVC), so it only proves the
server can render the page for *some* client, not that the progressive +
ClearCodec + bitmap-cache path we exercise is being served correctly.

## 1. Capture

Close Adit, then:

```powershell
Remove-Item "$env:APPDATA\Adit\frames\*","$env:APPDATA\Adit\clear-*","$env:APPDATA\Adit\progressive-*","$env:APPDATA\Adit\rdp-helper.log" -Force -ErrorAction SilentlyContinue; $env:ADIT_RDP_FRAMES="$env:APPDATA\Adit\frames"; $env:ADIT_RDP_DUMP="1"; & "$env:LOCALAPPDATA\Programs\Adit\Adit.exe"
```

Reproduce the artefact **briefly** — connect, trigger it once, pause a few
seconds so the screen settles, close Adit. A 30-second capture is worth more
than a five-minute one: the dump caps (65536) are generous but a busy session
still produces a lot of trace, and analysis time scales with it.

**Always clean the old capture first** (the `Remove-Item` above). The dump
counter restarts at 0 per process, so a second session silently overwrites
`progressive-0.bin` onwards — a mixed capture cost one full analysis round
once.

What lands under `%APPDATA%\Adit`:

| Artefact | Contents |
|----------|----------|
| `rdp-helper.log` | Uncapped paint trace: one line per canvas write with `source` (`progressive` / `progressive-upgrade` / `clear` / `solid-fill` / `cache-to-surface` / `surface-to-cache` / `bitmap-update`), cache `slot`, rectangle, solid-fill colour (`r= g= b=`), plus surface created/mapped/deleted. |
| `progressive-N.bin` + `.txt` | Every RemoteFX Progressive stream, in arrival order, with outcome/surface sidecar. |
| `clear-NNN.bin` + `.txt` | Every ClearCodec stream, with `rect=WxH at x,y` sidecar. |
| `frames/frame-NNN.raw` + `.txt` | Ring of the last 40 emitted framebuffers (raw RGBA, sidecar is `WIDTHxHEIGHT`). Ground truth for what the app actually displayed. |

Captures are pictures of the desktop. They stay on the machine and never enter
the repo.

## 2. Replay

The workhorse is `crates/adit-rdp/tests/merged_dump.rs` in log-driven mode: it
re-executes the session from the paint trace — codec dumps decoded in log
order, solid fills painted from their logged colour, and both bitmap-cache ops
simulated as real canvas copies, exactly like the live helper. On the capture
that settled the tile-application question it matched the live framebuffer to
99.7% (the remainder arrived after the last dump).

```bash
ADIT_MERGE_DIR=$APPDATA/Adit ADIT_MERGE_LOG=$APPDATA/Adit/rdp-helper.log \
ADIT_MERGE_SURFACE=1908x1152 \
  cargo test --manifest-path crates/adit-rdp/Cargo.toml \
    --test merged_dump -- --ignored --nocapture
```

Outputs `merged-sim.png` (final canvas) into the capture directory. Extra
levers, all env vars:

- `ADIT_MERGE_SIM_SNAPSHOT=HH:MM:SS.mmm` — save `sim-snapshot.png` the first
  time the log clock reaches that value. Use it to see what the canvas held at
  a `surface-to-cache` moment, i.e. what the bitmap cache captured.
- `ADIT_MERGE_PROBE="x,y[;x,y]"` — save every decoded progressive tile
  covering those points as `probe-progressive-N-tXxY.png`. This is the
  decoder's output *before* compositing: it separates "the decoder authored
  the bad pixels" from "the compositor lost the good ones".
- `ADIT_MERGE_CLIP=1` — legacy clip-everything compositing, kept so an old
  behaviour can be reproduced for comparison.

`tests/progressive_dump.rs` and `tests/clearcodec_dump.rs` replay one codec in
isolation; reach for them only when the question is about a single decoder,
because each lies by omission about everything it doesn't replay.

**Fix workflow**: change the decoder or compositing rule, re-run the replay on
the same capture, look at `merged-sim.png`. A fix that doesn't clean the sim
will not clean the live screen; a fix that does has already been tested
against the real byte stream.

## 3. Analysis moves that have actually cracked cases

In escalating order of effort:

1. **Convert `frames/*.raw` to PNG and look.** Python/PIL one-liner
   (`Image.frombytes('RGBA', (W, H), data)`). Whether the artefact is in the
   framebuffer or only on screen splits renderer bugs from decode bugs in one
   step.
2. **Final-writer census.** Parse the paint trace; for each 64px cell record
   the last op that wrote it. Frozen residue has a signature: final writer is
   `cache-to-surface` from a slot filled seconds earlier.
3. **Lineage backtrace.** For a bad pixel, walk backwards: last paint covering
   it; if that is a cache restore, jump to the slot's fill (same offset within
   the rect), and continue before the fill time. Bottoms out at the codec op
   that authored the pixels. Slot numbers are in the trace for exactly this.
4. **Probe tile PNGs** (`ADIT_MERGE_PROBE`) to see what the decoder produced
   for the authoring stream and every later stream that touched the tile —
   compare against what the canvas kept.
5. **Hand-parse the raw stream** when op *semantics* are in doubt. The
   progressive block layout (REGION rects, tile headers) parses in ~30 lines
   of Python; the ghost root cause was settled by reading a rect
   `(317,448,282x64)` under a full-tile re-encode straight out of the bytes.

Two traps this loop has already paid for:

- **Never trust a capped counter.** A probe capped at 60 lines reported the
  cache path as "60 calls at session start" when the truth was 5714 across the
  whole session; that false exoneration cost two rounds. Everything here is
  uncapped or high-capped on purpose.
- **The paint trace is authority, not the dumps' mtimes.** Same-tick mtimes
  transpose, and codec state is order-sensitive.

## 4. Case files

- Duplicated-icon scroll residue + half-old/half-new cells: both halves of the
  hybrid tile-application rule (fresh passes blit whole, upgrades clip to the
  region rects) — see the progressive table in
  [`crates/adit-rdp/IRONRDP-PATCHES.md`](../crates/adit-rdp/IRONRDP-PATCHES.md)
  and commits `df55dd8`, `e344d80`.
- The earlier mosaic/flicker campaign (NSCodec from scratch, ClearCodec
  destination seeding, progressive quant fixes) is also indexed in
  `IRONRDP-PATCHES.md`.
