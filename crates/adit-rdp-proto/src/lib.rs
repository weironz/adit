//! Wire protocol shared between the main Adit app and the out-of-process RDP
//! helper (`adit-rdp-host`).
//!
//! RDP can't be linked into the main binary (IronRDP's `picky` exact-pins
//! pre-release RustCrypto that conflicts with russh — see the RDP dependency
//! note), so it runs as a child process. The app writes [`ClientMsg`]s to the
//! child's stdin and reads [`HostMsg`]s from its stdout, each length-prefixed
//! (4-byte little-endian length + bincode payload). The child's stderr carries
//! logs and never the protocol. A credential-free hello and acknowledgement
//! agree on [`IPC_PROTOCOL_VERSION`] before normal messages cross the pipe.
//!
//! This crate has no heavy dependencies on purpose: it is compiled independently
//! by both workspaces, so it must never pull anything that could reintroduce the
//! version conflict.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod clipboard_policy;
pub use clipboard_policy::SharedClipboardPolicy;

/// Hard cap on a single framed message. It must cover the largest full-frame
/// `Tile` the helper can produce: the desktop is clamped to 8192 per side, so a
/// full RGBA frame is 8192·8192·4 = 256 MiB. 288 MiB leaves room for the bincode
/// framing overhead while still bounding allocation on a corrupt stream. Both
/// `write_msg` and `read_msg` enforce it, so an oversized frame fails loudly at
/// the writer instead of desyncing the reader.
pub const MAX_MESSAGE_BYTES: usize = 288 * 1024 * 1024;

/// Straight-alpha RGBA from IronRDP's accelerated pointer decoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerUpdate {
    Default,
    Hidden,
    Position {
        x: u16,
        y: u16,
    },
    Bitmap {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: Vec<u8>,
    },
}

/// Coordinates are relative to the primary monitor at (0, 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
    pub scale_percent: u32,
}

