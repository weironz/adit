use super::*;

#[cfg(feature = "test-fixtures")]
#[test]
fn unavailable_or_missing_clipboard_channel_is_never_reported_confirmed() {
    use crate::clipboard_test_fixture::SyntheticRdp;
    let profile = ConnectionProfile::new("policy", "fixture.invalid", 22, "fixture");
    let mut manager = SessionManager::with_profiles(vec![profile.clone()]);
    let session = manager.open_mock_session(profile.id).unwrap();
    assert_eq!(manager.rdp_clipboard_policy_status(session), None);
    let peer = SyntheticRdp::attach(&mut manager, session);
    peer.prepare_policy(&manager).unwrap();
    peer.policy_channel_unavailable(&manager);
    peer.acknowledge_policy(&manager, 0, true);
    peer.policy_channel_ready(&manager, 1);
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Failed {
            epoch: 0,
            enabled: true,
            reason: RdpClipboardPolicyFailure::ChannelNotNegotiated,
        })
    );
}

#[cfg(feature = "test-fixtures")]
#[test]
fn policy_status_buffers_new_offers_and_ignores_late_confirmation() {
    use crate::clipboard_test_fixture::SyntheticRdp;
    let profile = ConnectionProfile::new("policy", "fixture.invalid", 22, "fixture");
    let mut manager = SessionManager::with_profiles(vec![profile.clone()]);
    let session = manager.open_mock_session(profile.id).unwrap();
    manager.activate(session).unwrap();
    let peer = SyntheticRdp::attach(&mut manager, session);
    assert_eq!(peer.prepare_policy(&manager).unwrap(), 0);
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Pending {
            epoch: 0,
            enabled: true
        })
    );
    peer.acknowledge_policy(&manager, 0, true);
    assert!(matches!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Pending { .. })
    ));
    peer.policy_channel_ready(&manager, 1);
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Confirmed {
            epoch: 0,
            enabled: true
        })
    );
    manager.set_rdp_clipboard(false);
    manager.set_rdp_clipboard(true);
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Pending {
            epoch: 2,
            enabled: true
        })
    );
    let file = |name: &str| {
        vec![ClipFile {
            name: name.into(),
            size: 1,
            is_dir: false,
        }]
    };
    peer.offer_remote_files(&manager, 0, 7, file("old.bin"));
    peer.offer_remote_files(&manager, 2, 8, file("new-before-ack.bin"));
    peer.acknowledge_policy(&manager, 1, false);
    manager.poll_events();
    assert!(manager.take_rdp_clipboard_files().is_none());
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Pending {
            epoch: 2,
            enabled: true
        })
    );
    peer.acknowledge_policy(&manager, 2, true);
    manager.poll_events();
    assert_eq!(
        manager.take_rdp_clipboard_files().unwrap().files[0].name,
        "new-before-ack.bin"
    );
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Confirmed {
            epoch: 2,
            enabled: true
        })
    );

    manager.set_rdp_clipboard(false);
    manager.set_rdp_clipboard(true);
    peer.offer_remote_files(&manager, 4, 9, file("after-timeout.bin"));
    peer.timeout_policy(&manager);
    assert_eq!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Failed {
            epoch: 4,
            enabled: true,
            reason: RdpClipboardPolicyFailure::ConfirmationTimedOut
        })
    );
    peer.acknowledge_policy(&manager, 2, true);
    assert!(matches!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Failed {
            reason: RdpClipboardPolicyFailure::ConfirmationTimedOut,
            ..
        })
    ));
    peer.acknowledge_policy(&manager, 4, true);
    manager.poll_events();
    assert_eq!(
        manager.take_rdp_clipboard_files().unwrap().files[0].name,
        "after-timeout.bin"
    );
    peer.reject_policy(&manager, 3, RdpClipboardPolicyFailure::HelperRejected);
    assert!(matches!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Confirmed { epoch: 4, .. })
    ));
    peer.reject_policy(&manager, 4, RdpClipboardPolicyFailure::HelperRejected);
    assert!(matches!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Failed {
            reason: RdpClipboardPolicyFailure::HelperRejected,
            ..
        })
    ));
    peer.disconnect_policy(&manager);
    assert!(matches!(
        manager.rdp_clipboard_policy_status(session),
        Some(RdpClipboardPolicyStatus::Failed {
            reason: RdpClipboardPolicyFailure::HelperDisconnected,
            ..
        })
    ));
}

#[test]
fn global_clipboard_observation_freezes_unfocused_and_connecting_sessions() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
        ConnectionProfile::new("other", "other.invalid", 22, "fixture"),
    ];
    let mut manager = SessionManager::with_profiles(profiles.clone());
    let a = manager.open_mock_session(profiles[0].id).unwrap();
    let b = manager.open_mock_session(profiles[1].id).unwrap();
    let other = manager.open_mock_session(profiles[2].id).unwrap();
    let (first, first_commands) = RdpClientHandle::test_fixture_with_commands();
    let (second, second_commands) = RdpClientHandle::test_fixture_with_commands();
    manager.sessions.get_mut(&a).unwrap().rdp = Some(first);
    manager.sessions.get_mut(&b).unwrap().rdp = Some(second);
    let source = Arc::new(AtomicU32::new(10));
    manager.activate(a).unwrap();
    let reader = Arc::clone(&source);
    manager.offer_clipboard_to_active_rdp_with_sequence(
        "old".into(),
        RdpClipboardSequence::new(10, move || reader.load(Ordering::Acquire)),
    );
    manager.activate(b).unwrap();
    let reader = Arc::clone(&source);
    manager.offer_image_to_active_rdp_with_sequence(
        vec![1, 2, 3],
        RdpClipboardSequence::new(10, move || reader.load(Ordering::Acquire)),
    );
    manager.sessions.get_mut(&b).unwrap().summary.status = SessionStatus::Connecting;
    manager.activate(other).unwrap();
    source.store(0, Ordering::Release);
    manager.observe_local_clipboard_sequence(0);
    for (session, commands) in [(a, first_commands), (b, second_commands)] {
        let handle = manager.sessions[&session].rdp.as_ref().unwrap();
        assert!(handle
            .test_write_optional_command(commands.try_recv().unwrap())
            .is_none());
        assert!(matches!(
            handle.test_write_optional_command(commands.try_recv().unwrap()),
            Some(ClientMsg::LocalFileOfferRejected)
        ));
        assert!(commands.try_recv().is_err());
    }
    assert_eq!(manager.active_session(), Some(other));
}

