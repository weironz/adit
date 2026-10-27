use super::*;
use crate::clipboard_files::ChunkBridge;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
};
use windows::Win32::{
    Foundation::E_ABORT,
    System::{
        Com::{IStream, FORMATETC},
        DataExchange::RegisterClipboardFormatW,
        Ole::ReleaseStgMedium,
        SystemServices::{MK_SHIFT, MODIFIERKEYS_FLAGS},
    },
};

fn lease() -> Lease {
    Lease {
        generation: 7,
        current: Arc::new(|| true),
        session: adit_domain::SessionId::new(),
        bridge: ChunkBridge::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn sta(test: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let _sta = Sta::new().unwrap();
        test();
    })
    .join()
    .unwrap();
}

fn hidden_window() -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("Adit synthetic drag test"),
            WS_OVERLAPPED,
            0,
            0,
            32,
            32,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    }
}

#[test]
fn clicking_or_lost_capture_never_starts_a_file_operation() {
    assert!(!drag_intent((0, 0), (1, 1), true, true, (4, 4)));
    assert!(!drag_intent((0, 0), (20, 20), false, true, (4, 4)));
    assert!(!drag_intent((0, 0), (20, 20), true, false, (4, 4)));
    assert!(drag_intent((0, 0), (20, 20), true, true, (4, 4)));
}
fn boundary(lease: &Lease) -> IDropTarget {
    let (tx, _rx) = mpsc::channel();
    InboundBoundary {
        lease: lease.clone(),
        notices: tx,
    }
    .into()
}

#[test]
fn copy_source_handles_modifiers_escape_release_and_disconnect_without_move() {
    sta(|| {
        let lease = lease();
        let source: IDropSource = CopySource {
            lease: lease.clone(),
        }
        .into();
        unsafe {
            assert_eq!(source.QueryContinueDrag(false, MK_LBUTTON), S_OK);
            assert_eq!(
                source.QueryContinueDrag(false, MK_LBUTTON | MK_SHIFT),
                DRAGDROP_S_CANCEL
            );
            assert!(source
                .GiveFeedback(windows::Win32::System::Ole::DROPEFFECT_MOVE)
                .is_err());
            assert!(source
                .GiveFeedback(windows::Win32::System::Ole::DROPEFFECT_LINK)
                .is_err());
            assert_eq!(
                source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS::default()),
                DRAGDROP_S_DROP
            );
            assert_eq!(
                source.QueryContinueDrag(true, MK_LBUTTON),
                DRAGDROP_S_CANCEL
            );
            lease.cancel();
            assert_eq!(
                source.QueryContinueDrag(false, MK_LBUTTON),
                DRAGDROP_S_CANCEL
            );
        }
        assert_eq!(DROPEFFECT_COPY.0, 1);
    });
}

