//! VNC owns a Tokio runtime off the UI thread. Only the latest framebuffer is
//! retained, so a hidden tab cannot build an unbounded queue of desktop images.

use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use vnc::{PixelFormat, Rect, VncClient, VncConnector, VncEncoding, VncError, VncEvent, X11Event};

use crate::rdp_client::Surface;
use crate::{RdpClientEvent, RdpFrame, RdpInput, RdpMouseButton};

pub(super) struct VncClientHandle {
    commands: mpsc::Sender<Command>,
    stop: watch::Sender<bool>,
    events: std_mpsc::Receiver<RdpClientEvent>,
    errors: std_mpsc::Sender<RdpClientEvent>,
    surface: Arc<Mutex<Surface>>,
}

enum Command {
    Input(RdpInput),
    Key { keysym: u32, pressed: bool },
}

impl VncClientHandle {
    fn send(&self, command: Command) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.commands.try_send(command) {
            // Losing a key-up silently would leave a remote modifier held. A
            // bounded queue and an explicit disconnect are safer than that.
            let _ = self.errors.send(RdpClientEvent::Error(
                "VNC 输入队列已满，连接已关闭，请重新连接".into(),
            ));
            self.close();
        }
    }

    pub(super) fn send_input(&self, input: RdpInput) {
        self.send(Command::Input(input));
    }

    pub(super) fn send_key(&self, keysym: u32, pressed: bool) {
        self.send(Command::Key { keysym, pressed });
    }

    pub(super) fn close(&self) {
        let _ = self.stop.send(true);
    }

    pub(super) fn try_recv(&self) -> Option<RdpClientEvent> {
        self.events.try_recv().ok()
    }

    pub(super) fn frame_if_newer(&self, generation: u64) -> Option<RdpFrame> {
        let surface = self.surface.lock().ok()?;
        (surface.generation != generation).then(|| RdpFrame {
            rgba: surface.rgba.clone(),
            width: surface.width,
            height: surface.height,
            generation: surface.generation,
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
) -> std::io::Result<VncClientHandle> {
    let (commands, receiver) = mpsc::channel(256);
    let (stop, stopped) = watch::channel(false);
    let (events, event_rx) = std_mpsc::channel();
    let errors = events.clone();
    let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
    let background_surface = Arc::clone(&surface);
    std::thread::Builder::new()
        .name("adit-vnc".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(VncError::IoError)
                .and_then(|runtime| {
                    runtime.block_on(run(
                        host,
                        port,
                        password,
                        receiver,
                        stopped,
                        &events,
                        &background_surface,
                    ))
                });
            if let Err(error) = result {
                let message = error_message(&error);
                let event = if is_auth_error(&error) {
                    RdpClientEvent::AuthRejected(message)
                } else {
                    RdpClientEvent::Error(message)
                };
                let _ = events.send(event);
            }
            let _ = events.send(RdpClientEvent::Closed);
        })?;
    Ok(VncClientHandle {
        commands,
        stop,
        events: event_rx,
        errors,
        surface,
    })
}

fn is_auth_error(error: &VncError) -> bool {
    matches!(error, VncError::WrongPassword | VncError::NoPassword)
        || matches!(error, VncError::General(message) if {
            let message = message.to_ascii_lowercase();
            message.contains("authentication fail") || message.contains("password")
                || message.contains("authentication reject")
        })
}

fn error_message(error: &VncError) -> String {
    if is_auth_error(error) {
        return "VNC 密码认证失败，请重新输入 VNC 密码".into();
    }
    match error {
        VncError::InvalidSecurityTyep(_) => {
            "VNC 服务器的认证方式暂不支持；当前支持 VncAuth 和无认证连接".into()
        }
        VncError::General(message) if message.contains("Security type") => {
            "VNC 服务器的认证方式暂不支持；当前支持 VncAuth 和无认证连接".into()
        }
        _ => format!("VNC 连接失败: {error}"),
    }
}

async fn run(
    host: String,
    port: u16,
    password: String,
    mut commands: mpsc::Receiver<Command>,
    mut stopped: watch::Receiver<bool>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    surface: &Arc<Mutex<Surface>>,
) -> Result<(), VncError> {
    let connect = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
        tcp.set_nodelay(true)?;
        VncConnector::new(tcp)
            .set_auth_method(async move { Ok(password) })
            .add_encoding(VncEncoding::Zrle)
            .add_encoding(VncEncoding::CopyRect)
            .add_encoding(VncEncoding::Raw)
            .add_encoding(VncEncoding::DesktopSizePseudo)
            .add_encoding(VncEncoding::LastRectPseudo)
            .allow_shared(true)
            .set_pixel_format(PixelFormat::rgba())
            .build()?
            .try_start()
            .await?
            .finish()
    };
    let client = tokio::select! {
        biased;
        _ = stopped.changed() => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(30), connect) =>
            result.map_err(|_| VncError::General("连接或认证超时（30 秒）".into()))??,
    };
    let result = pump(&client, &mut commands, &mut stopped, events, surface).await;
    let _ = client.close().await;
    result
}

