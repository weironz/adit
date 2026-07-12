//! Client side of the out-of-process RDP helper.
//!
//! Native RDP can't be linked into the main binary (IronRDP's crypto deps clash
//! with russh's — see the RDP dependency note), so it runs as the `adit-rdp-host`
//! child process. This module spawns that helper, drives it over stdin/stdout
//! using [`adit_rdp_proto`], keeps the decoded framebuffer, and presents a small
//! handle to the session layer that mirrors [`adit_ssh::LiveShellHandle`].

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg, InputEvent};

/// Environment override for the helper path (mainly for development/tests).
const HELPER_ENV: &str = "ADIT_RDP_HOST";
/// Helper executable file name.
const HELPER_EXE: &str = "adit-rdp-host.exe";

/// A discrete event from the RDP session (framebuffer updates are applied to the
/// surface directly and don't appear here).
#[derive(Debug, Clone)]
pub enum RdpClientEvent {
    Connected { width: u16, height: u16 },
    Resized { width: u16, height: u16 },
    ClipboardText(String),
    Error(String),
    Closed,
}

/// The current decoded desktop image plus a monotonic generation the UI uses to
/// avoid re-uploading an unchanged frame.
struct Surface {
    rgba: Vec<u8>,
    width: u16,
    height: u16,
    generation: u64,
}

/// Ceiling per side for a framebuffer allocation. Mirrors the request-side clamp
/// in the helper (`adit-rdp` clamps the desktop to 8192). Without this, a server
/// that reports a huge desktop size in `Connected`/`Resized` would make us try to
/// allocate gigabytes — and a zeroed-`Vec` allocation failure `abort()`s the whole
/// process, taking every other tab down with it. Oversized tiles are then simply
/// dropped by `blit`'s bounds check.
const MAX_RDP_DIMENSION: u16 = 8192;

impl Surface {
    fn new(width: u16, height: u16) -> Self {
        let width = width.clamp(1, MAX_RDP_DIMENSION);
        let height = height.clamp(1, MAX_RDP_DIMENSION);
        Self {
            rgba: vec![0u8; usize::from(width) * usize::from(height) * 4],
            width,
            height,
            generation: 0,
        }
    }

    /// Blit a tile into the surface, ignoring anything out of bounds (defensive
    /// against a malformed message).
    fn blit(&mut self, x: u16, y: u16, tw: u16, th: u16, rgba: &[u8]) {
        let (sw, sh) = (usize::from(self.width), usize::from(self.height));
        let (x, y, tw, th) = (usize::from(x), usize::from(y), usize::from(tw), usize::from(th));
        if x + tw > sw || y + th > sh || rgba.len() < tw * th * 4 {
            return;
        }
        for row in 0..th {
            let dst = ((y + row) * sw + x) * 4;
            let src = row * tw * 4;
            self.rgba[dst..dst + tw * 4].copy_from_slice(&rgba[src..src + tw * 4]);
        }
        self.generation = self.generation.wrapping_add(1);
    }
}

/// A snapshot of the framebuffer for the renderer.
#[derive(Clone)]
pub struct RdpFrame {
    pub rgba: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub generation: u64,
}

/// Handle to a live RDP session running in the helper process.
pub struct RdpClientHandle {
    cmd_tx: std_mpsc::Sender<ClientMsg>,
    event_rx: std_mpsc::Receiver<RdpClientEvent>,
    surface: Arc<Mutex<Surface>>,
    child: Arc<Mutex<Child>>,
}

impl RdpClientHandle {
    /// Send an input event to the session.
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.cmd_tx.send(ClientMsg::Input(event));
    }

    /// Ask the session to disconnect gracefully.
    pub fn close(&self) {
        let _ = self.cmd_tx.send(ClientMsg::Close);
    }

    /// Poll for the next discrete session event.
    #[must_use]
    pub fn try_recv(&self) -> Option<RdpClientEvent> {
        self.event_rx.try_recv().ok()
    }

    /// The current framebuffer generation (cheap; no pixel copy).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.surface.lock().map(|s| s.generation).unwrap_or(0)
    }

    /// A copy of the framebuffer, but only if it changed since `last_generation`.
    #[must_use]
    pub fn frame_if_newer(&self, last_generation: u64) -> Option<RdpFrame> {
        let surface = self.surface.lock().ok()?;
        if surface.generation == last_generation {
            return None;
        }
        Some(RdpFrame {
            rgba: surface.rgba.clone(),
            width: surface.width,
            height: surface.height,
            generation: surface.generation,
        })
    }
}

