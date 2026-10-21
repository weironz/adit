//! Manual input acceptance on an explicitly selected private/loopback VNC fixture.
//! argv: host port [seconds] [--bmp-only]. Password is the first stdin line; never argv/env.
//! Remaining stdin commands: WAIT ms, KEY hex-keysym 0|1, TEXT text, CLICK x y,
//! CLIP text, READONLY 0|1, SNAP absolute.ppm, ACK token, CLOSE. No input bodies are logged.
use adit_domain::{ConnectionProfile, Protocol, SessionStatus};
use adit_session::{RdpInput, RdpMouseButton, SessionManager};
use std::{
    io::{self, BufRead, Write},
    sync::mpsc,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let bmp_only = args.last().is_some_and(|arg| arg == "--bmp-only");
    let positional_len = args.len() - usize::from(bmp_only);
    if !(3..=4).contains(&positional_len) {
        return Err(
            "usage: vnc_fixture_driver private-ip port [seconds] [--bmp-only]; password on stdin".into(),
        );
    }
    let host: std::net::Ipv4Addr = args[1].parse()?;
    if !host.is_private() && !host.is_loopback() {
        return Err("input is restricted to an authorized private/loopback fixture".into());
    }
    let timeout = if positional_len == 4 { args[3].parse()? } else { 120 };
    let mut password = zeroize::Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password)?;
    let mut profile = ConnectionProfile::new("manual VNC fixture", &args[1], args[2].parse()?, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.auto_reconnect = false;
    profile.vnc.unicode_bmp_only = bmp_only;
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session =
        manager.open_live_vnc_session(id, password.trim_end_matches(['\r', '\n']).into())?;
    let (sender, commands) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let mut ready = false;
    let mut next = Instant::now();
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        while start.elapsed() < Duration::from_secs(timeout) {
            manager.poll_events();
            if let Some(error) = manager.active_desktop_error() {
                return Err(error.into());
            }
            if let Some(text) = manager.take_rdp_clipboard() {
                if text.starts_with("ADIT_VNC_TEST:") {
                    println!("{text}");
                }
            }
            if let Some(notice) = manager.take_desktop_notice() {
                println!("notice={notice}");
            }
            if !ready
                && manager
                    .active_session_summary()
                    .is_some_and(|s| s.status == SessionStatus::Connected)
            {
                ready = manager.active_rdp_frame_if_newer(0).is_some_and(|f| {
                    f.width > 1
                        && f.height > 1
                        && f.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
                });
                if ready {
                    println!("ready_ms={}", start.elapsed().as_millis());
                }
            }
            if ready && Instant::now() >= next {
                if let Ok(command) = commands.try_recv() {
                    if command == "CLOSE" {
                        return Ok(());
                    }
                    if let Some(ms) = command.strip_prefix("WAIT ") {
                        next = Instant::now() + Duration::from_millis(ms.parse()?);
                    } else if let Some(token) = command.strip_prefix("ACK ") {
                        if token.is_empty() || token.len() > 64
                            || !token.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                        {
                            return Err("ACK requires 1-64 ASCII letters, digits, hyphens or underscores".into());
                        }
                        println!("ack={token}");
                        io::stdout().flush()?;
                    } else if let Some(text) = command.strip_prefix("TEXT ") {
                        if let Err(notice) = manager.send_vnc_text_to_active(text) {
                            let reason = if notice == SessionManager::vnc_bmp_only_notice() {
                                "unicode_compatibility"
                            } else if notice == SessionManager::vnc_text_too_long_notice() {
                                "text_too_long"
                            } else {
                                "other"
                            };
                            println!("text_rejected={reason}");
                            println!("notice={notice}");
                        }
                    } else if let Some(text) = command.strip_prefix("CLIP ") {
                        manager.offer_clipboard_to_active_rdp(text.into());
                    } else if let Some(value) = command.strip_prefix("READONLY ") {
                        manager.set_active_vnc_read_only(value == "1");
                    } else if let Some(key) = command.strip_prefix("KEY ") {
                        let parts: Vec<_> = key.split_whitespace().collect();
                        if parts.len() != 2 {
                            return Err("KEY requires keysym and pressed".into());
                        }
                        manager.send_vnc_key_to_active(
                            u32::from_str_radix(parts[0], 16)?,
                            parts[1] == "1",
                        );
                    } else if let Some(point) = command.strip_prefix("CLICK ") {
                        let parts: Vec<_> = point.split_whitespace().collect();
                        if parts.len() != 2 {
                            return Err("CLICK requires x y".into());
                        }
                        manager.send_rdp_input_to_active(RdpInput::MouseMove {
                            x: parts[0].parse()?,
                            y: parts[1].parse()?,
                        });
                        for pressed in [true, false] {
                            manager.send_rdp_input_to_active(RdpInput::MouseButton {
                                button: RdpMouseButton::Left,
                                pressed,
                            });
                        }
                    } else if let Some(path) = command.strip_prefix("SNAP ") {
                        let path = std::path::Path::new(path);
                        if !path.is_absolute() {
                            return Err("snapshot requires absolute path".into());
                        }
                        let f = manager.active_rdp_frame_if_newer(0).ok_or("no frame")?;
                        let mut file = io::BufWriter::new(std::fs::File::create(path)?);
                        write!(file, "P6\n{} {}\n255\n", f.width, f.height)?;
                        for pixel in f.rgba.as_chunks::<4>().0 {
                            file.write_all(&pixel[..3])?;
                        }
                        file.flush()?;
                        println!("snapshot_saved {}x{}", f.width, f.height);
                    } else {
                        return Err("unknown stdin command".into());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("fixture deadline expired".into())
    })();
    manager.close(session);
    std::thread::sleep(Duration::from_millis(600));
    result
}
