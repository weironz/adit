//! Clipboard ownership and FUSE lifetime run off the iced thread. X11 selections
//! expose URI metadata; file reads are serviced by the captured remote offer.
use super::linux_fuse::RemoteMount;
use super::{ChunkBridge, ClipFile, MAX_CLIPBOARD_FILES};
use clipboard_rs::{
    Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum FileSelection {
    NonFile,
    Local,
    RemoteOwned,
    #[default]
    Unknown,
}

struct Offer {
    generation: u64,
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    request: super::remote_tree::Requester,
    current: super::remote_tree::CurrentOffer,
    chunk: u32,
}

#[derive(Default)]
struct Shared {
    latest: Mutex<Option<Offer>>,
    generation: AtomicU64,
    sequence: AtomicU32,
    dirty: AtomicBool,
    change_epoch: AtomicU64,
    published_epoch: AtomicU64,
    stop: AtomicBool,
    failed: AtomicBool,
    watcher_failed: AtomicBool,
    paths: Mutex<Option<Vec<PathBuf>>>,
    file_selection: Mutex<FileSelection>,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn mark_dirty(&self) {
        self.change_epoch.fetch_add(1, Ordering::AcqRel);
        self.dirty.store(true, Ordering::Release);
    }

    fn current_sequence(&self) -> u32 {
        if self.failed.load(Ordering::Acquire)
            || self.watcher_failed.load(Ordering::Acquire)
            || self.stop.load(Ordering::Acquire)
            || self.dirty.load(Ordering::Acquire)
        {
            return 0;
        }
        let epoch = self.change_epoch.load(Ordering::Acquire);
        let published = self.published_epoch.load(Ordering::Acquire);
        let sequence = self.sequence.load(Ordering::Acquire);
        if epoch == 0
            || epoch != published
            || self.change_epoch.load(Ordering::Acquire) != epoch
            || self.dirty.load(Ordering::Acquire)
            || self.failed.load(Ordering::Acquire)
            || self.watcher_failed.load(Ordering::Acquire)
            || self.stop.load(Ordering::Acquire)
        {
            0
        } else {
            sequence
        }
    }

    fn retire(&self, message: &'static str, watcher: bool) {
        if watcher {
            self.watcher_failed.store(true, Ordering::Release);
        } else {
            self.failed.store(true, Ordering::Release);
        }
        self.mark_dirty();
        if !self.stop.load(Ordering::Acquire) {
            self.error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| message.into());
        }
    }

    fn accepts_remote_offers(&self) -> bool {
        !self.failed.load(Ordering::Acquire) && !self.stop.load(Ordering::Acquire)
    }
}

struct RetireOnExit {
    shared: Arc<Shared>,
    message: &'static str,
    watcher: bool,
}

impl Drop for RetireOnExit {
    fn drop(&mut self) {
        self.shared.retire(self.message, self.watcher);
    }
}

struct Service {
    shared: Arc<Shared>,
    wake: SyncSender<()>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

struct Changes(Arc<Shared>);
impl ClipboardHandler for Changes {
    fn on_clipboard_change(&mut self) {
        self.0.mark_dirty();
    }
}

static SERVICE: OnceLock<Option<Service>> = OnceLock::new();
static START_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn service() -> Option<&'static Service> {
    SERVICE
        .get_or_init(|| {
            let shared = Arc::new(Shared::default());
            let (wake, events) = sync_channel(1);
            let worker_shared = shared.clone();
            let worker = thread::Builder::new()
                .name("adit-file-clipboard".into())
                .spawn(move || {
                    let _retire = RetireOnExit {
                        shared: worker_shared.clone(),
                        message: "文件剪贴板工作线程已退出；本地剪贴板同步已暂停",
                        watcher: false,
                    };
                    if let Err(error) = run(worker_shared.clone(), events) {
                        *worker_shared
                            .error
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(error);
                    }
                })
                .map_err(|_| {
                    *START_ERROR.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some("无法启动文件剪贴板工作线程".into());
                })
                .ok()?;
            Some(Service {
                shared,
                wake,
                worker: Mutex::new(Some(worker)),
            })
        })
        .as_ref()
}

