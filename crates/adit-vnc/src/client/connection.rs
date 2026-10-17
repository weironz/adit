use futures::TryStreamExt;
use tokio_stream::wrappers::ReceiverStream;

use std::{
    future::Future,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        mpsc::{channel, error::TryRecvError, Receiver, Sender},
        oneshot, Mutex,
    },
};
use tokio_util::compat::*;
use tracing::*;

use crate::{codec, PixelFormat, Rect, VncEncoding, VncError, VncEvent, X11Event};
const NETWORK_CHANNEL_SIZE: usize = 16;
const INPUT_CHANNEL_SIZE: usize = 256;
const OUTPUT_CHANNEL_SIZE: usize = 2;

mod output;

#[cfg(not(target_arch = "wasm32"))]
use tokio::spawn;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local as spawn;

use super::messages::{ClientMsg, ServerMsg};
use super::resize::DesktopState;

struct ImageRect {
    rect: Rect,
    encoding: VncEncoding,
}

impl TryFrom<[u8; 12]> for ImageRect {
    type Error = VncError;
    fn try_from(buf: [u8; 12]) -> Result<Self, VncError> {
        Ok(Self {
            rect: Rect {
                x: ((buf[0] as u16) << 8) | buf[1] as u16,
                y: ((buf[2] as u16) << 8) | buf[3] as u16,
                width: ((buf[4] as u16) << 8) | buf[5] as u16,
                height: ((buf[6] as u16) << 8) | buf[7] as u16,
            },
            encoding: VncEncoding::from_wire(
                ((buf[8] as u32) << 24)
                    | ((buf[9] as u32) << 16)
                    | ((buf[10] as u32) << 8)
                    | (buf[11] as u32),
            )?,
        })
    }
}

impl ImageRect {
    async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut rect_buf = [0_u8; 12];
        reader.read_exact(&mut rect_buf).await?;
        rect_buf.try_into()
    }
}

/// The framebuffer size is shared between the decoder, which learns of resizes,
/// and the input path, which sizes refresh requests; pack it into one atomic.
fn pack_screen((width, height): (u16, u16)) -> u32 {
    (u32::from(width) << 16) | u32::from(height)
}

fn unpack_screen(packed: u32) -> (u16, u16) {
    ((packed >> 16) as u16, packed as u16)
}

struct VncInner {
    name: String,
    screen: Arc<AtomicU32>,
    desktop: Arc<DesktopState>,
    flow: Arc<crate::flow::Flow>,
    clipboard: Arc<crate::clipboard::Clipboard>,
    input_ch: Sender<ClientMsg>,
    output_ch: Receiver<VncEvent>,
    decoding_stop: Option<oneshot::Sender<()>>,
    net_conn_stop: Option<oneshot::Sender<()>>,
    closed: bool,
}

