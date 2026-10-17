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
