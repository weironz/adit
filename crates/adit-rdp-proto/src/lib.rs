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

/// The geometry actually reported by the server; scale is not part of the
/// RDP monitor definition and does not change framebuffer input coordinates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorGeometry {
    pub left: i32,
    pub top: i32,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
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
                        && a.top <= b.top + i32::from(b.height)
                        && b.top <= a.top + i32::from(a.height))
                        || ((a.top + i32::from(a.height) == b.top
                            || b.top + i32::from(b.height) == a.top)
                            && a.left <= b.left + i32::from(b.width)
                            && b.left <= a.left + i32::from(a.width)))
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
        return Err("monitors must form a connected layout");
    }
    Ok((left, top, (right - left) as u16, (bottom - top) as u16))
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayControlState {
    pub max_monitors: u32,
    pub max_area: u64,
    /// Last layout actually encoded onto the DisplayControl channel.
    pub sent_layout: Vec<Monitor>,
    /// The framebuffer bounds match the sent layout. This does not confirm the
    /// server accepted the monitor arrangement: DisplayControl has no layout ack.
    pub size_matches: bool,
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
/// Adding a field here changes the bincode layout, and bincode has no version
/// tolerance: an app talking to a helper built from a different revision fails
/// to deserialise rather than degrading. That is why the two are built and
/// shipped together — see the helper note in CLAUDE.md.
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
    MonitorLayout {
        request_id: u64,
        monitors: Vec<Monitor>,
    },
    /// Discard a still-parked request after the app's deadline expires.
    CancelMonitorLayout {
        request_id: u64,
    },
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

/// App → helper (over the helper's stdin).
///
/// Merge blocker: these epoch-bearing messages change the bincode schema.
/// The handshake integrated from #94 must use a version newer than its v8; this
/// branch deliberately does not implement a separate startup handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Must be the first message; opens the session.
    Connect(ConnectRequest),
    /// Legacy untagged input. An epoch-aware helper rejects it.
    Input(InputEvent),
    /// Offer a locally-copied image to the remote clipboard, as raw `CF_DIB`
    /// bytes (a BITMAPINFOHEADER, any palette, then the pixels).
    ///
    /// Separate from `ClipboardText` because a screenshot is neither text nor a
    /// file: nothing exists on disk, so the file path cannot carry it, and the
    /// bytes are not a string. It is a third clipboard format, and MS-RDPECLIP
    /// treats it as exactly that.
    ClipboardImage(Vec<u8>),
    /// Offer a local file selection to the remote clipboard. The helper holds
    /// the metadata and answers the server's descriptor request from it; the
    /// bytes are fetched lazily, one [`HostMsg::FileContentsNeeded`] at a time.
    ClipboardFiles(Vec<ClipFile>),
    /// Bytes the app read from a local file, answering
    /// [`HostMsg::FileContentsNeeded`]. `None` reports a read failure, which the
    /// helper turns into the protocol's error response — a paste that fails
    /// loudly beats one that silently writes a truncated file.
    FileContents {
        /// Connection that issued the request, even if the reply is delayed.
        epoch: u64,
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
    /// Input belongs to the helper connection epoch announced after redirect.
    /// Epoch 0 is the initial connection. The old `Input` variant is legacy
    /// and must not be forwarded by an epoch-aware helper.
    EpochInput {
        epoch: u64,
        event: InputEvent,
    },
    EpochClipboardImage {
        epoch: u64,
        image: Vec<u8>,
    },
    EpochClipboardFiles {
        epoch: u64,
        files: Vec<ClipFile>,
    },
}

impl ClientMsg {
    pub fn input_at(epoch: u64, event: InputEvent) -> Self {
        Self::EpochInput { epoch, event }
    }
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
    /// Answer with [`ClientMsg::FileContents`] carrying the same `stream_id`.
    FileContentsNeeded {
        /// Connection whose CLIPRDR channel issued this request.
        epoch: u64,
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
    DisplayControl {
        epoch: u64,
        state: DisplayControlState,
    },
    /// Reports local encoding/writing only, not acceptance by the server.
    MonitorLayoutResult {
        epoch: u64,
        request_id: u64,
        result: Result<(), String>,
    },
    /// The helper is reconnecting after server redirection; the app may reissue
    /// the current layout with a fresh request ID.
    DisplayControlReset {
        epoch: u64,
    },
    /// A key from an obsolete connection could not safely be replayed.
    InputDiscarded,
    /// The numbered ordinary resize was written to the RDP connection.
    /// This is a local write confirmation, not server acceptance.
    ResizeSent {
        epoch: u64,
        revision: u64,
    },
    EpochClipboardText {
        epoch: u64,
        text: String,
    },
    EpochClipboardImage {
        epoch: u64,
        image: Vec<u8>,
    },
    /// Dense framebuffer pixels with a row-major, least-significant-bit-first
    /// mask identifying which pixels were actually painted in this update.
    /// This variant is part of the #97 IPC schema that must be negotiated by #94.
    MaskedTile {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        rgba: Vec<u8>,
        painted: Vec<u8>,
    },
    /// The ordinary resize was abandoned before any bytes were accepted.
    ResizeRejected {
        epoch: u64,
        revision: u64,
    },
    /// Server-reported monitor topology; an empty list means unavailable.
    MonitorLayoutObserved {
        epoch: u64,
        monitors: Vec<MonitorGeometry>,
    },
    /// A server reset invalidates prior paint coverage even at the same size.
    GraphicsReset {
        epoch: u64,
        width: u16,
        height: u16,
    },
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
                epoch: 3,
                stream_id: 7,
                data: None,
            },
        )
        .expect("write failure");
        write_msg(
            &mut pipe,
            &ClientMsg::FileContents {
                epoch: 4,
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
                epoch: 3,
                stream_id: 7,
                data: None
            }
        ));
        assert!(matches!(
            empty,
            ClientMsg::FileContents { epoch: 4, stream_id: 8, data: Some(ref bytes) } if bytes.is_empty()
        ));
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

    #[test]
    fn issue97_corner_touching_monitors_are_valid() {
        let monitors = vec![
            Monitor {
                left: 0,
                top: 0,
                width: 200,
                height: 200,
                primary: true,
                scale_percent: 100,
            },
            Monitor {
                left: 200,
                top: 200,
                width: 200,
                height: 200,
                primary: false,
                scale_percent: 100,
            },
        ];
        assert_eq!(monitor_bounds(&monitors), Ok((0, 0, 400, 400)));
    }

    #[test]
    fn issue97_legacy_file_reply_can_be_silently_misdecoded_without_handshake() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        enum LegacyClientMsg {
            Connect,
            Input,
            ClipboardImage,
            ClipboardFiles,
            FileContents {
                stream_id: u32,
                data: Option<Vec<u8>>,
            },
        }
        let mut bytes = Vec::new();
        write_msg(
            &mut bytes,
            &ClientMsg::FileContents {
                epoch: 1,
                stream_id: 7,
                data: Some(vec![42]),
            },
        )
        .unwrap();
        let decoded = read_msg::<_, LegacyClientMsg>(&mut std::io::Cursor::new(bytes)).unwrap();
        assert!(
            matches!(
                decoded,
                Some(LegacyClientMsg::FileContents {
                    stream_id: 1,
                    data: None
                })
            ),
            "this incompatibility must be rejected by the #94 version handshake"
        );
    }

    #[test]
    fn issue97_masked_tile_and_unsent_resize_round_trip() {
        let mut bytes = Vec::new();
        write_msg(
            &mut bytes,
            &HostMsg::MaskedTile {
                x: 3,
                y: 4,
                width: 2,
                height: 1,
                rgba: vec![1, 2, 3, 255, 4, 5, 6, 255],
                painted: vec![1],
            },
        )
        .unwrap();
        write_msg(
            &mut bytes,
            &HostMsg::ResizeRejected {
                epoch: 7,
                revision: 9,
            },
        )
        .unwrap();
        write_msg(
            &mut bytes,
            &HostMsg::MonitorLayoutObserved {
                epoch: 7,
                monitors: vec![MonitorGeometry {
                    left: -200,
                    top: 0,
                    width: 200,
                    height: 200,
                    primary: false,
                }],
            },
        )
        .unwrap();
        write_msg(
            &mut bytes,
            &HostMsg::GraphicsReset {
                epoch: 7,
                width: 400,
                height: 200,
            },
        )
        .unwrap();
        let mut input = std::io::Cursor::new(bytes);
        assert!(matches!(read_msg::<_, HostMsg>(&mut input).unwrap(),
            Some(HostMsg::MaskedTile { x: 3, y: 4, width: 2, height: 1, painted, .. }) if painted == [1]));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut input).unwrap(),
            Some(HostMsg::ResizeRejected {
                epoch: 7,
                revision: 9
            })
        ));
        assert!(matches!(read_msg::<_, HostMsg>(&mut input).unwrap(),
            Some(HostMsg::MonitorLayoutObserved { epoch: 7, monitors }) if monitors[0].left == -200));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut input).unwrap(),
            Some(HostMsg::GraphicsReset {
                epoch: 7,
                width: 400,
                height: 200
            })
        ));
    }
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
            &ClientMsg::EpochInput {
                epoch: 3,
                event: InputEvent::MonitorLayout {
                    request_id: 42,
                    monitors: layout.clone(),
                },
            },
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
            Some(ClientMsg::EpochInput { epoch: 3, event: InputEvent::MonitorLayout { request_id: 42, monitors: got } }) if got == layout));
        assert!(
            matches!(read_msg::<_, HostMsg>(&mut input).unwrap(), Some(HostMsg::Pointer(got)) if got == pointer)
        );
    }

    #[test]
    fn redirect_epoch_and_layout_result_survive_framed_ipc() {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &HostMsg::DisplayControlReset { epoch: 7 }).unwrap();
        write_msg(
            &mut bytes,
            &HostMsg::MonitorLayoutResult {
                epoch: 7,
                request_id: 42,
                result: Ok(()),
            },
        )
        .unwrap();
        write_msg(
            &mut bytes,
            &ClientMsg::EpochInput {
                epoch: 7,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700,
                },
            },
        )
        .unwrap();
        let mut reader = io::Cursor::new(bytes);
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut reader).unwrap(),
            Some(HostMsg::DisplayControlReset { epoch: 7 })
        ));
        assert!(matches!(
            read_msg::<_, HostMsg>(&mut reader).unwrap(),
            Some(HostMsg::MonitorLayoutResult {
                epoch: 7,
                request_id: 42,
                result: Ok(())
            })
        ));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut reader).unwrap(),
            Some(ClientMsg::EpochInput {
                epoch: 7,
                event: InputEvent::Resize {
                    width: 900,
                    height: 700
                }
            })
        ));
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
