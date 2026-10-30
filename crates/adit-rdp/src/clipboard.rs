//! CLIPRDR adapter for text, images and generation-bound file snapshots.
//!
//! ## Why this shape
//!
//! IronRDP ships `ironrdp-cliprdr-native`, whose Windows backend owns the real
//! system clipboard and therefore needs a window and a `WM_CLIPBOARDUPDATE`
//! message pump. This helper is a windowless console process the GUI spawns, so
//! it has neither — which is exactly why RDP clipboard sat unimplemented, with
//! "needs design work" rather than a feature flag.
//!
//! The split that dissolves that problem: **the helper speaks CLIPRDR on the RDP
//! wire and nothing else; the GUI app owns the actual Windows clipboard.** The
//! app is a real windowed `iced` process with a message pump, so it can read and
//! write the system clipboard natively, and the two halves already have a
//! transport — [`adit_rdp_proto::InputEvent::ClipboardText`] inbound and
//! [`adit_rdp_proto::HostMsg::ClipboardText`] outbound. So this backend is a
//! pure protocol adapter with no OS dependency at all, and — usefully — its
//! whole state machine is testable without a live RDP host.
//!
//! ## File snapshot ownership
//!
//! Images and file ranges travel through IPC, while all OS interaction remains
//! in the app. Each remote file offer binds metadata, a generation and the
//! server's clipboard lock. Neither delayed rendering nor a queued request may
//! look up whichever lock happens to be current when it finally runs.
//!
//! ## Direction asymmetry (deliberate)
//!
//! Local → remote is *delay-rendered*, like mstsc: we only advertise the format
//! list, and the text crosses the wire when something on the remote actually
//! pastes. Remote → local is *eager*: as soon as the remote advertises text we
//! request it, because the app cannot answer a synchronous "give me the
//! clipboard now" from Windows across an async IPC hop.

mod remote_files;
use remote_files::{BoundRead, RemoteFiles, Reply};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::host::{ClipboardPolicySource, HostSendError, HostSender};
use adit_rdp_proto::{
    ClipFile, ClipboardFileRequestId, ClipboardStamp, HostMsg, RemoteClipboardStamp,
};
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_session::ActiveStage;

/// Ceiling on a single clipboard transfer in either direction, in bytes of the
/// decoded UTF-8 string. A hostile or buggy server can advertise and hand back
/// an arbitrarily large blob; the helper IPC would then try to frame it (capped
/// at 288 MiB) and the app would try to put it on the Windows clipboard. 8 MiB
/// is far past any plausible copy-paste and keeps both bounded.
pub(crate) const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

/// Text formats we understand, most preferred first. `CF_UNICODETEXT` is the
/// only one we ever *advertise*; the other two are accepted inbound because a
/// legacy or non-Windows server may offer nothing better.
const TEXT_FORMATS: [ClipboardFormatId; 3] = [
    ClipboardFormatId::CF_UNICODETEXT,
    ClipboardFormatId::CF_TEXT,
    ClipboardFormatId::CF_OEMTEXT,
];

fn is_text_format(id: ClipboardFormatId) -> bool {
    TEXT_FORMATS.contains(&id)
}

/// Image formats, most preferred first. `CF_DIB` is the one we advertise and
/// the one a Windows screenshot lands on; `CF_DIBV5` is accepted inbound
/// because some sources offer only that.
///
/// The bytes are passed through untouched in both directions — a DIB is a
/// BITMAPINFOHEADER followed by its pixels, and both ends already agree on that
/// layout, so parsing it here would only add a way to get it wrong.
const IMAGE_FORMATS: [ClipboardFormatId; 2] =
    [ClipboardFormatId::CF_DIB, ClipboardFormatId::CF_DIBV5];

fn is_image_format(id: ClipboardFormatId) -> bool {
    IMAGE_FORMATS.contains(&id)
}

/// Ceiling on one clipboard image. Far above `MAX_CLIPBOARD_BYTES`, because
/// that limit was sized for text and a screenshot is not text: an uncompressed
/// 4K DIB is about 33 MB, and refusing it would make the feature useless on the
/// screens people actually take screenshots of.
pub(crate) const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// The registered clipboard-format name for a file list. Constant across every
/// implementation per MS-RDPECLIP 1.3.1.2, while the numeric id is arbitrary and
/// OS-specific — so the name is the only reliable way to recognise it.
const FILE_LIST_FORMAT: &str = "FileGroupDescriptorW";

/// Ceiling on how many entries a clipboard file list may carry, in either
/// direction. A copied tree arrives fully expanded, so this is generous — but a
/// remote that advertises millions of descriptors must not be able to make the
/// app allocate for all of them.
const MAX_CLIPBOARD_FILES: usize = 65_536;

// The callback may receive many requests in one RDP frame before the session
// can drain it. File-direction overload is isolated, with an explicit notice;
// ambiguous requests cannot safely be answered even with an error.
const MAX_QUEUED_ACTIONS: usize = 512;
const MAX_QUEUED_ACTION_BYTES: usize = 128 * 1024 * 1024;
const MAX_LOCAL_FILE_LOCKS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    channel_epoch: u64,
    generation: u64,
    revision: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum LocalFileDirection {
    #[default]
    Fresh,
    Exposed(FileSnapshot),
    Sealed,
}

/// One unit of work the backend decided on, for the session loop to carry out.
///
/// The backend's callbacks run *inside* `ActiveStage::process`, where it cannot
/// reach the `Cliprdr` processor that owns it (it is the thing being borrowed),
/// let alone the socket. So it queues intent here and the session loop drains it
/// afterwards. That indirection is also what makes the state machine testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClipboardAction {
    /// Advertise these formats to the remote (`FormatList`). An empty list is
    /// meaningful and legal: "the local clipboard holds nothing you can use".
    Advertise(Vec<ClipboardFormatId>),
    /// Ask the remote for its clipboard in this format (`FormatDataRequest`).
    RequestText(ClipboardFormatId),
    /// Answer the remote's `FormatDataRequest`. `text: None` ⇒ error response.
    SubmitText {
        format: ClipboardFormatId,
        revision: Option<u64>,
        text: Option<String>,
    },
    /// Text arrived from the remote; hand it to the app to put on the real
    /// Windows clipboard.
    Inbound(String),
    /// Answer the remote's request for an image. `image: None` ⇒ error response.
    SubmitImage {
        format: ClipboardFormatId,
        revision: Option<u64>,
        image: Option<Vec<u8>>,
    },
    /// An image arrived from the remote; hand it to the app.
    InboundImage(Vec<u8>),
    InboundBoundText {
        stamp: RemoteClipboardStamp,
        text: String,
    },
    InboundBoundImage {
        stamp: RemoteClipboardStamp,
        image: Vec<u8>,
    },
    RemoteCopy(RemoteClipboardStamp),
    Notice(String),
    /// Offer a local file selection to the remote (`initiate_file_copy`).
    AdvertiseFiles(Vec<ClipFile>, FileSnapshot),
    /// Reply to the file-list format request with the exact cached snapshot.
    SubmitFileDescriptor {
        snapshot: Option<FileSnapshot>,
        response: OwnedFormatDataResponse,
    },
    /// Answer the remote's `FileContentsRequest` with bytes the app read.
    SubmitFileContents {
        generation: Option<u64>,
        revision: Option<u64>,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// A unique request received after the local file direction was closed.
    RefuseClosedFileContents {
        channel_epoch: u64,
        stream_id: u32,
    },
    /// Pull a byte range of a remote file (`request_file_contents`).
    RequestFileContents(BoundRead),
    /// The remote copied files; hand the list to the app to offer locally.
    InboundFiles {
        policy_epoch: u64,
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// The remote is pasting and needs bytes from a local file; ask the app.
    NeedFileContents {
        channel_epoch: u64,
        snapshot_revision: u64,
        generation: u64,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// Bytes arrived from the remote; hand them to the waiting local paste.
    InboundFileContents(Reply),
}

impl ClipboardAction {
    fn is_local_file_action(&self) -> bool {
        matches!(
            self,
            Self::AdvertiseFiles(..)
                | Self::SubmitFileDescriptor { .. }
                | Self::SubmitFileContents { .. }
                | Self::RefuseClosedFileContents { .. }
                | Self::NeedFileContents { .. }
        )
    }

    fn queued_bytes(&self) -> usize {
        let files_bytes = |files: &[ClipFile], capacity: usize| {
            files.iter().fold(
                capacity.saturating_mul(std::mem::size_of::<ClipFile>()),
                |size, file| size.saturating_add(file.name.capacity()),
            )
        };
        let payload = match self {
            Self::Advertise(ids) => ids
                .capacity()
                .saturating_mul(std::mem::size_of::<ClipboardFormatId>()),
            Self::SubmitText { text, .. } => text.as_ref().map_or(0, String::capacity),
            Self::Inbound(text) => text.capacity(),
            Self::InboundBoundText { text, .. } | Self::Notice(text) => text.capacity(),
            Self::SubmitImage { image, .. } => image.as_ref().map_or(0, Vec::capacity),
            Self::InboundImage(image) => image.capacity(),
            Self::InboundBoundImage { image, .. } => image.capacity(),
            Self::AdvertiseFiles(files, _) | Self::InboundFiles { files, .. } => {
                files_bytes(files, files.capacity())
            }
            Self::SubmitFileDescriptor { response, .. } => response.data().len(),
            Self::SubmitFileContents { data, .. }
            | Self::InboundFileContents(Reply { data, .. }) => {
                data.as_ref().map_or(0, Vec::capacity)
            }
            Self::RequestText(_)
            | Self::RemoteCopy(_)
            | Self::RequestFileContents(_)
            | Self::NeedFileContents { .. }
            | Self::RefuseClosedFileContents { .. } => 0,
        };
        std::mem::size_of::<Self>().saturating_add(payload)
    }
}

/// Split a flat wire name (`docs\notes.txt`) into the descriptor's directory and
/// leaf halves. MS-RDPECLIP carries the tree this way and so does [`ClipFile`],
/// so this is the only place the two representations meet.
fn split_relative(name: &str) -> (Option<String>, String) {
    match name.rsplit_once('\\') {
        Some((dir, leaf)) if !dir.is_empty() => (Some(dir.to_owned()), leaf.to_owned()),
        _ => (None, name.to_owned()),
    }
}

/// Rejoin a descriptor's two halves into the flat form [`ClipFile`] uses.
fn join_relative(descriptor: &FileDescriptor) -> String {
    match descriptor.relative_path.as_deref() {
        Some(dir) if !dir.is_empty() => format!("{dir}\\{}", descriptor.name),
        _ => descriptor.name.clone(),
    }
}

fn to_descriptor(file: &ClipFile) -> FileDescriptor {
    let (relative_path, name) = split_relative(&file.name);
    let mut attributes = ClipboardFileAttributes::empty();
    if file.is_dir {
        attributes |= ClipboardFileAttributes::DIRECTORY;
    }
    // `FileDescriptor` is #[non_exhaustive], so it is built through its
    // constructor and then filled in rather than with a struct literal.
    let mut descriptor = FileDescriptor::new(name);
    descriptor.attributes = Some(attributes);
    // A directory has no size, and sending one makes some servers try to read
    // bytes out of it.
    descriptor.file_size = (!file.is_dir).then_some(file.size);
    descriptor.relative_path = relative_path;
    descriptor
}

fn from_descriptor(descriptor: &FileDescriptor) -> ClipFile {
    let is_dir = descriptor
        .attributes
        .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY));
    ClipFile {
        name: join_relative(descriptor),
        size: descriptor.file_size.unwrap_or(0),
        is_dir,
    }
}

/// Validate before flattening attributes, so a reparse/device descriptor cannot
/// lose its unsafe type and masquerade as a regular file in the GUI.
fn validated_remote_files(files: &[FileDescriptor]) -> Result<Vec<ClipFile>, &'static str> {
    if files.len() > MAX_CLIPBOARD_FILES {
        return Err("too many remote file descriptors");
    }
    if files.iter().any(|file| {
        file.attributes
            .is_some_and(|a| a.bits() & (0x400 | 0x40) != 0)
    }) {
        return Err("remote reparse points and devices are unsupported");
    }
    let listed: Vec<_> = files.iter().map(from_descriptor).collect();
    adit_rdp_proto::validate_clipboard_file_list(&listed)?;
    Ok(listed)
}

/// Everything the backend and the session loop share. Session-scoped fields are
/// reset when a new backend is built, because a server redirection rebuilds the
/// CLIPRDR channel from scratch while the app's offered text stays valid.
#[derive(Debug, Clone, Copy)]
struct RemoteCopyRequest {
    format: ClipboardFormatId,
    file_list: bool,
    policy_epoch: u64,
    revision: u64,
    content_revision: u64,
    channel_epoch: u64,
}

impl RemoteCopyRequest {
    fn stamp(self) -> RemoteClipboardStamp {
        RemoteClipboardStamp {
            local: ClipboardStamp {
                policy_epoch: self.policy_epoch,
                content_revision: self.content_revision,
            },
            channel_epoch: self.channel_epoch,
            copy_revision: self.revision,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ClipboardState {
    clipboard_policy_epoch: Arc<ClipboardPolicySource>,
    clipboard_ready_pending: Option<u64>,
    clipboard_ready_announced: Option<u64>,
    /// Newest text the app offered from the local (GUI-owned) clipboard.
    local_text: Option<String>,
    local_content_revision: u64,
    local_stamp: Option<ClipboardStamp>,
    /// Newest image the app offered, as raw `CF_DIB` bytes.
    local_image: Option<Vec<u8>>,
    /// Repeat-suppression for the image, as `advertised_text` is for text. The
    /// GUI polls on a timer and a screenshot is megabytes, so re-advertising an
    /// unchanged one is the expensive kind of pointless.
    advertised_image_len: Option<usize>,
    /// The text last put into a `FormatList`. Advertising is idempotent on the
    /// wire but not free, and the GUI polls its clipboard on a timer, so this
    /// suppresses the repeats.
    advertised_text: Option<String>,
    /// The format we asked the remote for. `FormatDataResponse` does not echo
    /// the format, so without this we could not pick the right character set.
    pending_remote_format: Option<ClipboardFormatId>,
    // FormatDataResponse carries no request id. Keep one request on the wire
    // and coalesce newer copies, so a response cannot inherit a later policy.
    remote_copy_revision: u64,
    active_remote_copy: Option<RemoteCopyRequest>,
    deferred_remote_copy: Option<RemoteCopyRequest>,
    active_remote_since: Option<Instant>,
    remote_timeout_notified: bool,
    /// CLIPRDR reached `Ready` (capabilities exchanged and our first
    /// `FormatList` acknowledged). Advertising before that is out of sequence.
    ready: bool,
    /// Files the app offered from the local clipboard, flat with relative paths.
    /// Held here rather than only inside `Cliprdr` because a `SIZE` request is
    /// answered straight from this metadata without troubling the app.
    local_files: Vec<ClipFile>,
    local_file_generation: u64,
    // Internal identity for the actual snapshot; the externally supplied
    // generation policy stays unchanged, including repeated generations.
    local_file_revision: u64,
    // Both counters remain helper-scoped across channel reconstruction.
    channel_epoch: u64,
    pending_local_reads: HashSet<ClipboardFileRequestId>,
    // Includes metadata and refused requests until their wire reply completes.
    local_file_streams: HashSet<u32>,
    // A closed channel remembers both ambiguous old streams and every new
    // refusal until the next channel, so a reused ID can never get a guessed reply.
    // `true` means a unique refusal has not yet reached the transport.
    closed_file_streams: HashMap<u32, bool>,
    // Identifies IronRDP's cached list, even while a newer offer is waiting to
    // replace it. Do not clear this on revocation before the processor is updated.
    cached_file_snapshot: Option<FileSnapshot>,
    local_file_direction: LocalFileDirection,
    local_file_suspended: bool,
    local_file_suspension_ack: Option<u64>,
    local_file_ready_pending: Option<u64>,
    local_file_ready_announced: Option<u64>,
    local_file_notice_pending: VecDeque<u64>,
    local_file_locks: HashMap<u32, FileSnapshot>,
    /// Same repeat-suppression as `advertised_text`: the GUI polls on a timer.
    advertised_files: Vec<ClipFile>,
    /// The lock id the server handed us with the remote's file list. Quoted back
    /// on every `request_file_contents` so the bytes come from the snapshot that
    /// was copied, not from whatever the remote clipboard holds by the time the
    /// paste actually runs.
    remote_files: RemoteFiles,
    /// Whether the server negotiated file transfer at all. Without the
    /// capability `initiate_file_copy` and `request_file_contents` both refuse,
    /// so this is what stops us queueing work that can only fail.
    files_negotiated: bool,
    actions: VecDeque<ClipboardAction>,
    actions_overloaded: bool,
    rejected_file_advertise_pending: bool,
}

impl ClipboardState {
    fn remote_copy_is_current(&self, request: RemoteCopyRequest) -> bool {
        request.revision == self.remote_copy_revision
            && request.channel_epoch == self.channel_epoch
            && self.clipboard_policy_epoch.snapshot_full()
                == (request.policy_epoch, true, request.content_revision)
    }

    fn local_content_is_current(&self) -> bool {
        if !self.clipboard_policy_epoch.is_shared() {
            return true;
        }
        self.local_stamp.is_some_and(|stamp| {
            self.clipboard_policy_epoch.snapshot_full()
                == (stamp.policy_epoch, true, stamp.content_revision)
        })
    }

    fn check_remote_timeout(&mut self) {
        if !self.remote_timeout_notified
            && self
                .active_remote_since
                .is_some_and(|since| since.elapsed() >= Duration::from_secs(10))
        {
            self.remote_timeout_notified = true;
            self.queue(ClipboardAction::Notice(
                "等待远端剪贴板响应超时；后续复制已暂停，等待旧请求结束或重新连接后恢复。".into(),
            ));
        }
    }

    fn start_remote_copy(&mut self, request: RemoteCopyRequest) {
        self.pending_remote_format = (!request.file_list).then_some(request.format);
        self.active_remote_copy = Some(request);
        self.active_remote_since = Some(Instant::now());
        self.remote_timeout_notified = false;
        self.queue(ClipboardAction::RequestText(request.format));
    }

    fn finish_remote_copy(&mut self) -> Option<RemoteCopyRequest> {
        let completed = self.active_remote_copy.take();
        self.active_remote_since = None;
        self.pending_remote_format = None;
        let valid = completed.filter(|request| self.remote_copy_is_current(*request));
        if let Some(next) = self
            .deferred_remote_copy
            .take()
            .filter(|next| self.remote_copy_is_current(*next))
        {
            self.start_remote_copy(next);
        }
        valid
    }
    fn remember_closed_file_streams(&mut self) {
        for &stream_id in &self.local_file_streams {
            self.closed_file_streams.insert(stream_id, false);
        }
    }

    fn refuse_closed_file_stream(&mut self, stream_id: u32) {
        if let Some(pending) = self.closed_file_streams.get_mut(&stream_id) {
            *pending = false;
            if !self.local_file_suspended {
                self.suspend_local_file_direction();
            }
            return;
        }
        // IDs are retained for the whole channel, including those whose reply
        // was written or dropped, so both memory and total failures are bounded.
        if self.closed_file_streams.len() >= MAX_QUEUED_ACTIONS {
            return;
        }
        let action = ClipboardAction::RefuseClosedFileContents {
            channel_epoch: self.channel_epoch,
            stream_id,
        };
        let queued_bytes = self
            .actions
            .iter()
            .fold(0usize, |sum, item| sum.saturating_add(item.queued_bytes()));
        let can_queue = !self.actions_overloaded
            && self.actions.len() < MAX_QUEUED_ACTIONS
            && queued_bytes.saturating_add(action.queued_bytes()) <= MAX_QUEUED_ACTION_BYTES;
        self.closed_file_streams.insert(stream_id, can_queue);
        if can_queue {
            self.actions.push_back(action);
        }
    }

    fn reserve_local_file_stream(&mut self, stream_id: u32) -> bool {
        if self.local_file_suspended {
            return false;
        }
        if self.local_file_streams.len() >= MAX_QUEUED_ACTIONS
            || !self.local_file_streams.insert(stream_id)
        {
            self.suspend_local_file_direction();
            return false;
        }
        true
    }

    fn suspend_local_file_direction(&mut self) {
        // An unattributed request or a new suspension can conflict with any
        // not-yet-written refusal, including one already encoded by the pump.
        self.remember_closed_file_streams();
        for pending in self.closed_file_streams.values_mut() {
            *pending = false;
        }
        self.actions
            .retain(|action| !matches!(action, ClipboardAction::RefuseClosedFileContents { .. }));
        if self.local_file_suspended {
            return;
        }
        self.local_file_suspended = true;
        self.local_file_ready_pending = None;
        self.local_file_suspension_ack = Some(self.channel_epoch);
        self.local_file_direction = LocalFileDirection::Sealed;
        self.pending_local_reads.clear();
        self.local_file_streams.clear();
        self.local_files.clear();
        self.local_file_generation = 0;
        self.local_file_locks.clear();
        self.advertised_files.clear();
        self.actions.retain(|action| !action.is_local_file_action());
        // Replace the remote file advertisement when there is capacity. Keep
        // text/image and the opposite file direction intact.
        if self.ready {
            self.actions
                .retain(|action| !matches!(action, ClipboardAction::Advertise(_)));
            let advert = ClipboardAction::Advertise(self.local_formats());
            let queued_bytes = self
                .actions
                .iter()
                .fold(0usize, |sum, item| sum.saturating_add(item.queued_bytes()));
            if self.actions.len() < MAX_QUEUED_ACTIONS
                && queued_bytes.saturating_add(advert.queued_bytes()) <= MAX_QUEUED_ACTION_BYTES
            {
                self.queue_advertise();
            }
        }
        tracing::warn!("本地文件发送至远端已禁用；远程桌面仍可使用");
    }

    fn seal_local_file_direction(&mut self) {
        if self.local_file_direction != LocalFileDirection::Sealed {
            self.remember_closed_file_streams();
            self.local_file_ready_pending = None;
            self.local_file_direction = LocalFileDirection::Sealed;
            self.local_file_notice_pending.push_back(self.channel_epoch);
            tracing::warn!("本地文件发送至远端已暂时禁用；桌面仍可使用。已发送或已被传输层接收的数据仍可能到达。");
        }
    }

    fn queue_local_file_ready_if_available(&mut self) {
        if self.ready
            && self.files_negotiated
            && !self.local_file_suspended
            && self.local_file_direction != LocalFileDirection::Sealed
            && self.local_file_ready_announced != Some(self.channel_epoch)
        {
            self.local_file_ready_pending = Some(self.channel_epoch);
        }
    }

    fn queue(&mut self, action: ClipboardAction) {
        self.queue_with_limits(action, MAX_QUEUED_ACTIONS, MAX_QUEUED_ACTION_BYTES);
    }

    fn queue_with_limits(&mut self, action: ClipboardAction, max_actions: usize, max_bytes: usize) {
        if self.actions_overloaded {
            return;
        }
        if self.local_file_suspended && action.is_local_file_action() {
            return;
        }
        let queued_bytes = self.actions.iter().fold(0usize, |sum, action| {
            sum.saturating_add(action.queued_bytes())
        });
        if self.actions.len() >= max_actions
            || queued_bytes.saturating_add(action.queued_bytes()) > max_bytes
        {
            if action.is_local_file_action() {
                self.suspend_local_file_direction();
            } else {
                self.actions_overloaded = true;
                self.suspend_local_file_direction();
                tracing::warn!("clipboard action budget exceeded; disabling clipboard work");
            }
            return;
        }
        self.actions.push_back(action);
    }

    fn revoke_remote_files(&mut self, expects_files: bool) {
        let (had_offer, replies) = self.remote_files.revoke(expects_files);
        for reply in replies {
            self.queue(ClipboardAction::InboundFileContents(reply));
        }
        if had_offer {
            self.queue(ClipboardAction::InboundFiles {
                policy_epoch: self.clipboard_policy_epoch.epoch(),
                generation: self.remote_files.generation(),
                files: Vec::new(),
            });
        }
    }

    /// Formats we can currently serve. Empty when the app has offered nothing:
    /// advertising `CF_UNICODETEXT` with no text behind it would make every
    /// remote paste answer with an error response.
    fn local_formats(&self) -> Vec<ClipboardFormatId> {
        let mut formats = Vec::new();
        if self.local_text.is_some() {
            formats.push(ClipboardFormatId::CF_UNICODETEXT);
        }
        if self.local_image.is_some() {
            formats.push(ClipboardFormatId::CF_DIB);
        }
        formats
    }

    fn queue_advertise(&mut self) {
        self.advertised_text = self.local_text.clone();
        self.advertised_image_len = self.local_image.as_ref().map(Vec::len);
        let formats = self.local_formats();
        self.queue(ClipboardAction::Advertise(formats));
    }

    fn queue_advertise_files(&mut self) {
        if self.local_file_direction == LocalFileDirection::Sealed {
            return;
        }
        self.advertised_text = None;
        self.advertised_image_len = None;
        self.advertised_files = self.local_files.clone();
        self.queue(ClipboardAction::AdvertiseFiles(
            self.local_files.clone(),
            FileSnapshot {
                channel_epoch: self.channel_epoch,
                generation: self.local_file_generation,
                revision: self.local_file_revision,
            },
        ));
    }

    fn invalidate_local_files(&mut self) -> bool {
        if matches!(self.local_file_direction, LocalFileDirection::Exposed(_)) {
            self.seal_local_file_direction();
        }
        let had_files = !self.local_files.is_empty()
            || !self.advertised_files.is_empty()
            || self
                .actions
                .iter()
                .any(|action| matches!(action, ClipboardAction::AdvertiseFiles(..)));
        match self.local_file_revision.checked_add(1) {
            Some(next) => self.local_file_revision = next,
            None => self.actions_overloaded = true,
        }
        self.local_files.clear();
        self.local_file_generation = 0;
        self.local_file_locks.clear();
        self.advertised_files.clear();
        self.actions
            .retain(|action| !matches!(action, ClipboardAction::AdvertiseFiles(..)));
        for action in &mut self.actions {
            if let ClipboardAction::NeedFileContents {
                channel_epoch,
                snapshot_revision,
                generation,
                stream_id,
                ..
            } = action
            {
                self.pending_local_reads.remove(&ClipboardFileRequestId {
                    channel_epoch: *channel_epoch,
                    snapshot_revision: *snapshot_revision,
                    generation: *generation,
                    stream_id: *stream_id,
                });
            }
            let stream_id = match action {
                ClipboardAction::NeedFileContents { stream_id, .. }
                | ClipboardAction::SubmitFileContents { stream_id, .. } => Some(*stream_id),
                _ => None,
            };
            if let Some(stream_id) = stream_id {
                *action = ClipboardAction::SubmitFileContents {
                    generation: None,
                    revision: None,
                    stream_id,
                    data: None,
                };
            }
        }
        had_files
    }

    fn invalidate_local_content(&mut self) {
        self.local_content_revision = self.local_content_revision.wrapping_add(1);
        for action in &mut self.actions {
            match action {
                ClipboardAction::SubmitText { revision, text, .. } => {
                    *revision = None;
                    *text = None;
                }
                ClipboardAction::SubmitImage {
                    revision, image, ..
                } => {
                    *revision = None;
                    *image = None;
                }
                _ => {}
            }
        }
    }

    /// Record text the app copied locally and, once the channel is up, offer it
    /// to the remote. Before `Ready` we only stash it: the initial `FormatList`
    /// is part of the handshake and is sent from `on_request_format_list`.
    /// Record an image the app copied locally and offer it to the remote.
    ///
    /// Like text and files, only the *format* is advertised — the bytes cross
    /// when something over there pastes. Unlike them, the bytes are already in
    /// hand, because a DIB has no path to fetch it from later.
    pub(crate) fn offer_local_image(&mut self, image: Vec<u8>) {
        if self.local_image.as_ref().is_some_and(|held| *held == image) {
            return;
        }
        self.revoke_remote_files(false);
        self.invalidate_local_content();
        self.invalidate_local_files();
        self.local_image = Some(image);
        self.rejected_file_advertise_pending = false;
        // One clipboard holds one thing: an image copy replaces whatever text
        // or file list was offered, exactly as it does on Windows.
        self.local_text = None;
        if self.ready {
            self.queue_advertise();
        }
    }

    pub(crate) fn offer_local_text(&mut self, text: String) {
        if self.local_text.as_deref() == Some(text.as_str()) {
            return;
        }
        self.revoke_remote_files(false);
        self.invalidate_local_content();
        self.invalidate_local_files();
        self.local_text = Some(text);
        self.rejected_file_advertise_pending = false;
        self.local_image = None;
        // One clipboard holds one thing. MS-RDPECLIP agrees: each FormatList
        // completely replaces the last, and `initiate_copy` drops the file list
        // upstream — mirroring it here keeps our idea of what is offered from
        // drifting away from the processor's.
        if self.ready {
            self.queue_advertise();
        }
    }

    /// Record files the app copied locally and offer them to the remote.
    ///
    /// Nothing is read from disk here or at any point in this process: only the
    /// metadata crosses. Bytes are pulled one range at a time when — and only
    /// when — something on the remote actually pastes.
    pub(crate) fn offer_local_files(&mut self, generation: u64, files: Vec<ClipFile>) {
        if self.local_file_suspended && !files.is_empty() {
            return;
        }
        let files =
            if generation != 0 && adit_rdp_proto::validate_clipboard_file_list(&files).is_ok() {
                files
            } else {
                Vec::new()
            };
        if files.is_empty() {
            let had_files = self.invalidate_local_files();
            if had_files {
                self.invalidate_local_content();
            }
            if had_files && self.ready {
                // initiate_copy clears IronRDP's cached descriptor list as well.
                self.queue_advertise();
            }
            return;
        }
        if self.local_files == files && self.local_file_generation == generation {
            return;
        }
        self.revoke_remote_files(false);
        self.invalidate_local_content();
        self.invalidate_local_files();
        self.local_files = files;
        self.local_file_generation = generation;
        self.local_file_locks.clear();
        self.local_text = None;
        self.local_image = None;
        if self.ready
            && self.files_negotiated
            && self.local_file_direction != LocalFileDirection::Sealed
        {
            self.queue_advertise_files();
        } else if self.ready {
            // File transfer is unavailable, but the old text/image offer is no
            // longer the GUI clipboard and must not remain advertised.
            self.queue_advertise();
        }
    }

    /// Look up an offered file by its index in the list the remote was given.
    fn local_file(&self, index: u32) -> Option<&ClipFile> {
        self.local_files.get(usize::try_from(index).ok()?)
    }

    fn reject_local_file_offer(&mut self) {
        self.invalidate_local_content();
        self.invalidate_local_files();
        self.local_text = None;
        self.local_image = None;

        // Replace an unsent initial or later format list as well as any
        // already-advertised content. Coalescing keeps file floods nonfatal.
        let had_advert = self
            .actions
            .iter()
            .any(|action| matches!(action, ClipboardAction::Advertise(_)));
        self.actions
            .retain(|action| !matches!(action, ClipboardAction::Advertise(_)));
        if self.ready || had_advert {
            let advert = ClipboardAction::Advertise(Vec::new());
            let queued_bytes = self.actions.iter().fold(0usize, |sum, action| {
                sum.saturating_add(action.queued_bytes())
            });
            if self.actions.len() < MAX_QUEUED_ACTIONS
                && queued_bytes.saturating_add(advert.queued_bytes()) <= MAX_QUEUED_ACTION_BYTES
            {
                self.rejected_file_advertise_pending = false;
                self.queue_advertise();
            } else {
                // Keep the empty replacement pending until actions drain.
                self.rejected_file_advertise_pending = true;
            }
        }
    }

    pub(crate) fn take_actions(&mut self) -> Vec<ClipboardAction> {
        self.actions.drain(..).collect()
    }

    /// Forget everything scoped to one CLIPRDR channel. The offered text is
    /// deliberately kept: a GNOME system-mode handover redirects and rebuilds
    /// the channel, and the user's clipboard did not change under them.
    fn reset_for_new_channel(&mut self) {
        self.clipboard_ready_pending = None;
        self.clipboard_ready_announced = None;
        match self.channel_epoch.checked_add(1) {
            Some(next) => self.channel_epoch = next,
            None => self.actions_overloaded = true,
        }
        self.pending_local_reads.clear();
        self.rejected_file_advertise_pending = false;
        self.local_file_streams.clear();
        self.closed_file_streams.clear();
        self.local_file_ready_pending = None;
        self.advertised_text = None;
        self.advertised_image_len = None;
        self.advertised_files.clear();
        self.cached_file_snapshot = None;
        self.local_file_direction = if self.local_file_suspended {
            LocalFileDirection::Sealed
        } else {
            LocalFileDirection::Fresh
        };
        self.local_file_locks.clear();
        self.pending_remote_format = None;
        self.active_remote_copy = None;
        self.deferred_remote_copy = None;
        self.active_remote_since = None;
        self.remote_timeout_notified = false;
        self.ready = false;
        // Scoped to the channel, not to the user's clipboard: the lock id and the
        // negotiated capability both belong to the CLIPRDR session that just went
        // away. `local_files` deliberately survives, exactly as `local_text` does.
        self.files_negotiated = false;
        // A response may already have left `remote_files.pending` but still be
        // waiting for the pump. Do not strand that local paste on redirection;
        // the old channel is gone, so report a failure for its queued reply.
        self.actions.retain(|action| match action {
            ClipboardAction::InboundFileContents(_) => true,
            // The app may still own the old requester even if the remote
            // snapshot has already been revoked locally.
            ClipboardAction::InboundFiles { files, .. } => files.is_empty(),
            _ => false,
        });
        for action in &mut self.actions {
            if let ClipboardAction::InboundFileContents(reply) = action {
                reply.data = None;
            }
        }
        self.revoke_remote_files(false);
    }
}

pub(crate) type SharedClipboard = Arc<Mutex<ClipboardState>>;

#[cfg(test)]
pub(crate) fn new_shared() -> SharedClipboard {
    new_shared_for_policy(Arc::new(ClipboardPolicySource::default()))
}

pub(crate) fn new_shared_for_policy(policy: Arc<ClipboardPolicySource>) -> SharedClipboard {
    Arc::new(Mutex::new(ClipboardState {
        clipboard_policy_epoch: policy,
        ..ClipboardState::default()
    }))
}

/// End the old channel before starting a redirect connection. Its local offer
/// survives, but all remote file snapshots and reads cease to be valid now.
pub(crate) async fn leave_channel(
    state: &SharedClipboard,
    host: &HostSender,
) -> Result<(), ClipboardPumpError> {
    {
        let Ok(mut guard) = state.lock() else {
            tracing::warn!("clipboard state is unavailable during channel replacement");
            return Ok(());
        };
        guard.reset_for_new_channel();
        host.send(HostMsg::ClipboardChannelChanged {
            channel_epoch: guard.channel_epoch,
        })
        .map_err(|error| match error {
            HostSendError::Closed => ClipboardPumpError::AppClosed,
            HostSendError::Overloaded => ClipboardPumpError::HostOverloaded,
        })?;
    }
    flush_app_actions(state, host).await
}

/// Hand text the app just copied locally to the CLIPRDR state machine. A
/// poisoned lock is ignored rather than propagated — losing a clipboard offer is
/// not worth tearing a live desktop session down.
pub(crate) fn offer_local_text(state: &SharedClipboard, text: String) {
    if let Ok(mut guard) = state.lock() {
        if !guard.clipboard_policy_epoch.is_shared() {
            guard.offer_local_text(text);
        }
    }
}

/// Hand an image the app just copied locally to the CLIPRDR state machine.
pub(crate) fn offer_local_image(state: &SharedClipboard, image: Vec<u8>) {
    if let Ok(mut guard) = state.lock() {
        if !guard.clipboard_policy_epoch.is_shared() {
            guard.offer_local_image(image);
        }
    }
}

pub(crate) fn offer_bound_text(state: &SharedClipboard, stamp: ClipboardStamp, text: String) {
    if let Ok(mut guard) = state.lock() {
        if guard.clipboard_policy_epoch.snapshot_full()
            == (stamp.policy_epoch, true, stamp.content_revision)
        {
            guard.offer_local_text(text);
            guard.local_stamp = Some(stamp);
        }
    }
}

pub(crate) fn offer_bound_image(state: &SharedClipboard, stamp: ClipboardStamp, image: Vec<u8>) {
    if let Ok(mut guard) = state.lock() {
        if guard.clipboard_policy_epoch.snapshot_full()
            == (stamp.policy_epoch, true, stamp.content_revision)
        {
            guard.offer_local_image(image);
            guard.local_stamp = Some(stamp);
        }
    }
}

/// Apply the exact shared snapshot before the session can acknowledge it.
pub(crate) fn apply_shared_policy(state: &SharedClipboard) -> (u64, bool) {
    loop {
        let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
        let (epoch, enabled, revision) = guard.clipboard_policy_epoch.snapshot_full();
        guard.actions.retain(|action| match action {
            ClipboardAction::InboundBoundText { stamp, .. }
            | ClipboardAction::InboundBoundImage { stamp, .. }
            | ClipboardAction::RemoteCopy(stamp) => {
                enabled
                    && stamp.local.policy_epoch == epoch
                    && stamp.local.content_revision == revision
            }
            _ => true,
        });
        if !enabled || !guard.local_content_is_current() {
            guard.invalidate_local_content();
            guard.invalidate_local_files();
            guard.local_text = None;
            guard.local_image = None;
            guard.local_stamp = None;
            guard.advertised_text = None;
            guard.advertised_image_len = None;
            guard.actions.retain(|action| {
                !matches!(
                    action,
                    ClipboardAction::Advertise(_) | ClipboardAction::AdvertiseFiles(..)
                )
            });
            if guard.ready {
                guard.queue_advertise();
            }
        }
        if !enabled {
            guard.deferred_remote_copy = None;
            guard.revoke_remote_files(false);
        }
        // Do not claim a different snapshot if the app changed it while we worked.
        if guard.clipboard_policy_epoch.snapshot_full() == (epoch, enabled, revision) {
            return (epoch, enabled);
        }
    }
}

/// Hand a local file selection to the CLIPRDR state machine. Metadata only —
/// nothing is read from disk until the remote pastes.
pub(crate) fn offer_local_files(state: &SharedClipboard, generation: u64, files: Vec<ClipFile>) {
    if let Ok(mut guard) = state.lock() {
        guard.offer_local_files(generation, files);
    }
}

/// A local file copy displaced the previous text or image, but file sharing is
/// disabled. Never let the prior content remain pasteable as a stale format.
pub(crate) fn reject_local_file_offer(state: &SharedClipboard) {
    if let Ok(mut guard) = state.lock() {
        guard.reject_local_file_offer();
    }
}

pub(crate) fn suspend_local_file_direction(state: &SharedClipboard) {
    if let Ok(mut guard) = state.lock() {
        guard.suspend_local_file_direction();
    }
}

/// Queue the app's answer to a [`HostMsg::FileContentsNeeded`].
pub(crate) fn submit_file_contents(
    state: &SharedClipboard,
    request: ClipboardFileRequestId,
    data: Option<Vec<u8>>,
) {
    if let Ok(mut guard) = state.lock() {
        // Never answer on a replacement channel, even with an error: its peer
        // may have reused the same stream ID for an unrelated request.
        if request.channel_epoch != guard.channel_epoch
            || !guard.pending_local_reads.remove(&request)
        {
            return;
        }
        let data = if guard.local_file_direction == LocalFileDirection::Sealed
            || request.generation == 0
            || guard.local_file_generation != request.generation
            || guard.local_file_revision != request.snapshot_revision
        {
            None
        } else {
            data
        };
        guard.queue(ClipboardAction::SubmitFileContents {
            generation: data.as_ref().map(|_| request.generation),
            revision: data.as_ref().map(|_| request.snapshot_revision),
            stream_id: request.stream_id,
            data,
        });
    }
}

/// Queue a pull of a byte range from a file the remote offered.
///
/// Refused outright when the server never negotiated file transfer: upstream's
/// `request_file_contents` would reject it anyway, and failing here means the
/// waiting local paste is told "no" instead of hanging on a stream that will
/// never be answered.
pub(crate) fn request_file_contents(
    state: &SharedClipboard,
    generation: u64,
    stream_id: u32,
    index: u32,
    offset: u64,
    length: u32,
) {
    if let Ok(mut guard) = state.lock() {
        let action = match guard
            .remote_files
            .request(generation, stream_id, index, offset, length)
        {
            Ok(read) => ClipboardAction::RequestFileContents(read),
            Err(reply) => ClipboardAction::InboundFileContents(reply),
        };
        guard.queue(action);
    }
}

fn fail_remote_read(state: &SharedClipboard, wire_id: u32) {
    if let Ok(mut guard) = state.lock() {
        if let Some(reply) = guard.remote_files.fail(wire_id) {
            guard.queue(ClipboardAction::InboundFileContents(reply));
        }
    }
}

fn prepare_remote_read(state: &SharedClipboard, read: &BoundRead) -> Option<FileContentsRequest> {
    let current = state.lock().ok()?.remote_files.current(read);
    if !current {
        fail_remote_read(state, read.wire_id);
        return None;
    }
    Some(FileContentsRequest {
        stream_id: read.wire_id,
        index: i32::try_from(read.index).ok()?,
        flags: FileContentsFlags::RANGE,
        position: read.offset,
        requested_size: read.length,
        data_id: Some(read.data_id),
    })
}

/// The CLIPRDR backend. Owns no OS handle at all — see the module docs.
#[derive(Debug)]
pub(crate) struct AditCliprdrBackend {
    state: SharedClipboard,
}

impl AditCliprdrBackend {
    pub(crate) fn new(state: SharedClipboard) -> Self {
        if let Ok(mut guard) = state.lock() {
            guard.reset_for_new_channel();
        }
        Self { state }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ClipboardState) -> R) -> Option<R> {
        self.state.lock().ok().map(|mut guard| f(&mut guard))
    }

    /// Refuse a reserved, unambiguous request while the direction is active.
    fn refuse(&self, stream_id: u32) {
        self.with_state(|state| {
            state.queue(ClipboardAction::SubmitFileContents {
                generation: None,
                revision: None,
                stream_id,
                data: None,
            });
        });
    }
}

ironrdp_core::impl_as_any!(AditCliprdrBackend);

impl CliprdrBackend for AditCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        // Announced by CLIPRDR_TEMP_DIRECTORY and never used by us: files are
        // delay-rendered straight from their real path on demand, so nothing is
        // ever staged. Some servers refuse the handshake without the field.
        "."
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // `Cliprdr` adds USE_LONG_FORMAT_NAMES itself.
        //
        // CAN_LOCK_CLIPDATA rides along with file support and is not really
        // optional: without it the server hands us no clipDataId, and a paste
        // that takes a while then reads whatever the remote clipboard holds as
        // each chunk arrives rather than what was copied. Locking is what pins
        // the snapshot for the length of the transfer.
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
    }

