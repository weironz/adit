use super::remote_tree::{Node, ROOT, RemoteReader, RemoteTree, Requester};
use super::{ChunkBridge, ClipFile};
use crate::i18n::t;
use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSFilePromiseProvider, NSFilePromiseProviderDelegate,
    NSFilePromiseReceiver, NSImage, NSImageNameMultipleDocuments, NSPasteboard,
    NSPasteboardTypeFileURL, NSTextField, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSLocalizedDescriptionKey, NSObject, NSObjectProtocol,
    NSOperationQueue, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

type Current = Arc<dyn Fn() -> bool + Send + Sync>;

struct Source {
    tree: RemoteTree,
    bridge: ChunkBridge,
    reader: RemoteReader,
    current: Current,
}

impl Source {
    fn live(&self) -> bool {
        self.bridge.is_open() && (self.current)()
    }

    fn check_live(&self) -> io::Result<()> {
        if self.live() {
            Ok(())
        } else {
            Err(io::Error::other(t("远端文件选择已失效，请重新复制")))
        }
    }

    fn write_to(&self, node: &Node, destination: &Path) -> io::Result<()> {
        self.check_live()?;
        let parent = destination.parent().ok_or(io::ErrorKind::InvalidInput)?;
        if !destination.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        // Stage next to the receiver's URL so a failed range never exposes a
        // partial file, and commit without replacing any existing destination.
        let stage = tempfile::tempdir_in(parent)?;
        let payload = stage.path().join("payload");
        self.write_node(node, &payload)?;
        self.check_live()?;
        let from = CString::new(payload.as_os_str().as_bytes())?;
        let to = CString::new(destination.as_os_str().as_bytes())?;
        // SAFETY: Both paths are owned NUL-terminated strings.
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn write_node(&self, node: &Node, path: &Path) -> io::Result<()> {
        self.check_live()?;
        if node.is_dir {
            fs::create_dir(path)?;
            for child in self.tree.children(node.ino) {
                self.write_node(child, &path.join(&child.name))?;
            }
        } else {
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            let mut offset = 0;
            while offset < node.size {
                let bytes = self
                    .reader
                    .read_live(node, offset, u32::MAX, || (self.current)())
                    .map_err(|_| io::Error::other(t("无法读取远端剪贴板文件内容")))?;
                file.write_all(&bytes)?;
                offset += bytes.len() as u64;
            }
            file.sync_all()?;
        }
        Ok(())
    }
}

struct Promise {
    source: Arc<Source>,
    node: Node,
    queue: Retained<NSOperationQueue>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = Promise]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(
            &self,
            _: &NSFilePromiseProvider,
            _: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(&self.ivars().node.name)
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(
            &self,
            _: &NSFilePromiseProvider,
        ) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let state = self.ivars();
            let result = if url.isFileURL() {
                // SAFETY: AppKit's file URL owns the NUL-terminated file system
                // representation for this callback.
                let path = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
                state
                    .source
                    .write_to(&state.node, Path::new(OsStr::from_bytes(path.to_bytes())))
            } else {
                Err(io::ErrorKind::InvalidInput.into())
            };
            match result {
                Ok(()) => completion.call((std::ptr::null_mut(),)),
                Err(error) => {
                    let description = NSString::from_str(&error.to_string());
                    // SAFETY: NSLocalizedDescriptionKey expects an NSString.
                    let error = unsafe {
                        let info = NSDictionary::from_slices(
                            &[NSLocalizedDescriptionKey],
                            &[description.as_ref() as &AnyObject],
                        );
                        NSError::errorWithDomain_code_userInfo(
                            &NSString::from_str("Adit.RemoteFilePromise"),
                            1,
                            Some(&info),
                        )
                    };
                    completion.call((Retained::as_ptr(&error).cast_mut(),));
                }
            }
        }
    }
);

impl PromiseDelegate {
    fn provider(
        source: Arc<Source>,
        node: Node,
        queue: Retained<NSOperationQueue>,
    ) -> Retained<NSFilePromiseProvider> {
        let file_type = NSString::from_str(if node.is_dir {
            "public.folder"
        } else {
            "public.data"
        });
        let this = Self::alloc().set_ivars(Promise {
            source,
            node,
            queue,
        });
        // SAFETY: Initialize the NSObject superclass after storing our ivars.
        let delegate: Retained<Self> = unsafe { msg_send![super(this), init] };
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &file_type,
            ProtocolObject::from_ref(&*delegate),
        );
        // SAFETY: userInfo is opaque to AppKit. The weak delegate must stay
        // alive for both pasteboard and drag consumers of this provider.
        unsafe { provider.setUserInfo(Some(&delegate)) };
        provider
    }
}

struct Offer {
    source: Arc<Source>,
    providers: Vec<Retained<NSFilePromiseProvider>>,
    pasteboard_count: isize,
}

