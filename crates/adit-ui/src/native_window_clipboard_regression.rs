use super::*;
use adit_rdp_proto::ClientMsg;
use adit_session::{clipboard_test_fixture::SyntheticRdp, RdpClientEvent};
use std::time::Duration;

fn desktop_fixture() -> (DesktopApplication, [SessionId; 3], [SyntheticRdp; 3]) {
    let profiles: Vec<_> = (0..3)
        .map(|n| ConnectionProfile::new(format!("fixture-{n}"), "fixture.invalid", 22, "fixture"))
        .collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let profile_ids: Vec<_> = manager.profiles().iter().map(|p| p.id).collect();
    let sessions = std::array::from_fn(|n| manager.open_mock_session(profile_ids[n]).unwrap());
    let peers = std::array::from_fn(|n| SyntheticRdp::attach(&mut manager, sessions[n]));
    let primary = window::Id::unique();
    let app = AditApp {
        manager,
        main_view: MainView::Terminal,
        native_main_window: Some(primary),
        native_window_id: Some(primary),
        ..AditApp::default()
    };
    (
        DesktopApplication {
            app,
            primary,
            focused: Some(primary),
            windows: HashMap::new(),
            clipboard_leases: HashMap::new(),
            modal_owner: None,
        },
        sessions,
        peers,
    )
}

#[test]
fn unfocused_sequence_change_freezes_text_and_images_in_all_windows_and_tabs() {
    use adit_rdp_proto::InputEvent;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    const CHILD: &str = "ADIT_SYNTHETIC_GLOBAL_SEQUENCE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_windows::clipboard_regression_tests::unfocused_sequence_change_freezes_text_and_images_in_all_windows_and_tabs",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "isolated global sequence test: {status}");
        return;
    }

    let (mut desktop, sessions, peers) = desktop_fixture();
    let _ = desktop.detach(sessions[0]);
    let _ = desktop.dispatch(desktop.primary, Message::TabClicked(sessions[1]));
    assert!(desktop.clipboard_leases.contains_key(&sessions[2]));
    // Switching tabs releases mouse buttons and modifiers for the old owner.
    for peer in &peers {
        while let Some(command) = peer.try_command() {
            assert!(matches!(command, ClientMsg::Input(_)));
        }
    }
    let sequence = Arc::new(AtomicU32::new(71));
    desktop.revoke_stale_local_file_offers(71);
    for peer in &peers {
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(Some(ClientMsg::LocalFileOfferRejected))
        ));
    }
    for session in sessions {
        desktop.app.manager.select_session_context(session).unwrap();
        let text_source = sequence.clone();
        desktop
            .app
            .manager
            .offer_clipboard_to_active_rdp_with_sequence(
                "synthetic old text".into(),
                adit_session::RdpClipboardSequence::new(71, move || {
                    text_source.load(Ordering::Acquire)
                }),
            );
        let image_source = sequence.clone();
        desktop.app.manager.offer_image_to_active_rdp_with_sequence(
            vec![1, 2, 3],
            adit_session::RdpClipboardSequence::new(71, move || {
                image_source.load(Ordering::Acquire)
            }),
        );
    }

    // Model focus outside Adit. No actual clipboard is opened or queried.
    desktop.focused = None;
    desktop.app.desktop_window_focused = false;
    for window in desktop.windows.values_mut() {
        window.state.desktop_window_focused = false;
    }
    let active = desktop.app.manager.active_session();
    sequence.store(72, Ordering::Release);
    desktop.revoke_stale_local_file_offers(72);
    assert_eq!(desktop.app.manager.active_session(), active);
    for peer in &peers {
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(None)
        ));
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(None)
        ));
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(Some(ClientMsg::LocalFileOfferRejected))
        ));
        assert!(peer.try_command().is_none());
    }
    desktop.revoke_stale_local_file_offers(72);
    assert!(peers.iter().all(|peer| peer.try_command().is_none()));

    for session in sessions {
        desktop.app.manager.select_session_context(session).unwrap();
        let source = sequence.clone();
        desktop
            .app
            .manager
            .offer_clipboard_to_active_rdp_with_sequence(
                "synthetic newer text".into(),
                adit_session::RdpClipboardSequence::new(72, move || source.load(Ordering::Acquire)),
            );
    }
    sequence.store(0, Ordering::Release);
    desktop.revoke_stale_local_file_offers(0);
    for peer in &peers {
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(None)
        ));
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(Some(ClientMsg::LocalFileOfferRejected))
        ));
    }
    desktop
        .app
        .manager
        .send_rdp_input_to_active(adit_session::RdpInput::Key {
            scancode: 30,
            extended: false,
            pressed: true,
        });
    assert!(matches!(
        peers[2].write_next_command(&desktop.app.manager, Duration::from_secs(1)),
        Ok(Some(ClientMsg::Input(InputEvent::Key { .. })))
    ));
}