/// The instance of a connected vnc client
///
impl VncInner {
    async fn new<S>(
        mut stream: S,
        shared: bool,
        mut pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
        tight: bool,
        utf8_clipboard: bool,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (conn_ch_tx, conn_ch_rx) = channel(NETWORK_CHANNEL_SIZE);
        let (input_ch_tx, input_ch_rx) = channel(INPUT_CHANNEL_SIZE);
        let (output_ch_tx, output_ch_rx) = channel(OUTPUT_CHANNEL_SIZE);
        let (decoding_stop_tx, mut decoding_stop_rx) = oneshot::channel();
        let (net_conn_stop_tx, net_conn_stop_rx) = oneshot::channel();

        trace!("client init msg");
        send_client_init(&mut stream, shared).await?;

        trace!("server init msg");
        let (name, (width, height)) =
            read_server_init(&mut stream, &mut pixel_format, &|e| async {
                output_ch_tx.send(e).await?;
                Ok(())
            })
            .await?;

        // Only Tight security appends interaction caps. Never consume these
        // bytes or emit vendor enable based on a desktop name or version string.
        let vendor_utf8 = if tight {
            crate::tight::interaction(&mut stream).await? && utf8_clipboard
        } else {
            false
        };
        if vendor_utf8 {
            stream.write_u32(crate::tight::ENABLE_UTF8).await?;
        }
        let clipboard = Arc::new(crate::clipboard::Clipboard::default());
        clipboard.tight_utf8.store(vendor_utf8, Ordering::Release);
        *clipboard.sender.lock().unwrap() = Some(input_ch_tx.clone());
        let decoder_clipboard = Arc::clone(&clipboard);
        let flow = Arc::new(crate::flow::Flow::initial());
        let decoder_flow = Arc::clone(&flow);
        let screen = Arc::new(AtomicU32::new(pack_screen((width, height))));
        let decoder_screen = Arc::clone(&screen);
        let desktop = Arc::new(DesktopState::new(Arc::clone(&flow)));
        let decoder_desktop = Arc::clone(&desktop);
        let network_desktop = Arc::clone(&desktop);
        trace!("client encodings: {:?}", encodings);
        send_client_encoding(&mut stream, encodings.clone()).await?;

        trace!("Require the first frame");
        input_ch_tx
            .send(ClientMsg::FramebufferUpdateRequest(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                0,
            ))
            .await?;

        // start the decoding thread
        spawn(async move {
            trace!("Decoding thread starts");
            let mut conn_ch_rx = {
                let conn_ch_rx = ReceiverStream::new(conn_ch_rx).into_async_read();
                FuturesAsyncReadCompatExt::compat(conn_ch_rx)
            };

            let output_func = |e| async {
                output_ch_tx.send(e).await?;
                Ok(())
            };

            // Deliver capability metadata after spawning: initialization may have
            // filled both bounded output slots when no custom pixel format is set.
            if vendor_utf8
                && output_func(VncEvent::ClipboardCapabilities { utf8: true })
                    .await
                    .is_err()
            {
                return;
            }
            let pf = pixel_format.as_ref().unwrap();
            let result = asycn_vnc_read_loop_flow(
                &mut conn_ch_rx,
                pf,
                &output_func,
                &mut decoding_stop_rx,
                &encodings,
                &decoder_screen,
                &decoder_desktop,
                &decoder_flow,
                &decoder_clipboard,
            )
            .await;
            // Release the network worker and fail any pending resize before waiting
            // for output capacity.
            drop(conn_ch_rx);
            decoder_desktop.close();
            if let Err(error) = result {
                output::report_error(error, &output_ch_tx, &mut decoding_stop_rx).await;
            }
            trace!("Decoding thread stops");
        });

        // start the traffic process thread
        spawn(async move {
            trace!("Net Connection thread starts");
            let _ =
                async_connection_process_loop(stream, input_ch_rx, conn_ch_tx, net_conn_stop_rx)
                    .await;
            network_desktop.close();
            trace!("Net Connection thread stops");
        });

        info!("VNC Client {name} starts");
        Ok(Self {
            name,
            screen,
            desktop,
            flow,
            clipboard,
            input_ch: input_ch_tx,
            output_ch: output_ch_rx,
            decoding_stop: Some(decoding_stop_tx),
            net_conn_stop: Some(net_conn_stop_tx),
            closed: false,
        })
    }

