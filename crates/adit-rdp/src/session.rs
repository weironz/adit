//! The RDP connection + active-session loop, adapted from the reference
//! `ironrdp-client`: direct TCP → TLS → CredSSP, then a `tokio::select!` loop
//! pumping server PDUs into framebuffer tiles and app input to the server.
//!
//! The loop is transport-agnostic: it receives [`InputEvent`]s from a Tokio
//! channel and emits [`HostMsg`]s to a std channel. The [`crate::host`] layer
//! bridges those channels to the child process's stdin/stdout.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::display::{self, SharedDisplay};
use adit_rdp_proto::{
    ClipFile, ClipboardFileRequestId, ConnectRequest, HostMsg, InputEvent, PointerUpdate,
};
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

use crate::clipboard::{self, SharedClipboard};
use crate::egfx::{self, EgfxHandler, SharedEgfx};
use crate::host::{HostSendError, HostSender, InputEnd, SessionInput};
use crate::{build_connector_config, input::map_input, RdpError};

/// A command from the app for the session loop.
///
/// Input used to be the only thing the app sent, so the channel carried
/// [`InputEvent`] directly. Clipboard file transfer added three messages that
/// are not input at all — they neither map to scancodes nor belong in the input
/// database — so the channel carries this instead and the loop dispatches.
#[derive(Debug)]
pub(crate) enum SessionCmd {
    Input(InputEvent),
    ReplaceableInput(std::sync::Arc<std::sync::Mutex<Option<InputEvent>>>),
    ClipboardTextForRevision {
        stamp: adit_rdp_proto::ClipboardStamp,
        text: String,
    },
    ClipboardImageForRevision {
        stamp: adit_rdp_proto::ClipboardStamp,
        image: Vec<u8>,
    },
    ClipboardPolicyChanged,
    /// Offer a locally-copied image to the remote clipboard (raw CF_DIB).
    ClipboardImage(Vec<u8>),
    /// Offer a local file selection to the remote clipboard (metadata only).
    ClipboardFiles {
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// A local file copy replaced text or an image, but its metadata was discarded.
    LocalFileOfferRejected,
    /// The app read the bytes the remote asked for, or failed to.
    FileContents {
        request: ClipboardFileRequestId,
        data: Option<Vec<u8>>,
    },
    SuspendLocalFileDirection,
    /// The app wants a byte range of a file the remote offered.
    RequestFileContents {
        generation: u64,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
}

impl SessionCmd {
    pub(crate) fn queued_bytes(&self) -> usize {
        let payload = match self {
            Self::Input(InputEvent::ClipboardText(text))
            | Self::ClipboardTextForRevision { text, .. } => text.capacity(),
            Self::Input(InputEvent::MonitorLayout(monitors)) => monitors
                .capacity()
                .saturating_mul(std::mem::size_of::<adit_rdp_proto::Monitor>()),
            Self::ClipboardImage(image) | Self::ClipboardImageForRevision { image, .. } => {
                image.capacity()
            }
            Self::ClipboardFiles { files, .. } => files.iter().fold(
                files
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ClipFile>()),
                |size, file| size.saturating_add(file.name.capacity()),
            ),
            Self::FileContents { data, .. } => data.as_ref().map_or(0, Vec::capacity),
            Self::Input(_)
            | Self::ReplaceableInput(_)
            | Self::LocalFileOfferRejected
            | Self::ClipboardPolicyChanged
            | Self::RequestFileContents { .. }
            | Self::SuspendLocalFileDirection => 0,
        };
        std::mem::size_of::<Self>().saturating_add(payload)
    }
}

fn input_end_error(reason: InputEnd) -> Option<&'static str> {
    match reason {
        InputEnd::Close => None,
        InputEnd::Overloaded => Some("RDP 辅助进程的输入队列已满，会话已关闭"),
        InputEnd::InvalidFrame => Some("RDP 辅助进程收到无效的控制消息，会话已关闭"),
    }
}

const LOCAL_FILES_DISABLED_NOTICE: &str =
    "RDP 本地文件发送至远端已暂时禁用；文字、图片和远端文件接收仍可使用，桌面连接保持正常。";

/// Seal this direction before a connector can create or use a CLIPRDR channel.
fn new_session_clipboard(host: &HostSender) -> SharedClipboard {
    let clipboard = clipboard::new_shared_for_policy(host.clipboard_policy_epoch());
    clipboard::suspend_local_file_direction(&clipboard);
    // Send the acknowledgement first so the specific, persistent explanation
    // is the last notice seen by the app. Neither notice can close the desktop.
    clipboard::flush_local_file_direction_notices(&clipboard, host);
    let _ = host.send(HostMsg::ClipboardFileDirectionUnavailable {
        channel_epoch: 0,
        message: LOCAL_FILES_DISABLED_NOTICE.into(),
    });
    clipboard
}

fn reject_local_file_offer(clipboard: &SharedClipboard, generation: u64, files: Vec<ClipFile>) {
    if files.is_empty() {
        clipboard::offer_local_files(clipboard, generation, files);
    } else {
        drop(files);
        clipboard::reject_local_file_offer(clipboard);
    }
}

