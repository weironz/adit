//! Client side of the out-of-process RDP helper.
//!
//! Native RDP can't be linked into the main binary (IronRDP's crypto deps clash
//! with russh's — see the RDP dependency note), so it runs as the `adit-rdp-host`
//! child process. This module spawns that helper, drives it over stdin/stdout
//! using [`adit_rdp_proto`], keeps the decoded framebuffer, and presents a small
//! handle to the session layer that mirrors [`adit_ssh::LiveShellHandle`].

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use adit_rdp_proto::{monitor_bounds, DisplayControlState, Monitor, PointerUpdate};
use adit_rdp_proto::{
    read_helper_hello, read_msg, write_client_hello, write_msg, ClientMsg, ClipFile,
    ClipboardFileRequestId, ClipboardStamp, ConnectRequest, HostMsg, InputEvent,
    RemoteClipboardStamp, SharedClipboardPolicy, IPC_PROTOCOL_VERSION,
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
    /// A remote file offer bound to the epoch observed by the IPC reader.
    ClipboardFilesForEpoch {
        generation: u64,
        revision: u64,
        files: Vec<ClipFile>,
    },
    /// Windows clipboard.
    ClipboardImage(Vec<u8>),
    ClipboardTextForRevision {
        stamp: RemoteClipboardStamp,
        text: String,
    },
    ClipboardImageForRevision {
        stamp: RemoteClipboardStamp,
        image: Vec<u8>,
    },
    #[doc(hidden)]
    ClipboardWake,
    /// Bytes answering a [`RdpClientHandle::file_requester`] request.
    FileContents {
        generation: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// A reply bound to the epoch of its original local requester.
    FileContentsForEpoch {
        generation: u64,
        revision: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// Unbound synthetic request retained for existing test fixtures. Live IPC
    /// uses `FileContentsNeededBound`; a helper rejects the unbound reply.
    FileContentsNeeded {
        /// Local offer observed by the IPC reader, scoped to this helper.
        generation: u64,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// A helper request retaining its channel and snapshot identity.
    FileContentsNeededBound {
        request: ClipboardFileRequestId,
        index: u32,
        offset: u64,
        length: u32,
    },
    Error(String),
    AuthRejected(String),
    Notice(String),
    ClipboardFileDirectionUnavailable {
        channel_epoch: u64,
        message: String,
    },
    ClipboardChannelChanged {
        channel_epoch: u64,
    },
    ClipboardLocalFileDirectionReady {
        channel_epoch: u64,
    },
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

/// Whether a session's requested sharing policy has reached its helper.
/// Confirmation also requires a ready CLIPRDR channel; it is not a server ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdpClipboardPolicyStatus {
    Pending {
        epoch: u64,
        enabled: bool,
    },
    Confirmed {
        epoch: u64,
        enabled: bool,
    },
    Failed {
        epoch: u64,
        enabled: bool,
        reason: RdpClipboardPolicyFailure,
    },
}

/// Stable reason codes that the UI can translate without parsing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdpClipboardPolicyFailure {
    HelperDisconnected,
    ChannelNotNegotiated,
    ConfirmationTimedOut,
    MappingUnavailable,
    HelperRejected,
    BufferLimit,
}

struct FileEpoch {
    closed: AtomicBool,
    duplicate_warning_sent: AtomicBool,
    stream_capacity_warning_sent: AtomicBool,
    policy_epoch: AtomicU64,
    policy_message_pending: AtomicBool,
    policy_update_pending: AtomicBool,
    policy_mapping: OnceLock<Arc<SharedClipboardPolicy>>,
    state: Mutex<RemoteFileState>,
    clipboard_events: Mutex<ClipboardEventSlot>,
}

#[derive(Default)]
struct ClipboardEventSlot {
    latest: Option<RdpClientEvent>,
    wake_queued: bool,
    overflow_notified: bool,
}

const MAX_QUEUED_REMOTE_CLIPBOARD_BYTES: usize = 64 * 1024 * 1024;

fn clipboard_event_bytes(event: &RdpClientEvent) -> usize {
    match event {
        RdpClientEvent::ClipboardText(text)
        | RdpClientEvent::ClipboardTextForRevision { text, .. } => text.capacity(),
        RdpClientEvent::ClipboardImage(image)
        | RdpClientEvent::ClipboardImageForRevision { image, .. } => image.capacity(),
        RdpClientEvent::ClipboardFiles { files, .. }
        | RdpClientEvent::ClipboardFilesForEpoch { files, .. } => files.iter().fold(
            files
                .capacity()
                .saturating_mul(std::mem::size_of::<ClipFile>()),
            |sum, file| sum.saturating_add(file.name.capacity()),
        ),
        _ => 0,
    }
}

// No consumer acknowledgement exists for both clipboard and drag events. A
// used ID therefore stays retired for this helper's lifetime. Sparse pages
// bound tracking while allowing over 16 million sequential IDs (about 1 TiB
// of full-sized chunks) within a 2 MiB bitmap, plus map overhead.
const REMOTE_STREAM_PAGE_BITS: u32 = 4096;
const REMOTE_STREAM_PAGE_WORDS: usize = REMOTE_STREAM_PAGE_BITS as usize / 64;
const MAX_REMOTE_STREAM_PAGES: usize = 4096;

struct UsedRemoteStreams {
    pages: HashMap<u32, Box<[u64; REMOTE_STREAM_PAGE_WORDS]>>,
    max_pages: usize,
}

impl UsedRemoteStreams {
    fn new(max_pages: usize) -> Self {
        Self {
            pages: HashMap::new(),
            max_pages,
        }
    }

    fn claim(&mut self, stream_id: u32) -> Result<(), RdpFileRequestError> {
        let page = stream_id / REMOTE_STREAM_PAGE_BITS;
        let bit = stream_id % REMOTE_STREAM_PAGE_BITS;
        if !self.pages.contains_key(&page) {
            if self.pages.len() >= self.max_pages {
                return Err(RdpFileRequestError::StreamIdCapacity);
            }
            self.pages
                .insert(page, Box::new([0; REMOTE_STREAM_PAGE_WORDS]));
        }
        let words = self.pages.get_mut(&page).unwrap();
        let word = &mut words[(bit / 64) as usize];
        let mask = 1_u64 << (bit % 64);
        if *word & mask != 0 {
            return Err(RdpFileRequestError::DuplicateStream);
        }
        *word |= mask;
        Ok(())
    }
}

struct RemoteFileState {
    generation: u64,
    enabled: bool,
    revision: u64,
    content_revision: u64,
    remote_copy: Option<RemoteClipboardStamp>,
    used_streams: UsedRemoteStreams,
    queued_requests: HashMap<(u64, u32), VecDeque<u64>>,
    inflight_requests: HashMap<(u64, u32), VecDeque<u64>>,
    policy_ack: Option<(u64, bool)>,
    clipboard_ready_epoch: Option<u64>,
    clipboard_channel_epoch: u64,
    clipboard_negotiated: bool,
    policy_failure: Option<RdpClipboardPolicyFailure>,
    policy_deadline: Option<Instant>,
    pending_policy_offer: Option<PendingPolicyOffer>,
}

struct PendingPolicyOffer {
    generation: u64,
    files: Vec<ClipFile>,
    policy_epoch: u64,
}

const POLICY_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// The clipboard sequence captured when an offer was sampled, and a cheap
/// source queried by the IPC writer immediately before it starts a frame.
#[derive(Clone)]
pub struct RdpClipboardSequence {
    expected: u32,
    source: Arc<dyn Fn() -> u32 + Send + Sync>,
}

impl RdpClipboardSequence {
    /// The source must be a cheap, thread-safe sequence query. RDP queries it
    /// from the IPC writer; zero means the sequence is unavailable.
    pub fn new(expected: u32, source: impl Fn() -> u32 + Send + Sync + 'static) -> Self {
        Self {
            expected,
            source: Arc::new(source),
        }
    }

    pub(crate) fn is_current(&self) -> bool {
        self.expected != 0 && (self.source)() == self.expected
    }
}

// Keep clipboard payloads bounded even if the helper's pipe stops draining.
const MAX_DESKTOP_OFFERS: usize = 8;
const MAX_DESKTOP_OFFER_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct DesktopOfferBudget {
    count: AtomicUsize,
    bytes: AtomicUsize,
}

struct DesktopOfferLease {
    state: u64,
    stamp: ClipboardStamp,
    sequence: RdpClipboardSequence,
    bytes: usize,
    budget: Arc<DesktopOfferBudget>,
}

struct FileOfferLease {
    bytes: usize,
    budget: Arc<DesktopOfferBudget>,
}

impl Drop for FileOfferLease {
    fn drop(&mut self) {
        self.budget.count.fetch_sub(1, Ordering::AcqRel);
        self.budget.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl Drop for DesktopOfferLease {
    fn drop(&mut self) {
        self.budget.count.fetch_sub(1, Ordering::AcqRel);
        self.budget.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct DesktopOffers {
    // High word is the per-session revision; low word is the observed OS sequence.
    state: AtomicU64,
    pending_marker: AtomicBool,
    warning_sent: AtomicBool,
    budget: Arc<DesktopOfferBudget>,
    queued: Mutex<VecDeque<DesktopOfferLease>>,
}

impl DesktopOffers {
    fn observe(&self, sequence: u32) -> bool {
        let mut old = self.state.load(Ordering::Acquire);
        loop {
            if old != 0 && old as u32 == sequence {
                return false;
            }
            let revision = ((old >> 32) as u32).wrapping_add(1).max(1);
            let next = (u64::from(revision) << 32) | u64::from(sequence);
            match self
                .state
                .compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(current) => old = current,
            }
        }
    }

    fn retire(&self) {
        let mut old = self.state.load(Ordering::Acquire);
        loop {
            let revision = ((old >> 32) as u32).wrapping_add(1).max(1);
            let next = (u64::from(revision) << 32) | (old & u64::from(u32::MAX));
            match self
                .state
                .compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(current) => old = current,
            }
        }
    }

    fn queue(
        &self,
        tx: &std_mpsc::Sender<ClientMsg>,
        message: ClientMsg,
        sequence: RdpClipboardSequence,
        bytes: usize,
        stamp: ClipboardStamp,
    ) -> bool {
        if sequence.expected == 0 || bytes > MAX_DESKTOP_OFFER_BYTES {
            return false;
        }
        let mut state = self.state.load(Ordering::Acquire);
        if state == 0 {
            let initial = (1_u64 << 32) | u64::from(sequence.expected);
            state =
                match self
                    .state
                    .compare_exchange(0, initial, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => initial,
                    Err(current) => current,
                };
        }
        if state as u32 != sequence.expected {
            return false;
        }
        let Ok(mut queue) = self.queued.try_lock() else {
            return false;
        };
        if self.budget.count.load(Ordering::Acquire) >= MAX_DESKTOP_OFFERS
            || self
                .budget
                .bytes
                .load(Ordering::Acquire)
                .saturating_add(bytes)
                > MAX_DESKTOP_OFFER_BYTES
            || self.state.load(Ordering::Acquire) != state
        {
            return false;
        }
        self.budget.count.fetch_add(1, Ordering::AcqRel);
        self.budget.bytes.fetch_add(bytes, Ordering::AcqRel);
        queue.push_back(DesktopOfferLease {
            state,
            stamp,
            sequence,
            bytes,
            budget: Arc::clone(&self.budget),
        });
        self.pending_marker.store(false, Ordering::Release);
        if tx.send(message).is_err() {
            queue.pop_back();
            return false;
        }
        self.warning_sent.store(false, Ordering::Release);
        true
    }

    fn take(&self) -> Option<DesktopOfferLease> {
        self.queued.lock().ok()?.pop_front()
    }

    fn accepts(&self, lease: &DesktopOfferLease) -> bool {
        self.state.load(Ordering::Acquire) == lease.state
            && lease.sequence.is_current()
            && self.state.load(Ordering::Acquire) == lease.state
    }
}

/// Local offers and remote offers have independent lifetimes. A generation is
/// sent to the helper and echoed by each request. The helper's channel epoch
/// and snapshot revision also travel with each admitted read.
struct LocalFileOffers {
    next: AtomicU64,
    current: AtomicU64,
    sequence: Mutex<Option<(u64, RdpClipboardSequence)>>,
    revocation_unconfirmed: AtomicBool,
    suspension_confirmed: AtomicBool,
    channel_epoch: AtomicU64,
    refusal_reserved: AtomicBool,
    reservation_warning_sent: AtomicBool,
    reads: Mutex<HashMap<ClipboardFileRequestId, InboundRead>>,
    budget: Arc<InboundReadBudget>,
    desktop: DesktopOffers,
    file_offer_budget: Arc<DesktopOfferBudget>,
    queued_file_offers: Mutex<HashMap<u64, FileOfferLease>>,
    pending_empty_file_revoke: AtomicBool,
}

// A request keeps its slot until the reply has actually left the writer. Merely
// bounding the UI queue would let a slow helper accumulate unbounded replies.
const MAX_INBOUND_READS_PER_SESSION: usize = 32;
const MAX_INBOUND_REFUSALS_PER_SESSION: usize = 16;
const MAX_INBOUND_READS_GLOBAL: usize = 256;
// Reserve a complete per-session refusal allowance for at most 64 helpers.
const MAX_INBOUND_REFUSALS_GLOBAL: usize = MAX_INBOUND_REFUSALS_PER_SESSION * 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum InboundReadKind {
    Read,
    Refusal,
}

struct InboundRead {
    kind: InboundReadKind,
    replied: bool,
}

#[derive(Default)]
struct InboundReadCounts {
    reads: usize,
    refusals: usize,
    reservations: usize,
}

pub(super) struct InboundReadBudget {
    counts: Mutex<InboundReadCounts>,
    max_reads: usize,
    max_refusals: usize,
}

impl InboundReadBudget {
    pub(super) fn new(max_reads: usize, max_refusals: usize) -> Self {
        Self {
            counts: Mutex::new(InboundReadCounts::default()),
            max_reads,
            max_refusals,
        }
    }

    #[cfg(test)]
    pub(super) fn test_counts(&self) -> (usize, usize) {
        let counts = self.counts.lock().unwrap();
        (counts.reads, counts.refusals)
    }
}

enum InboundAdmission {
    Read,
    Refuse,
    Suspend,
    Suspended,
}

impl Default for LocalFileOffers {
    fn default() -> Self {
        static BUDGET: OnceLock<Arc<InboundReadBudget>> = OnceLock::new();
        Self::with_budget(Arc::clone(BUDGET.get_or_init(|| {
            Arc::new(InboundReadBudget::new(
                MAX_INBOUND_READS_GLOBAL,
                MAX_INBOUND_REFUSALS_GLOBAL,
            ))
        })))
    }
}

impl LocalFileOffers {
    // A closed helper cannot be reactivated by an offer racing with close.
    const CLOSED: u64 = u64::MAX;
    const SUSPENDED: u64 = u64::MAX - 1;

    fn reserve_file_offer(&self, generation: u64, files: &[ClipFile], capacity: usize) -> bool {
        let bytes = files.iter().fold(
            capacity.saturating_mul(std::mem::size_of::<ClipFile>()),
            |sum, file| sum.saturating_add(file.name.capacity()),
        );
        let Ok(mut queued) = self.queued_file_offers.try_lock() else {
            return false;
        };
        if self.file_offer_budget.count.load(Ordering::Acquire) >= MAX_DESKTOP_OFFERS
            || self
                .file_offer_budget
                .bytes
                .load(Ordering::Acquire)
                .saturating_add(bytes)
                > MAX_DESKTOP_OFFER_BYTES
        {
            return false;
        }
        self.file_offer_budget.count.fetch_add(1, Ordering::AcqRel);
        self.file_offer_budget
            .bytes
            .fetch_add(bytes, Ordering::AcqRel);
        queued.insert(
            generation,
            FileOfferLease {
                bytes,
                budget: Arc::clone(&self.file_offer_budget),
            },
        );
        true
    }

    fn take_file_offer(&self, generation: u64) -> Option<FileOfferLease> {
        self.queued_file_offers.lock().ok()?.remove(&generation)
    }

    fn with_budget(budget: Arc<InboundReadBudget>) -> Self {
        Self {
            next: AtomicU64::new(0),
            current: AtomicU64::new(0),
            sequence: Mutex::new(None),
            revocation_unconfirmed: AtomicBool::new(false),
            suspension_confirmed: AtomicBool::new(false),
            channel_epoch: AtomicU64::new(0),
            refusal_reserved: AtomicBool::new(false),
            reservation_warning_sent: AtomicBool::new(false),
            reads: Mutex::new(HashMap::new()),
            budget,
            desktop: DesktopOffers::default(),
            file_offer_budget: Arc::new(DesktopOfferBudget::default()),
            queued_file_offers: Mutex::new(HashMap::new()),
            pending_empty_file_revoke: AtomicBool::new(false),
        }
    }

    // Reserve the entire per-session refusal allowance before advertising an
    // offer, so another helper cannot consume even the last failure slot.
    fn reserve_refusal(&self) -> bool {
        let mut counts = self.budget.counts.lock().unwrap();
        if self.refusal_reserved.load(Ordering::Acquire) {
            return true;
        }
        let allowance = MAX_INBOUND_REFUSALS_PER_SESSION.min(self.budget.max_refusals);
        if self.is_closed()
            || self.is_suspended()
            || allowance == 0
            || counts.reservations + allowance > self.budget.max_refusals
        {
            return false;
        }
        counts.reservations += allowance;
        self.refusal_reserved.store(true, Ordering::Release);
        true
    }

    fn admit_read(&self, request: ClipboardFileRequestId) -> InboundAdmission {
        self.observe_channel_epoch(request.channel_epoch);
        if self.is_suspended() {
            return InboundAdmission::Suspended;
        }
        // Unexpected stale requests can arrive before an offer. They still
        // need capacity for a real failure, not a successful empty chunk.
        if !self.reserve_refusal() {
            return InboundAdmission::Suspend;
        }
        let mut reads = self.reads.lock().unwrap();
        if !self.accepts_channel(request.channel_epoch) {
            return InboundAdmission::Suspended;
        }
        if self.is_suspended() {
            return InboundAdmission::Suspended;
        }
        if self.is_closed()
            || reads.keys().any(|other| {
                other.channel_epoch == request.channel_epoch && other.stream_id == request.stream_id
            })
        {
            // An overlapping stream ID has no unambiguous wire response, even
            // if the two requests claim different clipboard generations.
            return InboundAdmission::Suspend;
        }
        let mut counts = self.budget.counts.lock().unwrap();
        let local_reads = reads
            .values()
            .filter(|r| r.kind == InboundReadKind::Read)
            .count();
        let accept = self.accepts(request.generation)
            && local_reads < MAX_INBOUND_READS_PER_SESSION
            && counts.reads < self.budget.max_reads;
        let kind = if accept {
            counts.reads += 1;
            InboundReadKind::Read
        } else {
            let local_refusals = reads.len() - local_reads;
            let allowance = MAX_INBOUND_REFUSALS_PER_SESSION.min(self.budget.max_refusals);
            if local_refusals >= allowance {
                return InboundAdmission::Suspend;
            }
            counts.refusals += 1;
            InboundReadKind::Refusal
        };
        reads.insert(
            request,
            InboundRead {
                kind,
                replied: false,
            },
        );
        if accept {
            InboundAdmission::Read
        } else {
            InboundAdmission::Refuse
        }
    }

    fn mark_read_replied(&self, request: ClipboardFileRequestId) -> bool {
        let mut reads = self.reads.lock().unwrap();
        let Some(read) = reads.get_mut(&request) else {
            return false;
        };
        if self.is_closed()
            || self.is_suspended()
            || !self.accepts_channel(request.channel_epoch)
            || read.replied
        {
            return false;
        }
        read.replied = true;
        true
    }

    fn finish_read(&self, request: ClipboardFileRequestId) {
        let mut reads = self.reads.lock().unwrap();
        if !reads.get(&request).is_some_and(|read| read.replied) {
            return;
        }
        let read = reads.remove(&request).unwrap();
        let mut counts = self.budget.counts.lock().unwrap();
        match read.kind {
            InboundReadKind::Read => counts.reads -= 1,
            InboundReadKind::Refusal => counts.refusals -= 1,
        }
        if self.is_suspended()
            && reads.is_empty()
            && self.refusal_reserved.swap(false, Ordering::AcqRel)
        {
            counts.reservations -= MAX_INBOUND_REFUSALS_PER_SESSION.min(self.budget.max_refusals);
        }
    }

    fn publish(&self, sequence: RdpClipboardSequence) -> Option<u64> {
        if sequence.expected == 0 || !self.reserve_refusal() {
            return None;
        }
        // Publish the generation and its sequence together. The writer only
        // holds this lock long enough to clone the binding, never for a syscall.
        let mut binding = self.sequence.lock().ok()?;
        loop {
            let current = self.current.load(Ordering::Acquire);
            if current == Self::CLOSED || current == Self::SUSPENDED {
                return None;
            }
            let generation = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if generation == 0 || generation == Self::CLOSED || generation == Self::SUSPENDED {
                continue;
            }
            if self
                .current
                .compare_exchange(current, generation, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                *binding = Some((generation, sequence));
                return Some(generation);
            }
        }
    }

    fn revoke(&self) {
        self.revocation_unconfirmed.store(true, Ordering::Release);
        let _ = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != Self::CLOSED && current != Self::SUSPENDED).then_some(0)
            });
    }

    fn revoke_for_local_file_selection(&self) {
        if self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0 && current != Self::CLOSED && current != Self::SUSPENDED).then_some(0)
            })
            .is_ok()
        {
            self.revocation_unconfirmed.store(true, Ordering::Release);
        }
    }

    // Disable the direction before releasing slots, so in-flight and queued
    // replies cannot be mistaken for a new request with the same stream ID.
    fn suspend(&self) -> bool {
        let previous = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != Self::CLOSED && current != Self::SUSPENDED).then_some(Self::SUSPENDED)
            });
        if previous.is_err() {
            return false;
        }
        let mut reads = self.reads.lock().unwrap();
        let mut counts = self.budget.counts.lock().unwrap();
        // Replies already in the writer queue still own memory. Keep their
        // global slots until the writer discards them, even if it is blocked.
        reads.retain(|_, read| {
            if read.replied {
                true
            } else {
                match read.kind {
                    InboundReadKind::Read => counts.reads -= 1,
                    InboundReadKind::Refusal => counts.refusals -= 1,
                }
                false
            }
        });
        if reads.is_empty() && self.refusal_reserved.swap(false, Ordering::AcqRel) {
            counts.reservations -= MAX_INBOUND_REFUSALS_PER_SESSION.min(self.budget.max_refusals);
        }
        true
    }

    fn close(&self) {
        self.current.store(Self::CLOSED, Ordering::Release);
        let mut reads = self.reads.lock().unwrap();
        let mut counts = self.budget.counts.lock().unwrap();
        for (_, read) in reads.drain() {
            match read.kind {
                InboundReadKind::Read => counts.reads -= 1,
                InboundReadKind::Refusal => counts.refusals -= 1,
            }
        }
        if self.refusal_reserved.swap(false, Ordering::AcqRel) {
            counts.reservations -= MAX_INBOUND_REFUSALS_PER_SESSION.min(self.budget.max_refusals);
        }
    }

    fn is_closed(&self) -> bool {
        self.current.load(Ordering::Acquire) == Self::CLOSED
    }

    fn is_suspended(&self) -> bool {
        self.current.load(Ordering::Acquire) == Self::SUSPENDED
    }

    fn accepts_channel(&self, channel_epoch: u64) -> bool {
        channel_epoch == 0 || channel_epoch == self.channel_epoch.load(Ordering::Acquire)
    }

    fn observe_channel_epoch(&self, channel_epoch: u64) {
        if channel_epoch == 0
            || self
                .channel_epoch
                .fetch_max(channel_epoch, Ordering::AcqRel)
                >= channel_epoch
        {
            return;
        }
        let mut reads = self.reads.lock().unwrap();
        let mut counts = self.budget.counts.lock().unwrap();
        reads.retain(|request, read| {
            if request.channel_epoch == 0 || request.channel_epoch >= channel_epoch || read.replied
            {
                true
            } else {
                match read.kind {
                    InboundReadKind::Read => counts.reads -= 1,
                    InboundReadKind::Refusal => counts.refusals -= 1,
                }
                false
            }
        });
    }

    fn accepts(&self, generation: u64) -> bool {
        generation != 0
            && generation != Self::CLOSED
            && generation != Self::SUSPENDED
            && self.current.load(Ordering::Acquire) == generation
    }

    fn accepts_at_write(&self, generation: u64) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        let sequence = self.sequence.lock().ok().and_then(|binding| {
            binding
                .as_ref()
                .filter(|(bound_generation, _)| *bound_generation == generation)
                .map(|(_, sequence)| sequence.clone())
        });
        let Some(sequence) = sequence else {
            return false;
        };
        let observed = (sequence.source)();
        observed != 0 && observed == sequence.expected && self.accepts(generation)
    }
}

