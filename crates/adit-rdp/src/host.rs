//! IPC host: bridges the child process's stdin/stdout pipes to the RDP session.
//!
//! Two dedicated threads own the binary stdio so it never mixes with the async
//! RDP socket loop:
//! * the **stdin reader** negotiates IPC, then decodes [`ClientMsg`]s — the first
//!   `Connect` starts the session, and `Close`/EOF ends it;
//! * the **stdout writer** serialises [`HostMsg`]s from the session.
//!
//! Anything the helper prints to stdout that isn't a framed [`HostMsg`] would
//! corrupt the stream, so logs must go to stderr only.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use adit_rdp_proto::{
    read_client_hello, read_msg, write_helper_hello, write_msg, ClientMsg, ConnectRequest, HostMsg,
    SharedClipboardPolicy, IPC_PROTOCOL_VERSION,
};
use tokio::sync::{mpsc as tokio_mpsc, watch, Notify, OwnedSemaphorePermit, Semaphore};

use crate::session::{run_session, SessionCmd};
use crate::RdpError;

enum HelloResponse {
    Accepted,
    VersionMismatch,
    Unversioned,
}

// A helper belongs to exactly one session, so these limits cannot consume
// another session's budget. The pipe reader waits only a bounded time for space.
const MAX_QUEUED_COMMANDS: usize = 512;
const MAX_QUEUED_COMMAND_BYTES: u32 = 128 * 1024 * 1024;
const MAX_QUEUED_FILE_MESSAGES: usize = 128;
const MAX_QUEUED_HOST_MESSAGES: usize = 2048;
// A full 8192×8192 RGBA desktop is 256 MiB. Leave room for its frame
// metadata and some other traffic while still bounding a stalled stdout pipe.
const MAX_QUEUED_HOST_BYTES: usize = 320 * 1024 * 1024;
// Brief bursts should exert backpressure; a peer that stops making progress
// still has a finite budget and cannot keep the helper alive indefinitely.
const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(2);
const INPUT_RETRY_INTERVAL: Duration = Duration::from_millis(5);
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const STDOUT_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
const STDOUT_PROGRESS_POLL: Duration = Duration::from_millis(100);
const STDOUT_WRITE_CHUNK: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputEnd {
    Close,
    Overloaded,
    InvalidFrame,
}

pub(crate) struct QueuedCommand {
    pub(crate) command: SessionCmd,
    _bytes: OwnedSemaphorePermit,
    _offer: Option<PendingOffer>,
}

#[derive(Default)]
pub(crate) struct InputOrdering {
    pub(crate) pending_offers: usize,
    pub(crate) pending_suspensions: usize,
    pub(crate) wire_progress: Option<Instant>,
}

struct PendingOffer {
    ordering: Arc<Mutex<InputOrdering>>,
    suspension: bool,
}

impl Drop for PendingOffer {
    fn drop(&mut self) {
        let mut ordering = self
            .ordering
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ordering.pending_offers -= 1;
        ordering.pending_suspensions -= usize::from(self.suspension);
    }
}

pub(crate) struct InputSender {
    commands: tokio_mpsc::Sender<QueuedCommand>,
    bytes: Arc<Semaphore>,
    end: watch::Sender<Option<InputEnd>>,
    stdout_progress: Arc<StdoutProgress>,
    ordering: Arc<Mutex<InputOrdering>>,
    offer_notify: Arc<Notify>,
    replaceable: Mutex<Option<Arc<Mutex<Option<adit_rdp_proto::InputEvent>>>>>,
    #[cfg(test)]
    capacity_wait_observer: Option<std_mpsc::Sender<()>>,
}

pub(crate) struct SessionInput {
    pub(crate) commands: tokio_mpsc::Receiver<QueuedCommand>,
    pub(crate) end: watch::Receiver<Option<InputEnd>>,
    pub(crate) ordering: Arc<Mutex<InputOrdering>>,
    pub(crate) offer_notify: Arc<Notify>,
}

impl SessionInput {
    pub(crate) fn has_pending_offer(&self) -> bool {
        self.ordering
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_offers
            != 0
    }

    pub(crate) fn has_pending_suspension(&self) -> bool {
        self.ordering
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_suspensions
            != 0
    }
}

pub(crate) fn input_channel() -> (InputSender, SessionInput) {
    let (commands, receiver) = tokio_mpsc::channel(MAX_QUEUED_COMMANDS);
    let (end, end_receiver) = watch::channel(None);
    let ordering = Arc::new(Mutex::new(InputOrdering::default()));
    let offer_notify = Arc::new(Notify::new());
    (
        InputSender {
            commands,
            bytes: Arc::new(Semaphore::new(MAX_QUEUED_COMMAND_BYTES as usize)),
            end,
            stdout_progress: Arc::new(StdoutProgress::default()),
            ordering: Arc::clone(&ordering),
            offer_notify: Arc::clone(&offer_notify),
            replaceable: Mutex::new(None),
            #[cfg(test)]
            capacity_wait_observer: None,
        },
        SessionInput {
            commands: receiver,
            end: end_receiver,
            ordering,
            offer_notify,
        },
    )
}

impl InputSender {
    pub(crate) fn stop(&self, reason: InputEnd) {
        let changed = self.end.send_if_modified(|end| {
            if end.is_none() {
                *end = Some(reason);
                true
            } else {
                false
            }
        });
        if changed && reason == InputEnd::Overloaded {
            tracing::warn!("RDP helper input queue exceeded its limit");
        }
    }

    pub(crate) fn send(&self, command: SessionCmd) -> bool {
        if let SessionCmd::Input(
            event @ (adit_rdp_proto::InputEvent::MouseMove { .. }
            | adit_rdp_proto::InputEvent::Resize { .. }),
        ) = command
        {
            return self.send_replaceable(event);
        }
        // Edges partition the stream: a later move must never pass a button or key.
        *self
            .replaceable
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        match command {
            command @ SessionCmd::Input(adit_rdp_proto::InputEvent::ClipboardText(_))
                if command.queued_bytes() > MAX_QUEUED_COMMAND_BYTES as usize =>
            {
                // A clipboard item cannot fit, but losing this one item must
                // not end the desktop or leave the previous offer advertised.
                self.send_file_offer(0, true, None)
            }
            SessionCmd::ClipboardFiles { generation, files } => {
                let rejected = !files.is_empty();
                drop(files);
                self.send_file_offer(generation, rejected, None)
            }
            // These are responses for a direction that never issues reads.
            SessionCmd::FileContents { .. } => {
                self.end.borrow().is_none() && !self.commands.is_closed()
            }
            command @ (SessionCmd::SuspendLocalFileDirection
            | SessionCmd::RequestFileContents { .. }
            | SessionCmd::LocalFileOfferRejected) => {
                self.send_with_policy(command, BACKPRESSURE_TIMEOUT, false, None)
            }
            command => self.send_with_wait(command, BACKPRESSURE_TIMEOUT),
        }
    }

    fn send_replaceable(&self, event: adit_rdp_proto::InputEvent) -> bool {
        let mut latest = self
            .replaceable
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(slot) = latest.as_ref() {
            let mut pending = slot.lock().unwrap_or_else(|error| error.into_inner());
            if pending
                .as_ref()
                .is_some_and(|old| std::mem::discriminant(old) == std::mem::discriminant(&event))
            {
                *pending = Some(event);
                return self.end.borrow().is_none() && !self.commands.is_closed();
            }
        }
        let slot = Arc::new(Mutex::new(Some(event)));
        if self.send_with_wait(
            SessionCmd::ReplaceableInput(Arc::clone(&slot)),
            BACKPRESSURE_TIMEOUT,
        ) {
            *latest = Some(slot);
            true
        } else {
            *latest = None;
            false
        }
    }

    fn send_with_wait(&self, command: SessionCmd, wait: Duration) -> bool {
        self.send_with_policy(command, wait, true, None)
    }

    fn register_offer(&self, suspension: bool) -> PendingOffer {
        let mut ordering = self
            .ordering
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ordering.pending_offers += 1;
        ordering.pending_suspensions += usize::from(suspension);
        drop(ordering);
        self.offer_notify.notify_one();
        PendingOffer {
            ordering: Arc::clone(&self.ordering),
            suspension,
        }
    }

    fn send_file_offer(
        &self,
        generation: u64,
        rejected: bool,
        registered: Option<PendingOffer>,
    ) -> bool {
        let command = if rejected {
            SessionCmd::LocalFileOfferRejected
        } else {
            SessionCmd::ClipboardFiles {
                generation,
                files: Vec::new(),
            }
        };
        self.send_with_policy(command, BACKPRESSURE_TIMEOUT, false, registered)
    }