    fn input_message(&self, event: X11Event) -> Result<ClientMsg, VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            let (width, height) = unpack_screen(self.screen.load(Ordering::Acquire));
            let msg = match event {
                X11Event::FencePing(sequence) => {
                    if !self.clipboard.fence.load(Ordering::Acquire) {
                        return Err(VncError::General("Fence not negotiated".into()));
                    }
                    ClientMsg::Fence(0x8000_0000, sequence.to_be_bytes().to_vec())
                }
                X11Event::Refresh => ClientMsg::FramebufferUpdateRequest(
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    1,
                ),
                X11Event::FullRefresh => ClientMsg::FramebufferUpdateRequest(
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    0, // non-incremental: server sends entire framebuffer
                ),
                X11Event::KeyEvent(key) => ClientMsg::KeyEvent(key.keycode, key.down),
                X11Event::PointerEvent(mouse) => {
                    ClientMsg::PointerEvent(mouse.position_x, mouse.position_y, mouse.bottons)
                }
                X11Event::CopyText(text) => self.clipboard.copy(text)?,
            };
            Ok(msg)
        }
    }

    async fn recv_event(&mut self) -> Result<VncEvent, VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            match self.output_ch.recv().await {
                Some(e) => Ok(e),
                None => {
                    self.closed = true;
                    Err(VncError::ClientNotRunning)
                }
            }
        }
    }

    async fn poll_event(&mut self) -> Result<Option<VncEvent>, VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            match self.output_ch.try_recv() {
                Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    Err(VncError::ClientNotRunning)
                }
                Err(TryRecvError::Empty) => Ok(None),
                Ok(e) => Ok(Some(e)),
            }
            // Ok(self.output_ch.recv().await)
        }
    }

    /// Stop the VNC engine and release resources
    ///
    fn close(&mut self) -> Result<(), VncError> {
        self.clipboard.set_permissions(false, false);
        self.desktop.close();
        if self.net_conn_stop.is_some() {
            let net_conn_stop: oneshot::Sender<()> = self.net_conn_stop.take().unwrap();
            let _ = net_conn_stop.send(());
        }
        if self.decoding_stop.is_some() {
            let decoding_stop = self.decoding_stop.take().unwrap();
            let _ = decoding_stop.send(());
        }
        self.closed = true;
        Ok(())
    }
}

impl Drop for VncInner {
    fn drop(&mut self) {
        info!("VNC Client {} stops", self.name);
        let _ = self.close();
    }
}

/// Shared permission control, independent of the event receiver and socket locks.
#[derive(Clone)]
pub struct ClipboardPolicy(Arc<crate::clipboard::Clipboard>);
impl ClipboardPolicy {
    pub fn set(&self, send: bool, receive: bool) {
        self.0.set_permissions(send, receive);
    }
}

pub struct VncClient {
    security: crate::SecurityInfo,
    inner: Arc<Mutex<VncInner>>,
    desktop: Arc<DesktopState>,
    flow: Arc<crate::flow::Flow>,
    clipboard: Arc<crate::clipboard::Clipboard>,
    input_ch: Sender<ClientMsg>,
}

