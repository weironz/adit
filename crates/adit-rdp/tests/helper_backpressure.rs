//! Exercise the real helper pipes against a synthetic, stalled RDP handshake.

use std::io;
#[cfg(debug_assertions)]
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adit_rdp_proto::{
    read_helper_hello, read_msg, write_client_hello, write_msg, ClientMsg, ConnectRequest, HostMsg,
    SharedClipboardPolicy, IPC_PROTOCOL_VERSION,
};

const TIMEOUT: Duration = Duration::from_secs(15);

struct Helper {
    _child: ChildGuard,
    appdata: PathBuf,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    // Keep the synthetic server alive without completing RDP negotiation.
    _remote: TcpStream,
    _policy: SharedClipboardPolicy,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_helper() -> Helper {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let appdata = std::env::temp_dir().join(format!(
        "adit-helper-backpressure-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_adit-rdp-host"));
    let policy = SharedClipboardPolicy::create(0, true).unwrap();
    command
        .env("APPDATA", &appdata)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    policy.configure_child(&mut command).unwrap();
    let mut child = ChildGuard(command.spawn().unwrap());
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = child.0.stdout.take().unwrap();
    write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION).unwrap();
    let (hello_tx, hello_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = read_helper_hello(&mut stdout);
        let _ = hello_tx.send((result, stdout));
    });
    let (hello, stdout) = hello_rx
        .recv_timeout(TIMEOUT)
        .expect("helper hello timed out");
    hello.unwrap();
    write_msg(
        &mut stdin,
        &ClientMsg::Connect(ConnectRequest {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            username: "synthetic".into(),
            password: "synthetic".into(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: true,
            enable_audio: false,
            quality: Default::default(),
            clipboard_policy_id: Some(policy.id().to_owned()),
        }),
    )
    .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let remote = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!(
                "synthetic RDP connection failed: {error}; helper status: {:?}; log: {}",
                child.0.try_wait(),
                std::fs::read_to_string(appdata.join("Adit").join("rdp-helper.log"))
                    .unwrap_or_default()
            ),
        }
    };
    Helper {
        _child: child,
        appdata,
        stdin: Some(stdin),
        stdout: Some(stdout),
        _remote: remote,
        _policy: policy,
    }
}

fn read_messages(mut stdout: ChildStdout) -> mpsc::Receiver<io::Result<Option<HostMsg>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || loop {
        let result = read_msg(&mut stdout);
        let done = !matches!(result, Ok(Some(_)));
        if sender.send(result).is_err() || done {
            break;
        }
    });
    receiver
}

fn request(stream_id: u32) -> ClientMsg {
    ClientMsg::RequestFileContents {
        generation: 1,
        stream_id,
        index: 0,
        offset: 0,
        length: 1,
    }
}

fn expect_disabled_notice(events: &mpsc::Receiver<io::Result<Option<HostMsg>>>) {
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::LocalFileDirectionSuspended { .. })
    ));
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::ClipboardFileDirectionUnavailable { message, .. })
            if message.contains("暂时禁用") && !message.contains("重新连接")
    ));
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::ClipboardPolicyApplied {
            policy_epoch: 0,
            enabled: true
        })
    ));
}

#[test]
fn real_helper_refuses_handshake_time_requests_and_closes() {
    let mut helper = spawn_helper();
    let events = read_messages(helper.stdout.take().unwrap());
    expect_disabled_notice(&events);
    let stdin = helper.stdin.as_mut().unwrap();
    for stream_id in 0..32 {
        write_msg(stdin, &request(stream_id)).unwrap();
    }
    for stream_id in 0..32 {
        assert!(matches!(events.recv_timeout(TIMEOUT).unwrap().unwrap(),
            Some(HostMsg::FileContents { generation: 1, stream_id: got, data: None }) if got == stream_id));
    }
    write_msg(stdin, &ClientMsg::Close).unwrap();
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Error(_))
    ));
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Closed)
    ));
}

