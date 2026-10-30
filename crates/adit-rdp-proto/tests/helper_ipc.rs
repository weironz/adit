//! End-to-end IPC smoke test against the real `adit-rdp-host` helper.
//!
//! Requires the helper to be built (it lives in a separate workspace) and its
//! path in `ADIT_RDP_HOST`. Run:
//!   cargo build --manifest-path crates/adit-rdp/Cargo.toml --release
//!   ADIT_RDP_HOST=crates/adit-rdp/target/release/adit-rdp-host.exe \
//!     cargo test -p adit-rdp-proto --test helper_ipc -- --ignored --nocapture

use std::process::{Command, Stdio};

use adit_rdp_proto::{
    read_client_hello, read_helper_hello, read_msg, write_client_hello, write_msg, ClientMsg,
    ConnectRequest, HostMsg, IPC_PROTOCOL_VERSION,
};

#[test]
#[ignore = "needs the helper binary; see the module docs"]
fn helper_reports_error_on_unreachable_host() {
    let helper = std::env::var("ADIT_RDP_HOST").expect("set ADIT_RDP_HOST to the helper exe path");

    let mut child = Command::new(&helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn helper");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION).expect("send IPC hello");
    read_helper_hello(&mut stdout).expect("compatible helper");

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
        clipboard_policy_id: None,
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
#[ignore = "needs the isolated helper binary; see the module docs"]
fn real_helper_rejects_legacy_connect_and_wrong_version_without_opening_a_socket() {
    let helper =
        std::env::var_os("ADIT_RDP_HOST").expect("set ADIT_RDP_HOST to the helper exe path");
    for legacy in [true, false] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let appdata = std::env::temp_dir().join(format!(
            "adit-ipc-version-test-{}-{legacy}",
            std::process::id()
        ));
        let mut child = Command::new(&helper)
            .env("APPDATA", appdata)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        if legacy {
            // This models the old app, which writes Connect before reading stdout.
            // Its already-sent credentials cannot be prevented by the new helper.
            write_msg(
                &mut stdin,
                &ClientMsg::Connect(ConnectRequest {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                    username: "synthetic-user".into(),
                    password: "synthetic-password".into(),
                    domain: None,
                    width: 800,
                    height: 600,
                    enable_clipboard: true,
                    enable_audio: false,
                    quality: Default::default(),
                    clipboard_policy_id: None,
                }),
            )
            .unwrap();
        } else {
            write_client_hello(&mut stdin, IPC_PROTOCOL_VERSION - 1).unwrap();
        }
        if legacy {
            match read_msg::<_, HostMsg>(&mut stdout).unwrap() {
                Some(HostMsg::Error(message)) => {
                    assert!(message.contains("RDP IPC") && message.contains("版本"))
                }
                _ => panic!("unversioned app must receive an IPC version error"),
            }
            assert!(matches!(
                read_msg::<_, HostMsg>(&mut stdout).unwrap(),
                Some(HostMsg::Closed)
            ));
        } else {
            assert_eq!(
                read_client_hello(&mut stdout).unwrap(),
                IPC_PROTOCOL_VERSION
            );
            assert!(read_msg::<_, HostMsg>(&mut stdout).unwrap().is_none());
        }
        drop(stdin);
        let _ = child.wait().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "helper must not connect to the requested host before negotiation"
        );
    }
}
