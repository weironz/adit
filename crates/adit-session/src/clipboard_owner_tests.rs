use super::*;

#[test]
fn disabling_clipboard_rejects_each_sessions_queued_and_late_reads() {
    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
    ];
    let ids = [profiles[0].id, profiles[1].id];
    let mut manager = SessionManager::with_profiles(profiles);
    let a = manager.open_mock_session(ids[0]).unwrap();
    let b = manager.open_mock_session(ids[1]).unwrap();
    let (handle_a, rx_a, events_a) = RdpClientHandle::test_fixture_with_channels();
    let (handle_b, rx_b, events_b) = RdpClientHandle::test_fixture_with_channels();
    manager.sessions.get_mut(&a).unwrap().rdp = Some(handle_a);
    manager.sessions.get_mut(&b).unwrap().rdp = Some(handle_b);
    manager.activate(b).unwrap();
    let read = RdpFileRead {
        stream_id: 7,
        index: 0,
        offset: 0,
        length: 1,
    };
    manager.pending_rdp_file_reads = vec![(a, read), (b, read)];
    manager.set_rdp_clipboard(false);
    for rx in [&rx_a, &rx_b] {
        assert!(matches!(
            rx.try_recv(),
            Ok(adit_rdp_proto::ClientMsg::FileContents {
                stream_id: 7,
                data: None
            })
        ));
    }
    assert!(manager.pending_rdp_file_reads.is_empty());
    for tx in [events_a, events_b] {
        tx.send(RdpClientEvent::FileContentsNeeded {
            stream_id: 7,
            index: 0,
            offset: 0,
            length: 1,
        })
        .unwrap();
    }
    manager.poll_events();
    for rx in [&rx_a, &rx_b] {
        assert!(matches!(
            rx.try_recv(),
            Ok(adit_rdp_proto::ClientMsg::FileContents {
                stream_id: 7,
                data: None
            })
        ));
    }
    assert!(manager.pending_rdp_file_reads.is_empty());
    assert_eq!(manager.active_session, Some(b));
}

#[test]
fn enabled_clipboard_refuses_old_helper_reads_on_the_originating_session() {
    let a = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let b = ConnectionProfile::new("b", "b.invalid", 22, "fixture");
    let ids = [a.id, b.id];
    let mut manager = SessionManager::with_profiles(vec![a, b]);
    let a = manager.open_mock_session(ids[0]).unwrap();
    let b = manager.open_mock_session(ids[1]).unwrap();
    let (handle_a, rx_a, events_a) = RdpClientHandle::test_fixture_with_channels();
    let (handle_b, rx_b, events_b) = RdpClientHandle::test_fixture_with_channels();
    manager.sessions.get_mut(&a).unwrap().rdp = Some(handle_a);
    manager.sessions.get_mut(&b).unwrap().rdp = Some(handle_b);
    manager.set_rdp_clipboard(true);
    manager.activate(b).unwrap();
    for tx in [events_a, events_b] {
        tx.send(RdpClientEvent::FileContentsNeeded {
            stream_id: 9,
            index: 0,
            offset: 0,
            length: 7,
        })
        .unwrap();
    }
    manager.poll_events();
    for rx in [&rx_a, &rx_b] {
        assert!(matches!(
            rx.try_recv(),
            Ok(adit_rdp_proto::ClientMsg::FileContents {
                stream_id: 9,
                data: None
            })
        ));
    }
    assert!(manager.pending_rdp_file_reads.is_empty());
    assert_eq!(manager.active_session, Some(b));
    for id in [a, b] {
        assert_eq!(
            manager.sessions[&id].summary.status,
            SessionStatus::Connected
        );
    }
}

#[test]
fn legacy_session_api_cannot_send_file_metadata_or_bytes() {
    let (handle, rx, _) = RdpClientHandle::test_fixture_with_channels();
    handle.send_clipboard_files(vec![ClipFile {
        name: "private.txt".into(),
        size: 7,
        is_dir: false,
    }]);
    assert!(rx.try_recv().is_err());
    handle.answer_file_contents(4, Some(b"private".to_vec()));
    assert!(matches!(
        rx.try_recv(),
        Ok(adit_rdp_proto::ClientMsg::FileContents {
            stream_id: 4,
            data: None
        })
    ));
    handle.send_clipboard_files(Vec::new());
    assert!(
        matches!(rx.try_recv(), Ok(adit_rdp_proto::ClientMsg::ClipboardFiles(files))
        if files.is_empty())
    );
}

