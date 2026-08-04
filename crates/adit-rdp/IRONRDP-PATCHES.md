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

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-graphics/`

Pulled in via `[patch.crates-io] ironrdp-graphics = { path = "vendor/ironrdp-graphics" }`.
Eight `ADIT PATCH` hunks across `src/progressive.rs` and `src/srl.rs`, both about **RemoteFX Progressive
as Windows encodes it**. Upstream's decoder was written against GNOME Remote
Desktop and xrdp, which use the simpler tile mode; a Windows host exercises the
true progressive path (TILE_FIRST + TILE_UPGRADE) and hit both bugs at once.

| Hunk | Bug | Symptom |
|------|-----|---------|
| `dequantize_component_ccq` | Shift was `quant - 1`; MS-RDPRFX 3.1.8.1.4 and FreeRDP's `rfx_quantization_decode_block` use `quant - 6`. | Every coefficient 32x too large, all three planes clamped past the YCbCr→RGB limits: the whole desktop rendered as flat black/red/yellow/white. |
| TILE_FIRST / TILE_UPGRADE quant lookup | `quality == 0xFF` was treated as an index into `quantProgVals`; MS-RDPRFX 2.2.4.3.6 defines it as "losslessly encoded, no progressive quantization". Windows sends it with `numProgQuant` 0. | `quant index 255 exceeds table length 0`; every refinement frame dropped. |
| Context lookup | The band-layout flag was only inherited within one `codecContextId`. Windows sends SYNC + CONTEXT once per connection and then starts each new progressive sequence under a fresh id without repeating them. | Every frame of every sequence after the first was dropped, so the desktop froze on the last good image. |
| State keying | Tile state was keyed by `codecContextId`; FreeRDP keys it per **surface** and ignores the field (it exists for DELETE_ENCODING_CONTEXT, which FreeRDP also no-ops). Windows mints a fresh id per sequence while difference tiles reference the surface's accumulated coefficients. | New sequences started from zeroed tiles; every difference tile decoded against nothing. |
| Difference flag | TILE_FIRST/TILE_SIMPLE `flags` bit 0 was parsed and never honored — `decode_first` always overwrote. Per FreeRDP (`add_16s_inplace`) a flagged tile's delta accumulates onto the tile's coefficients, in the dequantized domain. | The 26 flagged tiles of a captured frame rendered as flat gray rectangles punched into the image. |
| Upgrade stream cursors | The SRL and raw refinement streams are ONE continuous bitstream per component, spanning all ten bands; both readers were rebuilt inside the band loop. | Only the first refined band read real data; every later band re-read the stream head as garbage — refinements added noise instead of detail. |
| Upgrade scale + LL3 | Refinement bits were shifted by `curr_bit_pos` alone; the stored scale is `(quant−6)+bit_pos` (FreeRDP: `quant+prog−1` in its ×32 domain). And LL3 reads every coefficient from RAW (FreeRDP `nonLL=FALSE`), never SRL. | Upgrade contributions landed up to 16× too small, and LL3's SRL detour desynchronized both streams. |

Both were found by capturing the real stream rather than by reading the spec at
the symptom: `egfx.rs` writes `progressive-N.bin` under `%APPDATA%\Adit` when
`ADIT_RDP_DUMP=1`, and `tests/progressive_dump.rs` replays a capture offline and
renders it to PNG. That loop is what turned "the picture is wrong" into a
one-line diff, twice. Keep it.

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

Capture harness: `ADIT_RDP_DUMP=1` also writes `clear-NNN.bin` + sidecars under
`%APPDATA%\Adit`, successes included — the caches are stateful, so a failing
stream replays only in arrival order.

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-connector/`

Pulled in via `[patch.crates-io] ironrdp-connector = { path = "vendor/ironrdp-connector" }`
in `Cargo.toml`. Only **three** sites differ from crates.io `ironrdp-connector`
`0.10.0`; each is tagged `ADIT PATCH`. Find them with:

```
grep -rn "ADIT PATCH" crates/adit-rdp/vendor/
```

All four are in `src/connection.rs`:

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

> Keeping the diff to four marked sites in one file is deliberate: re-vendoring a
> newer `ironrdp-connector` is a 4-hunk reapply, and each patch is independently
> removable as upstream closes the gap.

## Re-vendoring checklist

When bumping IronRDP:

1. Copy the new `ironrdp-connector` source into `vendor/ironrdp-connector/`.
2. Re-apply the three `ADIT PATCH` hunks above (or drop any that upstream fixed).
3. `cargo build --bin adit-rdp-host` in `crates/adit-rdp/` (its own workspace).
4. Smoke-test against a GNOME system-mode host — see the ignored harness
   `crates/adit-rdp-proto/tests/debug_connect.rs`.

## Why a separate workspace at all

`crates/adit-rdp` is `exclude`d from the root workspace with its own `Cargo.lock`.
IronRDP's `picky` exact-pins pre-release RustCrypto versions that conflict
irreconcilably with russh's. RDP therefore ships as an out-of-process helper
(`adit-rdp-host.exe`) the app drives over stdin/stdout. See the memory note
`rdp-ironrdp-dependency-conflict`.
