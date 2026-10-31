//! End-to-end IPC smoke test against the real `adit-rdp-host` helper.
//!
//! Requires the helper to be built (it lives in a separate workspace) and its
//! path in `ADIT_RDP_HOST`. Run:
//!   cargo build --manifest-path crates/adit-rdp/Cargo.toml --release
//!   ADIT_RDP_HOST=crates/adit-rdp/target/release/adit-rdp-host.exe \
//!     cargo test -p adit-rdp-proto --test helper_ipc -- --ignored --nocapture

use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use adit_rdp_proto::{
    read_ipc_hello, read_msg, write_ipc_hello, write_msg, ClientMsg, ConnectRequest, HostMsg,
    IPC_PROTOCOL_VERSION,
};

struct ChildGuard(Child);

impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn receive_hello(mut stdout: ChildStdout) -> (ChildStdout, u32) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = read_ipc_hello(&mut stdout);
        let _ = tx.send((stdout, result));
    });
    let (stdout, version) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("helper hello timed out");
    (stdout, version.expect("invalid helper hello"))
}

#[test]
#[ignore = "needs the helper binary; see the module docs"]
fn helper_reports_error_on_unreachable_host() {
    let helper = std::env::var("ADIT_RDP_HOST").expect("set ADIT_RDP_HOST to the helper exe path");

    let mut child = ChildGuard(
        Command::new(&helper)
            .env_remove("ADIT_RDP_DUMP")
            .env(
                "APPDATA",
                std::env::temp_dir().join(format!("adit-ipc-test-{}", std::process::id())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn helper"),
    );

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    write_ipc_hello(&mut stdin, IPC_PROTOCOL_VERSION).expect("send IPC hello");
    let (mut stdout, version) = receive_hello(stdout);
    assert_eq!(version, IPC_PROTOCOL_VERSION);

    // A closed local port ⇒ TCP connect refused ⇒ fast, deterministic failure.
    let request = ConnectRequest {
        host: "127.0.0.1".into(),
        port: 9,
        username: "tester".into(),
        password: "secret".into(),
        domain: None,
        width: 1024,
        height: 768,
        enable_clipboard: false,
        enable_audio: false,
        quality: adit_rdp_proto::Quality::default(),
    };
    write_msg(&mut stdin, &ClientMsg::Connect(request)).expect("send Connect");

    let mut saw_error = false;
    let mut saw_closed = false;
    for _ in 0..20 {
        match read_msg::<_, HostMsg>(&mut stdout) {
            Ok(Some(HostMsg::Connected { .. })) => panic!("unexpected Connected to a dead host"),
            Ok(Some(HostMsg::Error(message))) => {
                eprintln!("helper reported error: {message}");
                saw_error = true;
            }
            Ok(Some(HostMsg::Closed)) => {
                saw_closed = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let _ = child.kill();
    // Reap the child so it doesn't linger as a zombie (kill alone doesn't wait).
    let _ = child.wait();

    assert!(saw_error, "helper should report a connection error");
    assert!(saw_closed, "helper should close the session");
}

#[test]
#[ignore = "needs the helper binary; see the module docs"]
fn helper_rejects_legacy_and_mismatched_apps_without_opening_a_connection() {
    let helper = std::env::var_os("ADIT_RDP_HOST").expect("helper path");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for version in [None, Some(8), Some(9), Some(11)] {
        let mut child = ChildGuard(
            Command::new(&helper)
                .env_remove("ADIT_RDP_DUMP")
                .env(
                    "APPDATA",
                    std::env::temp_dir().join(format!("adit-ipc-test-{}", std::process::id())),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut bytes = Vec::new();
        if let Some(version) = version {
            write_ipc_hello(&mut bytes, version).unwrap();
        }
        write_msg(
            &mut bytes,
            &ClientMsg::Connect(ConnectRequest {
                host: "127.0.0.1".into(),
                port,
                username: "synthetic-user".into(),
                password: "synthetic-password".into(),
                domain: None,
                width: 2,
                height: 2,
                enable_clipboard: true,
                enable_audio: false,
                quality: adit_rdp_proto::Quality::default(),
            }),
        )
        .unwrap();
        stdin.write_all(&bytes).unwrap();
        drop(stdin);
        if version.is_some() {
            let (_, hello_version) = receive_hello(stdout);
            assert_eq!(hello_version, IPC_PROTOCOL_VERSION);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("incompatible helper did not exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "incompatible app must not start an RDP connection"
        );
    }
}
