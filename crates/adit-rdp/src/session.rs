//! The RDP connection + active-session loop, adapted from the reference
//! `ironrdp-client`: direct TCP → TLS → CredSSP, then a `tokio::select!` loop
//! pumping server PDUs into framebuffer tiles and app input to the server.
//!
//! The loop is transport-agnostic: it receives [`InputEvent`]s from a Tokio
//! channel and emits [`HostMsg`]s to a std channel. The [`crate::host`] layer
//! bridges those channels to the child process's stdin/stdout.

use std::sync::mpsc as std_mpsc;

use adit_rdp_proto::{ConnectRequest, HostMsg, InputEvent};
use ironrdp_connector::{ClientConnector, ConnectionResult, ServerName};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::Database;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{split_tokio_framed, FramedWrite, TokioFramed};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc as tokio_mpsc;

use crate::{build_connector_config, input::map_input, RdpError};

/// A type-erased upgraded (post-TLS) framed transport.
trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}
type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

/// Abort a TCP/TLS/CredSSP handshake that stalls this long.
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Drive a full RDP session to completion: connect, announce, then run the active
/// loop until the server terminates or the app drops its input channel.
pub(crate) async fn run_session(
    request: ConnectRequest,
    mut input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    host_tx: std_mpsc::Sender<HostMsg>,
) -> Result<(), RdpError> {
    // Connect with a timeout, and let a Close (the app dropping stdin ⇒ `input_rx`
    // closed) cancel a hung handshake instead of blocking forever. `input_rx` is
    // only borrowed here, so it's still available for the active session.
    let connect_fut = connect(&request);
    tokio::pin!(connect_fut);
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS));
    tokio::pin!(deadline);
    let (connection_result, framed) = loop {
        tokio::select! {
            result = &mut connect_fut => {
                break result.map_err(|e| RdpError::Connect(e.to_string()))?;
            }
            _ = &mut deadline => {
                return Err(RdpError::Connect(format!(
                    "connection timed out after {CONNECT_TIMEOUT_SECS}s"
                )));
            }
            msg = input_rx.recv() => {
                // None ⇒ the app closed the control channel (cancel). A stray input
                // before we're connected has no surface to act on; ignore it.
                if msg.is_none() {
                    return Err(RdpError::ControlChannelClosed);
                }
            }
        }
    };

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

    active_session(framed, connection_result, input_rx, &host_tx)
        .await
        .map_err(|e| RdpError::Session(e.to_string()))
}

/// Direct TCP connect, TLS upgrade, then CredSSP/NLA finalize.
async fn connect(
    request: &ConnectRequest,
) -> Result<(ConnectionResult, UpgradedFramed), ironrdp_connector::ConnectorError> {
    let dest = format!("{}:{}", request.host, request.port);
    let stream = TcpStream::connect(&dest)
        .await
        .map_err(|e| ironrdp_connector::custom_err!("TCP connect", e))?;
    let client_addr = stream
        .local_addr()
        .map_err(|e| ironrdp_connector::custom_err!("local address", e))?;
    let mut framed = TokioFramed::new(stream);

    let config = build_connector_config(request);

    // DVC (DisplayControl for dynamic resize) is the one dynamic channel we need.
    let drdynvc =
        DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
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
    let (tls_stream, tls_cert) = ironrdp_tls::upgrade(initial_stream, &request.host)
        .await
        .map_err(|e| ironrdp_connector::custom_err!("TLS upgrade", e))?;

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
    mut input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    host_tx: &std_mpsc::Sender<HostMsg>,
) -> Result<(), ironrdp_session::SessionError> {
    let (mut reader, mut writer) = split_tokio_framed(framed);

    let desktop_size = connection_result.desktop_size;
    let mut image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
    let activation_factory = connection_result.activation_factory;

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
                active_stage.process(&mut image, action, &payload)?
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
    }

    Ok(())
}
