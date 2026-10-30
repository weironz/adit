use super::*;
use std::sync::mpsc;

fn file(name: &str) -> ClipFile {
    ClipFile {
        name: name.into(),
        size: 3,
        is_dir: false,
    }
}
fn lease(id: SessionId) -> Lease {
    Lease {
        generation: 7,
        current: Arc::new(|| true),
        session: id,
        bridge: ChunkBridge::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn state_for(active: Lease) -> State {
    let mut state = State::default();
    state.active = Some(active);
    state
}

#[test]
fn pending_notices_are_coalesced_and_taken_by_session() {
    let first = SessionId::new();
    let second = SessionId::new();
    let mut state = State::default();
    for index in 0..1_000 {
        state.test_notice(first, &format!("first {index}"));
        state.test_notice(second, &format!("second {index}"));
    }

    state.update(Vec::new(), true, |id| id == first || id == second);
    assert_eq!(state.pending_notices.len(), 2);
    assert_eq!(state.take_notice(first).as_deref(), Some("first 999"));
    assert_eq!(state.take_notice(first), None);
    assert_eq!(state.take_notice(second).as_deref(), Some("second 999"));
    assert!(state.pending_notices.is_empty());
}

#[test]
fn pending_and_late_notices_for_non_live_sessions_are_discarded() {
    let alive = SessionId::new();
    let expired = SessionId::new();
    let mut state = State::default();
    state.test_notice(alive, "still live");
    state.test_notice(expired, "was live");
    state.update(Vec::new(), true, |id| id == alive || id == expired);

    state.test_notice(expired, "late notice");
    state.update(Vec::new(), true, |id| id == alive);
    assert_eq!(state.pending_notices.len(), 1);
    assert_eq!(state.take_notice(expired), None);
    assert_eq!(state.take_notice(alive).as_deref(), Some("still live"));
}

#[test]
fn removed_session_ignores_late_producers_from_its_old_slot() {
    let session = SessionId::new();
    let mut state = State::default();
    let old_slot = state.notice_slot(session);
    old_slot.fail(NoticeError::Open);
    state.update(Vec::new(), true, |_| false);
    assert!(state.notices.borrow().is_empty());

    let new_slot = state.notice_slot(session);
    old_slot.fail(NoticeError::Incomplete);
    new_slot.publish(Notice::Opened);
    state.update(Vec::new(), true, |id| id == session);
    assert_eq!(
        state.take_notice(session).as_deref(),
        Some(Notice::Opened.text())
    );
}

#[test]
fn stalled_ui_coalesces_a_flood_without_starving_other_sessions_or_losing_errors() {
    let first = SessionId::new();
    let second = SessionId::new();
    let mut state = State::default();
    let first_slot = state.notice_slot(first);
    let second_slot = state.notice_slot(second);
    for _ in 0..100_000 {
        first_slot.publish(Notice::InboundUnsupported);
    }
    first_slot.fail(NoticeError::InvalidFiles);
    for _ in 0..100_000 {
        first_slot.publish(Notice::Accepted);
        first_slot.publish(Notice::Cancelled);
    }
    second_slot.publish(Notice::Accepted);

    assert_eq!(state.notices.borrow().len(), 2);
    state.update(Vec::new(), true, |id| id == first || id == second);
    assert_eq!(
        state.take_notice(first).as_deref(),
        Some(NoticeError::InvalidFiles.text())
    );
    assert_eq!(
        state.take_notice(second).as_deref(),
        Some(Notice::Accepted.text())
    );
    state.update(Vec::new(), true, |_| true);
    assert_eq!(state.take_notice(first), None);
}

#[test]
fn pending_error_survives_later_status_until_its_own_session_displays_it() {
    let first = SessionId::new();
    let second = SessionId::new();
    let mut state = State::default();
    let first_slot = state.notice_slot(first);
    let second_slot = state.notice_slot(second);
    first_slot.fail(NoticeError::Open);
    second_slot.fail(NoticeError::Incomplete);
    state.update(Vec::new(), true, |_| true);

    first_slot.publish(Notice::Accepted);
    second_slot.publish(Notice::Cancelled);
    state.update(Vec::new(), true, |_| true);
    assert_eq!(
        state.take_notice(second).as_deref(),
        Some(NoticeError::Incomplete.text())
    );
    assert_eq!(
        state.take_notice(first).as_deref(),
        Some(NoticeError::Open.text())
    );

    first_slot.publish(Notice::Opened);
    state.update(Vec::new(), true, |_| true);
    assert_eq!(
        state.take_notice(first).as_deref(),
        Some(Notice::Opened.text())
    );
}

#[test]
fn distinct_errors_are_retained_in_bounded_slots_until_each_is_consumed() {
    let session = SessionId::new();
    let mut state = State::default();
    let slot = state.notice_slot(session);
    for _ in 0..10_000 {
        slot.fail(NoticeError::Open);
        slot.fail(NoticeError::InvalidFiles);
        slot.fail(NoticeError::Incomplete);
        slot.publish(Notice::Accepted);
    }
    state.update(Vec::new(), true, |_| true);
    for error in [
        NoticeError::Incomplete,
        NoticeError::InvalidFiles,
        NoticeError::Open,
    ] {
        slot.publish(Notice::Cancelled);
        state.update(Vec::new(), true, |_| true);
        assert_eq!(state.take_notice(session).as_deref(), Some(error.text()));
    }
    assert_eq!(state.take_notice(session), None);
}

#[test]
fn notice_producers_and_disconnect_do_not_wait_for_the_ui() {
    let session = SessionId::new();
    let active = lease(session);
    let mut state = state_for(active.clone());
    let slot = state.notice_slot(session);
    // The producer must not access the UI's session registry, even while it is
    // exclusively borrowed and no UI tick drains notices.
    let registry_guard = state.notices.borrow_mut();
    let (done, finished) = mpsc::sync_channel(2);
    let producer_done = done.clone();
    let producer = std::thread::spawn(move || {
        for _ in 0..100_000 {
            slot.publish(Notice::InboundUnsupported);
            slot.fail(NoticeError::Incomplete);
        }
        producer_done.send("producer").unwrap();
    });
    let disconnect = std::thread::spawn(move || {
        active.cancel();
        done.send("disconnect").unwrap();
    });
    let mut completed = [
        finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
    ];
    completed.sort_unstable();
    assert_eq!(completed, ["disconnect", "producer"]);
    producer.join().unwrap();
    disconnect.join().unwrap();
    drop(registry_guard);
    state.cancel();
    state.update(Vec::new(), true, |_| true);
    assert_eq!(
        state.take_notice(session).as_deref(),
        Some(NoticeError::Incomplete.text())
    );
}

#[test]
fn only_unambiguous_regular_leaf_files_are_exportable() {
    assert!(validate_files(&[file("报告.txt"), file("photo.png")]).is_ok());
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "dir\\file",
        "C:stream",
        "a:b",
        "a.",
        "a ",
        "NUL",
        "CON.txt",
        "COM1.txt",
        "LPT9",
        "a\0b",
        "wild*.txt",
        "x?y",
    ] {
        assert!(validate_files(&[file(name)]).is_err(), "{name:?}");
    }
    assert!(validate_files(&[file("A.txt"), file("a.txt")]).is_err());
    assert!(validate_files(&[file(&"a".repeat(260))]).is_err());
    let mut directory = file("directory");
    directory.is_dir = true;
    assert!(validate_files(&[directory]).is_err());
    let mut unknown_size = file("unknown");
    unknown_size.size = 0;
    assert!(validate_files(&[unknown_size]).is_err());
    let mut oversized = file("oversized");
    oversized.size = u64::MAX;
    assert!(validate_files(&[oversized]).is_err());
}

#[test]
fn metadata_and_background_session_events_do_not_read_or_cancel_other_session() {
    let a = SessionId::new();
    let b = SessionId::new();
    let active = lease(a);
    let mut state = state_for(active.clone());
    state.offers.insert(
        a,
        Offer {
            generation: 7,
            current: Arc::new(|| true),
            files: vec![file("a")],
            request: Arc::new(|_, _, _, _| panic!("metadata must not request contents")),
        },
    );
    state.update(vec![RdpFileDragEvent::Closed { session: b }], true, |_| {
        true
    });
    assert!(!active.cancelled.load(Ordering::Acquire));
    assert!(state.offers.contains_key(&a));
    state.update(vec![RdpFileDragEvent::Closed { session: a }], true, |_| {
        false
    });
    assert!(active.cancelled.load(Ordering::Acquire));
    assert!(active.bridge.begin().is_err());
    assert!(state.offers.is_empty());
}

#[test]
fn chunks_from_another_session_never_satisfy_a_drag_stream() {
    let a = SessionId::new();
    let active = lease(a);
    let id = active.bridge.begin().unwrap();
    let reader = active.bridge.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || tx.send(reader.wait(id)).unwrap());
    let mut state = state_for(active);
    state.update(
        vec![RdpFileDragEvent::Chunk {
            generation: 7,
            session: SessionId::new(),
            stream_id: id,
            data: Some(vec![99]),
        }],
        true,
        |_| true,
    );
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(50))
        .is_err());
    state.update(
        vec![RdpFileDragEvent::Chunk {
            generation: 7,
            session: a,
            stream_id: id,
            data: Some(vec![1, 0, 255]),
        }],
        true,
        |_| true,
    );
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap(),
        [1, 0, 255]
    );
    thread.join().unwrap();
}

