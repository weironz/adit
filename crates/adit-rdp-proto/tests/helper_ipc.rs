//! End-to-end IPC smoke test against the real `adit-rdp-host` helper.
//!
//! Requires the helper to be built (it lives in a separate workspace) and its
//! path in `ADIT_RDP_HOST`. Run:
//!   cargo build --manifest-path crates/adit-rdp/Cargo.toml --release
//!   ADIT_RDP_HOST=crates/adit-rdp/target/release/adit-rdp-host.exe \
//!     cargo test -p adit-rdp-proto --test helper_ipc -- --ignored --nocapture

use std::process::{Command, Stdio};

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg};

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

    assert!(saw_error, "helper should report a connection error");
    assert!(saw_closed, "helper should close the session");
}
