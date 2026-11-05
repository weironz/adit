use super::*;
use std::{
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use windows::core::{w, BOOL, HRESULT};
use windows::Win32::{
    Foundation::E_NOTIMPL,
    Graphics::Gdi::ClientToScreen,
    System::{
        Com::{IAdviseSink, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM_0},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND},
    },
    UI::{
        HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
        Shell::DROPFILES,
        WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
        },
    },
};

#[implement(IDataObject)]
struct Files {
    paths: Vec<PathBuf>,
    queries: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    reenter: Option<Box<dyn Fn()>>,
}

impl IDataObject_Impl for Files_Impl {
    fn GetData(&self, _: *const FORMATETC) -> WinResult<STGMEDIUM> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if let Some(reenter) = &self.reenter {
            reenter();
        }
        use std::os::windows::ffi::OsStrExt;
        let mut wide = Vec::new();
        for path in &self.paths {
            wide.extend(path.as_os_str().encode_wide());
            wide.push(0);
        }
        wide.push(0);
        let header = std::mem::size_of::<DROPFILES>();
        unsafe {
            let block = GlobalAlloc(GHND, header + wide.len() * 2)?;
            let pointer = GlobalLock(block).cast::<u8>();
            assert!(!pointer.is_null());
            pointer.cast::<DROPFILES>().write(DROPFILES {
                pFiles: header as u32,
                fWide: true.into(),
                ..Default::default()
            });
            std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer.add(header).cast(), wide.len());
            let _ = GlobalUnlock(block);
            Ok(STGMEDIUM {
                tymed: 1,
                u: STGMEDIUM_0 { hGlobal: block },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, _: *const FORMATETC) -> HRESULT {
        self.queries.fetch_add(1, Ordering::Relaxed);
        S_OK
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, _: u32) -> WinResult<IEnumFORMATETC> {
        Err(E_NOTIMPL.into())
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<'_, IAdviseSink>) -> WinResult<u32> {
        Err(E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _: u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

struct NativeWindow {
    id: window::Id,
    hwnd: HWND,
    target: IDropTarget,
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd).unwrap();
        }
    }
}

fn native_window() -> NativeWindow {
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("Adit native drop unit test"),
            WS_OVERLAPPEDWINDOW,
            320,
            210,
            640,
            480,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    };
    let id = window::Id::unique();
    assert!(register(id, hwnd.0 as isize));
    let target = WINDOWS.with(|windows| windows.borrow()[&id]._target.clone());
    NativeWindow { id, hwnd, target }
}

fn screen(window: &NativeWindow, x: i32, y: i32) -> POINTL {
    let mut point = POINT { x, y };
    assert!(unsafe { ClientToScreen(window.hwnd, &mut point) }.as_bool());
    POINTL {
        x: point.x,
        y: point.y,
    }
}

fn publish(window: &NativeWindow, policy: Policy) {
    let metrics = dimensions(window.hwnd.0 as isize).unwrap();
    let scale = metrics.dpi as f32 / 96.0;
    let bounds = iced::Rectangle::new(
        Point::ORIGIN,
        iced::Size::new(metrics.width as f32 / scale, metrics.height as f32 / scale),
    );
    let regions = match &policy {
        Policy::Panel {
            instance,
            revision,
            cwd,
        } => vec![Region {
            destination: Destination::Panel {
                instance: *instance,
                revision: *revision,
                cwd: cwd.clone(),
            },
            bounds,
        }],
        _ => Vec::new(),
    };
    let mut state = shared().lock().unwrap();
    state.reconcile(
        window.id,
        Context {
            policy: policy.clone(),
            selected: None,
            panes: Vec::new(),
            metrics: [0.0; 6],
            main_view: MainView::Terminal,
            tile_mode: TileMode::Grid,
            local_layout: [false; 2],
        },
    );
    state.publish(window.id, Some(metrics), bounds, policy, regions);
}

