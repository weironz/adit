//! Cancellable VNC transport. Decoder reception and input run in one select
//! loop; rendering samples a bounded shared surface independently.
use crate::rdp_client::Surface;
use crate::vnc_input::Pointer;
#[cfg(test)]
use crate::RdpMouseButton;
use crate::{RdpClientEvent, RdpFrame, RdpInput};
use adit_domain::{VncOptions, VncQuality, VncSecurity};
use adit_ssh::{LiveShellCommand, LiveShellEvent, LiveShellRequest};
use std::collections::{HashSet, VecDeque};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, Notify};
use vnc::{PixelFormat, Rect, VncClient, VncConnector, VncEncoding, VncError, VncEvent, X11Event};
use zeroize::Zeroizing;

#[derive(Clone, Debug, Default)]
pub struct VncCursor {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
    pub generation: u64,
}

/// Metadata only: never stores a password, key, clipboard body, or input text.
#[derive(Clone, Debug, Default)]
pub struct VncDiagnostics {
    pub tcp_ms: u64,
    pub handshake_ms: u64,
    pub first_pixel_ms: Option<u64>,
    pub first_complete_ms: Option<u64>,
    pub complete_updates: u64,
    pub wire_bytes: u64,
    pub pending_updates: u8,
    pub fence_rtt_ms: Option<u64>,
    /// Rectangle area can overlap; do not interpret this as unique coverage.
    pub painted_pixels: u64,
    pub covered_pixels: u64,
    pub framebuffer_pixels: u64,
    pub first_covered_ms: Option<u64>,
    pub sampled_generation: u64,
    pub queue_high_water: usize,
    pub security: String,
    pub encoding: String,
    pub clipboard_utf8: bool,
    pub status: String,
}

struct SharedState {
    options: VncOptions,
    cursor: Option<VncCursor>,
    diagnostics: VncDiagnostics,
    accepting_input: bool,
    clipboard_enabled: bool,
    clipboard_policy: Option<vnc::ClipboardPolicy>,
}

impl SharedState {
    fn apply_clipboard_policy(&self) {
        if let Some(policy) = &self.clipboard_policy {
            policy.set(
                self.clipboard_enabled && !self.options.read_only && self.options.clipboard_send,
                self.clipboard_enabled && self.options.clipboard_receive,
            );
        }
    }
}

pub(super) struct VncClientHandle {
    commands: Arc<CommandQueue>,
    gateway: mpsc::UnboundedSender<LiveShellCommand>,
    stop: watch::Sender<bool>,
    events: std_mpsc::Receiver<RdpClientEvent>,
    errors: std_mpsc::Sender<RdpClientEvent>,
    surface: Arc<Mutex<Surface>>,
    state: Arc<Mutex<SharedState>>,
}

enum Command {
    Input(RdpInput),
    Key { keysym: u32, pressed: bool },
    ReadOnly(bool),
    ResizeRemote(bool),
    Quality(VncQuality),
}

