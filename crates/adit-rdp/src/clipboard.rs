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

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use adit_rdp_proto::{ClipFile, HostMsg};
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

/// One unit of work the backend decided on, for the session loop to carry out.
///
/// The backend's callbacks run *inside* `ActiveStage::process`, where it cannot
/// reach the `Cliprdr` processor that owns it (it is the thing being borrowed),
/// let alone the socket. So it queues intent here and the session loop drains it
/// afterwards. That indirection is also what makes the state machine testable.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ClipboardAction {
    /// Advertise these formats to the remote (`FormatList`). An empty list is
    /// meaningful and legal: "the local clipboard holds nothing you can use".
    Advertise(Vec<ClipboardFormatId>),
    /// Ask the remote for its clipboard in this format (`FormatDataRequest`).
    RequestText(ClipboardFormatId),
    /// Answer the remote's `FormatDataRequest`. `text: None` ⇒ error response.
    SubmitText {
        format: ClipboardFormatId,
        text: Option<String>,
    },
    /// Text arrived from the remote; hand it to the app to put on the real
    /// Windows clipboard.
    Inbound(String),
    /// Answer the remote's request for an image. `image: None` ⇒ error response.
    SubmitImage {
        format: ClipboardFormatId,
        image: Option<Vec<u8>>,
    },
    /// An image arrived from the remote; hand it to the app.
    InboundImage(Vec<u8>),
    /// Refuse a request for a local file without carrying metadata or bytes.
    RefuseFileContents { stream_id: u32 },
    /// Pull a byte range of a remote file (`request_file_contents`).
    RequestFileContents(BoundRead),
    /// The remote copied files; hand the list to the app to offer locally.
    InboundFiles {
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// Bytes arrived from the remote; hand them to the waiting local paste.
    InboundFileContents(Reply),
}

impl ClipboardAction {
    fn log_metadata(&self) -> (&'static str, usize) {
        match self {
            Self::Advertise(ids) => ("advertise", ids.len()),
            Self::RequestText(_) => ("request_format", 0),
            Self::SubmitText { text, .. } => ("submit_text", text.as_ref().map_or(0, String::len)),
            Self::SubmitImage { image, .. } => ("submit_image", image.as_ref().map_or(0, Vec::len)),
            Self::Inbound(text) => ("inbound_text", text.len()),
            Self::InboundImage(image) => ("inbound_image", image.len()),
            Self::RefuseFileContents { .. } => ("refuse_file", 0),
            Self::RequestFileContents(_) => ("request_file", 0),
            Self::InboundFiles { files, .. } => ("inbound_files", files.len()),
            Self::InboundFileContents(reply) => {
                ("inbound_file_data", reply.data.as_ref().map_or(0, Vec::len))
            }
        }
    }
}

impl std::fmt::Debug for ClipboardAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, len) = self.log_metadata();
        f.debug_struct("ClipboardAction")
            .field("kind", &kind)
            .field("length", &len)
            .finish()
    }
}

fn log_clipboard_failure(action: &ClipboardAction, phase: &'static str) {
    let (kind, length) = action.log_metadata();
    tracing::warn!(kind, length, phase, "clipboard PDU failed");
}

