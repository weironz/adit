//! The RDP connection + active-session loop, adapted from the reference
//! `ironrdp-client`: direct TCP → TLS → CredSSP, then a `tokio::select!` loop
//! pumping server PDUs into framebuffer tiles and app input to the server.
//!
//! The loop is transport-agnostic: it receives [`InputEvent`]s from a Tokio
//! channel and emits [`HostMsg`]s to a std channel. The [`crate::host`] layer
//! bridges those channels to the child process's stdin/stdout.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::display::{self, SharedDisplay};
use adit_rdp_proto::{ClipFile, ConnectRequest, HostMsg, InputEvent, PointerUpdate};
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
use ironrdp_tokio::{split_tokio_framed, TokioFramed};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc as tokio_mpsc;

use crate::clipboard::{self, SharedClipboard};
use crate::egfx::{self, EgfxHandler, SharedEgfx};
use crate::{build_connector_config, input::map_input, RdpError};

/// A command from the app for the session loop.
///
/// Input used to be the only thing the app sent, so the channel carried
/// [`InputEvent`] directly. Clipboard file transfer added three messages that
/// are not input at all — they neither map to scancodes nor belong in the input
/// database — so the channel carries this instead and the loop dispatches.
#[derive(Debug)]
pub(crate) enum SessionCmd {
    Discarded,
    RejectedKeyboard,
    Input {
        epoch: u64,
        event: InputEvent,
    },
    Pointer {
        event: InputEvent,
        epoch: u64,
        revision: u64,
    },
    Resize {
        epoch: u64,
        revision: u64,
        width: u16,
        height: u16,
    },
    /// Offer a locally-copied image to the remote clipboard (raw CF_DIB).
    ClipboardImage {
        epoch: u64,
        image: Vec<u8>,
    },
    /// Offer a local file selection to the remote clipboard (metadata only).
    ClipboardFiles {
        epoch: u64,
        files: Vec<ClipFile>,
    },
    /// The app read the bytes the remote asked for, or failed to.
    FileContents {
        epoch: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
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
    fn belongs_to(&self, current: u64) -> bool {
        match self {
            Self::Input { epoch, .. }
            | Self::Pointer { epoch, .. }
            | Self::Resize { epoch, .. }
            | Self::ClipboardImage { epoch, .. }
            | Self::ClipboardFiles { epoch, .. }
            | Self::FileContents { epoch, .. } => *epoch == current,
            _ => true,
        }
    }
}

#[derive(Default)]
pub(crate) struct InputRouting {
    state: Mutex<InputRoutingState>,
}

#[derive(Default)]
struct InputRoutingState {
    epoch: u64,
    display_revision: u64,
    resize_revision: u64,
}

impl InputRouting {
    pub(crate) fn accepts_epoch(&self, epoch: u64) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).epoch == epoch
    }

    pub(crate) fn file_contents(
        &self,
        epoch: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    ) -> Option<SessionCmd> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.epoch == epoch).then_some(SessionCmd::FileContents {
            epoch,
            stream_id,
            data,
        })
    }

    /// Classify under the same lock as redirect so an old message can neither
    /// enter the new epoch nor invalidate its pointer revision.
    pub(crate) fn route(&self, epoch: u64, event: InputEvent) -> Option<(bool, SessionCmd)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if epoch != state.epoch {
            return None;
        }
        if is_display_input(&event) {
            state.display_revision = state.display_revision.checked_add(1)?;
            if let InputEvent::Resize { width, height } = event {
                state.resize_revision = state.resize_revision.checked_add(1)?;
                return Some((
                    true,
                    SessionCmd::Resize {
                        epoch,
                        revision: state.resize_revision,
                        width,
                        height,
                    },
                ));
            }
            Some((true, SessionCmd::Input { epoch, event }))
        } else if is_pointer_input(&event) {
            Some((
                false,
                SessionCmd::Pointer {
                    epoch,
                    revision: state.display_revision,
                    event,
                },
            ))
        } else {
            Some((false, SessionCmd::Input { epoch, event }))
        }
    }

    pub(crate) fn advance(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = state.epoch.checked_add(1)?;
        let revision = state.display_revision.checked_add(1)?;
        state.epoch = epoch;
        state.display_revision = revision;
        state.resize_revision = 0;
        Some(epoch)
    }

    fn display_revision(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .display_revision
    }
}

pub(crate) fn is_display_input(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::MonitorLayout { .. }
            | InputEvent::CancelMonitorLayout { .. }
            | InputEvent::Resize { .. }
    )
}

pub(crate) fn is_pointer_input(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::MouseMove { .. } | InputEvent::MouseButton { .. } | InputEvent::Wheel { .. }
    )
}

fn current_pointer_event(event: &InputEvent, epoch: u64, current: u64) -> bool {
    epoch == current || matches!(event, InputEvent::MouseButton { pressed: false, .. })
}

fn release_pointer_buttons(
    db: &mut Database,
) -> Vec<ironrdp_pdu::input::fast_path::FastPathInputEvent> {
    use adit_rdp_proto::MouseButton;
    let mut events = Vec::new();
    for button in [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::X1,
        MouseButton::X2,
    ] {
        events.extend(map_input(
            db,
            &InputEvent::MouseButton {
                button,
                pressed: false,
            },
        ));
    }
    events
}

// Display commands retain their order but must not wait for unrelated input
// (for example a stream of mouse motion) before they can supersede a layout.
async fn next_command(
    input: &mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    display: &mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    epoch: u64,
) -> Option<SessionCmd> {
    let command = tokio::select! {
        biased;
        command = display.recv() => command,
        command = input.recv() => command,
    };
    command.map(|command| classify_command(command, epoch))
}

fn classify_command(command: SessionCmd, epoch: u64) -> SessionCmd {
    if command.belongs_to(epoch) {
        command
    } else if matches!(
        command,
        SessionCmd::Input {
            event: InputEvent::Key { .. } | InputEvent::Unicode { .. },
            ..
        }
    ) {
        SessionCmd::RejectedKeyboard
    } else {
        SessionCmd::Discarded
    }
}

async fn next_active_command(
    input: &mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    display: &mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    deferred: &mut VecDeque<SessionCmd>,
    epoch: u64,
) -> Option<SessionCmd> {
    if deferred.front().is_some_and(|command| {
        matches!(
            command,
            SessionCmd::Resize { .. }
                | SessionCmd::Input {
                    event: InputEvent::MonitorLayout { .. }
                        | InputEvent::CancelMonitorLayout { .. },
                    ..
                }
        )
    }) {
        return deferred
            .pop_front()
            .map(|command| classify_command(command, epoch));
    }
    if !deferred.is_empty() {
        let command = display.try_recv().ok().or_else(|| deferred.pop_front());
        return command.map(|command| classify_command(command, epoch));
    }
    next_command(input, display, epoch).await
}

struct SessionInput<'a> {
    ordinary: &'a mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    display: &'a mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    routing: &'a InputRouting,
    epoch: u64,
    deferred: &'a mut VecDeque<SessionCmd>,
    idle_generation: &'a AtomicU64,
    network_progress: &'a AtomicU64,
}

type PendingResize = (u32, u32, u64);
type DisplayRequest<'a> = (
    &'a SharedDisplay,
    &'a mut Option<display::PendingLayout>,
    &'a mut Option<PendingResize>,
);

struct ActivationControls<'a> {
    input: &'a mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    display: &'a mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
    deferred: &'a mut VecDeque<SessionCmd>,
    pending_layout: &'a mut Option<display::PendingLayout>,
    pending_resize: &'a mut Option<PendingResize>,
    host: &'a std_mpsc::Sender<HostMsg>,
    epoch: u64,
}

// Reactivation reads the same connection as the main loop. Keep processing
// cancellation and Close while that read is stalled, without losing commands.
async fn wait_activation_read<T>(
    read: impl std::future::Future<Output = T>,
    controls: &mut ActivationControls<'_>,
) -> Option<T> {
    tokio::pin!(read);
    loop {
        let deadline = controls
            .pending_layout
            .as_ref()
            .and_then(|request| request.deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + display::CAPABILITIES_TIMEOUT);
        tokio::select! {
            result = &mut read => return Some(result),
            _ = tokio::time::sleep_until(deadline), if controls.pending_layout.is_some() => {
                if let Some(request) = controls.pending_layout.take() {
                    if controls.host.send(HostMsg::MonitorLayoutResult {
                        epoch: controls.epoch,
                        request_id: request.request_id,
                        result: Err("DisplayControl capabilities timed out".into()),
                    }).is_err() { return None; }
                }
            }
            command = next_command(controls.input, controls.display, controls.epoch) => {
                match command {
                    None => return None,
                    Some(SessionCmd::Input { event: InputEvent::MonitorLayout { request_id, monitors }, .. }) => {
                        stage_layout(controls.pending_layout, controls.pending_resize, request_id, monitors, Some(tokio::time::Instant::now()));
                    }
                    Some(SessionCmd::Input { event: InputEvent::CancelMonitorLayout { request_id }, .. }) => {
                        display::cancel_pending(controls.pending_layout, request_id);
                    }
                    Some(SessionCmd::Resize { width, height, revision, .. }) => {
                        let (width, height) = MonitorLayoutEntry::adjust_display_size(u32::from(width), u32::from(height));
                        *controls.pending_resize = Some((width, height, revision));
                    }
                    Some(SessionCmd::RejectedKeyboard) => {
                        if controls.host.send(HostMsg::InputDiscarded).is_err() { return None; }
                    }
                    Some(command) => {
                        if !defer_connect_command(controls.deferred, command)
                            && controls.host.send(HostMsg::InputDiscarded).is_err() { return None; }
                    }
                }
            }
        }
    }
}