fn run(shared: Arc<Shared>, events: std::sync::mpsc::Receiver<()>) -> Result<(), String> {
    let clipboard =
        ClipboardContext::new().map_err(|_| "无法连接 X11 文件剪贴板；需要可用的 DISPLAY")?;
    let mut watcher = ClipboardWatcherContext::new().map_err(|_| "无法监视 X11 文件剪贴板")?;
    watcher.add_handler(Changes(shared.clone()));
    let stop_watcher = watcher.get_shutdown_channel();
    let watcher_shared = shared.clone();
    let watcher = thread::Builder::new()
        .name("adit-clipboard-watch".into())
        .spawn(move || {
            // Also runs during unwinding: start_watch can panic when X11 fails.
            let _retire = RetireOnExit {
                shared: watcher_shared,
                message: "文件剪贴板监听已退出；本地剪贴板同步已暂停",
                watcher: true,
            };
            watcher.start_watch();
        })
        .map_err(|_| "无法启动文件剪贴板监视线程")?;
    let mut mount: Option<RemoteMount> = None;
    shared.mark_dirty();
    while !shared.stop.load(Ordering::Acquire) && !shared.failed.load(Ordering::Acquire) {
        let _ = events.recv_timeout(Duration::from_millis(100));
        if shared.stop.load(Ordering::Acquire) || shared.failed.load(Ordering::Acquire) {
            break;
        }
        let offer = shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(offer) = offer {
            if offer.files.is_empty() {
                offer.bridge.close();
                drop(mount.take());
                *shared.paths.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *shared
                    .file_selection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = FileSelection::Unknown;
                shared.mark_dirty();
                continue;
            }
            let generation = offer.generation;
            let result = RemoteMount::new_guarded(
                offer.files,
                offer.bridge.clone(),
                offer.request,
                offer.current,
                offer.chunk,
            );
            if generation == shared.generation.load(Ordering::Acquire)
                && !shared.stop.load(Ordering::Acquire)
            {
                match result {
                    Ok(next) => {
                        if clipboard.set_files(next.paths.clone()).is_ok() {
                            mount = Some(next);
                            shared.mark_dirty();
                        } else {
                            offer.bridge.close();
                            *shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some("无法发布 RDP 文件剪贴板".into());
                        }
                    }
                    Err(error) => {
                        offer.bridge.close();
                        *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                    }
                }
            }
        }
        if !shared.watcher_failed.load(Ordering::Acquire)
            && shared.dirty.swap(false, Ordering::AcqRel)
        {
            let epoch = shared.change_epoch.load(Ordering::Acquire);
            // Never fetch arbitrary text/image formats, and never reflect our
            // own virtual paths back as a local-to-remote file offer.
            let formats = clipboard.available_formats().ok();
            let contents = formats
                .as_ref()
                .filter(|formats| formats.iter().any(|format| format == "text/uri-list"))
                .and_then(|_| clipboard.get_buffer("text/uri-list").ok());
            let (selection, paths) =
                classify_file_selection(formats.as_deref(), contents.as_deref(), |path| {
                    mount.as_ref().is_some_and(|mount| mount.owns(path))
                });
            *shared.paths.lock().unwrap_or_else(|e| e.into_inner()) = paths;
            *shared
                .file_selection
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = selection;
            if shared
                .sequence
                .fetch_add(1, Ordering::Release)
                .wrapping_add(1)
                == 0
            {
                shared.sequence.fetch_add(1, Ordering::Release);
            }
            shared.published_epoch.store(epoch, Ordering::Release);
        }
    }
    drop(mount);
    stop_watcher.stop();
    let _ = watcher.join();
    Ok(())
}

fn classify_file_selection(
    formats: Option<&[String]>,
    contents: Option<&[u8]>,
    is_remote: impl Fn(&Path) -> bool,
) -> (FileSelection, Option<Vec<PathBuf>>) {
    let Some(formats) = formats else {
        return (FileSelection::Unknown, None);
    };
    // GTK adds a private text-buffer target alongside UTF8_STRING for normal
    // text copies. Alone it is not enough to identify an unknown selection.
    let gtk_text = formats.iter().any(|format| format == "UTF8_STRING");
    let non_file = |format: &str| {
        known_non_file_format(format)
            || (gtk_text
                && matches!(
                    format,
                    "GTK_TEXT_BUFFER_CONTENTS"
                        | "text/_moz_htmlcontext"
                        | "text/_moz_htmlinfo"
                        | "application/x-moz-nativehtml"
                ))
    };
    let uri_list = formats.iter().any(|format| format == "text/uri-list");
    if !uri_list {
        return if formats.iter().any(|format| !non_file(format)) {
            (FileSelection::Unknown, None)
        } else {
            (FileSelection::NonFile, None)
        };
    }
    if formats.iter().any(|format| {
        format != "text/uri-list" && !non_file(format) && !known_file_companion_format(format)
    }) {
        return (FileSelection::Unknown, None);
    }
    let Some(contents) = contents.filter(|data| data.len() <= 8 * 1024 * 1024) else {
        return (FileSelection::Unknown, None);
    };
    let Ok(text) = std::str::from_utf8(contents) else {
        return (FileSelection::Unknown, None);
    };
    let mut values = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with("file:") || values.len() == MAX_CLIPBOARD_FILES {
            return (FileSelection::Unknown, None);
        }
        values.push(line.to_owned());
    }
    let Some(paths) = local_paths(values, None) else {
        return (FileSelection::Unknown, None);
    };
    let remote = paths.iter().filter(|path| is_remote(path)).count();
    if remote == 0 {
        (FileSelection::Local, Some(paths))
    } else if remote == paths.len() {
        (FileSelection::RemoteOwned, None)
    } else {
        (FileSelection::Unknown, None)
    }
}