// Coalesce only adjacent motion. A discrete input seals its preceding coordinates.
#[derive(Default)]
struct CommandQueue {
    pending: Mutex<VecDeque<Command>>,
    ready: Notify,
}
impl CommandQueue {
    fn push(&self, command: Command) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if matches!(command, Command::Input(RdpInput::MouseMove { .. }))
            && matches!(
                pending.back(),
                Some(Command::Input(RdpInput::MouseMove { .. }))
            )
        {
            *pending.back_mut().unwrap() = command;
        } else {
            if pending.len() >= 256 {
                return false;
            }
            pending.push_back(command);
        }
        drop(pending);
        self.ready.notify_one();
        true
    }
    async fn recv(&self) -> Command {
        loop {
            let ready = self.ready.notified();
            if let Some(command) = self.pending.lock().unwrap().pop_front() {
                return command;
            }
            ready.await;
        }
    }
}
impl VncClientHandle {
    fn send(&self, command: Command) {
        if !self.commands.push(command) {
            let _ = self.errors.send(RdpClientEvent::Error(
                "VNC 输入队列已满，连接已关闭，请重新连接".into(),
            ));
            self.close();
        }
        let size = self.commands.pending.lock().unwrap().len();
        let mut state = self.state.lock().unwrap();
        state.diagnostics.queue_high_water = state.diagnostics.queue_high_water.max(size);
    }
    pub(super) fn send_input(&self, input: RdpInput) {
        let state = self.state.lock().unwrap();
        let allowed = match &input {
            RdpInput::Resize { .. } => state.options.resize_remote,
            RdpInput::ClipboardText(_) => {
                !state.options.read_only
                    && state.options.clipboard_send
                    && state.clipboard_enabled
                    && state.accepting_input
            }
            _ => !state.options.read_only && state.accepting_input,
        };
        drop(state);
        if allowed {
            self.send(Command::Input(input));
        }
    }
    pub(super) fn send_key(&self, keysym: u32, pressed: bool) {
        let state = self.state.lock().unwrap();
        let allowed = !state.options.read_only && state.accepting_input;
        drop(state);
        if allowed {
            self.send(Command::Key { keysym, pressed });
        }
    }
    pub(super) fn send_gateway(&self, command: LiveShellCommand) {
        let _ = self.gateway.send(command);
    }
    pub(super) fn read_only(&self) -> bool {
        self.state.lock().unwrap().options.read_only
    }
    pub(super) fn set_read_only(&self, enabled: bool) {
        {
            let mut state = self.state.lock().unwrap();
            state.options.read_only = enabled;
            state.apply_clipboard_policy();
        }
        self.send(Command::ReadOnly(enabled));
    }
    pub(super) fn set_clipboard_sharing(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap();
        state.clipboard_enabled = enabled;
        state.apply_clipboard_policy();
    }
    pub(super) fn resize_remote(&self) -> bool {
        self.state.lock().unwrap().options.resize_remote
    }
    pub(super) fn set_resize_remote(&self, enabled: bool) {
        self.state.lock().unwrap().options.resize_remote = enabled;
        self.send(Command::ResizeRemote(enabled));
    }
    pub(super) fn set_quality(&self, quality: VncQuality) {
        self.state.lock().unwrap().options.quality = quality;
        self.send(Command::Quality(quality));
    }
    pub(super) fn cursor(&self) -> Option<VncCursor> {
        self.state.lock().unwrap().cursor.clone()
    }
    pub(super) fn diagnostics(&self) -> VncDiagnostics {
        self.state.lock().unwrap().diagnostics.clone()
    }
    pub(super) fn close(&self) {
        let _ = self.stop.send(true);
    }
    pub(super) fn try_recv(&self) -> Option<RdpClientEvent> {
        self.events.try_recv().ok()
    }
    pub(super) fn frame_if_newer(&self, generation: u64) -> Option<RdpFrame> {
        let surface = self.surface.lock().ok()?;
        (surface.generation != generation).then(|| {
            self.state.lock().unwrap().diagnostics.sampled_generation = surface.generation;
            RdpFrame {
                rgba: surface.rgba.clone(),
                width: surface.width,
                height: surface.height,
                generation: surface.generation,
            }
        })
    }
}
impl Drop for VncClientHandle {
    fn drop(&mut self) {
        self.close();
    }
}

pub(super) fn spawn_vnc_session(
    host: String,
    port: u16,
    password: String,
    options: VncOptions,
    gateway: Option<LiveShellRequest>,
) -> std::io::Result<VncClientHandle> {
    let commands = Arc::new(CommandQueue::default());
    let (stop, stopped) = watch::channel(false);
    let (events, event_rx) = std_mpsc::channel();
    let errors = events.clone();
    let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
    let state = Arc::new(Mutex::new(SharedState {
        options,
        cursor: None,
        diagnostics: VncDiagnostics::default(),
        accepting_input: true,
        clipboard_enabled: true,
        clipboard_policy: None,
    }));
    let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
    let worker_errors = errors.clone();
    let worker = Worker {
        commands: Arc::clone(&commands),
        surface: Arc::clone(&surface),
        state: Arc::clone(&state),
        stopped,
        events,
    };
    std::thread::Builder::new()
        .name("adit-vnc".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(VncError::IoError)
                .and_then(|rt| {
                    rt.block_on(worker.run(
                        host,
                        port,
                        Zeroizing::new(password),
                        gateway,
                        gateway_rx,
                    ))
                });
            if let Err(error) = result {
                let message = error_message(&error);
                let event = if is_auth_error(&error) {
                    RdpClientEvent::AuthRejected(message)
                } else {
                    RdpClientEvent::Error(message)
                };
                let _ = worker_errors.send(event);
            }
            let _ = worker_errors.send(RdpClientEvent::Closed);
        })?;
    Ok(VncClientHandle {
        commands,
        gateway: gateway_tx,
        stop,
        events: event_rx,
        errors,
        surface,
        state,
    })
}