fn stage_layout(
    pending: &mut Option<display::PendingLayout>,
    resize: &mut Option<PendingResize>,
    request_id: u64,
    monitors: Vec<adit_rdp_proto::Monitor>,
    now: Option<tokio::time::Instant>,
) {
    let mut request = display::PendingLayout::new(request_id, monitors);
    if let Some(now) = now {
        request.start_waiting(now);
    }
    *pending = Some(request);
    *resize = None;
}

fn reset_display_requests(
    pending: &mut Option<display::PendingLayout>,
    resize: &mut Option<PendingResize>,
    display: &mut tokio_mpsc::UnboundedReceiver<SessionCmd>,
) {
    // Late messages that were not yet in this queue are rejected by their
    // explicit epoch both at stdin admission and at session consumption.
    *pending = None;
    *resize = None;
    while display.try_recv().is_ok() {}
}

/// A type-erased upgraded (post-TLS) framed transport.
trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}
type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

/// Abort a TCP/TLS/CredSSP handshake that stalls this long.
const CONNECT_TIMEOUT_SECS: u64 = 30;
const MAX_DEFERRED_COMMANDS: usize = 4096;
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// Wrap the socket *below TLS*: a TLS flush may perform many partial network
// writes while its own future stays pending. Only actual socket progress can
// distinguish that from a stalled peer.
struct IdleWrite<T> {
    inner: T,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    generation: Arc<AtomicU64>,
    progress: Arc<AtomicU64>,
    observed_generation: u64,
}

impl<T> IdleWrite<T> {
    fn new(inner: T) -> Self {
        Self::with_generation(inner, Arc::new(AtomicU64::new(0)))
    }

    fn with_generation(inner: T, generation: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            deadline: None,
            observed_generation: generation.load(Ordering::Relaxed),
            generation,
            progress: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for IdleWrite<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for IdleWrite<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let generation = self.generation.load(Ordering::Relaxed);
        if generation != self.observed_generation {
            self.deadline = None;
            self.observed_generation = generation;
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(result) => {
                self.deadline = None;
                if let Ok(bytes) = result.as_ref() {
                    self.progress.fetch_add(*bytes as u64, Ordering::Relaxed);
                }
                Poll::Ready(result)
            }
            Poll::Pending => {
                let deadline = self
                    .deadline
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(WRITE_TIMEOUT)));
                if std::future::Future::poll(deadline.as_mut(), cx).is_ready() {
                    self.deadline = None;
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "RDP write stalled",
                    )))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// A failed write may have transmitted a prefix, so the caller must close the
// connection instead of sending another PDU on a corrupted stream.
#[cfg(test)]
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut TokioFramed<W>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let (stream, _) = writer.get_inner_mut();
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]).await {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => written += n,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "RDP frame write {}: {error}",
                        if written == 0 {
                            "before reported progress"
                        } else {
                            "after partial progress"
                        }
                    ),
                ))
            }
        }
    }
    stream.flush().await
}

enum DisplayWrite {
    Sent,
    Superseded(SessionCmd),
    UnstartedTimeout,
}

/// Only a write that has not accepted any plaintext can be cancelled safely.
/// Once TLS accepted a prefix we must finish it, or fail the connection.
async fn write_display_frame<W: AsyncWrite + Unpin>(
    writer: &mut TokioFramed<W>,
    bytes: &[u8],
    mut display_rx: Option<&mut tokio_mpsc::UnboundedReceiver<SessionCmd>>,
    idle_generation: &AtomicU64,
    network_progress: &AtomicU64,
    epoch: u64,
    layout_request: Option<u64>,
) -> std::io::Result<DisplayWrite> {
    let (stream, _) = writer.get_inner_mut();
    let start_progress = network_progress.load(Ordering::Relaxed);
    let deadline = tokio::time::sleep(WRITE_TIMEOUT);
    tokio::pin!(deadline);
    let mut written = 0;
    loop {
        if written == bytes.len() {
            match stream.flush().await {
                Ok(()) => return Ok(DisplayWrite::Sent),
                Err(error)
                    if error.kind() == std::io::ErrorKind::TimedOut
                        && network_progress.load(Ordering::Relaxed) == start_progress =>
                {
                    // TLS has buffered the PDU, so it cannot be cancelled; an
                    // idle socket can retry the flush without corrupting it.
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        if written == 0 {
            tokio::select! {
                biased;
                command = receive_optional_display(&mut display_rx) => {
                    if let Some(command) = command {
                        let command = classify_command(command, epoch);
                        if matches!(command, SessionCmd::Discarded) || matches!(&command,
                            SessionCmd::Input { event: InputEvent::CancelMonitorLayout { request_id }, .. }
                                if Some(*request_id) != layout_request) {
                            continue;
                        }
                        idle_generation.fetch_add(1, Ordering::Relaxed);
                        return Ok(DisplayWrite::Superseded(command));
                    }
                }
                _ = &mut deadline => {
                    idle_generation.fetch_add(1, Ordering::Relaxed);
                    return Ok(DisplayWrite::UnstartedTimeout);
                }
                result = stream.write(bytes) => {
                    match result? {
                        0 => return Err(std::io::ErrorKind::WriteZero.into()),
                        n => written = n,
                    }
                }
            }
        } else {
            match stream.write(&bytes[written..]).await? {
                0 => return Err(std::io::ErrorKind::WriteZero.into()),
                n => written += n,
            }
        }
    }
}

async fn receive_optional_display(
    display_rx: &mut Option<&mut tokio_mpsc::UnboundedReceiver<SessionCmd>>,
) -> Option<SessionCmd> {
    match display_rx.as_deref_mut() {
        Some(rx) if !rx.is_closed() || !rx.is_empty() => rx.recv().await,
        _ => std::future::pending().await,
    }
}

/// Protocol responses and pointer releases cannot be dropped. If the socket
/// has not accepted this PDU, keep retrying; once a prefix is on the wire, a
/// stalled write remains a fatal stream error.
async fn write_required_frame<W: AsyncWrite + Unpin>(
    writer: &mut TokioFramed<W>,
    bytes: &[u8],
    idle_generation: &AtomicU64,
    network_progress: &AtomicU64,
) -> std::io::Result<()> {
    loop {
        match write_display_frame(
            writer,
            bytes,
            None,
            idle_generation,
            network_progress,
            0,
            None,
        )
        .await?
        {
            DisplayWrite::Sent => return Ok(()),
            DisplayWrite::UnstartedTimeout => continue,
            DisplayWrite::Superseded(_) => {
                unreachable!("required frame has no cancellation receiver")
            }
        }
    }
}

async fn while_control_open<T>(
    operation: impl std::future::Future<Output = std::io::Result<T>>,
    input_rx: &tokio_mpsc::UnboundedReceiver<SessionCmd>,
) -> std::io::Result<T> {
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => return result,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                if input_rx.is_closed() {
                    return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "RDP control channel closed"));
                }
            }
        }
    }
}

fn defer_connect_command(deferred: &mut VecDeque<SessionCmd>, command: SessionCmd) -> bool {
    if !matches!(&command, SessionCmd::Pointer { .. } | SessionCmd::Discarded) {
        if deferred.len() == MAX_DEFERRED_COMMANDS {
            // None of this batch reached the new server. Discard it together
            // so a key press cannot replay without its release, and report it.
            deferred.clear();
            return false;
        }
        deferred.push_back(command);
    }
    true
}

fn announce_desktop(
    host_tx: &std_mpsc::Sender<HostMsg>,
    announced: &mut bool,
    width: u16,
    height: u16,
) -> Result<(), RdpError> {
    let message = if *announced {
        HostMsg::Resized { width, height }
    } else {
        *announced = true;
        HostMsg::Connected { width, height }
    };
    host_tx
        .send(message)
        .map_err(|_| RdpError::ControlChannelClosed)
}

fn server_monitor_layout(
    payload: &[u8],
    io_channel_id: u16,
) -> Option<Vec<adit_rdp_proto::MonitorGeometry>> {
    let ctx = ironrdp_pdu::mcs::decode_send_data_indication(payload).ok()?;
    if ctx.channel_id != io_channel_id {
        return None;
    }
    let data = ironrdp_pdu::rdp::headers::decode_share_data(ctx).ok()?;
    match data.pdu {
        ironrdp_pdu::rdp::headers::ShareDataPdu::MonitorLayout(layout) => {
            Some(egfx::monitor_geometries(&layout.monitors))
        }
        _ => None,
    }
}

/// Cap on chained server redirections (guards against a redirect loop).
const MAX_REDIRECTS: u32 = 5;