async fn pump(
    client: &VncClient,
    commands: &mut mpsc::Receiver<Command>,
    stopped: &mut watch::Receiver<bool>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    surface: &Arc<Mutex<Surface>>,
) -> Result<(), VncError> {
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pointer = Pointer::default();
    let mut connected = false;
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed() => return Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()) };
                let inputs = match command {
                    Command::Key { keysym, pressed } =>
                        vec![X11Event::KeyEvent((keysym, pressed).into())],
                    Command::Input(input) => pointer.input(input),
                };
                for input in inputs {
                    // A blocked socket must not make the UI's disconnect wait
                    // on network backpressure or leave a worker alive forever.
                    tokio::select! {
                        biased;
                        _ = stopped.changed() => return Ok(()),
                        result = tokio::time::timeout(Duration::from_secs(5), client.input(input)) =>
                            result.map_err(|_| VncError::General("发送输入超时".into()))??,
                    }
                }
            }
            _ = tick.tick() => {
                // recv_event holds the library's mutex while waiting. Polling
                // avoids starving input/close on a desktop with no changes.
                for _ in 0..256 {
                    let Some(event) = client.poll_event().await? else { break };
                    match event {
                        VncEvent::SetResolution(screen) => {
                            resize(surface, screen.width, screen.height)?;
                            let event = if connected {
                                RdpClientEvent::Resized { width: screen.width, height: screen.height }
                            } else {
                                connected = true;
                                RdpClientEvent::Connected { width: screen.width, height: screen.height }
                            };
                            let _ = events.send(event);
                        }
                        VncEvent::RawImage(rect, mut rgba) => {
                            // RFB's fourth byte is padding, not alpha. iced
                            // would otherwise render a healthy desktop black.
                            for pixel in rgba.as_chunks_mut::<4>().0 { pixel[3] = 255; }
                            if let Ok(mut surface) = surface.lock() {
                                surface.blit(rect.x, rect.y, rect.width, rect.height, &rgba);
                            }
                        }
                        VncEvent::Copy(dst, src) => copy_rect(surface, dst, src)?,
                        VncEvent::Text(text) if text.is_ascii() => { let _ = events.send(RdpClientEvent::ClipboardText(text)); }
                        VncEvent::Error(message) => return Err(VncError::General(message)),
                        _ => {}
                    }
                }
                tokio::select! {
                    biased;
                    _ = stopped.changed() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(5), client.input(X11Event::Refresh)) =>
                        result.map_err(|_| VncError::General("请求画面更新超时".into()))??,
                }
            }
        }
    }
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

#[derive(Default)]
struct Pointer {
    x: u16,
    y: u16,
    buttons: u8,
}

impl Pointer {
    fn event(&self, buttons: u8) -> X11Event {
        X11Event::PointerEvent((self.x, self.y, buttons).into())
    }

    fn input(&mut self, input: RdpInput) -> Vec<X11Event> {
        match input {
            RdpInput::MouseMove { x, y } => {
                self.x = x;
                self.y = y;
                vec![self.event(self.buttons)]
            }
            RdpInput::MouseButton { button, pressed } => {
                let bit = match button {
                    RdpMouseButton::Left => 1,
                    RdpMouseButton::Middle => 2,
                    RdpMouseButton::Right => 4,
                    _ => return vec![],
                };
                if pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                vec![self.event(self.buttons)]
            }
            RdpInput::Wheel { vertical, delta } => {
                if delta == 0 {
                    return vec![];
                }
                let bit = match (vertical, delta > 0) {
                    (true, true) => 8,
                    (true, false) => 16,
                    (false, true) => 64,
                    (false, false) => 32,
                };
                (0..(delta.unsigned_abs() / 120).clamp(1, 32))
                    .flat_map(|_| [self.event(self.buttons | bit), self.event(self.buttons)])
                    .collect()
            }
            // Used by the toolbar's secure-attention sequence and by the
            // Windows fullscreen hook. Ordinary keys arrive as X11 keysyms.
            RdpInput::Key {
                scancode,
                extended,
                pressed,
            } => scancode_keysym(scancode, extended)
                .map(|key| vec![X11Event::KeyEvent((key, pressed).into())])
                .unwrap_or_default(),
            RdpInput::Unicode { ch, pressed } => {
                vec![X11Event::KeyEvent((unicode_keysym(ch), pressed).into())]
            }
            RdpInput::ClipboardText(text) if text.is_ascii() && text.len() <= 1_048_576 => {
                vec![X11Event::CopyText(text)]
            }
            // VNC desktop size is server-controlled. Window resizes use local
            // fit/1:1 scaling rather than changing the remote user's desktop.
            _ => vec![],
        }
    }
}

pub fn unicode_keysym(ch: char) -> u32 {
    let code = u32::from(ch);
    if code <= 0xff {
        code
    } else {
        0x0100_0000 | code
    }
}

fn scancode_keysym(code: u8, extended: bool) -> Option<u32> {
    Some(match (code, extended) {
        (0x1d, false) => 0xffe3,
        (0x1d, true) => 0xffe4,
        (0x2a, _) => 0xffe1,
        (0x36, _) => 0xffe2,
        (0x38, false) => 0xffe9,
        (0x38, true) => 0xffea,
        (0x5b, true) => 0xffeb,
        (0x5c, true) => 0xffec,
        (0x0f, _) => 0xff09,
        (0x01, _) => 0xff1b,
        (0x53, true) => 0xffff,
        _ => return None,
    })
}