fn handle_active_clipboard_command(command: SessionCmd, clipboard: &SharedClipboard) {
    match command {
        SessionCmd::Input(InputEvent::ClipboardText(text)) => {
            clipboard::offer_local_text(clipboard, text);
        }
        SessionCmd::ClipboardImage(image) => clipboard::offer_local_image(clipboard, image),
        SessionCmd::ClipboardTextForRevision { stamp, text } => {
            clipboard::offer_bound_text(clipboard, stamp, text)
        }
        SessionCmd::ClipboardImageForRevision { stamp, image } => {
            clipboard::offer_bound_image(clipboard, stamp, image)
        }
        SessionCmd::ClipboardFiles { generation, files } => {
            reject_local_file_offer(clipboard, generation, files);
        }
        SessionCmd::LocalFileOfferRejected => clipboard::reject_local_file_offer(clipboard),
        SessionCmd::FileContents { request, data: _ } => {
            // Keep the failure bookkeeping, but never pass local bytes to the
            // clipboard backend, even for an unsolicited or stale response.
            clipboard::submit_file_contents(clipboard, request, None);
        }
        SessionCmd::SuspendLocalFileDirection => {
            clipboard::suspend_local_file_direction(clipboard);
        }
        SessionCmd::RequestFileContents {
            generation,
            stream_id,
            index,
            offset,
            length,
        } => clipboard::request_file_contents(
            clipboard, generation, stream_id, index, offset, length,
        ),
        _ => unreachable!("only clipboard commands are routed here"),
    }
}

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
    mut input_rx: SessionInput,
    host_tx: HostSender,
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
    let shared_display = SharedDisplay::default();
    let mut pending_layout = None;
    // Clipboard state outlives a single CLIPRDR channel on purpose: a GNOME
    // system-mode handover redirects and rebuilds the channel, and the text the
    // user copied did not change under them.
    let shared_clipboard = new_session_clipboard(&host_tx);
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
            let connect_fut = Box::pin(connect(
                &request,
                &shared_egfx,
                &shared_clipboard,
                &shared_display,
                routing_token.as_deref(),
                rdstls_creds.as_ref(),
            ));
            drive_handshake(
                async {
                    connect_fut.await.map_err(|error| {
                        tracing::error!("RDP connect failed: {error}");
                        RdpError::Connect(error.to_string())
                    })
                },
                &mut input_rx,
                &shared_clipboard,
                &mut pending_layout,
                &host_tx,
                std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
            )
            .await?
        };

        // Announce the negotiated size once (the first successful connect); after a
        // redirect the desktop size, if different, comes through as a Resized tile.
        if !announced {
            announced = true;
            let desktop = connection_result.desktop_size;
            host_tx
                .send_when_ready(HostMsg::Connected {
                    width: desktop.width,
                    height: desktop.height,
                })
                .await
                .map_err(|error| match error {
                    HostSendError::Closed => RdpError::ControlChannelClosed,
                    HostSendError::Overloaded => RdpError::Session(error.to_string()),
                })?;
        }

        let active_end = input_rx.end.clone();
        let active = async {
            Box::pin(active_session(
                framed,
                connection_result,
                &mut input_rx,
                &host_tx,
                &shared_egfx,
                &shared_clipboard,
                (&shared_display, &mut pending_layout),
            ))
            .await
            .map_err(|e| RdpError::Session(e.to_string()))
        };
        let Some(active_result) = guard_active_session(active, active_end).await? else {
            return Ok(());
        };
        match active_result {
            None => return Ok(()),
            Some(redir) => {
                // The old server is already gone. Fail its reads and revoke its
                // advertised files before DNS/TCP/TLS for the new server begins.
                guard_handshake(
                    async {
                        clipboard::leave_channel(&shared_clipboard, &host_tx)
                            .await
                            .map_err(|error| RdpError::Session(error.to_string()))
                    },
                    &mut input_rx.end,
                    std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
                )
                .await?;
                if pending_layout.is_none() {
                    let sent = shared_display
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .sent_layout
                        .clone();
                    if !sent.is_empty() {
                        pending_layout = Some(sent);
                    }
                }
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

/// Allow the active loop to perform its normal graceful shutdown; a Close must
/// still terminate it when output backpressure prevents the loop from polling.
pub(crate) async fn guard_active_session<F, T>(
    active: F,
    mut end: tokio::sync::watch::Receiver<Option<InputEnd>>,
) -> Result<Option<T>, RdpError>
where
    F: std::future::Future<Output = Result<T, RdpError>>,
{
    tokio::pin!(active);
    let reason = loop {
        if let Some(reason) = *end.borrow_and_update() {
            break reason;
        }
        tokio::select! {
            biased;
            result = &mut active => return result.map(Some),
            changed = end.changed() => {
                if changed.is_err() { break InputEnd::Close; }
            }
        }
    };
    if let Some(message) = input_end_error(reason) {
        return Err(RdpError::Session(message.into()));
    }
    match tokio::time::timeout(std::time::Duration::from_millis(100), &mut active).await {
        Ok(Ok(result)) => Ok(Some(result)),
        Ok(Err(_)) | Err(_) => Ok(None),
    }
}

/// Keep the deadline and shutdown signal live while output waits for capacity.
pub(crate) async fn guard_handshake<F, T>(
    work: F,
    end: &mut tokio::sync::watch::Receiver<Option<InputEnd>>,
    timeout: std::time::Duration,
) -> Result<T, RdpError>
where
    F: std::future::Future<Output = Result<T, RdpError>>,
{
    let stopped = async {
        loop {
            let reason = *end.borrow_and_update();
            if let Some(reason) = reason {
                return Err(input_end_error(reason)
                    .map_or(RdpError::ControlChannelClosed, |message| {
                        RdpError::Session(message.into())
                    }));
            }
            if end.changed().await.is_err() {
                return Err(RdpError::ControlChannelClosed);
            }
        }
    };
    tokio::select! {
        biased;
        result = stopped => result,
        _ = tokio::time::sleep(timeout) => Err(RdpError::Connect(format!(
            "connection timed out after {}s", timeout.as_secs()
        ))),
        result = work => result,
    }
}

/// The completed connect is a barrier for commands already queued when the
/// new channel became ready.
pub(crate) async fn drive_handshake<F, T>(
    connect: F,
    input: &mut SessionInput,
    clipboard: &SharedClipboard,
    pending_layout: &mut Option<Vec<adit_rdp_proto::Monitor>>,
    host: &HostSender,
    timeout: std::time::Duration,
) -> Result<T, RdpError>
where
    F: std::future::Future<Output = Result<T, RdpError>>,
{
    let SessionInput {
        commands,
        end,
        ordering,
        ..
    } = input;
    let work = async {
        tokio::pin!(connect);
        let mut clipboard_tick = tokio::time::interval(std::time::Duration::from_millis(50));
        clipboard_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            clipboard::flush_app_actions(clipboard, host)
                .await
                .map_err(|error| RdpError::Session(error.to_string()))?;
            tokio::select! {
                _ = clipboard_tick.tick() => {}
                command = commands.recv() => match command {
                    Some(queued) => handle_handshake_command(
                        queued.command, clipboard, pending_layout, host,
                    ).await?,
                    None => return Err(RdpError::ControlChannelClosed),
                },
                result = &mut connect => {
                    let connected = result?;
                    // A ready callback and an input enqueue can happen in the
                    // same connect poll. Apply that finite snapshot before any
                    // wire action on the newly connected channel is pumped.
                    let queued_at_connect = {
                        let _ordering = ordering.lock().unwrap_or_else(|error| error.into_inner());
                        commands.len()
                    };
                    for _ in 0..queued_at_connect {
                        let Ok(queued) = commands.try_recv() else { break };
                        handle_handshake_command(
                            queued.command, clipboard, pending_layout, host,
                        ).await?;
                    }
                    clipboard::flush_app_actions(clipboard, host)
                        .await
                        .map_err(|error| RdpError::Session(error.to_string()))?;
                    return Ok(connected);
                }
            }
        }
    };
    guard_handshake(work, end, timeout).await
}

/// Keep the GUI-owned clipboard current while a redirect rebuilds CLIPRDR.
/// Input destined for the old desktop cannot be replayed on the new connection.
pub(crate) async fn handle_handshake_command(
    command: SessionCmd,
    clipboard: &SharedClipboard,
    pending_layout: &mut Option<Vec<adit_rdp_proto::Monitor>>,
    host: &HostSender,
) -> Result<(), RdpError> {
    if matches!(&command, SessionCmd::SuspendLocalFileDirection) {
        // An already-decoded suspension must suppress a queued Ready notice and
        // old file actions before this handshake flush publishes either one.
        clipboard::suspend_local_file_direction(clipboard);
    }
    // Deliver earlier remote revocations before responding to a later command.
    clipboard::flush_app_actions(clipboard, host)
        .await
        .map_err(|error| RdpError::Session(error.to_string()))?;
    match command {
        SessionCmd::Input(InputEvent::MonitorLayout(layout)) => *pending_layout = Some(layout),
        SessionCmd::Input(InputEvent::ClipboardText(text)) => {
            clipboard::offer_local_text(clipboard, text);
        }
        SessionCmd::ClipboardImage(image) => clipboard::offer_local_image(clipboard, image),
        SessionCmd::ClipboardTextForRevision { stamp, text } => {
            clipboard::offer_bound_text(clipboard, stamp, text)
        }
        SessionCmd::ClipboardImageForRevision { stamp, image } => {
            clipboard::offer_bound_image(clipboard, stamp, image)
        }
        SessionCmd::ClipboardPolicyChanged => {
            let (policy_epoch, enabled) = clipboard::apply_shared_policy(clipboard);
            host.send(HostMsg::ClipboardPolicyApplied {
                policy_epoch,
                enabled,
            })
            .map_err(|error| RdpError::Session(error.to_string()))?;
        }
        SessionCmd::ClipboardFiles { generation, files } => {
            reject_local_file_offer(clipboard, generation, files);
        }
        SessionCmd::LocalFileOfferRejected => clipboard::reject_local_file_offer(clipboard),
        SessionCmd::SuspendLocalFileDirection => {
            clipboard::suspend_local_file_direction(clipboard);
        }
        SessionCmd::RequestFileContents {
            generation,
            stream_id,
            ..
        } => {
            // The old channel is gone, so explicitly fail a paste racing a redirect.
            host.send_when_ready(HostMsg::FileContents {
                generation,
                stream_id,
                data: None,
            })
            .await
            .map_err(|error| match error {
                HostSendError::Closed => RdpError::ControlChannelClosed,
                HostSendError::Overloaded => RdpError::Session(error.to_string()),
            })?;
        }
        SessionCmd::Input(_)
        | SessionCmd::ReplaceableInput(_)
        | SessionCmd::FileContents { .. } => {}
    }
    clipboard::flush_local_file_direction_notices(clipboard, host);
    clipboard::flush_app_actions(clipboard, host)
        .await
        .map_err(|error| RdpError::Session(error.to_string()))
}

/// Direct TCP connect, TLS upgrade, then finalize. On the initial connect the
/// connector does CredSSP/NLA; on a redirection reconnect (`routing_token` set) it
/// negotiates RDSTLS instead, and `rdstls_creds` carries the one-time credentials
/// we authenticate with on the TLS stream before MCS.
async fn connect(
    request: &ConnectRequest,
    shared_egfx: &SharedEgfx,
    shared_clipboard: &SharedClipboard,
    shared_display: &SharedDisplay,
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
    // don't.
    //
    // With an H.264 decoder plugged in, AVC420 becomes the preferred codec:
    // the server encodes the screen as video (the same family of path mstsc
    // uses) instead of the RemoteFX-Progressive tile mosaic, which stays as
    // the fallback for hosts that can't encode AVC (GPU-less VMs, xrdp,
    // GNOME Remote Desktop without VA-API). If the decoder fails to
    // construct, fall back to no-AVC caps rather than failing the connect —
    // a session on the tile path beats no session.
    let h264: Option<Box<dyn ironrdp_egfx::decode::H264Decoder>> =
        match ironrdp_egfx::decode::OpenH264Decoder::new() {
            Ok(decoder) => Some(Box::new(decoder)),
            Err(error) => {
                tracing::warn!(%error, "H.264 decoder unavailable; falling back to tile codecs");
                None
            }
        };
    let egfx_client =
        GraphicsPipelineClient::new(Box::new(EgfxHandler::new(Arc::clone(shared_egfx))), h264);
    let display_caps = Arc::clone(shared_display);
    *shared_display.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
    let drdynvc = DrdynvcClient::new()
        .with_dynamic_channel(DisplayControlClient::new(move |caps| {
            display::capabilities(&display_caps, &caps);
            Ok(Vec::new())
        }))
        .with_dynamic_channel(egfx_client);
    // `mut` for the `&mut connector` borrows in `connect_begin` /
    // `mark_as_upgraded`, and for the optional-channel blocks below, which
    // consume and re-assign it.
    let mut connector = ClientConnector::new(config, client_addr).with_static_channel(drdynvc);

    // Audio (RDPSND) via cpal.
    #[cfg(feature = "sound")]
    if request.enable_audio {
        use ironrdp_rdpsnd_native::cpal;
        connector = connector.with_static_channel(ironrdp_rdpsnd::client::Rdpsnd::new(Box::new(
            cpal::RdpsndBackend::new(),
        )));
    }

    // Clipboard (CLIPRDR). The backend here is a pure protocol adapter — the GUI
    // app owns the real Windows clipboard, because this helper is windowless and
    // has no message pump. See `clipboard.rs`.
    if request.enable_clipboard {
        connector = connector.with_static_channel(ironrdp_cliprdr::CliprdrClient::new(Box::new(
            crate::clipboard::AditCliprdrBackend::new(Arc::clone(shared_clipboard)),
        )));
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

/// Copy only changed legacy pixels, using the same cost planner as EGFX.
fn image_tiles(image: &DecodedImage, mut rects: Vec<egfx::Rect>) -> Vec<HostMsg> {
    egfx::prefer_full_frame(
        &mut rects,
        usize::from(image.width()),
        usize::from(image.height()),
    );
    rects
        .into_iter()
        .filter_map(|(x, y, w, h)| {
            let fw = usize::from(image.width());
            let fh = usize::from(image.height());
            if x >= fw || y >= fh {
                return None;
            }
            let w = w.min(fw - x);
            let h = h.min(fh - y);
            if w == 0 || h == 0 {
                return None;
            }
            let mut rgba = Vec::with_capacity(w * h * 4);
            for row in y..y + h {
                let start = (row * fw + x) * 4;
                rgba.extend_from_slice(&image.data()[start..start + w * 4]);
            }
            Some(HostMsg::Tile {
                x: x as u16,
                y: y as u16,
                width: w as u16,
                height: h as u16,
                rgba,
            })
        })
        .collect()
}

/// Keep accelerated RGBA and the hotspot intact, including cached updates.
fn pointer_update(output: &ActiveStageOutput) -> Option<PointerUpdate> {
    Some(match output {
        ActiveStageOutput::PointerDefault => PointerUpdate::Default,
        ActiveStageOutput::PointerHidden => PointerUpdate::Hidden,
        ActiveStageOutput::PointerPosition { x, y } => PointerUpdate::Position { x: *x, y: *y },
        ActiveStageOutput::PointerBitmap(p) => PointerUpdate::Bitmap {
            width: p.width,
            height: p.height,
            hotspot_x: p.hotspot_x,
            hotspot_y: p.hotspot_y,
            rgba: p.bitmap_data.clone(),
        },
        _ => return None,
    })
}

/// Call immediately after process(), before inspecting any output. The EGFX
/// trait cannot return a decode error, and its stateful failure must not be
/// mistaken for one of GNOME's independently skippable, unrecognised PDUs.
pub(crate) fn finish_active_process(
    processed: Result<Vec<ActiveStageOutput>, ironrdp_session::SessionError>,
    egfx: &SharedEgfx,
    action: ironrdp_pdu::Action,
    payload: &[u8],
) -> Result<Vec<ActiveStageOutput>, ironrdp_session::SessionError> {
    if let Some(message) = egfx::take_decode_error(egfx) {
        // Custom-error Display omits its source chain; Reason keeps the codec
        // failure visible when run_session serialises this error to the GUI.
        return Err(ironrdp_session::reason_err!("EGFX decode", "{message}"));
    }
    match processed {
        Ok(outputs) => Ok(outputs),
        Err(_) => {
            static SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let count = SKIPPED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .wrapping_add(1);
            if count <= 4 || count.is_power_of_two() {
                tracing::warn!(
                    ?action,
                    payload_len = payload.len(),
                    count,
                    "skipping PDU the active stage could not process"
                );
            }
            Ok(Vec::new())
        }
    }
}

/// The active-session pump. Returns when the server terminates the session or the
/// app drops its input channel.
async fn active_session(
    framed: UpgradedFramed,
    connection_result: ConnectionResult,
    input_rx: &mut SessionInput,
    host_tx: &HostSender,
    egfx: &SharedEgfx,
    clip: &SharedClipboard,
    display_request: (&SharedDisplay, &mut Option<Vec<adit_rdp_proto::Monitor>>),
) -> Result<Option<crate::redirect::Redirection>, ironrdp_session::SessionError> {
    let (display, pending_layout) = display_request;
    let (mut reader, mut writer) = split_tokio_framed(framed);

    let desktop_size = connection_result.desktop_size;
    // Size the app last knows about; an EGFX reset to a different size sends a
    // Resized before the next tile.
    let mut egfx_size = (desktop_size.width, desktop_size.height);
    // A resize requested before the DisplayControl channel is up would be
    // silently dropped; park it here and retry once the channel connects.
    let mut pending_resize: Option<(u32, u32)> = None;
    let mut last_display_state = None;
    let mut keys_logged = 0u32;
    // Frame-flow probe: a frozen desktop and a healthy one look identical from
    // outside, so record what actually reached the app.
    let mut frames_emitted = 0u32;
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
    let mut pending_clipboard_frames = VecDeque::new();
    let mut clipboard_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    clipboard_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Latches once the EGFX pipeline paints; from then on the legacy `image` is
    // not the display surface and must never be emitted (it would flash black).
    let mut egfx_active = false;

    'session: loop {
        if let Some(reason) = input_open.then(|| *input_rx.end.borrow()).flatten() {
            if let Some(message) = input_end_error(reason) {
                return Err(ironrdp_session::custom_err!(
                    "helper input",
                    std::io::Error::other(message)
                ));
            }
        }
        // A queued clipboard replacement must be applied before we can commit
        // any older file frame. Ordinary keyboard/mouse input does not block it.
        let pending_offer = input_open && input_rx.has_pending_offer();
        let outputs = tokio::select! {
            _ = clipboard_tick.tick() => Vec::new(),
            _ = input_rx.end.changed(), if input_open => {
                let reason = input_rx.end.borrow_and_update().unwrap_or(InputEnd::Close);
                if let Some(message) = input_end_error(reason) {
                    return Err(ironrdp_session::custom_err!("helper input", std::io::Error::other(message)));
                }
                input_open = false;
                active_stage.graceful_shutdown()?
            }
            frame_read = reader.read_pdu(), if !pending_offer => {
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
                let processed = active_stage.process(&mut image, action, &payload);
                finish_active_process(processed, egfx, action, &payload)?
            }
            input = input_rx.commands.recv(), if input_open => {
                let reason = *input_rx.end.borrow();
                if let Some(message) = reason.and_then(input_end_error) {
                    return Err(ironrdp_session::custom_err!("helper input", std::io::Error::other(message)));
                }
                let input = if reason.is_some() { None } else { input.map(|queued| queued.command) };
                let input = match input {
                    Some(SessionCmd::ReplaceableInput(slot)) => {
                        let event = slot.lock().unwrap_or_else(|error| error.into_inner()).take();
                        if event.is_none() { continue; }
                        event.map(SessionCmd::Input)
                    }
                    other => other,
                };
                match input {
                    Some(SessionCmd::ReplaceableInput(_)) => unreachable!("replaceable input is resolved above"),
                    // App dropped the sender: shut down gracefully, exactly once.
                    None => {
                        let reason = input_rx.end.borrow().unwrap_or(InputEnd::Close);
                        if let Some(message) = input_end_error(reason) {
                            return Err(ironrdp_session::custom_err!("helper input", std::io::Error::other(message)));
                        }
                        input_open = false;
                        active_stage.graceful_shutdown()?
                    }
                    Some(SessionCmd::Input(InputEvent::MonitorLayout(layout))) => {
                        *pending_layout = Some(layout);
                        pending_resize = None;
                        Vec::new()
                    }
                    Some(SessionCmd::Input(InputEvent::Resize { width, height })) => {
                        let (w, h) = MonitorLayoutEntry::adjust_display_size(
                            u32::from(width), u32::from(height));
                        match active_stage.encode_resize(w, h, None, None) {
                            Some(frame_res) => {
                                // Logged because the whole path was silent, and a
                                // desktop stuck at its connect-time size is
                                // indistinguishable from one that was never asked
                                // to move. Now the log says which.
                                tracing::info!(
                                    requested_w = width, requested_h = height,
                                    adjusted_w = w, adjusted_h = h,
                                    "resize sent to the server"
                                );
                                pending_resize = None;
                                vec![ActiveStageOutput::ResponseFrame(frame_res?)]
                            }
                            // DisplayControl not connected yet. Park the request
                            // and retry each loop turn: dropping it silently left
                            // the desktop at its connect-time size for the rest of
                            // the session whenever the window was resized in the
                            // first seconds (the app-side dedupe records the
                            // request as sent and never repeats it).
                            None => {
                                // DisplayControl is a dynamic channel the server
                                // opens on its own schedule, and some never do. A
                                // resize parked here forever is exactly what
                                // "the desktop never refills the window" looks
                                // like, so say so rather than retrying in silence.
                                tracing::info!(
                                    requested_w = w, requested_h = h,
                                    "resize parked: DisplayControl is not connected yet"
                                );
                                pending_resize = Some((w, h));
                                Vec::new()
                            }
                        }
                    }
                    Some(SessionCmd::ClipboardPolicyChanged) => {
                        apply_clipboard_policy_barrier(clip, &mut pending_clipboard_frames, &mut active_stage, host_tx)?;
                        Vec::new()
                    }
                    Some(command @ (SessionCmd::Input(InputEvent::ClipboardText(_))
                        | SessionCmd::ClipboardImage(_)
                        | SessionCmd::ClipboardTextForRevision { .. }
                        | SessionCmd::ClipboardImageForRevision { .. }
                        | SessionCmd::ClipboardFiles { .. }
                        | SessionCmd::LocalFileOfferRejected
                        | SessionCmd::FileContents { .. }
                        | SessionCmd::SuspendLocalFileDirection
                        | SessionCmd::RequestFileContents { .. })) => {
                        handle_active_clipboard_command(command, clip);
                        Vec::new()
                    }
                    // Mouse / key / unicode all fold into fast-path events.
                    Some(SessionCmd::Input(other)) => {
                        // Input-path probe: the first few key events per session
                        // go to the log, so "typing does nothing" can be split
                        // into UI-side loss vs server-side rejection by reading
                        // the helper log alone.
                        if keys_logged < 40 {
                            if let InputEvent::Key { scancode, extended, pressed } = &other {
                                keys_logged += 1;
                                tracing::info!(scancode, extended, pressed, "keyboard input reached the helper");
                            } else if let InputEvent::Unicode { ch, pressed } = &other {
                                keys_logged += 1;
                                tracing::info!(%ch, pressed, "unicode input reached the helper");
                            }
                        }
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

        let mut dirty_rects = Vec::new();
        for out in outputs {
            if let Some(pointer) = pointer_update(&out) {
                send_host(host_tx, HostMsg::Pointer(pointer)).await?;
                continue;
            }
            match out {
                ActiveStageOutput::ResponseFrame(frame_bytes) => writer
                    .write_all(&frame_bytes)
                    .await
                    .map_err(|e| ironrdp_session::custom_err!("write frame", e))?,
                ActiveStageOutput::GraphicsUpdate(region) => {
                    if region.right >= region.left && region.bottom >= region.top {
                        egfx::add_dirty_rect(
                            &mut dirty_rects,
                            (
                                usize::from(region.left),
                                usize::from(region.top),
                                usize::from(region.right) - usize::from(region.left) + 1,
                                usize::from(region.bottom) - usize::from(region.top) + 1,
                            ),
                        );
                    }
                }
                ActiveStageOutput::DeactivateAll => {
                    // Deactivation-Reactivation (e.g. after a resize): re-run the
                    // activation sequence from the factory captured at connect time,
                    // then rebuild the image at the new size.
                    use ironrdp_connector::connection_activation::ConnectionActivationState;
                    use ironrdp_connector::Sequence;
                    use ironrdp_core::WriteBuf;
                    use ironrdp_session::fast_path;

                    let mut connection_activation = activation_factory.create();
                    let mut buf = WriteBuf::new();
                    'activation: loop {
                        // The GNOME handover can land HERE, not only in the main
                        // loop. Logging in at GDM makes gnome-remote-desktop hand the
                        // client to the session daemon it has just created, and that
                        // redirection arrives while a deactivation-reactivation is in
                        // flight. The main loop peeks every X.224 PDU for a Server
                        // Redirection; this loop reads the same stream and used to
                        // hand every PDU straight to the activation sequence, which
                        // cannot decode one — so the handover was swallowed, reported
                        // as a decode error, and abandoned. The session then sat on a
                        // daemon that had stopped painting: a **black desktop on a
                        // connection that looked healthy**, and reconnecting "fixed"
                        // it only because by then there was a session to route to.
                        //
                        // Inlined instead of `single_sequence_step_read` for one
                        // reason: the peek has to happen between the read and the
                        // step, and that helper does both.
                        let pdu = if let Some(hint) = connection_activation.next_pdu_hint() {
                            match reader.read_by_hint(hint).await {
                                Ok(pdu) => Some(pdu),
                                Err(error) => {
                                    tracing::warn!(
                                        "abandoning reactivation, keeping session: read {error}"
                                    );
                                    break 'activation;
                                }
                            }
                        } else {
                            None
                        };
                        buf.clear();
                        let stepped = match &pdu {
                            Some(bytes) => {
                                if let Some(redirection) =
                                    crate::redirect::detect(bytes, io_channel_id)
                                {
                                    tracing::info!(
                                        "server redirection arrived during reactivation"
                                    );
                                    return Ok(Some(redirection));
                                }
                                connection_activation.step(bytes, &mut buf)
                            }
                            None => connection_activation.step_no_input(&mut buf),
                        };
                        // Tolerate a failed reactivation step rather than tearing the
                        // whole session down: GNOME interleaves stray PDUs into the
                        // reactivation sequence that the sequence can't decode, and
                        // for an EGFX session the graphics keep flowing on the shared
                        // buffer regardless of the legacy `image`. Abandon the
                        // reactivation and keep the session alive.
                        let written = match stepped {
                            Ok(w) => w,
                            Err(_) => {
                                // The length is useful for diagnosis without logging
                                // clipboard or other sensitive PDU payloads.
                                tracing::warn!(
                                    payload_len = pdu.as_ref().map_or(0, |bytes| bytes.len()),
                                    "abandoning reactivation, keeping session"
                                );
                                break 'activation;
                            }
                        };
                        if written.size().is_some() {
                            if let Err(error) = writer.write_all(buf.filled()).await {
                                tracing::warn!(
                                    "reactivation write failed, keeping session: {error}"
                                );
                                break 'activation;
                            }
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
                            // Only when the desktop actually changed size: the
                            // app repaints from a fresh surface on Resized, so an
                            // unconditional send here blacked the pane on every
                            // deactivation-reactivation until the next repaint.
                            if (desktop_size.width, desktop_size.height) != egfx_size {
                                egfx_size = (desktop_size.width, desktop_size.height);
                                send_host(
                                    host_tx,
                                    HostMsg::Resized {
                                        width: desktop_size.width,
                                        height: desktop_size.height,
                                    },
                                )
                                .await?;
                            }
                            break 'activation;
                        }
                    }
                    egfx::add_dirty_rect(
                        &mut dirty_rects,
                        (
                            0,
                            0,
                            usize::from(image.width()),
                            usize::from(image.height()),
                        ),
                    );
                }
                ActiveStageOutput::Terminate(_reason) => break 'session,
                // Multitransport (UDP) and auto-detect are advisory; ignore.
                _ => {}
            }
        }

        // CLIPRDR work the backend queued while it ran inside `process` above (it
        // is borrowed by the processor there and can reach neither the channel
        // nor the socket), plus anything a local copy just enqueued. Emitted
        // after this iteration's response frames so the wire ordering stays sane.
        flush_clipboard_output(
            writer.get_inner_mut().0,
            &mut pending_clipboard_frames,
            clip,
            &mut active_stage,
            host_tx,
            input_open.then_some(&*input_rx),
        )
        .await?;

        // Emit the legacy DecodedImage only for legacy-bitmap servers. Once the
        // EGFX pipeline has produced a frame, `image` is NOT the display surface
        // (GNOME/modern Windows paint via EGFX into the shared buffer, leaving
        // `image` black apart from a software cursor); emitting it would flash a
        // black frame over the real content — including right after a
        // Deactivation-Reactivation, which rebuilds `image` empty.
        if !egfx_active {
            for tile in image_tiles(&image, dirty_rects) {
                send_host(host_tx, tile).await?;
            }
        }

        if let Some(layout) = pending_layout.as_ref() {
            if let Some(encoded) = display::encode_layout(&mut active_stage, layout, display) {
                match encoded {
                    Ok(bytes) => {
                        writer
                            .write_all(&bytes)
                            .await
                            .map_err(|e| ironrdp_session::custom_err!("write monitor layout", e))?;
                        let mut state = display.lock().unwrap_or_else(|e| e.into_inner());
                        state.sent_layout = layout.clone();
                        state.applied = false;
                        state.error = None;
                    }
                    Err(error) => {
                        display.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error)
                    }
                }
                *pending_layout = None;
            }
        }
        let state = display.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if last_display_state.as_ref() != Some(&state) {
            send_host(host_tx, HostMsg::DisplayControl(state.clone())).await?;
            last_display_state = Some(state);
        }

        // Retry a parked resize now that more channels may have connected.
        if let Some((w, h)) = pending_resize {
            if let Some(frame_res) = active_stage.encode_resize(w, h, None, None) {
                tracing::info!(w, h, "parked resize went out once DisplayControl connected");
                pending_resize = None;
                writer
                    .write_all(&frame_res?)
                    .await
                    .map_err(|e| ironrdp_session::custom_err!("write resize frame", e))?;
            }
        }

        // EGFX graphics (GNOME RDP / modern Windows) are composited into the shared
        // buffer by the pipeline handler that just ran inside `process`; emit them
        // as tiles, preceded by a Resized if the graphics output size changed.
        let updates = egfx::take_frames(egfx);
        if let Some(first) = updates.first() {
            egfx_active = true;
            if (first.surface_width, first.surface_height) != egfx_size {
                egfx_size = (first.surface_width, first.surface_height);
                send_host(
                    host_tx,
                    HostMsg::Resized {
                        width: first.surface_width,
                        height: first.surface_height,
                    },
                )
                .await?;
            }
        }
        // Only the changed regions cross the pipe: full frames are ~9 MB at
        // high-DPI sizes, and serializing one per present was a large slice of
        // the perceived lag. Several small tiles beat one rect stretched around
        // them — the far side blits each into the same surface, so the extra
        // messages cost a header apiece and nothing else.
        for update in updates {
            if frames_emitted < 30 {
                frames_emitted += 1;
                tracing::info!(
                    x = update.x,
                    y = update.y,
                    w = update.width,
                    h = update.height,
                    "emitted frame region"
                );
            }
            send_host(
                host_tx,
                HostMsg::Tile {
                    x: update.x,
                    y: update.y,
                    width: update.width,
                    height: update.height,
                    rgba: update.rgba,
                },
            )
            .await?;
        }
    }

    Ok(None)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClipboardCommit {
    Deferred,
    Written,
    Discarded,
}

pub(crate) fn apply_clipboard_policy_barrier(
    clip: &SharedClipboard,
    pending: &mut VecDeque<clipboard::ClipboardWireFrame>,
    stage: &mut ironrdp_session::ActiveStage,
    host: &HostSender,
) -> Result<(), ironrdp_session::SessionError> {
    let (policy_epoch, enabled) = clipboard::apply_shared_policy(clip);
    // The session cannot reach this point while a partially written frame is
    // still being completed. Every remaining frame has sent no first byte.
    let mut valid = VecDeque::new();
    while let Some(mut frame) = pending.pop_front() {
        if clipboard::revalidate_wire_frame(&mut frame, clip, stage).unwrap_or(false)
            && (enabled || frame.is_safe_failure())
        {
            valid.push_back(frame);
        }
    }
    *pending = valid;
    host.send(HostMsg::ClipboardPolicyApplied {
        policy_epoch,
        enabled,
    })
    .map_err(|error| ironrdp_session::custom_err!("clipboard policy", error))
}

pub(crate) async fn flush_clipboard_output<W: AsyncWrite + Unpin>(
    writer: &mut W,
    pending: &mut VecDeque<clipboard::ClipboardWireFrame>,
    clip: &SharedClipboard,
    stage: &mut ironrdp_session::ActiveStage,
    host: &HostSender,
    input: Option<&SessionInput>,
) -> Result<(), ironrdp_session::SessionError> {
    loop {
        clipboard::flush_local_file_direction_notices(clip, host);
        if pending.is_empty() {
            match clipboard::pump_with_pending_input(clip, stage, host, input).await {
                Ok(frames) => pending.extend(frames),
                Err(error @ clipboard::ClipboardPumpError::AppClosed) => {
                    return Err(ironrdp_session::custom_err!("clipboard", error));
                }
                Err(error) => {
                    tracing::debug!(%error, "clipboard work is unavailable; keeping the desktop open");
                    return Ok(());
                }
            }
            if pending.is_empty() {
                clipboard::flush_local_file_direction_notices(clip, host);
                return Ok(());
            }
        }
        while let Some(frame) = pending.front_mut() {
            let result = commit_clipboard_frame(writer, frame, clip, stage, input).await?;
            clipboard::flush_local_file_direction_notices(clip, host);
            match result {
                ClipboardCommit::Deferred => return Ok(()),
                ClipboardCommit::Written | ClipboardCommit::Discarded => {
                    pending.pop_front();
                }
            }
        }
    }
}

/// A successful first write transfers ownership to the transport. Until then a
/// replacement wakes this future and is processed before revalidation. TLS may
/// buffer accepted bytes without sending them yet, but `AsyncWrite` cannot retract
/// them; completing the frame avoids corrupting the live RDP byte stream.
pub(crate) async fn commit_clipboard_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &mut clipboard::ClipboardWireFrame,
    clip: &SharedClipboard,
    stage: &mut ironrdp_session::ActiveStage,
    input: Option<&SessionInput>,
) -> Result<ClipboardCommit, ironrdp_session::SessionError> {
    use std::task::Poll;

    loop {
        let needs_barrier = frame.needs_offer_barrier();
        let needs_suspension_barrier = frame.needs_suspension_barrier();
        let notified = async {
            match input {
                Some(input) => input.offer_notify.notified().await,
                None => std::future::pending::<()>().await,
            }
        };
        let first_write = std::future::poll_fn(|cx| {
            let mut ordering = input.map(|input| {
                input
                    .ordering
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
            });
            if ordering.as_ref().is_some_and(|order| {
                (needs_barrier && order.pending_offers != 0)
                    || (needs_suspension_barrier && order.pending_suspensions != 0)
            }) {
                return Poll::Ready(Ok((ClipboardCommit::Deferred, 0)));
            }
            match clipboard::revalidate_wire_frame(frame, clip, stage) {
                Ok(false) => return Poll::Ready(Ok((ClipboardCommit::Discarded, 0))),
                Err(error) => {
                    tracing::warn!(%error, "discarding a clipboard reply that could not be encoded");
                    if frame.needs_suspension_barrier() {
                        clipboard::suspend_local_file_direction(clip);
                    }
                    return Poll::Ready(Ok((ClipboardCommit::Discarded, 0)));
                }
                Ok(true) => {}
            }
            match std::pin::Pin::new(&mut *writer).poll_write(cx, &frame.bytes) {
                Poll::Ready(Ok(0)) => Poll::Ready(Err(ironrdp_session::custom_err!(
                    "write clipboard frame",
                    std::io::Error::new(std::io::ErrorKind::WriteZero, "zero-length write")
                ))),
                Poll::Ready(Ok(written)) => {
                    if let Some(ordering) = ordering.as_mut() {
                        ordering.wire_progress = Some(std::time::Instant::now());
                    }
                    clipboard::note_wire_frame_started(frame, clip);
                    Poll::Ready(Ok((ClipboardCommit::Written, written)))
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(ironrdp_session::custom_err!(
                    "write clipboard frame",
                    error
                ))),
                Poll::Pending => Poll::Pending,
            }
        });
        let (result, written) = if needs_barrier || needs_suspension_barrier {
            tokio::select! {
                biased;
                _ = notified => {
                    if input.is_some_and(|input| {
                        (needs_barrier && input.has_pending_offer())
                            || (needs_suspension_barrier && input.has_pending_suspension())
                    }) {
                        return Ok(ClipboardCommit::Deferred);
                    }
                    continue;
                }
                result = first_write => result?,
            }
        } else {
            first_write.await?
        };
        if result == ClipboardCommit::Written {
            let mut offset = written;
            while offset < frame.bytes.len() {
                let count = tokio::io::AsyncWriteExt::write(writer, &frame.bytes[offset..])
                    .await
                    .map_err(|error| {
                        ironrdp_session::custom_err!("write clipboard frame", error)
                    })?;
                if count == 0 {
                    return Err(ironrdp_session::custom_err!(
                        "write clipboard frame",
                        std::io::Error::new(std::io::ErrorKind::WriteZero, "zero-length write")
                    ));
                }
                offset += count;
                if let Some(input) = input {
                    input
                        .ordering
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .wire_progress = Some(std::time::Instant::now());
                }
            }
            tokio::io::AsyncWriteExt::flush(writer)
                .await
                .map_err(|error| ironrdp_session::custom_err!("flush clipboard frame", error))?;
            if let Some(input) = input {
                input
                    .ordering
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .wire_progress = None;
            }
            clipboard::note_wire_frame_finished(frame, clip);
        }
        return Ok(result);
    }
}

async fn send_host(
    host: &HostSender,
    message: HostMsg,
) -> Result<(), ironrdp_session::SessionError> {
    host.send_when_ready(message)
        .await
        .map_err(|error| ironrdp_session::custom_err!("helper output", error))
}

#[cfg(test)]
mod process_error_tests {
    use super::*;

    #[test]
    fn bad_frame_logs_are_sampled_and_never_include_payload_or_error_text() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = Capture(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..512 {
                let error = ironrdp_session::custom_err!(
                    "synthetic-secret-error",
                    std::io::Error::other("synthetic-secret-source")
                );
                assert!(finish_active_process(
                    Err(error),
                    &egfx::new_shared(),
                    ironrdp_pdu::Action::X224,
                    b"synthetic-secret-payload"
                )
                .is_ok());
            }
        });
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(!output.contains("synthetic-secret"));
        assert!(output.contains("payload_len"));
        assert!(
            output.lines().count() <= 16,
            "bad-frame logs were not sampled"
        );
    }

    #[test]
    fn unrelated_stage_error_still_skips_one_pdu_and_next_output_survives() {
        let shared = egfx::new_shared();
        let skipped = finish_active_process(
            Err(ironrdp_session::custom_err!(
                "unrecognised GNOME PDU",
                std::io::Error::other("unsupported")
            )),
            &shared,
            ironrdp_pdu::Action::X224,
            &[],
        )
        .expect("unrelated PDU errors remain recoverable");
        assert!(skipped.is_empty());
        let outputs = finish_active_process(
            Ok(vec![ActiveStageOutput::ResponseFrame(vec![1, 2, 3])]),
            &shared,
            ironrdp_pdu::Action::X224,
            &[],
        )
        .expect("next PDU still processes normally");
        assert!(
            matches!(outputs.as_slice(), [ActiveStageOutput::ResponseFrame(bytes)] if bytes == &[1, 2, 3])
        );
    }
}

#[cfg(test)]
mod desktop_output_tests {
    use super::*;
    use ironrdp_graphics::pointer::DecodedPointer;
    #[test]
    fn actual_stage_pointer_outputs_keep_pixels_hotspot_visibility_and_position() {
        let shape = Arc::new(DecodedPointer {
            width: 2,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            bitmap_data: vec![250, 120, 60, 128, 0, 255, 0, 255],
        });
        let event = pointer_update(&ActiveStageOutput::PointerBitmap(Arc::clone(&shape))).unwrap();
        let mut pipe = Vec::new();
        adit_rdp_proto::write_msg(&mut pipe, &HostMsg::Pointer(event.clone())).unwrap();
        let got = adit_rdp_proto::read_msg::<_, HostMsg>(&mut std::io::Cursor::new(pipe))
            .unwrap()
            .unwrap();
        assert!(
            matches!(got, HostMsg::Pointer(PointerUpdate::Bitmap { width: 2, height: 1,
            hotspot_x: 1, hotspot_y: 0, rgba }) if rgba == shape.bitmap_data)
        );
        assert_eq!(
            pointer_update(&ActiveStageOutput::PointerHidden),
            Some(PointerUpdate::Hidden)
        );
        assert_eq!(
            pointer_update(&ActiveStageOutput::PointerDefault),
            Some(PointerUpdate::Default)
        );
        assert_eq!(
            pointer_update(&ActiveStageOutput::PointerPosition { x: 77, y: 31 }),
            Some(PointerUpdate::Position { x: 77, y: 31 })
        );
        assert_eq!(
            pointer_update(&ActiveStageOutput::PointerBitmap(shape)),
            Some(event)
        );
    }
    #[test]
    fn legacy_dirty_rects_copy_only_the_inclusive_changed_region() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 16, 12);
        let tiles = image_tiles(&image, vec![(15, 10, 3, 3)]);
        assert_eq!(tiles.len(), 1);
        let HostMsg::Tile {
            x,
            y,
            width,
            height,
            rgba,
        } = &tiles[0]
        else {
            panic!("tile")
        };
        assert_eq!((*x, *y, *width, *height), (15, 10, 1, 2));
        let expected: Vec<u8> = [10usize, 11]
            .into_iter()
            .flat_map(|row| {
                image.data()[(row * 16 + 15) * 4..(row * 16 + 16) * 4]
                    .iter()
                    .copied()
            })
            .collect();
        assert_eq!(rgba, &expected);
    }
}

