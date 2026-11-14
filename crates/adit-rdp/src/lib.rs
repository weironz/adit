//! Native RDP helper for Adit, built on the IronRDP crate stack.
//!
//! This crate is the guts of the out-of-process RDP helper (`adit-rdp-host`). It
//! can't live in the main binary: IronRDP's `picky` exact-pins pre-release
//! RustCrypto versions that conflict with russh's, so RDP runs as a child process
//! the app drives over stdin/stdout using [`adit_rdp_proto`]. See the crate's
//! `[workspace]` note in Cargo.toml.
//!
//! Connection path: direct TCP → TLS → CredSSP/NLA (`sspi`) → active session loop
//! (`ActiveStage`), mirroring the reference `ironrdp-client`. The desktop image is
//! `RGBA32`. The server pointer is delivered as separate updates (not composited),
//! then sent over IPC; both GUI desktop views draw its bitmap at the scaled
//! hotspot while local motion stays independent of remote frame delivery.

use ironrdp_connector::{Config as ConnectorConfig, Credentials, DesktopSize};
use ironrdp_pdu::gcc;
use thiserror::Error;

mod avc444;
mod clipboard;
mod display;
mod egfx;
mod host;
mod input;
mod rdstls;
mod redirect;
mod session;

pub use host::run_host;

#[derive(Debug, Error)]
pub enum RdpError {
    #[error("could not start the Tokio runtime: {0}")]
    Runtime(String),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("RDP session error: {0}")]
    Session(String),
    #[error("the app closed the control channel")]
    ControlChannelClosed,
    /// The app asked the session to end.
    ///
    /// Distinct from [`RdpError::ControlChannelClosed`] because the two mean
    /// opposite things: the app closing the channel is how an *ordinary*
    /// session ends, and it must not be reported as a failure. A session that
    /// ended this way is a success, and the helper exits 0 — otherwise every
    /// normal disconnect would exit non-zero and read as a crash.
    ///
    /// The variant exists because the app's `Close` message and a *broken*
    /// pipe both close the same channel: the first is deliberate, the second
    /// is not, and only the session layer can tell them apart by whether the
    /// `Close` actually arrived.
    #[error("the app closed the session")]
    AppClosed,
}

/// Extract the value of a `Cookie: msts=<value>\r\n` load-balance / routing token
/// (IronRDP's `routing_token` re-adds the `Cookie: msts=` prefix and CRLF).
pub(crate) fn routing_token_value(load_balance_info: &[u8]) -> String {
    let text = String::from_utf8_lossy(load_balance_info);
    let text = text.trim_end_matches(['\r', '\n']);
    text.strip_prefix("Cookie: msts=")
        .unwrap_or(text)
        .to_owned()
}

/// RDP performance flags for a quality preset, mirroring what mstsc's
/// "Experience" tab sends.
///
/// Each flag is an instruction to the *server* about what to stop drawing, so
/// what this buys is fewer pixels changing, not better compression — which is
/// why the effect is dramatic on a desktop with a photographic wallpaper and
/// nearly invisible on a bare login screen.
///
/// `ENABLE_*` are the two positive flags in the set: absent means "off", so
/// Speed switches them off simply by not naming them.
pub(crate) fn performance_flags(
    quality: adit_rdp_proto::Quality,
) -> ironrdp_pdu::rdp::client_info::PerformanceFlags {
    use adit_rdp_proto::Quality;
    use ironrdp_pdu::rdp::client_info::PerformanceFlags as Flags;

    match quality {
        // Nothing disabled, and composition asked for on top. Aero/DWM effects
        // cost real bandwidth, so this is a LAN setting.
        Quality::High => Flags::ENABLE_FONT_SMOOTHING | Flags::ENABLE_DESKTOP_COMPOSITION,
        // Byte-for-byte IronRDP's `PerformanceFlags::default()`, spelled out
        // rather than delegated: this is the behaviour every session had before
        // the preset existed, and it must not drift if upstream's default does.
        Quality::Balanced => {
            Flags::DISABLE_FULLWINDOWDRAG
                | Flags::DISABLE_MENUANIMATIONS
                | Flags::ENABLE_FONT_SMOOTHING
        }
        Quality::Speed => {
            Flags::DISABLE_WALLPAPER
                | Flags::DISABLE_FULLWINDOWDRAG
                | Flags::DISABLE_MENUANIMATIONS
                | Flags::DISABLE_THEMING
                | Flags::DISABLE_CURSOR_SHADOW
                | Flags::DISABLE_CURSORSETTINGS
        }
    }
}

