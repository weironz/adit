//! Wire-level tests use a loopback RFB peer, not a mocked client library. The
//! ignored test also runs against real x11vnc and TigerVNC servers in CI.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use adit_domain::{ConnectionProfile, Protocol, SessionStatus};
use adit_session::{RdpInput, RdpMouseButton, SessionManager};

fn peer(f: impl FnOnce(TcpStream) + Send + 'static) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        f(stream);
    });
    (port, task)
}

fn read<const N: usize>(stream: &mut TcpStream) -> [u8; N] {
    let mut bytes = [0; N];
    stream.read_exact(&mut bytes).unwrap();
    bytes
}

fn version(stream: &mut TcpStream) {
    stream.write_all(b"RFB 003.008\n").unwrap();
    assert_eq!(&read::<12>(stream), b"RFB 003.008\n");
}

fn init(stream: &mut TcpStream) {
    init_size(stream, 4, 2);
}

fn init_size(stream: &mut TcpStream, width: u16, height: u16) {
    version(stream);
    stream.write_all(&[1, 1]).unwrap(); // one offered type: None
    assert_eq!(read::<1>(stream), [1]);
    stream.write_all(&0_u32.to_be_bytes()).unwrap();
    assert_eq!(read::<1>(stream), [1]); // shared session, never evict another viewer
    stream.write_all(&width.to_be_bytes()).unwrap();
    stream.write_all(&height.to_be_bytes()).unwrap();
    stream
        .write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
        .unwrap();
    stream.write_all(&4_u32.to_be_bytes()).unwrap();
    stream.write_all(b"test").unwrap();
}

#[test]
fn metadata_only_initial_update_recovers_full_coverage_without_refresh_storm() {
    // A completed metadata update does not establish that the desktop pixels
    // requested by the initial non-incremental request have been received.
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let (incremental_tx, incremental_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init_size(&mut stream, 64, 64);
        let mut metadata_sent = false;
        let mut partial_sent = false;
        let mut full_sent = false;
        let mut outstanding = false;
        while let Some((tag, data)) = message(&mut stream) {
            if tag != 3 {
                continue;
            }
            let rect = std::array::from_fn::<_, 4, _>(|i| {
                u16::from_be_bytes([data[1 + i * 2], data[2 + i * 2]])
            });
            recorded.lock().unwrap().push((data[0], rect));
            assert!(
                !outstanding,
                "second request while the previous one is unanswered"
            );
            assert_eq!(rect, [0, 0, 64, 64]);
            if !metadata_sent {
                assert_eq!(data[0], 0, "initial request must be non-incremental");
                // UltraVNC can acknowledge ExtendedDesktopSize support before
                // sending any framebuffer pixels, despite the full request.
                let mut metadata = vec![0, 0, 0, 1];
                for v in [0_u16, 0, 64, 64] {
                    metadata.extend(v.to_be_bytes());
                }
                metadata.extend((-308_i32).to_be_bytes());
                metadata.extend([1, 0, 0, 0]);
                metadata.extend(0_u32.to_be_bytes());
                for v in [0_u16, 0, 64, 64] {
                    metadata.extend(v.to_be_bytes());
                }
                metadata.extend(0_u32.to_be_bytes());
                stream.write_all(&metadata).unwrap();
                metadata_sent = true;
            } else if full_sent {
                assert_eq!(
                    data[0], 1,
                    "complete coverage must restore incremental requests"
                );
                incremental_tx.send(()).unwrap();
                outstanding = true;
            } else if data[0] == 0 || !partial_sent {
                let size = if data[0] == 0 { 64_u16 } else { 32_u16 };
                let mut pixels = vec![0, 0, 0, 1];
                for v in [0, 0, size, size] {
                    pixels.extend(v.to_be_bytes());
                }
                pixels.extend(0_i32.to_be_bytes());
                for _ in 0..usize::from(size).pow(2) {
                    pixels.extend([20, 40, 60, 0]);
                }
                stream.write_all(&pixels).unwrap();
                full_sent = size == 64;
                partial_sent = true;
            } else {
                // Nothing is dirty after the small tile. Keep this incremental
                // request unanswered, as an idle server may legitimately do.
                outstanding = true;
            }
        }
    });
    // None is explicitly allowed only for this synthetic loopback peer.
    let (mut manager, profile) = manager(port);
    let session = manager
        .open_live_vnc_session(profile, String::new())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut covered = false;
    let mut incremental_after_full = false;
    let mut max_pending = 0;
    let mut stable_since = None;
    while Instant::now() < deadline {
        manager.poll_events();
        if let Some(diagnostics) = manager.active_vnc_diagnostics() {
            max_pending = max_pending.max(diagnostics.pending_updates);
            covered = diagnostics.covered_pixels == 4096;
        }
        incremental_after_full |= incremental_rx.try_recv().is_ok();
        if covered && incremental_after_full {
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(150) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let diagnostics = manager.active_vnc_diagnostics();
    let complete_frame = manager.active_rdp_frame_if_newer(0).is_some_and(|frame| {
        (frame.width, frame.height) == (64, 64)
            && frame
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [20, 40, 60, 255])
    });
    manager.disconnect(session).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Disconnected
    });
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    eprintln!("coverage recovery requests={requests:?}; diagnostics={diagnostics:?}");
    assert!(
        covered && complete_frame,
        "initial full coverage was not recovered: {requests:?}"
    );
    assert!(
        incremental_after_full,
        "no incremental request after complete coverage"
    );
    assert!(max_pending <= 1);
    assert_eq!(
        requests
            .iter()
            .filter(|(incremental, _)| *incremental == 0)
            .count(),
        2
    );
    assert_eq!(requests.last().map(|request| request.0), Some(1));
}

