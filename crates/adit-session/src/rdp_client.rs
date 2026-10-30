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
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

fn display_error(error: &str) -> &str {
    match error {
        "DisplayControl capabilities timed out" => "等待服务端显示能力超时",
        "DisplayControl channel closed"
        | "DisplayControl channel disappeared"
        | "DisplayControl channel not connected" => "显示控制通道已关闭",
        "DisplayControl processor unavailable" => "显示控制通道不可用",
        "DisplayControl capabilities not ready" => "服务端显示能力尚未就绪",
        "requested layout exceeds server monitor count/area capability" => {
            "显示器布局超过服务端能力"
        }
        _ => error,
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
    ClipboardText {
        text: String,
        owner: RdpClipboardToken,
    },
    VncClipboardText(vnc::ClipboardText),
    /// The remote copied files. The app should offer them on the Windows
    /// clipboard; nothing crosses the wire until something here pastes.
    ClipboardFiles {
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// The remote copied an image; raw `CF_DIB` bytes for the app to put on the
    /// Windows clipboard.
    ClipboardImage {
        image: Vec<u8>,
        owner: RdpClipboardToken,
    },
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
        epoch: u64,
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

type Viewport = (u16, u16, u16, u16);
type PointerLocation = (u16, u16, Viewport);

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
    layout_pending: Option<PendingLayout>,
    next_layout_id: u64,
    redirect_epoch: u64,
    blocked_resize: Option<(u16, u16)>,
    latest_resize: Option<(u16, u16)>,
    resize_revision: u64,
    resize_pending: Option<PendingResize>,
    desired_layout: Option<(Vec<Monitor>, Option<usize>)>,
    input_epoch: u64,
    pointer_epoch: Option<u64>,
    pressed_buttons: Vec<(adit_rdp_proto::MouseButton, u64, u64)>,
    last_pointer: Option<PointerLocation>,
    awaiting_layout_pixels: bool,
    /// A complete current-connection picture received before the layout result.
    /// It may be shown while a sent layout waits for newer pixels, but cannot
    /// authorize input for that layout.
    early_presentable_frame: bool,
    clear_before_layout_pixels: bool,
    painted_layout_pixels: Option<PaintedPixels>,
    paint_deadline: Option<Instant>,
    closed: bool,
    pointer_paused: bool,
    may_resume_pointer: bool,
}

// The first post-layout tiles may cover only part of the framebuffer. A bit
// per pixel prevents input into the still-blank remainder until it is painted.
struct PaintedPixels {
    width: usize,
    bits: Vec<u64>,
    remaining: usize,
}

impl PaintedPixels {
    fn new(width: usize, height: usize) -> Self {
        let pixels = width * height;
        Self {
            width,
            bits: vec![0; pixels.div_ceil(64)],
            remaining: pixels,
        }
    }

    fn mark(&mut self, x: usize, y: usize, width: usize, height: usize) {
        for row in y..y + height {
            let mut start = row * self.width + x;
            let end = start + width;
            while start < end {
                let offset = start % 64;
                let count = (64 - offset).min(end - start);
                let mask = (u64::MAX >> (64 - count)) << offset;
                let word = &mut self.bits[start / 64];
                self.remaining -= (mask & !*word).count_ones() as usize;
                *word |= mask;
                start += count;
            }
        }
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        let index = usize::from(y) * self.width + usize::from(x);
        self.bits
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }
}

// Includes the helper's 30-second connection deadline. Once it has sent the
// layout, allow a separate short window for a matching framebuffer size.
const LAYOUT_HELPER_TIMEOUT: Duration = Duration::from_secs(40);
const LAYOUT_SIZE_TIMEOUT: Duration = Duration::from_secs(8);
const PAINT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(20);
static NEXT_INPUT_EPOCH: AtomicU64 = AtomicU64::new(1);

fn next_input_epoch() -> u64 {
    NEXT_INPUT_EPOCH.fetch_add(1, Ordering::Relaxed).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutPhase {
    WaitingForHelper,
    WaitingForSize,
}

#[derive(Clone, Copy)]
struct PendingResize {
    revision: u64,
    expected: (u16, u16),
    sent: bool,
    requires_transition: bool,
    saw_other_size: bool,
}

struct PendingLayout {
    id: u64,
    monitors: Vec<Monitor>,
    selected: Option<usize>,
    phase: LayoutPhase,
    deadline: Instant,
    previous_layout_safe: bool,
    previous_may_resume_pointer: bool,
    has_fresh_pixels: bool,
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
            layout_pending: None,
            next_layout_id: 0,
            redirect_epoch: 0,
            blocked_resize: None,
            latest_resize: None,
            resize_revision: 0,
            resize_pending: None,
            desired_layout: None,
            input_epoch: next_input_epoch(),
            pointer_epoch: None,
            pressed_buttons: Vec::new(),
            last_pointer: None,
            awaiting_layout_pixels: false,
            early_presentable_frame: false,
            clear_before_layout_pixels: false,
            painted_layout_pixels: None,
            paint_deadline: None,
            closed: false,
            pointer_paused: false,
            may_resume_pointer: true,
        }
    }

    fn invalidate_pointer_mapping(&mut self) {
        self.input_epoch = next_input_epoch();
        self.pointer_epoch = None;
        self.last_pointer = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn viewport(&self) -> Viewport {
        if self.display.size_matches {
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
        next.pressed_buttons = std::mem::take(&mut self.pressed_buttons);
        next.display = std::mem::take(&mut self.display);
        next.selected_monitor = self.selected_monitor;
        next.layout_pending = self.layout_pending.take();
        next.next_layout_id = self.next_layout_id;
        next.redirect_epoch = self.redirect_epoch;
        next.blocked_resize = self.blocked_resize.take();
        next.latest_resize = self.latest_resize;
        next.resize_revision = self.resize_revision;
        next.resize_pending = self.resize_pending;
        if let Some(resize) = next.resize_pending.as_mut() {
            if resize.sent && resize.expected != (next.width, next.height) {
                resize.saw_other_size = true;
            }
        }
        next.desired_layout = self.desired_layout.take();
        next.awaiting_layout_pixels = self.awaiting_layout_pixels;
        next.early_presentable_frame = self.early_presentable_frame;
        next.clear_before_layout_pixels = self.clear_before_layout_pixels;
        if (self.width, self.height) == (next.width, next.height) {
            next.paint_deadline = self.paint_deadline;
        }
        next.closed = self.closed;
        next.pointer_paused = self.pointer_paused;
        next.may_resume_pointer = self.may_resume_pointer;
        if (self.width, self.height) == (next.width, next.height) {
            next.painted_layout_pixels = self.painted_layout_pixels.take();
        } else {
            next.awaiting_layout_pixels = true;
            next.early_presentable_frame = false;
            next.clear_before_layout_pixels = true;
            if next.pointer_paused && next.layout_pending.is_none() {
                next.awaiting_layout_pixels = false;
                next.clear_before_layout_pixels = false;
                next.painted_layout_pixels = Some(PaintedPixels::new(
                    usize::from(next.width),
                    usize::from(next.height),
                ));
            }
        }
        if let Some(pending) = next.layout_pending.as_mut() {
            pending.has_fresh_pixels = false;
        }
        next.input_epoch = self.input_epoch;
        next.generation = self.generation;
        next.invalidate_pointer_mapping();
        next.match_layout_size();
        *self = next;
    }

    fn match_layout_size(&mut self) {
        self.display.size_matches = monitor_bounds(&self.display.sent_layout)
            .is_ok_and(|(_, _, w, h)| (w, h) == (self.width, self.height));
    }

    fn finish_matching_layout(&mut self) -> Option<(u16, u16)> {
        if self.display.size_matches
            && self.layout_pending.as_ref().is_some_and(|pending| {
                pending.phase == LayoutPhase::WaitingForSize && pending.has_fresh_pixels
            })
        {
            let pending = self.layout_pending.take().expect("checked above");
            self.selected_monitor = pending.selected;
            if self.painted_layout_pixels.is_none() && self.may_resume_pointer {
                self.pointer_paused = false;
                self.display.error = None;
            }
            self.invalidate_pointer_mapping();
            if self.display.sent_layout.len() <= 1 {
                return self.blocked_resize.take();
            }
        }
        None
    }

    fn fail_layout(&mut self, error: &str, known_unsent: bool) -> Option<(u16, u16)> {
        let pending = self.layout_pending.take();
        let unsent = known_unsent
            && pending.as_ref().is_some_and(|pending| {
                pending.phase == LayoutPhase::WaitingForHelper && pending.previous_layout_safe
            });
        if unsent {
            self.may_resume_pointer = pending
                .as_ref()
                .expect("checked above")
                .previous_may_resume_pointer;
        }
        if pending.is_some() && !unsent {
            self.may_resume_pointer = false;
            self.pause_pointer(error);
        }
        let old_layout_usable =
            unsent && self.display.size_matches && !self.display.sent_layout.is_empty();
        if !old_layout_usable {
            self.display.sent_layout.clear();
            self.display.size_matches = false;
            self.selected_monitor = None;
        }
        if !self.pointer_paused {
            self.display.error = Some(error.to_owned());
        }
        self.invalidate_pointer_mapping();
        if old_layout_usable && self.display.sent_layout.len() > 1 {
            None
        } else {
            self.blocked_resize.take()
        }
    }

    fn expire_layout(&mut self, now: Instant) -> Option<(u64, Option<(u16, u16)>)> {
        let pending = self.layout_pending.as_ref()?;
        if now < pending.deadline {
            return None;
        }
        let id = pending.id;
        let error = match pending.phase {
            LayoutPhase::WaitingForHelper => "显示器布局请求等待 helper 响应超时",
            LayoutPhase::WaitingForSize => "布局已发送，但桌面尺寸等待超时",
        };
        Some((id, self.fail_layout(error, false)))
    }

    fn close_layout(&mut self) {
        self.closed = true;
        self.paint_deadline = None;
        self.layout_pending = None;
        self.display.sent_layout.clear();
        self.display.size_matches = false;
        self.selected_monitor = None;
        self.blocked_resize = None;
        self.invalidate_pointer_mapping();
    }

    fn pause_pointer(&mut self, error: &str) {
        if self.painted_layout_pixels.is_none() {
            self.painted_layout_pixels = Some(PaintedPixels::new(
                usize::from(self.width),
                usize::from(self.height),
            ));
        }
        self.awaiting_layout_pixels = false;
        self.clear_before_layout_pixels = false;
        self.paint_deadline = None;
        self.pointer_paused = true;
        self.display.error = Some(format!("{error}；鼠标已暂停，可手动重新连接"));
        self.invalidate_pointer_mapping();
    }

    fn display_capabilities(&mut self, state: DisplayControlState) -> Option<(u16, u16)> {
        let closed = self.display.max_monitors > 0 && state.max_monitors == 0;
        self.display.max_monitors = state.max_monitors;
        self.display.max_area = state.max_area;
        self.generation = self.generation.wrapping_add(1);
        if closed
            && self
                .layout_pending
                .as_ref()
                .is_some_and(|pending| pending.phase == LayoutPhase::WaitingForHelper)
        {
            // A capability snapshot can have been queued before the current
            // request. Its result (or its deadline) owns that request instead.
            self.display.sent_layout.clear();
            self.display.size_matches = false;
            self.selected_monitor = None;
            self.invalidate_pointer_mapping();
            None
        } else {
            closed
                .then(|| self.fail_layout("显示控制通道已关闭", false))
                .flatten()
        }
    }

    fn layout_result(
        &mut self,
        request_id: u64,
        result: Result<(), String>,
        now: Instant,
    ) -> Option<(u16, u16)> {
        let pending = self.layout_pending.as_ref()?;
        if pending.id != request_id || pending.phase != LayoutPhase::WaitingForHelper {
            return None;
        }
        if let Err(error) = result {
            return self.fail_layout(display_error(&error), true);
        }
        self.display.sent_layout = pending.monitors.clone();
        self.may_resume_pointer = true;
        if !self.pointer_paused {
            self.display.error = None;
        }
        self.match_layout_size();
        self.awaiting_layout_pixels = true;
        self.clear_before_layout_pixels = true;
        let pending = self.layout_pending.as_mut().expect("checked above");
        pending.phase = LayoutPhase::WaitingForSize;
        pending.deadline = now + LAYOUT_SIZE_TIMEOUT;
        self.generation = self.generation.wrapping_add(1);
        self.finish_matching_layout()
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
        if tw == 0 || th == 0 || x + tw > sw || y + th > sh || rgba.len() < tw * th * 4 {
            return;
        }
        if let Some(resize) = self.resize_pending {
            if !resize.sent
                || resize.expected != (self.width, self.height)
                || (resize.requires_transition && !resize.saw_other_size)
            {
                return;
            }
            // Only pixels received after the matching helper write confirmation
            // can start the new resize's coverage.
            self.resize_pending = None;
        }
        let layout_pixels = self.layout_pending.as_ref().map_or(
            self.awaiting_layout_pixels,
            |pending| match pending.phase {
                // On redirect the first frame can arrive while the saved layout
                // is still waiting for the helper. Track it for an unsent
                // failure, but do not treat it as proof of a sent layout.
                LayoutPhase::WaitingForHelper => self.awaiting_layout_pixels,
                LayoutPhase::WaitingForSize => self.display.size_matches,
            },
        );
        if layout_pixels {
            // Preserve the previous picture while tracking which pixels are
            // fresh. Unpainted areas cannot receive pointer input.
            if self.clear_before_layout_pixels {
                self.painted_layout_pixels = Some(PaintedPixels::new(sw, sh));
                self.clear_before_layout_pixels = false;
            }
            self.awaiting_layout_pixels = false;
            if let Some(pending) = self
                .layout_pending
                .as_mut()
                .filter(|pending| pending.phase == LayoutPhase::WaitingForSize)
            {
                pending.has_fresh_pixels = true;
            }
        }
        for row in 0..th {
            let dst = ((y + row) * sw + x) * 4;
            let src = row * tw * 4;
            self.rgba[dst..dst + tw * 4].copy_from_slice(&rgba[src..src + tw * 4]);
        }
        let mut coverage_changed = false;
        if let Some(painted) = &mut self.painted_layout_pixels {
            let remaining = painted.remaining;
            painted.mark(x, y, tw, th);
            coverage_changed = painted.remaining != remaining;
            if painted.remaining == 0 {
                if self
                    .layout_pending
                    .as_ref()
                    .is_some_and(|pending| pending.phase == LayoutPhase::WaitingForHelper)
                {
                    self.early_presentable_frame = true;
                }
                self.painted_layout_pixels = None;
                self.paint_deadline = None;
                if self.pointer_paused && self.may_resume_pointer && self.layout_pending.is_none() {
                    self.pointer_paused = false;
                    self.display.error = None;
                }
            }
        }
        if coverage_changed {
            self.input_epoch = next_input_epoch();
            self.pointer_epoch = None;
        }
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Debug)]
struct FileEpoch {
    generation: AtomicU64,
    connection_epoch: AtomicU64,
    clipboard_revision: AtomicU64,
    closed: AtomicBool,
    enabled: AtomicBool,
}
impl Default for FileEpoch {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            connection_epoch: AtomicU64::new(0),
            clipboard_revision: AtomicU64::new(0),
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

/// Proves that a queued text or image still belongs to the same helper and
/// redirection epoch when the GUI consumes it.
#[derive(Debug, Clone)]
pub struct RdpClipboardToken {
    current: Arc<FileEpoch>,
    epoch: u64,
    revision: u64,
}

impl RdpClipboardToken {
    pub(crate) fn is_current(&self) -> bool {
        !self.current.closed.load(Ordering::Acquire)
            && self.current.enabled.load(Ordering::Acquire)
            && self.current.connection_epoch.load(Ordering::Acquire) == self.epoch
            && self.current.clipboard_revision.load(Ordering::Acquire) == self.revision
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
    /// Identifies the viewport used to crop these pixels, independently of
    /// frequent pixel updates. VNC does not use this RDP-specific lease.
    pub rdp_input_epoch: Option<u64>,
}

pub struct RdpCursorSnapshot {
    pub shape: Option<super::VncCursor>,
    pub position: Option<(u16, u16)>,
    pub position_generation: u64,
}

fn send_deferred_resize(
    surface: &mut Surface,
    tx: &std_mpsc::Sender<ClientMsg>,
    size: Option<(u16, u16)>,
) {
    if let Some((width, height)) = size {
        let expected = ((width & !1).clamp(200, 8192), height.clamp(200, 8192));
        if surface
            .resize_pending
            .is_some_and(|resize| resize.expected == expected)
        {
            return;
        }
        let requires_transition =
            surface.resize_pending.is_some() && expected == (surface.width, surface.height);
        let Some(revision) = surface.resize_revision.checked_add(1) else {
            surface.pause_pointer("RDP 桌面尺寸请求编号已耗尽");
            return;
        };
        if tx
            .send(ClientMsg::EpochInput {
                epoch: surface.redirect_epoch,
                event: InputEvent::Resize { width, height },
            })
            .is_ok()
        {
            surface.resize_revision = revision;
            surface.resize_pending = Some(PendingResize {
                revision,
                expected,
                sent: false,
                requires_transition,
                saw_other_size: false,
            });
            surface.awaiting_layout_pixels = true;
            surface.early_presentable_frame = false;
            surface.clear_before_layout_pixels = true;
            surface.painted_layout_pixels = None;
            surface.invalidate_pointer_mapping();
        }
    }
}

fn expire_layout(
    surface: &mut Surface,
    tx: &std_mpsc::Sender<ClientMsg>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    now: Instant,
) -> bool {
    let Some((request_id, resize)) = surface.expire_layout(now) else {
        return false;
    };
    let epoch = surface.redirect_epoch;
    let _ = tx.send(ClientMsg::EpochInput {
        epoch,
        event: InputEvent::CancelMonitorLayout { request_id },
    });
    // Keep the healthy connection and keyboard alive while the uncertain
    // mapping is paused; a fresh layout or manual reconnect can recover it.
    let _ = events.send(RdpClientEvent::Notice(
        "RDP 显示器布局恢复超时；鼠标已暂停，可手动重新连接".into(),
    ));
    send_deferred_resize(surface, tx, resize);
    true
}

fn start_layout_watchdog(
    surface: &Arc<Mutex<Surface>>,
    tx: &Arc<std_mpsc::Sender<ClientMsg>>,
    wake: &Arc<Condvar>,
    events: std_mpsc::Sender<RdpClientEvent>,
    request_id: u64,
) -> std::io::Result<()> {
    let surface = Arc::downgrade(surface);
    let tx = Arc::downgrade(tx);
    let wake = Arc::clone(wake);
    thread::Builder::new()
        .name("adit-rdp-layout-deadline".into())
        .spawn(move || {
            let Some(surface) = surface.upgrade() else {
                return;
            };
            let Ok(mut surface) = surface.lock() else {
                return;
            };
            loop {
                let Some(pending) = surface
                    .layout_pending
                    .as_ref()
                    .filter(|p| p.id == request_id)
                else {
                    return;
                };
                let remaining = pending.deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    if let Some(tx) = tx.upgrade() {
                        expire_layout(&mut surface, &tx, &events, Instant::now());
                    } else {
                        let _ = surface.expire_layout(Instant::now());
                    }
                    wake.notify_all();
                    return;
                }
                let Ok((next, _)) = wake.wait_timeout(surface, remaining) else {
                    return;
                };
                surface = next;
            }
        })?;
    Ok(())
}

fn ensure_paint_watchdog(
    surface: &mut Surface,
    shared: &Arc<Mutex<Surface>>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    wake: &Arc<Condvar>,
) {
    if surface.layout_pending.is_some()
        || surface.closed
        || surface.pointer_paused
        || (!surface.awaiting_layout_pixels && surface.painted_layout_pixels.is_none())
        || surface.paint_deadline.is_some()
    {
        return;
    }
    let deadline = Instant::now() + PAINT_RECOVERY_TIMEOUT;
    surface.paint_deadline = Some(deadline);
    let shared = Arc::downgrade(shared);
    let worker_events = events.clone();
    let wake = Arc::clone(wake);
    if thread::Builder::new()
        .name("adit-rdp-paint-deadline".into())
        .spawn(move || {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            let Ok(mut surface) = shared.lock() else {
                return;
            };
            loop {
                let Some(deadline) = surface.paint_deadline else {
                    return;
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    surface.pause_pointer("RDP 桌面画面未完整恢复");
                    let _ = worker_events.send(RdpClientEvent::Notice(
                        "RDP 桌面画面未完整恢复；鼠标已暂停，可手动重新连接".into(),
                    ));
                    return;
                }
                let Ok((next, _)) = wake.wait_timeout(surface, remaining) else {
                    return;
                };
                surface = next;
            }
        })
        .is_err()
    {
        surface.pause_pointer("无法启动 RDP 画面恢复计时");
        let _ = events.send(RdpClientEvent::Notice(
            "无法启动 RDP 画面恢复计时；鼠标已暂停，可手动重新连接".into(),
        ));
    }
}

// The caller holds the surface lock so a deadline, a reconnect, and a new user
// request cannot reorder their command-channel writes.
fn queue_monitor_layout(
    surface: &mut Surface,
    shared_surface: &Arc<Mutex<Surface>>,
    tx: &Arc<std_mpsc::Sender<ClientMsg>>,
    wake: &Arc<Condvar>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    monitors: Vec<Monitor>,
    selected: Option<usize>,
) -> Result<(), String> {
    if surface.closed {
        return Err("RDP 会话已关闭".into());
    }
    surface.next_layout_id = surface
        .next_layout_id
        .checked_add(1)
        .ok_or("显示器布局请求编号已耗尽")?;
    let request_id = surface.next_layout_id;
    let previous_layout_safe = surface.layout_pending.is_none() && surface.resize_pending.is_none();
    let previous_may_resume_pointer = surface.may_resume_pointer;
    let previous_desired = surface.desired_layout.clone();
    surface.layout_pending = Some(PendingLayout {
        id: request_id,
        monitors: monitors.clone(),
        selected,
        phase: LayoutPhase::WaitingForHelper,
        deadline: Instant::now() + LAYOUT_HELPER_TIMEOUT,
        previous_layout_safe,
        previous_may_resume_pointer,
        has_fresh_pixels: false,
    });
    surface.resize_pending = None;
    surface.paint_deadline = None;
    surface.may_resume_pointer = false;
    surface.desired_layout = Some((monitors.clone(), selected));
    surface.invalidate_pointer_mapping();
    if !surface.pointer_paused {
        surface.display.error = None;
    }
    wake.notify_all();
    if let Err(error) = start_layout_watchdog(shared_surface, tx, wake, events.clone(), request_id)
    {
        surface.desired_layout = previous_desired;
        let resize = surface.fail_layout("无法启动显示器布局超时计时", true);
        send_deferred_resize(surface, tx, resize);
        ensure_paint_watchdog(surface, shared_surface, events, wake);
        return Err(error.to_string());
    }
    if tx
        .send(ClientMsg::EpochInput {
            epoch: surface.redirect_epoch,
            event: InputEvent::MonitorLayout {
                request_id,
                monitors,
            },
        })
        .is_err()
    {
        surface.desired_layout = previous_desired;
        let resize = surface.fail_layout("RDP helper 命令通道已关闭", true);
        send_deferred_resize(surface, tx, resize);
        ensure_paint_watchdog(surface, shared_surface, events, wake);
        wake.notify_all();
        return Err("RDP helper 命令通道已关闭".to_owned());
    }
    Ok(())
}

/// Handle to a live RDP session running in the helper process.
pub struct RdpClientHandle {
    remote_offer: Arc<FileEpoch>,
    event_tx: std_mpsc::Sender<RdpClientEvent>,
    cmd_tx: Arc<std_mpsc::Sender<ClientMsg>>,
    event_rx: std_mpsc::Receiver<RdpClientEvent>,
    surface: Arc<Mutex<Surface>>,
    layout_wake: Arc<Condvar>,
    // Populated asynchronously: the helper is spawned on a background thread so the
    // UI thread never blocks on `Command::spawn` (see `spawn_rdp_session`). `None`
    // until the spawn completes.
    child: Arc<Mutex<Option<Child>>>,
}

impl RdpClientHandle {
    #[cfg(test)]
    pub(crate) fn clipboard_token(&self, epoch: u64) -> RdpClipboardToken {
        RdpClipboardToken {
            current: Arc::clone(&self.remote_offer),
            epoch,
            revision: self.remote_offer.clipboard_revision.load(Ordering::Acquire),
        }
    }

    pub(crate) fn owns_clipboard_token(&self, token: &RdpClipboardToken) -> bool {
        Arc::ptr_eq(&self.remote_offer, &token.current) && token.is_current()
    }
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
                cmd_tx: Arc::new(cmd_tx),
                event_rx,
                surface: Arc::new(Mutex::new(Surface::new(1, 1))),
                layout_wake: Arc::new(Condvar::new()),
                child: Arc::new(Mutex::new(None)),
            },
            cmd_rx,
            event_tx,
        )
    }

    /// Send non-pointer input or a button release. Pointer moves, presses and
    /// wheels must carry the viewport lease from `send_pointer_input`.
    pub fn send_input(&self, event: InputEvent) {
        if matches!(
            event,
            InputEvent::MouseMove { .. }
                | InputEvent::MouseButton { pressed: true, .. }
                | InputEvent::Wheel { .. }
        ) {
            return;
        }
        let Ok(mut surface) = self.surface.lock() else {
            return;
        };
        if surface.closed {
            return;
        }
        if let InputEvent::MouseButton {
            button,
            pressed: false,
        } = &event
        {
            let Some(index) = surface
                .pressed_buttons
                .iter()
                .position(|(held, epoch, _)| held == button && *epoch == surface.redirect_epoch)
            else {
                return;
            };
            let (_, epoch, _) = surface.pressed_buttons.remove(index);
            let _ = self.cmd_tx.send(ClientMsg::EpochInput { epoch, event });
            return;
        }
        if let InputEvent::Resize { width, height } = &event {
            surface.latest_resize = Some((*width, *height));
        }
        match &event {
            // Window resize must not replace a multi-monitor layout with
            // ActiveStage's single-primary resize message.
            InputEvent::Resize { width, height }
                if surface.layout_pending.is_some()
                    || (surface.display.size_matches && surface.display.sent_layout.len() > 1) =>
            {
                surface.blocked_resize = Some((*width, *height));
                return;
            }
            InputEvent::Resize { width, height } => {
                surface.blocked_resize = None;
                send_deferred_resize(&mut surface, &self.cmd_tx, Some((*width, *height)));
                ensure_paint_watchdog(
                    &mut surface,
                    &self.surface,
                    &self.event_tx,
                    &self.layout_wake,
                );
                self.layout_wake.notify_all();
                return;
            }
            _ => {}
        }
        // Keep this lock until the unbounded send completes: otherwise a new
        // layout could be queued between the resize check and the resize send.
        let _ = self.cmd_tx.send(ClientMsg::EpochInput {
            epoch: surface.redirect_epoch,
            event,
        });
    }

    /// A pointer event is valid only for the viewport attached to the image
    /// that produced it. Releases remain unconditional to avoid stuck buttons.
    pub fn send_pointer_input(&self, mut event: InputEvent, epoch: Option<u64>) {
        let Ok(mut surface) = self.surface.lock() else {
            return;
        };
        if surface.closed {
            return;
        }
        if let InputEvent::MouseButton {
            button,
            pressed: false,
        } = &event
        {
            if let Some(index) =
                surface
                    .pressed_buttons
                    .iter()
                    .position(|(held, connection, lease)| {
                        held == button
                            && *connection == surface.redirect_epoch
                            && epoch == Some(*lease)
                    })
            {
                let (_, connection, _) = surface.pressed_buttons.remove(index);
                let _ = self.cmd_tx.send(ClientMsg::EpochInput {
                    epoch: connection,
                    event,
                });
            }
            return;
        }
        let Some(epoch) = epoch else { return };
        if surface.layout_pending.is_some()
            || surface.resize_pending.is_some()
            || surface.pointer_paused
            || surface.awaiting_layout_pixels
            || epoch != surface.input_epoch
        {
            return;
        }
        match &mut event {
            InputEvent::MouseMove { x, y } => {
                let (left, top, width, height) = surface.viewport();
                *x = (*x).min(width - 1).saturating_add(left);
                *y = (*y).min(height - 1).saturating_add(top);
                surface.last_pointer = Some((*x, *y, (left, top, width, height)));
                if surface
                    .painted_layout_pixels
                    .as_ref()
                    .is_some_and(|painted| !painted.contains(*x, *y))
                {
                    surface.pointer_epoch = None;
                    return;
                }
                surface.pointer_epoch = Some(epoch);
            }
            InputEvent::MouseButton { pressed: true, .. } | InputEvent::Wheel { .. }
                if surface.pointer_epoch != Some(epoch) =>
            {
                let Some((x, y, viewport)) = surface.last_pointer else {
                    return;
                };
                if viewport != surface.viewport()
                    || surface
                        .painted_layout_pixels
                        .as_ref()
                        .is_some_and(|painted| !painted.contains(x, y))
                {
                    return;
                }
                let _ = self.cmd_tx.send(ClientMsg::EpochInput {
                    epoch: surface.redirect_epoch,
                    event: InputEvent::MouseMove { x, y },
                });
                surface.pointer_epoch = Some(epoch);
            }
            InputEvent::MouseButton { .. } | InputEvent::Wheel { .. } => {}
            _ => return,
        }
        if let InputEvent::MouseButton {
            button,
            pressed: true,
        } = &event
        {
            surface
                .pressed_buttons
                .retain(|(held, _, _)| held != button);
            let held = (*button, surface.redirect_epoch, epoch);
            surface.pressed_buttons.push(held);
        }
        let _ = self.cmd_tx.send(ClientMsg::EpochInput {
            epoch: surface.redirect_epoch,
            event,
        });
    }

    /// Offer a locally-copied image to the remote clipboard (raw `CF_DIB`).
    pub fn send_clipboard_image(&self, image: Vec<u8>) {
        if let Ok(surface) = self.surface.lock() {
            if !surface.closed {
                let _ = self.cmd_tx.send(ClientMsg::EpochClipboardImage {
                    epoch: surface.redirect_epoch,
                    image,
                });
            }
        }
    }

    /// Offer a local file selection to the remote clipboard. Metadata only —
    /// nothing is read from disk until the remote pastes.
    pub fn send_clipboard_files(&self, files: Vec<ClipFile>) {
        if let Ok(surface) = self.surface.lock() {
            if !surface.closed || files.is_empty() {
                let _ = self.cmd_tx.send(ClientMsg::EpochClipboardFiles {
                    epoch: surface.redirect_epoch,
                    files,
                });
            }
        }
    }

    /// Answer a [`RdpClientEvent::FileContentsNeeded`]. `None` reports a failed
    /// read, which becomes the protocol's error response.
    pub fn answer_file_contents(&self, epoch: u64, stream_id: u32, data: Option<Vec<u8>>) {
        let _ = self.cmd_tx.send(ClientMsg::FileContents {
            epoch,
            stream_id,
            data,
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
            self.remote_offer
                .clipboard_revision
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn file_requester_for_generation(&self, generation: u64) -> RdpFileRequester {
        RdpFileRequester {
            tx: Arc::new(Mutex::new(self.cmd_tx.as_ref().clone())),
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
        if let Ok(mut surface) = self.surface.lock() {
            surface.close_layout();
        }
        self.layout_wake.notify_all();
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
        // A resized buffer initially contains copied pixels, not a frame from
        // the new layout. Keep the renderer's old snapshot until pixels arrive.
        if (surface.awaiting_layout_pixels && !surface.early_presentable_frame)
            || surface.generation == last_generation
        {
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
            rdp_input_epoch: (surface.layout_pending.is_none()
                && surface.resize_pending.is_none()
                && !surface.awaiting_layout_pixels
                && !surface.pointer_paused)
                .then_some(surface.input_epoch),
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
        let mut surface = self.surface.lock().map_err(|_| "无法读取显示器布局")?;
        queue_monitor_layout(
            &mut surface,
            &self.surface,
            &self.cmd_tx,
            &self.layout_wake,
            &self.event_tx,
            monitors,
            selected,
        )
    }
}

impl Drop for RdpClientHandle {
    fn drop(&mut self) {
        self.remote_offer.closed.store(true, Ordering::Release);
        self.remote_offer.generation.store(0, Ordering::Release);
        if let Ok(mut surface) = self.surface.lock() {
            surface.close_layout();
        }
        self.layout_wake.notify_all();
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
    let cmd_tx = Arc::new(cmd_tx);
    let layout_wake = Arc::new(Condvar::new());
    // A clone for the stdout reader, so it can answer a file-contents request
    // with a refusal itself. The reader must not depend on the app being awake:
    // a remote paste blocks on the stream it asked for.
    let cmd_tx_reader = cmd_tx.clone();
    let layout_wake_reader = Arc::clone(&layout_wake);
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
                    if let Ok(mut surface) = surface_bg.lock() {
                        surface.close_layout();
                    }
                    layout_wake_reader.notify_all();
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
                (&surface_bg, Arc::clone(&layout_wake_reader)),
                remote_offer_bg.clone(),
            ) {
                if let Ok(mut surface) = surface_bg.lock() {
                    surface.close_layout();
                }
                layout_wake_reader.notify_all();
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
        layout_wake,
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
    refuse_tx: Arc<std_mpsc::Sender<ClientMsg>>,
    event_tx: std_mpsc::Sender<RdpClientEvent>,
    display: (&Arc<Mutex<Surface>>, Arc<Condvar>),
    remote_offer: Arc<FileEpoch>,
) -> Result<(), RdpClientError> {
    let (surface, layout_wake) = display;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdout unavailable".into()))?;

    // Merge blocker: #97 changes the bincode schema, while this legacy write
    // sends credentials before the helper confirms a version. Integrate the
    // shared #94 handshake and mixed-version tests before shipping either end;
    // the combined epoch-bearing schema must use a version newer than #94's v8.
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
            read_helper_messages(
                stdout,
                reader_surface,
                event_tx,
                refuse_tx,
                remote_offer,
                layout_wake,
            )
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    Ok(())
}

fn read_helper_messages<R: std::io::Read>(
    mut stdout: R,
    reader_surface: Arc<Mutex<Surface>>,
    event_tx: std_mpsc::Sender<RdpClientEvent>,
    refuse_tx: Arc<std_mpsc::Sender<ClientMsg>>,
    remote_offer: Arc<FileEpoch>,
    layout_wake: Arc<Condvar>,
) {
    loop {
        match read_msg::<_, HostMsg>(&mut stdout) {
            Ok(Some(HostMsg::Connected { width, height })) => {
                if let Ok(mut s) = reader_surface.lock() {
                    if s.closed {
                        continue;
                    }
                    expire_layout(&mut s, &refuse_tx, &event_tx, Instant::now());
                    s.resize(width, height);
                    let resize = s.finish_matching_layout();
                    send_deferred_resize(&mut s, &refuse_tx, resize);
                    ensure_paint_watchdog(&mut s, &reader_surface, &event_tx, &layout_wake);
                    layout_wake.notify_all();
                }
                let _ = event_tx.send(RdpClientEvent::Connected { width, height });
            }
            Ok(Some(HostMsg::Resized { width, height })) => {
                if let Ok(mut s) = reader_surface.lock() {
                    if s.closed {
                        continue;
                    }
                    expire_layout(&mut s, &refuse_tx, &event_tx, Instant::now());
                    s.resize(width, height);
                    let resize = s.finish_matching_layout();
                    send_deferred_resize(&mut s, &refuse_tx, resize);
                    ensure_paint_watchdog(&mut s, &reader_surface, &event_tx, &layout_wake);
                    layout_wake.notify_all();
                }
                let _ = event_tx.send(RdpClientEvent::Resized { width, height });
            }
            // Clipboard file transfer. The helper speaks the whole
            // MS-RDPECLIP file protocol, but the half that has to answer
            // these lives in the GUI process and is not built yet: it
            // needs a Win32 `CF_HDROP` reader and a COM `IDataObject`
            // with delay-rendered `IStream`s, neither of which iced's
            // text-only clipboard can express.
            //
            // Answering `FileContentsNeeded` with a refusal rather than
            // dropping it is the important part — a remote paste blocks
            // on the stream it asked for, so silence would hang Explorer
            // over there instead of failing it.
            Ok(Some(HostMsg::ClipboardImage(_))) | Ok(Some(HostMsg::ClipboardText(_))) => {
                // Legacy messages cannot be attributed to a redirect epoch.
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
                epoch,
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
                        epoch,
                        stream_id,
                        index,
                        offset,
                        length,
                    })
                    .is_err()
                {
                    let _ = refuse_tx.send(ClientMsg::FileContents {
                        epoch,
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
                    if s.closed {
                        continue;
                    }
                    s.update_pointer(pointer);
                }
            }
            Ok(Some(HostMsg::InputDiscarded)) => {
                let _ = event_tx.send(RdpClientEvent::Notice(
                    "重定向期间有键盘输入未发送，或剪贴板命令未发送，请重试".into(),
                ));
            }
            Ok(Some(HostMsg::DisplayControl { epoch, state })) => {
                if let Ok(mut s) = reader_surface.lock() {
                    if s.closed || epoch != s.redirect_epoch {
                        continue;
                    }
                    let resize = s.display_capabilities(state);
                    send_deferred_resize(&mut s, &refuse_tx, resize);
                    layout_wake.notify_all();
                }
            }
            Ok(Some(HostMsg::MonitorLayoutResult {
                epoch,
                request_id,
                result,
            })) => {
                if let Ok(mut s) = reader_surface.lock() {
                    if s.closed || epoch != s.redirect_epoch {
                        continue;
                    }
                    let now = Instant::now();
                    if expire_layout(&mut s, &refuse_tx, &event_tx, now) {
                        continue;
                    }
                    let resize = s.layout_result(request_id, result, now);
                    send_deferred_resize(&mut s, &refuse_tx, resize);
                    ensure_paint_watchdog(&mut s, &reader_surface, &event_tx, &layout_wake);
                    layout_wake.notify_all();
                }
            }
            Ok(Some(HostMsg::DisplayControlReset { epoch })) => {
                if let Ok(mut s) = reader_surface.lock() {
                    if s.closed {
                        continue;
                    }
                    if epoch <= s.redirect_epoch {
                        continue;
                    }
                    let discontinuous = s.redirect_epoch.checked_add(1) != Some(epoch);
                    s.redirect_epoch = epoch;
                    remote_offer
                        .connection_epoch
                        .store(epoch, Ordering::Release);
                    s.resize_revision = 0;
                    s.resize_pending = None;
                    remote_offer.generation.store(0, Ordering::Release);
                    s.pressed_buttons.clear();
                    // Coverage and its deadline belong to the old framebuffer.
                    // Never combine pixels from two connection epochs.
                    s.paint_deadline = None;
                    s.painted_layout_pixels = None;
                    s.pointer_paused = false;
                    s.pointer = None;
                    s.pointer_position = None;
                    s.pointer_generation = s.pointer_generation.wrapping_add(1);
                    s.pointer_position_generation = s.pointer_position_generation.wrapping_add(1);
                    let desired = s.desired_layout.clone();
                    s.layout_pending = None;
                    // Until a layout is actually sent, an unsent failure can
                    // safely fall back to this connection's default geometry.
                    s.may_resume_pointer = !discontinuous;
                    s.display.max_monitors = 0;
                    s.display.max_area = 0;
                    s.display.sent_layout.clear();
                    s.display.size_matches = false;
                    s.selected_monitor = None;
                    s.awaiting_layout_pixels = true;
                    s.early_presentable_frame = false;
                    s.clear_before_layout_pixels = true;
                    s.invalidate_pointer_mapping();
                    if discontinuous {
                        s.pause_pointer("RDP helper 重定向代次不连续");
                        let _ = event_tx.send(RdpClientEvent::Notice(
                            "RDP 重定向状态不连续；鼠标已暂停，可手动重新连接".into(),
                        ));
                    }
                    if let Some((monitors, selected)) = desired {
                        s.blocked_resize = s.latest_resize;
                        let _ = queue_monitor_layout(
                            &mut s,
                            &reader_surface,
                            &refuse_tx,
                            &layout_wake,
                            &event_tx,
                            monitors,
                            selected,
                        );
                    } else {
                        s.blocked_resize = None;
                        let resize = s.latest_resize;
                        send_deferred_resize(&mut s, &refuse_tx, resize);
                    }
                    layout_wake.notify_all();
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
                    if s.closed {
                        continue;
                    }
                    s.blit(x, y, width, height, &rgba);
                    let resize = s.finish_matching_layout();
                    send_deferred_resize(&mut s, &refuse_tx, resize);
                    ensure_paint_watchdog(&mut s, &reader_surface, &event_tx, &layout_wake);
                    layout_wake.notify_all();
                }
            }
            Ok(Some(HostMsg::EpochClipboardText { epoch, text })) => {
                let owner = RdpClipboardToken {
                    current: Arc::clone(&remote_offer),
                    epoch,
                    revision: remote_offer.clipboard_revision.load(Ordering::Acquire),
                };
                if owner.is_current() {
                    let _ = event_tx.send(RdpClientEvent::ClipboardText { text, owner });
                }
            }
            Ok(Some(HostMsg::EpochClipboardImage { epoch, image })) => {
                let owner = RdpClipboardToken {
                    current: Arc::clone(&remote_offer),
                    epoch,
                    revision: remote_offer.clipboard_revision.load(Ordering::Acquire),
                };
                if owner.is_current() {
                    let _ = event_tx.send(RdpClientEvent::ClipboardImage { image, owner });
                }
            }
            Ok(Some(HostMsg::ResizeSent { epoch, revision })) => {
                if let Ok(mut s) = reader_surface.lock() {
                    if !s.closed && epoch == s.redirect_epoch {
                        let current_size = (s.width, s.height);
                        if let Some(resize) =
                            s.resize_pending.as_mut().filter(|r| r.revision == revision)
                        {
                            resize.sent = true;
                            resize.saw_other_size |= resize.expected != current_size;
                        }
                    }
                }
            }
            Ok(Some(HostMsg::Error(message))) => {
                remote_offer.closed.store(true, Ordering::Release);
                remote_offer.generation.store(0, Ordering::Release);
                if let Ok(mut s) = reader_surface.lock() {
                    s.close_layout();
                }
                layout_wake.notify_all();
                let _ = event_tx.send(RdpClientEvent::Error(message));
            }
            Ok(Some(HostMsg::Closed)) | Ok(None) => {
                remote_offer.closed.store(true, Ordering::Release);
                remote_offer.generation.store(0, Ordering::Release);
                if let Ok(mut s) = reader_surface.lock() {
                    s.close_layout();
                }
                layout_wake.notify_all();
                let _ = event_tx.send(RdpClientEvent::Closed);
                break;
            }
            Err(_) => {
                remote_offer.closed.store(true, Ordering::Release);
                remote_offer.generation.store(0, Ordering::Release);
                if let Ok(mut s) = reader_surface.lock() {
                    s.close_layout();
                }
                layout_wake.notify_all();
                let _ = event_tx.send(RdpClientEvent::Closed);
                break;
            }
        }
    }
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
            if !surface.display.size_matches
                || selected.is_some_and(|i| i >= surface.display.sent_layout.len())
                || surface
                    .layout_pending
                    .as_ref()
                    .is_some_and(|pending| selected.is_some_and(|i| i >= pending.monitors.len()))
            {
                return Err("桌面尺寸尚未匹配显示器布局".into());
            }
            surface.layout_pending.as_ref().map_or_else(
                || surface.display.sent_layout.clone(),
                |pending| pending.monitors.clone(),
            )
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
            let current = surface
                .layout_pending
                .as_ref()
                .map_or_else(|| &surface.display.sent_layout, |pending| &pending.monitors);
            if !surface.display.size_matches
                || *current != monitors
                || selected.is_some_and(|i| i >= surface.display.sent_layout.len())
            {
                return Err("显示器布局已变化，请重试".into());
            }
            surface.selected_monitor = selected;
            if let Some(pending) = surface.layout_pending.as_mut() {
                pending.selected = selected;
            }
            surface.desired_layout = Some((monitors, selected));
            surface.invalidate_pointer_mapping();
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

    #[test]
    fn painted_pixel_coverage_handles_rows_and_word_boundaries() {
        let mut painted = PaintedPixels::new(70, 2);
        painted.mark(60, 0, 10, 2);
        assert_eq!(painted.remaining, 120);
        assert!(painted.contains(60, 0));
        assert!(painted.contains(69, 1));
        assert!(!painted.contains(59, 0));
        painted.mark(60, 0, 10, 2);
        assert_eq!(painted.remaining, 120);
        painted.mark(0, 0, 70, 2);
        assert_eq!(painted.remaining, 0);
    }

    #[test]
    fn painted_coverage_uses_absolute_coordinates_for_a_cropped_monitor() {
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.match_layout_size();
        surface.selected_monitor = Some(1);
        surface.awaiting_layout_pixels = true;
        surface.clear_before_layout_pixels = true;
        surface.blit(200, 10, 2, 2, &[7, 8, 9, 255].repeat(4));
        let (handle, commands) = handle(surface);
        let frame = handle.frame_if_newer(0).unwrap();
        assert_eq!((frame.width, frame.height), (200, 200));
        assert_eq!(&frame.rgba[10 * 200 * 4..10 * 200 * 4 + 4], &[7, 8, 9, 255]);
        handle.send_pointer_input(InputEvent::MouseMove { x: 0, y: 10 }, frame.rdp_input_epoch);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 200, y: 10 }
            }
        ));
        handle.send_pointer_input(InputEvent::MouseMove { x: 3, y: 10 }, frame.rdp_input_epoch);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            frame.rdp_input_epoch,
        );
        assert!(commands.try_recv().is_err());
        handle
            .surface
            .lock()
            .unwrap()
            .blit(203, 10, 1, 1, &[4, 5, 6, 255]);
        handle.send_pointer_input(InputEvent::MouseMove { x: 3, y: 10 }, frame.rdp_input_epoch);
        assert!(
            commands.try_recv().is_err(),
            "the UI has not sampled the newly painted pixels"
        );
        let next = handle.frame_if_newer(frame.generation).unwrap();
        assert_ne!(next.rdp_input_epoch, frame.rdp_input_epoch);
        handle.send_pointer_input(InputEvent::MouseMove { x: 3, y: 10 }, next.rdp_input_epoch);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 203, y: 10 }
            }
        ));
    }

    #[test]
    fn stationary_pointer_is_synchronized_before_first_click_on_newly_painted_pixels() {
        let mut surface = Surface::new(2, 1);
        surface.awaiting_layout_pixels = true;
        surface.clear_before_layout_pixels = true;
        surface.blit(0, 0, 1, 1, &[1, 2, 3, 255]);
        let (handle, commands) = handle(surface);
        let first = handle.frame_if_newer(0).unwrap();
        handle.send_pointer_input(InputEvent::MouseMove { x: 1, y: 0 }, first.rdp_input_epoch);
        assert!(commands.try_recv().is_err());
        handle
            .surface
            .lock()
            .unwrap()
            .blit(1, 0, 1, 1, &[4, 5, 6, 255]);
        let second = handle.frame_if_newer(first.generation).unwrap();
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            second.rdp_input_epoch,
        );
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::MouseMove { x: 1, y: 0 },
                ..
            }
        ));
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::MouseButton { pressed: true, .. },
                ..
            }
        ));
    }

    #[test]
    fn resized_frame_waits_for_new_pixels_before_accepting_pointer_input() {
        let mut surface = Surface::new(2, 2);
        surface.rgba.fill(255);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let old = handle.frame_if_newer(0).unwrap();
        handle.surface.lock().unwrap().resize(4, 2);
        assert!(handle.frame_if_newer(old.generation).is_none());
        handle.send_pointer_input(InputEvent::MouseMove { x: 1, y: 1 }, old.rdp_input_epoch);
        assert!(commands.try_recv().is_err());
        handle
            .surface
            .lock()
            .unwrap()
            .blit(1, 1, 1, 1, &[1, 2, 3, 255]);
        let painted = handle.frame_if_newer(old.generation).unwrap();
        assert_eq!(&painted.rgba[..4], &[255, 255, 255, 255]);
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 1, y: 1 },
            painted.rdp_input_epoch,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 1, y: 1 }
            }
        ));
    }

    #[test]
    fn manager_does_not_present_a_frame_before_connected_or_after_error() {
        let mut manager = super::super::SessionManager::with_demo_profiles();
        let session = manager.open_mock_session(manager.profiles()[0].id).unwrap();
        let mut surface = Surface::new(2, 2);
        surface.generation = 1;
        let (handle, _commands) = handle(surface);
        let record = manager.sessions.get_mut(&session).unwrap();
        record.rdp = Some(handle);
        record.summary.status = adit_domain::SessionStatus::Connecting;
        assert!(manager.active_rdp_frame_if_newer(0).is_none());
        manager.sessions.get_mut(&session).unwrap().summary.status =
            adit_domain::SessionStatus::Connected;
        assert!(manager.active_rdp_frame_if_newer(0).is_some());
        manager.sessions.get_mut(&session).unwrap().desktop_error = Some("synthetic error".into());
        assert!(manager.active_rdp_frame_if_newer(0).is_none());
    }

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
                cmd_tx: Arc::new(cmd_tx),
                event_rx,
                surface: Arc::new(Mutex::new(surface)),
                layout_wake: Arc::new(Condvar::new()),
                child: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }

    // A synthetic helper writes the real framed IPC format to the production
    // reader; it never starts a process, connects a server, or opens a clipboard.
    struct SyntheticStdout {
        chunks: std_mpsc::Receiver<Vec<u8>>,
        current: std::io::Cursor<Vec<u8>>,
    }

    impl std::io::Read for SyntheticStdout {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let read = std::io::Read::read(&mut self.current, bytes)?;
                if read != 0 || bytes.is_empty() {
                    return Ok(read);
                }
                match self.chunks.recv() {
                    Ok(chunk) => self.current = std::io::Cursor::new(chunk),
                    Err(_) => return Ok(0),
                }
            }
        }
    }

    fn synthetic_helper(
        handle: &RdpClientHandle,
    ) -> (std_mpsc::Sender<Vec<u8>>, thread::JoinHandle<()>) {
        let (tx, rx) = std_mpsc::channel();
        let reader = SyntheticStdout {
            chunks: rx,
            current: std::io::Cursor::new(Vec::new()),
        };
        let surface = Arc::clone(&handle.surface);
        let events = handle.event_tx.clone();
        let commands = Arc::clone(&handle.cmd_tx);
        let offers = Arc::clone(&handle.remote_offer);
        let wake = Arc::clone(&handle.layout_wake);
        let thread = thread::spawn(move || {
            read_helper_messages(reader, surface, events, commands, offers, wake)
        });
        (tx, thread)
    }

    fn helper_send(tx: &std_mpsc::Sender<Vec<u8>>, message: HostMsg) {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &message).unwrap();
        tx.send(bytes).unwrap();
    }

    #[test]
    fn framed_file_requests_keep_their_connection_across_redirect() {
        let (handle, commands) = handle(Surface::new(2, 2));
        handle.remote_offer.observe_offer(5, true);
        let (host, reader) = synthetic_helper(&handle);
        for message in [
            HostMsg::FileContentsNeeded {
                epoch: 0,
                stream_id: 17,
                index: 0,
                offset: 0,
                length: 1,
            },
            HostMsg::DisplayControlReset { epoch: 1 },
            HostMsg::FileContentsNeeded {
                epoch: 1,
                stream_id: 17,
                index: 0,
                offset: 0,
                length: 1,
            },
        ] {
            helper_send(&host, message);
        }
        let old = handle
            .event_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let new = handle
            .event_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            old,
            RdpClientEvent::FileContentsNeeded {
                epoch: 0,
                stream_id: 17,
                ..
            }
        ));
        assert!(matches!(
            new,
            RdpClientEvent::FileContentsNeeded {
                epoch: 1,
                stream_id: 17,
                ..
            }
        ));
        assert!(!handle.remote_offer.accepts(5));
        handle.answer_file_contents(0, 17, Some(vec![1]));
        handle.answer_file_contents(1, 17, Some(vec![2]));
        for (expected, byte) in [(0, 1), (1, 2)] {
            let mut bytes = Vec::new();
            write_msg(
                &mut bytes,
                &commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            )
            .unwrap();
            assert!(
                matches!(read_msg::<_, ClientMsg>(&mut std::io::Cursor::new(bytes)).unwrap(),
                Some(ClientMsg::FileContents { epoch, stream_id: 17, data: Some(data) }) if epoch == expected && data == vec![byte])
            );
        }
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn stale_keyboard_notice_crosses_framed_ipc_without_closing() {
        let (handle, commands) = handle(Surface::new(2, 2));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(&host, HostMsg::InputDiscarded);
        assert!(
            matches!(handle.event_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RdpClientEvent::Notice(message) if message.contains("键盘输入未发送"))
        );
        handle.send_input(InputEvent::Unicode {
            ch: 'a',
            pressed: true,
        });
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::Unicode { ch: 'a', .. },
                ..
            }
        ));
        assert!(commands.try_recv().is_err());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn redirect_retires_cursor_shape_and_position_before_new_updates() {
        let (handle, _commands) = handle(Surface::new(2, 2));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(&host, HostMsg::Pointer(PointerUpdate::Hidden));
        helper_send(
            &host,
            HostMsg::Pointer(PointerUpdate::Position { x: 1, y: 1 }),
        );
        helper_send(
            &host,
            HostMsg::Resized {
                width: 2,
                height: 2,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        let generation = handle.surface.lock().unwrap().pointer_position_generation;
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        helper_send(
            &host,
            HostMsg::Resized {
                width: 2,
                height: 2,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        let surface = handle.surface.lock().unwrap();
        assert!(surface.pointer.is_none());
        assert!(surface.pointer_position.is_none());
        assert_ne!(surface.pointer_position_generation, generation);
        drop(surface);
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn redirect_never_combines_old_and_new_paint_coverage() {
        let mut surface = Surface::new(2, 1);
        surface.awaiting_layout_pixels = true;
        surface.clear_before_layout_pixels = true;
        surface.blit(0, 0, 1, 1, &[1, 2, 3, 255]);
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        helper_send(
            &host,
            HostMsg::Resized {
                width: 2,
                height: 1,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        helper_send(
            &host,
            HostMsg::Tile {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![4, 5, 6, 255],
            },
        );
        helper_send(
            &host,
            HostMsg::Resized {
                width: 2,
                height: 1,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        let frame = handle.frame_if_newer(0).unwrap();
        handle.send_pointer_input(InputEvent::MouseMove { x: 0, y: 0 }, frame.rdp_input_epoch);
        assert!(
            commands.try_recv().is_err(),
            "old pixels are not fresh for the new connection"
        );
        handle.send_pointer_input(InputEvent::MouseMove { x: 1, y: 0 }, frame.rdp_input_epoch);
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::MouseMove { x: 1, y: 0 }
            }
        ));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn layout_timeout_keeps_the_desktop_and_keyboard_but_pauses_uncertain_pointer() {
        let mut surface = Surface::new(200, 200);
        surface.rgba = [11, 22, 33, 255].repeat(200 * 200);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        let before = handle.frame_if_newer(0).unwrap();
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (id, _) = helper_request(&commands);
        handle
            .surface
            .lock()
            .unwrap()
            .layout_pending
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        handle.layout_wake.notify_all();
        assert!(
            matches!(commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput { event: InputEvent::CancelMonitorLayout { request_id }, .. } if request_id == id)
        );
        assert!(
            matches!(handle.event_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RdpClientEvent::Notice(message) if message.contains("手动重新连接"))
        );
        let after = handle.frame_if_newer(before.generation).unwrap();
        assert_eq!(after.rgba, before.rgba);
        assert_eq!(after.rdp_input_epoch, None);
        handle.send_pointer_input(InputEvent::MouseMove { x: 1, y: 1 }, before.rdp_input_epoch);
        handle.send_input(InputEvent::Unicode {
            ch: '测',
            pressed: true,
        });
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::Unicode {
                    ch: '测',
                    pressed: true
                },
                ..
            }
        ));
        assert!(
            commands.try_recv().is_err(),
            "timeout must not close the connection"
        );
        assert!(!handle.surface.lock().unwrap().closed);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (retry, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: retry,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        let recovered = handle.frame_if_newer(after.generation).unwrap();
        assert!(recovered.rdp_input_epoch.is_some());
        assert_eq!((recovered.width, recovered.height), (200, 200));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn partial_paint_timeout_keeps_keyboard_alive_and_full_paint_restores_pointer() {
        let mut surface = Surface::new(2, 2);
        surface.rgba = [8, 9, 10, 255].repeat(4);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        helper_send(
            &host,
            HostMsg::Resized {
                width: 4,
                height: 2,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        helper_send(
            &host,
            HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 255],
            },
        );
        helper_send(
            &host,
            HostMsg::Resized {
                width: 4,
                height: 2,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        {
            let mut surface = handle.surface.lock().unwrap();
            assert!(surface.painted_layout_pixels.is_some());
            assert!(surface.paint_deadline.is_some());
            surface.paint_deadline = Some(Instant::now());
        }
        handle.layout_wake.notify_all();
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        let paused = handle.frame_if_newer(0).unwrap();
        assert_eq!(paused.rdp_input_epoch, None);
        assert_eq!(&paused.rgba[4..8], &[8, 9, 10, 255]);
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::Key { scancode: 30, .. },
                ..
            }
        ));
        assert!(
            commands.try_recv().is_err(),
            "partial paint must not close the connection"
        );
        helper_send(
            &host,
            HostMsg::Tile {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
                rgba: [4, 5, 6, 255].repeat(8),
            },
        );
        helper_send(
            &host,
            HostMsg::Resized {
                width: 4,
                height: 2,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        let recovered = handle.frame_if_newer(paused.generation).unwrap();
        assert!(recovered.rdp_input_epoch.is_some());
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 1, y: 1 },
            recovered.rdp_input_epoch,
        );
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::MouseMove { x: 1, y: 1 },
                ..
            }
        ));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn missing_paint_after_resize_preserves_a_picture_and_keyboard() {
        let mut surface = Surface::new(2, 2);
        surface.rgba = [9, 8, 7, 255].repeat(4);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        helper_send(
            &host,
            HostMsg::Resized {
                width: 4,
                height: 2,
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Resized { .. }
        ));
        handle.surface.lock().unwrap().paint_deadline = Some(Instant::now());
        handle.layout_wake.notify_all();
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        let frame = handle.frame_if_newer(0).unwrap();
        assert_eq!(&frame.rgba[..4], &[9, 8, 7, 255]);
        assert_eq!(frame.rdp_input_epoch, None);
        handle.send_input(InputEvent::Unicode {
            ch: 'a',
            pressed: true,
        });
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::Unicode { ch: 'a', .. },
                ..
            }
        ));
        assert!(commands.try_recv().is_err());
        close_synthetic_helper(host, reader, &handle);
    }

    fn helper_resize(
        tx: &std_mpsc::Sender<Vec<u8>>,
        handle: &RdpClientHandle,
        width: u16,
        height: u16,
    ) {
        helper_send(tx, HostMsg::Resized { width, height });
        assert!(matches!(
            handle.event_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RdpClientEvent::Resized { width: w, height: h } if (w, h) == (width, height)
        ));
        // A fresh tile, followed by an event barrier, models a painted desktop
        // rather than treating matching dimensions as a new frame.
        helper_send(
            tx,
            HostMsg::Tile {
                x: 0,
                y: 0,
                width,
                height,
                rgba: [42, 43, 44, 255].repeat(usize::from(width) * usize::from(height)),
            },
        );
        helper_send(tx, HostMsg::Resized { width, height });
        assert!(matches!(
            handle.event_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RdpClientEvent::Resized { width: w, height: h } if (w, h) == (width, height)
        ));
    }

    fn helper_request(rx: &std_mpsc::Receiver<ClientMsg>) -> (u64, Vec<Monitor>) {
        let message = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &message).unwrap();
        match read_msg::<_, ClientMsg>(&mut std::io::Cursor::new(bytes))
            .unwrap()
            .unwrap()
        {
            ClientMsg::EpochInput {
                epoch: _,
                event:
                    InputEvent::MonitorLayout {
                        request_id,
                        monitors,
                    },
            } => (request_id, monitors),
            other => panic!("expected a framed monitor request, got {other:?}"),
        }
    }

    fn caps() -> DisplayControlState {
        DisplayControlState {
            max_monitors: 2,
            max_area: 800 * 600 * 2,
            ..Default::default()
        }
    }

    fn close_synthetic_helper(
        tx: std_mpsc::Sender<Vec<u8>>,
        reader: thread::JoinHandle<()>,
        handle: &RdpClientHandle,
    ) {
        helper_send(&tx, HostMsg::Closed);
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Closed
        ));
        reader.join().unwrap();
    }

    #[test]
    fn synthetic_helper_delayed_capabilities_and_monitor_switch_keep_input_coordinates() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (id, requested) = helper_request(&commands);
        assert_eq!(requested, monitors());
        handle.send_input(InputEvent::Resize {
            width: 700,
            height: 500,
        });
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(handle.surface.lock().unwrap().layout_pending.is_some());
        assert!(commands.try_recv().is_err());

        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        {
            let surface = handle.surface.lock().unwrap();
            assert!(surface.layout_pending.is_none());
            assert!(surface.display.size_matches);
            assert_eq!(surface.viewport(), (200, 0, 200, 200));
        }
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 17, y: 23 },
            handle.frame_if_newer(0).unwrap().rdp_input_epoch,
        );
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 217, y: 23 }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Key {
                    scancode: 30,
                    pressed: true,
                    ..
                }
            }
        ));

        // A redirected session reissues the layout with a fresh ID; an old
        // reply cannot disturb the new request or its selected monitor.
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        let (reissued, layout) = helper_request(&commands);
        assert_ne!(reissued, id);
        assert_eq!(layout, requested);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Err("old session result".into()),
            },
        );
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 1,
                state: caps(),
            },
        );
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 1,
                request_id: reissued,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        assert_eq!(
            handle.surface.lock().unwrap().viewport(),
            (200, 0, 200, 200)
        );

        // Switching back to one screen releases the latest resize, which the UI
        // has already deduplicated and would otherwise never send again.
        handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .unwrap();
        let (single, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 1,
                request_id: single,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 700,
                    height: 500
                }
            }
        ));
        assert!(handle.frame_if_newer(0).is_none());
        helper_send(
            &host,
            HostMsg::ResizeSent {
                epoch: 1,
                revision: 1,
            },
        );
        helper_resize(&host, &handle, 700, 500);
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 17, y: 23 },
            handle.frame_if_newer(0).unwrap().rdp_input_epoch,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 17, y: 23 }
            }
        ));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn synthetic_helper_failures_retry_and_ignore_old_results() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (old, _) = helper_request(&commands);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (current, _) = helper_request(&commands);
        assert_ne!(old, current);
        handle.send_input(InputEvent::Resize {
            width: 900,
            height: 700,
        });
        // This channel-closed snapshot may have been queued before the retry.
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: DisplayControlState::default(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert_eq!(
            handle
                .surface
                .lock()
                .unwrap()
                .layout_pending
                .as_ref()
                .unwrap()
                .id,
            current
        );
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: old,
                result: Err("old failure".into()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert_eq!(
            handle
                .surface
                .lock()
                .unwrap()
                .layout_pending
                .as_ref()
                .unwrap()
                .id,
            current
        );
        assert!(commands.try_recv().is_err());

        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: current,
                result: Err("encoding failed".into()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        assert!(handle
            .surface
            .lock()
            .unwrap()
            .display
            .sent_layout
            .is_empty());

        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (retry, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: old,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert_eq!(
            handle
                .surface
                .lock()
                .unwrap()
                .layout_pending
                .as_ref()
                .unwrap()
                .id,
            retry
        );
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: retry,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        assert_eq!(
            handle.surface.lock().unwrap().viewport(),
            (200, 0, 200, 200)
        );

        // Channel closure after a usable multi-screen layout also releases the
        // resize most recently suppressed while the layout was active.
        handle.send_input(InputEvent::Resize {
            width: 1000,
            height: 800,
        });
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: DisplayControlState::default(),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 1000,
                    height: 800
                }
            }
        ));
        assert_eq!(handle.surface.lock().unwrap().viewport(), (0, 0, 400, 200));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn synthetic_unsent_failure_preserves_multimonitor_layout_and_latest_resize() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (multi, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: multi,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        handle.send_input(InputEvent::Resize {
            width: 800,
            height: 600,
        });
        handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .unwrap();
        let (single, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: single,
                result: Err("encoding failed".into()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        assert!(commands.try_recv().is_err());
        let surface = handle.surface.lock().unwrap();
        assert!(surface.layout_pending.is_none());
        assert_eq!(surface.display.sent_layout, monitors());
        assert_eq!(surface.viewport(), (200, 0, 200, 200));
        drop(surface);
        handle.send_input(InputEvent::Resize {
            width: 900,
            height: 700,
        });
        handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .unwrap();
        let (retry, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: retry,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn pointer_events_follow_the_presented_layout_epoch_and_keyboard_still_works() {
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.match_layout_size();
        surface.selected_monitor = Some(0);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let old = handle.frame_if_newer(0).unwrap().rdp_input_epoch;
        handle.send_input(InputEvent::MouseMove { x: 12, y: 9 });
        handle.send_input(InputEvent::MouseButton {
            button: adit_rdp_proto::MouseButton::Left,
            pressed: true,
        });
        handle.send_input(InputEvent::Wheel {
            vertical: true,
            delta: 120,
        });
        assert!(
            commands.try_recv().is_err(),
            "pointer input without a frame lease must be rejected"
        );
        handle.send_pointer_input(InputEvent::MouseMove { x: 12, y: 9 }, old);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 12, y: 9 }
            }
        ));
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            old,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::MouseButton { pressed: true, .. }
            }
        ));
        {
            let mut surface = handle.surface.lock().unwrap();
            surface.selected_monitor = Some(1);
            surface.invalidate_pointer_mapping();
        }
        handle.send_pointer_input(InputEvent::MouseMove { x: 12, y: 9 }, old);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            old,
        );
        handle.send_pointer_input(
            InputEvent::Wheel {
                vertical: true,
                delta: 120,
            },
            old,
        );
        assert!(commands.try_recv().is_err());
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: false,
            },
            old,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Key { pressed: true, .. }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseButton { pressed: false, .. }
            }
        ));
        let new = handle.frame_if_newer(1).unwrap().rdp_input_epoch;
        assert_ne!(old, new);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            new,
        );
        assert!(commands.try_recv().is_err());
        handle.send_pointer_input(InputEvent::MouseMove { x: 250, y: 250 }, new);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            new,
        );
        handle.send_pointer_input(
            InputEvent::Wheel {
                vertical: true,
                delta: 120,
            },
            new,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 399, y: 199 }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseButton { pressed: true, .. }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Wheel { delta: 120, .. }
            }
        ));
        handle.configure_monitors(monitors(), Some(0)).unwrap();
        let (request_id, _) = helper_request(&commands);
        let pending = handle.frame_if_newer(1).unwrap();
        assert_eq!(pending.rdp_input_epoch, None);
        handle.send_pointer_input(InputEvent::MouseMove { x: 10, y: 10 }, new);
        handle.send_pointer_input(
            InputEvent::Wheel {
                vertical: true,
                delta: 120,
            },
            new,
        );
        assert!(commands.try_recv().is_err());
        handle
            .surface
            .lock()
            .unwrap()
            .layout_result(request_id, Ok(()), Instant::now());
        assert!(handle.frame_if_newer(pending.generation).is_none());
        {
            let mut surface = handle.surface.lock().unwrap();
            surface.blit(0, 0, 1, 1, &[9, 8, 7, 255]);
            surface.finish_matching_layout();
        }
        let settled = handle.frame_if_newer(pending.generation).unwrap();
        assert!(settled.rdp_input_epoch.is_some());
        assert_ne!(settled.rdp_input_epoch, new);
        assert_eq!(&settled.rgba[..4], &[9, 8, 7, 255]);
        assert_eq!(&settled.rgba[4..8], &[0, 0, 0, 0]);
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 1, y: 0 },
            settled.rdp_input_epoch,
        );
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            settled.rdp_input_epoch,
        );
        assert!(
            commands.try_recv().is_err(),
            "unpainted pixels must not accept a click"
        );
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 0, y: 0 },
            settled.rdp_input_epoch,
        );
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            settled.rdp_input_epoch,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 0, y: 0 }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseButton { pressed: true, .. }
            }
        ));
    }

    #[test]
    fn ordinary_resize_retires_the_previously_presented_pointer_lease() {
        let mut surface = Surface::new(400, 200);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let old = handle.frame_if_newer(0).unwrap().rdp_input_epoch;
        handle.send_input(InputEvent::Resize {
            width: 600,
            height: 400,
        });
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                event: InputEvent::Resize { .. },
                ..
            }
        ));
        handle.send_pointer_input(InputEvent::MouseMove { x: 5, y: 6 }, old);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            old,
        );
        assert!(
            commands.try_recv().is_err(),
            "old picture must not authorize input after resize"
        );
    }

    #[test]
    fn repeated_resize_waits_for_the_latest_write_and_new_pixels() {
        let mut surface = Surface::new(400, 200);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let old = handle.frame_if_newer(0).unwrap().rdp_input_epoch;
        let (host, reader) = synthetic_helper(&handle);
        for width in [400, 600, 400] {
            handle.send_input(InputEvent::Resize { width, height: 200 });
            assert!(matches!(
                commands.recv().unwrap(),
                ClientMsg::EpochInput {
                    event: InputEvent::Resize { .. },
                    ..
                }
            ));
        }
        let tile = || HostMsg::Tile {
            x: 0,
            y: 0,
            width: 400,
            height: 200,
            rgba: [1, 2, 3, 255].repeat(400 * 200),
        };
        helper_send(
            &host,
            HostMsg::ResizeSent {
                epoch: 0,
                revision: 1,
            },
        );
        helper_send(&host, tile());
        helper_send(&host, HostMsg::InputDiscarded);
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        assert!(handle.frame_if_newer(1).is_none());
        handle.send_pointer_input(InputEvent::MouseMove { x: 10, y: 10 }, old);
        assert!(commands.try_recv().is_err());
        helper_send(
            &host,
            HostMsg::ResizeSent {
                epoch: 0,
                revision: 3,
            },
        );
        helper_send(&host, tile());
        helper_send(&host, HostMsg::InputDiscarded);
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        assert!(
            handle.frame_if_newer(1).is_none(),
            "an old same-size tile after the write cannot prove A→B→A completed"
        );
        for width in [600, 400] {
            helper_send(&host, HostMsg::Resized { width, height: 200 });
            assert!(matches!(
                handle
                    .event_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                RdpClientEvent::Resized { .. }
            ));
        }
        helper_send(&host, tile());
        helper_send(&host, HostMsg::InputDiscarded);
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        assert!(handle.frame_if_newer(1).unwrap().rdp_input_epoch.is_some());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn redirect_frame_before_layout_result_remains_visible_and_failure_recovers_input() {
        for sent in [false, true] {
            let monitor = Monitor {
                left: 0,
                top: 0,
                width: 200,
                height: 200,
                primary: true,
                scale_percent: 100,
            };
            let mut surface = Surface::new(200, 200);
            surface.desired_layout = Some((vec![monitor], None));
            let (handle, commands) = handle(surface);
            let (host, reader) = synthetic_helper(&handle);
            helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
            let (request_id, _) = helper_request(&commands);
            helper_send(
                &host,
                HostMsg::Resized {
                    width: 200,
                    height: 200,
                },
            );
            assert!(matches!(
                handle
                    .event_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                RdpClientEvent::Resized { .. }
            ));
            helper_send(
                &host,
                HostMsg::Tile {
                    x: 0,
                    y: 0,
                    width: 200,
                    height: 200,
                    rgba: [8, 9, 10, 255].repeat(200 * 200),
                },
            );
            helper_send(
                &host,
                HostMsg::MonitorLayoutResult {
                    epoch: 1,
                    request_id,
                    result: if sent { Ok(()) } else { Err("not sent".into()) },
                },
            );
            // This event follows both messages on the real framed reader.
            helper_send(&host, HostMsg::InputDiscarded);
            assert!(matches!(
                handle
                    .event_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                RdpClientEvent::Notice(_)
            ));
            let frame = handle
                .frame_if_newer(0)
                .expect("the first complete frame remains visible");
            assert_eq!(&frame.rgba[..4], &[8, 9, 10, 255]);
            if sent {
                assert!(
                    frame.rdp_input_epoch.is_none(),
                    "a pre-send frame cannot authorize the new layout"
                );
                helper_send(
                    &host,
                    HostMsg::Tile {
                        x: 0,
                        y: 0,
                        width: 200,
                        height: 200,
                        rgba: [11, 12, 13, 255].repeat(200 * 200),
                    },
                );
                helper_send(&host, HostMsg::InputDiscarded);
                assert!(matches!(
                    handle
                        .event_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap(),
                    RdpClientEvent::Notice(_)
                ));
                assert!(handle
                    .frame_if_newer(frame.generation)
                    .unwrap()
                    .rdp_input_epoch
                    .is_some());
            } else {
                assert!(
                    frame.rdp_input_epoch.is_some(),
                    "an unsent request leaves the new connection's mapping usable"
                );
            }
            close_synthetic_helper(host, reader, &handle);
        }
    }

    #[test]
    fn framed_clipboard_payloads_are_revoked_at_redirect_even_if_already_queued() {
        let (handle, _) = handle(Surface::new(2, 2));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(
            &host,
            HostMsg::EpochClipboardText {
                epoch: 0,
                text: "old".into(),
            },
        );
        helper_send(
            &host,
            HostMsg::EpochClipboardImage {
                epoch: 0,
                image: vec![1, 2],
            },
        );
        let old_text = handle
            .event_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let old_image = handle
            .event_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        helper_send(
            &host,
            HostMsg::EpochClipboardText {
                epoch: 0,
                text: "late".into(),
            },
        );
        helper_send(
            &host,
            HostMsg::EpochClipboardImage {
                epoch: 0,
                image: vec![3],
            },
        );
        helper_send(
            &host,
            HostMsg::EpochClipboardText {
                epoch: 1,
                text: "new".into(),
            },
        );
        let new_text = handle
            .event_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(old_text, RdpClientEvent::ClipboardText { owner, .. } if !owner.is_current())
        );
        assert!(
            matches!(old_image, RdpClientEvent::ClipboardImage { owner, .. } if !owner.is_current())
        );
        assert!(
            matches!(new_text, RdpClientEvent::ClipboardText { text, owner } if text == "new" && owner.is_current())
        );
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn disabling_then_enabling_clipboard_does_not_revive_an_old_payload() {
        let (handle, _) = handle(Surface::new(2, 2));
        let old = handle.clipboard_token(0);
        assert!(old.is_current());
        handle.set_file_offers_enabled(false);
        handle.set_file_offers_enabled(true);
        assert!(!old.is_current());
        assert!(handle.clipboard_token(0).is_current());
    }

    #[test]
    fn redirect_retries_saved_layout_without_replaying_an_older_resize() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (id, _) = helper_request(&commands);
        handle.send_input(InputEvent::Resize {
            width: 640,
            height: 480,
        });
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Err("encoding failed".into()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 640,
                    height: 480
                }
            }
        ));
        handle.send_input(InputEvent::Resize {
            width: 900,
            height: 700,
        });
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        let (new_id, layout) = helper_request(&commands);
        assert_ne!(new_id, id);
        assert_eq!(layout, monitors());
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 1,
                request_id: new_id,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        assert!(commands.try_recv().is_err());
        assert_eq!(
            handle.surface.lock().unwrap().viewport(),
            (200, 0, 200, 200)
        );
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn redirect_without_a_monitor_preference_reissues_only_the_latest_size() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        for (width, height) in [(640, 480), (900, 700)] {
            handle.send_input(InputEvent::Resize { width, height });
            assert!(
                matches!(commands.recv().unwrap(), ClientMsg::EpochInput { epoch: _, event: InputEvent::Resize { width: w, height: h } } if (w, h) == (width, height))
            );
        }
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        assert!(commands.try_recv().is_err());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn redirect_epoch_rejects_old_responses_and_tags_new_requests_and_pointer() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::MonitorLayout { .. }
            }
        ));
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        let request_id = match commands.recv_timeout(Duration::from_secs(2)).unwrap() {
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::MonitorLayout { request_id, .. },
            } => request_id,
            other => panic!("expected new-epoch layout, got {other:?}"),
        };
        // Even a forged old result with the current request ID cannot win.
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id,
                result: Ok(()),
            },
        );
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        helper_resize(&host, &handle, 400, 200);
        {
            let surface = handle.surface.lock().unwrap();
            assert_eq!(surface.redirect_epoch, 1);
            assert_eq!(surface.display.max_monitors, 0);
            assert_eq!(surface.layout_pending.as_ref().unwrap().id, request_id);
            assert_eq!(
                surface.layout_pending.as_ref().unwrap().phase,
                LayoutPhase::WaitingForHelper
            );
        }
        assert!(commands.try_recv().is_err());
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 1,
                state: caps(),
            },
        );
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 1,
                request_id,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        let frame = handle.frame_if_newer(0).unwrap();
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 17, y: 23 },
            frame.rdp_input_epoch,
        );
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::MouseMove { x: 217, y: 23 }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::Key {
                    scancode: 30,
                    pressed: true,
                    ..
                }
            }
        ));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn skipped_redirect_epoch_keeps_keyboard_and_pauses_unknown_mapping() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 2 });
        assert!(
            matches!(handle.event_rx.recv_timeout(Duration::from_secs(2)).unwrap(), RdpClientEvent::Notice(message) if message.contains("不连续"))
        );
        handle.send_input(InputEvent::Unicode {
            ch: 'x',
            pressed: true,
        });
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientMsg::EpochInput {
                epoch: 2,
                event: InputEvent::Unicode { ch: 'x', .. }
            }
        ));
        assert!(commands.try_recv().is_err());
        let surface = handle.surface.lock().unwrap();
        assert_eq!(surface.redirect_epoch, 2);
        assert!(surface.pointer_paused && !surface.closed);
        drop(surface);
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn a_delayed_old_release_cannot_steal_a_new_connection_button() {
        let mut surface = Surface::new(2, 2);
        surface.generation = 1;
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        let old = handle.frame_if_newer(0).unwrap().rdp_input_epoch;
        handle.send_pointer_input(InputEvent::MouseMove { x: 1, y: 1 }, old);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            old,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::MouseMove { .. }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::MouseButton { pressed: true, .. }
            }
        ));
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        helper_resize(&host, &handle, 2, 2);
        let new = handle.frame_if_newer(0).unwrap().rdp_input_epoch;
        handle.send_pointer_input(InputEvent::MouseMove { x: 1, y: 1 }, new);
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: true,
            },
            new,
        );
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: false,
            },
            old,
        );
        handle.send_pointer_input(
            InputEvent::MouseButton {
                button: adit_rdp_proto::MouseButton::Left,
                pressed: false,
            },
            new,
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::MouseMove { .. }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::MouseButton { pressed: true, .. }
            }
        ));
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::MouseButton { pressed: false, .. }
            }
        ));
        assert!(commands.try_recv().is_err());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn redirect_replays_latest_size_after_the_saved_single_monitor() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .unwrap();
        let (id, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        handle.send_input(InputEvent::Resize {
            width: 900,
            height: 700,
        });
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        let (retry, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 1,
                request_id: retry,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        assert!(commands.try_recv().is_err());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn forwarding_a_new_resize_discards_an_older_blocked_resize() {
        let mut surface = Surface::new(200, 200);
        surface.blocked_resize = Some((640, 480));
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        handle.send_input(InputEvent::Resize {
            width: 900,
            height: 700,
        });
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .unwrap();
        let (id, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(commands.try_recv().is_err());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn superseded_request_makes_the_previous_layout_uncertain() {
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.match_layout_size();
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .unwrap();
        let (first, _) = helper_request(&commands);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (second, _) = helper_request(&commands);
        handle.send_input(InputEvent::Resize {
            width: 900,
            height: 700,
        });
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: first,
                result: Ok(()),
            },
        );
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: second,
                result: Err("encoding failed".into()),
            },
        );
        helper_resize(&host, &handle, 400, 200);
        assert!(handle
            .surface
            .lock()
            .unwrap()
            .display
            .sent_layout
            .is_empty());
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            }
        ));
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn selection_made_while_request_waits_wins_over_its_old_response() {
        let mut manager = super::super::SessionManager::with_demo_profiles();
        let profile = manager.profiles()[0].id;
        let session = manager.open_mock_session(profile).unwrap();
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.match_layout_size();
        surface.selected_monitor = Some(0);
        let (handle, commands) = handle(surface);
        let (host, reader) = synthetic_helper(&handle);
        let mut requested = monitors();
        requested[0].left = 0;
        requested[0].top = -200;
        handle
            .configure_monitors(requested.clone(), Some(0))
            .unwrap();
        let (id, _) = helper_request(&commands);
        manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
        manager.select_active_rdp_monitor(Some(1)).unwrap();
        let handle = manager.sessions[&session].rdp.as_ref().unwrap();
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Ok(()),
            },
        );
        helper_resize(&host, handle, 200, 400);
        assert_eq!(handle.surface.lock().unwrap().selected_monitor, Some(1));
        assert_eq!(
            handle.surface.lock().unwrap().viewport(),
            (0, 200, 200, 200)
        );
        assert_eq!(
            manager
                .profile(profile)
                .unwrap()
                .session_options
                .rdp_selected_monitor,
            Some(1)
        );
        assert_eq!(
            manager
                .profile(profile)
                .unwrap()
                .session_options
                .rdp_monitors,
            Some(requested.iter().map(to_profile_monitor).collect())
        );
        helper_send(&host, HostMsg::DisplayControlReset { epoch: 1 });
        let (retry, _) = helper_request(&commands);
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 1,
                request_id: retry,
                result: Ok(()),
            },
        );
        helper_resize(&host, handle, 200, 400);
        assert_eq!(handle.surface.lock().unwrap().selected_monitor, Some(1));
        close_synthetic_helper(host, reader, handle);
    }

    #[test]
    fn synthetic_helper_deadlines_cancel_and_release_both_waiting_phases() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (first, _) = helper_request(&commands);
        handle.send_input(InputEvent::Resize {
            width: 640,
            height: 480,
        });
        handle
            .surface
            .lock()
            .unwrap()
            .layout_pending
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        handle.layout_wake.notify_all();
        assert!(
            matches!(commands.recv_timeout(Duration::from_secs(2)).unwrap(), ClientMsg::EpochInput { epoch: _, event: InputEvent::CancelMonitorLayout { request_id } } if request_id == first)
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 640,
                    height: 480
                }
            }
        ));
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: first,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(handle
            .surface
            .lock()
            .unwrap()
            .display
            .sent_layout
            .is_empty());

        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (retry, _) = helper_request(&commands);
        handle.send_input(InputEvent::Resize {
            width: 800,
            height: 600,
        });
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: retry,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(!handle.surface.lock().unwrap().display.size_matches);
        handle
            .surface
            .lock()
            .unwrap()
            .layout_pending
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        handle.layout_wake.notify_all();
        assert!(
            matches!(commands.recv_timeout(Duration::from_secs(2)).unwrap(), ClientMsg::EpochInput { epoch: _, event: InputEvent::CancelMonitorLayout { request_id } } if request_id == retry)
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 800,
                    height: 600
                }
            }
        ));
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        assert!(handle.surface.lock().unwrap().layout_pending.is_none());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn a_late_result_cannot_win_a_race_with_the_deadline_worker() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (id, _) = helper_request(&commands);
        handle.send_input(InputEvent::Resize {
            width: 640,
            height: 480,
        });
        // Do not wake the worker: the reader itself must reject a late result.
        handle
            .surface
            .lock()
            .unwrap()
            .layout_pending
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Ok(()),
            },
        );
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            RdpClientEvent::Notice(_)
        ));
        helper_resize(&host, &handle, 200, 200);
        assert!(
            matches!(commands.recv().unwrap(), ClientMsg::EpochInput { epoch: _, event: InputEvent::CancelMonitorLayout { request_id } } if request_id == id)
        );
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 640,
                    height: 480
                }
            }
        ));
        let surface = handle.surface.lock().unwrap();
        assert!(surface.layout_pending.is_none());
        assert!(surface.display.sent_layout.is_empty());
        drop(surface);
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn synthetic_helper_reports_capability_timeout_and_channel_closure() {
        for error in [
            "DisplayControl capabilities timed out",
            "DisplayControl channel closed",
        ] {
            let (handle, commands) = handle(Surface::new(200, 200));
            let (host, reader) = synthetic_helper(&handle);
            handle.configure_monitors(monitors(), Some(1)).unwrap();
            let (id, _) = helper_request(&commands);
            handle.send_input(InputEvent::Resize {
                width: 640,
                height: 480,
            });
            helper_send(
                &host,
                HostMsg::MonitorLayoutResult {
                    epoch: 0,
                    request_id: id,
                    result: Err(error.into()),
                },
            );
            helper_resize(&host, &handle, 200, 200);
            assert!(matches!(
                commands.recv().unwrap(),
                ClientMsg::EpochInput {
                    epoch: _,
                    event: InputEvent::Resize {
                        width: 640,
                        height: 480
                    }
                }
            ));
            let surface = handle.surface.lock().unwrap();
            assert!(surface.layout_pending.is_none());
            assert_eq!(surface.display.error.as_deref(), Some(display_error(error)));
            drop(surface);
            close_synthetic_helper(host, reader, &handle);
        }
    }

    #[test]
    fn channel_close_after_send_releases_resize_without_waiting_for_size_timeout() {
        let (handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&handle);
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: caps(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (id, _) = helper_request(&commands);
        handle.send_input(InputEvent::Resize {
            width: 640,
            height: 480,
        });
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Ok(()),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert_eq!(
            handle
                .surface
                .lock()
                .unwrap()
                .layout_pending
                .as_ref()
                .unwrap()
                .phase,
            LayoutPhase::WaitingForSize
        );
        helper_send(
            &host,
            HostMsg::DisplayControl {
                epoch: 0,
                state: DisplayControlState::default(),
            },
        );
        helper_resize(&host, &handle, 200, 200);
        assert!(matches!(
            commands.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 640,
                    height: 480
                }
            }
        ));
        assert!(handle.surface.lock().unwrap().layout_pending.is_none());
        close_synthetic_helper(host, reader, &handle);
    }

    #[test]
    fn a_closed_command_channel_never_leaves_a_layout_pending() {
        let (handle, commands) = handle(Surface::new(200, 200));
        drop(commands);
        assert!(handle.configure_monitors(monitors(), Some(1)).is_err());
        let surface = handle.surface.lock().unwrap();
        assert!(surface.layout_pending.is_none());
        assert!(surface.display.sent_layout.is_empty());
    }

    #[test]
    fn exhausted_request_ids_do_not_wrap_or_send_a_new_layout() {
        let mut surface = Surface::new(200, 200);
        surface.next_layout_id = u64::MAX;
        let (handle, commands) = handle(surface);
        assert!(handle.configure_monitors(monitors(), Some(1)).is_err());
        assert!(handle.surface.lock().unwrap().layout_pending.is_none());
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn a_locally_unsent_request_keeps_the_previous_layout_and_preference() {
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.match_layout_size();
        surface.selected_monitor = Some(1);
        surface.desired_layout = Some((monitors(), Some(1)));
        surface.blocked_resize = Some((900, 700));
        let (handle, commands) = handle(surface);
        drop(commands);
        assert!(handle
            .configure_monitors(vec![monitors()[1].clone()], None)
            .is_err());
        let surface = handle.surface.lock().unwrap();
        assert_eq!(surface.display.sent_layout, monitors());
        assert_eq!(surface.desired_layout, Some((monitors(), Some(1))));
        assert_eq!(surface.blocked_resize, Some((900, 700)));
        assert_eq!(surface.viewport(), (200, 0, 200, 200));
    }

    #[test]
    fn synthetic_helper_close_clears_pending_without_touching_a_new_session() {
        let (old_handle, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&old_handle);
        old_handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (old_id, _) = helper_request(&commands);
        old_handle.send_input(InputEvent::Resize {
            width: 640,
            height: 480,
        });
        close_synthetic_helper(host, reader, &old_handle);
        assert!(old_handle.surface.lock().unwrap().layout_pending.is_none());
        assert!(commands.try_recv().is_err());

        let (new_handle, new_commands) = handle(Surface::new(200, 200));
        new_handle.configure_monitors(monitors(), Some(1)).unwrap();
        let (new_id, _) = helper_request(&new_commands);
        assert_eq!(old_id, new_id); // IDs are scoped to the helper session.
        old_handle
            .surface
            .lock()
            .unwrap()
            .layout_result(old_id, Ok(()), Instant::now());
        assert_eq!(
            new_handle
                .surface
                .lock()
                .unwrap()
                .layout_pending
                .as_ref()
                .unwrap()
                .id,
            new_id
        );
    }

    #[test]
    fn failed_session_layout_does_not_remove_saved_monitor_preferences() {
        let mut manager = super::super::SessionManager::with_demo_profiles();
        let profile_id = manager.profiles()[0].id;
        let mut options = manager.profile(profile_id).unwrap().session_options.clone();
        options.rdp_monitors = Some(monitors().iter().map(to_profile_monitor).collect());
        options.rdp_selected_monitor = Some(1);
        manager
            .set_profile_session_options(profile_id, options.clone())
            .unwrap();
        let session = manager.open_mock_session(profile_id).unwrap();
        let (rdp, commands) = handle(Surface::new(200, 200));
        let (host, reader) = synthetic_helper(&rdp);
        rdp.configure_monitors(monitors(), Some(1)).unwrap();
        let (id, _) = helper_request(&commands);
        manager.sessions.get_mut(&session).unwrap().rdp = Some(rdp);
        let rdp = manager
            .sessions
            .get(&session)
            .unwrap()
            .rdp
            .as_ref()
            .unwrap();
        helper_send(
            &host,
            HostMsg::MonitorLayoutResult {
                epoch: 0,
                request_id: id,
                result: Err("DisplayControl capabilities timed out".into()),
            },
        );
        helper_resize(&host, rdp, 200, 200);
        close_synthetic_helper(host, reader, rdp);
        let saved = &manager.profile(profile_id).unwrap().session_options;
        assert_eq!(saved.rdp_monitors, options.rdp_monitors);
        assert_eq!(saved.rdp_selected_monitor, Some(1));
        let encoded = serde_json::to_string(manager.profile(profile_id).unwrap()).unwrap();
        let decoded: adit_domain::ConnectionProfile = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.session_options.rdp_monitors, options.rdp_monitors);
        assert_eq!(decoded.session_options.rdp_selected_monitor, Some(1));
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
        let ClientMsg::EpochInput {
            epoch: _,
            event: InputEvent::MonitorLayout {
                monitors: layout, ..
            },
        } = rx.recv().unwrap()
        else {
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
    fn failed_layout_does_not_keep_suppressing_window_resize() {
        let mut surface = Surface::new(400, 200);
        surface.display.sent_layout = monitors();
        surface.display.error = Some("DisplayControl channel disappeared".into());
        let (handle, rx) = handle(surface);

        handle.send_input(InputEvent::Resize {
            width: 800,
            height: 600,
        });
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::Resize {
                    width: 800,
                    height: 600
                }
            })
        ));
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
        handle.send_pointer_input(
            InputEvent::MouseMove { x: 17, y: 23 },
            frame.rdp_input_epoch,
        );
        assert!(matches!(
            rx.recv().unwrap(),
            ClientMsg::EpochInput {
                epoch: _,
                event: InputEvent::MouseMove { x: 217, y: 23 }
            }
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
        assert!(!s.display.size_matches);
        assert_eq!(s.viewport(), (0, 0, 200, 200));
        s.resize(400, 200);
        assert!(s.display.size_matches);
        assert_eq!(s.viewport(), (200, 0, 200, 200));
        s.resize(9000, 1);
        assert_eq!((s.width, s.height), (8192, 1));
        assert!(!s.display.size_matches);
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