#[test]
fn real_helper_discards_local_file_messages_without_closing() {
    let mut helper = spawn_helper();
    let events = read_messages(helper.stdout.take().unwrap());
    expect_disabled_notice(&events);
    write_msg(
        helper.stdin.as_mut().unwrap(),
        &ClientMsg::FileContents {
            generation: 7,
            stream_id: 41,
            data: Some(vec![1]),
        },
    )
    .unwrap();
    write_msg(
        helper.stdin.as_mut().unwrap(),
        &ClientMsg::BoundFileContents {
            request: adit_rdp_proto::ClipboardFileRequestId {
                channel_epoch: 1,
                snapshot_revision: 1,
                generation: 7,
                stream_id: 41,
            },
            data: Some(vec![3, 4, 5]),
        },
    )
    .unwrap();
    write_msg(
        helper.stdin.as_mut().unwrap(),
        &ClientMsg::ClipboardFiles {
            generation: 7,
            files: vec![adit_rdp_proto::ClipFile {
                name: "synthetic-private.bin".into(),
                size: 3,
                is_dir: false,
            }],
        },
    )
    .unwrap();
    write_msg(
        helper.stdin.as_mut().unwrap(),
        &ClientMsg::ClipboardFiles {
            generation: 0,
            files: Vec::new(),
        },
    )
    .unwrap();
    write_msg(
        helper.stdin.as_mut().unwrap(),
        &ClientMsg::LocalFileOfferRejected,
    )
    .unwrap();
    write_msg(helper.stdin.as_mut().unwrap(), &request(42)).unwrap();
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::FileContents {
            stream_id: 42,
            data: None,
            ..
        })
    ));
    assert!(helper._child.0.try_wait().unwrap().is_none());
    write_msg(helper.stdin.as_mut().unwrap(), &ClientMsg::Close).unwrap();
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Error(_))
    ));
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Closed)
    ));
}

#[test]
fn real_helper_flood_ends_with_error_and_does_not_consume_another_sessions_budget() {
    let mut flooded = spawn_helper();
    let mut other = spawn_helper();
    let mut stdin = flooded.stdin.take().unwrap();
    let (finished_tx, finished_rx) = mpsc::channel();
    thread::spawn(move || {
        for stream_id in 0..100_000 {
            if write_msg(&mut stdin, &request(stream_id)).is_err() {
                break;
            }
        }
        let _ = finished_tx.send(());
    });
    // The pipe writer may block while stdout is deliberately unread. Wait for
    // the helper's own overload signal before draining, then it can report the
    // error and exit, releasing the stalled writer as well.
    let log = flooded.appdata.join("Adit").join("rdp-helper.log");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("RDP helper input queue exceeded its limit")
            || text.contains("RDP helper output queue exceeded its limit")
            || text.contains("RDP helper stdout made no write progress before its deadline")
        {
            break;
        }
        assert!(Instant::now() < deadline, "helper did not signal overload");
        thread::sleep(Duration::from_millis(10));
    }

    let events = read_messages(flooded.stdout.take().unwrap());
    expect_disabled_notice(&events);
    let mut saw_error = false;
    let mut saw_closed = false;
    for _ in 0..10_002 {
        match events
            .recv_timeout(TIMEOUT)
            .expect("helper did not report overload")
            .unwrap()
        {
            Some(HostMsg::FileContents { data: None, .. }) => {}
            Some(HostMsg::Error(message)) => {
                assert!(
                    message.contains("队列") || message.contains("输出停滞"),
                    "unexpected helper error: {message}"
                );
                saw_error = true;
            }
            Some(HostMsg::Closed) => {
                saw_closed = true;
                break;
            }
            other => panic!("unexpected helper message: {other:?}"),
        }
    }
    assert!(saw_error && saw_closed);
    finished_rx
        .recv_timeout(TIMEOUT)
        .expect("flooding writer did not finish after close");

    let other_events = read_messages(other.stdout.take().unwrap());
    expect_disabled_notice(&other_events);
    let other_stdin = other.stdin.as_mut().unwrap();
    write_msg(other_stdin, &request(99)).unwrap();
    assert!(matches!(
        other_events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::FileContents {
            stream_id: 99,
            data: None,
            ..
        })
    ));
    write_msg(other_stdin, &ClientMsg::Close).unwrap();
    assert!(matches!(
        other_events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Error(_))
    ));
    assert!(matches!(
        other_events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Closed)
    ));
}