#[test]
fn disabling_channel_or_losing_connection_releases_pending_reads() {
    for enabled in [false, true] {
        let active = lease(SessionId::new());
        let id = active.bridge.begin().unwrap();
        let mut state = state_for(active.clone());
        state.update(Vec::new(), enabled, |_| false);
        assert!(active.cancelled.load(Ordering::Acquire));
        assert!(active.bridge.wait(id).is_err());
        assert!(state.active.is_none());
    }
}

#[test]
fn stream_ids_are_not_reused_across_clipboard_offers_or_drags() {
    let old = ChunkBridge::new();
    let new = ChunkBridge::new();
    let a = old.begin().unwrap();
    let b = new.begin().unwrap();
    assert_ne!(a, b);
    new.deliver(a, Some(vec![99]));
    new.deliver(b, Some(vec![42]));
    assert_eq!(new.wait(b).unwrap(), [42]);
    old.close();
}

#[test]
fn revocation_in_a_batch_precedes_even_an_earlier_queued_chunk() {
    let id = SessionId::new();
    let active = lease(id);
    let stream_id = active.bridge.begin().unwrap();
    let mut state = state_for(active.clone());
    state.update(
        vec![
            RdpFileDragEvent::Chunk {
                generation: 7,
                session: id,
                stream_id,
                data: Some(vec![99]),
            },
            RdpFileDragEvent::Closed { session: id },
        ],
        true,
        |_| true,
    );
    assert!(active.bridge.wait(stream_id).is_err());
    assert!(active.cancelled.load(Ordering::Acquire));
}