impl VncClient {
    #[cfg(test)]
    pub(super) async fn new<S>(
        stream: S,
        shared: bool,
        pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::new_with_options(stream, shared, pixel_format, encodings, false, true).await
    }
    pub(super) async fn new_with_options<S>(
        stream: S,
        shared: bool,
        pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
        tight: bool,
        utf8_clipboard: bool,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let inner = VncInner::new(
            stream,
            shared,
            pixel_format,
            encodings,
            tight,
            utf8_clipboard,
        )
        .await?;
        Ok(Self {
            security: crate::SecurityInfo::VncAuth,
            desktop: Arc::clone(&inner.desktop),
            flow: Arc::clone(&inner.flow),
            clipboard: Arc::clone(&inner.clipboard),
            input_ch: inner.input_ch.clone(),
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    pub(crate) fn with_security(mut self, security: crate::SecurityInfo) -> Self {
        self.security = security;
        self
    }
    pub fn security(&self) -> crate::SecurityInfo {
        self.security
    }

    pub fn clipboard_policy(&self) -> ClipboardPolicy {
        ClipboardPolicy(Arc::clone(&self.clipboard))
    }

    /// Control automatic Extended Clipboard replies as well as explicit offers.
    /// Disabling send clears cached local clipboard ownership.
    pub fn set_clipboard_permissions(&self, send: bool, receive: bool) {
        self.clipboard.set_permissions(send, receive);
    }

    /// Last validated ExtendedDesktopSize layout; None means support is unconfirmed.
    pub fn desktop_layout(&self) -> Option<crate::DesktopLayout> {
        self.desktop.layout()
    }

    /// Request a negotiated single-screen resize and wait for the server result.
    /// Available on native targets with a Tokio time driver; wasm supports observation only.
    /// Continue draining events concurrently. Each queue/confirmation wait is bounded
    /// to five seconds. Cancellation after dispatch makes later requests uncertain;
    /// reconnect and observe before retrying. Dispatch includes one incremental one-pixel
    /// update request so servers can deliver the confirmation.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn resize_desktop(
        &self,
        width: u16,
        height: u16,
    ) -> Result<crate::DesktopLayout, crate::ResizeError> {
        self.desktop.request(&self.input_ch, width, height).await
    }

    /// Input a `X11Event` from the frontend
    ///
    pub fn pending_updates(&self) -> u8 {
        self.flow.pending()
    }
    pub async fn input(&self, mut event: X11Event) -> Result<(), VncError> {
        let sender = {
            let inner = self.inner.lock().await;
            if inner.closed {
                return Err(VncError::ClientNotRunning);
            }
            inner.input_ch.clone()
        };
        // An already pending refresh needs no queue slot. Remember an explicit
        // full refresh before checking pending, so completion cannot lose it.
        if matches!(event, X11Event::FullRefresh) {
            self.flow.mark_full();
        }
        if matches!(event, X11Event::Refresh | X11Event::FullRefresh) && self.flow.pending() != 0 {
            return Ok(());
        }
        // Do not hold the client mutex while backpressure waits: close needs it.
        let permit = sender.reserve().await?;
        let inner = self.inner.lock().await;
        if matches!(event, X11Event::Refresh | X11Event::FullRefresh) {
            let Some(full) = self.flow.request(matches!(event, X11Event::FullRefresh)) else {
                return Ok(());
            };
            event = if full {
                X11Event::FullRefresh
            } else {
                X11Event::Refresh
            };
        }

        permit.send(inner.input_message(event)?);
        Ok(())
    }

    /// Receive a `VncEvent` from the engine
    /// This function will block until a `VncEvent` is received
    ///
    pub async fn recv_event(&self) -> Result<VncEvent, VncError> {
        let event = self.inner.lock().await.recv_event().await?;
        if matches!(event, VncEvent::FramebufferUpdateComplete(_)) && self.flow.needs_followup() {
            self.input(if self.flow.has_full() {
                X11Event::FullRefresh
            } else {
                X11Event::Refresh
            })
            .await?;
        }
        Ok(event)
    }

    /// polling `VncEvent` from the engine and give it to the client
    ///
    pub async fn poll_event(&self) -> Result<Option<VncEvent>, VncError> {
        let event = self.inner.lock().await.poll_event().await?;
        if matches!(event, Some(VncEvent::FramebufferUpdateComplete(_)))
            && self.flow.needs_followup()
        {
            self.input(if self.flow.has_full() {
                X11Event::FullRefresh
            } else {
                X11Event::Refresh
            })
            .await?;
        }
        Ok(event)
    }

    /// Wait until earlier input has reached the transport writer. This is not
    /// a remote acknowledgement, but prevents close from discarding key-ups.
    pub async fn flush_input(&self) -> Result<(), VncError> {
        let (done, finished) = oneshot::channel();
        self.input_ch.send(ClientMsg::Flush(done)).await?;
        finished.await.map_err(|_| VncError::ClientNotRunning)
    }

    /// Stop the VNC engine and release resources
    ///
    pub async fn close(&self) -> Result<(), VncError> {
        self.inner.lock().await.close()
    }
}

impl Clone for VncClient {
    fn clone(&self) -> Self {
        Self {
            security: self.security,
            inner: self.inner.clone(),
            desktop: Arc::clone(&self.desktop),
            flow: Arc::clone(&self.flow),
            clipboard: Arc::clone(&self.clipboard),
            input_ch: self.input_ch.clone(),
        }
    }
}

async fn send_client_init<S>(stream: &mut S, shared: bool) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    trace!("Send shared flag: {}", shared);
    stream.write_u8(shared as u8).await?;
    Ok(())
}

async fn read_server_init<S, F, Fut>(
    stream: &mut S,
    pf: &mut Option<PixelFormat>,
    output_func: &F,
) -> Result<(String, (u16, u16)), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    // +--------------+--------------+------------------------------+
    // | No. of bytes | Type [Value] | Description                  |
    // +--------------+--------------+------------------------------+
    // | 2            | U16          | framebuffer-width in pixels  |
    // | 2            | U16          | framebuffer-height in pixels |
    // | 16           | PIXEL_FORMAT | server-pixel-format          |
    // | 4            | U32          | name-length                  |
    // | name-length  | U8 array     | name-string                  |
    // +--------------+--------------+------------------------------+