#[test]
fn real_helper_exits_when_stdout_stays_open_and_is_never_drained() {
    let mut helper = spawn_helper();
    // Keep stdout open after the hello, but never read another byte. Pace the
    // requests so the session can generate replies until the OS pipe fills.
    let mut stdin = helper.stdin.take().unwrap();
    let (finished_tx, finished_rx) = mpsc::channel();
    thread::spawn(move || {
        for stream_id in 0..100_000 {
            if write_msg(&mut stdin, &request(stream_id)).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = finished_tx.send(());
    });
    let log = helper.appdata.join("Adit").join("rdp-helper.log");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("RDP helper output queue exceeded its limit")
            || text.contains("RDP helper input queue exceeded its limit")
            || text.contains("RDP helper stdout made no write progress before its deadline")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stdout did not reach the output limit"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = helper._child.0.try_wait().unwrap() {
            assert!(!status.success(), "sustained overload must be reported");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "helper is stuck on its stdout writer"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        helper.stdout.is_some(),
        "stdout must remain open and unread"
    );
    finished_rx
        .recv_timeout(TIMEOUT)
        .expect("stdin writer did not finish after helper exit");
}

#[cfg(debug_assertions)]
fn spawn_stdout_fixture(blocking_task: bool) -> (ChildGuard, ChildStdin, ChildStdout, PathBuf) {
    spawn_stdout_fixture_with_policy(blocking_task, false)
}

#[cfg(debug_assertions)]
fn spawn_stdout_fixture_with_policy(
    blocking_task: bool,
    invalid_policy: bool,
) -> (ChildGuard, ChildStdin, ChildStdout, PathBuf) {
    let appdata = std::env::temp_dir().join(format!(
        "adit-single-frame-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_adit-rdp-host"));
    command
        .env("APPDATA", &appdata)
        .env("ADIT_RDP_TEST_STDOUT_FIXTURE", "enabled")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if blocking_task {
        command.env("ADIT_RDP_TEST_BLOCKING_TASK", "enabled");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = child.0.stdout.take().unwrap();
    write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION).unwrap();
    let (hello_tx, hello_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = read_helper_hello(&mut stdout);
        let _ = hello_tx.send((result, stdout));
    });
    let (hello, stdout) = hello_rx
        .recv_timeout(TIMEOUT)
        .expect("helper hello timed out");
    hello.unwrap();
    write_msg(
        &mut stdin,
        &ClientMsg::Connect(ConnectRequest {
            host: "adit-stdout-fixture.invalid".into(),
            port: 0,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 512,
            height: 512,
            enable_clipboard: invalid_policy,
            enable_audio: false,
            quality: Default::default(),
            clipboard_policy_id: invalid_policy.then(|| "invalid-policy-id".into()),
        }),
    )
    .unwrap();
    (child, stdin, stdout, appdata)
}

#[cfg(debug_assertions)]
#[test]
fn invalid_policy_mapping_rejects_clipboard_but_keeps_desktop_and_input_alive() {
    let (mut child, mut stdin, mut stdout, _appdata) =
        spawn_stdout_fixture_with_policy(false, true);
    let (events, received) = mpsc::channel();
    thread::spawn(move || loop {
        let message = read_msg::<_, HostMsg>(&mut stdout);
        let end = !matches!(&message, Ok(Some(_)));
        if events.send(message).is_err() || end {
            break;
        }
    });
    assert!(matches!(
        received.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::ClipboardPolicyRejected)
    ));
    assert!(matches!(
        received.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Tile { .. })
    ));
    for pressed in [true, false] {
        write_msg(
            &mut stdin,
            &ClientMsg::Input(adit_rdp_proto::InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed,
            }),
        )
        .unwrap();
    }
    write_msg(
        &mut stdin,
        &ClientMsg::RequestFileContents {
            generation: 7,
            stream_id: 9,
            index: 0,
            offset: 0,
            length: 1,
        },
    )
    .unwrap();
    assert!(
        matches!(received.recv_timeout(TIMEOUT).unwrap().unwrap(), Some(HostMsg::FileContents { generation: 7, stream_id: 9, data: Some(keys) }) if keys == [1, 0])
    );
    write_msg(&mut stdin, &ClientMsg::Close).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while child.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "helper did not close after synthetic test"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(debug_assertions)]
