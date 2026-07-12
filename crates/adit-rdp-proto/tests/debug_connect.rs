//! Manual debug harness: drive the real helper against a real RDP host and dump
//! the event stream. Credentials come from env vars so they never touch disk.
//!
//!   cargo build --manifest-path crates/adit-rdp/Cargo.toml
//!   ADIT_RDP_HOST=crates/adit-rdp/target/debug/adit-rdp-host.exe \
//!   ADIT_RDP_TEST_HOST=192.168.x.y ADIT_RDP_TEST_USER=will ADIT_RDP_TEST_PASS=... \
//!   RUST_LOG=debug \
//!     cargo test -p adit-rdp-proto --test debug_connect -- --ignored --nocapture

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg};

#[test]
#[ignore = "manual: needs a real RDP host + creds in env"]
fn debug_connect_real_host() {
    let helper = std::env::var("ADIT_RDP_HOST").expect("ADIT_RDP_HOST");
    let host = std::env::var("ADIT_RDP_TEST_HOST").expect("ADIT_RDP_TEST_HOST");
    let username = std::env::var("ADIT_RDP_TEST_USER").expect("ADIT_RDP_TEST_USER");
    let password = std::env::var("ADIT_RDP_TEST_PASS").expect("ADIT_RDP_TEST_PASS");
    let port: u16 = std::env::var("ADIT_RDP_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3389);

    let mut child = Command::new(&helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()) // helper logs (RUST_LOG) + panics
        .spawn()
        .expect("spawn helper");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    write_msg(
        &mut stdin,
        &ClientMsg::Connect(ConnectRequest {
            host,
            port,
            username,
            password,
            domain: None,
            width: 1280,
            height: 720,
            enable_clipboard: false,
            enable_audio: false,
        }),
    )
    .expect("send Connect");

    // Read HostMsgs on a thread so we can bound the whole run with a timeout.
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let start = Instant::now();
        let mut frames = 0u64;
        loop {
            match read_msg::<_, HostMsg>(&mut stdout) {
                Ok(Some(HostMsg::Tile { x, y, width, height, rgba })) => {
                    frames += 1;
                    // Don't spam every frame; log the first few + every 30th.
                    if frames <= 3 || frames % 30 == 0 {
                        let _ = tx.send(format!(
                            "[{:>6.2}s] Tile #{frames} {width}x{height}@({x},{y}) {} bytes",
                            start.elapsed().as_secs_f32(),
                            rgba.len()
                        ));
                    }
                }
                Ok(Some(other)) => {
                    let _ = tx.send(format!(
                        "[{:>6.2}s] {other:?}",
                        start.elapsed().as_secs_f32()
                    ));
                    if matches!(other, HostMsg::Closed) {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(String::from("stdout EOF"));
                    break;
                }
                Err(e) => {
                    let _ = tx.send(format!("read error: {e}"));
                    break;
                }
            }
        }
        let _ = tx.send(format!("reader done after {frames} frames"));
    });

    // Print for up to 20s.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                eprintln!("{line}");
                if line.contains("reader done") || line == "stdout EOF" {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Probe whether the helper process is still alive (froze) or exited (crashed).
    match child.try_wait() {
        Ok(Some(status)) => eprintln!("helper EXITED: {status:?}"),
        Ok(None) => eprintln!("helper STILL RUNNING (would need kill)"),
        Err(e) => eprintln!("try_wait error: {e}"),
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(reader);

    // Drain any late lines.
    while let Ok(line) = rx.try_recv() {
        eprintln!("{line}");
    }
}
