//! Bounded helper output, including the frame currently inside a pipe write.
//!
//! Producers never wait on stdout: a full budget ends the connection instead of
//! parking the RDP loop ahead of a key/button release or a protocol response.
//! Cancellation cannot retract bytes already accepted by the pipe. A partially
//! written frame must end the stream; it must never be followed by another frame.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use adit_rdp_proto::{write_ipc_hello, write_msg, HostMsg, PointerUpdate, IPC_PROTOCOL_VERSION};
use tokio::sync::watch;

use crate::RdpError;

const WRITE_CHUNK: usize = 4096;
const FINAL_ERROR_BYTES: usize = 16 * 1024;
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct Limits {
    messages: usize,
    bytes: usize,
    stall: Duration,
    drain: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            messages: 2048,
            // An 8192×8192 RGBA desktop and paint mask need 264 MiB. Leave room
            // for control/clipboard traffic without an unbounded frame backlog.
            bytes: 320 * 1024 * 1024,
            stall: Duration::from_secs(10),
            // Even a peer that reads one byte per deadline cannot extend exit
            // forever. Normal session traffic has only the inactivity deadline.
            drain: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HostSendError {
    #[error("RDP 控制通道已关闭")]
    Closed,
    #[error("RDP 辅助进程输出队列超出预算，会话已关闭")]
    Overloaded,
    #[error("RDP 辅助进程输出停滞，会话已关闭")]
    Stalled,
}

impl HostSendError {
    fn session_error(self) -> RdpError {
        match self {
            Self::Closed => RdpError::ControlChannelClosed,
            error => RdpError::Session(error.to_string()),
        }
    }
}

#[derive(Default)]
struct Budget {
    messages: usize,
    bytes: usize,
    sealed: bool,
}

struct Progress {
    active: Option<Instant>,
    last_write: Instant,
}

struct OutputState {
    budget: Mutex<Budget>,
    progress: Mutex<Progress>,
    failure: watch::Sender<Option<HostSendError>>,
    cancelled: AtomicBool,
    /// The app asked the session to close.
    ///
    /// Set when the app's own close announcement reaches the session, so the
    /// output pipe breaking afterwards can be recognised as the *expected*
    /// consequence of that close rather than a delivery failure.
    app_closing: AtomicBool,
    limits: Limits,
}

impl OutputState {
    fn fail(&self, error: HostSendError) {
        self.failure.send_if_modified(|failure| {
            if failure.is_none() {
                *failure = Some(error);
                true
            } else {
                false
            }
        });
    }

    fn abort(&self, error: HostSendError) {
        self.cancelled.store(true, Ordering::Release);
        self.fail(error);
    }

    fn begin_frame(&self) {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = Some(Instant::now());
    }

    fn made_progress(&self) {
        let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        progress.active = Some(now);
        progress.last_write = now;
    }

    fn finish_frame(&self) {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = None;
    }

    fn stalled(&self, now: Instant) -> bool {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .is_some_and(|last| now.saturating_duration_since(last) >= self.limits.stall)
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            // write_all retries Interrupted, so cancellation must be terminal.
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "helper output cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

struct MessagePermit {
    state: Arc<OutputState>,
    bytes: usize,
}

impl Drop for MessagePermit {
    fn drop(&mut self) {
        let mut budget = self.state.budget.lock().unwrap_or_else(|e| e.into_inner());
        budget.messages -= 1;
        budget.bytes -= self.bytes;
    }
}

struct QueuedMessage {
    message: HostMsg,
    _permit: Option<MessagePermit>,
}

#[derive(Clone)]
pub(crate) struct HostSender {
    messages: mpsc::Sender<QueuedMessage>,
    state: Arc<OutputState>,
}

pub(crate) struct HostReceiver {
    messages: mpsc::Receiver<QueuedMessage>,
    state: Arc<OutputState>,
    completed: bool,
}

impl Drop for HostReceiver {
    fn drop(&mut self) {
        // Also wakes an idle session if the writer panics or disappears before
        // another producer happens to send a message.
        if !self.completed {
            self.state.abort(HostSendError::Closed);
        }
    }
}

pub(crate) fn host_channel() -> (HostSender, HostReceiver) {
    channel_with_limits(Limits::default())
}

fn channel_with_limits(limits: Limits) -> (HostSender, HostReceiver) {
    let (messages, receiver) = mpsc::channel();
    let (failure, _) = watch::channel(None);
    let state = Arc::new(OutputState {
        budget: Mutex::new(Budget::default()),
        progress: Mutex::new(Progress {
            active: None,
            last_write: Instant::now(),
        }),
        failure,
        cancelled: AtomicBool::new(false),
        app_closing: AtomicBool::new(false),
        limits,
    });
    (
        HostSender {
            messages,
            state: Arc::clone(&state),
        },
        HostReceiver {
            messages: receiver,
            state,
            completed: false,
        },
    )
}

impl HostSender {
    pub(crate) fn send(&self, message: HostMsg) -> Result<(), HostSendError> {
        match message {
            HostMsg::Error(error) => return self.finish(Some(error)),
            HostMsg::Closed => return self.finish(None),
            _ => {}
        }
        let bytes = message_bytes(&message);
        let mut budget = self.state.budget.lock().unwrap_or_else(|e| e.into_inner());
        if budget.sealed || self.state.failure.borrow().is_some() {
            return Err(HostSendError::Closed);
        }
        if budget.messages >= self.state.limits.messages
            || bytes > self.state.limits.bytes.saturating_sub(budget.bytes)
        {
            drop(budget);
            self.state.fail(HostSendError::Overloaded);
            return Err(HostSendError::Overloaded);
        }
        budget.messages += 1;
        budget.bytes += bytes;
        // Enqueue under the admission lock so finish cannot put Closed in
        // front of a message whose permit has already been reserved.
        let sent = self.messages.send(QueuedMessage {
            message,
            _permit: Some(MessagePermit {
                state: Arc::clone(&self.state),
                bytes,
            }),
        });
        drop(budget);
        // Dropping a failed send releases its permit, outside the budget lock.
        sent.map_err(|_| {
            self.state.abort(HostSendError::Closed);
            HostSendError::Closed
        })
    }

    pub(super) fn finish(&self, error: Option<String>) -> Result<(), HostSendError> {
        let mut budget = self.state.budget.lock().unwrap_or_else(|e| e.into_inner());
        if budget.sealed {
            return Ok(());
        }
        budget.sealed = true;
        if self.state.cancelled.load(Ordering::Acquire) {
            return Err(HostSendError::Closed);
        }
        // Exactly one bounded terminal suffix can bypass the ordinary budget.
        // No sender can append data after it, even when sender clones survive.
        let result = (|| {
            if let Some(error) = error {
                let mut end = error.len().min(FINAL_ERROR_BYTES);
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                // Copy the bounded text so a short string with a huge spare
                // capacity cannot evade the reserved terminal memory bound.
                let error = error[..end].to_owned();
                self.messages.send(QueuedMessage {
                    message: HostMsg::Error(error),
                    _permit: None,
                })?;
            }
            self.messages.send(QueuedMessage {
                message: HostMsg::Closed,
                _permit: None,
            })
        })();
        drop(budget);
        result.map_err(|_| {
            self.state.abort(HostSendError::Closed);
            HostSendError::Closed
        })
    }

    /// Record that the app asked the session to close.
    ///
    /// The app stops reading the helper's stdout once it has asked for the
    /// close, so the output pipe breaks as a *consequence*. Remembering the
    /// request is what tells that apart from a pipe that broke while the
    /// session was still running, which is a real delivery failure.
    pub(crate) fn note_app_closing(&self) {
        self.state.app_closing.store(true, Ordering::Release);
    }

    pub(super) fn startup_failed(&self) -> bool {        if self.state.stalled(Instant::now()) {
            self.state.abort(HostSendError::Stalled);
        }
        self.state.failure.borrow().is_some()
    }

    pub(super) async fn supervise<F>(&self, session: F) -> Result<(), RdpError>
    where
        F: std::future::Future<Output = Result<(), RdpError>>,
    {
        tokio::pin!(session);
        let mut failure = self.state.failure.subscribe();
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if let Some(error) = *failure.borrow_and_update() {
                return Err(error.session_error());
            }
            tokio::select! {
                biased;
                _ = failure.changed() => {},
                result = &mut session => return self.final_result(result),
                _ = tick.tick() => {
                    if self.state.stalled(Instant::now()) {
                        self.state.abort(HostSendError::Stalled);
                    }
                }
            }
        }
    }

    pub(super) fn final_result(&self, result: Result<(), RdpError>) -> Result<(), RdpError> {
        match (result, *self.state.failure.borrow()) {
            // The app asked the session to close, so it stopped reading the
            // helper's stdout, and the writer's final flush hit a broken pipe.
            //
            // Reporting that made the helper exit non-zero on *every* normal
            // disconnect, which reads as a crash to anything watching the exit
            // code — the app's own disconnect path, the acceptance harness and
            // the release gate all saw a failure where none happened.
            //
            // Only a pipe that closed *because the app asked it to* is
            // forgiven. A pipe that broke while the session was still running
            // is a real delivery failure and still fails the session — the
            // distinction is the app's own announcement, not the pipe.
            (Ok(()), Some(HostSendError::Closed)) if self.state.app_closing.load(Ordering::Acquire) => {
                Ok(())
            }
            // Existing session callers map a failed send to ControlChannelClosed.
            // Keep the actual output cause when admission triggered that return.
            (Ok(()), Some(error)) | (Err(RdpError::ControlChannelClosed), Some(error)) => {
                Err(error.session_error())
            }
            (result, _) => result,
        }
    }
}

// Count allocated capacity, not only wire length: a short Vec/String may still
// own a large allocation. Keep this exhaustive when the existing schema grows.
fn message_bytes(message: &HostMsg) -> usize {
    let payload = match message {
        HostMsg::Tile { rgba, .. } | HostMsg::Pointer(PointerUpdate::Bitmap { rgba, .. }) => {
            rgba.capacity()
        }
        HostMsg::MaskedTile { rgba, painted, .. } => {
            rgba.capacity().saturating_add(painted.capacity())
        }
        HostMsg::ClipboardText(text)
        | HostMsg::EpochClipboardText { text, .. }
        | HostMsg::Error(text) => text.capacity(),
        HostMsg::ClipboardImage(image) | HostMsg::EpochClipboardImage { image, .. } => {
            image.capacity()
        }
        HostMsg::ClipboardFiles { files, .. } => files.iter().fold(
            files
                .capacity()
                .saturating_mul(std::mem::size_of::<adit_rdp_proto::ClipFile>()),
            |sum, file| sum.saturating_add(file.name.capacity()),
        ),
        HostMsg::FileContents { data, .. } => data.as_ref().map_or(0, Vec::capacity),
        HostMsg::DisplayControl { state, .. } => state
            .sent_layout
            .capacity()
            .saturating_mul(std::mem::size_of::<adit_rdp_proto::Monitor>())
            .saturating_add(state.error.as_ref().map_or(0, String::capacity)),
        HostMsg::MonitorLayoutResult { result, .. } => {
            result.as_ref().err().map_or(0, |error| error.capacity())
        }
        HostMsg::MonitorLayoutObserved { monitors, .. } => monitors
            .capacity()
            .saturating_mul(std::mem::size_of::<adit_rdp_proto::MonitorGeometry>()),
        HostMsg::Connected { .. }
        | HostMsg::Resized { .. }
        | HostMsg::FileContentsNeeded { .. }
        | HostMsg::Closed
        | HostMsg::Pointer(_)
        | HostMsg::DisplayControlReset { .. }
        | HostMsg::InputDiscarded
        | HostMsg::ResizeSent { .. }
        | HostMsg::ResizeRejected { .. }
        | HostMsg::GraphicsReset { .. } => 0,
    };
    std::mem::size_of::<QueuedMessage>().saturating_add(payload)
}

struct ProgressWriter<'a, W> {
    writer: &'a mut W,
    state: &'a OutputState,
}

impl<W: Write> Write for ProgressWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.state.check_cancelled()?;
        // A synchronous Windows pipe write may wait until the whole slice is
        // read. Small slices let a slow, healthy reader demonstrate progress.
        let written = self.writer.write(&bytes[..bytes.len().min(WRITE_CHUNK)])?;
        if written > 0 {
            self.state.made_progress();
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.check_cancelled()?;
        self.writer.flush()
    }
}