thread_local! {
    static OFFER: RefCell<Option<Offer>> = const { RefCell::new(None) };
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn make_source(
    files: &[ClipFile],
    bridge: ChunkBridge,
    current: Current,
    request: Requester,
    chunk: u32,
) -> Result<Arc<Source>, &'static str> {
    adit_session::validate_clipboard_file_list(files)?;
    Ok(Arc::new(Source {
        tree: RemoteTree::new(files)?,
        reader: RemoteReader::new(bridge.clone(), request, chunk),
        bridge,
        current,
    }))
}

pub(super) fn offer(
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    current: Current,
    request: Requester,
    chunk: u32,
) {
    let Some(_mtm) = MainThreadMarker::new() else {
        return;
    };
    let source = match make_source(&files, bridge, current, request, chunk) {
        Ok(source) if source.live() => source,
        Ok(_) => return,
        Err(error) => {
            ERROR.with_borrow_mut(|slot| *slot = Some(error.into()));
            return;
        }
    };
    close_drag_window();
    let queue = NSOperationQueue::new();
    queue.setMaxConcurrentOperationCount(4);
    let providers: Vec<_> = source
        .tree
        .children(ROOT)
        .map(|node| PromiseDelegate::provider(source.clone(), node.clone(), queue.clone()))
        .collect();
    let writers: Vec<_> = providers
        .iter()
        .map(|p| ProtocolObject::from_ref(&**p))
        .collect();
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    let published = board.writeObjects(&NSArray::from_slice(&writers));
    OFFER.with_borrow_mut(|slot| {
        *slot = Some(Offer {
            source,
            providers,
            pasteboard_count: board.changeCount(),
        })
    });
    if !published {
        ERROR.with_borrow_mut(|slot| {
            *slot = Some(t("无法在 macOS 剪贴板上发布远端文件承诺").into())
        });
    }
}

pub(super) fn take_error() -> Option<String> {
    ERROR.with_borrow_mut(Option::take)
}

pub(super) fn text_token() -> Option<u32> {
    let _mtm = MainThreadMarker::new()?;
    let board = NSPasteboard::generalPasteboard();
    let before = board.changeCount();
    let file_types = NSFilePromiseReceiver::readableDraggedTypes();
    // SAFETY: This AppKit constant points to an immutable NSString.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    let legacy_files = NSString::from_str("NSFilenamesPboardType");
    let has_files = board.availableTypeFromArray(&file_types).is_some()
        || board
            .availableTypeFromArray(&NSArray::from_slice(&[file_url, &legacy_files]))
            .is_some();
    (!has_files && before == board.changeCount()).then_some(before as u32)
}

pub(super) fn shutdown() {
    if MainThreadMarker::new().is_none() {
        return;
    }
    if let Some(offer) = OFFER.with_borrow_mut(Option::take) {
        offer.source.bridge.close();
        let board = NSPasteboard::generalPasteboard();
        if board.changeCount() == offer.pasteboard_count {
            board.clearContents();
        }
    }
    close_drag_window();
}

fn close_drag_window() {
    if let Some(window) = WINDOW.with_borrow_mut(Option::take) {
        window.close();
    }
}

struct Drag {
    source: Arc<Source>,
    providers: Vec<Retained<NSFilePromiseProvider>>,
    dragging: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSTextField))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Drag]
    struct DragView;

    impl DragView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _: &NSEvent) {}

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let state = self.ivars();
            if state.dragging.get() || !state.source.live() {
                return;
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let frame = NSRect::new(NSPoint::new(point.x - 16.0, point.y - 16.0), NSSize::new(32.0, 32.0));
            // SAFETY: This AppKit constant points to an immutable NSString.
            let icon = NSImage::imageNamed(unsafe { NSImageNameMultipleDocuments });
            let items: Vec<_> = state.providers.iter().map(|provider| {
                let item = NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(), ProtocolObject::from_ref(&**provider),
                );
                // SAFETY: Dragging items accept an NSImage as their preview.
                unsafe { item.setDraggingFrame_contents(frame, icon.as_deref().map(|i| i as &AnyObject)) };
                item
            }).collect();
            state.dragging.set(true);
            self.beginDraggingSessionWithItems_event_source(
                &NSArray::from_retained_slice(&items), event, ProtocolObject::from_ref(self),
            );
        }
    }

    unsafe impl NSObjectProtocol for DragView {}

    unsafe impl NSDraggingSource for DragView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(
            &self, _: &NSDraggingSession, _: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn drag_ended(
            &self, _: &NSDraggingSession, _: NSPoint, _: NSDragOperation,
        ) {
            self.ivars().dragging.set(false);
        }
    }
);