/// Build the IronRDP connector config from a connect request. On a redirection
/// reconnect, `routing_token` carries the server's load-balance info, which goes
/// into the X.224 connection request so the server routes us to the right session.
pub(crate) fn build_connector_config(
    request: &adit_rdp_proto::ConnectRequest,
    routing_token: Option<&[u8]>,
) -> ConnectorConfig {
    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_pdu::nego::NegoRequestData;
    use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;

    // Redirection reconnect ⇒ send the routing token; initial connect ⇒ leave it
    // None so the connector falls back to a `mstshash` cookie with the username.
    let request_data =
        routing_token.map(|token| NegoRequestData::routing_token(routing_token_value(token)));

    // RDP desktop width must be even; clamp both dims into the protocol's range.
    let width = request.width.clamp(200, 8192) & !1;
    let height = request.height.clamp(200, 8192);

    let domain = request
        .domain
        .as_ref()
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());

    // A `MicrosoftAccount\` prefix is stripped before CredSSP ever sees it.
    //
    // The username box accepts `DOMAIN\user`, and typing the Microsoft-account
    // form there is the obvious thing to try when Windows asks for it — but it
    // reaches sspi's `Username::new`, which refuses a UPN paired with a domain
    // and fails the connection outright ("invalid username", credssp.rs:104),
    // leaving the password dialog reappearing forever. It is not needed here
    // either way: the vendored connector adds `MicrosoftAccount` to the Client
    // Info PDU by itself, which is the only place Windows wants it.
    let username = request
        .username
        .split_once('\\')
        .filter(|(domain, _)| domain.eq_ignore_ascii_case("MicrosoftAccount"))
        .map_or_else(|| request.username.clone(), |(_, user)| user.to_owned());

    ConnectorConfig {
        credentials: Credentials::UsernamePassword {
            username,
            password: request.password.clone(),
        },
        domain,
        // NLA (CredSSP) is the modern, secure default; plain TLS-only is legacy.
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize { width, height },
        desktop_scale_factor: 0,
        bitmap: None,
        client_build: 0,
        client_name: "Adit".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::WINDOWS,
        hardware_id: None,
        request_data,
        // INFO_AUTOLOGON, exactly as mstsc sets it whenever a password is
        // supplied: without it NLA authenticates the CONNECTION but Windows
        // still parks the user at the interactive LogonUI to type the same
        // password again.
        autologon: !request.password.is_empty(),
        enable_audio_playback: request.enable_audio,
        // Accelerated pointer decoding yields straight-alpha RGBA for the GUI
        // overlay. Keep it out of both the legacy and EGFX framebuffers.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        client_monitors: gcc_monitors(&request.monitors),
        performance_flags: performance_flags(request.quality),
        license_cache: None,
        timezone_info: Default::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        compression_type: None,
    }
}