#[test]
fn local_file_selection_notifies_all_live_rdp_helpers_without_changing_tabs() {
    use adit_rdp_proto::ClientMsg;

    let profiles: Vec<_> = (0..5)
        .map(|index| {
            ConnectionProfile::new(format!("fixture-{index}"), "fixture.invalid", 22, "fixture")
        })
        .collect();
    let mut manager = SessionManager::with_profiles(profiles.clone());
    let sessions: Vec<_> = profiles
        .iter()
        .map(|profile| manager.open_mock_session(profile.id).unwrap())
        .collect();
    let (active, active_commands) = RdpClientHandle::test_fixture_with_commands();
    let old_lease = active.test_local_offer();
    let (connecting, connecting_commands) = RdpClientHandle::test_fixture_with_commands();
    connecting.test_suspend_local_files();
    let (disconnected, disconnected_commands) = RdpClientHandle::test_fixture_with_commands();
    let (failed, failed_commands) = RdpClientHandle::test_fixture_with_commands();
    manager.sessions.get_mut(&sessions[0]).unwrap().rdp = Some(active);
    manager.sessions.get_mut(&sessions[1]).unwrap().rdp = Some(connecting);
    manager
        .sessions
        .get_mut(&sessions[1])
        .unwrap()
        .summary
        .status = SessionStatus::Connecting;
    manager.sessions.get_mut(&sessions[2]).unwrap().rdp = Some(disconnected);
    manager
        .sessions
        .get_mut(&sessions[2])
        .unwrap()
        .summary
        .status = SessionStatus::Disconnected;
    manager.sessions.get_mut(&sessions[3]).unwrap().rdp = Some(failed);
    manager
        .sessions
        .get_mut(&sessions[3])
        .unwrap()
        .summary
        .status = SessionStatus::Error;
    // A different active tab and a disabled sharing preference must not hide
    // stale content on helpers that are still alive.
    manager.activate(sessions[4]).unwrap();
    manager.rdp_clipboard = false;
    let selected = manager.active_session();

    manager.notify_local_file_selection_to_all_rdp();

    assert_eq!(manager.active_session(), selected);
    assert!(!old_lease.is_current());
    assert!(matches!(
        active_commands.try_recv(),
        Ok(ClientMsg::LocalFileOfferRejected)
    ));
    assert!(matches!(
        connecting_commands.try_recv(),
        Ok(ClientMsg::LocalFileOfferRejected)
    ));
    assert!(active_commands.try_recv().is_err());
    assert!(connecting_commands.try_recv().is_err());
    assert!(disconnected_commands.try_recv().is_err());
    assert!(failed_commands.try_recv().is_err());
    assert_eq!(
        manager.sessions[&sessions[0]].summary.status,
        SessionStatus::Connected
    );
    assert_eq!(
        manager.sessions[&sessions[1]].summary.status,
        SessionStatus::Connecting
    );
    manager.active_session = None;
    manager.notify_local_file_selection_to_all_rdp();
    assert_eq!(manager.active_session(), None);
    assert!(active_commands.try_recv().is_err());
    assert!(connecting_commands.try_recv().is_err());
}

#[test]
fn inactive_desktop_notice_is_retained_until_its_tab_is_active() {
    let first = ConnectionProfile::new("first", "first.invalid", 22, "fixture");
    let second = ConnectionProfile::new("second", "second.invalid", 22, "fixture");
    let mut manager = SessionManager::with_profiles(vec![first.clone(), second.clone()]);
    let a = manager.open_mock_session(first.id).unwrap();
    let b = manager.open_mock_session(second.id).unwrap();
    let before = manager.sessions[&a].summary.status;
    let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
    handle.test_send_event(RdpClientEvent::ClipboardFileDirectionUnavailable {
        channel_epoch: 7,
        message: "本地文件发送至远端已暂时禁用".into(),
    });
    manager.sessions.get_mut(&a).unwrap().rdp = Some(handle);
    manager.activate(b).unwrap();
    manager.poll_events();
    assert_eq!(manager.sessions[&a].summary.status, before);
    assert!(commands.try_recv().is_err());
    assert!(manager.take_desktop_notice().is_none());
    manager.activate(a).unwrap();
    manager.pending_trigger_notices.push_back("trigger".into());
    assert!(manager.take_desktop_notice().is_none());
    assert_eq!(manager.take_trigger_notice().as_deref(), Some("trigger"));
    assert!(manager
        .take_desktop_notice()
        .is_some_and(|text| text.contains("暂时禁用")));
    assert!(manager.take_desktop_notice().is_none());
}

