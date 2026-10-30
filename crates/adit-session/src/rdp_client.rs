//! Client side of the out-of-process RDP helper.
//!
//! Native RDP can't be linked into the main binary (IronRDP's crypto deps clash
//! with russh's — see the RDP dependency note), so it runs as the `adit-rdp-host`
//! child process. This module spawns that helper, drives it over stdin/stdout
//! using [`adit_rdp_proto`], keeps the decoded framebuffer, and presents a small
//! handle to the session layer that mirrors [`adit_ssh::LiveShellHandle`].

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use adit_rdp_proto::{monitor_bounds, DisplayControlState, Monitor, PointerUpdate};
use adit_rdp_proto::{
    read_msg, write_msg, ClientMsg, ClipFile, ConnectRequest, HostMsg, InputEvent,
};

pub(crate) fn to_wire_monitor(m: &adit_domain::DesktopMonitor) -> Monitor {
    Monitor {
        left: m.left,
        top: m.top,
        width: m.width,
        height: m.height,
        primary: m.primary,
        scale_percent: m.scale_percent,
    }
}

fn to_profile_monitor(m: &Monitor) -> adit_domain::DesktopMonitor {
    adit_domain::DesktopMonitor {
        left: m.left,
        top: m.top,
        width: m.width,
        height: m.height,
        primary: m.primary,
        scale_percent: m.scale_percent,
    }
}

/// Environment override for the helper path (mainly for development/tests).
const HELPER_ENV: &str = "ADIT_RDP_HOST";
/// Helper executable file name.
#[cfg(windows)]
const HELPER_EXE: &str = "adit-rdp-host.exe";
#[cfg(not(windows))]
const HELPER_EXE: &str = "adit-rdp-host";