    let screen_width = stream.read_u16().await?;
    let screen_height = stream.read_u16().await?;
    crate::limits::dimensions(screen_width, screen_height)?;
    let mut send_our_pf = false;

    output_func(VncEvent::SetResolution(
        (screen_width, screen_height).into(),
    ))
    .await?;

    let pixel_format = PixelFormat::read(stream).await?;
    if pf.is_none() {
        output_func(VncEvent::SetPixelFormat(pixel_format)).await?;
        let _ = pf.insert(pixel_format);
    } else {
        send_our_pf = true;
    }

    let name = crate::limits::string(stream, crate::limits::MAX_NAME).await?;

    if send_our_pf {
        trace!("Send customized pixel format {:#?}", pf);
        ClientMsg::SetPixelFormat(*pf.as_ref().unwrap())
            .write(stream)
            .await?;
    }
    Ok((name, (screen_width, screen_height)))
}

async fn send_client_encoding<S>(
    stream: &mut S,
    encodings: Vec<VncEncoding>,
) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    ClientMsg::SetEncodings(encodings).write(stream).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn asycn_vnc_read_loop_flow<S, F, Fut>(
    stream: &mut S,
    pf: &PixelFormat,
    output_func: &F,
    stop_ch: &mut oneshot::Receiver<()>,
    encodings: &[VncEncoding],
    screen: &AtomicU32,
    desktop: &DesktopState,
    flow: &crate::flow::Flow,
    clipboard: &crate::clipboard::Clipboard,
) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    tokio::select! {
        biased;
        _ = stop_ch => Ok(()),
        result = read_vnc_messages_flow(stream, pf, output_func, encodings, screen, desktop, flow, clipboard) => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_vnc_messages_flow<S, F, Fut>(
    stream: &mut S,
    pf: &PixelFormat,
    output_func: &F,
    encodings: &[VncEncoding],
    shared_screen: &AtomicU32,
    desktop: &DesktopState,
    flow: &crate::flow::Flow,
    clipboard: &crate::clipboard::Clipboard,
) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    let mut counted = crate::flow::CountingRead {
        inner: stream,
        bytes: 0,
    };
    let stream = &mut counted;
    let mut sequence = 0;
    let mut raw_decoder = codec::RawDecoder::new();
    let mut zrle_decoder = codec::ZrleDecoder::new();
    let mut tight_decoder = codec::TightDecoder::new();
    let mut trle_decoder = codec::TrleDecoder::new();
    let mut cursor = codec::CursorDecoder::new();
    let mut screen = unpack_screen(shared_screen.load(Ordering::Acquire));

    // main decoding loop
    loop {
        let start_bytes = stream.bytes;
        let server_msg = ServerMsg::read(stream).await?;
        // Never trace server messages: CutText and Fence payloads can carry private text.
        match server_msg {
            ServerMsg::FramebufferUpdate(rect_num) => {
                sequence += 1;
                let mut stats = crate::UpdateStats {
                    sequence,
                    ..Default::default()
                };
                let mut updates = crate::desktop::UpdateBatch::default();
                let mut framebuffer_changed = false;
                for _ in 0..rect_num {
                    let rect = ImageRect::read(stream).await?;
                    stats.rectangles += 1;
                    let code = rect.encoding as i32;
                    if let Some((_, count)) = stats
                        .encodings
                        .iter_mut()
                        .find(|(encoding, _)| *encoding == code)
                    {
                        *count += 1;
                    } else {
                        stats.encodings.push((code, 1));
                    }
                    if matches!(
                        rect.encoding,
                        VncEncoding::Raw
                            | VncEncoding::CopyRect
                            | VncEncoding::Zrle
                            | VncEncoding::Trle
                            | VncEncoding::Tight
                    ) {
                        stats.pixels += u64::from(rect.rect.width) * u64::from(rect.rect.height);
                    }
                    if rect.encoding != VncEncoding::Raw && !encodings.contains(&rect.encoding) {
                        return Err(VncError::InvalidImageData);
                    }
                    if !matches!(
                        rect.encoding,
                        VncEncoding::DesktopSizePseudo
                            | VncEncoding::ExtendedDesktopSizePseudo
                            | VncEncoding::LastRectPseudo
                            | VncEncoding::CursorPseudo
                    ) {
                        crate::limits::rectangle(&rect.rect, screen)?;
                    }
                    if !matches!(
                        rect.encoding,
                        VncEncoding::CursorPseudo
                            | VncEncoding::ExtendedDesktopSizePseudo
                            | VncEncoding::LastRectPseudo
                    ) {
                        if !updates.is_empty() {
                            return Err(VncError::InvalidImageData);
                        }
                        framebuffer_changed = true;
                    }

                    match rect.encoding {
                        VncEncoding::Raw => {
                            raw_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::CopyRect => {
                            let source_x = stream.read_u16().await?;
                            let source_y = stream.read_u16().await?;
                            let mut src_rect = rect.rect;
                            src_rect.x = source_x;
                            src_rect.y = source_y;
                            crate::limits::rectangle(&src_rect, screen)?;
                            output_func(VncEvent::Copy(rect.rect, src_rect)).await?;
                        }
                        VncEncoding::Tight => {
                            tight_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::Trle => {
                            trle_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::Zrle => {
                            zrle_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::CursorPseudo => {
                            cursor.decode(pf, &rect.rect, stream, output_func).await?;
                        }
                        VncEncoding::ExtendedDesktopSizePseudo => {
                            // Confirm only after the entire message excludes framebuffer changes.
                            // Cursor metadata may accompany desktop layout updates.
                            if framebuffer_changed {
                                return Err(VncError::InvalidImageData);
                            }
                            updates.push(crate::DesktopUpdate::read(stream, rect.rect).await?)?;
                        }
                        VncEncoding::DesktopSizePseudo => {
                            crate::limits::dimensions(rect.rect.width, rect.rect.height)?;
                            if rect.rect.x != 0 || rect.rect.y != 0 {
                                return Err(VncError::InvalidImageData);
                            }
                            screen = (rect.rect.width, rect.rect.height);
                            shared_screen.store(pack_screen(screen), Ordering::Release);
                            desktop.legacy_resize();
                            output_func(VncEvent::SetResolution(
                                (rect.rect.width, rect.rect.height).into(),
                            ))
                            .await?;
                        }
                        VncEncoding::LastRectPseudo => {
                            break;
                        }
                        _ => return Err(VncError::InvalidImageData),
                    }
                }
                for update in updates.into_updates() {
                    if let Some(layout) = &update.layout {
                        screen = (layout.width, layout.height);
                        shared_screen.store(pack_screen(screen), Ordering::Release);
                    }
                    desktop.observe(&update);
                    output_func(VncEvent::DesktopUpdate(update)).await?;
                }
                stats.wire_bytes = stream.bytes - start_bytes;
                flow.complete();
                output_func(VncEvent::FramebufferUpdateComplete(stats)).await?;
            }
            ServerMsg::Fence(flags, payload) => {
                if !encodings.contains(&VncEncoding::FencePseudo) {
                    return Err(VncError::InvalidImageData);
                }
                if !clipboard.fence.swap(true, Ordering::AcqRel) {
                    output_func(VncEvent::FenceSupported).await?;
                }
                if flags & 0x8000_0000 != 0 {
                    let sender = clipboard.sender.lock().unwrap().clone();
                    if let Some(sender) = sender {
                        sender.send(ClientMsg::Fence(0, payload)).await?;
                    }
                } else if payload.len() == 8 {
                    output_func(VncEvent::FenceReply(u64::from_be_bytes(
                        payload.try_into().unwrap(),
                    )))
                    .await?;
                }
            }
            // SetColorMapEntries,
            ServerMsg::Bell => {
                output_func(VncEvent::Bell).await?;
            }
            ServerMsg::ExtendedCutText(payload) => {
                if !encodings.contains(&VncEncoding::ExtendedClipboard) {
                    return Err(VncError::InvalidImageData);
                }
                let (text, capability) = clipboard.receive(&payload).await?;
                if let Some(utf8) = capability {
                    output_func(VncEvent::ClipboardCapabilities { utf8 }).await?;
                }
                if let Some(text) = text {
                    output_func(VncEvent::Text(text)).await?;
                }
            }
            ServerMsg::TightCutText(text) => {
                if !clipboard.tight_utf8.load(Ordering::Acquire) {
                    return Err(VncError::WrongServerMessage);
                }
                output_func(VncEvent::Text(clipboard.receipt(text))).await?;
            }
            ServerMsg::ServerCutText(text) => {
                output_func(VncEvent::Text(clipboard.receipt(text))).await?;
            }
        }
    }
}

async fn async_connection_process_loop<S>(
    mut stream: S,
    mut input_ch: Receiver<ClientMsg>,
    conn_ch: Sender<std::io::Result<Vec<u8>>>,
    mut stop_ch: oneshot::Receiver<()>,
) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut buffer = [0; 65535];
    let mut pending = 0;

    loop {
        tokio::select! {
            _ = &mut stop_ch => break,
            _ = conn_ch.closed() => break,
            permit = conn_ch.reserve(), if pending > 0 => {
                match permit {
                    Ok(permit) => {
                        permit.send(Ok(buffer[..pending].to_vec()));
                        pending = 0;
                    }
                    Err(_) => break,
                }
            }
            result = stream.read(&mut buffer), if pending == 0 => {
                match result {
                    Ok(0) | Err(_) => break,
                    Ok(length) => pending = length,
                }
            }
            message = input_ch.recv() => {
                let Some(message) = message else { break; };
                tokio::select! {
                    biased;
                    _ = &mut stop_ch => break,
                    result = message.write(&mut stream) => result?,
                }
            }
        }
    }
    // Dropping the bridge signals EOF without blocking shutdown on a full queue.
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod queue_tests;

#[cfg(test)]
mod resize_tests;

#[cfg(test)]
async fn asycn_vnc_read_loop<S, F, Fut>(
    stream: &mut S,
    pf: &PixelFormat,
    output_func: &F,
    stop_ch: &mut oneshot::Receiver<()>,
    encodings: &[VncEncoding],
    screen: &AtomicU32,
    desktop: &DesktopState,
) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    asycn_vnc_read_loop_flow(
        stream,
        pf,
        output_func,
        stop_ch,
        encodings,
        screen,
        desktop,
        &crate::flow::Flow::default(),
        &crate::clipboard::Clipboard::default(),
    )
    .await
}