#[test]
fn completed_stream_cannot_be_reused_while_either_upper_layer_queue_holds_its_reply() {
    use adit_rdp_proto::ClientMsg;

    let profile = ConnectionProfile::new("fixture", "fixture.invalid", 22, "fixture");
    let mut manager = SessionManager::with_profiles(vec![profile.clone()]);
    let session = manager.open_mock_session(profile.id).unwrap();
    let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
    let requester = handle.test_remote_file_offer(7);
    assert_eq!(requester.try_request(41, 0, 0, 1), Ok(()));
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::RequestFileContents { stream_id: 41, .. })
    ));
    handle.test_complete_remote_request(7, 41, Some(vec![9]));
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    manager.activate(session).unwrap();
    manager.poll_events();

    assert_eq!(
        requester.try_request(41, 1, 0, 1),
        Err(RdpFileRequestError::DuplicateStream)
    );
    let chunks = manager.take_rdp_file_chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].data.as_deref(), Some(&[9][..]));
    drop(chunks);
    assert_eq!(
        requester.try_request(41, 1, 0, 1),
        Err(RdpFileRequestError::DuplicateStream)
    );
    let drag = manager.take_rdp_file_drag_events();
    assert!(
        matches!(drag.as_slice(), [RdpFileDragEvent::Chunk { stream_id: 41, data: Some(data), .. }] if data == &[9])
    );
    drop(drag);
    assert_eq!(
        requester.try_request(41, 1, 0, 1),
        Err(RdpFileRequestError::DuplicateStream)
    );
    assert_eq!(requester.try_request(42, 1, 0, 1), Ok(()));
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::RequestFileContents { stream_id: 42, .. })
    ));
    assert!(commands.try_recv().is_err());
}

#[test]
fn gui_file_reads_retire_old_channels_when_streams_are_reused() {
    use adit_rdp_proto::{ClientMsg, ClipboardFileRequestId};

    let profile = ConnectionProfile::new("fixture", "fixture.invalid", 22, "fixture");
    let profile_id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(profile_id).unwrap();
    let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
    handle.test_local_offer();
    let generation = handle.test_local_generation();
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    manager.activate(session).unwrap();

    let old = ClipboardFileRequestId {
        channel_epoch: 10,
        snapshot_revision: 3,
        generation,
        stream_id: 41,
    };
    let new = ClipboardFileRequestId {
        channel_epoch: 11,
        ..old
    };
    for request in [old, new] {
        manager.sessions[&session]
            .rdp
            .as_ref()
            .unwrap()
            .test_send_event(RdpClientEvent::FileContentsNeededBound {
                request,
                index: 0,
                offset: 0,
                length: 4,
            });
    }
    manager.poll_events();
    let reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 1);
    reads.into_iter().next().unwrap().answer(Some(vec![4]));
    assert!(
        matches!(commands.try_recv(), Ok(ClientMsg::BoundFileContents { request: got, data: Some(data) })
        if got == new && data == [4])
    );
    assert!(commands.try_recv().is_err());
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
    manager.pending_rdp_file_reads = [a, b]
        .into_iter()
        .map(|session| PendingRdpFileRead {
            session,
            read: RdpFileRead {
                generation: 1,
                stream_id: 1,
                index: 0,
                offset: 0,
                length: 1,
                lease: manager.sessions[&session]
                    .rdp
                    .as_ref()
                    .unwrap()
                    .test_local_offer(),
                answered: false,
            },
        })
        .collect();
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
    let reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!((reads[0].stream_id, reads[0].index), (1, 0));
    assert!(manager.take_rdp_clipboard_image().is_none());
    assert!(manager.take_rdp_clipboard_files().is_none());
    assert!(manager.take_rdp_file_reads().is_empty());
    manager.activate(a).unwrap();
    let chunks = manager.take_rdp_file_chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].session, chunks[0].stream_id), (a, 1));
    assert_eq!(chunks[0].data, Some(vec![10]));
    let reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!((reads[0].stream_id, reads[0].index), (1, 0));
    assert_eq!(manager.take_rdp_clipboard_image(), Some(vec![30]));
    assert!(manager.take_rdp_clipboard_files().is_some());
    assert!(manager.pending_rdp_file_chunks.is_empty());
    assert!(manager.pending_rdp_file_reads.is_empty());
}