#[test]
fn real_helper_keeps_new_copy_before_confirmation_during_a_partial_input_frame() {
    use adit_rdp_proto::InputEvent;
    use std::io::Write;
    let root = std::env::temp_dir().join(format!(
        "adit-policy-helper-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let signal = root.join("synthetic-copy.signal");
    let policy = SharedClipboardPolicy::create(0, true).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_adit-rdp-host"));
    command
        .env("APPDATA", &root)
        .env("ADIT_RDP_TEST_STDOUT_FIXTURE", "enabled")
        .env("ADIT_RDP_TEST_POLICY_SIGNAL", &signal)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    policy.configure_child(&mut command).unwrap();
    let mut child = ChildGuard(command.spawn().unwrap());
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = child.0.stdout.take().unwrap();
    write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION).unwrap();
    let (hello_tx, hello_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = read_helper_hello(&mut stdout);
        let _ = hello_tx.send((result, stdout));
    });
    let (hello, mut stdout) = hello_rx
        .recv_timeout(TIMEOUT)
        .expect("helper hello timed out");
    hello.unwrap();
    write_msg(
        &mut stdin,
        &ClientMsg::Connect(ConnectRequest {
            host: "adit-stdout-fixture.invalid".into(),
            port: 0,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 512,
            height: 512,
            enable_clipboard: true,
            enable_audio: false,
            quality: Default::default(),
            clipboard_policy_id: Some(policy.id().to_owned()),
        }),
    )
    .unwrap();
    let (events, received) = mpsc::channel();
    thread::spawn(move || loop {
        let message = read_msg::<_, HostMsg>(&mut stdout);
        let end = !matches!(&message, Ok(Some(_)));
        if events.send(message).is_err() || end {
            break;
        }
    });
    assert!(matches!(
        received.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::ClipboardPolicyApplied {
            policy_epoch: 0,
            enabled: true
        })
    ));
    assert!(matches!(
        received.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Tile { .. })
    ));
    assert!(matches!(
        received.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::ClipboardChannelReady { channel_epoch: 1 })
    ));
    let mut image_frame = Vec::new();
    write_msg(&mut image_frame, &ClientMsg::ClipboardImage(vec![3; 1024])).unwrap();
    stdin.write_all(&image_frame[..8]).unwrap();
    stdin.flush().unwrap();
    policy.store(1, false).unwrap();
    policy.store(2, true).unwrap();
    std::fs::write(&signal, b"synthetic").unwrap();
    assert!(matches!(received.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::ClipboardFilesForPolicy { policy_epoch: 2, generation: 8, files }) if files[0].name == "synthetic-new.bin"));
    // The helper stdin reader cannot acknowledge the new policy until the
    // already-started image frame is complete.
    assert!(received.try_recv().is_err());
    stdin.write_all(&image_frame[8..]).unwrap();
    write_msg(&mut stdin, &ClientMsg::ClipboardPolicyEpoch(2)).unwrap();
    for pressed in [true, false] {
        write_msg(
            &mut stdin,
            &ClientMsg::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed,
            }),
        )
        .unwrap();
    }
    write_msg(
        &mut stdin,
        &ClientMsg::RequestFileContents {
            generation: 8,
            stream_id: 91,
            index: 0,
            offset: 0,
            length: 1,
        },
    )
    .unwrap();
    let mut ack = false;
    let mut keys = false;
    while !ack || !keys {
        match received.recv_timeout(TIMEOUT).unwrap().unwrap().unwrap() {
            HostMsg::ClipboardPolicyApplied {
                policy_epoch: 2,
                enabled: true,
            } => ack = true,
            HostMsg::FileContents {
                generation: 8,
                stream_id: 91,
                data: Some(edges),
            } => {
                assert_eq!(edges, [1, 0]);
                keys = true;
            }
            HostMsg::Error(message) => panic!("synthetic helper failed: {message}"),
            _ => {}
        }
    }
    write_msg(&mut stdin, &ClientMsg::Close).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "helper did not close");
        thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(signal);
    let _ = std::fs::remove_file(root.join("Adit").join("rdp-helper.log"));
    let _ = std::fs::remove_dir(root.join("Adit"));
    let _ = std::fs::remove_dir(root);
}

#[cfg(debug_assertions)]
#[test]
fn single_blocked_frame_exits_without_a_second_message_or_queue_overflow() {
    let (mut child, _stdin, stdout, appdata) = spawn_stdout_fixture(true);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "helper did not stop on a stalled single frame"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // The handle has remained open and unread since the version handshake.
    drop(stdout);
    let log = std::fs::read_to_string(appdata.join("Adit").join("rdp-helper.log")).unwrap();
    assert!(log.contains("RDP helper stdout made no write progress before its deadline"));
    assert!(!log.contains("RDP helper output queue exceeded its limit"));
}