    fn on_ready(&mut self) {
        self.with_state(|state| {
            state.ready = true;
            if state.clipboard_policy_epoch.is_shared()
                && state.clipboard_ready_announced != Some(state.channel_epoch)
            {
                state.clipboard_ready_pending = Some(state.channel_epoch);
            }
            state.queue_local_file_ready_if_available();
            // The app may have changed its clipboard after the initial list.
            if state.local_text != state.advertised_text
                || state.local_image.as_ref().map(Vec::len) != state.advertised_image_len
            {
                state.queue_advertise();
            }
            // Capabilities can arrive before Ready. In that ordering the
            // capability callback cannot advertise the current file list yet.
            if state.files_negotiated
                && !state.local_files.is_empty()
                && state.advertised_files != state.local_files
            {
                state.queue_advertise_files();
            }
        });
        tracing::debug!("CLIPRDR channel ready");
    }

    fn on_request_format_list(&mut self) {
        // Part of the handshake, not a user action: in Initialization state
        // `initiate_copy` is what bundles Capabilities + TemporaryDirectory +
        // FormatList, and the channel only reaches Ready once the server
        // acknowledges that FormatList. So this must be queued even when we have
        // nothing to offer — an empty list is the legal way to say so.
        self.with_state(ClipboardState::queue_advertise);
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        let files = capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED);
        self.with_state(|state| {
            state.files_negotiated = files;
            state.queue_local_file_ready_if_available();
            if !files {
                state.revoke_remote_files(false);
            }
            // Files copied before the handshake finished missed their chance to
            // be advertised; this is the first moment we know they can be.
            if files
                && state.ready
                && !state.local_files.is_empty()
                && state.advertised_files != state.local_files
            {
                state.queue_advertise_files();
            }
        });
        tracing::debug!(?capabilities, files, "CLIPRDR capabilities negotiated");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // The format list identifies the copy; the delayed data response does
        // not. Bind its policy here before asking the remote for descriptors.
        let Some((policy_epoch, enabled, content_revision)) =
            self.with_state(|state| state.clipboard_policy_epoch.snapshot_full())
        else {
            return;
        };
        let files = available_formats
            .iter()
            .find(|format| format.name().is_some_and(|n| n.value() == FILE_LIST_FORMAT));
        let text_available = TEXT_FORMATS
            .iter()
            .any(|wanted| available_formats.iter().any(|f| f.id == *wanted));
        let chosen = if let Some(format) = files {
            Some((format.id, true))
        } else if !text_available {
            IMAGE_FORMATS
                .iter()
                .copied()
                .find(|wanted| available_formats.iter().any(|f| f.id == *wanted))
                .map(|format| (format, false))
        } else {
            TEXT_FORMATS
                .iter()
                .copied()
                .find(|wanted| available_formats.iter().any(|f| f.id == *wanted))
                .map(|format| (format, false))
        };
        self.with_state(|state| {
            state.revoke_remote_files(files.is_some());
            let Some(revision) = state.remote_copy_revision.checked_add(1) else {
                state.actions_overloaded = true;
                return;
            };
            state.remote_copy_revision = revision;
            state.deferred_remote_copy = None;
            let stamp = RemoteClipboardStamp {
                local: ClipboardStamp {
                    policy_epoch,
                    content_revision,
                },
                channel_epoch: state.channel_epoch,
                copy_revision: revision,
            };
            if enabled && state.clipboard_policy_epoch.is_shared() {
                state.queue(ClipboardAction::RemoteCopy(stamp));
            }
            if let (Some((format, file_list)), true) = (chosen, enabled) {
                let request = RemoteCopyRequest {
                    format,
                    file_list,
                    policy_epoch,
                    revision,
                    content_revision,
                    channel_epoch: state.channel_epoch,
                };
                if state.active_remote_copy.is_some() {
                    state.deferred_remote_copy = Some(request);
                } else {
                    state.start_remote_copy(request);
                }
            }
        });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let format = request.format;
        self.with_state(|state| {
            if is_image_format(format) {
                let image = if state.ready && state.local_content_is_current() {
                    state.local_image.clone()
                } else {
                    None
                };
                if image.is_none() {
                    tracing::debug!(format = format.value(), "no image to serve");
                }
                let revision = image.as_ref().map(|_| state.local_content_revision);
                state.queue(ClipboardAction::SubmitImage {
                    format,
                    revision,
                    image,
                });
                return;
            }
            let text = if state.ready && state.local_content_is_current() && is_text_format(format)
            {
                state.local_text.clone()
            } else {
                None
            };
            if text.is_none() {
                tracing::debug!(
                    format = format.value(),
                    "remote asked for a format we cannot serve; answering with an error response"
                );
            }
            let revision = text.as_ref().map(|_| state.local_content_revision);
            state.queue(ClipboardAction::SubmitText {
                format,
                revision,
                text,
            });
        });
    }

    fn on_local_file_list_response(
        &mut self,
        response: OwnedFormatDataResponse,
    ) -> Option<OwnedFormatDataResponse> {
        self.with_state(|state| {
            let snapshot = state.cached_file_snapshot;
            // A list whose provenance is unknown cannot be tied to an active
            // offer. Still answer the request so the peer is not left waiting.
            let response = if state.ready
                && snapshot.is_some()
                && state.local_file_direction != LocalFileDirection::Sealed
            {
                response
            } else {
                OwnedFormatDataResponse::new_error()
            };
            state.queue(ClipboardAction::SubmitFileDescriptor { snapshot, response });
        });
        None
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let request = self
            .with_state(ClipboardState::finish_remote_copy)
            .flatten();
        if response.is_error() {
            tracing::warn!(?request, "remote refused to hand over its clipboard");
            return;
        }
        let Some(request) = request.filter(|request| !request.file_list) else {
            // Nothing correlates this response to a request of ours.
            tracing::warn!("unsolicited CLIPRDR format data response; ignoring");
            return;
        };
        let format = request.format;
        if is_image_format(format) {
            let bytes = response.data();
            if bytes.is_empty() || bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
                tracing::warn!(
                    len = bytes.len(),
                    "remote clipboard image out of range; dropping"
                );
                return;
            }
            let image = bytes.to_vec();
            self.with_state(|state| {
                if state.remote_copy_is_current(request) {
                    if state.clipboard_policy_epoch.is_shared() {
                        state.queue(ClipboardAction::InboundBoundImage {
                            stamp: request.stamp(),
                            image,
                        });
                    } else {
                        state.queue(ClipboardAction::InboundImage(image));
                    }
                }
            });
            return;
        }
        let decoded = if format == ClipboardFormatId::CF_UNICODETEXT {
            response.to_unicode_string()
        } else {
            response.to_string()
        };
        match decoded {
            Ok(text) if text.len() > MAX_CLIPBOARD_BYTES => {
                tracing::warn!(
                    len = text.len(),
                    "remote clipboard text exceeds the transfer cap; dropping"
                );
            }
            Ok(text) => {
                self.with_state(|state| {
                    if state.remote_copy_is_current(request) {
                        if state.clipboard_policy_epoch.is_shared() {
                            state.queue(ClipboardAction::InboundBoundText {
                                stamp: request.stamp(),
                                text,
                            });
                        } else {
                            state.queue(ClipboardAction::Inbound(text));
                        }
                    }
                });
            }
            Err(error) => tracing::warn!("could not decode remote clipboard text: {error}"),
        }
    }

    /// The remote is pasting files we offered and wants either a file size or a
    /// byte range from one.
    fn intercept_file_contents_request(&mut self, request: &FileContentsRequest) -> bool {
        self.on_file_contents_request(request.clone());
        true
    }

    fn on_malformed_file_contents_request(&mut self, stream_id: Option<u32>) -> bool {
        self.with_state(|state| {
            let Some(stream_id) = stream_id else {
                state.suspend_local_file_direction();
                return;
            };
            if state.local_file_direction == LocalFileDirection::Sealed {
                state.refuse_closed_file_stream(stream_id);
                return;
            }
            if state.reserve_local_file_stream(stream_id) {
                state.queue(ClipboardAction::SubmitFileContents {
                    generation: None,
                    revision: None,
                    stream_id,
                    data: None,
                });
            }
        });
        true
    }

    fn on_malformed_file_lock(&mut self) -> bool {
        self.with_state(|state| state.suspend_local_file_direction());
        true
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let stream_id = request.stream_id;
        if self.with_state(|state| {
            if state.local_file_direction == LocalFileDirection::Sealed {
                state.refuse_closed_file_stream(stream_id);
                return true;
            }
            if !state.reserve_local_file_stream(stream_id) {
                return true;
            }
            if state.local_file_direction != LocalFileDirection::Sealed
                && state.files_negotiated
                && state.ready
            {
                return false;
            }
            state.queue(ClipboardAction::SubmitFileContents {
                generation: None,
                revision: None,
                stream_id,
                data: None,
            });
            true
        }) != Some(false)
        {
            return;
        }
        // `lindex` is signed on the wire and negative is invalid. Upstream
        // rejects it during decode, but the conversion has to be total anyway.
        let Ok(index) = u32::try_from(request.index) else {
            tracing::warn!(
                stream_id,
                index = request.index,
                "negative file index; refusing"
            );
            self.refuse(stream_id);
            return;
        };

        self.with_state(|state| {
            let current_snapshot = FileSnapshot {
                channel_epoch: state.channel_epoch,
                generation: state.local_file_generation,
                revision: state.local_file_revision,
            };
            if request
                .data_id
                .is_some_and(|id| state.local_file_locks.get(&id) != Some(&current_snapshot))
            {
                state.suspend_local_file_direction();
                return;
            }
            let Some(file) = state.local_file(index).cloned() else {
                tracing::warn!(stream_id, index, "request for a file we never offered");
                state.queue(ClipboardAction::SubmitFileContents {
                    generation: None,
                    revision: None,
                    stream_id,
                    data: None,
                });
                return;
            };

            // A SIZE request is answered from metadata already in hand — no round
            // trip to the app and no disk read. Explorer issues one per file
            // before reading a single byte, so this is most of the traffic.
            if request.flags.contains(FileContentsFlags::SIZE) {
                state.queue(ClipboardAction::SubmitFileContents {
                    generation: Some(state.local_file_generation),
                    revision: Some(state.local_file_revision),
                    stream_id,
                    data: Some(file.size.to_le_bytes().to_vec()),
                });
                return;
            }

            if file.is_dir {
                tracing::warn!(stream_id, index, "byte range requested from a directory");
                state.queue(ClipboardAction::SubmitFileContents {
                    generation: None,
                    revision: None,
                    stream_id,
                    data: None,
                });
                return;
            }

            // Clamped rather than honoured: `cbRequested` is remote-controlled,
            // and an outsized value would have the app allocate it and then try
            // to frame it down the same pipe the desktop is drawn through.
            let length = request.requested_size.min(adit_rdp_proto::FILE_CHUNK_BYTES);
            // The request is already reserved in local_file_streams.
            if state.pending_local_reads.len() >= MAX_QUEUED_ACTIONS
                || state
                    .pending_local_reads
                    .iter()
                    .any(|read| read.stream_id == stream_id)
            {
                state.suspend_local_file_direction();
                return;
            }
            let request_id = ClipboardFileRequestId {
                channel_epoch: state.channel_epoch,
                snapshot_revision: state.local_file_revision,
                generation: state.local_file_generation,
                stream_id,
            };
            state.pending_local_reads.insert(request_id);
            state.queue(ClipboardAction::NeedFileContents {
                channel_epoch: request_id.channel_epoch,
                snapshot_revision: request_id.snapshot_revision,
                generation: state.local_file_generation,
                stream_id,
                index,
                offset: request.position,
                length,
            });
        });
    }

    /// Bytes (or a size) arriving for a local paste we started.
    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let wire_id = response.stream_id();
        let data = (!response.is_error()
            && response.data().len() <= adit_rdp_proto::FILE_CHUNK_BYTES as usize)
            .then(|| response.data().to_vec());
        self.with_state(|state| {
            if let Some(reply) = state.remote_files.respond(wire_id, data) {
                state.queue(ClipboardAction::InboundFileContents(reply));
            }
        });
    }

    /// Descriptors for a file list the remote copied.
    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        if !files.is_empty() && clip_data_id.is_none() {
            tracing::warn!("remote file clipboard has no stable lock; refusing file offer");
        }
        let listed = validated_remote_files(files).unwrap_or_else(|reason| {
            tracing::warn!(reason, "remote file descriptors refused");
            Vec::new()
        });
        self.with_state(|state| {
            let had_request = state.active_remote_copy.is_some();
            let completed = state.finish_remote_copy();
            let policy_epoch = match completed {
                Some(request) if request.file_list => request.policy_epoch,
                // Unit fixtures can inject a descriptor directly; live peers
                // must have a matching original FormatList request.
                None if !had_request && !state.clipboard_policy_epoch.is_shared() => {
                    state.clipboard_policy_epoch.epoch()
                }
                _ => return,
            };
            let expects_files = state.remote_files.expects_files();
            state.revoke_remote_files(expects_files);
            let accepted =
                state.files_negotiated && state.remote_files.install(listed.clone(), clip_data_id);
            state.queue(ClipboardAction::InboundFiles {
                policy_epoch,
                generation: state.remote_files.generation(),
                files: if accepted { listed } else { Vec::new() },
            });
        });
    }

    fn allow_incoming_file_lock(&mut self, data_id: &LockDataId) -> bool {
        self.with_state(|state| {
            if state.local_file_suspended
                || state.local_file_direction == LocalFileDirection::Sealed
            {
                return false;
            }
            if state.local_files.is_empty()
                && state.local_file_direction == LocalFileDirection::Fresh
            {
                // A lock on an empty clipboard has no snapshot to bind. In
                // particular, IronRDP may still cache an unexposed old list
                // while its replacement advertisement is waiting to be sent.
                return false;
            }
            let snapshot = FileSnapshot {
                channel_epoch: state.channel_epoch,
                generation: state.local_file_generation,
                revision: state.local_file_revision,
            };
            if !state.ready
                || !state.files_negotiated
                || snapshot.generation == 0
                || state.local_files.is_empty()
                || state.cached_file_snapshot != Some(snapshot)
                || state.local_file_direction != LocalFileDirection::Exposed(snapshot)
                || (state.local_file_locks.len() >= MAX_LOCAL_FILE_LOCKS
                    && !state.local_file_locks.contains_key(&data_id.0))
            {
                state.suspend_local_file_direction();
                return false;
            }
            // Reserve the identity while the state is locked. If the offer is
            // revoked before IronRDP finishes its callback, on_lock must never
            // attach this ID to whatever snapshot replaced it.
            state.local_file_locks.insert(data_id.0, snapshot);
            true
        }) == Some(true)
    }

    fn on_incoming_file_lock_rejected(&mut self, _data_id: &LockDataId) {
        self.with_state(|state| state.suspend_local_file_direction());
    }

    fn on_local_file_snapshot_limit(&mut self) {
        self.with_state(|state| state.suspend_local_file_direction());
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        self.with_state(|state| {
            if state.local_file_suspended
                || state.local_file_direction == LocalFileDirection::Sealed
            {
                return;
            }
            let snapshot = FileSnapshot {
                channel_epoch: state.channel_epoch,
                generation: state.local_file_generation,
                revision: state.local_file_revision,
            };
            if state.local_file_locks.get(&data_id.0) != Some(&snapshot)
                || state.cached_file_snapshot != Some(snapshot)
                || state.local_file_direction != LocalFileDirection::Exposed(snapshot)
            {
                state.suspend_local_file_direction();
            }
        });
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.with_state(|state| {
            state.local_file_locks.remove(&data_id.0);
        });
    }
}

/// Build the `FormatDataResponse` that answers a remote paste. Split out so the
/// encoding choice (UTF-16 for `CF_UNICODETEXT`, 8-bit for the legacy formats)
/// is exercised by tests without a live session.
fn format_data_response(format: ClipboardFormatId, text: Option<&str>) -> OwnedFormatDataResponse {
    match text {
        Some(text) if format == ClipboardFormatId::CF_UNICODETEXT => {
            OwnedFormatDataResponse::new_unicode_string(text)
        }
        Some(text) if is_text_format(format) => OwnedFormatDataResponse::new_string(text),
        _ => OwnedFormatDataResponse::new_error(),
    }
}

/// Split the actions addressed to the app off from the ones addressed to the
/// wire, handing the latter back untouched.
fn for_app(action: ClipboardAction) -> Result<HostMsg, ClipboardAction> {
    match action {
        ClipboardAction::Inbound(text) => Ok(HostMsg::ClipboardText(text)),
        ClipboardAction::InboundImage(image) => Ok(HostMsg::ClipboardImage(image)),
        ClipboardAction::InboundBoundText { stamp, text } => {
            Ok(HostMsg::ClipboardTextForRevision { stamp, text })
        }
        ClipboardAction::InboundBoundImage { stamp, image } => {
            Ok(HostMsg::ClipboardImageForRevision { stamp, image })
        }
        ClipboardAction::RemoteCopy(stamp) => Ok(HostMsg::ClipboardRemoteCopy(stamp)),
        ClipboardAction::Notice(message) => Ok(HostMsg::ClipboardNotice(message)),
        ClipboardAction::InboundFiles {
            policy_epoch,
            generation,
            files,
        } => Ok(HostMsg::ClipboardFilesForPolicy {
            policy_epoch,
            generation,
            files,
        }),
        ClipboardAction::NeedFileContents {
            channel_epoch,
            snapshot_revision,
            generation,
            stream_id,
            index,
            offset,
            length,
        } => Ok(HostMsg::FileContentsNeeded {
            channel_epoch,
            snapshot_revision,
            generation,
            stream_id,
            index,
            offset,
            length,
        }),
        ClipboardAction::InboundFileContents(Reply {
            generation,
            stream_id,
            data,
        }) => Ok(HostMsg::FileContents {
            generation,
            stream_id,
            data,
        }),
        other => Err(other),
    }
}

/// Drain the queued clipboard work: turn it into CLIPRDR frames for the wire and
/// inbound remote text into a [`HostMsg`] for the app.
///
/// Clipboard failures are kept separate from failures of the shared app pipe.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ClipboardPumpError {
    #[error("RDP 剪贴板动作队列已满")]
    ActionsOverloaded,
    #[error("RDP 剪贴板请求或回复队列已满，会话已关闭")]
    HostOverloaded,
    #[error("RDP 剪贴板状态不可用")]
    StateUnavailable,
    #[error("RDP 控制通道已关闭")]
    AppClosed,
    #[error("无法发送 RDP 剪贴板回复：{0}")]
    ReplyFailed(String),
}

/// Deliver app-bound events without consuming the new channel's wire actions.
/// This also works while TCP, TLS or the CLIPRDR handshake is still pending.
pub(crate) async fn flush_app_actions(
    state: &SharedClipboard,
    host: &HostSender,
) -> Result<(), ClipboardPumpError> {
    flush_local_file_direction_notices(state, host);
    let (ready, messages) = {
        let Ok(mut guard) = state.lock() else {
            return Ok(());
        };
        if guard.actions_overloaded {
            return Ok(());
        }
        guard.check_remote_timeout();
        let mut messages = Vec::new();
        let mut wire_actions = VecDeque::new();
        for action in guard.take_actions() {
            match for_app(action) {
                Ok(message) => messages.push(message),
                Err(action) => wire_actions.push_back(action),
            }
        }
        guard.actions = wire_actions;
        (guard.clipboard_ready_pending.take(), messages)
    };
    if let Some(channel_epoch) = ready {
        host.send(HostMsg::ClipboardChannelReady { channel_epoch })
            .map_err(|error| match error {
                HostSendError::Closed => ClipboardPumpError::AppClosed,
                HostSendError::Overloaded => ClipboardPumpError::HostOverloaded,
            })?;
        if let Ok(mut guard) = state.lock() {
            if guard.channel_epoch == channel_epoch {
                guard.clipboard_ready_announced = Some(channel_epoch);
            }
        }
    }
    for message in messages {
        match send_app_clipboard_message(state, host, message).await {
            Err(ClipboardPumpError::StateUnavailable) => return Ok(()),
            result => result?,
        }
    }
    Ok(())
}

async fn send_app_clipboard_message(
    state: &SharedClipboard,
    host: &HostSender,
    message: HostMsg,
) -> Result<(), ClipboardPumpError> {
    if matches!(message, HostMsg::FileContentsNeeded { .. }) {
        if state
            .lock()
            .map_err(|_| ClipboardPumpError::StateUnavailable)?
            .local_file_suspended
        {
            return Ok(());
        }
        return match host.send(message) {
            Ok(()) => Ok(()),
            Err(HostSendError::Closed) => Err(ClipboardPumpError::AppClosed),
            Err(HostSendError::Overloaded) => {
                suspend_local_file_direction(state);
                Ok(())
            }
        };
    }
    match host.send_when_ready(message).await {
        Ok(()) => Ok(()),
        Err(HostSendError::Closed) => Err(ClipboardPumpError::AppClosed),
        Err(HostSendError::Overloaded) => {
            tracing::warn!("clipboard message could not be delivered to the app");
            Ok(())
        }
    }
}

#[cfg(test)]
async fn pump(
    state: &SharedClipboard,
    active_stage: &mut ActiveStage,
    host_tx: &HostSender,
) -> Result<Vec<Vec<u8>>, ClipboardPumpError> {
    Ok(pump_with_pending_input(state, active_stage, host_tx, None)
        .await?
        .into_iter()
        .map(|frame| frame.bytes)
        .collect())
}

#[derive(Debug)]
pub(crate) struct ClipboardWireFrame {
    pub(crate) bytes: Vec<u8>,
    binding: Option<FrameBinding>,
    safe_failure: bool,
    local_file_reply: bool,
    local_file_stream: Option<(u64, u32)>,
}

impl ClipboardWireFrame {
    pub(crate) fn is_safe_failure(&self) -> bool {
        self.safe_failure
    }
    pub(crate) fn needs_offer_barrier(&self) -> bool {
        !self.safe_failure
    }

    pub(crate) fn needs_suspension_barrier(&self) -> bool {
        self.local_file_reply
    }
}