pub(crate) fn open_drag_window(active: &ChunkBridge, title: &str) -> Result<(), &'static str> {
    let mtm = MainThreadMarker::new().ok_or(t("文件拖出窗口必须在主线程上打开"))?;
    let (source, providers) = OFFER
        .with_borrow(|offer| {
            offer
                .as_ref()
                .filter(|offer| active.same_offer(&offer.source.bridge) && offer.source.live())
                .map(|offer| (offer.source.clone(), offer.providers.clone()))
        })
        .ok_or(t("请先在该远端会话中选择并复制文件，再打开文件拖出窗口"))?;

    let size = NSSize::new(360.0, 76.0);
    let rect = NSRect::new(NSPoint::ZERO, size);
    // SAFETY: The window is retained in WINDOW, not released by -close.
    let window = unsafe {
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        window.setReleasedWhenClosed(false);
        window
    };
    let label_frame = NSRect::new(NSPoint::new(12.0, 12.0), NSSize::new(336.0, 44.0));
    let view = DragView::alloc(mtm).set_ivars(Drag {
        source,
        providers,
        dragging: Cell::new(false),
    });
    // SAFETY: Initialize the NSTextField superclass with our content frame.
    let view: Retained<DragView> = unsafe { msg_send![super(view), initWithFrame: label_frame] };
    view.setEditable(false);
    view.setSelectable(false);
    view.setBezeled(false);
    view.setDrawsBackground(false);
    view.setStringValue(&NSString::from_str(t(
        "按住此处，将远端文件拖到访达（只复制）",
    )));
    let content = window.contentView().ok_or(t("无法创建文件拖出窗口"))?;
    content.addSubview(&view);
    window.setTitle(&NSString::from_str(&format!(
        "{} — {title}",
        t("远端文件拖出")
    )));
    window.center();
    window.makeKeyAndOrderFront(None);
    if let Some(old) = WINDOW.with_borrow_mut(|slot| slot.replace(window)) {
        old.close();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn file(name: &str, size: u64) -> ClipFile {
        ClipFile {
            name: name.into(),
            size,
            is_dir: false,
        }
    }

    #[test]
    fn metadata_only_until_write_then_streams_directory_members_by_original_index() {
        let bytes = b"abcdefgh";
        let bridge = ChunkBridge::new();
        let replies = bridge.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let trace = requests.clone();
        let source = make_source(
            &[
                file("z.bin", 1),
                file("docs\\content.txt", bytes.len() as u64),
                file("docs\\empty", 0),
            ],
            bridge,
            Arc::new(|| true),
            Arc::new(move |id, index, offset, len| {
                trace.lock().unwrap().push((index, offset, len));
                let start = offset as usize;
                replies.deliver(id, Some(bytes[start..start + len as usize].to_vec()));
            }),
            3,
        )
        .unwrap();
        assert!(requests.lock().unwrap().is_empty());
        let folder = source.tree.children(ROOT).next().unwrap();
        assert_eq!(folder.name, "docs");
        let directory = tempfile::tempdir().unwrap();
        let dest = directory.path().join("renamed by receiver");
        source.write_to(folder, &dest).unwrap();
        assert_eq!(fs::read(dest.join("content.txt")).unwrap(), bytes);
        assert!(fs::read(dest.join("empty")).unwrap().is_empty());
        assert_eq!(*requests.lock().unwrap(), [(1, 0, 3), (1, 3, 3), (1, 6, 2)]);
    }

    #[test]
    fn failures_never_publish_partial_files_or_overwrite_existing_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local");
        let live = Arc::new(AtomicBool::new(true));
        for mode in 0..4 {
            live.store(true, Ordering::Relaxed);
            let bridge = ChunkBridge::new();
            let replies = bridge.clone();
            let source_live = live.clone();
            let callback_live = live.clone();
            let source = make_source(
                &[file("remote", 6)],
                bridge,
                Arc::new(move || source_live.load(Ordering::Relaxed)),
                Arc::new(move |id, _, offset, len| {
                    let data = match (mode, offset) {
                        (0, 3) => vec![],
                        (1, 3) => vec![5; len as usize + 1],
                        (2, 3) => {
                            callback_live.store(false, Ordering::Relaxed);
                            vec![5; len as usize]
                        }
                        _ => vec![5; len as usize],
                    };
                    replies.deliver(id, Some(data));
                }),
                3,
            )
            .unwrap();
            if mode == 3 {
                fs::write(&path, b"existing").unwrap();
            }
            assert!(
                source
                    .write_to(source.tree.children(ROOT).next().unwrap(), &path)
                    .is_err()
            );
            if mode == 3 {
                assert_eq!(fs::read(&path).unwrap(), b"existing");
            } else {
                assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn unsafe_metadata_and_revoked_empty_files_are_refused_without_requests() {
        let bridge = ChunkBridge::new();
        let request: Requester = Arc::new(|_, _, _, _| panic!("unexpected request"));
        for files in [
            vec![file("..\\outside", 1)],
            vec![file("A", 1), file("a", 2)],
        ] {
            assert!(
                make_source(
                    &files,
                    bridge.clone(),
                    Arc::new(|| true),
                    request.clone(),
                    3
                )
                .is_err()
            );
        }
        let source = make_source(
            &[file("empty", 0)],
            bridge.clone(),
            Arc::new(|| true),
            request,
            3,
        )
        .unwrap();
        bridge.close();
        let dir = tempfile::tempdir().unwrap();
        assert!(
            source
                .write_to(
                    source.tree.children(ROOT).next().unwrap(),
                    &dir.path().join("empty")
                )
                .is_err()
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
