//! Shared-desktop resize acceptance against an isolated real TigerVNC server.
use adit_domain::{ConnectionProfile, Protocol, SessionStatus};
use adit_session::{RdpInput, SessionManager};
use std::time::{Duration, Instant};

fn until(manager: &mut SessionManager, mut ready: impl FnMut(&mut SessionManager) -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        manager.poll_events();
        if ready(manager) {
            return;
        }
        assert!(
            Instant::now() < end,
            "real resize timed out: {:?}",
            manager.active_vnc_diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn has_frame(manager: &SessionManager, expected: (u16, u16)) -> bool {
    manager.active_rdp_frame_if_newer(0).is_some_and(|f| {
        (f.width, f.height) == expected && f.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
    })
}
#[test]
#[ignore = "requires loopback TigerVNC fixture tests/vnc/start-resize.sh on 15908"]
fn real_resize_is_opt_in_and_both_shared_clients_follow_changes() {
    let mut controller = ConnectionProfile::new("resize controller", "127.0.0.1", 15908, "");
    controller.protocol = Protocol::Vnc;
    controller.vnc.auto_reconnect = false;
    let mut observer = ConnectionProfile::new("resize observer", "127.0.0.1", 15908, "");
    observer.protocol = Protocol::Vnc;
    observer.vnc.auto_reconnect = false;
    let a = controller.id;
    let b = observer.id;
    let mut manager = SessionManager::with_profiles(vec![controller, observer]);
    let first = manager.open_live_vnc_session(a, "adit-ci".into()).unwrap();
    until(&mut manager, |m| has_frame(m, (800, 600)));
    let second = manager.open_live_vnc_session(b, "adit-ci".into()).unwrap();
    until(&mut manager, |m| has_frame(m, (800, 600)));
    manager.activate(first).unwrap();
    manager.send_rdp_input_to_active(RdpInput::Resize {
        width: 1024,
        height: 768,
    });
    std::thread::sleep(Duration::from_millis(400));
    manager.poll_events();
    assert!(
        has_frame(&manager, (800, 600)),
        "default local scaling changed the server"
    );
    manager.set_active_vnc_resize_remote(true);
    manager.send_rdp_input_to_active(RdpInput::Resize {
        width: 1024,
        height: 768,
    });
    until(&mut manager, |m| has_frame(m, (1024, 768)));
    manager.activate(second).unwrap();
    until(&mut manager, |m| has_frame(m, (1024, 768)));
    manager.set_active_vnc_resize_remote(true);
    manager.send_rdp_input_to_active(RdpInput::Resize {
        width: 800,
        height: 600,
    });
    until(&mut manager, |m| has_frame(m, (800, 600)));
    manager.activate(first).unwrap();
    until(&mut manager, |m| has_frame(m, (800, 600)));
    manager.send_rdp_input_to_active(RdpInput::Resize {
        width: 800,
        height: 600,
    });
    std::thread::sleep(Duration::from_millis(400));
    manager.poll_events();
    assert!(has_frame(&manager, (800, 600)));
    assert_eq!(
        manager.active_session_summary().unwrap().status,
        SessionStatus::Connected
    );
    assert!(manager.active_vnc_diagnostics().unwrap().pending_updates <= 1);
    manager.close(first);
    manager.close(second);
}