impl Drop for LocalFileOffers {
    fn drop(&mut self) {
        self.close();
    }
}

pub(super) struct LocalFileReadLease {
    current: Arc<LocalFileOffers>,
    generation: u64,
    request: Option<ClipboardFileRequestId>,
    replies: std_mpsc::Sender<ClientMsg>,
    tracked: bool,
}

impl LocalFileReadLease {
    pub(super) fn is_current(&self) -> bool {
        self.current.accepts(self.generation)
            && self
                .request
                .is_none_or(|request| self.current.accepts_channel(request.channel_epoch))
    }

    pub(super) fn current_guard(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let current = Arc::clone(&self.current);
        let generation = self.generation;
        let epoch = self.request.map_or(0, |request| request.channel_epoch);
        move || current.accepts(generation) && current.accepts_channel(epoch)
    }

    pub(super) fn refuse(&self, stream_id: u32) {
        self.answer(stream_id, None);
    }

    pub(super) fn answer(&self, stream_id: u32, data: Option<Vec<u8>>) {
        let request = self.request.unwrap_or(ClipboardFileRequestId {
            channel_epoch: 0,
            snapshot_revision: 0,
            generation: self.generation,
            stream_id,
        });
        if request.stream_id != stream_id {
            return;
        }
        if self.current.is_closed()
            || self.current.is_suspended()
            || (self.tracked && !self.current.mark_read_replied(request))
        {
            return;
        }
        let data = if self.is_current() { data } else { None };
        let message = if self.request.is_some() {
            ClientMsg::BoundFileContents { request, data }
        } else {
            ClientMsg::FileContents {
                generation: self.generation,
                stream_id,
                data,
            }
        };
        if self.replies.send(message).is_err() && self.tracked {
            self.current.finish_read(request);
        }
    }
}

// Keep admission on the IPC reader, before the unbounded general event queue.
// A refusal consumes a separate bounded slot until the writer completes it.
fn dispatch_inbound_file_request(
    offers: &LocalFileOffers,
    replies: &std_mpsc::Sender<ClientMsg>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    event: RdpClientEvent,
) -> bool {
    let (request, bound) = match &event {
        RdpClientEvent::FileContentsNeeded {
            generation,
            stream_id,
            ..
        } => (
            ClipboardFileRequestId {
                channel_epoch: 0,
                snapshot_revision: 0,
                generation: *generation,
                stream_id: *stream_id,
            },
            false,
        ),
        RdpClientEvent::FileContentsNeededBound { request, .. } => (*request, true),
        _ => unreachable!("only file requests pass through admission"),
    };
    match offers.admit_read(request) {
        InboundAdmission::Read => events.send(event).is_ok(),
        InboundAdmission::Refuse => {
            let message = if bound {
                ClientMsg::BoundFileContents {
                    request,
                    data: None,
                }
            } else {
                ClientMsg::FileContents {
                    generation: request.generation,
                    stream_id: request.stream_id,
                    data: None,
                }
            };
            if offers.mark_read_replied(request) {
                replies.send(message).is_ok()
            } else {
                offers.is_suspended() || !offers.accepts_channel(request.channel_epoch)
            }
        }
        InboundAdmission::Suspend => {
            suspend_for_inbound_overload(offers, replies, events, request.channel_epoch);
            true
        }
        InboundAdmission::Suspended => true,
    }
}

fn suspend_for_inbound_overload(
    offers: &LocalFileOffers,
    replies: &std_mpsc::Sender<ClientMsg>,
    events: &std_mpsc::Sender<RdpClientEvent>,
    channel_epoch: u64,
) {
    if offers.suspend() {
        let _ = replies.send(ClientMsg::SuspendLocalFileDirection);
        let _ = events.send(RdpClientEvent::ClipboardFileDirectionUnavailable {
            channel_epoch,
            message: "远端文件请求过多或流 ID 重复，已停止本地文件共享，正在等待辅助进程确认；桌面连接仍可使用。".into(),
        });
    }
}
impl Default for FileEpoch {
    fn default() -> Self {
        Self {
            closed: AtomicBool::new(false),
            duplicate_warning_sent: AtomicBool::new(false),
            stream_capacity_warning_sent: AtomicBool::new(false),
            policy_epoch: AtomicU64::new(0),
            policy_message_pending: AtomicBool::new(false),
            policy_update_pending: AtomicBool::new(false),
            policy_mapping: OnceLock::new(),
            clipboard_events: Mutex::new(ClipboardEventSlot::default()),
            state: Mutex::new(RemoteFileState {
                generation: 0,
                enabled: true,
                revision: 0,
                content_revision: 0,
                remote_copy: None,
                used_streams: UsedRemoteStreams::new(MAX_REMOTE_STREAM_PAGES),
                queued_requests: HashMap::new(),
                inflight_requests: HashMap::new(),
                policy_ack: None,
                clipboard_ready_epoch: None,
                clipboard_channel_epoch: 0,
                clipboard_negotiated: true,
                policy_failure: None,
                policy_deadline: None,
                pending_policy_offer: None,
            }),
        }
    }
}
impl FileEpoch {
    fn queue_clipboard_event(
        &self,
        events: &std_mpsc::Sender<RdpClientEvent>,
        event: RdpClientEvent,
    ) {
        let mut slot = self.clipboard_events.lock().unwrap();
        if clipboard_event_bytes(&event) > MAX_QUEUED_REMOTE_CLIPBOARD_BYTES {
            slot.latest = None;
            if !slot.overflow_notified {
                slot.overflow_notified = true;
                let _ = events.send(RdpClientEvent::Notice(
                    "远端剪贴板内容过大，已跳过；远程桌面仍可使用。".into(),
                ));
            }
            return;
        }
        slot.overflow_notified = false;
        slot.latest = Some(event);
        if !slot.wake_queued {
            slot.wake_queued = true;
            if events.send(RdpClientEvent::ClipboardWake).is_err() {
                slot.wake_queued = false;
                slot.latest = None;
            }
        }
    }

    fn take_clipboard_event(&self) -> Option<RdpClientEvent> {
        let mut slot = self.clipboard_events.lock().unwrap();
        slot.wake_queued = false;
        slot.latest.take()
    }
    fn configure_clipboard(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap();
        state.enabled = enabled;
        state.clipboard_negotiated = enabled;
    }

    fn mapping_failed(&self) -> bool {
        matches!(
            self.state.lock().unwrap().policy_failure,
            Some(
                RdpClipboardPolicyFailure::MappingUnavailable
                    | RdpClipboardPolicyFailure::HelperRejected
            )
        )
    }

    fn ensure_policy_mapping(&self) -> std::io::Result<Arc<SharedClipboardPolicy>> {
        if let Some(mapping) = self.policy_mapping.get() {
            return Ok(Arc::clone(mapping));
        }
        // Creating an OS object can block, so the spawn worker does it before
        // taking the short state lock shared with the UI.
        let mapping = Arc::new(SharedClipboardPolicy::create(0, false)?);
        let mut state = self.state.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return Err(std::io::Error::other("RDP session closed"));
        }
        mapping.store_full(
            self.policy_epoch.load(Ordering::Acquire),
            state.enabled,
            state.content_revision,
        )?;
        let _ = self.policy_mapping.set(Arc::clone(&mapping));
        state.policy_deadline = state
            .clipboard_negotiated
            .then(|| Instant::now() + POLICY_CONFIRM_TIMEOUT);
        Ok(Arc::clone(self.policy_mapping.get().unwrap()))
    }

    fn fail_policy(&self, reason: RdpClipboardPolicyFailure) {
        let mut state = self.state.lock().unwrap();
        Self::mark_policy_failed(&mut state, reason);
    }

    #[cfg(feature = "test-fixtures")]
    fn fail_policy_at(&self, epoch: u64, reason: RdpClipboardPolicyFailure) {
        let mut state = self.state.lock().unwrap();
        if self.policy_epoch.load(Ordering::Acquire) == epoch {
            Self::mark_policy_failed(&mut state, reason);
        }
    }

    fn mark_policy_failed(state: &mut RemoteFileState, reason: RdpClipboardPolicyFailure) {
        state.policy_failure = Some(reason);
        if matches!(
            reason,
            RdpClipboardPolicyFailure::MappingUnavailable
                | RdpClipboardPolicyFailure::HelperRejected
        ) {
            state.clipboard_negotiated = false;
        }
        state.pending_policy_offer = None;
        state.generation = 0;
        state.revision = state.revision.wrapping_add(1);
    }

    fn expire_policy(state: &mut RemoteFileState) {
        if state.policy_failure.is_none()
            && state
                .policy_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.policy_failure = Some(RdpClipboardPolicyFailure::ConfirmationTimedOut);
            state.generation = 0;
            state.revision = state.revision.wrapping_add(1);
        }
    }

    fn policy_status(&self) -> RdpClipboardPolicyStatus {
        let mut state = self.state.lock().unwrap();
        Self::expire_policy(&mut state);
        let epoch = self.policy_epoch.load(Ordering::Acquire);
        let enabled = state.enabled;
        if self.closed.load(Ordering::Acquire) {
            return RdpClipboardPolicyStatus::Failed {
                epoch,
                enabled,
                reason: RdpClipboardPolicyFailure::HelperDisconnected,
            };
        }
        if let Some(reason) = state.policy_failure {
            return RdpClipboardPolicyStatus::Failed {
                epoch,
                enabled,
                reason,
            };
        }
        if !state.clipboard_negotiated {
            return RdpClipboardPolicyStatus::Failed {
                epoch,
                enabled,
                reason: RdpClipboardPolicyFailure::ChannelNotNegotiated,
            };
        }
        if self.policy_mapping.get().is_some()
            && state.policy_ack == Some((epoch, enabled))
            && state.clipboard_ready_epoch.is_some()
        {
            RdpClipboardPolicyStatus::Confirmed { epoch, enabled }
        } else {
            RdpClipboardPolicyStatus::Pending { epoch, enabled }
        }
    }

    fn take_policy_update(&self) -> Option<ClientMsg> {
        self.policy_update_pending
            .swap(false, Ordering::AcqRel)
            .then(|| ClientMsg::ClipboardPolicyEpoch(self.policy_epoch.load(Ordering::Acquire)))
    }

    fn revision(&self) -> u64 {
        self.state.lock().unwrap().revision
    }

    fn content_stamp(&self) -> Option<ClipboardStamp> {
        let state = self.state.lock().unwrap();
        (!self.closed.load(Ordering::Acquire) && state.enabled && state.policy_failure.is_none())
            .then(|| ClipboardStamp {
                policy_epoch: self.policy_epoch.load(Ordering::Acquire),
                content_revision: state.content_revision,
            })
    }

    fn advance_local_content(&self) {
        let mut state = self.state.lock().unwrap();
        let epoch = self.policy_epoch.load(Ordering::Acquire);
        let next = state.content_revision.checked_add(1);
        let published = next.is_some_and(|revision| {
            self.policy_mapping
                .get()
                .is_none_or(|mapping| mapping.store_full(epoch, state.enabled, revision).is_ok())
        });
        if !published {
            if let Some(mapping) = self.policy_mapping.get() {
                let _ = mapping.store_full(epoch, false, state.content_revision);
            }
            Self::mark_policy_failed(&mut state, RdpClipboardPolicyFailure::MappingUnavailable);
            return;
        }
        state.content_revision = next.unwrap();
        state.remote_copy = None;
        self.clipboard_events.lock().unwrap().latest = None;
    }

    fn observe_remote_copy(&self, stamp: RemoteClipboardStamp) -> bool {
        let mut state = self.state.lock().unwrap();
        if self.closed.load(Ordering::Acquire)
            || !state.enabled
            || state.policy_failure.is_some()
            || stamp.local.policy_epoch != self.policy_epoch.load(Ordering::Acquire)
            || stamp.local.content_revision != state.content_revision
            || stamp.channel_epoch < state.clipboard_channel_epoch
            || state.remote_copy.is_some_and(|old| {
                old.channel_epoch > stamp.channel_epoch
                    || (old.channel_epoch == stamp.channel_epoch
                        && old.copy_revision >= stamp.copy_revision)
            })
        {
            return false;
        }
        state.remote_copy = Some(stamp);
        true
    }

    fn accepts_remote_copy(&self, stamp: RemoteClipboardStamp) -> bool {
        let state = self.state.lock().unwrap();
        self.accepts_remote_copy_locked(&state, stamp)
    }

    fn accepts_remote_copy_locked(
        &self,
        state: &RemoteFileState,
        stamp: RemoteClipboardStamp,
    ) -> bool {
        !self.closed.load(Ordering::Acquire)
            && state.enabled
            && state.policy_failure.is_none()
            && stamp.local.policy_epoch == self.policy_epoch.load(Ordering::Acquire)
            && stamp.local.content_revision == state.content_revision
            && state.remote_copy == Some(stamp)
            && stamp.channel_epoch >= state.clipboard_channel_epoch
    }
    fn observe_offer_at(
        &self,
        generation: u64,
        has_files: bool,
        revision: u64,
    ) -> Option<(bool, u64)> {
        let mut state = self.state.lock().unwrap();
        if state.revision != revision {
            return None;
        }
        // Even an offer that reuses a numeric generation is a new epoch.
        state.revision = state.revision.wrapping_add(1);
        state.generation = if has_files && !self.closed.load(Ordering::Acquire) && state.enabled {
            generation
        } else {
            0
        };
        Some((state.generation != 0, state.revision))
    }
    fn observe_policy_offer(
        &self,
        generation: u64,
        files: Vec<ClipFile>,
        policy_epoch: u64,
    ) -> Option<RdpClientEvent> {
        let bytes = files.iter().fold(
            files
                .capacity()
                .saturating_mul(std::mem::size_of::<ClipFile>()),
            |sum, file| sum.saturating_add(file.name.capacity()),
        );
        let mut state = self.state.lock().unwrap();
        Self::expire_policy(&mut state);
        if self.policy_epoch.load(Ordering::Acquire) != policy_epoch
            || self.closed.load(Ordering::Acquire)
            || !state.enabled
            || state
                .policy_failure
                .is_some_and(|reason| reason != RdpClipboardPolicyFailure::ConfirmationTimedOut)
        {
            return None;
        }
        if self.policy_mapping.get().is_some()
            && (state.policy_ack != Some((policy_epoch, true))
                || state.clipboard_ready_epoch.is_none())
        {
            if bytes > MAX_DESKTOP_OFFER_BYTES {
                state.policy_failure = Some(RdpClipboardPolicyFailure::BufferLimit);
                state.pending_policy_offer = None;
            } else {
                // A clipboard has one current value: replace older pending
                // copies instead of letting a stalled helper grow this queue.
                state.pending_policy_offer = Some(PendingPolicyOffer {
                    generation,
                    files,
                    policy_epoch,
                });
            }
            return None;
        }
        Some(Self::publish_policy_offer(&mut state, generation, files))
    }

    fn publish_policy_offer(
        state: &mut RemoteFileState,
        generation: u64,
        files: Vec<ClipFile>,
    ) -> RdpClientEvent {
        state.revision = state.revision.wrapping_add(1);
        state.generation = if files.is_empty() { 0 } else { generation };
        RdpClientEvent::ClipboardFilesForEpoch {
            generation,
            revision: state.revision,
            files,
        }
    }

    fn release_confirmed_offer(&self, state: &mut RemoteFileState) -> Option<RdpClientEvent> {
        let epoch = self.policy_epoch.load(Ordering::Acquire);
        if state.policy_failure.is_some()
            || state.policy_ack != Some((epoch, state.enabled))
            || state.clipboard_ready_epoch.is_none()
        {
            return None;
        }
        state.policy_deadline = None;
        let offer = state.pending_policy_offer.take()?;
        if !state.enabled || offer.policy_epoch != epoch {
            return None;
        }
        Some(Self::publish_policy_offer(
            state,
            offer.generation,
            offer.files,
        ))
    }

    fn acknowledge_policy(&self, epoch: u64, enabled: bool) -> Option<RdpClientEvent> {
        let mut state = self.state.lock().unwrap();
        Self::expire_policy(&mut state);
        if self.closed.load(Ordering::Acquire)
            || state
                .policy_failure
                .is_some_and(|reason| reason != RdpClipboardPolicyFailure::ConfirmationTimedOut)
            || epoch != self.policy_epoch.load(Ordering::Acquire)
            || enabled != state.enabled
        {
            return None;
        }
        if state.policy_failure == Some(RdpClipboardPolicyFailure::ConfirmationTimedOut) {
            state.policy_failure = None;
            state.policy_deadline = state
                .clipboard_ready_epoch
                .is_none()
                .then(|| Instant::now() + POLICY_CONFIRM_TIMEOUT);
        }
        state.policy_ack = Some((epoch, enabled));
        self.release_confirmed_offer(&mut state)
    }

    fn clipboard_channel_ready(&self, channel_epoch: u64) -> Option<RdpClientEvent> {
        let mut state = self.state.lock().unwrap();
        Self::expire_policy(&mut state);
        if self.closed.load(Ordering::Acquire) || channel_epoch < state.clipboard_channel_epoch {
            return None;
        }
        state.clipboard_channel_epoch = channel_epoch;
        state.clipboard_ready_epoch = Some(channel_epoch);
        if state.policy_failure == Some(RdpClipboardPolicyFailure::ConfirmationTimedOut)
            && state.policy_ack == Some((self.policy_epoch.load(Ordering::Acquire), state.enabled))
        {
            state.policy_failure = None;
            state.policy_deadline = None;
        }
        self.release_confirmed_offer(&mut state)
    }

    fn clipboard_channel_changed(&self, channel_epoch: u64) {
        let mut state = self.state.lock().unwrap();
        if channel_epoch <= state.clipboard_channel_epoch {
            return;
        }
        state.clipboard_channel_epoch = channel_epoch;
        state.clipboard_ready_epoch = None;
        state.pending_policy_offer = None;
        state.remote_copy = None;
        state.generation = 0;
        state.revision = state.revision.wrapping_add(1);
        state.policy_deadline = (self.policy_mapping.get().is_some() && state.clipboard_negotiated)
            .then(|| Instant::now() + POLICY_CONFIRM_TIMEOUT);
    }
    #[cfg(test)]
    fn observe_offer(&self, generation: u64, has_files: bool) {
        let _ = self.observe_offer_at(generation, has_files, self.revision());
    }
    fn set_enabled(&self, enabled: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.enabled != enabled {
            let Some(epoch) = self.policy_epoch.load(Ordering::Acquire).checked_add(1) else {
                state.enabled = enabled;
                state.policy_failure = Some(RdpClipboardPolicyFailure::MappingUnavailable);
                state.pending_policy_offer = None;
                state.generation = 0;
                state.revision = state.revision.wrapping_add(1);
                return false;
            };
            if self.policy_mapping.get().is_some_and(|mapping| {
                mapping
                    .store_full(epoch, enabled, state.content_revision)
                    .is_err()
            }) {
                if let Some(mapping) = self.policy_mapping.get() {
                    let _ = mapping.store_full(
                        self.policy_epoch.load(Ordering::Acquire),
                        false,
                        state.content_revision,
                    );
                }
                state.enabled = enabled;
                state.policy_failure = Some(RdpClipboardPolicyFailure::MappingUnavailable);
                state.pending_policy_offer = None;
                state.generation = 0;
                state.revision = state.revision.wrapping_add(1);
                return false;
            }
            state.enabled = enabled;
            state.remote_copy = None;
            self.clipboard_events.lock().unwrap().latest = None;
            state.generation = 0;
            state.revision = state.revision.wrapping_add(1);
            state.policy_ack = None;
            state.pending_policy_offer = None;
            if self.policy_mapping.get().is_some()
                && !matches!(
                    state.policy_failure,
                    Some(
                        RdpClipboardPolicyFailure::MappingUnavailable
                            | RdpClipboardPolicyFailure::HelperRejected
                    )
                )
            {
                state.policy_failure = None;
                state.policy_deadline = state
                    .clipboard_negotiated
                    .then(|| Instant::now() + POLICY_CONFIRM_TIMEOUT);
            }
            self.policy_epoch.store(epoch, Ordering::Release);
            return true;
        }
        false
    }
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        if let Some(mapping) = self.policy_mapping.get() {
            let _ = mapping.store_full(
                self.policy_epoch.load(Ordering::Acquire),
                false,
                state.content_revision,
            );
        }
        state.generation = 0;
        state.revision = state.revision.wrapping_add(1);
        state.pending_policy_offer = None;
        state.remote_copy = None;
        self.clipboard_events.lock().unwrap().latest = None;
        state.policy_failure = Some(RdpClipboardPolicyFailure::HelperDisconnected);
    }
    fn current_generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }
    fn accepts(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap();
        generation != 0
            && !self.closed.load(Ordering::Acquire)
            && state.enabled
            && state.generation == generation
    }

    fn revision_is_current(&self, revision: u64) -> bool {
        let state = self.state.lock().unwrap();
        !self.closed.load(Ordering::Acquire) && state.revision == revision
    }

    fn accepts_revision(&self, generation: u64, revision: u64) -> bool {
        let state = self.state.lock().unwrap();
        generation != 0
            && !self.closed.load(Ordering::Acquire)
            && state.enabled
            && state.generation == generation
            && state.revision == revision
    }

    // Keep the local epoch beside the queued wire message. Holding this lock
    // through the nonblocking send preserves FIFO even across requester clones.
    fn queue_request(
        &self,
        generation: u64,
        revision: u64,
        stream_id: u32,
        tx: &std_mpsc::Sender<ClientMsg>,
        message: ClientMsg,
    ) -> Result<(), RdpFileRequestError> {
        let mut state = self.state.lock().unwrap();
        let key = (generation, stream_id);
        // Claim before even an unavailable request: the compatibility API may
        // enqueue its failure, which must never reach a later request either.
        state.used_streams.claim(stream_id)?;
        if generation == 0
            || self.closed.load(Ordering::Acquire)
            || !state.enabled
            || state.generation != generation
            || state.revision != revision
        {
            return Err(RdpFileRequestError::Unavailable);
        }
        state
            .queued_requests
            .entry(key)
            .or_default()
            .push_back(revision);
        if tx.send(message).is_ok() {
            Ok(())
        } else {
            let revisions = state.queued_requests.get_mut(&key).unwrap();
            revisions.pop_back();
            if revisions.is_empty() {
                state.queued_requests.remove(&key);
            }
            Err(RdpFileRequestError::Unavailable)
        }
    }

    fn take_queued_revision(&self, generation: u64, stream_id: u32) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let key = (generation, stream_id);
        let revisions = state.queued_requests.get_mut(&key)?;
        let revision = revisions.pop_front();
        if revisions.is_empty() {
            state.queued_requests.remove(&key);
        }
        revision
    }

    fn start_queued_request(&self, generation: u64, stream_id: u32) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let key = (generation, stream_id);
        let revisions = state.queued_requests.get_mut(&key)?;
        let revision = revisions.pop_front()?;
        if revisions.is_empty() {
            state.queued_requests.remove(&key);
        }
        state
            .inflight_requests
            .entry(key)
            .or_default()
            .push_back(revision);
        Some(revision)
    }

    fn take_inflight_revision(&self, generation: u64, stream_id: u32) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let key = (generation, stream_id);
        let revisions = state.inflight_requests.get_mut(&key)?;
        let revision = revisions.pop_front();
        if revisions.is_empty() {
            state.inflight_requests.remove(&key);
        }
        revision
    }

    fn cancel_started_request(&self, generation: u64, stream_id: u32) {
        let mut state = self.state.lock().unwrap();
        let key = (generation, stream_id);
        if let Some(revisions) = state.inflight_requests.get_mut(&key) {
            revisions.pop_back();
            if revisions.is_empty() {
                state.inflight_requests.remove(&key);
            }
        }
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
    revision: u64,
    current: Arc<FileEpoch>,
    replies: std_mpsc::Sender<RdpClientEvent>,
}

