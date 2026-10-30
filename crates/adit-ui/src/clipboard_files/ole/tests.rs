use super::*;
use crate::clipboard_files::ChunkError;
use adit_session::{
    clipboard_test_fixture::SyntheticRdp, RdpFileRequestError, RdpFileRequester, SessionManager,
};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

fn fixture() -> (SessionManager, SyntheticRdp, RdpFileRequester) {
    let profile = adit_domain::ConnectionProfile::new("fixture", "fixture.invalid", 22, "fixture");
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_mock_session(id).unwrap();
    let peer = SyntheticRdp::attach(&mut manager, session);
    let requester = peer.remote_offer(&manager, 7);
    (manager, peer, requester)
}

fn queued_offer(rx: &Receiver<Offer>, bridge: ChunkBridge, requester: RdpFileRequester) -> Offer {
    super::super::offer_remote_files(
        vec![ClipFile {
            name: "fixture.bin".into(),
            size: 1,
            is_dir: false,
        }],
        bridge,
        requester,
        1,
    );
    rx.recv_timeout(Duration::from_secs(2)).unwrap()
}

fn expect_refused(bridge: ChunkBridge, stream: u32) {
    let waiting = bridge.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || tx.send(waiting.wait(stream)).unwrap());
    let received = rx.recv_timeout(Duration::from_secs(2));
    if received.is_err() {
        bridge.close();
    }
    thread.join().unwrap();
    assert_eq!(received.unwrap(), Err(ChunkError::Refused));
}

#[test]
fn request_refusal_clipboard_offer_wakes_its_bridge_without_system_clipboard() {
    const CHILD: &str = "ADIT_TEST_SYNTHETIC_CLIPBOARD_REFUSAL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // OLE_THREAD is process-wide. In the child, preinstall an in-memory
        // queue before offer_remote_files can start a real clipboard thread.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clipboard_files::ole::tests::request_refusal_clipboard_offer_wakes_its_bridge_without_system_clipboard",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated clipboard regression failed: {status}"
        );
        return;
    }

    let (tx, rx) = mpsc::channel();
    assert!(OLE_THREAD
        .set(Some(OleThread {
            offers: Mutex::new(tx),
            // No thread receives this wakeup; the test consumes the queue.
            thread_id: 0,
            owns_remote_object: Arc::new(AtomicBool::new(false)),
        }))
        .is_ok());

    let (mut manager, peer, requester) = fixture();
    let bridge = ChunkBridge::new();
    let stream = bridge.begin().unwrap();
    let offer = queued_offer(&rx, bridge.clone(), requester.clone());
    let _replacement = peer.remote_offer(&manager, 8);
    assert_eq!(
        requester.try_request(u32::MAX, 0, 0, 1),
        Err(RdpFileRequestError::Unavailable)
    );
    (offer.request)(stream, 0, 0, 1);
    expect_refused(bridge, stream);
    assert!(peer.try_command().is_none());
    manager.poll_events();
    assert!(manager.take_rdp_file_chunks().is_empty());
    assert!(manager.take_rdp_file_drag_events().is_empty());

    let (mut manager, peer, requester) = fixture();
    let bridge = ChunkBridge::new();
    let stream = bridge.begin().unwrap();
    let offer = queued_offer(&rx, bridge.clone(), requester.clone());
    // Keep the bridge's page unused while filling the normal history limit.
    const PAGE_BITS: u32 = 4096;
    const PAGE_LIMIT: u32 = 4096;
    let rejected_page = stream / PAGE_BITS;
    let mut filled = 0;
    for page in 0..=PAGE_LIMIT {
        if page == rejected_page {
            continue;
        }
        assert_eq!(requester.try_request(page * PAGE_BITS, 0, 0, 1), Ok(()));
        filled += 1;
        if filled == PAGE_LIMIT {
            break;
        }
    }
    assert_eq!(filled, PAGE_LIMIT);
    for _ in 0..PAGE_LIMIT {
        assert!(peer.try_command().is_some());
    }
    assert_eq!(
        requester.try_request(stream, 0, 0, 1),
        Err(RdpFileRequestError::StreamIdCapacity)
    );
    (offer.request)(stream, 0, 0, 1);
    expect_refused(bridge, stream);
    assert!(peer.try_command().is_none());
    manager.poll_events();
    assert!(manager.take_rdp_file_chunks().is_empty());
    assert!(manager.take_rdp_file_drag_events().is_empty());

    let (mut manager, peer, requester) = fixture();
    let session = manager.active_session().unwrap();
    let first_window = crate::window_presentation::WindowPresentation {
        rdp_file_scope: Some((session, 7)),
        ..Default::default()
    };
    let original = first_window.rdp_chunk_bridge.clone();
    let stream = original.begin().unwrap();
    // Production IDs are process-wide; inject a collision only in this
    // test to prove that the rejected owner's bridge receives the failure.
    let second_window = crate::window_presentation::WindowPresentation {
        rdp_chunk_bridge: super::super::bridge::tests::bridge_with_test_id(stream),
        rdp_file_scope: Some((session, 7)),
        ..Default::default()
    };
    let rejected = second_window.rdp_chunk_bridge.clone();
    let original_offer = queued_offer(&rx, original.clone(), requester.clone());
    let offer = queued_offer(&rx, rejected.clone(), requester.clone());
    (original_offer.request)(stream, 0, 0, 1);
    assert!(matches!(
        peer.try_command(),
        Some(adit_rdp_proto::ClientMsg::RequestFileContents { stream_id, .. })
            if stream_id == stream
    ));
    assert_eq!(
        requester.try_request(stream, 0, 0, 1),
        Err(RdpFileRequestError::DuplicateStream)
    );
    (offer.request)(stream, 0, 0, 1);
    expect_refused(rejected, stream);
    assert!(peer.try_command().is_none());
    manager.poll_events();
    assert!(manager.take_rdp_file_chunks().is_empty());
    assert!(manager.take_rdp_file_drag_events().is_empty());

    assert!(original.test_is_pending(stream));
    let waiting = original.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || tx.send(waiting.wait(stream)).unwrap());
    original.deliver(stream, Some(vec![42]));
    let answer = rx.recv_timeout(Duration::from_secs(2));
    if answer.is_err() {
        original.close();
    }
    thread.join().unwrap();
    assert_eq!(answer.unwrap(), Ok(vec![42]));
}