/// A discrete event from the RDP session (framebuffer updates are applied to the
/// surface directly and don't appear here).
#[derive(Debug, Clone)]
pub enum RdpClientEvent {
    Connected {
        width: u16,
        height: u16,
    },
    Resized {
        width: u16,
        height: u16,
    },
    ClipboardText(String),
    VncClipboardText(vnc::ClipboardText),
    /// The remote copied files. The app should offer them on the Windows
    /// clipboard; nothing crosses the wire until something here pastes.
    ClipboardFiles {
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// The remote copied an image; raw `CF_DIB` bytes for the app to put on the
    /// Windows clipboard.
    ClipboardImage(Vec<u8>),
    /// Bytes answering a [`RdpClientHandle::file_requester`] request.
    FileContents {
        generation: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// The remote is pasting files we offered and wants a byte range. The app
    /// must answer — with bytes or with a failure — because a remote paste
    /// blocks on the stream it asked for.
    FileContentsNeeded {
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    Error(String),
    AuthRejected(String),
    Notice(String),
    GatewayHostKey(adit_ssh::HostKeyPrompt),
    GatewayAuthPrompt(adit_ssh::AuthPromptRequest),
    VncReconnecting {
        attempt: u32,
        delay_secs: u64,
    },
    Closed,
}

/// The current decoded desktop image plus a monotonic generation the UI uses to
/// avoid re-uploading an unchanged frame.
pub(super) struct Surface {
    pub(super) rgba: Vec<u8>,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) generation: u64,
    pointer: Option<super::VncCursor>,
    pointer_position: Option<(u16, u16)>,
    pointer_generation: u64,
    pointer_position_generation: u64,
    display: DisplayControlState,
    selected_monitor: Option<usize>,
    layout_pending: bool,
}

/// Ceiling per side for a framebuffer allocation. Mirrors the request-side clamp
/// in the helper (`adit-rdp` clamps the desktop to 8192). Without this, a server
/// that reports a huge desktop size in `Connected`/`Resized` would make us try to
/// allocate gigabytes — and a zeroed-`Vec` allocation failure `abort()`s the whole
/// process, taking every other tab down with it. Oversized tiles are then simply
/// dropped by `blit`'s bounds check.
const MAX_RDP_DIMENSION: u16 = 8192;

impl Surface {
    pub(super) fn new(width: u16, height: u16) -> Self {
        let width = width.clamp(1, MAX_RDP_DIMENSION);
        let height = height.clamp(1, MAX_RDP_DIMENSION);
        Self {
            rgba: vec![0u8; usize::from(width) * usize::from(height) * 4],
            width,
            height,
            generation: 0,
            pointer: None,
            pointer_position: None,
            pointer_generation: 0,
            pointer_position_generation: 0,
            display: DisplayControlState::default(),
            selected_monitor: None,
            layout_pending: false,
        }
    }

    fn viewport(&self) -> (u16, u16, u16, u16) {
        if self.display.applied {
            if let Some(m) = self
                .selected_monitor
                .and_then(|i| self.display.sent_layout.get(i))
            {
                if let Ok((left, top, w, h)) = monitor_bounds(&self.display.sent_layout) {
                    if (w, h) == (self.width, self.height) {
                        return (
                            (m.left - left) as u16,
                            (m.top - top) as u16,
                            m.width,
                            m.height,
                        );
                    }
                }
            }
        }
        (0, 0, self.width, self.height)
    }

    fn update_pointer(&mut self, update: PointerUpdate) {
        let position_only = matches!(&update, PointerUpdate::Position { .. });
        match update {
            PointerUpdate::Position { x, y } => {
                self.pointer_position = Some((x, y));
                self.pointer_position_generation = self.pointer_position_generation.wrapping_add(1);
            }
            PointerUpdate::Default => self.pointer = None,
            PointerUpdate::Hidden => self.pointer = Some(super::VncCursor::default()),
            PointerUpdate::Bitmap {
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => {
                if width == 0 && height == 0 && rgba.is_empty() {
                    self.pointer = Some(super::VncCursor::default());
                } else if width <= 384
                    && height <= 384
                    && hotspot_x < width
                    && hotspot_y < height
                    && rgba.len() == usize::from(width) * usize::from(height) * 4
                {
                    self.pointer = Some(super::VncCursor {
                        width,
                        height,
                        hotspot_x,
                        hotspot_y,
                        rgba,
                        generation: 0,
                    });
                } else {
                    return;
                }
            }
        }
        if !position_only {
            self.pointer_generation = self.pointer_generation.wrapping_add(1);
            if let Some(shape) = &mut self.pointer {
                shape.generation = self.pointer_generation;
            }
        }
    }

    fn resize(&mut self, width: u16, height: u16) {
        let mut next = Surface::new(width, height);
        let copy_w = usize::from(self.width.min(next.width)) * 4;
        for row in 0..usize::from(self.height.min(next.height)) {
            let src = row * usize::from(self.width) * 4;
            let dst = row * usize::from(next.width) * 4;
            next.rgba[dst..dst + copy_w].copy_from_slice(&self.rgba[src..src + copy_w]);
        }
        next.pointer = self.pointer.take();
        next.pointer_position = self.pointer_position;
        next.pointer_generation = self.pointer_generation;
        next.pointer_position_generation = self.pointer_position_generation;
        next.display = std::mem::take(&mut self.display);
        next.selected_monitor = self.selected_monitor;
        next.layout_pending = self.layout_pending;
        next.generation = self.generation.wrapping_add(1);
        next.match_layout_size();
        *self = next;
    }

    fn match_layout_size(&mut self) {
        self.display.applied = monitor_bounds(&self.display.sent_layout)
            .is_ok_and(|(_, _, w, h)| (w, h) == (self.width, self.height));
    }

    /// Blit a tile into the surface, ignoring anything out of bounds (defensive
    /// against a malformed message).
    pub(super) fn blit(&mut self, x: u16, y: u16, tw: u16, th: u16, rgba: &[u8]) {
        let (sw, sh) = (usize::from(self.width), usize::from(self.height));
        let (x, y, tw, th) = (
            usize::from(x),
            usize::from(y),
            usize::from(tw),
            usize::from(th),
        );
        if x + tw > sw || y + th > sh || rgba.len() < tw * th * 4 {
            return;
        }
        for row in 0..th {
            let dst = ((y + row) * sw + x) * 4;
            let src = row * tw * 4;
            self.rgba[dst..dst + tw * 4].copy_from_slice(&rgba[src..src + tw * 4]);
        }
        self.generation = self.generation.wrapping_add(1);
    }
}

struct FileEpoch {
    generation: AtomicU64,
    closed: AtomicBool,
    enabled: AtomicBool,
}
impl Default for FileEpoch {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
        }
    }
}
impl FileEpoch {
    fn observe_offer(&self, generation: u64, has_files: bool) {
        self.generation.store(
            if has_files
                && !self.closed.load(Ordering::Acquire)
                && self.enabled.load(Ordering::Acquire)
            {
                generation
            } else {
                0
            },
            Ordering::Release,
        );
    }
    fn accepts(&self, generation: u64) -> bool {
        generation != 0
            && !self.closed.load(Ordering::Acquire)
            && self.enabled.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
    }
}

/// Asks the remote for a byte range of a file it offered.
///
/// `std::sync::mpsc::Sender` is `Send` but not `Sync`, and the COM streams that
/// call this are shared across Explorer's threads — hence the mutex, which is
/// only ever held for the length of a `send`.
#[derive(Clone)]
pub struct RdpFileRequester {
    tx: Arc<Mutex<std_mpsc::Sender<ClientMsg>>>,
    generation: u64,
    current: Arc<FileEpoch>,
    replies: std_mpsc::Sender<RdpClientEvent>,
}

impl RdpFileRequester {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn is_current(&self) -> bool {
        self.current.accepts(self.generation)
    }

    pub fn request(&self, stream_id: u32, index: u32, offset: u64, length: u32) {
        let sent = self.is_current()
            && self.tx.lock().ok().is_some_and(|tx| {
                tx.send(ClientMsg::RequestFileContents {
                    generation: self.generation,
                    stream_id,
                    index,
                    offset,
                    length,
                })
                .is_ok()
            });
        if !sent {
            let _ = self.replies.send(RdpClientEvent::FileContents {
                generation: self.generation,
                stream_id,
                data: None,
            });
        }
    }
}

/// A snapshot of the framebuffer for the renderer.
#[derive(Clone)]
pub struct RdpFrame {
    pub rgba: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub generation: u64,
}

pub struct RdpCursorSnapshot {
    pub shape: Option<super::VncCursor>,
    pub position: Option<(u16, u16)>,
    pub position_generation: u64,
}

/// Handle to a live RDP session running in the helper process.
pub struct RdpClientHandle {
    remote_offer: Arc<FileEpoch>,
    event_tx: std_mpsc::Sender<RdpClientEvent>,
    cmd_tx: std_mpsc::Sender<ClientMsg>,
    event_rx: std_mpsc::Receiver<RdpClientEvent>,
    surface: Arc<Mutex<Surface>>,
    // Populated asynchronously: the helper is spawned on a background thread so the
    // UI thread never blocks on `Command::spawn` (see `spawn_rdp_session`). `None`
    // until the spawn completes.
    child: Arc<Mutex<Option<Child>>>,
}

impl RdpClientHandle {
    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        Self::test_fixture_with_channels().0
    }

