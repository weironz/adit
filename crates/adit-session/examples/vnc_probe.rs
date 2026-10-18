//! Read-only live diagnostic: host port [seconds] [snapshot.ppm].
//! Password on stdin, never argv. Snapshots are saved only when explicitly requested.
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use adit_domain::{ConnectionProfile, Protocol};
use adit_session::SessionManager;

fn main() -> std::process::ExitCode {
    let all_args: Vec<_> = std::env::args().collect();
    if all_args.iter().filter(|arg| !arg.starts_with("--")).count() < 3 {
        eprintln!("usage: vnc_probe host port [seconds] [snapshot.ppm]; password on stdin");
        return std::process::ExitCode::from(2);
    }
    let mut password = zeroize::Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password).unwrap();
    std::process::ExitCode::from(u8::from(!run_probe(
        all_args,
        password.trim_end_matches(['\r', '\n']),
    )))
}

fn run_probe(all_args: Vec<String>, password: &str) -> bool {
    let wan = all_args.iter().any(|a| a == "--wan");
    let zrle = all_args.iter().any(|a| a == "--zrle");
    let synthetic_canary = all_args.iter().any(|a| a == "--synthetic-canary");
    assert!(!(wan && zrle), "choose only one VNC quality mode");
    let steady = all_args
        .iter()
        .find_map(|a| a.strip_prefix("--steady-seconds="))
        .map(|s| Duration::from_secs(s.parse().expect("invalid steady duration")));
    let tls_ca = all_args
        .iter()
        .find_map(|a| a.strip_prefix("--tls-ca="))
        .map(str::to_owned);
    let tls = tls_ca.is_some() || all_args.iter().any(|a| a == "--tls");
    let workload = all_args
        .iter()
        .find_map(|a| a.strip_prefix("--workload="))
        .unwrap_or("static")
        .to_owned();
    let args: Vec<_> = all_args
        .into_iter()
        .filter(|a| !a.starts_with("--"))
        .collect();
    if args.len() < 3 {
        eprintln!("usage: vnc_probe host port [seconds] [snapshot.ppm]; password on stdin");
        return false;
    }
    let mut profile = ConnectionProfile::new("VNC probe", &args[1], args[2].parse().unwrap(), "");
    profile.protocol = Protocol::Vnc;
    if tls {
        profile.vnc.security = adit_domain::VncSecurity::RequireTls;
        profile.vnc.tls_ca_file = tls_ca.unwrap_or_default();
    }
    assert!(
        matches!(workload.as_str(), "static" | "scroll" | "drag"),
        "unknown workload"
    );
    assert!(
        workload == "static" || matches!(args[1].as_str(), "127.0.0.1" | "localhost"),
        "input workloads are restricted to local isolated fixtures"
    );
    profile.vnc.read_only = workload == "static";
    profile.vnc.auto_reconnect = false;
    assert!(
        !synthetic_canary || matches!(args[1].as_str(), "127.0.0.1" | "localhost"),
        "synthetic pixel checks are restricted to local isolated fixtures"
    );
    if wan {
        profile.vnc.quality = adit_domain::VncQuality::Wan;
    } else if zrle {
        profile.vnc.quality = adit_domain::VncQuality::Zrle;
    }
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let start = Instant::now();
    let session = manager.open_live_vnc_session(id, password.into()).unwrap();
    let duration = Duration::from_secs(args.get(3).map_or(20, |s| s.parse().unwrap()));
    let mut generation = 0;
    let mut connected = false;
    let mut first = false;
    let mut complete = false;
    let mut canary_matches = !synthetic_canary;
    let mut complete_size = None;
    let mut connection_error = false;
    let mut disconnections = 0_u32;
    let mut completed_at: Option<Instant> = None;
    let mut report = Instant::now();
    let mut snapshot = args.get(4);
    let mut input_at = Instant::now();
    let mut step = 0_u16;
    let mut drag_down = false;
    loop {
        // Poll before the time boundary: an error already delivered during the
        // last tick cannot be hidden by a completed first-frame/steady timer.
        manager.poll_events();
        if let Some(error) = manager.active_desktop_error() {
            eprintln!("error: {error}");
            connection_error = true;
            break;
        }
        if connected
            && !manager
                .active_session_summary()
                .is_some_and(|summary| summary.status == adit_domain::SessionStatus::Connected)
        {
            disconnections += 1;
            eprintln!("error: VNC session left Connected during the observation window");
            break;
        }
        let finished = match (steady, completed_at) {
            (Some(window), Some(at)) => at.elapsed() >= window,
            _ => start.elapsed() >= duration,
        };
        if finished {
            break;
        }
        if !connected
            && manager
                .active_session_summary()
                .is_some_and(|s| s.status == adit_domain::SessionStatus::Connected)
        {
            println!("connected_ms={}", start.elapsed().as_millis());
            connected = true;
        }
        if let Some(frame) = manager.active_rdp_frame_if_newer(generation) {
            generation = frame.generation;
            let painted = if complete_size == Some((frame.width, frame.height)) {
                usize::from(frame.width) * usize::from(frame.height)
            } else {
                frame
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|pixel| pixel[3] == 255)
                    .count()
            };
            if painted > 0 && !first {
                println!(
                    "first_paint_ms={} size={}x{}",
                    start.elapsed().as_millis(),
                    frame.width,
                    frame.height
                );
                first = true;
            }
            if !complete && painted == usize::from(frame.width) * usize::from(frame.height) {
                println!(
                    "complete_paint_ms={} generation={generation}",
                    start.elapsed().as_millis()
                );
                complete = true;
                complete_size = Some((frame.width, frame.height));
                completed_at = Some(Instant::now());
                input_at = Instant::now();
                if synthetic_canary {
                    // Fixed, solid overlays distinguish the actual synthetic
                    // desktop from an opaque but empty capture. JPEG may vary
                    // channels slightly, so sample well inside both squares.
                    canary_matches = [
                        ((40_usize, 40_usize), [224_u8, 48, 32]),
                        ((670, 470), [32, 80, 224]),
                    ]
                    .into_iter()
                    .all(|((x, y), expected)| {
                        if x >= usize::from(frame.width) || y >= usize::from(frame.height) {
                            return false;
                        }
                        let offset = (y * usize::from(frame.width) + x) * 4;
                        frame.rgba[offset..offset + 3]
                            .iter()
                            .zip(expected)
                            .all(|(&actual, wanted)| actual.abs_diff(wanted) <= 24)
                    });
                    println!("synthetic_canary={canary_matches}");
                }
                if let Some(path) = snapshot.take() {
                    let mut file = std::fs::File::create(path).unwrap();
                    write!(file, "P6\n{} {}\n255\n", frame.width, frame.height).unwrap();
                    let rgb: Vec<_> = frame
                        .rgba
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|p| p[..3].iter().copied())
                        .collect();
                    file.write_all(&rgb).unwrap();
                }
            }
            if report.elapsed() >= Duration::from_millis(500) {
                println!(
                    "elapsed_ms={} generation={generation} coverage={:.1}%",
                    start.elapsed().as_millis(),
                    painted as f64 * 400.0 / frame.rgba.len() as f64
                );
                report = Instant::now();
            }
        }
        if complete && input_at.elapsed() >= Duration::from_millis(100) {
            use adit_session::{RdpInput, RdpMouseButton};
            if workload == "scroll" {
                manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 400, y: 300 });
                manager.send_rdp_input_to_active(RdpInput::Wheel {
                    vertical: true,
                    delta: if step % 20 < 10 { -120 } else { 120 },
                });
            } else if workload == "drag" {
                if !drag_down {
                    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 400, y: 300 });
                    manager.send_rdp_input_to_active(RdpInput::MouseButton {
                        button: RdpMouseButton::Left,
                        pressed: true,
                    });
                    drag_down = true;
                }
                let offset = if step % 40 < 20 {
                    step % 20
                } else {
                    20 - step % 20
                };
                manager.send_rdp_input_to_active(RdpInput::MouseMove {
                    x: 400 - offset * 8,
                    y: 300 - offset * 8,
                });
            }
            step = step.wrapping_add(1);
            input_at = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Final state belongs to the measured window, before intentional cleanup.
    manager.poll_events();
    if let Some(error) = manager.active_desktop_error() {
        if !connection_error {
            eprintln!("error: {error}");
        }
        connection_error = true;
    }
    let final_connected = manager
        .active_session_summary()
        .is_some_and(|summary| summary.status == adit_domain::SessionStatus::Connected);
    if connected && !final_connected && disconnections == 0 {
        disconnections += 1;
    }
    let steady_elapsed = completed_at.map_or(Duration::ZERO, |at| at.elapsed());
    let steady_complete =
        steady.is_none_or(|window| completed_at.is_some() && steady_elapsed >= window);
    println!("diagnostics={:?}", manager.active_vnc_diagnostics());
    println!("complete={complete}");
    println!("final_connected={final_connected} disconnects={disconnections} connection_error={connection_error} steady_complete={steady_complete}");
    println!(
        "sample_ms={} steady_ms={}",
        start.elapsed().as_millis(),
        steady_elapsed.as_millis()
    );
    if !complete && !connection_error {
        eprintln!("first_screen_timeout_s={}", duration.as_secs());
    }
    manager.close(session);
    // The GUI stays alive after closing a tab; a stand-alone probe must also
    // give its worker time to flush key/button releases before process exit.
    std::thread::sleep(Duration::from_millis(600));
    complete
        && canary_matches
        && !connection_error
        && disconnections == 0
        && final_connected
        && steady_complete
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        net::{TcpListener, TcpStream},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
    };
    enum End {
        Hold,
        Close,
        Malformed,
    }
    fn read<const N: usize>(s: &mut TcpStream) -> [u8; N] {
        let mut b = [0; N];
        s.read_exact(&mut b).unwrap();
        b
    }
    fn serve(end: End, partial: bool) -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let painted = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&painted);
        let task = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(4))).unwrap();
            s.write_all(b"RFB 003.008\n").unwrap();
            assert_eq!(&read::<12>(&mut s), b"RFB 003.008\n");
            s.write_all(&[1, 2]).unwrap();
            assert_eq!(read::<1>(&mut s), [2]);
            s.write_all(&[11; 16]).unwrap();
            let _response = read::<16>(&mut s);
            s.write_all(&0_u32.to_be_bytes()).unwrap();
            assert_eq!(read::<1>(&mut s), [1]);
            s.write_all(&[0, 2, 0, 2]).unwrap();
            s.write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
                .unwrap();
            s.write_all(&4_u32.to_be_bytes()).unwrap();
            s.write_all(b"test").unwrap();
            // Parse real client setup until the initial framebuffer request.
            loop {
                match read::<1>(&mut s)[0] {
                    0 => {
                        let _ = read::<19>(&mut s);
                    }
                    2 => {
                        let _ = read::<1>(&mut s);
                        let n = u16::from_be_bytes(read::<2>(&mut s));
                        for _ in 0..n {
                            let _ = read::<4>(&mut s);
                        }
                    }
                    3 => {
                        let _ = read::<9>(&mut s);
                        break;
                    }
                    tag => panic!("unexpected initial client message {tag}"),
                }
            }
            let height = if partial { 1_u16 } else { 2 };
            let mut update = vec![0, 0, 0, 1];
            for value in [0_u16, 0, 2, height] {
                update.extend(value.to_be_bytes());
            }
            update.extend(0_i32.to_be_bytes());
            for _ in 0..2 * height {
                update.extend([32, 64, 128, 0]);
            }
            s.write_all(&update).unwrap();
            observed.store(true, Ordering::Release);
            // The client has time to sample its full opaque first frame. The
            // failure is deliberately later, inside its requested steady window.
            match end {
                End::Close => thread::sleep(Duration::from_millis(250)),
                End::Malformed => {
                    thread::sleep(Duration::from_millis(250));
                    s.write_all(&[255]).unwrap();
                    thread::sleep(Duration::from_millis(100));
                }
                End::Hold => {
                    // Drain requests but leave them unanswered, as an idle real
                    // VNC server may do. No unrelated pixels/inputs are forged.
                    let mut b = [0; 4096];
                    while let Ok(n) = s.read(&mut b) {
                        if n == 0 {
                            break;
                        }
                    }
                }
            }
        });
        (port, painted, task)
    }
    fn probe(port: u16) -> bool {
        run_probe(
            vec![
                "vnc_probe".into(),
                "127.0.0.1".into(),
                port.to_string(),
                "1".into(),
                "--steady-seconds=1".into(),
            ],
            "public-synthetic-fixture",
        )
    }
    #[test]
    fn actual_first_frame_followed_by_transport_or_protocol_failure_cannot_pass() {
        for end in [End::Close, End::Malformed] {
            let (port, painted, server) = serve(end, false);
            assert!(
                !probe(port),
                "opaque first pixels must not hide a later failure"
            );
            assert!(painted.load(Ordering::Acquire));
            server.join().unwrap();
        }
    }
    #[test]
    fn actual_complete_frame_and_connected_full_steady_window_pass() {
        let (port, painted, server) = serve(End::Hold, false);
        let start = Instant::now();
        assert!(probe(port));
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert!(painted.load(Ordering::Acquire));
        server.join().unwrap();
    }
    #[test]
    fn actual_partial_frame_never_starts_or_satisfies_the_steady_window() {
        let (port, painted, server) = serve(End::Hold, true);
        assert!(!probe(port));
        assert!(painted.load(Ordering::Acquire));
        server.join().unwrap();
    }
}