#[test]
fn file_epoch_old_reply_in_the_same_session_cannot_fill_a_new_bridge() {
    let session = SessionId::new();
    let active = lease(session);
    let stream_id = active.bridge.begin().unwrap();
    let reader = active.bridge.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || tx.send(reader.wait(stream_id)).unwrap());
    let mut state = state_for(active);
    state.update(
        vec![RdpFileDragEvent::Chunk {
            session,
            generation: 6,
            stream_id,
            data: Some(vec![99]),
        }],
        true,
        |_| true,
    );
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(50))
        .is_err());
    state.update(
        vec![RdpFileDragEvent::Chunk {
            session,
            generation: 7,
            stream_id,
            data: Some(vec![42]),
        }],
        true,
        |_| true,
    );
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap(),
        [42]
    );
    thread.join().unwrap();
}

// These tests exercise the request installed by State::update without opening
// a drag window or the system clipboard. The native rejection-to-bridge adapter
// lives inside start_drag and cannot be reached through this fixture.
fn synthetic_drag_offer() -> (
    adit_session::SessionManager,
    SessionId,
    adit_session::clipboard_test_fixture::SyntheticRdp,
    adit_session::RdpFileRequester,
    State,
) {
    let profile = adit_domain::ConnectionProfile::new("fixture", "fixture.invalid", 22, "fixture");
    let profile_id = profile.id;
    let mut manager = adit_session::SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(profile_id).unwrap();
    let peer = adit_session::clipboard_test_fixture::SyntheticRdp::attach(&mut manager, session);
    let requester = peer.remote_offer(&manager, 7);
    let mut state = State::default();
    state.update(
        vec![RdpFileDragEvent::Offered {
            session,
            generation: 7,
            files: vec![file("fixture.bin")],
            requester: requester.clone(),
        }],
        true,
        |_| true,
    );
    (manager, session, peer, requester, state)
}