/// Drive an RDP session to completion, following any server redirections (GNOME
/// system-mode handover): connect, announce, run the active loop; on a redirection
/// PDU, reconnect to the target carrying the routing token, and repeat.
pub(crate) async fn run_session(
    mut request: ConnectRequest,
    mut input_rx: tokio_mpsc::UnboundedReceiver<SessionCmd>,
    mut display_rx: tokio_mpsc::UnboundedReceiver<SessionCmd>,
    routing: Arc<InputRouting>,
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
    let shared_display = SharedDisplay::default();
    let mut pending_layout = None;
    let mut pending_resize = None;
    // Clipboard state outlives a single CLIPRDR channel on purpose: a GNOME
    // system-mode handover redirects and rebuilds the channel, and the text the
    // user copied did not change under them.
    let shared_clipboard = clipboard::new_shared();
    let mut routing_token: Option<Vec<u8>> = None;
    // One-time credentials for the RDSTLS handover auth, populated on a redirect.
    let mut rdstls_creds: Option<crate::rdstls::RdstlsCreds> = None;
    let mut announced = false;
    let mut redirects = 0u32;
    let mut epoch = 0u64;
    let mut deferred_commands = VecDeque::new();

    loop {
        // A redirected connection owns a fresh graphics mailbox. Reusing the
        // old one lets unconsumed tiles cross the connection boundary.
        let shared_egfx = egfx::new_shared();
        // Connect with a timeout; a Close (input channel drop) cancels a hung
        // handshake instead of blocking forever. Scoped in its own block so the
        // futures borrowing `request` / `routing_token` are dropped before we may
        // reassign those on a redirect.
        let (connection_result, framed, idle_generation, network_progress) = {
            let connect_fut = connect(
                &request,
                &shared_egfx,
                &shared_clipboard,
                &shared_display,
                routing_token.as_deref(),
                rdstls_creds.as_ref(),
            );
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
                    msg = next_command(&mut input_rx, &mut display_rx, epoch) => {
                        match msg {
                            None => return Err(RdpError::ControlChannelClosed),
                            Some(SessionCmd::Input { event: InputEvent::MonitorLayout { request_id, monitors }, .. }) => {
                                stage_layout(&mut pending_layout, &mut pending_resize, request_id, monitors, None);
                            }
                            Some(SessionCmd::Input { event: InputEvent::CancelMonitorLayout { request_id }, .. }) => {
                                display::cancel_pending(&mut pending_layout, request_id);
                            }
                            Some(SessionCmd::Resize { width, height, revision, .. }) => {
                                let (width, height) = MonitorLayoutEntry::adjust_display_size(
                                    u32::from(width), u32::from(height));
                                pending_resize = Some((width, height, revision));
                            }
                            Some(SessionCmd::RejectedKeyboard) => {
                                host_tx.send(HostMsg::InputDiscarded).map_err(|_| RdpError::ControlChannelClosed)?;
                            }
                            Some(command) => {
                                if !defer_connect_command(&mut deferred_commands, command) {
                                    host_tx.send(HostMsg::InputDiscarded).map_err(|_| RdpError::ControlChannelClosed)?;
                                }
                            },
                        }
                    }
                }
            }
        };

        // The new channel can start with a different negotiated size without
        // ever emitting a graphics reset or a resize PDU.
        let desktop = connection_result.desktop_size;
        announce_desktop(&host_tx, &mut announced, desktop.width, desktop.height)?;

        match active_session(
            framed,
            connection_result,
            SessionInput {
                ordinary: &mut input_rx,
                display: &mut display_rx,
                routing: &routing,
                epoch,
                deferred: &mut deferred_commands,
                idle_generation: &idle_generation,
                network_progress: &network_progress,
            },
            &host_tx,
            &shared_egfx,
            &shared_clipboard,
            (&shared_display, &mut pending_layout, &mut pending_resize),
        )
        .await
        .map_err(|e| RdpError::Session(e.to_string()))?
        {
            None => return Ok(()),
            Some(redir) => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(RdpError::Session("too many server redirections".into()));
                }
                // The GUI owns request IDs and decides whether the previous
                // preference still needs replay after the channel is rebuilt.
                // Commands queued for the old connection are superseded by the
                // app's fresh request after DisplayControlReset.
                epoch = routing
                    .advance()
                    .ok_or_else(|| RdpError::Session("input epoch exhausted".into()))?;
                let discarded_commands = !deferred_commands.is_empty();
                deferred_commands.clear();
                reset_display_requests(&mut pending_layout, &mut pending_resize, &mut display_rx);
                if host_tx
                    .send(HostMsg::DisplayControlReset { epoch })
                    .is_err()
                {
                    return Err(RdpError::ControlChannelClosed);
                }
                if discarded_commands && host_tx.send(HostMsg::InputDiscarded).is_err() {
                    return Err(RdpError::ControlChannelClosed);
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
    shared_clipboard: &SharedClipboard,
    shared_display: &SharedDisplay,
    routing_token: Option<&[u8]>,
    rdstls_creds: Option<&crate::rdstls::RdstlsCreds>,
) -> Result<
    (
        ConnectionResult,
        UpgradedFramed,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ),
    ironrdp_connector::ConnectorError,
> {
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
    let timed_stream = IdleWrite::new(initial_stream);
    let idle_generation = Arc::clone(&timed_stream.generation);
    let network_progress = Arc::clone(&timed_stream.progress);
    let (mut tls_stream, tls_cert) = ironrdp_tls::upgrade(timed_stream, &request.host)
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

    Ok((
        connection_result,
        upgraded_framed,
        idle_generation,
        network_progress,
    ))
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

/// The head of a PDU as hex, for a log line about one we could not decode.
///
/// Deliberately a header, not a body. 32 bytes covers the TPKT/X.224/MCS framing,
/// the Share Control header (totalLength, pduType, pduSource) and the start of a
/// Share Data header — which together say what the PDU *was*. That is the
/// question every wrong fix for the black-desktop symptom has answered by
/// guessing. It cannot leak a credential: the bytes stop long before any payload,
/// and the one PDU that carries a secret (the RDSTLS auth request) is one we
/// send, never one we fail to decode.
fn pdu_prefix(pdu: Option<&[u8]>) -> String {
    let Some(bytes) = pdu else {
        return String::from("<none>");
    };
    let head: String = bytes
        .iter()
        .take(32)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("len={} head={head}", bytes.len())
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
        Err(error) => {
            tracing::warn!(
                ?action,
                payload_len = payload.len(),
                pdu = %pdu_prefix(Some(payload)),
                "skipping PDU the active stage could not process: {error}"
            );
            Ok(Vec::new())
        }
    }
}