#[test]
fn independent_windows_keep_their_own_file_scopes() {
    let (mut desktop, sessions, peers) = desktop_fixture();
    let _ = desktop.detach(sessions[0]);
    let _ = desktop.detach(sessions[1]);
    let windows = [desktop.owner(sessions[0]), desktop.owner(sessions[1])];
    let mut streams = Vec::new();
    for n in 0..2 {
        let generation = 17 + n as u64;
        let requester = peers[n].remote_offer(&desktop.app.manager, generation);
        let state = &mut desktop.windows.get_mut(&windows[n]).unwrap().state;
        state.rdp_file_scope = Some((sessions[n], generation));
        let bridge = state.rdp_chunk_bridge.clone();
        let stream = bridge.begin().unwrap();
        requester.request(stream, 0, 0, 1);
        peers[n].send_event(
            &desktop.app.manager,
            RdpClientEvent::FileContents {
                generation,
                stream_id: stream,
                data: Some(vec![n as u8 + 1]),
            },
        );
        streams.push((bridge, stream));
    }
    desktop.app.manager.poll_events();
    for id in windows {
        let _ = desktop.dispatch(id, Message::Tick);
    }
    for (n, (bridge, stream)) in streams.into_iter().enumerate() {
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = bridge.clone();
        let worker = std::thread::spawn(move || tx.send(waiting.wait(stream)).unwrap());
        let received = rx.recv_timeout(Duration::from_secs(2));
        if received.is_err() {
            bridge.close();
        }
        worker.join().unwrap();
        assert_eq!(received.unwrap(), Ok(vec![n as u8 + 1]));
    }
}

#[test]
fn revoked_queued_chunk_wakes_the_original_waiter_with_failure() {
    let (mut desktop, sessions, peers) = desktop_fixture();
    let _ = desktop.detach(sessions[0]);
    let child = desktop.owner(sessions[0]);
    let generation = 17;
    let requester = peers[0].remote_offer(&desktop.app.manager, generation);
    let state = &mut desktop.windows.get_mut(&child).unwrap().state;
    state.rdp_file_scope = Some((sessions[0], generation));
    let bridge = state.rdp_chunk_bridge.clone();
    let stream = bridge.begin().unwrap();
    requester.request(stream, 0, 0, 1);
    peers[0].send_event(
        &desktop.app.manager,
        RdpClientEvent::FileContents {
            generation,
            stream_id: stream,
            data: Some(vec![7]),
        },
    );
    desktop.app.manager.poll_events();
    let _replacement = peers[0].remote_offer(&desktop.app.manager, generation + 1);
    let _ = desktop.dispatch(child, Message::Tick);
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = bridge.clone();
    let worker = std::thread::spawn(move || tx.send(waiting.wait(stream)).unwrap());
    let received = rx.recv_timeout(Duration::from_secs(2));
    if received.is_err() {
        bridge.close();
    }
    worker.join().unwrap();
    assert!(received.unwrap().is_err());
}

#[test]
fn unfocused_detached_window_delivers_remote_chunks_without_changing_session() {
    let (mut desktop, sessions, peers) = desktop_fixture();
    let _ = desktop.detach(sessions[0]);
    let child = desktop.owner(sessions[0]);
    desktop.focus(child);

    // The OS clipboard object was already created while this window had focus.
    // This fixture holds its requester and bridge without publishing to the OS.
    let generation = 17;
    let requester = peers[0].remote_offer(&desktop.app.manager, generation);
    let bridge = desktop.windows[&child].state.rdp_chunk_bridge.clone();
    let stream = bridge.begin().unwrap();
    desktop
        .windows
        .get_mut(&child)
        .unwrap()
        .state
        .rdp_file_scope = Some((sessions[0], generation));
    desktop.focus(desktop.primary);
    let active = desktop.app.manager.active_session();
    assert_eq!(active, Some(sessions[2]));
    assert_eq!(desktop.focused, Some(desktop.primary));
    assert!(!desktop.windows[&child].state.desktop_window_focused);

    // Discard unrelated synthetic resize messages produced during detachment.
    while peers[0].try_command().is_some() {}
    while peers[1].try_command().is_some() {}
    while peers[2].try_command().is_some() {}
    requester.request(stream, 0, 0, 3);
    assert!(matches!(
        peers[0].next_command(Duration::from_secs(2)).unwrap(),
        ClientMsg::RequestFileContents { generation: 17, stream_id, index: 0, offset: 0, length: 3 }
            if stream_id == stream
    ));
    peers[0].send_event(
        &desktop.app.manager,
        RdpClientEvent::FileContents {
            generation,
            stream_id: stream,
            data: Some(vec![1, 2, 3]),
        },
    );
    peers[2].send_event(
        &desktop.app.manager,
        RdpClientEvent::ClipboardText("still-owned-by-primary".into()),
    );
    desktop.app.manager.poll_events();
    let _ = desktop.dispatch(child, Message::Tick);

    // Never wait for the bridge's full 30-second deadline on a regression.
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = bridge.clone();
    let worker = std::thread::spawn(move || tx.send(waiting.wait(stream)).unwrap());
    let received = rx.recv_timeout(Duration::from_secs(2));
    if received.is_err() {
        bridge.close();
    }
    worker.join().unwrap();
    assert_eq!(received.unwrap(), Ok(vec![1, 2, 3]));
    assert_eq!(desktop.app.manager.active_session(), active);
    assert_eq!(
        desktop.app.manager.take_rdp_clipboard().as_deref(),
        Some("still-owned-by-primary")
    );
    assert_eq!(desktop.focused, Some(desktop.primary));
    assert!(peers[1].try_command().is_none());
    assert!(peers[2].try_command().is_none());
}

