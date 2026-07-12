//! The RDP connection + active-session loop, adapted from the reference
//! `ironrdp-client`: direct TCP → TLS → CredSSP, then a `tokio::select!` loop
//! pumping server PDUs into framebuffer tiles and app input to the server.
//!
//! The loop is transport-agnostic: it receives [`InputEvent`]s from a Tokio
//! channel and emits [`HostMsg`]s to a std channel. The [`crate::host`] layer
//! bridges those channels to the child process's stdin/stdout.

use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use adit_rdp_proto::{ConnectRequest, HostMsg, InputEvent};
use ironrdp_connector::{ClientConnector, ConnectionResult, ServerName};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_egfx::client::GraphicsPipelineClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::Database;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{split_tokio_framed, FramedWrite, TokioFramed};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc as tokio_mpsc;

use crate::egfx::{self, EgfxHandler, SharedEgfx};
use crate::{build_connector_config, input::map_input, RdpError};

/// A type-erased upgraded (post-TLS) framed transport.
trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}
type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

/// Abort a TCP/TLS/CredSSP handshake that stalls this long.
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Cap on chained server redirections (guards against a redirect loop).
const MAX_REDIRECTS: u32 = 5;

/// Drive an RDP session to completion, following any server redirections (GNOME
/// system-mode handover): connect, announce, run the active loop; on a redirection
/// PDU, reconnect to the target carrying the routing token, and repeat.
pub(crate) async fn run_session(
    mut request: ConnectRequest,
    mut input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    host_tx: std_mpsc::Sender<HostMsg>,
) -> Result<(), RdpError> {
    // Shared framebuffer the EGFX handler (running inside `ActiveStage::process`)
    // composites into; the active loop samples it and emits tiles.
    tracing::info!(
        host = %request.host,
        port = request.port,
        user = %request.username,
        domain = ?request.domain,
        password_len = request.password.len(),
        "starting RDP session"
    );
    let shared_egfx = egfx::new_shared();
    let mut routing_token: Option<Vec<u8>> = None;
    // One-time credentials for the RDSTLS handover auth, populated on a redirect.
    let mut rdstls_creds: Option<crate::rdstls::RdstlsCreds> = None;
    let mut announced = false;
    let mut redirects = 0u32;

    loop {
        // Connect with a timeout; a Close (input channel drop) cancels a hung
        // handshake instead of blocking forever. Scoped in its own block so the
        // futures borrowing `request` / `routing_token` are dropped before we may
        // reassign those on a redirect.
        let (connection_result, framed) = {
            let connect_fut =
                connect(&request, &shared_egfx, routing_token.as_deref(), rdstls_creds.as_ref());
            tokio::pin!(connect_fut);
            let deadline = tokio::time::sleep(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    result = &mut connect_fut => {
                        match result {
                            Ok(v) => break v,
                            Err(e) => {
                                tracing::error!("RDP connect failed: {e}");
                                return Err(RdpError::Connect(e.to_string()));
                            }
                        }
                    }
                    _ = &mut deadline => {
                        return Err(RdpError::Connect(format!(
                            "connection timed out after {CONNECT_TIMEOUT_SECS}s"
                        )));
                    }
                    msg = input_rx.recv() => {
                        if msg.is_none() {
                            return Err(RdpError::ControlChannelClosed);
                        }
                    }
                }
            }
        };

        // Announce the negotiated size once (the first successful connect); after a
        // redirect the desktop size, if different, comes through as a Resized tile.
        if !announced {
            announced = true;
            let desktop = connection_result.desktop_size;
            if host_tx
                .send(HostMsg::Connected {
                    width: desktop.width,
                    height: desktop.height,
                })
                .is_err()
            {
                return Err(RdpError::ControlChannelClosed);
            }
        }

        match active_session(framed, connection_result, &mut input_rx, &host_tx, &shared_egfx)
            .await
            .map_err(|e| RdpError::Session(e.to_string()))?
        {
            None => return Ok(()),
            Some(redir) => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(RdpError::Session("too many server redirections".into()));
                }
                // Follow the redirection. GNOME hands off on the same host with a
                // routing token (usually no target address); carry the token into the
                // reconnect so the front daemon routes us to the pre-authenticated
                // session, and authenticate there with the one-time credentials via
                // RDSTLS (built below), not the original NLA credentials.
                if let Some(host) = redir.host() {
                    request.host = host.to_owned();
                }
                if let Some(user) = redir.username.as_deref().filter(|u| !u.is_empty()) {
                    request.username = user.to_owned();
                }
                if let Some(domain) = redir.domain.as_deref().filter(|d| !d.is_empty()) {
                    request.domain = Some(domain.to_owned());
                }
                routing_token = redir.load_balance_info.clone();
                // The RDSTLS auth request echoes the redirection GUID and forwards the
                // one-time username/domain/password verbatim (the password is an opaque
                // blob the handover daemon issued and re-validates).
                rdstls_creds = Some(crate::rdstls::RdstlsCreds {
                    redirection_guid: redir.redirection_guid.clone().unwrap_or_default(),
                    username: redir.username.clone().unwrap_or_default(),
                    domain: redir.domain.clone().unwrap_or_default(),
                    password: redir.password.clone().unwrap_or_default(),
                });
                tracing::info!(
                    host = %request.host,
                    user = %request.username,
                    has_token = routing_token.is_some(),
                    has_guid = redir.redirection_guid.is_some(),
                    pw_len = redir.password.as_deref().map(<[u8]>::len).unwrap_or(0),
                    "following RDP server redirection (RDSTLS handover)"
                );
            }
        }
    }
}