#[cfg(debug_assertions)]
#[test]
fn slow_reader_keeps_the_whole_frame_and_following_input_edges_in_order() {
    use adit_rdp_proto::InputEvent;
    let (mut child, mut stdin, stdout, _appdata) = spawn_stdout_fixture(false);
    for pressed in [true, false] {
        write_msg(
            &mut stdin,
            &ClientMsg::Input(InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed,
            }),
        )
        .unwrap();
    }
    // Fill the separate 128-message file budget behind the frame. The next
    // response must wait for the slow but advancing writer, not time out at 2 s.
    for stream_id in 0..129 {
        write_msg(&mut stdin, &request(stream_id)).unwrap();
    }
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdout = stdout;
        let result = (|| -> io::Result<(HostMsg, Vec<HostMsg>, ChildStdout, Duration)> {
            let started = Instant::now();
            let mut header = [0; 4];
            stdout.read_exact(&mut header)?;
            let length = u32::from_le_bytes(header) as usize;
            if length > adit_rdp_proto::MAX_MESSAGE_BYTES {
                return Err(io::Error::other("fixture frame too large"));
            }
            let mut encoded = vec![0; length + 4];
            encoded[..4].copy_from_slice(&header);
            for chunk in encoded[4..].chunks_mut(4096) {
                stdout.read_exact(chunk)?;
                thread::sleep(Duration::from_millis(45));
            }
            let frame = read_msg::<_, HostMsg>(&mut io::Cursor::new(encoded))?.unwrap();
            let mut replies = Vec::new();
            for _ in 0..129 {
                replies.push(read_msg::<_, HostMsg>(&mut stdout)?.unwrap());
            }
            Ok((frame, replies, stdout, started.elapsed()))
        })();
        let _ = done_tx.send(result);
    });
    let (frame, replies, stdout, elapsed) = done_rx
        .recv_timeout(Duration::from_secs(25))
        .expect("slow reader did not finish")
        .expect("slow reader failed");
    assert!(elapsed > Duration::from_secs(10));
    assert!(
        matches!(frame, HostMsg::Tile { x: 0, y: 0, width: 512, height: 512, rgba }
        if rgba.len() == 512 * 512 * 4 && rgba.iter().all(|byte| *byte == 0x5a))
    );
    for (stream_id, reply) in replies.into_iter().enumerate() {
        assert!(
            matches!(reply, HostMsg::FileContents { stream_id: got, data: Some(data), .. }
            if got == stream_id as u32 && data == [1, 0])
        );
    }
    assert!(child.0.try_wait().unwrap().is_none());
    let events = read_messages(stdout);
    write_msg(&mut stdin, &ClientMsg::Close).unwrap();
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Closed)
    ));
}

#[cfg(debug_assertions)]
#[test]
fn closing_during_a_slow_frame_still_finishes_that_frame() {
    let (_child, mut stdin, stdout, _appdata) = spawn_stdout_fixture(false);
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdout = stdout;
        let result = (|| -> io::Result<(HostMsg, HostMsg, Duration)> {
            let started = Instant::now();
            let mut header = [0; 4];
            stdout.read_exact(&mut header)?;
            let length = u32::from_le_bytes(header) as usize;
            if length > adit_rdp_proto::MAX_MESSAGE_BYTES {
                return Err(io::Error::other("fixture frame too large"));
            }
            let mut encoded = vec![0; length + 4];
            encoded[..4].copy_from_slice(&header);
            let _ = started_tx.send(());
            for chunk in encoded[4..].chunks_mut(4096) {
                stdout.read_exact(chunk)?;
                thread::sleep(Duration::from_millis(15));
            }
            let frame = read_msg::<_, HostMsg>(&mut io::Cursor::new(encoded))?.unwrap();
            let closed = read_msg::<_, HostMsg>(&mut stdout)?.unwrap();
            Ok((frame, closed, started.elapsed()))
        })();
        let _ = done_tx.send(result);
    });
    started_rx
        .recv_timeout(TIMEOUT)
        .expect("frame did not start");
    write_msg(&mut stdin, &ClientMsg::Close).unwrap();
    let (frame, closed, elapsed) = done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("slow frame did not finish after close")
        .expect("slow frame was truncated");
    assert!(elapsed > Duration::from_secs(2));
    assert!(
        matches!(frame, HostMsg::Tile { width: 512, height: 512, rgba, .. }
        if rgba.len() == 512 * 512 * 4 && rgba.iter().all(|byte| *byte == 0x5a))
    );
    assert!(matches!(closed, HostMsg::Closed));
}