#[test]
fn drop_target_refuses_physical_and_virtual_drops_without_reading_data() {
    sta(|| {
        let lease = lease();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let request: Request = Arc::new(move |_, _, _, _| {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let object: IDataObject = FileDataObject::for_drag(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 3,
                is_dir: false,
            }],
            lease.bridge.clone(),
            request,
            64 * 1024,
            lease.cancelled.clone(),
        )
        .unwrap()
        .into();
        let target = boundary(&lease);
        unsafe {
            let mut effect = DROPEFFECT_COPY;
            target
                .DragEnter(
                    &object,
                    MODIFIERKEYS_FLAGS::default(),
                    POINTL::default(),
                    &mut effect,
                )
                .unwrap();
            assert_eq!(effect, DROPEFFECT_NONE);
            effect = DROPEFFECT_COPY;
            target
                .Drop(
                    &object,
                    MODIFIERKEYS_FLAGS::default(),
                    POINTL::default(),
                    &mut effect,
                )
                .unwrap();
            assert_eq!(effect, DROPEFFECT_NONE);
            target.DragLeave().unwrap();
        }
        assert_eq!(reads.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn registration_is_sta_owned_revoked_and_never_replaces_an_existing_target() {
    sta(|| {
        let hwnd = hidden_window();
        let lease = lease();
        let first = Registration::new(hwnd, boundary(&lease)).unwrap();
        assert!(Registration::new(hwnd, boundary(&lease)).is_err());
        drop(first);
        let next = Registration::new(hwnd, boundary(&lease)).unwrap();
        let foreign_hwnd = hwnd.0 as isize;
        std::thread::spawn(move || {
            let _sta = Sta::new().unwrap();
            assert!(Registration::new(HWND(foreign_hwnd as *mut _), boundary(&lease)).is_err());
        })
        .join()
        .unwrap();
        drop(next);
        unsafe {
            DestroyWindow(hwnd).unwrap();
        }
    });
}

#[test]
fn stale_registration_cannot_revoke_a_recreated_window() {
    sta(|| {
        let lease = lease();
        let old_window = hidden_window();
        let old = Registration::new(old_window, boundary(&lease)).unwrap();
        unsafe {
            DestroyWindow(old_window).unwrap();
        }
        let new_window = hidden_window();
        let current = Registration::new(new_window, boundary(&lease)).unwrap();
        drop(old);
        assert!(Registration::new(new_window, boundary(&lease)).is_err());
        drop(current);
        unsafe {
            DestroyWindow(new_window).unwrap();
        }
    });
}

fn contents_format() -> FORMATETC {
    FORMATETC {
        cfFormat: unsafe { RegisterClipboardFormatW(w!("FileContents")) as u16 },
        dwAspect: 1,
        lindex: 0,
        tymed: 4,
        ..Default::default()
    }
}

#[test]
fn synthetic_istream_reads_binary_ranges_only_on_demand_and_cancels_immediately() {
    sta(|| {
        let lease = lease();
        let bridge = lease.bridge.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let request: Request = Arc::new(move |stream, index, offset, length| {
            observed.fetch_add(1, Ordering::Relaxed);
            assert_eq!((index, offset, length), (0, 0, 3));
            bridge.deliver(stream, Some(vec![0, 255, 1]));
        });
        let object: IDataObject = FileDataObject::for_drag(
            vec![ClipFile {
                name: "合成.bin".into(),
                size: 3,
                is_dir: false,
            }],
            lease.bridge.clone(),
            request,
            64 * 1024,
            lease.cancelled.clone(),
        )
        .unwrap()
        .into();
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        unsafe {
            let mut medium = object.GetData(&contents_format()).unwrap();
            let stream: IStream = medium.u.pstm.as_ref().unwrap().clone();
            let mut bytes = [0u8; 3];
            let mut count = 0;
            assert!(stream
                .Read(bytes.as_mut_ptr().cast(), 3, Some(&mut count))
                .is_ok());
            assert_eq!(count, 3);
            assert_eq!(bytes, [0, 255, 1]);
            assert_eq!(reads.load(Ordering::Relaxed), 1);
            lease.cancel();
            count = 10;
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 3, Some(&mut count)),
                E_ABORT
            );
            assert_eq!(count, 0);
            assert!(object.GetData(&contents_format()).is_err());
            ReleaseStgMedium(&mut medium);
        }
    });
}

#[test]
fn premature_eof_and_wrong_medium_are_errors_not_successful_empty_files() {
    sta(|| {
        let lease = lease();
        let bridge = lease.bridge.clone();
        let object: IDataObject = FileDataObject::for_drag(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 3,
                is_dir: false,
            }],
            lease.bridge.clone(),
            Arc::new(move |id, _, _, _| bridge.deliver(id, Some(Vec::new()))),
            64 * 1024,
            lease.cancelled.clone(),
        )
        .unwrap()
        .into();
        unsafe {
            let mut bad = contents_format();
            bad.lindex = 1;
            assert!(object.GetData(&bad).is_err());
            bad.lindex = 0;
            bad.tymed = 1;
            assert!(object.GetData(&bad).is_err());
            let mut medium = object.GetData(&contents_format()).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap().clone();
            let mut bytes = [0u8; 3];
            let mut count = 10;
            assert!(stream
                .Read(bytes.as_mut_ptr().cast(), 3, Some(&mut count))
                .is_err());
            assert_eq!(count, 0);
            ReleaseStgMedium(&mut medium);
        }
    });
}

#[test]
fn oversized_chunk_is_rejected_before_copying_into_the_destination_buffer() {
    sta(|| {
        let lease = lease();
        let bridge = lease.bridge.clone();
        let object: IDataObject = FileDataObject::for_drag(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 3,
                is_dir: false,
            }],
            lease.bridge.clone(),
            Arc::new(move |id, _, _, _| bridge.deliver(id, Some(vec![9; 4]))),
            64 * 1024,
            lease.cancelled.clone(),
        )
        .unwrap()
        .into();
        unsafe {
            let mut medium = object.GetData(&contents_format()).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap().clone();
            let mut bytes = [0u8; 3];
            let mut count = 9;
            assert!(stream
                .Read(bytes.as_mut_ptr().cast(), 3, Some(&mut count))
                .is_err());
            assert_eq!(count, 0);
            assert_eq!(bytes, [0; 3]);
            ReleaseStgMedium(&mut medium);
        }
    });
}

