use super::*;
use adit_rdp_proto::{read_ipc_hello, read_msg, ClientMsg, ClipFile, InputEvent};

fn limits(messages: usize, bytes: usize) -> Limits {
    Limits {
        messages,
        bytes,
        stall: Duration::from_millis(150),
        drain: Duration::from_secs(2),
    }
}

fn hello(version: Option<u32>) -> mpsc::Receiver<Option<u32>> {
    let (tx, rx) = mpsc::channel();
    tx.send(version).unwrap();
    rx
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decoded(capture: &Capture) -> Vec<HostMsg> {
    let mut input = io::Cursor::new(capture.0.lock().unwrap().clone());
    assert_eq!(read_ipc_hello(&mut input).unwrap(), IPC_PROTOCOL_VERSION);
    let mut messages = Vec::new();
    while let Some(message) = read_msg(&mut input).unwrap() {
        messages.push(message);
    }
    messages
}

struct GateWriter {
    capture: Capture,
    entered: Option<mpsc::Sender<()>>,
    resume: mpsc::Receiver<()>,
    on_flush: bool,
}

impl GateWriter {
    fn block(&mut self) {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.resume.recv().unwrap();
        }
    }
}

impl Write for GateWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.on_flush && self.capture.0.lock().unwrap().len() >= 20 {
            self.block();
        }
        self.capture.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.on_flush && self.capture.0.lock().unwrap().len() > 20 {
            self.block();
        }
        Ok(())
    }
}

fn gate_writer(on_flush: bool) -> (GateWriter, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, observe) = mpsc::channel();
    let (resume, blocked) = mpsc::channel();
    (
        GateWriter {
            capture: Capture::default(),
            entered: Some(entered),
            resume: blocked,
            on_flush,
        },
        observe,
        resume,
    )
}

#[test]
fn message_budget_and_reserved_terminal_suffix_are_bounded_and_ordered() {
    let (host, receiver) = channel_with_limits(limits(2, 4096));
    host.send(HostMsg::InputDiscarded).unwrap();
    host.send(HostMsg::ClipboardText("保留顺序".into()))
        .unwrap();
    assert_eq!(
        host.send(HostMsg::InputDiscarded),
        Err(HostSendError::Overloaded)
    );
    let clone = host.clone();
    host.finish(Some("失败".repeat(FINAL_ERROR_BYTES))).unwrap();
    for _ in 0..100 {
        clone.finish(Some("duplicate".into())).unwrap();
        assert_eq!(
            clone.send(HostMsg::InputDiscarded),
            Err(HostSendError::Closed)
        );
    }
    let capture = Capture::default();
    OutputWriter::spawn(capture.clone(), hello(Some(IPC_PROTOCOL_VERSION)), receiver)
        .unwrap()
        .finish();
    let messages = decoded(&capture);
    assert_eq!(messages.len(), 4);
    assert!(matches!(messages[0], HostMsg::InputDiscarded));
    assert!(matches!(&messages[1], HostMsg::ClipboardText(text) if text == "保留顺序"));
    assert!(matches!(&messages[2], HostMsg::Error(error) if error.len() <= FINAL_ERROR_BYTES));
    assert!(matches!(messages[3], HostMsg::Closed));
    assert_eq!(host.state.budget.lock().unwrap().messages, 0);
    assert!(host.final_result(Ok(())).is_err());
    assert!(matches!(
        host.final_result(Err(RdpError::ControlChannelClosed)),
        Err(RdpError::Session(error)) if error.contains("预算")
    ));
}

/// A closed output pipe is not a failure once the app has asked to close.
///
/// The app asks the helper to close, then stops reading the helper's stdout.
/// The writer's final flush therefore hits a broken pipe and aborts the
/// output — which used to be reported as the session's outcome, so the
/// helper exited non-zero on *every* ordinary disconnect.
///
/// The exit code is read by the app's disconnect path, by the acceptance
/// harness, and by the release gate. A normal close that reads as a crash
/// makes all three wrong.
#[test]
fn a_closed_output_pipe_does_not_fail_a_session_that_the_app_closed() {
    let (host, _receiver) = host_channel();
    host.note_app_closing();
    host.state.abort(HostSendError::Closed);
    assert!(
        host.final_result(Ok(())).is_ok(),
        "a session the app asked to close must not fail on the broken pipe"
    );
}

/// A pipe that broke while the session was still running is a real failure.
///
/// Forgiving a closed pipe must not swallow a delivery failure: the app is
/// still there, expecting frames, and the output stopped.
#[test]
fn a_closed_output_pipe_still_fails_a_session_the_app_did_not_close() {
    let (host, _receiver) = host_channel();
    host.state.abort(HostSendError::Closed);
    assert!(
        host.final_result(Ok(())).is_err(),
        "a pipe that broke mid-session must fail the session"
    );
}