#[test]
fn desktop_metadata_on_every_full_request_advances_to_incremental_pixels() {
    // TigerVNC answers a non-incremental request with desktop metadata first;
    // another non-incremental request repeats that metadata instead of pixels.
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let (incremental_tx, incremental_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init_size(&mut stream, 64, 64);
        let mut painted = false;
        let mut outstanding = false;
        while let Some((tag, data)) = message(&mut stream) {
            if tag != 3 {
                continue;
            }
            let rect = std::array::from_fn::<_, 4, _>(|i| {
                u16::from_be_bytes([data[1 + i * 2], data[2 + i * 2]])
            });
            recorded.lock().unwrap().push((data[0], rect));
            assert!(!outstanding, "request piled up behind an unanswered update");
            assert_eq!(rect, [0, 0, 64, 64]);
            if data[0] == 0 {
                // Same-size ExtendedDesktopSize, one screen, server reason.
                let mut metadata = vec![0, 0, 0, 1];
                for v in [0_u16, 0, 64, 64] {
                    metadata.extend(v.to_be_bytes());
                }
                metadata.extend((-308_i32).to_be_bytes());
                metadata.extend([1, 0, 0, 0]);
                metadata.extend(0_u32.to_be_bytes());
                for v in [0_u16, 0, 64, 64] {
                    metadata.extend(v.to_be_bytes());
                }
                metadata.extend(0_u32.to_be_bytes());
                stream.write_all(&metadata).unwrap();
            } else if !painted {
                assert_eq!(data[0], 1);
                let mut pixels = vec![0, 0, 0, 1];
                for v in [0_u16, 0, 64, 64] {
                    pixels.extend(v.to_be_bytes());
                }
                pixels.extend(0_i32.to_be_bytes());
                for _ in 0..4096 {
                    pixels.extend([20, 40, 60, 0]);
                }
                stream.write_all(&pixels).unwrap();
                painted = true;
            } else {
                assert_eq!(data[0], 1);
                incremental_tx.send(()).unwrap();
                // Leave the idle incremental request unanswered to detect any
                // new request storm after complete framebuffer coverage.
                outstanding = true;
            }
        }
    });
    // None is explicitly allowed only for this synthetic loopback peer.
    let (mut manager, profile) = manager(port);
    let session = manager
        .open_live_vnc_session(profile, String::new())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut covered = false;
    let mut incremental_after_pixels = false;
    let mut max_pending = 0;
    let mut stable_since = None;
    while Instant::now() < deadline {
        manager.poll_events();
        if let Some(diagnostics) = manager.active_vnc_diagnostics() {
            covered = diagnostics.covered_pixels == 4096;
            max_pending = max_pending.max(diagnostics.pending_updates);
        }
        incremental_after_pixels |= incremental_rx.try_recv().is_ok();
        if covered && incremental_after_pixels {
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(150) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let diagnostics = manager.active_vnc_diagnostics();
    let complete_frame = manager.active_rdp_frame_if_newer(0).is_some_and(|frame| {
        (frame.width, frame.height) == (64, 64)
            && frame
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [20, 40, 60, 255])
    });
    manager.disconnect(session).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Disconnected
    });
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    eprintln!("desktop metadata recovery requests={requests:?}; diagnostics={diagnostics:?}");
    assert!(
        covered && complete_frame,
        "metadata loop prevented full coverage: {requests:?}"
    );
    assert!(incremental_after_pixels);
    assert!(max_pending <= 1);
    assert_eq!(requests.first().map(|r| r.0), Some(0));
    assert!(requests.iter().filter(|r| r.0 == 0).count() <= 2);
    assert_eq!(requests.last().map(|r| r.0), Some(1));
}

