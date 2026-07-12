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

#[cfg(feature = "clipboard")]
mod clipboard;
mod host;
mod input;
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

/// Build the IronRDP connector config from a connect request.
pub(crate) fn build_connector_config(
    request: &adit_rdp_proto::ConnectRequest,
) -> ConnectorConfig {
    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
    use ironrdp_pdu::rdp::client_info::PerformanceFlags;

    // RDP desktop width must be even; clamp both dims into the protocol's range.
    let width = request.width.clamp(200, 8192) & !1;
    let height = request.height.clamp(200, 8192);

    let domain = request
        .domain
        .as_ref()
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());

    ConnectorConfig {
        credentials: Credentials::UsernamePassword {
            username: request.username.clone(),
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
        request_data: None,
        autologon: false,
        enable_audio_playback: request.enable_audio,
        // Deliver the pointer as separate updates (not composited into the image);
        // the app shows the OS cursor, so we avoid a laggy composited second
        // cursor. Rendering the real server cursor shape is a later refinement.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        license_cache: None,
        timezone_info: Default::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        compression_type: None,
    }
}