/// The client's monitor geometry, as the GCC Client Monitor Data block wants it.
///
/// The block is only meaningful for a real multi-screen layout, and FreeRDP
/// omits it entirely for a single screen (gcc.c, `gcc_write_client_monitor_data`).
/// Sending a one-monitor block would make a server that only understands the
/// old single-screen case reject the connection, so a single monitor is
/// dropped here and the plain `width` × `height` desktop is requested instead.
///
/// The geometry is already relative to the primary monitor at the origin —
/// that is what `adit_rdp_proto::monitor_bounds` validates — so it needs no
/// normalisation. `right`/`bottom` are *inclusive* on the wire, which is the
/// one place the GCC types differ from ours.
fn gcc_monitors(monitors: &[adit_rdp_proto::Monitor]) -> Option<Vec<gcc::Monitor>> {
    if monitors.len() < 2 {
        return None;
    }
    let converted = monitors
        .iter()
        .map(|m| gcc::Monitor {
            left: m.left,
            top: m.top,
            right: m.left + i32::from(m.width) - 1,
            bottom: m.top + i32::from(m.height) - 1,
            flags: if m.primary {
                gcc::MonitorFlags::PRIMARY
            } else {
                gcc::MonitorFlags::empty()
            },
        })
        .collect();
    Some(converted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_rdp_proto::Quality;
    use ironrdp_pdu::rdp::client_info::PerformanceFlags as Flags;

    /// The preset exists to change what an *existing* session looks like, so the
    /// default must not: every session before this feature ran on IronRDP's
    /// `PerformanceFlags::default()`, and Balanced is the promise that nothing
    /// moved for anyone who never touches the new control.
    #[test]
    fn balanced_is_exactly_the_pre_existing_default() {
        assert_eq!(performance_flags(Quality::Balanced), Flags::default());
        assert_eq!(performance_flags(Quality::default()), Flags::default());
    }

    /// Speed's whole point is a slow link: font smoothing and composition are
    /// the two flags that *cost* bandwidth, so naming either would defeat it.
    #[test]
    fn speed_asks_for_nothing_that_costs_bandwidth() {
        let flags = performance_flags(Quality::Speed);
        assert!(!flags.contains(Flags::ENABLE_FONT_SMOOTHING));
        assert!(!flags.contains(Flags::ENABLE_DESKTOP_COMPOSITION));
        assert!(flags.contains(Flags::DISABLE_WALLPAPER));
        assert!(flags.contains(Flags::DISABLE_THEMING));
    }

    /// High is "draw everything": no DISABLE_* bit may creep in, or the preset
    /// quietly stops being the high-fidelity end of the scale.
    #[test]
    fn high_disables_nothing() {
        let flags = performance_flags(Quality::High);
        assert!(flags.contains(Flags::ENABLE_DESKTOP_COMPOSITION));
        for disable in [
            Flags::DISABLE_WALLPAPER,
            Flags::DISABLE_FULLWINDOWDRAG,
            Flags::DISABLE_MENUANIMATIONS,
            Flags::DISABLE_THEMING,
            Flags::DISABLE_CURSOR_SHADOW,
            Flags::DISABLE_CURSORSETTINGS,
        ] {
            assert!(!flags.contains(disable), "High must not set {disable:?}");
        }
    }

    fn wire_monitor(left: i32, top: i32, primary: bool) -> adit_rdp_proto::Monitor {
        adit_rdp_proto::Monitor {
            left,
            top,
            width: 800,
            height: 600,
            primary,
            scale_percent: 100,
        }
    }

    /// A single-screen client must send no monitor block at all. FreeRDP omits
    /// it for one monitor (gcc.c, `gcc_write_client_monitor_data`), and some
    /// servers reject a connection that announces a one-monitor layout.
    #[test]
    fn a_single_monitor_sends_no_monitor_block() {
        assert!(gcc_monitors(&[]).is_none());
        assert!(gcc_monitors(&[wire_monitor(0, 0, true)]).is_none());
    }

    /// The GCC rectangle is *inclusive* on the right and bottom, ours is not:
    /// an 800×600 monitor at the origin is 0..799 × 0..599 on the wire.
    #[test]
    fn the_wire_rectangle_is_inclusive() {
        let monitors = gcc_monitors(&[wire_monitor(0, 0, true), wire_monitor(800, 0, false)])
            .expect("two monitors");
        assert_eq!(monitors[0].left, 0);
        assert_eq!(monitors[0].top, 0);
        assert_eq!(monitors[0].right, 799);
        assert_eq!(monitors[0].bottom, 599);
        assert_eq!(monitors[1].left, 800);
        assert_eq!(monitors[1].right, 1599);
    }

    /// Exactly one monitor carries the primary flag; the server uses it to
    /// anchor the layout, so a second one would make the origin ambiguous.
    #[test]
    fn only_the_primary_carries_the_primary_flag() {
        let monitors = gcc_monitors(&[wire_monitor(0, 0, true), wire_monitor(800, 0, false)])
            .expect("two monitors");
        assert!(monitors[0].flags.contains(gcc::MonitorFlags::PRIMARY));
        assert!(monitors[1].flags.is_empty());
    }

    /// The block's byte layout is what a server actually parses, so it is
    /// checked against the encoder rather than against our own struct: flags,
    /// count, then 20 bytes per monitor.
    #[test]
    fn the_encoded_block_matches_the_wire_layout() {
        let monitors = gcc_monitors(&[wire_monitor(0, 0, true), wire_monitor(800, 0, false)])
            .expect("two monitors");
        let block = gcc::ClientMonitorData {
            monitors: monitors.clone(),
        };
        let bytes = ironrdp_core::encode_vec(&block).expect("encode");
        assert_eq!(bytes.len(), 8 + 2 * 20);

        let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(count, 2);
        assert_eq!(
            i32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            0,
            "the primary monitor is at the origin"
        );
        assert_eq!(
            i32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            799,
            "right is inclusive"
        );
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            gcc::MonitorFlags::PRIMARY.bits(),
            "the primary flag rides in the first monitor's flags"
        );
    }

    /// The block as the server sees it on the wire: a GCC user-data header
    /// (type `0xC005`, length `20 * count + 12`) followed by the payload.
    /// FreeRDP's `gcc_write_client_monitor_data` writes exactly these numbers,
    /// and a server parses them literally — a length off by one desynchronises
    /// everything after the block.
    #[test]
    fn the_gcc_block_header_matches_the_wire_format() {
        let monitors = gcc_monitors(&[wire_monitor(0, 0, true), wire_monitor(800, 0, false)])
            .expect("two monitors");
        let block = gcc::ClientMonitorData {
            monitors: monitors.clone(),
        };
        let payload = ironrdp_core::encode_vec(&block).expect("encode");

        // The GCC user-data header the connector wraps the block in: the type
        // followed by the length, which counts the header itself.
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xC005u16.to_le_bytes());
        wire.extend_from_slice(&((payload.len() + 4) as u16).to_le_bytes());
        wire.extend_from_slice(&payload);

        assert_eq!(
            u16::from_le_bytes(wire[0..2].try_into().unwrap()),
            0xC005,
            "Client Monitor Data is GCC type 0xC005"
        );
        assert_eq!(
            u16::from_le_bytes(wire[2..4].try_into().unwrap()),
            20 * 2 + 12,
            "the block length is 20 bytes per monitor plus a 12-byte header"
        );
        assert_eq!(
            wire.len(),
            4 + 8 + 2 * 20,
            "header, then flags + count, then 20 bytes per monitor"
        );
    }
}