/// Direct TCP connect, TLS upgrade, then finalize. On the initial connect the
/// connector does CredSSP/NLA; on a redirection reconnect (`routing_token` set) it
/// negotiates RDSTLS instead, and `rdstls_creds` carries the one-time credentials
/// we authenticate with on the TLS stream before MCS.
async fn connect(
    request: &ConnectRequest,
    shared_egfx: &SharedEgfx,
    routing_token: Option<&[u8]>,
    rdstls_creds: Option<&crate::rdstls::RdstlsCreds>,
) -> Result<(ConnectionResult, UpgradedFramed), ironrdp_connector::ConnectorError> {
    let dest = format!("{}:{}", request.host, request.port);
    let stream = TcpStream::connect(&dest)
        .await
        .map_err(|e| ironrdp_connector::custom_err!("TCP connect", e))?;
    let client_addr = stream
        .local_addr()
        .map_err(|e| ironrdp_connector::custom_err!("local address", e))?;
    let mut framed = TokioFramed::new(stream);

    let config = build_connector_config(request, routing_token);

    // Dynamic virtual channels: DisplayControl (dynamic resize) and the EGFX
    // graphics pipeline. Opening the EGFX channel is what signals Graphics
    // Pipeline support — servers like GNOME Remote Desktop reject clients that
    // don't. No H.264 decoder ⇒ IronRDP advertises the V8 (no-AVC) caps and the
    // server uses a codec we can decode.
    let egfx_client =
        GraphicsPipelineClient::new(Box::new(EgfxHandler::new(Arc::clone(shared_egfx))), None);
    let drdynvc = DrdynvcClient::new()
        .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())))
        .with_dynamic_channel(egfx_client);
    // `mut` is always needed for the `&mut connector` borrows in `connect_begin` /
    // `mark_as_upgraded`; the feature blocks below may also reassign it.
    #[allow(unused_mut)]
    let mut connector = ClientConnector::new(config, client_addr).with_static_channel(drdynvc);

    // Audio (RDPSND) via cpal.
    #[cfg(feature = "sound")]
    if request.enable_audio {
        use ironrdp_rdpsnd_native::cpal;
        connector = connector.with_static_channel(ironrdp_rdpsnd::client::Rdpsnd::new(Box::new(
            cpal::RdpsndBackend::new(),
        )));
    }

    // Clipboard (CLIPRDR) via the native OS backend.
    #[cfg(feature = "clipboard")]
    if request.enable_clipboard {
        if let Some(channel) = crate::clipboard::build_channel() {
            connector.attach_static_channel(channel);
        }
    }

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector).await?;

    let (initial_stream, leftover) = framed.into_inner();
    let (mut tls_stream, tls_cert) = ironrdp_tls::upgrade(initial_stream, &request.host)
        .await
        .map_err(|e| ironrdp_connector::custom_err!("TLS upgrade", e))?;

    // GNOME system-mode handover: authenticate with the one-time credentials via
    // RDSTLS on the freshly-upgraded TLS stream, before MCS. The connector selected
    // RDSTLS (not HYBRID) for this reconnect, so `connect_finalize` skips CredSSP and
    // goes straight to the MCS connect once this succeeds.
    if let Some(creds) = rdstls_creds {
        crate::rdstls::authenticate(&mut tls_stream, creds)
            .await
            .map_err(|e| ironrdp_connector::custom_err!("RDSTLS auth", e))?;
    }

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

    let erased: Box<dyn AsyncReadWrite + Unpin + Send + Sync> = Box::new(tls_stream);
    let mut upgraded_framed = TokioFramed::new_with_leftover(erased, leftover);

    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&tls_cert)
        .ok_or_else(|| ironrdp_connector::general_err!("no TLS server public key"))?
        .to_owned();

    let connection_result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut ReqwestNetworkClient::new(),
        ServerName::new(&request.host),
        server_public_key,
        None,
    )
    .await?;

    Ok((connection_result, upgraded_framed))
}