fn request_drag(state: &State, session: SessionId, stream: u32) -> Result<(), ()> {
    (state.offers.get(&session).unwrap().request)(stream, 0, 0, 1)
}

#[test]
fn request_refusal_drag_offer_propagates_expired_and_capacity_errors() {
    use adit_session::RdpFileRequestError;

    let (mut manager, session, peer, requester, state) = synthetic_drag_offer();
    let _replacement = peer.remote_offer(&manager, 8);
    assert_eq!(
        requester.try_request(100, 0, 0, 1),
        Err(RdpFileRequestError::Unavailable)
    );
    assert_eq!(request_drag(&state, session, 101), Err(()));
    assert!(peer.try_command().is_none());
    manager.poll_events();
    assert!(manager.take_rdp_file_chunks().is_empty());
    assert!(manager.take_rdp_file_drag_events().is_empty());

    let (mut manager, session, peer, requester, state) = synthetic_drag_offer();
    // The fixture exposes the normal history limit, so fill its sparse pages
    // instead of changing production limits or consuming 16 million IDs.
    const PAGE_BITS: u32 = 4096;
    const PAGE_LIMIT: u32 = 4096;
    for page in 0..PAGE_LIMIT {
        assert_eq!(requester.try_request(page * PAGE_BITS, 0, 0, 1), Ok(()));
    }
    for _ in 0..PAGE_LIMIT {
        assert!(peer.try_command().is_some());
    }
    let rejected = PAGE_LIMIT * PAGE_BITS;
    assert_eq!(
        requester.try_request(rejected, 0, 0, 1),
        Err(RdpFileRequestError::StreamIdCapacity)
    );
    assert_eq!(request_drag(&state, session, rejected), Err(()));
    assert!(peer.try_command().is_none());
    manager.poll_events();
    assert!(manager.take_rdp_file_chunks().is_empty());
    assert!(manager.take_rdp_file_drag_events().is_empty());
}

#[test]
fn request_refusal_duplicate_drag_callback_does_not_fail_original_stream() {
    use adit_session::{RdpClientEvent, RdpFileRequestError};
    use std::time::Duration;

    let (mut manager, session, peer, requester, mut first) = synthetic_drag_offer();
    let mut second = State::default();
    second.update(
        vec![RdpFileDragEvent::Offered {
            session,
            generation: 7,
            files: vec![file("fixture.bin")],
            requester: requester.clone(),
        }],
        true,
        |_| true,
    );
    let active = lease(session);
    let bridge = active.bridge.clone();
    let stream = bridge.begin().unwrap();
    first.active = Some(active);
    assert_eq!(request_drag(&first, session, stream), Ok(()));
    assert!(matches!(
        peer.try_command(),
        Some(adit_rdp_proto::ClientMsg::RequestFileContents { stream_id, .. })
            if stream_id == stream
    ));
    assert_eq!(
        requester.try_request(stream, 0, 0, 1),
        Err(RdpFileRequestError::DuplicateStream)
    );
    assert_eq!(request_drag(&second, session, stream), Err(()));
    assert!(peer.try_command().is_none());
    manager.poll_events();
    assert!(manager.take_rdp_file_chunks().is_empty());
    assert!(manager.take_rdp_file_drag_events().is_empty());

    assert!(bridge.test_is_pending(stream));
    let reader = bridge.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || tx.send(reader.wait(stream)).unwrap());
    peer.send_event(
        &manager,
        RdpClientEvent::FileContents {
            generation: 7,
            stream_id: stream,
            data: Some(vec![42]),
        },
    );
    manager.poll_events();
    first.update(manager.take_rdp_file_drag_events(), true, |_| true);
    let answer = rx.recv_timeout(Duration::from_secs(2));
    if answer.is_err() {
        bridge.close();
    }
    thread.join().unwrap();
    assert_eq!(answer.unwrap(), Ok(vec![42]));
}
