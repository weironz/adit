use super::*;
use adit_session::{clipboard_test_fixture::SyntheticRdp, RdpClipboardPolicyFailure};
use rdp_clipboard_policy::PolicyPhase;

#[test]
fn policy_follows_the_owning_window_and_switching_keeps_sessions_open() {
    let profiles: Vec<_> = (0..3)
        .map(|n| {
            ConnectionProfile::new(
                format!("policy-window-{n}"),
                "fixture.invalid",
                22,
                "fixture",
            )
        })
        .collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let ids: Vec<_> = manager
        .profiles()
        .iter()
        .map(|profile| profile.id)
        .collect();
    let sessions: [SessionId; 3] =
        std::array::from_fn(|n| manager.open_mock_session(ids[n]).unwrap());
    let peers: [SyntheticRdp; 3] =
        std::array::from_fn(|n| SyntheticRdp::attach(&mut manager, sessions[n]));
    for peer in &peers {
        peer.prepare_policy(&manager).unwrap();
    }
    peers[0].policy_channel_ready(&manager, 1);
    peers[0].acknowledge_policy(&manager, 0, true);
    peers[1].timeout_policy(&manager);
    manager.activate(sessions[2]).unwrap();
    let primary = window::Id::unique();
    let app = AditApp {
        manager,
        main_view: MainView::Terminal,
        rdp_clipboard: true,
        native_main_window: Some(primary),
        native_window_id: Some(primary),
        ..AditApp::default()
    };
    let mut desktop = DesktopApplication {
        app,
        primary,
        focused: Some(primary),
        windows: HashMap::new(),
        clipboard_leases: HashMap::new(),
        modal_owner: None,
    };
    let _ = desktop.detach(sessions[0]);
    let child = desktop.owner(sessions[0]);
    assert_ne!(child, primary);
    desktop.windows.get_mut(&child).unwrap().state.fullscreen = true;
    desktop.app.fullscreen = true;

    let selected = rdp_clipboard_policy::for_selected_session(&desktop.app).unwrap();
    let detached =
        rdp_clipboard_policy::for_session(&desktop.app, desktop.windows[&child].session).unwrap();
    assert_eq!(desktop.app.manager.active_session(), Some(sessions[2]));
    assert_eq!(selected.phase, PolicyPhase::Pending);
    assert_eq!(desktop.windows[&child].session, sessions[0]);
    assert_eq!(detached.phase, PolicyPhase::Confirmed);
    let _ = desktop.view(primary);
    let _ = desktop.view(child);

    desktop.app.manager.activate(sessions[1]).unwrap();
    let selected = rdp_clipboard_policy::for_selected_session(&desktop.app).unwrap();
    assert_eq!(
        selected.phase,
        PolicyPhase::Failed(RdpClipboardPolicyFailure::ConfirmationTimedOut)
    );
    assert_eq!(
        rdp_clipboard_policy::for_session(&desktop.app, sessions[0])
            .unwrap()
            .phase,
        PolicyPhase::Confirmed
    );

    // A primary window must not present the detached session as its own.
    desktop.app.manager.activate(sessions[0]).unwrap();
    assert_eq!(
        rdp_clipboard_policy::for_selected_session(&desktop.app),
        None
    );
    assert_eq!(
        rdp_clipboard_policy::for_session(&desktop.app, sessions[0])
            .unwrap()
            .phase,
        PolicyPhase::Confirmed
    );
    let _ = desktop.in_window(child, |app| update(app, Message::ToggleRdpClipboard(false)));
    assert!(!desktop.app.rdp_clipboard);
    assert_eq!(
        rdp_clipboard_policy::for_session(&desktop.app, sessions[0])
            .unwrap()
            .phase,
        PolicyPhase::Pending
    );
    peers[0].acknowledge_policy(&desktop.app.manager, 1, false);
    let child_status = rdp_clipboard_policy::for_session(&desktop.app, sessions[0]).unwrap();
    assert_eq!(
        (child_status.enabled, child_status.phase),
        (false, PolicyPhase::Confirmed)
    );
    desktop.app.manager.activate(sessions[2]).unwrap();
    let primary_status = rdp_clipboard_policy::for_selected_session(&desktop.app).unwrap();
    assert_eq!(desktop.app.manager.active_session(), Some(sessions[2]));
    assert_eq!(
        (primary_status.enabled, primary_status.phase),
        (false, PolicyPhase::Pending)
    );
    assert_eq!(desktop.app.manager.sessions().len(), 3);
    assert!(sessions.into_iter().all(|session| desktop
        .app
        .manager
        .session_summary(session)
        .is_some_and(|summary| summary.status == SessionStatus::Connected)));
    for peer in &peers {
        while let Some(command) = peer.try_command() {
            assert!(!matches!(command, adit_rdp_proto::ClientMsg::Close));
        }
    }
}