#[test]
fn unsafe_metadata_cannot_bypass_the_shared_validator_via_the_clipboard_constructor() {
    sta(|| {
        let bridge = ChunkBridge::new();
        let object: IDataObject = FileDataObject::new(
            vec![ClipFile {
                name: "..\\escape".into(),
                size: 3,
                is_dir: false,
            }],
            bridge,
            Arc::new(|_, _, _, _| panic!("unsafe metadata must never request bytes")),
            64 * 1024,
        )
        .into();
        unsafe {
            let descriptor = FORMATETC {
                cfFormat: RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16,
                dwAspect: 1,
                lindex: -1,
                tymed: 1,
                ..Default::default()
            };
            assert!(object.QueryGetData(&descriptor).is_err());
            assert!(object.GetData(&descriptor).is_err());
        }
    });
}

#[test]
fn disconnect_wakes_a_blocked_com_stream_with_an_error() {
    sta(|| {
        let lease = lease();
        let cancel = lease.clone();
        let (tx, rx) = mpsc::channel();
        let closer = std::thread::spawn(move || {
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            cancel.cancel();
        });
        let object: IDataObject = FileDataObject::for_drag(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 3,
                is_dir: false,
            }],
            lease.bridge.clone(),
            Arc::new(move |_, _, _, _| {
                tx.send(()).unwrap();
            }),
            64 * 1024,
            lease.cancelled.clone(),
        )
        .unwrap()
        .into();
        unsafe {
            let mut medium = object.GetData(&contents_format()).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap().clone();
            let mut bytes = [0u8; 3];
            let mut count = 9;
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 3, Some(&mut count)),
                E_ABORT
            );
            assert_eq!(count, 0);
            ReleaseStgMedium(&mut medium);
        }
        closer.join().unwrap();
    });
}

#[test]
fn destroying_the_owner_window_revokes_the_native_transfer_window_and_lease() {
    sta(|| {
        let owner = hidden_window();
        let owner_id = owner.0 as isize;
        let lease = lease();
        let observed = lease.clone();
        let (notices, ready) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = run(
                owner_id,
                "synthetic owner".into(),
                vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 3,
                    is_dir: false,
                }],
                Arc::new(|_, _, _, _| panic!("opening a hidden window must not fetch bytes")),
                lease,
                notices,
                false,
            );
            done.send(result.is_ok()).unwrap();
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        unsafe {
            DestroyWindow(owner).unwrap();
        }
        assert!(finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap());
        thread.join().unwrap();
        assert!(observed.cancelled.load(Ordering::Acquire));
        assert!(observed.bridge.begin().is_err());
    });
}

#[test]
fn file_epoch_old_clipboard_stream_aborts_without_gui_poll_or_bridge_close() {
    sta(|| {
        let bridge = ChunkBridge::new();
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(7));
        let current = generation.clone();
        let (tx, rx) = mpsc::channel();
        let observer = std::thread::spawn(move || {
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            generation.store(8, Ordering::Release); // IPC reader observes a replacement/text offer.
        });
        let object: IDataObject = FileDataObject::new(
            vec![ClipFile {
                name: "synthetic.bin".into(),
                size: 3,
                is_dir: false,
            }],
            bridge.clone(),
            Arc::new(move |_, _, _, _| {
                tx.send(()).unwrap();
            }),
            64 * 1024,
        )
        .with_liveness(Arc::new(move || current.load(Ordering::Acquire) == 7))
        .into();
        unsafe {
            let mut medium = object.GetData(&contents_format()).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap().clone();
            let mut bytes = [0u8; 3];
            let mut count = 9;
            let started = std::time::Instant::now();
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 3, Some(&mut count)),
                E_ABORT
            );
            assert!(started.elapsed() < std::time::Duration::from_secs(1));
            assert_eq!(count, 0);
            assert!(object.GetData(&contents_format()).is_err());
            ReleaseStgMedium(&mut medium);
        }
        observer.join().unwrap();
        assert!(
            bridge.begin().is_ok(),
            "GUI did not close this bridge; the epoch guard aborted the read"
        );
    });
}

#[test]
fn file_epoch_copy_source_cancels_before_gui_releases_its_lease() {
    sta(|| {
        let current = Arc::new(AtomicBool::new(true));
        let observed = current.clone();
        let mut lease = lease();
        lease.current = Arc::new(move || observed.load(Ordering::Acquire));
        let source: IDropSource = CopySource {
            lease: lease.clone(),
        }
        .into();
        current.store(false, Ordering::Release);
        assert!(!lease.cancelled.load(Ordering::Acquire));
        unsafe {
            assert_eq!(
                source.QueryContinueDrag(false, MK_LBUTTON),
                DRAGDROP_S_CANCEL
            );
        }
    });
}