#[test]
fn disabling_file_sharing_refuses_queued_requests_for_each_helper_and_retires_old_epochs() {
    use adit_rdp_proto::ClientMsg;

    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
    ];
    let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let a = manager.open_mock_session(ids[0]).unwrap();
    let b = manager.open_mock_session(ids[1]).unwrap();
    let (a_handle, a_commands) = RdpClientHandle::test_fixture_with_commands();
    let (b_handle, b_commands) = RdpClientHandle::test_fixture_with_commands();
    manager.sessions.get_mut(&a).unwrap().rdp = Some(a_handle);
    manager.sessions.get_mut(&b).unwrap().rdp = Some(b_handle);
    let file = ClipFile {
        name: "synthetic.bin".into(),
        size: 4,
        is_dir: false,
    };
    let mut generations = Vec::new();
    for session in [a, b] {
        manager.activate(session).unwrap();
        manager.offer_rdp_files_to_active(
            vec![file.clone()],
            Some(RdpClipboardSequence::new(1, || 1)),
        );
        let handle = manager.sessions[&session].rdp.as_ref().unwrap();
        let generation = handle.test_local_generation();
        generations.push(generation);
        handle.test_send_event(RdpClientEvent::FileContentsNeeded {
            generation,
            stream_id: 7,
            index: 0,
            offset: 0,
            length: 4,
        });
    }
    assert!(
        matches!(a_commands.try_recv(), Ok(ClientMsg::ClipboardFiles { generation, files }) if generation == generations[0] && files == vec![file.clone()])
    );
    assert!(
        matches!(b_commands.try_recv(), Ok(ClientMsg::ClipboardFiles { generation, files }) if generation == generations[1] && files == vec![file.clone()])
    );
    manager.poll_events();
    assert_eq!(manager.pending_rdp_file_reads.len(), 2);

    manager.set_rdp_clipboard(false);
    for commands in [&a_commands, &b_commands] {
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::ClipboardPolicyEpoch(0))
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::LocalFileOfferRejected)
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(ClientMsg::FileContents {
                stream_id: 7,
                data: None,
                ..
            })
        ));
        assert!(commands.try_recv().is_err());
    }
    assert!(manager.pending_rdp_file_reads.is_empty());

    // This event was captured before revocation, but reaches the manager only
    // after a new offer in the same session has been published.
    let handle = manager.sessions[&a].rdp.as_ref().unwrap();
    handle.test_send_event(RdpClientEvent::FileContentsNeeded {
        generation: generations[0],
        stream_id: 8,
        index: 0,
        offset: 0,
        length: 4,
    });
    assert!(matches!(
        a_commands.try_recv(),
        Ok(ClientMsg::FileContents {
            stream_id: 8,
            data: None,
            generation,
        }) if generation == generations[0]
    ));
    manager.set_rdp_clipboard(true);
    manager.activate(a).unwrap();
    manager.offer_rdp_files_to_active(vec![file], Some(RdpClipboardSequence::new(1, || 1)));
    let handle = manager.sessions[&a].rdp.as_ref().unwrap();
    let fresh = handle.test_local_generation();
    assert_ne!(fresh, generations[0]);
    handle.test_send_event(RdpClientEvent::FileContentsNeeded {
        generation: fresh,
        stream_id: 9,
        index: 0,
        offset: 0,
        length: 4,
    });
    assert!(matches!(
        a_commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { generation, .. }) if generation == fresh
    ));
    manager.poll_events();
    let reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].stream_id, 9);
    assert!(b_commands.try_recv().is_err());
}

#[test]
fn replacing_an_offer_invalidates_its_queued_file_reads() {
    use adit_rdp_proto::ClientMsg;

    let profile = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(id).unwrap();
    let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    manager.activate(session).unwrap();
    let file = ClipFile {
        name: "synthetic.bin".into(),
        size: 4,
        is_dir: false,
    };
    manager.offer_rdp_files_to_active(vec![file.clone()], Some(RdpClipboardSequence::new(1, || 1)));
    let handle = manager.sessions[&session].rdp.as_ref().unwrap();
    handle.test_send_event(RdpClientEvent::FileContentsNeeded {
        generation: handle.test_local_generation(),
        stream_id: 10,
        index: 0,
        offset: 0,
        length: 4,
    });
    manager.poll_events();
    manager.offer_rdp_files_to_active(vec![file], Some(RdpClipboardSequence::new(1, || 1)));
    assert!(manager.take_rdp_file_reads().is_empty());
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { .. })
    ));
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { .. })
    ));
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::FileContents {
            stream_id: 10,
            data: None,
            ..
        })
    ));
}

#[test]
fn a_taken_read_replies_to_its_original_helper_and_respects_later_revocation() {
    use adit_rdp_proto::ClientMsg;

    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
    ];
    let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let a = manager.open_mock_session(ids[0]).unwrap();
    let b = manager.open_mock_session(ids[1]).unwrap();
    let (a_handle, a_commands) = RdpClientHandle::test_fixture_with_commands();
    let (b_handle, b_commands) = RdpClientHandle::test_fixture_with_commands();
    manager.sessions.get_mut(&a).unwrap().rdp = Some(a_handle);
    manager.sessions.get_mut(&b).unwrap().rdp = Some(b_handle);
    manager.activate(a).unwrap();
    let file = ClipFile {
        name: "synthetic.bin".into(),
        size: 1,
        is_dir: false,
    };
    manager.offer_rdp_files_to_active(vec![file.clone()], Some(RdpClipboardSequence::new(1, || 1)));
    let handle = manager.sessions[&a].rdp.as_ref().unwrap();
    let generation = handle.test_local_generation();
    for stream_id in [1, 2] {
        handle.test_send_event(RdpClientEvent::FileContentsNeeded {
            generation,
            stream_id,
            index: 0,
            offset: 0,
            length: 1,
        });
    }
    manager.poll_events();
    let mut reads = manager.take_rdp_file_reads().into_iter();
    let first = reads.next().unwrap();
    let second = reads.next().unwrap();
    manager.activate(b).unwrap();
    first.answer(Some(vec![42]));
    assert!(matches!(
        a_commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { .. })
    ));
    assert!(matches!(a_commands.try_recv(), Ok(ClientMsg::FileContents {
        generation: got, stream_id: 1, data: Some(data)
    }) if got == generation && data == vec![42]));
    assert!(b_commands.try_recv().is_err());

    manager.set_rdp_clipboard(false);
    manager.set_rdp_clipboard(true);
    manager.activate(a).unwrap();
    manager.offer_rdp_files_to_active(vec![file], Some(RdpClipboardSequence::new(1, || 1)));
    second.answer(Some(vec![99]));
    assert!(matches!(
        a_commands.try_recv(),
        Ok(ClientMsg::ClipboardPolicyEpoch(0))
    ));
    assert!(matches!(
        a_commands.try_recv(),
        Ok(ClientMsg::LocalFileOfferRejected)
    ));
    assert!(matches!(
        a_commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { .. })
    ));
    assert!(matches!(a_commands.try_recv(), Ok(ClientMsg::FileContents {
        generation: got, stream_id: 2, data: None
    }) if got == generation));
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