pub(crate) fn note_wire_frame_finished(frame: &ClipboardWireFrame, state: &SharedClipboard) {
    if let Some(FrameBinding::ClosedFileFailure {
        channel_epoch,
        stream_id,
    }) = &frame.binding
    {
        let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
        if guard.channel_epoch == *channel_epoch {
            if let Some(pending) = guard.closed_file_streams.get_mut(stream_id) {
                *pending = false;
            }
        }
    }
    if let Some((epoch, stream_id)) = frame.local_file_stream {
        let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
        if guard.channel_epoch == epoch {
            guard.local_file_streams.remove(&stream_id);
        }
    }
}

#[derive(Debug)]
enum FrameBinding {
    Descriptor(FileSnapshot),
    Contents {
        generation: u64,
        revision: u64,
        stream_id: u32,
    },
    Offer(FileSnapshot),
    Content {
        revision: Option<u64>,
    },
    ClosedFileFailure {
        channel_epoch: u64,
        stream_id: u32,
    },
}

/// Called by the session writer after its first successful write of this frame,
/// while it still holds the input ordering lock. A buffered TLS write counts as
/// exposure because accepted bytes cannot be withdrawn.
pub(crate) fn note_wire_frame_started(frame: &ClipboardWireFrame, state: &SharedClipboard) {
    let Some(FrameBinding::Offer(snapshot)) = &frame.binding else {
        return;
    };
    let snapshot = *snapshot;
    let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
    if snapshot.channel_epoch != guard.channel_epoch {
        return;
    }
    if snapshot.generation == guard.local_file_generation
        && snapshot.revision == guard.local_file_revision
        && guard.local_file_direction != LocalFileDirection::Sealed
    {
        guard.local_file_direction = LocalFileDirection::Exposed(snapshot);
    } else {
        guard.seal_local_file_direction();
    }
}

/// Peek a nonfatal notification for the sealed channel. The session must send
/// it as a notice, not as the fatal `HostMsg::Error`, and acknowledge it only
/// after the notification has been accepted for delivery.
pub(crate) fn local_file_direction_notice(state: &SharedClipboard) -> Option<(u64, &'static str)> {
    let guard = state.lock().unwrap_or_else(|error| error.into_inner());
    guard.local_file_notice_pending.front().copied().map(|epoch| (epoch, "本地文件发送至远端已暂时禁用；桌面、文字、图片和远端文件接收仍可使用。已发送或已被传输层接收的数据仍可能到达。"))
}

pub(crate) fn acknowledge_local_file_direction_notice(state: &SharedClipboard, channel_epoch: u64) {
    let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
    if guard.local_file_notice_pending.front() == Some(&channel_epoch) {
        guard.local_file_notice_pending.pop_front();
    }
}

/// Enqueue notices independently of the data budget, without closing the desktop
/// if the app pipe is unavailable. An unsuccessful enqueue stays pending.
pub(crate) fn flush_local_file_direction_notices(state: &SharedClipboard, host: &HostSender) {
    let pending_ack = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .local_file_suspension_ack;
    if let Some(channel_epoch) = pending_ack {
        if host
            .send(HostMsg::LocalFileDirectionSuspended { channel_epoch })
            .is_ok()
        {
            let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
            if guard.local_file_suspension_ack == Some(channel_epoch) {
                guard.local_file_suspension_ack = None;
            }
        }
    }
    while let Some((epoch, message)) = local_file_direction_notice(state) {
        match host.send(HostMsg::ClipboardFileDirectionUnavailable {
            channel_epoch: epoch,
            message: message.to_owned(),
        }) {
            Ok(()) => acknowledge_local_file_direction_notice(state, epoch),
            Err(error) => {
                tracing::warn!(%error, "本地文件共享提示尚未送达");
                break;
            }
        }
    }
    let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
    if guard.local_file_suspension_ack.is_none() && guard.local_file_notice_pending.is_empty() {
        if let Some(channel_epoch) = guard.local_file_ready_pending {
            if host
                .send(HostMsg::ClipboardLocalFileDirectionReady { channel_epoch })
                .is_ok()
            {
                guard.local_file_ready_pending = None;
                guard.local_file_ready_announced = Some(channel_epoch);
            }
        }
    }
}

/// Recheck a not-yet-written frame after queued clipboard offers have been
/// applied. A refused read still needs a wire error so the peer can finish it.
pub(crate) fn revalidate_wire_frame(
    frame: &mut ClipboardWireFrame,
    state: &SharedClipboard,
    stage: &mut ActiveStage,
) -> Result<bool, ClipboardPumpError> {
    if state
        .lock()
        .map_err(|_| ClipboardPumpError::StateUnavailable)?
        .actions_overloaded
    {
        return Ok(false);
    }
    if let Some(FrameBinding::ClosedFileFailure {
        channel_epoch,
        stream_id,
    }) = &frame.binding
    {
        let guard = state
            .lock()
            .map_err(|_| ClipboardPumpError::StateUnavailable)?;
        return Ok(guard.channel_epoch == *channel_epoch
            && guard.closed_file_streams.get(stream_id) == Some(&true));
    }
    if frame.local_file_reply
        && state
            .lock()
            .map_err(|_| ClipboardPumpError::StateUnavailable)?
            .local_file_suspended
    {
        return Ok(false);
    }
    let Some(binding) = &frame.binding else {
        return Ok(true);
    };
    let (
        current_epoch,
        current_generation,
        current_revision,
        current_content_revision,
        content_current,
        sealed,
    ) = {
        let guard = state
            .lock()
            .map_err(|_| ClipboardPumpError::StateUnavailable)?;
        (
            guard.channel_epoch,
            guard.local_file_generation,
            guard.local_file_revision,
            guard.local_content_revision,
            guard.local_content_is_current(),
            guard.local_file_direction == LocalFileDirection::Sealed,
        )
    };
    if let FrameBinding::Descriptor(snapshot) = *binding {
        if !sealed
            && snapshot.channel_epoch == current_epoch
            && snapshot.generation != 0
            && snapshot.generation == current_generation
            && snapshot.revision == current_revision
        {
            return Ok(true);
        }
        let client = stage
            .get_svc_processor_mut::<CliprdrClient>()
            .ok_or_else(|| ClipboardPumpError::ReplyFailed("CLIPRDR channel is not open".into()))?;
        let messages = client
            .submit_format_data(OwnedFormatDataResponse::new_error())
            .map_err(|error| ClipboardPumpError::ReplyFailed(error.to_string()))?;
        frame.bytes = stage
            .process_svc_processor_messages(messages)
            .map_err(|error| ClipboardPumpError::ReplyFailed(error.to_string()))?;
        frame.binding = None;
        frame.safe_failure = true;
        return Ok(true);
    }
    if let FrameBinding::Content { revision } = *binding {
        if content_current && revision == Some(current_content_revision) {
            return Ok(true);
        }
        let client = stage
            .get_svc_processor_mut::<CliprdrClient>()
            .ok_or_else(|| ClipboardPumpError::ReplyFailed("CLIPRDR channel is not open".into()))?;
        let messages = client
            .submit_format_data(OwnedFormatDataResponse::new_error())
            .map_err(|error| ClipboardPumpError::ReplyFailed(error.to_string()))?;
        frame.bytes = stage
            .process_svc_processor_messages(messages)
            .map_err(|error| ClipboardPumpError::ReplyFailed(error.to_string()))?;
        frame.binding = None;
        frame.safe_failure = true;
        return Ok(true);
    }
    let (generation, revision, stream_id) = match *binding {
        FrameBinding::Contents {
            generation,
            revision,
            stream_id,
        } => (generation, revision, Some(stream_id)),
        FrameBinding::Offer(snapshot) => {
            if snapshot.channel_epoch != current_epoch {
                return Ok(false);
            }
            (snapshot.generation, snapshot.revision, None)
        }
        FrameBinding::Content { .. }
        | FrameBinding::Descriptor(_)
        | FrameBinding::ClosedFileFailure { .. } => unreachable!(),
    };
    if !sealed
        && generation != 0
        && generation == current_generation
        && revision == current_revision
    {
        return Ok(true);
    }
    let Some(stream_id) = stream_id else {
        return Ok(false);
    };
    let client = stage
        .get_svc_processor_mut::<CliprdrClient>()
        .ok_or_else(|| ClipboardPumpError::ReplyFailed("CLIPRDR channel is not open".into()))?;
    let messages = client
        .submit_file_contents(FileContentsResponse::new_error(stream_id))
        .map_err(|error| ClipboardPumpError::ReplyFailed(error.to_string()))?;
    frame.bytes = stage
        .process_svc_processor_messages(messages)
        .map_err(|error| ClipboardPumpError::ReplyFailed(error.to_string()))?;
    frame.binding = None;
    frame.safe_failure = true;
    Ok(true)
}

fn abandon_wire_action(state: &SharedClipboard, action: &ClipboardAction) {
    if let ClipboardAction::RequestFileContents(read) = action {
        fail_remote_read(state, read.wire_id);
    } else if let ClipboardAction::RequestText(format) = action {
        let Ok(mut guard) = state.lock() else { return };
        if guard
            .active_remote_copy
            .is_some_and(|request| request.format == *format)
        {
            guard.finish_remote_copy();
        }
    } else if action.is_local_file_action() {
        suspend_local_file_direction(state);
    }
}

