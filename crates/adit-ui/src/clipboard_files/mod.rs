//! Local half of RDP clipboard **file** transfer.
//!
//! ## Why this is not behind a cross-platform trait
//!
//! The helper's CLIPRDR side is pure protocol and runs anywhere. This side
//! cannot be, and the three platforms do not merely differ in API — they differ
//! in *design*:
//!
//! * **Windows** offers files through a COM `IDataObject` carrying
//!   `CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`, with the contents
//!   **delay-rendered**: Explorer calls back for each file's bytes at paste time.
//! * **macOS** promises them through `NSFilePromiseProvider`, a model built
//!   mainly for drag-and-drop.
//! * **X11/Wayland** has no delayed-content mechanism for files at all. Its
//!   `text/uri-list` names paths that must already exist, which is why FreeRDP
//!   mounts a FUSE filesystem to make remote files appear real without
//!   downloading them first.
//!
//! The app publishes remote files through delayed reads. Local file-to-RDP
//! sharing is temporarily disabled; the production path does not enumerate or
//! open locally selected files. Linux publishes a private read-only FUSE mount
//! and reports DISPLAY/FUSE errors without closing the RDP session.

// The bridge's *users* — `data_object` and `ole` — are Windows-only, but the
// type itself cannot be, because it appears in `offer_remote_files`'s
// signature and that function has a stub for the other platforms. So on
// non-Windows every item in here is unreachable by construction, which
// `-D warnings` reports as six dead-code errors. Saying so is honest; gating
// the module would mean two different signatures for one function.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod bridge;
#[cfg(windows)]
pub(crate) mod data_object;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod linux_fuse;
#[cfg(windows)]
mod ole;
#[cfg(any(target_os = "linux", test))]
mod remote_file_echo;
#[cfg(any(target_os = "linux", test))]
mod remote_tree;
#[allow(unused_imports)]
pub(crate) use bridge::{ChunkBridge, ChunkError};

/// Publish a file list the remote copied on the Windows clipboard.
///
/// Metadata only: the data object is delay-rendered, so nothing crosses the
/// wire until something on this machine actually pastes.
#[cfg(windows)]
pub(crate) fn offer_remote_files(
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    requester: adit_session::RdpFileRequester,
    chunk: u32,
) {
    let current = requester.clone();
    ole::offer_to_clipboard(ole::Offer {
        current: std::sync::Arc::new(move || current.is_current()),
        files,
        bridge,
        request: std::sync::Arc::new(move |stream_id, index, offset, length| {
            requester.request(stream_id, index, offset, length);
        }),
        chunk,
    });
}

/// Linux exposes URI metadata backed by a private read-only FUSE mount.
#[cfg(target_os = "linux")]
pub(crate) fn offer_remote_files(
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    requester: adit_session::RdpFileRequester,
    chunk: u32,
) {
    linux::offer(files, bridge, requester, chunk);
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn offer_remote_files(
    _files: Vec<ClipFile>,
    _bridge: ChunkBridge,
    _requester: adit_session::RdpFileRequester,
    _chunk: u32,
) {
}

pub(crate) fn take_clipboard_error() -> Option<String> {
    #[cfg(target_os = "linux")]
    return linux::take_error();
    #[cfg(not(target_os = "linux"))]
    None
}

pub(crate) fn is_own_remote_file_text(text: &str, token: Option<u32>) -> bool {
    #[cfg(target_os = "linux")]
    return linux::is_own_remote_file_text(text, token);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (text, token);
        false
    }
}

pub(crate) fn shutdown_file_clipboard() {
    #[cfg(target_os = "linux")]
    linux::shutdown();
}

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use adit_session::ClipFile;

/// Ceiling on how many entries one copied selection expands to. A directory tree
/// arrives fully flattened, so copying a source checkout could otherwise turn
/// into hundreds of thousands of descriptors — enumerated on the UI thread and
/// then framed down the helper pipe. The helper enforces the same bound.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MAX_CLIPBOARD_FILES: usize = 65_536;

// Legacy local-file code below is retained only for isolated tests; it is not
// compiled into the app while local-to-RDP transfers are disabled.
/// How deep directory recursion may go. Guards against a pathological tree and,
/// on Windows, against a directory junction that points at one of its own
/// ancestors — which `read_dir` will happily walk forever.
#[cfg(test)]
const MAX_DEPTH: u32 = 32;

