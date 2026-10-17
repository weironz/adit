//! Read-only live diagnostic: host port [seconds] [snapshot.ppm].
//! Password on stdin, never argv. Snapshots are saved only when explicitly requested.
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use adit_domain::{ConnectionProfile, Protocol};
use adit_session::SessionManager;

fn main() {
    let all_args: Vec<_> = std::env::args().collect();
    let wan = all_args.iter().any(|a| a == "--wan");
    let zrle = all_args.iter().any(|a| a == "--zrle");
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
        std::process::exit(2);
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
    if wan {
        profile.vnc.quality = adit_domain::VncQuality::Wan;
    } else if zrle {
        profile.vnc.quality = adit_domain::VncQuality::Zrle;
    }
    let id = profile.id;
    let mut password = zeroize::Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password).unwrap();
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let start = Instant::now();
    let session = manager
        .open_live_vnc_session(id, password.trim_end_matches(['\r', '\n']).into())
        .unwrap();
    let duration = Duration::from_secs(args.get(3).map_or(20, |s| s.parse().unwrap()));
    let mut generation = 0;
    let mut connected = false;
    let mut first = false;
    let mut complete = false;
    let mut complete_size = None;
    let mut connection_error = false;
    let mut completed_at: Option<Instant> = None;
    let mut report = Instant::now();
    let mut snapshot = args.get(4);
    let mut input_at = Instant::now();
    let mut step = 0_u16;
    let mut drag_down = false;
    loop {
        let finished = match (steady, completed_at) {
            (Some(window), Some(at)) => at.elapsed() >= window,
            _ => start.elapsed() >= duration,
        };
        if finished {
            break;
        }
        manager.poll_events();
        if let Some(error) = manager.active_desktop_error() {
            eprintln!("error: {error}");
            connection_error = true;
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
    println!("diagnostics={:?}", manager.active_vnc_diagnostics());
    println!("complete={complete}");
    println!(
        "sample_ms={} steady_ms={}",
        start.elapsed().as_millis(),
        completed_at.map_or(0, |at| at.elapsed().as_millis())
    );
    if !complete && !connection_error {
        eprintln!("first_screen_timeout_s={}", duration.as_secs());
    }
    manager.close(session);
    // The GUI stays alive after closing a tab; a stand-alone probe must also
    // give its worker time to flush key/button releases before process exit.
    std::thread::sleep(Duration::from_millis(600));
    if !complete {
        std::process::exit(1);
    }
}