fn attach_test_rdp(manager: &mut SessionManager, session: SessionId) -> RdpFileRequester {
    let handle = RdpClientHandle::test_fixture();
    let requester = handle.file_requester_for_generation(1);
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    requester
}

#[test]
fn clipboard_stream_ids_and_payloads_cannot_cross_native_window_sessions() {
    let a = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let b = ConnectionProfile::new("b", "b.invalid", 22, "fixture");
    let (a_id, b_id) = (a.id, b.id);
    let mut manager = SessionManager::with_profiles(vec![a, b]);
    let a = manager.open_mock_session(a_id).unwrap();
    let b = manager.open_mock_session(b_id).unwrap();
    let a_requester = attach_test_rdp(&mut manager, a);
    let b_requester = attach_test_rdp(&mut manager, b);
    // Different servers routinely reuse the same stream IDs.
    manager.pending_rdp_file_chunks = vec![
        RdpFileChunk {
            session: a,
            generation: 1,
            stream_id: 1,
            data: Some(vec![10]),
            owner: a_requester.clone(),
        },
        RdpFileChunk {
            session: b,
            generation: 1,
            stream_id: 1,
            data: Some(vec![20]),
            owner: b_requester.clone(),
        },
    ];
    let read = RdpFileRead {
        stream_id: 1,
        index: 0,
        offset: 0,
        length: 1,
    };
    manager.pending_rdp_file_reads = vec![(a, read), (b, read)];
    manager.pending_rdp_clipboard_image = Some((a, vec![30]));
    manager.pending_rdp_clipboard_files = Some(RdpClipboardFileOffer {
        session: a,
        generation: 1,
        files: Vec::new(),
        requester: a_requester.clone(),
    });
    manager.activate(b).unwrap();
    let chunks = manager.take_rdp_file_chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].session, chunks[0].stream_id), (b, 1));
    assert_eq!(chunks[0].data, Some(vec![20]));
    assert_eq!(manager.take_rdp_file_reads(), vec![read]);
    assert!(manager.take_rdp_clipboard_image().is_none());
    assert!(manager.take_rdp_clipboard_files().is_none());
    assert!(manager.take_rdp_file_reads().is_empty());
    manager.activate(a).unwrap();
    let chunks = manager.take_rdp_file_chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].session, chunks[0].stream_id), (a, 1));
    assert_eq!(chunks[0].data, Some(vec![10]));
    assert_eq!(manager.take_rdp_file_reads(), vec![read]);
    assert_eq!(manager.take_rdp_clipboard_image(), Some(vec![30]));
    assert!(manager.take_rdp_clipboard_files().is_some());
    assert!(manager.pending_rdp_file_chunks.is_empty());
    assert!(manager.pending_rdp_file_reads.is_empty());
}

#[test]
fn temporary_window_projection_preserves_pending_offers_and_focus_retirement_keeps_file_streams() {
    let a = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let b = ConnectionProfile::new("b", "b.invalid", 22, "fixture");
    let (a_id, b_id) = (a.id, b.id);
    let mut manager = SessionManager::with_profiles(vec![a, b]);
    let a = manager.open_mock_session(a_id).unwrap();
    let b = manager.open_mock_session(b_id).unwrap();
    let requester = attach_test_rdp(&mut manager, a);
    manager.pending_rdp_clipboard = Some((a, PendingDesktopClipboard::Rdp("fixture-a".into())));
    manager.select_session_context(b).unwrap();
    assert!(manager.take_rdp_clipboard().is_none());
    manager.select_session_context(a).unwrap();
    assert_eq!(manager.take_rdp_clipboard().as_deref(), Some("fixture-a"));
    manager.pending_rdp_clipboard = Some((a, PendingDesktopClipboard::Rdp("retired".into())));
    manager.pending_rdp_file_chunks.push(RdpFileChunk {
        session: a,
        generation: 1,
        stream_id: 1,
        data: Some(vec![7]),
        owner: requester,
    });
    manager.discard_clipboard_offers(a);
    assert!(manager.take_rdp_clipboard().is_none());
    let chunks = manager.take_rdp_file_chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        (chunks[0].stream_id, chunks[0].data.clone()),
        (1, Some(vec![7]))
    );
}