/// A file offered to the remote: where it really lives, and what the remote was
/// told about it. The two are kept together because a `FileContentsRequest`
/// names an index into the offered list, and the local path is the only thing
/// that can answer it.
// Window state still carries this type, but production no longer reads local
// file metadata while the temporary restriction is active.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedFile {
    pub(crate) path: PathBuf,
    pub(crate) meta: ClipFile,
}

/// Expand a selection of paths into the flat, relative-path list CLIPRDR wants.
///
/// Directories become their own zero-length entries *and* are recursed into —
/// both are required. Without the directory entry the receiver has nowhere to
/// put the files under it; without the recursion an empty folder is all that
/// arrives. Entry order matters: a directory must precede its contents, because
/// that is the order the receiver creates them in.
///
/// Names are relative to each selected root's parent, so copying `C:\work\docs`
/// produces `docs`, `docs\a.txt`, … — which is what makes the paste land as a
/// folder rather than as loose files.
#[cfg(test)]
pub(crate) fn expand(roots: &[PathBuf]) -> Vec<OfferedFile> {
    let mut out = Vec::new();
    for root in roots {
        // The base is the *parent*, so the selected item's own name survives into
        // the relative path. A root with no parent (`C:\`) has nothing to strip.
        let base = root.parent().unwrap_or(root).to_path_buf();
        push_entry(&mut out, root, &base, 0);
        if out.len() >= MAX_CLIPBOARD_FILES {
            break;
        }
    }
    out.truncate(MAX_CLIPBOARD_FILES);
    out
}

#[cfg(test)]
fn push_entry(out: &mut Vec<OfferedFile>, path: &Path, base: &Path, depth: u32) {
    if out.len() >= MAX_CLIPBOARD_FILES {
        return;
    }
    let Some(name) = relative_name(path, base) else {
        return;
    };
    // `symlink_metadata`, not `metadata`: a symlink is described by what it is,
    // so a link pointing outside the selection cannot smuggle its target's bytes
    // into the copy — and a self-referential one cannot be recursed into.
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return;
        } // FILE_ATTRIBUTE_REPARSE_POINT
    }
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir() {
        out.push(OfferedFile {
            path: path.to_path_buf(),
            meta: ClipFile {
                name,
                size: 0,
                is_dir: true,
            },
        });
        if depth >= MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        // Sorted, so copying the same folder twice produces the same indices.
        // `read_dir` order is filesystem-defined, and the index is what a
        // `FileContentsRequest` refers to.
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            push_entry(out, &child, base, depth + 1);
        }
    } else if meta.is_file() {
        out.push(OfferedFile {
            path: path.to_path_buf(),
            meta: ClipFile {
                name,
                size: meta.len(),
                is_dir: false,
            },
        });
    }
    // Anything else (a symlink, a device) is skipped rather than guessed at.
}

/// The `\`-separated path of `path` relative to `base`, or `None` if it escapes.
#[cfg(test)]
fn relative_name(path: &Path, base: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("\\");
    // An empty name is meaningless on the wire and upstream drops it anyway;
    // catching it here keeps a degenerate root from producing a blank entry.
    adit_session::validate_clipboard_relative_name(&name).ok()?;
    Some(name)
}

/// Read one byte range out of an offered file.
///
/// Returns `None` on any failure, which the caller turns into the protocol's
/// error response. Reporting a failed read as success would hand the remote a
/// silently truncated file, which is the one outcome a file transfer must never
/// produce — so a short read at the end of the file is fine (the range simply
/// runs past the end), but an I/O error is not.
#[cfg(test)]
pub(crate) fn read_range(path: &Path, offset: u64, length: u32) -> Option<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0u8; length as usize];
    let mut filled = 0usize;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break, // end of file: a short final chunk is legitimate
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    buffer.truncate(filled);
    Some(buffer)
}

/// Do not open a previously offered path while clipboard sharing is disabled.
#[cfg(test)]
pub(crate) fn read_offered_range(
    enabled: bool,
    files: &[OfferedFile],
    index: u32,
    offset: u64,
    length: u32,
) -> Option<Vec<u8>> {
    if !enabled {
        return None;
    }
    files
        .get(index as usize)
        .and_then(|file| read_range(&file.path, offset, length))
}

/// `CF_DIB`. Not exported by the `windows` crate's clipboard bindings.
#[cfg(windows)]
const CF_DIB: u32 = 8;