fn is_auth_error(error: &VncError) -> bool {
    matches!(error, VncError::WrongPassword | VncError::NoPassword)
}
fn error_message(error: &VncError) -> String {
    match error {
        VncError::WrongPassword | VncError::NoPassword => {
            "VNC 密码认证失败，请重新输入 VNC 密码".into()
        }
        VncError::InvalidSecurityTyep(_) => "VNC 服务器的认证方式暂不支持".into(),
        VncError::SecurityPolicy(message) => format!("VNC 安全策略不满足: {message}"),
        VncError::Tls(message) => format!("VNC TLS/证书验证失败: {message}"),
        _ => format!("VNC 连接失败: {error}"),
    }
}

trait DesktopStream: Sync + tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: Sync + tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> DesktopStream for T {}
struct Worker {
    commands: Arc<CommandQueue>,
    surface: Arc<Mutex<Surface>>,
    state: Arc<Mutex<SharedState>>,
    stopped: watch::Receiver<bool>,
    events: std_mpsc::Sender<RdpClientEvent>,
}
impl Worker {
    async fn run(
        mut self,
        host: String,
        port: u16,
        password: Zeroizing<String>,
        gateway: Option<LiveShellRequest>,
        mut gateway_commands: mpsc::UnboundedReceiver<LiveShellCommand>,
    ) -> Result<(), VncError> {
        let mut ever_connected = false;
        let mut attempts = 0_u32;
        loop {
            let start = Instant::now();
            let (ssh_events, ssh_rx) = std_mpsc::channel();
            // One gateway command receiver is moved into each handshake. On
            // reconnect it is replaced; forwarding decisions remains on the main
            // worker receiver, so credentials and trust are never reused blindly.
            let (handshake_tx, handshake_rx) = mpsc::unbounded_channel();
            let options = {
                let mut state = self.state.lock().unwrap();
                state.diagnostics = VncDiagnostics::default();
                state.options.clone()
            };
            let connect = connect(
                &host,
                port,
                password.to_string(),
                &options,
                gateway.clone(),
                handshake_rx,
                ssh_events,
                Arc::clone(&self.state),
            );
            tokio::pin!(connect);
            let client_result = loop {
                tokio::select! {
                    biased;
                    _ = self.stopped.changed() => return Ok(()),
                    command = gateway_commands.recv() => {
                        if let Some(command) = command { let _ = handshake_tx.send(command); }
                    }
                    result = &mut connect => break result,
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        for event in ssh_rx.try_iter() {
                            match event {
                                LiveShellEvent::HostKeyPrompt(p) => { let _ = self.events.send(RdpClientEvent::GatewayHostKey(p)); }
                                LiveShellEvent::AuthPrompt(p) => { let _ = self.events.send(RdpClientEvent::GatewayAuthPrompt(p)); }
                                _ => {}
                            }
                        }
                    }
                }
            };
            let result = match client_result {
                Ok(client) => {
                    ever_connected = true;
                    self.state.lock().unwrap().accepting_input = true;
                    let result = self.pump(&client, start).await;
                    self.state.lock().unwrap().clipboard_policy = None;
                    let _ = client.close().await;
                    result
                }
                Err(error) => Err(error),
            };
            if *self.stopped.borrow() {
                return Ok(());
            }
            let error = match result {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            let transient = matches!(&error, VncError::IoError(_) | VncError::ClientNotRunning)
                || matches!(&error, VncError::General(m) if m == "Channel closed");
            if !ever_connected || !options.auto_reconnect || !transient || attempts >= 10 {
                return Err(error);
            }
            if start.elapsed() > Duration::from_secs(30) {
                attempts = 0;
            }
            let delay = Duration::from_secs((1_u64 << attempts.min(5)).min(30));
            attempts += 1;
            {
                let mut state = self.state.lock().unwrap();
                state.accepting_input = false;
                state.cursor = None;
                state.diagnostics.status =
                    format!("连接中断，{} 秒后重试（{attempts}/10）", delay.as_secs());
            }
            self.commands.pending.lock().unwrap().clear();
            // Clear stale pixels/clipboard ownership instead of presenting a
            // previous connection as an actively updating desktop.
            resize(&self.surface, 1, 1)?;
            let _ = self.events.send(RdpClientEvent::VncReconnecting {
                attempt: attempts,
                delay_secs: delay.as_secs(),
            });
            tokio::select! { _ = self.stopped.changed() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
        }
    }

    async fn pump(&mut self, client: &VncClient, start: Instant) -> Result<(), VncError> {
        {
            let mut state = self.state.lock().unwrap();
            state.clipboard_policy = Some(client.clipboard_policy());
            state.apply_clipboard_policy();
        }
        let mut pointer = Pointer::default();
        let mut keys = HashSet::new();
        let mut connected = false;
        let mut fence_supported = false;
        let mut fence_pending: Option<(u64, Instant)> = None;
        let mut next_fence = Instant::now();
        let mut fence_sequence = 0_u64;
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut resize_task: Option<
            tokio::task::JoinHandle<Result<vnc::DesktopLayout, vnc::ResizeError>>,
        > = None;
        let mut requested_size = None;
        let result = async {
        loop {
            tokio::select! {
                _ = self.stopped.changed() => {
                    return Ok(());
                }
                command = self.commands.recv() => {
                    let inputs = match command {
                        Command::ReadOnly(enabled) => {
                            {
                                let state = self.state.lock().unwrap();
                                state.apply_clipboard_policy();
                            }
                            if enabled { release(client, &mut pointer, &mut keys).await; }
                            vec![]
                        }
                        Command::ResizeRemote(enabled) => {
                            if !enabled { requested_size = None; }
                            vec![]
                        }
                        Command::Quality(quality) => {
                            // The next connection carries the chosen codec policy.
                            self.state.lock().unwrap().diagnostics.status = format!("画质已设为 {quality:?}，重连后生效");
                            vec![]
                        }
                        Command::Input(RdpInput::Resize { width, height }) => {
                            if self.state.lock().unwrap().options.resize_remote { requested_size = Some((width, height)); }
                            vec![]
                        }
                        Command::Key { keysym, pressed } => {
                            vec![X11Event::KeyEvent((keysym, pressed).into())]
                        }
                        Command::Input(input) => pointer.input(input),
                    };
                    for input in inputs {
                        let key_transition = if let X11Event::KeyEvent(ref key) = input { Some((key.keycode,key.down)) } else { None };
                        match self.send_event(client,input).await {
                            Err(VncError::ClipboardUnsupported) => {
                                let notice = "未发送剪贴板：文本包含对端不支持的字符、NUL，或超过大小限制".to_owned();
                                self.state.lock().unwrap().diagnostics.status = notice.clone();
                                let _ = self.events.send(RdpClientEvent::Notice(notice));
                            },
                            result => result?,
                        }
                        if let Some((key,pressed)) = key_transition {
                            if pressed { keys.insert(key); } else { keys.remove(&key); }
                        }
                    }
                }
                event = client.recv_event() => {
                    match event? {
                        VncEvent::SetResolution(screen) => {
                            resize(&self.surface, screen.width, screen.height)?;
                            {
                                let mut state = self.state.lock().unwrap();
                                state.diagnostics.covered_pixels = 0;
                                state.diagnostics.framebuffer_pixels = u64::from(screen.width) * u64::from(screen.height);
                                state.diagnostics.first_covered_ms = None;
                            }
                            let event = if connected { RdpClientEvent::Resized {width:screen.width,height:screen.height} }
                                else { connected=true; RdpClientEvent::Connected {width:screen.width,height:screen.height} };
                            let _ = self.events.send(event);
                        }
                        VncEvent::RawImage(rect, mut rgba) => {
                            for pixel in rgba.as_chunks_mut::<4>().0 { pixel[3] = 255; }
                            self.paint(rect, &rgba, start);
                        }
                        VncEvent::JpegImage(rect, data) => {
                            let rgba = decode_jpeg(rect, data)?;
                            self.paint(rect, &rgba, start);
                        }
                        VncEvent::Copy(dst,src) => copy_rect(&self.surface,dst,src)?,
                        VncEvent::SetCursor(rect, rgba) => {
                            let valid = rgba.len() == usize::from(rect.width)*usize::from(rect.height)*4
                                && rect.width<=512 && rect.height<=512
                                && (rect.width==0 || rect.x<rect.width) && (rect.height==0 || rect.y<rect.height);
                            if !valid { return Err(VncError::InvalidImageData); }
                            let mut state = self.state.lock().unwrap();
                            let generation = state.cursor.as_ref().map_or(1, |c| c.generation.wrapping_add(1));
                            state.cursor=Some(VncCursor {width:rect.width,height:rect.height,hotspot_x:rect.x,hotspot_y:rect.y,rgba,generation});
                        }
                        VncEvent::Text(text) => {
                            let receive = { let state = self.state.lock().unwrap(); state.clipboard_enabled && state.options.clipboard_receive };
                            if receive {
                                let _ = self.events.send(RdpClientEvent::ClipboardText(text));
                            }
                        }
                        VncEvent::FenceSupported => { fence_supported = true; }
                        VncEvent::FenceReply(sequence) => {
                            if let Some((expected, sent)) = fence_pending {
                                if sequence == expected {
                                    self.state.lock().unwrap().diagnostics.fence_rtt_ms = Some(sent.elapsed().as_millis() as u64);
                                    fence_pending = None;
                                }
                            }
                        }
                        VncEvent::TransportClosed(message) => return Err(VncError::IoError(std::io::Error::new(std::io::ErrorKind::ConnectionReset,message))),
                        VncEvent::FramebufferUpdateComplete(stats) => {
                            // Scan only until initial complete coverage. Repeated updates
                            // thereafter keep the refresh path independent of frame size.
                            let needs_coverage = self.state.lock().unwrap().diagnostics.first_covered_ms.is_none();
                            let coverage = needs_coverage.then(|| {
                                let surface = self.surface.lock().unwrap();
                                (surface.rgba.as_chunks::<4>().0.iter().filter(|pixel| pixel[3] == 255).count() as u64,
                                 u64::from(surface.width) * u64::from(surface.height))
                            });
                            let mut state=self.state.lock().unwrap();
                            if let Some((covered,total)) = coverage {
                                state.diagnostics.covered_pixels = covered;
                                state.diagnostics.framebuffer_pixels = total;
                                if covered == total { state.diagnostics.first_covered_ms = Some(start.elapsed().as_millis() as u64); }
                            }
                            state.diagnostics.complete_updates+=1;
                            state.diagnostics.wire_bytes += stats.wire_bytes;
                            state.diagnostics.encoding = format!("{:?}", stats.encodings);
                            state.diagnostics.first_complete_ms.get_or_insert(start.elapsed().as_millis() as u64);
                            state.diagnostics.status="已连接".into();
                        }
                        VncEvent::ClipboardCapabilities {utf8} => self.state.lock().unwrap().diagnostics.clipboard_utf8=utf8,
                        VncEvent::DesktopUpdate(update) => {
                            if let Some(layout) = update.layout {
                                let different=self.surface.lock().is_ok_and(|s| s.width!=layout.width || s.height!=layout.height);
                                if different { resize(&self.surface,layout.width,layout.height)?;
                                    { let mut state = self.state.lock().unwrap(); state.diagnostics.first_covered_ms = None; state.diagnostics.covered_pixels = 0; state.diagnostics.framebuffer_pixels = u64::from(layout.width)*u64::from(layout.height); }
                                    let _ = self.events.send(RdpClientEvent::Resized{width:layout.width,height:layout.height}); }
                            }
                        }
                        VncEvent::Error(message) => return Err(VncError::General(message)),
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    self.send_event(client,X11Event::Refresh).await?;
                    self.state.lock().unwrap().diagnostics.pending_updates = client.pending_updates();
                    if fence_pending.is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(15)) {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut,"VNC Fence heartbeat timeout").into());
                    }
                    if fence_supported && fence_pending.is_none() && Instant::now() >= next_fence {
                        fence_sequence = fence_sequence.wrapping_add(1);
                        self.send_event(client,X11Event::FencePing(fence_sequence)).await?;
                        fence_pending = Some((fence_sequence, Instant::now()));
                        next_fence = Instant::now() + Duration::from_secs(5);
                    }
                    if resize_task.as_ref().is_some_and(|t|t.is_finished()) {
                        if let Some(task)=resize_task.take() {
                            if let Ok(Err(error))=task.await {
                                self.state.lock().unwrap().diagnostics.status=format!("远端分辨率未更改: {error}");
                            }
                        }
                    }
                    if resize_task.is_none() {
                        if let Some((width,height))=requested_size.take() {
                            if client.desktop_layout().is_some() {
                                let client=client.clone();
                                resize_task=Some(tokio::spawn(async move { client.resize_desktop(width,height).await }));
                            } else {
                                self.state.lock().unwrap().diagnostics.status="对端未协商远端分辨率调整，继续本地缩放".into();
                            }
                        }
                    }
                }
            }
        }
        }.await;
        // Every exit releases ownership, including a decoder/protocol failure.
        // Dropping a JoinHandle alone would leave its resize task running.
        if let Some(task) = resize_task.take() {
            task.abort();
        }
        release(client, &mut pointer, &mut keys).await;
        result
    }
    async fn send_event(&mut self, client: &VncClient, event: X11Event) -> Result<(), VncError> {
        tokio::select! {
            _ = self.stopped.changed() => Err(VncError::ClientNotRunning),
            result = tokio::time::timeout(Duration::from_secs(5),client.input(event)) =>
                result.unwrap_or_else(|_|Err(std::io::Error::new(std::io::ErrorKind::TimedOut,"VNC input timeout").into())),
        }
    }
    fn paint(&self, rect: Rect, rgba: &[u8], start: Instant) {
        if let Ok(mut surface) = self.surface.lock() {
            surface.blit(rect.x, rect.y, rect.width, rect.height, rgba);
        }
        let mut state = self.state.lock().unwrap();
        state
            .diagnostics
            .first_pixel_ms
            .get_or_insert(start.elapsed().as_millis() as u64);
        state.diagnostics.painted_pixels += u64::from(rect.width) * u64::from(rect.height);
    }
}