pub(crate) async fn pump_with_pending_input(
    state: &SharedClipboard,
    active_stage: &mut ActiveStage,
    host_tx: &HostSender,
    input: Option<&crate::host::SessionInput>,
) -> Result<Vec<ClipboardWireFrame>, ClipboardPumpError> {
    let mut frames = Vec::new();
    loop {
        let actions = {
            let mut guard = state
                .lock()
                .map_err(|_| ClipboardPumpError::StateUnavailable)?;
            guard.check_remote_timeout();
            if guard.actions_overloaded {
                return Err(ClipboardPumpError::ActionsOverloaded);
            }
            if guard.actions.is_empty() && guard.rejected_file_advertise_pending {
                guard.rejected_file_advertise_pending = false;
                guard.queue_advertise();
            }
            guard.take_actions()
        };
        if actions.is_empty() {
            break;
        }
        let mut actions = actions.into_iter();
        while let Some(action) = actions.next() {
            {
                let guard = state
                    .lock()
                    .map_err(|_| ClipboardPumpError::StateUnavailable)?;
                match &action {
                    ClipboardAction::RefuseClosedFileContents {
                        channel_epoch,
                        stream_id,
                    } if guard.channel_epoch != *channel_epoch
                        || guard.closed_file_streams.get(stream_id) != Some(&true) =>
                    {
                        continue;
                    }
                    ClipboardAction::RefuseClosedFileContents { .. } => {}
                    _ if action.is_local_file_action() && guard.local_file_suspended => continue,
                    _ => {}
                }
            }
            let ordering = input.map(|input| {
                input
                    .ordering
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
            });
            let safe_failure = matches!(
                &action,
                ClipboardAction::SubmitFileContents { data: None, .. }
                    | ClipboardAction::RefuseClosedFileContents { .. }
                    | ClipboardAction::InboundFileContents(Reply { data: None, .. })
                    | ClipboardAction::SubmitText { text: None, .. }
                    | ClipboardAction::SubmitImage { image: None, .. }
            ) || matches!(&action, ClipboardAction::SubmitFileDescriptor { response, .. } if response.is_error());
            if ordering.as_ref().is_some_and(|ordering| {
                (!safe_failure && ordering.pending_offers != 0)
                    || (action.is_local_file_action() && ordering.pending_suspensions != 0)
            }) {
                // A Ready callback can queue an old descriptor in the same
                // turn that a revocation reaches stdin. Let the session apply
                // its queued commands before committing another wire action.
                let mut guard = state
                    .lock()
                    .map_err(|_| ClipboardPumpError::StateUnavailable)?;
                let mut deferred = VecDeque::from([action]);
                deferred.extend(actions);
                deferred.append(&mut guard.actions);
                guard.actions = deferred;
                return Ok(frames);
            }
            // Four of the actions are addressed to the app rather than the wire.
            // `for_app` hands back the ones that are not, because matching by value
            // would otherwise move the action away from the CLIPRDR branch below.
            let action = match for_app(action) {
                Ok(message) => {
                    drop(ordering);
                    send_app_clipboard_message(state, host_tx, message).await?;
                    continue;
                }
                Err(action) => action,
            };

            let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() else {
                tracing::warn!("CLIPRDR channel is not open; dropping clipboard action");
                abandon_wire_action(state, &action);
                continue;
            };
            let messages = match &action {
                ClipboardAction::Advertise(ids) => {
                    let formats: Vec<ClipboardFormat> =
                        ids.iter().copied().map(ClipboardFormat::new).collect();
                    state
                        .lock()
                        .map_err(|_| ClipboardPumpError::StateUnavailable)?
                        .cached_file_snapshot = None;
                    cliprdr.initiate_copy(&formats)
                }
                ClipboardAction::RequestText(format) => cliprdr.initiate_paste(*format),
                ClipboardAction::SubmitText { format, text, .. } => {
                    cliprdr.submit_format_data(format_data_response(*format, text.as_deref()))
                }
                ClipboardAction::SubmitImage { image, .. } => {
                    cliprdr.submit_format_data(match image {
                        // Passed through byte for byte: both ends already agree on
                        // the DIB layout, so touching it could only break it.
                        Some(bytes) => OwnedFormatDataResponse::new_data(bytes.clone()),
                        None => OwnedFormatDataResponse::new_error(),
                    })
                }
                ClipboardAction::AdvertiseFiles(files, snapshot) => {
                    let result =
                        cliprdr.initiate_file_copy(files.iter().map(to_descriptor).collect());
                    state
                        .lock()
                        .map_err(|_| ClipboardPumpError::StateUnavailable)?
                        .cached_file_snapshot = result.as_ref().ok().map(|_| *snapshot);
                    result
                }
                ClipboardAction::SubmitFileDescriptor { response, .. } => {
                    cliprdr.submit_format_data(response.clone())
                }
                ClipboardAction::SubmitFileContents {
                    stream_id, data, ..
                } => cliprdr.submit_file_contents(match data {
                    Some(bytes) => {
                        FileContentsResponse::new_data_response(*stream_id, bytes.clone())
                    }
                    None => FileContentsResponse::new_error(*stream_id),
                }),
                ClipboardAction::RefuseClosedFileContents { stream_id, .. } => {
                    cliprdr.submit_file_contents(FileContentsResponse::new_error(*stream_id))
                }
                ClipboardAction::RequestFileContents(read) => {
                    let Some(request) = prepare_remote_read(state, read) else {
                        continue;
                    };
                    cliprdr.request_file_contents(request)
                }
                // Handled above; the borrow of `cliprdr` is what forces this shape.
                ClipboardAction::Inbound(_)
                | ClipboardAction::InboundImage(_)
                | ClipboardAction::InboundBoundText { .. }
                | ClipboardAction::InboundBoundImage { .. }
                | ClipboardAction::RemoteCopy(_)
                | ClipboardAction::Notice(_)
                | ClipboardAction::InboundFiles { .. }
                | ClipboardAction::NeedFileContents { .. }
                | ClipboardAction::InboundFileContents(_) => {
                    unreachable!("app-addressed actions are handled above")
                }
            };
            let messages = match messages {
                Ok(messages) => messages,
                Err(error) => {
                    tracing::warn!("could not build clipboard PDU: {error}");
                    abandon_wire_action(state, &action);
                    continue;
                }
            };
            let messages = if matches!(&action, ClipboardAction::AdvertiseFiles(..)) {
                let mut messages = Vec::<ironrdp_svc::SvcMessage>::from(messages);
                let Some(offer) = messages.pop() else {
                    tracing::warn!("file offer did not produce a clipboard format list");
                    continue;
                };
                // initiate_file_copy clears the outgoing locks immediately and
                // returns their UnlockData PDUs before its final FormatList.
                // Only that FormatList may be discarded when the offer expires.
                if !messages.is_empty() {
                    match active_stage
                        .process_svc_processor_messages::<CliprdrClient>(messages.into())
                    {
                        Ok(bytes) => frames.push(ClipboardWireFrame {
                            bytes,
                            binding: None,
                            safe_failure: false,
                            local_file_reply: false,
                            local_file_stream: None,
                        }),
                        Err(error) => {
                            tracing::warn!("could not encode clipboard unlocks: {error}");
                            continue;
                        }
                    }
                }
                vec![offer].into()
            } else {
                messages
            };
            match active_stage.process_svc_processor_messages(messages) {
                Ok(bytes) => {
                    let binding = match &action {
                        ClipboardAction::SubmitFileContents {
                            generation,
                            revision,
                            stream_id,
                            data: Some(_),
                        } => Some(FrameBinding::Contents {
                            generation: generation.unwrap_or(0),
                            revision: revision.unwrap_or(0),
                            stream_id: *stream_id,
                        }),
                        ClipboardAction::AdvertiseFiles(_, snapshot) => {
                            Some(FrameBinding::Offer(*snapshot))
                        }
                        ClipboardAction::SubmitFileDescriptor {
                            snapshot: Some(snapshot),
                            response,
                        } if !response.is_error() => Some(FrameBinding::Descriptor(*snapshot)),
                        ClipboardAction::SubmitText {
                            revision,
                            text: Some(_),
                            ..
                        }
                        | ClipboardAction::SubmitImage {
                            revision,
                            image: Some(_),
                            ..
                        } => Some(FrameBinding::Content {
                            revision: *revision,
                        }),
                        ClipboardAction::RefuseClosedFileContents {
                            channel_epoch,
                            stream_id,
                        } => Some(FrameBinding::ClosedFileFailure {
                            channel_epoch: *channel_epoch,
                            stream_id: *stream_id,
                        }),
                        _ => None,
                    };
                    let safe_failure = matches!(
                        action,
                        ClipboardAction::SubmitFileContents { data: None, .. }
                            | ClipboardAction::RefuseClosedFileContents { .. }
                            | ClipboardAction::SubmitText { text: None, .. }
                            | ClipboardAction::SubmitImage { image: None, .. }
                    ) || matches!(&action, ClipboardAction::SubmitFileDescriptor { response, .. } if response.is_error());
                    frames.push(ClipboardWireFrame {
                        bytes,
                        binding,
                        safe_failure,
                        local_file_reply: matches!(
                            action,
                            ClipboardAction::SubmitFileContents { .. }
                                | ClipboardAction::SubmitFileDescriptor { .. }
                                | ClipboardAction::RefuseClosedFileContents { .. }
                        ),
                        local_file_stream: match action {
                            ClipboardAction::SubmitFileContents { stream_id, .. } => Some((
                                state
                                    .lock()
                                    .map_err(|_| ClipboardPumpError::StateUnavailable)?
                                    .channel_epoch,
                                stream_id,
                            )),
                            _ => None,
                        },
                    });
                }
                Err(error) => {
                    tracing::warn!("could not encode clipboard PDU: {error}");
                    abandon_wire_action(state, &action);
                }
            }
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, Encode as _, WriteCursor};

    use super::*;

    #[tokio::test]
    async fn shared_content_observation_and_policy_ack_revoke_unsent_text_and_image() {
        use crate::session::{
            apply_clipboard_policy_barrier, commit_clipboard_frame, ClipboardCommit,
        };
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use tokio::io::AsyncReadExt;

        for image in [false, true] {
            let (host, receiver) = crate::host::host_channel();
            let mapping = Arc::new(adit_rdp_proto::SharedClipboardPolicy::create(0, true).unwrap());
            let source = host.clipboard_policy_epoch();
            source.bind_test_mapping(Arc::clone(&mapping));
            let (state, mut stage) = descriptor_stage(
                vec![ClipFile {
                    name: "fixture.txt".into(),
                    size: 1,
                    is_dir: false,
                }],
                &host,
            )
            .await;
            state.lock().unwrap().clipboard_policy_epoch = source;
            let stamp = ClipboardStamp {
                policy_epoch: 0,
                content_revision: 0,
            };
            let format = if image {
                offer_bound_image(&state, stamp, vec![19, 37, 71, 89]);
                ClipboardFormatId::CF_DIB
            } else {
                offer_bound_text(&state, stamp, "synthetic secret".into());
                ClipboardFormatId::CF_UNICODETEXT
            };
            let _ = pump_with_pending_input(&state, &mut stage, &host, None)
                .await
                .unwrap();
            process_format_request(&mut stage, format.value());
            let mut frames = VecDeque::from(
                pump_with_pending_input(&state, &mut stage, &host, None)
                    .await
                    .unwrap(),
            );
            assert_eq!(frames.len(), 1);

            // The app has observed a replacement, but its IPC marker is paused.
            mapping.store_full(0, true, 1).unwrap();
            let mut before_ack = frames.pop_front().unwrap();
            let (mut wire, mut reader) = tokio::io::duplex(4096);
            assert_eq!(
                commit_clipboard_frame(&mut wire, &mut before_ack, &state, &mut stage, None)
                    .await
                    .unwrap(),
                ClipboardCommit::Written
            );
            drop(wire);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            let (pdu, _) = decode_one_clipboard_message(&bytes);
            assert!(
                matches!(decode::<ClipboardPdu<'_>>(&pdu).unwrap(), ClipboardPdu::FormatDataResponse(response) if response.is_error())
            );

            // A disabled policy cannot be acknowledged while stale cached data
            // or an encoded, not-yet-started response is still usable.
            mapping.store_full(0, true, 2).unwrap();
            let new_stamp = ClipboardStamp {
                policy_epoch: 0,
                content_revision: 2,
            };
            if image {
                offer_bound_image(&state, new_stamp, vec![19, 37, 71, 89]);
            } else {
                offer_bound_text(&state, new_stamp, "synthetic secret".into());
            }
            let _ = pump_with_pending_input(&state, &mut stage, &host, None)
                .await
                .unwrap();
            process_format_request(&mut stage, format.value());
            let mut pending = VecDeque::from(
                pump_with_pending_input(&state, &mut stage, &host, None)
                    .await
                    .unwrap(),
            );
            mapping.store_full(1, false, 2).unwrap();
            apply_clipboard_policy_barrier(&state, &mut pending, &mut stage, &host).unwrap();
            assert!(state.lock().unwrap().local_text.is_none());
            assert!(state.lock().unwrap().local_image.is_none());
            assert!(pending.iter().all(ClipboardWireFrame::is_safe_failure));
            assert!(receiver.try_iter().any(|queued| matches!(
                queued.message,
                HostMsg::ClipboardPolicyApplied {
                    policy_epoch: 1,
                    enabled: false
                }
            )));
        }
    }

    #[tokio::test]
    async fn malformed_clipboard_svc_through_active_stage_does_not_end_other_channels() {
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::image::DecodedImage;
        use ironrdp_svc::SvcMessage;
        let (host, _receiver) = crate::host::host_channel();
        let (_state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "fixture.txt".into(),
                size: 1,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let mut packet = ironrdp_svc::server_encode_svc_messages(
            vec![SvcMessage::from(vec![0_u8; 8])],
            1004,
            1001,
        )
        .unwrap();
        let header = packet.len() - 8 - 8;
        packet[header..header + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        let action = ironrdp_pdu::Action::X224;
        let processed = stage.process(&mut image, action, &packet);
        assert!(
            processed.is_err(),
            "the real SVC decoder must reject the malformed length"
        );
        let outputs = crate::session::finish_active_process(
            processed,
            &crate::egfx::new_shared(),
            action,
            &packet,
        )
        .unwrap();
        assert!(outputs.is_empty());
        assert_video_and_other_svc_still_work(&mut stage);
    }

    #[tokio::test]
    async fn slow_partial_image_and_mouse_flood_preserve_desktop_input() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit, SessionCmd};
        use adit_rdp_proto::{InputEvent, MouseButton};
        use tokio::io::AsyncReadExt;
        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "fixture.txt".into(),
                size: 1,
                is_dir: false,
            }],
            &host,
        )
        .await;
        offer_local_image(&state, vec![0x6d; 256 * 1024]);
        let _ = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        process_format_request(&mut stage, ClipboardFormatId::CF_DIB.value());
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let expected = frames[0].bytes.len();
        let (sender, mut input) = crate::host::input_channel();
        let (mut wire, mut reader) = tokio::io::duplex(4096);
        {
            let committing =
                commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, Some(&input));
            tokio::pin!(committing);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(committing.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(sender.send(SessionCmd::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true
            })));
            for n in 0..1024 {
                assert!(sender.send(SessionCmd::Input(InputEvent::MouseMove { x: n, y: n })));
            }
            assert!(sender.send(SessionCmd::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false
            })));
            assert_eq!(input.commands.len(), 3);
            assert_eq!(*input.end.borrow(), None);
            let receive = async {
                let mut bytes = vec![0; expected];
                reader.read_exact(&mut bytes).await.unwrap();
                bytes
            };
            let (result, bytes) = tokio::join!(committing, receive);
            assert_eq!(result.unwrap(), ClipboardCommit::Written);
            assert_eq!(bytes.len(), expected);
        }
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::Input(InputEvent::MouseButton { pressed: true, .. })
        ));
        let SessionCmd::ReplaceableInput(slot) = input.commands.try_recv().unwrap().command else {
            panic!("missing latest move")
        };
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(InputEvent::MouseMove { x: 1023, y: 1023 })
        ));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::Input(InputEvent::MouseButton { pressed: false, .. })
        ));
        assert_video_and_other_svc_still_work(&mut stage);
    }

    // Frame-level tests start after the request has already reached the GUI.
    fn pending_test_read(
        state: &SharedClipboard,
        generation: u64,
        stream_id: u32,
    ) -> ClipboardFileRequestId {
        let mut guard = state.lock().unwrap();
        let request = ClipboardFileRequestId {
            channel_epoch: guard.channel_epoch,
            snapshot_revision: guard.local_file_revision,
            generation,
            stream_id,
        };
        assert!(guard.pending_local_reads.insert(request));
        request
    }

    fn requested_read(actions: Vec<ClipboardAction>) -> ClipboardFileRequestId {
        let message = actions
            .into_iter()
            .find_map(|action| for_app(action).ok())
            .unwrap();
        match message {
            HostMsg::FileContentsNeeded {
                channel_epoch,
                snapshot_revision,
                generation,
                stream_id,
                ..
            } => ClipboardFileRequestId {
                channel_epoch,
                snapshot_revision,
                generation,
                stream_id,
            },
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn redirected_channel_drops_late_and_unrequested_gui_replies() {
        let (mut old_backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        let read = || FileContentsRequest {
            stream_id: 41,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 4,
            data_id: None,
        };
        old_backend.on_file_contents_request(read());
        let old = requested_read(actions(&state));

        let mut new_backend = AditCliprdrBackend::new(Arc::clone(&state));
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        let _ = actions(&state);
        new_backend.on_file_contents_request(read());
        let new = requested_read(actions(&state));
        assert_ne!(old.channel_epoch, new.channel_epoch);
        assert_eq!(
            (old.generation, old.snapshot_revision, old.stream_id),
            (new.generation, new.snapshot_revision, new.stream_id)
        );

        for data in [Some(vec![9]), None] {
            submit_file_contents(&state, old, data);
            assert!(
                actions(&state).is_empty(),
                "old channel must not get any response on the new channel"
            );
        }
        submit_file_contents(
            &state,
            ClipboardFileRequestId {
                stream_id: 42,
                ..new
            },
            Some(vec![8]),
        );
        submit_file_contents(
            &state,
            ClipboardFileRequestId {
                snapshot_revision: new.snapshot_revision + 1,
                ..new
            },
            Some(vec![8]),
        );
        assert!(actions(&state).is_empty());
        assert!(state.lock().unwrap().pending_local_reads.contains(&new));

        submit_file_contents(&state, new, Some(vec![1, 2, 3, 4]));
        assert!(
            matches!(actions(&state).as_slice(), [ClipboardAction::SubmitFileContents {
            generation: Some(7), revision: Some(revision), stream_id: 41, data: Some(data),
        }] if *revision == new.snapshot_revision && data == &[1, 2, 3, 4])
        );
        submit_file_contents(&state, new, None);
        assert!(actions(&state).is_empty());
    }

    #[test]
    fn replaced_snapshot_refuses_its_still_pending_read_on_the_same_channel() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 4,
            is_dir: false,
        };
        offer_local_files(&state, 7, vec![file("old.bin")]);
        let _ = actions(&state);
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 41,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 4,
            data_id: None,
        });
        let old = requested_read(actions(&state));
        offer_local_files(&state, 7, vec![file("new.bin")]);
        let _ = actions(&state);
        submit_file_contents(&state, old, Some(vec![9]));
        assert!(matches!(
            actions(&state).as_slice(),
            [ClipboardAction::SubmitFileContents {
                stream_id: 41,
                data: None,
                ..
            }]
        ));
    }

    #[test]
    fn overlapping_stream_on_one_channel_isolates_without_guessing() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        let request = |flags| FileContentsRequest {
            stream_id: 41,
            index: 0,
            flags,
            position: 0,
            requested_size: 4,
            data_id: None,
        };
        backend.on_file_contents_request(request(FileContentsFlags::RANGE));
        let _ = requested_read(actions(&state));
        backend.on_file_contents_request(request(FileContentsFlags::SIZE));
        for _ in 0..10_000 {
            backend.on_file_contents_request(request(FileContentsFlags::RANGE));
        }
        let guard = state.lock().unwrap();
        assert!(guard.local_file_suspended);
        assert!(!guard.actions_overloaded);
        assert!(guard.pending_local_reads.is_empty());
        assert!(guard.local_file_streams.is_empty());
        assert!(guard
            .actions
            .iter()
            .all(|action| !action.is_local_file_action()));
    }

    fn active_stage_without_clipboard() -> ActiveStage {
        ironrdp_session::ActiveStageBuilder {
            static_channels: Default::default(),
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

    #[tokio::test]
    async fn file_refusal_flood_is_bounded_and_isolates_the_file_direction() {
        let (mut backend, state) = backend();
        for id in 0..=MAX_QUEUED_ACTIONS as u32 {
            backend.on_file_contents_request(FileContentsRequest {
                stream_id: id,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                requested_size: 1,
                data_id: None,
            });
        }
        {
            let mut guard = state.lock().unwrap();
            assert!(guard.actions.is_empty());
            assert!(guard.local_file_suspended);
            assert!(!guard.actions_overloaded);
            // A redirect must not silently re-enable the unsafe direction.
            guard.reset_for_new_channel();
            assert!(guard.local_file_suspended);
        }
        let (host, receiver) = crate::host::host_channel();
        flush_local_file_direction_notices(&state, &host);
        assert!(pump(&state, &mut active_stage_without_clipboard(), &host)
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn valid_file_request_flood_is_bounded_before_the_gui_queue() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        for stream_id in 0..=MAX_QUEUED_ACTIONS as u32 {
            backend.on_file_contents_request(FileContentsRequest {
                stream_id,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                requested_size: 1,
                data_id: None,
            });
        }
        {
            let guard = state.lock().unwrap();
            assert!(guard.local_file_suspended);
            assert!(!guard.actions_overloaded);
            assert!(guard.pending_local_reads.is_empty());
            assert!(guard
                .actions
                .iter()
                .all(|action| !action.is_local_file_action()));
        }
        let (host, receiver) = crate::host::host_channel();
        flush_local_file_direction_notices(&state, &host);
        assert!(pump(&state, &mut active_stage_without_clipboard(), &host)
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
    }

    #[test]
    fn action_byte_budget_counts_payloads_and_fails_closed() {
        let mut state = ClipboardState::default();
        let first = ClipboardAction::InboundImage(vec![1; 32]);
        let limit = first.queued_bytes() * 2 - 1;
        state.queue_with_limits(first, 10, limit);
        state.queue_with_limits(ClipboardAction::InboundImage(vec![2; 32]), 10, limit);
        assert_eq!(state.actions.len(), 1);
        assert!(state.actions_overloaded);
    }

    #[tokio::test]
    async fn missing_channel_returns_a_failure_for_a_waiting_local_paste() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.files_negotiated = true;
            guard.revoke_remote_files(true);
        }
        let mut descriptor = FileDescriptor::new("synthetic.bin");
        descriptor.file_size = Some(4);
        backend.on_remote_file_list(&[descriptor], Some(7));
        let generation = state.lock().unwrap().remote_files.generation();
        let _ = actions(&state);
        request_file_contents(&state, generation, 42, 0, 0, 4);
        let (host, receiver) = crate::host::host_channel();
        assert!(pump(&state, &mut active_stage_without_clipboard(), &host)
            .await
            .unwrap()
            .is_empty());
        assert!(
            matches!(receiver.try_recv().unwrap().message, HostMsg::FileContents { generation: got, stream_id: 42, data: None } if got == generation)
        );
    }

    #[test]
    fn channel_reset_keeps_unpumped_file_replies_as_failures() {
        let state = new_shared();
        {
            let mut guard = state.lock().unwrap();
            guard.queue(ClipboardAction::InboundFileContents(Reply {
                generation: 3,
                stream_id: 41,
                data: Some(vec![1, 2, 3]),
            }));
            guard.queue(ClipboardAction::InboundFileContents(Reply {
                generation: 3,
                stream_id: 42,
                data: None,
            }));
            guard.reset_for_new_channel();
        }
        let queued = actions(&state);
        for stream_id in [41, 42] {
            assert!(queued.iter().any(|action| matches!(action,
                ClipboardAction::InboundFileContents(Reply { generation: 3, stream_id: got, data: None }) if *got == stream_id)));
        }
    }

    #[tokio::test]
    async fn remote_file_action_keeps_the_policy_when_created_before_a_toggle() {
        let (host, receiver) = crate::host::host_channel();
        let policy = host.clipboard_policy_epoch();
        let state = new_shared_for_policy(Arc::clone(&policy));
        let mut backend = AditCliprdrBackend::new(Arc::clone(&state));
        {
            let mut guard = state.lock().unwrap();
            guard.files_negotiated = true;
            guard.revoke_remote_files(true);
        }
        let mut old = FileDescriptor::new("old.bin");
        old.file_size = Some(1);
        backend.on_remote_file_list(&[old], Some(10));
        policy.set_local_epoch(2);
        flush_app_actions(&state, &host).await.unwrap();
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardFilesForPolicy { policy_epoch: 0, files, .. } if files[0].name == "old.bin"));

        state.lock().unwrap().revoke_remote_files(true);
        let mut fresh = FileDescriptor::new("new.bin");
        fresh.file_size = Some(1);
        backend.on_remote_file_list(&[fresh], Some(11));
        flush_app_actions(&state, &host).await.unwrap();
        assert!(receiver.try_iter().any(|queued| matches!(queued.message,
            HostMsg::ClipboardFilesForPolicy { policy_epoch: 2, files, .. }
                if files.first().is_some_and(|file| file.name == "new.bin"))));
    }

    #[test]
    fn delayed_old_descriptor_cannot_be_reclassified_after_reenable() {
        use ironrdp_cliprdr::pdu::ClipboardFormatName;
        let (host, _) = crate::host::host_channel();
        let policy = host.clipboard_policy_epoch();
        let state = new_shared_for_policy(Arc::clone(&policy));
        let mut backend = AditCliprdrBackend::new(Arc::clone(&state));
        state.lock().unwrap().files_negotiated = true;
        let file_format = ClipboardFormat::new(ClipboardFormatId::new(0xC123))
            .with_name(ClipboardFormatName::FILE_LIST);
        backend.on_remote_copy(std::slice::from_ref(&file_format));
        assert!(actions(&state)
            .iter()
            .any(|action| matches!(action, ClipboardAction::RequestText(_))));

        policy.set_local_epoch(1);
        policy.set_local_epoch(2);
        backend.on_remote_copy(std::slice::from_ref(&file_format));
        assert!(!actions(&state)
            .iter()
            .any(|action| matches!(action, ClipboardAction::RequestText(_))));
        let mut old = FileDescriptor::new("old.bin");
        old.file_size = Some(1);
        backend.on_remote_file_list(&[old], Some(10));
        let queued = actions(&state);
        assert!(queued
            .iter()
            .any(|action| matches!(action, ClipboardAction::RequestText(_))));
        assert!(!queued.iter().any(|action| matches!(action, ClipboardAction::InboundFiles { files, .. } if !files.is_empty())));

        let mut fresh = FileDescriptor::new("fresh.bin");
        fresh.file_size = Some(1);
        backend.on_remote_file_list(&[fresh], Some(11));
        assert!(actions(&state).iter().any(|action| matches!(action,
            ClipboardAction::InboundFiles { policy_epoch: 2, files, .. }
                if files.first().is_some_and(|file| file.name == "fresh.bin"))));
    }

    #[tokio::test]
    async fn redirect_revokes_and_fails_reads_before_reconnect_can_finish() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.files_negotiated = true;
            guard.revoke_remote_files(true);
        }
        let mut descriptor = FileDescriptor::new("remote.bin");
        descriptor.file_size = Some(8);
        backend.on_remote_file_list(&[descriptor], Some(10));
        let generation = state.lock().unwrap().remote_files.generation();
        let (host, receiver) = crate::host::host_channel();
        flush_app_actions(&state, &host).await.unwrap();
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardFilesForPolicy { generation: got, ref files, .. } if got == generation && !files.is_empty()));

        request_file_contents(&state, generation, 41, 0, 0, 2);
        request_file_contents(&state, generation, 42, 0, 0, 2);
        let read = actions(&state)
            .into_iter()
            .find_map(|action| match action {
                ClipboardAction::RequestFileContents(read) => Some(read),
                _ => None,
            })
            .unwrap();
        backend.on_file_contents_response(FileContentsResponse::new_data_response(
            read.wire_id,
            vec![1, 2],
        ));
        // No new TCP connection is involved: this is the instant the old
        // active session returns a redirection.
        leave_channel(&state, &host).await.unwrap();
        let messages: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|queued| queued.message)
            .collect();
        for stream_id in [41, 42] {
            assert!(messages.iter().any(|message| matches!(message,
                HostMsg::FileContents { generation: got, stream_id: id, data: None }
                if *got == generation && *id == stream_id)));
        }
        assert!(messages.iter().any(|message| matches!(message,
            HostMsg::ClipboardFilesForPolicy { generation: next, files, .. } if *next > generation && files.is_empty())));
        // A request arriving while the reconnect stalls is also refused now.
        crate::session::handle_handshake_command(
            crate::session::SessionCmd::RequestFileContents {
                generation,
                stream_id: 43,
                index: 0,
                offset: 0,
                length: 2,
            },
            &state,
            &mut None,
            &host,
        )
        .await
        .unwrap();
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::FileContents { generation: got, stream_id: 43, data: None } if got == generation));
    }

    #[tokio::test]
    async fn channel_rebuild_keeps_unpumped_empty_revocation_for_the_app() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.files_negotiated = true;
            guard.revoke_remote_files(true);
        }
        let mut descriptor = FileDescriptor::new("remote.bin");
        descriptor.file_size = Some(8);
        backend.on_remote_file_list(&[descriptor], Some(10));
        let _ = actions(&state);
        state.lock().unwrap().revoke_remote_files(false);
        // Building another backend previously discarded this empty list.
        let _redirected = AditCliprdrBackend::new(Arc::clone(&state));
        let (host, receiver) = crate::host::host_channel();
        flush_app_actions(&state, &host).await.unwrap();
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardFilesForPolicy { files, .. } if files.is_empty()));
    }

    #[tokio::test]
    async fn handshake_flush_preserves_wire_actions() {
        let state = new_shared();
        {
            let mut guard = state.lock().unwrap();
            guard.queue(ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT,
            ]));
            guard.queue(ClipboardAction::InboundFiles {
                policy_epoch: 0,
                generation: 3,
                files: Vec::new(),
            });
        }
        let (host, receiver) = crate::host::host_channel();
        flush_app_actions(&state, &host).await.unwrap();
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardFilesForPolicy { generation: 3, files, .. } if files.is_empty()));
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );
    }

    #[tokio::test]
    async fn failure_reply_with_no_clipboard_channel_isolates_files_not_the_desktop() {
        let (mut backend, state) = backend();
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 9,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 1,
            data_id: None,
        });
        let (host, receiver) = crate::host::host_channel();
        assert!(pump(&state, &mut active_stage_without_clipboard(), &host)
            .await
            .unwrap()
            .is_empty());
        assert!(state.lock().unwrap().local_file_suspended);
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn refused_file_read_is_encoded_as_a_wire_error() {
        use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatListResponse};
        use ironrdp_svc::{StaticChannelSet, SvcProcessor};
        use std::any::TypeId;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
        let ready = ClipboardPdu::FormatListResponse(FormatListResponse::Ok);
        let mut ready_bytes = vec![0; ready.size()];
        ready
            .encode(&mut WriteCursor::new(&mut ready_bytes))
            .unwrap();
        client.process(&ready_bytes).unwrap();
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();

        submit_file_contents(&state, pending_test_read(&state, 0, 77), None);
        let (host, _receiver) = crate::host::host_channel();
        let frames = pump(&state, &mut stage, &host).await.unwrap();
        assert_eq!(frames.len(), 1);
        let expected = ClipboardPdu::FileContentsResponse(FileContentsResponse::new_error(77));
        let mut expected_bytes = vec![0; expected.size()];
        expected
            .encode(&mut WriteCursor::new(&mut expected_bytes))
            .unwrap();
        let wire = frames[0]
            .windows(expected_bytes.len())
            .find(|bytes| *bytes == expected_bytes)
            .expect("encoded CLIPRDR error in the RDP frame");
        let decoded: ClipboardPdu<'_> = decode(wire).unwrap();
        assert!(
            matches!(decoded, ClipboardPdu::FileContentsResponse(response) if response.stream_id() == 77 && response.is_error())
        );
    }

    #[tokio::test]
    async fn untagged_requests_and_late_locks_after_exposure_are_refused() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_session::ActiveStageOutput;
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 4,
            is_dir: false,
        };
        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(vec![file("bootstrap.bin")], &host).await;
        let mut backend = AditCliprdrBackend {
            state: Arc::clone(&state),
        };
        offer_local_files(&state, 1, vec![file("old.bin")]);
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(backend.allow_incoming_file_lock(&LockDataId(90)));
        backend.on_lock(LockDataId(90));
        offer_local_files(&state, 0, Vec::new());
        offer_local_files(&state, 2, vec![file("new.bin")]);
        let _ = actions(&state);
        // The same wire fields can mean a late lock for the old offer or a new
        // lock for the new offer. An unlocked request carries no generation either.
        let late_lock = ClipboardPdu::LockData(LockDataId(90));
        let output =
            process_clipboard_bytes(&mut stage, ironrdp_core::encode_vec(&late_lock).unwrap());
        assert!(output.iter().all(|item| !matches!(item,
            ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
        for (stream_id, data_id) in [(1, None), (2, Some(90))] {
            let request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
                stream_id,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                requested_size: 1,
                data_id,
            });
            let output =
                process_clipboard_bytes(&mut stage, ironrdp_core::encode_vec(&request).unwrap());
            assert!(output.iter().all(|item| !matches!(item,
                ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
        }
        {
            let guard = state.lock().unwrap();
            assert_eq!(guard.local_file_direction, LocalFileDirection::Sealed);
            assert!(guard.local_file_locks.is_empty());
            for stream_id in [1, 2] {
                assert!(guard.actions.iter().any(|action| matches!(action,
                    ClipboardAction::RefuseClosedFileContents { channel_epoch, stream_id: got }
                        if *channel_epoch == guard.channel_epoch && *got == stream_id)));
            }
            assert!(guard.actions.iter().all(|action| !matches!(
                action,
                ClipboardAction::NeedFileContents { .. }
                    | ClipboardAction::SubmitFileContents { data: Some(_), .. }
            )));
        }
        let wire = flush_test_wire(&state, &mut stage, &host).await;
        let responses = decode_clipboard_messages(&wire);
        assert_eq!(responses.len(), 2);
        for (bytes, stream_id) in responses.iter().zip([1, 2]) {
            assert!(matches!(decode::<ClipboardPdu<'_>>(bytes).unwrap(),
                ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == stream_id && reply.is_error()));
        }
        assert!(receiver.try_iter().all(|queued| !matches!(
            queued.message,
            HostMsg::FileContentsNeeded { .. } | HostMsg::Closed | HostMsg::Error(_)
        )));
    }

    #[test]
    fn file_epoch_backend_rejects_old_gui_requests_and_late_wire_replies() {
        let (mut backend, state) = backend();
        {
            let mut state = state.lock().unwrap();
            state.files_negotiated = true;
            state.revoke_remote_files(true);
        }
        let mut descriptor = FileDescriptor::new("A.bin");
        descriptor.file_size = Some(8);
        backend.on_remote_file_list(&[descriptor.clone()], Some(10));
        let a = state.lock().unwrap().remote_files.generation();
        request_file_contents(&state, a, 42, 0, 0, 3);
        let queued = actions(&state);
        let old = queued
            .iter()
            .find_map(|a| match a {
                ClipboardAction::RequestFileContents(read) => Some(*read),
                _ => None,
            })
            .unwrap();
        assert_eq!(prepare_remote_read(&state, &old).unwrap().data_id, Some(10));
        descriptor.name = "B.bin".into();
        backend.on_remote_file_list(&[descriptor], Some(20));
        let b = state.lock().unwrap().remote_files.generation();
        // GUI is still holding A: helper authority, not GUI polling, must refuse.
        request_file_contents(&state, a, 43, 0, 0, 3);
        assert!(prepare_remote_read(&state, &old).is_none());
        let queued = actions(&state);
        assert!(queued.iter().any(
            |action| matches!(action, ClipboardAction::InboundFileContents(
            Reply { generation, stream_id: 43, data: None }) if *generation == a)
        ));
        // Verify directly rather than relying on the caller to choose fresh ids.
        request_file_contents(&state, b, 42, 0, 0, 3);
        let new = actions(&state)
            .into_iter()
            .find_map(|a| match a {
                ClipboardAction::RequestFileContents(read) => Some(read),
                _ => None,
            })
            .unwrap();
        assert_ne!(old.wire_id, new.wire_id);
        assert_eq!(prepare_remote_read(&state, &new).unwrap().data_id, Some(20));
        backend.on_file_contents_response(FileContentsResponse::new_data_response(
            old.wire_id,
            vec![99],
        ));
        assert!(actions(&state).is_empty());
        backend.on_file_contents_response(FileContentsResponse::new_data_response(
            new.wire_id,
            vec![1, 2, 3],
        ));
        assert!(actions(&state).iter().any(|a| matches!(a, ClipboardAction::InboundFileContents(
            Reply { generation, stream_id: 42, data: Some(bytes) }) if *generation == b && bytes == &[1, 2, 3])));
    }

    #[test]
    fn file_epoch_text_format_list_and_redirection_revoke_active_files() {
        let (mut backend, state) = backend();
        {
            let mut state = state.lock().unwrap();
            state.files_negotiated = true;
            state.revoke_remote_files(true);
        }
        let mut descriptor = FileDescriptor::new("A.bin");
        descriptor.file_size = Some(8);
        backend.on_remote_file_list(&[descriptor], Some(10));
        let generation = state.lock().unwrap().remote_files.generation();
        let _ = actions(&state);
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        request_file_contents(&state, generation, 1, 0, 0, 3);
        let queued = actions(&state);
        assert!(queued
            .iter()
            .any(|a| matches!(a, ClipboardAction::InboundFiles { files, .. } if files.is_empty())));
        assert!(queued.iter().any(|a| matches!(a, ClipboardAction::InboundFileContents(Reply { generation: got, data: None, .. }) if *got == generation)));
        assert!(!state.lock().unwrap().remote_files.active());
        state.lock().unwrap().reset_for_new_channel();
        request_file_contents(&state, generation, 2, 0, 0, 3);
        assert!(actions(&state)
            .iter()
            .all(|a| !matches!(a, ClipboardAction::RequestFileContents(_))));
    }

    #[test]
    fn unsafe_remote_file_descriptors_revoke_the_previous_offer() {
        let (mut backend, state) = backend();
        for name in ["C:\\escape", "..\\escape", "\\absolute", "NUL.txt"] {
            let descriptor = FileDescriptor::new(name);
            assert!(validated_remote_files(std::slice::from_ref(&descriptor)).is_err());
            backend.on_remote_file_list(&[descriptor], Some(7));
            assert!(actions(&state).iter().any(
                |a| matches!(a, ClipboardAction::InboundFiles { files, .. } if files.is_empty())
            ));
            assert!(!state.lock().unwrap().remote_files.active());
        }
        let mut reparse = FileDescriptor::new("link.txt");
        reparse.attributes = Some(ClipboardFileAttributes::from_bits_retain(0x400));
        assert!(validated_remote_files(&[reparse]).is_err());
        let mut huge = FileDescriptor::new("huge.txt");
        huge.file_size = Some(u64::MAX);
        assert!(validated_remote_files(&[huge]).is_err());
    }

    /// Drive the backend and take whatever it queued, ignoring the shared handle
    /// bookkeeping the session loop would otherwise do.
    fn backend() -> (AditCliprdrBackend, SharedClipboard) {
        let state = new_shared();
        (AditCliprdrBackend::new(Arc::clone(&state)), state)
    }

    fn actions(state: &SharedClipboard) -> Vec<ClipboardAction> {
        state.lock().expect("state lock").take_actions()
    }

    fn file_offer(state: &SharedClipboard, files: Vec<ClipFile>) -> ClipboardAction {
        let state = state.lock().unwrap();
        ClipboardAction::AdvertiseFiles(
            files,
            FileSnapshot {
                channel_epoch: state.channel_epoch,
                generation: state.local_file_generation,
                revision: state.local_file_revision,
            },
        )
    }

    #[tokio::test]
    async fn ready_and_an_already_queued_revocation_cannot_reoffer_old_files() {
        use crate::session::{drive_handshake, SessionCmd};

        for capabilities_first in [false, true] {
            let (mut old, state) = backend();
            old.on_process_negotiated_capabilities(
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
            );
            old.on_ready();
            offer_local_files(
                &state,
                7,
                vec![ClipFile {
                    name: "old.bin".into(),
                    size: 4,
                    is_dir: false,
                }],
            );
            let _ = actions(&state);
            let mut redirected = AditCliprdrBackend::new(Arc::clone(&state));
            redirected.on_request_format_list();
            let _ = actions(&state);
            let (sender, mut input) = crate::host::input_channel();
            let (host, _messages) = crate::host::host_channel();
            let mut layout = None;
            // A single connect poll can make Ready and enqueue a GUI revocation
            // before returning. The connect result must not win over that command.
            let connect = async {
                if capabilities_first {
                    redirected.on_process_negotiated_capabilities(
                        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
                    );
                    redirected.on_ready();
                } else {
                    redirected.on_ready();
                    redirected.on_process_negotiated_capabilities(
                        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
                    );
                }
                assert!(sender.send(SessionCmd::ClipboardFiles {
                    generation: 0,
                    files: Vec::new(),
                }));
                Ok::<_, crate::RdpError>(())
            };
            drive_handshake(
                connect,
                &mut input,
                &state,
                &mut layout,
                &host,
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert!(state.lock().unwrap().local_files.is_empty());
            assert!(!actions(&state)
                .iter()
                .any(|action| matches!(action, ClipboardAction::AdvertiseFiles(..))));
        }
    }

    #[tokio::test]
    async fn handshake_deadline_and_close_interrupt_a_full_output_queue() {
        use crate::host::InputEnd;
        use crate::session::drive_handshake;
        use std::time::Duration;

        for close in [false, true] {
            let (_backend, state) = backend();
            let (sender, mut input) = crate::host::input_channel();
            let (host, _messages) = crate::host::host_channel();
            for stream_id in 0..128 {
                host.send(HostMsg::FileContents {
                    generation: 1,
                    stream_id,
                    data: None,
                })
                .unwrap();
            }
            state
                .lock()
                .unwrap()
                .queue(ClipboardAction::InboundFileContents(Reply {
                    generation: 1,
                    stream_id: 999,
                    data: None,
                }));
            let mut layout = None;
            let handshake = drive_handshake(
                std::future::pending::<Result<(), crate::RdpError>>(),
                &mut input,
                &state,
                &mut layout,
                &host,
                if close {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(100)
                },
            );
            tokio::pin!(handshake);
            if close {
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), &mut handshake)
                        .await
                        .is_err()
                );
                sender.stop(InputEnd::Close);
            }
            let result = tokio::time::timeout(Duration::from_secs(1), &mut handshake)
                .await
                .expect("output backpressure hid the handshake deadline or Close");
            if close {
                assert!(matches!(result, Err(crate::RdpError::ControlChannelClosed)));
            } else {
                assert!(
                    matches!(result, Err(crate::RdpError::Connect(message)) if message.contains("timed out"))
                );
            }
        }
    }

    #[tokio::test]
    async fn redirect_flush_is_also_interruptible_when_stdout_is_full() {
        use crate::host::InputEnd;
        use crate::session::guard_handshake;
        use std::time::Duration;

        for close in [false, true] {
            let (_backend, state) = backend();
            state
                .lock()
                .unwrap()
                .queue(ClipboardAction::InboundFileContents(Reply {
                    generation: 1,
                    stream_id: 999,
                    data: Some(vec![1]),
                }));
            let (sender, mut input) = crate::host::input_channel();
            let (host, _messages) = crate::host::host_channel();
            for stream_id in 0..128 {
                host.send(HostMsg::FileContents {
                    generation: 1,
                    stream_id,
                    data: None,
                })
                .unwrap();
            }
            let leaving = guard_handshake(
                async {
                    leave_channel(&state, &host)
                        .await
                        .map_err(|error| crate::RdpError::Session(error.to_string()))
                },
                &mut input.end,
                if close {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(100)
                },
            );
            tokio::pin!(leaving);
            if close {
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), &mut leaving)
                        .await
                        .is_err()
                );
                sender.stop(InputEnd::Close);
            }
            let result = tokio::time::timeout(Duration::from_secs(1), &mut leaving)
                .await
                .expect("redirect flush hid the deadline or Close");
            if close {
                assert!(matches!(result, Err(crate::RdpError::ControlChannelClosed)));
            } else {
                assert!(
                    matches!(result, Err(crate::RdpError::Connect(message)) if message.contains("timed out"))
                );
            }
        }
    }

    #[tokio::test]
    async fn active_pump_defers_a_ready_offer_behind_a_queued_revocation() {
        use crate::session::{handle_handshake_command, SessionCmd};
        use std::time::Duration;

        for arrives_during_output_wait in [false, true] {
            let (mut backend, state) = backend();
            backend.on_process_negotiated_capabilities(
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
            );
            backend.on_ready();
            let _ = actions(&state);
            let file = ClipFile {
                name: "old.bin".into(),
                size: 4,
                is_dir: false,
            };
            if arrives_during_output_wait {
                state
                    .lock()
                    .unwrap()
                    .queue(ClipboardAction::InboundFileContents(Reply {
                        generation: 1,
                        stream_id: 999,
                        data: None,
                    }));
            }
            offer_local_files(&state, 7, vec![file]);
            let (sender, mut input) = crate::host::input_channel();
            let (host, receiver) = crate::host::host_channel();
            if arrives_during_output_wait {
                for stream_id in 0..128 {
                    host.send(HostMsg::FileContents {
                        generation: 1,
                        stream_id,
                        data: None,
                    })
                    .unwrap();
                }
            } else {
                assert!(sender.send(SessionCmd::ClipboardFiles {
                    generation: 0,
                    files: Vec::new()
                }));
            }
            {
                let mut stage = active_stage_without_clipboard();
                let pumping = pump_with_pending_input(&state, &mut stage, &host, Some(&input));
                tokio::pin!(pumping);
                if arrives_during_output_wait {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(30), &mut pumping)
                            .await
                            .is_err()
                    );
                    assert!(sender.send(SessionCmd::ClipboardFiles {
                        generation: 0,
                        files: Vec::new()
                    }));
                    drop(receiver.try_recv().unwrap());
                }
                let frames = tokio::time::timeout(Duration::from_secs(1), &mut pumping)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(frames.is_empty());
            }
            assert!(state
                .lock()
                .unwrap()
                .actions
                .iter()
                .any(|action| matches!(action, ClipboardAction::AdvertiseFiles(..))));
            let command = input.commands.try_recv().unwrap().command;
            handle_handshake_command(command, &state, &mut None, &host)
                .await
                .unwrap();
            assert!(!actions(&state)
                .iter()
                .any(|action| matches!(action, ClipboardAction::AdvertiseFiles(..))));
        }
    }

    #[tokio::test]
    async fn encoded_file_reply_is_revalidated_after_stdout_wait_and_queued_revocation() {
        use crate::session::{flush_clipboard_output, handle_handshake_command, SessionCmd};
        use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatListResponse};
        use ironrdp_svc::{StaticChannelSet, SvcProcessor};
        use std::any::TypeId;
        use std::time::Duration;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
        let ready = ClipboardPdu::FormatListResponse(FormatListResponse::Ok);
        let mut ready_bytes = vec![0; ready.size()];
        ready
            .encode(&mut WriteCursor::new(&mut ready_bytes))
            .unwrap();
        client.process(&ready_bytes).unwrap();
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        submit_file_contents(
            &state,
            pending_test_read(&state, 7, 41),
            Some(vec![9, 8, 7, 6]),
        );
        state
            .lock()
            .unwrap()
            .queue(ClipboardAction::InboundFileContents(Reply {
                generation: 1,
                stream_id: 999,
                data: None,
            }));

        let (sender, mut input) = crate::host::input_channel();
        let (host, receiver) = crate::host::host_channel();
        for stream_id in 0..128 {
            host.send(HostMsg::FileContents {
                generation: 1,
                stream_id,
                data: None,
            })
            .unwrap();
        }
        let frames = {
            let pumping = pump_with_pending_input(&state, &mut stage, &host, Some(&input));
            tokio::pin!(pumping);
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut pumping)
                    .await
                    .is_err()
            );
            assert!(sender.send(SessionCmd::ClipboardFiles {
                generation: 0,
                files: Vec::new()
            }));
            drop(receiver.try_recv().unwrap());
            tokio::time::timeout(Duration::from_secs(1), &mut pumping)
                .await
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            frames.len(),
            1,
            "the successful reply must already be encoded"
        );
        assert!(frames[0]
            .bytes
            .windows(4)
            .any(|bytes| bytes == [9, 8, 7, 6]));
        let mut pending = VecDeque::from(frames);
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        flush_clipboard_output(
            &mut wire,
            &mut pending,
            &state,
            &mut stage,
            &host,
            Some(&input),
        )
        .await
        .unwrap();
        assert_eq!(pending.len(), 1);
        let command = input.commands.try_recv().unwrap().command;
        handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        flush_clipboard_output(
            &mut wire,
            &mut pending,
            &state,
            &mut stage,
            &host,
            Some(&input),
        )
        .await
        .unwrap();
        assert!(pending.is_empty());
        assert!(
            state.lock().unwrap().actions.is_empty(),
            "the replacement FormatList must also be sent"
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let expected = ClipboardPdu::FileContentsResponse(FileContentsResponse::new_error(41));
        let mut expected_bytes = vec![0; expected.size()];
        expected
            .encode(&mut WriteCursor::new(&mut expected_bytes))
            .unwrap();
        assert!(observed
            .windows(expected_bytes.len())
            .any(|bytes| bytes == expected_bytes));
        assert!(!observed.windows(4).any(|bytes| bytes == [9, 8, 7, 6]));
    }

    async fn prepared_file_reply() -> (SharedClipboard, ActiveStage, ClipboardWireFrame) {
        use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatListResponse};
        use ironrdp_svc::{StaticChannelSet, SvcProcessor};
        use std::any::TypeId;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
        let ready = ClipboardPdu::FormatListResponse(FormatListResponse::Ok);
        let mut bytes = vec![0; ready.size()];
        ready.encode(&mut WriteCursor::new(&mut bytes)).unwrap();
        client.process(&bytes).unwrap();
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        submit_file_contents(
            &state,
            pending_test_read(&state, 7, 41),
            Some(vec![9, 8, 7, 6]),
        );
        let (host, _receiver) = crate::host::host_channel();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        (state, stage, frames.remove(0))
    }

    // Keep the original reproduction with a backend that uses the upstream
    // default hook. Adit's backend overrides it; other backends remain unchanged.
    #[derive(Debug)]
    struct InlineBaselineBackend(AditCliprdrBackend);
    ironrdp_core::impl_as_any!(InlineBaselineBackend);

    impl CliprdrBackend for InlineBaselineBackend {
        fn temporary_directory(&self) -> &str {
            self.0.temporary_directory()
        }
        fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
            self.0.client_capabilities()
        }
        fn on_ready(&mut self) {
            self.0.on_ready();
        }
        fn on_request_format_list(&mut self) {
            self.0.on_request_format_list();
        }
        fn on_process_negotiated_capabilities(&mut self, flags: ClipboardGeneralCapabilityFlags) {
            self.0.on_process_negotiated_capabilities(flags);
        }
        fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
            self.0.on_remote_copy(formats);
        }
        fn on_format_data_request(&mut self, request: FormatDataRequest) {
            self.0.on_format_data_request(request);
        }
        fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
            self.0.on_format_data_response(response);
        }
        fn on_file_contents_request(&mut self, request: FileContentsRequest) {
            self.0.on_file_contents_request(request);
        }
        fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
            self.0.on_file_contents_response(response);
        }
        fn on_lock(&mut self, id: LockDataId) {
            self.0.on_lock(id);
        }
        fn on_unlock(&mut self, id: LockDataId) {
            self.0.on_unlock(id);
        }
    }

    #[test]
    fn inline_descriptor_response_is_not_routed_through_the_clipboard_pump() {
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatListResponse,
        };
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::{image::DecodedImage, ActiveStageOutput};
        use ironrdp_svc::{StaticChannelSet, SvcMessage, SvcProcessor};
        use std::any::TypeId;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(InlineBaselineBackend(
            AditCliprdrBackend::new(Arc::clone(&state)),
        )));
        let caps = ClipboardPdu::Capabilities(Capabilities::new(
            ClipboardProtocolVersion::V2,
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
                | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
        ));
        let mut bytes = vec![0; caps.size()];
        caps.encode(&mut WriteCursor::new(&mut bytes)).unwrap();
        client.process(&bytes).unwrap();
        let ready = ClipboardPdu::FormatListResponse(FormatListResponse::Ok);
        let mut bytes = vec![0; ready.size()];
        ready.encode(&mut WriteCursor::new(&mut bytes)).unwrap();
        client.process(&bytes).unwrap();
        let file = ClipFile {
            name: "synthetic-old.txt".into(),
            size: 4,
            is_dir: false,
        };
        offer_local_files(&state, 7, vec![file.clone()]);
        let _ = actions(&state);
        client
            .initiate_file_copy(vec![to_descriptor(&file)])
            .unwrap();

        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        let request = ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::new(0xC0FE),
        });
        let mut request_bytes = vec![0; request.size()];
        request
            .encode(&mut WriteCursor::new(&mut request_bytes))
            .unwrap();
        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![SvcMessage::from(request_bytes)],
            1004,
            1001,
        )
        .unwrap();
        let (sender, input) = crate::host::input_channel();
        assert!(sender.send(crate::session::SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new()
        }));
        assert!(input.has_pending_offer());
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        let output = stage
            .process(&mut image, ironrdp_pdu::Action::X224, &packet)
            .unwrap();
        let name_utf16: Vec<u8> = file
            .name
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(output.iter().any(|item| matches!(item,
            ActiveStageOutput::ResponseFrame(frame) if frame.windows(name_utf16.len()).any(|bytes| bytes == name_utf16))));
        assert!(
            input.has_pending_offer(),
            "the inline response did not consume the queued revoke"
        );
    }

    #[test]
    fn descriptor_with_unknown_cached_snapshot_is_refused() {
        let (mut backend, state) = backend();
        assert!(backend
            .on_local_file_list_response(OwnedFormatDataResponse::new_data(vec![1, 2, 3]))
            .is_none());
        assert!(matches!(actions(&state).as_slice(),
            [ClipboardAction::SubmitFileDescriptor { snapshot: None, response }] if response.is_error()));
    }

    #[derive(Debug)]
    struct EchoChannel;
    ironrdp_core::impl_as_any!(EchoChannel);

    impl ironrdp_svc::SvcProcessor for EchoChannel {
        fn channel_name(&self) -> ironrdp_pdu::gcc::ChannelName {
            ironrdp_pdu::gcc::ChannelName::new(*b"echo\0\0\0\0")
        }

        fn process(
            &mut self,
            payload: &[u8],
        ) -> ironrdp_pdu::PduResult<Vec<ironrdp_svc::SvcMessage>> {
            Ok(vec![ironrdp_svc::SvcMessage::from(payload.to_vec())])
        }
    }

    async fn descriptor_stage(
        files: Vec<ClipFile>,
        host: &HostSender,
    ) -> (SharedClipboard, ActiveStage) {
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatListResponse,
        };
        use ironrdp_svc::{StaticChannelSet, SvcProcessor};
        use std::any::TypeId;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
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
        let _ = actions(&state);
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        channels.insert(EchoChannel);
        channels.attach_channel_id(TypeId::of::<EchoChannel>(), 1005);
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        offer_local_files(&state, 7, files);
        assert!(!pump_with_pending_input(&state, &mut stage, host, None)
            .await
            .unwrap()
            .is_empty());
        (state, stage)
    }

    fn process_format_request(
        stage: &mut ActiveStage,
        format: u32,
    ) -> Vec<ironrdp_session::ActiveStageOutput> {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::image::DecodedImage;
        use ironrdp_svc::SvcMessage;
        let request = ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::new(format),
        });
        let mut request_bytes = vec![0; request.size()];
        request
            .encode(&mut WriteCursor::new(&mut request_bytes))
            .unwrap();
        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![SvcMessage::from(request_bytes)],
            1004,
            1001,
        )
        .unwrap();
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        stage
            .process(&mut image, ironrdp_pdu::Action::X224, &packet)
            .unwrap()
    }

    fn decode_one_clipboard_message(bytes: &[u8]) -> (Vec<u8>, usize) {
        use ironrdp_pdu::mcs::McsMessage;
        use ironrdp_pdu::rdp::vc::{ChannelControlFlags, ChannelPduHeader};
        use ironrdp_pdu::x224::X224;

        let mut offset = 0;
        let mut chunks = 0;
        let mut data = Vec::new();
        let mut total = None;
        while offset < bytes.len() {
            assert_eq!(bytes[offset], 3, "each chunk has a TPKT header");
            let length = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
            let X224(message) =
                decode::<X224<McsMessage<'_>>>(&bytes[offset..offset + length]).unwrap();
            let McsMessage::SendDataRequest(request) = message else {
                panic!("expected an MCS SendDataRequest");
            };
            assert_eq!(request.channel_id, 1004);
            let header: ChannelPduHeader = decode(&request.user_data[..8]).unwrap();
            assert_eq!(
                header.flags.contains(ChannelControlFlags::FLAG_FIRST),
                chunks == 0
            );
            assert_eq!(total.get_or_insert(header.length), &header.length);
            data.extend_from_slice(&request.user_data[8..]);
            offset += length;
            chunks += 1;
            assert_eq!(
                header.flags.contains(ChannelControlFlags::FLAG_LAST),
                offset == bytes.len()
            );
        }
        assert_eq!(data.len(), usize::try_from(total.unwrap()).unwrap());
        (data, chunks)
    }

    fn decode_clipboard_messages(bytes: &[u8]) -> Vec<Vec<u8>> {
        use ironrdp_pdu::mcs::McsMessage;
        use ironrdp_pdu::rdp::vc::{ChannelControlFlags, ChannelPduHeader};
        use ironrdp_pdu::x224::X224;

        let mut messages = Vec::new();
        let mut start = 0;
        let mut offset = 0;
        while offset < bytes.len() {
            assert_eq!(bytes[offset], 3, "each chunk has a TPKT header");
            let length = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
            let X224(message) =
                decode::<X224<McsMessage<'_>>>(&bytes[offset..offset + length]).unwrap();
            let McsMessage::SendDataRequest(request) = message else {
                panic!("expected an MCS SendDataRequest");
            };
            assert_eq!(request.channel_id, 1004);
            let header: ChannelPduHeader = decode(&request.user_data[..8]).unwrap();
            offset += length;
            if header.flags.contains(ChannelControlFlags::FLAG_LAST) {
                messages.push(decode_one_clipboard_message(&bytes[start..offset]).0);
                start = offset;
            }
        }
        assert_eq!(start, bytes.len(), "the last SVC message must be complete");
        messages
    }

    fn remote_file_locks_on_wire(stage: &mut ActiveStage, count: usize) -> Vec<u32> {
        use ironrdp_cliprdr::pdu::{ClipboardFormatName, ClipboardPdu, FormatList};
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::{image::DecodedImage, ActiveStageOutput};
        use ironrdp_svc::SvcMessage;

        let format = ClipboardFormat::new(ClipboardFormatId::new(0xC123))
            .with_name(ClipboardFormatName::FILE_LIST);
        let list = ClipboardPdu::FormatList(FormatList::new_unicode(&[format], true).unwrap());
        let mut payload = vec![0; list.size()];
        list.encode(&mut WriteCursor::new(&mut payload)).unwrap();
        let packet =
            ironrdp_svc::server_encode_svc_messages(vec![SvcMessage::from(payload)], 1004, 1001)
                .unwrap();
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        let mut locks = Vec::new();
        for _ in 0..count {
            let output = stage
                .process(&mut image, ironrdp_pdu::Action::X224, &packet)
                .unwrap();
            let [ActiveStageOutput::ResponseFrame(bytes)] = output.as_slice() else {
                panic!("expected FormatListResponse and LockData on the wire");
            };
            let messages = decode_clipboard_messages(bytes);
            assert_eq!(messages.len(), 2);
            assert!(matches!(
                decode::<ClipboardPdu<'_>>(&messages[0]).unwrap(),
                ClipboardPdu::FormatListResponse(_)
            ));
            let ClipboardPdu::LockData(id) = decode::<ClipboardPdu<'_>>(&messages[1]).unwrap()
            else {
                panic!("expected LockData after the format-list response");
            };
            locks.push(id.0);
        }
        locks
    }

    fn assert_video_and_other_svc_still_work(stage: &mut ActiveStage) {
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_pdu::bitmap::{BitmapData, BitmapUpdateData, Compression};
        use ironrdp_pdu::fast_path::{
            EncryptionFlags, FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode,
        };
        use ironrdp_pdu::geometry::InclusiveRectangle;
        use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
        use ironrdp_session::{image::DecodedImage, ActiveStageOutput};
        use ironrdp_svc::SvcMessage;

        let pixels = [0x12, 0x34, 0x56, 0];
        let bitmap = BitmapUpdateData {
            rectangles: vec![BitmapData {
                rectangle: InclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                width: 1,
                height: 1,
                bits_per_pixel: 32,
                compression_flags: Compression::empty(),
                compressed_data_header: None,
                bitmap_data: &pixels,
            }],
        };
        let bitmap_bytes = ironrdp_core::encode_vec(&bitmap).unwrap();
        let update = FastPathUpdatePdu {
            fragmentation: Fragmentation::Single,
            update_code: UpdateCode::Bitmap,
            compression_flags: None,
            compression_type: None,
            data: &bitmap_bytes,
        };
        let update_bytes = ironrdp_core::encode_vec(&update).unwrap();
        let mut video = ironrdp_core::encode_vec(&FastPathHeader::new(
            EncryptionFlags::empty(),
            update_bytes.len(),
        ))
        .unwrap();
        video.extend(update_bytes);
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        let output = stage
            .process(&mut image, ironrdp_pdu::Action::FastPath, &video)
            .unwrap();
        assert!(output.iter().any(|item| matches!(item,
            ActiveStageOutput::GraphicsUpdate(rect) if rect.left == 0 && rect.top == 0 && rect.right == 0 && rect.bottom == 0)));
        assert_eq!(&image.data()[..3], &[0x56, 0x34, 0x12]);
        let keyboard = stage
            .process_fastpath_input(
                &mut image,
                &[FastPathInputEvent::KeyboardEvent(
                    KeyboardFlags::empty(),
                    0x1e,
                )],
            )
            .unwrap();
        assert!(keyboard.iter().any(
            |item| matches!(item, ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())
        ));
        let mut input = ironrdp_input::Database::new();
        let mouse = crate::input::map_input(
            &mut input,
            &adit_rdp_proto::InputEvent::MouseMove { x: 1, y: 1 },
        );
        assert!(!mouse.is_empty());
        let output = stage.process_fastpath_input(&mut image, &mouse).unwrap();
        assert!(output.iter().any(
            |item| matches!(item, ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())
        ));

        let payload: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![SvcMessage::from(payload.clone())],
            1005,
            1001,
        )
        .unwrap();
        let expected =
            ironrdp_svc::client_encode_svc_messages(vec![SvcMessage::from(payload)], 1005, 1001)
                .unwrap();
        let mut offset = 0;
        let mut chunks = 0;
        let mut response = Vec::new();
        while offset < packet.len() {
            let len = usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]));
            let output = stage
                .process(
                    &mut image,
                    ironrdp_pdu::Action::X224,
                    &packet[offset..offset + len],
                )
                .unwrap();
            for item in output {
                if let ActiveStageOutput::ResponseFrame(bytes) = item {
                    response.extend(bytes);
                }
            }
            chunks += 1;
            offset += len;
        }
        assert!(chunks > 1, "the other SVC message must span several chunks");
        assert_eq!(response, expected);
    }

    fn wire_file_request(stream_id: u32) -> Vec<u8> {
        ironrdp_core::encode_vec(&ironrdp_cliprdr::pdu::ClipboardPdu::FileContentsRequest(
            FileContentsRequest {
                stream_id,
                index: 0,
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id: None,
            },
        ))
        .unwrap()
    }

    fn malformed_file_requests(stream_id: u32) -> Vec<Vec<u8>> {
        let valid = wire_file_request(stream_id);
        let mut bad_index = valid.clone();
        bad_index[12..16].copy_from_slice(&(-1_i32).to_le_bytes());
        let mut no_flags = valid.clone();
        no_flags[16..20].copy_from_slice(&0_u32.to_le_bytes());
        let mut conflicting_flags = valid.clone();
        conflicting_flags[16..20].copy_from_slice(&3_u32.to_le_bytes());
        let mut unknown_flags = valid.clone();
        unknown_flags[16..20].copy_from_slice(&0x101_u32.to_le_bytes());
        let mut bad_size = valid.clone();
        bad_size[28..32].copy_from_slice(&4_u32.to_le_bytes());
        let mut bad_position = valid.clone();
        bad_position[20..24].copy_from_slice(&1_u32.to_le_bytes());
        let mut partial_data_id = valid.clone();
        partial_data_id[4..8].copy_from_slice(&25_u32.to_le_bytes());
        partial_data_id.push(0);
        let mut bad_header_flags = valid.clone();
        bad_header_flags[2..4].copy_from_slice(&1_u16.to_le_bytes());
        let mut trailing = valid.clone();
        trailing.push(0);
        let mut malformed = vec![
            bad_index,
            no_flags,
            conflicting_flags,
            unknown_flags,
            bad_size,
            bad_position,
            partial_data_id,
            bad_header_flags,
            trailing,
        ];
        for length in [4_u32, 23, 27, 29, u32::MAX] {
            let mut bytes = valid.clone();
            bytes[4..8].copy_from_slice(&length.to_le_bytes());
            malformed.push(bytes.clone());
            bytes.extend_from_slice(&[0x5a, 0xa5, 0x7c, 0xe1]);
            malformed.push(bytes);
        }
        for extra in [2_usize, 3, 5, 8] {
            let mut bytes = valid.clone();
            bytes.extend(std::iter::repeat_n(0xa5, extra));
            malformed.push(bytes);
        }
        malformed.extend((12..valid.len()).map(|length| valid[..length].to_vec()));
        let mut with_lock = valid;
        with_lock[4..8].copy_from_slice(&28_u32.to_le_bytes());
        with_lock.extend_from_slice(&90_u32.to_le_bytes());
        malformed.extend((32..with_lock.len()).map(|length| with_lock[..length].to_vec()));
        malformed
    }

    fn process_clipboard_bytes(
        stage: &mut ActiveStage,
        payload: Vec<u8>,
    ) -> Vec<ironrdp_session::ActiveStageOutput> {
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::image::DecodedImage;

        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![ironrdp_svc::SvcMessage::from(payload)],
            1004,
            1001,
        )
        .unwrap();
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        stage
            .process(&mut image, ironrdp_pdu::Action::X224, &packet)
            .unwrap()
    }

    async fn flush_test_wire(
        state: &SharedClipboard,
        stage: &mut ActiveStage,
        host: &HostSender,
    ) -> Vec<u8> {
        let (mut writer, mut reader) = tokio::io::duplex(1024 * 1024);
        crate::session::flush_clipboard_output(
            &mut writer,
            &mut VecDeque::new(),
            state,
            stage,
            host,
            None,
        )
        .await
        .unwrap();
        drop(writer);
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes)
            .await
            .unwrap();
        bytes
    }

    #[tokio::test]
    async fn active_stage_early_format_requests_fail_without_closing_and_ready_recovers() {
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatListResponse,
        };
        use ironrdp_session::ActiveStageOutput;
        use ironrdp_svc::StaticChannelSet;
        use std::any::TypeId;

        for kind in 0..3 {
            let (host, receiver) = crate::host::host_channel();
            let state = new_shared();
            let client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
            let format = match kind {
                0 => {
                    offer_local_text(&state, "synthetic text".into());
                    ClipboardFormatId::CF_UNICODETEXT
                }
                1 => {
                    offer_local_image(&state, vec![4, 3, 2, 1]);
                    ClipboardFormatId::CF_DIB
                }
                _ => {
                    offer_local_files(
                        &state,
                        7,
                        vec![ClipFile {
                            name: "synthetic-file.bin".into(),
                            size: 4,
                            is_dir: false,
                        }],
                    );
                    ClipboardFormatId::new(0xc0fe)
                }
            };
            let mut channels = StaticChannelSet::new();
            channels.insert(client);
            channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
            channels.insert(EchoChannel);
            channels.attach_channel_id(TypeId::of::<EchoChannel>(), 1005);
            let mut stage = ironrdp_session::ActiveStageBuilder {
                static_channels: channels,
                user_channel_id: 1001,
                io_channel_id: 1003,
                message_channel_id: None,
                share_id: 1,
                compression_type: None,
                enable_server_pointer: false,
                pointer_software_rendering: false,
            }
            .build();

            let output = process_format_request(&mut stage, format.value());
            assert!(output.iter().all(|item| !matches!(item,
                ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
            let wire = flush_test_wire(&state, &mut stage, &host).await;
            let messages = decode_clipboard_messages(&wire);
            assert_eq!(messages.len(), 1);
            assert!(matches!(decode::<ClipboardPdu<'_>>(&messages[0]).unwrap(),
                ClipboardPdu::FormatDataResponse(reply) if reply.is_error()));

            for pdu in [
                ClipboardPdu::Capabilities(Capabilities::new(
                    ClipboardProtocolVersion::V2,
                    ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                        | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
                        | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
                )),
                ClipboardPdu::MonitorReady,
            ] {
                process_clipboard_bytes(&mut stage, ironrdp_core::encode_vec(&pdu).unwrap());
            }
            assert!(!flush_test_wire(&state, &mut stage, &host).await.is_empty());
            let ready = ClipboardPdu::FormatListResponse(FormatListResponse::Ok);
            process_clipboard_bytes(&mut stage, ironrdp_core::encode_vec(&ready).unwrap());
            let _ = flush_test_wire(&state, &mut stage, &host).await;

            process_format_request(&mut stage, format.value());
            let wire = flush_test_wire(&state, &mut stage, &host).await;
            let messages = decode_clipboard_messages(&wire);
            assert_eq!(messages.len(), 1);
            let ClipboardPdu::FormatDataResponse(reply) =
                decode::<ClipboardPdu<'_>>(&messages[0]).unwrap()
            else {
                panic!("expected a format response after Ready");
            };
            assert!(!reply.is_error());
            match kind {
                0 => assert_eq!(reply.to_unicode_string().unwrap(), "synthetic text"),
                1 => assert_eq!(reply.data(), &[4, 3, 2, 1]),
                _ => assert_eq!(
                    reply.to_file_list().unwrap().files[0].name,
                    "synthetic-file.bin"
                ),
            }
            assert_video_and_other_svc_still_work(&mut stage);
            assert!(receiver
                .try_iter()
                .all(|queued| !matches!(queued.message, HostMsg::Closed | HostMsg::Error(_))));
        }
    }

    #[tokio::test]
    async fn active_stage_closed_file_requests_are_bounded_and_correlated() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::{ClipboardPdu, PackedFileList};

        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "local-fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;

        // The duplicate makes the already prepared response ambiguous.
        process_clipboard_bytes(&mut stage, wire_file_request(41));
        let mut old = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(old.len(), 1);
        process_clipboard_bytes(&mut stage, wire_file_request(41));
        assert!(state.lock().unwrap().local_file_suspended);
        let (mut writer, _reader) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut writer, &mut old[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Discarded
        );
        let _ = flush_test_wire(&state, &mut stage, &host).await;

        process_clipboard_bytes(&mut stage, wire_file_request(42));
        let bytes = flush_test_wire(&state, &mut stage, &host).await;
        let messages = decode_clipboard_messages(&bytes);
        assert_eq!(messages.len(), 1);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&messages[0]).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == 42 && reply.is_error()));
        process_clipboard_bytes(&mut stage, wire_file_request(42));
        assert!(flush_test_wire(&state, &mut stage, &host).await.is_empty());
        // The ID of the original ambiguous request remains quarantined as well.
        process_clipboard_bytes(&mut stage, wire_file_request(41));
        assert!(flush_test_wire(&state, &mut stage, &host).await.is_empty());

        process_clipboard_bytes(&mut stage, wire_file_request(43));
        let mut pending = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        process_clipboard_bytes(&mut stage, wire_file_request(43));
        assert_eq!(
            commit_clipboard_frame(&mut writer, &mut pending[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Discarded
        );

        process_clipboard_bytes(&mut stage, wire_file_request(44));
        let mut pending = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        process_clipboard_bytes(&mut stage, wire_file_request(99)[..8].to_vec());
        assert_eq!(
            commit_clipboard_frame(&mut writer, &mut pending[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Discarded
        );

        let mut malformed = wire_file_request(45);
        malformed[12..16].copy_from_slice(&(-1_i32).to_le_bytes());
        process_clipboard_bytes(&mut stage, malformed);
        let bytes = flush_test_wire(&state, &mut stage, &host).await;
        let messages = decode_clipboard_messages(&bytes);
        assert_eq!(messages.len(), 1);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&messages[0]).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == 45 && reply.is_error()));

        // Exercise the opposite direction through the real SVC processor too.
        let lock_id = remote_file_locks_on_wire(&mut stage, 1)[0];
        let request = flush_test_wire(&state, &mut stage, &host).await;
        let messages = decode_clipboard_messages(&request);
        assert!(matches!(
            decode::<ClipboardPdu<'_>>(&messages[0]).unwrap(),
            ClipboardPdu::FormatDataRequest(_)
        ));
        let mut descriptor = FileDescriptor::new("remote-fixture.bin");
        descriptor.file_size = Some(3);
        let response = ClipboardPdu::FormatDataResponse(
            OwnedFormatDataResponse::new_file_list(&PackedFileList {
                files: vec![descriptor],
            })
            .unwrap(),
        );
        process_clipboard_bytes(&mut stage, ironrdp_core::encode_vec(&response).unwrap());
        let _ = flush_test_wire(&state, &mut stage, &host).await;
        let messages: Vec<_> = receiver.try_iter().map(|queued| queued.message).collect();
        assert!(messages
            .iter()
            .all(|message| !matches!(message, HostMsg::Closed | HostMsg::Error(_))));
        let generation = messages
            .iter()
            .find_map(|message| match message {
                HostMsg::ClipboardFilesForPolicy {
                    generation, files, ..
                } if !files.is_empty() => Some(*generation),
                _ => None,
            })
            .expect("the remote file offer reached the app");
        request_file_contents(&state, generation, 71, 0, 0, 3);
        let wire = flush_test_wire(&state, &mut stage, &host).await;
        let messages = decode_clipboard_messages(&wire);
        assert_eq!(messages.len(), 1);
        let ClipboardPdu::FileContentsRequest(request) =
            decode::<ClipboardPdu<'_>>(&messages[0]).unwrap()
        else {
            panic!("expected the remote file read");
        };
        assert_eq!(request.data_id, Some(lock_id));
        let response = ClipboardPdu::FileContentsResponse(FileContentsResponse::new_data_response(
            request.stream_id,
            vec![4, 5, 6],
        ));
        process_clipboard_bytes(&mut stage, ironrdp_core::encode_vec(&response).unwrap());
        let _ = flush_test_wire(&state, &mut stage, &host).await;
        let messages: Vec<_> = receiver.try_iter().map(|queued| queued.message).collect();
        assert!(messages.iter().any(|message| matches!(message,
            HostMsg::FileContents { generation: got, stream_id: 71, data: Some(data) }
                if *got == generation && data == &[4, 5, 6])));
        assert!(messages
            .iter()
            .all(|message| !matches!(message, HostMsg::Closed | HostMsg::Error(_))));
        assert_video_and_other_svc_still_work(&mut stage);
    }

    #[tokio::test]
    async fn active_stage_closed_file_refusals_have_a_per_channel_budget() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;

        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "local-fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        suspend_local_file_direction(&state);
        let _ = flush_test_wire(&state, &mut stage, &host).await;
        for stream_id in 0..=(MAX_QUEUED_ACTIONS as u32) {
            process_clipboard_bytes(&mut stage, wire_file_request(stream_id));
        }
        {
            let guard = state.lock().unwrap();
            assert_eq!(guard.closed_file_streams.len(), MAX_QUEUED_ACTIONS);
            assert!(guard.actions.len() <= MAX_QUEUED_ACTIONS);
            assert!(!guard.actions_overloaded);
        }
        let wire = flush_test_wire(&state, &mut stage, &host).await;
        let messages = decode_clipboard_messages(&wire);
        assert_eq!(messages.len(), MAX_QUEUED_ACTIONS);
        assert!(messages.iter().all(|bytes| matches!(
            decode::<ClipboardPdu<'_>>(bytes).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if reply.is_error()
        )));
        process_clipboard_bytes(&mut stage, wire_file_request(MAX_QUEUED_ACTIONS as u32 + 1));
        assert!(flush_test_wire(&state, &mut stage, &host).await.is_empty());
        assert_video_and_other_svc_still_work(&mut stage);
    }

    #[tokio::test]
    async fn active_stage_clipboard_overload_discards_prepared_data_without_closing() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};

        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "local-fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        offer_local_text(&state, "private text".into());
        let _ = flush_test_wire(&state, &mut stage, &host).await;
        process_format_request(&mut stage, ClipboardFormatId::CF_UNICODETEXT.value());
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        {
            let mut guard = state.lock().unwrap();
            guard.queue_with_limits(ClipboardAction::InboundImage(vec![1]), 0, 0);
            assert!(guard.actions_overloaded);
            assert!(guard.local_file_suspended);
        }
        let (mut writer, _reader) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut writer, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Discarded
        );
        flush_app_actions(&state, &host).await.unwrap();
        assert!(flush_test_wire(&state, &mut stage, &host).await.is_empty());
        assert_video_and_other_svc_still_work(&mut stage);
        assert!(receiver
            .try_iter()
            .all(|queued| !matches!(queued.message, HostMsg::Closed | HostMsg::Error(_))));
    }

    #[tokio::test]
    async fn active_stage_closed_failure_cannot_cross_a_channel_replacement() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::StaticChannelSet;
        use std::any::TypeId;

        let (host, _receiver) = crate::host::host_channel();
        let (state, mut old_stage) = descriptor_stage(
            vec![ClipFile {
                name: "local-fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        suspend_local_file_direction(&state);
        let _ = flush_test_wire(&state, &mut old_stage, &host).await;
        process_clipboard_bytes(&mut old_stage, wire_file_request(55));
        let mut old = pump_with_pending_input(&state, &mut old_stage, &host, None)
            .await
            .unwrap();
        assert_eq!(old.len(), 1);

        let client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut new_stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        process_clipboard_bytes(&mut new_stage, wire_file_request(55));
        let (mut writer, _reader) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut writer, &mut old[0], &state, &mut new_stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Discarded
        );
        let wire = flush_test_wire(&state, &mut new_stage, &host).await;
        let messages = decode_clipboard_messages(&wire);
        assert_eq!(messages.len(), 1);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&messages[0]).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == 55 && reply.is_error()));
    }

    fn synthetic_ready_client(state: &SharedClipboard) -> CliprdrClient {
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatListResponse,
        };
        use ironrdp_svc::SvcProcessor;

        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(state))));
        for pdu in [
            ClipboardPdu::Capabilities(Capabilities::new(
                ClipboardProtocolVersion::V2,
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                    | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
                    | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
            )),
            ClipboardPdu::FormatListResponse(FormatListResponse::Ok),
        ] {
            client
                .process(&ironrdp_core::encode_vec(&pdu).unwrap())
                .unwrap();
        }
        let _ = actions(state);
        client
    }

    fn install_synthetic_file(
        state: &SharedClipboard,
        client: &mut CliprdrClient,
        generation: u64,
        name: &str,
        exposed: bool,
    ) {
        let file = ClipFile {
            name: name.into(),
            size: 4,
            is_dir: false,
        };
        offer_local_files(state, generation, vec![file.clone()]);
        client
            .initiate_file_copy(vec![to_descriptor(&file)])
            .unwrap();
        let mut guard = state.lock().unwrap();
        let snapshot = FileSnapshot {
            channel_epoch: guard.channel_epoch,
            generation: guard.local_file_generation,
            revision: guard.local_file_revision,
        };
        guard.cached_file_snapshot = Some(snapshot);
        if exposed {
            guard.local_file_direction = LocalFileDirection::Exposed(snapshot);
        }
    }

    fn synthetic_pdu_bytes(pdu: &ironrdp_cliprdr::pdu::ClipboardPdu<'_>, padded: bool) -> Vec<u8> {
        let mut bytes = ironrdp_core::encode_vec(pdu).unwrap();
        if padded {
            // Appendix A does not require the extra bytes to be zero.
            bytes.extend_from_slice(&[0x5a, 0xa5, 0x7c, 0xe1]);
        }
        bytes
    }

    #[test]
    fn legacy_pdu_padding_preserves_declared_ids_and_rejects_bad_lengths() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_core::{Decode as _, ReadCursor};

        let request = |data_id| FileContentsRequest {
            stream_id: 41,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id,
        };
        let range = |data_id| FileContentsRequest {
            stream_id: 42,
            index: 1,
            flags: FileContentsFlags::RANGE,
            position: 0x1_0000_0001,
            requested_size: 16,
            data_id,
        };
        for pdu in [
            ClipboardPdu::LockData(LockDataId(90)),
            ClipboardPdu::UnlockData(LockDataId(90)),
            ClipboardPdu::FileContentsRequest(request(None)),
            ClipboardPdu::FileContentsRequest(request(Some(90))),
            ClipboardPdu::FileContentsRequest(range(None)),
            ClipboardPdu::FileContentsRequest(range(Some(90))),
        ] {
            let base = synthetic_pdu_bytes(&pdu, false);
            for padded in [false, true] {
                let bytes = synthetic_pdu_bytes(&pdu, padded);
                let mut cursor = ReadCursor::new(&bytes);
                assert_eq!(ClipboardPdu::decode(&mut cursor).unwrap(), pdu);
                assert_eq!(cursor.len(), 0, "decoder must consume the optional padding");

                let mut bad_flags = bytes.clone();
                bad_flags[2..4].copy_from_slice(&1_u16.to_le_bytes());
                assert!(decode::<ClipboardPdu<'_>>(&bad_flags).is_err());
                for length in [0_u32, 1, 3, 5, 23, 25, 26, 27, 29, 31, 32, u32::MAX] {
                    let mut bad_length = bytes.clone();
                    bad_length[4..8].copy_from_slice(&length.to_le_bytes());
                    assert!(
                        decode::<ClipboardPdu<'_>>(&bad_length).is_err(),
                        "accepted declared length {length} for {pdu:?}"
                    );
                }
            }
            for length in 0..base.len() {
                assert!(decode::<ClipboardPdu<'_>>(&base[..length]).is_err());
            }
            for extra in [1_usize, 2, 3, 5, 8] {
                let mut bad_padding = base.clone();
                bad_padding.extend(std::iter::repeat_n(0xa5, extra));
                assert!(decode::<ClipboardPdu<'_>>(&bad_padding).is_err());
            }
        }
    }

    #[test]
    fn empty_clipboard_locks_do_not_seal_or_bind_old_ids_to_new_files() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcProcessor;

        for stale_cache in [false, true] {
            for unlock in [false, true] {
                for padded in [false, true] {
                    let state = new_shared();
                    let mut client = synthetic_ready_client(&state);
                    if stale_cache {
                        install_synthetic_file(&state, &mut client, 7, "old.bin", false);
                        offer_local_files(&state, 0, Vec::new());
                        assert!(state.lock().unwrap().cached_file_snapshot.is_some());
                    }
                    let lock = ClipboardPdu::LockData(LockDataId(90));
                    client.process(&synthetic_pdu_bytes(&lock, padded)).unwrap();
                    if unlock {
                        let unlock = ClipboardPdu::UnlockData(LockDataId(90));
                        client
                            .process(&synthetic_pdu_bytes(&unlock, padded))
                            .unwrap();
                    }
                    {
                        let guard = state.lock().unwrap();
                        assert_eq!(guard.local_file_direction, LocalFileDirection::Fresh);
                        assert!(!guard.local_file_suspended);
                        assert!(guard.local_file_locks.is_empty());
                    }

                    install_synthetic_file(&state, &mut client, 8, "new.bin", true);
                    assert!(actions(&state).iter().any(|action| matches!(
                        action, ClipboardAction::AdvertiseFiles(files, _) if files[0].name == "new.bin"
                    )));
                    let request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
                        stream_id: 41,
                        index: 0,
                        flags: FileContentsFlags::SIZE,
                        position: 0,
                        requested_size: 8,
                        data_id: Some(90),
                    });
                    client
                        .process(&synthetic_pdu_bytes(&request, padded))
                        .unwrap();
                    let guard = state.lock().unwrap();
                    assert!(guard.local_file_suspended);
                    assert!(guard.local_file_locks.is_empty());
                    assert!(guard.pending_local_reads.is_empty());
                    assert!(!guard.actions.iter().any(|action| matches!(
                        action,
                        ClipboardAction::NeedFileContents { .. }
                            | ClipboardAction::SubmitFileContents { data: Some(_), .. }
                    )));
                }
            }
        }
    }

    #[test]
    fn empty_lock_unlock_allows_a_new_lock_and_padded_file_requests() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcProcessor;

        for new_lock_id in [90_u32, 91] {
            let state = new_shared();
            let mut client = synthetic_ready_client(&state);
            for pdu in [
                ClipboardPdu::LockData(LockDataId(90)),
                ClipboardPdu::UnlockData(LockDataId(90)),
            ] {
                client.process(&synthetic_pdu_bytes(&pdu, true)).unwrap();
            }
            install_synthetic_file(&state, &mut client, 7, "new.bin", true);
            let _ = actions(&state);

            let lock = ClipboardPdu::LockData(LockDataId(new_lock_id));
            client.process(&synthetic_pdu_bytes(&lock, true)).unwrap();
            assert!(state
                .lock()
                .unwrap()
                .local_file_locks
                .contains_key(&new_lock_id));
            for (stream_id, data_id) in [(41, None), (42, Some(new_lock_id))] {
                let request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
                    stream_id,
                    index: 0,
                    flags: FileContentsFlags::SIZE,
                    position: 0,
                    requested_size: 8,
                    data_id,
                });
                client
                    .process(&synthetic_pdu_bytes(&request, true))
                    .unwrap();
            }
            let unlock = ClipboardPdu::UnlockData(LockDataId(new_lock_id));
            client.process(&synthetic_pdu_bytes(&unlock, true)).unwrap();
            let guard = state.lock().unwrap();
            assert!(!guard.local_file_suspended);
            assert!(guard.local_file_locks.is_empty());
            for stream_id in [41, 42] {
                assert!(guard.actions.iter().any(|action| matches!(action,
                    ClipboardAction::SubmitFileContents { stream_id: id, data: Some(bytes), .. }
                        if *id == stream_id && bytes.as_slice() == &4_u64.to_le_bytes()[..])));
            }
        }
    }

    #[tokio::test]
    async fn malformed_duplicate_requests_discard_prepared_replies_and_keep_desktop_usable() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::image::DecodedImage;

        for malformed in malformed_file_requests(41) {
            let (host, receiver) = crate::host::host_channel();
            let (state, mut stage) = descriptor_stage(
                vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 4,
                    is_dir: false,
                }],
                &host,
            )
            .await;
            let packet = |bytes| {
                ironrdp_svc::server_encode_svc_messages(
                    vec![ironrdp_svc::SvcMessage::from(bytes)],
                    1004,
                    1001,
                )
                .unwrap()
            };
            let mut first = wire_file_request(41);
            first[12..16].copy_from_slice(&99_i32.to_le_bytes());
            let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
            stage
                .process(&mut image, ironrdp_pdu::Action::X224, &packet(first))
                .unwrap();
            let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
                .await
                .unwrap();
            assert_eq!(frames.len(), 1);
            let (bytes, _) = decode_one_clipboard_message(&frames[0].bytes);
            assert!(matches!(decode::<ClipboardPdu<'_>>(&bytes).unwrap(),
                ClipboardPdu::FileContentsResponse(reply) if reply.is_error()));
            stage
                .process(&mut image, ironrdp_pdu::Action::X224, &packet(malformed))
                .unwrap();
            assert!(state.lock().unwrap().local_file_suspended);
            let (mut wire, _peer) = tokio::io::duplex(4096);
            assert_eq!(
                commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                    .await
                    .unwrap(),
                ClipboardCommit::Discarded
            );
            flush_local_file_direction_notices(&state, &host);
            assert!(matches!(
                receiver.try_recv().unwrap().message,
                HostMsg::LocalFileDirectionSuspended { .. }
            ));
            assert_video_and_other_svc_still_work(&mut stage);
        }
    }

    #[test]
    fn malformed_unique_requests_are_reserved_and_unattributed_requests_suspend() {
        use ironrdp_svc::SvcProcessor;
        for malformed in malformed_file_requests(41) {
            for duplicate in [malformed.clone(), wire_file_request(41)] {
                let state = new_shared();
                let mut client =
                    CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
                client.process(&malformed).unwrap();
                let guard = state.lock().unwrap();
                assert!(!guard.local_file_suspended);
                assert!(guard.local_file_streams.contains(&41));
                assert!(guard.actions.iter().any(|action| matches!(
                    action,
                    ClipboardAction::SubmitFileContents {
                        stream_id: 41,
                        data: None,
                        ..
                    }
                )));
                drop(guard);
                client.process(&duplicate).unwrap();
                let guard = state.lock().unwrap();
                assert!(guard.local_file_suspended);
                assert!(guard.local_file_suspension_ack.is_some());
                assert!(!guard.actions_overloaded);
                assert!(guard
                    .actions
                    .iter()
                    .all(|action| !action.is_local_file_action()));
            }
        }
        let valid = wire_file_request(41);
        let mut unattributed: Vec<Vec<u8>> =
            (0..12).map(|length| valid[..length].to_vec()).collect();
        for length in 0..4_u32 {
            let mut bytes = valid.clone();
            bytes[4..8].copy_from_slice(&length.to_le_bytes());
            unattributed.push(bytes);
        }
        for malformed in unattributed {
            let state = new_shared();
            let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
            client.process(&malformed).unwrap();
            let guard = state.lock().unwrap();
            assert!(guard.local_file_suspended);
            assert!(guard.local_file_suspension_ack.is_some());
            assert!(!guard.actions_overloaded);
        }
    }

    #[test]
    fn malformed_request_flood_obeys_the_same_stream_budget() {
        use ironrdp_svc::SvcProcessor;
        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
        for stream_id in 0..=MAX_QUEUED_ACTIONS as u32 {
            let mut malformed = wire_file_request(stream_id);
            malformed[12..16].copy_from_slice(&(-1_i32).to_le_bytes());
            client.process(&malformed).unwrap();
        }
        let guard = state.lock().unwrap();
        assert!(guard.local_file_suspended);
        assert!(guard.local_file_suspension_ack.is_some());
        assert!(!guard.actions_overloaded);
        assert!(guard.local_file_streams.is_empty());
    }

    #[test]
    fn malformed_locks_do_not_bind_or_disconnect() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcProcessor;

        for pdu in [
            ClipboardPdu::LockData(LockDataId(90)),
            ClipboardPdu::UnlockData(LockDataId(90)),
        ] {
            let valid = ironrdp_core::encode_vec(&pdu).unwrap();
            let mut malformed: Vec<Vec<u8>> = (0..valid.len())
                .map(|length| valid[..length].to_vec())
                .collect();
            for length in [0_u32, 3, 5, u32::MAX] {
                let mut bytes = valid.clone();
                bytes[4..8].copy_from_slice(&length.to_le_bytes());
                malformed.push(bytes.clone());
                bytes.extend_from_slice(&[0x5a, 0xa5, 0x7c, 0xe1]);
                malformed.push(bytes);
            }
            let mut bad_flags = valid.clone();
            bad_flags[2..4].copy_from_slice(&1_u16.to_le_bytes());
            malformed.push(bad_flags);
            let mut trailing = valid.clone();
            trailing.push(0);
            malformed.push(trailing);
            for extra in [2_usize, 3, 5, 8] {
                let mut bytes = valid.clone();
                bytes.extend(std::iter::repeat_n(0xa5, extra));
                malformed.push(bytes);
            }
            for bytes in malformed {
                let state = new_shared();
                let mut client =
                    CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
                client.process(&bytes).unwrap();
                let guard = state.lock().unwrap();
                assert!(guard.local_file_suspended);
                assert!(guard.local_file_locks.is_empty());
                assert!(guard.local_file_suspension_ack.is_some());
                assert!(!guard.actions_overloaded);
            }
        }
    }

    #[test]
    fn lock_without_an_ironrdp_snapshot_suspends_after_backend_admission() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcProcessor;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
            guard.local_files = vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }];
            guard.local_file_generation = 7;
            let snapshot = FileSnapshot {
                channel_epoch: guard.channel_epoch,
                generation: guard.local_file_generation,
                revision: guard.local_file_revision,
            };
            guard.cached_file_snapshot = Some(snapshot);
            guard.local_file_direction = LocalFileDirection::Exposed(snapshot);
        }
        let lock = ClipboardPdu::LockData(LockDataId(90));
        client
            .process(&ironrdp_core::encode_vec(&lock).unwrap())
            .unwrap();
        let guard = state.lock().unwrap();
        assert!(guard.local_file_suspended);
        assert!(guard.local_file_locks.is_empty());
        assert!(guard.local_file_suspension_ack.is_some());
    }

    #[tokio::test]
    async fn malformed_unique_request_fails_on_wire_and_releases_its_stream() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcProcessor;

        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let mut malformed = wire_file_request(41);
        malformed[12..16].copy_from_slice(&(-1_i32).to_le_bytes());
        stage
            .get_svc_processor_mut::<CliprdrClient>()
            .unwrap()
            .process(&malformed)
            .unwrap();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (bytes, _) = decode_one_clipboard_message(&frames[0].bytes);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&bytes).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == 41 && reply.is_error()));
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(!state.lock().unwrap().local_file_streams.contains(&41));
        stage
            .get_svc_processor_mut::<CliprdrClient>()
            .unwrap()
            .process(&wire_file_request(41))
            .unwrap();
        let guard = state.lock().unwrap();
        assert!(!guard.local_file_suspended);
        assert!(guard.actions.iter().any(|action| matches!(
            action,
            ClipboardAction::SubmitFileContents {
                stream_id: 41,
                data: Some(_),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn unmatched_lock_snapshot_and_lock_limit_suspend_without_closing_desktop() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcProcessor;

        let (host, receiver) = crate::host::host_channel();
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 4,
            is_dir: false,
        };
        let (state, mut stage) = descriptor_stage(vec![file("old.bin")], &host).await;
        // The old encoded offer was not exposed. A lock must not be rebound to
        // the newer local generation while IronRDP still caches the old one.
        offer_local_files(&state, 8, vec![file("new.bin")]);
        let lock = ironrdp_core::encode_vec(&ClipboardPdu::LockData(LockDataId(90))).unwrap();
        stage
            .get_svc_processor_mut::<CliprdrClient>()
            .unwrap()
            .process(&lock)
            .unwrap();
        assert!(state.lock().unwrap().local_file_suspended);
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
        assert_video_and_other_svc_still_work(&mut stage);

        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(vec![file("current.bin")], &host).await;
        state.lock().unwrap().queue_advertise_files();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        for id in (0..100).chain([0]) {
            let lock = ironrdp_core::encode_vec(&ClipboardPdu::LockData(LockDataId(id))).unwrap();
            stage
                .get_svc_processor_mut::<CliprdrClient>()
                .unwrap()
                .process(&lock)
                .unwrap();
        }
        assert_eq!(state.lock().unwrap().local_file_locks.len(), 100);
        let lock = ironrdp_core::encode_vec(&ClipboardPdu::LockData(LockDataId(100))).unwrap();
        stage
            .get_svc_processor_mut::<CliprdrClient>()
            .unwrap()
            .process(&lock)
            .unwrap();
        {
            let guard = state.lock().unwrap();
            assert!(guard.local_file_suspended);
            assert!(guard.local_file_locks.is_empty());
            assert!(!guard.actions_overloaded);
        }
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
        assert_video_and_other_svc_still_work(&mut stage);
    }

    #[test]
    fn snapshot_budget_counts_shared_locks_once_and_suspends_on_new_generation() {
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatListResponse,
        };
        use ironrdp_svc::SvcProcessor;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(state.clone())));
        for pdu in [
            ClipboardPdu::Capabilities(Capabilities::new(
                ClipboardProtocolVersion::V2,
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                    | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
                    | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
            )),
            ClipboardPdu::FormatListResponse(FormatListResponse::Ok),
        ] {
            client
                .process(&ironrdp_core::encode_vec(&pdu).unwrap())
                .unwrap();
        }
        let mut large_name = String::with_capacity(128 * 1024 * 1024 - 4096);
        large_name.push_str("first.bin");
        client
            .initiate_file_copy(vec![FileDescriptor::new(large_name)])
            .unwrap();
        {
            let mut guard = state.lock().unwrap();
            guard.local_files = vec![ClipFile {
                name: "first.bin".into(),
                size: 1,
                is_dir: false,
            }];
            guard.local_file_generation = 7;
            let snapshot = FileSnapshot {
                channel_epoch: guard.channel_epoch,
                generation: guard.local_file_generation,
                revision: guard.local_file_revision,
            };
            guard.cached_file_snapshot = Some(snapshot);
            guard.local_file_direction = LocalFileDirection::Exposed(snapshot);
        }
        for id in 0..MAX_LOCAL_FILE_LOCKS {
            let lock = ClipboardPdu::LockData(LockDataId(u32::try_from(id).unwrap()));
            client
                .process(&ironrdp_core::encode_vec(&lock).unwrap())
                .unwrap();
        }
        assert_eq!(
            state.lock().unwrap().local_file_locks.len(),
            MAX_LOCAL_FILE_LOCKS
        );
        let mut next_name = String::with_capacity(8192);
        next_name.push_str("next.bin");
        assert!(client
            .initiate_file_copy(vec![FileDescriptor::new(next_name)])
            .is_err());
        {
            let guard = state.lock().unwrap();
            assert!(guard.local_file_suspended);
            assert!(guard.local_file_locks.is_empty());
            assert!(!guard.actions_overloaded);
        }
        let (host, receiver) = crate::host::host_channel();
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
    }

    #[tokio::test]
    async fn duplicate_wire_requests_discard_even_prepared_failures_and_keep_desktop_usable() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_session::image::DecodedImage;

        for (first_index, duplicate_index) in [(0, 0), (0, 99), (99, 0), (99, 99)] {
            let (host, receiver) = crate::host::host_channel();
            let (state, mut stage) = descriptor_stage(
                vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 4,
                    is_dir: false,
                }],
                &host,
            )
            .await;
            let packet = |index| {
                let request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
                    stream_id: 41,
                    index,
                    flags: FileContentsFlags::SIZE,
                    position: 0,
                    requested_size: 8,
                    data_id: None,
                });
                ironrdp_svc::server_encode_svc_messages(
                    vec![ironrdp_svc::SvcMessage::from(
                        ironrdp_core::encode_vec(&request).unwrap(),
                    )],
                    1004,
                    1001,
                )
                .unwrap()
            };
            let first_packet = packet(first_index);
            let duplicate_packet = packet(duplicate_index);
            let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
            let output = stage
                .process(&mut image, ironrdp_pdu::Action::X224, &first_packet)
                .unwrap();
            assert!(output.iter().all(|item| !matches!(item, ironrdp_session::ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
            let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
                .await
                .unwrap();
            assert_eq!(frames.len(), 1);
            let output = stage
                .process(&mut image, ironrdp_pdu::Action::X224, &duplicate_packet)
                .unwrap();
            assert!(output.iter().all(|item| !matches!(item, ironrdp_session::ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
            for _ in 0..1000 {
                stage
                    .process(&mut image, ironrdp_pdu::Action::X224, &duplicate_packet)
                    .unwrap();
            }
            assert!(state.lock().unwrap().local_file_suspended);
            let (mut wire, _peer) = tokio::io::duplex(4096);
            assert_eq!(
                commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                    .await
                    .unwrap(),
                ClipboardCommit::Discarded
            );
            flush_local_file_direction_notices(&state, &host);
            assert!(matches!(
                receiver.try_recv().unwrap().message,
                HostMsg::LocalFileDirectionSuspended { .. }
            ));
            assert!(receiver.try_recv().is_err());
            assert_video_and_other_svc_still_work(&mut stage);

            let generation = {
                let mut guard = state.lock().unwrap();
                guard.remote_files.revoke(true);
                assert!(guard.remote_files.install(
                    vec![ClipFile {
                        name: "remote.bin".into(),
                        size: 4,
                        is_dir: false,
                    }],
                    Some(77)
                ));
                guard.remote_files.generation()
            };
            request_file_contents(&state, generation, 42, 0, 0, 1);
            assert!(state
                .lock()
                .unwrap()
                .actions
                .iter()
                .any(|action| matches!(action, ClipboardAction::RequestFileContents(_))));
        }
    }

    #[tokio::test]
    async fn queued_suspension_blocks_a_prepared_failure_before_its_first_byte() {
        use crate::session::{
            commit_clipboard_frame, handle_handshake_command, ClipboardCommit, SessionCmd,
        };
        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 1,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let mut backend = AditCliprdrBackend {
            state: state.clone(),
        };
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 41,
            index: 99,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        });
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (sender, mut input) = crate::host::input_channel();
        assert!(sender.send(SessionCmd::SuspendLocalFileDirection));
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Deferred
        );
        let command = input.commands.try_recv().unwrap().command;
        handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Discarded
        );
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
        assert_video_and_other_svc_still_work(&mut stage);
    }

    #[test]
    fn new_channel_announces_file_recovery_only_after_ready_and_capabilities() {
        let (_backend, state) = backend();
        state.lock().unwrap().seal_local_file_direction();
        let (host, receiver) = crate::host::host_channel();
        flush_local_file_direction_notices(&state, &host);
        let old_epoch = match receiver.try_recv().unwrap().message {
            HostMsg::ClipboardFileDirectionUnavailable { channel_epoch, .. } => channel_epoch,
            other => panic!("unexpected notice: {other:?}"),
        };
        let mut backend = AditCliprdrBackend::new(state.clone());
        backend.on_ready();
        flush_local_file_direction_notices(&state, &host);
        assert!(receiver.try_recv().is_err());
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardLocalFileDirectionReady { channel_epoch } if channel_epoch > old_epoch));

        suspend_local_file_direction(&state);
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(
            receiver.try_recv().unwrap().message,
            HostMsg::LocalFileDirectionSuspended { .. }
        ));
        let mut redirected = AditCliprdrBackend::new(state.clone());
        redirected.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        redirected.on_ready();
        flush_local_file_direction_notices(&state, &host);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn helper_seals_an_exposed_file_generation_until_a_new_channel() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;

        let file = |name: &str, size| ClipFile {
            name: name.into(),
            size,
            is_dir: false,
        };
        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(vec![file("a.txt", 4)], &host).await;
        let mut backend = AditCliprdrBackend {
            state: Arc::clone(&state),
        };
        // The fixture builds a ready channel; put its offer on a synthetic
        // transport and invoke the writer hook at the transport boundary.
        state.lock().unwrap().queue_advertise_files();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(matches!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Exposed(_)
        ));

        let request = |stream_id, flags, data_id| FileContentsRequest {
            stream_id,
            index: 0,
            flags,
            position: 0,
            requested_size: if flags.contains(FileContentsFlags::SIZE) {
                8
            } else {
                4
            },
            data_id,
        };
        assert!(backend.allow_incoming_file_lock(&LockDataId(90)));
        backend.on_lock(LockDataId(90));
        backend.on_file_contents_request(request(11, FileContentsFlags::RANGE, None));
        let old_read = requested_read(actions(&state));
        submit_file_contents(&state, old_read, Some(vec![9, 8, 7, 6]));
        backend.on_file_contents_request(request(12, FileContentsFlags::SIZE, Some(90)));
        let mut old_replies = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(old_replies.len(), 2);
        backend.on_file_contents_request(request(13, FileContentsFlags::RANGE, None));
        let outstanding = requested_read(actions(&state));

        // Reusing both the generation and the old lock id cannot identify B.
        offer_local_files(&state, 7, vec![file("b.txt", 5)]);
        assert_eq!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Sealed
        );
        let queued: Vec<_> = state.lock().unwrap().actions.iter().cloned().collect();
        assert!(!queued
            .iter()
            .any(|action| matches!(action, ClipboardAction::AdvertiseFiles(..))));
        assert!(queued.iter().any(|action| matches!(action,
            ClipboardAction::Advertise(formats) if formats.is_empty())));
        for frame in &mut old_replies {
            assert!(revalidate_wire_frame(frame, &state, &mut stage).unwrap());
            let (bytes, _) = decode_one_clipboard_message(&frame.bytes);
            assert!(matches!(decode::<ClipboardPdu<'_>>(&bytes).unwrap(),
                ClipboardPdu::FileContentsResponse(reply) if reply.is_error()));
        }
        submit_file_contents(&state, outstanding, Some(vec![1, 2, 3, 4]));
        assert!(!backend.allow_incoming_file_lock(&LockDataId(90)));
        backend.on_lock(LockDataId(90));
        backend.on_lock(LockDataId(91));
        let lock = ClipboardPdu::LockData(LockDataId(90));
        let lock_bytes = ironrdp_core::encode_vec(&lock).unwrap();
        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![ironrdp_svc::SvcMessage::from(lock_bytes)],
            1004,
            1001,
        )
        .unwrap();
        let mut image = ironrdp_session::image::DecodedImage::new(
            ironrdp_graphics::image_processing::PixelFormat::RgbA32,
            1,
            1,
        );
        let lock_output = stage
            .process(&mut image, ironrdp_pdu::Action::X224, &packet)
            .unwrap();
        assert!(lock_output.iter().all(|item| !matches!(item,
            ironrdp_session::ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
        assert!(state.lock().unwrap().local_file_locks.is_empty());
        for request in [
            request(20, FileContentsFlags::SIZE, None),
            request(21, FileContentsFlags::RANGE, None),
            request(22, FileContentsFlags::RANGE, Some(90)),
        ] {
            let pdu = ClipboardPdu::FileContentsRequest(request);
            let packet = ironrdp_svc::server_encode_svc_messages(
                vec![ironrdp_svc::SvcMessage::from(
                    ironrdp_core::encode_vec(&pdu).unwrap(),
                )],
                1004,
                1001,
            )
            .unwrap();
            stage
                .process(&mut image, ironrdp_pdu::Action::X224, &packet)
                .unwrap();
        }
        let descriptor_output = process_format_request(&mut stage, 0xC0FE);
        assert!(descriptor_output.iter().all(|item| !matches!(item,
            ironrdp_session::ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
        let rejected: Vec<_> = state.lock().unwrap().actions.iter().cloned().collect();
        assert!(rejected.iter().any(|action| matches!(
            action,
            ClipboardAction::SubmitFileContents {
                stream_id: 13,
                data: None,
                ..
            }
        )));
        for stream_id in [20, 21, 22] {
            assert!(rejected.iter().any(|action| matches!(action,
                ClipboardAction::RefuseClosedFileContents { stream_id: id, .. } if *id == stream_id)));
        }
        assert!(rejected.iter().any(|action| matches!(action,
            ClipboardAction::SubmitFileDescriptor { response, .. } if response.is_error())));
        assert!(!rejected.iter().any(|action| matches!(
            action,
            ClipboardAction::NeedFileContents { .. } | ClipboardAction::AdvertiseFiles(..)
        )));
        let mut pending = VecDeque::new();
        let (mut rejection_wire, mut rejection_peer) = tokio::io::duplex(4096);
        crate::session::flush_clipboard_output(
            &mut rejection_wire,
            &mut pending,
            &state,
            &mut stage,
            &host,
            None,
        )
        .await
        .unwrap();
        let HostMsg::ClipboardFileDirectionUnavailable {
            channel_epoch,
            message,
        } = receiver.try_recv().unwrap().message
        else {
            panic!("expected the nonfatal file-direction notice");
        };
        assert_eq!(channel_epoch, state.lock().unwrap().channel_epoch);
        assert!(message.contains("暂时禁用"));
        assert!(message.contains("仍可能到达"));
        assert!(local_file_direction_notice(&state).is_none());
        drop(rejection_wire);
        let mut rejection_bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut rejection_peer, &mut rejection_bytes)
            .await
            .unwrap();
        let messages = decode_clipboard_messages(&rejection_bytes);
        for stream_id in [13, 20, 21, 22] {
            assert!(messages.iter().any(|bytes| matches!(decode::<ClipboardPdu<'_>>(bytes).unwrap(),
                ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == stream_id && reply.is_error())));
        }
        assert!(messages
            .iter()
            .any(|bytes| matches!(decode::<ClipboardPdu<'_>>(bytes).unwrap(),
            ClipboardPdu::FormatDataResponse(reply) if reply.is_error())));
        assert!(messages
            .iter()
            .any(|bytes| matches!(decode::<ClipboardPdu<'_>>(bytes).unwrap(),
            ClipboardPdu::FormatList(list) if list.get_formats(true).unwrap().is_empty())));
        assert!(receiver.try_recv().is_err());

        offer_local_text(&state, "仍可复制文本".into());
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert!(actions(&state).iter().any(|action| matches!(action,
            ClipboardAction::SubmitText { text: Some(text), .. } if text == "仍可复制文本")));
        offer_local_image(&state, vec![1, 2, 3]);
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        assert!(actions(&state).iter().any(|action| matches!(action,
            ClipboardAction::SubmitImage { image: Some(image), .. } if image == &[1, 2, 3])));
        assert_video_and_other_svc_still_work(&mut stage);

        // A new CLIPRDR processor and channel are the only way out of Sealed.
        drop(stage);
        drop(backend);
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
        let _ = actions(&state);
        assert_eq!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Fresh
        );
        use ironrdp_cliprdr::pdu::{Capabilities, ClipboardProtocolVersion, FormatListResponse};
        use ironrdp_svc::{StaticChannelSet, SvcProcessor};
        use std::any::TypeId;
        for pdu in [
            ClipboardPdu::Capabilities(Capabilities::new(
                ClipboardProtocolVersion::V2,
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                    | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
                    | ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES,
            )),
            ClipboardPdu::FormatListResponse(FormatListResponse::Ok),
        ] {
            client
                .process(&ironrdp_core::encode_vec(&pdu).unwrap())
                .unwrap();
        }
        let _ = actions(&state);
        let mut channels = StaticChannelSet::new();
        channels.insert(client);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut next_stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        offer_local_files(&state, 7, vec![file("b.txt", 5)]);
        let mut next_frames = pump_with_pending_input(&state, &mut next_stage, &host, None)
            .await
            .unwrap();
        assert_eq!(next_frames.len(), 1);
        let (mut next_wire, _next_peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(
                &mut next_wire,
                &mut next_frames[0],
                &state,
                &mut next_stage,
                None
            )
            .await
            .unwrap(),
            ClipboardCommit::Written
        );
        for pdu in [
            ClipboardPdu::LockData(LockDataId(90)),
            ClipboardPdu::FileContentsRequest(request(30, FileContentsFlags::SIZE, Some(90))),
        ] {
            let packet = ironrdp_svc::server_encode_svc_messages(
                vec![ironrdp_svc::SvcMessage::from(
                    ironrdp_core::encode_vec(&pdu).unwrap(),
                )],
                1004,
                1001,
            )
            .unwrap();
            next_stage
                .process(&mut image, ironrdp_pdu::Action::X224, &packet)
                .unwrap();
        }
        let replies = pump_with_pending_input(&state, &mut next_stage, &host, None)
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        let (bytes, _) = decode_one_clipboard_message(&replies[0].bytes);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&bytes).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if !reply.is_error() && reply.data() == 5_u64.to_le_bytes()));
    }

    #[test]
    fn replacing_an_offer_before_the_transport_accepts_it_does_not_seal() {
        let (mut backend, state) = backend();
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        backend.on_ready();
        let _ = actions(&state);
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 1,
            is_dir: false,
        };
        offer_local_files(&state, 7, vec![file("a.txt")]);
        offer_local_files(&state, 7, vec![file("b.txt")]);
        assert_eq!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Fresh
        );
        assert!(actions(&state).contains(&file_offer(&state, vec![file("b.txt")])));
        assert!(local_file_direction_notice(&state).is_none());
    }

    #[test]
    fn nonfatal_notice_is_retained_until_enqueued_even_across_channel_reset() {
        let state = new_shared();
        {
            let mut guard = state.lock().unwrap();
            guard.reset_for_new_channel();
            guard.seal_local_file_direction();
        }
        let epoch = local_file_direction_notice(&state).unwrap().0;
        let (broken, receiver) = crate::host::host_channel();
        drop(receiver);
        flush_local_file_direction_notices(&state, &broken);
        assert_eq!(local_file_direction_notice(&state).unwrap().0, epoch);
        let later_epoch = {
            let mut guard = state.lock().unwrap();
            guard.reset_for_new_channel();
            guard.seal_local_file_direction();
            guard.channel_epoch
        };
        acknowledge_local_file_direction_notice(&state, later_epoch);
        assert_eq!(local_file_direction_notice(&state).unwrap().0, epoch);
        let (host, receiver) = crate::host::host_channel();
        flush_local_file_direction_notices(&state, &host);
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardFileDirectionUnavailable { channel_epoch, message }
                if channel_epoch == epoch && message.contains("暂时禁用")));
        assert!(matches!(receiver.try_recv().unwrap().message,
            HostMsg::ClipboardFileDirectionUnavailable { channel_epoch, .. }
                if channel_epoch == later_epoch));
        assert!(local_file_direction_notice(&state).is_none());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn nonfatal_notice_is_not_starved_by_a_full_video_queue() {
        let state = new_shared();
        state.lock().unwrap().seal_local_file_direction();
        let (host, receiver) = crate::host::host_channel();
        let mut full = false;
        for _ in 0..4096 {
            if host
                .send(HostMsg::Tile {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    rgba: vec![1, 2, 3, 255],
                })
                .is_err()
            {
                full = true;
                break;
            }
        }
        assert!(full);
        flush_local_file_direction_notices(&state, &host);
        assert!(local_file_direction_notice(&state).is_none());
        let messages: Vec<_> = receiver.try_iter().map(|queued| queued.message).collect();
        assert!(messages
            .iter()
            .any(|message| matches!(message, HostMsg::ClipboardFileDirectionUnavailable { .. })));
        assert!(!messages
            .iter()
            .any(|message| matches!(message, HostMsg::Error(_) | HostMsg::Closed)));
    }

    #[tokio::test]
    async fn first_accepted_byte_seals_replaced_files_without_truncating_the_pdu() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use std::task::Poll;

        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 1,
            is_dir: false,
        };
        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(vec![file("a.txt")], &host).await;
        state.lock().unwrap().queue_advertise_files();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let length = frames[0].bytes.len();
        let (mut wire, mut peer) = tokio::io::duplex(1);
        let committing =
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None);
        tokio::pin!(committing);
        std::future::poll_fn(|cx| {
            assert!(matches!(
                std::future::Future::poll(committing.as_mut(), cx),
                Poll::Pending
            ));
            Poll::Ready(())
        })
        .await;
        let mut first = [0];
        tokio::io::AsyncReadExt::read_exact(&mut peer, &mut first)
            .await
            .unwrap();
        assert_eq!(first, [3]); // TPKT has begun; the rest is still blocked.
        offer_local_files(&state, 7, vec![file("b.txt")]);
        assert_eq!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Sealed
        );
        let receive = async {
            let mut rest = vec![0; length - 1];
            tokio::io::AsyncReadExt::read_exact(&mut peer, &mut rest)
                .await
                .unwrap();
            rest
        };
        let (result, rest) = tokio::join!(committing, receive);
        assert_eq!(result.unwrap(), ClipboardCommit::Written);
        let mut observed = first.to_vec();
        observed.extend(rest);
        let (pdu, _) = decode_one_clipboard_message(&observed);
        assert!(matches!(
            decode::<ClipboardPdu<'_>>(&pdu).unwrap(),
            ClipboardPdu::FormatList(_)
        ));
        assert!(local_file_direction_notice(&state).is_some());
    }

    #[tokio::test]
    async fn file_copy_upstream_returns_unlocks_before_the_offer_and_forgets_the_locks() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::SvcMessage;

        for lock_count in [0, 1, 2] {
            let (host, _receiver) = crate::host::host_channel();
            let (state, mut stage) = descriptor_stage(
                vec![ClipFile {
                    name: "initial.txt".into(),
                    size: 1,
                    is_dir: false,
                }],
                &host,
            )
            .await;
            let mut locks = remote_file_locks_on_wire(&mut stage, lock_count);
            let _ = actions(&state);
            let file = ClipFile {
                name: "local.txt".into(),
                size: 4,
                is_dir: false,
            };
            let client = stage.get_svc_processor_mut::<CliprdrClient>().unwrap();
            let messages: Vec<SvcMessage> = client
                .initiate_file_copy(vec![to_descriptor(&file)])
                .unwrap()
                .into();
            assert_eq!(messages.len(), lock_count + 1);
            let mut unlocked = Vec::new();
            for message in &messages[..lock_count] {
                let bytes = message.encode_unframed_pdu().unwrap();
                let ClipboardPdu::UnlockData(id) = decode::<ClipboardPdu<'_>>(&bytes).unwrap()
                else {
                    panic!("expected UnlockData before the file offer");
                };
                unlocked.push(id.0);
            }
            locks.sort_unstable();
            unlocked.sort_unstable();
            assert_eq!(unlocked, locks);
            let bytes = messages[lock_count].encode_unframed_pdu().unwrap();
            assert!(matches!(
                decode::<ClipboardPdu<'_>>(&bytes).unwrap(),
                ClipboardPdu::FormatList(_)
            ));

            let next: Vec<SvcMessage> = client
                .initiate_file_copy(vec![to_descriptor(&file)])
                .unwrap()
                .into();
            assert_eq!(next.len(), 1, "the prior locks are no longer tracked");
            let bytes = next[0].encode_unframed_pdu().unwrap();
            assert!(matches!(
                decode::<ClipboardPdu<'_>>(&bytes).unwrap(),
                ClipboardPdu::FormatList(_)
            ));
        }
    }

    #[tokio::test]
    async fn unlocks_precede_a_current_file_offer_on_the_wire() {
        use crate::session::flush_clipboard_output;
        use ironrdp_cliprdr::pdu::ClipboardPdu;

        for lock_count in [0, 1, 2] {
            let (host, _receiver) = crate::host::host_channel();
            let (state, mut stage) = descriptor_stage(
                vec![ClipFile {
                    name: "initial.txt".into(),
                    size: 1,
                    is_dir: false,
                }],
                &host,
            )
            .await;
            let mut locks = remote_file_locks_on_wire(&mut stage, lock_count);
            let _ = actions(&state);
            offer_local_files(
                &state,
                8,
                vec![ClipFile {
                    name: "current.txt".into(),
                    size: 4,
                    is_dir: false,
                }],
            );
            let mut pending = VecDeque::new();
            let (mut wire, mut peer) = tokio::io::duplex(4096);
            flush_clipboard_output(&mut wire, &mut pending, &state, &mut stage, &host, None)
                .await
                .unwrap();
            drop(wire);
            let mut observed = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
                .await
                .unwrap();
            let messages = decode_clipboard_messages(&observed);
            assert_eq!(messages.len(), lock_count + 1);
            let mut unlocked = Vec::new();
            for message in &messages[..lock_count] {
                let ClipboardPdu::UnlockData(id) = decode::<ClipboardPdu<'_>>(message).unwrap()
                else {
                    panic!("expected UnlockData before the current offer");
                };
                unlocked.push(id.0);
            }
            locks.sort_unstable();
            unlocked.sort_unstable();
            assert_eq!(unlocked, locks);
            let ClipboardPdu::FormatList(list) =
                decode::<ClipboardPdu<'_>>(&messages[lock_count]).unwrap()
            else {
                panic!("expected the current file offer");
            };
            assert!(list.get_formats(true).unwrap().iter().any(|format| {
                format
                    .name()
                    .is_some_and(|name| name.value() == FILE_LIST_FORMAT)
            }));
        }
    }

    #[tokio::test]
    async fn revocation_between_unlock_and_offer_does_not_send_the_stale_offer() {
        use crate::session::{flush_clipboard_output, handle_handshake_command, SessionCmd};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_svc::{ChannelFlags, SvcMessage};
        use std::task::Poll;

        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "initial.txt".into(),
                size: 1,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let lock_id = remote_file_locks_on_wire(&mut stage, 1)[0];
        let _ = actions(&state);
        offer_local_files(
            &state,
            8,
            vec![ClipFile {
                name: "revoked.txt".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let mut pending = VecDeque::from(
            pump_with_pending_input(&state, &mut stage, &host, None)
                .await
                .unwrap(),
        );
        let unlock = ClipboardPdu::UnlockData(LockDataId(lock_id));
        let unlock_bytes = ironrdp_core::encode_vec(&unlock).unwrap();
        let expected = ironrdp_svc::client_encode_svc_messages(
            vec![SvcMessage::from(unlock_bytes).with_flags(ChannelFlags::SHOW_PROTOCOL)],
            1004,
            1001,
        )
        .unwrap();
        let (sender, mut input) = crate::host::input_channel();
        let (mut wire, mut peer) = tokio::io::duplex(expected.len());
        {
            let flushing = flush_clipboard_output(
                &mut wire,
                &mut pending,
                &state,
                &mut stage,
                &host,
                Some(&input),
            );
            tokio::pin!(flushing);
            std::future::poll_fn(|cx| {
                assert!(matches!(
                    std::future::Future::poll(flushing.as_mut(), cx),
                    Poll::Pending
                ));
                Poll::Ready(())
            })
            .await;
            assert!(sender.send(SessionCmd::ClipboardFiles {
                generation: 0,
                files: Vec::new(),
            }));
            std::future::poll_fn(|cx| {
                assert!(matches!(
                    std::future::Future::poll(flushing.as_mut(), cx),
                    Poll::Ready(Ok(()))
                ));
                Poll::Ready(())
            })
            .await;
        }
        let mut observed_unlock = vec![0; expected.len()];
        tokio::io::AsyncReadExt::read_exact(&mut peer, &mut observed_unlock)
            .await
            .unwrap();
        assert_eq!(observed_unlock, expected);
        let command = input.commands.try_recv().unwrap().command;
        handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        flush_clipboard_output(
            &mut wire,
            &mut pending,
            &state,
            &mut stage,
            &host,
            Some(&input),
        )
        .await
        .unwrap();
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let messages = decode_clipboard_messages(&observed);
        assert_eq!(messages.len(), 1);
        let ClipboardPdu::FormatList(list) = decode::<ClipboardPdu<'_>>(&messages[0]).unwrap()
        else {
            panic!("expected the replacement format list");
        };
        assert!(list.get_formats(true).unwrap().is_empty());
    }

    #[tokio::test]
    async fn outgoing_unlocks_survive_an_offer_revoked_before_its_first_byte() {
        use crate::session::{flush_clipboard_output, handle_handshake_command, SessionCmd};
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use std::task::Poll;

        #[derive(Clone, Copy)]
        enum Replacement {
            Clear,
            Text,
            Image,
            Files,
        }
        for (lock_count, replacement) in [
            (1, Replacement::Clear),
            (2, Replacement::Clear),
            (1, Replacement::Text),
            (1, Replacement::Image),
            (2, Replacement::Files),
        ] {
            let (host, _receiver) = crate::host::host_channel();
            let (state, mut stage) = descriptor_stage(
                vec![ClipFile {
                    name: "initial.txt".into(),
                    size: 1,
                    is_dir: false,
                }],
                &host,
            )
            .await;
            let mut locks = remote_file_locks_on_wire(&mut stage, lock_count);
            let _ = actions(&state);
            offer_local_files(
                &state,
                8,
                vec![ClipFile {
                    name: "revoked.txt".into(),
                    size: 4,
                    is_dir: false,
                }],
            );
            let mut pending = VecDeque::from(
                pump_with_pending_input(&state, &mut stage, &host, None)
                    .await
                    .unwrap(),
            );
            assert!(!pending.is_empty());
            let (sender, mut input) = crate::host::input_channel();
            let (mut wire, mut peer) = tokio::io::duplex(1);
            tokio::io::AsyncWriteExt::write_all(&mut wire, &[0xaa])
                .await
                .unwrap();
            {
                let flushing = flush_clipboard_output(
                    &mut wire,
                    &mut pending,
                    &state,
                    &mut stage,
                    &host,
                    Some(&input),
                );
                tokio::pin!(flushing);
                std::future::poll_fn(|cx| {
                    assert!(matches!(
                        std::future::Future::poll(flushing.as_mut(), cx),
                        Poll::Pending
                    ));
                    Poll::Ready(())
                })
                .await;
                let replacement_command = match replacement {
                    Replacement::Clear => SessionCmd::ClipboardFiles {
                        generation: 0,
                        files: Vec::new(),
                    },
                    Replacement::Text => SessionCmd::Input(
                        adit_rdp_proto::InputEvent::ClipboardText("replacement".into()),
                    ),
                    Replacement::Image => SessionCmd::ClipboardImage(vec![1, 2, 3]),
                    Replacement::Files => SessionCmd::ClipboardFiles {
                        // Reusing the public generation still invalidates the
                        // old offer because the internal revision changed.
                        generation: 8,
                        files: vec![ClipFile {
                            name: "replacement.txt".into(),
                            size: 5,
                            is_dir: false,
                        }],
                    },
                };
                assert!(sender.send(replacement_command));
                flushing.await.unwrap();
            }
            let mut filler = [0];
            tokio::io::AsyncReadExt::read_exact(&mut peer, &mut filler)
                .await
                .unwrap();
            assert_eq!(filler, [0xaa], "no clipboard bytes were accepted");
            let command = input.commands.try_recv().unwrap().command;
            handle_handshake_command(command, &state, &mut None, &host)
                .await
                .unwrap();
            let send = async {
                flush_clipboard_output(
                    &mut wire,
                    &mut pending,
                    &state,
                    &mut stage,
                    &host,
                    Some(&input),
                )
                .await
                .unwrap();
                assert!(pending.is_empty());
                drop(wire);
            };
            let receive = async {
                let mut observed = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
                    .await
                    .unwrap();
                observed
            };
            let (_, observed) = tokio::join!(send, receive);
            let messages = decode_clipboard_messages(&observed);
            assert_eq!(messages.len(), lock_count + 1);
            let mut unlocked = Vec::new();
            for message in &messages[..lock_count] {
                let ClipboardPdu::UnlockData(id) = decode::<ClipboardPdu<'_>>(message).unwrap()
                else {
                    panic!("each retained lock needs its own UnlockData");
                };
                unlocked.push(id.0);
            }
            locks.sort_unstable();
            unlocked.sort_unstable();
            assert_eq!(unlocked, locks);
            let ClipboardPdu::FormatList(list) =
                decode::<ClipboardPdu<'_>>(&messages[lock_count]).unwrap()
            else {
                panic!("expected the replacement format list after all unlocks");
            };
            let formats = list.get_formats(true).unwrap();
            match replacement {
                Replacement::Clear => assert!(formats.is_empty()),
                Replacement::Text => {
                    assert_eq!(formats.len(), 1);
                    assert_eq!(formats[0].id, ClipboardFormatId::CF_UNICODETEXT);
                }
                Replacement::Image => {
                    assert_eq!(formats.len(), 1);
                    assert_eq!(formats[0].id, ClipboardFormatId::CF_DIB);
                }
                Replacement::Files => assert!(formats.is_empty()),
            }
            assert_video_and_other_svc_still_work(&mut stage);
            assert!(actions(&state).is_empty());
        }
    }

    #[tokio::test]
    async fn active_stage_descriptor_response_waits_for_the_snapshot_barrier() {
        use ironrdp_session::ActiveStageOutput;
        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "snapshot-barrier.txt".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let output = process_format_request(&mut stage, 0xC0FE);
        assert!(
            output.iter().all(|item| !matches!(item,
            ActiveStageOutput::ResponseFrame(frame) if !frame.is_empty())),
            "the descriptor response must not bypass the clipboard commit barrier"
        );
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1, "the request needs one protocol response");

        // Reuse the externally supplied generation: only the internal revision
        // distinguishes the newly copied list from the encoded old response.
        let (sender, mut input) = crate::host::input_channel();
        assert!(sender.send(crate::session::SessionCmd::ClipboardFiles {
            generation: 7,
            files: vec![ClipFile {
                name: "replacement.txt".into(),
                size: 4,
                is_dir: false,
            }],
        }));
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        assert_eq!(
            crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[0],
                &state,
                &mut stage,
                Some(&input),
            )
            .await
            .unwrap(),
            crate::session::ClipboardCommit::Deferred
        );
        let command = input.commands.try_recv().unwrap().command;
        crate::session::handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        let failure = stage
            .get_svc_processor_mut::<CliprdrClient>()
            .unwrap()
            .submit_format_data(OwnedFormatDataResponse::new_error())
            .unwrap();
        let expected = stage.process_svc_processor_messages(failure).unwrap();
        assert_eq!(
            crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[0],
                &state,
                &mut stage,
                Some(&input),
            )
            .await
            .unwrap(),
            crate::session::ClipboardCommit::Written
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        assert_eq!(
            observed, expected,
            "the peer receives one complete failure PDU"
        );
    }

    #[tokio::test]
    async fn active_stage_descriptor_request_and_queued_revoke_receive_one_failure() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_session::ActiveStageOutput;
        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "revoked-before-pump.txt".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let (sender, mut input) = crate::host::input_channel();
        assert!(sender.send(crate::session::SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new(),
        }));
        let output = process_format_request(&mut stage, 0xC0FE);
        assert!(output.iter().all(|item| !matches!(item,
            ActiveStageOutput::ResponseFrame(frame) if !frame.is_empty())));
        assert!(
            pump_with_pending_input(&state, &mut stage, &host, Some(&input))
                .await
                .unwrap()
                .is_empty()
        );
        let command = input.commands.try_recv().unwrap().command;
        crate::session::handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, Some(&input))
            .await
            .unwrap();
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        assert_eq!(
            crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[0],
                &state,
                &mut stage,
                Some(&input),
            )
            .await
            .unwrap(),
            crate::session::ClipboardCommit::Written
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let (pdu, chunks) = decode_one_clipboard_message(&observed);
        assert_eq!(chunks, 1);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&pdu).unwrap(),
            ClipboardPdu::FormatDataResponse(response) if response.is_error() && response.data().is_empty()));
    }

    #[tokio::test]
    async fn active_stage_descriptor_waiting_for_first_byte_is_interrupted_by_revoke() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use std::task::Poll;
        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "blocked-before-write.txt".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let _ = process_format_request(&mut stage, 0xC0FE);
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (sender, mut input) = crate::host::input_channel();
        let (mut wire, mut peer) = tokio::io::duplex(1);
        tokio::io::AsyncWriteExt::write_all(&mut wire, &[0xaa])
            .await
            .unwrap();
        {
            let committing = crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[0],
                &state,
                &mut stage,
                Some(&input),
            );
            tokio::pin!(committing);
            std::future::poll_fn(|cx| {
                assert!(matches!(
                    std::future::Future::poll(committing.as_mut(), cx),
                    Poll::Pending
                ));
                Poll::Ready(())
            })
            .await;
            assert!(sender.send(crate::session::SessionCmd::ClipboardFiles {
                generation: 0,
                files: Vec::new(),
            }));
            assert_eq!(
                committing.await.unwrap(),
                crate::session::ClipboardCommit::Deferred
            );
        }
        let mut filler = [0];
        tokio::io::AsyncReadExt::read_exact(&mut peer, &mut filler)
            .await
            .unwrap();
        assert_eq!(filler, [0xaa]);
        let command = input.commands.try_recv().unwrap().command;
        crate::session::handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        let failure = stage
            .get_svc_processor_mut::<CliprdrClient>()
            .unwrap()
            .submit_format_data(OwnedFormatDataResponse::new_error())
            .unwrap();
        let expected = stage.process_svc_processor_messages(failure).unwrap();
        let receive = async {
            let mut observed = vec![0; expected.len()];
            tokio::io::AsyncReadExt::read_exact(&mut peer, &mut observed)
                .await
                .unwrap();
            observed
        };
        let (result, observed) = tokio::join!(
            crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[0],
                &state,
                &mut stage,
                Some(&input),
            ),
            receive,
        );
        assert_eq!(result.unwrap(), crate::session::ClipboardCommit::Written);
        assert_eq!(observed, expected);
        let (pdu, _) = decode_one_clipboard_message(&observed);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&pdu).unwrap(),
            ClipboardPdu::FormatDataResponse(response) if response.is_error()));
    }

    #[tokio::test]
    async fn active_stage_descriptor_keeps_its_original_revision_even_if_names_recur() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_session::ActiveStageOutput;
        let (host, _receiver) = crate::host::host_channel();
        let old = ClipFile {
            name: "same-name.txt".into(),
            size: 4,
            is_dir: false,
        };
        let (state, mut stage) = descriptor_stage(vec![old.clone()], &host).await;
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "intermediate.txt".into(),
                size: 4,
                is_dir: false,
            }],
        );
        offer_local_files(&state, 7, vec![old]);
        let output = process_format_request(&mut stage, 0xC0FE);
        assert!(output.iter().all(|item| !matches!(item,
            ActiveStageOutput::ResponseFrame(frame) if !frame.is_empty())));
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(
            frames.len(),
            2,
            "new advertisement precedes the old request's response"
        );
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        assert_eq!(
            crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[1],
                &state,
                &mut stage,
                None,
            )
            .await
            .unwrap(),
            crate::session::ClipboardCommit::Written
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let (pdu, _) = decode_one_clipboard_message(&observed);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&pdu).unwrap(),
            ClipboardPdu::FormatDataResponse(response) if response.is_error() && response.data().is_empty()));
    }

    #[tokio::test]
    async fn active_stage_multichunk_descriptor_finishes_after_its_first_byte() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_session::ActiveStageOutput;
        use std::task::Poll;
        let files: Vec<_> = (0..12)
            .map(|n| ClipFile {
                name: format!("synthetic-{n:02}.txt"),
                size: n,
                is_dir: false,
            })
            .collect();
        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(files.clone(), &host).await;
        let output = process_format_request(&mut stage, 0xC0FE);
        assert!(output.iter().all(|item| !matches!(item,
            ActiveStageOutput::ResponseFrame(frame) if !frame.is_empty())));
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let expected_len = frames[0].bytes.len();
        let (sender, input) = crate::host::input_channel();
        let (mut wire, mut peer) = tokio::io::duplex(1);
        let committing = crate::session::commit_clipboard_frame(
            &mut wire,
            &mut frames[0],
            &state,
            &mut stage,
            Some(&input),
        );
        tokio::pin!(committing);
        std::future::poll_fn(|cx| {
            assert!(matches!(
                std::future::Future::poll(committing.as_mut(), cx),
                Poll::Pending
            ));
            Poll::Ready(())
        })
        .await;
        assert!(sender.send(crate::session::SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new(),
        }));
        let receive = async {
            let mut observed = vec![0; expected_len];
            tokio::io::AsyncReadExt::read_exact(&mut peer, &mut observed)
                .await
                .unwrap();
            observed
        };
        let (result, observed) = tokio::join!(committing, receive);
        assert_eq!(result.unwrap(), crate::session::ClipboardCommit::Written);
        let (pdu, chunks) = decode_one_clipboard_message(&observed);
        assert!(chunks > 1, "the file list must span several SVC chunks");
        let ClipboardPdu::FormatDataResponse(response) = decode::<ClipboardPdu<'_>>(&pdu).unwrap()
        else {
            panic!("expected a format data response");
        };
        assert!(!response.is_error());
        let decoded = response.to_file_list().unwrap();
        assert_eq!(
            decoded
                .files
                .iter()
                .map(from_descriptor)
                .collect::<Vec<_>>(),
            files
        );
    }

    #[tokio::test]
    async fn active_stage_other_channels_input_and_image_keep_their_own_paths() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_graphics::image_processing::PixelFormat;
        use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
        use ironrdp_session::{image::DecodedImage, ActiveStageOutput};
        use ironrdp_svc::SvcMessage;

        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "ordinary.txt".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;

        let request = ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::new(0xC0FE),
        });
        let mut payload = vec![0; request.size()];
        request.encode(&mut WriteCursor::new(&mut payload)).unwrap();
        let packet = ironrdp_svc::server_encode_svc_messages(
            vec![SvcMessage::from(payload.clone())],
            1005,
            1001,
        )
        .unwrap();
        let expected =
            ironrdp_svc::client_encode_svc_messages(vec![SvcMessage::from(payload)], 1005, 1001)
                .unwrap();
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 1, 1);
        let output = stage
            .process(&mut image, ironrdp_pdu::Action::X224, &packet)
            .unwrap();
        assert!(
            matches!(output.as_slice(), [ActiveStageOutput::ResponseFrame(frame)] if *frame == expected)
        );
        assert!(actions(&state).is_empty());

        let output = stage
            .process_fastpath_input(
                &mut image,
                &[FastPathInputEvent::KeyboardEvent(
                    KeyboardFlags::empty(),
                    0x1e,
                )],
            )
            .unwrap();
        assert!(output.iter().any(
            |item| matches!(item, ActiveStageOutput::ResponseFrame(frame) if !frame.is_empty())
        ));
        assert!(actions(&state).is_empty());

        let output = process_format_request(&mut stage, 0xC0FD);
        let [ActiveStageOutput::ResponseFrame(drop_effect)] = output.as_slice() else {
            panic!("expected the unchanged inline Preferred DropEffect response");
        };
        let (pdu, _) = decode_one_clipboard_message(drop_effect);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&pdu).unwrap(),
            ClipboardPdu::FormatDataResponse(response) if !response.is_error() && response.data() == 1_u32.to_le_bytes()));
        assert!(actions(&state).is_empty());

        let dib = vec![0x93, 0x71, 0x52, 0x34];
        offer_local_image(&state, dib.clone());
        let _ = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        let _ = process_format_request(&mut stage, ClipboardFormatId::CF_DIB.value());
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        assert_eq!(
            crate::session::commit_clipboard_frame(
                &mut wire,
                &mut frames[0],
                &state,
                &mut stage,
                None,
            )
            .await
            .unwrap(),
            crate::session::ClipboardCommit::Written
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let (pdu, _) = decode_one_clipboard_message(&observed);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&pdu).unwrap(),
            ClipboardPdu::FormatDataResponse(response) if !response.is_error() && response.data() == dib));
    }

    #[tokio::test]
    async fn encoded_remote_read_can_still_be_written_after_the_snapshot_is_revoked() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::{
            Capabilities, ClipboardPdu, ClipboardProtocolVersion, FormatListResponse,
        };
        use ironrdp_svc::{StaticChannelSet, SvcProcessor};
        use std::any::TypeId;

        let state = new_shared();
        let mut client = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
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
        let mut stage = ironrdp_session::ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        let generation = {
            let mut guard = state.lock().unwrap();
            guard.remote_files.revoke(true);
            assert!(guard.remote_files.install(
                vec![ClipFile {
                    name: "synthetic-remote.bin".into(),
                    size: 4,
                    is_dir: false,
                }],
                Some(77)
            ));
            guard.remote_files.generation()
        };
        let _ = actions(&state);
        request_file_contents(&state, generation, 41, 0, 0, 2);
        let read = state
            .lock()
            .unwrap()
            .actions
            .iter()
            .find_map(|action| match action {
                ClipboardAction::RequestFileContents(read) => Some(*read),
                _ => None,
            })
            .unwrap();
        let (host, _receiver) = crate::host::host_channel();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        offer_local_text(&state, "synthetic replacement".into());
        assert!(!state.lock().unwrap().remote_files.current(&read));
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
            stream_id: read.wire_id,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 2,
            data_id: Some(77),
        });
        let mut request_bytes = vec![0; request.size()];
        request
            .encode(&mut WriteCursor::new(&mut request_bytes))
            .unwrap();
        assert!(
            observed
                .windows(request_bytes.len())
                .any(|bytes| bytes == request_bytes),
            "the current implementation still sends this already-encoded stale remote read"
        );
        assert!(state.lock().unwrap().actions.iter().any(|action| matches!(action,
            ClipboardAction::InboundFileContents(Reply { generation: got, stream_id: 41, data: None }) if *got == generation)));
    }

    #[tokio::test]
    async fn revocation_wakes_a_file_frame_waiting_for_its_first_network_byte() {
        use crate::session::{
            commit_clipboard_frame, handle_handshake_command, ClipboardCommit, SessionCmd,
        };
        use std::time::Duration;

        let (state, mut stage, mut frame) = prepared_file_reply().await;
        let (sender, mut input) = crate::host::input_channel();
        let (host, _receiver) = crate::host::host_channel();
        let (mut wire, _peer) = tokio::io::duplex(1);
        tokio::io::AsyncWriteExt::write_all(&mut wire, &[0xaa])
            .await
            .unwrap();
        {
            let committing =
                commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, Some(&input));
            tokio::pin!(committing);
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut committing)
                    .await
                    .is_err()
            );
            assert!(sender.send(SessionCmd::ClipboardFiles {
                generation: 0,
                files: Vec::new()
            }));
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), &mut committing)
                    .await
                    .unwrap()
                    .unwrap(),
                ClipboardCommit::Deferred
            );
        }
        let command = input.commands.try_recv().unwrap().command;
        handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(!frame.bytes.windows(4).any(|bytes| bytes == [9, 8, 7, 6]));
    }

    #[tokio::test]
    async fn decoded_revoke_blocks_wire_commit_even_before_queue_capacity_is_available() {
        use crate::session::{
            commit_clipboard_frame, handle_handshake_command, ClipboardCommit, SessionCmd,
        };
        use adit_rdp_proto::InputEvent;
        use std::time::Duration;

        let (state, mut stage, mut frame) = prepared_file_reply().await;
        let (sender, mut input) = crate::host::input_channel();
        for _ in 0..512 {
            assert!(sender.send(SessionCmd::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true
            })));
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let sent = sender.send(SessionCmd::ClipboardFiles {
                generation: 0,
                files: Vec::new(),
            });
            let _ = done_tx.send(sent);
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !input.has_pending_offer() {
            assert!(
                std::time::Instant::now() < deadline,
                "decoded revoke did not reach the barrier"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Deferred
        );
        drop(input.commands.try_recv().unwrap());
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        for _ in 1..512 {
            drop(input.commands.try_recv().unwrap());
        }
        let command = input.commands.try_recv().unwrap().command;
        let (host, _receiver) = crate::host::host_channel();
        handle_handshake_command(command, &state, &mut None, &host)
            .await
            .unwrap();
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(!frame.bytes.windows(4).any(|bytes| bytes == [9, 8, 7, 6]));
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn revocation_after_the_first_network_byte_does_not_truncate_a_frame() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit, SessionCmd};
        use std::time::Duration;

        let (state, mut stage, mut frame) = prepared_file_reply().await;
        let expected = frame.bytes.clone();
        let (sender, input) = crate::host::input_channel();
        let (mut wire, mut peer) = tokio::io::duplex(1);
        let committing =
            commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, Some(&input));
        tokio::pin!(committing);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut committing)
                .await
                .is_err()
        );
        let mut first = [0];
        tokio::io::AsyncReadExt::read_exact(&mut peer, &mut first)
            .await
            .unwrap();
        assert!(sender.send(SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new()
        }));
        let receive = async {
            let mut rest = vec![0; expected.len() - 1];
            tokio::io::AsyncReadExt::read_exact(&mut peer, &mut rest)
                .await
                .unwrap();
            let mut bytes = first.to_vec();
            bytes.extend(rest);
            bytes
        };
        let (result, observed) = tokio::join!(&mut committing, receive);
        assert_eq!(result.unwrap(), ClipboardCommit::Written);
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn buffered_transport_can_accept_old_data_before_its_flush_reaches_the_peer() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit, SessionCmd};
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};
        use std::time::Duration;
        use tokio::io::AsyncWrite;

        #[derive(Default)]
        struct Gate {
            ready: bool,
            waker: Option<Waker>,
            peer: Vec<u8>,
        }
        struct BufferedWriter {
            pending: Vec<u8>,
            gate: Arc<Mutex<Gate>>,
        }
        impl AsyncWrite for BufferedWriter {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                self.pending.extend_from_slice(bytes);
                Poll::Ready(Ok(bytes.len()))
            }
            fn poll_flush(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                let gate = Arc::clone(&self.gate);
                let mut gate = gate.lock().unwrap();
                if !gate.ready {
                    gate.waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
                gate.peer
                    .extend_from_slice(&std::mem::take(&mut self.pending));
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                self.poll_flush(cx)
            }
        }

        let (state, mut stage, mut frame) = prepared_file_reply().await;
        let (sender, input) = crate::host::input_channel();
        let gate = Arc::new(Mutex::new(Gate::default()));
        let mut writer = BufferedWriter {
            pending: Vec::new(),
            gate: Arc::clone(&gate),
        };
        let committing =
            commit_clipboard_frame(&mut writer, &mut frame, &state, &mut stage, Some(&input));
        tokio::pin!(committing);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut committing)
                .await
                .is_err()
        );
        assert!(
            gate.lock().unwrap().peer.is_empty(),
            "the synthetic peer has received nothing yet"
        );
        assert!(sender.send(SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new()
        }));
        {
            let mut gate = gate.lock().unwrap();
            gate.ready = true;
            if let Some(waker) = gate.waker.take() {
                waker.wake();
            }
        }
        assert_eq!(committing.await.unwrap(), ClipboardCommit::Written);
        assert!(
            gate.lock()
                .unwrap()
                .peer
                .windows(4)
                .any(|bytes| bytes == [9, 8, 7, 6]),
            "a generic AsyncWrite can commit to a private buffer before flush"
        );

        let (host, _receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "a.txt".into(),
                size: 1,
                is_dir: false,
            }],
            &host,
        )
        .await;
        state.lock().unwrap().queue_advertise_files();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        let gate = Arc::new(Mutex::new(Gate::default()));
        let mut writer = BufferedWriter {
            pending: Vec::new(),
            gate: Arc::clone(&gate),
        };
        let committing =
            commit_clipboard_frame(&mut writer, &mut frames[0], &state, &mut stage, None);
        tokio::pin!(committing);
        std::future::poll_fn(|cx| {
            assert!(matches!(
                std::future::Future::poll(committing.as_mut(), cx),
                Poll::Pending
            ));
            Poll::Ready(())
        })
        .await;
        assert!(gate.lock().unwrap().peer.is_empty());
        assert!(matches!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Exposed(_)
        ));
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "b.txt".into(),
                size: 1,
                is_dir: false,
            }],
        );
        assert_eq!(
            state.lock().unwrap().local_file_direction,
            LocalFileDirection::Sealed
        );
        {
            let mut gate = gate.lock().unwrap();
            gate.ready = true;
            if let Some(waker) = gate.waker.take() {
                waker.wake();
            }
        }
        assert_eq!(committing.await.unwrap(), ClipboardCommit::Written);
        assert!(!gate.lock().unwrap().peer.is_empty());
    }

    #[tokio::test]
    async fn replacing_the_file_snapshot_invalidates_an_encoded_reply_even_if_generation_is_reused()
    {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};

        let (state, mut stage, mut frame) = prepared_file_reply().await;
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "replacement.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(!frame.bytes.windows(4).any(|bytes| bytes == [9, 8, 7, 6]));
    }

    async fn stale_format_reply_is_refused(image: bool, replacement: u8) {
        use crate::session::{commit_clipboard_frame, ClipboardCommit};
        use ironrdp_cliprdr::pdu::ClipboardPdu;

        let (state, mut stage, _earlier) = prepared_file_reply().await;
        let mut backend = AditCliprdrBackend {
            state: Arc::clone(&state),
        };
        let marker = if image {
            let old = vec![0x91, 0x82, 0x73, 0x64];
            offer_local_image(&state, old.clone());
            old
        } else {
            offer_local_text(&state, "old-secret".into());
            "old-secret"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect()
        };
        let _ = actions(&state);
        backend.on_format_data_request(FormatDataRequest {
            format: if image {
                ClipboardFormatId::CF_DIB
            } else {
                ClipboardFormatId::CF_UNICODETEXT
            },
        });
        let (host, _receiver) = crate::host::host_channel();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0]
            .bytes
            .windows(marker.len())
            .any(|bytes| bytes == marker));
        match replacement {
            0 => offer_local_text(&state, "replacement".into()),
            1 => offer_local_image(&state, vec![4, 5, 6]),
            _ => reject_local_file_offer(&state),
        }
        let (mut wire, mut peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, None)
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        drop(wire);
        let mut observed = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
            .await
            .unwrap();
        let expected = ClipboardPdu::FormatDataResponse(OwnedFormatDataResponse::new_error());
        let mut expected_bytes = vec![0; expected.size()];
        expected
            .encode(&mut WriteCursor::new(&mut expected_bytes))
            .unwrap();
        assert!(observed
            .windows(expected_bytes.len())
            .any(|bytes| bytes == expected_bytes));
        assert!(!observed.windows(marker.len()).any(|bytes| bytes == marker));
    }

    #[tokio::test]
    async fn encoded_stale_text_reply_is_refused_after_replacement() {
        for replacement in 0..3 {
            stale_format_reply_is_refused(false, replacement).await;
        }
    }

    #[tokio::test]
    async fn encoded_stale_image_reply_is_refused_after_replacement() {
        for replacement in 0..3 {
            stale_format_reply_is_refused(true, replacement).await;
        }
    }

    #[tokio::test]
    async fn rejected_marker_blocks_an_encoded_old_text_or_image_reply() {
        use crate::session::{commit_clipboard_frame, handle_handshake_command, ClipboardCommit};
        use std::io::{self, Read};
        use std::sync::mpsc;
        use std::time::Duration;

        struct GateReader {
            input: io::Cursor<Vec<u8>>,
            observed: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            released: bool,
        }
        impl Read for GateReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.input.position() >= 8 && !self.released {
                    self.observed.send(()).unwrap();
                    self.release.recv().unwrap();
                    self.released = true;
                }
                self.input.read(buf)
            }
        }

        for image in [false, true] {
            let (state, mut stage, _old_file_frame) = prepared_file_reply().await;
            suspend_local_file_direction(&state);
            let mut backend = AditCliprdrBackend {
                state: Arc::clone(&state),
            };
            let secret = if image {
                let bytes = vec![0x91, 0x82, 0x73, 0x64];
                offer_local_image(&state, bytes.clone());
                bytes
            } else {
                offer_local_text(&state, "prior-private-text".into());
                "prior-private-text"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect()
            };
            let _ = actions(&state);
            backend.on_format_data_request(FormatDataRequest {
                format: if image {
                    ClipboardFormatId::CF_DIB
                } else {
                    ClipboardFormatId::CF_UNICODETEXT
                },
            });
            let (host, _messages) = crate::host::host_channel();
            let mut frames = pump_with_pending_input(&state, &mut stage, &host, None)
                .await
                .unwrap();
            assert_eq!(frames.len(), 1);
            assert!(frames[0]
                .bytes
                .windows(secret.len())
                .any(|window| window == secret.as_slice()));

            let mut ipc = Vec::new();
            adit_rdp_proto::write_msg(&mut ipc, &adit_rdp_proto::ClientMsg::LocalFileOfferRejected)
                .unwrap();
            // A trailing byte lets the fixture pause the reader after the
            // marker is classified and queued, before the frame is drained.
            ipc[..4].copy_from_slice(&5_u32.to_le_bytes());
            ipc.push(0);
            let (sender, mut input) = crate::host::input_channel();
            let (observed, waiting) = mpsc::channel();
            let (release, proceed) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut reader = GateReader {
                    input: io::Cursor::new(ipc),
                    observed,
                    release: proceed,
                    released: false,
                };
                matches!(
                    crate::host::read_client_frame(&mut reader, &sender).unwrap(),
                    Some(crate::host::ClientFrame::FileHandled)
                )
            });
            waiting.recv_timeout(Duration::from_secs(1)).unwrap();
            let (mut wire, mut peer) = tokio::io::duplex(4096);
            assert_eq!(
                commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, Some(&input))
                    .await
                    .unwrap(),
                ClipboardCommit::Deferred
            );
            release.send(()).unwrap();
            assert!(reader.join().unwrap());
            let command = input.commands.try_recv().unwrap().command;
            handle_handshake_command(command, &state, &mut None, &host)
                .await
                .unwrap();
            assert_eq!(
                commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, Some(&input))
                    .await
                    .unwrap(),
                ClipboardCommit::Written
            );
            drop(wire);
            let mut observed = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut observed)
                .await
                .unwrap();
            let failure =
                ironrdp_core::encode_vec(&ironrdp_cliprdr::pdu::ClipboardPdu::FormatDataResponse(
                    OwnedFormatDataResponse::new_error(),
                ))
                .unwrap();
            assert!(observed
                .windows(failure.len())
                .any(|window| window == failure.as_slice()));
            assert!(!observed
                .windows(secret.len())
                .any(|window| window == secret.as_slice()));
            assert!(input.end.borrow().is_none());
        }
    }

    #[test]
    fn empty_file_revocation_preserves_an_unchanged_text_or_image_reply() {
        for image in [false, true] {
            let (mut backend, state) = backend();
            backend.on_ready();
            if image {
                offer_local_image(&state, vec![1, 2, 3]);
            } else {
                offer_local_text(&state, "unchanged".into());
            }
            let _ = actions(&state);
            backend.on_format_data_request(FormatDataRequest {
                format: if image {
                    ClipboardFormatId::CF_DIB
                } else {
                    ClipboardFormatId::CF_UNICODETEXT
                },
            });
            offer_local_files(&state, 0, Vec::new());
            {
                let guard = state.lock().unwrap();
                assert!(guard.actions.iter().any(|action| if image {
                    matches!(action, ClipboardAction::SubmitImage { image: Some(bytes), .. } if bytes == &[1, 2, 3])
                } else {
                    matches!(action, ClipboardAction::SubmitText { text: Some(text), .. } if text == "unchanged")
                }));
            }
            offer_local_text(&state, "replacement".into());
            let queued = actions(&state);
            assert!(queued.iter().any(|action| if image {
                matches!(action, ClipboardAction::SubmitImage { image: None, .. })
            } else {
                matches!(action, ClipboardAction::SubmitText { text: None, .. })
            }));
        }
    }

    #[test]
    fn empty_revocation_clears_a_suspended_file_snapshot_without_clearing_text() {
        let (_backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.local_file_suspended = true;
            guard.local_file_direction = LocalFileDirection::Sealed;
            guard.local_file_generation = 7;
            guard.local_files = vec![ClipFile {
                name: "synthetic-old.bin".into(),
                size: 4,
                is_dir: false,
            }];
            guard.advertised_files = guard.local_files.clone();
            guard.local_text = Some("keep text".into());
        }
        offer_local_files(&state, 0, Vec::new());
        let guard = state.lock().unwrap();
        assert!(guard.local_files.is_empty());
        assert!(guard.advertised_files.is_empty());
        assert_eq!(guard.local_file_generation, 0);
        assert_eq!(guard.local_text.as_deref(), Some("keep text"));
        assert!(guard.actions.iter().any(|action| matches!(
            action, ClipboardAction::Advertise(formats)
                if formats.as_slice() == [ClipboardFormatId::CF_UNICODETEXT]
        )));
    }

    #[tokio::test]
    async fn rejected_file_copy_coalesces_and_defers_advertising_under_action_pressure() {
        let state = new_shared();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.local_text = Some("old text".into());
            for _ in 0..MAX_QUEUED_ACTIONS {
                guard.queue(ClipboardAction::RequestText(
                    ClipboardFormatId::CF_UNICODETEXT,
                ));
            }
            assert_eq!(guard.actions.len(), MAX_QUEUED_ACTIONS);
        }
        for _ in 0..4 {
            reject_local_file_offer(&state);
        }
        let (host, receiver) = crate::host::host_channel();
        flush_app_actions(&state, &host).await.unwrap();
        {
            let mut guard = state.lock().unwrap();
            assert!(!guard.actions_overloaded);
            assert!(guard.rejected_file_advertise_pending);
            assert!(guard.local_text.is_none());
            assert_eq!(guard.take_actions().len(), MAX_QUEUED_ACTIONS);
        }
        assert!(pump_with_pending_input(
            &state,
            &mut active_stage_without_clipboard(),
            &host,
            None
        )
        .await
        .unwrap()
        .is_empty());
        let guard = state.lock().unwrap();
        assert!(!guard.actions_overloaded);
        assert!(!guard.rejected_file_advertise_pending);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn queued_key_flood_does_not_starve_a_file_reply() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit, SessionCmd};
        use adit_rdp_proto::InputEvent;

        let (state, mut stage, mut frame) = prepared_file_reply().await;
        let (sender, mut input) = crate::host::input_channel();
        for index in 0..200 {
            assert!(sender.send(SessionCmd::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: index % 2 == 0,
            })));
        }
        assert!(!input.has_pending_offer());
        submit_file_contents(
            &state,
            pending_test_read(&state, 7, 42),
            Some(vec![4, 3, 2, 1]),
        );
        let (host, _receiver) = crate::host::host_channel();
        let mut later = pump_with_pending_input(&state, &mut stage, &host, Some(&input))
            .await
            .unwrap();
        assert_eq!(
            later.len(),
            1,
            "non-clipboard input must not defer the pump"
        );
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frame, &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut later[0], &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(frame.bytes.windows(4).any(|bytes| bytes == [9, 8, 7, 6]));
        for index in 0..200 {
            assert!(matches!(input.commands.try_recv().unwrap().command,
                SessionCmd::Input(InputEvent::Key { pressed, .. }) if pressed == (index % 2 == 0)));
        }
    }

    #[tokio::test]
    async fn queued_clipboard_replacement_does_not_starve_a_file_failure() {
        use crate::session::{commit_clipboard_frame, ClipboardCommit, SessionCmd};

        let (state, mut stage, _earlier) = prepared_file_reply().await;
        let (sender, input) = crate::host::input_channel();
        assert!(sender.send(SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new()
        }));
        submit_file_contents(&state, pending_test_read(&state, 7, 88), None);
        let (host, _receiver) = crate::host::host_channel();
        let mut frames = pump_with_pending_input(&state, &mut stage, &host, Some(&input))
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        let (mut wire, _peer) = tokio::io::duplex(4096);
        assert_eq!(
            commit_clipboard_frame(&mut wire, &mut frames[0], &state, &mut stage, Some(&input))
                .await
                .unwrap(),
            ClipboardCommit::Written
        );
        assert!(input.has_pending_offer());
    }

    #[tokio::test]
    async fn redirect_handshake_applies_revocation_and_rejects_new_files() {
        use crate::session::{handle_handshake_command, SessionCmd};

        for replacement in [false, true] {
            for capabilities_first in [false, true] {
                let (mut previous, state) = backend();
                previous.on_process_negotiated_capabilities(
                    ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
                );
                previous.on_ready();
                let file = |name: &str| ClipFile {
                    name: name.into(),
                    size: 4,
                    is_dir: false,
                };
                offer_local_files(&state, 7, vec![file("old.bin")]);
                let _ = actions(&state);

                let mut redirected = AditCliprdrBackend::new(Arc::clone(&state));
                let (host, _messages) = crate::host::host_channel();
                let mut layout = None;
                handle_handshake_command(
                    SessionCmd::ClipboardFiles {
                        generation: 0,
                        files: Vec::new(),
                    },
                    &state,
                    &mut layout,
                    &host,
                )
                .await
                .unwrap();
                if replacement {
                    handle_handshake_command(
                        SessionCmd::ClipboardFiles {
                            generation: 8,
                            files: vec![file("new.bin")],
                        },
                        &state,
                        &mut layout,
                        &host,
                    )
                    .await
                    .unwrap();
                }

                redirected.on_request_format_list();
                assert_eq!(
                    actions(&state),
                    vec![ClipboardAction::Advertise(Vec::new())]
                );
                if capabilities_first {
                    redirected.on_process_negotiated_capabilities(
                        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
                    );
                    redirected.on_ready();
                } else {
                    redirected.on_ready();
                    redirected.on_process_negotiated_capabilities(
                        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
                    );
                }
                let announced = actions(&state);
                assert!(announced.is_empty(), "local files must not be advertised");
                redirected.on_file_contents_request(FileContentsRequest {
                    stream_id: 4,
                    index: 0,
                    flags: FileContentsFlags::SIZE,
                    position: 0,
                    requested_size: 0,
                    data_id: None,
                });
                assert!(matches!(
                    actions(&state).as_slice(),
                    [ClipboardAction::SubmitFileContents {
                        stream_id: 4,
                        data: None,
                        ..
                    }]
                ));
            }
        }
    }

    #[tokio::test]
    async fn redirect_handshake_keeps_the_latest_text_or_image() {
        use crate::session::{handle_handshake_command, SessionCmd};

        let (mut backend, state) = backend();
        let (host, _messages) = crate::host::host_channel();
        let mut layout = None;
        backend.on_request_format_list();
        let _ = actions(&state);
        handle_handshake_command(
            SessionCmd::Input(adit_rdp_proto::InputEvent::ClipboardText("new text".into())),
            &state,
            &mut layout,
            &host,
        )
        .await
        .unwrap();
        backend.on_ready();
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );

        let mut redirected = AditCliprdrBackend::new(Arc::clone(&state));
        redirected.on_request_format_list();
        let _ = actions(&state);
        handle_handshake_command(
            SessionCmd::ClipboardImage(vec![1, 2, 3]),
            &state,
            &mut layout,
            &host,
        )
        .await
        .unwrap();
        redirected.on_ready();
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![ClipboardFormatId::CF_DIB])]
        );
    }

    #[tokio::test]
    async fn file_copy_during_handshake_revokes_the_previous_text_format() {
        use crate::session::{handle_handshake_command, SessionCmd};

        for files_supported in [false, true] {
            let (mut backend, state) = backend();
            let (host, _messages) = crate::host::host_channel();
            let mut layout = None;
            offer_local_text(&state, "old text".into());
            backend.on_request_format_list();
            assert_eq!(
                actions(&state),
                vec![ClipboardAction::Advertise(vec![
                    ClipboardFormatId::CF_UNICODETEXT
                ])]
            );
            let file = |name: &str| ClipFile {
                name: name.into(),
                size: 4,
                is_dir: false,
            };
            handle_handshake_command(
                SessionCmd::ClipboardFiles {
                    generation: 1,
                    files: vec![file("new.bin")],
                },
                &state,
                &mut layout,
                &host,
            )
            .await
            .unwrap();
            backend.on_process_negotiated_capabilities(if files_supported {
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            } else {
                ClipboardGeneralCapabilityFlags::empty()
            });
            backend.on_ready();
            assert_eq!(
                actions(&state),
                vec![ClipboardAction::Advertise(Vec::new())]
            );

            if !files_supported {
                offer_local_files(&state, 2, vec![file("later.bin")]);
                assert_eq!(
                    actions(&state),
                    vec![ClipboardAction::Advertise(Vec::new())]
                );
            }
        }
    }

    #[tokio::test]
    async fn a_rejected_offer_revokes_an_unpumped_pre_redirect_file_list() {
        use crate::session::{handle_handshake_command, SessionCmd};

        for revoke_first in [false, true] {
            let (mut old, state) = backend();
            old.on_process_negotiated_capabilities(
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
            );
            old.on_ready();
            let file = |name: &str| ClipFile {
                name: name.into(),
                size: 4,
                is_dir: false,
            };
            offer_local_files(&state, 7, vec![file("old.bin")]);
            let _ = actions(&state);
            let mut redirected = AditCliprdrBackend::new(Arc::clone(&state));
            redirected.on_request_format_list();
            let _ = actions(&state);
            redirected.on_process_negotiated_capabilities(
                ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
            );
            redirected.on_ready(); // old offer is now queued, but not sent

            let (host, _messages) = crate::host::host_channel();
            let mut layout = None;
            if revoke_first {
                handle_handshake_command(
                    SessionCmd::ClipboardFiles {
                        generation: 0,
                        files: Vec::new(),
                    },
                    &state,
                    &mut layout,
                    &host,
                )
                .await
                .unwrap();
            }
            handle_handshake_command(
                SessionCmd::ClipboardFiles {
                    generation: 8,
                    files: vec![file("new.bin")],
                },
                &state,
                &mut layout,
                &host,
            )
            .await
            .unwrap();
            let announced = actions(&state);
            assert!(!announced
                .iter()
                .any(|action| matches!(action, ClipboardAction::AdvertiseFiles(..))));
            assert!(announced.contains(&ClipboardAction::Advertise(Vec::new())));
        }
    }

    #[test]
    fn remote_file_locks_are_bounded_and_require_an_active_offer() {
        let (mut unowned, unowned_state) = backend();
        assert!(!unowned.allow_incoming_file_lock(&LockDataId(0)));
        assert!(!unowned_state.lock().unwrap().local_file_suspended);
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            1,
            vec![ClipFile {
                name: "fixture.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        {
            let mut guard = state.lock().unwrap();
            let snapshot = FileSnapshot {
                channel_epoch: guard.channel_epoch,
                generation: guard.local_file_generation,
                revision: guard.local_file_revision,
            };
            guard.cached_file_snapshot = Some(snapshot);
            guard.local_file_direction = LocalFileDirection::Exposed(snapshot);
        }
        for id in 0..1000 {
            let id = LockDataId(id);
            if backend.allow_incoming_file_lock(&id) {
                backend.on_lock(id);
            }
        }
        let guard = state.lock().unwrap();
        assert!(guard.local_file_suspended);
        assert!(guard.local_file_locks.is_empty());
        assert!(guard.local_file_suspension_ack.is_some());
    }

    #[test]
    fn lock_admission_cannot_rebind_after_the_offer_changes() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 1,
            is_dir: false,
        };
        offer_local_files(&state, 7, vec![file("old.bin")]);
        {
            let mut guard = state.lock().unwrap();
            let snapshot = FileSnapshot {
                channel_epoch: guard.channel_epoch,
                generation: guard.local_file_generation,
                revision: guard.local_file_revision,
            };
            guard.cached_file_snapshot = Some(snapshot);
            guard.local_file_direction = LocalFileDirection::Exposed(snapshot);
        }
        assert!(backend.allow_incoming_file_lock(&LockDataId(90)));
        offer_local_files(&state, 7, vec![file("new.bin")]);
        backend.on_lock(LockDataId(90));
        let guard = state.lock().unwrap();
        assert_eq!(guard.local_file_direction, LocalFileDirection::Sealed);
        assert!(guard.local_file_locks.is_empty());
        assert!(!guard.local_file_notice_pending.is_empty());
    }

    #[test]
    fn empty_local_offer_revokes_cached_sizes_ranges_and_pending_file_actions() {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            1,
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        let request = |stream_id, flags| FileContentsRequest {
            stream_id,
            index: 0,
            flags,
            position: 0,
            requested_size: 4,
            data_id: None,
        };
        {
            let mut guard = state.lock().unwrap();
            let snapshot = FileSnapshot {
                channel_epoch: guard.channel_epoch,
                generation: guard.local_file_generation,
                revision: guard.local_file_revision,
            };
            guard.local_file_locks.insert(90, snapshot);
        }
        backend.on_file_contents_request(request(1, FileContentsFlags::SIZE));
        let mut locked = request(2, FileContentsFlags::RANGE);
        locked.data_id = Some(90);
        backend.on_file_contents_request(locked);
        offer_local_files(&state, 0, Vec::new());
        let revoked = actions(&state);
        for stream_id in [1, 2] {
            assert!(revoked.iter().any(|action| matches!(action,
                ClipboardAction::SubmitFileContents { stream_id: got, data: None, .. } if *got == stream_id)));
        }
        assert!(revoked.iter().any(
            |action| matches!(action, ClipboardAction::Advertise(formats) if formats.is_empty())
        ));
        assert!(!revoked.iter().any(|action| matches!(
            action,
            ClipboardAction::AdvertiseFiles(..) | ClipboardAction::NeedFileContents { .. }
        )));

        backend.on_file_contents_request(request(3, FileContentsFlags::SIZE));
        backend.on_file_contents_request(request(4, FileContentsFlags::RANGE));
        let after = actions(&state);
        for stream_id in [3, 4] {
            assert!(after.iter().any(|action| matches!(action,
                ClipboardAction::SubmitFileContents { stream_id: got, data: None, .. } if *got == stream_id)));
        }

        offer_local_files(
            &state,
            2,
            vec![ClipFile {
                name: "new-synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        let _ = actions(&state);
        let mut stale_lock = request(9, FileContentsFlags::RANGE);
        stale_lock.data_id = Some(90);
        backend.on_file_contents_request(stale_lock);
        backend.on_lock(LockDataId(91));
        let mut new_lock = request(6, FileContentsFlags::RANGE);
        new_lock.data_id = Some(91);
        backend.on_file_contents_request(new_lock);
        let guard = state.lock().unwrap();
        assert!(guard.local_file_suspended);
        assert!(guard.local_file_locks.is_empty());
        assert!(guard.local_file_suspension_ack.is_some());
        assert!(guard.pending_local_reads.is_empty());
        assert_eq!(guard.closed_file_streams.get(&9), Some(&false));
        assert_eq!(guard.closed_file_streams.get(&6), Some(&true));
        let file_actions: Vec<_> = guard
            .actions
            .iter()
            .filter(|action| action.is_local_file_action())
            .collect();
        assert_eq!(file_actions.len(), 1);
        assert!(matches!(file_actions[0],
            ClipboardAction::RefuseClosedFileContents { channel_epoch, stream_id: 6 }
                if *channel_epoch == guard.channel_epoch));
    }

    #[tokio::test]
    async fn active_stage_unbound_lock_suspension_only_replies_to_new_unique_streams() {
        use ironrdp_cliprdr::pdu::ClipboardPdu;
        use ironrdp_session::ActiveStageOutput;

        let (host, receiver) = crate::host::host_channel();
        let (state, mut stage) = descriptor_stage(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 4,
                is_dir: false,
            }],
            &host,
        )
        .await;
        let request = |stream_id, data_id| {
            ClipboardPdu::FileContentsRequest(FileContentsRequest {
                stream_id,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                requested_size: 1,
                data_id: Some(data_id),
            })
        };

        let output = process_clipboard_bytes(
            &mut stage,
            ironrdp_core::encode_vec(&request(9, 90)).unwrap(),
        );
        assert!(output.iter().all(|item| !matches!(item,
            ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
        assert!(state.lock().unwrap().local_file_suspended);
        let wire = flush_test_wire(&state, &mut stage, &host).await;
        assert!(decode_clipboard_messages(&wire)
            .iter()
            .all(|bytes| !matches!(
                decode::<ClipboardPdu<'_>>(bytes).unwrap(),
                ClipboardPdu::FileContentsResponse(_)
            )));

        // The triggering ID is ambiguous, and a duplicate cancels an unsent
        // refusal. Only the still-unique stream may get a correlated failure.
        for (stream_id, data_id) in [(9, 90), (6, 91), (6, 91), (7, 91)] {
            let output = process_clipboard_bytes(
                &mut stage,
                ironrdp_core::encode_vec(&request(stream_id, data_id)).unwrap(),
            );
            assert!(output.iter().all(|item| !matches!(item,
                ActiveStageOutput::ResponseFrame(bytes) if !bytes.is_empty())));
        }
        let wire = flush_test_wire(&state, &mut stage, &host).await;
        let responses = decode_clipboard_messages(&wire);
        assert_eq!(responses.len(), 1);
        assert!(matches!(decode::<ClipboardPdu<'_>>(&responses[0]).unwrap(),
            ClipboardPdu::FileContentsResponse(reply) if reply.stream_id() == 7 && reply.is_error()));
        {
            let guard = state.lock().unwrap();
            assert!(guard.local_file_locks.is_empty());
            assert!(guard.local_files.is_empty());
            assert!(guard.pending_local_reads.is_empty());
            assert_eq!(guard.closed_file_streams.get(&9), Some(&false));
            assert_eq!(guard.closed_file_streams.get(&6), Some(&false));
        }
        assert!(receiver.try_iter().all(|queued| !matches!(
            queued.message,
            HostMsg::FileContentsNeeded { .. } | HostMsg::Closed | HostMsg::Error(_)
        )));
        assert_video_and_other_svc_still_work(&mut stage);
    }

    fn replacement_invalidates_old_file_actions(replacement: impl FnOnce(&SharedClipboard)) {
        let (mut backend, state) = backend();
        {
            let mut guard = state.lock().unwrap();
            guard.ready = true;
            guard.files_negotiated = true;
        }
        offer_local_files(
            &state,
            7,
            vec![ClipFile {
                name: "old.bin".into(),
                size: 4,
                is_dir: false,
            }],
        );
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 1,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 0,
            data_id: None,
        });
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 2,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 4,
            data_id: None,
        });
        submit_file_contents(
            &state,
            pending_test_read(&state, 7, 3),
            Some(vec![9, 8, 7, 6]),
        );
        replacement(&state);
        let queued = actions(&state);
        assert!(!queued.iter().any(|action| matches!(action,
            ClipboardAction::AdvertiseFiles(files, _) if files.iter().any(|file| file.name == "old.bin"))));
        assert!(!queued.iter().any(|action| matches!(
            action,
            ClipboardAction::NeedFileContents { generation: 7, .. }
        )));
        for stream_id in [1, 2, 3] {
            assert!(queued.iter().any(|action| matches!(action,
                ClipboardAction::SubmitFileContents { stream_id: got, data: None, .. } if *got == stream_id)),
                "stream {stream_id} was not refused: {queued:?}");
        }
    }

    #[test]
    fn text_replacement_invalidates_old_file_actions() {
        replacement_invalidates_old_file_actions(|state| {
            offer_local_text(state, "new text".into())
        });
    }

    #[test]
    fn image_replacement_invalidates_old_file_actions() {
        replacement_invalidates_old_file_actions(|state| offer_local_image(state, vec![1, 2, 3]));
    }

    #[test]
    fn new_file_replacement_invalidates_old_file_actions() {
        replacement_invalidates_old_file_actions(|state| {
            offer_local_files(
                state,
                8,
                vec![ClipFile {
                    name: "new.bin".into(),
                    size: 2,
                    is_dir: false,
                }],
            )
        });
    }

    fn text_format(id: ClipboardFormatId) -> ClipboardFormat {
        ClipboardFormat::new(id)
    }

    /// Round-trip an owned response through the wire encoding, which is what the
    /// remote actually sees.
    fn encoded(response: &OwnedFormatDataResponse) -> Vec<u8> {
        let mut buf = vec![0u8; response.size()];
        let mut cursor = WriteCursor::new(&mut buf);
        response.encode(&mut cursor).expect("encode response");
        buf
    }

    /// A screenshot is a third format, and offering it must not look like text.
    #[test]
    fn an_offered_image_advertises_cf_dib() {
        let (mut backend, state) = backend();
        backend.on_ready();
        let _ = actions(&state);

        offer_local_image(&state, vec![0x42; 64]);

        let advertised = actions(&state);
        assert!(advertised.iter().any(|a| matches!(
            a,
            ClipboardAction::Advertise(ids) if ids.contains(&ClipboardFormatId::CF_DIB)
        )));
    }

    /// One clipboard holds one thing. Copying an image replaces the offered
    /// text, exactly as it does on Windows — otherwise the remote would be told
    /// both are available and paste whichever it preferred.
    #[test]
    fn an_image_replaces_the_offered_text() {
        let (mut backend, state) = backend();
        backend.on_ready();
        offer_local_text(&state, String::from("earlier"));
        offer_local_image(&state, vec![1, 2, 3, 4]);

        let advertised = actions(&state);
        let last = advertised
            .iter()
            .rev()
            .find_map(|a| match a {
                ClipboardAction::Advertise(ids) => Some(ids.clone()),
                _ => None,
            })
            .expect("an advertise");
        assert!(last.contains(&ClipboardFormatId::CF_DIB));
        assert!(!last.contains(&ClipboardFormatId::CF_UNICODETEXT));
    }

    /// When the remote offers text *and* an image, the text wins. A copy
    /// carrying both is text with a rendering attached — a styled run, a
    /// spreadsheet cell — and the text is what the user meant to paste.
    #[test]
    fn text_is_preferred_over_an_image_when_both_are_offered() {
        let (mut backend, state) = backend();
        backend.on_ready();
        let _ = actions(&state);

        backend.on_remote_copy(&[
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
            text_format(ClipboardFormatId::CF_UNICODETEXT),
        ]);

        let requested = actions(&state);
        assert!(requested.iter().any(|a| matches!(
            a,
            ClipboardAction::RequestText(id) if *id == ClipboardFormatId::CF_UNICODETEXT
        )));
        assert!(!requested.iter().any(|a| matches!(
            a,
            ClipboardAction::RequestText(id) if *id == ClipboardFormatId::CF_DIB
        )));
    }

    /// An image alone is asked for.
    #[test]
    fn an_image_alone_is_requested() {
        let (mut backend, state) = backend();
        backend.on_ready();
        let _ = actions(&state);

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_DIB)]);

        assert!(actions(&state).iter().any(|a| matches!(
            a,
            ClipboardAction::RequestText(id) if *id == ClipboardFormatId::CF_DIB
        )));
    }

    #[test]
    fn handshake_advertises_even_with_an_empty_local_clipboard() {
        // The initial FormatList is what drives the channel to Ready, so it has
        // to be sent whether or not we have anything to offer.
        let (mut backend, state) = backend();
        backend.on_request_format_list();
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(Vec::new())]
        );
    }

    #[test]
    fn local_text_is_not_advertised_before_the_channel_is_ready() {
        let (mut backend, state) = backend();
        offer_local_text(&state, "hello".into());
        assert!(actions(&state).is_empty());

        // ...but it rides the handshake's FormatList once it goes out.
        backend.on_request_format_list();
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );
    }

    #[test]
    fn text_copied_during_the_handshake_is_advertised_once_ready() {
        let (mut backend, state) = backend();
        backend.on_request_format_list();
        let _ = actions(&state);

        offer_local_text(&state, "late".into());
        backend.on_ready();
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );
    }

    #[test]
    fn ready_does_not_re_advertise_what_the_handshake_already_sent() {
        let (mut backend, state) = backend();
        offer_local_text(&state, "hello".into());
        backend.on_request_format_list();
        let _ = actions(&state);

        backend.on_ready();
        assert!(actions(&state).is_empty());
    }

    #[test]
    fn repeat_offers_of_the_same_text_do_not_re_advertise() {
        // The GUI polls its clipboard on a timer, so it re-offers the same text
        // constantly; the wire must not see that.
        let (mut backend, state) = backend();
        backend.on_ready();
        let _ = actions(&state);

        offer_local_text(&state, "hello".into());
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );
        offer_local_text(&state, "hello".into());
        assert!(actions(&state).is_empty());
        offer_local_text(&state, "world".into());
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );
    }

    #[test]
    fn a_remote_copy_requests_the_best_text_format_available() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[
            text_format(ClipboardFormatId::CF_TEXT),
            text_format(ClipboardFormatId::CF_UNICODETEXT),
        ]);
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::RequestText(
                ClipboardFormatId::CF_UNICODETEXT
            )]
        );
    }

    #[test]
    fn unanswered_old_format_reports_timeout_without_reusing_its_late_response() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_UNICODETEXT)]);
        let _ = actions(&state);
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_UNICODETEXT)]);
        {
            let mut guard = state.lock().unwrap();
            guard.active_remote_since = Some(Instant::now() - Duration::from_secs(11));
            guard.check_remote_timeout();
        }
        assert!(
            matches!(actions(&state).as_slice(), [ClipboardAction::Notice(message)] if message.contains("超时"))
        );
        let old = encoded(&OwnedFormatDataResponse::new_unicode_string("obsolete"));
        backend.on_format_data_response(decode::<FormatDataResponse<'_>>(&old).unwrap());
        assert!(matches!(
            actions(&state).as_slice(),
            [ClipboardAction::RequestText(
                ClipboardFormatId::CF_UNICODETEXT
            )]
        ));
        let fresh = encoded(&OwnedFormatDataResponse::new_unicode_string("fresh"));
        backend.on_format_data_response(decode::<FormatDataResponse<'_>>(&fresh).unwrap());
        assert!(
            matches!(actions(&state).as_slice(), [ClipboardAction::Inbound(text)] if text == "fresh")
        );
    }

    #[test]
    fn a_remote_copy_falls_back_to_legacy_text_formats() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_OEMTEXT)]);
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::RequestText(ClipboardFormatId::CF_OEMTEXT)]
        );
    }

    #[test]
    fn a_remote_copy_of_a_format_we_do_not_handle_is_ignored() {
        // Not every format is worth asking for: sound has nowhere to go here,
        // and requesting it would only earn an error response.
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId(12))]);
        assert!(actions(&state).is_empty());
    }

    #[test]
    fn remote_unicode_text_round_trips_to_the_app() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_UNICODETEXT)]);
        let _ = actions(&state);

        // Decode from the encoded PDU, exactly as the session would.
        let wire = encoded(&OwnedFormatDataResponse::new_unicode_string("你好 world"));
        let response: FormatDataResponse<'_> = decode(&wire).expect("decode response");
        backend.on_format_data_response(response);

        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Inbound("你好 world".into())]
        );
    }

    #[test]
    fn remote_ansi_text_is_decoded_with_the_requested_charset() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_TEXT)]);
        let _ = actions(&state);

        let wire = encoded(&OwnedFormatDataResponse::new_string("plain"));
        let response: FormatDataResponse<'_> = decode(&wire).expect("decode response");
        backend.on_format_data_response(response);

        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Inbound("plain".into())]
        );
    }

    #[test]
    fn an_error_response_produces_nothing_and_clears_the_pending_format() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_UNICODETEXT)]);
        let _ = actions(&state);

        backend.on_format_data_response(FormatDataResponse::new_error());
        assert!(actions(&state).is_empty());
        assert!(state
            .lock()
            .expect("state lock")
            .pending_remote_format
            .is_none());
    }

    #[test]
    fn an_unsolicited_response_is_ignored() {
        // Without a pending request there is no format to decode against, so
        // guessing would be worse than dropping it.
        let (mut backend, state) = backend();
        let wire = encoded(&OwnedFormatDataResponse::new_unicode_string("surprise"));
        let response: FormatDataResponse<'_> = decode(&wire).expect("decode response");
        backend.on_format_data_response(response);
        assert!(actions(&state).is_empty());
    }

    #[test]
    fn oversized_remote_text_is_dropped() {
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_UNICODETEXT)]);
        let _ = actions(&state);

        let huge = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
        let wire = encoded(&OwnedFormatDataResponse::new_unicode_string(&huge));
        let response: FormatDataResponse<'_> = decode(&wire).expect("decode response");
        backend.on_format_data_response(response);
        assert!(actions(&state).is_empty());
    }

    #[test]
    fn a_remote_paste_is_answered_with_our_local_text() {
        let (mut backend, state) = backend();
        backend.on_ready();
        offer_local_text(&state, "clip".into());
        let _ = actions(&state);

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::SubmitText {
                format: ClipboardFormatId::CF_UNICODETEXT,
                revision: Some(1),
                text: Some("clip".into()),
            }]
        );
    }

    #[test]
    fn a_remote_paste_with_nothing_copied_locally_gets_an_error_response() {
        let (mut backend, state) = backend();
        backend.on_ready();
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::SubmitText {
                format: ClipboardFormatId::CF_UNICODETEXT,
                revision: None,
                text: None,
            }]
        );
        assert!(format_data_response(ClipboardFormatId::CF_UNICODETEXT, None).is_error());
    }

    #[test]
    fn a_remote_paste_of_a_format_we_never_offered_gets_an_error_response() {
        let (mut backend, state) = backend();
        backend.on_ready();
        offer_local_text(&state, "clip".into());
        let _ = actions(&state);

        // A format with no local content behind it at all — CF_DIB would now be
        // served from the image slot, so it is no longer an example of one.
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId(12),
        });
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::SubmitText {
                format: ClipboardFormatId(12),
                revision: None,
                text: None,
            }]
        );
    }

    #[test]
    fn submitted_text_is_encoded_for_the_format_the_remote_asked_for() {
        let unicode = format_data_response(ClipboardFormatId::CF_UNICODETEXT, Some("hi"));
        assert!(!unicode.is_error());
        // UTF-16LE + a NUL terminator: 'h','\0','i','\0','\0','\0'.
        assert_eq!(unicode.data(), b"h\0i\0\0\0");

        let ansi = format_data_response(ClipboardFormatId::CF_TEXT, Some("hi"));
        assert_eq!(ansi.data(), b"hi\0");

        assert!(format_data_response(ClipboardFormatId::CF_DIB, Some("hi")).is_error());
    }

    #[test]
    fn a_redirection_resets_channel_state_but_keeps_the_users_clipboard() {
        // GNOME's system-mode handover redirects and rebuilds CLIPRDR; the text
        // the user copied did not change under them.
        let state = new_shared();
        let mut backend = AditCliprdrBackend::new(Arc::clone(&state));
        backend.on_ready();
        offer_local_text(&state, "carried over".into());
        let _ = actions(&state);

        let mut reconnected = AditCliprdrBackend::new(Arc::clone(&state));
        {
            let guard = state.lock().expect("state lock");
            assert!(!guard.ready);
            assert_eq!(guard.local_text.as_deref(), Some("carried over"));
        }
        reconnected.on_request_format_list();
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::Advertise(vec![
                ClipboardFormatId::CF_UNICODETEXT
            ])]
        );
    }
}
