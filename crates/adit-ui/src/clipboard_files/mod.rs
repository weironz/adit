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
//! A trait over those three would be inventing a fourth model. So the shared
//! part here is the part that genuinely is shared — turning a set of selected
//! paths into the flat, relative-path list MS-RDPECLIP wants, and reading a byte
//! range out of one — and the OS-specific clipboard access sits behind `cfg`.
//!
//! Nothing is ever copied or staged: a selection becomes metadata, and bytes are
//! read from the original path only when the remote actually pastes.

mod bridge;
#[cfg(windows)]
mod data_object;
#[cfg(windows)]
mod ole;
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
    ole::offer_to_clipboard(ole::Offer {
        files,
        bridge,
        request: std::sync::Arc::new(move |stream_id, index, offset, length| {
            requester.request(stream_id, index, offset, length);
        }),
        chunk,
    });
}

/// Non-Windows builds ship no RDP at all, so there is nothing to publish.
#[cfg(not(windows))]
pub(crate) fn offer_remote_files(
    _files: Vec<ClipFile>,
    _bridge: ChunkBridge,
    _requester: adit_session::RdpFileRequester,
    _chunk: u32,
) {
}

use std::path::{Path, PathBuf};

use adit_session::ClipFile;

/// Ceiling on how many entries one copied selection expands to. A directory tree
/// arrives fully flattened, so copying a source checkout could otherwise turn
/// into hundreds of thousands of descriptors — enumerated on the UI thread and
/// then framed down the helper pipe. The helper enforces the same bound.
pub(crate) const MAX_CLIPBOARD_FILES: usize = 65_536;

/// How deep directory recursion may go. Guards against a pathological tree and,
/// on Windows, against a directory junction that points at one of its own
/// ancestors — which `read_dir` will happily walk forever.
const MAX_DEPTH: u32 = 32;

/// A file offered to the remote: where it really lives, and what the remote was
/// told about it. The two are kept together because a `FileContentsRequest`
/// names an index into the offered list, and the local path is the only thing
/// that can answer it.
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
fn relative_name(path: &Path, base: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("\\");
    // An empty name is meaningless on the wire and upstream drops it anyway;
    // catching it here keeps a degenerate root from producing a blank entry.
    (!name.is_empty()).then_some(name)
}

/// Read one byte range out of an offered file.
///
/// Returns `None` on any failure, which the caller turns into the protocol's
/// error response. Reporting a failed read as success would hand the remote a
/// silently truncated file, which is the one outcome a file transfer must never
/// produce — so a short read at the end of the file is fine (the range simply
/// runs past the end), but an I/O error is not.
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

/// `CF_DIB`. Not exported by the `windows` crate's clipboard bindings.
#[cfg(windows)]
const CF_DIB: u32 = 8;

/// Ceiling on one clipboard image, mirroring the helper's. Sized for a
/// screenshot rather than for text: an uncompressed 4K DIB is about 33 MB.
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

#[cfg(not(windows))]
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

/// The paths currently on the Windows clipboard as a file selection (`CF_HDROP`),
/// or `None` when it holds something else.
#[cfg(windows)]
pub(crate) fn clipboard_paths() -> Option<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt as _;

    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    const CF_HDROP: u32 = 15;

    // SAFETY: the clipboard is opened and unconditionally closed on every path
    // out of this block. Holding it open freezes every other application that
    // tries to read the clipboard, so the close is not optional.
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() {
            return None;
        }
        // A null owner is legal and means "the current task". Opening can fail
        // simply because another process holds it; that is a normal race on a
        // 500 ms poll, not an error worth reporting.
        if OpenClipboard(None).is_err() {
            return None;
        }

        let paths = (|| {
            let handle = GetClipboardData(CF_HDROP).ok()?;
            let drop_handle = HDROP(handle.0);
            // Index 0xFFFF_FFFF asks for the count rather than a name.
            let count = DragQueryFileW(drop_handle, u32::MAX, None);
            let mut paths = Vec::with_capacity(count as usize);
            for index in 0..count.min(MAX_CLIPBOARD_FILES as u32) {
                // Called twice per file by design: once for the length, once for
                // the characters. The first call must pass no buffer.
                let len = DragQueryFileW(drop_handle, index, None);
                if len == 0 {
                    continue;
                }
                // +1 for the null terminator the second call writes.
                let mut buffer = vec![0u16; len as usize + 1];
                let written = DragQueryFileW(drop_handle, index, Some(&mut buffer));
                if written == 0 {
                    continue;
                }
                buffer.truncate(written as usize);
                paths.push(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
            }
            (!paths.is_empty()).then_some(paths)
        })();

        let _ = CloseClipboard();
        paths
    }
}

/// Non-Windows builds do not ship RDP at all (the helper is
/// `adit-rdp-host.exe`, located by name), so there is nothing to offer here.
#[cfg(not(windows))]
pub(crate) fn clipboard_paths() -> Option<Vec<PathBuf>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(names, ["work", "work\\a.bin", "work\\sub", "work\\sub\\b.txt"]);

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

        assert_eq!(read_range(&path, 0, 8).expect("full read"), [0, 1, 2, 3, 4, 5, 6, 7]);
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