fn message(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut tag = [0];
    match stream.read(&mut tag) {
        Ok(0) => return None,
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            return None
        }
        Err(error) => panic!("RFB peer read failed: {error}"),
    }
    let data = match tag[0] {
        0 => read::<19>(stream).to_vec(),
        2 => {
            let header = read::<3>(stream);
            let mut encodings =
                vec![0; usize::from(u16::from_be_bytes([header[1], header[2]])) * 4];
            stream.read_exact(&mut encodings).unwrap();
            encodings
        }
        3 => read::<9>(stream).to_vec(),
        4 => read::<7>(stream).to_vec(),
        5 => read::<5>(stream).to_vec(),
        248 => {
            let header = read::<8>(stream);
            let mut payload = vec![0; usize::from(header[7])];
            assert!(payload.len() <= 64);
            stream.read_exact(&mut payload).unwrap();
            [header.as_slice(), payload.as_slice()].concat()
        }
        6 => {
            let header = read::<7>(stream);
            let len = u32::from_be_bytes(header[3..7].try_into().unwrap()) as usize;
            assert!(len < 1024);
            let mut text = vec![0; len];
            stream.read_exact(&mut text).unwrap();
            text
        }
        other => panic!("unexpected RFB message {other}"),
    };
    Some((tag[0], data))
}

fn manager(port: u16) -> (SessionManager, adit_domain::ProfileId) {
    let mut profile = ConnectionProfile::new("VNC test", "127.0.0.1", port, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.security = adit_domain::VncSecurity::AllowNone;
    profile.vnc.auto_reconnect = false;
    let id = profile.id;
    (SessionManager::with_profiles(vec![profile]), id)
}

fn wait(manager: &mut SessionManager, mut ready: impl FnMut(&mut SessionManager) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        manager.poll_events();
        if ready(manager) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "VNC timed out: {:?}, {:?}, diagnostics={:?}",
            manager.active_session_summary(),
            manager.active_desktop_error(),
            manager.active_vnc_diagnostics()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn rfb_desktop_paints_copies_and_exchanges_input_and_clipboard() {
    let (wire_tx, wire_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        while let Some((tag, _)) = message(&mut stream) {
            if tag == 3 {
                break;
            }
        }
        // One Raw rectangle followed by an overlapping CopyRect. Pixel padding
        // is deliberately zero: treating it as alpha used to hide whole desktops.
        let mut update = vec![0, 0, 0, 2];
        for v in [0_u16, 0, 4, 2] {
            update.extend(v.to_be_bytes());
        }
        update.extend(0_i32.to_be_bytes());
        for red in 1..=8 {
            update.extend([red, 20, 30, 0]);
        }
        for v in [0_u16, 1, 3, 1] {
            update.extend(v.to_be_bytes());
        }
        update.extend(1_i32.to_be_bytes());
        update.extend(1_u16.to_be_bytes());
        update.extend(0_u16.to_be_bytes());
        stream.write_all(&update).unwrap();
        stream.write_all(&[3, 0, 0, 0, 0, 0, 0, 11]).unwrap();
        stream.write_all(b"from server").unwrap();
        while let Some((tag, data)) = message(&mut stream) {
            if tag != 3 {
                wire_tx.send((tag, data)).unwrap();
            }
        }
    });
    let (mut manager, profile_id) = manager(port);
    let session = manager
        .open_live_vnc_session(profile_id, String::new())
        .unwrap();
    assert!(manager.active_is_vnc());
    assert!(manager.active_is_desktop());
    wait(&mut manager, |m| {
        m.active_rdp_frame_if_newer(0)
            .is_some_and(|f| f.generation >= 3)
    });
    let frame = manager.active_rdp_frame_if_newer(0).unwrap();
    assert_eq!((frame.width, frame.height), (4, 2));
    assert_eq!(&frame.rgba[..4], &[1, 20, 30, 255]);
    assert_eq!(
        &frame.rgba[16..28],
        &[2, 20, 30, 255, 3, 20, 30, 255, 4, 20, 30, 255]
    );
    assert!(manager
        .active_rdp_frame_if_newer(frame.generation)
        .is_none());
    wait(&mut manager, |m| {
        m.take_rdp_clipboard().as_deref() == Some("from server")
    });
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 3, y: 1 });
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: true,
    });
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: false,
    });
    manager.send_vnc_key_to_active(0x61, true);
    manager.send_vnc_key_to_active(0x61, false);
    manager.offer_clipboard_to_active_rdp("local text".into());
    let received: Vec<_> = (0..6)
        .map(|_| wire_rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    assert_eq!(received[0], (5, vec![0, 0, 3, 0, 1]));
    assert_eq!(received[1].1[0], 1);
    assert_eq!(received[2].1[0], 0);
    assert_eq!(received[3], (4, vec![1, 0, 0, 0, 0, 0, 0x61]));
    assert_eq!(received[4].1[0], 0);
    assert_eq!(received[5], (6, b"local text".to_vec()));
    manager.disconnect(session).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Disconnected
    });
    server.join().unwrap();
}

