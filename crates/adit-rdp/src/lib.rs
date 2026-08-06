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
//! and the app draws the OS cursor over the surface — so there's no laggy second
//! cursor; rendering the real server cursor shape is a later refinement.

use ironrdp_connector::{Config as ConnectorConfig, Credentials, DesktopSize};
use thiserror::Error;

mod clipboard;
mod avc444;
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
}

/// Extract the value of a `Cookie: msts=<value>\r\n` load-balance / routing token
/// (IronRDP's `routing_token` re-adds the `Cookie: msts=` prefix and CRLF).
pub(crate) fn routing_token_value(load_balance_info: &[u8]) -> String {
    let text = String::from_utf8_lossy(load_balance_info);
    let text = text.trim_end_matches(['\r', '\n']);
    text.strip_prefix("Cookie: msts=").unwrap_or(text).to_owned()
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
        // Deliver the pointer as separate updates (not composited into the image);
        // the app shows the OS cursor, so we avoid a laggy composited second
        // cursor. Rendering the real server cursor shape is a later refinement.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        performance_flags: performance_flags(request.quality),
        license_cache: None,
        timezone_info: Default::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        compression_type: None,
    }
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
}