fn known_file_companion_format(format: &str) -> bool {
    matches!(
        format,
        "x-special/gnome-copied-files"
            | "x-special/nautilus-clipboard"
            | "x-special/mate-copied-files"
            | "x-special/xfce-copied-files"
            | "application/x-kde-cutselection"
    )
}

fn known_non_file_format(format: &str) -> bool {
    matches!(
        format,
        "UTF8_STRING"
            | "STRING"
            | "TEXT"
            | "COMPOUND_TEXT"
            | "text/plain"
            | "text/html"
            | "text/rtf"
            | "text/markdown"
            | "text/csv"
            | "application/rtf"
            | "application/json"
            | "application/x-qt-image"
    ) || format.starts_with("text/plain;")
        || format.starts_with("image/")
}

fn local_paths(values: Vec<String>, mount: Option<&RemoteMount>) -> Option<Vec<PathBuf>> {
    if values.is_empty()
        || values.len() > MAX_CLIPBOARD_FILES
        || values.iter().map(String::len).sum::<usize>() > 8 * 1024 * 1024
    {
        return None;
    }
    let mut paths = Vec::with_capacity(values.len());
    for value in values {
        let path = if value.starts_with("file:") {
            let url = url::Url::parse(&value).ok()?;
            if url.scheme() != "file"
                || url.query().is_some()
                || url.fragment().is_some()
                || url.host_str().is_some_and(|host| host != "localhost")
            {
                return None;
            }
            url.to_file_path().ok()?
        } else {
            PathBuf::from(value)
        };
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return None;
        }
        if mount.is_some_and(|mount| mount.owns(&path)) {
            return None;
        }
        paths.push(path);
    }
    Some(paths)
}

pub(super) fn offer(
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    requester: adit_session::RdpFileRequester,
    chunk: u32,
) {
    let Some(service) = service() else {
        bridge.close();
        return;
    };
    let shared = &service.shared;
    if !shared.accepts_remote_offers() {
        bridge.close();
        *shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
            Some("文件剪贴板工作线程不可用；远程连接不受影响".into());
        return;
    }
    let generation = shared
        .generation
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let current = requester.clone();
    let reply = bridge.clone();
    let previous = shared
        .latest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(Offer {
            generation,
            files,
            bridge,
            request: Arc::new(move |stream, index, offset, length| {
                if requester
                    .try_request(stream, index, offset, length)
                    .is_err()
                {
                    reply.deliver(stream, None);
                }
            }),
            current: Arc::new(move || current.is_current()),
            chunk,
        });
    if let Some(previous) = previous {
        previous.bridge.close();
    }
    let _ = service.wake.try_send(());
}

pub(super) fn sequence() -> u32 {
    service().map_or(0, |service| {
        if service.shared.failed.load(Ordering::Acquire) {
            0
        } else {
            service.shared.current_sequence()
        }
    })
}

pub(super) fn file_selection() -> FileSelection {
    service().map_or(FileSelection::Unknown, |service| {
        if service.shared.failed.load(Ordering::Acquire)
            || service.shared.watcher_failed.load(Ordering::Acquire)
            || service.shared.stop.load(Ordering::Acquire)
        {
            FileSelection::Unknown
        } else {
            *service
                .shared
                .file_selection
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        }
    })
}

pub(super) fn unavailable() -> bool {
    match SERVICE.get() {
        Some(Some(service)) => {
            service.shared.failed.load(Ordering::Acquire)
                || service.shared.watcher_failed.load(Ordering::Acquire)
                || service.shared.stop.load(Ordering::Acquire)
        }
        Some(None) => true,
        None => false,
    }
}

