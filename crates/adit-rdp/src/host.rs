//! IPC host: bridges the child process's stdin/stdout pipes to the RDP session.
//!
//! Two dedicated threads own the binary stdio so it never mixes with the async
//! RDP socket loop:
//! * the **stdin reader** decodes [`ClientMsg`]s — the first `Connect` starts the
//!   session, epoch-tagged input feeds the session, and `Close`/EOF ends it;
//! * the **stdout writer** serialises [`HostMsg`]s from the session.
//!
//! Anything the helper prints to stdout that isn't a framed [`HostMsg`] would
//! corrupt the stream, so logs must go to stderr only.

use std::io::{self, Write};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg};
use tokio::sync::mpsc as tokio_mpsc;

use crate::session::{run_session, InputRouting, SessionCmd};
use crate::RdpError;

fn read_app_messages<R: io::Read>(
    mut input: R,
    request_tx: std_mpsc::Sender<ConnectRequest>,
    input_tx: tokio_mpsc::UnboundedSender<SessionCmd>,
    display_tx: tokio_mpsc::UnboundedSender<SessionCmd>,
    routing: Arc<InputRouting>,
) {
    let mut request_tx = Some(request_tx);
    loop {
        match read_msg::<_, ClientMsg>(&mut input) {
            Ok(Some(ClientMsg::Connect(request))) => {
                if let Some(tx) = request_tx.take() {
                    let _ = tx.send(request);
                }
            }
            // Untagged input must not cross a redirection.
            Ok(Some(ClientMsg::Input(_))) => {}
            Ok(Some(ClientMsg::EpochInput { epoch, event })) => {
                let key = matches!(
                    &event,
                    adit_rdp_proto::InputEvent::Key { .. }
                        | adit_rdp_proto::InputEvent::Unicode { .. }
                );
                if let Some((priority, command)) = routing.route(epoch, event) {
                    let tx = if priority { &display_tx } else { &input_tx };
                    if tx.send(command).is_err() {
                        break;
                    }
                } else if key && input_tx.send(SessionCmd::RejectedKeyboard).is_err() {
                    break;
                }
            }
            Ok(Some(ClientMsg::ClipboardImage(_))) => {}
            Ok(Some(ClientMsg::ClipboardFiles(files))) => {
                if files.is_empty()
                    && input_tx
                        .send(SessionCmd::ClearLocalClipboard)
                        .is_err()
                {
                    break;
                }
            }
            Ok(Some(ClientMsg::EpochClipboardImage { epoch, image })) => {
                if routing.accepts_epoch(epoch)
                    && input_tx
                        .send(SessionCmd::ClipboardImage { epoch, image })
                        .is_err()
                {
                    break;
                }
            }
            Ok(Some(ClientMsg::EpochClipboardFiles { epoch, files })) => {
                if routing.accepts_epoch(epoch) && files.is_empty() {
                    if input_tx
                        .send(SessionCmd::ClearLocalClipboard)
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Ok(Some(ClientMsg::FileContents {
                epoch,
                stream_id,
                data,
            })) => {
                if let Some(command) = routing.file_contents(epoch, stream_id, data) {
                    if input_tx.send(command).is_err() {
                        break;
                    }
                }
            }
            Ok(Some(ClientMsg::RequestFileContents {
                generation,
                stream_id,
                index,
                offset,
                length,
            })) => {
                if input_tx
                    .send(SessionCmd::RequestFileContents {
                        generation,
                        stream_id,
                        index,
                        offset,
                        length,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(Some(ClientMsg::Close)) | Ok(None) | Err(_) => break,
        }
    }
}

/// Entry point for the `adit-rdp-host` binary. Blocks until the session ends.
pub fn run_host() -> Result<(), RdpError> {
    // Diagnostics go to a log file next to the app config (the GUI app discards
    // the helper's stderr), so an RDP session's lifecycle/errors are visible even
    // in a release build. Appended across launches (see below); `try_init` so a
    // double-init cannot panic.
    let log_path = {
        let base = std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("Adit").join("rdp-helper.log")
    };
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append, and roll only once the file has actually grown large.
    //
    // This used to truncate on every launch, which is wrong the moment there is
    // more than one RDP session: each opens its own helper, every helper writes
    // to this one path, and the second to start wiped the first one's log. That
    // cost a real diagnosis — a clipboard bug was reproduced and the evidence
    // was already gone, because opening a second session had reset the file.
    //
    // The pid goes on the banner so interleaved lines can be told apart, now
    // that one file holds every live helper.
    const LOG_ROLL_BYTES: u64 = 4 * 1024 * 1024;
    if std::fs::metadata(&log_path).is_ok_and(|meta| meta.len() > LOG_ROLL_BYTES) {
        let _ = std::fs::remove_file(&log_path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "=== adit-rdp-host {} start pid={} ===",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
        );
    }
    // Default: session lifecycle + warnings/errors only (no per-frame spam).
    // Override with RUST_LOG for deeper tracing.
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "warn,adit_rdp=info,ironrdp_async=info".to_owned());
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || -> Box<dyn std::io::Write + Send> {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(std::io::sink()),
            }
        })
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .try_init();

    let (req_tx, req_rx) = std_mpsc::channel::<ConnectRequest>();
    let (input_tx, input_rx) = tokio_mpsc::unbounded_channel();
    let (display_tx, display_rx) = tokio_mpsc::unbounded_channel();
    let routing = Arc::new(InputRouting::default());
    let reader_routing = Arc::clone(&routing);
    let (host_tx, host_rx) = std_mpsc::channel::<HostMsg>();

    // ── stdin reader ─────────────────────────────────────────────────────────
    // A single lock for the process lifetime: `StdinLock` is internally buffered,
    // so re-locking would drop bytes read past a message boundary.
    thread::Builder::new()
        .name("adit-rdp-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            read_app_messages(stdin.lock(), req_tx, input_tx, display_tx, reader_routing);
        })
        .map_err(|e| RdpError::Runtime(e.to_string()))?;

    // ── stdout writer ────────────────────────────────────────────────────────
    let writer = thread::Builder::new()
        .name("adit-rdp-stdout".into())
        .spawn(move || {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            while let Ok(msg) = host_rx.recv() {
                let is_final = matches!(msg, HostMsg::Closed);
                if write_msg(&mut lock, &msg).is_err() {
                    break;
                }
                let _ = lock.flush();
                if is_final {
                    break;
                }
            }
        })
        .map_err(|e| RdpError::Runtime(e.to_string()))?;

    // Wait for the app's Connect before spinning up the RDP runtime.
    let request = req_rx.recv().map_err(|_| RdpError::ControlChannelClosed)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| RdpError::Runtime(e.to_string()))?;

    let result = runtime.block_on(run_session(
        request,
        input_rx,
        display_rx,
        routing,
        host_tx.clone(),
    ));

    // Announce the outcome, then let the writer flush and exit.
    match &result {
        Ok(()) => {
            let _ = host_tx.send(HostMsg::Closed);
        }
        Err(e) => {
            let _ = host_tx.send(HostMsg::Error(e.to_string()));
            let _ = host_tx.send(HostMsg::Closed);
        }
    }
    drop(host_tx);
    let _ = writer.join();

    result
}