/// The other output failures are real and must still fail the session.
///
/// Forgiving the closed pipe must not turn an output that was overloaded or
/// stalled into a success: those mean the helper could not deliver frames
/// the app needed.
#[test]
fn a_real_output_failure_still_fails_the_session() {
    let (host, _receiver) = host_channel();
    host.note_app_closing();
    host.state.abort(HostSendError::Overloaded);
    assert!(host.final_result(Ok(())).is_err());
    let (host, _receiver) = host_channel();
    host.note_app_closing();
    host.state.abort(HostSendError::Stalled);
    assert!(host.final_result(Ok(())).is_err());
}

#[test]
fn watchdog_counts_only_active_writes_and_resets_on_progress() {    let (host, _receiver) = host_channel();    let now = Instant::now();
    assert!(!host.state.stalled(now + Duration::from_secs(60)));
    host.state.begin_frame();
    assert!(host.state.stalled(now + Duration::from_secs(60)));
    host.state.made_progress();
    assert!(!host.state.stalled(Instant::now()));
    host.state.finish_frame();
    assert!(!host.state.stalled(now + Duration::from_secs(60)));
}

#[test]
fn byte_budget_counts_capacity_masks_and_nested_clipboard_allocations() {
    let messages = [
        HostMsg::MaskedTile {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            rgba: Vec::with_capacity(1024),
            painted: Vec::with_capacity(4096),
        },
        HostMsg::Pointer(PointerUpdate::Bitmap {
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: Vec::with_capacity(1024),
        }),
        HostMsg::EpochClipboardText {
            epoch: 4,
            text: String::with_capacity(4096),
        },
        HostMsg::EpochClipboardImage {
            epoch: 4,
            image: Vec::with_capacity(4096),
        },
        HostMsg::ClipboardFiles {
            generation: 7,
            files: vec![ClipFile {
                name: String::with_capacity(8192),
                size: 0,
                is_dir: false,
            }],
        },
        HostMsg::FileContents {
            generation: 7,
            stream_id: 3,
            data: Some(Vec::with_capacity(4096)),
        },
        HostMsg::DisplayControl {
            epoch: 4,
            state: adit_rdp_proto::DisplayControlState {
                sent_layout: Vec::with_capacity(16),
                error: Some(String::with_capacity(1024)),
                ..Default::default()
            },
        },
        HostMsg::MonitorLayoutResult {
            epoch: 4,
            request_id: 1,
            result: Err(String::with_capacity(1024)),
        },
        HostMsg::MonitorLayoutObserved {
            epoch: 4,
            monitors: Vec::with_capacity(16),
        },
    ];
    for message in messages {
        let bytes = message_bytes(&message);
        assert!(bytes > std::mem::size_of::<QueuedMessage>());
        let (host, _receiver) = channel_with_limits(limits(20, bytes - 1));
        assert_eq!(host.send(message), Err(HostSendError::Overloaded));
        assert_eq!(host.state.budget.lock().unwrap().bytes, 0);
    }
}

#[test]
fn in_flight_write_and_flush_retain_the_entire_payload_budget() {
    for on_flush in [false, true] {
        let message = HostMsg::ClipboardImage(vec![42; 64 * 1024]);
        let bytes = message_bytes(&message);
        let (host, receiver) = channel_with_limits(limits(10, bytes));
        host.send(message).unwrap();
        let (writer, entered, resume) = gate_writer(on_flush);
        let output =
            OutputWriter::spawn(writer, hello(Some(IPC_PROTOCOL_VERSION)), receiver).unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let budget = host.state.budget.lock().unwrap();
        assert_eq!((budget.messages, budget.bytes), (1, bytes));
        drop(budget);
        assert_eq!(
            host.send(HostMsg::InputDiscarded),
            Err(HostSendError::Overloaded)
        );
        host.finish(Some("overloaded".into())).unwrap();
        resume.send(()).unwrap();
        output.finish();
        let budget = host.state.budget.lock().unwrap();
        assert_eq!((budget.messages, budget.bytes), (0, 0));
    }
}

#[tokio::test]
async fn blocked_output_cancels_pending_session_and_has_bounded_finish() {
    let (host, receiver) = channel_with_limits(limits(4, 1024));
    host.send(HostMsg::ClipboardText("blocked".into())).unwrap();
    let (writer, entered, resume) = gate_writer(false);
    let output = OutputWriter::spawn(writer, hello(Some(IPC_PROTOCOL_VERSION)), receiver).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        host.supervise(std::future::pending()),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(RdpError::Session(error)) if error.contains("停滞")));
    let start = Instant::now();
    output.finish();
    assert!(start.elapsed() < Duration::from_secs(1));
    // An OS write cannot be recalled, but once it returns cancellation must
    // prevent the rest of this frame and every subsequent queued message.
    resume.send(()).unwrap();
}

