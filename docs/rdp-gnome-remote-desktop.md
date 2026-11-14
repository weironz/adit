# Native RDP → GNOME Remote Desktop (system mode)

Date: 2026-07-12
Status: **Shipped in v0.1.41.** Connects to GNOME Remote Desktop's system
("Remote Login", headless) mode end-to-end and streams the live desktop. Verified
against a real Ubuntu 26.04 / GNOME host.

This document is the design + **踩坑记录 (pitfalls log)** for the hardest part of
Adit's native RDP work: getting the built-in client to connect to Linux/GNOME's
headless remote-login mode — the same target `mstsc` reaches. It complements two
neighbouring docs:

- [`crates/adit-rdp/IRONRDP-PATCHES.md`](../crates/adit-rdp/IRONRDP-PATCHES.md) — the
  precise list of vendored/patched IronRDP hunks and how to re-vendor.
- The base RDP integration (connect + NLA + input + screen) is described in the
  Phase 2 plan and the `adit-rdp` crate docs; this doc assumes that as a starting
  point and covers only the GNOME-specific handover.

---

## 1. TL;DR

GNOME Remote Desktop's system mode is **not** a normal single-hop RDP connect. It
is a two-stage handover with a security protocol IronRDP doesn't implement:

```
                 ┌─────────────────────────── front daemon (gnome-remote-desktop)
   TCP → TLS → CredSSP/NLA  (original credentials)
                 │
                 └─► Server Redirection PDU  (routing token + one-time credentials)
                                │
   TCP → TLS → RDSTLS  (one-time credentials) ─────────── per-session daemon (--handover)
                                │
                          MCS → capabilities → EGFX frames  (the actual desktop)
```

Making this work required closing three gaps in the IronRDP stack:

| Gap | Why GNOME needs it | Where Adit implements it |
|-----|--------------------|--------------------------|
| **EGFX** graphics pipeline | GNOME mandates MS-RDPEGFX; refuses clients that don't advertise it | [`egfx.rs`](../crates/adit-rdp/src/egfx.rs) + a connector patch |
| **Server Redirection** | the front daemon hands off to a per-session daemon | [`redirect.rs`](../crates/adit-rdp/src/redirect.rs) + the reconnect loop in [`session.rs`](../crates/adit-rdp/src/session.rs) |
| **RDSTLS** auth | the handover reconnect uses one-time creds, not NLA | [`rdstls.rs`](../crates/adit-rdp/src/rdstls.rs) |

---

## 2. Why RDP runs out-of-process at all