/// The origin and copy generation of a remote text or image.
#[derive(Clone)]
pub struct RdpClipboardLease {
    current: Arc<FileEpoch>,
    stamp: RemoteClipboardStamp,
}

impl RdpClipboardLease {
    pub fn is_current(&self) -> bool {
        self.current.accepts_remote_copy(self.stamp)
    }

    /// Keep policy and remote-copy admission stable until the clipboard write
    /// has actually been handed to the platform, not merely queued to the UI.
    pub fn with_current<T>(&self, write: impl FnOnce() -> T) -> Option<T> {
        let state = self.current.state.lock().ok()?;
        self.current
            .accepts_remote_copy_locked(&state, self.stamp)
            .then(write)
    }
}

impl std::fmt::Debug for RdpClipboardLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdpClipboardLease")
            .field("stamp", &self.stamp)
            .finish()
    }
}

/// Why a remote file request could not be admitted locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdpFileRequestError {
    /// The stream ID was already used by this helper, even if its reply arrived.
    DuplicateStream,
    /// The bounded stream-ID history is full; reconnect before using new pages.
    StreamIdCapacity,
    /// The offer has been revoked or the helper command channel is unavailable.
    Unavailable,
}

impl RdpFileRequester {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn is_current(&self) -> bool {
        self.current
            .accepts_revision(self.generation, self.revision)
    }

    /// Compatibility entry point. Prefer [`Self::try_request`] when the caller
    /// needs an immediate per-request rejection. A duplicate emits a session
    /// notice rather than a response that could be delivered to the first read.
    pub fn request(&self, stream_id: u32, index: u32, offset: u64, length: u32) {
        match self.try_request(stream_id, index, offset, length) {
            Ok(()) => {}
            Err(RdpFileRequestError::Unavailable) => {
                fail_remote_request(&self.replies, self.generation, self.revision, stream_id);
            }
            Err(RdpFileRequestError::DuplicateStream) => {
                // A failure event with the same ID would be delivered to the
                // original request. Legacy callers get a notice instead.
                if !self
                    .current
                    .duplicate_warning_sent
                    .swap(true, Ordering::AcqRel)
                {
                    let _ = self.replies.send(RdpClientEvent::Notice(
                        "文件读取请求的流 ID 重复，后续请求已拒绝。".into(),
                    ));
                }
            }
            Err(RdpFileRequestError::StreamIdCapacity) => {
                if !self
                    .current
                    .stream_capacity_warning_sent
                    .swap(true, Ordering::AcqRel)
                {
                    let _ = self.replies.send(RdpClientEvent::Notice(
                        "远程文件读取 ID 跟踪空间已满，请手动重新连接后再试；远程桌面仍可使用。"
                            .into(),
                    ));
                }
            }
        }
    }