/// Render an error together with every cause behind it.
///
/// IronRDP's `SessionError` prints only its outermost context and a kind
/// ("custom error"); the source chain — the part that names the PDU the
/// server actually sent — is dropped by `to_string()`. The helper log is the
/// only window into a failed RDP session (the GUI discards the helper's
/// stderr), so a flattened error hides the one fact that would explain it.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(", caused by: ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod error_chain_tests {
    use super::error_chain;
    use std::error::Error;
    use std::fmt;

    #[derive(Debug)]
    struct Inner;

    impl fmt::Display for Inner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "not enough bytes")
        }
    }

    impl Error for Inner {}

    #[derive(Debug)]
    struct Middle(Inner);

    impl fmt::Display for Middle {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "decode error")
        }
    }

    impl Error for Middle {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    /// A flattened error is what made the helper log say only "custom error"
    /// while the real cause — the PDU the server sent — stayed invisible. The
    /// chain has to survive every hop.
    #[test]
    fn keeps_every_cause_in_the_chain() {
        let text = error_chain(&Middle(Inner));
        assert_eq!(
            text,
            "decode error, caused by: not enough bytes",
            "the innermost cause must survive"
        );
    }

    /// An error with no cause must not gain a dangling "caused by".
    #[test]
    fn a_chainless_error_stands_alone() {
        let text = error_chain(&Inner);
        assert_eq!(text, "not enough bytes");
        assert!(!text.contains("caused by"));
    }
}