    #[cfg(test)]
    pub(crate) fn test_fixture_with_channels() -> (
        Self,
        std_mpsc::Receiver<ClientMsg>,
        std_mpsc::Sender<RdpClientEvent>,
    ) {
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = std_mpsc::channel();
        (
            Self {
                remote_offer: Arc::new(FileEpoch::default()),
                event_tx: event_tx.clone(),
                cmd_tx,
                event_rx,
                surface: Arc::new(Mutex::new(Surface::new(1, 1))),
                child: Arc::new(Mutex::new(None)),
            },
            cmd_rx,
            event_tx,
        )
    }

    /// Send an input event to the session.
    pub fn send_input(&self, mut event: InputEvent) {
        if let Ok(surface) = self.surface.lock() {
            match &mut event {
                InputEvent::MouseMove { x, y } => {
                    let (left, top, width, height) = surface.viewport();
                    *x = (*x).min(width - 1).saturating_add(left);
                    *y = (*y).min(height - 1).saturating_add(top);
                }
                // Window resize must not replace a multi-monitor layout with
                // ActiveStage's single-primary resize message.
                InputEvent::Resize { .. }
                    if surface.layout_pending || surface.display.sent_layout.len() > 1 =>
                {
                    return
                }
                _ => {}
            }
        }
        let _ = self.cmd_tx.send(ClientMsg::Input(event));
    }

    /// Offer a locally-copied image to the remote clipboard (raw `CF_DIB`).
    pub fn send_clipboard_image(&self, image: Vec<u8>) {
        let _ = self.cmd_tx.send(ClientMsg::ClipboardImage(image));
    }

    /// Local file offers are temporarily disabled. An empty offer is retained
    /// as a metadata-free IPC barrier for older peers.
    pub fn send_clipboard_files(&self, files: Vec<ClipFile>) {
        if files.is_empty() {
            let _ = self.cmd_tx.send(ClientMsg::ClipboardFiles(files));
        }
    }

    /// Always reject local file reads, including requests from an older helper.
    pub fn answer_file_contents(&self, stream_id: u32, _data: Option<Vec<u8>>) {
        let _ = self.cmd_tx.send(ClientMsg::FileContents {
            stream_id,
            data: None,
        });
    }

    /// A handle for asking the remote for file bytes.
    ///
    /// Separate from `&self` because the callers are COM streams running on
    /// Explorer's threads: it has to be `Send + Sync` and outlive any borrow of
    /// the session.
    pub fn file_requester(&self) -> RdpFileRequester {
        self.file_requester_for_generation(self.remote_offer.generation.load(Ordering::Acquire))
    }

    pub(crate) fn invalidate_file_offer(&self) {
        self.remote_offer.enabled.store(false, Ordering::Release);
        self.remote_offer.generation.store(0, Ordering::Release);
    }

    pub(crate) fn set_file_offers_enabled(&self, enabled: bool) {
        if self.remote_offer.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            self.remote_offer.generation.store(0, Ordering::Release);
        }
    }

    pub(crate) fn file_requester_for_generation(&self, generation: u64) -> RdpFileRequester {
        RdpFileRequester {
            tx: Arc::new(Mutex::new(self.cmd_tx.clone())),
            generation,
            current: self.remote_offer.clone(),
            replies: self.event_tx.clone(),
        }
    }

    pub(crate) fn owns_file_requester(&self, requester: &RdpFileRequester) -> bool {
        Arc::ptr_eq(&self.remote_offer, &requester.current)
    }

    /// Ask the session to disconnect gracefully.
    pub fn close(&self) {
        self.remote_offer.closed.store(true, Ordering::Release);
        self.remote_offer.generation.store(0, Ordering::Release);
        let _ = self.cmd_tx.send(ClientMsg::Close);
    }

    /// Poll for the next discrete session event.
    #[must_use]
    pub fn try_recv(&self) -> Option<RdpClientEvent> {
        self.event_rx.try_recv().ok()
    }

    /// The current framebuffer generation (cheap; no pixel copy).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.surface.lock().map(|s| s.generation).unwrap_or(0)
    }

    /// A copy of the framebuffer, but only if it changed since `last_generation`.
    #[must_use]
    pub fn frame_if_newer(&self, last_generation: u64) -> Option<RdpFrame> {
        let surface = self.surface.lock().ok()?;
        if surface.generation == last_generation {
            return None;
        }
        let (x, y, width, height) = surface.viewport();
        let mut rgba = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
        for row in y..y + height {
            let start = (usize::from(row) * usize::from(surface.width) + usize::from(x)) * 4;
            rgba.extend_from_slice(&surface.rgba[start..start + usize::from(width) * 4]);
        }
        Some(RdpFrame {
            rgba,
            width,
            height,
            generation: surface.generation,
        })
    }

    /// Session creation adapter for persisted monitor preferences.
    pub fn spawn_with_monitors(
        request: ConnectRequest,
        monitors: Vec<Monitor>,
        selected_monitor: Option<usize>,
    ) -> Result<Self, RdpClientError> {
        if !monitors.is_empty() {
            monitor_bounds(&monitors).map_err(|e| RdpClientError::Io(e.to_owned()))?;
        }
        if selected_monitor.is_some_and(|i| i >= monitors.len()) {
            return Err(RdpClientError::Io(
                "selected monitor index is outside the layout".to_owned(),
            ));
        }
        let handle = spawn_rdp_session(request)?;
        if !monitors.is_empty() {
            handle
                .configure_monitors(monitors, selected_monitor)
                .map_err(RdpClientError::Io)?;
        }
        Ok(handle)
    }

    pub fn configure_monitors(
        &self,
        monitors: Vec<Monitor>,
        selected: Option<usize>,
    ) -> Result<(), String> {
        monitor_bounds(&monitors).map_err(str::to_owned)?;
        if selected.is_some_and(|i| i >= monitors.len()) {
            return Err("selected monitor index is outside the layout".to_owned());
        }
        if let Ok(mut surface) = self.surface.lock() {
            surface.layout_pending = true;
            surface.selected_monitor = selected;
        }
        self.cmd_tx
            .send(ClientMsg::Input(InputEvent::MonitorLayout(monitors)))
            .map_err(|_| "RDP helper command channel is closed".to_owned())
    }
}