#[test]
fn password_rejection_prompts_and_retries_the_same_tab() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        version(&mut stream);
        stream.write_all(&[1, 2]).unwrap();
        assert_eq!(read::<1>(&mut stream), [2]);
        stream.write_all(&[0; 16]).unwrap();
        assert_ne!(read::<16>(&mut stream), [0; 16]);
        stream.write_all(&1_u32.to_be_bytes()).unwrap();
        let reason = b"Authentication failed";
        stream
            .write_all(&(reason.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(reason).unwrap();
        drop(stream);
        let (mut retry, _) = listener.accept().unwrap();
        retry
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        init(&mut retry);
        while message(&mut retry).is_some() {}
    });
    let (mut manager, profile_id) = manager(port);
    let session = manager
        .open_live_vnc_session(profile_id, "incorrect".into())
        .unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Error
    });
    assert_eq!(manager.take_auth_rejection().unwrap().0, session);
    assert!(manager
        .retry_vnc_with_password(session, "correct".into())
        .unwrap());
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    assert_eq!(manager.sessions().len(), 1);
    assert_eq!(manager.active_session(), Some(session));
    assert!(manager.active_desktop_error().is_none());
    manager.close(session);
    server.join().unwrap();
}

#[test]
fn unsupported_security_is_visible_and_does_not_reprompt_for_a_password() {
    let (port, server) = peer(|mut stream| {
        version(&mut stream);
        stream.write_all(&[1, 19]).unwrap(); // VeNCrypt only
    });
    let (mut manager, id) = manager(port);
    manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Error
    });
    assert!(manager
        .active_desktop_error()
        .unwrap()
        .contains("安全策略不满足"));
    assert!(manager.take_auth_rejection().is_none());
    server.join().unwrap();
}

#[test]
fn an_oversized_remote_framebuffer_fails_without_allocating_it() {
    let (port, server) = peer(|mut stream| {
        version(&mut stream);
        stream.write_all(&[1, 1]).unwrap();
        assert_eq!(read::<1>(&mut stream), [1]);
        stream.write_all(&0_u32.to_be_bytes()).unwrap();
        assert_eq!(read::<1>(&mut stream), [1]);
        stream.write_all(&[255, 255, 255, 255]).unwrap();
    });
    let (mut manager, id) = manager(port);
    manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Error
    });
    assert!(manager.active_desktop_error().is_some());
    assert!(manager.active_rdp_frame_if_newer(0).is_none());
    server.join().unwrap();
}

#[test]
fn closing_a_tab_cancels_a_stalled_handshake_immediately() {
    let (closed, observed) = mpsc::channel();
    let (accepted, connected) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        accepted.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
        closed.send(()).unwrap();
    });
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    connected.recv_timeout(Duration::from_secs(2)).unwrap();
    manager.close(session);
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    assert!(manager.sessions().is_empty());
}