    /// Admit a read synchronously. IDs cannot be reused during a helper's
    /// lifetime because replies can remain in either upper-layer event queue.
    pub fn try_request(
        &self,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<(), RdpFileRequestError> {
        let tx = self.tx.lock().unwrap_or_else(|error| error.into_inner());
        self.current.queue_request(
            self.generation,
            self.revision,
            stream_id,
            &tx,
            ClientMsg::RequestFileContents {
                generation: self.generation,
                stream_id,
                index,
                offset,
                length,
            },
        )
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

pub(super) enum RdpEventPoll {
    Event(RdpClientEvent),
    Discarded,
    Empty,
}

/// Handle to a live RDP session running in the helper process.
pub struct RdpClientHandle {
    remote_offer: Arc<FileEpoch>,
    local_offers: Arc<LocalFileOffers>,
    event_tx: std_mpsc::Sender<RdpClientEvent>,
    cmd_tx: std_mpsc::Sender<ClientMsg>,
    event_rx: std_mpsc::Receiver<RdpClientEvent>,
    surface: Arc<Mutex<Surface>>,
    // Populated asynchronously: the helper is spawned on a background thread so the
    // UI thread never blocks on `Command::spawn` (see `spawn_rdp_session`). `None`
    // until the spawn completes.
    child: Arc<Mutex<Option<Child>>>,
    reaper_started: AtomicBool,
}

#[cfg(feature = "test-fixtures")]
impl crate::SessionManager {
    /// Put a synthetic marker on the real helper writer without using the UI input gate.
    #[doc(hidden)]
    pub fn test_rdp_fifo_barrier(&self, session: adit_domain::SessionId) {
        let rdp = self
            .sessions
            .get(&session)
            .and_then(|record| record.rdp.as_ref())
            .unwrap();
        rdp.send_input(InputEvent::Key {
            scancode: 0x7e,
            extended: false,
            pressed: true,
        });
    }

    /// Synchronize a synthetic IPC test with the spawned helper's ownership.
    #[doc(hidden)]
    pub fn test_rdp_helper_attached(&self, session: adit_domain::SessionId) -> bool {
        self.sessions
            .get(&session)
            .and_then(|record| record.rdp.as_ref())
            .and_then(|rdp| rdp.child.lock().ok())
            .and_then(|mut child| child.as_mut().map(|child| child.try_wait()))
            .is_some_and(|status| matches!(status, Ok(None)))
    }

    /// Exercise the session/UI error gate while the real IPC writer is alive.
    /// This deliberately bypasses the reader's fatal-error shutdown and must be
    /// paired with a separate test of an actual `HostMsg::Error`.
    #[doc(hidden)]
    pub fn test_inject_rdp_error_with_live_writer(
        &self,
        session: adit_domain::SessionId,
        message: &str,
    ) {
        let rdp = self.sessions[&session]
            .rdp
            .as_ref()
            .expect("live RDP test session");
        assert!(self.test_rdp_helper_attached(session));
        rdp.test_send_event(RdpClientEvent::Error(message.to_owned()));
    }
}

impl RdpClientHandle {
    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        Self::test_fixture_with_commands().0
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub(crate) fn test_fixture_with_commands() -> (Self, std_mpsc::Receiver<ClientMsg>) {
        Self::test_fixture_with_budget(Arc::new(InboundReadBudget::new(
            MAX_INBOUND_READS_GLOBAL,
            MAX_INBOUND_REFUSALS_GLOBAL,
        )))
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub(super) fn test_fixture_with_budget(
        budget: Arc<InboundReadBudget>,
    ) -> (Self, std_mpsc::Receiver<ClientMsg>) {
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = std_mpsc::channel();
        (
            Self {
                remote_offer: Arc::new(FileEpoch::default()),
                local_offers: Arc::new(LocalFileOffers::with_budget(budget)),
                event_tx,
                cmd_tx,
                event_rx,
                surface: Arc::new(Mutex::new(Surface::new(1, 1))),
                child: Arc::new(Mutex::new(None)),
                reaper_started: AtomicBool::new(false),
            },
            cmd_rx,
        )
    }

    #[cfg(test)]
    pub(super) fn test_local_offer(&self) -> LocalFileReadLease {
        self.local_offers
            .publish(RdpClipboardSequence::new(1, || 1))
            .expect("fixture is open");
        let mut lease = self.local_file_read_lease(self.test_local_generation());
        lease.tracked = false;
        lease
    }

    #[cfg(test)]
    pub(super) fn test_local_generation(&self) -> u64 {
        self.local_offers.current.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn test_suspend_local_files(&self) {
        assert!(self.local_offers.suspend());
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub(super) fn test_send_event(&self, event: RdpClientEvent) {
        if let RdpClientEvent::ClipboardChannelChanged { channel_epoch }
        | RdpClientEvent::ClipboardLocalFileDirectionReady { channel_epoch } = &event
        {
            self.local_offers.observe_channel_epoch(*channel_epoch);
        }
        if matches!(
            event,
            RdpClientEvent::FileContentsNeeded { .. }
                | RdpClientEvent::FileContentsNeededBound { .. }
        ) {
            if self.local_offers.is_closed() {
                return;
            }
            if !dispatch_inbound_file_request(
                &self.local_offers,
                &self.cmd_tx,
                &self.event_tx,
                event,
            ) {
                self.local_offers.close();
            }
        } else {
            self.event_tx.send(event).unwrap();
        }
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub(super) fn test_remote_file_offer(&self, generation: u64) -> RdpFileRequester {
        self.remote_offer
            .observe_offer_at(generation, true, self.remote_offer.revision());
        self.file_requester_for_generation(generation)
    }

    #[cfg(test)]
    pub(super) fn test_complete_remote_request(
        &self,
        generation: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    ) {
        assert!(self
            .remote_offer
            .start_queued_request(generation, stream_id)
            .is_some());
        dispatch_remote_file_contents(
            &self.remote_offer,
            &self.event_tx,
            generation,
            stream_id,
            data,
        );
    }

    #[cfg(test)]
    pub(super) fn test_write_command(&self, command: ClientMsg) -> ClientMsg {
        let mut bytes = Vec::new();
        assert!(write_queued_client_message(
            &mut bytes,
            command,
            &self.local_offers
        ));
        read_msg(&mut std::io::Cursor::new(bytes)).unwrap().unwrap()
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub(super) fn test_write_optional_command(&self, mut command: ClientMsg) -> Option<ClientMsg> {
        if matches!(command, ClientMsg::ClipboardPolicyEpoch(_)) {
            self.remote_offer
                .policy_message_pending
                .store(false, Ordering::Release);
            command = self.remote_offer.take_policy_update()?;
        }
        let mut bytes = Vec::new();
        let _ = write_queued_client_message(&mut bytes, command, &self.local_offers);
        read_msg(&mut std::io::Cursor::new(bytes)).unwrap()
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_prepare_policy(&self) -> std::io::Result<u64> {
        self.remote_offer.ensure_policy_mapping()?;
        Ok(self.remote_offer.policy_epoch.load(Ordering::Acquire))
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_ack(&self, epoch: u64, enabled: bool) {
        if let Some(event) = self.remote_offer.acknowledge_policy(epoch, enabled) {
            let _ = self.event_tx.send(event);
        }
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_ready(&self, channel_epoch: u64) {
        if let Some(event) = self.remote_offer.clipboard_channel_ready(channel_epoch) {
            let _ = self.event_tx.send(event);
        }
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_channel_changed(&self, channel_epoch: u64) {
        self.remote_offer.clipboard_channel_changed(channel_epoch);
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_channel_unavailable(&self) {
        self.remote_offer.state.lock().unwrap().clipboard_negotiated = false;
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_failed(&self, epoch: u64, reason: RdpClipboardPolicyFailure) {
        self.remote_offer.fail_policy_at(epoch, reason);
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_offer(&self, epoch: u64, generation: u64, files: Vec<ClipFile>) {
        dispatch_remote_clipboard_offer(
            &self.remote_offer,
            &self.event_tx,
            generation,
            files,
            Some(epoch),
            0,
        );
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_disconnected(&self) {
        self.remote_offer.close();
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_policy_timeout(&self) {
        self.remote_offer.state.lock().unwrap().policy_deadline = Some(Instant::now());
    }

    #[cfg(feature = "test-fixtures")]
    pub(super) fn test_raw_command_observer(&self) -> impl Fn(&ClientMsg) + Send + Sync + 'static {
        let offers = Arc::clone(&self.local_offers);
        let remote = Arc::clone(&self.remote_offer);
        move |command| match command {
            ClientMsg::Input(InputEvent::ClipboardText(_)) | ClientMsg::ClipboardImage(_) => {
                offers.desktop.take();
            }
            ClientMsg::ClipboardFiles { generation, files } if !files.is_empty() => {
                offers.take_file_offer(*generation);
            }
            ClientMsg::ClipboardPolicyEpoch(_) => {
                remote
                    .policy_message_pending
                    .store(false, Ordering::Release);
                let _ = remote.take_policy_update();
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(super) fn test_discard_suspended_command(&self, command: ClientMsg) {
        let mut bytes = Vec::new();
        assert!(write_queued_client_message(
            &mut bytes,
            command,
            &self.local_offers
        ));
        assert!(bytes.is_empty());
    }

    /// Send an input event to the session.
    pub fn send_input(&self, mut event: InputEvent) {
        // Clipboard text needs an OS sequence binding; unbound generic input is
        // not safe to queue behind a blocked pipe.
        if matches!(event, InputEvent::ClipboardText(_)) {
            self.notice_desktop_offer_rejected(false);
            return;
        }
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
    #[cfg(test)]
    pub fn send_clipboard_image(&self, image: Vec<u8>) {
        self.send_clipboard_image_with_sequence(image, RdpClipboardSequence::new(1, || 1));
    }

    pub(crate) fn send_clipboard_text_with_sequence(
        &self,
        text: String,
        sequence: RdpClipboardSequence,
    ) {
        let bytes = text.capacity();
        let Some(stamp) = self.remote_offer.content_stamp() else {
            return;
        };
        let accepted = self.local_offers.desktop.queue(
            &self.cmd_tx,
            ClientMsg::Input(InputEvent::ClipboardText(text)),
            sequence,
            bytes,
            stamp,
        );
        self.notice_desktop_offer_rejected(accepted);
    }

    pub(crate) fn send_clipboard_image_with_sequence(
        &self,
        image: Vec<u8>,
        sequence: RdpClipboardSequence,
    ) {
        let bytes = image.capacity();
        let Some(stamp) = self.remote_offer.content_stamp() else {
            return;
        };
        let accepted = self.local_offers.desktop.queue(
            &self.cmd_tx,
            ClientMsg::ClipboardImage(image),
            sequence,
            bytes,
            stamp,
        );
        self.notice_desktop_offer_rejected(accepted);
    }

    fn notice_desktop_offer_rejected(&self, accepted: bool) {
        if !accepted
            && !self
                .local_offers
                .desktop
                .warning_sent
                .swap(true, Ordering::AcqRel)
        {
            let _ = self.event_tx.send(RdpClientEvent::Notice(
                "本次剪贴板内容未发送；请重新复制后重试，远程桌面仍可使用。".into(),
            ));
        }
    }

    pub(crate) fn reject_unbound_desktop_offer(&self) {
        self.notice_desktop_offer_rejected(false);
    }

    pub(crate) fn observe_local_clipboard_sequence(&self, sequence: u32) {
        if self.local_offers.desktop.observe(sequence) {
            self.remote_offer.advance_local_content();
            self.local_offers.revoke_for_local_file_selection();
            if !self
                .local_offers
                .desktop
                .pending_marker
                .swap(true, Ordering::AcqRel)
            {
                let _ = self.cmd_tx.send(ClientMsg::LocalFileOfferRejected);
            }
        }
    }

    pub(crate) fn retire_local_desktop_offers(&self) {
        self.local_offers.desktop.retire();
        self.remote_offer.advance_local_content();
        if !self
            .local_offers
            .desktop
            .pending_marker
            .swap(true, Ordering::AcqRel)
        {
            let _ = self.cmd_tx.send(ClientMsg::LocalFileOfferRejected);
        }
    }

    /// Offer a local file selection to the remote clipboard. Metadata only —
    /// nothing is read from disk until the remote pastes.
    pub fn send_clipboard_files(
        &self,
        files: Vec<ClipFile>,
        sequence: Option<RdpClipboardSequence>,
    ) {
        if self.local_offers.is_suspended() && !files.is_empty() {
            return;
        }
        let generation = if files.is_empty() {
            if !self.local_offers.is_suspended() {
                self.local_offers.revoke();
            }
            0
        } else {
            let Some(sequence) = sequence.filter(|sequence| sequence.expected != 0) else {
                self.send_clipboard_files(Vec::new(), None);
                return;
            };
            let Some(generation) = self.local_offers.publish(sequence) else {
                if !self.local_offers.is_closed()
                    && !self.local_offers.refusal_reserved.load(Ordering::Acquire)
                    && !self
                        .local_offers
                        .reservation_warning_sent
                        .swap(true, Ordering::AcqRel)
                {
                    let _ = self.event_tx.send(RdpClientEvent::Notice(
                        "文件共享资源已满，本会话未启用本地文件共享；远程桌面仍可使用。".into(),
                    ));
                }
                self.send_clipboard_files(Vec::new(), None);
                return;
            };
            generation
        };
        if self.local_offers.is_closed() {
            return;
        }
        if files.is_empty() {
            if self
                .local_offers
                .pending_empty_file_revoke
                .swap(true, Ordering::AcqRel)
            {
                return;
            }
        } else {
            if !self
                .local_offers
                .reserve_file_offer(generation, &files, files.capacity())
            {
                self.local_offers.revoke();
                self.retire_local_desktop_offers();
                self.notice_desktop_offer_rejected(false);
                return;
            }
            self.local_offers
                .pending_empty_file_revoke
                .store(false, Ordering::Release);
            // A later sequence change needs a marker after this queued offer,
            // even if an earlier marker is still in the pipe.
            self.local_offers
                .desktop
                .pending_marker
                .store(false, Ordering::Release);
        }
        if self
            .cmd_tx
            .send(ClientMsg::ClipboardFiles { generation, files })
            .is_err()
        {
            self.local_offers.take_file_offer(generation);
            if generation == 0 {
                self.local_offers
                    .pending_empty_file_revoke
                    .store(false, Ordering::Release);
            }
            self.local_offers.revoke();
        } else if generation != 0 {
            self.local_offers
                .desktop
                .warning_sent
                .store(false, Ordering::Release);
        }
    }

    /// Tell this helper that a local file copy displaced prior text or image.
    /// This marker carries no file metadata and also works while files are suspended.
    pub(crate) fn notify_local_file_selection(&self) {
        if self.local_offers.is_closed() {
            return;
        }
        self.local_offers.revoke_for_local_file_selection();
        self.retire_local_desktop_offers();
    }

    pub(crate) fn revoke_local_files(&self) {
        self.send_clipboard_files(Vec::new(), None);
        let _ = self.event_tx.send(RdpClientEvent::Notice(
            "本地文件共享已停止；远端撤销尚未确认，已发送或正在发送的数据仍可能到达。".into(),
        ));
    }

    pub(crate) fn has_local_file_offer(&self) -> bool {
        let generation = self.local_offers.current.load(Ordering::Acquire);
        generation != 0
            && generation != LocalFileOffers::CLOSED
            && generation != LocalFileOffers::SUSPENDED
    }

    /// The current IPC protocol has no acknowledgement for remote revocation.
    pub fn local_file_revocation_unconfirmed(&self) -> bool {
        self.local_offers
            .revocation_unconfirmed
            .load(Ordering::Acquire)
    }

    pub(super) fn local_file_read_lease(&self, generation: u64) -> LocalFileReadLease {
        LocalFileReadLease {
            current: self.local_offers.clone(),
            generation,
            request: None,
            replies: self.cmd_tx.clone(),
            tracked: true,
        }
    }

    pub(super) fn local_file_read_lease_for_request(
        &self,
        request: ClipboardFileRequestId,
    ) -> LocalFileReadLease {
        LocalFileReadLease {
            current: self.local_offers.clone(),
            generation: request.generation,
            request: Some(request),
            replies: self.cmd_tx.clone(),
            tracked: true,
        }
    }

    pub(super) fn owns_local_file_read_lease(&self, lease: &LocalFileReadLease) -> bool {
        Arc::ptr_eq(&self.local_offers, &lease.current)
    }

    /// A handle for asking the remote for file bytes.
    ///
    /// Separate from `&self` because the callers are COM streams running on
    /// Explorer's threads: it has to be `Send + Sync` and outlive any borrow of
    /// the session.
    pub fn file_requester(&self) -> RdpFileRequester {
        self.file_requester_for_generation(self.remote_offer.current_generation())
    }

    pub(crate) fn invalidate_file_offer(&self) {
        self.set_file_offers_enabled(false);
    }

    pub(crate) fn set_file_offers_enabled(&self, enabled: bool) {
        if self.remote_offer.set_enabled(enabled) {
            self.remote_offer
                .policy_update_pending
                .store(true, Ordering::Release);
            if !self
                .remote_offer
                .policy_message_pending
                .swap(true, Ordering::AcqRel)
                && self
                    .cmd_tx
                    .send(ClientMsg::ClipboardPolicyEpoch(0))
                    .is_err()
            {
                self.remote_offer
                    .policy_message_pending
                    .store(false, Ordering::Release);
            }
            // The policy command itself is the helper-side barrier. Retire
            // app-side leases immediately without adding a second IPC marker.
            self.local_offers.desktop.retire();
            self.remote_offer.advance_local_content();
        }
    }

    pub(crate) fn clipboard_policy_status(&self) -> RdpClipboardPolicyStatus {
        self.remote_offer.policy_status()
    }

    pub(crate) fn file_requester_for_generation(&self, generation: u64) -> RdpFileRequester {
        self.file_requester_for_epoch(generation, self.remote_offer.revision())
    }

    pub(super) fn file_requester_for_epoch(
        &self,
        generation: u64,
        revision: u64,
    ) -> RdpFileRequester {
        RdpFileRequester {
            tx: Arc::new(Mutex::new(self.cmd_tx.clone())),
            generation,
            revision,
            current: self.remote_offer.clone(),
            replies: self.event_tx.clone(),
        }
    }

    pub(crate) fn owns_file_requester(&self, requester: &RdpFileRequester) -> bool {
        Arc::ptr_eq(&self.remote_offer, &requester.current)
    }

    pub(crate) fn clipboard_lease(&self, stamp: RemoteClipboardStamp) -> RdpClipboardLease {
        RdpClipboardLease {
            current: self.remote_offer.clone(),
            stamp,
        }
    }

    pub(crate) fn owns_clipboard_lease(&self, lease: &RdpClipboardLease) -> bool {
        Arc::ptr_eq(&self.remote_offer, &lease.current)
    }

    /// Ask the session to disconnect gracefully.
    pub fn close(&self) {
        self.local_offers.close();
        self.remote_offer.close();
        let _ = self.cmd_tx.send(ClientMsg::Close);
        if !self.reaper_started.swap(true, Ordering::AcqRel) {
            reap_helper_after_close(Arc::clone(&self.child));
        }
    }

    fn poll_event(&self) -> RdpEventPoll {
        let Ok(event) = self.event_rx.try_recv() else {
            return RdpEventPoll::Empty;
        };
        let event = if matches!(event, RdpClientEvent::ClipboardWake) {
            let Some(event) = self.remote_offer.take_clipboard_event() else {
                return RdpEventPoll::Discarded;
            };
            event
        } else {
            event
        };
        match &event {
            RdpClientEvent::ClipboardTextForRevision { stamp, .. }
            | RdpClientEvent::ClipboardImageForRevision { stamp, .. }
                if !self.remote_offer.accepts_remote_copy(*stamp) =>
            {
                return RdpEventPoll::Discarded
            }
            RdpClientEvent::ClipboardFiles { generation, files }
                if !files.is_empty() && !self.remote_offer.accepts(*generation) =>
            {
                return RdpEventPoll::Discarded;
            }
            RdpClientEvent::ClipboardFilesForEpoch {
                generation,
                revision,
                files,
            } if !self.remote_offer.revision_is_current(*revision)
                || (!files.is_empty()
                    && !self.remote_offer.accepts_revision(*generation, *revision)) =>
            {
                return RdpEventPoll::Discarded;
            }
            _ => {}
        }
        RdpEventPoll::Event(event)
    }

    pub(super) fn poll_event_for_session(&self) -> RdpEventPoll {
        self.poll_event()
    }

    /// Poll for a deliverable event, discarding at most one tick's stale offers.
    #[must_use]
    pub fn try_recv(&self) -> Option<RdpClientEvent> {
        for _ in 0..super::MAX_DESKTOP_EVENTS_PER_POLL {
            match self.poll_event() {
                RdpEventPoll::Event(event) => return Some(event),
                RdpEventPoll::Discarded => {}
                RdpEventPoll::Empty => return None,
            }
        }
        None
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
        self.close();
    }
}

// Killing the direct child normally releases its pipes, but an inheriting
// descendant can keep them open. Do not block the UI or claim to reap a tree.
fn terminate_helper(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn reap_helper_after_close(child: Arc<Mutex<Option<Child>>>) {
    let _ = thread::Builder::new()
        .name("adit-rdp-client-reap".into())
        .spawn(move || {
            // Allow a graceful disconnect and asynchronous spawn before killing
            // the direct child. A spawn finishing later checks the closed state.
            for _ in 0..400 {
                if let Ok(mut guard) = child.lock() {
                    if let Some(c) = guard.as_mut() {
                        if matches!(c.try_wait(), Ok(Some(_))) {
                            return;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(25));
            }
            if let Ok(mut guard) = child.lock() {
                if let Some(c) = guard.as_mut() {
                    terminate_helper(c);
                }
            }
        });
}

#[derive(Debug, thiserror::Error)]
pub enum RdpClientError {
    #[error("the RDP helper ({0}) was not found; reinstall Adit")]
    HelperMissing(String),
    #[error("could not start the RDP helper: {0}")]
    Spawn(String),
    #[error("could not talk to the RDP helper: {0}")]
    Io(String),
    #[error("RDP IPC 协商失败，请同时更新主程序和 RDP 辅助进程：{0}")]
    Protocol(String),
    #[error("RDP 连接已取消")]
    Cancelled,
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

fn helper_command(helper: &std::path::Path) -> Command {
    let mut command = Command::new(helper);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    no_window(&mut command);
    command
}

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
    remote_offer.configure_clipboard(request.enable_clipboard);
    let remote_offer_bg = remote_offer.clone();
    let local_offers = Arc::new(LocalFileOffers::default());
    let local_offers_bg = local_offers.clone();
    let handle_event_tx = event_tx.clone();
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

    let surface_bg = Arc::clone(&surface);
    let child_bg = Arc::clone(&child_slot);
    thread::Builder::new()
        .name("adit-rdp-client-spawn".into())
        .spawn(move || {
            if remote_offer_bg.closed.load(Ordering::Acquire) {
                let _ = event_tx.send(RdpClientEvent::Closed);
                return;
            }
            let mut request = request;
            let mut command = helper_command(&helper);
            let configured = match remote_offer_bg.ensure_policy_mapping() {
                Ok(mapping) if mapping.configure_child(&mut command).is_ok() => {
                    request.clipboard_policy_id = Some(mapping.id().to_owned());
                    true
                }
                _ => {
                    remote_offer_bg.fail_policy(RdpClipboardPolicyFailure::MappingUnavailable);
                    request.enable_clipboard = false;
                    request.clipboard_policy_id = None;
                    false
                }
            };
            let spawned = match command.spawn() {
                Ok(child) => Ok(child),
                Err(_) if configured => {
                    // An inherited capability can fail independently of process
                    // creation. Retry without clipboard rather than losing RDP.
                    remote_offer_bg.fail_policy(RdpClipboardPolicyFailure::MappingUnavailable);
                    request.enable_clipboard = false;
                    request.clipboard_policy_id = None;
                    helper_command(&helper).spawn()
                }
                Err(error) => Err(error),
            };
            let mut child = match spawned {
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
                WireOptions {
                    remote_offer: Arc::clone(&remote_offer_bg),
                    local_offers: local_offers_bg,
                    handshake_timeout: Duration::from_secs(10),
                    cleanup_child: Some(Arc::clone(&child_bg)),
                },
            ) {
                terminate_helper(&mut child);
                if !matches!(error, RdpClientError::Cancelled) {
                    let _ = event_tx.send(RdpClientEvent::Error(error.to_string()));
                }
                let _ = event_tx.send(RdpClientEvent::Closed);
                return;
            }
            if let Ok(mut slot) = child_bg.lock() {
                if remote_offer_bg.closed.load(Ordering::Acquire) {
                    // Close can win after handshake, including after the close
                    // reaper's initial wait for a slow spawn has expired.
                    terminate_helper(&mut child);
                } else {
                    *slot = Some(child);
                }
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    Ok(RdpClientHandle {
        remote_offer,
        local_offers,
        event_tx: handle_event_tx,
        cmd_tx,
        event_rx,
        surface,
        child: child_slot,
        reaper_started: AtomicBool::new(false),
    })
}

fn wait_for_handshake<T>(
    receiver: std_mpsc::Receiver<T>,
    remote_offer: &FileEpoch,
    deadline: Instant,
    stage: &str,
) -> Result<T, RdpClientError> {
    loop {
        if remote_offer.closed.load(Ordering::Acquire) {
            return Err(RdpClientError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RdpClientError::Protocol(format!("等待辅助进程{stage}超时")));
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(20))) {
            Ok(value) => {
                // A close or deadline can race the wakeup; it takes precedence
                // over a late acknowledgement or completed write.
                if remote_offer.closed.load(Ordering::Acquire) {
                    return Err(RdpClientError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(RdpClientError::Protocol(format!("等待辅助进程{stage}超时")));
                }
                return Ok(value);
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RdpClientError::Protocol(format!(
                    "等待辅助进程{stage}时管道线程退出"
                )));
            }
        }
    }
}

struct HandshakeWriter<'a, W> {
    inner: &'a mut W,
    remote_offer: &'a FileEpoch,
    deadline: Instant,
}

impl<W: std::io::Write> HandshakeWriter<'_, W> {
    fn check_open(&self) -> std::io::Result<()> {
        if self.remote_offer.closed.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "RDP handshake stopped",
            ))
        } else {
            Ok(())
        }
    }
}

impl<W: std::io::Write> std::io::Write for HandshakeWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.check_open()?;
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.check_open()?;
        self.inner.flush()
    }
}

fn open_control_channel<W: std::io::Write + Send + 'static, R: std::io::Read + Send + 'static>(
    mut stdin: W,
    mut stdout: R,
    mut request: ConnectRequest,
    timeout: Duration,
    remote_offer: Arc<FileEpoch>,
) -> Result<(W, R), RdpClientError> {
    let deadline = Instant::now() + timeout;
    if remote_offer.closed.load(Ordering::Acquire) {
        return Err(RdpClientError::Cancelled);
    }

    // Neither hello worker owns the credentials. A late acknowledgement can
    // only return stdout; it cannot start Connect after cancellation.
    let (hello_write_tx, hello_write_rx) = std_mpsc::sync_channel(1);
    thread::Builder::new()
        .name("adit-rdp-client-hello-write".into())
        .spawn(move || {
            let result = write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION);
            let _ = hello_write_tx.send((stdin, result));
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;
    let (hello_read_tx, hello_read_rx) = std_mpsc::sync_channel(1);
    thread::Builder::new()
        .name("adit-rdp-client-hello-read".into())
        .spawn(move || {
            let result = read_helper_hello(&mut stdout);
            let _ = hello_read_tx.send((stdout, result));
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;
    let (mut stdin, hello_write) =
        wait_for_handshake(hello_write_rx, &remote_offer, deadline, "接收版本信息")?;
    hello_write.map_err(|e| RdpClientError::Protocol(e.to_string()))?;
    let (stdout, hello) = wait_for_handshake(hello_read_rx, &remote_offer, deadline, "确认版本")?;
    hello.map_err(|e| RdpClientError::Protocol(e.to_string()))?;

    match (!remote_offer.mapping_failed()).then(|| remote_offer.ensure_policy_mapping()) {
        Some(Ok(mapping)) => request.clipboard_policy_id = Some(mapping.id().to_owned()),
        _ => {
            // The desktop remains usable even if a policy mapping cannot be
            // created; without it no clipboard channel may be negotiated.
            remote_offer.fail_policy(RdpClipboardPolicyFailure::MappingUnavailable);
            request.clipboard_policy_id = None;
            request.enable_clipboard = false;
        }
    }

    // A pipe write can block indefinitely if the helper stops reading. Keep the
    // owning spawn thread free to notice close/timeout and kill the helper.
    let (connect_tx, connect_rx) = std_mpsc::sync_channel(1);
    let writer_offer = Arc::clone(&remote_offer);
    thread::Builder::new()
        .name("adit-rdp-client-connect".into())
        .spawn(move || {
            let result = if writer_offer.closed.load(Ordering::Acquire) {
                Err(RdpClientError::Cancelled)
            } else if Instant::now() >= deadline {
                Err(RdpClientError::Protocol("发送连接请求超时".into()))
            } else {
                let mut guarded = HandshakeWriter {
                    inner: &mut stdin,
                    remote_offer: &writer_offer,
                    deadline,
                };
                write_msg(&mut guarded, &ClientMsg::Connect(request)).map_err(|e| {
                    if writer_offer.closed.load(Ordering::Acquire) {
                        RdpClientError::Cancelled
                    } else if Instant::now() >= deadline {
                        RdpClientError::Protocol("发送连接请求超时".into())
                    } else {
                        RdpClientError::Io(e.to_string())
                    }
                })
            };
            let _ = connect_tx.send((stdin, result));
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;
    let (stdin, result) = wait_for_handshake(connect_rx, &remote_offer, deadline, "接收连接请求")?;
    result?;
    Ok((stdin, stdout))
}

struct RevocableWriter<'a, W> {
    inner: &'a mut W,
    offers: &'a LocalFileOffers,
    generation: Option<u64>,
    file_reply: bool,
    file_epoch: u64,
    written: usize,
    revoked: bool,
    desktop: Option<&'a DesktopOfferLease>,
}

impl<W: std::io::Write> RevocableWriter<'_, W> {
    fn check_current(&mut self) -> std::io::Result<()> {
        if !self.offers.is_closed()
            && !(self.file_reply && self.offers.is_suspended())
            && (!self.file_reply || self.offers.accepts_channel(self.file_epoch))
            && self
                .generation
                .is_none_or(|generation| self.offers.accepts_at_write(generation))
            && self
                .desktop
                .is_none_or(|lease| self.offers.desktop.accepts(lease))
        {
            Ok(())
        } else {
            self.revoked = true;
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "RDP IPC write revoked or closed",
            ))
        }
    }
}

impl<W: std::io::Write> std::io::Write for RevocableWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written == 0 {
            self.check_current()?;
        }
        // The clipboard check and pipe write cannot be atomic. Once any frame
        // byte was written, finish the frame to preserve the session; data
        // already in the pipe cannot be withdrawn.
        let written = self.inner.write(bytes)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn write_client_message<W: std::io::Write>(
    stdin: &mut W,
    message: ClientMsg,
    offers: &LocalFileOffers,
    desktop: Option<&DesktopOfferLease>,
) -> std::io::Result<()> {
    if matches!(message, ClientMsg::Close) {
        return write_msg(stdin, &message);
    }
    let is_file_reply = matches!(
        &message,
        ClientMsg::BoundFileContents { .. } | ClientMsg::FileContents { .. }
    );
    let file_epoch = match &message {
        ClientMsg::BoundFileContents { request, .. } => request.channel_epoch,
        _ => 0,
    };
    if is_file_reply && (offers.is_suspended() || !offers.accepts_channel(file_epoch)) {
        return Ok(());
    }
    let (message, file_reply, guard_generation) = match message {
        ClientMsg::BoundFileContents { request, data } => {
            let data = data.filter(|_| offers.accepts(request.generation));
            let file_reply = data.as_ref().map(|_| (request, true));
            (
                ClientMsg::BoundFileContents { request, data },
                file_reply,
                file_reply.map(|(request, _)| request.generation),
            )
        }
        ClientMsg::FileContents {
            generation,
            stream_id,
            data,
        } => {
            let data = data.filter(|_| offers.accepts(generation));
            let file_reply = data.as_ref().map(|_| {
                (
                    ClipboardFileRequestId {
                        channel_epoch: 0,
                        snapshot_revision: 0,
                        generation,
                        stream_id,
                    },
                    false,
                )
            });
            (
                ClientMsg::FileContents {
                    generation,
                    stream_id,
                    data,
                },
                file_reply,
                file_reply.map(|(request, _)| request.generation),
            )
        }
        ClientMsg::ClipboardFiles { generation, files } if !files.is_empty() => {
            if !offers.accepts(generation) {
                return Ok(());
            }
            (
                ClientMsg::ClipboardFiles { generation, files },
                None,
                Some(generation),
            )
        }
        message => (message, None, None),
    };
    let message = match (message, desktop) {
        (ClientMsg::Input(InputEvent::ClipboardText(text)), Some(lease)) => {
            ClientMsg::ClipboardTextForRevision {
                stamp: lease.stamp,
                text,
            }
        }
        (ClientMsg::ClipboardImage(image), Some(lease)) => ClientMsg::ClipboardImageForRevision {
            stamp: lease.stamp,
            image,
        },
        (message, _) => message,
    };
    let mut guarded = RevocableWriter {
        inner: stdin,
        offers,
        generation: guard_generation,
        file_reply: is_file_reply,
        file_epoch,
        written: 0,
        revoked: false,
        desktop,
    };
    let result = write_msg(&mut guarded, &message);
    if guarded.revoked
        && guarded.written == 0
        && is_file_reply
        && (offers.is_suspended() || !offers.accepts_channel(file_epoch))
    {
        return Ok(());
    }
    let can_refuse = guarded.revoked && guarded.written == 0 && !offers.is_closed();
    if let (true, Some((request, bound))) = (can_refuse, file_reply) {
        // No part of the frame was written, so a failed response is still safe.
        guarded.generation = None;
        let refusal = if !bound {
            ClientMsg::FileContents {
                generation: request.generation,
                stream_id: request.stream_id,
                data: None,
            }
        } else {
            ClientMsg::BoundFileContents {
                request,
                data: None,
            }
        };
        guarded.revoked = false;
        let result = write_msg(&mut guarded, &refusal);
        if guarded.revoked
            && guarded.written == 0
            && (offers.is_suspended() || !offers.accepts_channel(file_epoch))
        {
            Ok(())
        } else {
            result
        }
    } else if can_refuse && (guard_generation.is_some() || desktop.is_some()) {
        // A stale file offer that never started is simply omitted.
        Ok(())
    } else {
        result
    }
}

#[cfg(test)]
fn write_client_commands<W: std::io::Write>(
    stdin: &mut W,
    cmd_rx: std_mpsc::Receiver<ClientMsg>,
    offers: &LocalFileOffers,
) {
    while let Ok(message) = cmd_rx.recv() {
        if !write_queued_client_message(stdin, message, offers) {
            break;
        }
    }
    offers.close();
}

fn write_queued_client_message<W: std::io::Write>(
    stdin: &mut W,
    message: ClientMsg,
    offers: &LocalFileOffers,
) -> bool {
    let _file_offer = match &message {
        ClientMsg::ClipboardFiles { generation, files } if !files.is_empty() => {
            offers.take_file_offer(*generation)
        }
        _ => None,
    };
    // Close wins over everything still in the queue, including commands
    // queued before the handshake handed this pipe to the writer.
    let message = if offers.is_closed() {
        ClientMsg::Close
    } else {
        message
    };
    let closing = matches!(message, ClientMsg::Close);
    if closing {
        offers.close();
    }
    let file_reply = match &message {
        ClientMsg::BoundFileContents { request, .. } => Some(*request),
        ClientMsg::FileContents {
            generation,
            stream_id,
            ..
        } => Some(ClipboardFileRequestId {
            channel_epoch: 0,
            snapshot_revision: 0,
            generation: *generation,
            stream_id: *stream_id,
        }),
        _ => None,
    };
    let desktop = if matches!(
        &message,
        ClientMsg::Input(InputEvent::ClipboardText(_)) | ClientMsg::ClipboardImage(_)
    ) {
        offers.desktop.take()
    } else {
        None
    };
    // Every local text/image command must carry its original sequence.
    if matches!(
        &message,
        ClientMsg::Input(InputEvent::ClipboardText(_)) | ClientMsg::ClipboardImage(_)
    ) && desktop.is_none()
    {
        return !closing;
    }
    let result = write_client_message(stdin, message, offers, desktop.as_ref());
    if let Some(request) = file_reply {
        offers.finish_read(request);
    }
    result.is_ok() && !closing
}

fn fail_remote_request(
    events: &std_mpsc::Sender<RdpClientEvent>,
    generation: u64,
    revision: u64,
    stream_id: u32,
) {
    let _ = events.send(RdpClientEvent::FileContentsForEpoch {
        generation,
        revision,
        stream_id,
        data: None,
    });
}

fn dispatch_remote_file_contents(
    remote: &FileEpoch,
    events: &std_mpsc::Sender<RdpClientEvent>,
    generation: u64,
    stream_id: u32,
    data: Option<Vec<u8>>,
) {
    if let Some(revision) = remote.take_inflight_revision(generation, stream_id) {
        let data = data.filter(|_| remote.accepts_revision(generation, revision));
        let _ = events.send(RdpClientEvent::FileContentsForEpoch {
            generation,
            revision,
            stream_id,
            data,
        });
    }
    // An untracked response cannot safely identify its owner.
}

fn dispatch_remote_clipboard_offer(
    remote: &FileEpoch,
    events: &std_mpsc::Sender<RdpClientEvent>,
    generation: u64,
    files: Vec<ClipFile>,
    policy: Option<u64>,
    read_revision: u64,
) {
    if let Some(epoch) = policy {
        if let Some(event) = remote.observe_policy_offer(generation, files, epoch) {
            remote.queue_clipboard_event(events, event);
        }
    } else if remote.policy_mapping.get().is_none()
        && remote.policy_epoch.load(Ordering::Acquire) == 0
    {
        if let Some((accepted, revision)) =
            remote.observe_offer_at(generation, !files.is_empty(), read_revision)
        {
            let files = if accepted && remote.accepts_revision(generation, revision) {
                files
            } else {
                Vec::new()
            };
            remote.queue_clipboard_event(
                events,
                RdpClientEvent::ClipboardFilesForEpoch {
                    generation,
                    revision,
                    files,
                },
            );
        }
    }
}

fn read_host_frame<R: std::io::Read>(
    reader: &mut R,
    remote: &FileEpoch,
) -> (u64, std::io::Result<Option<HostMsg>>) {
    let revision = remote.revision();
    (revision, read_msg(reader))
}

struct RemoteRequestWriter<'a, W> {
    inner: &'a mut W,
    remote: &'a FileEpoch,
    generation: u64,
    revision: u64,
    written: usize,
    revoked: bool,
}

impl<W: std::io::Write> std::io::Write for RemoteRequestWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written == 0 && !self.remote.accepts_revision(self.generation, self.revision) {
            self.revoked = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "RDP remote file offer revoked",
            ));
        }
        // A started IPC frame must be completed; its bytes cannot be recalled.
        let written = self.inner.write(bytes)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn write_client_commands_with_remote<W: std::io::Write>(
    stdin: &mut W,
    cmd_rx: std_mpsc::Receiver<ClientMsg>,
    offers: &LocalFileOffers,
    remote: &FileEpoch,
    events: &std_mpsc::Sender<RdpClientEvent>,
) {
    while let Ok(message) = cmd_rx.recv() {
        // A queued policy update may pass any frame that has not yet started.
        if let Some(policy) = remote.take_policy_update() {
            if !write_queued_client_message(stdin, policy, offers) {
                break;
            }
        }
        if matches!(message, ClientMsg::ClipboardPolicyEpoch(_)) {
            remote
                .policy_message_pending
                .store(false, Ordering::Release);
            // A toggle racing with removal of the wakeup must not be lost.
            if let Some(policy) = remote.take_policy_update() {
                if !write_queued_client_message(stdin, policy, offers) {
                    break;
                }
            }
            continue;
        } else if let ClientMsg::RequestFileContents {
            generation,
            stream_id,
            ..
        } = &message
        {
            let (generation, stream_id) = (*generation, *stream_id);
            let revision = remote.start_queued_request(generation, stream_id);
            if offers.is_closed() {
                if revision.is_some() {
                    remote.cancel_started_request(generation, stream_id);
                }
                fail_remote_request(events, generation, revision.unwrap_or(0), stream_id);
                let _ = write_queued_client_message(stdin, ClientMsg::Close, offers);
                break;
            }
            let Some(revision) = revision else {
                fail_remote_request(events, generation, 0, stream_id);
                continue;
            };
            let mut guarded = RemoteRequestWriter {
                inner: stdin,
                remote,
                generation,
                revision,
                written: 0,
                revoked: false,
            };
            let result = write_msg(&mut guarded, &message);
            if guarded.revoked && guarded.written == 0 {
                remote.cancel_started_request(generation, stream_id);
                fail_remote_request(events, generation, revision, stream_id);
                continue;
            }
            if result.is_err() {
                if guarded.written == 0 {
                    remote.cancel_started_request(generation, stream_id);
                    fail_remote_request(events, generation, revision, stream_id);
                }
                break;
            }
        } else if !write_queued_client_message(stdin, message, offers) {
            break;
        }
    }
    // No accepted request may be left behind when the writer stops.
    remote.close();
    for message in cmd_rx.try_iter() {
        if let ClientMsg::RequestFileContents {
            generation,
            stream_id,
            ..
        } = message
        {
            let revision = remote
                .take_queued_revision(generation, stream_id)
                .unwrap_or(0);
            fail_remote_request(events, generation, revision, stream_id);
        }
    }
    offers.close();
}

struct WireOptions {
    remote_offer: Arc<FileEpoch>,
    local_offers: Arc<LocalFileOffers>,
    handshake_timeout: Duration,
    cleanup_child: Option<Arc<Mutex<Option<Child>>>>,
}

/// Negotiate the helper's version before sending `Connect`, then start the pipe
/// bridge threads. Runs on the background spawn thread. `cmd_rx`
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
    options: WireOptions,
) -> Result<(), RdpClientError> {
    let WireOptions {
        remote_offer,
        local_offers,
        handshake_timeout,
        cleanup_child,
    } = options;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdout unavailable".into()))?;

    let (mut stdin, mut stdout) = open_control_channel(
        stdin,
        stdout,
        request,
        handshake_timeout,
        Arc::clone(&remote_offer),
    )?;
    if matches!(
        remote_offer.policy_status(),
        RdpClipboardPolicyStatus::Failed {
            reason: RdpClipboardPolicyFailure::MappingUnavailable,
            ..
        }
    ) {
        let _ = event_tx.send(RdpClientEvent::Notice(
            "剪贴板共享不可用，远程桌面仍可使用。".into(),
        ));
    }
    if remote_offer.closed.load(Ordering::Acquire) || local_offers.is_closed() {
        return Err(RdpClientError::Cancelled);
    }

    // stdin writer: serialise ClientMsgs to the helper.
    let writer_offers = Arc::clone(&local_offers);
    let writer_remote = Arc::clone(&remote_offer);
    let writer_events = event_tx.clone();
    thread::Builder::new()
        .name("adit-rdp-client-stdin".into())
        .spawn(move || {
            write_client_commands_with_remote(
                &mut stdin,
                cmd_rx,
                &writer_offers,
                &writer_remote,
                &writer_events,
            );
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    // stdout reader: apply framebuffer tiles and forward discrete events.
    let reader_surface = Arc::clone(surface);
    thread::Builder::new()
        .name("adit-rdp-client-stdout".into())
        .spawn(move || {
            loop {
                // An IPC read may span disable and re-enable. An offer from
                // that read cannot be assigned to the new sharing epoch.
                let (read_revision, frame) = read_host_frame(&mut stdout, &remote_offer);
                match frame {
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
                    Ok(Some(HostMsg::ClipboardImage(_))) => {
                        // Unbound content cannot cross a live policy boundary.
                    }
                    Ok(Some(HostMsg::ClipboardRemoteCopy(stamp))) => {
                        remote_offer.observe_remote_copy(stamp);
                    }
                    Ok(Some(HostMsg::ClipboardImageForRevision { stamp, image })) => {
                        if remote_offer.accepts_remote_copy(stamp) {
                            remote_offer.queue_clipboard_event(&event_tx, RdpClientEvent::ClipboardImageForRevision { stamp, image });
                        }
                    }
                    Ok(Some(HostMsg::ClipboardTextForRevision { stamp, text })) => {
                        if remote_offer.accepts_remote_copy(stamp) {
                            remote_offer.queue_clipboard_event(&event_tx, RdpClientEvent::ClipboardTextForRevision { stamp, text });
                        }
                    }
                    Ok(Some(HostMsg::ClipboardNotice(message))) => {
                        let _ = event_tx.send(RdpClientEvent::Notice(message));
                    }
                    Ok(Some(HostMsg::ClipboardFiles { generation, files })) => {
                        dispatch_remote_clipboard_offer(&remote_offer, &event_tx, generation, files, None, read_revision);
                    }
                    Ok(Some(HostMsg::ClipboardFilesForPolicy { policy_epoch, generation, files })) => {
                        dispatch_remote_clipboard_offer(&remote_offer, &event_tx, generation, files, Some(policy_epoch), read_revision);
                    }
                    Ok(Some(HostMsg::ClipboardPolicyApplied { policy_epoch, enabled })) => {
                        if let Some(event) = remote_offer.acknowledge_policy(policy_epoch, enabled) {
                            remote_offer.queue_clipboard_event(&event_tx, event);
                        }
                    }
                    Ok(Some(HostMsg::ClipboardChannelReady { channel_epoch })) => {
                        if let Some(event) = remote_offer.clipboard_channel_ready(channel_epoch) {
                            remote_offer.queue_clipboard_event(&event_tx, event);
                        }
                    }
                    Ok(Some(HostMsg::ClipboardPolicyRejected)) => {
                        remote_offer.fail_policy(RdpClipboardPolicyFailure::HelperRejected);
                        let _ = event_tx.send(RdpClientEvent::Notice("辅助进程无法启用剪贴板共享；远程桌面仍可使用。".into()));
                    }
                    Ok(Some(HostMsg::FileContentsNeeded {
                        channel_epoch,
                        snapshot_revision,
                        generation,
                        stream_id,
                        index,
                        offset,
                        length,
                    })) => {
                        let event = RdpClientEvent::FileContentsNeededBound {
                            request: ClipboardFileRequestId {
                                channel_epoch,
                                snapshot_revision,
                                generation,
                                stream_id,
                            },
                            index,
                            offset,
                            length,
                        };
                        if !dispatch_inbound_file_request(
                            &local_offers,
                            &refuse_tx,
                            &event_tx,
                            event,
                        ) {
                            local_offers.close();
                            remote_offer.close();
                            let _ = refuse_tx.send(ClientMsg::Close);
                            if let Some(child) = cleanup_child {
                                reap_helper_after_close(child);
                            }
                            break;
                        }
                    }
                    Ok(Some(HostMsg::FileContents {
                        generation,
                        stream_id,
                        data,
                    })) => {
                        dispatch_remote_file_contents(
                            &remote_offer,
                            &event_tx,
                            generation,
                            stream_id,
                            data,
                        );
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
                    Ok(Some(HostMsg::ClipboardText(_))) => {
                        // Legacy unbound frames are accepted only by synthetic peers.
                    }
                    Ok(Some(HostMsg::ClipboardFileDirectionUnavailable {
                        channel_epoch,
                        message,
                    })) => {
                        let _ = event_tx.send(RdpClientEvent::ClipboardFileDirectionUnavailable {
                            channel_epoch,
                            message,
                        });
                    }
                    Ok(Some(HostMsg::LocalFileDirectionSuspended { channel_epoch })) => {
                        local_offers.suspend();
                        local_offers.suspension_confirmed.store(true, Ordering::Release);
                        let _ = event_tx.send(RdpClientEvent::ClipboardFileDirectionUnavailable {
                            channel_epoch,
                            message: "本地文件发送至远端已暂时禁用；桌面、文字、图片和远端文件接收仍可使用。已发送或已被传输层接收的数据仍可能到达。".into(),
                        });
                    }
                    Ok(Some(HostMsg::ClipboardChannelChanged { channel_epoch })) => {
                        remote_offer.clipboard_channel_changed(channel_epoch);
                        local_offers.observe_channel_epoch(channel_epoch);
                        let _ = event_tx.send(RdpClientEvent::ClipboardChannelChanged { channel_epoch });
                    }
                    Ok(Some(HostMsg::ClipboardLocalFileDirectionReady { channel_epoch })) => {
                        local_offers.observe_channel_epoch(channel_epoch);
                        if !local_offers.is_suspended() && local_offers.accepts_channel(channel_epoch) {
                            let _ = event_tx.send(RdpClientEvent::ClipboardLocalFileDirectionReady { channel_epoch });
                        }
                    }
                    Ok(Some(HostMsg::Error(message))) => {
                        local_offers.close();
                        remote_offer.close();
                        let _ = refuse_tx.send(ClientMsg::Close);
                        let _ = event_tx.send(RdpClientEvent::Error(message));
                    }
                    Ok(Some(HostMsg::Closed)) | Ok(None) => {
                        local_offers.close();
                        remote_offer.close();
                        let _ = refuse_tx.send(ClientMsg::Close);
                        let _ = event_tx.send(RdpClientEvent::Closed);
                        break;
                    }
                    Err(_) => {
                        local_offers.close();
                        remote_offer.close();
                        let _ = refuse_tx.send(ClientMsg::Close);
                        let _ = event_tx.send(RdpClientEvent::Closed);
                        break;
                    }
                }
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod ipc_writer_tests {
    use super::*;
    use std::io::{self, BufRead, Write};

    #[test]
    fn queued_remote_content_is_rechecked_across_policy_copy_and_session_changes() {
        let (first, _) = RdpClientHandle::test_fixture_with_commands();
        let (second, _) = RdpClientHandle::test_fixture_with_commands();
        first.remote_offer.ensure_policy_mapping().unwrap();
        second.remote_offer.ensure_policy_mapping().unwrap();
        let stamp = RemoteClipboardStamp {
            local: ClipboardStamp {
                policy_epoch: 0,
                content_revision: 0,
            },
            channel_epoch: 1,
            copy_revision: 1,
        };
        for handle in [&first, &second] {
            assert!(handle.remote_offer.observe_remote_copy(stamp));
            handle.remote_offer.queue_clipboard_event(
                &handle.event_tx,
                RdpClientEvent::ClipboardTextForRevision {
                    stamp,
                    text: "synthetic".into(),
                },
            );
        }
        first.set_file_offers_enabled(false);
        first.set_file_offers_enabled(true);
        assert!(matches!(first.poll_event(), RdpEventPoll::Discarded));
        assert!(matches!(
            second.poll_event(),
            RdpEventPoll::Event(RdpClientEvent::ClipboardTextForRevision { .. })
        ));

        let local = first.remote_offer.content_stamp().unwrap();
        let old = RemoteClipboardStamp {
            local,
            channel_epoch: 1,
            copy_revision: 2,
        };
        let new = RemoteClipboardStamp {
            copy_revision: 3,
            ..old
        };
        assert!(first.remote_offer.observe_remote_copy(old));
        let lease = first.clipboard_lease(old);
        first.remote_offer.queue_clipboard_event(
            &first.event_tx,
            RdpClientEvent::ClipboardImageForRevision {
                stamp: old,
                image: vec![1, 2, 3],
            },
        );
        assert!(first.remote_offer.observe_remote_copy(new));
        assert!(!lease.is_current());
        assert_eq!(
            lease.with_current(|| panic!("stale content reached the synthetic clipboard")),
            None::<()>
        );
        assert!(matches!(first.poll_event(), RdpEventPoll::Discarded));
        assert!(second.remote_offer.accepts_remote_copy(stamp));
    }

    #[test]
    fn desktop_offers_are_bound_to_each_session_until_the_first_ipc_byte() {
        use std::sync::atomic::AtomicU32;
        let current = Arc::new(AtomicU32::new(10));
        let (first, first_commands) = RdpClientHandle::test_fixture_with_commands();
        let (second, second_commands) = RdpClientHandle::test_fixture_with_commands();
        for handle in [&first, &second] {
            let source = Arc::clone(&current);
            handle.send_clipboard_text_with_sequence(
                "old text".into(),
                RdpClipboardSequence::new(10, move || source.load(Ordering::Acquire)),
            );
            let source = Arc::clone(&current);
            handle.send_clipboard_image_with_sequence(
                vec![1, 2, 3],
                RdpClipboardSequence::new(10, move || source.load(Ordering::Acquire)),
            );
        }
        current.store(11, Ordering::Release);
        for handle in [&first, &second] {
            handle.observe_local_clipboard_sequence(11);
            let source = Arc::clone(&current);
            handle.send_clipboard_text_with_sequence(
                "new text".into(),
                RdpClipboardSequence::new(11, move || source.load(Ordering::Acquire)),
            );
            handle.send_input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            });
        }
        for (handle, commands) in [(&first, first_commands), (&second, second_commands)] {
            let mut wire = Vec::new();
            while let Ok(command) = commands.try_recv() {
                assert!(write_queued_client_message(
                    &mut wire,
                    command,
                    &handle.local_offers
                ));
            }
            let mut wire = io::Cursor::new(wire);
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut wire).unwrap(),
                Some(ClientMsg::LocalFileOfferRejected)
            ));
            assert!(
                matches!(read_msg::<_, ClientMsg>(&mut wire).unwrap(), Some(ClientMsg::ClipboardTextForRevision { text, .. }) if text == "new text")
            );
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut wire).unwrap(),
                Some(ClientMsg::Input(InputEvent::Key { .. }))
            ));
            assert!(read_msg::<_, ClientMsg>(&mut wire).unwrap().is_none());
            assert!(!handle.local_offers.is_closed());
        }
    }

    #[test]
    fn observing_sequence_never_waits_for_a_blocked_writer_sequence_query() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        handle.send_clipboard_text_with_sequence(
            "stale".into(),
            RdpClipboardSequence::new(7, move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                7
            }),
        );
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || {
            let mut wire = Vec::new();
            write_client_commands(&mut wire, commands, &offers);
            wire
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.observe_local_clipboard_sequence(0);
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        release_tx.send(()).unwrap();
        let mut wire = io::Cursor::new(worker.join().unwrap());
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::LocalFileOfferRejected)
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Input(InputEvent::Key { .. }))
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Close)
        ));
    }

    #[test]
    fn desktop_offer_capacity_is_bounded_and_reports_a_recoverable_notice() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        for _ in 0..MAX_DESKTOP_OFFERS + 3 {
            handle.send_clipboard_text_with_sequence(
                "bounded".into(),
                RdpClipboardSequence::new(1, || 1),
            );
        }
        assert_eq!(
            handle
                .local_offers
                .desktop
                .budget
                .count
                .load(Ordering::Acquire),
            MAX_DESKTOP_OFFERS
        );
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::Notice(message)) if message.contains("重新复制") && message.contains("仍可使用"))
        );
        assert!(handle.try_recv().is_none());
        let mut wire = Vec::new();
        while let Ok(command) = commands.try_recv() {
            assert!(write_queued_client_message(
                &mut wire,
                command,
                &handle.local_offers
            ));
        }
        assert_eq!(
            handle
                .local_offers
                .desktop
                .budget
                .count
                .load(Ordering::Acquire),
            0
        );
        handle
            .send_clipboard_text_with_sequence("retry".into(), RdpClipboardSequence::new(1, || 1));
        assert!(write_queued_client_message(
            &mut wire,
            commands.try_recv().unwrap(),
            &handle.local_offers
        ));
        assert!(!handle.local_offers.is_closed());
    }

    #[test]
    fn sequence_markers_coalesce_until_a_file_offer_needs_a_new_revoke() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        for sequence in 1..=1000 {
            handle.observe_local_clipboard_sequence(sequence);
        }
        handle.send_clipboard_files(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 1,
                is_dir: false,
            }],
            Some(RdpClipboardSequence::new(1000, || 1000)),
        );
        handle.observe_local_clipboard_sequence(1001);
        let mut wire = Vec::new();
        while let Ok(command) = commands.try_recv() {
            assert!(write_queued_client_message(
                &mut wire,
                command,
                &handle.local_offers
            ));
        }
        let mut wire = io::Cursor::new(wire);
        for _ in 0..2 {
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut wire).unwrap(),
                Some(ClientMsg::LocalFileOfferRejected)
            ));
        }
        assert!(read_msg::<_, ClientMsg>(&mut wire).unwrap().is_none());
        assert!(!handle.local_offers.is_closed());
    }

    #[test]
    fn repeated_file_offers_and_revoke_markers_stay_bounded_while_the_pipe_is_stalled() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        for sequence in 1..=128 {
            handle.observe_local_clipboard_sequence(sequence);
            handle.send_clipboard_files(
                vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
                Some(RdpClipboardSequence::new(sequence, move || sequence)),
            );
        }
        assert_eq!(
            handle
                .local_offers
                .file_offer_budget
                .count
                .load(Ordering::Acquire),
            MAX_DESKTOP_OFFERS
        );
        let mut wire = Vec::new();
        let mut commands_seen = 0;
        while let Ok(command) = commands.try_recv() {
            commands_seen += 1;
            assert!(write_queued_client_message(
                &mut wire,
                command,
                &handle.local_offers
            ));
        }
        assert!(commands_seen <= 2 * MAX_DESKTOP_OFFERS + 1);
        assert_eq!(
            handle
                .local_offers
                .file_offer_budget
                .count
                .load(Ordering::Acquire),
            0
        );
        assert!(!handle.local_offers.is_closed());
        handle.send_clipboard_files(
            vec![ClipFile {
                name: "retry.bin".into(),
                size: 1,
                is_dir: false,
            }],
            Some(RdpClipboardSequence::new(128, || 128)),
        );
        assert!(write_queued_client_message(
            &mut wire,
            commands.try_recv().unwrap(),
            &handle.local_offers
        ));
        assert_eq!(
            handle
                .local_offers
                .file_offer_budget
                .count
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn repeated_empty_file_revocations_are_coalesced_between_offers() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        for _ in 0..1000 {
            handle.send_clipboard_files(Vec::new(), None);
        }
        assert!(
            matches!(commands.try_recv(), Ok(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty())
        );
        assert!(commands.try_recv().is_err());
        handle.send_clipboard_files(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 1,
                is_dir: false,
            }],
            Some(RdpClipboardSequence::new(1, || 1)),
        );
        handle.send_clipboard_files(Vec::new(), None);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::ClipboardFiles { generation: 1, .. })
        ));
        assert!(
            matches!(commands.try_recv(), Ok(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty())
        );
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn policy_tag_keeps_new_file_after_an_idle_read_crosses_off_on() {
        struct GateReader {
            pipe: io::Cursor<Vec<u8>>,
            entered: Option<std_mpsc::Sender<()>>,
            release: std_mpsc::Receiver<()>,
        }
        impl io::Read for GateReader {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    self.release
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(io::Error::other)?;
                }
                io::Read::read(&mut self.pipe, bytes)
            }
        }
        let epoch = Arc::new(FileEpoch::default());
        let (events, received) = std_mpsc::channel();
        let file = |name: &str| {
            vec![ClipFile {
                name: name.into(),
                size: 1,
                is_dir: false,
            }]
        };
        let mut bytes = Vec::new();
        write_msg(
            &mut bytes,
            &HostMsg::ClipboardFilesForPolicy {
                policy_epoch: 0,
                generation: 7,
                files: file("old.bin"),
            },
        )
        .unwrap();
        write_msg(
            &mut bytes,
            &HostMsg::ClipboardFilesForPolicy {
                policy_epoch: 2,
                generation: 8,
                files: file("new.bin"),
            },
        )
        .unwrap();
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let reader_epoch = Arc::clone(&epoch);
        let worker = thread::spawn(move || {
            let mut pipe = GateReader {
                pipe: io::Cursor::new(bytes),
                entered: Some(entered_tx),
                release: release_rx,
            };
            for _ in 0..2 {
                let (read_revision, frame) = read_host_frame(&mut pipe, &reader_epoch);
                let Some(HostMsg::ClipboardFilesForPolicy {
                    policy_epoch,
                    generation,
                    files,
                }) = frame.unwrap()
                else {
                    panic!("expected file offer")
                };
                dispatch_remote_clipboard_offer(
                    &reader_epoch,
                    &events,
                    generation,
                    files,
                    Some(policy_epoch),
                    read_revision,
                );
            }
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        epoch.set_enabled(false);
        epoch.set_enabled(true);
        assert_eq!(epoch.policy_epoch.load(Ordering::Acquire), 2);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(matches!(
            received.try_recv(),
            Ok(RdpClientEvent::ClipboardWake)
        ));
        assert!(
            matches!(epoch.take_clipboard_event(), Some(RdpClientEvent::ClipboardFilesForEpoch { generation: 8, files, .. }) if files[0].name == "new.bin")
        );
        assert!(received.try_recv().is_err());
        assert!(epoch.accepts(8));
    }

    #[test]
    fn an_old_policy_offer_cannot_be_reclassified_after_reenable() {
        let epoch = FileEpoch::default();
        let (events, received) = std_mpsc::channel();
        epoch.set_enabled(false);
        epoch.set_enabled(true);
        // A frame created before the shared policy changed retains its old
        // epoch, even if the reader receives it after re-enabling.
        dispatch_remote_clipboard_offer(
            &epoch,
            &events,
            9,
            vec![ClipFile {
                name: "old-policy.bin".into(),
                size: 1,
                is_dir: false,
            }],
            Some(0),
            0,
        );
        assert!(received.try_recv().is_err());
        assert!(!epoch.accepts(9));
    }

    #[test]
    fn sharing_policy_overtakes_unstarted_frames_and_coalesces_toggles() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        for _ in 0..1000 {
            handle.set_file_offers_enabled(false);
            handle.set_file_offers_enabled(true);
        }
        handle.send_input(InputEvent::Key {
            scancode: 31,
            extended: false,
            pressed: true,
        });
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        let mut wire = Vec::new();
        write_client_commands_with_remote(
            &mut wire,
            commands,
            &handle.local_offers,
            &handle.remote_offer,
            &handle.event_tx,
        );
        let mut wire = io::Cursor::new(wire);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::ClipboardPolicyEpoch(2000))
        ));
        for scancode in [30, 31] {
            assert!(matches!(read_msg::<_, ClientMsg>(&mut wire).unwrap(),
                Some(ClientMsg::Input(InputEvent::Key { scancode: actual, .. })) if actual == scancode));
        }
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Close)
        ));
        assert!(read_msg::<_, ClientMsg>(&mut wire).unwrap().is_none());
    }

    #[test]
    fn duplicate_remote_stream_is_rejected_before_the_helper_can_reply() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let first = handle.test_remote_file_offer(7);
        let second = handle.file_requester_for_generation(7);

        assert_eq!(first.try_request(41, 0, 0, 1), Ok(()));
        assert_eq!(
            second.try_request(41, 1, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        assert!(handle.try_recv().is_none());
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents {
                generation: 7,
                stream_id: 41,
                index: 0,
                ..
            })
        ));
        assert!(commands.try_recv().is_err());

        // The move from the writer queue to in-flight ownership is atomic.
        assert_eq!(
            handle.remote_offer.start_queued_request(7, 41),
            Some(first.revision)
        );
        assert_eq!(
            second.try_request(41, 1, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        dispatch_remote_file_contents(&handle.remote_offer, &handle.event_tx, 7, 41, Some(vec![9]));
        assert!(matches!(
            handle.try_recv(),
            Some(RdpClientEvent::FileContentsForEpoch { generation: 7, stream_id: 41, data: Some(data), .. })
                if data == [9]
        ));
        assert!(handle.try_recv().is_none());
    }

    #[test]
    fn completed_remote_stream_stays_owned_until_its_reply_is_delivered() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let requester = handle.test_remote_file_offer(7);
        assert_eq!(requester.try_request(41, 0, 0, 1), Ok(()));
        assert!(commands.try_recv().is_ok());
        assert_eq!(
            handle.remote_offer.start_queued_request(7, 41),
            Some(requester.revision)
        );
        dispatch_remote_file_contents(&handle.remote_offer, &handle.event_tx, 7, 41, Some(vec![9]));
        // The first response is still queued for its caller. Reusing the pair
        // now could deliver that response to the second caller instead.
        assert_eq!(
            requester.try_request(41, 1, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::FileContentsForEpoch {
            stream_id: 41, data: Some(data), ..
        }) if data == [9])
        );
        // Neither dequeuing nor discarding one consumer's copy acknowledges
        // delivery to the other consumer.
        assert_eq!(
            requester.try_request(41, 1, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        assert!(handle.try_recv().is_none());
    }

    #[test]
    fn bounded_stream_history_refuses_new_pages_without_closing_the_desktop() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        handle
            .remote_offer
            .state
            .lock()
            .unwrap()
            .used_streams
            .max_pages = 1;
        let requester = handle.test_remote_file_offer(7);
        assert_eq!(requester.try_request(0, 0, 0, 1), Ok(()));
        assert_eq!(
            requester.try_request(REMOTE_STREAM_PAGE_BITS, 0, 0, 1),
            Err(RdpFileRequestError::StreamIdCapacity)
        );
        requester.request(REMOTE_STREAM_PAGE_BITS, 0, 0, 1);
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::Notice(message))
            if message.contains("手动重新连接") && message.contains("仍可使用"))
        );
        assert!(handle.try_recv().is_none());
        // An unused ID in the existing page remains safe despite exhaustion.
        assert_eq!(
            requester.try_request(REMOTE_STREAM_PAGE_BITS - 1, 0, 0, 1),
            Ok(())
        );
        handle.send_clipboard_image(vec![7]);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents { stream_id: 0, .. })
        ));
        assert!(
            matches!(commands.try_recv(), Ok(ClientMsg::RequestFileContents { stream_id, .. })
            if stream_id == REMOTE_STREAM_PAGE_BITS - 1)
        );
        assert!(matches!(commands.try_recv(), Ok(ClientMsg::ClipboardImage(data)) if data == [7]));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn queued_unavailable_failure_cannot_be_delivered_to_a_new_request() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let stale = handle.test_remote_file_offer(7);
        handle.set_file_offers_enabled(false);
        handle.set_file_offers_enabled(true);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::ClipboardPolicyEpoch(0))
        ));
        let current = handle.test_remote_file_offer(7);

        // The compatibility API emits an asynchronous failure for a stale
        // request; its ID must be retired before that event is queued.
        stale.request(41, 0, 0, 1);
        assert_eq!(
            current.try_request(41, 0, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::FileContentsForEpoch {
            stream_id: 41, data: None, revision, ..
        }) if revision == stale.revision)
        );
        assert!(commands.try_recv().is_err());
        assert_eq!(current.try_request(42, 0, 0, 1), Ok(()));
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents { stream_id: 42, .. })
        ));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn legacy_duplicate_request_reports_a_notice_not_a_reply_to_the_original() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let requester = handle.test_remote_file_offer(7);
        requester.request(41, 0, 0, 1);
        requester.request(41, 1, 0, 1);
        assert!(matches!(handle.try_recv(), Some(RdpClientEvent::Notice(_))));
        assert!(handle.try_recv().is_none());
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents { index: 0, .. })
        ));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn concurrent_requesters_cannot_admit_the_same_remote_stream() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let requester = handle.test_remote_file_offer(7);
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let workers: Vec<_> = (0..16)
            .map(|index| {
                let requester = requester.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    requester.try_request(41, index, 0, 1)
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(RdpFileRequestError::DuplicateStream))
                .count(),
            15
        );
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents { stream_id: 41, .. })
        ));
        assert!(commands.try_recv().is_err());
        assert!(handle.try_recv().is_none());
    }

    #[test]
    fn reused_generation_cannot_claim_a_stream_still_owned_by_an_old_epoch() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let old = handle.test_remote_file_offer(7);
        assert_eq!(old.try_request(41, 0, 0, 1), Ok(()));
        assert!(commands.try_recv().is_ok());
        assert_eq!(
            handle.remote_offer.start_queued_request(7, 41),
            Some(old.revision)
        );

        handle.set_file_offers_enabled(false);
        handle.set_file_offers_enabled(true);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::ClipboardPolicyEpoch(0))
        ));
        let new = handle.test_remote_file_offer(7);
        assert_eq!(
            new.try_request(41, 1, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        dispatch_remote_file_contents(&handle.remote_offer, &handle.event_tx, 7, 41, Some(vec![9]));
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::FileContentsForEpoch {
            revision, stream_id: 41, data: None, ..
        }) if revision == old.revision)
        );
        assert_eq!(
            new.try_request(41, 1, 0, 1),
            Err(RdpFileRequestError::DuplicateStream)
        );
        assert_eq!(new.try_request(42, 1, 0, 1), Ok(()));
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents { index: 1, .. })
        ));
    }

    #[test]
    fn bound_reply_keeps_its_identity_through_the_ipc_writer() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        handle.test_local_offer();
        let request = ClipboardFileRequestId {
            channel_epoch: 12,
            snapshot_revision: 3,
            generation: handle.test_local_generation(),
            stream_id: 41,
        };
        handle.test_send_event(RdpClientEvent::FileContentsNeededBound {
            request,
            index: 0,
            offset: 0,
            length: 1,
        });
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::FileContentsNeededBound { request: got, .. }) if got == request)
        );
        handle
            .local_file_read_lease_for_request(request)
            .answer(41, Some(vec![7]));
        let mut bytes = Vec::new();
        assert!(write_queued_client_message(
            &mut bytes,
            commands.try_recv().unwrap(),
            &handle.local_offers
        ));
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut io::Cursor::new(bytes)).unwrap(),
            Some(ClientMsg::BoundFileContents { request: got, data: Some(data) })
                if got == request && data == [7])
        );
        assert!(handle.local_offers.reads.lock().unwrap().is_empty());
    }

    #[test]
    fn revoked_bound_reply_retains_its_identity_when_refused_by_the_writer() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        handle.test_local_offer();
        let request = ClipboardFileRequestId {
            channel_epoch: 12,
            snapshot_revision: 3,
            generation: handle.test_local_generation(),
            stream_id: 41,
        };
        handle.test_send_event(RdpClientEvent::FileContentsNeededBound {
            request,
            index: 0,
            offset: 0,
            length: 1,
        });
        handle
            .local_file_read_lease_for_request(request)
            .answer(41, Some(vec![7]));
        handle.local_offers.revoke();
        let mut bytes = Vec::new();
        assert!(write_queued_client_message(
            &mut bytes,
            commands.try_recv().unwrap(),
            &handle.local_offers
        ));
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut io::Cursor::new(bytes)).unwrap(),
            Some(ClientMsg::BoundFileContents { request: got, data: None }) if got == request)
        );
    }

    struct PauseAtFirstFlush {
        bytes: Arc<Mutex<Vec<u8>>>,
        ready: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
    }

    impl Write for PauseAtFirstFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some(ready) = self.ready.take() {
                ready.send(()).unwrap();
                self.release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
            }
            Ok(())
        }
    }

    #[test]
    fn exhausted_refusals_suspend_one_session_and_discard_its_queued_replies() {
        let budget = Arc::new(InboundReadBudget::new(1, 32));
        let (a, a_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
        let (b, b_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
        a.test_local_offer();
        b.test_local_offer();
        let a_generation = a.test_local_generation();
        let b_generation = b.test_local_generation();
        let request = |generation, stream_id| RdpClientEvent::FileContentsNeededBound {
            request: ClipboardFileRequestId {
                channel_epoch: 7,
                snapshot_revision: 1,
                generation,
                stream_id,
            },
            index: 0,
            offset: 0,
            length: 1,
        };
        a.test_send_event(request(a_generation, 0));
        let first = ClipboardFileRequestId {
            channel_epoch: 7,
            snapshot_revision: 1,
            generation: a_generation,
            stream_id: 0,
        };
        a.local_file_read_lease_for_request(first)
            .answer(0, Some(vec![7]));
        for stream_id in 1..=16 {
            a.test_send_event(request(a_generation, stream_id));
        }
        assert_eq!(budget.test_counts(), (1, 16));
        a.test_send_event(request(a_generation, 17));
        for stream_id in 18..10_000 {
            a.test_send_event(request(a_generation, stream_id));
        }
        assert!(a.local_offers.is_suspended());
        assert_eq!(budget.test_counts(), (1, 16));

        // A blocked writer still owns its slots; the other session can send
        // an explicit refusal from its own reserved allowance.
        b.test_send_event(request(b_generation, 99));
        let mut other_wire = Vec::new();
        assert!(write_queued_client_message(
            &mut other_wire,
            b_commands.try_recv().unwrap(),
            &b.local_offers
        ));
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut io::Cursor::new(other_wire)).unwrap(),
            Some(ClientMsg::BoundFileContents { request, data: None }) if request.stream_id == 99)
        );
        assert_eq!(budget.test_counts(), (1, 16));

        a.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        let mut wire = Vec::new();
        while let Ok(command) = a_commands.try_recv() {
            assert!(write_queued_client_message(
                &mut wire,
                command,
                &a.local_offers
            ));
        }
        assert_eq!(budget.test_counts(), (0, 0));
        let mut wire = io::Cursor::new(wire);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::SuspendLocalFileDirection)
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Input(InputEvent::Key { .. }))
        ));
        assert!(read_msg::<_, ClientMsg>(&mut wire).unwrap().is_none());
        b.test_send_event(request(b_generation, 100));
        let second = ClipboardFileRequestId {
            channel_epoch: 7,
            snapshot_revision: 1,
            generation: b_generation,
            stream_id: 100,
        };
        b.local_file_read_lease_for_request(second)
            .answer(100, Some(vec![8]));
        let mut other_wire = Vec::new();
        assert!(write_queued_client_message(
            &mut other_wire,
            b_commands.try_recv().unwrap(),
            &b.local_offers
        ));
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut io::Cursor::new(other_wire)).unwrap(),
            Some(ClientMsg::BoundFileContents { request, data: Some(data) }) if request == second && data == [8])
        );
        assert_eq!(budget.test_counts(), (0, 0));
        assert!(matches!(
            a.try_recv(),
            Some(RdpClientEvent::FileContentsNeededBound { .. })
        ));
        assert!(matches!(
            a.try_recv(),
            Some(RdpClientEvent::ClipboardFileDirectionUnavailable { .. })
        ));
        assert!(a.try_recv().is_none());
    }

    #[test]
    fn selection_marker_revokes_queued_local_file_bytes_without_closing() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let lease = handle.test_local_offer();
        let secret = vec![201, 149, 71, 37, 23];
        lease.answer(7, Some(secret.clone()));
        handle.notify_local_file_selection();
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        assert!(!lease.is_current());
        let mut wire = Vec::new();
        while let Ok(command) = commands.try_recv() {
            assert!(write_queued_client_message(
                &mut wire,
                command,
                &handle.local_offers
            ));
        }
        assert!(!wire
            .windows(secret.len())
            .any(|window| window == secret.as_slice()));
        let mut wire = io::Cursor::new(wire);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::FileContents {
                stream_id: 7,
                data: None,
                ..
            })
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::LocalFileOfferRejected)
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Input(InputEvent::Key { .. }))
        ));
        assert!(read_msg::<_, ClientMsg>(&mut wire).unwrap().is_none());
        assert!(!handle.local_offers.is_closed());
    }

    #[test]
    fn selection_marker_and_empty_revoke_survive_suspension_without_closing() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        assert!(handle.local_offers.suspend());
        handle.notify_local_file_selection();
        handle.send_clipboard_files(Vec::new(), None);
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        let mut wire = Vec::new();
        while let Ok(command) = commands.try_recv() {
            assert!(write_queued_client_message(
                &mut wire,
                command,
                &handle.local_offers
            ));
        }
        let mut wire = io::Cursor::new(wire);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::LocalFileOfferRejected)
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty()
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Input(InputEvent::Key { .. }))
        ));
        assert!(read_msg::<_, ClientMsg>(&mut wire).unwrap().is_none());
        assert!(handle.local_offers.is_suspended());
        assert!(!handle.local_offers.is_closed());
        assert!(!handle.local_file_revocation_unconfirmed());

        let (closed, closed_commands) = RdpClientHandle::test_fixture_with_commands();
        closed.local_offers.close();
        closed.notify_local_file_selection();
        assert!(closed_commands.try_recv().is_err());

        let (missing_writer, commands) = RdpClientHandle::test_fixture_with_commands();
        drop(commands);
        missing_writer.notify_local_file_selection();
        assert!(!missing_writer.local_offers.is_closed());
    }

    #[test]
    fn channel_change_retires_old_leases_without_misrouting_or_freeing_queued_bytes() {
        let budget = Arc::new(InboundReadBudget::new(2, 16));
        let (handle, commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
        handle.test_local_offer();
        let generation = handle.test_local_generation();
        let request = |epoch, stream_id| ClipboardFileRequestId {
            channel_epoch: epoch,
            snapshot_revision: 1,
            generation,
            stream_id,
        };
        let send = |id| {
            handle.test_send_event(RdpClientEvent::FileContentsNeededBound {
                request: id,
                index: 0,
                offset: 0,
                length: 1,
            })
        };
        let old_pending = request(7, 41);
        let old_queued = request(7, 42);
        send(old_pending);
        send(old_queued);
        let pending_lease = handle.local_file_read_lease_for_request(old_pending);
        let queued_lease = handle.local_file_read_lease_for_request(old_queued);
        let guard = pending_lease.current_guard();
        assert!(guard());
        queued_lease.answer(42, Some(vec![3]));
        assert_eq!(budget.test_counts(), (2, 0));
        handle.test_send_event(RdpClientEvent::ClipboardChannelChanged { channel_epoch: 8 });
        assert!(!guard());
        assert!(!pending_lease.is_current());
        assert_eq!(budget.test_counts(), (1, 0));
        pending_lease.answer(41, Some(vec![9]));
        handle.test_discard_suspended_command(commands.try_recv().unwrap());
        assert!(commands.try_recv().is_err());
        assert_eq!(budget.test_counts(), (0, 0));
        let fresh = request(9, 41);
        send(fresh);
        let lease = handle.local_file_read_lease_for_request(fresh);
        assert!(lease.is_current());
        lease.answer(41, Some(vec![8]));
        assert!(
            matches!(handle.test_write_command(commands.try_recv().unwrap()),
            ClientMsg::BoundFileContents { request, data: Some(data) } if request == fresh && data == [8])
        );
        assert_eq!(budget.test_counts(), (0, 0));
    }

    #[test]
    fn inbound_budget_tracks_blocked_writer_and_recovers_after_actual_writes() {
        let budget = Arc::new(InboundReadBudget::new(1, 2));
        let (handle, commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
        handle.test_local_offer();
        let generation = handle.test_local_generation();
        let event = |stream_id| RdpClientEvent::FileContentsNeeded {
            generation,
            stream_id,
            index: 0,
            offset: 0,
            length: 1,
        };
        handle.test_send_event(event(1));
        assert!(matches!(
            handle.try_recv(),
            Some(RdpClientEvent::FileContentsNeeded { stream_id: 1, .. })
        ));
        handle
            .local_file_read_lease(generation)
            .answer(1, Some(vec![42]));

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let mut writer = PauseAtFirstFlush {
            bytes: Arc::clone(&bytes),
            ready: Some(ready_tx),
            release: release_rx,
        };
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || write_client_commands(&mut writer, commands, &offers));
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.test_send_event(event(2));
        handle.test_send_event(event(3));
        assert_eq!(budget.test_counts(), (1, 2));

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while budget.test_counts() != (0, 0) {
            assert!(
                Instant::now() < deadline,
                "writer did not release the budgets"
            );
            thread::sleep(Duration::from_millis(5));
        }
        handle.close();
        worker.join().unwrap();
        let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(), Some(ClientMsg::FileContents { stream_id: 1, data: Some(data), .. }) if data == [42])
        );
        for stream_id in [2, 3] {
            assert!(
                matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(), Some(ClientMsg::FileContents { stream_id: got, data: None, .. }) if got == stream_id)
            );
        }
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
    }

    #[test]
    fn concurrent_offer_and_close_cannot_leak_refusal_reservations() {
        for _ in 0..64 {
            let budget = Arc::new(InboundReadBudget::new(1, 16));
            let offers = Arc::new(LocalFileOffers::with_budget(budget.clone()));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let publishing = {
                let offers = offers.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    offers.publish(RdpClipboardSequence::new(1, || 1))
                })
            };
            barrier.wait();
            offers.close();
            publishing.join().unwrap();
            drop(offers);
            assert_eq!(budget.counts.lock().unwrap().reservations, 0);
        }
    }

    #[test]
    fn queued_file_reply_checks_captured_sequence_only_on_writer_thread() {
        use std::sync::atomic::AtomicU32;

        for (observed, expected_data) in [(41, Some(vec![7, 8, 9])), (42, None), (0, None)] {
            let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
            let current_sequence = Arc::new(AtomicU32::new(41));
            let sequence_for_writer = Arc::clone(&current_sequence);
            let (calls_tx, calls_rx) = std_mpsc::channel();
            handle.send_clipboard_files(
                vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 3,
                    is_dir: false,
                }],
                Some(RdpClipboardSequence::new(41, move || {
                    calls_tx.send(thread::current().id()).unwrap();
                    sequence_for_writer.load(Ordering::Acquire)
                })),
            );
            let generation = handle.test_local_generation();
            let mut lease = handle.local_file_read_lease(generation);
            lease.tracked = false;

            let bytes = Arc::new(Mutex::new(Vec::new()));
            let (ready_tx, ready_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let writer = PauseAtFirstFlush {
                bytes: Arc::clone(&bytes),
                ready: Some(ready_tx),
                release: release_rx,
            };
            assert!(calls_rx.try_recv().is_err());
            let offers = Arc::clone(&handle.local_offers);
            let worker = thread::spawn(move || {
                let mut writer = writer;
                write_client_commands(&mut writer, commands, &offers);
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let source_thread = calls_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_ne!(source_thread, thread::current().id());

            // The finished read is queued while the writer is held at a barrier.
            lease.answer(11, Some(vec![7, 8, 9]));
            assert!(lease.is_current());
            assert!(calls_rx.try_recv().is_err());
            current_sequence.store(observed, Ordering::Release);
            handle.cmd_tx.send(ClientMsg::Close).unwrap();
            release_tx.send(()).unwrap();
            assert_eq!(
                calls_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                source_thread
            );
            worker.join().unwrap();

            let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::ClipboardFiles { generation: actual, files })
                    if actual == generation && files[0].name == "synthetic.bin"
            ));
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::FileContents {
                    generation: actual, stream_id: 11, data
                }) if actual == generation && data == expected_data
            ));
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::Close)
            ));
            assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
        }
    }

    #[test]
    fn queued_file_replies_use_their_own_session_sequence_binding() {
        use std::sync::atomic::AtomicU32;

        let (first, first_commands) = RdpClientHandle::test_fixture_with_commands();
        let (second, second_commands) = RdpClientHandle::test_fixture_with_commands();
        let first_sequence = Arc::new(AtomicU32::new(101));
        let second_sequence = Arc::new(AtomicU32::new(202));
        let first_source = Arc::clone(&first_sequence);
        let second_source = Arc::clone(&second_sequence);
        let file = ClipFile {
            name: "synthetic.bin".into(),
            size: 1,
            is_dir: false,
        };
        first.send_clipboard_files(
            vec![file.clone()],
            Some(RdpClipboardSequence::new(101, move || {
                first_source.load(Ordering::Acquire)
            })),
        );
        second.send_clipboard_files(
            vec![file],
            Some(RdpClipboardSequence::new(202, move || {
                second_source.load(Ordering::Acquire)
            })),
        );
        let first_generation = first.test_local_generation();
        let second_generation = second.test_local_generation();
        assert_eq!(first_generation, second_generation);

        let first_bytes = Arc::new(Mutex::new(Vec::new()));
        let second_bytes = Arc::new(Mutex::new(Vec::new()));
        let (first_ready_tx, first_ready_rx) = std_mpsc::channel();
        let (first_release_tx, first_release_rx) = std_mpsc::channel();
        let (second_ready_tx, second_ready_rx) = std_mpsc::channel();
        let (second_release_tx, second_release_rx) = std_mpsc::channel();
        let first_writer = PauseAtFirstFlush {
            bytes: Arc::clone(&first_bytes),
            ready: Some(first_ready_tx),
            release: first_release_rx,
        };
        let second_writer = PauseAtFirstFlush {
            bytes: Arc::clone(&second_bytes),
            ready: Some(second_ready_tx),
            release: second_release_rx,
        };
        let first_offers = Arc::clone(&first.local_offers);
        let second_offers = Arc::clone(&second.local_offers);
        let first_worker = thread::spawn(move || {
            let mut writer = first_writer;
            write_client_commands(&mut writer, first_commands, &first_offers);
        });
        let second_worker = thread::spawn(move || {
            let mut writer = second_writer;
            write_client_commands(&mut writer, second_commands, &second_offers);
        });
        first_ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        second_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let mut first_lease = first.local_file_read_lease(first_generation);
        let mut second_lease = second.local_file_read_lease(second_generation);
        first_lease.tracked = false;
        second_lease.tracked = false;
        first_lease.answer(9, Some(vec![10]));
        second_lease.answer(9, Some(vec![20]));
        first_sequence.store(303, Ordering::Release);
        first.cmd_tx.send(ClientMsg::Close).unwrap();
        second.cmd_tx.send(ClientMsg::Close).unwrap();
        first_release_tx.send(()).unwrap();
        second_release_tx.send(()).unwrap();
        first_worker.join().unwrap();
        second_worker.join().unwrap();

        for (bytes, generation, expected_data) in [
            (first_bytes, first_generation, None),
            (second_bytes, second_generation, Some(vec![20])),
        ] {
            let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::ClipboardFiles { generation: actual, files })
                    if actual == generation && files[0].name == "synthetic.bin"
            ));
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::FileContents {
                    generation: actual, stream_id: 9, data
                }) if actual == generation && data == expected_data
            ));
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::Close)
            ));
            assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
        }
    }

    #[test]
    fn sequence_query_does_not_block_publication_and_rechecks_generation() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let generation = handle
            .local_offers
            .publish(RdpClipboardSequence::new(101, move || {
                ready_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                101
            }))
            .unwrap();
        let mut lease = handle.local_file_read_lease(generation);
        lease.tracked = false;
        lease.answer(4, Some(vec![7]));
        let offers = Arc::clone(&handle.local_offers);
        let writer = thread::spawn(move || {
            let mut bytes = Vec::new();
            write_client_commands(&mut bytes, commands, &offers);
            bytes
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let offers = Arc::clone(&handle.local_offers);
        let (published_tx, published_rx) = std_mpsc::channel();
        let publisher = thread::spawn(move || {
            let fresh = offers.publish(RdpClipboardSequence::new(202, || 202));
            published_tx.send(fresh).unwrap();
        });
        let published_while_query_waited = published_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        publisher.join().unwrap();
        let bytes = writer.join().unwrap();

        let fresh = published_while_query_waited
            .expect("the sequence source must not hold the publication lock")
            .expect("the helper is open");
        assert_ne!(fresh, generation);
        let mut pipe = io::Cursor::new(bytes);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::FileContents {
                generation: actual, stream_id: 4, data: None
            }) if actual == generation
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
        assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
    }

    #[test]
    fn missing_or_zero_clipboard_sequence_revokes_instead_of_publishing() {
        for sequence in [
            None,
            Some(RdpClipboardSequence::new(0, || {
                panic!("zero must not be queried")
            })),
        ] {
            let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
            let old = handle.test_local_offer();
            handle.send_clipboard_files(
                vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
                sequence,
            );
            assert!(!old.is_current());
            assert!(!handle.has_local_file_offer());
            assert!(matches!(
                commands.try_recv(),
                Ok(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty()
            ));
        }
    }

    #[test]
    fn stale_queued_file_offers_are_not_advertised_after_revocation_or_replacement() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 1,
            is_dir: false,
        };
        handle.send_clipboard_files(
            vec![file("old.bin")],
            Some(RdpClipboardSequence::new(1, || 1)),
        );
        handle.revoke_local_files();
        assert!(handle.local_file_revocation_unconfirmed());
        assert!(
            matches!(handle.try_recv(), Some(RdpClientEvent::Notice(message))
            if message.contains("远端撤销尚未确认"))
        );
        handle.send_clipboard_files(
            vec![file("new.bin")],
            Some(RdpClipboardSequence::new(1, || 1)),
        );
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        let mut pipe = Vec::new();
        write_client_commands(&mut pipe, commands, &handle.local_offers);
        let mut pipe = io::Cursor::new(pipe);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty()));
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardFiles { generation, files }) if generation != 0 && files[0].name == "new.bin"));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
        assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
    }

    #[test]
    fn completed_frame_flush_can_finish_after_revocation() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let lease = handle.test_local_offer();
        lease.answer(5, Some(vec![4, 5, 6]));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let writer = PauseAtFirstFlush {
            bytes: Arc::clone(&bytes),
            ready: Some(ready_tx),
            release: release_rx,
        };
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || {
            let mut writer = writer;
            write_client_commands(&mut writer, commands, &offers);
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.revoke_local_files();
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::FileContents { stream_id: 5, data: Some(data), .. }) if data == [4, 5, 6]));
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty()));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
    }

    #[test]
    fn remote_offer_read_started_before_disable_cannot_restore_after_reenable() {
        let epoch = FileEpoch::default();
        let old_read = epoch.revision();
        epoch.set_enabled(false);
        epoch.set_enabled(true);
        assert_eq!(epoch.observe_offer_at(7, true, old_read), None);
        assert!(!epoch.accepts(7));
        assert_eq!(
            epoch
                .observe_offer_at(8, true, epoch.revision())
                .map(|result| result.0),
            Some(true)
        );
        assert!(epoch.accepts(8));
    }

    #[test]
    fn idle_read_crossing_a_toggle_cannot_identify_a_fresh_offer_without_a_barrier() {
        // An old buffered offer and a fresh offer arriving after re-enable have
        // the same observable read revision. Accepting either would accept both.
        for _origin in ["buffered before disable", "arrived after re-enable"] {
            let epoch = FileEpoch::default();
            let blocked_read = epoch.revision();
            epoch.set_enabled(false);
            epoch.set_enabled(true);
            assert_eq!(epoch.observe_offer_at(7, true, blocked_read), None);
            assert_eq!(epoch.current_generation(), 0);
            assert!(epoch.observe_offer_at(8, true, epoch.revision()).unwrap().0);
        }
    }

    #[test]
    fn queued_remote_request_is_rechecked_before_the_first_pipe_write() {
        for mode in ["unchanged", "disabled", "reopened_same_generation"] {
            let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
            let requester = handle.test_remote_file_offer(7);
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let (ready_tx, ready_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let writer = PauseAtFirstFlush {
                bytes: Arc::clone(&bytes),
                ready: Some(ready_tx),
                release: release_rx,
            };
            handle.send_clipboard_image(vec![1]);
            let offers = Arc::clone(&handle.local_offers);
            let remote = Arc::clone(&handle.remote_offer);
            let events = handle.event_tx.clone();
            let worker = thread::spawn(move || {
                let mut writer = writer;
                write_client_commands_with_remote(&mut writer, commands, &offers, &remote, &events);
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            requester.request(11, 2, 3, 4);
            assert!(requester.is_current());
            if mode != "unchanged" {
                handle.set_file_offers_enabled(false);
                if mode == "reopened_same_generation" {
                    handle.set_file_offers_enabled(true);
                    let fresh = handle.test_remote_file_offer(7);
                    assert!(fresh.is_current());
                }
                assert!(!requester.is_current());
            }
            handle.cmd_tx.send(ClientMsg::Close).unwrap();
            release_tx.send(()).unwrap();
            worker.join().unwrap();

            let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::ClipboardImageForRevision { image: data, .. }) if data == [1]
            ));
            if mode == "unchanged" {
                assert!(matches!(
                    read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                    Some(ClientMsg::RequestFileContents {
                        generation: 7,
                        stream_id: 11,
                        index: 2,
                        offset: 3,
                        length: 4
                    })
                ));
                assert!(handle.try_recv().is_none());
            } else {
                assert!(matches!(
                    handle.try_recv(),
                    Some(RdpClientEvent::FileContentsForEpoch {
                        generation: 7,
                        stream_id: 11,
                        data: None,
                        ..
                    })
                ));
                assert!(handle.try_recv().is_none());
            }
            if mode != "unchanged" {
                let expected = if mode == "disabled" { 1 } else { 2 };
                assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                    Some(ClientMsg::ClipboardPolicyEpoch(epoch)) if epoch == expected));
            }
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::Close)
            ));
            assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
        }
    }

    #[test]
    fn remote_response_keeps_the_request_epoch_after_the_frame_was_sent() {
        for disabled in [false, true] {
            let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
            let requester = handle.test_remote_file_offer(7);
            requester.request(11, 0, 0, 1);
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let (ready_tx, ready_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let writer = PauseAtFirstFlush {
                bytes: Arc::clone(&bytes),
                ready: Some(ready_tx),
                release: release_rx,
            };
            let offers = Arc::clone(&handle.local_offers);
            let remote = Arc::clone(&handle.remote_offer);
            let events = handle.event_tx.clone();
            let worker = thread::spawn(move || {
                let mut writer = writer;
                write_client_commands_with_remote(&mut writer, commands, &offers, &remote, &events);
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if disabled {
                handle.set_file_offers_enabled(false);
                handle.set_file_offers_enabled(true);
                assert!(handle.test_remote_file_offer(7).is_current());
                assert!(!requester.is_current());
            }
            dispatch_remote_file_contents(
                &handle.remote_offer,
                &handle.event_tx,
                7,
                11,
                Some(vec![42]),
            );
            assert!(matches!(
                handle.try_recv(),
                Some(RdpClientEvent::FileContentsForEpoch {
                    generation: 7, stream_id: 11, data, ..
                }) if data == if disabled { None } else { Some(vec![42]) }
            ));
            dispatch_remote_file_contents(
                &handle.remote_offer,
                &handle.event_tx,
                7,
                11,
                Some(vec![99]),
            );
            assert!(handle.try_recv().is_none());
            handle.cmd_tx.send(ClientMsg::Close).unwrap();
            release_tx.send(()).unwrap();
            worker.join().unwrap();

            let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::RequestFileContents {
                    generation: 7,
                    stream_id: 11,
                    ..
                })
            ));
            if disabled {
                assert!(matches!(
                    read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                    Some(ClientMsg::ClipboardPolicyEpoch(2))
                ));
            }
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::Close)
            ));
        }
    }

    #[test]
    fn stale_offers_do_not_revive_when_a_generation_is_reused() {
        let (handle, _commands) = RdpClientHandle::test_fixture_with_commands();
        handle.test_remote_file_offer(7);
        let revision = handle.remote_offer.revision();
        for files in [
            vec![ClipFile {
                name: "old.bin".into(),
                size: 1,
                is_dir: false,
            }],
            Vec::new(),
        ] {
            handle.test_send_event(RdpClientEvent::ClipboardFilesForEpoch {
                generation: 7,
                revision,
                files,
            });
        }
        handle.set_file_offers_enabled(false);
        handle.set_file_offers_enabled(true);
        assert!(handle.test_remote_file_offer(7).is_current());
        handle.test_send_event(RdpClientEvent::Closed);
        assert!(matches!(handle.try_recv(), Some(RdpClientEvent::Closed)));
    }

    #[test]
    fn queued_remote_offer_is_not_delivered_after_disable_and_reenable() {
        let (handle, _commands) = RdpClientHandle::test_fixture_with_commands();
        handle.remote_offer.observe_offer(7, true);
        handle.test_send_event(RdpClientEvent::ClipboardFiles {
            generation: 7,
            files: vec![ClipFile {
                name: "old.bin".into(),
                size: 1,
                is_dir: false,
            }],
        });
        handle.set_file_offers_enabled(false);
        handle.set_file_offers_enabled(true);
        assert!(handle.try_recv().is_none());
    }

    #[test]
    fn queued_file_bytes_are_refused_after_revocation_even_if_a_new_offer_exists() {
        for reopen in [false, true] {
            let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
            let old = handle.test_local_offer();
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let (ready_tx, ready_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let writer = PauseAtFirstFlush {
                bytes: Arc::clone(&bytes),
                ready: Some(ready_tx),
                release: release_rx,
            };
            let offers = Arc::clone(&handle.local_offers);
            handle.send_clipboard_image(vec![1]);
            let worker = thread::spawn(move || {
                let mut writer = writer;
                write_client_commands(&mut writer, commands, &offers);
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

            // The lease was valid when the data entered the writer queue.
            old.answer(11, Some(vec![7, 8, 9]));
            handle.revoke_local_files();
            let new_generation = if reopen {
                let fresh = handle.test_local_offer();
                let generation = fresh.generation;
                fresh.answer(12, Some(vec![4, 5]));
                Some(generation)
            } else {
                None
            };
            handle.cmd_tx.send(ClientMsg::Close).unwrap();
            release_tx.send(()).unwrap();
            worker.join().unwrap();

            let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::ClipboardImageForRevision { image: data, .. }) if data == [1]
            ));
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::FileContents { generation, stream_id: 11, data: None })
                    if generation == old.generation
            ));
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty()
            ));
            if let Some(generation) = new_generation {
                assert!(matches!(
                    read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                    Some(ClientMsg::FileContents { generation: got, stream_id: 12, data: Some(data) })
                        if got == generation && data == [4, 5]
                ));
            }
            assert!(matches!(
                read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
                Some(ClientMsg::Close)
            ));
            assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
        }
    }

    #[test]
    fn close_before_writer_handoff_discards_queue_and_cannot_reopen() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let old = handle.test_local_offer();
        old.answer(7, Some(vec![1, 2, 3]));
        handle.send_clipboard_image(vec![4, 5, 6]);
        let mut hello = Vec::new();
        adit_rdp_proto::write_helper_hello(&mut hello, IPC_PROTOCOL_VERSION).unwrap();
        let request = ConnectRequest {
            host: "adit.invalid".into(),
            port: 3389,
            username: "synthetic-user".into(),
            password: "synthetic-password".into(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: true,
            enable_audio: false,
            quality: Default::default(),
            clipboard_policy_id: None,
        };
        let (mut pipe, _) = open_control_channel(
            Vec::new(),
            io::Cursor::new(hello),
            request,
            Duration::from_secs(2),
            Arc::clone(&handle.remote_offer),
        )
        .unwrap();
        // The handshake succeeded, but the queue has not been handed to its
        // writer yet. Close must take precedence at that handoff.
        handle.close();
        handle.local_offers.revoke();
        assert!(handle
            .local_offers
            .publish(RdpClipboardSequence::new(1, || 1))
            .is_none());
        assert!(!old.is_current());

        // Keep the sender and old lease alive: Close, not channel disconnection,
        // must stop the writer after a successful handshake hands over the pipe.
        write_client_commands(&mut pipe, commands, &handle.local_offers);
        let mut pipe = io::Cursor::new(pipe);
        assert_eq!(
            adit_rdp_proto::read_client_hello(&mut pipe).unwrap(),
            IPC_PROTOCOL_VERSION
        );
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Connect(request)) if request.host == "adit.invalid"
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
        assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
    }

    struct PartialFrameWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        writes: usize,
        ready: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
        dropped: std_mpsc::Sender<()>,
    }

    impl Write for PartialFrameWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            let count = if self.writes == 2 {
                self.ready.take().unwrap().send(()).unwrap();
                self.release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
                bytes.len().min(3)
            } else {
                bytes.len()
            };
            self.bytes
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for PartialFrameWriter {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    #[test]
    fn a_started_desktop_frame_finishes_before_the_revocation_marker() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        handle.send_clipboard_text_with_sequence(
            "started".into(),
            RdpClipboardSequence::new(1, || 1),
        );
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let writer = PartialFrameWriter {
            bytes: Arc::clone(&bytes),
            writes: 0,
            ready: Some(ready_tx),
            release: release_rx,
            dropped: dropped_tx,
        };
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || {
            let mut writer = writer;
            write_client_commands(&mut writer, commands, &offers);
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.observe_local_clipboard_sequence(2);
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        let mut wire = io::Cursor::new(bytes.lock().unwrap().clone());
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut wire).unwrap(), Some(ClientMsg::ClipboardTextForRevision { text, .. }) if text == "started")
        );
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::LocalFileOfferRejected)
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Input(InputEvent::Key { .. }))
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Close)
        ));
    }

    #[test]
    fn revoking_a_partially_written_frame_finishes_it_and_keeps_the_pipe_framed() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let lease = handle.test_local_offer();
        lease.answer(31, Some(vec![99; 256]));
        handle.send_clipboard_image(vec![42]);
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let writer = PartialFrameWriter {
            bytes: Arc::clone(&bytes),
            writes: 0,
            ready: Some(ready_tx),
            release: release_rx,
            dropped: dropped_tx,
        };
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || {
            let mut writer = writer;
            write_client_commands(&mut writer, commands, &offers);
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.revoke_local_files();
        assert!(handle.local_file_revocation_unconfirmed());
        handle.cmd_tx.send(ClientMsg::Close).unwrap();
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();

        let written = bytes.lock().unwrap().clone();
        let mut pipe = io::Cursor::new(written);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::FileContents { data: Some(data), .. }) if data == vec![99; 256]));
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardImageForRevision { image: data, .. }) if data == [42]));
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty()));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
        assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
        assert!(handle.local_offers.is_closed());
    }

    #[test]
    fn close_during_a_partial_message_drops_queued_file_bytes() {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let lease = handle.test_local_offer();
        handle.send_clipboard_image(vec![42; 256]);
        lease.answer(31, Some(vec![99; 256]));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let writer = PartialFrameWriter {
            bytes: Arc::clone(&bytes),
            writes: 0,
            ready: Some(ready_tx),
            release: release_rx,
            dropped: dropped_tx,
        };
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || {
            let mut writer = writer;
            write_client_commands(&mut writer, commands, &offers);
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.close();
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();

        let mut pipe = io::Cursor::new(bytes.lock().unwrap().clone());
        assert!(matches!(read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardImageForRevision { image: data, .. }) if data == vec![42; 256]));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::Close)
        ));
        assert!(read_msg::<_, ClientMsg>(&mut pipe).unwrap().is_none());
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn close_reaps_a_stalled_direct_child_and_releases_the_real_pipe_writer() {
        struct ChildCleanup(Arc<Mutex<Option<Child>>>);
        impl Drop for ChildCleanup {
            fn drop(&mut self) {
                if let Ok(mut slot) = self.0.lock() {
                    if let Some(child) = slot.as_mut() {
                        if matches!(child.try_wait(), Ok(None)) {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }
        }
        struct TrackedStdin {
            inner: std::process::ChildStdin,
            dropped: std_mpsc::Sender<()>,
        }
        impl Write for TrackedStdin {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.inner.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.inner.flush()
            }
        }
        impl Drop for TrackedStdin {
            fn drop(&mut self) {
                let _ = self.dropped.send(());
            }
        }

        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        let mut command = Command::new(executable);
        command
            .env("ADIT_RDP_IPC_TEST_MODE", "blocked_connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_window(&mut command);
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION).unwrap();
        read_helper_hello(&mut stdout).unwrap();
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let slot = Arc::clone(&handle.child);
        let _cleanup = ChildCleanup(Arc::clone(&slot));
        *slot.lock().unwrap() = Some(child);
        let lease = handle.test_local_offer();
        lease.answer(7, Some(vec![99; 8 * 1024 * 1024]));
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let offers = Arc::clone(&handle.local_offers);
        let worker = thread::spawn(move || {
            let mut stdin = TrackedStdin {
                inner: stdin,
                dropped: dropped_tx,
            };
            write_client_commands(&mut stdin, commands, &offers);
        });
        let (ready_tx, ready_rx) = std_mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let result = io::BufReader::new(stderr).read_line(&mut line);
            let _ = ready_tx.send(result.map(|_| line));
        });
        assert_eq!(
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .trim(),
            "connect-started"
        );
        assert!(matches!(
            dropped_rx.recv_timeout(Duration::from_millis(150)),
            Err(std_mpsc::RecvTimeoutError::Timeout)
        ));
        handle.close();
        dropped_rx
            .recv_timeout(Duration::from_secs(13))
            .expect("production close did not release the blocked pipe writer");
        worker.join().unwrap();
        assert!(slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some());
    }
}

