//! Manual debug harness: drive the real helper against a real RDP host and dump
//! the event stream. Credentials come from env vars so they never touch disk.
//!
//!   cargo build --manifest-path crates/adit-rdp/Cargo.toml
//!   ADIT_RDP_HOST=crates/adit-rdp/target/debug/adit-rdp-host.exe \
//!   ADIT_RDP_TEST_HOST=192.168.x.y ADIT_RDP_TEST_USER=will ADIT_RDP_TEST_PASS=... \
//!   RUST_LOG=debug \
//!     cargo test -p adit-rdp-proto --test debug_connect -- --ignored --nocapture
//!
//! Set ADIT_RDP_DUMP_DIR to save decoded frames as PNG for visual inspection.
//! Each Tile is classified BLACK vs content by its non-black pixel ratio, so an
//! interleave of black legacy frames with real EGFX frames (the flicker) shows
//! up directly in the log.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg};

/// Fraction of non-near-black pixels below which a tile is deemed "black".
const BLACK_RATIO: f32 = 0.01;

/// Count pixels whose R,G,B are all < 8 (near-black); return the non-black ratio.
fn nonblack_ratio(rgba: &[u8]) -> f32 {
    let total = rgba.len() / 4;
    if total == 0 {
        return 0.0;
    }
    let mut nonblack = 0usize;
    for px in rgba.chunks_exact(4) {
        if px[0] >= 8 || px[1] >= 8 || px[2] >= 8 {
            nonblack += 1;
        }
    }
    nonblack as f32 / total as f32
}

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
    let dump_dir = std::env::var("ADIT_RDP_DUMP_DIR").ok();
    if let Some(dir) = &dump_dir {
        let _ = std::fs::create_dir_all(dir);
    }
    let run_secs: u64 = std::env::var("ADIT_RDP_RUN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let mut child = Command::new(&helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()) // helper logs (RUST_LOG) + panics
        .spawn()
        .expect("spawn helper");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    write_msg(
        &mut stdin,
        &ClientMsg::Connect(ConnectRequest {
            host,
            port,
            username,
            password,
            domain: None,
            width: 1600,
            height: 900,
            enable_clipboard: false,
            enable_audio: false,
        }),
    )
    .expect("send Connect");

    // GNOME only paints when the desktop changes, so an idle session emits just
    // the initial (dark) frames. Drive input to generate real content: open the
    // right-click context menu (a big opaque region, like the user's repro) and
    // sweep the pointer over its items to force hover redraws.
    let _input = std::thread::spawn(move || {
        use adit_rdp_proto::{InputEvent, MouseButton};
        let send = |stdin: &mut std::process::ChildStdin, ev| {
            let _ = write_msg(stdin, &ClientMsg::Input(ev));
        };
        std::thread::sleep(Duration::from_millis(1500)); // let the handover settle
        send(&mut stdin, InputEvent::MouseMove { x: 650, y: 360 });
        std::thread::sleep(Duration::from_millis(400));
        // Right-click → context menu.
        send(&mut stdin, InputEvent::MouseButton { button: MouseButton::Right, pressed: true });
        send(&mut stdin, InputEvent::MouseButton { button: MouseButton::Right, pressed: false });
        std::thread::sleep(Duration::from_millis(800));
        // Sweep the pointer down the menu items, then around the desktop, in a
        // loop, so redraws keep coming for the whole capture window.
        for round in 0..6 {
            for i in 0..12 {
                let y = 360 + i * 30;
                send(&mut stdin, InputEvent::MouseMove { x: 700, y: y.min(880) });
                std::thread::sleep(Duration::from_millis(120));
            }
            // Halfway through, resize the desktop — this is the path that used to
            // kill the session (DeactivateAll → reactivation error). Verifies the
            // session survives and re-renders at the new size.
            if round == 2 {
                send(&mut stdin, InputEvent::Resize { width: 1920, height: 1080 });
            }
            send(&mut stdin, InputEvent::MouseMove { x: 200 + round * 60, y: 200 });
            std::thread::sleep(Duration::from_millis(200));
        }
    });

    // Read HostMsgs on a thread so we can bound the whole run with a timeout.
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut stdout = stdout;
        let start = Instant::now();
        let mut frames = 0u64;
        loop {
            match read_msg::<_, HostMsg>(&mut stdout) {
                Ok(Some(HostMsg::Tile { x, y, width, height, rgba })) => {
                    frames += 1;
                    let ratio = nonblack_ratio(&rgba);
                    let is_black = ratio < BLACK_RATIO;
                    let kind = if is_black { "BLACK " } else { "content" };
                    // Log the full black/content sequence for the first 80 frames
                    // (enough to expose an interleave), then every 30th.
                    if frames <= 80 || frames % 30 == 0 {
                        let _ = tx.send(format!(
                            "[{:>6.2}s] Tile #{frames:<3} {width}x{height}@({x},{y}) {kind} nonblack={:>5.1}%",
                            start.elapsed().as_secs_f32(),
                            ratio * 100.0
                        ));
                    }
                    // Save a spread of frames for visual inspection: early content
                    // (coarse), a few black ones (to confirm the interleave), and
                    // late content (fully refined).
                    if let Some(dir) = &dump_dir {
                        // Save a spread across the run regardless of classification
                        // (the GNOME desktop background is genuinely near-black).
                        let want = matches!(frames, 1..=3 | 15 | 30 | 60 | 90 | 150);
                        if want && width as usize * height as usize * 4 == rgba.len() {
                            let name = format!(
                                "{dir}/frame_{frames:04}_{}_{:03}pct.png",
                                if is_black { "black" } else { "content" },
                                (ratio * 100.0) as u32
                            );
                            if image::save_buffer(
                                &name,
                                &rgba,
                                width as u32,
                                height as u32,
                                image::ColorType::Rgba8,
                            )
                            .is_ok()
                            {
                                let _ = tx.send(format!("  saved {name}"));
                            }
                        }
                    }
                }
                Ok(Some(other)) => {
                    let _ = tx.send(format!("[{:>6.2}s] {other:?}", start.elapsed().as_secs_f32()));
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

    let deadline = Instant::now() + Duration::from_secs(run_secs);
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

    while let Ok(line) = rx.try_recv() {
        eprintln!("{line}");
    }
}