#[test]
fn vnc_profiles_need_no_ssh_username_and_preserve_their_identity() {
    let (mut manager, id) = manager(5901);
    let mut profile = manager.profile(id).unwrap().clone();
    profile.name = "Linux desktop".into();
    manager.save_vnc_profile(profile).unwrap();
    assert_eq!(manager.profiles().len(), 1);
    assert_eq!(manager.profile(id).unwrap().endpoint(), "127.0.0.1:5901");
    assert_eq!(Protocol::Vnc.default_port(), Some(5900));
    assert!(!Protocol::Vnc.is_terminal());
    assert!(!Protocol::Vnc.is_ssh_based());
}

#[test]
#[ignore = "needs a real VNC server; CI provides x11vnc and TigerVNC"]
fn real_vnc_server_authenticates_and_paints() {
    let port = std::env::var("ADIT_VNC_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let password = std::env::var("ADIT_VNC_TEST_PASSWORD").unwrap();
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, password).unwrap();
    eprintln!("real VNC: awaiting complete framebuffer");
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
            // Tight can deliver the white input window before the red background.
            && m.active_vnc_diagnostics().is_some_and(|d| d.first_covered_ms.is_some())
    });
    let frame = manager.active_rdp_frame_if_newer(0).unwrap();
    assert_eq!((frame.width, frame.height), (800, 600));
    assert!(frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[0] > 100 && p[1] < 100));
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 30, y: 40 });
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: true,
    });
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: false,
    });
    manager.send_vnc_key_to_active(0x61, true);
    manager.send_vnc_key_to_active(0x61, false);
    if let Ok(path) = std::env::var("ADIT_VNC_TEST_EVENTS") {
        eprintln!("real VNC: awaiting target keyboard, button release and pointer events");
        wait(&mut manager, |_| {
            let events = std::fs::read_to_string(&path).unwrap_or_default();
            events.contains("keysym 0x61, a")
                && events.contains("ButtonRelease")
                && events.contains("MotionNotify")
        });
        let position = std::process::Command::new("xdotool")
            .args(["getmouselocation", "--shell"])
            .output()
            .unwrap();
        let position = String::from_utf8(position.stdout).unwrap();
        assert!(
            position.contains("X=30\n") && position.contains("Y=40\n"),
            "{position}"
        );
    }
    manager.disconnect(session).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Disconnected
    });
}

#[test]
fn a_burst_of_small_rectangles_is_not_throttled_by_ui_ticks() {
    let (sent, received) = mpsc::channel();
    let (key_sent, key_received) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        while let Some((tag, data)) = message(&mut stream) {
            if tag == 2 {
                assert!(
                    data.as_chunks::<4>()
                        .0
                        .iter()
                        .any(|e| i32::from_be_bytes(*e) == -239),
                    "local cursor must be negotiated"
                );
            }
            if tag == 3 {
                break;
            }
        }
        // Match a ZRLE decoder burst: its two-slot output queue used to deliver
        // only two rectangles every 16 ms, even with all bytes already local.
        let mut update = vec![0, 0];
        update.extend(1024_u16.to_be_bytes());
        for _ in 0..1024 {
            for value in [0_u16, 0, 1, 1] {
                update.extend(value.to_be_bytes());
            }
            update.extend(0_i32.to_be_bytes());
            update.extend([10, 20, 30, 0]);
        }
        // Include a cursor shape; it must not contaminate the desktop pixels.
        update[2..4].copy_from_slice(&1025_u16.to_be_bytes());
        for value in [0_u16, 0, 1, 1] {
            update.extend(value.to_be_bytes());
        }
        update.extend((-239_i32).to_be_bytes());
        update.extend([255, 0, 0, 0, 0x80]);
        stream.write_all(&update).unwrap();
        sent.send(Instant::now()).unwrap();
        while let Some((tag, data)) = message(&mut stream) {
            if tag == 4 {
                key_sent.send(data).unwrap();
            }
        }
    });
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    let start = received.recv_timeout(Duration::from_secs(5)).unwrap();
    wait(&mut manager, |m| {
        m.active_rdp_frame_if_newer(0)
            .is_some_and(|f| f.generation >= 1025)
    });
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "decoder output stalled for {:?}",
        start.elapsed()
    );
    let frame = manager.active_rdp_frame_if_newer(0).unwrap();
    assert_eq!(&frame.rgba[..4], &[10, 20, 30, 255]);
    // Input must still run while recv_event is waiting on an idle server.
    manager.send_vnc_key_to_active(0x61, false);
    assert_eq!(
        key_received.recv_timeout(Duration::from_secs(2)).unwrap(),
        vec![0, 0, 0, 0, 0, 0, 0x61]
    );
    manager.close(session);
    server.join().unwrap();
}