pub(super) fn paths() -> Option<Vec<PathBuf>> {
    let shared = &service()?.shared;
    if shared.failed.load(Ordering::Acquire)
        || shared.watcher_failed.load(Ordering::Acquire)
        || shared.stop.load(Ordering::Acquire)
    {
        return None;
    }
    shared
        .paths
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(super) fn take_error() -> Option<String> {
    SERVICE
        .get()
        .and_then(Option::as_ref)
        .and_then(|service| {
            service
                .shared
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        })
        .or_else(|| START_ERROR.lock().unwrap_or_else(|e| e.into_inner()).take())
}

pub(super) fn shutdown() {
    let Some(Some(service)) = SERVICE.get() else {
        return;
    };
    service.shared.stop.store(true, Ordering::Release);
    if let Some(pending) = service
        .shared
        .latest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        pending.bridge.close();
    }
    let _ = service.wake.try_send(());
    if let Some(worker) = service
        .worker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_exit_and_panic_retire_the_published_sequence() {
        for panics in [false, true] {
            let shared = Arc::new(Shared::default());
            shared.change_epoch.store(1, Ordering::Release);
            shared.published_epoch.store(1, Ordering::Release);
            shared.sequence.store(7, Ordering::Release);
            assert_eq!(shared.current_sequence(), 7);
            let watcher_shared = shared.clone();
            let result = thread::spawn(move || {
                let _retire = RetireOnExit {
                    shared: watcher_shared,
                    message: "synthetic watcher stopped",
                    watcher: true,
                };
                assert!(!panics, "synthetic watcher panic");
            })
            .join();
            assert_eq!(result.is_err(), panics);
            assert!(shared.watcher_failed.load(Ordering::Acquire));
            assert!(!shared.failed.load(Ordering::Acquire));
            assert_eq!(shared.current_sequence(), 0);
            assert!(shared.accepts_remote_offers());
            assert_eq!(
                shared.error.lock().unwrap().as_deref(),
                Some("synthetic watcher stopped")
            );
        }

        // A dead worker cannot publish remote files; a dead watcher alone can.
        let shared = Arc::new(Shared::default());
        drop(RetireOnExit {
            shared: shared.clone(),
            message: "synthetic worker stopped",
            watcher: false,
        });
        assert!(shared.failed.load(Ordering::Acquire));
        assert!(!shared.accepts_remote_offers());
    }

    #[test]
    fn change_notification_invalidates_sequence_until_latest_snapshot_is_published() {
        let shared = Shared::default();
        shared.mark_dirty();
        assert_eq!(shared.current_sequence(), 0);
        assert!(shared.dirty.swap(false, Ordering::AcqRel));
        let first = shared.change_epoch.load(Ordering::Acquire);
        shared.sequence.store(1, Ordering::Release);
        shared.published_epoch.store(first, Ordering::Release);
        assert_eq!(shared.current_sequence(), 1);

        shared.mark_dirty();
        assert_eq!(shared.current_sequence(), 0);
        assert!(shared.dirty.swap(false, Ordering::AcqRel));
        let stale = shared.change_epoch.load(Ordering::Acquire);
        shared.mark_dirty();
        shared.sequence.store(2, Ordering::Release);
        shared.published_epoch.store(stale, Ordering::Release);
        assert_eq!(shared.current_sequence(), 0);

        assert!(shared.dirty.swap(false, Ordering::AcqRel));
        shared.sequence.store(3, Ordering::Release);
        shared.published_epoch.store(
            shared.change_epoch.load(Ordering::Acquire),
            Ordering::Release,
        );
        assert_eq!(shared.current_sequence(), 3);
    }

    #[test]
    fn new_epoch_invalidates_the_snapshot_before_dirty_flag_is_set() {
        let shared = Shared::default();
        shared.sequence.store(1, Ordering::Release);
        shared.change_epoch.store(1, Ordering::Release);
        shared.published_epoch.store(1, Ordering::Release);
        assert_eq!(shared.current_sequence(), 1);

        // Model the interleaving between the two writes in mark_dirty.
        shared.change_epoch.fetch_add(1, Ordering::AcqRel);
        assert_eq!(shared.current_sequence(), 0);
        shared.dirty.store(true, Ordering::Release);
        assert_eq!(shared.current_sequence(), 0);
    }
    use std::path::Path;

    #[test]
    fn clipboard_uris_must_name_local_absolute_files_without_a_remote_authority() {
        assert_eq!(
            local_paths(vec!["file:///tmp/a%20b".into()], None).unwrap(),
            [Path::new("/tmp/a b")]
        );
        for value in [
            "file://remote/tmp/file",
            "https://example.test/file",
            "../outside",
            "file:///tmp/a?x=1",
            "file:///tmp/a#b",
            "file:///tmp/%00",
        ] {
            assert!(local_paths(vec![value.into()], None).is_none(), "{value}");
        }
    }

    #[test]
    fn file_formats_are_distinct_from_non_files_and_unknown_contents() {
        let text = vec!["UTF8_STRING".to_owned()];
        let files = vec!["text/uri-list".to_owned()];
        let other_files = vec!["x-special/gnome-copied-files".to_owned()];
        let unknown_format = vec!["Unknown".to_owned()];
        let mixed_unknown = vec!["UTF8_STRING".to_owned(), "application/x-opaque".to_owned()];
        assert_eq!(
            classify_file_selection(Some(&text), None, |_| false),
            (FileSelection::NonFile, None)
        );
        for (formats, contents) in [
            (None, None),
            (Some(files.as_slice()), None),
            (Some(files.as_slice()), Some(&b""[..])),
            (Some(files.as_slice()), Some(&b"not a file URI\n"[..])),
            (Some(files.as_slice()), Some(&b"file:///tmp/%00\n"[..])),
            (Some(files.as_slice()), Some(&b"\xff"[..])),
            (Some(other_files.as_slice()), None),
            (Some(unknown_format.as_slice()), None),
            (Some(mixed_unknown.as_slice()), None),
        ] {
            assert_eq!(
                classify_file_selection(formats, contents, |_| false),
                (FileSelection::Unknown, None)
            );
        }
        let oversized = vec![b'x'; 8 * 1024 * 1024 + 1];
        assert_eq!(
            classify_file_selection(Some(&files), Some(&oversized), |_| false),
            (FileSelection::Unknown, None)
        );
        let too_many = "file:///tmp/synthetic\n".repeat(MAX_CLIPBOARD_FILES + 1);
        assert_eq!(
            classify_file_selection(Some(&files), Some(too_many.as_bytes()), |_| false),
            (FileSelection::Unknown, None)
        );
    }

    #[test]
    fn gtk_text_targets_do_not_mask_files_or_unknown_formats() {
        let targets = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>()
        };
        for formats in [
            targets(&["UTF8_STRING", "GTK_TEXT_BUFFER_CONTENTS"]),
            targets(&[
                "GTK_TEXT_BUFFER_CONTENTS",
                "UTF8_STRING",
                "text/plain;charset=utf-8",
            ]),
            targets(&[
                "UTF8_STRING",
                "text/_moz_htmlcontext",
                "text/_moz_htmlinfo",
                "application/x-moz-nativehtml",
            ]),
        ] {
            assert_eq!(
                classify_file_selection(Some(&formats), None, |_| false),
                (FileSelection::NonFile, None)
            );
        }
        for formats in [
            targets(&["GTK_TEXT_BUFFER_CONTENTS"]),
            targets(&["application/x-moz-nativehtml"]),
            targets(&[
                "UTF8_STRING",
                "text/_moz_htmlcontext",
                "application/x-moz-file",
            ]),
            targets(&[
                "UTF8_STRING",
                "GTK_TEXT_BUFFER_CONTENTS",
                "application/x-opaque",
            ]),
            targets(&[
                "UTF8_STRING",
                "GTK_TEXT_BUFFER_CONTENTS",
                "x-special/gnome-copied-files",
            ]),
        ] {
            assert_eq!(
                classify_file_selection(Some(&formats), None, |_| false),
                (FileSelection::Unknown, None)
            );
        }
        let formats = targets(&["UTF8_STRING", "GTK_TEXT_BUFFER_CONTENTS", "text/uri-list"]);
        assert_eq!(
            classify_file_selection(Some(&formats), Some(b"file:///tmp/synthetic\n"), |_| false),
            (
                FileSelection::Local,
                Some(vec![PathBuf::from("/tmp/synthetic")])
            )
        );
        assert_eq!(
            classify_file_selection(Some(&formats), Some(b"file:///tmp/synthetic\n"), |_| true),
            (FileSelection::RemoteOwned, None)
        );
        assert_eq!(
            classify_file_selection(Some(&formats), None, |_| false),
            (FileSelection::Unknown, None)
        );
        let unknown_uri = targets(&[
            "UTF8_STRING",
            "GTK_TEXT_BUFFER_CONTENTS",
            "text/uri-list",
            "application/x-opaque",
        ]);
        assert_eq!(
            classify_file_selection(Some(&unknown_uri), Some(b"file:///tmp/synthetic\n"), |_| {
                true
            }),
            (FileSelection::Unknown, None)
        );
    }

    #[test]
    fn remote_virtual_files_are_not_mistaken_for_local_file_copies() {
        let formats = vec!["text/uri-list".to_owned()];
        let local = std::env::temp_dir().join("adit-synthetic-local");
        let remote = std::env::temp_dir().join("adit-synthetic-remote");
        let local_uri = url::Url::from_file_path(&local).unwrap().to_string();
        let remote_uri = url::Url::from_file_path(&remote).unwrap().to_string();
        let owns = |path: &Path| path == remote;

        assert_eq!(
            classify_file_selection(Some(&formats), Some(local_uri.as_bytes()), owns),
            (FileSelection::Local, Some(vec![local]))
        );
        assert_eq!(
            classify_file_selection(Some(&formats), Some(remote_uri.as_bytes()), owns),
            (FileSelection::RemoteOwned, None)
        );
        let with_companion = vec![
            "text/uri-list".to_owned(),
            "x-special/nautilus-clipboard".to_owned(),
        ];
        assert_eq!(
            classify_file_selection(Some(&with_companion), Some(remote_uri.as_bytes()), owns),
            (FileSelection::RemoteOwned, None)
        );
        let with_unknown = vec![
            "text/uri-list".to_owned(),
            "application/x-opaque".to_owned(),
        ];
        assert_eq!(
            classify_file_selection(Some(&with_unknown), Some(remote_uri.as_bytes()), owns),
            (FileSelection::Unknown, None)
        );
        let mixed = format!("{local_uri}\n{remote_uri}");
        assert_eq!(
            classify_file_selection(Some(&formats), Some(mixed.as_bytes()), owns),
            (FileSelection::Unknown, None)
        );
    }

    #[test]
    #[ignore = "requires an isolated X11 display, /dev/fuse and fusermount3"]
    fn native_worker_publishes_without_echo_and_stops_with_its_mount() {
        let expected = std::env::var("ADIT_CLIPBOARD_FIXTURE_DISPLAY")
            .expect("explicit private-display fixture required");
        assert_eq!(std::env::var("DISPLAY").unwrap(), expected);
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let (wake, events) = sync_channel(1);
        let worker = thread::spawn(move || run(worker_shared, events));
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.sequence.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "worker did not initialize its private clipboard"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let initial_sequence = shared.sequence.load(Ordering::Acquire);
        let bridge = ChunkBridge::new();
        let reply = bridge.clone();
        shared.generation.store(1, Ordering::Release);
        *shared.latest.lock().unwrap() = Some(Offer {
            generation: 1,
            files: vec![ClipFile {
                name: "worker-fixture.bin".into(),
                size: 3,
                is_dir: false,
            }],
            bridge,
            request: Arc::new(move |id, _, offset, length| {
                let bytes = [1, 0, 255];
                let from = offset as usize;
                let end = (from + length as usize).min(bytes.len());
                reply.deliver(id, Some(bytes[from..end].to_vec()));
            }),
            current: Arc::new(|| true),
            chunk: 4096,
        });
        wake.send(()).unwrap();
        let clipboard = ClipboardContext::new().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let uri = loop {
            if let Ok(files) = clipboard.get_files() {
                if let Some(uri) = files
                    .into_iter()
                    .find(|uri| uri.ends_with("worker-fixture.bin"))
                {
                    break uri;
                }
            }
            assert!(
                Instant::now() < deadline,
                "worker did not publish its private URI"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let path = url::Url::parse(&uri).unwrap().to_file_path().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [1, 0, 255]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.sequence.load(Ordering::Acquire) == initial_sequence {
            assert!(
                Instant::now() < deadline,
                "clipboard ownership change was not observed"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            shared.paths.lock().unwrap().is_none(),
            "an owned remote URI must not echo back"
        );
        shared.stop.store(true, Ordering::Release);
        let _ = wake.try_send(());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() {
            assert!(
                Instant::now() < deadline,
                "worker/mount cleanup did not finish"
            );
            thread::sleep(Duration::from_millis(10));
        }
        worker.join().unwrap().unwrap();
        assert!(shared.error.lock().unwrap().is_none());
        assert!(!path.exists(), "stopped worker left a mounted file behind");
    }
}
