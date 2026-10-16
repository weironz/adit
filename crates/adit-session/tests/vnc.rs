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
    version(stream);
    stream.write_all(&[1, 1]).unwrap(); // one offered type: None
    assert_eq!(read::<1>(stream), [1]);
    stream.write_all(&0_u32.to_be_bytes()).unwrap();
    assert_eq!(read::<1>(stream), [1]); // shared session, never evict another viewer
    stream.write_all(&4_u16.to_be_bytes()).unwrap();
    stream.write_all(&2_u16.to_be_bytes()).unwrap();
    stream
        .write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
        .unwrap();
    stream.write_all(&4_u32.to_be_bytes()).unwrap();
    stream.write_all(b"test").unwrap();
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
            "VNC timed out: {:?}, {:?}",
            manager.active_session_summary(),
            manager.active_desktop_error()
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
    wait(&mut manager, |m| m.active_rdp_frame_if_newer(0).is_some_and(|f| f.generation >= 3));
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
        .contains("认证方式暂不支持"));
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
    wait(&mut manager, |m| m.active_session_summary().unwrap().status == SessionStatus::Error);
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
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
            && m.active_rdp_frame_if_newer(1).is_some()
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
    manager.send_rdp_input_to_active(RdpInput::MouseButton { button: RdpMouseButton::Left, pressed: true });
    manager.send_rdp_input_to_active(RdpInput::MouseButton { button: RdpMouseButton::Left, pressed: false });
    manager.send_vnc_key_to_active(0x61, true);
    manager.send_vnc_key_to_active(0x61, false);
    if let Ok(path) = std::env::var("ADIT_VNC_TEST_EVENTS") {
        wait(&mut manager, |_| {
            let events = std::fs::read_to_string(&path).unwrap_or_default();
            events.contains("keysym 0x61, a") && events.contains("ButtonRelease")
                && events.contains("MotionNotify")
        });
        let position = std::process::Command::new("xdotool")
            .args(["getmouselocation", "--shell"]).output().unwrap();
        let position = String::from_utf8(position.stdout).unwrap();
        assert!(position.contains("X=30\n") && position.contains("Y=40\n"), "{position}");
    }
    manager.disconnect(session).unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Disconnected
    });
}