    fn send_with_policy(
        &self,
        command: SessionCmd,
        wait: Duration,
        disconnect_on_timeout: bool,
        registered: Option<PendingOffer>,
    ) -> bool {
        if self.end.borrow().is_some() {
            return false;
        }
        let is_suspension = matches!(&command, SessionCmd::SuspendLocalFileDirection);
        let is_offer = matches!(
            &command,
            SessionCmd::Input(adit_rdp_proto::InputEvent::ClipboardText(_))
                | SessionCmd::ClipboardImage(_)
                | SessionCmd::ClipboardTextForRevision { .. }
                | SessionCmd::ClipboardImageForRevision { .. }
                | SessionCmd::ClipboardPolicyChanged
                | SessionCmd::ClipboardFiles { .. }
                | SessionCmd::LocalFileOfferRejected
                | SessionCmd::SuspendLocalFileDirection
        );
        // The stdin reader already owns this message. Register it before either
        // byte or queue capacity can block; otherwise a pending revocation is
        // invisible to the wire-commit barrier while the queue is full.
        let offer = if is_offer {
            Some(registered.unwrap_or_else(|| self.register_offer(is_suspension)))
        } else {
            drop(registered);
            None
        };
        let mut deadline = Instant::now() + wait;
        let mut capacity = (self.commands.capacity(), self.bytes.available_permits());
        let bytes = command.queued_bytes();
        let Ok(bytes) = u32::try_from(bytes.max(1)) else {
            self.stop(InputEnd::Overloaded);
            return false;
        };
        if bytes > MAX_QUEUED_COMMAND_BYTES {
            // No amount of output progress can make an oversized command fit.
            self.stop(InputEnd::Overloaded);
            return false;
        }
        let permit = loop {
            match Arc::clone(&self.bytes).try_acquire_many_owned(bytes) {
                Ok(permit) => break permit,
                Err(_)
                    if self.wait_for_capacity(
                        &mut deadline,
                        wait,
                        &mut capacity,
                        disconnect_on_timeout,
                    ) => {}
                Err(_) => return false,
            }
        };
        let mut queued = QueuedCommand {
            command,
            _bytes: permit,
            _offer: offer,
        };
        loop {
            let sent = {
                let _ordering = self
                    .ordering
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                self.commands.try_send(queued)
            };
            match sent {
                Ok(()) => return true,
                Err(tokio_mpsc::error::TrySendError::Full(returned)) => {
                    queued = returned;
                    if !self.wait_for_capacity(
                        &mut deadline,
                        wait,
                        &mut capacity,
                        disconnect_on_timeout,
                    ) {
                        return false;
                    }
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
    }

    fn wait_for_capacity(
        &self,
        deadline: &mut Instant,
        wait: Duration,
        previous_capacity: &mut (usize, usize),
        disconnect_on_timeout: bool,
    ) -> bool {
        if self.end.borrow().is_some() || self.commands.is_closed() {
            return false;
        }
        #[cfg(test)]
        if let Some(observer) = &self.capacity_wait_observer {
            let _ = observer.send(());
        }
        let now = Instant::now();
        let capacity = (self.commands.capacity(), self.bytes.available_permits());
        if capacity.0 > previous_capacity.0 || capacity.1 > previous_capacity.1 {
            *deadline = now + wait;
        }
        *previous_capacity = capacity;
        let stdout_in_progress = self
            .stdout_progress
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some_and(|last| now.saturating_duration_since(last) < STDOUT_PROGRESS_TIMEOUT);
        let wire_in_progress = self
            .ordering
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .wire_progress
            .is_some_and(|last| now.saturating_duration_since(last) < STDOUT_PROGRESS_TIMEOUT);
        if stdout_in_progress || wire_in_progress {
            // The session may be waiting behind a large frame. Backpressure the
            // pipe while stdout advances; its own watchdog detects a real stall.
            *deadline = now + wait;
        }
        let remaining = match deadline.checked_duration_since(now) {
            Some(remaining) if !remaining.is_zero() => remaining,
            _ if !disconnect_on_timeout => {
                *deadline = now + wait.max(INPUT_RETRY_INTERVAL);
                INPUT_RETRY_INTERVAL
            }
            _ => {
                self.stop(InputEnd::Overloaded);
                return false;
            }
        };
        thread::sleep(remaining.min(INPUT_RETRY_INTERVAL));
        true
    }
}

// These are the versioned bincode ClientMsg discriminants. File payloads are
// never deserialized: the offer header is enough to distinguish a revocation
// from a rejected copy, and both kinds of content reply can be discarded.
const CLIENT_CLIPBOARD_FILES: u32 = 3;
const CLIENT_INPUT: u32 = 1;
const INPUT_CLIPBOARD_TEXT: u32 = 6;
const CLIENT_FILE_CONTENTS: u32 = 4;
const CLIENT_BOUND_FILE_CONTENTS: u32 = 7;
const CLIENT_LOCAL_FILE_OFFER_REJECTED: u32 = 9;

pub(crate) enum ClientFrame {
    Message(ClientMsg),
    FileHandled,
    SessionEnded,
}

fn discard_frame_bytes(reader: &mut impl Read, mut remaining: usize) -> io::Result<()> {
    let mut scratch = [0u8; 8192];
    while remaining != 0 {
        let count = remaining.min(scratch.len());
        reader.read_exact(&mut scratch[..count])?;
        remaining -= count;
    }
    Ok(())
}

pub(crate) fn read_client_frame(
    reader: &mut impl Read,
    input: &InputSender,
) -> io::Result<Option<ClientFrame>> {
    let mut length_bytes = [0u8; 4];
    match reader.read_exact(&mut length_bytes) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short IPC frame",
        ));
    }
    let mut tag_bytes = [0u8; 4];
    reader.read_exact(&mut tag_bytes)?;
    let tag = u32::from_le_bytes(tag_bytes);
    match tag {
        CLIENT_INPUT
            if length >= 8
                && length
                    .saturating_sub(16)
                    .saturating_add(std::mem::size_of::<SessionCmd>())
                    > MAX_QUEUED_COMMAND_BYTES as usize =>
        {
            let mut event_tag = [0u8; 4];
            reader.read_exact(&mut event_tag)?;
            if u32::from_le_bytes(event_tag) == INPUT_CLIPBOARD_TEXT {
                let registered = input.register_offer(false);
                if !input.send_file_offer(0, true, Some(registered)) {
                    return Ok(Some(ClientFrame::SessionEnded));
                }
                discard_frame_bytes(reader, length - 8)?;
                return Ok(Some(ClientFrame::FileHandled));
            }
            let mut header = [0u8; 12];
            header[..4].copy_from_slice(&length_bytes);
            header[4..8].copy_from_slice(&tag_bytes);
            header[8..].copy_from_slice(&event_tag);
            let mut reconstructed = io::Cursor::new(header).chain(reader);
            let message = read_msg::<_, ClientMsg>(&mut reconstructed)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing IPC frame"))?;
            Ok(Some(ClientFrame::Message(message)))
        }
        CLIENT_CLIPBOARD_FILES => {
            // Install the ordering barrier as soon as the message type is
            // known, including while its header or large body is still arriving.
            let registered = input.register_offer(false);
            let remaining = length - 4;
            let (generation, rejected, body) = if remaining >= 16 {
                let mut header = [0u8; 16];
                reader.read_exact(&mut header)?;
                let generation = u64::from_le_bytes(header[..8].try_into().unwrap());
                let count = u64::from_le_bytes(header[8..].try_into().unwrap());
                (generation, count != 0 || remaining != 16, remaining - 16)
            } else {
                (0, true, remaining)
            };
            if !input.send_file_offer(generation, rejected, Some(registered)) {
                return Ok(Some(ClientFrame::SessionEnded));
            }
            // Drain after enqueueing: the session can invalidate old content
            // while the rest of a large, untrusted file offer arrives.
            discard_frame_bytes(reader, body)?;
            Ok(Some(ClientFrame::FileHandled))
        }
        CLIENT_FILE_CONTENTS | CLIENT_BOUND_FILE_CONTENTS => {
            discard_frame_bytes(reader, length - 4)?;
            Ok(Some(ClientFrame::FileHandled))
        }
        CLIENT_LOCAL_FILE_OFFER_REJECTED => {
            let registered = input.register_offer(false);
            if !input.send_file_offer(0, true, Some(registered)) {
                return Ok(Some(ClientFrame::SessionEnded));
            }
            discard_frame_bytes(reader, length - 4)?;
            Ok(Some(ClientFrame::FileHandled))
        }
        _ => {
            // Reuse the protocol crate for every other variant, including its
            // framing limits and ordinary bincode validation.
            let mut header = [0u8; 8];
            header[..4].copy_from_slice(&length_bytes);
            header[4..].copy_from_slice(&tag_bytes);
            let mut reconstructed = io::Cursor::new(header).chain(reader);
            let message = read_msg::<_, ClientMsg>(&mut reconstructed)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing IPC frame"))?;
            Ok(Some(ClientFrame::Message(message)))
        }
    }
}

// Messages keep their slots until stdout has finished the actual frame, not
// merely until it has taken the message out of its queue. File requests and
// replies also have a separate limit. The final Error/Closed bypass the budget
// so a saturated queue can still explain why the session ended.
#[derive(Clone)]
pub(crate) struct HostSender {
    messages: std_mpsc::Sender<QueuedHostMessage>,
    budget: Arc<Mutex<HostBudget>>,
    available: Arc<Notify>,
    progress: Arc<StdoutProgress>,
    clipboard_policy_epoch: Arc<ClipboardPolicySource>,
    #[cfg(test)]
    policy_ack_pending: Arc<AtomicBool>,
}

#[derive(Default)]
pub(crate) struct ClipboardPolicySource {
    local_epoch: AtomicU64,
    shared: OnceLock<Arc<SharedClipboardPolicy>>,
}

impl std::fmt::Debug for ClipboardPolicySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardPolicySource")
            .field("shared", &self.is_shared())
            .finish_non_exhaustive()
    }
}

impl ClipboardPolicySource {
    pub(crate) fn epoch(&self) -> u64 {
        self.snapshot().0
    }
    pub(crate) fn snapshot(&self) -> (u64, bool) {
        self.shared.get().map_or_else(
            || (self.local_epoch.load(Ordering::Acquire), true),
            |mapping| mapping.load(),
        )
    }
    pub(crate) fn snapshot_full(&self) -> (u64, bool, u64) {
        self.shared.get().map_or_else(
            || (self.local_epoch.load(Ordering::Acquire), true, 0),
            |mapping| mapping.load_full(),
        )
    }
    fn bind(&self, id: &str) -> io::Result<()> {
        let mapping = Arc::new(SharedClipboardPolicy::open_in_child(id)?);
        self.shared
            .set(mapping)
            .map_err(|_| io::Error::other("clipboard policy already bound"))
    }
    pub(crate) fn is_shared(&self) -> bool {
        self.shared.get().is_some()
    }
    pub(crate) fn set_local_epoch(&self, epoch: u64) {
        self.local_epoch.fetch_max(epoch, Ordering::AcqRel);
    }
    #[cfg(test)]
    pub(crate) fn bind_test_mapping(&self, mapping: Arc<SharedClipboardPolicy>) {
        assert!(self.shared.set(mapping).is_ok());
    }
}

#[derive(Default)]
struct HostBudget {
    messages: usize,
    bytes: usize,
    file_messages: usize,
}

struct HostMessagePermit {
    budget: Arc<Mutex<HostBudget>>,
    available: Arc<Notify>,
    bytes: usize,
    is_file: bool,
}

impl Drop for HostMessagePermit {
    fn drop(&mut self) {
        let mut budget = self
            .budget
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        budget.messages -= 1;
        budget.bytes -= self.bytes;
        budget.file_messages -= usize::from(self.is_file);
        drop(budget);
        self.available.notify_waiters();
    }
}