struct SlowWriter {
    capture: Capture,
    delay: Duration,
}

impl Write for SlowWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        thread::sleep(self.delay);
        self.capture.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn slow_progress_is_not_a_total_frame_deadline() {
    let (host, receiver) = channel_with_limits(limits(10, 1024 * 1024));
    host.send(HostMsg::ClipboardImage(vec![3; 512 * 1024]))
        .unwrap();
    host.finish(None).unwrap();
    let capture = Capture::default();
    let output = OutputWriter::spawn(
        SlowWriter {
            capture: capture.clone(),
            delay: Duration::from_millis(3),
        },
        hello(Some(IPC_PROTOCOL_VERSION)),
        receiver,
    )
    .unwrap();
    host.supervise(async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        Ok(())
    })
    .await
    .unwrap();
    output.finish();
    assert!(host.final_result(Ok(())).is_ok());
    assert!(
        matches!(&decoded(&capture)[0], HostMsg::ClipboardImage(image) if image.len() == 512 * 1024)
    );
}

#[test]
fn continuous_progress_cannot_extend_shutdown_forever() {
    let (host, receiver) = channel_with_limits(Limits {
        drain: Duration::from_millis(180),
        ..limits(4, 1024 * 1024)
    });
    host.send(HostMsg::ClipboardImage(vec![1; 512 * 1024]))
        .unwrap();
    host.finish(None).unwrap();
    let output = OutputWriter::spawn(
        SlowWriter {
            capture: Capture::default(),
            delay: Duration::from_millis(5),
        },
        hello(Some(IPC_PROTOCOL_VERSION)),
        receiver,
    )
    .unwrap();
    let start = Instant::now();
    output.finish();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(host.final_result(Ok(())).is_err());
}

struct BrokenWriter {
    capture: Capture,
    fail_flush: bool,
}

impl Write for BrokenWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.fail_flush && self.capture.0.lock().unwrap().len() >= 24 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // Fail after accepting a prefix of the first framed message.
        self.capture.write(&bytes[..bytes.len().min(4)])
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush && self.capture.0.lock().unwrap().len() > 20 {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn write_or_flush_failure_cancels_idle_session_without_appending_closed() {
    for fail_flush in [false, true] {
        let (host, receiver) = host_channel();
        host.send(HostMsg::ClipboardText("prefix".into())).unwrap();
        host.finish(Some("must not append to a broken frame".into()))
            .unwrap();
        let capture = Capture::default();
        let output = OutputWriter::spawn(
            BrokenWriter {
                capture: capture.clone(),
                fail_flush,
            },
            hello(Some(IPC_PROTOCOL_VERSION)),
            receiver,
        )
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                host.supervise(std::future::pending())
            )
            .await
            .unwrap(),
            Err(RdpError::ControlChannelClosed)
        ));
        output.finish();
        let mut input = io::Cursor::new(capture.0.lock().unwrap().clone());
        read_ipc_hello(&mut input).unwrap();
        if fail_flush {
            assert!(matches!(
                read_msg::<_, HostMsg>(&mut input).unwrap(),
                Some(HostMsg::ClipboardText(_))
            ));
            assert!(read_msg::<_, HostMsg>(&mut input).unwrap().is_none());
        } else {
            assert!(read_msg::<_, HostMsg>(&mut input).is_err());
        }
        assert!(host.final_result(Ok(())).is_err());
        assert_eq!(host.state.budget.lock().unwrap().bytes, 0);
    }
}