#[test]
fn unfocused_detached_window_rejects_local_reads_and_services_remote_files() {
    let root = std::env::temp_dir().join(format!("adit-two-way-rdp-{}", SessionId::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("synthetic.bin");
    std::fs::write(&path, b"local-data").unwrap();
    let local = clipboard_files::OfferedFile::test_bind(&path);

    let (mut desktop, sessions, peers) = desktop_fixture();
    let _ = desktop.detach(sessions[0]);
    let child = desktop.owner(sessions[0]);
    desktop.focus(child);
    let _ = desktop.in_window(child, |app| {
        app.rdp_offered_files = vec![local];
        Task::none()
    });
    while peers[0].try_command().is_some() {}
    while peers[1].try_command().is_some() {}
    while peers[2].try_command().is_some() {}
    let metadata = desktop.windows[&child].state.rdp_offered_files[0]
        .meta
        .clone();
    peers[0].send_local_offer(&desktop.app.manager, vec![metadata]);
    let local_generation = match peers[0].next_command(Duration::from_secs(2)).unwrap() {
        ClientMsg::ClipboardFiles { generation, files } if files.len() == 1 => generation,
        other => panic!("unexpected offer: {other:?}"),
    };

    let remote_generation = 17;
    let requester = peers[0].remote_offer(&desktop.app.manager, remote_generation);
    let bridge = desktop.windows[&child].state.rdp_chunk_bridge.clone();
    let stream = bridge.begin().unwrap();
    desktop
        .windows
        .get_mut(&child)
        .unwrap()
        .state
        .rdp_file_scope = Some((sessions[0], remote_generation));
    desktop.focus(desktop.primary);
    let active = desktop.app.manager.active_session();
    assert_eq!(active, Some(sessions[2]));
    assert!(!desktop.windows[&child].state.desktop_window_focused);

    while peers[0].try_command().is_some() {}
    while peers[1].try_command().is_some() {}
    while peers[2].try_command().is_some() {}
    requester.request(stream, 0, 0, 3);
    assert!(matches!(
        peers[0].next_command(Duration::from_secs(2)).unwrap(),
        ClientMsg::RequestFileContents { generation: 17, stream_id, .. } if stream_id == stream
    ));
    peers[0].send_event(
        &desktop.app.manager,
        RdpClientEvent::FileContents {
            generation: remote_generation,
            stream_id: stream,
            data: Some(vec![1, 2, 3]),
        },
    );
    peers[0].send_event(
        &desktop.app.manager,
        RdpClientEvent::FileContentsNeeded {
            generation: local_generation,
            stream_id: 99,
            index: 0,
            offset: 1,
            length: 4,
        },
    );
    peers[2].send_event(
        &desktop.app.manager,
        RdpClientEvent::ClipboardText("still-owned-by-primary".into()),
    );
    desktop.app.manager.poll_events();
    let _ = desktop.dispatch(child, Message::Tick);
    assert!(matches!(
        peers[0].next_command(Duration::from_secs(2)).unwrap(),
        ClientMsg::FileContents { generation, stream_id: 99, data: None }
            if generation == local_generation
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = bridge.clone();
    let worker = std::thread::spawn(move || tx.send(waiting.wait(stream)).unwrap());
    let received = rx.recv_timeout(Duration::from_secs(2));
    if received.is_err() {
        bridge.close();
    }
    worker.join().unwrap();
    assert_eq!(received.unwrap(), Ok(vec![1, 2, 3]));
    assert_eq!(desktop.app.manager.active_session(), active);
    assert_eq!(
        desktop.app.manager.take_rdp_clipboard().as_deref(),
        Some("still-owned-by-primary")
    );
    assert_eq!(desktop.focused, Some(desktop.primary));
    assert!(peers[1].try_command().is_none());
    assert!(peers[2].try_command().is_none());
    drop(desktop);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sequence_seen_inside_poll_retires_window_and_parked_file_handles() {
    const CHILD: &str = "ADIT_SYNTHETIC_SEQUENCE_RACE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The production tracker is process-wide. A child gives this test its
        // own tracker even when the test harness runs other tests in parallel.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_windows::clipboard_regression_tests::sequence_seen_inside_poll_retires_window_and_parked_file_handles",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated sequence regression failed: {status}"
        );
        return;
    }

    let root = std::env::temp_dir().join(format!("adit-sequence-race-{}", SessionId::new()));
    std::fs::create_dir(&root).unwrap();
    let paths: Vec<_> = (0..3).map(|n| root.join(format!("{n}.bin"))).collect();
    for (n, path) in paths.iter().enumerate() {
        std::fs::write(path, [n as u8]).unwrap();
    }

    let (mut desktop, sessions, peers) = desktop_fixture();
    // This is the initial native tick's sample, before the local poll races.
    desktop.revoke_stale_local_file_offers(41);
    for peer in &peers {
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(1)),
            Ok(Some(ClientMsg::LocalFileOfferRejected))
        ));
    }
    let parked = clipboard_files::OfferedFile::test_bind(&paths[2]);
    let primary = clipboard_files::OfferedFile::test_bind(&paths[1]);
    let detached = clipboard_files::OfferedFile::test_bind(&paths[0]);
    let alive = [
        parked.test_is_bound(),
        primary.test_is_bound(),
        detached.test_is_bound(),
    ];
    let metadata = [
        detached.meta.clone(),
        primary.meta.clone(),
        parked.meta.clone(),
    ];
    for (peer, file) in peers.iter().zip(metadata) {
        peer.send_local_offer(&desktop.app.manager, vec![file]);
        assert!(matches!(
            peer.write_next_command(&desktop.app.manager, Duration::from_secs(2)),
            Ok(Some(ClientMsg::ClipboardFiles { generation, files })) if generation != 0 && files.len() == 1
        ));
    }
    desktop.app.rdp_offered_files = vec![parked];
    let _ = desktop.dispatch(desktop.primary, Message::TabClicked(sessions[1]));
    desktop.app.rdp_offered_files = vec![primary];
    let _ = desktop.detach(sessions[0]);
    let child = desktop.owner(sessions[0]);
    let _ = desktop.in_window(child, |app| {
        app.rdp_offered_files = vec![detached];
        Task::none()
    });
    assert!(desktop.clipboard_leases.contains_key(&sessions[2]));
    assert!(alive.iter().all(|is_bound| is_bound()));
    let active = desktop.app.manager.active_session();

    // The second sample, inside the focused poll, sees the replacement first.
    // Feed it directly to the same tracker without opening the real clipboard.
    update_loop::observe_local_clipboard_sequence(&mut desktop.app, 42);
    let (result, superseded) = clipboard_files::poll_local_offer(
        sessions[1],
        desktop.app.desktop_clipboard_epoch,
        42,
        None,
        &sessions,
    );
    assert!(
        !superseded,
        "the sampling path already recorded the sequence"
    );
    assert!(matches!(result, clipboard_files::OfferPoll::Pending));
    desktop.revoke_stale_local_file_offers(42);
    assert!(desktop.app.rdp_offered_files.is_empty());
    assert!(desktop.windows[&child].state.rdp_offered_files.is_empty());
    assert!(desktop.clipboard_leases[&sessions[2]].test_local_files_empty());
    assert!(alive.iter().all(|is_bound| !is_bound()));
    assert_eq!(desktop.app.manager.active_session(), active);
    for peer in &peers {
        let mut revoked = false;
        while let Some(command) = peer.try_command() {
            revoked |= matches!(command, ClientMsg::ClipboardFiles { generation: 0, ref files } if files.is_empty())
                || matches!(command, ClientMsg::LocalFileOfferRejected);
        }
        assert!(revoked, "every helper must revoke the old file offer");
    }

    drop(desktop);
    for _ in 0..100 {
        if std::fs::remove_dir_all(&root).is_ok() || !root.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("could not remove synthetic fixture {root:?}");
}