#[cfg(test)]
mod ipc_handshake_tests {
    use super::*;
    use std::io::{BufRead, Read, Write};

    #[derive(Clone, Default)]
    struct RecordedWrites(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordedWrites {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct DelayedAck {
        bytes: std::io::Cursor<Vec<u8>>,
        ready: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
        dropped: std_mpsc::Sender<()>,
    }

    impl Read for DelayedAck {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(());
                self.release.recv().unwrap();
            }
            self.bytes.read(buffer)
        }
    }

    impl Drop for DelayedAck {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    struct StalledConnectWriter {
        written: usize,
        observed: RecordedWrites,
        ready: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
        dropped: std_mpsc::Sender<()>,
    }

    impl Write for StalledConnectWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let hello_len = 4 + 4 + adit_rdp_proto::IPC_HELLO_MAGIC.len() + 4;
            if self.written >= hello_len {
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(());
                    self.release.recv().unwrap();
                }
            }
            self.written += bytes.len();
            self.observed.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for StalledConnectWriter {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    struct TrackedPipe {
        inner: std::process::ChildStdin,
        dropped: std_mpsc::Sender<()>,
    }

    impl Write for TrackedPipe {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.inner.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    impl Drop for TrackedPipe {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    fn synthetic_request() -> ConnectRequest {
        ConnectRequest {
            host: "adit.invalid".into(),
            port: 3389,
            username: "synthetic-user".into(),
            password: "synthetic-password".into(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: true,
            enable_audio: false,
            quality: Default::default(),
            clipboard_policy_id: None,
        }
    }

    #[test]
    fn incompatible_or_missing_ack_never_writes_connect() {
        let mut wrong_version = Vec::new();
        adit_rdp_proto::write_helper_hello(&mut wrong_version, IPC_PROTOCOL_VERSION + 1).unwrap();
        let mut previous_version = Vec::new();
        adit_rdp_proto::write_helper_hello(&mut previous_version, 5).unwrap();
        for response in [Vec::new(), wrong_version, previous_version] {
            let recorded = RecordedWrites::default();
            let result = open_control_channel(
                recorded.clone(),
                std::io::Cursor::new(response),
                synthetic_request(),
                Duration::from_secs(1),
                Arc::new(FileEpoch::default()),
            );
            assert!(matches!(result, Err(RdpClientError::Protocol(_))));
            let sent = recorded.0.lock().unwrap();
            let mut wire = std::io::Cursor::new(sent.as_slice());
            assert_eq!(
                adit_rdp_proto::read_client_hello(&mut wire).unwrap(),
                IPC_PROTOCOL_VERSION
            );
            assert_eq!(
                wire.position() as usize,
                sent.len(),
                "only the hello was written"
            );
            assert!(!sent
                .windows(b"synthetic-password".len())
                .any(|w| w == b"synthetic-password"));
        }
    }

    #[test]
    fn matching_ack_writes_synthetic_connect_after_hello() {
        let mut response = Vec::new();
        adit_rdp_proto::write_helper_hello(&mut response, IPC_PROTOCOL_VERSION).unwrap();
        let (sent, _) = open_control_channel(
            Vec::new(),
            std::io::Cursor::new(response),
            synthetic_request(),
            Duration::from_secs(1),
            Arc::new(FileEpoch::default()),
        )
        .unwrap();
        let mut wire = std::io::Cursor::new(sent);
        assert_eq!(
            adit_rdp_proto::read_client_hello(&mut wire).unwrap(),
            IPC_PROTOCOL_VERSION
        );
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut wire).unwrap(),
            Some(ClientMsg::Connect(request))
                if request.host == "adit.invalid" && request.password == "synthetic-password"
        ));
    }

    #[test]
    fn closed_before_handshake_writes_nothing() {
        let recorded = RecordedWrites::default();
        let remote_offer = Arc::new(FileEpoch::default());
        remote_offer.closed.store(true, Ordering::Release);
        let result = open_control_channel(
            recorded.clone(),
            std::io::Cursor::new(Vec::<u8>::new()),
            synthetic_request(),
            Duration::from_secs(1),
            remote_offer,
        );
        assert!(matches!(result, Err(RdpClientError::Cancelled)));
        assert!(recorded.0.lock().unwrap().is_empty());
    }

    #[test]
    fn late_ack_after_close_cannot_start_connect() {
        let mut response = Vec::new();
        adit_rdp_proto::write_helper_hello(&mut response, IPC_PROTOCOL_VERSION).unwrap();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let (result_tx, result_rx) = std_mpsc::channel();
        let recorded = RecordedWrites::default();
        let writer = recorded.clone();
        let (handle, _commands) = RdpClientHandle::test_fixture_with_commands();
        let remote_offer = Arc::clone(&handle.remote_offer);
        thread::spawn(move || {
            let reader = DelayedAck {
                bytes: std::io::Cursor::new(response),
                ready: Some(ready_tx),
                release: release_rx,
                dropped: dropped_tx,
            };
            let result = open_control_channel(
                writer,
                reader,
                synthetic_request(),
                Duration::from_secs(3),
                remote_offer,
            );
            let _ = result_tx.send(result.map(|_| ()));
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.close();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(RdpClientError::Cancelled)
        ));
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let sent = recorded.0.lock().unwrap();
        let mut wire = std::io::Cursor::new(sent.as_slice());
        assert_eq!(
            adit_rdp_proto::read_client_hello(&mut wire).unwrap(),
            IPC_PROTOCOL_VERSION
        );
        assert_eq!(wire.position() as usize, sent.len());
    }

    #[test]
    fn stalled_connect_write_observes_close_and_deadline() {
        for action in ["close", "timeout"] {
            let mut response = Vec::new();
            adit_rdp_proto::write_helper_hello(&mut response, IPC_PROTOCOL_VERSION).unwrap();
            let (ready_tx, ready_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let (dropped_tx, dropped_rx) = std_mpsc::channel();
            let (result_tx, result_rx) = std_mpsc::channel();
            let (handle, _commands) = RdpClientHandle::test_fixture_with_commands();
            let remote_offer = Arc::clone(&handle.remote_offer);
            let recorded = RecordedWrites::default();
            let observed = recorded.clone();
            thread::spawn(move || {
                let writer = StalledConnectWriter {
                    written: 0,
                    observed,
                    ready: Some(ready_tx),
                    release: release_rx,
                    dropped: dropped_tx,
                };
                let result = open_control_channel(
                    writer,
                    std::io::Cursor::new(response),
                    synthetic_request(),
                    Duration::from_secs(2),
                    remote_offer,
                );
                let _ = result_tx.send(result.map(|_| ()));
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            if action == "close" {
                handle.close();
            }
            let result = result_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            if action == "close" {
                assert!(matches!(result, Err(RdpClientError::Cancelled)));
            } else {
                assert!(matches!(result, Err(RdpClientError::Protocol(_))));
            }
            release_tx.send(()).unwrap();
            dropped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let sent = recorded.0.lock().unwrap();
            let mut wire = std::io::Cursor::new(sent.as_slice());
            adit_rdp_proto::read_client_hello(&mut wire).unwrap();
            assert_eq!(sent.len() - wire.position() as usize, 4);
            assert!(!sent
                .windows(b"synthetic-password".len())
                .any(|bytes| bytes == b"synthetic-password"));
        }
    }

    fn peer_line(child: &mut Child) -> String {
        let stderr = child.stderr.take().expect("synthetic peer stderr");
        let (tx, rx) = std_mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut line = String::new();
            let result = std::io::BufReader::new(stderr).read_line(&mut line);
            let _ = tx.send(result.map(|_| line));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("synthetic peer did not reach the expected stage")
            .expect("cannot read synthetic peer stage")
            .trim()
            .to_owned()
    }

    fn synthetic_peer(executable: &std::ffi::OsStr, mode: &str) -> Command {
        let mut command = Command::new(executable);
        command
            .env("ADIT_RDP_IPC_TEST_MODE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_window(&mut command);
        command
    }

    fn wait_for_peer(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "synthetic peer rejected the IPC sequence: {status}"
                );
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("synthetic peer did not exit");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn client_never_sends_credentials_or_queued_files_until_version_matches() {
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        for mode in [
            "legacy",
            "mismatch",
            "old_v3",
            "old_v4",
            "old_v5",
            "old_v6",
            "old_v7",
            "old_v8",
            "missing_ack",
            "silent",
            "matching",
            "notice",
        ] {
            let mut child = Command::new(&executable)
                .env("ADIT_RDP_IPC_TEST_MODE", mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let (cmd_tx, cmd_rx) = std_mpsc::channel();
            cmd_tx
                .send(ClientMsg::ClipboardFiles {
                    generation: 17,
                    files: vec![ClipFile {
                        name: "synthetic.bin".into(),
                        size: 1,
                        is_dir: false,
                    }],
                })
                .unwrap();
            let (event_tx, event_rx) = std_mpsc::channel();
            let input_tx = cmd_tx.clone();
            let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
            let offers = Arc::new(LocalFileOffers::default());
            offers.next.store(16, Ordering::Release);
            assert_eq!(offers.publish(RdpClipboardSequence::new(7, || 7)), Some(17));
            let result = wire_up(
                &mut child,
                synthetic_request(),
                cmd_rx,
                cmd_tx,
                event_tx,
                &surface,
                WireOptions {
                    remote_offer: Arc::new(FileEpoch::default()),
                    local_offers: offers,
                    handshake_timeout: Duration::from_secs(1),
                    cleanup_child: None,
                },
            );
            if mode == "matching" || mode == "notice" {
                result.expect("matching handshake opens the session");
                assert!(matches!(
                    event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                    RdpClientEvent::Connected {
                        width: 2,
                        height: 2
                    }
                ));
                if mode == "notice" {
                    assert!(
                        matches!(event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                        RdpClientEvent::ClipboardFileDirectionUnavailable { channel_epoch: 7, message } if message.contains("暂时禁用") && !message.contains("重新连接"))
                    );
                    input_tx
                        .send(ClientMsg::Input(InputEvent::Key {
                            scancode: 30,
                            extended: false,
                            pressed: true,
                        }))
                        .unwrap();
                }
                assert!(matches!(
                    event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                    RdpClientEvent::Closed
                ));
                if mode == "notice" {
                    assert_eq!(&surface.lock().unwrap().rgba[..4], &[9, 8, 7, 255]);
                }
            } else {
                match result {
                    Err(error @ RdpClientError::Protocol(_)) => {
                        let message = error.to_string();
                        assert!(message.contains("IPC") && message.contains("版本"));
                    }
                    _ => panic!("{mode} must fail as an IPC negotiation error"),
                }
            }
            wait_for_peer(&mut child);
        }
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn real_pipe_clipboard_flood_stays_bounded_for_two_sessions_and_keeps_desktop_live() {
        struct Peer(Child);
        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
            let mapping = handle.remote_offer.ensure_policy_mapping().unwrap();
            let mut command = synthetic_peer(&executable, "flood");
            mapping.configure_child(&mut command).unwrap();
            let mut child = Peer(command.spawn().unwrap());
            handle.local_offers.next.store(16, Ordering::Release);
            assert_eq!(
                handle
                    .local_offers
                    .publish(RdpClipboardSequence::new(7, || 7)),
                Some(17)
            );
            handle
                .cmd_tx
                .send(ClientMsg::ClipboardFiles {
                    generation: 17,
                    files: vec![ClipFile {
                        name: "synthetic.bin".into(),
                        size: 1,
                        is_dir: false,
                    }],
                })
                .unwrap();
            wire_up(
                &mut child.0,
                synthetic_request(),
                commands,
                handle.cmd_tx.clone(),
                handle.event_tx.clone(),
                &handle.surface,
                WireOptions {
                    remote_offer: Arc::clone(&handle.remote_offer),
                    local_offers: Arc::clone(&handle.local_offers),
                    handshake_timeout: Duration::from_secs(1),
                    cleanup_child: None,
                },
            )
            .unwrap();
            assert!(matches!(
                handle
                    .event_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap(),
                RdpClientEvent::Connected { .. }
            ));
            let stderr = child.0.stderr.take().unwrap();
            let (marker_tx, marker_rx) = std_mpsc::channel();
            thread::spawn(move || {
                let mut line = String::new();
                let _ = std::io::BufReader::new(stderr).read_line(&mut line);
                let _ = marker_tx.send(line);
            });
            peers.push((handle, child, marker_rx));
        }
        for (handle, child, marker) in &mut peers {
            assert_eq!(
                marker.recv_timeout(Duration::from_secs(15)).unwrap().trim(),
                "flood-flushed"
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if handle
                    .frame_if_newer(0)
                    .is_some_and(|frame| frame.rgba.starts_with(&[9, 8, 7, 255]))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "desktop redraw did not pass the flood"
                );
                thread::yield_now();
            }
            let slot = handle.remote_offer.clipboard_events.lock().unwrap();
            assert!(slot.wake_queued);
            assert!(
                matches!(slot.latest.as_ref(), Some(RdpClientEvent::ClipboardImageForRevision { stamp, image })
                if stamp.copy_revision == 128 && image.len() == 256 * 1024)
            );
            drop(slot);
            assert!(
                matches!(handle.try_recv(), Some(RdpClientEvent::ClipboardImageForRevision { stamp, .. }) if stamp.copy_revision == 128)
            );
            assert!(handle.try_recv().is_none());
            handle.send_input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            });
            wait_for_peer(&mut child.0);
        }
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn policy_epoch_preserves_the_first_new_file_over_a_blocked_real_pipe() {
        struct Peer(Child);
        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let mapping = handle.remote_offer.ensure_policy_mapping().unwrap();
        let mut command = synthetic_peer(&executable, "policy");
        mapping.configure_child(&mut command).unwrap();
        let mut child = Peer(command.spawn().unwrap());
        handle.local_offers.next.store(16, Ordering::Release);
        assert_eq!(
            handle
                .local_offers
                .publish(RdpClipboardSequence::new(7, || 7)),
            Some(17)
        );
        handle
            .cmd_tx
            .send(ClientMsg::ClipboardFiles {
                generation: 17,
                files: vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            })
            .unwrap();
        wire_up(
            &mut child.0,
            synthetic_request(),
            commands,
            handle.cmd_tx.clone(),
            handle.event_tx.clone(),
            &handle.surface,
            WireOptions {
                remote_offer: Arc::clone(&handle.remote_offer),
                local_offers: Arc::clone(&handle.local_offers),
                handshake_timeout: Duration::from_secs(1),
                cleanup_child: None,
            },
        )
        .unwrap();
        assert!(matches!(
            handle
                .event_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            RdpClientEvent::Connected { .. }
        ));
        let stderr = child.0.stderr.take().unwrap();
        let (marker_tx, marker_rx) = std_mpsc::channel();
        thread::spawn(move || {
            for marker in std::io::BufReader::new(stderr).lines() {
                if marker_tx.send(marker).is_err() {
                    break;
                }
            }
        });
        let marker = marker_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(marker.trim(), "old-file-frame-started");
        handle.set_file_offers_enabled(false);
        handle.set_file_offers_enabled(true);
        assert!(matches!(
            handle.clipboard_policy_status(),
            RdpClipboardPolicyStatus::Pending {
                epoch: 2,
                enabled: true
            }
        ));
        let marker = marker_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(marker.trim(), "new-file-before-confirmation");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let buffered = handle
                .remote_offer
                .state
                .lock()
                .unwrap()
                .pending_policy_offer
                .as_ref()
                .is_some_and(|offer| offer.generation == 8);
            if buffered {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new offer was not buffered before confirmation"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert!(handle.event_rx.try_recv().is_err());
        handle.remote_offer.state.lock().unwrap().policy_deadline = Some(Instant::now());
        assert_eq!(
            handle.clipboard_policy_status(),
            RdpClipboardPolicyStatus::Failed {
                epoch: 2,
                enabled: true,
                reason: RdpClipboardPolicyFailure::ConfirmationTimedOut,
            }
        );
        handle.send_input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let offer = loop {
            if let Some(event) = handle.try_recv() {
                break event;
            }
            assert!(Instant::now() < deadline, "new file offer timed out");
            thread::sleep(Duration::from_millis(2));
        };
        let RdpClientEvent::ClipboardFilesForEpoch {
            generation,
            revision,
            files,
        } = offer
        else {
            panic!("new file offer missing")
        };
        assert_eq!(generation, 8);
        assert_eq!(files[0].name, "new.bin");
        assert_eq!(
            handle.clipboard_policy_status(),
            RdpClipboardPolicyStatus::Confirmed {
                epoch: 2,
                enabled: true
            }
        );
        let requester = handle.file_requester_for_epoch(generation, revision);
        requester.try_request(91, 0, 0, 1).unwrap();
        assert!(
            matches!(handle.event_rx.recv_timeout(Duration::from_secs(5)).unwrap(), RdpClientEvent::FileContentsForEpoch { generation: 8, stream_id: 91, data: Some(bytes), .. } if bytes == [42])
        );
        wait_for_peer(&mut child.0);
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn channel_change_invalidates_old_lease_before_the_same_stream_is_reused() {
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        let mut child = synthetic_peer(&executable, "channel").spawn().unwrap();
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        cmd_tx
            .send(ClientMsg::ClipboardFiles {
                generation: 17,
                files: vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            })
            .unwrap();
        let (event_tx, event_rx) = std_mpsc::channel();
        let offers = Arc::new(LocalFileOffers::with_budget(Arc::new(
            InboundReadBudget::new(4, 16),
        )));
        offers.next.store(16, Ordering::Release);
        assert_eq!(offers.publish(RdpClipboardSequence::new(7, || 7)), Some(17));
        let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
        wire_up(
            &mut child,
            synthetic_request(),
            cmd_rx,
            cmd_tx.clone(),
            event_tx,
            &surface,
            WireOptions {
                remote_offer: Arc::new(FileEpoch::default()),
                local_offers: offers.clone(),
                handshake_timeout: Duration::from_secs(1),
                cleanup_child: None,
            },
        )
        .unwrap();
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::Connected { .. }
        ));
        let RdpClientEvent::FileContentsNeededBound { request: old, .. } =
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected the old channel's request");
        };
        let old_lease = LocalFileReadLease {
            current: offers.clone(),
            generation: 17,
            request: Some(old),
            replies: cmd_tx.clone(),
            tracked: true,
        };
        let guard = old_lease.current_guard();
        assert!(guard());
        cmd_tx
            .send(ClientMsg::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            }))
            .unwrap();
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::ClipboardChannelChanged { channel_epoch: 8 }
        ));
        assert!(!guard());
        old_lease.answer(41, Some(vec![9]));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::ClipboardLocalFileDirectionReady { channel_epoch: 9 }
        ));
        let RdpClientEvent::FileContentsNeededBound { request: fresh, .. } =
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected the new channel's request");
        };
        assert_eq!(fresh.channel_epoch, 9);
        let new_lease = LocalFileReadLease {
            current: offers,
            generation: 17,
            request: Some(fresh),
            replies: cmd_tx,
            tracked: true,
        };
        assert!(new_lease.is_current());
        new_lease.answer(41, Some(vec![8]));
        wait_for_peer(&mut child);
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn duplicate_inbound_request_is_acknowledged_without_closing_desktop() {
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        let mut child = synthetic_peer(&executable, "suspend").spawn().unwrap();
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        cmd_tx
            .send(ClientMsg::ClipboardFiles {
                generation: 17,
                files: vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            })
            .unwrap();
        let (event_tx, event_rx) = std_mpsc::channel();
        let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
        let offers = Arc::new(LocalFileOffers::default());
        offers.next.store(16, Ordering::Release);
        assert_eq!(offers.publish(RdpClipboardSequence::new(7, || 7)), Some(17));
        let remote = Arc::new(FileEpoch::default());
        wire_up(
            &mut child,
            synthetic_request(),
            cmd_rx,
            cmd_tx.clone(),
            event_tx.clone(),
            &surface,
            WireOptions {
                remote_offer: remote.clone(),
                local_offers: offers.clone(),
                handshake_timeout: Duration::from_secs(1),
                cleanup_child: None,
            },
        )
        .unwrap();
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::Connected { .. }
        ));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::FileContentsNeededBound { .. }
        ));
        assert!(
            matches!(event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::ClipboardFileDirectionUnavailable { message, .. } if message.contains("等待辅助进程确认"))
        );
        assert!(
            matches!(event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::ClipboardFileDirectionUnavailable { message, .. } if message.contains("暂时禁用") && message.contains("仍可使用") && !message.contains("重新连接"))
        );
        assert!(offers.is_suspended());
        assert!(offers.suspension_confirmed.load(Ordering::Acquire));
        let deadline = Instant::now() + Duration::from_secs(5);
        while surface.lock().unwrap().rgba[..4] != [9, 8, 7, 255] {
            assert!(
                Instant::now() < deadline,
                "frame was not processed after suspension"
            );
            thread::sleep(Duration::from_millis(5));
        }
        cmd_tx
            .send(ClientMsg::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            }))
            .unwrap();
        remote.observe_offer(77, true);
        let requester = RdpFileRequester {
            tx: Arc::new(Mutex::new(cmd_tx)),
            generation: 77,
            revision: remote.revision(),
            current: remote,
            replies: event_tx,
        };
        assert_eq!(requester.try_request(9, 0, 0, 1), Ok(()));
        assert!(
            matches!(event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            RdpClientEvent::FileContentsForEpoch { stream_id: 9, data: Some(data), .. } if data == [42])
        );
        wait_for_peer(&mut child);
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn close_drop_and_timeout_discard_a_late_ack_without_sending_credentials() {
        static NEXT_SIGNAL: AtomicU64 = AtomicU64::new(0);
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        for action in ["close", "drop", "timeout"] {
            let signal = std::env::temp_dir().join(format!(
                "adit-rdp-ipc-{}-{}-{}",
                std::process::id(),
                NEXT_SIGNAL.fetch_add(1, Ordering::Relaxed),
                Instant::now().elapsed().as_nanos(),
            ));
            let mut command = synthetic_peer(&executable, "late_ack");
            command.env("ADIT_RDP_IPC_TEST_SIGNAL", &signal);
            let mut child = command.spawn().unwrap();
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let (handle, _commands) = RdpClientHandle::test_fixture_with_commands();
            let remote_offer = Arc::clone(&handle.remote_offer);
            let (result_tx, result_rx) = std_mpsc::sync_channel(1);
            let timeout = if action == "timeout" {
                Duration::from_secs(1)
            } else {
                Duration::from_secs(5)
            };
            thread::spawn(move || {
                let result =
                    open_control_channel(stdin, stdout, synthetic_request(), timeout, remote_offer)
                        .map(|_| ());
                let _ = result_tx.send(result);
            });
            assert_eq!(peer_line(&mut child), "hello-ready");
            match action {
                "close" => handle.close(),
                "drop" => drop(handle),
                "timeout" => {}
                _ => unreachable!(),
            }
            let result = result_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("handshake did not stop promptly");
            if action == "timeout" {
                assert!(matches!(result, Err(RdpClientError::Protocol(_))));
            } else {
                assert!(matches!(result, Err(RdpClientError::Cancelled)));
            }
            // Release the ACK only after close/drop/timeout returned. The peer
            // checks the raw remaining stdin for even a partial Connect frame.
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&signal)
                .unwrap();
            wait_for_peer(&mut child);
            std::fs::remove_file(signal).unwrap();
        }
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn blocked_connect_write_is_cancelled_or_times_out_and_the_child_is_reaped() {
        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        for action in ["close", "drop", "timeout"] {
            let mut child = synthetic_peer(&executable, "blocked_connect")
                .spawn()
                .unwrap();
            let stderr = child.stderr.take().unwrap();
            let (handle, cmd_rx) = RdpClientHandle::test_fixture_with_commands();
            let remote_offer = Arc::clone(&handle.remote_offer);
            let local_offers = Arc::clone(&handle.local_offers);
            let surface = Arc::clone(&handle.surface);
            let refuse_tx = handle.cmd_tx.clone();
            let event_tx = handle.event_tx.clone();
            let mut request = synthetic_request();
            // Larger than a pipe buffer so the fixture can stop reading after
            // the length prefix and reliably block the Connect write.
            request.password = "synthetic-password".repeat(512 * 1024);
            let timeout = if action == "timeout" {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(5)
            };
            let (result_tx, result_rx) = std_mpsc::sync_channel(1);
            thread::spawn(move || {
                let result = wire_up(
                    &mut child,
                    request,
                    cmd_rx,
                    refuse_tx,
                    event_tx,
                    &surface,
                    WireOptions {
                        remote_offer,
                        local_offers,
                        handshake_timeout: timeout,
                        cleanup_child: None,
                    },
                );
                // The production spawn thread performs the same cleanup on any
                // wiring failure, releasing a writer blocked in the OS pipe.
                let _ = child.kill();
                let status = child.wait();
                let _ = result_tx.send((result, status));
            });
            let (stage_tx, stage_rx) = std_mpsc::sync_channel(1);
            thread::spawn(move || {
                let mut line = String::new();
                let result = std::io::BufReader::new(stderr).read_line(&mut line);
                let _ = stage_tx.send(result.map(|_| line));
            });
            assert_eq!(
                stage_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap()
                    .trim(),
                "connect-started"
            );
            assert!(matches!(
                result_rx.recv_timeout(Duration::from_millis(150)),
                Err(std_mpsc::RecvTimeoutError::Timeout)
            ));
            match action {
                "close" => handle.close(),
                "drop" => drop(handle),
                "timeout" => {}
                _ => unreachable!(),
            }
            let (result, status) = result_rx
                .recv_timeout(Duration::from_secs(4))
                .expect("blocked Connect did not stop promptly");
            assert!(status.is_ok(), "synthetic child was not reaped");
            if action == "timeout" {
                assert!(matches!(result, Err(RdpClientError::Protocol(_))));
            } else {
                assert!(matches!(result, Err(RdpClientError::Cancelled)));
            }
        }
    }

    #[test]
    #[ignore = "build rdp_handshake_test_peer and set ADIT_RDP_IPC_TEST_HELPER"]
    fn killing_a_blocked_peer_releases_the_connect_writer() {
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let executable = std::env::var_os("ADIT_RDP_IPC_TEST_HELPER").expect("fixture path");
        let mut child = ChildGuard(
            synthetic_peer(&executable, "blocked_connect")
                .spawn()
                .unwrap(),
        );
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let stdin = TrackedPipe {
            inner: child.0.stdin.take().unwrap(),
            dropped: dropped_tx,
        };
        let stdout = child.0.stdout.take().unwrap();
        let (handle, _commands) = RdpClientHandle::test_fixture_with_commands();
        let remote_offer = Arc::clone(&handle.remote_offer);
        let (result_tx, result_rx) = std_mpsc::channel();
        let mut request = synthetic_request();
        request.password = "synthetic-password".repeat(512 * 1024);
        thread::spawn(move || {
            let result =
                open_control_channel(stdin, stdout, request, Duration::from_secs(5), remote_offer);
            let _ = result_tx.send(result.map(|_| ()));
        });
        assert_eq!(peer_line(&mut child.0), "connect-started");
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(150)),
            Err(std_mpsc::RecvTimeoutError::Timeout)
        ));
        handle.close();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(RdpClientError::Cancelled)
        ));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        dropped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the blocked pipe writer did not exit after the helper was killed");
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
                local_offers: Arc::new(LocalFileOffers::default()),
                event_tx,
                cmd_tx,
                event_rx,
                surface: Arc::new(Mutex::new(surface)),
                child: Arc::new(Mutex::new(None)),
                reaper_started: AtomicBool::new(false),
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
            Some(RdpClientEvent::FileContentsForEpoch {
                generation: 10,
                stream_id: 42,
                data: None,
                ..
            })
        ));
        let current = handle.file_requester_for_generation(11);
        assert_eq!(
            current.try_request(42, 1, 4, 3),
            Err(RdpFileRequestError::DuplicateStream)
        );
        current.request(43, 1, 4, 3);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::RequestFileContents {
                generation: 11,
                stream_id: 43,
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
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::ClipboardPolicyEpoch(0))
        ));
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
