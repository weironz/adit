//! Uses an owned synthetic executable, never a user's configured shell/profile.
use std::time::{Duration, Instant};

use adit_domain::{ConnectionProfile, OutputTrigger, Protocol, TriggerAction};
use adit_session::SessionManager;
use adit_terminal::Viewport;

#[test]
#[ignore = "requires explicitly built ADIT_TRIGGER_FIXTURE_EXE and starts a local PTY"]
fn output_trigger_reaches_the_real_pty_input_path() {
    let peer = std::env::var("ADIT_TRIGGER_FIXTURE_EXE")
        .expect("build the trigger_pty_peer example and set ADIT_TRIGGER_FIXTURE_EXE");
    assert!(std::path::Path::new(&peer).is_file());
    let mut profile = ConnectionProfile::new("synthetic trigger", "loopback.invalid", 22, "");
    profile.protocol = Protocol::LocalShell;
    profile.identity_file = peer;
    profile.session_options.triggers = vec![OutputTrigger {
        enabled: true,
        pattern: "ADIT_READY".into(),
        action: TriggerAction::SendLine("adit-trigger-submit".into()),
    }];
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager
        .open_live_ssh_session(id, String::new(), String::new())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut consumed = false;
    while Instant::now() < deadline {
        manager.poll_events();
        let snapshot = manager.snapshot_for(session, Viewport::visible(50));
        let text: String = snapshot
            .lines
            .iter()
            .flat_map(|line| line.cells.iter().map(|cell| cell.text.as_str()))
            .collect();
        if text.contains("ADIT_DONE") {
            consumed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    manager.close(session);
    assert!(
        consumed,
        "the synthetic PTY peer did not consume the triggered line"
    );
}
