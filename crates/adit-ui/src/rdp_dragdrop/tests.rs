use super::*;

fn file(name: &str) -> ClipFile {
    ClipFile {
        name: name.into(),
        size: 3,
        is_dir: false,
    }
}
fn lease(id: SessionId) -> Lease {
    Lease {
        session: id,
        bridge: ChunkBridge::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn state_for(active: Lease) -> State {
    let (notice_tx, notices) = mpsc::channel();
    State {
        offers: HashMap::new(),
        active: Some(active),
        notices,
        notice_tx,
    }
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