#[test]
fn a_mouse_burst_during_handshake_keeps_the_latest_position_and_click_order() {
    let (start, started) = mpsc::channel();
    let (wire_tx, wire_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        init(&mut stream);
        while let Some((tag, data)) = message(&mut stream) {
            if tag == 5 {
                wire_tx.send(data).unwrap();
            }
        }
    });
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    for x in 0..10_000 {
        manager.send_rdp_input_to_active(RdpInput::MouseMove { x: x % 4, y: 1 });
    }
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: true,
    });
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 2, y: 0 });
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: false,
    });
    start.send(()).unwrap();
    let received: Vec<_> = (0..4)
        .map(|_| wire_rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    assert_eq!(
        received,
        vec![
            vec![0, 0, 3, 0, 1],
            vec![1, 0, 3, 0, 1],
            vec![1, 0, 2, 0, 0],
            vec![0, 0, 2, 0, 0]
        ]
    );
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    manager.close(session);
    server.join().unwrap();
}

#[test]
fn readonly_releases_held_input_then_blocks_keyboard_mouse_and_clipboard_push() {
    let (wire_tx, wire_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        while let Some((tag, data)) = message(&mut stream) {
            if matches!(tag, 4..=6) {
                let _ = wire_tx.send((tag, data));
            }
        }
    });
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    manager.send_vnc_key_to_active(0xffe1, true);
    manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: RdpMouseButton::Left,
        pressed: true,
    });
    assert_eq!(wire_rx.recv_timeout(Duration::from_secs(2)).unwrap().0, 4);
    assert_eq!(wire_rx.recv_timeout(Duration::from_secs(2)).unwrap().0, 5);
    manager.set_active_vnc_read_only(true);
    let key_up = wire_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(key_up, (4, vec![0, 0, 0, 0, 0, 0xff, 0xe1]));
    assert_eq!(
        wire_rx.recv_timeout(Duration::from_secs(2)).unwrap().1[0],
        0
    );
    manager.send_vnc_key_to_active(0x61, true);
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 2, y: 1 });
    manager.send_rdp_input_to_active(RdpInput::Wheel {
        vertical: true,
        delta: 120,
    });
    manager.offer_clipboard_to_active_rdp("blocked local".into());
    assert!(wire_rx.recv_timeout(Duration::from_millis(200)).is_err());
    manager.set_active_vnc_read_only(false);
    manager.send_vnc_key_to_active(0x62, true);
    assert_eq!(
        wire_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        (4, vec![1, 0, 0, 0, 0, 0, 0x62])
    );
    manager.close(session);
    assert_eq!(
        wire_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        (4, vec![0, 0, 0, 0, 0, 0, 0x62])
    );
    server.join().unwrap();
}

#[test]
fn dropped_transport_reconnects_the_same_tab_and_manual_close_stops_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (drop_tx, drop_rx) = mpsc::channel();
    let (second_tx, second_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        init(&mut stream);
        while message(&mut stream).is_some_and(|m| m.0 != 3) {}
        drop_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        init(&mut stream);
        while message(&mut stream).is_some_and(|m| m.0 != 3) {}
        second_tx.send(()).unwrap();
        while message(&mut stream).is_some() {}
    });
    let (mut manager, id) = manager(port);
    let mut profile = manager.profile(id).unwrap().clone();
    profile.vnc.auto_reconnect = true;
    profile.vnc.read_only = true;
    profile.session_options.reconnect = Some(adit_domain::ReconnectPolicy {
        enabled: true,
        max_attempts: 2,
        initial_delay_secs: 2,
        max_delay_secs: 2,
    });
    manager.save_vnc_profile(profile).unwrap();
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    drop_tx.send(()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connecting
    });
    assert!(manager
        .active_vnc_diagnostics()
        .unwrap()
        .status
        .contains("（1/2）"));
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    second_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(manager.sessions().len(), 1);
    assert_eq!(manager.active_session_summary().unwrap().id, session);
    assert!(manager.active_vnc_read_only());
    manager.close(session);
    server.join().unwrap();
}