pub(crate) struct QueuedHostMessage {
    pub(crate) message: HostMsg,
    _permit: Option<HostMessagePermit>,
    clipboard_policy_epoch: u64,
    policy_ack: Option<(Arc<ClipboardPolicySource>, Arc<AtomicBool>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HostSendError {
    #[error("RDP 控制通道已关闭")]
    Closed,
    #[error("RDP 辅助进程的输出队列已满，会话已关闭")]
    Overloaded,
}

pub(crate) fn host_channel() -> (HostSender, std_mpsc::Receiver<QueuedHostMessage>) {
    let (messages, receiver) = std_mpsc::channel();
    (
        HostSender {
            messages,
            budget: Arc::new(Mutex::new(HostBudget::default())),
            available: Arc::new(Notify::new()),
            progress: Arc::new(StdoutProgress::default()),
            clipboard_policy_epoch: Arc::new(ClipboardPolicySource::default()),
            #[cfg(test)]
            policy_ack_pending: Arc::new(AtomicBool::new(false)),
        },
        receiver,
    )
}

fn observe_clipboard_policy(policy: &ClipboardPolicySource, epoch: u64) {
    policy.set_local_epoch(epoch);
}

impl HostSender {
    pub(crate) fn clipboard_policy_epoch(&self) -> Arc<ClipboardPolicySource> {
        Arc::clone(&self.clipboard_policy_epoch)
    }

    #[cfg(test)]
    fn queue_policy_ack(&self) -> Result<(), HostSendError> {
        if self.policy_ack_pending.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let queued = QueuedHostMessage {
            message: HostMsg::ClipboardPolicyApplied {
                policy_epoch: 0,
                enabled: false,
            },
            _permit: None,
            clipboard_policy_epoch: 0,
            policy_ack: Some((
                Arc::clone(&self.clipboard_policy_epoch),
                Arc::clone(&self.policy_ack_pending),
            )),
        };
        if self.messages.send(queued).is_err() {
            self.policy_ack_pending.store(false, Ordering::Release);
            Err(HostSendError::Closed)
        } else {
            Ok(())
        }
    }

    pub(crate) fn send(&self, message: HostMsg) -> Result<(), HostSendError> {
        let policy = self.clipboard_policy_epoch.epoch();
        let permit = self.reserve(&message).inspect_err(|_| {
            tracing::warn!("RDP helper output queue exceeded its limit");
        })?;
        self.enqueue(message, permit, policy)
    }

    pub(crate) async fn send_when_ready(&self, message: HostMsg) -> Result<(), HostSendError> {
        let policy = self.clipboard_policy_epoch.epoch();
        let mut inactive_since = Instant::now();
        loop {
            // Register before checking the budget, or a writer completing between
            // the check and the wait could leave space available without a wakeup.
            let notified = self.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.reserve(&message) {
                Ok(permit) => return self.enqueue(message, permit, policy),
                Err(HostSendError::Overloaded) => {
                    let now = Instant::now();
                    let last = *self
                        .progress
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let expired = match last {
                        Some(last) => {
                            inactive_since = now;
                            now.saturating_duration_since(last) >= STDOUT_PROGRESS_TIMEOUT
                        }
                        None => {
                            now.saturating_duration_since(inactive_since) >= BACKPRESSURE_TIMEOUT
                        }
                    };
                    if expired {
                        tracing::warn!("RDP helper output queue exceeded its limit");
                        return Err(HostSendError::Overloaded);
                    }
                    // A single large in-flight frame keeps its budget until it
                    // finishes. Do not reject following messages while stdout
                    // is demonstrably advancing through that frame.
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep(STDOUT_PROGRESS_POLL) => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn reserve(&self, message: &HostMsg) -> Result<Option<HostMessagePermit>, HostSendError> {
        let is_file = matches!(
            message,
            HostMsg::FileContentsNeeded { .. } | HostMsg::FileContents { .. }
        );
        let permit = if matches!(
            message,
            HostMsg::Error(_)
                | HostMsg::Closed
                | HostMsg::ClipboardFileDirectionUnavailable { .. }
                | HostMsg::LocalFileDirectionSuspended { .. }
                | HostMsg::ClipboardChannelChanged { .. }
                | HostMsg::ClipboardLocalFileDirectionReady { .. }
                | HostMsg::ClipboardChannelReady { .. }
                | HostMsg::ClipboardPolicyRejected
        ) {
            None
        } else {
            let bytes = host_message_bytes(message);
            let mut budget = self
                .budget
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if budget.messages >= MAX_QUEUED_HOST_MESSAGES
                || budget.bytes.saturating_add(bytes) > MAX_QUEUED_HOST_BYTES
                || (is_file && budget.file_messages >= MAX_QUEUED_FILE_MESSAGES)
            {
                return Err(HostSendError::Overloaded);
            }
            budget.messages += 1;
            budget.bytes += bytes;
            budget.file_messages += usize::from(is_file);
            Some(HostMessagePermit {
                budget: Arc::clone(&self.budget),
                available: Arc::clone(&self.available),
                bytes,
                is_file,
            })
        };
        Ok(permit)
    }

    fn enqueue(
        &self,
        message: HostMsg,
        permit: Option<HostMessagePermit>,
        clipboard_policy_epoch: u64,
    ) -> Result<(), HostSendError> {
        self.messages
            .send(QueuedHostMessage {
                message,
                _permit: permit,
                clipboard_policy_epoch,
                policy_ack: None,
            })
            .map_err(|_| HostSendError::Closed)
    }
}

fn host_message_bytes(message: &HostMsg) -> usize {
    use adit_rdp_proto::PointerUpdate;
    let payload =
        match message {
            HostMsg::Tile { rgba, .. } | HostMsg::Pointer(PointerUpdate::Bitmap { rgba, .. }) => {
                rgba.capacity()
            }
            HostMsg::ClipboardText(text)
            | HostMsg::ClipboardTextForRevision { text, .. }
            | HostMsg::ClipboardNotice(text) => text.capacity(),
            HostMsg::ClipboardImage(image) | HostMsg::ClipboardImageForRevision { image, .. } => {
                image.capacity()
            }
            HostMsg::ClipboardFiles { files, .. }
            | HostMsg::ClipboardFilesForPolicy { files, .. } => files.iter().fold(
                files
                    .capacity()
                    .saturating_mul(std::mem::size_of::<adit_rdp_proto::ClipFile>()),
                |sum, file| sum.saturating_add(file.name.capacity()),
            ),
            HostMsg::FileContents { data, .. } => data.as_ref().map_or(0, Vec::capacity),
            HostMsg::DisplayControl(state) => state
                .sent_layout
                .capacity()
                .saturating_mul(std::mem::size_of::<adit_rdp_proto::Monitor>())
                .saturating_add(state.error.as_ref().map_or(0, String::capacity)),
            HostMsg::Error(message)
            | HostMsg::ClipboardFileDirectionUnavailable { message, .. } => message.capacity(),
            HostMsg::Connected { .. }
            | HostMsg::Resized { .. }
            | HostMsg::FileContentsNeeded { .. }
            | HostMsg::LocalFileDirectionSuspended { .. }
            | HostMsg::ClipboardChannelChanged { .. }
            | HostMsg::ClipboardLocalFileDirectionReady { .. }
            | HostMsg::ClipboardPolicyApplied { .. }
            | HostMsg::ClipboardChannelReady { .. }
            | HostMsg::ClipboardPolicyRejected
            | HostMsg::ClipboardRemoteCopy(_)
            | HostMsg::Closed
            | HostMsg::Pointer(_) => 0,
        };
    std::mem::size_of::<HostMsg>().saturating_add(payload)
}

struct StdoutProgress(Mutex<Option<Instant>>, watch::Sender<bool>);

impl Default for StdoutProgress {
    fn default() -> Self {
        let (failed, _) = watch::channel(false);
        Self(Mutex::new(None), failed)
    }
}

impl StdoutProgress {
    fn begin_write(&self) {
        let mut last = self.0.lock().unwrap_or_else(|error| error.into_inner());
        last.get_or_insert_with(Instant::now);
    }

    fn made_progress(&self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(Instant::now());
    }

    fn finish_frame(&self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }

    fn fail(&self) {
        self.1.send_replace(true);
    }

    fn has_failed(&self) -> bool {
        *self.1.borrow()
    }

    fn stalled_at(&self, now: Instant, timeout: Duration) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some_and(|last| now.saturating_duration_since(last) >= timeout)
    }
}

struct ProgressWriter<'a, W> {
    writer: &'a mut W,
    progress: Arc<StdoutProgress>,
}

impl<W: Write> Write for ProgressWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        // Windows can block one synchronous pipe write until its entire buffer
        // is read. Small writes expose progress even for a slow reader.
        self.progress.begin_write();
        let written = self
            .writer
            .write(&bytes[..bytes.len().min(STDOUT_WRITE_CHUNK)])?;
        if written > 0 {
            self.progress.made_progress();
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn write_host_messages_with_progress<W: Write>(
    writer: &mut W,
    receiver: std_mpsc::Receiver<QueuedHostMessage>,
    progress: Arc<StdoutProgress>,
) {
    let mut writer = ProgressWriter {
        writer,
        progress: Arc::clone(&progress),
    };
    while let Ok(queued) = receiver.recv() {
        let is_final = matches!(queued.message, HostMsg::Closed);
        let message = if let Some((policy, pending)) = &queued.policy_ack {
            pending.store(false, Ordering::Release);
            let (policy_epoch, enabled) = policy.snapshot();
            HostMsg::ClipboardPolicyApplied {
                policy_epoch,
                enabled,
            }
        } else {
            match queued.message {
                HostMsg::ClipboardFiles { generation, files } => HostMsg::ClipboardFilesForPolicy {
                    policy_epoch: queued.clipboard_policy_epoch,
                    generation,
                    files,
                },
                message => message,
            }
        };
        let result = write_msg(&mut writer, &message).and_then(|_| writer.flush());
        progress.finish_frame();
        if result.is_err() {
            progress.fail();
            break;
        }
        if is_final {
            break;
        }
    }
}

#[cfg(test)]
fn write_host_messages<W: Write>(writer: &mut W, receiver: std_mpsc::Receiver<QueuedHostMessage>) {
    write_host_messages_with_progress(writer, receiver, Arc::new(StdoutProgress::default()));
}

async fn run_with_stdout_watchdog<F>(
    session: F,
    progress: Arc<StdoutProgress>,
) -> Result<(), RdpError>
where
    F: std::future::Future<Output = Result<(), RdpError>>,
{
    tokio::pin!(session);
    let mut writer_failed = progress.1.subscribe();
    let mut tick = tokio::time::interval(STDOUT_PROGRESS_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *writer_failed.borrow_and_update() {
            return Err(RdpError::ControlChannelClosed);
        }
        tokio::select! {
            biased;
            _ = writer_failed.changed() => {},
            result = &mut session => return result,
            _ = tick.tick() => {
                if progress.stalled_at(Instant::now(), STDOUT_PROGRESS_TIMEOUT) {
                    tracing::warn!("RDP helper stdout made no write progress before its deadline");
                    return Err(RdpError::Session("RDP 辅助进程输出停滞，会话已关闭".into()));
                }
            }
        }
    }
}

fn finish_writer(
    writer: thread::JoinHandle<()>,
    finished: std_mpsc::Receiver<()>,
    progress: &StdoutProgress,
) -> bool {
    let mut last_progress = Instant::now();
    let completed = loop {
        match finished.recv_timeout(STDOUT_PROGRESS_POLL) {
            Ok(()) => break true,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                progress.fail();
                break true;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                if let Some(last) = *progress.0.lock().unwrap_or_else(|error| error.into_inner()) {
                    last_progress = last;
                }
                if Instant::now().saturating_duration_since(last_progress)
                    >= STDOUT_PROGRESS_TIMEOUT
                {
                    // A slow reader that keeps advancing may finish the current
                    // frame; a stopped reader cannot keep shutdown waiting.
                    tracing::warn!("RDP helper stdout did not finish before its progress deadline");
                    break false;
                }
            }
        }
    };
    // The signal is sent after the stdout lock is released; even a thread
    // suspended immediately afterward must not make shutdown wait in join().
    drop(writer);
    completed
}

fn final_session_result(
    result: Result<(), RdpError>,
    drained: bool,
    progress: &StdoutProgress,
) -> Result<(), RdpError> {
    if result.is_ok() && progress.has_failed() {
        return Err(RdpError::ControlChannelClosed);
    }
    if !drained && result.is_ok() {
        return Err(RdpError::Session("RDP 辅助进程输出停滞，会话已关闭".into()));
    }
    result
}

/// Entry point for the `adit-rdp-host` binary. Blocks until the session ends.
pub fn run_host() -> Result<(), RdpError> {
    // Diagnostics go to a log file next to the app config (the GUI app discards
    // the helper's stderr), so an RDP session's lifecycle/errors are visible even
    // in a release build. Appended across launches (see below); `try_init` so a
    // double-init cannot panic.
    let log_path = {
        let base = std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("Adit").join("rdp-helper.log")
    };
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append, and roll only once the file has actually grown large.
    //
    // This used to truncate on every launch, which is wrong the moment there is
    // more than one RDP session: each opens its own helper, every helper writes
    // to this one path, and the second to start wiped the first one's log. That
    // cost a real diagnosis — a clipboard bug was reproduced and the evidence
    // was already gone, because opening a second session had reset the file.
    //
    // The pid goes on the banner so interleaved lines can be told apart, now
    // that one file holds every live helper.
    const LOG_ROLL_BYTES: u64 = 4 * 1024 * 1024;
    if std::fs::metadata(&log_path).is_ok_and(|meta| meta.len() > LOG_ROLL_BYTES) {
        let _ = std::fs::remove_file(&log_path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "=== adit-rdp-host {} start pid={} ===",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
        );
    }
    // Default: session lifecycle + warnings/errors only (no per-frame spam).
    // Override with RUST_LOG for deeper tracing.
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "warn,adit_rdp=info,ironrdp_async=info".to_owned());
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || -> Box<dyn std::io::Write + Send> {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(std::io::sink()),
            }
        })
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .try_init();

    let (req_tx, req_rx) = std_mpsc::channel::<ConnectRequest>();
    let (host_tx, host_rx) = host_channel();
    let (mut input_tx, input_rx) = input_channel();
    let (hello_tx, hello_rx) = std_mpsc::channel::<HelloResponse>();
    let stdout_progress = Arc::clone(&host_tx.progress);
    input_tx.stdout_progress = Arc::clone(&stdout_progress);
    let writer_progress = Arc::clone(&stdout_progress);
    let input_policy_epoch = Arc::clone(&host_tx.clipboard_policy_epoch);
    let input_host = host_tx.clone();

    // ── stdin reader ─────────────────────────────────────────────────────────
    // A single lock for the process lifetime: `StdinLock` is internally buffered,
    // so re-locking would drop bytes read past a message boundary.
    thread::Builder::new()
        .name("adit-rdp-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            let response = match read_client_hello(&mut lock) {
                Ok(version) if version == IPC_PROTOCOL_VERSION => HelloResponse::Accepted,
                Ok(_) => HelloResponse::VersionMismatch,
                Err(_) => HelloResponse::Unversioned,
            };
            let accepted = matches!(response, HelloResponse::Accepted);
            if hello_tx.send(response).is_err() || !accepted {
                return;
            }
            let mut req_tx = Some(req_tx);
            loop {
                match read_client_frame(&mut lock, &input_tx) {
                    Ok(Some(ClientFrame::FileHandled)) => {}
                    Ok(Some(ClientFrame::SessionEnded)) => break,
                    Ok(Some(ClientFrame::Message(ClientMsg::Connect(mut request)))) => {
                        // Only the first Connect matters; ignore any extras.
                        if let Some(tx) = req_tx.take() {
                            let bound = request
                                .clipboard_policy_id
                                .as_deref()
                                .is_some_and(|id| input_policy_epoch.bind(id).is_ok());
                            if bound {
                                if !input_tx.send(SessionCmd::ClipboardPolicyChanged) {
                                    break;
                                }
                            } else if request.enable_clipboard {
                                // An unavailable mapping cannot weaken the generation
                                // boundary; the desktop itself can still connect.
                                request.enable_clipboard = false;
                                let _ = input_host.send(HostMsg::ClipboardPolicyRejected);
                            }
                            let _ = tx.send(request);
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::Input(event)))) => {
                        if !input_tx.send(SessionCmd::Input(event)) {
                            break; // session ended
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::ClipboardImage(image)))) => {
                        if !input_tx.send(SessionCmd::ClipboardImage(image)) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::ClipboardTextForRevision {
                        stamp,
                        text,
                    }))) => {
                        if !input_tx.send(SessionCmd::ClipboardTextForRevision { stamp, text }) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::ClipboardImageForRevision {
                        stamp,
                        image,
                    }))) => {
                        if !input_tx.send(SessionCmd::ClipboardImageForRevision { stamp, image }) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::ClipboardFiles {
                        generation,
                        files,
                    }))) => {
                        if !input_tx.send(SessionCmd::ClipboardFiles { generation, files }) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::BoundFileContents {
                        request,
                        data,
                    }))) => {
                        if !input_tx.send(SessionCmd::FileContents { request, data }) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::SuspendLocalFileDirection))) => {
                        if !input_tx.send(SessionCmd::SuspendLocalFileDirection) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::LocalFileOfferRejected))) => {
                        if !input_tx.send(SessionCmd::LocalFileOfferRejected) {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::ClipboardPolicyEpoch(epoch)))) => {
                        observe_clipboard_policy(&input_policy_epoch, epoch);
                        if input_policy_epoch.is_shared()
                            && !input_tx.send(SessionCmd::ClipboardPolicyChanged)
                        {
                            break;
                        }
                    }
                    Ok(Some(ClientFrame::Message(ClientMsg::FileContents { .. }))) => {}
                    Ok(Some(ClientFrame::Message(ClientMsg::RequestFileContents {
                        generation,
                        stream_id,
                        index,
                        offset,
                        length,
                    }))) => {
                        if !input_tx.send(SessionCmd::RequestFileContents {
                            generation,
                            stream_id,
                            index,
                            offset,
                            length,
                        }) {
                            break;
                        }
                    }
                    // Close or a clean EOF ⇒ drop `input_tx`, which the session
                    // observes as a graceful-shutdown request.
                    Ok(Some(ClientFrame::Message(ClientMsg::Close))) | Ok(None) => {
                        input_tx.stop(InputEnd::Close);
                        break;
                    }
                    Err(_) => {
                        input_tx.stop(InputEnd::InvalidFrame);
                        break;
                    }
                }
            }
        })
        .map_err(|e| RdpError::Runtime(e.to_string()))?;

    // ── stdout writer ────────────────────────────────────────────────────────
    let (writer_finished_tx, writer_finished_rx) = std_mpsc::channel();
    let writer = thread::Builder::new()
        .name("adit-rdp-stdout".into())
        .spawn(move || {
            (|| {
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                match hello_rx.recv() {
                Ok(HelloResponse::Accepted) => {
                    if write_helper_hello(&mut lock, IPC_PROTOCOL_VERSION).is_err() {
                        writer_progress.fail();
                        return;
                    }
                }
                Ok(HelloResponse::VersionMismatch) => {
                    // Return our actual version in the stable envelope, so the
                    // app can report both versions without decoding HostMsg.
                    let _ = write_helper_hello(&mut lock, IPC_PROTOCOL_VERSION);
                    return;
                }
                Ok(HelloResponse::Unversioned) => {
                    // The most recent unversioned app recognizes these indices.
                    // Much older apps used different indices and may only close.
                    let _ = write_msg(&mut lock, &HostMsg::Error(
                        "RDP IPC 协商失败：主程序版本不兼容或未发送版本握手；请同时更新主程序和 RDP 辅助进程".to_owned(),
                    ));
                    let _ = write_msg(&mut lock, &HostMsg::Closed);
                    return;
                }
                Err(_) => return,
                }
                write_host_messages_with_progress(&mut lock, host_rx, writer_progress);
            })();
            let _ = writer_finished_tx.send(());
        })
        .map_err(|e| RdpError::Runtime(e.to_string()))?;

    // Wait for the app's Connect before spinning up the RDP runtime.
    let request = loop {
        match req_rx.recv_timeout(STDOUT_PROGRESS_POLL) {
            Ok(request) => break request,
            Err(std_mpsc::RecvTimeoutError::Timeout) if !stdout_progress.has_failed() => {}
            Err(_) => {
                // Flush the rejection before exiting so compatible unversioned apps
                // can display it instead of seeing only EOF.
                drop(host_tx);
                finish_writer(writer, writer_finished_rx, &stdout_progress);
                return Err(RdpError::ControlChannelClosed);
            }
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| RdpError::Runtime(e.to_string()))?;

    let session_host = host_tx.clone();
    let session = async move {
        #[cfg(debug_assertions)]
        if request.host == "adit-stdout-fixture.invalid"
            && request.port == 0
            && std::env::var_os("ADIT_RDP_TEST_STDOUT_FIXTURE").as_deref()
                == Some(std::ffi::OsStr::new("enabled"))
        {
            let end = input_rx.end.clone();
            return crate::session::guard_active_session(
                synthetic_stdout_session(input_rx, session_host),
                end,
            )
            .await
            .map(|_| ());
        }
        Box::pin(run_session(request, input_rx, session_host)).await
    };
    let result = runtime.block_on(run_with_stdout_watchdog(
        session,
        Arc::clone(&stdout_progress),
    ));
    // Runtime drop otherwise waits indefinitely for unrelated blocking tasks,
    // even after the stdout watchdog has ended the session.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);

    // Announce the outcome, then let the writer flush and exit.
    match &result {
        Ok(()) => {
            let _ = host_tx.send(HostMsg::Closed);
        }
        Err(e) => {
            let _ = host_tx.send(HostMsg::Error(e.to_string()));
            let _ = host_tx.send(HostMsg::Closed);
        }
    }
    drop(host_tx);
    let drained = finish_writer(writer, writer_finished_rx, &stdout_progress);

    final_session_result(result, drained, &stdout_progress)
}