fn write_output<W: Write>(
    writer: &mut W,
    hello: mpsc::Receiver<Option<u32>>,
    receiver: &mut HostReceiver,
) -> io::Result<()> {
    let state = Arc::clone(&receiver.state);
    let mut writer = ProgressWriter {
        writer,
        state: &state,
    };
    let version = loop {
        state.check_cancelled()?;
        match hello.recv_timeout(POLL_INTERVAL) {
            Ok(Some(version)) => break version,
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    // An unversioned peer never receives enum data; a mismatched peer receives
    // only the fixed hello, exactly as before this output queue was introduced.
    state.begin_frame();
    write_ipc_hello(&mut writer, IPC_PROTOCOL_VERSION)?;
    state.finish_frame();
    if version != IPC_PROTOCOL_VERSION {
        return Ok(());
    }
    loop {
        state.check_cancelled()?;
        let queued = match receiver.messages.recv_timeout(POLL_INTERVAL) {
            Ok(queued) => queued,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let is_final = matches!(queued.message, HostMsg::Closed);
        state.begin_frame();
        write_msg(&mut writer, &queued.message)?;
        state.finish_frame();
        // The payload and its budget permit live through encoding, all partial
        // writes, and flush. Taking it out of the queue releases no capacity.
        drop(queued);
        if is_final {
            return Ok(());
        }
    }
}

pub(super) struct OutputWriter {
    thread: thread::JoinHandle<()>,
    finished: mpsc::Receiver<()>,
    state: Arc<OutputState>,
}

impl OutputWriter {
    pub(super) fn spawn<W: Write + Send + 'static>(
        mut writer: W,
        hello: mpsc::Receiver<Option<u32>>,
        mut receiver: HostReceiver,
    ) -> io::Result<Self> {
        let state = Arc::clone(&receiver.state);
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("adit-rdp-stdout".into())
            .spawn(move || {
                let result = write_output(&mut writer, hello, &mut receiver);
                if result.is_err() {
                    receiver.state.abort(HostSendError::Closed);
                }
                receiver.completed = result.is_ok();
                drop(receiver);
                drop(writer);
                // Signal after releasing the OS handle. Do not follow this signal
                // with a join: the thread can be descheduled even at its final step.
                let _ = finished_tx.send(());
            })?;
        Ok(Self {
            thread,
            finished,
            state,
        })
    }

    pub(super) fn finish(self) {
        let start = Instant::now();
        loop {
            match self.finished.recv_timeout(POLL_INTERVAL) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.state.abort(HostSendError::Closed);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    let last = self
                        .state
                        .progress
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .last_write
                        .max(start);
                    if self.state.cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    if now.duration_since(start) >= self.state.limits.drain
                        || now.saturating_duration_since(last) >= self.state.limits.stall
                    {
                        self.state.abort(HostSendError::Stalled);
                        break;
                    }
                }
            }
        }
        // Atomic cancellation cannot interrupt an OS write already in progress.
        // Detach it; the binary's main returns and process exit terminates it.
        // stdout_pipe bypasses StdoutLock, including Rust's exit-time flush lock.
        drop(self.thread);
    }
}

pub(super) fn stdout_pipe() -> io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        io::stdout()
            .as_handle()
            .try_clone_to_owned()
            .map(std::fs::File::from)
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .map(std::fs::File::from)
    }
}

#[cfg(test)]
impl HostReceiver {
    pub(crate) fn recv(&self) -> Result<HostMsg, mpsc::RecvError> {
        self.messages.recv().map(|queued| queued.message)
    }

    pub(crate) fn try_recv(&self) -> Result<HostMsg, mpsc::TryRecvError> {
        self.messages.try_recv().map(|queued| queued.message)
    }
}

#[cfg(test)]
mod tests;
