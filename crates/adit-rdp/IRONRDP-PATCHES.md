# IronRDP patches & additions (Adit)

Adit drives native RDP through the [IronRDP](https://github.com/Devolutions/IronRDP)
crate stack. Two things IronRDP can't yet do out of the box are needed to connect
to **GNOME Remote Desktop's system ("Remote Login", headless) mode**:

1. it mandates the **EGFX graphics pipeline**, and
2. it authenticates via a **Server Redirection** handover secured with **RDSTLS**
   and one-time credentials — none of which IronRDP implements.

To keep upstream easy to re-adopt, everything Adit-specific is either an
**additive module** in this crate or a **narrowly-scoped, marked patch** in a
single vendored crate. This file is the map. When IronRDP grows native support
(tracking issues below), delete the corresponding piece and switch back to the
crates.io connector.

> For the **design + pitfalls narrative** (the connection flow, wire details, and
> the debugging journey behind these patches), see
> [`docs/rdp-gnome-remote-desktop.md`](../../docs/rdp-gnome-remote-desktop.md).
> This file is the mechanical patch reference; that one is the "why".

## What lives where

### Additive (no upstream changes) — `crates/adit-rdp/src/`

| File | Purpose | Drop when |
|------|---------|-----------|
| `egfx.rs` | `GraphicsPipelineHandler` compositing EGFX bitmaps into a shared framebuffer; wired onto drdynvc in `session.rs`. | IronRDP ships a ready-to-use EGFX→framebuffer client. |
| `redirect.rs` | Parses the **Server Redirection PDU** (MS-RDPBCGR 2.2.13, pduType `0xa`) off the I/O channel — IronRDP's `ShareControlPdu` can't decode it. | IronRDP [#139](https://github.com/Devolutions/IronRDP/issues/139) (Server Redirection) lands. |
| `rdstls.rs` | The **RDSTLS** security exchange (recv Capabilities → send password AuthReq → recv AuthRsp), ported from FreeRDP `rdstls.c`. Runs on the TLS stream before MCS. | IronRDP implements RDSTLS (only the `SecurityProtocol::RDSTLS` flag exists today). |
| `session.rs` `run_session` | Reconnect loop that follows a redirection: carries the routing token, builds the one-time RDSTLS creds, and reconnects. | Same as `redirect.rs`. |

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-egfx/`

Crates.io `ironrdp-egfx` 0.3.0 discards the monitor definitions in
`ResetGraphics` before invoking its handler. The marked patch in `src/client.rs`
adds a backward-compatible handler callback carrying those definitions, so Adit
can verify the server's actual topology before granting pointer input. Remove
the patch when upstream exposes the same information. The synthetic
`issue97_real_egfx_reset_pdu_preserves_reported_monitor_geometry` test exercises
the encoded PDU through the graphics channel decoder.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-svc/`

Crates.io `ironrdp-svc` 0.8.0 only watches LAST while appending received chunks;
it neither bounds the accumulated data nor checks the advertised message length.
The `ADIT PATCH` in `src/lib.rs` caps each uncompressed receive message at
**128 MiB**, checks the repeated total length and accumulated length before
copying, and requires a complete message to match its advertised length.
Capacity grows with received bytes using fallible reservation, never with the
peer's length claim alone. Every reassembly error, including a truncated header,
drops both the partial allocation and sequence state. A new FIRST also drops
an unfinished message so its bytes cannot contaminate the new one.

Compatibility is deliberately broader than the earlier strict draft:

- [MS-RDPBCGR 3.1.5.2.2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/334495b7-47b9-4e91-8d08-05d46285799b)
  permits a complete PDU with neither FIRST nor LAST when no sequence is active.
  Length-matching LAST-only messages remain accepted as in the published crate.
  Middle chunks without either flag remain part of the active sequence.
- Empty FIRST|LAST messages, empty fragments, and a message that reaches its
  advertised size before a separate empty LAST are accepted. Repeated FIRST is
  a resynchronization point, rather than an error that loses the new message.
- [MS-RDPBCGR 2.2.6.1.1](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/f125c65e-6901-43c3-8071-d7d5aaee7ae4)
  defines SUSPEND/RESUME as transport flow control. Header-only controls are
  consumed as `Ok(None)` **before** data-length and FIRST/LAST processing. They
  neither dispatch application data nor alter the pending buffer or expected
  length, including when the control also carries FIRST|LAST. Thus
  `FIRST(4, "a") -> SUSPEND -> RESUME -> LAST(4, "bcd")` yields exactly `"abcd"`.
  Idle controls likewise do not establish a fragment sequence. Control bits on
  data-bearing chunks introduce no flag-based rejection: those bytes follow
  the existing bounded data-reassembly path.
- IronRDP 0.11.0's `ironrdp-session/src/x224/mod.rs` passes SVC bytes directly to
  this crate without bulk decompression. COMPRESSED is therefore **explicitly
  rejected**, even when its encoded size happens to equal the declared size.
  FLUSHED and compression-package metadata on **uncompressed** data remain
  accepted: [MS-RDPBCGR 3.1.8.2.1](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/9355a663-ef22-4431-afeb-d72ac68f25fd)
  does not require FLUSHED to be accompanied by COMPRESSED. SHOW_PROTOCOL,
  SHADOW_PERSISTENT, and other non-control metadata keep the existing payload
  interface; this patch adds no new flag whitelist or chunk-size restriction.

The bound covers reassembled content and requested buffer capacity **per channel**;
it is not a connection-wide budget, an unfinished-message timeout, or a bound on
allocator overhead, transient reallocations, input storage, or consumer copies.
128 MiB allows a raw 7680 x 4320 x 4-byte image plus its inner protocol header;
clipboard consumers retain their separate limits. This patch does not implement
SVC bulk decompression or session-wide suspend/resume, and does not change the
outgoing encoder or any CLIPRDR implementation.

**Existing outbound limitation:** the SVC encoder and session send path do not
coordinate a global paused state across static channels. Consuming a SUSPEND
control here does not gate subsequent application sends or responses, and
RESUME does not drain a deferred-send queue. The specification's requirement to
pause/resume all virtual-channel traffic is therefore still an existing
session-level gap, not a capability supplied by this receive-memory patch.
Implementing that coordination requires a separate change at the session's
outbound owner; rejecting controls or discarding receive data is not a substitute.

`tests/dechunkify.rs` is explicitly registered as the helper's `svc_dechunkify`
target in `Cargo.toml`: the vendor's upstream `autotests = false` / `test = false`
settings do not hide it. Run it through the **helper** workspace:

```powershell
cargo test --manifest-path crates/adit-rdp/Cargo.toml --locked --test svc_dechunkify
cargo clippy --manifest-path crates/adit-rdp/Cargo.toml --all-targets --locked -- -D warnings
```

The suite covers the exact 128 MiB limit and larger claims, length and flag
handling, interleaved and idle SUSPEND/RESUME controls without data dispatch,
error recovery and channel isolation, the real outgoing chunk encoder,
the Microsoft 2062-byte reassembly example, and the actual wire dumps from
[MS-RDPECLIP 4.1.1](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/017fbfda-2f6d-449a-b9cf-396b2cc58591)
and [4.4.2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/539f2ca7-ee67-46ba-8995-b2274fd8bcea),
decoded again by the published CLIPRDR parser. The enclosing SVC frames are
synthetic; these are offline protocol regressions, not a live-server acceptance
claim. Provenance and the crate archive checksum are in `vendor/ironrdp-svc/ADIT-PATCH.md`.
Remove the patch when upstream supplies equivalent bounds and recovery while
preserving these receive formats; revisit compression/control handling if those
capabilities are implemented in the session layer.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-graphics/`

Pulled in via `[patch.crates-io] ironrdp-graphics = { path = "vendor/ironrdp-graphics" }`.
One cosmetic hunk sits apart from the rest and is **not** an `ADIT PATCH`: the
`prog_quant_vals` parameter of `decode_tile_block` in `src/progressive.rs` had a
fully-qualified type that upstream's own `unused_qualifications` lint flags on
rustc 1.98, and that type is already imported at the top of the same file. It is
upstream's lint firing on upstream's code — the only reason we see it at all is
that a vendored crate is a *path* dependency, whose warnings cargo shows, unlike
a registry one's. Drop this hunk on the next sync; upstream will most likely have
fixed it themselves.

A set of `ADIT PATCH` hunks across `src/progressive.rs` and `src/srl.rs`, all about **RemoteFX Progressive
as Windows encodes it**. Upstream's decoder was written against GNOME Remote
Desktop and xrdp, which use the simpler tile mode; a Windows host exercises the
true progressive path (TILE_FIRST + TILE_UPGRADE) and hit all of these.

| Hunk | Bug | Symptom |
|------|-----|---------|
| `dequantize_component_ccq` | Shift was `quant - 1`; MS-RDPRFX 3.1.8.1.4 and FreeRDP's `rfx_quantization_decode_block` use `quant - 6`. | Every coefficient 32x too large, all three planes clamped past the YCbCr→RGB limits: the whole desktop rendered as flat black/red/yellow/white. |
| TILE_FIRST / TILE_UPGRADE quant lookup | `quality == 0xFF` was treated as an index into `quantProgVals`; MS-RDPRFX 2.2.4.3.6 defines it as "losslessly encoded, no progressive quantization". Windows sends it with `numProgQuant` 0. | `quant index 255 exceeds table length 0`; every refinement frame dropped. |
| Context lookup | The band-layout flag was only inherited within one `codecContextId`. Windows sends SYNC + CONTEXT once per connection and then starts each new progressive sequence under a fresh id without repeating them. | Every frame of every sequence after the first was dropped, so the desktop froze on the last good image. |
| State keying | Tile state was keyed by `codecContextId`; FreeRDP keys it per **surface** and ignores the field (it exists for DELETE_ENCODING_CONTEXT, which FreeRDP also no-ops). Windows mints a fresh id per sequence while difference tiles reference the surface's accumulated coefficients. | New sequences started from zeroed tiles; every difference tile decoded against nothing. |
| Difference flag | TILE_FIRST/TILE_SIMPLE `flags` bit 0 was parsed and never honored — `decode_first` always overwrote. Per FreeRDP (`add_16s_inplace`) a flagged tile's delta accumulates onto the tile's coefficients, in the dequantized domain. | The 26 flagged tiles of a captured frame rendered as flat gray rectangles punched into the image. |
| Upgrade stream cursors | The SRL and raw refinement streams are ONE continuous bitstream per component, spanning all ten bands; both readers were rebuilt inside the band loop. | Only the first refined band read real data; every later band re-read the stream head as garbage — refinements added noise instead of detail. |
| Upgrade scale + LL3 | Refinement bits were shifted by `curr_bit_pos` alone; the stored scale is `(quant−6)+bit_pos` (FreeRDP: `quant+prog−1` in its ×32 domain). And LL3 reads every coefficient from RAW (FreeRDP `nonLL=FALSE`), never SRL. | Upgrade contributions landed up to 16× too small, and LL3's SRL detour desynchronized both streams. |
| Upgrade with no first pass | The upgrade path bailed with `Ok(Vec::new())` when the tile had never had a first pass — nothing painted, nothing logged, server counts the region as drawn. FreeRDP decodes unconditionally (`tile->pass++`). Now decoded, occurrences counted in `DecodedRegion::upgrades_without_base`. | A cell reached that way froze for the session. (Real drop bug, though it turned out not to fire in the ghost captures.) |
| `DecodedTile::fresh` + hybrid application | Region rects are NOT a uniform clip mask. A FRESH pass (TILE_FIRST/TILE_SIMPLE) re-encodes the tile's *current* content and must blit **whole** — a captured stream re-encoded a whole tile icon→background under a rect overlapping it by 3px. An UPGRADE reconstructs the tile *as of its last FIRST* and must be **clipped to the rects**, else it rolls back pixels a `CacheToSurface` painted since. FreeRDP clips both; the split is ours, each half proven by its own capture. `egfx.rs` keys the blit on the flag. | Clipping fresh passes froze stale icons into the bitmap cache (duplicated-icon scroll residue); whole-blitting upgrades produced half-old half-new cells. |

Both were found by capturing the real stream rather than by reading the spec at
the symptom: `egfx.rs` writes `progressive-N.bin` under `%APPDATA%\Adit` when
`ADIT_RDP_DUMP=1`, and `tests/progressive_dump.rs` replays a capture offline and
renders it to PNG. That loop is what turned "the picture is wrong" into a
one-line diff, twice. Keep it.

The strongest form of that loop is `tests/merged_dump.rs` with
`ADIT_MERGE_LOG`: it replays a whole session from the helper's own paint
trace — progressive and ClearCodec dumps in log order, solid fills from their
logged colour, and both bitmap-cache ops as real canvas copies. On the capture
that settled the hybrid-application question it matched the live framebuffer
to 99.7%, which is what made it possible to prove a fix on a captured session
*before* deploying it. Keep that one too.

The crate carries its own `[workspace]` table so `cargo test` can be run inside
it — one hunk changes an upstream test's expected values.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-pdu/`

Pulled in via `[patch.crates-io] ironrdp-pdu = { path = "vendor/ironrdp-pdu" }`.
Two `ADIT PATCH` hunks in `src/codecs/clearcodec/`, both about **ClearCodec as
Windows encodes it**. Together with the three in `ironrdp-graphics` below they
took a logged-in desktop from "every ClearCodec region is a white rectangle"
(228 decode failures in one session) to rendering.

| Hunk | Bug | Symptom |
|------|-----|---------|
| `bands.rs` SHORT_VBAR_CACHE_MISS | `yOn` was read from bits 13:6 and `yOff` from bits 5:0. MS-RDPEGFX 2.2.4.1.1.2.1.1.3 and FreeRDP put `yOn` in the LOW byte and `yOff` at 13:8 — transposed, and the wrong widths. Also dropped a `yOff > band_height` rejection FreeRDP does not have (it bounds the run at 52 instead). | 136 x `shortVBarYOff < shortVBarYOn`; arithmetically rejects most legal headers. |
| `rlex.rs` one-entry palette | Special-cased `paletteCount == 1` to zero stop-index bits and one byte per segment. FreeRDP's `CLEAR_LOG2_FLOOR[0] + 1` is **1** bit, so it still reads a (packed, runLength) pair. | 16 x `suite exceeds region pixel count`, from a desynchronised segment parse in single-colour regions. |

The crate carries its own `[workspace]` table so `cargo test` runs in place —
one hunk corrects an upstream test that had encoded the transposed layout.

### Added — `vendor/ironrdp-graphics/src/nscodec.rs` (MS-RDPNSC)

**IronRDP has no NSCodec at all** — only capability-set constants naming it.
That stayed invisible until ClearCodec was wired up, because ClearCodec can
carry a whole region as an NSCodec *subcodec*, and that arm was a silent
no-op whose comment read "encoder avoids generating NSCodec tiles" (true of
IronRDP's own encoder, irrelevant to Windows). Windows uses NSCodec for every
photographic and gradient region.

In a captured session **101 of 512** ClearCodec streams carried nothing but an
NSCodec subcodec: zero decode errors, zero pixels painted. Composited as
opaque black that was a field of black holes; composited alpha-masked it left
stale content — the mosaic, in both of its forms.

`nscodec.rs` is a port of FreeRDP `libfreerdp/codec/nsc.c` (`nsc_stream_initialize`,
`nsc_rle_decode`, `nsc_rle_decompress_data`, `nsc_decode`): four RLE planes
(Y, Co, Cg, A), optional chroma subsampling, YCoCg→RGB with the colour-loss
shift. Verified by replaying the capture: 512/512 tiles now decode fully
opaque, and the regions that were blank come back as the article thumbnails,
app icons and avatars they always were.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-graphics/` (ClearCodec)

Three more `ADIT PATCH` hunks in `src/clearcodec/`, all cascade-limiting: the
v-bar caches are written by a cursor the server and client advance in lockstep
(the wire only ever names READ indices), so anything that aborts a PDU or skips
a column desynchronises them permanently.

| Hunk | Bug |
|------|-----|
| `mod.rs` column loop | `resolve_vbar` was skipped for columns outside the tile, silently dropping cache writes. FreeRDP guards only the blit. |
| `mod.rs` `resolve_vbar` | A cache miss aborted the whole PDU. FreeRDP warns and substitutes background-filled dummy data, then keeps going — so one lost slot no longer poisons every later frame. |
| `mod.rs` glyph cache | Stored only glyphs <= 1024 px (FreeRDP: 1024x1024) and required exact dimension equality on read (FreeRDP: the request must merely fit). Both left slots the server believes populated permanently empty. |

Glyph hits interpret the cached pixels as a linear stream using the requested
width, matching [FreeRDP `clear_decompress_glyph_data`](https://github.com/FreeRDP/FreeRDP/blob/master/libfreerdp/codec/clear.c).
The previous cached-stride crop could panic on a valid 2x2 cache read as 1x4
(issue #223: end index 20 for a 16-byte cache), and rejected a valid 4x1 read.
The decoder now checks the actual byte slice before returning it. Synthetic
regressions cover changed aspect ratios with distinct pixels, smaller requests,
oversized requests and truncated cache storage; they do not require a live host
or a recording of a user's desktop. Run them in this standalone vendor workspace:

```powershell
cargo test --manifest-path crates/adit-rdp/vendor/ironrdp-graphics/Cargo.toml glyph_hit_
```

Capture harness: `ADIT_RDP_DUMP=1` also writes `clear-NNN.bin` + sidecars under
`%APPDATA%\Adit`, successes included — the caches are stateful, so a failing
stream replays only in arrival order.

### Additive (no vendor changes) — `crates/adit-rdp/src/egfx.rs` + `src/session.rs`

The ClearCodec panic that opened issue #223 has **no live site any more** — the
cached-glyph read that produced it now bounds checks (see the ClearCodec table
above), so the decoder cannot unwind on that input. What is left of #223 is the
*defence*, and that is entirely Adit-side: the vendored codecs are not Adit's
code and can change, and other codec paths exist.

The failure mode was specific and expensive to diagnose. A decoder panic
unwound straight through `active_stage.process` (nothing on that path runs on
a `spawn_blocking` thread) out of `block_on`, so `run_host` never returned and
the helper exited **101**. The app saw only stdout EOF, sent a bare
`RdpClientEvent::Closed`, and the GUI degraded to
「连接已断开，按 Enter 重新连接」 — with `desktop_error` left `None`. The panic
text never reached `rdp-helper.log` either, because that file is a
`tracing_subscriber` writer and a panic goes to stderr. The only evidence the
field report had was the exit code, which said nothing about *why*.

There are now two guards, both in Adit's own code:

1. **`egfx.rs` — the decode seam.** `decode_clear_codec` wraps the decoder
   call in `catch_unwind` and turns a panic into an ordinary decode error,
   which the session layer already carries to the GUI with its concrete
   reason (`finish_active_process` → `HostMsg::Error`). The partial paint from
   before the failure is discarded, so no half-decoded frame crosses the IPC
   pipe.
2. **`session.rs` — the session layer.** `process_active_stage` wraps the
   whole active stage, so a panic from *any* codec path ends the session with
   a reported reason instead of taking the helper down.

Both carry the reason as a **plain string**, never as a decode error's source
chain: `ironrdp_error::Error`'s `Display` prints only `[context] kind` and
drops the source, so a source would be lost on the way to the GUI — exactly
the detail this path exists to preserve. The reason is also `tracing::error!`-ed
so it survives in `rdp-helper.log` on its own, even if the session dies before
the app polls.

The regression tests raise the panic **deliberately** (a `cfg(test)` hook in
`EgfxHandler`) and say so in their names — they are not reproductions of the
field failure, because no live panic remains to reproduce. What they prove is
that the guard still works for the next decoder bug.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-connector/`

Pulled in via `[patch.crates-io] ironrdp-connector = { path = "vendor/ironrdp-connector" }`
in `Cargo.toml`. Only **five** patches differ from crates.io `ironrdp-connector`
`0.10.0`; each is tagged `ADIT PATCH`. Find them with:

```
grep -rn "ADIT PATCH" crates/adit-rdp/vendor/ironrdp-connector/
```

That finds **six** markers, not five: five of them are in `src/connection.rs`,
and the sixth is the new `Config::client_monitors` field in `src/lib.rs` — patch
5 below is one logical change that spans both files, so it carries a marker on
each side:

1. **RDSTLS on redirect** (`ConnectionInitiationSendRequest`). When the config
   carries a routing token (i.e. we're following a GNOME handover), request
   `SecurityProtocol::RDSTLS` **exclusively** instead of SSL/HYBRID. This forces
   the handover daemon down the RDSTLS path (or fails negotiation cleanly) rather
   than silently selecting a protocol we then wouldn't authenticate. The caller
   (`session.rs::connect`) performs the RDSTLS exchange after the TLS upgrade; the
   connector's own state machine already skips CredSSP for a non-HYBRID protocol,
   so it proceeds straight to MCS.

2. **EGFX capability advertisement** (`create_gcc_blocks`, `early_capability_flags`).
   Added `SUPPORT_DYN_VC_GFX_PROTOCOL` (and dropped `SUPPORT_NET_CHAR_AUTODETECT`,
   paired with patch 3). The client must advertise Graphics Pipeline support during
   the GCC capabilities exchange or EGFX-mandatory servers reject the connection.

3. **`message_channel: None`** (`create_gcc_blocks`). Requesting the message
   channel drives the connector into `ConnectTimeAutoDetection`, where IronRDP can
   deadlock against servers that send message-channel PDUs its `AutoDetectReqPdu`
   decoder rejects (reproduced against a real Windows host; present in 0.10.0 and
   master). Skipping the channel goes straight to licensing. We lose optional
   network auto-detect / UDP multitransport / heartbeat, which Adit doesn't use.

4. **`expect` → `allow` on `single_use_lifetimes`** (`create_gcc_blocks`). Not a
   behaviour change — a build-noise one. `#[expect(lint)]` warns when its lint does
   *not* fire, `single_use_lifetimes` is allow-by-default, and upstream turns it on
   through a workspace lint table this vendored copy does not inherit. The
   expectation was therefore unfulfillable here and every single build of the RDP
   helper reported it. `allow` is a no-op while the lint is off and still
   suppresses it if Adit ever turns it on. Drop this the moment the vendored crate
   is built under a lint table that enables the lint.

5. **Client Monitor Data advertisement** (`Config::client_monitors` in
   `src/lib.rs`, consumed by `create_gcc_blocks` in `src/connection.rs`). Upstream
   hard-codes `monitor: None` in the GCC blocks, so the client never tells the
   server what its monitor geometry is at connect time and the server can only
   size its virtual desktop from `desktop_size`. Adit knows the real layout
   (it is persisted per profile and re-negotiated later over the DisplayControl
   DVC), so the connector now carries it and emits the GCC Client Monitor Data
   block.

   The block is sent **only when there is more than one monitor**, matching
   FreeRDP (`gcc_write_client_monitor_data`): a single-screen client sends no
   monitor block at all, and a one-monitor block makes some servers reject the
   connection. The geometry is already relative to the primary monitor at the
   origin — the same coordinates the DisplayControl channel later confirms — so
   the block needs no normalisation; only the inclusive `right`/`bottom` wire
   convention differs from Adit's own struct.

   FreeRDP additionally gates the block on the server having advertised
   `EXTENDED_CLIENT_DATA_SUPPORTED` in its negotiation response
   (`gcc_write_client_data_blocks`). We do not: the GCC blocks exchange *is*
   the extended-client-data mechanism, and upstream IronRDP already emits
   core/security/network blocks unconditionally, so a server that could not
   parse extended client data would already have failed before reaching this
   block. Gating it would mean threading the negotiation response flags
   through four connector states for no additional safety.

> Keeping the diff to five patches across two files is deliberate: re-vendoring
> a newer `ironrdp-connector` is a 5-patch reapply, and each patch is
> independently removable as upstream closes the gap.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-displaycontrol/`

Pulled in via `[patch.crates-io] ironrdp-displaycontrol = { path = "vendor/ironrdp-displaycontrol" }`
in `Cargo.toml`. Crates.io `ironrdp-displaycontrol` 0.8.0 has **two** marked
patches; find them with:

```
grep -rn "ADIT PATCH" crates/adit-rdp/vendor/ironrdp-displaycontrol/
```

1. **Capability parsing past the PDU header** (`src/client.rs`). The server
   sends the whole `DISPLAYCONTROL_CAPS_PDU`, i.e. the capability set prefixed
   with the 8-byte `DISPLAYCONTROL_HEADER` (Type + Length), per
   [MS-RDPEDISP 2.2.2.1](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpedisp/f4d5b25d-6d1a-4a29-8a3f-fd4dcd0b0dba).
   0.8.0 decoded the capability fields straight out of the DVC payload
   instead of parsing the PDU, so the header's two words were read as the
   capabilities. Against GNOME Remote Desktop the server's real
   `16, 8192, 8192` came out as `5, 20, 16` — the PDU type, the PDU length,
   and only then the actual monitor count — which made the advertised maximum
   area `5 × 20 × 16 = 1600` square pixels instead of
   `16 × 8192 × 8192 = 1 073 741 824`. `display.rs` then rejected **every**
   layout as "exceeds server monitor count/area capability", including a
   single 800×600 screen. The patch parses the full PDU, which is what
   upstream 0.9.0 does.

2. **`DisplayControlCapabilities::max_num_monitors()`** (`src/pdu/mod.rs`).
   Upstream added this getter in 0.9.0 alongside the fix above. Without it the
   caller has to re-encode the capabilities to read the monitor count. The
   patch adds only the getter.

The regression test lives in `crates/adit-rdp/src/display.rs`:
`server_capabilities_are_read_past_the_display_control_header` feeds the
exact bytes GNOME Remote Desktop puts on the wire and asserts the caps come
out as `16` monitors and `1 073 741 824` square pixels. It fails against
unpatched 0.8.0 with `left: 5, right: 16`.

> Both patches exist upstream now, but only in 0.9.0/0.10.0, whose dependency
> cascade (ironrdp-core 0.3, ironrdp-dvc 0.9, ironrdp-pdu 0.10, ironrdp-svc
> 0.9) cannot be reconciled with this repo's pinned 0.8-era versions — the same
> wall as `ironrdp-connector`. Re-vendor when the root workspace's IronRDP
> pins move up as a set.

## Re-vendoring checklist

When bumping IronRDP:

1. Copy the new `ironrdp-connector` source into `vendor/ironrdp-connector/`.
2. Re-apply the five `ADIT PATCH` patches above (or drop any that upstream fixed).
3. `cargo build --bin adit-rdp-host` in `crates/adit-rdp/` (its own workspace).
4. Smoke-test against a GNOME system-mode host — see the ignored harness
   `crates/adit-rdp-proto/tests/debug_connect.rs`.

## Why a separate workspace at all

`crates/adit-rdp` is `exclude`d from the root workspace with its own `Cargo.lock`.
IronRDP's `picky` exact-pins pre-release RustCrypto versions that conflict
irreconcilably with russh's. RDP therefore ships as an out-of-process helper
(`adit-rdp-host.exe`) the app drives over stdin/stdout. See the memory note
`rdp-ironrdp-dependency-conflict`.