async fn release(client: &VncClient, pointer: &mut Pointer, keys: &mut HashSet<u32>) {
    // One deadline for the complete release batch: close must remain bounded
    // even if a peer stops reading with many physical keys held down.
    let held: Vec<_> = keys.drain().collect();
    let had_buttons = pointer.buttons != 0;
    pointer.buttons = 0;
    let pointer_up = pointer.event(0);
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        for keysym in held {
            let _ = client
                .input(X11Event::KeyEvent((keysym, false).into()))
                .await;
        }
        if had_buttons {
            let _ = client.input(pointer_up).await;
        }
        let _ = client.flush_input().await;
    })
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn connect(
    host: &str,
    port: u16,
    password: String,
    options: &VncOptions,
    gateway: Option<LiveShellRequest>,
    gateway_commands: mpsc::UnboundedReceiver<LiveShellCommand>,
    gateway_events: std_mpsc::Sender<LiveShellEvent>,
    state: Arc<Mutex<SharedState>>,
) -> Result<VncClient, VncError> {
    let start = Instant::now();
    let connect = async {
        let via_ssh = gateway.is_some();
        let stream: Box<dyn DesktopStream> = if let Some(gateway) = gateway {
            let stream = adit_ssh::desktop_gateway::connect(
                gateway,
                host.into(),
                port,
                gateway_commands,
                gateway_events,
            )
            .await
            .map_err(gateway_error)?;
            Box::new(stream)
        } else {
            let tcp = tokio::net::TcpStream::connect((host, port)).await?;
            tcp.set_nodelay(true)?;
            socket2::SockRef::from(&tcp).set_tcp_keepalive(
                &socket2::TcpKeepalive::new()
                    .with_time(Duration::from_secs(15))
                    .with_interval(Duration::from_secs(5)),
            )?;
            Box::new(tcp)
        };
        state.lock().unwrap().diagnostics.tcp_ms = start.elapsed().as_millis() as u64;
        let policy = match options.security {
            VncSecurity::AllowNone => vnc::SecurityPolicy::AllowNone,
            VncSecurity::RequireVncAuth => vnc::SecurityPolicy::RequireVncAuth,
            VncSecurity::RequireTls => vnc::SecurityPolicy::RequireTls,
        };
        let mut connector = VncConnector::new(stream)
            .set_auth_method(async move { Ok(password) })
            .security_policy(policy)
            .tight_quality(2, (options.quality == VncQuality::Wan).then_some(7))
            .add_encoding(VncEncoding::FencePseudo)
            .add_encoding(VncEncoding::Tight)
            .add_encoding(VncEncoding::Zrle)
            .add_encoding(VncEncoding::CursorPseudo)
            .add_encoding(VncEncoding::CopyRect)
            .add_encoding(VncEncoding::Raw)
            .add_encoding(VncEncoding::DesktopSizePseudo)
            .add_encoding(VncEncoding::ExtendedDesktopSizePseudo)
            .add_encoding(VncEncoding::LastRectPseudo)
            .allow_shared(true)
            .set_pixel_format(PixelFormat::rgba());
        if !options.clipboard_legacy {
            connector = connector.add_encoding(VncEncoding::ExtendedClipboard);
        }
        if options.security == VncSecurity::RequireTls {
            let mut tls = vnc::TlsConfig {
                server_name: host.into(),
                ca_certificates_der: vec![],
            };
            if !options.tls_ca_file.trim().is_empty() {
                // Dedicated trust material is not a credential. Read off UI thread.
                let bytes = tokio::fs::read(options.tls_ca_file.trim()).await?;
                tls.ca_certificates_der = certificate_der(&bytes)?;
            }
            connector = connector.tls_config(tls);
        }
        let client = connector.build()?.try_start().await?.finish()?;
        let mut state = state.lock().unwrap();
        state.diagnostics.handshake_ms = start.elapsed().as_millis() as u64;
        state.diagnostics.security = format!(
            "{}{:?}",
            if via_ssh { "SSH + " } else { "" },
            client.security()
        );
        state.diagnostics.encoding = format!("Tight/ZRLE ({:?})", options.quality);
        Ok(client)
    };
    tokio::time::timeout(Duration::from_secs(60), connect)
        .await
        .map_err(|_| {
            VncError::IoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "VNC connect/handshake timeout",
            ))
        })?
}