#[test]
fn framed_pipe_preserves_large_frame_clipboard_and_terminal_order() {
    let (host, receiver) = host_channel();
    host.send(HostMsg::Tile {
        x: 0,
        y: 0,
        width: 3840,
        height: 2160,
        rgba: vec![42; 3840 * 2160 * 4],
    })
    .unwrap();
    host.send(HostMsg::EpochClipboardText {
        epoch: 5,
        text: "正常剪贴板".into(),
    })
    .unwrap();
    host.send(HostMsg::EpochClipboardImage {
        epoch: 5,
        image: vec![7; 1024 * 1024],
    })
    .unwrap();
    host.send(HostMsg::FileContents {
        generation: 8,
        stream_id: 9,
        data: Some(vec![6; 64 * 1024]),
    })
    .unwrap();
    host.finish(Some("session ended".into())).unwrap();
    let (mut reader, writer) = io::pipe().unwrap();
    let output = OutputWriter::spawn(writer, hello(Some(IPC_PROTOCOL_VERSION)), receiver).unwrap();
    assert_eq!(read_ipc_hello(&mut reader).unwrap(), IPC_PROTOCOL_VERSION);
    assert!(
        matches!(read_msg::<_, HostMsg>(&mut reader).unwrap(), Some(HostMsg::Tile { rgba, .. }) if rgba.len() == 3840 * 2160 * 4 && rgba[0] == 42)
    );
    assert!(
        matches!(read_msg::<_, HostMsg>(&mut reader).unwrap(), Some(HostMsg::EpochClipboardText { epoch: 5, text }) if text == "正常剪贴板")
    );
    assert!(
        matches!(read_msg::<_, HostMsg>(&mut reader).unwrap(), Some(HostMsg::EpochClipboardImage { epoch: 5, image }) if image.len() == 1024 * 1024)
    );
    assert!(
        matches!(read_msg::<_, HostMsg>(&mut reader).unwrap(), Some(HostMsg::FileContents { stream_id: 9, data: Some(data), .. }) if data.len() == 64 * 1024)
    );
    assert!(matches!(
        read_msg::<_, HostMsg>(&mut reader).unwrap(),
        Some(HostMsg::Error(_))
    ));
    assert!(matches!(
        read_msg::<_, HostMsg>(&mut reader).unwrap(),
        Some(HostMsg::Closed)
    ));
    output.finish();
    assert!(read_msg::<_, HostMsg>(&mut reader).unwrap().is_none());
    assert!(host.final_result(Ok(())).is_ok());
}

#[test]
fn maximum_desktop_and_paint_mask_fit_the_default_budget() {
    let rgba = Vec::with_capacity(8192 * 8192 * 4);
    let painted = Vec::with_capacity(8192 * 8192 / 8);
    let (host, receiver) = host_channel();
    host.send(HostMsg::MaskedTile {
        x: 0,
        y: 0,
        width: 8192,
        height: 8192,
        rgba,
        painted,
    })
    .unwrap();
    host.send(HostMsg::EpochClipboardImage {
        epoch: 0,
        image: Vec::with_capacity(8 * 1024 * 1024),
    })
    .unwrap();
    assert!(host.state.budget.lock().unwrap().bytes < Limits::default().bytes);
    drop(receiver);
    assert_eq!(host.state.budget.lock().unwrap().bytes, 0);
}

#[test]
fn hello_compatibility_never_encodes_host_enums_for_an_incompatible_peer() {
    for version in [None, Some(IPC_PROTOCOL_VERSION - 1)] {
        let (host, receiver) = host_channel();
        host.finish(Some("not for this peer".into())).unwrap();
        let capture = Capture::default();
        OutputWriter::spawn(capture.clone(), hello(version), receiver)
            .unwrap()
            .finish();
        let bytes = capture.0.lock().unwrap();
        if version.is_none() {
            assert!(bytes.is_empty());
        } else {
            let mut input = io::Cursor::new(&*bytes);
            assert_eq!(read_ipc_hello(&mut input).unwrap(), IPC_PROTOCOL_VERSION);
            assert!(read_msg::<_, HostMsg>(&mut input).unwrap().is_none());
        }
    }
}

#[test]
fn framed_key_release_and_close_are_read_while_stdout_is_blocked() {
    let (host, receiver) = host_channel();
    host.send(HostMsg::ClipboardText("blocked".into())).unwrap();
    let (writer, entered, resume) = gate_writer(false);
    let output = OutputWriter::spawn(writer, hello(Some(IPC_PROTOCOL_VERSION)), receiver).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let (reader, mut app) = io::pipe().unwrap();
    let (request_tx, _) = mpsc::channel();
    let (input_tx, mut input) = tokio::sync::mpsc::unbounded_channel();
    let (display_tx, _display) = tokio::sync::mpsc::unbounded_channel();
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        crate::host::read_app_messages(
            reader,
            request_tx,
            input_tx,
            display_tx,
            Arc::new(crate::session::InputRouting::default()),
            crate::clipboard::new_shared(),
        );
        done.send(()).unwrap();
    });
    for pressed in [true, false] {
        write_msg(
            &mut app,
            &ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed,
                },
            },
        )
        .unwrap();
    }
    write_msg(&mut app, &ClientMsg::Close).unwrap();
    finished.recv_timeout(Duration::from_secs(2)).unwrap();
    for expected in [true, false] {
        assert!(
            matches!(input.try_recv().unwrap(), crate::session::SessionCmd::Input { event: InputEvent::Key { pressed, .. }, .. } if pressed == expected)
        );
    }
    assert!(input.is_closed());
    host.state.abort(HostSendError::Stalled);
    output.finish();
    resume.send(()).unwrap();
}