/// The active-session pump. Returns when the server terminates the session or the
/// app drops its input channel.
async fn active_session(
    framed: UpgradedFramed,
    connection_result: ConnectionResult,
    input: SessionInput<'_>,
    host_tx: &std_mpsc::Sender<HostMsg>,
    egfx: &SharedEgfx,
    clip: &SharedClipboard,
    display_request: DisplayRequest<'_>,
) -> Result<Option<crate::redirect::Redirection>, ironrdp_session::SessionError> {
    let SessionInput {
        ordinary: input_rx,
        display: display_rx,
        routing,
        epoch,
        deferred,
        idle_generation,
        network_progress,
    } = input;
    let (display, pending_layout, pending_resize) = display_request;
    let (mut reader, mut writer) = split_tokio_framed(framed);

    let desktop_size = connection_result.desktop_size;
    // Size the app last knows about; an EGFX reset to a different size sends a
    // Resized before the next tile.
    let mut egfx_size = (desktop_size.width, desktop_size.height);
    // A resize requested before the DisplayControl channel is up would be
    // silently dropped; park it here and retry once the channel connects.
    let mut last_display_state = None;
    let mut display_channel = display::ChannelWatch::default();
    if let Some(request) = pending_layout.as_mut() {
        request.start_waiting(tokio::time::Instant::now());
    }
    // A request received during connection must be considered even if the
    // newly established desktop is otherwise completely idle.
    let mut pump_initial_layout = pending_layout.is_some() || pending_resize.is_some();
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
    let mut pointer_epoch = None;
    // Once the app's input channel closes we request a graceful shutdown ONCE and
    // then stop polling it: `recv()` returns `None` immediately on every subsequent
    // call, which would otherwise busy-loop the whole session and re-send shutdown
    // PDUs until the server happens to close the TCP connection.
    let mut input_open = true;
    // Latches once the EGFX pipeline paints; from then on the legacy `image` is
    // not the display surface and must never be emitted (it would flash black).
    let mut egfx_active = false;

    'session: loop {
        let layout_deadline = pending_layout
            .as_ref()
            .and_then(|request| request.deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + display::CAPABILITIES_TIMEOUT);
        let outputs = tokio::select! {
            _ = std::future::ready(()), if pump_initial_layout => {
                pump_initial_layout = false;
                Vec::new()
            }
            _ = tokio::time::sleep_until(layout_deadline), if pending_layout.is_some() => {
                Vec::new()
            }
            frame_read = reader.read_pdu() => {
                let (action, payload) = frame_read
                    .map_err(|e| ironrdp_session::custom_err!("read PDU", e))?;
                // Intercept a Server Redirection PDU (GNOME system-mode handover)
                // before the active stage, which can't decode it.
                if matches!(action, ironrdp_pdu::Action::X224) {
                    if let Some(redirection) = crate::redirect::detect(&payload, io_channel_id) {
                        return Ok(Some(redirection));
                    }
                    if let Some(monitors) = server_monitor_layout(&payload, io_channel_id) {
                        if host_tx.send(HostMsg::MonitorLayoutObserved { epoch, monitors }).is_err() { break 'session; }
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
            input = next_active_command(input_rx, display_rx, deferred, epoch), if input_open => {
                match input {
                    Some(SessionCmd::Discarded) => Vec::new(),
                    Some(SessionCmd::RejectedKeyboard) => {
                        if host_tx.send(HostMsg::InputDiscarded).is_err() { break 'session; }
                        Vec::new()
                    }
                    // App dropped the sender: shut down gracefully, exactly once.
                    None => {
                        input_open = false;
                        active_stage.graceful_shutdown()?
                    }
                    Some(SessionCmd::Input { event: InputEvent::MonitorLayout { request_id, monitors }, .. }) => {
                        stage_layout(pending_layout, pending_resize, request_id, monitors, Some(tokio::time::Instant::now()));
                        pointer_epoch = None;
                        let released = release_pointer_buttons(&mut input_db);
                        if released.is_empty() { Vec::new() } else { active_stage.process_fastpath_input(&mut image, &released)? }
                    }
                    Some(SessionCmd::Input { event: InputEvent::CancelMonitorLayout { request_id }, .. }) => {
                        display::cancel_pending(pending_layout, request_id);
                        Vec::new()
                    }
                    Some(SessionCmd::Resize { width, height, revision, .. }) => {
                        let (w, h) = MonitorLayoutEntry::adjust_display_size(
                            u32::from(width), u32::from(height));
                        *pending_resize = Some((w, h, revision));
                        pointer_epoch = None;
                        let released = release_pointer_buttons(&mut input_db);
                        if released.is_empty() { Vec::new() } else { active_stage.process_fastpath_input(&mut image, &released)? }
                    }
                    // The GUI owns the real Windows clipboard and hands us
                    // freshly-copied text; we only advertise it to the remote
                    // (CLIPRDR delay-rendering), so nothing crosses the wire
                    // until something over there actually pastes. The advertise
                    // itself is emitted by the clipboard pump below. With the
                    // channel disabled it just overwrites a string and stops there.
                    Some(SessionCmd::Input { event: InputEvent::ClipboardText(text), .. }) => {
                        clipboard::offer_local_text(clip, text);
                        Vec::new()
                    }
                    // File transfer: all three are metadata-or-bytes bookkeeping
                    // for the CLIPRDR pump below rather than anything that touches
                    // the desktop. Like the text offer above, nothing is read from
                    // disk here — `ClipboardFiles` advertises names and sizes, and
                    // bytes move only when one side actually pastes.
                    Some(SessionCmd::ClipboardImage { image, .. }) => {
                        clipboard::offer_local_image(clip, image);
                        Vec::new()
                    }
                    Some(SessionCmd::ClipboardFiles { files, .. }) => {
                        clipboard::offer_local_files(clip, files);
                        Vec::new()
                    }
                    Some(SessionCmd::FileContents { stream_id, data, .. }) => {
                        clipboard::submit_file_contents(clip, stream_id, data);
                        Vec::new()
                    }
                    Some(SessionCmd::RequestFileContents { generation, stream_id, index, offset, length }) => {
                        clipboard::request_file_contents(clip, generation, stream_id, index, offset, length);
                        Vec::new()
                    }
                    Some(SessionCmd::Pointer { ref event, revision, .. })
                        if !current_pointer_event(event, revision, routing.display_revision()) => Vec::new(),
                    Some(SessionCmd::Pointer { ref event, revision, .. })
                        if matches!(event, InputEvent::MouseButton { pressed: true, .. } | InputEvent::Wheel { .. })
                            && pointer_epoch != Some(revision) => Vec::new(),
                    Some(SessionCmd::Pointer { event: other @ InputEvent::MouseMove { .. }, revision, .. }) => {
                        pointer_epoch = Some(revision);
                        let events = map_input(&mut input_db, &other);
                        if events.is_empty() { Vec::new() } else { active_stage.process_fastpath_input(&mut image, &events)? }
                    }
                    // Mouse / key / unicode all fold into fast-path events.
                    Some(SessionCmd::Input { event: other, .. }) | Some(SessionCmd::Pointer { event: other, .. }) => {
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
                if host_tx.send(HostMsg::Pointer(pointer)).is_err() {
                    break 'session;
                }
                continue;
            }
            match out {
                ActiveStageOutput::ResponseFrame(frame_bytes) => while_control_open(
                    write_required_frame(
                        &mut writer,
                        &frame_bytes,
                        idle_generation,
                        network_progress,
                    ),
                    input_rx,
                )
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

                    pointer_epoch = None;
                    let released = release_pointer_buttons(&mut input_db);
                    if !released.is_empty() {
                        for out in active_stage.process_fastpath_input(&mut image, &released)? {
                            if let ActiveStageOutput::ResponseFrame(bytes) = out {
                                while_control_open(
                                    write_required_frame(
                                        &mut writer,
                                        &bytes,
                                        idle_generation,
                                        network_progress,
                                    ),
                                    input_rx,
                                )
                                .await
                                .map_err(|e| {
                                    ironrdp_session::custom_err!(
                                        "release pointer before reactivation",
                                        e
                                    )
                                })?;
                            }
                        }
                    }

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
                            let mut controls = ActivationControls {
                                input: input_rx,
                                display: display_rx,
                                deferred,
                                pending_layout,
                                pending_resize,
                                host: host_tx,
                                epoch,
                            };
                            let Some(read) =
                                wait_activation_read(reader.read_by_hint(hint), &mut controls)
                                    .await
                            else {
                                return Ok(None);
                            };
                            match read {
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
                                if let Some(monitors) = server_monitor_layout(bytes, io_channel_id)
                                {
                                    if host_tx
                                        .send(HostMsg::MonitorLayoutObserved { epoch, monitors })
                                        .is_err()
                                    {
                                        return Ok(None);
                                    }
                                    // IronRDP's finalization sequence rejects this
                                    // optional metadata PDU; consuming it here keeps
                                    // the reactivation state machine in sync.
                                    continue 'activation;
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
                            Err(error) => {
                                // The bytes are the point. Whether the PDU that ended
                                // the reactivation was a handover in a shape `detect`
                                // does not recognise is readable straight off its
                                // Share Control header — and guessing at that instead
                                // of reading it is what has produced wrong fixes for
                                // this symptom before.
                                tracing::warn!(
                                    pdu = %pdu_prefix(pdu.as_deref()),
                                    "abandoning reactivation, keeping session: {error}"
                                );
                                break 'activation;
                            }
                        };
                        if written.size().is_some() {
                            while_control_open(
                                write_required_frame(
                                    &mut writer,
                                    buf.filled(),
                                    idle_generation,
                                    network_progress,
                                ),
                                input_rx,
                            )
                            .await
                            .map_err(|e| ironrdp_session::custom_err!("write reactivation", e))?;
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
                                if host_tx
                                    .send(HostMsg::Resized {
                                        width: desktop_size.width,
                                        height: desktop_size.height,
                                    })
                                    .is_err()
                                {
                                    break 'session; // app stopped listening
                                }
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

        // Announce a graphics reset before writing any newly queued layout.
        // Otherwise a reset processed in this iteration could be mistaken for
        // confirmation of a layout sent later in the same iteration.
        if let Some((width, height, monitors)) = egfx::take_reset(egfx) {
            egfx_size = (width, height);
            if host_tx
                .send(HostMsg::MonitorLayoutObserved { epoch, monitors })
                .is_err()
            {
                break 'session;
            }
            if host_tx.send(HostMsg::Resized { width, height }).is_err() {
                break 'session;
            }
        }

        // CLIPRDR work the backend queued while it ran inside `process` above (it
        // is borrowed by the processor there and can reach neither the channel
        // nor the socket), plus anything a local copy just enqueued. Emitted
        // after this iteration's response frames so the wire ordering stays sane.
        match clipboard::pump(clip, &mut active_stage, host_tx, epoch) {
            Some(frames) => {
                for frame in frames {
                    while_control_open(
                        write_required_frame(
                            &mut writer,
                            &frame,
                            idle_generation,
                            network_progress,
                        ),
                        input_rx,
                    )
                    .await
                    .map_err(|e| ironrdp_session::custom_err!("write clipboard frame", e))?;
                }
            }
            // The app stopped listening; end the session.
            None => break 'session,
        }

        // Emit the legacy DecodedImage only for legacy-bitmap servers. Once the
        // EGFX pipeline has produced a frame, `image` is NOT the display surface
        // (GNOME/modern Windows paint via EGFX into the shared buffer, leaving
        // `image` black apart from a software cursor); emitting it would flash a
        // black frame over the real content — including right after a
        // Deactivation-Reactivation, which rebuilds `image` empty.
        if !egfx_active {
            for tile in image_tiles(&image, dirty_rects) {
                if host_tx.send(tile).is_err() {
                    break 'session;
                }
            }
        }

        let channel_open = display::channel_open(&mut active_stage);
        let channel_closed = display_channel.observe(channel_open);
        if channel_closed {
            let mut state = display.lock().unwrap_or_else(|e| e.into_inner());
            state.max_monitors = 0;
            state.max_area = 0;
        }
        if let Some(request) = pending_layout.as_ref() {
            let encoded = display::poll_layout(
                request,
                tokio::time::Instant::now(),
                channel_closed,
                |monitors| {
                    // Give already-queued cancellations and newer requests a
                    // chance to supersede this one before it reaches the wire.
                    if display_rx.is_empty() {
                        display::encode_layout(&mut active_stage, monitors, display)
                    } else {
                        None
                    }
                },
            );
            if let Some(encoded) = encoded {
                let request_id = request.request_id;
                let result = match encoded {
                    Ok(bytes) => {
                        match while_control_open(
                            write_display_frame(
                                &mut writer,
                                &bytes,
                                Some(display_rx),
                                idle_generation,
                                network_progress,
                                epoch,
                                Some(request_id),
                            ),
                            input_rx,
                        )
                        .await
                        .map_err(|e| ironrdp_session::custom_err!("write monitor layout", e))?
                        {
                            DisplayWrite::Sent => Ok(()),
                            DisplayWrite::UnstartedTimeout => {
                                Err("DisplayControl write timed out before starting".into())
                            }
                            DisplayWrite::Superseded(command) => {
                                *pending_layout = None;
                                deferred.push_front(command);
                                pump_initial_layout = true;
                                continue 'session;
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                *pending_layout = None;
                if host_tx
                    .send(HostMsg::MonitorLayoutResult {
                        epoch,
                        request_id,
                        result,
                    })
                    .is_err()
                {
                    break 'session;
                }
            }
        }
        let state = display.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if last_display_state.as_ref() != Some(&state) {
            if host_tx
                .send(HostMsg::DisplayControl {
                    epoch,
                    state: state.clone(),
                })
                .is_err()
            {
                break 'session;
            }
            last_display_state = Some(state);
        }

        // Retry a parked resize now that more channels may have connected.
        if let Some((w, h, revision)) = pending_resize.filter(|_| {
            display_rx.is_empty()
                && pending_layout.is_none()
                && display::ready(&mut active_stage, display)
        }) {
            if let Some(frame_res) = active_stage.encode_resize(w, h, None, None) {
                tracing::info!(w, h, "parked resize went out once DisplayControl connected");
                *pending_resize = None;
                match while_control_open(
                    write_display_frame(
                        &mut writer,
                        &frame_res?,
                        Some(display_rx),
                        idle_generation,
                        network_progress,
                        epoch,
                        None,
                    ),
                    input_rx,
                )
                .await
                .map_err(|e| ironrdp_session::custom_err!("write resize frame", e))?
                {
                    DisplayWrite::Sent => {
                        if host_tx
                            .send(HostMsg::ResizeSent { epoch, revision })
                            .is_err()
                        {
                            break 'session;
                        }
                    }
                    DisplayWrite::UnstartedTimeout => {
                        if host_tx
                            .send(HostMsg::ResizeRejected { epoch, revision })
                            .is_err()
                        {
                            break 'session;
                        }
                    }
                    DisplayWrite::Superseded(command) => {
                        deferred.push_front(command);
                        pump_initial_layout = true;
                        continue 'session;
                    }
                }
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
                if host_tx
                    .send(HostMsg::Resized {
                        width: first.surface_width,
                        height: first.surface_height,
                    })
                    .is_err()
                {
                    break 'session;
                }
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
            let message = if let Some(painted) = update.painted {
                HostMsg::MaskedTile {
                    x: update.x,
                    y: update.y,
                    width: update.width,
                    height: update.height,
                    rgba: update.rgba,
                    painted,
                }
            } else {
                HostMsg::Tile {
                    x: update.x,
                    y: update.y,
                    width: update.width,
                    height: update.height,
                    rgba: update.rgba,
                }
            };
            if host_tx.send(message).is_err() {
                break 'session;
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod display_deadline_tests {
    use super::*;
    use ironrdp_connector::connection_activation::ConnectionActivationFactory;

    static TEST_IDLE_GENERATION: AtomicU64 = AtomicU64::new(0);
    static TEST_NETWORK_PROGRESS: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn issue97_legacy_monitor_layout_pdu_preserves_actual_geometry() {
        use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
        use ironrdp_pdu::mcs::SendDataIndication;
        use ironrdp_pdu::rdp::client_info::CompressionType;
        use ironrdp_pdu::rdp::finalization_messages::MonitorLayoutPdu;
        use ironrdp_pdu::rdp::headers::{
            CompressionFlags, ShareControlHeader, ShareControlPdu, ShareDataHeader, ShareDataPdu,
            StreamPriority,
        };
        use ironrdp_pdu::x224::X224;
        let monitors = vec![
            Monitor {
                left: -400,
                top: 0,
                right: -1,
                bottom: 199,
                flags: MonitorFlags::empty(),
            },
            Monitor {
                left: 0,
                top: 0,
                right: 199,
                bottom: 199,
                flags: MonitorFlags::PRIMARY,
            },
        ];
        let body = ironrdp_core::encode_vec(&ShareControlHeader {
            share_control_pdu: ShareControlPdu::Data(ShareDataHeader {
                share_data_pdu: ShareDataPdu::MonitorLayout(MonitorLayoutPdu {
                    monitors: monitors.clone(),
                }),
                stream_priority: StreamPriority::Medium,
                compression_flags: CompressionFlags::empty(),
                compression_type: CompressionType::K8,
            }),
            pdu_source: 1001,
            share_id: 1,
        })
        .unwrap();
        let packet = ironrdp_core::encode_vec(&X224(SendDataIndication {
            initiator_id: 1001,
            channel_id: 1003,
            user_data: body.into(),
        }))
        .unwrap();
        assert_eq!(
            server_monitor_layout(&packet, 1003),
            Some(egfx::monitor_geometries(&monitors))
        );
        assert_eq!(server_monitor_layout(&packet, 1004), None);
    }

    fn monitor() -> adit_rdp_proto::Monitor {
        adit_rdp_proto::Monitor {
            left: 0,
            top: 0,
            width: 800,
            height: 600,
            primary: true,
            scale_percent: 100,
        }
    }

    #[test]
    fn redirect_epoch_never_wraps_back_to_an_old_connection() {
        let routing = InputRouting::default();
        routing.state.lock().unwrap().epoch = u64::MAX;
        assert_eq!(routing.advance(), None);
        assert!(routing
            .route(0, InputEvent::MouseMove { x: 1, y: 1 })
            .is_none());
        assert!(routing
            .route(u64::MAX, InputEvent::MouseMove { x: 1, y: 1 })
            .is_some());
    }

    #[test]
    fn every_redirect_announces_its_negotiated_desktop_size() {
        let (tx, rx) = std_mpsc::channel();
        let mut announced = false;
        announce_desktop(&tx, &mut announced, 800, 600).unwrap();
        announce_desktop(&tx, &mut announced, 1024, 768).unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            HostMsg::Connected {
                width: 800,
                height: 600
            }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            HostMsg::Resized {
                width: 1024,
                height: 768
            }
        ));
    }

    #[tokio::test]
    async fn file_reply_routed_before_redirect_is_discarded_at_consumption() {
        let routing = InputRouting::default();
        let old = routing.file_contents(0, 17, Some(vec![1])).unwrap();
        assert_eq!(routing.advance(), Some(1));
        let (tx, mut input) = tokio_mpsc::unbounded_channel();
        let (_display_tx, mut display) = tokio_mpsc::unbounded_channel();
        tx.send(old).unwrap();
        tx.send(routing.file_contents(1, 17, Some(vec![2])).unwrap())
            .unwrap();
        assert!(matches!(
            next_command(&mut input, &mut display, 1).await,
            Some(SessionCmd::Discarded)
        ));
        assert!(matches!(next_command(&mut input, &mut display, 1).await,
            Some(SessionCmd::FileContents { epoch: 1, stream_id: 17, data: Some(data) }) if data == vec![2]));
    }

    #[tokio::test]
    async fn queued_old_key_is_reported_instead_of_crossing_connections_silently() {
        let routing = InputRouting::default();
        let (_, old) = routing
            .route(
                0,
                InputEvent::Unicode {
                    ch: 'x',
                    pressed: true,
                },
            )
            .unwrap();
        assert_eq!(routing.advance(), Some(1));
        let (tx, mut input) = tokio_mpsc::unbounded_channel();
        let (_display_tx, mut display) = tokio_mpsc::unbounded_channel();
        tx.send(old).unwrap();
        assert!(matches!(
            next_command(&mut input, &mut display, 1).await,
            Some(SessionCmd::RejectedKeyboard)
        ));
    }

    #[test]
    fn reconnect_keyboard_overflow_discards_the_batch_without_a_stuck_press() {
        let mut deferred = VecDeque::new();
        for _ in 0..MAX_DEFERRED_COMMANDS {
            assert!(defer_connect_command(
                &mut deferred,
                SessionCmd::Input {
                    epoch: 1,
                    event: InputEvent::Key {
                        scancode: 30,
                        extended: false,
                        pressed: true
                    },
                }
            ));
        }
        assert!(!defer_connect_command(
            &mut deferred,
            SessionCmd::Input {
                epoch: 1,
                event: InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed: false
                },
            }
        ));
        assert!(deferred.is_empty());
    }

    #[tokio::test]
    async fn stalled_reactivation_read_processes_cancel_close_and_clipboard_commands() {
        use tokio::io::AsyncReadExt;
        let (mut stream, _peer) = tokio::io::duplex(8);
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let _display_hold = display_tx.clone();
        let (host, _) = std_mpsc::channel();
        let mut deferred = VecDeque::new();
        let mut pending_layout = None;
        let mut pending_resize = None;
        let mut controls = ActivationControls {
            input: &mut input_rx,
            display: &mut display_rx,
            deferred: &mut deferred,
            pending_layout: &mut pending_layout,
            pending_resize: &mut pending_resize,
            host: &host,
            epoch: 1,
        };
        let mut bytes = [0; 1];
        let send = async move {
            display_tx
                .send(SessionCmd::Input {
                    epoch: 1,
                    event: InputEvent::MonitorLayout {
                        request_id: 9,
                        monitors: vec![monitor()],
                    },
                })
                .unwrap();
            display_tx
                .send(SessionCmd::Input {
                    epoch: 1,
                    event: InputEvent::CancelMonitorLayout { request_id: 9 },
                })
                .unwrap();
            input_tx
                .send(SessionCmd::Input {
                    epoch: 1,
                    event: InputEvent::ClipboardText("copied".into()),
                })
                .unwrap();
            input_tx
                .send(SessionCmd::ClipboardImage {
                    epoch: 1,
                    image: vec![7, 8],
                })
                .unwrap();
            drop(input_tx);
        };
        let (read, ()) = tokio::join!(
            wait_activation_read(stream.read_exact(&mut bytes), &mut controls),
            send
        );
        assert!(read.is_none());
        assert!(pending_layout.is_none());
        assert!(
            matches!(deferred.pop_front(), Some(SessionCmd::Input { event: InputEvent::ClipboardText(text), .. }) if text == "copied")
        );
        assert!(
            matches!(deferred.pop_front(), Some(SessionCmd::ClipboardImage { image, .. }) if image == [7, 8])
        );
    }

    #[tokio::test]
    async fn deactivate_all_pdu_stalled_reactivation_observes_close_and_cancel() {
        use ironrdp_pdu::mcs::SendDataIndication;
        use ironrdp_pdu::rdp::headers::{ServerDeactivateAll, ShareControlHeader, ShareControlPdu};
        use ironrdp_pdu::x224::X224;
        let request = ConnectRequest {
            host: "memory.invalid".into(),
            port: 0,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: false,
            enable_audio: false,
            quality: adit_rdp_proto::Quality::default(),
        };
        let connection = ConnectionResult {
            io_channel_id: 1003,
            user_channel_id: 1001,
            message_channel_id: None,
            share_id: 1,
            static_channels: Default::default(),
            desktop_size: ironrdp_connector::DesktopSize {
                width: 800,
                height: 600,
            },
            enable_server_pointer: false,
            pointer_software_rendering: false,
            activation_factory: ConnectionActivationFactory::new(
                build_connector_config(&request, None),
                1003,
                1001,
            ),
            compression_type: None,
        };
        let (stream, mut peer) = tokio::io::duplex(4096);
        let framed: UpgradedFramed = TokioFramed::new(Box::new(stream));
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let routing = InputRouting::default();
        let (host, _) = std_mpsc::channel();
        let egfx = egfx::new_shared();
        let clip = clipboard::new_shared();
        let display = SharedDisplay::default();
        let (mut pending, mut resize, mut deferred) = (None, None, VecDeque::new());
        let mut session = Box::pin(active_session(
            framed,
            connection,
            SessionInput {
                ordinary: &mut input_rx,
                display: &mut display_rx,
                routing: &routing,
                epoch: 0,
                deferred: &mut deferred,
                idle_generation: &TEST_IDLE_GENERATION,
                network_progress: &TEST_NETWORK_PROGRESS,
            },
            &host,
            &egfx,
            &clip,
            (&display, &mut pending, &mut resize),
        ));
        let body = ironrdp_core::encode_vec(&ShareControlHeader {
            share_control_pdu: ShareControlPdu::ServerDeactivateAll(ServerDeactivateAll),
            pdu_source: 1001,
            share_id: 1,
        })
        .unwrap();
        let packet = ironrdp_core::encode_vec(&X224(SendDataIndication {
            initiator_id: 1001,
            channel_id: 1003,
            user_data: body.into(),
        }))
        .unwrap();
        peer.write_all(&packet).await.unwrap();
        tokio::select! {
            biased;
            result = &mut session => panic!("session exited before reactivation: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        display_tx
            .send(SessionCmd::Input {
                epoch: 0,
                event: InputEvent::MonitorLayout {
                    request_id: 9,
                    monitors: vec![monitor()],
                },
            })
            .unwrap();
        display_tx
            .send(SessionCmd::Input {
                epoch: 0,
                event: InputEvent::CancelMonitorLayout { request_id: 9 },
            })
            .unwrap();
        drop(input_tx);
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut session)
            .await
            .expect("DeactivateAll read ignored cancellation and close")
            .unwrap();
        assert!(result.is_none());
        drop(session);
        assert!(pending.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_unstarted_and_partial_write_timeouts_are_distinguishable() {
        let (mut blocked, _peer) = tokio::io::duplex(1);
        blocked.write_all(&[0]).await.unwrap();
        let mut unstarted = TokioFramed::new(IdleWrite::new(blocked));
        let no_bytes = write_frame(&mut unstarted, &[1, 2]).await.unwrap_err();

        let (partial, _peer) = tokio::io::duplex(1);
        let mut partial = TokioFramed::new(IdleWrite::new(partial));
        let prefix = write_frame(&mut partial, &[1, 2]).await.unwrap_err();
        assert_eq!(no_bytes.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(prefix.kind(), std::io::ErrorKind::TimedOut);
        assert_ne!(
            no_bytes.to_string(),
            prefix.to_string(),
            "an untouched write and a partial PDU require different recovery"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_switch_cancels_an_unstarted_display_write_without_closing_stream() {
        use tokio::io::AsyncReadExt;
        let (mut stream, mut peer) = tokio::io::duplex(1);
        stream.write_all(&[99]).await.unwrap();
        let timed = IdleWrite::new(stream);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(timed);
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        tx.send(SessionCmd::Input {
            epoch: 0,
            event: InputEvent::MonitorLayout {
                request_id: 2,
                monitors: vec![monitor()],
            },
        })
        .unwrap();
        assert!(matches!(
            write_display_frame(
                &mut writer,
                &[1, 2],
                Some(&mut rx),
                &generation,
                &progress,
                0,
                Some(1)
            )
            .await
            .unwrap(),
            DisplayWrite::Superseded(SessionCmd::Input {
                event: InputEvent::MonitorLayout { request_id: 2, .. },
                ..
            })
        ));
        let mut first = [0];
        peer.read_exact(&mut first).await.unwrap();
        assert_eq!(first, [99]);
        write_frame(&mut writer, &[7]).await.unwrap();
        peer.read_exact(&mut first).await.unwrap();
        assert_eq!(
            first,
            [7],
            "no part of the cancelled layout reached the peer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_unstarted_display_timeout_preserves_the_stream() {
        use tokio::io::AsyncReadExt;
        let (mut stream, mut peer) = tokio::io::duplex(1);
        stream.write_all(&[99]).await.unwrap();
        let timed = IdleWrite::new(stream);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(timed);
        let (_tx, mut rx) = tokio_mpsc::unbounded_channel();
        assert!(matches!(
            write_display_frame(
                &mut writer,
                &[1, 2],
                Some(&mut rx),
                &generation,
                &progress,
                0,
                Some(1)
            )
            .await
            .unwrap(),
            DisplayWrite::UnstartedTimeout
        ));
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        write_frame(&mut writer, &[7]).await.unwrap();
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [7]);
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_pointer_release_waits_for_an_idle_socket_without_disconnect() {
        use tokio::io::AsyncReadExt;
        let (mut socket, mut peer) = tokio::io::duplex(1);
        socket.write_all(&[99]).await.unwrap();
        let timed = IdleWrite::new(socket);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(timed);
        let write = write_required_frame(&mut writer, &[7], &generation, &progress);
        tokio::pin!(write);
        tokio::select! {
            result = &mut write => panic!("idle pointer release ended: {result:?}"),
            _ = tokio::time::sleep(WRITE_TIMEOUT + std::time::Duration::from_secs(1)) => {}
        }
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [99]);
        write.await.unwrap();
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [7]);
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_explicit_close_interrupts_an_idle_required_write() {
        let (mut socket, _peer) = tokio::io::duplex(1);
        socket.write_all(&[99]).await.unwrap();
        let timed = IdleWrite::new(socket);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(timed);
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        drop(tx);
        let result = while_control_open(
            write_required_frame(&mut writer, &[7], &generation, &progress),
            &rx,
        )
        .await;
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_buffered_display_write_survives_idle_socket_timeout() {
        use tokio::io::AsyncReadExt;
        struct Buffered {
            inner: IdleWrite<tokio::io::DuplexStream>,
            bytes: Vec<u8>,
            offset: usize,
        }
        impl AsyncWrite for Buffered {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                self.bytes.extend_from_slice(bytes);
                Poll::Ready(Ok(bytes.len()))
            }
            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                let this = self.get_mut();
                while this.offset < this.bytes.len() {
                    match Pin::new(&mut this.inner).poll_write(cx, &this.bytes[this.offset..]) {
                        Poll::Ready(Ok(0)) => {
                            return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()))
                        }
                        Poll::Ready(Ok(n)) => this.offset += n,
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
            }
        }
        let (mut socket, mut peer) = tokio::io::duplex(1);
        socket.write_all(&[99]).await.unwrap();
        let timed = IdleWrite::new(socket);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(Buffered {
            inner: timed,
            bytes: Vec::new(),
            offset: 0,
        });
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let write = write_display_frame(
            &mut writer,
            &[7],
            Some(&mut rx),
            &generation,
            &progress,
            0,
            Some(1),
        );
        tokio::pin!(write);
        tokio::select! {
            result = &mut write => panic!("idle buffered write closed the connection: {}", result.is_err()),
            _ = tokio::time::sleep(WRITE_TIMEOUT + std::time::Duration::from_secs(1)) => {}
        }
        tx.send(SessionCmd::Input {
            epoch: 0,
            event: InputEvent::MonitorLayout {
                request_id: 2,
                monitors: vec![monitor()],
            },
        })
        .unwrap();
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [99]);
        assert!(matches!(write.await.unwrap(), DisplayWrite::Sent));
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [7]);
    }

    #[tokio::test]
    async fn issue97_switch_waits_for_a_partially_written_pdu() {
        use tokio::io::AsyncReadExt;
        let (stream, mut peer) = tokio::io::duplex(1);
        let timed = IdleWrite::new(stream);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(timed);
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut bytes = [0; 2];
        {
            let write = write_display_frame(
                &mut writer,
                &[1, 2],
                Some(&mut rx),
                &generation,
                &progress,
                0,
                Some(1),
            );
            tokio::pin!(write);
            tokio::select! {
                result = &mut write => panic!("partial write ended early: {}", result.is_ok()),
                _ = tokio::task::yield_now() => {}
            }
            tx.send(SessionCmd::Input {
                epoch: 0,
                event: InputEvent::MonitorLayout {
                    request_id: 2,
                    monitors: vec![monitor()],
                },
            })
            .unwrap();
            peer.read_exact(&mut bytes[..1]).await.unwrap();
            assert!(matches!(write.await.unwrap(), DisplayWrite::Sent));
        }
        peer.read_exact(&mut bytes[1..]).await.unwrap();
        assert_eq!(bytes, [1, 2]);
        assert!(matches!(
            rx.try_recv(),
            Ok(SessionCmd::Input {
                event: InputEvent::MonitorLayout { request_id: 2, .. },
                ..
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn issue97_partial_display_write_timeout_is_fatal() {
        use tokio::io::AsyncReadExt;
        let (socket, mut peer) = tokio::io::duplex(1);
        let timed = IdleWrite::new(socket);
        let generation = Arc::clone(&timed.generation);
        let progress = Arc::clone(&timed.progress);
        let mut writer = TokioFramed::new(timed);
        let (_tx, mut rx) = tokio_mpsc::unbounded_channel();
        let error = match write_display_frame(
            &mut writer,
            &[1, 2],
            Some(&mut rx),
            &generation,
            &progress,
            0,
            Some(1),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("a partial stalled PDU cannot be abandoned safely"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(progress.load(Ordering::Relaxed), 1);
        let mut first = [0];
        peer.read_exact(&mut first).await.unwrap();
        assert_eq!(first, [1]);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_real_framed_write_times_out_after_a_partial_pdu() {
        use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags};
        use tokio::io::AsyncReadExt;
        let frame = ironrdp_core::encode_vec(&FastPathInput::single(
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 30),
        ))
        .unwrap();
        let (stream, mut peer) = tokio::io::duplex(1);
        let mut writer = TokioFramed::new(IdleWrite::new(stream));
        let error = write_frame(&mut writer, &frame).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let mut first = [0];
        peer.read_exact(&mut first).await.unwrap();
        assert_eq!(
            first,
            [frame[0]],
            "a timed-out write may already have sent a prefix"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progressing_framed_write_can_exceed_the_idle_deadline() {
        use tokio::io::AsyncReadExt;
        let (stream, mut peer) = tokio::io::duplex(1);
        let mut writer = TokioFramed::new(IdleWrite::new(stream));
        let receiver = tokio::spawn(async move {
            let mut result = Vec::new();
            for _ in 0..4 {
                let mut byte = [0];
                peer.read_exact(&mut byte).await.unwrap();
                result.push(byte[0]);
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
            result
        });
        write_frame(&mut writer, &[1, 2, 3, 4]).await.unwrap();
        assert_eq!(receiver.await.unwrap(), [1, 2, 3, 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn buffered_flush_can_progress_longer_than_the_idle_deadline() {
        use tokio::io::AsyncReadExt;
        struct Buffered {
            inner: IdleWrite<tokio::io::DuplexStream>,
            bytes: Vec<u8>,
            offset: usize,
        }
        impl AsyncWrite for Buffered {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                self.bytes.extend_from_slice(bytes);
                Poll::Ready(Ok(bytes.len()))
            }
            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                let this = self.get_mut();
                while this.offset < this.bytes.len() {
                    match Pin::new(&mut this.inner).poll_write(cx, &this.bytes[this.offset..]) {
                        Poll::Ready(Ok(n)) if n > 0 => this.offset += n,
                        Poll::Ready(Ok(_)) => {
                            return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()))
                        }
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Pin::new(&mut this.inner).poll_flush(cx)
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
            }
        }
        let (stream, mut peer) = tokio::io::duplex(1);
        let mut writer = TokioFramed::new(Buffered {
            inner: IdleWrite::new(stream),
            bytes: Vec::new(),
            offset: 0,
        });
        let receiver = tokio::spawn(async move {
            let mut result = Vec::new();
            for _ in 0..4 {
                let mut byte = [0];
                peer.read_exact(&mut byte).await.unwrap();
                result.push(byte[0]);
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
            result
        });
        write_frame(&mut writer, &[1, 2, 3, 4]).await.unwrap();
        assert_eq!(receiver.await.unwrap(), [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn keys_buffered_during_redirect_are_encoded_by_the_real_active_stage() {
        use ironrdp_core::{Decode, ReadCursor};
        use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags};
        let request = ConnectRequest {
            host: "memory.invalid".into(),
            port: 0,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: false,
            enable_audio: false,
            quality: adit_rdp_proto::Quality::default(),
        };
        let connection = ConnectionResult {
            io_channel_id: 1003,
            user_channel_id: 1001,
            message_channel_id: None,
            share_id: 1,
            static_channels: Default::default(),
            desktop_size: ironrdp_connector::DesktopSize {
                width: 800,
                height: 600,
            },
            enable_server_pointer: false,
            pointer_software_rendering: false,
            activation_factory: ConnectionActivationFactory::new(
                build_connector_config(&request, None),
                1003,
                1001,
            ),
            compression_type: None,
        };
        let (stream, peer) = tokio::io::duplex(1024);
        let framed: UpgradedFramed = TokioFramed::new(Box::new(stream));
        let mut peer = TokioFramed::new(peer);
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (_display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let routing = InputRouting::default();
        let (_, stale) = routing
            .route(
                0,
                InputEvent::Unicode {
                    ch: 'x',
                    pressed: true,
                },
            )
            .unwrap();
        assert_eq!(routing.advance(), Some(1));
        input_tx.send(stale).unwrap();
        let mut deferred = VecDeque::new();
        for pressed in [true, false] {
            let (_, command) = routing
                .route(
                    1,
                    InputEvent::Key {
                        scancode: 30,
                        extended: false,
                        pressed,
                    },
                )
                .unwrap();
            assert!(defer_connect_command(&mut deferred, command));
        }
        let (_, clipboard_text) = routing
            .route(
                1,
                InputEvent::ClipboardText("copied during reconnect".into()),
            )
            .unwrap();
        assert!(defer_connect_command(&mut deferred, clipboard_text));
        assert!(defer_connect_command(
            &mut deferred,
            SessionCmd::ClipboardImage {
                epoch: 1,
                image: vec![3, 4, 5]
            },
        ));
        let (host_tx, host_rx) = std_mpsc::channel();
        let egfx = egfx::new_shared();
        let clip = clipboard::new_shared();
        let display = SharedDisplay::default();
        let (mut pending, mut resize) = (None, None);
        let session = active_session(
            framed,
            connection,
            SessionInput {
                ordinary: &mut input_rx,
                display: &mut display_rx,
                routing: &routing,
                epoch: 1,
                deferred: &mut deferred,
                idle_generation: &TEST_IDLE_GENERATION,
                network_progress: &TEST_NETWORK_PROGRESS,
            },
            &host_tx,
            &egfx,
            &clip,
            (&display, &mut pending, &mut resize),
        );
        tokio::pin!(session);
        for released in [false, true] {
            let bytes = tokio::select! {
                result = &mut session => panic!("memory session exited: {result:?}"),
                result = peer.read_pdu() => result.unwrap().1,
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => panic!("buffered key did not reach the RDP stream"),
            };
            let pdu = FastPathInput::decode(&mut ReadCursor::new(&bytes)).unwrap();
            assert!(
                matches!(pdu.input_events(), [FastPathInputEvent::KeyboardEvent(flags, 30)] if flags.contains(KeyboardFlags::RELEASE) == released)
            );
        }
        let notice = async {
            loop {
                if matches!(host_rx.try_recv(), Ok(HostMsg::InputDiscarded)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            result = &mut session => panic!("memory session exited: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(2), notice) => result.expect("stale key was not reported"),
        }
        assert_eq!(clipboard::test_local_image(&clip), Some(vec![3, 4, 5]));
    }

    #[tokio::test]
    async fn display_control_is_not_starved_by_ordinary_input_backlog() {
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        for _ in 0..10_000 {
            let event = InputEvent::MouseMove { x: 5, y: 6 };
            assert!(!is_display_input(&event));
            assert!(is_pointer_input(&event));
            input_tx
                .send(SessionCmd::Pointer {
                    event,
                    epoch: 0,
                    revision: 0,
                })
                .unwrap();
        }
        let layout = InputEvent::MonitorLayout {
            request_id: 9,
            monitors: vec![monitor()],
        };
        assert!(is_display_input(&layout));
        display_tx
            .send(SessionCmd::Input {
                epoch: 0,
                event: layout,
            })
            .unwrap();
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 0).await,
            Some(SessionCmd::Input {
                event: InputEvent::MonitorLayout { request_id: 9, .. },
                ..
            })
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 0).await,
            Some(SessionCmd::Pointer {
                event: InputEvent::MouseMove { x: 5, y: 6 },
                epoch: 0,
                revision: 0,
            })
        ));
        assert!(!current_pointer_event(
            &InputEvent::MouseMove { x: 5, y: 6 },
            0,
            1
        ));
        assert!(!current_pointer_event(
            &InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true
            },
            0,
            1
        ));
        assert!(current_pointer_event(
            &InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: false
            },
            0,
            1
        ));
    }

    #[tokio::test]
    async fn redirect_discards_commands_routed_before_but_queued_after_the_epoch_change() {
        let routing = InputRouting::default();
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let (_, old_resize) = routing
            .route(
                0,
                InputEvent::Resize {
                    width: 640,
                    height: 480,
                },
            )
            .unwrap();
        let (_, old_layout) = routing
            .route(
                0,
                InputEvent::MonitorLayout {
                    request_id: 1,
                    monitors: vec![monitor()],
                },
            )
            .unwrap();
        let (_, old_release) = routing
            .route(
                0,
                InputEvent::MouseButton {
                    button: adit_rdp_proto::MouseButton::Left,
                    pressed: false,
                },
            )
            .unwrap();
        assert_eq!(routing.advance(), Some(1));
        let revision = routing.display_revision();
        assert!(routing
            .route(
                0,
                InputEvent::Resize {
                    width: 800,
                    height: 600
                }
            )
            .is_none());
        assert!(routing
            .route(2, InputEvent::MouseMove { x: 1, y: 1 })
            .is_none());
        assert_eq!(routing.display_revision(), revision);
        let (_, new_layout) = routing
            .route(
                1,
                InputEvent::MonitorLayout {
                    request_id: 2,
                    monitors: vec![monitor()],
                },
            )
            .unwrap();
        let (_, new_pointer) = routing
            .route(1, InputEvent::MouseMove { x: 12, y: 34 })
            .unwrap();
        let current_revision = routing.display_revision();
        assert!(routing
            .route(
                0,
                InputEvent::MonitorLayout {
                    request_id: 3,
                    monitors: vec![monitor()]
                }
            )
            .is_none());
        assert_eq!(routing.display_revision(), current_revision);
        display_tx.send(old_resize).unwrap();
        display_tx.send(old_layout).unwrap();
        display_tx.send(new_layout).unwrap();
        input_tx.send(old_release).unwrap();
        input_tx.send(new_pointer).unwrap();
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Discarded)
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Discarded)
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Input {
                epoch: 1,
                event: InputEvent::MonitorLayout { request_id: 2, .. }
            })
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Discarded)
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Pointer {
                epoch: 1,
                event: InputEvent::MouseMove { x: 12, y: 34 },
                ..
            })
        ));
        assert!(input_rx.is_empty() && display_rx.is_empty());
    }

    #[tokio::test]
    async fn old_routed_commands_arriving_after_new_commands_are_also_discarded() {
        let routing = InputRouting::default();
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let (_, old_resize) = routing
            .route(
                0,
                InputEvent::Resize {
                    width: 640,
                    height: 480,
                },
            )
            .unwrap();
        let (_, old_pointer) = routing
            .route(0, InputEvent::MouseMove { x: 1, y: 2 })
            .unwrap();
        assert_eq!(routing.advance(), Some(1));
        let (_, new_resize) = routing
            .route(
                1,
                InputEvent::Resize {
                    width: 900,
                    height: 700,
                },
            )
            .unwrap();
        let (_, new_pointer) = routing
            .route(1, InputEvent::MouseMove { x: 12, y: 34 })
            .unwrap();
        display_tx.send(new_resize).unwrap();
        display_tx.send(old_resize).unwrap();
        input_tx.send(new_pointer).unwrap();
        input_tx.send(old_pointer).unwrap();
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Resize {
                epoch: 1,
                revision: 1,
                width: 900,
                height: 700
            })
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Discarded)
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Pointer {
                epoch: 1,
                event: InputEvent::MouseMove { x: 12, y: 34 },
                ..
            })
        ));
        assert!(matches!(
            next_command(&mut input_rx, &mut display_rx, 1).await,
            Some(SessionCmd::Discarded)
        ));
    }

    #[tokio::test]
    async fn stale_pointer_never_writes_to_the_new_synthetic_rdp_channel() {
        use ironrdp_core::{Decode, ReadCursor};
        use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
        use tokio::io::{AsyncRead, ReadBuf};
        let request = ConnectRequest {
            host: "synthetic.invalid".into(),
            port: 0,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: false,
            enable_audio: false,
            quality: adit_rdp_proto::Quality::default(),
        };
        let connection = ConnectionResult {
            io_channel_id: 1003,
            user_channel_id: 1001,
            message_channel_id: None,
            share_id: 1,
            static_channels: Default::default(),
            desktop_size: ironrdp_connector::DesktopSize {
                width: 800,
                height: 600,
            },
            enable_server_pointer: false,
            pointer_software_rendering: false,
            activation_factory: ConnectionActivationFactory::new(
                build_connector_config(&request, None),
                1003,
                1001,
            ),
            compression_type: None,
        };
        let (stream, mut peer) = tokio::io::duplex(1024);
        let framed: UpgradedFramed = TokioFramed::new(Box::new(stream));
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (_display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let routing = InputRouting::default();
        let (_, old) = routing
            .route(0, InputEvent::MouseMove { x: 10, y: 20 })
            .unwrap();
        assert_eq!(routing.advance(), Some(1));
        input_tx.send(old).unwrap(); // delivered across the thread boundary after redirect
        let (host_tx, host_rx) = std_mpsc::channel();
        let egfx = egfx::new_shared();
        let clip = clipboard::new_shared();
        let display = SharedDisplay::default();
        let (mut pending, mut pending_resize) = (None, None);
        let mut deferred = VecDeque::new();
        let session = active_session(
            framed,
            connection,
            SessionInput {
                ordinary: &mut input_rx,
                display: &mut display_rx,
                routing: &routing,
                epoch: 1,
                deferred: &mut deferred,
                idle_generation: &TEST_IDLE_GENERATION,
                network_progress: &TEST_NETWORK_PROGRESS,
            },
            &host_tx,
            &egfx,
            &clip,
            (&display, &mut pending, &mut pending_resize),
        );
        tokio::pin!(session);
        tokio::select! {
            biased;
            result = &mut session => panic!("synthetic session exited: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(matches!(
            host_rx.try_recv(),
            Ok(HostMsg::DisplayControl { epoch: 1, .. })
        ));
        let mut scratch = [0u8; 128];
        let readable = std::future::poll_fn(|cx| {
            let mut out = ReadBuf::new(&mut scratch);
            std::task::Poll::Ready(matches!(std::pin::Pin::new(&mut peer).poll_read(cx, &mut out), std::task::Poll::Ready(Ok(())) if !out.filled().is_empty()))
        }).await;
        assert!(!readable, "stale pointer wrote to the new channel");
        let (_, current) = routing
            .route(1, InputEvent::MouseMove { x: 21, y: 34 })
            .unwrap();
        input_tx.send(current).unwrap();
        let mut peer = TokioFramed::new(peer);
        tokio::select! {
            biased;
            result = &mut session => panic!("synthetic session exited: {result:?}"),
            result = peer.read_pdu() => {
                let (action, bytes) = result.unwrap();
                assert!(matches!(action, ironrdp_pdu::Action::FastPath));
                let input = FastPathInput::decode(&mut ReadCursor::new(&bytes)).unwrap();
                assert!(matches!(input.input_events(), [FastPathInputEvent::MouseEvent(mouse)] if (mouse.x_position, mouse.y_position) == (21, 34)));
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => panic!("current pointer was starved"),
        }
    }

    #[test]
    fn new_layout_clears_parked_resize_and_redirect_discards_old_commands() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut pending = None;
        let mut resize = Some((640, 480, 1));
        stage_layout(&mut pending, &mut resize, 1, vec![monitor()], None);
        assert!(resize.is_none());
        assert_eq!(pending.as_ref().unwrap().request_id, 1);
        resize = Some((800, 600, 2));
        tx.send(SessionCmd::Input {
            epoch: 0,
            event: InputEvent::Resize {
                width: 900,
                height: 700,
            },
        })
        .unwrap();
        reset_display_requests(&mut pending, &mut resize, &mut rx);
        assert!(pending.is_none() && resize.is_none() && rx.is_empty());
        tx.send(SessionCmd::Input {
            epoch: 1,
            event: InputEvent::MonitorLayout {
                request_id: 2,
                monitors: vec![monitor()],
            },
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(SessionCmd::Input {
                epoch: 1,
                event: InputEvent::MonitorLayout { request_id: 2, .. }
            })
        ));
    }

    #[test]
    fn changing_layout_releases_held_buttons_without_releasing_keys() {
        let mut db = Database::new();
        let button = adit_rdp_proto::MouseButton::Left;
        assert!(!map_input(
            &mut db,
            &InputEvent::MouseButton {
                button,
                pressed: true
            }
        )
        .is_empty());
        assert!(!map_input(
            &mut db,
            &InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true
            }
        )
        .is_empty());
        assert_eq!(release_pointer_buttons(&mut db).len(), 1);
        assert!(release_pointer_buttons(&mut db).is_empty());
        assert!(!map_input(
            &mut db,
            &InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: false
            }
        )
        .is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_active_session_reports_missing_capabilities_without_a_server_pdu() {
        let request = ConnectRequest {
            host: "synthetic.invalid".into(),
            port: 0,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: false,
            enable_audio: false,
            quality: adit_rdp_proto::Quality::default(),
        };
        let config = build_connector_config(&request, None);
        let connection = ConnectionResult {
            io_channel_id: 1003,
            user_channel_id: 1001,
            message_channel_id: None,
            share_id: 1,
            static_channels: Default::default(),
            desktop_size: ironrdp_connector::DesktopSize {
                width: 800,
                height: 600,
            },
            enable_server_pointer: false,
            pointer_software_rendering: false,
            activation_factory: ConnectionActivationFactory::new(config, 1003, 1001),
            compression_type: None,
        };
        let (stream, _peer) = tokio::io::duplex(1024);
        let framed: UpgradedFramed = TokioFramed::new(Box::new(stream));
        let (_input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (_display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let routing = InputRouting::default();
        let (host_tx, host_rx) = std_mpsc::channel();
        let egfx = egfx::new_shared();
        let clip = clipboard::new_shared();
        let display = SharedDisplay::default();
        let mut pending = Some(display::PendingLayout::new(
            42,
            vec![adit_rdp_proto::Monitor {
                left: 0,
                top: 0,
                width: 800,
                height: 600,
                primary: true,
                scale_percent: 100,
            }],
        ));
        let mut pending_resize = None;
        let mut deferred = VecDeque::new();
        let session = active_session(
            framed,
            connection,
            SessionInput {
                ordinary: &mut input_rx,
                display: &mut display_rx,
                routing: &routing,
                epoch: 0,
                deferred: &mut deferred,
                idle_generation: &TEST_IDLE_GENERATION,
                network_progress: &TEST_NETWORK_PROGRESS,
            },
            &host_tx,
            &egfx,
            &clip,
            (&display, &mut pending, &mut pending_resize),
        );
        tokio::pin!(session);
        // The initial capability snapshot proves the loop has started waiting.
        tokio::select! {
            biased;
            result = &mut session => panic!("idle session exited: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(matches!(
            host_rx.try_recv(),
            Ok(HostMsg::DisplayControl { epoch: 0, .. })
        ));
        tokio::select! {
            biased;
            result = &mut session => panic!("idle session exited: {result:?}"),
            _ = tokio::time::advance(display::CAPABILITIES_TIMEOUT) => {}
        }
        tokio::select! {
            biased;
            result = &mut session => panic!("idle session exited: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(matches!(
            host_rx.try_recv(),
            Ok(HostMsg::MonitorLayoutResult { epoch: 0, request_id: 42, result: Err(error) })
                if error == "DisplayControl capabilities timed out"
        ));
    }
}

#[cfg(test)]
mod process_error_tests {
    use super::*;

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