fn on_sta(test: impl FnOnce() + Send + 'static) {
    let _serial = test_support::SERIAL.lock().unwrap();
    std::thread::spawn(move || {
        assert!(!unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        }
        .0
        .is_null());
        test();
    })
    .join()
    .unwrap();
}

#[test]
fn screen_to_client_uses_the_actual_nonzero_window_origin_and_physical_dpi_units() {
    on_sta(|| {
        let window = native_window();
        let point = screen(&window, 240, 120);
        assert_ne!((point.x, point.y), (240, 120));
        let (metrics, x, y) = sample(window.hwnd.0 as isize, &point).unwrap();
        assert_eq!((x, y), (240, 120));
        assert_eq!(
            metrics.logical_point(x, y),
            Some(Point::new(
                240.0 * 96.0 / metrics.dpi as f32,
                120.0 * 96.0 / metrics.dpi as f32
            ))
        );
        assert!(sample(window.hwnd.0 as isize, &screen(&window, -1, 10))
            .unwrap()
            .0
            .logical_point(-1, 10)
            .is_none());
    });
}

#[test]
fn real_com_drop_keeps_the_complete_unicode_batch_and_rechecks_after_foreign_getdata() {
    on_sta(|| {
        let window = native_window();
        let policy = Policy::Panel {
            instance: SessionId::new(),
            revision: 7,
            cwd: "/srv".into(),
        };
        publish(&window, policy.clone());
        let paths: Vec<PathBuf> = (0..150)
            .map(|i| format!(r"C:\报告\{i}.txt").into())
            .collect();
        let reads = Arc::new(AtomicUsize::new(0));
        let object: IDataObject = Files {
            paths: paths.clone(),
            reads: reads.clone(),
            queries: Arc::new(AtomicUsize::new(0)),
            reenter: None,
        }
        .into();
        let point = screen(&window, 100, 100);
        unsafe {
            let mut effect = DROPEFFECT_COPY;
            window
                .target
                .DragEnter(&object, MODIFIERKEYS_FLAGS::default(), point, &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY);
            effect = DROPEFFECT_COPY;
            window
                .target
                .Drop(&object, MODIFIERKEYS_FLAGS::default(), point, &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY);
        }
        let requests = drain();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].window, window.id);
        assert_eq!(requests[0].paths, paths);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        let id = window.id;
        let object: IDataObject = Files {
            paths: vec!["old.txt".into()],
            reads,
            queries: Arc::new(AtomicUsize::new(0)),
            reenter: Some(Box::new(move || invalidate(id))),
        }
        .into();
        unsafe {
            let mut effect = DROPEFFECT_COPY;
            window
                .target
                .DragEnter(&object, MODIFIERKEYS_FLAGS::default(), point, &mut effect)
                .unwrap();
            effect = DROPEFFECT_COPY;
            window
                .target
                .Drop(&object, MODIFIERKEYS_FLAGS::default(), point, &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_NONE);
        }
        assert!(drain().is_empty());
    });
}

#[test]
fn disabled_rdp_or_modal_surface_never_queries_the_incoming_object_or_replaces_a_target() {
    on_sta(|| {
        let window = native_window();
        publish(&window, Policy::Disabled);
        assert!(!register(window::Id::unique(), window.hwnd.0 as isize));
        let queries = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let object: IDataObject = Files {
            paths: vec!["never-read.txt".into()],
            queries: queries.clone(),
            reads: reads.clone(),
            reenter: None,
        }
        .into();
        let point = screen(&window, 100, 100);
        unsafe {
            let mut effect = DROPEFFECT_COPY;
            window
                .target
                .DragEnter(&object, MODIFIERKEYS_FLAGS::default(), point, &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_NONE);
            effect = DROPEFFECT_COPY;
            window
                .target
                .Drop(&object, MODIFIERKEYS_FLAGS::default(), point, &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_NONE);
        }
        assert_eq!(queries.load(Ordering::Relaxed), 0);
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        let id = window.id;
        drop(window);
        assert!(!shared().lock().unwrap().slots.contains_key(&id));
    });
}