/// Shared validation keeps helper allocations and GUI input coordinates bounded.
pub fn monitor_bounds(monitors: &[Monitor]) -> Result<(i32, i32, u16, u16), &'static str> {
    if monitors.is_empty()
        || monitors.len() > 16
        || monitors.iter().filter(|m| m.primary).count() != 1
    {
        return Err("a layout needs 1..16 monitors and exactly one primary");
    }
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for (i, m) in monitors.iter().enumerate() {
        if !(200..=8192).contains(&m.width)
            || m.width & 1 != 0
            || !(200..=8192).contains(&m.height)
            || !(100..=500).contains(&m.scale_percent)
            || (m.primary && (m.left != 0 || m.top != 0))
            || !(-8192..=8192).contains(&m.left)
            || !(-8192..=8192).contains(&m.top)
        {
            return Err("invalid monitor dimensions, origin or scale");
        }
        let r = m.left + i32::from(m.width);
        let b = m.top + i32::from(m.height);
        for other in &monitors[..i] {
            if m.left < other.left + i32::from(other.width)
                && other.left < r
                && m.top < other.top + i32::from(other.height)
                && other.top < b
            {
                return Err("monitors must not overlap");
            }
        }
        left = left.min(m.left);
        top = top.min(m.top);
        right = right.max(r);
        bottom = bottom.max(b);
    }
    if right - left > 8192 || bottom - top > 8192 {
        return Err("combined desktop exceeds 8192 pixels per side");
    }
    let mut connected = vec![false; monitors.len()];
    connected[monitors
        .iter()
        .position(|m| m.primary)
        .expect("validated primary")] = true;
    loop {
        let mut changed = false;
        for (i, a) in monitors.iter().enumerate() {
            if connected[i] {
                continue;
            }
            if monitors.iter().enumerate().any(|(j, b)| {
                connected[j]
                    && (((a.left + i32::from(a.width) == b.left
                        || b.left + i32::from(b.width) == a.left)
                        && a.top < b.top + i32::from(b.height)
                        && b.top < a.top + i32::from(a.height))
                        || ((a.top + i32::from(a.height) == b.top
                            || b.top + i32::from(b.height) == a.top)
                            && a.left < b.left + i32::from(b.width)
                            && b.left < a.left + i32::from(a.width)))
            }) {
                connected[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if connected.contains(&false) {
        return Err("monitors must share connected edges");
    }
    Ok((left, top, (right - left) as u16, (bottom - top) as u16))
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayControlState {
    pub max_monitors: u32,
    pub max_area: u64,
    /// Last layout actually encoded onto the DisplayControl channel.
    pub sent_layout: Vec<Monitor>,
    /// Selection opens only after the corresponding desktop size arrives.
    pub applied: bool,
    pub error: Option<String>,
}

/// How much of the remote desktop's visual fidelity to ask for, traded against
/// bandwidth. This is mstsc's "Experience" tab, and like mstsc's it is a
/// **connect-time** choice: it becomes RDP performance flags in the Client Info
/// PDU, which is sent once during the handshake and never renegotiated. Changing
/// it on a live session therefore means reconnecting — there is no wire message
/// that could do it in place, which is why none is offered here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    /// Everything on, including desktop composition: for a LAN.
    High,
    /// The historical default, and mstsc's on a fast link — drop the two
    /// animations nobody misses, keep wallpaper, themes and font smoothing.
    #[default]
    Balanced,
    /// Everything the protocol can switch off: wallpaper, themes, cursor
    /// shadow and blink, font smoothing. For a slow or metered link.
    Speed,
}

/// Everything needed to open an RDP session. The password rides the stdin pipe,
/// never argv/env, so it isn't visible in the process list.
///
/// Adding a field changes the bincode layout. Bump [`IPC_PROTOCOL_VERSION`] for
/// every incompatible wire change; deserialisation alone cannot detect all of
/// them, because one revision can silently interpret another's fields as data.
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
    pub quality: Quality,
    /// Per-session shared policy mapping, sent only through the control pipe.
    pub clipboard_policy_id: Option<String>,
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
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Wheel scroll; `delta` is in wheel units (±120 per notch), + is up/right.
    Wheel {
        vertical: bool,
        delta: i16,
    },
    /// A physical key by RDP scancode; `extended` marks the E0 set.
    Key {
        scancode: u8,
        extended: bool,
        pressed: bool,
    },
    /// A character via the Unicode input path (IME, unmapped layouts).
    Unicode {
        ch: char,
        pressed: bool,
    },
    /// Resize the remote desktop.
    Resize {
        width: u16,
        height: u16,
    },
    /// Offer freshly-copied local text to the remote clipboard.
    ClipboardText(String),
    /// Send a complete MS-RDPEDISP monitor layout.
    MonitorLayout(Vec<Monitor>),
}

/// How many bytes one file-transfer chunk carries.
///
/// Deliberately small next to [`MAX_MESSAGE_BYTES`]. File chunks share the one
/// stdio pipe with framebuffer tiles, so a chunk size chosen for throughput
/// alone would park a multi-megabyte write in front of the next frame and stall
/// the picture. 64 KiB is the size a `FileContentsRequest` typically asks for
/// anyway, and it keeps a transfer interleaving politely with the desktop.
pub const FILE_CHUNK_BYTES: u32 = 64 * 1024;

/// One entry in a clipboard file list.
///
/// The list is **flat**, with directory structure carried in `name` as a
/// relative `\`-separated path — that is how MS-RDPECLIP's
/// `FileGroupDescriptorW` represents a copied tree, and matching it here means
/// no translation on the wire. Directories appear as their own zero-length
/// entries and must, or the receiver has nowhere to create them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipFile {
    /// Relative path from the copy root, e.g. `docs\notes.txt`. Never absolute:
    /// a path from the sender's filesystem would be meaningless on the receiver's
    /// and is the obvious way this becomes a directory-traversal bug.
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

mod file_safety;
pub use file_safety::{
    validate_clipboard_file_list, validate_clipboard_relative_name, MAX_CLIPBOARD_DESCRIPTORS,
};

/// Identity of a local-file read within a helper's CLIPRDR channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipboardFileRequestId {
    pub channel_epoch: u64,
    pub snapshot_revision: u64,
    pub generation: u64,
    pub stream_id: u32,
}

/// App-owned policy and local clipboard observation at which content was sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardStamp {
    pub policy_epoch: u64,
    pub content_revision: u64,
}

/// Identity of a remote clipboard copy within one helper session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteClipboardStamp {
    pub local: ClipboardStamp,
    pub channel_epoch: u64,
    pub copy_revision: u64,
}