/// Rejoin a descriptor's two halves into the flat form [`ClipFile`] uses.
fn join_relative(descriptor: &FileDescriptor) -> String {
    match descriptor.relative_path.as_deref() {
        Some(dir) if !dir.is_empty() => format!("{dir}\\{}", descriptor.name),
        _ => descriptor.name.clone(),
    }
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
#[derive(Debug, Default)]
pub(crate) struct ClipboardState {
    /// Newest text the app offered from the local (GUI-owned) clipboard.
    local_text: Option<String>,
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
    /// CLIPRDR reached `Ready` (capabilities exchanged and our first
    /// `FormatList` acknowledged). Advertising before that is out of sequence.
    ready: bool,
    /// The lock id the server handed us with the remote's file list. Quoted back
    /// on every `request_file_contents` so the bytes come from the snapshot that
    /// was copied, not from whatever the remote clipboard holds by the time the
    /// paste actually runs.
    remote_files: RemoteFiles,
    /// Whether the server negotiated file transfer. Remote-to-local requests
    /// need this capability even though local-to-remote files are disabled.
    files_negotiated: bool,
    actions: VecDeque<ClipboardAction>,
}

impl ClipboardState {
    fn revoke_remote_files(&mut self, expects_files: bool) {
        let (had_offer, replies) = self.remote_files.revoke(expects_files);
        self.actions.extend(
            replies
                .into_iter()
                .map(ClipboardAction::InboundFileContents),
        );
        if had_offer {
            self.actions.push_back(ClipboardAction::InboundFiles {
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
        self.actions.push_back(ClipboardAction::Advertise(formats));
    }

    /// Record an image the app copied locally and offer its format to the
    /// remote. The DIB bytes cross only when the remote pastes.
    pub(crate) fn offer_local_image(&mut self, image: Vec<u8>) {
        if self.local_image.as_ref().is_some_and(|held| *held == image) {
            return;
        }
        self.revoke_remote_files(false);
        self.local_image = Some(image);
        // A local image replaces the text previously offered.
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
        self.local_text = Some(text);
        self.local_image = None;
        // Each new FormatList replaces the prior image offer.
        if self.ready {
            self.queue_advertise();
        }
    }

    /// Retire only our local text/image offer; remote file snapshots are a
    /// separate direction and must survive clipboard invalidation.
    fn clear_local_offer(&mut self, advertise: bool) {
        let had_local = self.local_text.take().is_some() | self.local_image.take().is_some();
        self.advertised_text = None;
        self.advertised_image_len = None;
        if had_local && self.ready {
            self.actions
                .retain(|action| !matches!(action, ClipboardAction::Advertise(_)));
        }
        for action in &mut self.actions {
            match action {
                // Before Ready, sanitize the initial list rather than dropping
                // the message the handshake needs to complete.
                ClipboardAction::Advertise(ids) if had_local => ids.clear(),
                ClipboardAction::SubmitText { text, .. } => *text = None,
                ClipboardAction::SubmitImage { image, .. } => *image = None,
                _ => {}
            }
        }
        if advertise && had_local && self.ready {
            self.queue_advertise();
        }
    }

    pub(crate) fn take_actions(&mut self) -> Vec<ClipboardAction> {
        self.actions.drain(..).collect()
    }

    /// Forget everything scoped to one CLIPRDR channel. The offered text is
    /// deliberately kept: a GNOME system-mode handover redirects and rebuilds
    /// the channel, and the user's clipboard did not change under them.
    fn reset_for_new_channel(&mut self) {
        self.advertised_text = None;
        self.advertised_image_len = None;
        self.pending_remote_format = None;
        self.ready = false;
        // Scoped to the channel, not to the user's clipboard: the lock id and the
        // negotiated capability both belong to the CLIPRDR session that just went
        // away.
        self.files_negotiated = false;
        self.actions.clear();
        self.revoke_remote_files(false);
    }
}

pub(crate) type SharedClipboard = Arc<Mutex<ClipboardState>>;

pub(crate) fn new_shared() -> SharedClipboard {
    Arc::new(Mutex::new(ClipboardState::default()))
}

/// Hand text the app just copied locally to the CLIPRDR state machine. A
/// poisoned lock is ignored rather than propagated — losing a clipboard offer is
/// not worth tearing a live desktop session down.
pub(crate) fn offer_local_text(state: &SharedClipboard, text: String) {
    if let Ok(mut guard) = state.lock() {
        guard.offer_local_text(text);
    }
}

/// Hand an image the app just copied locally to the CLIPRDR state machine.
pub(crate) fn offer_local_image(state: &SharedClipboard, image: Vec<u8>) {
    if let Ok(mut guard) = state.lock() {
        guard.offer_local_image(image);
    }
}

/// Clear the local clipboard cache without revoking the remote file offer.
pub(crate) fn clear_local_offer(state: &SharedClipboard) {
    if let Ok(mut state) = state.lock() {
        state.clear_local_offer(true);
    }
}

/// Request a range from the captured remote file offer.
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
        guard.actions.push_back(action);
    }
}

fn fail_remote_read(state: &SharedClipboard, wire_id: u32) {
    if let Ok(mut guard) = state.lock() {
        if let Some(reply) = guard.remote_files.fail(wire_id) {
            guard
                .actions
                .push_back(ClipboardAction::InboundFileContents(reply));
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

    /// Answer a file request with the protocol error response. Every refusal
    /// goes through here so a request is never simply dropped: the remote paste
    /// would otherwise hang waiting for a stream that never comes.
    fn refuse(&self, stream_id: u32) {
        self.with_state(|state| {
            state
                .actions
                .push_back(ClipboardAction::RefuseFileContents { stream_id });
        });
    }
}

ironrdp_core::impl_as_any!(AditCliprdrBackend);

impl CliprdrBackend for AditCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        // Announced for servers that require the field during the handshake.
        // Remote file contents are fetched on demand rather than staged here.
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
            // The app may have copied something while the handshake was in
            // flight; that text missed the initial FormatList.
            if state.local_text != state.advertised_text {
                state.queue_advertise();
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
            if !files {
                state.revoke_remote_files(false);
            }
        });
        tracing::debug!(?capabilities, files, "CLIPRDR capabilities negotiated");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // A file list is delay-rendered by the remote too: the FormatList only
        // names the format, and `initiate_paste` is what fetches the descriptors.
        // They arrive at `on_remote_file_list` below rather than at
        // `on_format_data_response`, which is why this returns early instead of
        // recording a `pending_remote_format`.
        let files = available_formats
            .iter()
            .find(|format| format.name().is_some_and(|n| n.value() == FILE_LIST_FORMAT));
        self.with_state(|state| {
            // The remote now owns the clipboard. Never answer requests with a
            // local copy from before that ownership change.
            state.clear_local_offer(false);
            state.revoke_remote_files(files.is_some());
        });
        if let Some(format) = files {
            self.with_state(|state| {
                state
                    .actions
                    .push_back(ClipboardAction::RequestText(format.id));
            });
            return;
        }

        let text_available = TEXT_FORMATS
            .iter()
            .any(|wanted| available_formats.iter().any(|f| f.id == *wanted));
        if !text_available {
            // Images only when there is no text. A copy offering both is text
            // with a rendering attached (a spreadsheet cell, a styled run), and
            // the text is what the user meant to paste.
            if let Some(format) = IMAGE_FORMATS
                .iter()
                .copied()
                .find(|wanted| available_formats.iter().any(|f| f.id == *wanted))
            {
                self.with_state(|state| {
                    state.pending_remote_format = Some(format);
                    state
                        .actions
                        .push_back(ClipboardAction::RequestText(format));
                });
                return;
            }
        }
        let chosen = TEXT_FORMATS
            .iter()
            .copied()
            .find(|wanted| available_formats.iter().any(|f| f.id == *wanted));
        match chosen {
            Some(format) => {
                self.with_state(|state| {
                    state.pending_remote_format = Some(format);
                    state
                        .actions
                        .push_back(ClipboardAction::RequestText(format));
                });
            }
            None => {
                tracing::debug!(
                    formats = ?available_formats.iter().map(|f| f.id.value()).collect::<Vec<_>>(),
                    "remote clipboard holds no text format we support; ignoring"
                );
            }
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let format = request.format;
        self.with_state(|state| {
            if is_image_format(format) {
                let image = state.local_image.clone();
                if image.is_none() {
                    tracing::debug!(format = format.value(), "no image to serve");
                }
                state
                    .actions
                    .push_back(ClipboardAction::SubmitImage { format, image });
                return;
            }
            let text = if is_text_format(format) {
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
            state
                .actions
                .push_back(ClipboardAction::SubmitText { format, text });
        });
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let format = self
            .with_state(|state| state.pending_remote_format.take())
            .flatten();
        if response.is_error() {
            tracing::warn!(?format, "remote refused to hand over its clipboard");
            return;
        }
        let Some(format) = format else {
            // Nothing correlates this response to a request of ours.
            tracing::warn!("unsolicited CLIPRDR format data response; ignoring");
            return;
        };
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
                state
                    .actions
                    .push_back(ClipboardAction::InboundImage(image));
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
                self.with_state(|state| state.actions.push_back(ClipboardAction::Inbound(text)));
            }
            Err(error) => tracing::warn!("could not decode remote clipboard text: {error}"),
        }
    }

    /// Refuse a remote request for local file metadata or content.
    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        // Reject SIZE and RANGE alike. Never ask the app to read a local file,
        // even when a peer sends a stale or unsolicited request.
        self.refuse(request.stream_id);
    }

    /// Bytes (or a size) arriving for a local paste we started.
    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let wire_id = response.stream_id();
        let data = (!response.is_error()
            && response.data().len() <= adit_rdp_proto::FILE_CHUNK_BYTES as usize)
            .then(|| response.data().to_vec());
        self.with_state(|state| {
            if let Some(reply) = state.remote_files.respond(wire_id, data) {
                state
                    .actions
                    .push_back(ClipboardAction::InboundFileContents(reply));
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
            let expects_files = state.remote_files.expects_files();
            state.revoke_remote_files(expects_files);
            let accepted =
                state.files_negotiated && state.remote_files.install(listed.clone(), clip_data_id);
            state.actions.push_back(ClipboardAction::InboundFiles {
                generation: state.remote_files.generation(),
                files: if accepted { listed } else { Vec::new() },
            });
        });
    }

    // Inbound locks concern the *server's* view of our clipboard. `Cliprdr`
    // tracks them itself, and there is nothing for this layer to release: a
    // delay-rendered file is read from its real path when asked for, so no
    // snapshot was ever taken that a lock could be pinning.

    fn on_lock(&mut self, data_id: LockDataId) {
        tracing::debug!(?data_id, "server locked our clipboard data");
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        tracing::debug!(?data_id, "server released our clipboard data");
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
        ClipboardAction::InboundFiles { generation, files } => {
            Ok(HostMsg::ClipboardFiles { generation, files })
        }
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
/// Returns `None` when the app stopped listening (the caller should end the
/// session). Protocol errors are logged and swallowed on purpose: a clipboard
/// hiccup — a server that never opened the channel, a PDU we could not encode —
/// must never take a working desktop down with it.
pub(crate) fn pump(
    state: &SharedClipboard,
    active_stage: &mut ActiveStage,
    host_tx: &std_mpsc::Sender<HostMsg>,
) -> Option<Vec<Vec<u8>>> {
    let actions = match state.lock() {
        Ok(mut guard) => guard.take_actions(),
        Err(_) => return Some(Vec::new()),
    };
    if actions.is_empty() {
        return Some(Vec::new());
    }

    let mut frames = Vec::new();
    for action in actions {
        // Four of the actions are addressed to the app rather than the wire.
        // `for_app` hands back the ones that are not, because matching by value
        // would otherwise move the action away from the CLIPRDR branch below.
        let action = match for_app(action) {
            Ok(message) => {
                if host_tx.send(message).is_err() {
                    return None;
                }
                continue;
            }
            Err(action) => action,
        };

        let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() else {
            tracing::warn!("CLIPRDR channel is not open; dropping clipboard action");
            if let ClipboardAction::RequestFileContents(read) = &action {
                fail_remote_read(state, read.wire_id);
            }
            continue;
        };
        let messages = match &action {
            ClipboardAction::Advertise(ids) => {
                let formats: Vec<ClipboardFormat> =
                    ids.iter().copied().map(ClipboardFormat::new).collect();
                cliprdr.initiate_copy(&formats)
            }
            ClipboardAction::RequestText(format) => cliprdr.initiate_paste(*format),
            ClipboardAction::SubmitText { format, text } => {
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
            ClipboardAction::RefuseFileContents { stream_id } => {
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
            | ClipboardAction::InboundFiles { .. }
            | ClipboardAction::InboundFileContents(_) => {
                unreachable!("app-addressed actions are handled above")
            }
        };
        let messages = match messages {
            Ok(messages) => messages,
            Err(_error) => {
                log_clipboard_failure(&action, "build");
                if let ClipboardAction::RequestFileContents(read) = &action {
                    fail_remote_read(state, read.wire_id);
                }
                continue;
            }
        };
        match active_stage.process_svc_processor_messages(messages) {
            Ok(frame) => frames.push(frame),
            Err(_error) => {
                log_clipboard_failure(&action, "encode");
                if let ClipboardAction::RequestFileContents(read) = &action {
                    fail_remote_read(state, read.wire_id);
                }
            }
        }
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, Encode as _, WriteCursor};

    use super::*;

    #[test]
    fn repro_action_debug_must_not_expose_clipboard_payloads() {
        let action = ClipboardAction::SubmitText {
            format: ClipboardFormatId::CF_UNICODETEXT,
            text: Some("private-clipboard-marker".into()),
        };
        let rendered = format!("{action:?}");
        assert!(!rendered.contains("private-clipboard-marker"));
        assert!(rendered.contains("submit_text"));
        assert!(rendered.contains("24"));
        let image = ClipboardAction::SubmitImage {
            format: ClipboardFormatId::CF_DIB,
            image: Some(vec![17, 34, 51]),
        };
        let rendered = format!("{image:?}");
        assert!(!rendered.contains("17"));
        assert!(rendered.contains("submit_image"));
        assert_eq!(image.log_metadata(), ("submit_image", 3));
    }

    #[test]
    fn clipboard_failure_logs_never_include_text_or_image_bytes() {
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false)
            .with_writer(move || writer.clone()).finish();
        tracing::subscriber::with_default(subscriber, || {
            log_clipboard_failure(&ClipboardAction::SubmitText {
                format: ClipboardFormatId::CF_UNICODETEXT,
                text: Some("sensitive-fixture-marker".into()),
            }, "build");
            log_clipboard_failure(&ClipboardAction::SubmitImage {
                format: ClipboardFormatId::CF_DIB,
                image: Some(vec![17, 34, 51]),
            }, "encode");
        });
        let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(rendered.contains("submit_text") && rendered.contains("submit_image"));
        assert!(rendered.contains("build") && rendered.contains("encode"));
        assert!(!rendered.contains("sensitive-fixture-marker"));
        assert!(!rendered.contains("17, 34, 51"));
    }

    #[test]
    fn clearing_local_offer_refuses_stale_text_image_and_queued_responses() {
        let (mut backend, state) = backend();
        backend.on_ready();
        offer_local_text(&state, "private-text".into());
        let _ = actions(&state);
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        clear_local_offer(&state);
        let queued = actions(&state);
        assert!(queued
            .iter()
            .any(|a| matches!(a, ClipboardAction::SubmitText { text: None, .. })));
        assert!(queued
            .iter()
            .any(|a| matches!(a, ClipboardAction::Advertise(ids) if ids.is_empty())));
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert!(matches!(
            actions(&state).as_slice(),
            [ClipboardAction::SubmitText { text: None, .. }]
        ));
        offer_local_image(&state, vec![11, 22, 33]);
        let _ = actions(&state);
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        clear_local_offer(&state);
        assert!(actions(&state)
            .iter()
            .any(|a| matches!(a, ClipboardAction::SubmitImage { image: None, .. })));
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        assert!(matches!(
            actions(&state).as_slice(),
            [ClipboardAction::SubmitImage { image: None, .. }]
        ));
        offer_local_text(&state, "new-text".into());
        let _ = actions(&state);
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert!(
            matches!(actions(&state).as_slice(), [ClipboardAction::SubmitText { text: Some(text), .. }] if text == "new-text")
        );
    }

    #[test]
    fn clearing_before_ready_sanitizes_the_required_handshake_list() {
        let (mut backend, state) = backend();
        offer_local_text(&state, "before-handshake".into());
        backend.on_request_format_list();
        clear_local_offer(&state);
        assert!(
            matches!(actions(&state).as_slice(), [ClipboardAction::Advertise(ids)] if ids.is_empty())
        );
        backend.on_ready();
        offer_local_text(&state, "after-handshake".into());
        assert!(actions(&state).iter().any(|a| matches!(a, ClipboardAction::Advertise(ids) if ids.contains(&ClipboardFormatId::CF_UNICODETEXT))));
    }

    #[test]
    fn clearing_local_offer_does_not_revoke_a_remote_file_snapshot() {
        let (mut backend, state) = backend();
        {
            let mut state = state.lock().unwrap();
            state.files_negotiated = true;
            state.revoke_remote_files(true);
        }
        let mut descriptor = FileDescriptor::new("remote.txt");
        descriptor.file_size = Some(4);
        backend.on_remote_file_list(&[descriptor], Some(7));
        assert!(state.lock().unwrap().remote_files.active());
        let _ = actions(&state);
        clear_local_offer(&state);
        assert!(state.lock().unwrap().remote_files.active());
        assert!(actions(&state).is_empty());
    }

    #[test]
    fn real_cliprdr_refuses_unsolicited_file_metadata_and_content_requests() {
        use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatListResponse};
        use ironrdp_svc::{SvcMessage, SvcProcessor as _};

        let state = new_shared();
        let backend = AditCliprdrBackend::new(Arc::clone(&state));
        let mut clip = CliprdrClient::new(Box::new(backend));

        // Drive the real processor to Ready using its normal wire handshake.
        let ready = ClipboardPdu::MonitorReady;
        clip.process(&ironrdp_core::encode_vec(&ready).unwrap())
            .unwrap();
        assert!(
            matches!(actions(&state).as_slice(), [ClipboardAction::Advertise(ids)] if ids.is_empty())
        );
        let initial: Vec<SvcMessage> = clip.initiate_copy(&[]).unwrap().into();
        assert_eq!(initial.len(), 3); // capabilities, temporary directory, empty format list
        let ack = ClipboardPdu::FormatListResponse(FormatListResponse::Ok);
        clip.process(&ironrdp_core::encode_vec(&ack).unwrap())
            .unwrap();
        assert!(actions(&state).is_empty());

        // A registered file-list format can only be serviced inline by IronRDP
        // if initiate_file_copy installed a list. The new helper never does so.
        let list_request = ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId(0xC001),
        });
        assert!(clip
            .process(&ironrdp_core::encode_vec(&list_request).unwrap())
            .unwrap()
            .is_empty());
        let queued = actions(&state);
        assert!(matches!(
            queued.as_slice(),
            [ClipboardAction::SubmitText {
                format: ClipboardFormatId(0xC001),
                text: None
            }]
        ));
        let refusal: Vec<SvcMessage> = clip
            .submit_format_data(format_data_response(ClipboardFormatId(0xC001), None))
            .unwrap()
            .into();
        let refusal_wire = refusal[0].encode_unframed_pdu().unwrap();
        assert!(matches!(
            decode::<ClipboardPdu<'_>>(&refusal_wire).unwrap(),
            ClipboardPdu::FormatDataResponse(ref response) if response.is_error()
        ));

        for (stream_id, flags, size) in [
            (10, FileContentsFlags::SIZE, 8),
            (11, FileContentsFlags::RANGE, 32),
        ] {
            let request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
                stream_id,
                index: 0,
                flags,
                position: 0,
                requested_size: size,
                data_id: Some(3),
            });
            let messages = clip
                .process(&ironrdp_core::encode_vec(&request).unwrap())
                .unwrap();
            assert_eq!(messages.len(), 1);
            let wire = messages[0].encode_unframed_pdu().unwrap();
            let response = decode::<ClipboardPdu<'_>>(&wire).unwrap();
            assert!(matches!(response, ClipboardPdu::FileContentsResponse(ref r)
                if r.stream_id() == stream_id && r.is_error() && r.data().is_empty()));
            assert!(actions(&state).is_empty());
        }

        // The backend itself also refuses if an upstream processor ever calls it.
        let mut backend = AditCliprdrBackend {
            state: Arc::clone(&state),
        };
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 12,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 1,
            data_id: None,
        });
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::RefuseFileContents { stream_id: 12 }]
        );
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