// This debug-only fixture drives the real helper pipes and watchdog without a
// remote RDP server or credentials. It is absent from release builds.
#[cfg(debug_assertions)]
async fn synthetic_stdout_session(
    mut input: SessionInput,
    host: HostSender,
) -> Result<(), RdpError> {
    if host.clipboard_policy_epoch().is_shared() {
        match input.commands.recv().await.map(|queued| queued.command) {
            Some(SessionCmd::ClipboardPolicyChanged) => {
                let (policy_epoch, enabled) = host.clipboard_policy_epoch().snapshot();
                host.send(HostMsg::ClipboardPolicyApplied {
                    policy_epoch,
                    enabled,
                })
                .map_err(|error| RdpError::Session(error.to_string()))?;
            }
            _ => return Err(RdpError::Session("missing fixture policy".into())),
        }
    }
    if std::env::var_os("ADIT_RDP_TEST_BLOCKING_TASK").as_deref()
        == Some(std::ffi::OsStr::new("enabled"))
    {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            thread::sleep(Duration::from_secs(30));
        });
        started_rx
            .await
            .map_err(|error| RdpError::Runtime(error.to_string()))?;
    }
    host.send_when_ready(HostMsg::Tile {
        x: 0,
        y: 0,
        width: 512,
        height: 512,
        rgba: vec![0x5a; 512 * 512 * 4],
    })
    .await
    .map_err(|error| RdpError::Session(error.to_string()))?;
    let mut policy_signal =
        std::env::var_os("ADIT_RDP_TEST_POLICY_SIGNAL").map(std::path::PathBuf::from);
    if policy_signal.is_some() {
        host.send(HostMsg::ClipboardChannelReady { channel_epoch: 1 })
            .map_err(|error| RdpError::Session(error.to_string()))?;
    }
    let mut policy_tick = tokio::time::interval(Duration::from_millis(20));
    policy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut key_edges = Vec::new();
    loop {
        tokio::select! {
            _ = policy_tick.tick(), if policy_signal.is_some() => {
                if policy_signal.as_ref().is_some_and(|path| path.is_file()) {
                    policy_signal = None;
                    host.send_when_ready(HostMsg::ClipboardFiles {
                        generation: 8,
                        files: vec![adit_rdp_proto::ClipFile { name: "synthetic-new.bin".into(), size: 1, is_dir: false }],
                    }).await.map_err(|error| RdpError::Session(error.to_string()))?;
                }
            },
            _ = input.end.changed() => return Ok(()),
            command = input.commands.recv() => match command.map(|queued| queued.command) {
                Some(SessionCmd::ClipboardPolicyChanged) => {
                    let (policy_epoch, enabled) = host.clipboard_policy_epoch().snapshot();
                    host.send_when_ready(HostMsg::ClipboardPolicyApplied { policy_epoch, enabled })
                        .await.map_err(|error| RdpError::Session(error.to_string()))?;
                }
                Some(SessionCmd::Input(adit_rdp_proto::InputEvent::Key { pressed, .. })) => {
                    if key_edges.len() < 16 { key_edges.push(u8::from(pressed)); }
                }
                Some(SessionCmd::RequestFileContents { generation, stream_id, .. }) => {
                    host.send_when_ready(HostMsg::FileContents { generation, stream_id, data: Some(key_edges.clone()) })
                        .await.map_err(|error| RdpError::Session(error.to_string()))?;
                }
                None => return Ok(()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_client(message: &ClientMsg) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, message).unwrap();
        bytes
    }

    #[test]
    fn file_ipc_is_classified_before_payloads_reach_the_input_queue() {
        use adit_rdp_proto::{ClipFile, ClipboardFileRequestId, InputEvent};
        let request = ClipboardFileRequestId {
            channel_epoch: 1,
            snapshot_revision: 2,
            generation: 3,
            stream_id: 4,
        };
        let offer = ClientMsg::ClipboardFiles {
            generation: 7,
            files: vec![ClipFile {
                name: "synthetic-private-123.txt".into(),
                size: 0x1234_5678_9abc_def0,
                is_dir: false,
            }],
        };
        let legacy = ClientMsg::FileContents {
            generation: 7,
            stream_id: 4,
            data: Some(b"synthetic-private-bytes".to_vec()),
        };
        let bound = ClientMsg::BoundFileContents {
            request,
            data: Some(b"synthetic-private-bytes".to_vec()),
        };
        let empty = ClientMsg::ClipboardFiles {
            generation: 0,
            files: Vec::new(),
        };
        let marker = ClientMsg::LocalFileOfferRejected;
        let key = ClientMsg::Input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
        for (message, tag) in [
            (&offer, CLIENT_CLIPBOARD_FILES),
            (&legacy, CLIENT_FILE_CONTENTS),
            (&bound, CLIENT_BOUND_FILE_CONTENTS),
            (&marker, CLIENT_LOCAL_FILE_OFFER_REJECTED),
        ] {
            let encoded = encoded_client(message);
            assert_eq!(&encoded[4..8], &tag.to_le_bytes());
        }
        let empty_bytes = encoded_client(&empty);
        assert_eq!(empty_bytes.len(), 24);
        assert_eq!(&empty_bytes[16..24], &0_u64.to_le_bytes());
        let bytes: Vec<u8> = [&offer, &legacy, &bound, &empty, &marker, &key]
            .into_iter()
            .flat_map(encoded_client)
            .collect();
        let (sender, mut input) = input_channel();
        let mut reader = io::Cursor::new(bytes);
        for _ in 0..5 {
            assert!(matches!(
                read_client_frame(&mut reader, &sender).unwrap(),
                Some(ClientFrame::FileHandled)
            ));
        }
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::LocalFileOfferRejected
        ));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::ClipboardFiles { generation: 0, files } if files.is_empty()
        ));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::LocalFileOfferRejected
        ));
        assert!(input.commands.try_recv().is_err());
        assert!(matches!(
            read_client_frame(&mut reader, &sender).unwrap(),
            Some(ClientFrame::Message(ClientMsg::Input(
                InputEvent::Key { .. }
            )))
        ));
        assert!(input.end.borrow().is_none());
    }

    #[test]
    fn file_header_blocks_old_clipboard_output_before_the_body_arrives() {
        struct GateReader {
            bytes: io::Cursor<Vec<u8>>,
            waiting: std_mpsc::Sender<()>,
            release: std_mpsc::Receiver<()>,
            released: bool,
        }
        impl Read for GateReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.bytes.position() >= 8 && !self.released {
                    self.waiting.send(()).unwrap();
                    self.release.recv().unwrap();
                    self.released = true;
                }
                self.bytes.read(buf)
            }
        }
        let offer = ClientMsg::ClipboardFiles {
            generation: 9,
            files: vec![adit_rdp_proto::ClipFile {
                name: "synthetic.txt".into(),
                size: 42,
                is_dir: false,
            }],
        };
        let (sender, mut input) = input_channel();
        let (waiting, observed) = std_mpsc::channel();
        let (release, proceed) = std_mpsc::channel();
        let worker = thread::spawn(move || {
            let mut reader = GateReader {
                bytes: io::Cursor::new(encoded_client(&offer)),
                waiting,
                release: proceed,
                released: false,
            };
            matches!(
                read_client_frame(&mut reader, &sender).unwrap(),
                Some(ClientFrame::FileHandled)
            )
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(input.has_pending_offer());
        assert!(input.commands.try_recv().is_err());
        release.send(()).unwrap();
        assert!(worker.join().unwrap());
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::LocalFileOfferRejected
        ));
        assert!(!input.has_pending_offer());
        assert!(input.end.borrow().is_none());
    }

    #[test]
    fn saturated_file_input_waits_without_closing_and_preserves_order() {
        use adit_rdp_proto::InputEvent;
        for (message, rejected) in [
            (
                ClientMsg::ClipboardFiles {
                    generation: 0,
                    files: Vec::new(),
                },
                false,
            ),
            (ClientMsg::LocalFileOfferRejected, true),
        ] {
            let (mut sender, mut input) = input_channel();
            let (waiting, observed) = std_mpsc::channel();
            sender.capacity_wait_observer = Some(waiting);
            for _ in 0..MAX_QUEUED_COMMANDS {
                assert!(sender.send(SessionCmd::Input(InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed: true,
                })));
            }
            let (done, result) = std_mpsc::channel();
            let worker = thread::spawn(move || {
                let mut reader = io::Cursor::new(encoded_client(&message));
                let accepted = matches!(
                    read_client_frame(&mut reader, &sender).unwrap(),
                    Some(ClientFrame::FileHandled)
                );
                done.send(accepted).unwrap();
            });
            observed.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(input.has_pending_offer());
            assert!(matches!(
                result.recv_timeout(BACKPRESSURE_TIMEOUT + Duration::from_millis(100)),
                Err(std_mpsc::RecvTimeoutError::Timeout)
            ));
            assert!(input.end.borrow().is_none());
            drop(input.commands.try_recv().unwrap());
            assert!(result.recv_timeout(Duration::from_secs(1)).unwrap());
            for _ in 1..MAX_QUEUED_COMMANDS {
                drop(input.commands.try_recv().unwrap());
            }
            let command = input.commands.try_recv().unwrap().command;
            if rejected {
                assert!(matches!(command, SessionCmd::LocalFileOfferRejected));
            } else {
                assert!(
                    matches!(command, SessionCmd::ClipboardFiles { generation: 0, files } if files.is_empty())
                );
            }
            assert!(!input.has_pending_offer());
            assert!(input.end.borrow().is_none());
            worker.join().unwrap();
        }
    }

    #[test]
    fn oversized_file_frames_are_drained_in_bounded_chunks_without_disconnect() {
        struct ReadMeter<R> {
            inner: R,
            largest: usize,
        }
        impl<R: Read> Read for ReadMeter<R> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.largest = self.largest.max(buf.len());
                self.inner.read(buf)
            }
        }
        for tag in [
            CLIENT_CLIPBOARD_FILES,
            CLIENT_FILE_CONTENTS,
            CLIENT_BOUND_FILE_CONTENTS,
            CLIENT_LOCAL_FILE_OFFER_REJECTED,
        ] {
            let length = adit_rdp_proto::MAX_MESSAGE_BYTES + 1;
            let mut header = Vec::new();
            header.extend_from_slice(&(length as u32).to_le_bytes());
            header.extend_from_slice(&tag.to_le_bytes());
            let used = if tag == CLIENT_CLIPBOARD_FILES {
                header.extend_from_slice(&7_u64.to_le_bytes());
                header.extend_from_slice(&1_u64.to_le_bytes());
                20
            } else {
                4
            };
            let after = encoded_client(&ClientMsg::Input(adit_rdp_proto::InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            }));
            let source = io::Cursor::new(header)
                .chain(io::repeat(0xa5).take((length - used) as u64))
                .chain(io::Cursor::new(after));
            let mut reader = ReadMeter {
                inner: source,
                largest: 0,
            };
            let (sender, mut input) = input_channel();
            assert!(matches!(
                read_client_frame(&mut reader, &sender).unwrap(),
                Some(ClientFrame::FileHandled)
            ));
            if tag == CLIENT_CLIPBOARD_FILES || tag == CLIENT_LOCAL_FILE_OFFER_REJECTED {
                assert!(matches!(
                    input.commands.try_recv().unwrap().command,
                    SessionCmd::LocalFileOfferRejected
                ));
            } else {
                assert!(input.commands.try_recv().is_err());
            }
            assert!(matches!(
                read_client_frame(&mut reader, &sender).unwrap(),
                Some(ClientFrame::Message(ClientMsg::Input(_)))
            ));
            assert!(reader.largest <= 8192);
            assert!(input.end.borrow().is_none());
        }
    }

    #[test]
    fn stdout_deadline_tracks_write_progress_not_idle_time() {
        let progress = StdoutProgress::default();
        let now = Instant::now();
        assert!(!progress.stalled_at(now + STDOUT_PROGRESS_TIMEOUT * 2, STDOUT_PROGRESS_TIMEOUT));
        progress.begin_write();
        let first = progress.0.lock().unwrap().unwrap();
        assert!(!progress.stalled_at(
            first + STDOUT_PROGRESS_TIMEOUT - Duration::from_millis(1),
            STDOUT_PROGRESS_TIMEOUT
        ));
        assert!(progress.stalled_at(first + STDOUT_PROGRESS_TIMEOUT, STDOUT_PROGRESS_TIMEOUT));
        progress.made_progress();
        let next = progress.0.lock().unwrap().unwrap();
        assert!(!progress.stalled_at(next, STDOUT_PROGRESS_TIMEOUT));
        progress.finish_frame();
        assert!(!progress.stalled_at(next + STDOUT_PROGRESS_TIMEOUT * 2, STDOUT_PROGRESS_TIMEOUT));
    }

    #[tokio::test]
    async fn stdout_failure_before_watchdog_subscription_is_not_lost() {
        let progress = Arc::new(StdoutProgress::default());
        progress.fail();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_with_stdout_watchdog(std::future::pending(), progress),
        )
        .await
        .expect("watchdog missed the earlier writer failure");
        assert!(matches!(result, Err(RdpError::ControlChannelClosed)));
    }

    #[tokio::test]
    async fn close_interrupts_active_output_wait_without_discarding_queued_frames() {
        let (sender, input) = input_channel();
        let (host, receiver) = host_channel();
        host.send(HostMsg::Tile {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        })
        .unwrap();
        for stream_id in 0..MAX_QUEUED_FILE_MESSAGES as u32 {
            host.send(HostMsg::FileContents {
                generation: 1,
                stream_id,
                data: None,
            })
            .unwrap();
        }
        let active = async {
            host.send_when_ready(HostMsg::FileContents {
                generation: 1,
                stream_id: 999,
                data: None,
            })
            .await
            .map_err(|error| RdpError::Session(error.to_string()))
        };
        let guarded = crate::session::guard_active_session(active, input.end.clone());
        tokio::pin!(guarded);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut guarded)
                .await
                .is_err()
        );
        sender.stop(InputEnd::Close);
        assert!(tokio::time::timeout(Duration::from_secs(1), &mut guarded)
            .await
            .expect("active session ignored Close")
            .unwrap()
            .is_none());
        host.send(HostMsg::Closed).unwrap();
        let mut bytes = Vec::new();
        write_host_messages(&mut bytes, receiver);
        let mut pipe = io::Cursor::new(bytes);
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::Tile { x: 1, rgba, .. }) if rgba == [1, 2, 3, 4])
        );
        for stream_id in 0..MAX_QUEUED_FILE_MESSAGES as u32 {
            assert!(
                matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::FileContents { stream_id: got, .. }) if got == stream_id)
            );
        }
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut pipe).unwrap(),
            Some(HostMsg::Closed)
        ));
    }

    #[test]
    fn writer_failure_while_sending_final_closed_is_not_reported_as_success() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "synthetic broken stdout",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (host, receiver) = host_channel();
        let progress = Arc::clone(&host.progress);
        host.send(HostMsg::Closed).unwrap();
        write_host_messages_with_progress(&mut BrokenWriter, receiver, Arc::clone(&progress));
        assert!(matches!(
            final_session_result(Ok(()), true, &progress),
            Err(RdpError::ControlChannelClosed)
        ));
    }

    fn file_request(stream_id: u32) -> SessionCmd {
        SessionCmd::RequestFileContents {
            generation: 1,
            stream_id,
            index: 0,
            offset: 0,
            length: 1,
        }
    }

    #[test]
    fn mouse_flood_coalesces_without_losing_button_or_key_edges() {
        use adit_rdp_proto::{InputEvent, MouseButton};
        let (sender, mut input) = input_channel();
        let (other, mut other_input) = input_channel();
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
        assert!(sender.send(SessionCmd::Input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true
        })));
        for n in 0..1024 {
            assert!(sender.send(SessionCmd::Input(InputEvent::Resize {
                width: n + 1,
                height: n + 1
            })));
        }
        assert!(other.send(SessionCmd::Input(InputEvent::MouseMove { x: 3, y: 4 })));
        assert_eq!(input.commands.len(), 5);
        assert_eq!(*input.end.borrow(), None);
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::Input(InputEvent::MouseButton { pressed: true, .. })
        ));
        let SessionCmd::ReplaceableInput(move_slot) = input.commands.try_recv().unwrap().command
        else {
            panic!("missing move")
        };
        assert!(matches!(
            move_slot.lock().unwrap().take(),
            Some(InputEvent::MouseMove { x: 1023, y: 1023 })
        ));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::Input(InputEvent::MouseButton { pressed: false, .. })
        ));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::Input(InputEvent::Key { pressed: true, .. })
        ));
        let SessionCmd::ReplaceableInput(resize_slot) = input.commands.try_recv().unwrap().command
        else {
            panic!("missing resize")
        };
        assert!(matches!(
            resize_slot.lock().unwrap().take(),
            Some(InputEvent::Resize {
                width: 1024,
                height: 1024
            })
        ));
        let SessionCmd::ReplaceableInput(other_slot) =
            other_input.commands.try_recv().unwrap().command
        else {
            panic!("missing other session move")
        };
        assert!(matches!(
            other_slot.lock().unwrap().take(),
            Some(InputEvent::MouseMove { x: 3, y: 4 })
        ));
    }

    #[test]
    fn progressing_clipboard_wire_extends_input_queue_backpressure_deadline() {
        let (mut sender, mut input) = input_channel();
        for id in 0..MAX_QUEUED_COMMANDS as u32 {
            assert!(sender.send(file_request(id)));
        }
        input.ordering.lock().unwrap().wire_progress = Some(Instant::now());
        let (waiting_tx, waiting_rx) = std_mpsc::channel();
        sender.capacity_wait_observer = Some(waiting_tx);
        let (done_tx, done_rx) = std_mpsc::channel();
        let writer = thread::spawn(move || {
            let result = sender.send_with_wait(
                SessionCmd::Input(adit_rdp_proto::InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed: true,
                }),
                Duration::from_millis(20),
            );
            done_tx.send(result).unwrap();
        });
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(60)).is_err());
        assert_eq!(*input.end.borrow(), None);
        drop(input.commands.try_recv().unwrap());
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        writer.join().unwrap();
    }

    #[test]
    fn input_flood_is_bounded_and_overload_does_not_affect_another_session() {
        let (sender, mut receiver) = input_channel();
        let (other, other_receiver) = input_channel();
        for id in 0..MAX_QUEUED_COMMANDS as u32 {
            assert!(sender.send(file_request(id)));
        }
        assert!(!sender.send_with_wait(
            SessionCmd::Input(adit_rdp_proto::InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            }),
            Duration::ZERO,
        ));
        assert_eq!(*receiver.end.borrow(), Some(InputEnd::Overloaded));
        sender.stop(InputEnd::Close);
        assert_eq!(*receiver.end.borrow(), Some(InputEnd::Overloaded));
        assert!(!sender.send(file_request(10000)));
        assert!(other.send(file_request(1)));
        assert_eq!(*other_receiver.end.borrow(), None);
        assert_eq!(receiver.commands.len(), MAX_QUEUED_COMMANDS);
        assert!(matches!(
            receiver.commands.try_recv().unwrap().command,
            SessionCmd::RequestFileContents { stream_id: 0, .. }
        ));
    }

    #[test]
    fn pending_offer_tracking_follows_the_fifo_and_releases_failed_sends() {
        use adit_rdp_proto::InputEvent;
        let (sender, mut receiver) = input_channel();
        assert!(sender.send(SessionCmd::Input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        })));
        assert!(sender.send(SessionCmd::ClipboardFiles {
            generation: 0,
            files: Vec::new()
        }));
        assert!(sender.send(SessionCmd::ClipboardImage(vec![1])));
        assert!(receiver.has_pending_offer());
        let _key = receiver.commands.try_recv().unwrap().command;
        assert!(receiver.has_pending_offer());
        let _files = receiver.commands.try_recv().unwrap().command;
        assert!(receiver.has_pending_offer());
        let _image = receiver.commands.try_recv().unwrap().command;
        assert!(!receiver.has_pending_offer());

        for id in 0..MAX_QUEUED_COMMANDS as u32 {
            assert!(sender.send(file_request(id)));
        }
        assert!(!sender.send_with_wait(SessionCmd::ClipboardImage(vec![1]), Duration::ZERO));
        assert!(
            !receiver.has_pending_offer(),
            "an offer that was never enqueued must release its marker"
        );
        let (sender, receiver) = input_channel();
        drop(receiver);
        assert!(!sender.send(SessionCmd::ClipboardImage(vec![1])));
    }

    #[test]
    fn a_decoded_revoke_remains_visible_while_the_input_queue_is_full() {
        let (mut sender, mut input) = input_channel();
        let (waiting_tx, waiting_rx) = std_mpsc::channel();
        sender.capacity_wait_observer = Some(waiting_tx);
        for id in 0..MAX_QUEUED_COMMANDS as u32 {
            assert!(sender.send(file_request(id)));
        }
        let (done_tx, done_rx) = std_mpsc::channel();
        let worker = thread::spawn(move || {
            let sent = sender.send_with_wait(
                SessionCmd::ClipboardFiles {
                    generation: 0,
                    files: Vec::new(),
                },
                Duration::from_secs(2),
            );
            let _ = done_tx.send(sent);
        });
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            input.has_pending_offer(),
            "the decoded revoke disappeared behind a full queue"
        );
        assert!(matches!(
            done_rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
        drop(input.commands.try_recv().unwrap());
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert!(input.has_pending_offer());
        for _ in 1..MAX_QUEUED_COMMANDS {
            drop(input.commands.try_recv().unwrap());
        }
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::ClipboardFiles { generation: 0, .. }
        ));
        assert!(!input.has_pending_offer());
        worker.join().unwrap();
    }

    #[test]
    fn a_decoded_revoke_remains_visible_while_waiting_for_byte_capacity() {
        let (mut sender, mut input) = input_channel();
        let (waiting_tx, waiting_rx) = std_mpsc::channel();
        sender.capacity_wait_observer = Some(waiting_tx);
        sender.bytes = Arc::new(Semaphore::new(file_request(1).queued_bytes()));
        assert!(sender.send(file_request(1)));
        let (done_tx, done_rx) = std_mpsc::channel();
        let worker = thread::spawn(move || {
            let sent = sender.send_with_wait(
                SessionCmd::ClipboardFiles {
                    generation: 0,
                    files: Vec::new(),
                },
                Duration::from_secs(2),
            );
            let _ = done_tx.send(sent);
        });
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            input.has_pending_offer(),
            "the decoded revoke must be visible while byte permits are held"
        );
        assert!(matches!(
            done_rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
        drop(input.commands.try_recv().unwrap());
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::ClipboardFiles { generation: 0, .. }
        ));
        assert!(!input.has_pending_offer());
        worker.join().unwrap();
    }

    #[test]
    fn input_byte_budget_is_held_until_a_command_is_taken() {
        let (mut sender, mut receiver) = input_channel();
        let bytes = file_request(1).queued_bytes();
        sender.bytes = Arc::new(Semaphore::new(bytes));
        assert!(sender.send(file_request(1)));
        let queued = receiver.commands.try_recv().unwrap();
        assert_eq!(sender.bytes.available_permits(), 0);
        drop(queued);
        assert_eq!(sender.bytes.available_permits(), bytes);
        assert!(sender.send(file_request(2)));
        assert!(!sender.send_with_wait(
            SessionCmd::Input(adit_rdp_proto::InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            }),
            Duration::ZERO,
        ));
        assert_eq!(*receiver.end.borrow(), Some(InputEnd::Overloaded));
    }

    #[test]
    fn oversized_clipboard_text_is_drained_without_ending_the_desktop() {
        use adit_rdp_proto::InputEvent;
        let (sender, mut input) = input_channel();
        let payload = MAX_QUEUED_COMMAND_BYTES as usize;
        let mut header = Vec::new();
        header.extend_from_slice(&((16 + payload) as u32).to_le_bytes());
        header.extend_from_slice(&CLIENT_INPUT.to_le_bytes());
        header.extend_from_slice(&INPUT_CLIPBOARD_TEXT.to_le_bytes());
        header.extend_from_slice(&(payload as u64).to_le_bytes());
        let key = encoded_client(&ClientMsg::Input(InputEvent::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        }));
        let mut pipe = io::Cursor::new(header)
            .chain(io::repeat(b'x').take(payload as u64))
            .chain(io::Cursor::new(key));
        assert!(matches!(
            read_client_frame(&mut pipe, &sender).unwrap(),
            Some(ClientFrame::FileHandled)
        ));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::LocalFileOfferRejected
        ));
        let Some(ClientFrame::Message(ClientMsg::Input(key))) =
            read_client_frame(&mut pipe, &sender).unwrap()
        else {
            panic!("key frame lost after rejected text")
        };
        assert!(sender.send(SessionCmd::Input(key)));
        assert!(matches!(
            input.commands.try_recv().unwrap().command,
            SessionCmd::Input(InputEvent::Key { .. })
        ));
        assert_eq!(*input.end.borrow(), None);
    }

    #[test]
    fn output_file_offers_keep_the_policy_of_their_original_queue_admission() {
        let (host, receiver) = host_channel();
        let offer = |generation| HostMsg::ClipboardFiles {
            generation,
            files: vec![adit_rdp_proto::ClipFile {
                name: format!("{generation}.bin"),
                size: 1,
                is_dir: false,
            }],
        };
        host.send(offer(7)).unwrap();
        let (sender, _input) = input_channel();
        let mut command = io::Cursor::new(encoded_client(&ClientMsg::ClipboardPolicyEpoch(2)));
        let Some(ClientFrame::Message(ClientMsg::ClipboardPolicyEpoch(epoch))) =
            read_client_frame(&mut command, &sender).unwrap()
        else {
            panic!("policy message missing")
        };
        observe_clipboard_policy(&host.clipboard_policy_epoch, epoch);
        host.send(offer(8)).unwrap();
        host.send(HostMsg::Closed).unwrap();
        let mut wire = Vec::new();
        write_host_messages(&mut wire, receiver);
        let mut wire = io::Cursor::new(wire);
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut wire).unwrap(),
            Some(HostMsg::ClipboardFilesForPolicy {
                policy_epoch: 0,
                generation: 7,
                ..
            })
        ));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut wire).unwrap(),
            Some(HostMsg::ClipboardFilesForPolicy {
                policy_epoch: 2,
                generation: 8,
                ..
            })
        ));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut wire).unwrap(),
            Some(HostMsg::Closed)
        ));
    }

    #[test]
    fn policy_acknowledgements_are_bounded_and_report_the_latest_epoch() {
        let (host, receiver) = host_channel();
        for epoch in 1..=10_000 {
            observe_clipboard_policy(&host.clipboard_policy_epoch, epoch);
            host.queue_policy_ack().unwrap();
        }
        host.send(HostMsg::Closed).unwrap();
        let mut wire = Vec::new();
        write_host_messages(&mut wire, receiver);
        let mut wire = io::Cursor::new(wire);
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut wire).unwrap(),
            Some(HostMsg::ClipboardPolicyApplied {
                policy_epoch: 10_000,
                enabled: true
            })
        ));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut wire).unwrap(),
            Some(HostMsg::Closed)
        ));
        assert!(read_msg::<_, HostMsg>(&mut wire).unwrap().is_none());
    }

    #[test]
    fn a_command_larger_than_the_input_budget_fails_even_if_stdout_advances() {
        let (sender, receiver) = input_channel();
        sender.stdout_progress.begin_write();
        let (done_tx, done_rx) = std_mpsc::channel();
        thread::spawn(move || {
            let image = Vec::with_capacity(MAX_QUEUED_COMMAND_BYTES as usize + 1);
            let sent =
                sender.send_with_wait(SessionCmd::ClipboardImage(image), Duration::from_millis(5));
            let _ = done_tx.send(sent);
        });
        assert!(!done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("oversized command waited for impossible capacity"));
        assert_eq!(*receiver.end.borrow(), Some(InputEnd::Overloaded));
    }

    #[test]
    fn transient_input_backpressure_preserves_mouse_and_key_edges_in_order() {
        use adit_rdp_proto::{InputEvent, MouseButton};

        let (sender, mut receiver) = input_channel();
        for x in 0..MAX_QUEUED_COMMANDS as u16 {
            assert!(sender.send(SessionCmd::Input(InputEvent::Key {
                scancode: x as u8,
                extended: false,
                pressed: true,
            })));
        }
        let (started_tx, started_rx) = std_mpsc::channel();
        let (first_sent_tx, first_sent_rx) = std_mpsc::channel();
        let writer = thread::spawn(move || {
            started_tx.send(()).unwrap();
            for (index, event) in [
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
                InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed: true,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: false,
                },
                InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed: false,
                },
            ]
            .into_iter()
            .enumerate()
            {
                assert!(sender.send(SessionCmd::Input(event)));
                if index == 0 {
                    first_sent_tx.send(()).unwrap();
                }
            }
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            first_sent_rx.recv_timeout(Duration::from_millis(20)),
            Err(std_mpsc::RecvTimeoutError::Timeout)
        ));
        for x in 0..MAX_QUEUED_COMMANDS as u16 {
            assert!(matches!(receiver.commands.blocking_recv().unwrap().command,
                SessionCmd::Input(InputEvent::Key { scancode: got, extended: false, pressed: true }) if got == x as u8));
        }
        first_sent_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        for (button, pressed) in [(true, true), (false, true), (true, false), (false, false)] {
            let event = receiver.commands.blocking_recv().unwrap().command;
            if button {
                assert!(matches!(event, SessionCmd::Input(InputEvent::MouseButton {
                    button: MouseButton::Left, pressed: got
                }) if got == pressed));
            } else {
                assert!(matches!(event, SessionCmd::Input(InputEvent::Key {
                    scancode: 30, extended: false, pressed: got
                }) if got == pressed));
            }
        }
        writer.join().unwrap();
        assert_eq!(*receiver.end.borrow(), None);
    }

    #[test]
    fn persistent_input_backpressure_has_a_deadline() {
        use adit_rdp_proto::InputEvent;
        let (sender, receiver) = input_channel();
        for x in 0..MAX_QUEUED_COMMANDS as u16 {
            assert!(sender.send(SessionCmd::Input(InputEvent::Key {
                scancode: x as u8,
                extended: false,
                pressed: true,
            })));
        }
        assert!(!sender.send_with_wait(
            SessionCmd::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true
            }),
            Duration::from_millis(20),
        ));
        assert_eq!(*receiver.end.borrow(), Some(InputEnd::Overloaded));
    }

    #[test]
    fn close_and_invalid_frames_are_signalled_out_of_band() {
        for reason in [InputEnd::Close, InputEnd::InvalidFrame] {
            let (sender, receiver) = input_channel();
            assert!(sender.send(file_request(1)));
            sender.stop(reason);
            assert_eq!(*receiver.end.borrow(), Some(reason));
            assert!(!sender.send(file_request(2)));
        }
    }

    struct BlockingWriter {
        bytes: Vec<u8>,
        entered: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_writer_bounds_requests_and_failures_but_preserves_video_and_shutdown() {
        let (host, receiver) = host_channel();
        let (other, _other_receiver) = host_channel();
        for id in 0..MAX_QUEUED_FILE_MESSAGES as u32 {
            let message = if id % 2 == 0 {
                HostMsg::FileContentsNeeded {
                    channel_epoch: 1,
                    snapshot_revision: 1,
                    generation: 1,
                    stream_id: id,
                    index: 0,
                    offset: 0,
                    length: 1,
                }
            } else {
                HostMsg::FileContents {
                    generation: 1,
                    stream_id: id,
                    data: None,
                }
            };
            host.send(message).unwrap();
        }
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let writer = thread::spawn(move || {
            let mut writer = BlockingWriter {
                bytes: Vec::new(),
                entered: Some(entered_tx),
                release: release_rx,
            };
            write_host_messages(&mut writer, receiver);
            writer.bytes
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            host.budget.lock().unwrap().file_messages,
            MAX_QUEUED_FILE_MESSAGES
        );
        assert_eq!(
            host.send(HostMsg::FileContents {
                generation: 1,
                stream_id: 999,
                data: None
            }),
            Err(HostSendError::Overloaded)
        );
        assert!(other
            .send(HostMsg::FileContents {
                generation: 2,
                stream_id: 1,
                data: None
            })
            .is_ok());
        host.send(HostMsg::Tile {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        })
        .unwrap();
        host.send(HostMsg::Error("overloaded".into())).unwrap();
        host.send(HostMsg::Closed).unwrap();
        release_tx.send(()).unwrap();
        let bytes = writer.join().unwrap();
        assert_eq!(host.budget.lock().unwrap().file_messages, 0);
        let mut pipe = io::Cursor::new(bytes);
        for id in 0..MAX_QUEUED_FILE_MESSAGES as u32 {
            let message = read_msg::<_, HostMsg>(&mut pipe).unwrap().unwrap();
            if id % 2 == 0 {
                assert!(
                    matches!(message, HostMsg::FileContentsNeeded { stream_id, .. } if stream_id == id)
                );
            } else {
                assert!(
                    matches!(message, HostMsg::FileContents { stream_id, data: None, .. } if stream_id == id)
                );
            }
        }
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::Tile { rgba, .. }) if rgba == [1, 2, 3, 4])
        );
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::Error(message)) if message == "overloaded")
        );
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut pipe).unwrap(),
            Some(HostMsg::Closed)
        ));
    }

    #[test]
    fn broken_stdout_releases_all_file_slots_and_reports_closed() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("synthetic broken pipe"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (host, receiver) = host_channel();
        for id in 0..3 {
            host.send(HostMsg::FileContents {
                generation: 1,
                stream_id: id,
                data: None,
            })
            .unwrap();
        }
        write_host_messages(&mut BrokenWriter, receiver);
        assert_eq!(host.budget.lock().unwrap().file_messages, 0);
        assert_eq!(
            host.send(HostMsg::FileContents {
                generation: 1,
                stream_id: 4,
                data: None
            }),
            Err(HostSendError::Closed)
        );
        assert_eq!(host.budget.lock().unwrap().file_messages, 0);
    }

    #[test]
    fn video_output_is_bounded_and_error_and_closed_still_follow_it() {
        let (host, receiver) = host_channel();
        for id in 0..MAX_QUEUED_HOST_MESSAGES {
            host.send(HostMsg::Tile {
                x: id as u16,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            })
            .unwrap();
        }
        assert_eq!(
            host.send(HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4]
            }),
            Err(HostSendError::Overloaded)
        );
        host.send(HostMsg::Error("output overloaded".into()))
            .unwrap();
        host.send(HostMsg::Closed).unwrap();
        let mut bytes = Vec::new();
        write_host_messages(&mut bytes, receiver);
        let mut pipe = io::Cursor::new(bytes);
        for id in 0..MAX_QUEUED_HOST_MESSAGES {
            assert!(
                matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::Tile { x, rgba, .. }) if usize::from(x) == id && rgba == [1, 2, 3, 4])
            );
        }
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::Error(message)) if message == "output overloaded")
        );
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut pipe).unwrap(),
            Some(HostMsg::Closed)
        ));
        assert_eq!(host.budget.lock().unwrap().messages, 0);
    }

    #[tokio::test]
    async fn transient_video_backpressure_keeps_all_incremental_tiles_in_order() {
        let (host, receiver) = host_channel();
        for id in 0..MAX_QUEUED_HOST_MESSAGES {
            host.send(HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![id as u8, 0, 0, 255],
            })
            .unwrap();
        }
        let pending = host.send_when_ready(HostMsg::Tile {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            rgba: vec![222, 0, 0, 255],
        });
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        let first = receiver.try_recv().unwrap();
        assert!(matches!(first.message, HostMsg::Tile { ref rgba, .. } if rgba == &[0, 0, 0, 255]));
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        host.send(HostMsg::Closed).unwrap();
        let mut bytes = Vec::new();
        write_host_messages(&mut bytes, receiver);
        let mut pipe = io::Cursor::new(bytes);
        for id in 1..MAX_QUEUED_HOST_MESSAGES {
            assert!(matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(),
                Some(HostMsg::Tile { rgba, .. }) if rgba == [id as u8, 0, 0, 255]));
        }
        assert!(matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(),
            Some(HostMsg::Tile { rgba, .. }) if rgba == [222, 0, 0, 255]));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut pipe).unwrap(),
            Some(HostMsg::Closed)
        ));
    }

    #[tokio::test]
    async fn full_output_budget_waits_while_a_frame_keeps_progressing() {
        let (host, receiver) = host_channel();
        for stream_id in 0..MAX_QUEUED_FILE_MESSAGES as u32 {
            host.send(HostMsg::FileContents {
                generation: 1,
                stream_id,
                data: None,
            })
            .unwrap();
        }
        host.progress.begin_write();
        let pending = host.send_when_ready(HostMsg::FileContents {
            generation: 1,
            stream_id: 999,
            data: None,
        });
        tokio::pin!(pending);
        let started = Instant::now();
        while started.elapsed() < BACKPRESSURE_TIMEOUT + Duration::from_millis(200) {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut pending)
                    .await
                    .is_err()
            );
            host.progress.made_progress();
        }
        drop(receiver.try_recv().unwrap());
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        host.progress.finish_frame();
        assert_eq!(
            host.budget.lock().unwrap().file_messages,
            MAX_QUEUED_FILE_MESSAGES
        );
    }

    #[tokio::test]
    async fn close_and_overload_interrupt_a_connect_before_any_network_access() {
        for reason in [
            InputEnd::Close,
            InputEnd::Overloaded,
            InputEnd::InvalidFrame,
        ] {
            let (input, receiver) = input_channel();
            input.stop(reason);
            let (host, _messages) = host_channel();
            let request = ConnectRequest {
                host: "unresolved.invalid".into(),
                port: 9,
                username: String::new(),
                password: String::new(),
                domain: None,
                width: 1024,
                height: 768,
                enable_clipboard: true,
                enable_audio: false,
                quality: adit_rdp_proto::Quality::default(),
                clipboard_policy_id: None,
            };
            let result = run_session(request, receiver, host).await;
            match reason {
                InputEnd::Close => assert!(matches!(result, Err(RdpError::ControlChannelClosed))),
                InputEnd::Overloaded | InputEnd::InvalidFrame => {
                    assert!(matches!(result, Err(RdpError::Session(_))))
                }
            }
        }
    }
}