fn synthetic_read(handle: &RdpClientHandle, generation: u64, stream_id: u32) {
    handle.test_send_event(RdpClientEvent::FileContentsNeeded {
        generation,
        stream_id,
        index: 0,
        offset: 0,
        length: 1,
    });
}

#[test]
fn active_session_changes_keep_queued_replies_bound_to_their_sequence() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
    ];
    let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let first = manager.open_mock_session(ids[0]).unwrap();
    let second = manager.open_mock_session(ids[1]).unwrap();
    let (first_handle, first_commands) = RdpClientHandle::test_fixture_with_commands();
    let (second_handle, second_commands) = RdpClientHandle::test_fixture_with_commands();
    manager.sessions.get_mut(&first).unwrap().rdp = Some(first_handle);
    manager.sessions.get_mut(&second).unwrap().rdp = Some(second_handle);
    let first_sequence = Arc::new(AtomicU32::new(10));
    let second_sequence = Arc::new(AtomicU32::new(20));
    let file = ClipFile {
        name: "synthetic.bin".into(),
        size: 1,
        is_dir: false,
    };

    for (session, current, sequence) in [
        (first, Arc::clone(&first_sequence), 10),
        (second, Arc::clone(&second_sequence), 20),
    ] {
        manager.activate(session).unwrap();
        manager.offer_rdp_files_to_active(
            vec![file.clone()],
            Some(RdpClipboardSequence::new(sequence, move || {
                current.load(Ordering::Acquire)
            })),
        );
    }
    assert!(matches!(
        first_commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { generation: 1, .. })
    ));
    assert!(matches!(
        second_commands.try_recv(),
        Ok(ClientMsg::ClipboardFiles { generation: 1, .. })
    ));
    for session in [first, second] {
        let handle = manager.sessions[&session].rdp.as_ref().unwrap();
        synthetic_read(handle, handle.test_local_generation(), 9);
    }
    manager.poll_events();
    for (session, byte) in [(first, 11), (second, 22)] {
        manager.activate(session).unwrap();
        let reads = manager.take_rdp_file_reads();
        assert_eq!(reads.len(), 1);
        reads.into_iter().next().unwrap().answer(Some(vec![byte]));
    }
    let first_reply = first_commands.try_recv().unwrap();
    let second_reply = second_commands.try_recv().unwrap();
    first_sequence.store(30, Ordering::Release);
    let first_handle = manager.sessions[&first].rdp.as_ref().unwrap();
    let second_handle = manager.sessions[&second].rdp.as_ref().unwrap();
    assert!(matches!(
        first_handle.test_write_command(first_reply),
        ClientMsg::FileContents {
            stream_id: 9,
            data: None,
            ..
        }
    ));
    assert!(matches!(
        second_handle.test_write_command(second_reply),
        ClientMsg::FileContents { stream_id: 9, data: Some(data), .. } if data == [22]
    ));
}

#[test]
fn unfocused_sessions_share_a_global_budget_but_reply_to_their_own_streams() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::Arc;

    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
    ];
    let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let a = manager.open_mock_session(ids[0]).unwrap();
    let b = manager.open_mock_session(ids[1]).unwrap();
    let budget = Arc::new(rdp_client::InboundReadBudget::new(2, 32));
    let (a_handle, a_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    let (b_handle, b_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    a_handle.test_local_offer();
    b_handle.test_local_offer();
    let a_generation = a_handle.test_local_generation();
    let b_generation = b_handle.test_local_generation();
    manager.sessions.get_mut(&a).unwrap().rdp = Some(a_handle);
    manager.sessions.get_mut(&b).unwrap().rdp = Some(b_handle);
    manager.activate(b).unwrap();

    // The same numeric stream is legal on two different helpers.
    for (session, generation) in [(a, a_generation), (b, b_generation)] {
        synthetic_read(
            manager.sessions[&session].rdp.as_ref().unwrap(),
            generation,
            7,
        );
    }
    manager.poll_events();
    assert_eq!(budget.test_counts(), (2, 0));
    assert_eq!(manager.pending_rdp_file_reads.len(), 2);
    for (session, generation) in [(a, a_generation), (b, b_generation)] {
        synthetic_read(
            manager.sessions[&session].rdp.as_ref().unwrap(),
            generation,
            8,
        );
    }
    let a_rejection = a_commands.try_recv().unwrap();
    let b_rejection = b_commands.try_recv().unwrap();
    assert!(
        matches!(a_rejection, ClientMsg::FileContents { generation, stream_id: 8, data: None } if generation == a_generation)
    );
    assert!(
        matches!(b_rejection, ClientMsg::FileContents { generation, stream_id: 8, data: None } if generation == b_generation)
    );
    assert_eq!(budget.test_counts(), (2, 2));
    manager.sessions[&a]
        .rdp
        .as_ref()
        .unwrap()
        .test_write_command(a_rejection);
    manager.sessions[&b]
        .rdp
        .as_ref()
        .unwrap()
        .test_write_command(b_rejection);
    assert_eq!(budget.test_counts(), (2, 0));

    manager.activate(a).unwrap();
    let mut reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 1);
    reads.pop().unwrap().answer(Some(vec![11]));
    let a_reply = a_commands.try_recv().unwrap();
    assert!(
        matches!(a_reply, ClientMsg::FileContents { generation, stream_id: 7, data: Some(ref data) } if generation == a_generation && data == &[11])
    );
    assert!(b_commands.try_recv().is_err());
    // The slot remains held while the reply is merely queued.
    assert_eq!(budget.test_counts(), (2, 0));
    manager.sessions[&a]
        .rdp
        .as_ref()
        .unwrap()
        .test_write_command(a_reply);
    assert_eq!(budget.test_counts(), (1, 0));

    synthetic_read(manager.sessions[&a].rdp.as_ref().unwrap(), a_generation, 9);
    manager.poll_events();
    assert_eq!(budget.test_counts(), (2, 0));
    manager.close(a);
    assert_eq!(budget.test_counts(), (1, 0));
    assert!(manager
        .pending_rdp_file_reads
        .iter()
        .all(|pending| pending.session != a));
    synthetic_read(manager.sessions[&b].rdp.as_ref().unwrap(), b_generation, 9);
    manager.poll_events();
    assert_eq!(budget.test_counts(), (2, 0));
    manager.activate(b).unwrap();
    let reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 2);
    for read in reads {
        read.answer(None);
    }
    for _ in 0..2 {
        let reply = b_commands.try_recv().unwrap();
        assert!(
            matches!(reply, ClientMsg::FileContents { generation, data: None, .. } if generation == b_generation)
        );
        manager.sessions[&b]
            .rdp
            .as_ref()
            .unwrap()
            .test_write_command(reply);
    }
    assert_eq!(budget.test_counts(), (0, 0));
}

