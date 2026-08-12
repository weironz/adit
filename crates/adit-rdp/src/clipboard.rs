//! CLIPRDR (MS-RDPECLIP) adapter — **text only**.
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
//! ## Scope
//!
//! Text only: we advertise `CF_UNICODETEXT` and accept `CF_UNICODETEXT` /
//! `CF_TEXT` / `CF_OEMTEXT` from the remote. **Images (`CF_DIB`) and file
//! transfer (`FileGroupDescriptorW`) are explicitly out of scope** — files need
//! a real temporary directory, chunked `FileContents` streaming and clipboard
//! locking, none of which the helper IPC carries. The file/lock callbacks below
//! are deliberate no-ops, not oversights.
//!
//! ## Direction asymmetry (deliberate)
//!
//! Local → remote is *delay-rendered*, like mstsc: we only advertise the format
//! list, and the text crosses the wire when something on the remote actually
//! pastes. Remote → local is *eager*: as soon as the remote advertises text we
//! request it, because the app cannot answer a synchronous "give me the
//! clipboard now" from Windows across an async IPC hop.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use adit_rdp_proto::{ClipFile, HostMsg};
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId,
    ClipboardGeneralCapabilityFlags, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
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

/// Ceiling on one inbound file chunk. `FILE_CHUNK_BYTES` is what we *ask* for;
/// this is what we are willing to accept, with room for a server that rounds up.
const MAX_FILE_CHUNK_BYTES: usize = 4 * 1024 * 1024;

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
    /// Offer a local file selection to the remote (`initiate_file_copy`).
    AdvertiseFiles(Vec<ClipFile>),
    /// Answer the remote's `FileContentsRequest` with bytes the app read.
    SubmitFileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// Pull a byte range of a remote file (`request_file_contents`).
    RequestFileContents {
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// The remote copied files; hand the list to the app to offer locally.
    InboundFiles(Vec<ClipFile>),
    /// The remote is pasting and needs bytes from a local file; ask the app.
    NeedFileContents {
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// Bytes arrived from the remote; hand them to the waiting local paste.
    InboundFileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
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
    /// Files the app offered from the local clipboard, flat with relative paths.
    /// Held here rather than only inside `Cliprdr` because a `SIZE` request is
    /// answered straight from this metadata without troubling the app.
    local_files: Vec<ClipFile>,
    /// Same repeat-suppression as `advertised_text`: the GUI polls on a timer.
    advertised_files: Vec<ClipFile>,
    /// The lock id the server handed us with the remote's file list. Quoted back
    /// on every `request_file_contents` so the bytes come from the snapshot that
    /// was copied, not from whatever the remote clipboard holds by the time the
    /// paste actually runs.
    remote_clip_data_id: Option<u32>,
    /// Whether the server negotiated file transfer at all. Without the
    /// capability `initiate_file_copy` and `request_file_contents` both refuse,
    /// so this is what stops us queueing work that can only fail.
    files_negotiated: bool,
    actions: VecDeque<ClipboardAction>,
}

impl ClipboardState {
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
        self.local_image = Some(image);
        // One clipboard holds one thing: an image copy replaces whatever text
        // or file list was offered, exactly as it does on Windows.
        self.local_text = None;
        self.local_files.clear();
        if self.ready {
            self.queue_advertise();
        }
    }

    pub(crate) fn offer_local_text(&mut self, text: String) {
        if self.local_text.as_deref() == Some(text.as_str()) {
            return;
        }
        self.local_text = Some(text);
        self.local_image = None;
        // One clipboard holds one thing. MS-RDPECLIP agrees: each FormatList
        // completely replaces the last, and `initiate_copy` drops the file list
        // upstream — mirroring it here keeps our idea of what is offered from
        // drifting away from the processor's.
        self.local_files.clear();
        self.advertised_files.clear();
        if self.ready {
            self.queue_advertise();
        }
    }

    /// Record files the app copied locally and offer them to the remote.
    ///
    /// Nothing is read from disk here or at any point in this process: only the
    /// metadata crosses. Bytes are pulled one range at a time when — and only
    /// when — something on the remote actually pastes.
    pub(crate) fn offer_local_files(&mut self, files: Vec<ClipFile>) {
        if self.local_files == files {
            return;
        }
        self.local_files = files;
        self.local_text = None;
        self.local_image = None;
        self.advertised_text = None;
        if self.ready && self.files_negotiated {
            self.advertised_files = self.local_files.clone();
            self.actions
                .push_back(ClipboardAction::AdvertiseFiles(self.local_files.clone()));
        }
    }

    /// Look up an offered file by its index in the list the remote was given.
    fn local_file(&self, index: u32) -> Option<&ClipFile> {
        self.local_files.get(usize::try_from(index).ok()?)
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
        self.advertised_files.clear();
        self.pending_remote_format = None;
        self.ready = false;
        // Scoped to the channel, not to the user's clipboard: the lock id and the
        // negotiated capability both belong to the CLIPRDR session that just went
        // away. `local_files` deliberately survives, exactly as `local_text` does.
        self.remote_clip_data_id = None;
        self.files_negotiated = false;
        self.actions.clear();
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

/// Hand a local file selection to the CLIPRDR state machine. Metadata only —
/// nothing is read from disk until the remote pastes.
pub(crate) fn offer_local_files(state: &SharedClipboard, files: Vec<ClipFile>) {
    if let Ok(mut guard) = state.lock() {
        guard.offer_local_files(files);
    }
}

/// Queue the app's answer to a [`HostMsg::FileContentsNeeded`].
pub(crate) fn submit_file_contents(
    state: &SharedClipboard,
    stream_id: u32,
    data: Option<Vec<u8>>,
) {
    if let Ok(mut guard) = state.lock() {
        guard
            .actions
            .push_back(ClipboardAction::SubmitFileContents { stream_id, data });
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
    stream_id: u32,
    index: u32,
    offset: u64,
    length: u32,
) {
    if let Ok(mut guard) = state.lock() {
        if !guard.files_negotiated {
            guard
                .actions
                .push_back(ClipboardAction::InboundFileContents {
                    stream_id,
                    data: None,
                });
            return;
        }
        guard.actions.push_back(ClipboardAction::RequestFileContents {
            stream_id,
            index,
            offset,
            length: length.min(adit_rdp_proto::FILE_CHUNK_BYTES),
        });
    }
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
            state.actions.push_back(ClipboardAction::SubmitFileContents {
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

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        let files = capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED);
        self.with_state(|state| {
            state.files_negotiated = files;
            // Files copied before the handshake finished missed their chance to
            // be advertised; this is the first moment we know they can be.
            if files && state.ready && state.advertised_files != state.local_files {
                state.advertised_files = state.local_files.clone();
                state
                    .actions
                    .push_back(ClipboardAction::AdvertiseFiles(state.local_files.clone()));
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
                    state.actions.push_back(ClipboardAction::RequestText(format));
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
                    state.actions.push_back(ClipboardAction::RequestText(format));
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
                tracing::warn!(len = bytes.len(), "remote clipboard image out of range; dropping");
                return;
            }
            let image = bytes.to_vec();
            self.with_state(|state| {
                state.actions.push_back(ClipboardAction::InboundImage(image));
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

    /// The remote is pasting files we offered and wants either a file size or a
    /// byte range from one.
    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let stream_id = request.stream_id;
        // `lindex` is signed on the wire and negative is invalid. Upstream
        // rejects it during decode, but the conversion has to be total anyway.
        let Ok(index) = u32::try_from(request.index) else {
            tracing::warn!(stream_id, index = request.index, "negative file index; refusing");
            self.refuse(stream_id);
            return;
        };

        self.with_state(|state| {
            let Some(file) = state.local_file(index).cloned() else {
                tracing::warn!(stream_id, index, "request for a file we never offered");
                state.actions.push_back(ClipboardAction::SubmitFileContents {
                    stream_id,
                    data: None,
                });
                return;
            };

            // A SIZE request is answered from metadata already in hand — no round
            // trip to the app and no disk read. Explorer issues one per file
            // before reading a single byte, so this is most of the traffic.
            if request.flags.contains(FileContentsFlags::SIZE) {
                state.actions.push_back(ClipboardAction::SubmitFileContents {
                    stream_id,
                    data: Some(file.size.to_le_bytes().to_vec()),
                });
                return;
            }

            if file.is_dir {
                tracing::warn!(stream_id, index, "byte range requested from a directory");
                state.actions.push_back(ClipboardAction::SubmitFileContents {
                    stream_id,
                    data: None,
                });
                return;
            }

            // Clamped rather than honoured: `cbRequested` is remote-controlled,
            // and an outsized value would have the app allocate it and then try
            // to frame it down the same pipe the desktop is drawn through.
            let length = request.requested_size.min(adit_rdp_proto::FILE_CHUNK_BYTES);
            state.actions.push_back(ClipboardAction::NeedFileContents {
                stream_id,
                index,
                offset: request.position,
                length,
            });
        });
    }

    /// Bytes (or a size) arriving for a local paste we started.
    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let stream_id = response.stream_id();
        let data = if response.is_error() {
            tracing::warn!(stream_id, "remote refused a file contents request");
            None
        } else if response.data().len() > MAX_FILE_CHUNK_BYTES {
            // A response is not obliged to respect the size we asked for.
            tracing::warn!(
                stream_id,
                len = response.data().len(),
                "remote file chunk exceeds the transfer cap; dropping"
            );
            None
        } else {
            Some(response.data().to_vec())
        };
        self.with_state(|state| {
            state
                .actions
                .push_back(ClipboardAction::InboundFileContents { stream_id, data });
        });
    }

    /// Descriptors for a file list the remote copied.
    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        if files.len() > MAX_CLIPBOARD_FILES {
            tracing::warn!(count = files.len(), "remote file list is implausibly long; dropping");
            return;
        }
        let listed: Vec<ClipFile> = files.iter().map(from_descriptor).collect();
        self.with_state(|state| {
            state.remote_clip_data_id = clip_data_id;
            state.actions.push_back(ClipboardAction::InboundFiles(listed));
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
        ClipboardAction::InboundFiles(files) => Ok(HostMsg::ClipboardFiles(files)),
        ClipboardAction::NeedFileContents {
            stream_id,
            index,
            offset,
            length,
        } => Ok(HostMsg::FileContentsNeeded {
            stream_id,
            index,
            offset,
            length,
        }),
        ClipboardAction::InboundFileContents { stream_id, data } => {
            Ok(HostMsg::FileContents { stream_id, data })
        }
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
            ClipboardAction::AdvertiseFiles(files) => {
                cliprdr.initiate_file_copy(files.iter().map(to_descriptor).collect())
            }
            ClipboardAction::SubmitFileContents { stream_id, data } => {
                cliprdr.submit_file_contents(match data {
                    Some(bytes) => FileContentsResponse::new_data_response(*stream_id, bytes.clone()),
                    None => FileContentsResponse::new_error(*stream_id),
                })
            }
            ClipboardAction::RequestFileContents {
                stream_id,
                index,
                offset,
                length,
            } => {
                // `data_id` quotes the lock the server took when it advertised
                // the list, so the bytes come from the snapshot that was copied
                // rather than from whatever is on the remote clipboard now.
                let data_id = state.lock().ok().and_then(|guard| guard.remote_clip_data_id);
                cliprdr.request_file_contents(FileContentsRequest {
                    stream_id: *stream_id,
                    index: i32::try_from(*index).unwrap_or(i32::MAX),
                    flags: FileContentsFlags::RANGE,
                    position: *offset,
                    requested_size: *length,
                    data_id,
                })
            }
            // Handled above; the borrow of `cliprdr` is what forces this shape.
            ClipboardAction::Inbound(_)
            | ClipboardAction::InboundImage(_)
            | ClipboardAction::InboundFiles(_)
            | ClipboardAction::NeedFileContents { .. }
            | ClipboardAction::InboundFileContents { .. } => {
                unreachable!("app-addressed actions are handled above")
            }
        };
        let messages = match messages {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!(?action, "could not build clipboard PDU: {error}");
                continue;
            }
        };
        match active_stage.process_svc_processor_messages(messages) {
            Ok(frame) => frames.push(frame),
            Err(error) => tracing::warn!(?action, "could not encode clipboard PDU: {error}"),
        }
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, Encode as _, WriteCursor};

    use super::*;

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
        assert_eq!(actions(&state), vec![ClipboardAction::Advertise(Vec::new())]);
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

        assert_eq!(actions(&state), vec![ClipboardAction::Inbound("plain".into())]);
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
