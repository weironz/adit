//! Exercise the shipped helper entry point with an actual unread stdout pipe.
//! No synthetic helper mode, alternate IPC schema, or releasable writer gate.

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adit_rdp_proto::{
    read_ipc_hello, read_msg, write_ipc_hello, write_msg, ClientMsg, ConnectRequest, HostMsg,
    InputEvent, Quality, IPC_PROTOCOL_VERSION,
};

#[test]
fn unread_stdout_cannot_keep_the_real_helper_process_alive() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let log_dir = std::env::temp_dir().join(format!(
        "adit-helper-backpressure-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_adit-rdp-host"))
        .env("APPDATA", &log_dir)
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    write_ipc_hello(&mut input, IPC_PROTOCOL_VERSION).unwrap();
    write_msg(
        &mut input,
        &ClientMsg::Connect(ConnectRequest {
            host: "127.0.0.1".into(),
            port,
            username: "fixture".into(),
            password: String::new(),
            domain: None,
            width: 800,
            height: 600,
            enable_clipboard: true,
            enable_audio: false,
            quality: Quality::High,
        }),
    )
    .unwrap();

    // Hold a real TCP connection before sending the inputs. The RDP handshake
    // intentionally stays pending, so a connection failure cannot fake success.
    let accepted_deadline = Instant::now() + Duration::from_secs(5);
    let peer = loop {
        match listener.accept() {
            Ok((peer, _)) => break peer,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= accepted_deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("helper did not start its real RDP connection");
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    };
    let (sent, finished) = mpsc::channel();
    let feeder = thread::spawn(move || {
        let mut frame = Vec::new();
        write_msg(
            &mut frame,
            &ClientMsg::EpochInput {
                epoch: 1,
                event: InputEvent::Key {
                    scancode: 30,
                    extended: false,
                    pressed: false,
                },
            },
        )
        .unwrap();
        // A stale keyboard event produces InputDiscarded during connection.
        // This is finite, but much larger than OS pipe capacity plus the helper
        // message budget. The feeder may itself block until the helper exits.
        let mut count = 0;
        'feed: for _ in 0..4096 {
            for _ in 0..64 {
                if input.write_all(&frame).is_err() {
                    break 'feed;
                }
                count += 1;
            }
            // Let a healthy writer drain each small burst. An unpaced flood
            // can exhaust the message budget before stdout actually blocks,
            // then exit normally without testing the OS-write shutdown path.
            thread::sleep(Duration::from_millis(5));
        }
        let _ = sent.send(count);
    });

    // Deliberately do not take or read child.stdout anywhere in this interval.
    let start = Instant::now();
    let deadline = start + Duration::from_secs(25);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            drop(peer);
            let _ = feeder.join();
            panic!("unread stdout prevented helper process exit (including stdio cleanup)");
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(peer);
    let count = finished.recv_timeout(Duration::from_secs(2)).unwrap();
    feeder.join().unwrap();
    assert!(
        !status.success(),
        "output overload/stall must fail the helper"
    );
    assert!(
        count > 2048,
        "fixture never exerted output backpressure: {count}"
    );
    assert!(
        start.elapsed() >= Duration::from_secs(9),
        "helper exited before exercising the 10s blocked-output deadline: {:?}",
        start.elapsed()
    );

    // Inspect evidence only after exit. The original process could not have
    // drained this pipe or obtained an exit-time StdoutLock from the parent.
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let mut output = io::Cursor::new(bytes);
    assert_eq!(read_ipc_hello(&mut output).unwrap(), IPC_PROTOCOL_VERSION);
    assert!(matches!(
        read_msg::<_, HostMsg>(&mut output).unwrap(),
        Some(HostMsg::InputDiscarded)
    ));
    let mut error = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(
        error.contains("adit-rdp-host:"),
        "missing helper exit diagnostic: {error}"
    );
    eprintln!(
        "real helper exited after {:?}; submitted {count} framed inputs",
        start.elapsed()
    );

    let _ = std::fs::remove_dir_all(log_dir);
}