#[test]
fn file_safety_notice_survives_errors_and_background_tabs_until_a_new_ready_channel() {
    let profiles = vec![
        ConnectionProfile::new("a", "a.invalid", 22, "fixture"),
        ConnectionProfile::new("b", "b.invalid", 22, "fixture"),
    ];
    let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let a = manager.open_mock_session(ids[0]).unwrap();
    let b = manager.open_mock_session(ids[1]).unwrap();
    let handle = RdpClientHandle::test_fixture();
    handle.test_send_event(RdpClientEvent::ClipboardFileDirectionUnavailable {
        channel_epoch: 7,
        message: "synthetic safety notice".into(),
    });
    manager.sessions.get_mut(&a).unwrap().rdp = Some(handle);
    manager.sessions.get_mut(&a).unwrap().desktop_error = Some("synthetic error".into());
    manager.activate(b).unwrap();
    manager.poll_events();
    assert_eq!(
        manager.rdp_local_file_safety_notice(a),
        Some("synthetic safety notice")
    );
    assert_eq!(manager.rdp_local_file_safety_notice(b), None);
    assert_eq!(
        manager.sessions[&a].desktop_error.as_deref(),
        Some("synthetic error")
    );
    let handle = manager.sessions[&a].rdp.as_ref().unwrap();
    handle.test_send_event(RdpClientEvent::ClipboardChannelChanged { channel_epoch: 8 });
    handle.test_send_event(RdpClientEvent::ClipboardLocalFileDirectionReady { channel_epoch: 7 });
    manager.poll_events();
    assert_eq!(
        manager.rdp_local_file_safety_notice(a),
        Some("synthetic safety notice")
    );
    manager.sessions[&a]
        .rdp
        .as_ref()
        .unwrap()
        .test_send_event(RdpClientEvent::ClipboardLocalFileDirectionReady { channel_epoch: 8 });
    manager.poll_events();
    assert_eq!(manager.rdp_local_file_safety_notice(a), None);
    assert_eq!(
        manager.sessions[&a].desktop_error.as_deref(),
        Some("synthetic error")
    );
}

#[test]
fn dropped_reads_refuse_once_and_slots_recover_only_after_the_writer_finishes() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::Arc;

    let profile = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(id).unwrap();
    let budget = Arc::new(rdp_client::InboundReadBudget::new(1, 2));
    let (handle, commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    handle.test_local_offer();
    let generation = handle.test_local_generation();
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    let handle = manager.sessions[&session].rdp.as_ref().unwrap();
    synthetic_read(handle, generation, 10);
    manager.poll_events();
    let read = manager.take_rdp_file_reads().pop().unwrap();
    drop(read);
    let reply = commands.try_recv().unwrap();
    assert!(matches!(
        reply,
        ClientMsg::FileContents {
            stream_id: 10,
            data: None,
            ..
        }
    ));
    assert_eq!(budget.test_counts(), (1, 0));
    let handle = manager.sessions[&session].rdp.as_ref().unwrap();
    synthetic_read(handle, generation, 11);
    let rejection = commands.try_recv().unwrap();
    assert!(matches!(
        rejection,
        ClientMsg::FileContents {
            stream_id: 11,
            data: None,
            ..
        }
    ));
    assert_eq!(budget.test_counts(), (1, 1));
    handle.test_write_command(reply);
    handle.test_write_command(rejection);
    assert_eq!(budget.test_counts(), (0, 0));
    synthetic_read(handle, generation, 10);
    manager.poll_events();
    let read = manager.take_rdp_file_reads().pop().unwrap();
    read.answer(Some(vec![77]));
    let reply = commands.try_recv().unwrap();
    assert!(
        matches!(reply, ClientMsg::FileContents { stream_id: 10, data: Some(ref data), .. } if data == &[77])
    );
    manager.sessions[&session]
        .rdp
        .as_ref()
        .unwrap()
        .test_write_command(reply);
    assert_eq!(budget.test_counts(), (0, 0));
    assert!(commands.try_recv().is_err());
}