/// Ceiling on one clipboard image, mirroring the helper's. Sized for a
/// screenshot rather than for text: an uncompressed 4K DIB is about 33 MB.
///
/// Only the Windows clipboard functions consult it; the stubs beside them do
/// not, so elsewhere it is dead by construction.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// A counter Windows bumps on every clipboard change, or 0 if unavailable.
///
/// The point is to avoid copying the image at all when nothing changed: a
/// full-screen screenshot is tens of megabytes and the RDP clipboard poll runs
/// twice a second, so comparing this number is a syscall instead of a copy.
#[cfg(windows)]
pub(crate) fn clipboard_sequence() -> u32 {
    // SAFETY: no arguments and no handles; 0 means "not available".
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(target_os = "linux")]
pub(crate) fn clipboard_sequence() -> u32 {
    linux::sequence()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn clipboard_sequence() -> u32 {
    0
}

/// The image on the Windows clipboard as raw `CF_DIB` bytes, or `None` when it
/// holds no image.
///
/// Handed over untouched: a DIB is a BITMAPINFOHEADER followed by its pixels,
/// and the remote understands that layout already. Decoding it here would only
/// add a way to get it wrong.
#[cfg(windows)]
pub(crate) fn clipboard_image() -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    // SAFETY: the clipboard is opened and unconditionally closed on every path
    // out. Holding it open freezes every other application that reads it.
    unsafe {
        if IsClipboardFormatAvailable(CF_DIB).is_err() {
            return None;
        }
        if OpenClipboard(None).is_err() {
            // Another process holds it — a normal race on a timed poll, not an
            // error worth reporting.
            return None;
        }
        let bytes = (|| {
            let handle = GetClipboardData(CF_DIB).ok()?;
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            if size == 0 || size > MAX_CLIPBOARD_IMAGE_BYTES {
                return None;
            }
            let base = GlobalLock(global);
            if base.is_null() {
                return None;
            }
            let copy = std::slice::from_raw_parts(base.cast::<u8>(), size).to_vec();
            let _ = GlobalUnlock(global);
            Some(copy)
        })();
        let _ = CloseClipboard();
        bytes
    }
}

#[cfg(not(windows))]
pub(crate) fn clipboard_image() -> Option<Vec<u8>> {
    None
}

/// Put raw `CF_DIB` bytes on the Windows clipboard.
#[cfg(windows)]
pub(crate) fn set_clipboard_image(bytes: &[u8]) -> bool {
    // `GlobalFree` lives in Foundation, not Memory, unlike its Alloc/Lock kin.
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    if bytes.is_empty() || bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return false;
    }
    // SAFETY: the clipboard is opened and unconditionally closed. The allocation
    // is handed to SetClipboardData, after which the system owns it — freeing it
    // then would be a double free, so it is freed only when that call fails.
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
            let base = GlobalLock(global);
            if base.is_null() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(global);
            match SetClipboardData(CF_DIB, Some(HANDLE(global.0))) {
                Ok(_) => Some(()),
                Err(_) => {
                    let _ = GlobalFree(Some(global));
                    None
                }
            }
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(not(windows))]
pub(crate) fn set_clipboard_image(_bytes: &[u8]) -> bool {
    false
}

#[cfg(any(windows, test))]
const WINDOWS_FILE_FORMAT_NAMES: &[&str] = &[
    "FileGroupDescriptorW",
    "FileGroupDescriptor",
    "FileContents",
    "FileNameW",
    "FileName",
    "Shell IDList Array",
];

#[cfg(any(windows, test))]
fn windows_has_file_format(mut available: impl FnMut(&str) -> Option<bool>) -> bool {
    WINDOWS_FILE_FORMAT_NAMES
        .iter()
        .any(|name| available(name).unwrap_or(true))
}

/// Snapshot text only while Windows does not expose a file selection. Sequence
/// checks discard async reads if the clipboard changes before completion.
#[cfg(windows)]
pub(crate) fn rdp_text_clipboard_token() -> Option<u32> {
    use windows::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    let before = clipboard_sequence();
    // Check only format availability: do not fetch paths, descriptors or bytes.
    let has_files = unsafe {
        IsClipboardFormatAvailable(15).is_ok()
            || windows_has_file_format(|name| {
                let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                let id = RegisterClipboardFormatW(windows::core::PCWSTR(name.as_ptr()));
                (id != 0).then(|| IsClipboardFormatAvailable(id).is_ok())
            })
    };
    let after = clipboard_sequence();
    safe_clipboard_snapshot(before, after, has_files).then_some(before)
}