#[test]
fn an_initially_readonly_session_never_emits_input_even_during_close() {
    let (wire_tx, wire_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        while let Some((tag, data)) = message(&mut stream) {
            if matches!(tag, 4..=6) {
                let _ = wire_tx.send((tag, data));
            }
        }
    });
    let (mut manager, id) = manager(port);
    let mut profile = manager.profile(id).unwrap().clone();
    profile.vnc.read_only = true;
    manager.save_vnc_profile(profile).unwrap();
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 3, y: 1 });
    manager.send_vnc_key_to_active(0x61, true);
    manager.offer_clipboard_to_active_rdp("must not send".into());
    manager.close(session);
    server.join().unwrap();
    assert!(
        wire_rx.try_recv().is_err(),
        "readonly session emitted an unsolicited input packet"
    );
}

#[test]
fn fence_keeps_an_idle_desktop_alive_and_detects_a_controlled_half_open_peer() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let respond = Arc::new(AtomicBool::new(true));
    let server_respond = Arc::clone(&respond);
    let (ping_tx, ping_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        init(&mut stream);
        let mut announced = false;
        while let Some((tag, data)) = message(&mut stream) {
            if tag == 3 && !announced {
                announced = true;
                stream.write_all(&[0, 0, 0, 0]).unwrap();
                stream.write_all(&[248, 0, 0, 0, 0x80, 0, 0, 0, 0]).unwrap();
            }
            if tag == 248 && data[3] & 0x80 != 0 {
                let _ = ping_tx.send(());
                if server_respond.load(Ordering::Acquire) {
                    let mut reply = vec![248, 0, 0, 0, 0, 0, 0, 0, data[7]];
                    reply.extend_from_slice(&data[8..]);
                    stream.write_all(&reply).unwrap();
                }
            }
        }
    });
    let (mut manager, id) = manager(port);
    let mut profile = manager.profile(id).unwrap().clone();
    profile.vnc.auto_reconnect = true;
    profile.vnc.read_only = true;
    manager.save_vnc_profile(profile).unwrap();
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    ping_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let healthy = Instant::now();
    while healthy.elapsed() < Duration::from_secs(21) {
        manager.poll_events();
        assert_eq!(
            manager.active_session_summary().unwrap().status,
            SessionStatus::Connected
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(manager
        .active_vnc_diagnostics()
        .unwrap()
        .fence_rtt_ms
        .is_some());
    respond.store(false, Ordering::Release);
    let stalled = Instant::now();
    loop {
        manager.poll_events();
        if manager.active_session_summary().unwrap().status == SessionStatus::Connecting {
            break;
        }
        assert!(
            stalled.elapsed() < Duration::from_secs(24),
            "half-open connection was not detected"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(manager.sessions().len(), 1);
    assert!(manager.active_vnc_read_only());
    manager.close(session);
    server.join().unwrap();
    assert!(manager.sessions().is_empty());
}

#[cfg(windows)]
#[test]
fn a_quiet_no_fence_desktop_survives_the_windows_retransmission_limit() {
    let (quiet_tx, quiet_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        let mut requests = 0;
        while let Some((tag, data)) = message(&mut stream) {
            if tag != 3 {
                continue;
            }
            requests += 1;
            if requests > 2 {
                continue;
            }
            if requests == 2 {
                assert_eq!(data[0], 1);
                quiet_tx.send(()).unwrap();
                // RFB may leave an incremental request unanswered indefinitely.
                // The OS still ACKs TCP; this is not a packet blackhole.
                resume_rx.recv_timeout(Duration::from_secs(45)).unwrap();
            }
            let mut frame = vec![0, 0, 0, 1];
            for value in [0_u16, 0, 4, 2] {
                frame.extend(value.to_be_bytes());
            }
            frame.extend(0_i32.to_be_bytes());
            let rgb = if requests == 1 {
                [1, 2, 3, 0]
            } else {
                [4, 5, 6, 0]
            };
            for _ in 0..8 {
                frame.extend(rgb);
            }
            stream.write_all(&frame).unwrap();
        }
    });
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| m.active_rdp_frame_if_newer(0).is_some());
    quiet_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(35) {
        manager.poll_events();
        assert_eq!(
            manager.active_session_summary().unwrap().status,
            SessionStatus::Connected
        );
        assert_eq!(manager.active_session_summary().unwrap().id, session);
        assert!(manager
            .active_vnc_diagnostics()
            .unwrap()
            .fence_rtt_ms
            .is_none());
        thread::sleep(Duration::from_millis(20));
    }
    resume_tx.send(()).unwrap();
    wait(&mut manager, |m| {
        m.active_rdp_frame_if_newer(0)
            .is_some_and(|frame| frame.rgba[..4] == [4, 5, 6, 255])
    });
    assert_eq!(manager.sessions().len(), 1);
    manager.close(session);
    server.join().unwrap();
}

#[test]
fn queued_remote_clipboard_does_not_survive_sharing_off_then_on() {
    let (trigger, commands) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        while message(&mut stream).unwrap().0 != 3 {}
        for _ in 0..2 {
            let text: String = commands.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut packet = vec![3, 0, 0, 0];
            packet.extend((text.len() as u32).to_be_bytes());
            packet.extend(text.as_bytes());
            // The following update is an observable decoder/worker barrier.
            packet.extend([0, 0, 0, 0]);
            stream.write_all(&packet).unwrap();
        }
        while message(&mut stream).is_some() {}
    });
    let (mut manager, id) = manager(port);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
    });
    trigger.send("queued old remote text".to_string()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while manager.active_vnc_diagnostics().unwrap().complete_updates < 1 {
        assert!(
            Instant::now() < deadline,
            "worker did not pass the clipboard barrier"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // No poll_events after the barrier: the decoded text is queued in the
    // session event channel when the user changes the policy twice.
    manager.set_rdp_clipboard(false);
    manager.set_rdp_clipboard(true);
    manager.poll_events();
    assert!(
        manager.take_rdp_clipboard().is_none(),
        "old queued text crossed off/on"
    );
    trigger.send("fresh remote text".to_string()).unwrap();
    wait(&mut manager, |m| {
        m.take_rdp_clipboard().as_deref() == Some("fresh remote text")
    });
    manager.close(session);
    server.join().unwrap();
}

#[test]
fn zrle_compatibility_omits_tight_and_jpeg_and_paints_lossless_pixels() {
    let (encoding_tx, encoding_rx) = mpsc::channel();
    let (port, server) = peer(move |mut stream| {
        init(&mut stream);
        loop {
            let (tag, data) = message(&mut stream).unwrap();
            if tag == 2 {
                let encodings: Vec<_> = data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| i32::from_be_bytes(*bytes))
                    .collect();
                encoding_tx.send(encodings).unwrap();
            }
            if tag == 3 {
                break;
            }
        }
        let mut packet = vec![0, 0, 0, 1];
        for value in [0_u16, 0, 4, 2] {
            packet.extend(value.to_be_bytes());
        }
        packet.extend(16_i32.to_be_bytes()); // ZRLE solid-color tile.
                                             // Sync-flushed persistent zlib stream: subencoding 1 and one RGB pixel.
        let compressed = [120, 156, 98, 228, 22, 83, 4, 0, 0, 0, 255, 255];
        packet.extend((compressed.len() as u32).to_be_bytes());
        packet.extend(compressed);
        stream.write_all(&packet).unwrap();
        while message(&mut stream).is_some() {}
    });
    let mut profile = ConnectionProfile::new("compatibility", "127.0.0.1", port, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.security = adit_domain::VncSecurity::AllowNone;
    profile.vnc.quality = adit_domain::VncQuality::Zrle;
    profile.vnc.auto_reconnect = false;
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    let encodings = encoding_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(encodings.contains(&16));
    assert!(encodings.contains(&0) && encodings.contains(&1));
    assert!(!encodings.contains(&7), "Tight was still advertised");
    assert!(
        !encodings.iter().any(|value| (-32..=-23).contains(value)),
        "JPEG quality was still advertised"
    );
    wait(&mut manager, |m| {
        m.active_vnc_diagnostics().unwrap().covered_pixels == 8
    });
    let frame = manager.active_rdp_frame_if_newer(0).unwrap();
    assert_eq!(frame.rgba.as_slice(), [11, 22, 33, 255].repeat(8));
    manager.close(session);
    server.join().unwrap();
}