#[test]
fn each_active_helper_keeps_its_entire_refusal_allowance() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::Arc;

    let budget = Arc::new(rdp_client::InboundReadBudget::new(2, 32));
    let (a, a_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    let (b, b_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    a.test_local_offer();
    b.test_local_offer();
    let a_generation = a.test_local_generation();
    let b_generation = b.test_local_generation();
    synthetic_read(&a, a_generation, 1);
    synthetic_read(&b, b_generation, 1);
    for stream_id in 2..18 {
        synthetic_read(&a, a_generation, stream_id);
    }
    assert_eq!(budget.test_counts(), (2, 16));
    for stream_id in 2..18 {
        synthetic_read(&b, b_generation, stream_id);
    }
    assert_eq!(budget.test_counts(), (2, 32));
    for (handle, commands, generation) in [
        (&a, &a_commands, a_generation),
        (&b, &b_commands, b_generation),
    ] {
        for stream_id in 2..18 {
            let rejection = commands.try_recv().unwrap();
            assert!(matches!(rejection, ClientMsg::FileContents {
                generation: got_generation, stream_id: got, data: None
            } if got_generation == generation && got == stream_id));
            handle.test_write_command(rejection);
        }
        assert!(commands.try_recv().is_err());
    }
    assert_eq!(budget.test_counts(), (2, 0));
}

#[test]
fn missing_reservation_disables_only_the_local_offer_and_can_recover() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::Arc;

    let budget = Arc::new(rdp_client::InboundReadBudget::new(2, 1));
    let (a, _a_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    let (b, b_commands) = RdpClientHandle::test_fixture_with_budget(budget);
    a.test_local_offer();
    let file = ClipFile {
        name: "fixture.bin".into(),
        size: 1,
        is_dir: false,
    };
    b.send_clipboard_files(vec![file.clone()], Some(RdpClipboardSequence::new(1, || 1)));
    assert!(!b.has_local_file_offer());
    assert!(matches!(b.try_recv(), Some(RdpClientEvent::Notice(_))));
    assert!(
        matches!(b_commands.try_recv(), Ok(ClientMsg::ClipboardFiles { generation: 0, files }) if files.is_empty())
    );
    b.send_clipboard_image(vec![7]);
    assert!(matches!(b_commands.try_recv(), Ok(ClientMsg::ClipboardImage(data)) if data == [7]));
    assert!(b_commands.try_recv().is_err());

    a.close();
    b.send_clipboard_files(vec![file], Some(RdpClipboardSequence::new(1, || 1)));
    assert!(b.has_local_file_offer());
    assert!(
        matches!(b_commands.try_recv(), Ok(ClientMsg::ClipboardFiles { generation, .. }) if generation != 0)
    );
}

#[test]
fn continuous_inbound_flood_suspends_only_the_attacking_file_direction() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::Arc;

    let profiles = vec![
        ConnectionProfile::new("attacker", "attacker.invalid", 22, "fixture"),
        ConnectionProfile::new("other", "other.invalid", 22, "fixture"),
    ];
    let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
    let mut manager = SessionManager::with_profiles(profiles);
    let attacker = manager.open_mock_session(ids[0]).unwrap();
    let other = manager.open_mock_session(ids[1]).unwrap();
    let budget = Arc::new(rdp_client::InboundReadBudget::new(256, 64));
    let (handle, commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    let (other_handle, other_commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    handle.test_local_offer();
    other_handle.test_local_offer();
    let generation = handle.test_local_generation();
    let other_generation = other_handle.test_local_generation();
    manager.sessions.get_mut(&attacker).unwrap().rdp = Some(handle);
    manager.sessions.get_mut(&other).unwrap().rdp = Some(other_handle);
    manager.activate(other).unwrap();

    for stream_id in 0..32 {
        synthetic_read(
            manager.sessions[&attacker].rdp.as_ref().unwrap(),
            generation,
            stream_id,
        );
    }
    manager.poll_events();
    assert_eq!(manager.pending_rdp_file_reads.len(), 32);
    assert_eq!(budget.test_counts(), (32, 0));
    synthetic_read(
        manager.sessions[&other].rdp.as_ref().unwrap(),
        other_generation,
        0,
    );
    for stream_id in 32..10_000 {
        synthetic_read(
            manager.sessions[&attacker].rdp.as_ref().unwrap(),
            generation,
            stream_id,
        );
    }
    assert_eq!(budget.test_counts(), (1, 16));
    for stream_id in 32..48 {
        let refusal = commands.try_recv().unwrap();
        assert!(
            matches!(refusal, ClientMsg::FileContents { generation: got, stream_id: got_id, data: None } if got == generation && got_id == stream_id)
        );
        manager.sessions[&attacker]
            .rdp
            .as_ref()
            .unwrap()
            .test_discard_suspended_command(refusal);
    }
    assert_eq!(budget.test_counts(), (1, 0));
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::SuspendLocalFileDirection)
    ));
    assert!(commands.try_recv().is_err());
    assert!(other_commands.try_recv().is_err());
    manager.poll_events();
    assert!(manager.sessions[&attacker].desktop_error.is_none());
    assert!(manager
        .pending_rdp_file_reads
        .iter()
        .all(|pending| pending.session != attacker));
    let reads = manager.take_rdp_file_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].stream_id, 0);
    reads.into_iter().next().unwrap().answer(None);
    let reply = other_commands.try_recv().unwrap();
    manager.sessions[&other]
        .rdp
        .as_ref()
        .unwrap()
        .test_write_command(reply);
    assert_eq!(budget.test_counts(), (0, 0));
    manager.activate(attacker).unwrap();
    assert!(manager
        .take_desktop_notice()
        .is_some_and(|s| s.contains("桌面连接仍可使用")));
    let handle = manager.sessions[&attacker].rdp.as_ref().unwrap();
    handle.send_clipboard_image(vec![8]);
    assert!(matches!(commands.try_recv(), Ok(ClientMsg::ClipboardImage(image)) if image == [8]));
}