fn gateway_error(error: adit_ssh::SshError) -> VncError {
    use adit_ssh::SshError;
    match error {
        SshError::Timeout(_) => {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "SSH 网关连接超时").into()
        }
        other if adit_ssh::desktop_gateway::is_transport_error(&other) => {
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "SSH 网关网络连接中断").into()
        }
        other => VncError::SecurityPolicy(format!("SSH 网关连接失败: {other}")),
    }
}

fn certificate_der(bytes: &[u8]) -> Result<Vec<Vec<u8>>, VncError> {
    if bytes.starts_with(b"-----BEGIN") {
        let certs = rustls_pemfile::certs(&mut std::io::Cursor::new(bytes))
            .map(|r| r.map(|c| c.to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(VncError::IoError)?;
        if certs.is_empty() {
            return Err(VncError::Tls(
                "CA 文件未包含 X.509 证书，未回退公共根信任".into(),
            ));
        }
        Ok(certs)
    } else if bytes.is_empty() {
        Err(VncError::Tls("CA 文件为空，未回退公共根信任".into()))
    } else {
        Ok(vec![bytes.to_vec()])
    }
}

fn decode_jpeg(rect: Rect, data: Vec<u8>) -> Result<Vec<u8>, VncError> {
    use image::ImageDecoder;
    let mut decoder = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(data))
        .map_err(|_| VncError::InvalidImageData)?;
    if decoder.dimensions() != (u32::from(rect.width), u32::from(rect.height)) {
        return Err(VncError::InvalidImageData);
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    decoder
        .set_limits(limits)
        .map_err(|_| VncError::InvalidImageData)?;
    Ok(image::DynamicImage::from_decoder(decoder)
        .map_err(|_| VncError::InvalidImageData)?
        .into_rgba8()
        .into_raw())
}

fn resize(surface: &Arc<Mutex<Surface>>, width: u16, height: u16) -> Result<(), VncError> {
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || usize::from(width) * usize::from(height) > 8_294_400
    {
        return Err(VncError::InvalidImageData);
    }
    if let Ok(mut surface) = surface.lock() {
        let generation = surface.generation.wrapping_add(1);
        *surface = Surface::new(width, height);
        surface.generation = generation;
    }
    Ok(())
}

fn copy_rect(surface: &Arc<Mutex<Surface>>, dst: Rect, src: Rect) -> Result<(), VncError> {
    if let Ok(mut surface) = surface.lock() {
        let valid = |rect: Rect| {
            u32::from(rect.x) + u32::from(rect.width) <= u32::from(surface.width)
                && u32::from(rect.y) + u32::from(rect.height) <= u32::from(surface.height)
        };
        if !valid(dst) || !valid(src) || dst.width != src.width || dst.height != src.height {
            return Err(VncError::InvalidImageData);
        }
        // Copy direction matters for overlapping scroll rectangles. Each row's
        // copy_within handles horizontal overlap without a full-frame scratch.
        for n in 0..usize::from(src.height) {
            let row = if dst.y > src.y {
                usize::from(src.height) - 1 - n
            } else {
                n
            };
            let source =
                ((usize::from(src.y) + row) * usize::from(surface.width) + usize::from(src.x)) * 4;
            let target =
                ((usize::from(dst.y) + row) * usize::from(surface.width) + usize::from(dst.x)) * 4;
            surface
                .rgba
                .copy_within(source..source + usize::from(src.width) * 4, target);
        }
        surface.generation = surface.generation.wrapping_add(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_empty_or_non_certificate_ca_never_falls_back_to_public_roots() {
        assert!(certificate_der(b"").is_err());
        assert!(
            certificate_der(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n")
                .is_err()
        );
    }

    #[test]
    fn jpeg_decode_requires_exact_rectangle_and_rejects_invalid_payloads() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
            .encode(
                &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
                2,
                2,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let rgba = decode_jpeg(rect, jpeg.clone()).unwrap();
        assert_eq!(rgba.len(), 16);
        assert!(rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        assert!(decode_jpeg(Rect { width: 1, ..rect }, jpeg.clone()).is_err());
        assert!(decode_jpeg(rect, jpeg[..jpeg.len() / 2].to_vec()).is_err());
        assert!(decode_jpeg(rect, vec![0; 64]).is_err());
    }

    #[test]
    fn motion_is_coalesced_without_crossing_buttons_or_keys() {
        let queue = CommandQueue::default();
        for x in 0..10_000 {
            assert!(queue.push(Command::Input(RdpInput::MouseMove { x, y: 7 })));
        }
        assert!(queue.push(Command::Input(RdpInput::MouseButton {
            button: RdpMouseButton::Left,
            pressed: true,
        })));
        assert!(queue.push(Command::Input(RdpInput::MouseMove { x: 15, y: 20 })));
        assert!(queue.push(Command::Key {
            keysym: 0x61,
            pressed: false
        }));
        let mut pending = queue.pending.lock().unwrap();
        assert_eq!(pending.len(), 4);
        assert!(matches!(
            pending.pop_front(),
            Some(Command::Input(RdpInput::MouseMove { x: 9999, y: 7 }))
        ));
        assert!(matches!(
            pending.pop_front(),
            Some(Command::Input(RdpInput::MouseButton { pressed: true, .. }))
        ));
        assert!(matches!(
            pending.pop_front(),
            Some(Command::Input(RdpInput::MouseMove { x: 15, y: 20 }))
        ));
        assert!(matches!(
            pending.pop_front(),
            Some(Command::Key { pressed: false, .. })
        ));
    }

    #[test]
    fn non_motion_input_remains_bounded_and_never_silently_dropped() {
        let queue = CommandQueue::default();
        for _ in 0..256 {
            assert!(queue.push(Command::Key {
                keysym: 0x61,
                pressed: true
            }));
        }
        assert!(!queue.push(Command::Key {
            keysym: 0x61,
            pressed: false
        }));
    }
}
