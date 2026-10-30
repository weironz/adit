//! IPC host: bridges the child process's stdin/stdout pipes to the RDP session.
//!
//! Two dedicated threads own the binary stdio so it never mixes with the async
//! RDP socket loop:
//! * the **stdin reader** decodes [`ClientMsg`]s — the first `Connect` starts the
//!   session, subsequent `Input`s feed the session, and `Close`/EOF ends it;
//! * the **stdout writer** serialises [`HostMsg`]s from the session.
//!
//! Anything the helper prints to stdout that isn't a framed [`HostMsg`] would
//! corrupt the stream, so logs must go to stderr only.

use std::io::{self, Write};
use std::sync::mpsc as std_mpsc;
use std::thread;

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg};
use tokio::sync::mpsc as tokio_mpsc;

use crate::session::{run_session, SessionCmd};
use crate::RdpError;

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
    let (host_tx, host_rx) = std_mpsc::channel::<HostMsg>();

    // ── stdin reader ─────────────────────────────────────────────────────────
    // A single lock for the process lifetime: `StdinLock` is internally buffered,
    // so re-locking would drop bytes read past a message boundary.
    thread::Builder::new()
        .name("adit-rdp-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            let mut req_tx = Some(req_tx);
            loop {
                match read_msg::<_, ClientMsg>(&mut lock) {
                    Ok(Some(ClientMsg::Connect(request))) => {
                        // Only the first Connect matters; ignore any extras.
                        if let Some(tx) = req_tx.take() {
                            let _ = tx.send(request);
                        }
                    }
                    // Close or a clean EOF ends the session. Legacy file
                    // offers only clear cached text/image; their metadata is dropped.
                    Ok(Some(ClientMsg::Close)) | Ok(None) => break,
                    Ok(Some(message)) => {
                        if let Some(command) = session_command(message) {
                            if input_tx.send(command).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
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

    let result = runtime.block_on(run_session(request, input_rx, host_tx.clone()));

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

/// Only commands safe for this helper reach the RDP session. An older app
/// cannot inject file metadata or pre-read bytes.
fn session_command(message: ClientMsg) -> Option<SessionCmd> {
    match message {
        ClientMsg::Input(event) => Some(SessionCmd::Input(event)),
        ClientMsg::ClipboardImage(image) => Some(SessionCmd::ClipboardImage(image)),
        ClientMsg::RequestFileContents {
            generation,
            stream_id,
            index,
            offset,
            length,
        } => Some(SessionCmd::RequestFileContents {
            generation,
            stream_id,
            index,
            offset,
            length,
        }),
        // Old apps may still send nonempty file selections. Discard their
        // metadata but also retire any older local text/image offer.
        ClientMsg::ClipboardFiles(_) => Some(SessionCmd::ClearLocalClipboard),
        ClientMsg::FileContents { .. } | ClientMsg::Connect(_) | ClientMsg::Close => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_rdp_proto::{ClipFile, InputEvent};

    #[test]
    fn old_file_messages_are_dropped_at_the_helper_ipc_boundary() {
        let messages = [
            ClientMsg::ClipboardFiles(vec![ClipFile {
                name: "private.txt".into(),
                size: 7,
                is_dir: false,
            }]),
            ClientMsg::FileContents {
                stream_id: 11,
                data: Some(b"private".to_vec()),
            },
            ClientMsg::FileContents {
                stream_id: 11,
                data: None,
            },
        ];
        let mut bytes = Vec::new();
        for msg in &messages {
            write_msg(&mut bytes, msg).unwrap();
        }
        let mut input = bytes.as_slice();
        let file_offer = read_msg::<_, ClientMsg>(&mut input).unwrap().unwrap();
        assert!(matches!(
            session_command(file_offer),
            Some(SessionCmd::ClearLocalClipboard)
        ));
        for _ in 1..messages.len() {
            let decoded = read_msg::<_, ClientMsg>(&mut input).unwrap().unwrap();
            assert!(session_command(decoded).is_none());
        }
        assert!(matches!(
            session_command(ClientMsg::ClipboardFiles(Vec::new())),
            Some(SessionCmd::ClearLocalClipboard)
        ));
        assert!(matches!(
            session_command(ClientMsg::Input(InputEvent::ClipboardText("text".into()))),
            Some(SessionCmd::Input(InputEvent::ClipboardText(text))) if text == "text"
        ));
        assert!(matches!(
            session_command(ClientMsg::ClipboardImage(vec![1, 2])),
            Some(SessionCmd::ClipboardImage(image)) if image == [1, 2]
        ));
        assert!(matches!(
            session_command(ClientMsg::RequestFileContents {
                generation: 1,
                stream_id: 2,
                index: 0,
                offset: 0,
                length: 1
            }),
            Some(SessionCmd::RequestFileContents { generation: 1, .. })
        ));
    }
}
