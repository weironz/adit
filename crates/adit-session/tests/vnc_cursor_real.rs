//! Real X11 -> TigerVNC -> RFB -> SessionManager cursor acceptance.
//! Requires only the isolated fixture documented in tests/vnc/README.md.
use adit_domain::{ConnectionProfile, Protocol, SessionStatus, VncSecurity};
use adit_session::{RdpInput, SessionManager, VncCursor};
use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn fixture_shape(shape: &str) {
    let container =
        std::env::var("ADIT_VNC_CURSOR_CONTAINER").unwrap_or_else(|_| "adit-vnc-cursor".into());
    let output = Command::new("docker")
        .args([
            "exec",
            &container,
            "python3",
            "/cursor_target.py",
            "--set",
            shape,
        ])
        .output()
        .expect("docker cursor fixture control");
    assert!(
        output.status.success(),
        "cursor fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_cursor(
    manager: &mut SessionManager,
    after: u64,
    matches: impl Fn(&VncCursor) -> bool,
) -> VncCursor {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut move_at = Instant::now();
    let mut pointer_x = 100;
    loop {
        manager.poll_events();
        if Instant::now() >= move_at {
            // A new TigerVNC viewer has its own pointer device; give it actual
            // movement rather than re-sending the previous viewer's position.
            pointer_x = if pointer_x == 100 { 101 } else { 100 };
            manager.send_rdp_input_to_active(RdpInput::MouseMove {
                x: pointer_x,
                y: 100,
            });
            move_at = Instant::now() + Duration::from_millis(100);
        }
        if let Some(cursor) = manager.active_vnc_cursor() {
            if cursor.generation > after && matches(&cursor) {
                return cursor;
            }
        }
        assert!(
            Instant::now() < deadline,
            "cursor did not arrive after generation {after}: {:?}, {:?}",
            manager.active_vnc_cursor(),
            manager.active_desktop_error()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn visible(cursor: &VncCursor) -> bool {
    cursor
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != 0)
}

fn pixel(cursor: &VncCursor, x: usize, y: usize) -> &[u8] {
    let offset = (y * usize::from(cursor.width) + x) * 4;
    &cursor.rgba[offset..offset + 4]
}

#[test]
#[ignore = "requires dedicated adit-vnc-cursor TigerVNC/X11 fixture on localhost:15912"]
fn real_tigervnc_cursor_shapes_masks_hotspots_and_reconnect() {
    fixture_shape("arrow");
    let port = std::env::var("ADIT_VNC_CURSOR_PORT")
        .unwrap_or_else(|_| "15912".into())
        .parse()
        .unwrap();
    let mut profile = ConnectionProfile::new("isolated cursor fixture", "127.0.0.1", port, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.security = VncSecurity::AllowNone;
    profile.vnc.auto_reconnect = false;
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_live_vnc_session(id, String::new()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while manager.active_session_summary().unwrap().status != SessionStatus::Connected {
        manager.poll_events();
        assert!(Instant::now() < deadline, "cursor fixture did not connect");
        thread::sleep(Duration::from_millis(5));
    }
    // TigerVNC starts with no active client pointer. Put this viewer inside the
    // fixture window before expecting its first real X11 cursor shape.
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 100, y: 100 });
    let arrow = wait_cursor(&mut manager, 0, visible);
    assert!(arrow.width > 0 && arrow.height > 0);
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 100, y: 100 });

    fixture_shape("ibeam");
    let ibeam = wait_cursor(&mut manager, arrow.generation, |c| {
        visible(c) && c.rgba != arrow.rgba
    });
    fixture_shape("rgba");
    let rgba = wait_cursor(&mut manager, ibeam.generation, |c| {
        c.width == 16 && c.height == 16 && c.hotspot_x == 5 && c.hotspot_y == 7
    });
    assert_eq!(pixel(&rgba, 2, 2), &[0, 255, 0, 255]);
    assert_eq!(pixel(&rgba, 3, 2), &[0, 0, 255, 255]);
    assert_eq!(pixel(&rgba, 4, 2), &[255, 0, 0, 255]);
    assert_eq!(
        pixel(&rgba, 0, 0)[3],
        0,
        "transparent border survives RFB mask"
    );
    assert_eq!(
        pixel(&rgba, 7, 7)[3],
        0,
        "transparent hole survives RFB mask"
    );

    fixture_shape("hidden");
    let hidden = wait_cursor(&mut manager, rgba.generation, |c| !visible(c));
    fixture_shape("arrow");
    let restored = wait_cursor(&mut manager, hidden.generation, |c| c.rgba == arrow.rgba);
    fixture_shape("rgba");
    let before_reconnect = wait_cursor(&mut manager, restored.generation, |c| c.rgba == rgba.rgba);

    // A local cursor is an independent plane; none of these updates may burn
    // cursor-coloured pixels into the uniform framebuffer.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        manager.poll_events();
        if let Some(frame) = manager.active_rdp_frame_if_newer(0) {
            if frame.width == 800
                && frame.height == 600
                && frame
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p == &[36, 68, 102, 255])
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "cursor must not contaminate framebuffer"
        );
        thread::sleep(Duration::from_millis(5));
    }

    manager.disconnect(session).unwrap();
    assert_eq!(
        manager.active_session_summary().unwrap().status,
        SessionStatus::Disconnected
    );
    assert!(
        manager.active_vnc_cursor().is_none(),
        "disconnected cursor must not remain observable as live state"
    );
    fixture_shape("ibeam");
    assert!(manager
        .retry_vnc_with_password(session, String::new())
        .unwrap());
    assert!(
        manager.active_vnc_cursor().is_none(),
        "replacement connection starts without the old cursor"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while manager.active_session_summary().unwrap().status != SessionStatus::Connected {
        manager.poll_events();
        assert!(
            Instant::now() < deadline,
            "cursor fixture did not reconnect"
        );
        thread::sleep(Duration::from_millis(5));
    }
    manager.send_rdp_input_to_active(RdpInput::MouseMove { x: 100, y: 100 });
    let reconnected = wait_cursor(&mut manager, 0, |c| c.rgba == ibeam.rgba);
    assert!(
        reconnected.generation < before_reconnect.generation,
        "cursor generation belongs to the new connection"
    );
    assert_eq!(
        manager.sessions().len(),
        1,
        "reconnect preserves its original tab"
    );
    manager.close(session);
    println!("TigerVNC cursor acceptance: arrow, I-beam, RGB/transparent mask, hotspot(5,7), hidden, framebuffer isolation and same-tab reconnect verified");
}