Native RDP uses **IronRDP** (Devolutions, pure Rust). IronRDP's `ironrdp-connector`
pulls `picky` for CredSSP crypto, which **exact-pins pre-release RustCrypto** crates
(`ecdsa =0.17.0-rc.22`, `p256/p384/p521 =0.14.0-rc.14`). `russh` (Adit's SSH stack)
needs a different, incompatible set, and `=`-pins can't be reconciled with `[patch]`.
**IronRDP and russh cannot share one `Cargo.lock`.**

So `crates/adit-rdp` is a **standalone workspace** (its own `Cargo.lock`, `exclude`d
from the root workspace) that builds `adit-rdp-host.exe`. The main app spawns it per
RDP session and drives it over stdin/stdout with length-prefixed bincode
([`adit-rdp-proto`](../crates/adit-rdp-proto)). Screen frames come back as
`HostMsg::Tile` messages. This split is the foundation everything below sits on.

> Memory cross-ref: `rdp-ironrdp-dependency-conflict`.

---

## 3. The connection flow, step by step

`session.rs::run_session` is a reconnect loop. One pass:

1. **`connect()`** — TCP → `TokioFramed` → `connect_begin` (X.224 negotiation) →
   `ironrdp_tls::upgrade` → `connect_finalize` (CredSSP if HYBRID; MCS; capabilities).
   Dynamic virtual channels **DisplayControl** and **EGFX** are attached to drdynvc
   before the handshake.
2. **Announce** the negotiated desktop size once (`HostMsg::Connected`).
3. **`active_session()`** — the pump. Before handing a PDU to IronRDP's `ActiveStage`,
   it peeks X.224 PDUs for a **Server Redirection** (which `ActiveStage` can't decode).
   - No redirection → run until the server terminates → `Ok(None)`.
   - Redirection → `Ok(Some(Redirection))`.
4. On `Some(redir)`: set `request.host` to the target (if any), stash the **routing
   token**, build the **RDSTLS one-time credentials**, and loop back to step 1. A
   `MAX_REDIRECTS` guard bounds redirect chains.

On the redirect pass, `connect()` negotiates **RDSTLS** (not HYBRID) and runs the
RDSTLS exchange on the TLS stream before MCS.

---

## 4. The three pieces

### 4.1 EGFX (MS-RDPEGFX graphics pipeline)

GNOME composites the desktop with a compositor and delivers it over the **Graphics
Pipeline** dynamic virtual channel, not legacy bitmap/surface updates. It refuses
clients that don't advertise EGFX.

- [`egfx.rs`](../crates/adit-rdp/src/egfx.rs): an `EgfxHandler: GraphicsPipelineHandler`
  that blits decoded RGBA bitmaps into a shared framebuffer (`SharedEgfx =
  Arc<Mutex<EgfxFrame>>`). The active loop samples it via `take_frame()` and emits
  `HostMsg::Tile` (preceded by `Resized` if the graphics surface size changed).
- Wiring: a `GraphicsPipelineClient` on drdynvc **and** the
  `SUPPORT_DYN_VC_GFX_PROTOCOL` early-capability flag in the GCC core data — both are
  required; the flag alone or the channel alone is not enough.
- **No H.264 decoder** in the build, so IronRDP advertises the **V8 (no-AVC)** caps
  and GNOME falls back to **RemoteFX Progressive**.
- ⚠️ **`ironrdp-egfx` does NOT decode RemoteFX Progressive** — it only decodes H.264.
  For a progressive frame it merely frame-acknowledges and hands the raw stream to
  `GraphicsPipelineHandler::on_wire_to_surface2`. Leaving that unimplemented is why
  the desktop rendered **solid black** while the session was otherwise healthy:
  `on_bitmap_updated` never fired and the framebuffer stayed all-zero. `egfx.rs`
  therefore decodes it itself with
  [`ironrdp_graphics::progressive::ProgressiveDecoder`] (already a dependency — it
  carries the whole codec: dwt/rlgr/srl/quant/subband), compositing the 64×64 RGBA
  tiles at `surface_origin + tile_idx * 64`. The decoder is **stateful per
  codec-context** (progressive frames refine earlier ones), so keep one instance for
  the session's lifetime and drop a context only on `on_delete_encoding_context`.
- Don't zero the framebuffer on a **same-size** `RESET_GRAPHICS`: GNOME sends it
  immediately before repainting, so reallocating (which zeroes) flashes the surface
  black in the gap before the first new tile. Only reallocate on an actual size change.
- Windows hosts that DO negotiate H.264 still render black; that needs an H.264
  decoder and is a separate piece of work.

### 4.2 Server Redirection (MS-RDPBCGR 2.2.13)

After authenticating on the front daemon, GNOME sends a **Server Redirection PDU**
(`ShareControlPdu` pduType `0xa`) on the I/O channel. IronRDP's `ShareControlPdu`
can't parse `ServerRedirect`, so [`redirect.rs`](../crates/adit-rdp/src/redirect.rs)
does it directly using only IronRDP's public MCS decoder:

- `decode_send_data_indication` → check it's on the I/O channel → Share Control Header
  → pduType `0xa` → locate the `SEC_REDIRECTION_PKT` (`0x0400`) marker (the pad width
  varies by encoder, so we scan the next few bytes rather than hard-coding an offset).
- Parse the `LB_*` fields in flag order: target net address, **load-balance info
  (routing token)**, username, domain, **password**, FQDN, …, **redirection GUID**.

The **routing token** (`Cookie: msts=<id>`) goes into the X.224 connection request on
reconnect so the front daemon routes us to the pre-authenticated per-session daemon.
The **username / password / redirection GUID** feed the RDSTLS auth below.

### 4.3 RDSTLS (the handover authentication)

This is the crux. The handover reconnect is secured with **RDSTLS** — a distinct
security protocol (`SecurityProtocol::RDSTLS = 0x04`), **not** CredSSP/NLA. IronRDP
has only the flag, no implementation. [`rdstls.rs`](../crates/adit-rdp/src/rdstls.rs)
ports the client exchange from FreeRDP's `rdstls.c`.

**Framing:** RDSTLS PDUs are raw over the TLS stream — **no TPKT, no length prefix**.
Each PDU is `Version(u16 = 1) + Type(u16) + body`. We run the exchange on the concrete
TLS stream after `ironrdp_tls::upgrade` and before wrapping it in `TokioFramed` for MCS.

**Client exchange** (`recv → send → recv`):

1. **Receive Capabilities** (8 bytes): `Version(1) Type(CAPABILITIES=1) DataType(1)
   supportedVersions`. Validate the `RDSTLS_VERSION_1` bit.
2. **Send Auth Request** (password creds): `Version(1) Type(AUTHREQ=2)
   DataType(PASSWORD_CREDS=1)` then
   - `redirectionGuid` — `u16 len + bytes` (verbatim from the redirect),
   - `username` — `u16 byteLen(incl NUL) + UTF-16LE + NUL`,
   - `domain` — same string encoding,
   - `password` — `u16 len + bytes` (the one-time blob, **verbatim**).
3. **Receive Auth Response** (10 bytes): `Version(1) Type(AUTHRSP=4) DataType(1)
   resultCode(u32)`. `resultCode == 0` (`RDSTLS_RESULT_SUCCESS`) ⇒ authenticated;
   MCS proceeds.

Because the connector selected RDSTLS (not HYBRID), its own state machine skips
CredSSP and goes straight to MCS after the TLS upgrade — so slotting the RDSTLS
exchange in right after the upgrade "just works" with `connect_finalize`.

---

## 5. The vendored connector patches

Only **five** patches differ from crates.io `ironrdp-connector 0.10.0`, each tagged
`ADIT PATCH` in `vendor/ironrdp-connector/src/connection.rs` (patch 5 also touches
`src/lib.rs`):

1. **Request RDSTLS on redirect** — when the config carries a routing token, request
   `SecurityProtocol::RDSTLS` exclusively (forces the handover down the RDSTLS path or
   fails cleanly).
2. **Advertise EGFX** — add `SUPPORT_DYN_VC_GFX_PROTOCOL` (drop
   `SUPPORT_NET_CHAR_AUTODETECT`).
3. **`message_channel: None`** — skip the MCS message channel to avoid a
   ConnectTimeAutoDetection deadlock (see pitfall #2 below).
4. **`expect` → `allow` on `single_use_lifetimes`** — a build-noise fix, not a
   behaviour change (the vendored copy does not inherit upstream's lint table, so the
   expectation could never fire).
5. **Client Monitor Data advertisement** — carry the client's monitor geometry into
   the GCC blocks so the server learns the layout at connect time, not only later over
   DisplayControl.

Everything else Adit adds is a **separate additive module** — no upstream edits. Full
rationale + re-vendoring checklist in
[`IRONRDP-PATCHES.md`](../crates/adit-rdp/IRONRDP-PATCHES.md).

---

## 6. 踩坑记录 (pitfalls & how each was diagnosed)

In rough chronological order — each wall and its root cause.

1. **`picky` / RustCrypto vs. russh dependency conflict.**
   *Symptom:* can't add IronRDP to the workspace; `=`-pinned pre-release crates
   collide with russh's. *Fix:* out-of-process helper in a standalone workspace
   (§2). Do **not** try to bump russh to reconcile — it can't be.

2. **Connect-time hang (ConnectTimeAutoDetection deadlock).**
   *Symptom:* the connector hangs forever after basic settings, waiting for a
   licensing PDU. *Root cause:* requesting the MCS message channel makes IronRDP
   enter network auto-detect, where some servers send message-channel PDUs its
   `AutoDetectReqPdu` decoder rejects and silently drops — then both sides wait.
   *Fix:* `message_channel: None` (connector patch #3). Reproduced against a real
   Windows host too, so this is a general IronRDP issue, not GNOME-specific.

3. **"Client did not advertise support for the Graphics Pipeline."**
   *Symptom:* GNOME rejects the connection outright. *Root cause:* GNOME mandates
   EGFX. *Fix:* implement `egfx.rs` and advertise `SUPPORT_DYN_VC_GFX_PROTOCOL`
   (connector patch #2). After this we first reached `Connected`.

4. **A mysterious undecodable PDU containing `Cookie: msts=…`.**
   *Symptom:* `ActiveStage::process` errors on a PDU right after connect. *Root
   cause:* it's a **Server Redirection PDU** (pduType `0xa`) — the GNOME handover —
   which IronRDP can't parse. *Fix:* intercept and parse it in `redirect.rs`, drive
   a reconnect (§4.2).

5. **Borrow-checker: "cannot assign to `request.host` because it is borrowed."**
   *Root cause:* the pinned `connect` future held a borrow of `request` /
   `routing_token` across the redirect reassignment. *Fix:* block-scope the
   connect+timeout `select!` so the futures drop before the reassignment.

6. **CredSSP `0xc00700ea` on the redirect reconnect — even with the one-time
   credentials.** *Symptom:* NTLM completes, then auth fails, with both the original
   AND the one-time credentials. This was the big one — hours lost trying to make NLA
   accept the one-time password by hand (packet dumps, server logs, CredSSP traces).
   *Root cause (found by searching, not reversing):* **wrong protocol.** GNOME's
   handover authenticates with **RDSTLS**, not NLA/CredSSP. The gnome-remote-desktop
   maintainer's SUSE blog series documents this; FreeRDP's `rdstls.c` is the reference
   implementation. *Fix:* implement RDSTLS (§4.3).
   **Lesson (now a standing memory, `search-community-before-grinding`): on an
   interop/protocol wall or a cryptic error code, search GitHub/forums/reference
   implementations EARLY, before deep solo reverse-engineering.** One search round
   surfaced the answer, the reference code, and the exact IronRDP gaps (issues #139
   Server Redirection, #1016 duplicate). Hours of solo digging vs. minutes of reading.

7. **The `LB_PASSWORD_IS_PK_ENCRYPTED` flag is misleading.**
   It reads like "the password is encrypted, decrypt it." It is actually the **signal
   to send the password-credentials RDSTLS auth request**. FreeRDP (and Adit) forward
   the one-time password blob **verbatim** — the daemon that issued it re-validates the
   same bytes; it's an opaque one-time shared secret, not something the client decrypts.

8. **RDSTLS wire framing.** Initially unclear whether RDSTLS PDUs are TPKT/X.224-wrapped
   like the rest of RDP. They are **not** — `rdstls_send` writes `Version(u16)` then the
   body straight to the transport. Since the two PDUs we receive (Capabilities = 8 bytes,
   AuthResponse = 10 bytes) are fixed-size, a plain `read_exact` of the exact byte count
   is correct and robust (RDSTLS is strictly request/response, so nothing else is on the
   wire between them).

9. **App-side: connecting RDP aborted the whole app, silently (v0.1.41).**
   *Symptom:* the window vanished on connect — no error dialog, nothing in `crash.log`.
   *Diagnosis:* the empty crash.log is the tell — Adit's panic hook can't run for an
   `abort()`/guard-page fault. The **Windows Application event log** (Windows Error
   Reporting / Application Error) gave the real cause: exception `0xC00000FD`
   (`STATUS_STACK_OVERFLOW`) on the main thread. *Root cause:* the main thread reserves
   only **1 MiB** of stack on Windows (the PE default; Rust doesn't enlarge the main
   thread), and iced runs its recursive layout/draw + wgpu on it. Rendering the RDP image
   surface inside a deep widget tree spikes past 1 MiB. Flaky because it's tree-depth /
   transient-path dependent (steady-state render survives even a 256 KiB stack).
   *Fix (v0.1.42):* `crates/adit-app/build.rs` passes `/STACK:33554432` to the MSVC linker
   → 32 MiB main-thread reserve (virtual only). Validated: 1 MiB overflows a ~1.5 MiB-deep
   call, 32 MiB survives. Not RDP-protocol-specific, but the RDP surface is the deep render
   that triggered it. See the memory note `adit-windows-stack-overflow-crash`.

---

## 6.1 Landing on the GDM greeter is not a bug

**Symptom.** The handover completes, the desktop streams perfectly — and what
it streams is GDM's login screen with a user tile, not a desktop. Windows
hosts, by contrast, drop straight into the session.

**This is protocol reality, not a missing feature.** RDP's automatic-logon
flag (`INFO_AUTOLOGON` in the Client Info PDU, which `adit-rdp` sets whenever
a password is supplied) is a *Windows* convention: Windows' LogonUI reads
those credentials and signs the user in. **GDM does not read them at all.**
No RDP client can skip the GDM greeter, `mstsc` included — there is no wire
mechanism to do it.

**Reading the log tells you which half you are in.** A clean handover looks
like this, and means everything on the client side worked:

```text
INFO adit_rdp::session: starting RDP session host=… user=will …
INFO connect_finalize: … Connected with success
INFO adit_rdp::session: following RDP server redirection (RDSTLS handover) … has_token=true has_guid=true
INFO connect_finalize: … Connected with success
```

If those four lines are present, the greeter on screen is simply what the
remote session currently shows.

**The two GNOME modes behave differently, and that is the whole story:**

| Mode | What a connect gets you |
|------|-------------------------|
| **Remote Login** (system mode, what this document is about) | A *new* session every time ⇒ GDM first, log in there once |
| **Desktop Sharing** (user mode) | Takes over an *already logged-in* console session ⇒ straight to the desktop |

So "I want it to go straight to the desktop" is answered on the Ubuntu side
by switching to Desktop Sharing and leaving the console logged in — not by
anything in the client.

The same rule explains a Windows symptom that looks unrelated: an existing
session is reconnected without a logon, so a host that has been working all
day can start demanding credentials the moment it is rebooted and there is no
session left to reconnect to. *Have I got a session to return to?* is the
question behind both.

---

## 7. How to reproduce / smoke-test

An ignored harness spawns the helper and dumps events against a real host:

```sh
ADIT_RDP_HOST=<abs path to debug adit-rdp-host.exe> \
ADIT_RDP_TEST_HOST=<ip> ADIT_RDP_TEST_USER=<user> ADIT_RDP_TEST_PASS=<pass> \
RUST_LOG=info,ironrdp_connector=debug,adit_rdp=info \
cargo test -p adit-rdp-proto --test debug_connect -- --ignored --nocapture
```

Success looks like: `Connected` → `following RDP server redirection (RDSTLS handover)`
→ `Send ConnectionRequest { … protocol: SecurityProtocol(RDSTLS) }` → `Server confirmed
… RDSTLS` → `CredSSP is disabled, skipping NLA` → `Connected with success` → `Tile #1
…` frames.

The helper logs diagnostics to **stderr only** (stdout is the binary protocol), off
unless `RUST_LOG` is set. It never logs the password (only `pw_len`).

---

## 8. References

- **FreeRDP** `libfreerdp/core/rdstls.c` — the authoritative RDSTLS client/server
  exchange this port follows.
- **[MS-RDPBCGR]** §2.2.13 (Server Redirection), §5.4 (security). Note RDSTLS itself is
  thinly specified publicly; FreeRDP is the practical reference.
- **IronRDP** issue #139 (Support Server Redirection PDUs) and #1016 (duplicate) —
  the gaps that made this custom work necessary.
- The gnome-remote-desktop maintainer's SUSE blog series on RDP handover / system mode
  (documents the RDSTLS one-time-credential design).

---

## 9. Deferred (next RDP increment)

Audio (RDPSND — needs a CMake/Opus toolchain) and dirty-rect frame delivery
(currently full-frame tiles). `sound` is an `adit-rdp` cargo feature, OFF by
default.

**Multi-monitor is done** — the layout is picked, persisted per profile,
re-negotiated over the DisplayControl DVC, and switched at runtime, and the
client's geometry is advertised to the server at connect time through the GCC
Client Monitor Data block (so a server that never speaks DisplayControl still
learns the geometry up front). The toolbar carries the monitor-layout control.
Both halves have been run against a **real GNOME Remote Desktop** host: the
connect-time block makes the server size its desktop from the advertised
geometry (`2560x800` for two `1280x800` monitors, against a requested
`640x480`), and the post-connect DisplayControl exchange is **accepted**
(`result=ok`, with the resulting topology reported back). That second half
needed a client-side fix — IronRDP's `ironrdp-displaycontrol` 0.8.0 read the
capability fields without skipping the PDU header, so the server's
`16, 8192, 8192` arrived as `5, 20, 16` and every layout was rejected as too
large; the crate is now vendored and patched (see
[`IRONRDP-PATCHES.md`](../crates/adit-rdp/IRONRDP-PATCHES.md)).

What is *not* done is acceptance against a **physical** multi-screen server:
the GRD host presents a single screen, so the pointer-coordinate-across-screens
check still needs a host with more than one display. (The pointer cannot even
be observed there — GRD draws no cursor into the framebuffer, so a moved
pointer leaves the captured pixels byte-identical, and it sends no pointer
position updates after the client's own moves.) The **layout surviving a
reconnect** *is* covered: `crates/adit-ui/examples/multimon_reconnect_probe.rs`
drives the app's own connect path twice against the same on-disk profile,
reloading the profile from the file between rounds, and both rounds negotiate
the same two-monitor layout.

The **client half** of the pointer check is covered for real, though: the
acceptance tool drives the actual OS pointer and reads back the coordinate that
reaches the wire, and all four points land where they were aimed. What GRD
cannot provide is the *server's* view of that coordinate — and that is not a
gap in the method but a property of the host:

- GRD paints **no cursor** into the framebuffer. Three snapshots at different
  pointer positions are pixel-identical.
- `GrdRdpCursorUpdate` has **no position field**, so the server never states
  where it thinks the pointer is.
- GRD **logs no pointer coordinates**.
- Every host-side pointer oracle is blocked: the Shell `Screenshot` D-Bus call
  is `AccessDenied`, Shell `Eval` returns `(false, '')`, the GDK3 Wayland seat
  reports `0,0`, and `xdotool` reads the **X** pointer while GRD drives the
  **Wayland** cursor — two different things.

Two further limits of GRD headless are worth recording, because they shape
what the multi-monitor evidence can claim:

- **Headless GRD places its ei regions past the host's physical screen** and
  does not drive the live compositor. It serves a *virtual* desktop, not the
  host's screen. Its virtual monitors are real as far as the protocol is
  concerned (`MAX_MONITOR_COUNT_HEADLESS` is 16), but they are not physical
  displays.
- The host has **one** physical display. So the multi-monitor layout exercised
  against it is virtual on both ends, and the hardest case — a pointer
  crossing a *physical* screen boundary — remains unaccepted.

The evidence is in
[`tests/rdp/results/2026-11-14-rdp-grd-headless-multimonitor/`](../tests/rdp/results/2026-11-14-rdp-grd-headless-multimonitor/README.md).

**The server cursor shape is drawn** — `ActiveStageOutput::PointerBitmap` is
composited over the remote surface, with the hotspot and the local pointer kept
independent.

Clipboard (CLIPRDR) **is done** — text both ways, on by default, no cargo feature; it is
gated at runtime by `ConnectRequest::enable_clipboard`. See
[features.md](features.md#clipboard-cliprdr) for why the helper does not own the Windows
clipboard itself.
