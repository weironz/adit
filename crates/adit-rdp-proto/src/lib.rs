//! Wire protocol shared between the main Adit app and the out-of-process RDP
//! helper (`adit-rdp-host`).
//!
//! RDP can't be linked into the main binary (IronRDP's `picky` exact-pins
//! pre-release RustCrypto that conflicts with russh — see the RDP dependency
//! note), so it runs as a child process. The app writes [`ClientMsg`]s to the
//! child's stdin and reads [`HostMsg`]s from its stdout, each length-prefixed
//! (4-byte little-endian length + bincode payload). The child's stderr carries
//! logs and never the protocol.
//!
//! This crate has no heavy dependencies on purpose: it is compiled independently
//! by both workspaces, so it must never pull anything that could reintroduce the
//! version conflict.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Hard cap on a single framed message. It must cover the largest full-frame
/// `Tile` the helper can produce: the desktop is clamped to 8192 per side, so a
/// full RGBA frame is 8192·8192·4 = 256 MiB. 288 MiB leaves room for the bincode
/// framing overhead while still bounding allocation on a corrupt stream. Both
/// `write_msg` and `read_msg` enforce it, so an oversized frame fails loudly at
/// the writer instead of desyncing the reader.
pub const MAX_MESSAGE_BYTES: usize = 288 * 1024 * 1024;

/// Everything needed to open an RDP session. The password rides the stdin pipe,
/// never argv/env, so it isn't visible in the process list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    pub width: u16,
    pub height: u16,
    pub enable_clipboard: bool,
    pub enable_audio: bool,
}

/// A mouse button, protocol-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// A single input event from the app to the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute pointer position, in surface pixels.
    MouseMove { x: u16, y: u16 },
    MouseButton { button: MouseButton, pressed: bool },
    /// Wheel scroll; `delta` is in wheel units (±120 per notch), + is up/right.
    Wheel { vertical: bool, delta: i16 },
    /// A physical key by RDP scancode; `extended` marks the E0 set.
    Key {
        scancode: u8,
        extended: bool,
        pressed: bool,
    },
    /// A character via the Unicode input path (IME, unmapped layouts).
    Unicode { ch: char, pressed: bool },
    /// Resize the remote desktop.
    Resize { width: u16, height: u16 },
    /// Offer freshly-copied local text to the remote clipboard.
    ClipboardText(String),
}

/// App → helper (over the helper's stdin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Must be the first message; opens the session.
    Connect(ConnectRequest),
    Input(InputEvent),
    /// Ask for a graceful disconnect. Dropping stdin has the same effect.
    Close,
}

/// Helper → app (over the helper's stdout).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    /// Handshake finished; the negotiated desktop size is authoritative.
    Connected { width: u16, height: u16 },
    /// A rectangular framebuffer update. `rgba` is `width * height * 4` bytes,
    /// `R,G,B,A` order, row-major, to be blitted at (`x`, `y`) into the surface.
    Tile {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        rgba: Vec<u8>,
    },
    /// The desktop was resized; the app should reallocate its surface.
    Resized { width: u16, height: u16 },
    /// Server → client clipboard text.
    ClipboardText(String),
    /// A fatal error; [`HostMsg::Closed`] follows.
    Error(String),
    /// The session ended.
    Closed,
}

/// Write a length-prefixed, bincode-encoded message and flush. Fails if the
/// encoded message exceeds [`MAX_MESSAGE_BYTES`], so an oversized frame surfaces
/// as a writer error rather than a message the peer can't read (which would
/// desync the stream).
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let bytes = bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "framed message exceeds maximum size",
        ));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "message too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one length-prefixed, bincode-encoded message. Returns `Ok(None)` at a
/// clean end of stream (the peer closed the pipe between messages).
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "framed message exceeds maximum size",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let msg =
        bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}
