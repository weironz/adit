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

use adit_rdp_proto::HostMsg;
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
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
}

/// Everything the backend and the session loop share. Session-scoped fields are
/// reset when a new backend is built, because a server redirection rebuilds the
/// CLIPRDR channel from scratch while the app's offered text stays valid.
#[derive(Debug, Default)]
pub(crate) struct ClipboardState {
    /// Newest text the app offered from the local (GUI-owned) clipboard.
    local_text: Option<String>,
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
    actions: VecDeque<ClipboardAction>,
}

impl ClipboardState {
    /// Formats we can currently serve. Empty when the app has offered nothing:
    /// advertising `CF_UNICODETEXT` with no text behind it would make every
    /// remote paste answer with an error response.
    fn local_formats(&self) -> Vec<ClipboardFormatId> {
        match self.local_text {
            Some(_) => vec![ClipboardFormatId::CF_UNICODETEXT],
            None => Vec::new(),
        }
    }

    fn queue_advertise(&mut self) {
        self.advertised_text = self.local_text.clone();
        let formats = self.local_formats();
        self.actions.push_back(ClipboardAction::Advertise(formats));
    }

    /// Record text the app copied locally and, once the channel is up, offer it
    /// to the remote. Before `Ready` we only stash it: the initial `FormatList`
    /// is part of the handshake and is sent from `on_request_format_list`.
    pub(crate) fn offer_local_text(&mut self, text: String) {
        if self.local_text.as_deref() == Some(text.as_str()) {
            return;
        }
        self.local_text = Some(text);
        if self.ready {
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
        self.pending_remote_format = None;
        self.ready = false;
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
}

ironrdp_core::impl_as_any!(AditCliprdrBackend);

impl CliprdrBackend for AditCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        // Required by CLIPRDR_TEMP_DIRECTORY, but never used: file transfer is
        // out of scope, so the remote will never ask us to stage a file here.
        "."
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // No file-clip, no locking, no huge-file support — text only. `Cliprdr`
        // adds USE_LONG_FORMAT_NAMES itself.
        ClipboardGeneralCapabilityFlags::empty()
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
        tracing::debug!(?capabilities, "CLIPRDR capabilities negotiated");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
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

    // ---- Out of scope: file transfer and the clipboard locking that guards it.
    // We never advertise STREAM_FILECLIP_ENABLED or CAN_LOCK_CLIPDATA, so a
    // well-behaved server never sends these. Ignoring them is the correct
    // response to one that does anyway.

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
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
        // `Inbound` goes to the app, not the wire.
        if let ClipboardAction::Inbound(text) = action {
            if host_tx.send(HostMsg::ClipboardText(text)).is_err() {
                return None;
            }
            continue;
        }

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
            // Handled above; the borrow of `cliprdr` is what forces this shape.
            ClipboardAction::Inbound(_) => unreachable!("inbound handled above"),
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
    fn a_remote_copy_of_a_bitmap_is_ignored() {
        // Images are out of scope; asking for one would only earn an error.
        let (mut backend, state) = backend();
        backend.on_remote_copy(&[text_format(ClipboardFormatId::CF_DIB)]);
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

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        assert_eq!(
            actions(&state),
            vec![ClipboardAction::SubmitText {
                format: ClipboardFormatId::CF_DIB,
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