#[cfg(target_os = "linux")]
pub(crate) fn rdp_text_clipboard_token() -> Option<u32> {
    // Reject text captured before the X11 ownership observer advanced.
    Some(clipboard_sequence())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn rdp_text_clipboard_token() -> Option<u32> {
    Some(0)
}

#[cfg(any(windows, test))]
pub(crate) fn safe_clipboard_snapshot(before: u32, after: u32, has_files: bool) -> bool {
    before != 0 && before == after && !has_files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_file_formats_override_an_available_text_representation() {
        for format in [
            "FileNameW",
            "FileName",
            "Shell IDList Array",
            "FileGroupDescriptorW",
            "FileGroupDescriptor",
            "FileContents",
        ] {
            assert!(
                windows_has_file_format(|name| Some(name == format)),
                "{format}"
            );
        }
        assert!(!windows_has_file_format(|_| Some(false)));
        assert!(windows_has_file_format(
            |name| (name != "FileNameW").then_some(false)
        ));
    }

    #[test]
    fn disabled_sharing_does_not_read_an_offered_file() {
        let root = tree();
        let files = expand(&[root.join("work").join("a.bin")]);
        assert_eq!(
            read_offered_range(true, &files, 0, 0, 4),
            Some(vec![0, 1, 2, 3])
        );
        assert_eq!(read_offered_range(false, &files, 0, 0, 4), None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Build a small tree and hand back its root.
    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "adit-clipfiles-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("work").join("sub")).expect("mkdir");
        std::fs::write(root.join("work").join("a.bin"), [0u8, 1, 2, 3, 4, 5, 6, 7]).expect("write");
        std::fs::write(root.join("work").join("sub").join("b.txt"), b"hello").expect("write");
        root
    }

    /// The list is flat, relative to the selection's *parent*, and a directory
    /// comes before the things inside it — that ordering is what lets the
    /// receiver create the folder before writing into it.
    #[test]
    fn a_directory_expands_to_a_flat_relative_list_in_creation_order() {
        let root = tree();
        let files = expand(&[root.join("work")]);

        let names: Vec<&str> = files.iter().map(|f| f.meta.name.as_str()).collect();
        assert_eq!(
            names,
            ["work", "work\\a.bin", "work\\sub", "work\\sub\\b.txt"]
        );

        // Directories carry no size; files carry their real one.
        assert!(files[0].meta.is_dir && files[0].meta.size == 0);
        assert_eq!(files[1].meta.size, 8);
        assert!(files[2].meta.is_dir);
        assert_eq!(files[3].meta.size, 5);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Binary is the whole point: bytes come back exactly as written, including
    /// a NUL, and a range that runs past the end returns the short tail rather
    /// than failing.
    #[test]
    fn a_byte_range_is_returned_verbatim_and_clamps_at_the_end() {
        let root = tree();
        let path = root.join("work").join("a.bin");

        assert_eq!(
            read_range(&path, 0, 8).expect("full read"),
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(read_range(&path, 4, 2).expect("mid read"), [4, 5]);
        // Past the end: the tail, not an error.
        assert_eq!(read_range(&path, 6, 64).expect("tail read"), [6, 7]);
        // Entirely past the end is an empty chunk, which is how a transfer ends.
        assert!(read_range(&path, 99, 8).expect("beyond read").is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A read that cannot happen must be `None`, not an empty chunk: the caller
    /// turns `None` into the protocol error and an empty chunk into "end of
    /// file", and confusing the two writes a truncated file on the far side.
    #[test]
    fn a_missing_file_fails_rather_than_reading_as_empty() {
        let missing = std::env::temp_dir().join("adit-clipfiles-does-not-exist.bin");
        assert!(read_range(&missing, 0, 8).is_none());
    }

    /// A single file keeps its own name and nothing more — copying `a.bin` must
    /// paste as `a.bin`, not as `work\a.bin`.
    #[test]
    fn a_lone_file_is_offered_under_its_bare_name() {
        let root = tree();
        let files = expand(&[root.join("work").join("a.bin")]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].meta.name, "a.bin");
        assert!(!files[0].meta.is_dir);

        std::fs::remove_dir_all(&root).ok();
    }
}