/// App → helper (over the helper's stdin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Must be the first message after IPC negotiation; opens the session.
    Connect(ConnectRequest),
    Input(InputEvent),
    /// Offer a locally-copied image to the remote clipboard, as raw `CF_DIB`
    /// bytes (a BITMAPINFOHEADER, any palette, then the pixels).
    ///
    /// Separate from `ClipboardText` because a screenshot is neither text nor a
    /// file: nothing exists on disk, so the file path cannot carry it, and the
    /// bytes are not a string. It is a third clipboard format, and MS-RDPECLIP
    /// treats it as exactly that.
    ClipboardImage(Vec<u8>),
    /// Legacy local file offer. A live helper currently discards nonempty
    /// metadata; an empty list still revokes an earlier local file offer.
    ClipboardFiles {
        /// Identifies this local offer within one helper; zero revokes it.
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// Legacy unbound reply, retained for synthetic clients. A live helper
    /// discards it because it cannot identify the originating channel.
    FileContents {
        generation: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// Ask the remote for a byte range of a file it offered. Drives a local
    /// paste: Explorer pulls from our data object, which pulls through here.
    RequestFileContents {
        /// The exact remote offer, not the current clipboard at read time.
        generation: u64,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// Ask for a graceful disconnect. Dropping stdin has the same effect.
    Close,
    /// Reply to the exact local-file request issued by the helper.
    BoundFileContents {
        request: ClipboardFileRequestId,
        data: Option<Vec<u8>>,
    },
    /// Permanently isolate this helper's local-to-remote file direction.
    /// Other clipboard formats and the desktop remain available.
    SuspendLocalFileDirection,
    /// A local file selection displaced previous clipboard content, but no
    /// file names, sizes or bytes are sent to the helper.
    LocalFileOfferRejected,
    /// Tag subsequent helper file offers with the app's sharing policy epoch.
    ClipboardPolicyEpoch(u64),
    /// A sampled local value, bound before it entered the app's IPC queue.
    ClipboardTextForRevision {
        stamp: ClipboardStamp,
        text: String,
    },
    ClipboardImageForRevision {
        stamp: ClipboardStamp,
        image: Vec<u8>,
    },
}

/// Helper → app (over the helper's stdout).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    /// Handshake finished; the negotiated desktop size is authoritative.
    Connected {
        width: u16,
        height: u16,
    },
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
    Resized {
        width: u16,
        height: u16,
    },
    /// Server → client clipboard text.
    ClipboardText(String),
    /// The remote copied an image; the app should put these `CF_DIB` bytes on
    /// the Windows clipboard.
    ClipboardImage(Vec<u8>),
    /// The remote copied files. The app should offer them on the Windows
    /// clipboard; nothing crosses the wire until something over here pastes.
    /// An empty list revokes previous file offers without replacing text/image
    /// data on the OS clipboard. Generations are scoped to one helper session.
    ClipboardFiles {
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// The remote is pasting and wants a byte range of a file the app offered.
    /// Answer with [`ClientMsg::BoundFileContents`] carrying the entire identity.
    FileContentsNeeded {
        channel_epoch: u64,
        snapshot_revision: u64,
        /// The local offer the helper used to validate this request.
        generation: u64,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// Bytes from the remote, answering [`ClientMsg::RequestFileContents`].
    /// `None` means the remote refused or failed the read.
    FileContents {
        generation: u64,
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    /// A fatal error; [`HostMsg::Closed`] follows.
    Error(String),
    /// The session ended.
    Closed,
    Pointer(PointerUpdate),
    DisplayControl(DisplayControlState),
    /// Nonfatal local file-sharing suspension; the desktop stays connected.
    ClipboardFileDirectionUnavailable {
        channel_epoch: u64,
        message: String,
    },
    /// The helper has isolated local-to-remote files for its remaining lifetime.
    LocalFileDirectionSuspended {
        channel_epoch: u64,
    },
    /// The previous CLIPRDR channel has ended; its local file reads are stale.
    ClipboardChannelChanged {
        channel_epoch: u64,
    },
    /// This new channel is ready and negotiated local-to-remote file sharing.
    ClipboardLocalFileDirectionReady {
        channel_epoch: u64,
    },
    /// A remote file offer tagged before it entered the helper output queue.
    ClipboardFilesForPolicy {
        policy_epoch: u64,
        generation: u64,
        files: Vec<ClipFile>,
    },
    /// The helper observed this shared policy; it is not a server acceptance ack.
    ClipboardPolicyApplied {
        policy_epoch: u64,
        enabled: bool,
    },
    /// The CLIPRDR channel reported that it is ready.
    ClipboardChannelReady {
        channel_epoch: u64,
    },
    /// The helper could not bind the shared policy; clipboard stays disabled.
    ClipboardPolicyRejected,
    /// The remote has replaced its clipboard, even if it never answers a paste.
    ClipboardRemoteCopy(RemoteClipboardStamp),
    ClipboardTextForRevision {
        stamp: RemoteClipboardStamp,
        text: String,
    },
    ClipboardImageForRevision {
        stamp: RemoteClipboardStamp,
        image: Vec<u8>,
    },
    ClipboardNotice(String),
}

/// Version of the complete bincode message layout, not the application release.
/// Change this whenever either side's messages become wire-incompatible.
pub const IPC_PROTOCOL_VERSION: u32 = 9;

/// Marker shared by both halves of the fixed version negotiation.
pub const IPC_HELLO_MAGIC: [u8; 8] = *b"ADITRDP\0";

// The first payload word is deliberately outside the legacy ClientMsg enum.
// An old helper rejects it instead of treating a new field as an old message.
// This envelope must stay fixed across future protocol versions.
const HELLO_DISCRIMINANT: u32 = u32::MAX;
const HELLO_LENGTH: u32 = 4 + IPC_HELLO_MAGIC.len() as u32 + 4;

/// Send the credential-free first frame. Do not send `Connect` until the helper
/// has acknowledged the same version with [`read_helper_hello`].
pub fn write_client_hello<W: Write>(w: &mut W, version: u32) -> io::Result<()> {
    w.write_all(&HELLO_LENGTH.to_le_bytes())?;
    w.write_all(&HELLO_DISCRIMINANT.to_le_bytes())?;
    w.write_all(&IPC_HELLO_MAGIC)?;
    w.write_all(&version.to_le_bytes())?;
    w.flush()
}

/// Acknowledgements use the same fixed envelope, independently of `HostMsg`.
pub fn write_helper_hello<W: Write>(w: &mut W, version: u32) -> io::Result<()> {
    write_client_hello(w, version)
}

/// Read the first frame without deserialising a legacy `Connect`. Reject its
/// length or discriminator without parsing its body, which may hold a password.
/// A buffered reader can still prefetch bytes from the underlying pipe.
pub fn read_client_hello<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut word = [0; 4];
    r.read_exact(&mut word)?;
    if u32::from_le_bytes(word) != HELLO_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC hello length",
        ));
    }
    r.read_exact(&mut word)?;
    if u32::from_le_bytes(word) != HELLO_DISCRIMINANT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC hello marker",
        ));
    }
    let mut magic = [0; IPC_HELLO_MAGIC.len()];
    r.read_exact(&mut magic)?;
    if magic != IPC_HELLO_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC hello magic",
        ));
    }
    r.read_exact(&mut word)?;
    Ok(u32::from_le_bytes(word))
}

