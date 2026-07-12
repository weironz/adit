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

### Vendored + patched — `crates/adit-rdp/vendor/ironrdp-connector/`

Pulled in via `[patch.crates-io] ironrdp-connector = { path = "vendor/ironrdp-connector" }`
in `Cargo.toml`. Only **three** sites differ from crates.io `ironrdp-connector`
`0.10.0`; each is tagged `ADIT PATCH`. Find them with:

```
grep -rn "ADIT PATCH" crates/adit-rdp/vendor/
```

All three are in `src/connection.rs`:

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

> Keeping the diff to three marked sites in one file is deliberate: re-vendoring a
> newer `ironrdp-connector` is a 3-hunk reapply, and each patch is independently
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