#[cfg(test)]
mod local_file_fail_closed_tests {
    use super::*;
    use adit_rdp_proto::{write_msg, ClientMsg};
    use ironrdp_cliprdr::backend::CliprdrBackend;
    use ironrdp_cliprdr::pdu::{
        Capabilities, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
        ClipboardGeneralCapabilityFlags, ClipboardPdu, ClipboardProtocolVersion,
        FileContentsResponse, FileDescriptor, FormatDataRequest, FormatListResponse,
        OwnedFormatDataResponse,
    };
    use ironrdp_cliprdr::CliprdrClient;
    use ironrdp_core::{Encode as _, WriteCursor};
    use ironrdp_svc::{StaticChannelSet, SvcProcessor};
    use std::any::TypeId;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    const NAME: &str = "synthetic-secret-name-9261.bin";
    const SIZE: u64 = 0x7123_5678_9abc_def0;
    const CONTENT: &[u8] = b"synthetic-local-file-bytes-3974";

    #[derive(Default)]
    struct CaptureWriter(Vec<u8>);

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn local_file() -> ClipFile {
        ClipFile {
            name: NAME.into(),
            size: SIZE,
            is_dir: false,
        }
    }

    // Exercise the same framed reader and bounded input queue as stdin.
    fn enqueue_ipc(sender: &crate::host::InputSender, message: ClientMsg) {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &message).unwrap();
        let decoded = crate::host::read_client_frame(&mut Cursor::new(bytes), sender)
            .unwrap()
            .unwrap();
        let command = match decoded {
            crate::host::ClientFrame::FileHandled => return,
            crate::host::ClientFrame::Message(ClientMsg::Input(event)) => SessionCmd::Input(event),
            crate::host::ClientFrame::Message(ClientMsg::ClipboardImage(image)) => {
                SessionCmd::ClipboardImage(image)
            }
            crate::host::ClientFrame::Message(ClientMsg::RequestFileContents {
                generation,
                stream_id,
                index,
                offset,
                length,
            }) => SessionCmd::RequestFileContents {
                generation,
                stream_id,
                index,
                offset,
                length,
            },
            _ => panic!("unexpected synthetic IPC message"),
        };
        assert!(sender.send(command));
    }

    fn file_contents() -> ClientMsg {
        ClientMsg::BoundFileContents {
            request: ClipboardFileRequestId {
                channel_epoch: 1,
                snapshot_revision: 1,
                generation: 7,
                stream_id: 42,
            },
            data: Some(CONTENT.to_vec()),
        }
    }

    fn assert_no_local_file_data(bytes: &[u8]) {
        let utf16_name: Vec<u8> = NAME.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let descriptor_format: Vec<u8> = "FileGroupDescriptorW"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let size = SIZE.to_le_bytes();
        for secret in [
            NAME.as_bytes(),
            utf16_name.as_slice(),
            size.as_slice(),
            CONTENT,
            descriptor_format.as_slice(),
        ] {
            assert!(!bytes.windows(secret.len()).any(|window| window == secret));
        }
    }

    fn assert_state_does_not_cache_local_files(clipboard: &SharedClipboard) {
        let state = format!("{:?}", clipboard.lock().unwrap());
        assert!(!state.contains(NAME));
        assert!(!state.contains(&format!("size: {SIZE}")));
        assert!(!state.contains(&format!("{:?}", CONTENT)));
    }

    fn assert_format_request_failed(wire: &[u8]) {
        let error = ClipboardPdu::FormatDataResponse(OwnedFormatDataResponse::new_error());
        let bytes = ironrdp_core::encode_vec(&error).unwrap();
        assert!(wire
            .windows(bytes.len())
            .any(|window| window == bytes.as_slice()));
    }

    fn stage(clipboard: &SharedClipboard) -> ironrdp_session::ActiveStage {
        let mut client = CliprdrClient::new(Box::new(clipboard::AditCliprdrBackend::new(
            Arc::clone(clipboard),
        )));
        for pdu in [
            ClipboardPdu::Capabilities(Capabilities::new(
                ClipboardProtocolVersion::V2,
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                    | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
                    | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
            )),
            ClipboardPdu::FormatListResponse(FormatListResponse::Ok),
        ] {
            let mut bytes = vec![0; pdu.size()];
            pdu.encode(&mut WriteCursor::new(&mut bytes)).unwrap();
            client.process(&bytes).unwrap();
        }
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build()
    }

    async fn request_format_and_capture(
        clipboard: &SharedClipboard,
        host: &HostSender,
        format: ClipboardFormatId,
    ) -> Vec<u8> {
        let mut stage = stage(clipboard);
        let request = ClipboardPdu::FormatDataRequest(FormatDataRequest { format });
        let mut pdu = vec![0; request.size()];
        request.encode(&mut WriteCursor::new(&mut pdu)).unwrap();
        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![ironrdp_svc::SvcMessage::from(pdu)],
            1004,
            1001,
        )
        .unwrap();
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        let outputs = stage
            .process(&mut image, ironrdp_pdu::Action::X224, &packet)
            .unwrap();
        let mut writer = CaptureWriter::default();
        for output in outputs {
            if let ActiveStageOutput::ResponseFrame(bytes) = output {
                writer.0.extend(bytes);
            }
        }
        flush_clipboard_output(
            &mut writer,
            &mut VecDeque::new(),
            clipboard,
            &mut stage,
            host,
            None,
        )
        .await
        .unwrap();
        assert!(!writer.0.is_empty(), "the request receives a wire response");
        writer.0
    }

    fn host_messages(
        receiver: &std::sync::mpsc::Receiver<crate::host::QueuedHostMessage>,
    ) -> (Vec<HostMsg>, Vec<u8>) {
        let mut messages = Vec::new();
        let mut bytes = Vec::new();
        while let Ok(queued) = receiver.try_recv() {
            write_msg(&mut bytes, &queued.message).unwrap();
            messages.push(queued.message);
        }
        (messages, bytes)
    }

    #[tokio::test]
    async fn handshake_ipc_never_publishes_local_file_offers_or_contents() {
        let (host, receiver) = crate::host::host_channel();
        let clipboard = new_session_clipboard(&host);
        let (sender, mut input) = crate::host::input_channel();
        enqueue_ipc(
            &sender,
            ClientMsg::ClipboardFiles {
                generation: 7,
                files: vec![local_file()],
            },
        );
        enqueue_ipc(&sender, file_contents());
        enqueue_ipc(
            &sender,
            ClientMsg::ClipboardFiles {
                generation: 0,
                files: Vec::new(),
            },
        );
        drive_handshake(
            std::future::ready(Ok(())),
            &mut input,
            &clipboard,
            &mut None,
            &host,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_state_does_not_cache_local_files(&clipboard);
        let wire =
            request_format_and_capture(&clipboard, &host, ClipboardFormatId::new(0xc0fe)).await;
        assert_no_local_file_data(&wire);
        clipboard::leave_channel(&clipboard, &host).await.unwrap();
        handle_handshake_command(
            SessionCmd::ClipboardFiles {
                generation: 8,
                files: vec![local_file()],
            },
            &clipboard,
            &mut None,
            &host,
        )
        .await
        .unwrap();
        let redirected_wire =
            request_format_and_capture(&clipboard, &host, ClipboardFormatId::new(0xc0fe)).await;
        assert_no_local_file_data(&redirected_wire);
        assert_state_does_not_cache_local_files(&clipboard);
        let (messages, ipc) = host_messages(&receiver);
        assert_no_local_file_data(&ipc);
        assert!(messages.iter().any(|message| matches!(message,
            HostMsg::ClipboardFileDirectionUnavailable { message, .. }
                if message == LOCAL_FILES_DISABLED_NOTICE)));
        assert!(!messages.iter().any(|message| matches!(
            message,
            HostMsg::Closed
                | HostMsg::Error(_)
                | HostMsg::ClipboardLocalFileDirectionReady { .. }
                | HostMsg::FileContentsNeeded { .. }
        )));
        assert!(input.end.borrow().is_none());
    }

    #[tokio::test]
    async fn rejected_files_during_handshake_revoke_the_previous_text_or_image() {
        for (previous, format, bytes) in [
            (
                ClientMsg::Input(InputEvent::ClipboardText("prior secret".into())),
                ClipboardFormatId::CF_UNICODETEXT,
                "prior secret"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            ),
            (
                ClientMsg::ClipboardImage(vec![31, 41, 59, 26]),
                ClipboardFormatId::CF_DIB,
                vec![31, 41, 59, 26],
            ),
        ] {
            let (host, receiver) = crate::host::host_channel();
            let clipboard = new_session_clipboard(&host);
            let (sender, mut input) = crate::host::input_channel();
            enqueue_ipc(&sender, previous);
            enqueue_ipc(&sender, ClientMsg::LocalFileOfferRejected);
            drive_handshake(
                std::future::ready(Ok(())),
                &mut input,
                &clipboard,
                &mut None,
                &host,
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
            let wire = request_format_and_capture(&clipboard, &host, format).await;
            assert!(!wire
                .windows(bytes.len())
                .any(|window| window == bytes.as_slice()));
            assert_format_request_failed(&wire);
            assert_no_local_file_data(&wire);
            let (messages, _) = host_messages(&receiver);
            assert!(!messages
                .iter()
                .any(|message| matches!(message, HostMsg::Closed | HostMsg::Error(_))));
            assert!(input.end.borrow().is_none());
        }
    }

    #[tokio::test]
    async fn active_ipc_keeps_desktop_and_other_clipboard_formats_available() {
        let (host, receiver) = crate::host::host_channel();
        let clipboard = new_session_clipboard(&host);
        let (sender, mut input) = crate::host::input_channel();
        for message in [
            ClientMsg::ClipboardFiles {
                generation: 0,
                files: vec![local_file()],
            },
            ClientMsg::ClipboardFiles {
                generation: 7,
                files: vec![local_file()],
            },
            file_contents(),
            ClientMsg::Input(InputEvent::ClipboardText("synthetic text".into())),
        ] {
            enqueue_ipc(&sender, message);
        }
        while let Ok(queued) = input.commands.try_recv() {
            handle_active_clipboard_command(queued.command, &clipboard);
        }
        assert!(format!("{:?}", clipboard.lock().unwrap()).contains("synthetic text"));
        let text_wire =
            request_format_and_capture(&clipboard, &host, ClipboardFormatId::CF_UNICODETEXT).await;
        let text: Vec<u8> = "synthetic text"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(text_wire
            .windows(text.len())
            .any(|window| window == text.as_slice()));
        assert_no_local_file_data(&text_wire);
        enqueue_ipc(&sender, ClientMsg::ClipboardImage(vec![11, 22, 33, 44]));
        enqueue_ipc(
            &sender,
            ClientMsg::ClipboardFiles {
                generation: 0,
                files: Vec::new(),
            },
        );
        while let Ok(queued) = input.commands.try_recv() {
            handle_active_clipboard_command(queued.command, &clipboard);
        }
        assert!(format!("{:?}", clipboard.lock().unwrap()).contains("[11, 22, 33, 44]"));
        let image_wire =
            request_format_and_capture(&clipboard, &host, ClipboardFormatId::CF_DIB).await;
        assert!(image_wire
            .windows(4)
            .any(|window| window == [11, 22, 33, 44]));
        assert_no_local_file_data(&image_wire);
        assert_state_does_not_cache_local_files(&clipboard);
        let wire =
            request_format_and_capture(&clipboard, &host, ClipboardFormatId::new(0xc0fe)).await;
        assert_no_local_file_data(&wire);
        // An ordinary desktop update remains deliverable after the rejected IPC.
        host.send(HostMsg::Tile {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        })
        .unwrap();
        let (messages, ipc) = host_messages(&receiver);
        assert_no_local_file_data(&ipc);
        assert!(messages
            .iter()
            .any(|message| matches!(message, HostMsg::Tile { .. })));
        assert!(!messages.iter().any(|message| matches!(
            message,
            HostMsg::Closed
                | HostMsg::Error(_)
                | HostMsg::FileContentsNeeded { .. }
                | HostMsg::ClipboardLocalFileDirectionReady { .. }
        )));
        assert!(input.end.borrow().is_none());
    }

    #[tokio::test]
    async fn rejected_runtime_files_replace_old_text_and_images_with_errors() {
        for (previous, format, bytes) in [
            (
                ClientMsg::Input(InputEvent::ClipboardText("runtime secret".into())),
                ClipboardFormatId::CF_UNICODETEXT,
                "runtime secret"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            ),
            (
                ClientMsg::ClipboardImage(vec![37, 61, 83, 91]),
                ClipboardFormatId::CF_DIB,
                vec![37, 61, 83, 91],
            ),
        ] {
            let (host, receiver) = crate::host::host_channel();
            let clipboard = new_session_clipboard(&host);
            let (sender, mut input) = crate::host::input_channel();
            enqueue_ipc(&sender, previous);
            handle_active_clipboard_command(input.commands.try_recv().unwrap().command, &clipboard);
            let before = request_format_and_capture(&clipboard, &host, format).await;
            assert!(before
                .windows(bytes.len())
                .any(|window| window == bytes.as_slice()));
            enqueue_ipc(&sender, ClientMsg::LocalFileOfferRejected);
            handle_active_clipboard_command(input.commands.try_recv().unwrap().command, &clipboard);
            let after = request_format_and_capture(&clipboard, &host, format).await;
            assert!(!after
                .windows(bytes.len())
                .any(|window| window == bytes.as_slice()));
            assert_format_request_failed(&after);
            assert_no_local_file_data(&after);
            enqueue_ipc(
                &sender,
                ClientMsg::Input(InputEvent::ClipboardText("new text".into())),
            );
            handle_active_clipboard_command(input.commands.try_recv().unwrap().command, &clipboard);
            let new_text =
                request_format_and_capture(&clipboard, &host, ClipboardFormatId::CF_UNICODETEXT)
                    .await;
            let expected: Vec<u8> = "new text"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            assert!(new_text
                .windows(expected.len())
                .any(|window| window == expected.as_slice()));
            host.send(HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 255],
            })
            .unwrap();
            let (messages, _) = host_messages(&receiver);
            assert!(messages
                .iter()
                .any(|message| matches!(message, HostMsg::Tile { .. })));
            assert!(!messages
                .iter()
                .any(|message| matches!(message, HostMsg::Closed | HostMsg::Error(_))));
            assert!(input.end.borrow().is_none());
        }
    }

    #[test]
    fn empty_file_offer_remains_a_real_revocation() {
        // Exercise the empty path even on a pre-existing, unsealed snapshot.
        let clipboard = clipboard::new_shared();
        clipboard::offer_local_files(&clipboard, 7, vec![local_file()]);
        assert!(format!("{:?}", clipboard.lock().unwrap()).contains(NAME));
        reject_local_file_offer(&clipboard, 0, Vec::new());
        assert_state_does_not_cache_local_files(&clipboard);
    }

    #[tokio::test]
    async fn suspended_local_direction_keeps_remote_file_reads_working() {
        let (host, receiver) = crate::host::host_channel();
        let clipboard = new_session_clipboard(&host);
        let mut backend = clipboard::AditCliprdrBackend::new(Arc::clone(&clipboard));
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::new(0xc123))
            .with_name(ClipboardFormatName::FILE_LIST)]);
        let mut descriptor = FileDescriptor::new("remote-fixture.bin");
        descriptor.file_size = Some(3);
        backend.on_remote_file_list(&[descriptor], Some(10));
        clipboard::flush_app_actions(&clipboard, &host)
            .await
            .unwrap();
        let (messages, _) = host_messages(&receiver);
        let generation = messages
            .iter()
            .find_map(|message| match message {
                HostMsg::ClipboardFilesForPolicy {
                    generation, files, ..
                } if !files.is_empty() => Some(*generation),
                _ => None,
            })
            .expect("remote file offer remains available");
        let (sender, mut input) = crate::host::input_channel();
        enqueue_ipc(&sender, ClientMsg::LocalFileOfferRejected);
        handle_active_clipboard_command(input.commands.try_recv().unwrap().command, &clipboard);
        enqueue_ipc(
            &sender,
            ClientMsg::RequestFileContents {
                generation,
                stream_id: 71,
                index: 0,
                offset: 0,
                length: 3,
            },
        );
        handle_active_clipboard_command(input.commands.try_recv().unwrap().command, &clipboard);
        // This is the first synthetic remote read, whose channel wire id is 1.
        backend
            .on_file_contents_response(FileContentsResponse::new_data_response(1, vec![4, 5, 6]));
        clipboard::flush_app_actions(&clipboard, &host)
            .await
            .unwrap();
        let (messages, _) = host_messages(&receiver);
        assert!(messages.iter().any(|message| matches!(message,
            HostMsg::FileContents { generation: got, stream_id: 71, data: Some(data) }
                if *got == generation && data == &[4, 5, 6])));
        assert!(input.end.borrow().is_none());
    }
}