/// Accept only an explicit matching helper acknowledgement. An old helper
/// closes the pipe on the unknown hello marker; that is a version failure too.
pub fn read_helper_hello<R: Read>(r: &mut R) -> io::Result<()> {
    let version = read_client_hello(r).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("RDP IPC 协商失败：辅助进程版本不兼容或未确认版本（{error}）"),
        )
    })?;
    if version != IPC_PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("RDP IPC 协议版本不匹配：主程序 {IPC_PROTOCOL_VERSION}，辅助进程 {version}；请同时更新主程序和 RDP 辅助进程"),
        ));
    }
    Ok(())
}

/// Write a length-prefixed, bincode-encoded message and flush. Fails if the
/// encoded message exceeds [`MAX_MESSAGE_BYTES`], so an oversized frame surfaces
/// as a writer error rather than a message the peer can't read (which would
/// desync the stream).
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let bytes =
        bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // The variant indices and affected layouts are from the unversioned peer.
    #[allow(dead_code)]
    #[derive(Deserialize)]
    enum LegacyClientMsg {
        Connect(ConnectRequest),
        Input(InputEvent),
        ClipboardImage(Vec<u8>),
        ClipboardFiles(Vec<ClipFile>),
        FileContents {
            stream_id: u32,
            data: Option<Vec<u8>>,
        },
        RequestFileContents {
            generation: u64,
            stream_id: u32,
            index: u32,
            offset: u64,
            length: u32,
        },
        Close,
    }

    #[allow(dead_code)]
    #[derive(Deserialize)]
    enum LegacyHostMsg {
        Connected,
        Tile,
        Resized,
        ClipboardText,
        ClipboardImage,
        ClipboardFiles,
        FileContentsNeeded,
        FileContents,
        Error(String),
        Closed,
    }

    // Before image clipboard support, Error and Closed were one index earlier.
    #[allow(dead_code)]
    #[derive(Deserialize)]
    enum PreImageHostMsg {
        Connected,
        Tile,
        Resized,
        ClipboardText,
        ClipboardFiles,
        FileContentsNeeded,
        FileContents,
        Error(String),
        Closed,
    }

    #[allow(dead_code)]
    #[derive(Deserialize)]
    enum InitialHostMsg {
        Connected,
        Tile,
        Resized,
        ClipboardText,
        Error(String),
        Closed,
    }

    #[test]
    fn legacy_helper_rejects_hello_before_any_new_message_or_credential() {
        let mut sent = Vec::new();
        write_client_hello(&mut sent, IPC_PROTOCOL_VERSION).unwrap();
        assert!(read_msg::<_, LegacyClientMsg>(&mut io::Cursor::new(&sent)).is_err());
        assert!(!sent
            .windows(b"synthetic-password".len())
            .any(|w| w == b"synthetic-password"));

        // Demonstrate why relying on bincode errors is unsafe: this new file
        // message successfully decodes as the old empty offer, losing bytes.
        let mut file_message = Vec::new();
        write_msg(
            &mut file_message,
            &ClientMsg::ClipboardFiles {
                generation: 0,
                files: vec![],
            },
        )
        .unwrap();
        assert!(
            matches!(read_msg::<_, LegacyClientMsg>(&mut io::Cursor::new(file_message)).unwrap(), Some(LegacyClientMsg::ClipboardFiles(files)) if files.is_empty())
        );
    }

    #[test]
    fn legacy_connect_is_rejected_before_deserialising_its_body() {
        let mut sent = Vec::new();
        write_msg(
            &mut sent,
            &ClientMsg::Connect(ConnectRequest {
                host: "adit.invalid".into(),
                port: 3389,
                username: "synthetic-user".into(),
                password: "synthetic-password".into(),
                domain: None,
                width: 800,
                height: 600,
                enable_clipboard: true,
                enable_audio: false,
                quality: Quality::default(),
                clipboard_policy_id: None,
            }),
        )
        .unwrap();
        let mut input = io::Cursor::new(sent);
        assert!(read_client_hello(&mut input).is_err());
        assert_eq!(
            input.position(),
            4,
            "legacy payload must not be deserialised"
        );

        let mut error = Vec::new();
        write_msg(&mut error, &HostMsg::Error("RDP IPC 协议版本不兼容".into())).unwrap();
        assert!(
            matches!(read_msg::<_, LegacyHostMsg>(&mut io::Cursor::new(error)).unwrap(), Some(LegacyHostMsg::Error(message)) if message.contains("版本"))
        );
    }

    #[test]
    fn old_error_indices_cannot_be_negotiated_by_an_unversioned_app() {
        let mut error = Vec::new();
        write_msg(&mut error, &HostMsg::Error("RDP IPC 协议版本不兼容".into())).unwrap();
        // These historical apps cannot display the new error: the same index
        // was Closed or did not exist. Neither interprets it as clipboard data.
        assert!(matches!(
            read_msg::<_, PreImageHostMsg>(&mut io::Cursor::new(&error)).unwrap(),
            Some(PreImageHostMsg::Closed)
        ));
        assert!(read_msg::<_, InitialHostMsg>(&mut io::Cursor::new(error)).is_err());
    }

    #[test]
    fn hello_requires_matching_explicit_version() {
        let mut hello = Vec::new();
        write_client_hello(&mut hello, IPC_PROTOCOL_VERSION).unwrap();
        assert_eq!(
            read_client_hello(&mut io::Cursor::new(hello)).unwrap(),
            IPC_PROTOCOL_VERSION
        );

        let mut response = Vec::new();
        write_helper_hello(&mut response, IPC_PROTOCOL_VERSION).unwrap();
        assert!(read_helper_hello(&mut io::Cursor::new(response)).is_ok());

        let mut wrong = Vec::new();
        write_helper_hello(&mut wrong, IPC_PROTOCOL_VERSION + 1).unwrap();
        assert!(read_helper_hello(&mut io::Cursor::new(wrong))
            .unwrap_err()
            .to_string()
            .contains("版本不匹配"));
        let mut wrong_magic = Vec::new();
        write_helper_hello(&mut wrong_magic, IPC_PROTOCOL_VERSION).unwrap();
        wrong_magic[8] ^= 1;
        assert!(read_helper_hello(&mut io::Cursor::new(wrong_magic)).is_err());
        assert!(read_helper_hello(&mut io::Cursor::new(Vec::<u8>::new())).is_err());

        let mut malformed = Vec::new();
        malformed.extend_from_slice(&HELLO_LENGTH.to_le_bytes());
        malformed.extend_from_slice(&0u32.to_le_bytes());
        malformed.extend_from_slice(b"synthetic-password");
        let mut input = io::Cursor::new(malformed);
        assert!(read_client_hello(&mut input).is_err());
        assert_eq!(
            input.position(),
            8,
            "invalid marker must not consume the body"
        );
    }

    /// The clipboard split (app owns the Windows clipboard, helper owns CLIPRDR)
    /// puts arbitrary user text on this pipe in both directions, so the framing
    /// has to survive whatever ends up on someone's clipboard.
    #[test]
    fn clipboard_text_survives_the_pipe_in_both_directions() {
        let text = "行 1\r\nline 2\ttabbed\u{0}emoji 🦀";

        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &ClientMsg::Input(InputEvent::ClipboardText(text.to_owned())),
        )
        .expect("write app → helper");
        write_msg(&mut pipe, &HostMsg::ClipboardText(text.to_owned())).expect("write helper → app");

        let mut cursor = io::Cursor::new(pipe);
        let inbound: ClientMsg = read_msg(&mut cursor)
            .expect("read app → helper")
            .expect("a message");
        let outbound: HostMsg = read_msg(&mut cursor)
            .expect("read helper → app")
            .expect("a message");

        match inbound {
            ClientMsg::Input(InputEvent::ClipboardText(got)) => assert_eq!(got, text),
            other => panic!("unexpected message: {other:?}"),
        }
        match outbound {
            HostMsg::ClipboardText(got) => assert_eq!(got, text),
            other => panic!("unexpected message: {other:?}"),
        }
        // Nothing left over: the two messages framed exactly.
        assert!(read_msg::<_, HostMsg>(&mut cursor)
            .expect("clean end of stream")
            .is_none());
    }

    #[test]
    fn sharing_policy_epoch_survives_both_ipc_directions() {
        let mut pipe = Vec::new();
        write_msg(&mut pipe, &ClientMsg::ClipboardPolicyEpoch(23)).unwrap();
        write_msg(
            &mut pipe,
            &HostMsg::ClipboardFilesForPolicy {
                policy_epoch: 23,
                generation: 7,
                files: vec![ClipFile {
                    name: "fixture.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            },
        )
        .unwrap();
        let mut pipe = io::Cursor::new(pipe);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut pipe).unwrap(),
            Some(ClientMsg::ClipboardPolicyEpoch(23))
        ));
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut pipe).unwrap(), Some(HostMsg::ClipboardFilesForPolicy { policy_epoch: 23, generation: 7, files }) if files[0].name == "fixture.bin")
        );
    }

    /// A copied tree is a flat list of relative paths, directories included as
    /// their own entries — drop those and the receiver has nowhere to put the
    /// files under them.
    #[test]
    fn a_file_list_round_trips_with_its_directory_entries() {
        let files = vec![
            ClipFile {
                name: String::from("报告"),
                size: 0,
                is_dir: true,
            },
            ClipFile {
                name: String::from("报告\\第一章.docx"),
                size: 4096,
                is_dir: false,
            },
        ];

        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &HostMsg::ClipboardFiles {
                generation: 7,
                files: files.clone(),
            },
        )
        .expect("write");
        let msg: HostMsg = read_msg(&mut io::Cursor::new(pipe))
            .expect("read")
            .expect("a message");

        match msg {
            HostMsg::ClipboardFiles {
                generation,
                files: got,
            } => {
                assert_eq!(generation, 7);
                assert_eq!(got, files);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn file_epoch_requests_and_replies_preserve_the_full_generation() {
        let generation = u64::MAX - 1;
        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &ClientMsg::RequestFileContents {
                generation,
                stream_id: 42,
                index: 3,
                offset: 0x1_0000_0001,
                length: 123,
            },
        )
        .unwrap();
        write_msg(
            &mut pipe,
            &HostMsg::FileContents {
                generation,
                stream_id: 42,
                data: None,
            },
        )
        .unwrap();
        let mut cursor = io::Cursor::new(pipe);
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut cursor).unwrap().unwrap(),
            ClientMsg::RequestFileContents {
                generation: got, stream_id: 42, index: 3, offset: 0x1_0000_0001, length: 123,
            } if got == generation)
        );
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut cursor).unwrap().unwrap(),
            HostMsg::FileContents { generation: got, stream_id: 42, data: None } if got == generation)
        );
    }

    /// A failed read has to survive the pipe as a failure. Collapsing `None` into
    /// an empty chunk would hand the receiver a silently truncated file, which is
    /// the one outcome a file transfer must never produce.
    #[test]
    fn a_failed_chunk_stays_distinguishable_from_an_empty_one() {
        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &ClientMsg::FileContents {
                generation: 9,
                stream_id: 7,
                data: None,
            },
        )
        .expect("write failure");
        write_msg(
            &mut pipe,
            &ClientMsg::FileContents {
                generation: 10,
                stream_id: 8,
                data: Some(Vec::new()),
            },
        )
        .expect("write empty");

        let mut cursor = io::Cursor::new(pipe);
        let failure: ClientMsg = read_msg(&mut cursor).expect("read").expect("a message");
        let empty: ClientMsg = read_msg(&mut cursor).expect("read").expect("a message");

        assert!(matches!(
            failure,
            ClientMsg::FileContents {
                generation: 9,
                stream_id: 7,
                data: None
            }
        ));
        assert!(matches!(
            empty,
            ClientMsg::FileContents { generation: 10, stream_id: 8, data: Some(ref bytes) } if bytes.is_empty()
        ));
    }

    #[test]
    fn local_file_offer_and_read_keep_their_generation_across_ipc() {
        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &ClientMsg::ClipboardFiles {
                generation: 17,
                files: vec![ClipFile {
                    name: "synthetic.bin".into(),
                    size: 3,
                    is_dir: false,
                }],
            },
        )
        .unwrap();
        write_msg(
            &mut pipe,
            &HostMsg::FileContentsNeeded {
                channel_epoch: 3,
                snapshot_revision: 5,
                generation: 17,
                stream_id: 7,
                index: 0,
                offset: 1,
                length: 2,
            },
        )
        .unwrap();
        let mut cursor = io::Cursor::new(pipe);
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut cursor).unwrap(), Some(ClientMsg::ClipboardFiles { generation: 17, files }) if files.len() == 1)
        );
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut cursor).unwrap(),
            Some(HostMsg::FileContentsNeeded {
                channel_epoch: 3,
                snapshot_revision: 5,
                generation: 17,
                stream_id: 7,
                index: 0,
                offset: 1,
                length: 2
            })
        ));
    }

    #[test]
    fn bound_local_file_reply_preserves_the_complete_request_identity() {
        let request = ClipboardFileRequestId {
            channel_epoch: 12,
            snapshot_revision: 5,
            generation: 7,
            stream_id: 41,
        };
        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &ClientMsg::BoundFileContents {
                request,
                data: Some(vec![1, 2]),
            },
        )
        .unwrap();
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut io::Cursor::new(pipe)).unwrap(),
            Some(ClientMsg::BoundFileContents { request: got, data: Some(data) })
                if got == request && data == [1, 2]
        ));
    }

    #[test]
    fn previous_ipc_version_is_rejected_before_messages_are_exchanged() {
        for version in [5, 6, 7, 8] {
            let mut response = Vec::new();
            write_helper_hello(&mut response, version).unwrap();
            let error = read_helper_hello(&mut io::Cursor::new(response)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn fixed_v9_peer_hello_and_direction_control_layout_are_independent() {
        const HELLO_V9: [u8; 20] = [
            16, 0, 0, 0, 255, 255, 255, 255, b'A', b'D', b'I', b'T', b'R', b'D', b'P', 0, 9, 0, 0,
            0,
        ];
        let mut sent = Vec::new();
        write_client_hello(&mut sent, IPC_PROTOCOL_VERSION).unwrap();
        assert_eq!(sent, HELLO_V9);
        assert!(read_helper_hello(&mut io::Cursor::new(HELLO_V9)).is_ok());
        let mut old = HELLO_V9;
        old[16] = 8;
        assert!(read_helper_hello(&mut io::Cursor::new(old)).is_err());
        let mut notice = Vec::new();
        write_msg(
            &mut notice,
            &HostMsg::ClipboardFileDirectionUnavailable {
                channel_epoch: 7,
                message: "notice".into(),
            },
        )
        .unwrap();
        // Appended variant index 12 and bincode's little-endian field layout.
        assert_eq!(&notice[4..8], &12_u32.to_le_bytes());
        assert_eq!(&notice[8..16], &7_u64.to_le_bytes());
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut io::Cursor::new(notice)).unwrap(),
            Some(HostMsg::ClipboardFileDirectionUnavailable { channel_epoch: 7, message }) if message == "notice")
        );
        let mut command = Vec::new();
        write_msg(&mut command, &ClientMsg::SuspendLocalFileDirection).unwrap();
        assert_eq!(&command[4..8], &8_u32.to_le_bytes());
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut io::Cursor::new(command)).unwrap(),
            Some(ClientMsg::SuspendLocalFileDirection)
        ));
        let mut marker = Vec::new();
        write_msg(&mut marker, &ClientMsg::LocalFileOfferRejected).unwrap();
        assert_eq!(marker, [4, 0, 0, 0, 9, 0, 0, 0]);
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut io::Cursor::new(marker)).unwrap(),
            Some(ClientMsg::LocalFileOfferRejected)
        ));
        let mut acknowledgement = Vec::new();
        write_msg(
            &mut acknowledgement,
            &HostMsg::LocalFileDirectionSuspended { channel_epoch: 7 },
        )
        .unwrap();
        assert_eq!(&acknowledgement[4..8], &13_u32.to_le_bytes());
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut io::Cursor::new(acknowledgement)).unwrap(),
            Some(HostMsg::LocalFileDirectionSuspended { channel_epoch: 7 })
        ));
        for (message, discriminant) in [
            (
                HostMsg::ClipboardChannelChanged { channel_epoch: 8 },
                14_u32,
            ),
            (
                HostMsg::ClipboardLocalFileDirectionReady { channel_epoch: 9 },
                15_u32,
            ),
        ] {
            let mut encoded = Vec::new();
            write_msg(&mut encoded, &message).unwrap();
            assert_eq!(&encoded[4..8], &discriminant.to_le_bytes());
            assert!(read_msg::<_, HostMsg>(&mut io::Cursor::new(encoded))
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn an_empty_clipboard_offer_round_trips() {
        let mut pipe = Vec::new();
        write_msg(&mut pipe, &HostMsg::ClipboardText(String::new())).expect("write");
        let msg: HostMsg = read_msg(&mut io::Cursor::new(pipe))
            .expect("read")
            .expect("a message");
        assert!(matches!(msg, HostMsg::ClipboardText(text) if text.is_empty()));
    }
}

#[cfg(test)]
mod desktop_protocol_tests {
    use super::*;
    fn screen(left: i32, top: i32, primary: bool) -> Monitor {
        Monitor {
            left,
            top,
            width: 800,
            height: 600,
            primary,
            scale_percent: 150,
        }
    }
    #[test]
    fn signed_monitor_layout_and_pointer_bitmap_survive_ipc() {
        let layout = vec![screen(-800, 0, false), screen(0, 0, true)];
        assert_eq!(monitor_bounds(&layout), Ok((-800, 0, 1600, 600)));
        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &ClientMsg::Input(InputEvent::MonitorLayout(layout.clone())),
        )
        .unwrap();
        let pointer = PointerUpdate::Bitmap {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 1,
            rgba: vec![255, 0, 0, 128, 0, 255, 0, 255, 0, 0, 255, 0, 3, 5, 7, 255],
        };
        write_msg(&mut pipe, &HostMsg::Pointer(pointer.clone())).unwrap();
        let mut input = io::Cursor::new(pipe);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut input).unwrap(),
            Some(ClientMsg::Input(InputEvent::MonitorLayout(got))) if got == layout));
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut input).unwrap(), Some(HostMsg::Pointer(got)) if got == pointer)
        );
    }
    #[test]
    fn invalid_layouts_are_rejected_before_allocation() {
        for monitors in [
            vec![],
            vec![screen(0, 0, false)],
            vec![screen(0, 0, true), screen(0, 0, false)],
            vec![screen(0, 0, true), screen(1600, 0, false)],
            vec![screen(0, 0, true), screen(0, 0, true)],
        ] {
            assert!(monitor_bounds(&monitors).is_err());
        }
        let mut m = screen(0, 0, true);
        m.width = 801;
        assert!(monitor_bounds(&[m]).is_err());
        let mut too_wide = Vec::new();
        for i in 0..11 {
            too_wide.push(screen(i * 800, 0, i == 0));
        }
        assert!(monitor_bounds(&too_wide).is_err());
    }
    #[test]
    fn tile_framing_cost_is_24_bytes_before_pixels() {
        let mut pipe = Vec::new();
        write_msg(
            &mut pipe,
            &HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![0; 4],
            },
        )
        .unwrap();
        assert_eq!(pipe.len(), 24 + 4);
    }
}
