//! Clipboard ownership and FUSE lifetime run off the iced thread. X11 selections
//! expose URI metadata; file reads are serviced by the captured remote offer.
use super::linux_fuse::RemoteMount;
use super::{ChunkBridge, ClipFile, MAX_CLIPBOARD_FILES};
use clipboard_rs::{
    Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

struct Offer {
    generation: u64,
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    request: super::remote_tree::Requester,
    chunk: u32,
}

#[derive(Default)]
struct Shared {
    latest: Mutex<Option<Offer>>,
    generation: AtomicU64,
    sequence: AtomicU32,
    dirty: AtomicBool,
    stop: AtomicBool,
    failed: AtomicBool,
    paths: Mutex<Option<Vec<PathBuf>>>,
    error: Mutex<Option<String>>,
}

struct Service {
    shared: Arc<Shared>,
    wake: SyncSender<()>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

struct Changes(Arc<Shared>);
impl ClipboardHandler for Changes {
    fn on_clipboard_change(&mut self) {
        self.0.dirty.store(true, Ordering::Release);
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
                    if let Err(error) = run(worker_shared.clone(), events) {
                        worker_shared.failed.store(true, Ordering::Release);
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
    let watcher = thread::Builder::new()
        .name("adit-clipboard-watch".into())
        .spawn(move || watcher.start_watch())
        .map_err(|_| "无法启动文件剪贴板监视线程")?;
    let mut mount: Option<RemoteMount> = None;
    shared.dirty.store(true, Ordering::Release);
    while !shared.stop.load(Ordering::Acquire) {
        let _ = events.recv_timeout(Duration::from_millis(100));
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
                shared.sequence.fetch_add(1, Ordering::Release);
                continue;
            }
            let generation = offer.generation;
            let result = RemoteMount::new(
                offer.files,
                offer.bridge.clone(),
                offer.request,
                offer.chunk,
            );
            if generation == shared.generation.load(Ordering::Acquire)
                && !shared.stop.load(Ordering::Acquire)
            {
                match result {
                    Ok(next) => {
                        if clipboard.set_files(next.paths.clone()).is_ok() {
                            mount = Some(next);
                            shared.dirty.store(true, Ordering::Release);
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
        if shared.dirty.swap(false, Ordering::AcqRel) {
            // Never fetch arbitrary text/image formats, and never reflect our
            // own virtual paths back as a local-to-remote file offer.
            let paths = clipboard
                .get_files()
                .ok()
                .and_then(|values| local_paths(values, mount.as_ref()));
            *shared.paths.lock().unwrap_or_else(|e| e.into_inner()) = paths;
            if shared
                .sequence
                .fetch_add(1, Ordering::Release)
                .wrapping_add(1)
                == 0
            {
                shared.sequence.fetch_add(1, Ordering::Release);
            }
        }
    }
    drop(mount);
    stop_watcher.stop();
    let _ = watcher.join();
    Ok(())
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
    if shared.failed.load(Ordering::Acquire) {
        bridge.close();
        *shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
            Some("文件剪贴板工作线程不可用；远程连接不受影响".into());
        return;
    }
    let generation = shared
        .generation
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let previous = shared
        .latest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(Offer {
            generation,
            files,
            bridge,
            request: Arc::new(move |stream, index, offset, length| {
                requester.request(stream, index, offset, length)
            }),
            chunk,
        });
    if let Some(previous) = previous {
        previous.bridge.close();
    }
    let _ = service.wake.try_send(());
}

pub(super) fn sequence() -> u32 {
    service().map_or(0, |service| service.shared.sequence.load(Ordering::Acquire))
}

pub(super) fn paths() -> Option<Vec<PathBuf>> {
    service()?
        .shared
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
            assert!(Instant::now() < deadline, "worker did not initialize its private clipboard");
            thread::sleep(Duration::from_millis(10));
        }
        let initial_sequence = shared.sequence.load(Ordering::Acquire);
        let bridge = ChunkBridge::new();
        let reply = bridge.clone();
        shared.generation.store(1, Ordering::Release);
        *shared.latest.lock().unwrap() = Some(Offer {
            generation: 1,
            files: vec![ClipFile { name: "worker-fixture.bin".into(), size: 3, is_dir: false }],
            bridge,
            request: Arc::new(move |id, _, offset, length| {
                let bytes = [1, 0, 255];
                let from = offset as usize;
                let end = (from + length as usize).min(bytes.len());
                reply.deliver(id, Some(bytes[from..end].to_vec()));
            }),
            chunk: 4096,
        });
        wake.send(()).unwrap();
        let clipboard = ClipboardContext::new().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let uri = loop {
            if let Ok(files) = clipboard.get_files() {
                if let Some(uri) = files.into_iter().find(|uri| uri.ends_with("worker-fixture.bin")) {
                    break uri;
                }
            }
            assert!(Instant::now() < deadline, "worker did not publish its private URI");
            thread::sleep(Duration::from_millis(10));
        };
        let path = url::Url::parse(&uri).unwrap().to_file_path().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [1, 0, 255]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.sequence.load(Ordering::Acquire) == initial_sequence {
            assert!(Instant::now() < deadline, "clipboard ownership change was not observed");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(shared.paths.lock().unwrap().is_none(), "an owned remote URI must not echo back");
        shared.stop.store(true, Ordering::Release);
        let _ = wake.try_send(());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "worker/mount cleanup did not finish");
            thread::sleep(Duration::from_millis(10));
        }
        worker.join().unwrap().unwrap();
        assert!(shared.error.lock().unwrap().is_none());
        assert!(!path.exists(), "stopped worker left a mounted file behind");
    }
}