#[cfg(test)]
mod redirect_epoch_tests {
    use super::*;
    use adit_rdp_proto::{InputEvent, Monitor, MouseButton};
    use std::io::Read;
    use std::time::Duration;

    struct FragmentedInput {
        fragments: std_mpsc::Receiver<Vec<u8>>,
        loaded: std_mpsc::Sender<()>,
        current: io::Cursor<Vec<u8>>,
    }

    impl Read for FragmentedInput {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            loop {
                let n = self.current.read(out)?;
                if n > 0 || out.is_empty() {
                    return Ok(n);
                }
                match self.fragments.recv() {
                    Ok(fragment) => {
                        self.current = io::Cursor::new(fragment);
                        let _ = self.loaded.send(());
                    }
                    Err(_) => return Ok(0),
                }
            }
        }
    }

    fn frame(message: ClientMsg) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &message).unwrap();
        bytes
    }

    fn layout(request_id: u64) -> InputEvent {
        InputEvent::MonitorLayout {
            request_id,
            monitors: vec![Monitor {
                left: 0,
                top: 0,
                width: 800,
                height: 600,
                primary: true,
                scale_percent: 100,
            }],
        }
    }

    #[test]
    fn old_framed_inputs_arriving_after_redirect_never_reach_session_queues() {
        let routing = Arc::new(InputRouting::default());
        let (fragments_tx, fragments_rx) = std_mpsc::channel();
        let (loaded_tx, loaded_rx) = std_mpsc::channel();
        let (request_tx, _request_rx) = std_mpsc::channel();
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let (display_tx, mut display_rx) = tokio_mpsc::unbounded_channel();
        let reader = FragmentedInput {
            fragments: fragments_rx,
            loaded: loaded_tx,
            current: io::Cursor::new(Vec::new()),
        };
        let worker_routing = Arc::clone(&routing);
        let worker = thread::spawn(move || {
            read_app_messages(reader, request_tx, input_tx, display_tx, worker_routing)
        });
        let old = frame(ClientMsg::EpochInput {
            epoch: 0,
            event: InputEvent::Resize {
                width: 640,
                height: 480,
            },
        });
        fragments_tx.send(old[..4].to_vec()).unwrap();
        loaded_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(routing.advance(), Some(1));
        fragments_tx.send(old[4..].to_vec()).unwrap();
        for event in [
            InputEvent::MouseMove { x: 1, y: 2 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            },
            InputEvent::Wheel {
                vertical: true,
                delta: 120,
            },
            layout(1),
            InputEvent::CancelMonitorLayout { request_id: 1 },
            InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            },
        ] {
            fragments_tx
                .send(frame(ClientMsg::EpochInput { epoch: 0, event }))
                .unwrap();
        }
        fragments_tx
            .send(frame(ClientMsg::Input(InputEvent::Resize {
                width: 111,
                height: 222,
            })))
            .unwrap();
        fragments_tx
            .send(frame(ClientMsg::EpochInput {
                epoch: 2,
                event: layout(99),
            }))
            .unwrap();
        for event in [
            InputEvent::Resize {
                width: 900,
                height: 700,
            },
            layout(2),
            InputEvent::CancelMonitorLayout { request_id: 2 },
        ] {
            fragments_tx
                .send(frame(ClientMsg::EpochInput { epoch: 1, event }))
                .unwrap();
        }
        for event in [
            InputEvent::MouseMove { x: 12, y: 34 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            InputEvent::Wheel {
                vertical: true,
                delta: 120,
            },
            InputEvent::Key {
                scancode: 30,
                extended: false,
                pressed: true,
            },
        ] {
            fragments_tx
                .send(frame(ClientMsg::EpochInput { epoch: 1, event }))
                .unwrap();
        }
        // Old frames can arrive even after the new epoch has started flowing.
        fragments_tx
            .send(frame(ClientMsg::EpochInput {
                epoch: 0,
                event: layout(3),
            }))
            .unwrap();
        fragments_tx
            .send(frame(ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: false,
                },
            }))
            .unwrap();
        // The same stream id may be reused by the new CLIPRDR channel.
        fragments_tx
            .send(frame(ClientMsg::FileContents {
                epoch: 0,
                stream_id: 7,
                data: Some(vec![1]),
            }))
            .unwrap();
        fragments_tx
            .send(frame(ClientMsg::FileContents {
                epoch: 1,
                stream_id: 7,
                data: Some(vec![2]),
            }))
            .unwrap();
        fragments_tx.send(frame(ClientMsg::Close)).unwrap();
        worker.join().unwrap();
        assert!(matches!(
            display_rx.try_recv(),
            Ok(SessionCmd::Resize {
                epoch: 1,
                revision: 1,
                width: 900,
                height: 700
            })
        ));
        assert!(matches!(
            display_rx.try_recv(),
            Ok(SessionCmd::Input {
                epoch: 1,
                event: InputEvent::MonitorLayout { request_id: 2, .. }
            })
        ));
        assert!(matches!(
            display_rx.try_recv(),
            Ok(SessionCmd::Input {
                epoch: 1,
                event: InputEvent::CancelMonitorLayout { request_id: 2 }
            })
        ));
        assert!(display_rx.try_recv().is_err());
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionCmd::RejectedKeyboard)
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionCmd::Pointer {
                epoch: 1,
                event: InputEvent::MouseMove { x: 12, y: 34 },
                ..
            })
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionCmd::Pointer {
                epoch: 1,
                event: InputEvent::MouseButton { pressed: true, .. },
                ..
            })
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionCmd::Pointer {
                epoch: 1,
                event: InputEvent::Wheel { delta: 120, .. },
                ..
            })
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionCmd::Input {
                epoch: 1,
                event: InputEvent::Key { scancode: 30, .. }
            })
        ));
        assert!(matches!(input_rx.try_recv(), Ok(SessionCmd::FileContents {
            epoch: 1, stream_id: 7, data: Some(bytes)
        }) if bytes == vec![2]));
        assert!(input_rx.try_recv().is_err());
    }
}
