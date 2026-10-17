# Provenance
Based on vnc-rs 0.6.0 (https://github.com/HsuJv/vnc-rs), MIT OR Apache-2.0.
The bundled LICENSE-MIT and LICENSE-APACHE retain the original license terms.
Imported from the locked local Cargo source, including its existing decoder bounds,
cancellation, and ExtendedDesktopSize fixes. Adit additions provide explicit security
policy, certificate-validated VeNCrypt, bounded refresh, complete-update metadata,
and negotiated UTF-8 clipboard. Keep changes in this directory, never Cargo cache.

Clipboard text events carry a guarded `ClipboardText` receipt. Consumers keep it
queued until use, then call `into_current_text()`; they must not turn it into an
unguarded string while another queue still awaits delivery. Send and receive
permission generations are independent, so read-only transitions preserve queued
receive requests while disabling sharing revokes old incoming/outgoing text.


## TightVNC 2.8.90 UTF-8 clipboard wire interoperability

The Rust Tight-envelope and UTF-8 clipboard code is independently written here.
No TightVNC C/C++ implementation or GPL code has been copied into this fork.
Wire framing follows the [RFB Tight security specification](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst#tight-security-type).
The extension identifiers and field layouts were checked against the official
[TightVNC 2.8.90 source archive linked from its download page](https://www.tightvnc.com/download/):
`rfb/MsgDefs.h`, `rfb/MsgDefs.cpp`, `rfb/VendorDefs.cpp`, and the public wire-facing
capability/clipboard exchange interfaces. The archive is a reference, not a dependency.

Tight security type 16 is a capability envelope, not encryption. All tunnel/auth
records are bounded and matched by code, four-byte vendor and eight-byte signature.
Password-required policy refuses explicit or implicit None inside the envelope;
verified-TLS policy continues to require VeNCrypt. Extra ServerInit capability bytes
are consumed only after security type 16 was actually selected.

Vendor UTF-8 is enabled only when all three direction-specific records match
`TGHT` and their exact signatures: server `UTF8CUTS`, client `UTF8CUTC`, and client
`UTF8CUTE`. It sends enable code `0xFC000201`, then uses `0xFC000200` plus a bounded
U32 byte count and UTF-8 payload. These messages have no classic CutText padding
and no terminating NUL. Invalid UTF-8, embedded NUL and excessive lengths fail.
Outgoing CRLF expansion is bounded before queueing; incoming CRLF is normalized.
Existing independent send/receive permission epochs guard both wire offers and
queued received text. `VncConnector::utf8_clipboard(false)` disables automatic
vendor UTF-8 and removes the Extended Clipboard pseudo-encoding for explicit
legacy interoperability. No server-name, desktop-title or code-page guessing is used.