/// Snapshot the whole decoded image as one framebuffer tile. `data()` is already
/// `R,G,B,A`, which is exactly what the app's `iced` image expects.
//
// TODO(perf): send only the dirty region from `GraphicsUpdate` instead of the
// full frame to cut IPC bandwidth; the app already keeps a full framebuffer.
fn full_frame_tile(image: &DecodedImage) -> HostMsg {
    HostMsg::Tile {
        x: 0,
        y: 0,
        width: image.width(),
        height: image.height(),
        rgba: image.data().to_vec(),
    }
}

/// The active-session pump. Returns when the server terminates the session or the
/// app drops its input channel.
async fn active_session(
    framed: UpgradedFramed,
    connection_result: ConnectionResult,
    input_rx: &mut tokio_mpsc::UnboundedReceiver<InputEvent>,
    host_tx: &std_mpsc::Sender<HostMsg>,
    egfx: &SharedEgfx,
) -> Result<Option<crate::redirect::Redirection>, ironrdp_session::SessionError> {
    let (mut reader, mut writer) = split_tokio_framed(framed);

    let desktop_size = connection_result.desktop_size;
    // Size the app last knows about; an EGFX reset to a different size sends a
    // Resized before the next tile.
    let mut egfx_size = (desktop_size.width, desktop_size.height);
    let mut image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
    let activation_factory = connection_result.activation_factory;
    // Server Redirection PDUs arrive on the I/O channel; we intercept them before
    // the active stage (which can't decode them).
    let io_channel_id = connection_result.io_channel_id;

    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();

    // Input scancode/pointer state machine.
    let mut input_db = Database::new();
    // Once the app's input channel closes we request a graceful shutdown ONCE and
    // then stop polling it: `recv()` returns `None` immediately on every subsequent
    // call, which would otherwise busy-loop the whole session and re-send shutdown
    // PDUs until the server happens to close the TCP connection.
    let mut input_open = true;

    'session: loop {
        let outputs = tokio::select! {
            frame_read = reader.read_pdu() => {
                let (action, payload) = frame_read
                    .map_err(|e| ironrdp_session::custom_err!("read PDU", e))?;
                // Intercept a Server Redirection PDU (GNOME system-mode handover)
                // before the active stage, which can't decode it.
                if matches!(action, ironrdp_pdu::Action::X224) {
                    if let Some(redirection) = crate::redirect::detect(&payload, io_channel_id) {
                        return Ok(Some(redirection));
                    }
                }
                // Tolerate a single PDU the active stage can't decode/process rather
                // than tearing down the whole session: skip it and keep going. GNOME
                // Remote Desktop emits occasional PDUs IronRDP doesn't parse, and one
                // of them was killing the connection right after the handover (the
                // desktop showed "connecting" forever). Losing one frame/order is far
                // better than losing the session.
                match active_stage.process(&mut image, action, &payload) {
                    Ok(outputs) => outputs,
                    Err(error) => {
                        tracing::warn!(
                            ?action,
                            payload_len = payload.len(),
                            "skipping PDU the active stage could not process: {error}"
                        );
                        Vec::new()
                    }
                }
            }
            input = input_rx.recv(), if input_open => {
                match input {
                    // App dropped the sender: shut down gracefully, exactly once.
                    None => {
                        input_open = false;
                        active_stage.graceful_shutdown()?
                    }
                    Some(InputEvent::Resize { width, height }) => {
                        let (w, h) = MonitorLayoutEntry::adjust_display_size(
                            u32::from(width), u32::from(height));
                        match active_stage.encode_resize(w, h, None, None) {
                            Some(frame_res) => vec![ActiveStageOutput::ResponseFrame(frame_res?)],
                            // No DisplayControl acknowledgement yet; ignore until the
                            // channel is ready rather than tearing down the session.
                            None => Vec::new(),
                        }
                    }
                    #[cfg(feature = "clipboard")]
                    Some(InputEvent::ClipboardText(text)) => {
                        crate::clipboard::on_local_copy(&mut active_stage, &text)?
                    }
                    #[cfg(not(feature = "clipboard"))]
                    Some(InputEvent::ClipboardText(_)) => Vec::new(),
                    // Mouse / key / unicode all fold into fast-path events.
                    Some(other) => {
                        let events = map_input(&mut input_db, &other);
                        if events.is_empty() {
                            Vec::new()
                        } else {
                            active_stage.process_fastpath_input(&mut image, &events)?
                        }
                    }
                }
            }
        };

        let mut dirty = false;
        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame_bytes) => writer
                    .write_all(&frame_bytes)
                    .await
                    .map_err(|e| ironrdp_session::custom_err!("write frame", e))?,
                ActiveStageOutput::GraphicsUpdate(_region) => dirty = true,
                // Pointer is composited into `image` (software rendering), so these
                // need no separate handling for display.
                ActiveStageOutput::PointerDefault
                | ActiveStageOutput::PointerHidden
                | ActiveStageOutput::PointerPosition { .. }
                | ActiveStageOutput::PointerBitmap(_) => {}
                ActiveStageOutput::DeactivateAll => {
                    // Deactivation-Reactivation (e.g. after a resize): re-run the
                    // activation sequence from the factory captured at connect time,
                    // then rebuild the image at the new size.
                    use ironrdp_connector::connection_activation::ConnectionActivationState;
                    use ironrdp_core::WriteBuf;
                    use ironrdp_session::fast_path;
                    use ironrdp_tokio::single_sequence_step_read;

                    let mut connection_activation = activation_factory.create();
                    let mut buf = WriteBuf::new();
                    'activation: loop {
                        let written = single_sequence_step_read(
                            &mut reader,
                            &mut connection_activation,
                            &mut buf,
                        )
                        .await
                        .map_err(|e| ironrdp_session::custom_err!("reactivation step", e))?;
                        if written.size().is_some() {
                            writer.write_all(buf.filled()).await.map_err(|e| {
                                ironrdp_session::custom_err!("write reactivation", e)
                            })?;
                        }
                        if let ConnectionActivationState::Finalized {
                            desktop_size,
                            share_id,
                            enable_server_pointer,
                            pointer_software_rendering,
                        } = connection_activation.connection_activation_state()
                        {
                            image = DecodedImage::new(
                                PixelFormat::RgbA32,
                                desktop_size.width,
                                desktop_size.height,
                            );
                            active_stage.set_fastpath_processor(
                                fast_path::ProcessorBuilder {
                                    io_channel_id: connection_activation.io_channel_id(),
                                    user_channel_id: connection_activation.user_channel_id(),
                                    share_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                    bulk_decompressor: None,
                                }
                                .build(),
                            );
                            active_stage.set_share_id(share_id);
                            active_stage.set_enable_server_pointer(enable_server_pointer);
                            if host_tx
                                .send(HostMsg::Resized {
                                    width: desktop_size.width,
                                    height: desktop_size.height,
                                })
                                .is_err()
                            {
                                break 'session; // app stopped listening
                            }
                            break 'activation;
                        }
                    }
                    dirty = true;
                }
                ActiveStageOutput::Terminate(_reason) => break 'session,
                // Multitransport (UDP) and auto-detect are advisory; ignore.
                _ => {}
            }
        }

        if dirty && host_tx.send(full_frame_tile(&image)).is_err() {
            // The app stopped listening; end the session.
            break 'session;
        }

        // EGFX graphics (GNOME RDP / modern Windows) are composited into the shared
        // buffer by the pipeline handler that just ran inside `process`; emit them
        // as tiles, preceded by a Resized if the graphics output size changed.
        if let Some((width, height, rgba)) = egfx::take_frame(egfx) {
            if (width, height) != egfx_size {
                egfx_size = (width, height);
                if host_tx
                    .send(HostMsg::Resized { width, height })
                    .is_err()
                {
                    break 'session;
                }
            }
            let tile = HostMsg::Tile {
                x: 0,
                y: 0,
                width,
                height,
                rgba,
            };
            if host_tx.send(tile).is_err() {
                break 'session;
            }
        }
    }

    Ok(None)
}