impl Drop for RdpClientHandle {
    fn drop(&mut self) {
        self.remote_offer.closed.store(true, Ordering::Release);
        self.remote_offer.generation.store(0, Ordering::Release);
        // Ask for a graceful close, then let the helper actually run its RDP
        // shutdown sequence before we force-kill. A synchronous `kill()` here would
        // race ahead of the `Close` even reaching the helper's stdin, so we reap on
        // a background thread with a short grace window — and still guarantee no
        // leaked process if the helper hangs.
        //
        // Dropping `cmd_tx` also closes the writer thread's channel, which drops the
        // helper's stdin; the helper sees EOF and shuts down. That covers the rare
        // case where the handle is dropped before the async spawn even produced a
        // `Child` (nothing to kill yet) — the helper still exits on its own.
        let _ = self.cmd_tx.send(ClientMsg::Close);
        let child = Arc::clone(&self.child);
        let _ = thread::Builder::new()
            .name("adit-rdp-client-reap".into())
            .spawn(move || {
                // Wait up to ~10s: first for the spawn to produce a Child, then for
                // it to exit gracefully; force-kill if it overstays.
                for _ in 0..400 {
                    if let Ok(mut guard) = child.lock() {
                        if let Some(c) = guard.as_mut() {
                            if let Ok(Some(_)) = c.try_wait() {
                                return; // exited on its own
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                if let Ok(mut guard) = child.lock() {
                    if let Some(c) = guard.as_mut() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RdpClientError {
    #[error("the RDP helper ({0}) was not found; reinstall Adit")]
    HelperMissing(String),
    #[error("could not start the RDP helper: {0}")]
    Spawn(String),
    #[error("could not talk to the RDP helper: {0}")]
    Io(String),
}

/// Locate the `adit-rdp-host` executable: an explicit override, then next to the
/// current executable (the installed layout), then the dev build output.
fn locate_helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(HELPER_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(HELPER_EXE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Development fallbacks: the helper builds into its own workspace target dir.
    for rel in [
        "crates/adit-rdp/target/release",
        "crates/adit-rdp/target/debug",
    ] {
        let candidate = PathBuf::from(rel).join(HELPER_EXE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: keep the console helper from flashing a window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

/// Spawn the helper and open an RDP session. Returns immediately; connection
/// progress arrives as [`RdpClientEvent`]s.
///
/// The actual `Command::spawn` and pipe wiring happen on a **background thread**,
/// never on the caller's (UI) thread: the first spawn of a freshly-installed,
/// unsigned helper can block for seconds while Windows Defender scans the image,
/// and blocking the UI thread that long makes the whole app go "Not Responding".
/// A spawn failure surfaces as an [`RdpClientEvent::Error`] + `Closed` instead of
/// a returned `Err`.
pub fn spawn_rdp_session(request: ConnectRequest) -> Result<RdpClientHandle, RdpClientError> {
    // Locating the helper is a couple of cheap filesystem stats — keep it on the
    // caller so a missing/uninstalled helper still fails fast and visibly.
    let helper = locate_helper().ok_or_else(|| RdpClientError::HelperMissing(HELPER_EXE.into()))?;

    let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
    let (cmd_tx, cmd_rx) = std_mpsc::channel::<ClientMsg>();
    // A clone for the stdout reader, so it can answer a file-contents request
    // with a refusal itself. The reader must not depend on the app being awake:
    // a remote paste blocks on the stream it asked for.
    let cmd_tx_reader = cmd_tx.clone();
    let (event_tx, event_rx) = std_mpsc::channel::<RdpClientEvent>();
    let remote_offer = Arc::new(FileEpoch::default());
    let remote_offer_bg = remote_offer.clone();
    let handle_event_tx = event_tx.clone();
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

    let surface_bg = Arc::clone(&surface);
    let child_bg = Arc::clone(&child_slot);
    thread::Builder::new()
        .name("adit-rdp-client-spawn".into())
        .spawn(move || {
            let mut command = Command::new(&helper);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                // Let the helper's stderr diagnostics flow to our own stderr.
                .stderr(Stdio::inherit());
            no_window(&mut command);

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = event_tx.send(RdpClientEvent::Error(format!(
                        "could not start the RDP helper: {e}"
                    )));
                    let _ = event_tx.send(RdpClientEvent::Closed);
                    return;
                }
            };

            // Wire the pipes; on failure kill the child (its drop does NOT terminate
            // the process, so a half-wired helper would leak — possibly running a
            // real session with the password the user can't see or control). The
            // writer takes `cmd_rx` by value; the reader takes a clone of `event_tx`
            // so this thread keeps the original for error reporting.
            if let Err(error) = wire_up(
                &mut child,
                request,
                cmd_rx,
                cmd_tx_reader,
                event_tx.clone(),
                &surface_bg,
                remote_offer_bg.clone(),
            ) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = event_tx.send(RdpClientEvent::Error(error.to_string()));
                let _ = event_tx.send(RdpClientEvent::Closed);
                return;
            }
            if let Ok(mut slot) = child_bg.lock() {
                *slot = Some(child);
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    Ok(RdpClientHandle {
        remote_offer,
        event_tx: handle_event_tx,
        cmd_tx,
        event_rx,
        surface,
        child: child_slot,
    })
}

/// Take the helper's pipes, send the opening `Connect`, and start the stdin-writer
/// and stdout-reader bridge threads. Runs on the background spawn thread. `cmd_rx`
/// (moved into the writer) and `event_tx` (cloned into the reader) come from the
/// handle created in [`spawn_rdp_session`].
fn wire_up(
    child: &mut Child,
    request: ConnectRequest,
    cmd_rx: std_mpsc::Receiver<ClientMsg>,
    // A second handle on the command channel, for the reader thread to answer a
    // file request with a refusal without going through the app.
    refuse_tx: std_mpsc::Sender<ClientMsg>,
    event_tx: std_mpsc::Sender<RdpClientEvent>,
    surface: &Arc<Mutex<Surface>>,
    remote_offer: Arc<FileEpoch>,
) -> Result<(), RdpClientError> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdin unavailable".into()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdout unavailable".into()))?;

    // Open the session before handing stdin to the writer thread.
    write_msg(&mut stdin, &ClientMsg::Connect(request))
        .map_err(|e| RdpClientError::Io(e.to_string()))?;

    // stdin writer: serialise ClientMsgs to the helper.
    thread::Builder::new()
        .name("adit-rdp-client-stdin".into())
        .spawn(move || {
            while let Ok(msg) = cmd_rx.recv() {
                if write_msg(&mut stdin, &msg).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    // stdout reader: apply framebuffer tiles and forward discrete events.
    let reader_surface = Arc::clone(surface);
    thread::Builder::new()
        .name("adit-rdp-client-stdout".into())
        .spawn(move || {
            loop {
                match read_msg::<_, HostMsg>(&mut stdout) {
                    Ok(Some(HostMsg::Connected { width, height })) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            s.resize(width, height);
                        }
                        let _ = event_tx.send(RdpClientEvent::Connected { width, height });
                    }
                    Ok(Some(HostMsg::Resized { width, height })) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            s.resize(width, height);
                        }
                        let _ = event_tx.send(RdpClientEvent::Resized { width, height });
                    }
                    // Remote file offers stay bound to their session. A local
                    // file request from an older helper is refused by the
                    // session manager so a remote paste cannot wait forever.
                    Ok(Some(HostMsg::ClipboardImage(image))) => {
                        let _ = event_tx.send(RdpClientEvent::ClipboardImage(image));
                    }
                    Ok(Some(HostMsg::ClipboardFiles { generation, files })) => {
                        remote_offer.observe_offer(generation, !files.is_empty());
                        let files = if remote_offer.accepts(generation) {
                            files
                        } else {
                            Vec::new()
                        };
                        let _ = event_tx.send(RdpClientEvent::ClipboardFiles { generation, files });
                    }
                    Ok(Some(HostMsg::FileContentsNeeded {
                        stream_id,
                        index,
                        offset,
                        length,
                    })) => {
                        // Refuse on the spot if the app is no longer listening.
                        // Silence would leave the remote paste hanging on a
                        // stream that is never going to arrive.
                        if event_tx
                            .send(RdpClientEvent::FileContentsNeeded {
                                stream_id,
                                index,
                                offset,
                                length,
                            })
                            .is_err()
                        {
                            let _ = refuse_tx.send(ClientMsg::FileContents {
                                stream_id,
                                data: None,
                            });
                        }
                    }
                    Ok(Some(HostMsg::FileContents {
                        generation,
                        stream_id,
                        data,
                    })) => {
                        let data = data.filter(|_| remote_offer.accepts(generation));
                        let _ = event_tx.send(RdpClientEvent::FileContents {
                            generation,
                            stream_id,
                            data,
                        });
                    }
                    Ok(Some(HostMsg::Pointer(pointer))) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            s.update_pointer(pointer);
                        }
                    }
                    Ok(Some(HostMsg::DisplayControl(state))) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            s.display = state;
                            // A capability announcement is not the answer to an
                            // initial request still waiting inside the helper.
                            if !s.display.sent_layout.is_empty() || s.display.error.is_some() {
                                s.layout_pending = false;
                            }
                            s.match_layout_size();
                            s.generation = s.generation.wrapping_add(1);
                        }
                    }
                    Ok(Some(HostMsg::Tile {
                        x,
                        y,
                        width,
                        height,
                        rgba,
                    })) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            s.blit(x, y, width, height, &rgba);
                        }
                    }
                    Ok(Some(HostMsg::ClipboardText(text))) => {
                        let _ = event_tx.send(RdpClientEvent::ClipboardText(text));
                    }
                    Ok(Some(HostMsg::Error(message))) => {
                        remote_offer.closed.store(true, Ordering::Release);
                        remote_offer.generation.store(0, Ordering::Release);
                        let _ = event_tx.send(RdpClientEvent::Error(message));
                    }
                    Ok(Some(HostMsg::Closed)) | Ok(None) => {
                        remote_offer.closed.store(true, Ordering::Release);
                        remote_offer.generation.store(0, Ordering::Release);
                        let _ = event_tx.send(RdpClientEvent::Closed);
                        break;
                    }
                    Err(_) => {
                        remote_offer.closed.store(true, Ordering::Release);
                        remote_offer.generation.store(0, Ordering::Release);
                        let _ = event_tx.send(RdpClientEvent::Closed);
                        break;
                    }
                }
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    Ok(())
}

// These adapters live with the RDP client to keep the generic session lifecycle
// and ConnectionProfile untouched while exposing metadata to both desktop views.
impl super::SessionManager {
    pub fn session_rdp_cursor(&self, id: super::SessionId) -> Option<RdpCursorSnapshot> {
        let record = self.sessions.get(&id)?;
        if record.summary.status != super::SessionStatus::Connected
            || record.desktop_error.is_some()
        {
            return None;
        }
        let surface = record.rdp.as_ref()?.surface.lock().ok()?;
        let (left, top, width, height) = surface.viewport();
        let position = surface.pointer_position.and_then(|(x, y)| {
            (x >= left && y >= top && x - left < width && y - top < height)
                .then(|| (x - left, y - top))
        });
        Some(RdpCursorSnapshot {
            shape: surface.pointer.clone(),
            position,
            position_generation: surface.pointer_position_generation,
        })
    }

    pub fn active_rdp_display_control(&self) -> Option<(DisplayControlState, Option<usize>)> {
        let record = self.sessions.get(&self.active_session?)?;
        let surface = record.rdp.as_ref()?.surface.lock().ok()?;
        Some((surface.display.clone(), surface.selected_monitor))
    }

    pub fn select_active_rdp_monitor(&mut self, selected: Option<usize>) -> Result<(), String> {
        let id = self.active_session.ok_or("没有活动会话")?;
        let record = self.sessions.get(&id).ok_or("找不到活动会话")?;
        let rdp = record.rdp.as_ref().ok_or("当前会话不是 RDP")?;
        let profile_id = record.summary.profile_id;
        let monitors = {
            let surface = rdp.surface.lock().map_err(|_| "无法读取显示器布局")?;
            if !surface.display.applied
                || selected.is_some_and(|i| i >= surface.display.sent_layout.len())
            {
                return Err("远程桌面尚未应用该布局".into());
            }
            surface.display.sent_layout.clone()
        };
        let mut options = self
            .profile(profile_id)
            .ok_or("找不到会话配置")?
            .session_options
            .clone();
        options.rdp_monitors = Some(monitors.iter().map(to_profile_monitor).collect());
        options.rdp_selected_monitor = selected;
        options.validate()?;
        {
            let mut surface = rdp.surface.lock().map_err(|_| "无法读取显示器布局")?;
            if !surface.display.applied || surface.display.sent_layout != monitors {
                return Err("显示器布局已变化，请重试".into());
            }
            surface.selected_monitor = selected;
            surface.generation = surface.generation.wrapping_add(1);
        }
        self.set_profile_session_options(profile_id, options.clone())
            .map_err(|e| e.to_string())?;
        if let Some(record) = self.sessions.get_mut(&id) {
            record.options.rdp_monitors = options.rdp_monitors;
            record.options.rdp_selected_monitor = options.rdp_selected_monitor;
        }
        Ok(())
    }

    /// Convenient live single/dual layout choices. Arbitrary persisted layouts
    /// use RdpClientHandle::spawn_with_monitors/configure_monitors instead.
    pub fn set_active_rdp_monitor_count(&mut self, count: u16) -> Result<(), String> {
        if !(1..=2).contains(&count) {
            return Err("仅支持选择单屏或双屏布局".into());
        }
        let id = self.active_session.ok_or("没有活动会话")?;
        let record = self.sessions.get(&id).ok_or("找不到活动会话")?;
        let rdp = record.rdp.as_ref().ok_or("当前会话不是 RDP")?;
        let profile_id = record.summary.profile_id;
        let (width, height, max_area) = match rdp.surface.lock() {
            Ok(s) => {
                if count as u32 > s.display.max_monitors {
                    return Err("服务端不支持所选显示器数量".into());
                }
                let (w, h) = s
                    .display
                    .sent_layout
                    .first()
                    .map_or((s.width, s.height), |m| (m.width, m.height));
                // Grow the virtual desktop when adding a monitor. Splitting the
                // old bounding box would appear to succeed even if the server
                // ignored the request, because its size would already match.
                (
                    w.clamp(800, 4096) & !1,
                    h.clamp(600, 8192),
                    s.display.max_area,
                )
            }
            Err(_) => return Err("无法读取显示器布局".into()),
        };
        let monitors: Vec<Monitor> = (0..count)
            .map(|i| Monitor {
                left: i32::from(i * width),
                top: 0,
                width,
                height,
                primary: i == 0,
                scale_percent: 100,
            })
            .collect();
        monitor_bounds(&monitors).map_err(str::to_owned)?;
        let area: u64 = monitors
            .iter()
            .map(|m| u64::from(m.width) * u64::from(m.height))
            .sum();
        if area > max_area {
            return Err("布局超过服务端支持的总面积".into());
        }
        let mut options = self
            .profile(profile_id)
            .ok_or("找不到会话配置")?
            .session_options
            .clone();
        options.rdp_monitors = Some(monitors.iter().map(to_profile_monitor).collect());
        options.rdp_selected_monitor = None;
        options.validate()?;
        rdp.configure_monitors(monitors, None)?;
        self.set_profile_session_options(profile_id, options.clone())
            .map_err(|e| e.to_string())?;
        if let Some(record) = self.sessions.get_mut(&id) {
            record.options.rdp_monitors = options.rdp_monitors;
            record.options.rdp_selected_monitor = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod rdp_presentation_tests {
    use super::*;
    fn monitors() -> Vec<Monitor> {
        vec![
            Monitor {
                left: -200,
                top: 0,
                width: 200,
                height: 200,
                primary: false,
                scale_percent: 100,
            },
            Monitor {
                left: 0,
                top: 0,
                width: 200,
                height: 200,
                primary: true,
                scale_percent: 100,
            },
        ]
    }
    fn handle(surface: Surface) -> (RdpClientHandle, std_mpsc::Receiver<ClientMsg>) {
        let (cmd_tx, rx) = std_mpsc::channel();
        let (event_tx, event_rx) = std_mpsc::channel();
        (
            RdpClientHandle {
                remote_offer: Arc::new(FileEpoch::default()),
                event_tx,
                cmd_tx,
                event_rx,
                surface: Arc::new(Mutex::new(surface)),
                child: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }

    #[test]
    fn file_epoch_requester_never_binds_historical_metadata_to_the_latest_offer() {
        let (handle, commands) = handle(Surface::new(1, 1));
        handle.remote_offer.observe_offer(10, true);
        let old = handle.file_requester_for_generation(10);
        assert!(old.is_current());
        // The IPC reader sees B; SessionManager/GUI has not consumed A yet.
        handle.remote_offer.observe_offer(11, true);
        let delayed_a = handle.file_requester_for_generation(10);
        delayed_a.request(42, 0, 0, 3);
        assert!(commands.try_recv().is_err());
        assert!(matches!(
            handle.try_recv(),
            Some(RdpClientEvent::FileContents {
                generation: 10,
                stream_id: 42,
                data: None,
            })
        ));
        let current = handle.file_requester_for_generation(11);
        current.request(42, 1, 4, 3);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents {
                generation: 11,
                stream_id: 42,
                index: 1,
                offset: 4,
                length: 3,
            })
        ));
        assert_eq!(old.generation(), 10);
        assert!(!old.is_current());
    }

    #[test]
    fn file_epoch_text_and_close_revoke_callers_before_gui_poll() {
        let (handle, commands) = handle(Surface::new(1, 1));
        handle.remote_offer.observe_offer(7, true);
        let requester = handle.file_requester();
        handle.remote_offer.observe_offer(8, false); // text-only FormatList
        assert!(!requester.is_current());
        requester.request(1, 0, 0, 1);
        assert!(commands.try_recv().is_err());
        handle.close();
        assert!(matches!(commands.try_recv(), Ok(ClientMsg::Close)));
        handle.remote_offer.observe_offer(7, true); // a queued stale IPC frame
        assert!(!requester.is_current());
        requester.request(2, 0, 0, 1);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn file_epoch_disabling_file_sharing_cannot_be_undone_by_a_queued_offer() {
        let (handle, commands) = handle(Surface::new(1, 1));
        handle.remote_offer.observe_offer(7, true);
        let requester = handle.file_requester();
        handle.set_file_offers_enabled(false);
        handle.remote_offer.observe_offer(8, true);
        requester.request(1, 0, 0, 1);
        assert!(commands.try_recv().is_err());
        assert!(!requester.is_current());
        handle.set_file_offers_enabled(true);
        assert!(!requester.is_current());
        handle.remote_offer.observe_offer(9, true);
        assert!(handle.file_requester().is_current());
    }

    #[test]
    fn file_epoch_equal_numbers_in_a_reconnected_helper_do_not_share_identity() {
        let (a, _) = handle(Surface::new(1, 1));
        let (b, _) = handle(Surface::new(1, 1));
        a.remote_offer.observe_offer(7, true);
        b.remote_offer.observe_offer(7, true);
        let old = a.file_requester();
        assert!(a.owns_file_requester(&old));
        assert!(!b.owns_file_requester(&old));
        a.close();
        assert!(!old.is_current());
        assert!(b.file_requester().is_current());
    }

    #[test]
    fn monitor_choices_are_sent_and_persisted_without_replacing_other_session_options() {
        let mut manager = super::super::SessionManager::with_demo_profiles();
        let profile_id = manager.profiles()[0].id;
        let old = adit_domain::ReconnectPolicy {
            max_attempts: 3,
            ..Default::default()
        };
        let mut options = adit_domain::SessionOptions {
            reconnect: Some(old.clone()),
            ..Default::default()
        };
        manager
            .set_profile_session_options(profile_id, options.clone())
            .unwrap();
        let session = manager.open_mock_session(profile_id).unwrap();
        let mut surface = Surface::new(800, 600);
        surface.display.max_monitors = 2;
        surface.display.max_area = 2 * 800 * 600;
        let (rdp, rx) = handle(surface);
        manager.sessions.get_mut(&session).unwrap().rdp = Some(rdp);

        manager.set_active_rdp_monitor_count(2).unwrap();
        let ClientMsg::Input(InputEvent::MonitorLayout(layout)) = rx.recv().unwrap() else {
            panic!("expected an actual monitor layout command");
        };
        assert_eq!(layout.len(), 2);
        assert_eq!(
            manager
                .profile(profile_id)
                .unwrap()
                .session_options
                .rdp_monitors
                .as_ref()
                .unwrap()
                .len(),
            2
        );

        // A profile edit made after connection must not overwrite that session's
        // unrelated reconnect snapshot when choosing a monitor.
        options = manager.profile(profile_id).unwrap().session_options.clone();
        options.reconnect = Some(adit_domain::ReconnectPolicy {
            max_attempts: 7,
            ..Default::default()
        });
        manager
            .set_profile_session_options(profile_id, options)
            .unwrap();
        {
            let record = manager.sessions.get_mut(&session).unwrap();
            let mut surface = record.rdp.as_ref().unwrap().surface.lock().unwrap();
            surface.display.sent_layout = layout;
            surface.resize(1600, 600);
        }
        manager.select_active_rdp_monitor(Some(1)).unwrap();
        assert_eq!(
            manager
                .profile(profile_id)
                .unwrap()
                .session_options
                .rdp_selected_monitor,
            Some(1)
        );
        assert_eq!(
            manager.session_options(session).unwrap().reconnect,
            Some(old)
        );
        let saved = serde_json::to_string(manager.profile(profile_id).unwrap()).unwrap();
        let restored: adit_domain::ConnectionProfile = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.session_options.rdp_selected_monitor, Some(1));
        assert_eq!(restored.session_options.rdp_monitors.unwrap().len(), 2);
    }
    #[test]
    fn second_monitor_crop_and_input_use_identical_signed_layout_offset() {
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.match_layout_size();
        surface.selected_monitor = Some(1);
        for row in 0..200usize {
            for x in 0..400usize {
                let start = (row * 400 + x) * 4;
                surface.rgba[start..start + 4].copy_from_slice(&[
                    (x / 200) as u8,
                    row as u8,
                    x as u8,
                    255,
                ]);
            }
        }
        surface.generation = 3;
        let (handle, rx) = handle(surface);
        let frame = handle.frame_if_newer(0).unwrap();
        assert_eq!((frame.width, frame.height), (200, 200));
        assert_eq!(&frame.rgba[..4], &[1, 0, 200, 255]);
        assert_eq!(&frame.rgba[frame.rgba.len() - 4..], &[1, 199, 143, 255]);
        handle.send_input(InputEvent::MouseMove { x: 17, y: 23 });
        assert!(matches!(
            rx.recv().unwrap(),
            ClientMsg::Input(InputEvent::MouseMove { x: 217, y: 23 })
        ));
        handle.send_input(InputEvent::Resize {
            width: 800,
            height: 600,
        });
        assert!(
            rx.try_recv().is_err(),
            "automatic resize must not erase the layout"
        );
    }
    #[test]
    fn monitor_selection_waits_for_size_and_survives_resize_metadata() {
        let mut s = Surface::new(200, 200);
        s.display.sent_layout = monitors();
        s.selected_monitor = Some(1);
        s.match_layout_size();
        assert!(!s.display.applied);
        assert_eq!(s.viewport(), (0, 0, 200, 200));
        s.resize(400, 200);
        assert!(s.display.applied);
        assert_eq!(s.viewport(), (200, 0, 200, 200));
        s.resize(9000, 1);
        assert_eq!((s.width, s.height), (8192, 1));
        assert!(!s.display.applied);
    }
    #[test]
    fn ipc_cursor_stays_separate_from_frame_pixels_and_validates_malformed_bitmap() {
        let mut s = Surface::new(2, 2);
        let original = s.rgba.clone();
        let update = PointerUpdate::Bitmap {
            width: 2,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            rgba: vec![250, 120, 60, 128, 0, 255, 0, 255],
        };
        let mut pipe = Vec::new();
        write_msg(&mut pipe, &HostMsg::Pointer(update)).unwrap();
        let Some(HostMsg::Pointer(decoded)) = read_msg(&mut std::io::Cursor::new(pipe)).unwrap()
        else {
            panic!("pointer")
        };
        s.update_pointer(decoded);
        assert_eq!(s.pointer.as_ref().unwrap().hotspot_x, 1);
        assert_eq!(&s.pointer.as_ref().unwrap().rgba[..4], &[250, 120, 60, 128]);
        s.update_pointer(PointerUpdate::Bitmap {
            width: 2,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            rgba: vec![0],
        });
        assert_eq!(s.pointer.as_ref().unwrap().rgba.len(), 8);
        s.update_pointer(PointerUpdate::Position { x: 1, y: 1 });
        assert_eq!(s.pointer_position, Some((1, 1)));
        let warp_generation = s.pointer_position_generation;
        s.update_pointer(PointerUpdate::Position { x: 1, y: 1 });
        assert_ne!(
            s.pointer_position_generation, warp_generation,
            "repeated server warps must not disappear"
        );
        s.update_pointer(PointerUpdate::Hidden);
        assert_eq!(s.pointer.as_ref().unwrap().width, 0);
        s.update_pointer(PointerUpdate::Default);
        assert!(s.pointer.is_none());
        assert_eq!(s.rgba, original);
        assert_eq!(
            s.generation, 0,
            "pointer changes must not trigger full framebuffer upload"
        );
    }
}
