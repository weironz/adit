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

use crate::session::run_session;
use crate::RdpError;

/// Entry point for the `adit-rdp-host` binary. Blocks until the session ends.
pub fn run_host() -> Result<(), RdpError> {
    // Diagnostics to stderr only (stdout is the framed protocol). No-op unless
    // RUST_LOG is set. `try_init` so a double-init (e.g. tests) can't panic.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
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
                    Ok(Some(ClientMsg::Input(event))) => {
                        if input_tx.send(event).is_err() {
                            break; // session ended
                        }
                    }
                    // Close or a clean EOF ⇒ drop `input_tx`, which the session
                    // observes as a graceful-shutdown request.
                    Ok(Some(ClientMsg::Close)) | Ok(None) => break,
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
    let request = req_rx
        .recv()
        .map_err(|_| RdpError::ControlChannelClosed)?;

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