#[test]
fn duplicate_stream_ids_suspend_the_file_direction_without_misrouting_a_reply() {
    use adit_rdp_proto::ClientMsg;
    use std::sync::Arc;

    let profile = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(id).unwrap();
    let budget = Arc::new(rdp_client::InboundReadBudget::new(10, 10));
    let (handle, commands) = RdpClientHandle::test_fixture_with_budget(budget.clone());
    handle.test_local_offer();
    let generation = handle.test_local_generation();
    synthetic_read(&handle, generation, 3);
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    manager.poll_events();
    let read = manager.take_rdp_file_reads().pop().unwrap();
    let handle = manager.sessions[&session].rdp.as_ref().unwrap();
    handle.test_local_offer();
    synthetic_read(handle, handle.test_local_generation(), 3);
    assert_eq!(budget.test_counts(), (0, 0));
    assert!(matches!(
        commands.try_recv(),
        Ok(ClientMsg::SuspendLocalFileDirection)
    ));
    read.answer(Some(vec![1]));
    assert!(commands.try_recv().is_err());
    manager.poll_events();
    assert!(manager.sessions[&session].desktop_error.is_none());
    assert!(manager
        .take_desktop_notice()
        .is_some_and(|s| s.contains("流 ID 重复")));
}

#[test]
fn chunk_current_requires_original_generation_and_live_owner() {
    let handle = RdpClientHandle::test_fixture();
    let requester = handle.test_remote_file_offer(7);
    let chunk = RdpFileChunk {
        session: SessionId::new(),
        generation: 7,
        stream_id: 3,
        data: Some(vec![1]),
        owner: requester.clone(),
    };
    let mismatched = RdpFileChunk {
        generation: 8,
        owner: requester,
        ..chunk
    };
    assert!(!mismatched.is_current());
    let chunk = RdpFileChunk {
        generation: 7,
        ..mismatched
    };
    assert!(chunk.is_current());
    handle.set_file_offers_enabled(false);
    assert!(!chunk.is_current());
    handle.set_file_offers_enabled(true);
    assert!(handle.test_remote_file_offer(7).is_current());
    assert!(!chunk.is_current());
}

#[test]
fn failed_old_request_keeps_its_original_owner_after_generation_reuse() {
    let profile = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(id).unwrap();
    let handle = RdpClientHandle::test_fixture();
    let old = handle.test_remote_file_offer(7);
    handle.set_file_offers_enabled(false);
    old.request(9, 0, 0, 1);
    handle.set_file_offers_enabled(true);
    assert!(handle.test_remote_file_offer(7).is_current());
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);

    assert_eq!(manager.poll_events(), 1);
    let chunks = manager.take_rdp_file_chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].generation, chunks[0].stream_id), (7, 9));
    assert!(chunks[0].data.is_none());
    assert!(!chunks[0].is_current());
}

#[test]
fn stale_offers_consume_the_budget_without_hiding_closed() {
    for stale_count in [3, MAX_DESKTOP_EVENTS_PER_POLL + 3] {
        let profile = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
        let id = profile.id;
        let mut manager = SessionManager::with_profiles(vec![profile]);
        let session = manager.open_mock_session(id).unwrap();
        let handle = RdpClientHandle::test_fixture();
        for generation in 1..=stale_count as u64 {
            handle.test_send_event(RdpClientEvent::ClipboardFilesForEpoch {
                generation,
                revision: 0,
                files: vec![ClipFile {
                    name: "stale.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            });
        }
        handle.test_send_event(RdpClientEvent::Closed);
        manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
        assert_eq!(
            manager.poll_events(),
            (stale_count + 1).min(MAX_DESKTOP_EVENTS_PER_POLL)
        );
        if stale_count >= MAX_DESKTOP_EVENTS_PER_POLL {
            assert_eq!(
                manager.sessions[&session].summary.status,
                SessionStatus::Connected
            );
            assert_eq!(
                manager.poll_events(),
                stale_count + 1 - MAX_DESKTOP_EVENTS_PER_POLL
            );
        }
        assert_eq!(
            manager.sessions[&session].summary.status,
            SessionStatus::Disconnected
        );
        assert!(manager.take_rdp_clipboard_files().is_none());
        assert_eq!(manager.poll_events(), 0);
    }
}

#[test]
fn desktop_event_polling_is_bounded_even_with_a_replenished_queue() {
    let profile = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(id).unwrap();
    let handle = RdpClientHandle::test_fixture();
    for _ in 0..(MAX_DESKTOP_EVENTS_PER_POLL * 2 + 1) {
        handle.test_send_event(RdpClientEvent::Notice("synthetic".into()));
    }
    manager.sessions.get_mut(&session).unwrap().rdp = Some(handle);
    assert_eq!(manager.poll_events(), MAX_DESKTOP_EVENTS_PER_POLL);
    assert_eq!(manager.poll_events(), MAX_DESKTOP_EVENTS_PER_POLL);
    assert_eq!(manager.poll_events(), 1);
}