#[cfg(debug_assertions)]
#[test]
fn input_flood_waits_for_a_slow_but_advancing_stdout() {
    use adit_rdp_proto::InputEvent;

    const REQUESTS: u32 = 1_200;
    let (mut child, mut stdin, stdout, _appdata) = spawn_stdout_fixture(false);
    let (sent_tx, sent_rx) = mpsc::channel();
    thread::spawn(move || {
        let send_edge = |stdin: &mut ChildStdin, pressed| {
            write_msg(
                stdin,
                &ClientMsg::Input(InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed,
                }),
            )
        };
        let result = (|| -> io::Result<ChildStdin> {
            send_edge(&mut stdin, true)?;
            send_edge(&mut stdin, false)?;
            for id in 0..REQUESTS {
                write_msg(&mut stdin, &request(id))?;
            }
            send_edge(&mut stdin, true)?;
            send_edge(&mut stdin, false)?;
            write_msg(&mut stdin, &request(REQUESTS))?;
            Ok(stdin)
        })();
        let _ = sent_tx.send(result);
    });

    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdout = stdout;
        let result = (|| -> io::Result<(HostMsg, Vec<HostMsg>, ChildStdout, Duration)> {
            let started = Instant::now();
            let mut header = [0; 4];
            stdout.read_exact(&mut header)?;
            let length = u32::from_le_bytes(header) as usize;
            if length > adit_rdp_proto::MAX_MESSAGE_BYTES {
                return Err(io::Error::other("fixture frame too large"));
            }
            let mut encoded = vec![0; length + 4];
            encoded[..4].copy_from_slice(&header);
            for chunk in encoded[4..].chunks_mut(4096) {
                stdout.read_exact(chunk)?;
                thread::sleep(Duration::from_millis(15));
            }
            let frame = read_msg::<_, HostMsg>(&mut io::Cursor::new(encoded))?.unwrap();
            let mut replies = Vec::new();
            for _ in 0..=REQUESTS {
                replies.push(read_msg::<_, HostMsg>(&mut stdout)?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "helper closed during flood")
                })?);
            }
            Ok((frame, replies, stdout, started.elapsed()))
        })();
        let _ = done_tx.send(result);
    });

    let (frame, replies, stdout, elapsed) = done_rx
        .recv_timeout(TIMEOUT)
        .expect("slow reader did not finish")
        .expect("helper closed during slow progress");
    assert!(elapsed > Duration::from_secs(2));
    assert!(
        matches!(frame, HostMsg::Tile { width: 512, height: 512, rgba, .. }
        if rgba.len() == 512 * 512 * 4 && rgba.iter().all(|byte| *byte == 0x5a))
    );
    for (id, reply) in replies.into_iter().enumerate() {
        let expected_edges: &[u8] = if id == REQUESTS as usize {
            &[1, 0, 1, 0]
        } else {
            &[1, 0]
        };
        assert!(
            matches!(reply, HostMsg::FileContents { stream_id, data: Some(data), .. }
            if stream_id == id as u32 && data == expected_edges)
        );
    }
    assert!(child.0.try_wait().unwrap().is_none());
    let mut stdin = sent_rx.recv_timeout(TIMEOUT).unwrap().unwrap();
    let events = read_messages(stdout);
    write_msg(&mut stdin, &ClientMsg::Close).unwrap();
    assert!(matches!(
        events.recv_timeout(TIMEOUT).unwrap().unwrap(),
        Some(HostMsg::Closed)
    ));
}

#[cfg(debug_assertions)]
#[test]
fn broken_stdout_ends_an_otherwise_quiet_session() {
    let (mut child, _stdin, stdout, _appdata) = spawn_stdout_fixture(false);
    drop(stdout);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "helper stayed alive after stdout failed"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