impl Drop for RdpClientHandle {
    fn drop(&mut self) {
        // Ask for a graceful close, then let the helper actually run its RDP
        // shutdown sequence before we force-kill. A synchronous `kill()` here would
        // race ahead of the `Close` even reaching the helper's stdin, so we reap on
        // a background thread with a short grace window — and still guarantee no
        // leaked process if the helper hangs.
        let _ = self.cmd_tx.send(ClientMsg::Close);
        let child = Arc::clone(&self.child);
        let _ = thread::Builder::new()
            .name("adit-rdp-client-reap".into())
            .spawn(move || {
                for _ in 0..40 {
                    // Exited gracefully?
                    if let Ok(Ok(Some(_))) = child.lock().map(|mut c| c.try_wait()) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                if let Ok(mut c) = child.lock() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RdpClientError {
    #[error("the RDP helper ({0}) was not found; reinstall Adit")]
    HelperMissing(String),
    #[error("could not start the RDP helper: {0}")]
    Spawn(String),
    #[error("could not talk to the RDP helper: {0}")]
    Io(String),
}

/// Locate the `adit-rdp-host` executable: an explicit override, then next to the
/// current executable (the installed layout), then the dev build output.
fn locate_helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(HELPER_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(HELPER_EXE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Development fallbacks: the helper builds into its own workspace target dir.
    for rel in [
        "crates/adit-rdp/target/release/adit-rdp-host.exe",
        "crates/adit-rdp/target/debug/adit-rdp-host.exe",
    ] {
        let candidate = PathBuf::from(rel);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: keep the console helper from flashing a window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

/// Spawn the helper and open an RDP session. Returns immediately; connection
/// progress arrives as [`RdpClientEvent`]s.
pub fn spawn_rdp_session(request: ConnectRequest) -> Result<RdpClientHandle, RdpClientError> {
    let helper = locate_helper().ok_or_else(|| RdpClientError::HelperMissing(HELPER_EXE.into()))?;

    let mut command = Command::new(&helper);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Let the helper's stderr diagnostics flow to our own stderr.
        .stderr(Stdio::inherit());
    no_window(&mut command);

    let mut child = command
        .spawn()
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    // Any failure after spawn must kill the child: `Child`'s drop does NOT
    // terminate the process, so a half-wired helper would leak — possibly running
    // a real RDP session (with the password) that the user can't see or control.
    match wire_up(&mut child, request) {
        Ok((cmd_tx, event_rx, surface)) => Ok(RdpClientHandle {
            cmd_tx,
            event_rx,
            surface,
            child: Arc::new(Mutex::new(child)),
        }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

type WireUp = (
    std_mpsc::Sender<ClientMsg>,
    std_mpsc::Receiver<RdpClientEvent>,
    Arc<Mutex<Surface>>,
);

/// Wire the helper's pipes to channels + threads. On any error the caller kills
/// the child; on success the child's ownership moves into the returned handle.
fn wire_up(child: &mut Child, request: ConnectRequest) -> Result<WireUp, RdpClientError> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdin unavailable".into()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| RdpClientError::Spawn("helper stdout unavailable".into()))?;

    // Open the session before handing stdin to the writer thread.
    write_msg(&mut stdin, &ClientMsg::Connect(request))
        .map_err(|e| RdpClientError::Io(e.to_string()))?;

    let surface = Arc::new(Mutex::new(Surface::new(1, 1)));
    let (cmd_tx, cmd_rx) = std_mpsc::channel::<ClientMsg>();
    let (event_tx, event_rx) = std_mpsc::channel::<RdpClientEvent>();

    // stdin writer: serialise ClientMsgs to the helper.
    thread::Builder::new()
        .name("adit-rdp-client-stdin".into())
        .spawn(move || {
            while let Ok(msg) = cmd_rx.recv() {
                if write_msg(&mut stdin, &msg).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    // stdout reader: apply framebuffer tiles and forward discrete events.
    let reader_surface = Arc::clone(&surface);
    thread::Builder::new()
        .name("adit-rdp-client-stdout".into())
        .spawn(move || {
            loop {
                match read_msg::<_, HostMsg>(&mut stdout) {
                    Ok(Some(HostMsg::Connected { width, height })) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            *s = Surface::new(width, height);
                        }
                        let _ = event_tx.send(RdpClientEvent::Connected { width, height });
                    }
                    Ok(Some(HostMsg::Resized { width, height })) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            let generation = s.generation.wrapping_add(1);
                            *s = Surface::new(width, height);
                            s.generation = generation;
                        }
                        let _ = event_tx.send(RdpClientEvent::Resized { width, height });
                    }
                    Ok(Some(HostMsg::Tile {
                        x,
                        y,
                        width,
                        height,
                        rgba,
                    })) => {
                        if let Ok(mut s) = reader_surface.lock() {
                            s.blit(x, y, width, height, &rgba);
                        }
                    }
                    Ok(Some(HostMsg::ClipboardText(text))) => {
                        let _ = event_tx.send(RdpClientEvent::ClipboardText(text));
                    }
                    Ok(Some(HostMsg::Error(message))) => {
                        let _ = event_tx.send(RdpClientEvent::Error(message));
                    }
                    Ok(Some(HostMsg::Closed)) | Ok(None) => {
                        let _ = event_tx.send(RdpClientEvent::Closed);
                        break;
                    }
                    Err(_) => {
                        let _ = event_tx.send(RdpClientEvent::Closed);
                        break;
                    }
                }
            }
        })
        .map_err(|e| RdpClientError::Spawn(e.to_string()))?;

    Ok((cmd_tx, event_rx, surface))
}
