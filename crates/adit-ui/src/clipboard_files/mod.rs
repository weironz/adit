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
//! part here is the part that genuinely is shared — validating a read-only
//! remote tree with bounded reads, turning a set of selected
//! paths into the flat, relative-path list MS-RDPECLIP wants, and reading a byte
//! range out of one — and the OS-specific clipboard access sits behind `cfg`.
//!
//! Nothing is ever copied or staged: a selection becomes metadata and open file
//! handles, and bytes are read from those handles only when the remote pastes. Linux
//! publishes private read-only FUSE paths over X11's file-URI clipboard; reads
//! stay on the filesystem worker and retain the originating session requester.
//! Missing DISPLAY/FUSE support reports a clipboard error without closing RDP.

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
    let reply = bridge.clone();
    ole::offer_to_clipboard(ole::Offer {
        current: std::sync::Arc::new(move || current.is_current()),
        files,
        bridge,
        request: std::sync::Arc::new(move |stream_id, index, offset, length| {
            if requester
                .try_request(stream_id, index, offset, length)
                .is_err()
            {
                reply.deliver(stream_id, None);
            }
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

pub(crate) fn shutdown_file_clipboard() {
    #[cfg(target_os = "linux")]
    linux::shutdown();
}

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex, OnceLock,
};

use adit_domain::SessionId;
use adit_session::{ClipFile, RdpFileRead};

/// Ceiling on how many entries one copied selection expands to. A directory tree
/// arrives fully flattened, so copying a source checkout could otherwise turn
/// into hundreds of thousands of descriptors framed down the helper pipe. The
/// helper enforces the same bound.
pub(crate) const MAX_CLIPBOARD_FILES: usize = 65_536;

/// How deep directory recursion may go. Guards against a pathological tree and,
/// on Windows, against a directory junction that points at one of its own
/// ancestors — which `read_dir` will happily walk forever.
const MAX_DEPTH: u32 = 32;

// Pinning an unbounded directory tree could exhaust the process handle limit.
// Refuse the entire selection at this limit, rather than offer a partial tree.
const MAX_BOUND_FILES: usize = 1024;
// Includes openings in progress and handles waiting for the reaper, across all
// selections and sessions. Directory handles opened during binding count too.
const MAX_FILE_HANDLES: usize = 2048;
const MAX_PENDING_OFFERS: usize = 4;
const MAX_FILE_READ_BYTES: u32 = 64 * 1024;

// Temporary policy: the helper independently seals this direction. Keep the
// binding and read gates together until local-to-remote files are re-enabled.
const LOCAL_FILE_SEND_ENABLED: bool = false;
pub(crate) const LOCAL_FILE_SEND_DISABLED_NOTICE: &str =
    "RDP 本地文件发送至远端已暂时禁用；检测到文件或未知内容时暂停本地文字发送，远端文件接收仍可使用。";

/// A delayed read must retain the object named when this offer was prepared.
/// The path remains useful for diagnostics, but is never reopened for bytes.
#[derive(Debug, Clone)]
pub(crate) struct OfferedFile {
    pub(crate) path: PathBuf,
    pub(crate) meta: ClipFile,
    file: Option<Arc<Mutex<BoundFile>>>,
}

impl PartialEq for OfferedFile {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.meta == other.meta
            && match (&self.file, &other.file) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for OfferedFile {}

impl OfferedFile {
    #[cfg(test)]
    pub(crate) fn test_fixture(name: &str) -> Self {
        Self {
            path: PathBuf::from(name),
            meta: ClipFile {
                name: name.into(),
                size: 1,
                is_dir: false,
            },
            file: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_bind(path: &Path) -> Self {
        expand(&[path.to_path_buf()], &AtomicBool::new(false))
            .expect("bind synthetic fixture")
            .pop()
            .expect("one synthetic file")
    }

    #[cfg(test)]
    pub(crate) fn test_read(&self) -> Option<Vec<u8>> {
        read_range(self, 0, MAX_FILE_READ_BYTES)
    }

    #[cfg(test)]
    pub(crate) fn test_is_bound(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let file = Arc::downgrade(self.file.as_ref().expect("synthetic file handle"));
        move || file.upgrade().is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindError {
    Unavailable,
    SelectionLimit,
    HandleBudget,
}

impl From<()> for BindError {
    fn from(_: ()) -> Self {
        Self::Unavailable
    }
}

impl BindError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::SelectionLimit => "本次文件选择超过 1024 个普通文件，请分批复制；远程桌面仍可使用。",
            Self::HandleBudget => "文件共享句柄达到全局上限 2048，先前文件可能仍在关闭，请稍后重新复制；远程桌面仍可使用。",
            Self::Unavailable => "无法发布 RDP 文件剪贴板",
        }
    }
}

/// A permit covers the open itself, live offers, reads and the reaper queue.
#[derive(Debug)]
struct FileBudget {
    used: AtomicUsize,
    limit: usize,
}

impl FileBudget {
    fn reserve(self: &Arc<Self>) -> Result<FilePermit, BindError> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .map_err(|_| BindError::HandleBudget)?;
        Ok(FilePermit(self.clone()))
    }
}

#[derive(Debug)]
struct FilePermit(Arc<FileBudget>);

impl Drop for FilePermit {
    fn drop(&mut self) {
        let previous = self.0.used.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

// Field order also preserves the close-before-release invariant if the reaper
// unwinds and drops jobs still in its receiver.
#[derive(Debug)]
struct CloseJob {
    file: std::fs::File,
    permit: FilePermit,
}

impl CloseJob {
    fn close(self) {
        let Self { file, permit } = self;
        drop(file);
        drop(permit);
    }
}

#[derive(Clone)]
struct FileResources {
    budget: Arc<FileBudget>,
    closer: mpsc::Sender<CloseJob>,
}

impl FileResources {
    fn open(
        &self,
        open: impl FnOnce() -> Result<std::fs::File, ()>,
    ) -> Result<BoundFile, BindError> {
        let permit = self.budget.reserve()?;
        let file = open()?;
        Ok(BoundFile {
            file: Some(file),
            permit: Some(permit),
            closer: self.closer.clone(),
        })
    }
}

/// Closing a file on a network filesystem can block as well as opening it.
/// The last offer/read reference hands its handle and permit to the reaper.
#[derive(Debug)]
struct BoundFile {
    file: Option<std::fs::File>,
    permit: Option<FilePermit>,
    closer: mpsc::Sender<CloseJob>,
}

impl Drop for BoundFile {
    fn drop(&mut self) {
        if let (Some(file), Some(permit)) = (self.file.take(), self.permit.take()) {
            if let Err(error) = self.closer.send(CloseJob { file, permit }) {
                // Keep the permit with the leaked handle if the reaper died;
                // otherwise repeated offers could leak without a bound.
                std::mem::forget(error.0);
            }
        }
    }
}

fn file_resources() -> Option<FileResources> {
    static RESOURCES: OnceLock<Option<FileResources>> = OnceLock::new();
    RESOURCES
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<CloseJob>();
            std::thread::Builder::new()
                .name("adit-clipboard-file-close".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        job.close();
                    }
                })
                .ok()?;
            Some(FileResources {
                budget: Arc::new(FileBudget {
                    used: AtomicUsize::new(0),
                    limit: MAX_FILE_HANDLES,
                }),
                closer: tx,
            })
        })
        .clone()
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
fn expand(roots: &[PathBuf], cancelled: &AtomicBool) -> Result<Vec<OfferedFile>, BindError> {
    let resources = file_resources().ok_or(())?;
    expand_with_resources(roots, cancelled, &resources)
}

#[cfg(windows)]
fn expand_with_resources(
    roots: &[PathBuf],
    cancelled: &AtomicBool,
    resources: &FileResources,
) -> Result<Vec<OfferedFile>, BindError> {
    windows_bound::expand(roots, cancelled, resources)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn expand_with_resources(
    roots: &[PathBuf],
    cancelled: &AtomicBool,
    resources: &FileResources,
) -> Result<Vec<OfferedFile>, BindError> {
    unix_bound::expand(roots, cancelled, resources)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn expand_with_resources(
    roots: &[PathBuf],
    cancelled: &AtomicBool,
    resources: &FileResources,
) -> Result<Vec<OfferedFile>, BindError> {
    let mut out = Vec::new();
    let mut open_files = 0;
    for root in roots {
        // The base is the *parent*, so the selected item's own name survives into
        // the relative path. A root with no parent (`C:\`) has nothing to strip.
        let base = root.parent().unwrap_or(root).to_path_buf();
        push_entry(
            &mut out,
            root,
            &base,
            0,
            resources,
            &mut open_files,
            cancelled,
        )?;
    }
    Ok(out)
}

#[cfg(windows)]
mod windows_bound {
    use super::*;
    use std::ffi::{c_void, OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Component, Prefix};

    const REPARSE_POINT: u32 = 0x400;
    const STATUS_NO_MORE_FILES: i32 = 0x8000_0006u32 as i32;
    const STATUS_NO_SUCH_FILE: i32 = 0xc000_000fu32 as i32;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: *mut c_void,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtCreateFile(
            file: *mut *mut c_void,
            access: u32,
            attributes: *const ObjectAttributes,
            status: *mut IoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn NtQueryDirectoryFile(
            file: *mut c_void,
            event: *mut c_void,
            apc: *mut c_void,
            apc_context: *mut c_void,
            status: *mut IoStatusBlock,
            information: *mut c_void,
            length: u32,
            information_class: u32,
            single_entry: u8,
            name: *const UnicodeString,
            restart: u8,
        ) -> i32;
    }

    fn ordinary_directory(file: &std::fs::File) -> Result<(), ()> {
        let meta = file.metadata().map_err(|_| ())?;
        if !meta.is_dir() || meta.file_attributes() & REPARSE_POINT != 0 {
            return Err(());
        }
        Ok(())
    }

    // Resolve one name against an already-open directory. OBJ_DONT_REPARSE
    // prevents a directory changed into a junction from redirecting this open.
    pub(super) fn open_child(parent: &std::fs::File, name: &OsStr) -> Result<std::fs::File, ()> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(());
        }
        let mut wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty() || wide.contains(&0) {
            return Err(());
        }
        let bytes = u16::try_from(wide.len().checked_mul(2).ok_or(())?).map_err(|_| ())?;
        let mut name = UnicodeString {
            length: bytes,
            maximum_length: bytes,
            buffer: wide.as_mut_ptr(),
        };
        let attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: parent.as_raw_handle(),
            object_name: &mut name,
            attributes: 0x40 | 0x1000, // OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut io = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let mut handle = std::ptr::null_mut();
        // SAFETY: the name, object attributes and status block stay alive for
        // this synchronous call. A successful handle is transferred to File.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                0x0010_0081, // SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_READ_DATA/LIST_DIRECTORY
                &attributes,
                &mut io,
                std::ptr::null(),
                0,
                0x7,         // share read, write and delete
                1,           // FILE_OPEN
                0x0020_0020, // FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT
                std::ptr::null(),
                0,
            )
        };
        if status < 0 || handle.is_null() {
            return Err(());
        }
        // SAFETY: successful NtCreateFile returned a uniquely owned handle.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    pub(super) fn open_parent(path: &Path) -> Result<(std::fs::File, OsString), ()> {
        let mut components = path.components();
        let prefix = match components.next() {
            Some(Component::Prefix(prefix))
                if matches!(
                    prefix.kind(),
                    Prefix::Disk(_)
                        | Prefix::VerbatimDisk(_)
                        | Prefix::UNC(_, _)
                        | Prefix::VerbatimUNC(_, _)
                ) =>
            {
                prefix
            }
            _ => return Err(()),
        };
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(());
        }
        let mut root = PathBuf::from(prefix.as_os_str());
        root.push(Path::new("\\"));
        let mut directory = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(0x0010_0081)
            .share_mode(0x7)
            .custom_flags(0x0200_0000 | 0x0020_0000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
            .open(root)
            .map_err(|_| ())?;
        ordinary_directory(&directory)?;

        let mut remainder: Vec<_> = components.collect();
        let leaf = match remainder.pop() {
            Some(Component::Normal(name)) => name.to_os_string(),
            _ => return Err(()),
        };
        for component in remainder {
            let Component::Normal(name) = component else {
                return Err(());
            };
            let child = open_child(&directory, name)?;
            ordinary_directory(&child)?;
            directory = child;
        }
        Ok((directory, leaf))
    }

    // The directory handle, not a reopened path, is the enumeration source.
    pub(super) fn children(
        directory: &std::fs::File,
        remaining: usize,
    ) -> Result<Vec<PathBuf>, ()> {
        let mut buffer = vec![0u64; 8192];
        let capacity = buffer.len() * std::mem::size_of::<u64>();
        let mut restart = 1;
        let mut children = Vec::new();
        loop {
            let mut io = IoStatusBlock {
                status_or_pointer: 0,
                information: 0,
            };
            // SAFETY: the buffer is aligned, writable, and remains alive for
            // the synchronous query. The handle has directory-list access.
            let status = unsafe {
                NtQueryDirectoryFile(
                    directory.as_raw_handle(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut io,
                    buffer.as_mut_ptr().cast(),
                    capacity as u32,
                    12, // FileNamesInformation
                    0,
                    std::ptr::null(),
                    restart,
                )
            };
            restart = 0;
            if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
                break;
            }
            if status < 0 || status == 0x103 || io.information > capacity {
                return Err(());
            }
            // SAFETY: NtQueryDirectoryFile initialized precisely information
            // bytes in the allocated buffer, checked above.
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), io.information) };
            let mut offset = 0usize;
            loop {
                let header = bytes
                    .get(offset..offset.checked_add(12).ok_or(())?)
                    .ok_or(())?;
                let next = u32::from_le_bytes(header[0..4].try_into().map_err(|_| ())?) as usize;
                let name_len =
                    u32::from_le_bytes(header[8..12].try_into().map_err(|_| ())?) as usize;
                let end = if next == 0 {
                    bytes.len()
                } else {
                    offset.checked_add(next).ok_or(())?
                };
                let name_end = offset
                    .checked_add(12)
                    .and_then(|n| n.checked_add(name_len))
                    .ok_or(())?;
                if !name_len.is_multiple_of(2) || name_end > end || end > bytes.len() {
                    return Err(());
                }
                let wide: Vec<u16> = bytes[offset + 12..name_end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| u16::from_le_bytes(pair))
                    .collect();
                let name = OsString::from_wide(&wide);
                if name != "." && name != ".." {
                    if children.len() >= remaining {
                        return Err(());
                    }
                    children.push(PathBuf::from(name));
                }
                if next == 0 {
                    break;
                }
                if next < 12 {
                    return Err(());
                }
                offset = end;
            }
        }
        collect_children(children.into_iter().map(Ok), remaining)
    }

    struct Binder<'a> {
        out: Vec<OfferedFile>,
        resources: FileResources,
        open_files: usize,
        cancelled: &'a AtomicBool,
    }

    impl Binder<'_> {
        fn visit(
            &mut self,
            parent: &std::fs::File,
            name: &OsStr,
            path: &Path,
            base: &Path,
            depth: u32,
        ) -> Result<(), BindError> {
            if self.cancelled.load(Ordering::Acquire)
                || self.out.len() >= MAX_CLIPBOARD_FILES
                || depth > MAX_DEPTH
            {
                return Err(BindError::Unavailable);
            }
            let relative = relative_name(path, base).ok_or(())?;
            let file = self.resources.open(|| open_child(parent, name))?;
            let handle = file.file.as_ref().ok_or(())?;
            let meta = handle.metadata().map_err(|_| ())?;
            if meta.file_attributes() & REPARSE_POINT != 0 {
                return Ok(());
            }
            if meta.is_dir() {
                self.out.push(OfferedFile {
                    path: path.to_path_buf(),
                    meta: ClipFile {
                        name: relative,
                        size: 0,
                        is_dir: true,
                    },
                    file: None,
                });
                for child in children(handle, MAX_CLIPBOARD_FILES - self.out.len())? {
                    self.visit(
                        handle,
                        child.as_os_str(),
                        &path.join(&child),
                        base,
                        depth + 1,
                    )?;
                }
            } else if meta.is_file() {
                if self.open_files >= MAX_BOUND_FILES {
                    return Err(BindError::SelectionLimit);
                }
                self.open_files += 1;
                self.out.push(OfferedFile {
                    path: path.to_path_buf(),
                    meta: ClipFile {
                        name: relative,
                        size: meta.len(),
                        is_dir: false,
                    },
                    file: Some(Arc::new(Mutex::new(file))),
                });
            }
            Ok(())
        }
    }

    pub(super) fn expand(
        roots: &[PathBuf],
        cancelled: &AtomicBool,
        resources: &FileResources,
    ) -> Result<Vec<OfferedFile>, BindError> {
        expand_from_count(roots, cancelled, resources, 0)
    }

    fn expand_from_count(
        roots: &[PathBuf],
        cancelled: &AtomicBool,
        resources: &FileResources,
        open_files: usize,
    ) -> Result<Vec<OfferedFile>, BindError> {
        let mut binder = Binder {
            out: Vec::new(),
            resources: resources.clone(),
            open_files,
            cancelled,
        };
        for root in roots {
            let base = root.parent().ok_or(())?;
            let (parent, name) = open_parent(root)?;
            binder.visit(&parent, &name, root, base, 0)?;
        }
        Ok(binder.out)
    }

    #[cfg(test)]
    pub(super) fn expand_near_file_limit(
        roots: &[PathBuf],
        cancelled: &AtomicBool,
        resources: &FileResources,
    ) -> Result<Vec<OfferedFile>, BindError> {
        expand_from_count(roots, cancelled, resources, MAX_BOUND_FILES - 1)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_bound {
    use super::*;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Component;

    #[cfg(target_os = "linux")]
    mod sys {
        pub(super) use libc::{
            closedir, fdopendir, fstatat, openat, readdir, AT_SYMLINK_NOFOLLOW, O_CLOEXEC,
            O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK, O_RDONLY, S_IFDIR, S_IFMT, S_IFREG,
        };
        pub(super) use libc::{stat as Stat, DIR as Dir};

        pub(super) unsafe fn errno() -> *mut std::ffi::c_int {
            unsafe { libc::__errno_location() }
        }
    }

    // macOS does not currently declare libc as a direct dependency of adit-ui.
    // These are the Darwin POSIX ABI definitions needed by this binder.
    #[cfg(target_os = "macos")]
    mod sys {
        use std::ffi::{c_char, c_int, c_long, c_void};

        pub(super) const O_RDONLY: c_int = 0;
        pub(super) const O_NONBLOCK: c_int = 0x0004;
        pub(super) const O_NOFOLLOW: c_int = 0x0100;
        pub(super) const O_DIRECTORY: c_int = 0x0010_0000;
        pub(super) const O_CLOEXEC: c_int = 0x0100_0000;
        pub(super) const AT_SYMLINK_NOFOLLOW: c_int = 0x0020;
        pub(super) const S_IFMT: u16 = 0o170000;
        pub(super) const S_IFDIR: u16 = 0o040000;
        pub(super) const S_IFREG: u16 = 0o100000;

        #[repr(C)]
        pub(super) struct Stat {
            pub st_dev: i32,
            pub st_mode: u16,
            pub st_nlink: u16,
            pub st_ino: u64,
            pub st_uid: u32,
            pub st_gid: u32,
            pub st_rdev: i32,
            pub st_atime: c_long,
            pub st_atime_nsec: c_long,
            pub st_mtime: c_long,
            pub st_mtime_nsec: c_long,
            pub st_ctime: c_long,
            pub st_ctime_nsec: c_long,
            pub st_birthtime: c_long,
            pub st_birthtime_nsec: c_long,
            pub st_size: i64,
            pub st_blocks: i64,
            pub st_blksize: i32,
            pub st_flags: u32,
            pub st_gen: u32,
            pub st_lspare: i32,
            pub st_qspare: [i64; 2],
        }

        #[repr(C)]
        pub(super) struct Dirent {
            pub d_ino: u64,
            pub d_seekoff: u64,
            pub d_reclen: u16,
            pub d_namlen: u16,
            pub d_type: u8,
            pub d_name: [c_char; 1024],
        }

        pub(super) type Dir = c_void;

        extern "C" {
            pub(super) fn openat(dirfd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
            #[cfg_attr(target_arch = "x86_64", link_name = "fstatat$INODE64")]
            pub(super) fn fstatat(
                dirfd: c_int,
                path: *const c_char,
                stat: *mut Stat,
                flags: c_int,
            ) -> c_int;
            #[cfg_attr(target_arch = "x86_64", link_name = "fdopendir$INODE64")]
            pub(super) fn fdopendir(fd: c_int) -> *mut Dir;
            #[cfg_attr(target_arch = "x86_64", link_name = "readdir$INODE64")]
            pub(super) fn readdir(dir: *mut Dir) -> *mut Dirent;
            pub(super) fn closedir(dir: *mut Dir) -> c_int;
            fn __error() -> *mut c_int;
        }

        pub(super) unsafe fn errno() -> *mut c_int {
            unsafe { __error() }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Directory,
        File,
        Other,
    }

    struct Entry {
        kind: Kind,
        device: u64,
        inode: u64,
    }

    fn component(name: &OsStr) -> Result<CString, ()> {
        let mut parts = Path::new(name).components();
        if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
            return Err(());
        }
        CString::new(name.as_bytes()).map_err(|_| ())
    }

    fn inspect(parent: &std::fs::File, name: &OsStr) -> Result<Entry, ()> {
        let name = component(name)?;
        let mut stat = std::mem::MaybeUninit::<sys::Stat>::uninit();
        // SAFETY: parent is an open directory, name is NUL-terminated, and
        // fstatat initializes stat on success without following the entry.
        if unsafe {
            sys::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                sys::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(());
        }
        // SAFETY: a successful fstatat initialized the complete structure.
        let stat = unsafe { stat.assume_init() };
        let kind = match stat.st_mode & sys::S_IFMT {
            sys::S_IFDIR => Kind::Directory,
            sys::S_IFREG => Kind::File,
            _ => Kind::Other,
        };
        // Darwin and some Linux ABIs use narrower or signed dev_t/ino_t.
        #[allow(clippy::unnecessary_cast)]
        let (device, inode) = (stat.st_dev as u64, stat.st_ino as u64);
        Ok(Entry {
            kind,
            device,
            inode,
        })
    }

    fn open_child(
        parent: &std::fs::File,
        name: &OsStr,
        directory: bool,
    ) -> Result<std::fs::File, ()> {
        let name = component(name)?;
        let mut flags = sys::O_RDONLY | sys::O_CLOEXEC | sys::O_NOFOLLOW | sys::O_NONBLOCK;
        if directory {
            flags |= sys::O_DIRECTORY;
        }
        // SAFETY: parent and name remain valid for the call; no creation flag
        // is supplied, and a successful descriptor is uniquely owned below.
        let fd = unsafe { sys::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(());
        }
        // SAFETY: openat returned a newly owned descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    fn open_parent(path: &Path) -> Result<(std::fs::File, OsString), ()> {
        let mut parts = path.components();
        if !matches!(parts.next(), Some(Component::RootDir)) {
            return Err(());
        }
        let mut directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(sys::O_DIRECTORY | sys::O_NOFOLLOW | sys::O_CLOEXEC)
            .open("/")
            .map_err(|_| ())?;
        let mut parts: Vec<_> = parts.collect();
        let leaf = match parts.pop() {
            Some(Component::Normal(name)) => name.to_os_string(),
            _ => return Err(()),
        };
        for part in parts {
            let Component::Normal(name) = part else {
                return Err(());
            };
            directory = open_child(&directory, name, true)?;
        }
        Ok((directory, leaf))
    }

    struct DirectoryStream(*mut sys::Dir);

    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: fdopendir transferred ownership to this stream, and it
            // is closed exactly once after the last readdir call.
            unsafe { sys::closedir(self.0) };
        }
    }

    fn children(
        directory: &std::fs::File,
        remaining: usize,
        cancelled: &AtomicBool,
    ) -> Result<Vec<PathBuf>, ()> {
        let duplicate = directory.try_clone().map_err(|_| ())?;
        // SAFETY: duplicate is an open directory descriptor. fdopendir takes
        // ownership only on success, at which point it is detached from File.
        let stream = unsafe { sys::fdopendir(duplicate.as_raw_fd()) };
        if stream.is_null() {
            return Err(());
        }
        let _ = duplicate.into_raw_fd();
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(());
            }
            // SAFETY: errno is thread-local and the stream is used exclusively
            // here. Clearing errno distinguishes end-of-directory from error.
            let entry = unsafe {
                *sys::errno() = 0;
                sys::readdir(stream.0)
            };
            if entry.is_null() {
                // SAFETY: errno is a valid thread-local pointer.
                if unsafe { *sys::errno() } != 0 {
                    return Err(());
                }
                break;
            }
            // SAFETY: readdir returns a live dirent whose d_name is terminated;
            // copy it before the next call invalidates the entry.
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if names.len() >= remaining {
                return Err(());
            }
            names.push(PathBuf::from(OsString::from_vec(bytes.to_vec())));
        }
        collect_children(names.into_iter().map(Ok), remaining)
    }

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Stage {
        ParentOpened,
        EntryChecked,
        DirectoryOpened,
    }

    #[cfg(test)]
    type BindingHook<'a> = &'a mut dyn FnMut(Stage, &Path);

    struct Binder<'a> {
        out: Vec<OfferedFile>,
        resources: FileResources,
        open_files: usize,
        cancelled: &'a AtomicBool,
        #[cfg(test)]
        hook: Option<BindingHook<'a>>,
    }

    impl Binder<'_> {
        #[cfg(test)]
        fn observe(&mut self, stage: Stage, path: &Path) {
            if let Some(hook) = self.hook.as_mut() {
                hook(stage, path);
            }
        }

        fn visit(
            &mut self,
            parent: &std::fs::File,
            name: &OsStr,
            path: &Path,
            base: &Path,
            depth: u32,
        ) -> Result<(), BindError> {
            if self.cancelled.load(Ordering::Acquire)
                || self.out.len() >= MAX_CLIPBOARD_FILES
                || depth > MAX_DEPTH
            {
                return Err(BindError::Unavailable);
            }
            let relative = relative_name(path, base).ok_or(())?;
            let entry = inspect(parent, name)?;
            if entry.kind == Kind::Other {
                return Ok(());
            }
            #[cfg(test)]
            self.observe(Stage::EntryChecked, path);
            if entry.kind == Kind::File && self.open_files >= MAX_BOUND_FILES {
                return Err(BindError::SelectionLimit);
            }
            let file = self
                .resources
                .open(|| open_child(parent, name, entry.kind == Kind::Directory))?;
            let handle = file.file.as_ref().ok_or(())?;
            let meta = handle.metadata().map_err(|_| ())?;
            if (meta.dev(), meta.ino()) != (entry.device, entry.inode)
                || (entry.kind == Kind::Directory && !meta.is_dir())
                || (entry.kind == Kind::File && !meta.is_file())
            {
                return Err(BindError::Unavailable);
            }
            if entry.kind == Kind::Directory {
                #[cfg(test)]
                self.observe(Stage::DirectoryOpened, path);
                self.out.push(OfferedFile {
                    path: path.to_path_buf(),
                    meta: ClipFile {
                        name: relative,
                        size: 0,
                        is_dir: true,
                    },
                    file: None,
                });
                for child in children(handle, MAX_CLIPBOARD_FILES - self.out.len(), self.cancelled)?
                {
                    self.visit(
                        handle,
                        child.as_os_str(),
                        &path.join(&child),
                        base,
                        depth + 1,
                    )?;
                }
            } else {
                self.open_files += 1;
                self.out.push(OfferedFile {
                    path: path.to_path_buf(),
                    meta: ClipFile {
                        name: relative,
                        size: meta.len(),
                        is_dir: false,
                    },
                    file: Some(Arc::new(Mutex::new(file))),
                });
            }
            Ok(())
        }
    }

    // The shared lifetime is needed by the test hook, which is absent in production.
    #[cfg_attr(not(test), allow(clippy::needless_lifetimes))]
    fn expand_inner<'a>(
        roots: &[PathBuf],
        cancelled: &'a AtomicBool,
        resources: &FileResources,
        open_files: usize,
        #[cfg(test)] hook: Option<BindingHook<'a>>,
    ) -> Result<Vec<OfferedFile>, BindError> {
        let mut binder = Binder {
            out: Vec::new(),
            resources: resources.clone(),
            open_files,
            cancelled,
            #[cfg(test)]
            hook,
        };
        for root in roots {
            let base = root.parent().ok_or(())?;
            let (parent, name) = open_parent(root)?;
            #[cfg(test)]
            binder.observe(Stage::ParentOpened, root);
            binder.visit(&parent, &name, root, base, 0)?;
        }
        Ok(binder.out)
    }

    pub(super) fn expand(
        roots: &[PathBuf],
        cancelled: &AtomicBool,
        resources: &FileResources,
    ) -> Result<Vec<OfferedFile>, BindError> {
        expand_inner(
            roots,
            cancelled,
            resources,
            0,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(super) fn expand_with_hook(
        roots: &[PathBuf],
        cancelled: &AtomicBool,
        hook: &mut dyn FnMut(Stage, &Path),
    ) -> Result<Vec<OfferedFile>, BindError> {
        let resources = file_resources().ok_or(())?;
        expand_inner(roots, cancelled, &resources, 0, Some(hook))
    }

    #[cfg(test)]
    pub(super) fn expand_near_file_limit(
        roots: &[PathBuf],
        cancelled: &AtomicBool,
        resources: &FileResources,
    ) -> Result<Vec<OfferedFile>, BindError> {
        expand_inner(roots, cancelled, resources, MAX_BOUND_FILES - 1, None)
    }
}

fn collect_children(
    entries: impl Iterator<Item = std::io::Result<PathBuf>>,
    remaining: usize,
) -> Result<Vec<PathBuf>, ()> {
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        if children.len() >= remaining {
            return Err(());
        }
        children.push(entry);
    }
    children.sort();
    Ok(children)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn push_entry(
    out: &mut Vec<OfferedFile>,
    path: &Path,
    base: &Path,
    depth: u32,
    resources: &FileResources,
    open_files: &mut usize,
    cancelled: &AtomicBool,
) -> Result<(), BindError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(BindError::Unavailable);
    }
    if out.len() >= MAX_CLIPBOARD_FILES || depth > MAX_DEPTH {
        return Err(BindError::Unavailable);
    }
    let name = relative_name(path, base).ok_or(())?;
    // Inspect the entry itself so a selected symlink is never followed or
    // recursed into. The eventual file handle protects delayed reads from later
    // changes to this path and its ancestors.
    let meta = std::fs::symlink_metadata(path).map_err(|_| ())?;

    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        out.push(OfferedFile {
            path: path.to_path_buf(),
            meta: ClipFile {
                name,
                size: 0,
                is_dir: true,
            },
            file: None,
        });
        let entries = std::fs::read_dir(path).map_err(|_| ())?;
        // Sorted, so copying the same folder twice produces the same indices.
        // `read_dir` order is filesystem-defined, and the index is what a
        // `FileContentsRequest` refers to.
        let children = collect_children(
            entries.map(|entry| entry.map(|entry| entry.path())),
            MAX_CLIPBOARD_FILES - out.len(),
        )?;
        for child in children {
            push_entry(
                out,
                &child,
                base,
                depth + 1,
                resources,
                open_files,
                cancelled,
            )?;
        }
    } else if meta.is_file() {
        if *open_files >= MAX_BOUND_FILES {
            return Err(BindError::SelectionLimit);
        }
        let file = resources.open(|| {
            open_regular_file(path, &meta)
                .map(|(file, _)| file)
                .ok_or(())
        })?;
        let opened_meta = file.file.as_ref().ok_or(())?.metadata().map_err(|_| ())?;
        *open_files += 1;
        out.push(OfferedFile {
            path: path.to_path_buf(),
            meta: ClipFile {
                name,
                size: opened_meta.len(),
                is_dir: false,
            },
            file: Some(Arc::new(Mutex::new(file))),
        });
    }
    // Anything else (a symlink, a device) is skipped rather than guessed at.
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open_regular_file(
    _path: &Path,
    _before: &std::fs::Metadata,
) -> Option<(std::fs::File, std::fs::Metadata)> {
    None
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
    adit_session::validate_clipboard_relative_name(&name).ok()?;
    Some(name)
}

type FileWork = Box<dyn FnOnce() + Send + 'static>;

/// A bounded queue keeps a remote peer from creating an unbounded number of
/// blocking filesystem threads. No filesystem operation runs on the UI thread.
fn file_worker() -> Option<&'static mpsc::SyncSender<FileWork>> {
    static WORKER: OnceLock<Option<mpsc::SyncSender<FileWork>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel::<FileWork>(64);
            let rx = Arc::new(Mutex::new(rx));
            let mut started = 0;
            for index in 0..4 {
                let rx = rx.clone();
                if std::thread::Builder::new()
                    .name(format!("adit-clipboard-file-{index}"))
                    .spawn(move || loop {
                        let work = match rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => break,
                        };
                        let Ok(work) = work else { break };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                    })
                    .is_ok()
                {
                    started += 1;
                }
            }
            (started > 0).then_some(tx)
        })
        .as_ref()
}

fn try_file_work(work: FileWork) -> Result<(), FileWork> {
    let Some(worker) = file_worker() else {
        return Err(work);
    };
    worker.try_send(work).map_err(|error| match error {
        mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work) => work,
    })
}

struct PendingOffer {
    cancelled: Arc<AtomicBool>,
    roots: Vec<PathBuf>,
    result: mpsc::Receiver<Result<Vec<OfferedFile>, BindError>>,
    completed: Option<Result<Vec<OfferedFile>, BindError>>,
}

impl Drop for PendingOffer {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct TrackedOffer {
    sequence: u32,
    pending: Option<PendingOffer>,
    bound: Option<(Vec<PathBuf>, Vec<OfferedFile>)>,
}

#[derive(Default)]
struct OfferTracker {
    sessions: HashMap<SessionId, TrackedOffer>,
    disabled_notices: HashMap<SessionId, (u32, u64)>,
    oversized_text_notices: HashMap<SessionId, u32>,
    text_offer_invalidations: HashSet<SessionId>,
    image_offer_invalidations: HashSet<SessionId>,
    text_reads: HashMap<SessionId, (u64, u32)>,
    observed_sequence: Option<u32>,
    pending_remote_text_write: Option<SessionId>,
    remote_text_write: Option<(SessionId, u32)>,
    remote_image_write: Option<(SessionId, u32)>,
    desktop_cleanup_pending: bool,
    failed_binding: Option<(u32, BindError)>,
    // A worker may have opened files even after its session or result is gone.
    // Keep that generation consumed until a different nonzero one is observed.
    used_sequence: Option<u32>,
}

static OFFER_TRACKER: OnceLock<Mutex<OfferTracker>> = OnceLock::new();

pub(crate) enum OfferPoll {
    Started,
    Pending,
    Unchanged,
    Ready(Vec<OfferedFile>),
    Disabled {
        session: SessionId,
        epoch: u64,
    },
    Failed {
        session: SessionId,
        epoch: u64,
        error: BindError,
    },
}

impl OfferPoll {
    fn failed(session: SessionId, epoch: u64, error: BindError) -> Self {
        Self::Failed {
            session,
            epoch,
            error,
        }
    }
}

impl OfferTracker {
    fn record_file_selection_marker(
        &mut self,
        session: SessionId,
        sequence: u32,
        epoch: u64,
        live_sessions: &[SessionId],
    ) {
        if let Some((notice_sequence, notice_epoch)) = self.disabled_notices.get_mut(&session) {
            if *notice_sequence == sequence {
                *notice_epoch = epoch;
            }
        }
        self.invalidate_content_offers(live_sessions);
        self.text_reads.clear();
    }

    fn invalidate_content_offers(&mut self, sessions: &[SessionId]) {
        self.text_offer_invalidations
            .extend(sessions.iter().copied());
        self.image_offer_invalidations
            .extend(sessions.iter().copied());
    }

    fn confirm_sequence(&mut self, sequence: u32) {
        if sequence == 0 {
            return;
        }
        if self.used_sequence.is_some_and(|used| used != sequence) {
            self.used_sequence = None;
        }
        if self
            .failed_binding
            .is_some_and(|(failed, _)| failed != sequence)
        {
            self.failed_binding = None;
        }
    }

    fn observe_sequence(&mut self, sequence: u32) -> bool {
        self.confirm_sequence(sequence);
        let previous = self.observed_sequence.replace(sequence);
        let changed = previous.is_some_and(|previous| previous != sequence);
        if previous != Some(sequence) {
            // Linux reports 0 while its watcher classifies the new selection.
            // Keep the remote-write owner until a concrete sequence is published.
            self.remote_text_write = if sequence == 0 {
                None
            } else {
                self.pending_remote_text_write
                    .take()
                    .map(|session| (session, sequence))
            };
            if sequence != 0
                && self
                    .remote_image_write
                    .is_some_and(|(_, image_sequence)| image_sequence != sequence)
            {
                self.remote_image_write = None;
            }
        }
        if changed {
            self.text_reads.clear();
            self.oversized_text_notices.clear();
        }
        self.desktop_cleanup_pending |= changed;
        changed
    }

    fn take_desktop_cleanup(&mut self) -> bool {
        let changed = std::mem::take(&mut self.desktop_cleanup_pending);
        if changed {
            self.sessions.clear();
            // A focused poll can observe and report the new selection before
            // native windows finish retiring the previous sequence.
            self.disabled_notices
                .retain(|_, (sequence, _)| Some(*sequence) == self.observed_sequence);
        }
        changed
    }

    fn retain_live(&mut self, mut is_live: impl FnMut(SessionId) -> bool) {
        self.sessions.retain(|id, _| is_live(*id));
        self.disabled_notices.retain(|id, _| is_live(*id));
    }

    fn retain_text_offer_invalidations(&mut self, mut is_present: impl FnMut(SessionId) -> bool) {
        self.text_offer_invalidations.retain(|id| is_present(*id));
        self.image_offer_invalidations.retain(|id| is_present(*id));
        self.text_reads.retain(|id, _| is_present(*id));
        self.oversized_text_notices.retain(|id, _| is_present(*id));
    }

    fn cancel(&mut self) {
        self.sessions.clear();
        self.disabled_notices.clear();
        self.oversized_text_notices.clear();
        self.text_reads.clear();
        self.pending_remote_text_write = None;
        self.remote_text_write = None;
        self.remote_image_write = None;
        // Delayed reads must not use an observation from before sharing was disabled.
        self.observed_sequence = None;
    }

    fn poll_observing(
        &mut self,
        session: SessionId,
        epoch: u64,
        sequence: u32,
        paths: Option<Vec<PathBuf>>,
    ) -> (OfferPoll, bool) {
        let superseded = self.observe_sequence(sequence);
        // Do not bind a new selection before every window retires its old one.
        if self.desktop_cleanup_pending {
            return (OfferPoll::Pending, superseded);
        }
        (self.poll(session, epoch, sequence, paths), superseded)
    }

    fn poll_with_policy(
        &mut self,
        session: SessionId,
        epoch: u64,
        sequence: u32,
        paths: Option<Vec<PathBuf>>,
        enabled: bool,
    ) -> (OfferPoll, bool) {
        if enabled {
            return self.poll_observing(session, epoch, sequence, paths);
        }
        let superseded = self.observe_sequence(sequence);
        if self.desktop_cleanup_pending && paths.is_none() {
            return (OfferPoll::Pending, superseded);
        }

        // No old worker or handles may survive the policy. Keep used_sequence:
        // a consumed copy must not be rebound if the policy is later lifted.
        self.sessions.clear();
        // While disabled, Some(empty) is a format-only file selection: no
        // paths were retrieved and none can be sent to a binding worker.
        if paths.is_none() {
            return (OfferPoll::Started, superseded);
        }
        let previous = self.disabled_notices.insert(session, (sequence, epoch));
        let result = if previous == Some((sequence, epoch)) {
            OfferPoll::Unchanged
        } else {
            OfferPoll::Disabled { session, epoch }
        };
        (result, superseded)
    }

    fn poll(
        &mut self,
        session: SessionId,
        epoch: u64,
        sequence: u32,
        paths: Option<Vec<PathBuf>>,
    ) -> OfferPoll {
        if sequence == 0 {
            self.sessions.clear();
            return OfferPoll::Started;
        }
        self.confirm_sequence(sequence);
        // A finished inactive binding must retain its original handles: opening
        // the same paths on focus return could expose a replacement file.
        for (id, tracked) in &mut self.sessions {
            if *id == session {
                continue;
            }
            if tracked.sequence != sequence {
                tracked.pending = None;
                tracked.bound = None;
            } else if let Some(pending) = &mut tracked.pending {
                if pending.completed.is_none() {
                    pending.completed = match pending.result.try_recv() {
                        Ok(result) => Some(result),
                        Err(mpsc::TryRecvError::Disconnected) => Some(Err(BindError::Unavailable)),
                        Err(mpsc::TryRecvError::Empty) => None,
                    };
                }
            }
        }
        if let Some(tracked) = self.sessions.get_mut(&session) {
            if tracked.sequence == sequence {
                let Some(pending) = &mut tracked.pending else {
                    return OfferPoll::Unchanged;
                };
                if paths.is_none() {
                    // A busy clipboard cannot confirm that this is still the
                    // file selection whose handles the worker opened.
                    return OfferPoll::Pending;
                }
                if paths.as_ref() != Some(&pending.roots) {
                    tracked.pending = None;
                    return OfferPoll::failed(session, epoch, BindError::Unavailable);
                }
                let result = match pending.completed.take() {
                    Some(result) => Ok(result),
                    None => pending.result.try_recv(),
                };
                return match result {
                    Ok(Ok(files)) => {
                        tracked.bound = Some((pending.roots.clone(), files.clone()));
                        tracked.pending = None;
                        OfferPoll::Ready(files)
                    }
                    Ok(Err(error)) => {
                        self.failed_binding = Some((sequence, error));
                        tracked.pending = None;
                        // The same session can regain focus with a new epoch.
                        // Its sequence and roots were just revalidated above;
                        // tag this delivery with the current poll's epoch.
                        OfferPoll::failed(session, epoch, error)
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        tracked.pending = None;
                        OfferPoll::failed(session, epoch, BindError::Unavailable)
                    }
                    Err(mpsc::TryRecvError::Empty) => OfferPoll::Pending,
                };
            }
        }
        // Dropping the old receiver also cancels work captured for the old
        // clipboard generation, even if it finishes after this one.
        self.sessions.remove(&session);
        let Some(paths) = paths else {
            // The clipboard can be temporarily busy, so None is not proof that
            // this sequence contains no files. Revoke now and retry next poll.
            return OfferPoll::Started;
        };
        // The OS sequence describes one copy operation, even when another RDP
        // session comes into focus. Share its already-bound handles (or wait for
        // its worker), never reopen the same paths for a second session.
        let mut shared = None;
        let mut waiting = false;
        let mut failed = self
            .failed_binding
            .filter(|(failed_sequence, _)| *failed_sequence == sequence)
            .map(|(_, error)| error);
        for tracked in self
            .sessions
            .values()
            .filter(|offer| offer.sequence == sequence)
        {
            if let Some(pending) = &tracked.pending {
                if pending.roots != paths {
                    failed.get_or_insert(BindError::Unavailable);
                } else {
                    match &pending.completed {
                        Some(Ok(files)) => shared = Some(files.clone()),
                        Some(Err(error)) => {
                            if *error != BindError::Unavailable || failed.is_none() {
                                failed = Some(*error);
                            }
                        }
                        None => waiting = true,
                    }
                }
            } else if let Some((roots, files)) = &tracked.bound {
                if *roots == paths {
                    shared = Some(files.clone());
                } else {
                    failed.get_or_insert(BindError::Unavailable);
                }
            } else {
                failed.get_or_insert(BindError::Unavailable);
            }
        }
        // Reuse live handles first, but never reopen a consumed generation once
        // its last pending or bound session has been retired.
        if shared.is_none() && !waiting && self.used_sequence == Some(sequence) {
            failed.get_or_insert(BindError::Unavailable);
        }
        if let Some(error) = failed {
            self.failed_binding = Some((sequence, error));
            self.sessions.insert(
                session,
                TrackedOffer {
                    sequence,
                    pending: None,
                    bound: None,
                },
            );
            return OfferPoll::failed(session, epoch, error);
        }
        if let Some(files) = shared {
            self.sessions.insert(
                session,
                TrackedOffer {
                    sequence,
                    pending: None,
                    bound: Some((paths, files.clone())),
                },
            );
            return OfferPoll::Ready(files);
        }
        if waiting {
            return OfferPoll::Started;
        }
        if self
            .sessions
            .values()
            .filter(|entry| {
                entry
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.completed.is_none())
            })
            .count()
            >= MAX_PENDING_OFFERS
        {
            return OfferPoll::Started;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let roots = paths.clone();
        let (tx, rx) = mpsc::channel();
        let work: FileWork = Box::new(move || {
            let result = expand(&paths, &worker_cancelled);
            if !worker_cancelled.load(Ordering::Acquire) {
                let _ = tx.send(result);
            }
        });
        if try_file_work(work).is_err() {
            return OfferPoll::Started;
        }
        self.used_sequence = Some(sequence);
        self.sessions.insert(
            session,
            TrackedOffer {
                sequence,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled,
                    roots,
                    result: rx,
                    completed: None,
                }),
            },
        );
        OfferPoll::Started
    }
}

fn offer_tracker() -> &'static Mutex<OfferTracker> {
    OFFER_TRACKER.get_or_init(|| Mutex::new(OfferTracker::default()))
}

/// Record a sample without consuming the cleanup owed to other windows.
pub(crate) fn track_local_offer_sequence(sequence: u32) -> bool {
    offer_tracker()
        .lock()
        .map_or(true, |mut tracker| tracker.observe_sequence(sequence))
}

/// Check the sequence without opening the system clipboard, even while unfocused.
pub(crate) fn observe_local_offer_sequence(sequence: u32) -> bool {
    let Ok(mut tracker) = offer_tracker().lock() else {
        return true;
    };
    tracker.observe_sequence(sequence);
    tracker.take_desktop_cleanup()
}

pub(crate) fn poll_local_offer(
    session: SessionId,
    epoch: u64,
    sequence: u32,
    paths: Option<Vec<PathBuf>>,
    live_sessions: &[SessionId],
) -> (OfferPoll, bool) {
    let Ok(mut tracker) = offer_tracker().lock() else {
        let result = if !LOCAL_FILE_SEND_ENABLED {
            if paths.is_some() {
                OfferPoll::Disabled { session, epoch }
            } else {
                OfferPoll::Started
            }
        } else {
            OfferPoll::failed(session, epoch, BindError::Unavailable)
        };
        return (result, true);
    };
    tracker.retain_live(|id| live_sessions.contains(&id));
    tracker.poll_with_policy(session, epoch, sequence, paths, LOCAL_FILE_SEND_ENABLED)
}

pub(crate) fn local_file_send_enabled() -> bool {
    LOCAL_FILE_SEND_ENABLED
}

/// A file-selection notice belongs to its session, not the process-wide clipboard.
pub(crate) fn local_file_send_disabled_notice(session: SessionId) -> Option<&'static str> {
    OFFER_TRACKER
        .get()
        .and_then(|tracker| tracker.lock().ok())
        .filter(|tracker| {
            tracker
                .disabled_notices
                .get(&session)
                .is_some_and(|(sequence, _)| Some(*sequence) == tracker.observed_sequence)
        })
        .map(|_| LOCAL_FILE_SEND_DISABLED_NOTICE)
}

/// A file copy supersedes each helper's last text offer too. Keep this across
/// clipboard generations until the owning session next samples text.
pub(crate) fn record_file_selection_marker(
    session: SessionId,
    sequence: u32,
    epoch: u64,
    live_sessions: &[SessionId],
) {
    if let Ok(mut tracker) = offer_tracker().lock() {
        tracker.record_file_selection_marker(session, sequence, epoch, live_sessions);
    }
}

pub(crate) fn take_text_offer_invalidation(session: SessionId) -> bool {
    offer_tracker()
        .lock()
        .is_ok_and(|mut tracker| tracker.text_offer_invalidations.remove(&session))
}

pub(crate) fn record_remote_text_write(session: SessionId) {
    if let Ok(mut tracker) = offer_tracker().lock() {
        tracker.pending_remote_text_write = Some(session);
    }
}

pub(crate) fn take_remote_text_write(session: SessionId, sequence: u32) -> bool {
    offer_tracker().lock().is_ok_and(|mut tracker| {
        if tracker.remote_text_write == Some((session, sequence)) {
            tracker.remote_text_write = None;
            true
        } else {
            false
        }
    })
}

pub(crate) fn record_remote_image_write(session: SessionId, sequence: u32) {
    if let Ok(mut tracker) = offer_tracker().lock() {
        tracker.remote_image_write = (sequence != 0).then_some((session, sequence));
    }
}

pub(crate) fn is_remote_image_write(session: SessionId, sequence: u32) -> bool {
    offer_tracker()
        .lock()
        .is_ok_and(|tracker| tracker.remote_image_write == Some((session, sequence)))
}

pub(crate) fn invalidate_content_offer_caches(sessions: &[SessionId]) {
    if let Ok(mut tracker) = offer_tracker().lock() {
        tracker.invalidate_content_offers(sessions);
    }
}

pub(crate) fn take_image_offer_invalidation(session: SessionId) -> bool {
    offer_tracker()
        .lock()
        .is_ok_and(|mut tracker| tracker.image_offer_invalidations.remove(&session))
}

/// Keep an oversized-text warning attached to the session that rejected it.
pub(crate) fn record_oversized_text_notice(session: SessionId, sequence: u32) {
    if let Ok(mut tracker) = offer_tracker().lock() {
        if sequence != 0 && tracker.observed_sequence == Some(sequence) {
            tracker.oversized_text_notices.insert(session, sequence);
        }
    }
}

pub(crate) fn has_oversized_text_notice(session: SessionId) -> bool {
    offer_tracker().lock().is_ok_and(|tracker| {
        tracker
            .oversized_text_notices
            .get(&session)
            .is_some_and(|sequence| Some(*sequence) == tracker.observed_sequence)
    })
}

pub(crate) fn clear_oversized_text_notice(session: SessionId) {
    if let Ok(mut tracker) = offer_tracker().lock() {
        tracker.oversized_text_notices.remove(&session);
    }
}

/// Associate each asynchronous RDP text read with its classified generation.
/// A later read replaces the authorization rather than accepting an old result.
pub(crate) fn authorize_text_read(session: SessionId, epoch: u64, sequence: u32) -> bool {
    if sequence == 0 {
        return false;
    }
    offer_tracker().lock().is_ok_and(|mut tracker| {
        tracker.text_reads.insert(session, (epoch, sequence));
        true
    })
}

pub(crate) fn take_text_read_sequence(session: SessionId, epoch: u64) -> Option<u32> {
    let mut tracker = offer_tracker().lock().ok()?;
    let (authorized_epoch, sequence) = *tracker.text_reads.get(&session)?;
    if authorized_epoch != epoch {
        return None;
    }
    tracker.text_reads.remove(&session);
    Some(sequence)
}

pub(crate) fn cancel_local_offers() {
    if let Ok(mut tracker) = offer_tracker().lock() {
        tracker.cancel();
    }
}

/// Closed or disconnected sessions must not retain completed background
/// bindings merely because no desktop window remains focused to poll them.
pub(crate) fn retain_local_offers(mut is_live: impl FnMut(SessionId) -> bool) {
    if let Some(tracker) = OFFER_TRACKER.get() {
        if let Ok(mut tracker) = tracker.lock() {
            tracker.retain_live(&mut is_live);
        }
    }
}

/// Connecting sessions can receive a marker before they become live; retain
/// their invalidation until the session itself is gone, not merely unfocused.
pub(crate) fn retain_text_offer_invalidations(mut is_present: impl FnMut(SessionId) -> bool) {
    if let Some(tracker) = OFFER_TRACKER.get() {
        if let Ok(mut tracker) = tracker.lock() {
            tracker.retain_text_offer_invalidations(&mut is_present);
        }
    }
}

/// Read through the held handle, with the advertised size as an upper bound.
fn read_range(file: &OfferedFile, offset: u64, length: u32) -> Option<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    if file.meta.is_dir || length > MAX_FILE_READ_BYTES {
        return None;
    }
    let mut bound = file.file.as_ref()?.lock().ok()?;
    let file_handle = bound.file.as_mut()?;
    file_handle.seek(SeekFrom::Start(offset)).ok()?;
    let length = u64::from(length).min(file.meta.size.saturating_sub(offset));
    let mut buffer = vec![0u8; length as usize];
    let mut filled = 0usize;
    while filled < buffer.len() {
        match file_handle.read(&mut buffer[filled..]) {
            // The requested range was already clamped to the advertised size.
            // EOF here means the bound file shrank after it was offered.
            Ok(0) => return None,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(buffer)
}

type ReadReply = Box<dyn FnOnce(Option<Vec<u8>>) + Send + 'static>;

struct PendingReply(Option<ReadReply>);

impl PendingReply {
    fn answer(mut self, data: Option<Vec<u8>>) {
        if let Some(reply) = self.0.take() {
            reply(data);
        }
    }
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        if let Some(reply) = self.0.take() {
            reply(None);
        }
    }
}

fn queue_read(
    file: Option<OfferedFile>,
    offset: u64,
    length: u32,
    reply: ReadReply,
    still_current: impl FnOnce() -> bool + Send + 'static,
) {
    let reply = PendingReply(Some(reply));
    let Some(file) = file else { return };
    if file.meta.is_dir || file.file.is_none() || length > MAX_FILE_READ_BYTES {
        return;
    }
    let work: FileWork = Box::new(move || {
        if !still_current() {
            return;
        }
        let data = read_range(&file, offset, length);
        reply.answer(data);
    });
    // A rejected or panicking job drops PendingReply and refuses the request.
    let _ = try_file_work(work);
}

fn queue_offered_read_guarded(
    enabled: bool,
    files: &[OfferedFile],
    index: u32,
    offset: u64,
    length: u32,
    reply: ReadReply,
    still_current: impl FnOnce() -> bool + Send + 'static,
) {
    let file = enabled
        .then(|| files.get(index as usize).cloned())
        .flatten();
    queue_read(file, offset, length, reply, still_current);
}

#[cfg(test)]
fn queue_offered_read(
    enabled: bool,
    files: &[OfferedFile],
    index: u32,
    offset: u64,
    length: u32,
    reply: ReadReply,
) {
    queue_offered_read_guarded(enabled, files, index, offset, length, reply, || true);
}

/// Move the request and its original session lease to the filesystem worker.
/// `RdpFileRead::answer` rechecks the lease after the read, including revocation.
#[cfg(windows)]
fn clipboard_sequence_matches(expected: Option<u32>, observed: u32) -> bool {
    matches!(expected, Some(value) if value != 0 && value == observed)
}

pub(crate) fn answer_offered_read(enabled: bool, files: &[OfferedFile], read: RdpFileRead) {
    if !enabled || !LOCAL_FILE_SEND_ENABLED {
        read.answer(None);
        return;
    }
    let (index, offset, length) = (read.index, read.offset, read.length);
    let still_current = read.current_guard();
    #[cfg(windows)]
    let sequence = offer_tracker()
        .lock()
        .ok()
        .and_then(|tracker| tracker.observed_sequence);
    #[cfg(windows)]
    if !clipboard_sequence_matches(sequence, clipboard_sequence()) {
        read.answer(None);
        return;
    }
    queue_offered_read_guarded(
        enabled,
        files,
        index,
        offset,
        length,
        Box::new(move |data| {
            #[cfg(windows)]
            let data = if clipboard_sequence_matches(sequence, clipboard_sequence()) {
                data
            } else {
                None
            };
            read.answer(data);
        }),
        still_current,
    );
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
pub(crate) fn clipboard_image(sequence: u32) -> Option<Vec<u8>> {
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
        let bytes = verified_non_file_snapshot(
            sequence,
            clipboard_sequence,
            clipboard_file_selection,
            || {
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
            },
        );
        let _ = CloseClipboard();
        bytes
    }
}

#[cfg(not(windows))]
pub(crate) fn clipboard_image(_sequence: u32) -> Option<Vec<u8>> {
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

/// Commit a bound remote text while its session and policy lease is held.
#[cfg(windows)]
pub(crate) fn set_clipboard_text(text: &str) -> bool {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let size = text.len().saturating_mul(std::mem::size_of::<u16>());
    unsafe {
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, size) else {
            return false;
        };
        let data = GlobalLock(global);
        if data.is_null() {
            let _ = GlobalFree(Some(global));
            return false;
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), data.cast::<u16>(), text.len());
        let _ = GlobalUnlock(global);
        if OpenClipboard(None).is_err() {
            let _ = GlobalFree(Some(global));
            return false;
        }
        let written =
            EmptyClipboard().is_ok() && SetClipboardData(13, Some(HANDLE(global.0))).is_ok();
        let _ = CloseClipboard();
        if !written {
            let _ = GlobalFree(Some(global));
        }
        written
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn set_clipboard_text(text: &str) -> bool {
    use clipboard_rs::{Clipboard, ClipboardContext};
    thread_local! {
        // X11 selection ownership must outlive this write.
        static TEXT_CLIPBOARD: std::cell::RefCell<Option<ClipboardContext>> = const { std::cell::RefCell::new(None) };
    }
    TEXT_CLIPBOARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = ClipboardContext::new().ok();
        }
        slot.as_ref()
            .is_some_and(|clipboard| clipboard.set_text(text.to_owned()).is_ok())
    })
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) fn set_clipboard_text(_text: &str) -> bool {
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardFileSelection {
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    NonFile,
    LocalOrUnknown,
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    RemoteOwned,
}

/// The caller holds the Windows clipboard open while checking formats and
/// reading data. Recheck after delayed rendering as it can change the sequence.
#[cfg(any(windows, test))]
pub(crate) fn verified_non_file_snapshot<T>(
    expected: u32,
    sequence: impl Fn() -> u32,
    selection: impl Fn() -> ClipboardFileSelection,
    read: impl FnOnce() -> Option<T>,
) -> Option<T> {
    if expected == 0 || sequence() != expected || selection() != ClipboardFileSelection::NonFile {
        return None;
    }
    let result = read()?;
    if sequence() == expected && selection() == ClipboardFileSelection::NonFile {
        Some(result)
    } else {
        None
    }
}

/// Read ordinary text while the same Windows clipboard remains open for both
/// classification and data access. A delayed virtual-file representation must
/// never be read through an earlier NonFile observation.
#[cfg(windows)]
pub(crate) fn clipboard_text_snapshot(sequence: u32) -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;
    // SAFETY: the clipboard remains open until the copied data and all format
    // checks are complete; no handle or borrowed data escapes after closing.
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let text = verified_non_file_snapshot(
            sequence,
            clipboard_sequence,
            clipboard_file_selection,
            || {
                if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
                    return None;
                }
                let global = HGLOBAL(GetClipboardData(CF_UNICODETEXT).ok()?.0);
                let size = GlobalSize(global);
                if !(2..=isize::MAX as usize).contains(&size) {
                    return None;
                }
                let base = GlobalLock(global);
                if base.is_null() {
                    return None;
                }
                let units = std::slice::from_raw_parts(base.cast::<u16>(), size / 2);
                let result = units
                    .iter()
                    .position(|unit| *unit == 0)
                    .map(|end| String::from_utf16_lossy(&units[..end]));
                let _ = GlobalUnlock(global);
                result
            },
        );
        let _ = CloseClipboard();
        text
    }
}

/// A completed snapshot can wait in the UI queue while another copy happens.
#[cfg(any(windows, target_os = "linux"))]
pub(crate) fn observed_non_file_sample_current(
    sequence: u32,
    current_sequence: impl Fn() -> u32,
    selection: impl Fn() -> ClipboardFileSelection,
) -> bool {
    sequence != 0
        && current_sequence() == sequence
        && selection() == ClipboardFileSelection::NonFile
        && current_sequence() == sequence
}

#[cfg(any(windows, target_os = "linux"))]
pub(crate) fn text_sample_still_current(sequence: u32) -> bool {
    // On Linux this checks only what the asynchronous X11 watcher has seen;
    // a selection change whose notification is still pending can pass.
    observed_non_file_sample_current(sequence, clipboard_sequence, clipboard_file_selection)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn text_sample_still_current(_sequence: u32) -> bool {
    false
}

// All Windows file representations must be considered together: virtual
// files need not advertise CF_HDROP at all.
#[cfg(any(windows, test))]
pub(crate) fn classify_windows_file_selection(
    formats_registered: bool,
    hdrop: bool,
    descriptor: bool,
    contents: bool,
    remote_owned: bool,
) -> ClipboardFileSelection {
    if !formats_registered {
        ClipboardFileSelection::LocalOrUnknown
    } else if !hdrop && (descriptor || contents) && remote_owned {
        ClipboardFileSelection::RemoteOwned
    } else if hdrop || descriptor || contents {
        ClipboardFileSelection::LocalOrUnknown
    } else {
        ClipboardFileSelection::NonFile
    }
}

/// Classify the clipboard without opening selected files.
#[cfg(windows)]
pub(crate) fn clipboard_file_selection() -> ClipboardFileSelection {
    use windows::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };

    const CF_HDROP: u32 = 15;
    // SAFETY: registration uses static UTF-16 strings; format queries do not
    // open the clipboard or inspect the selected objects.
    unsafe {
        let descriptor = RegisterClipboardFormatW(windows::core::w!("FileGroupDescriptorW"));
        let descriptor_ansi = RegisterClipboardFormatW(windows::core::w!("FileGroupDescriptor"));
        let contents = RegisterClipboardFormatW(windows::core::w!("FileContents"));
        let filename = RegisterClipboardFormatW(windows::core::w!("FileName"));
        let filename_wide = RegisterClipboardFormatW(windows::core::w!("FileNameW"));
        let shell_id_list = RegisterClipboardFormatW(windows::core::w!("Shell IDList Array"));
        let formats = [
            descriptor,
            descriptor_ansi,
            contents,
            filename,
            filename_wide,
            shell_id_list,
        ];
        let sequence = clipboard_sequence();
        classify_windows_file_selection(
            formats.iter().all(|format| *format != 0),
            IsClipboardFormatAvailable(CF_HDROP).is_ok(),
            [
                descriptor,
                descriptor_ansi,
                filename,
                filename_wide,
                shell_id_list,
            ]
            .into_iter()
            .any(|format| format != 0 && IsClipboardFormatAvailable(format).is_ok()),
            contents != 0 && IsClipboardFormatAvailable(contents).is_ok(),
            ole::owns_clipboard(sequence),
        )
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn clipboard_file_selection() -> ClipboardFileSelection {
    match linux::file_selection() {
        linux::FileSelection::NonFile => ClipboardFileSelection::NonFile,
        linux::FileSelection::Local | linux::FileSelection::Unknown => {
            ClipboardFileSelection::LocalOrUnknown
        }
        linux::FileSelection::RemoteOwned => ClipboardFileSelection::RemoteOwned,
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn clipboard_file_selection() -> ClipboardFileSelection {
    ClipboardFileSelection::LocalOrUnknown
}

/// Whether a zero sequence means a failed backend rather than a transient read.
#[cfg(windows)]
pub(crate) fn clipboard_selection_unavailable() -> bool {
    true
}

#[cfg(target_os = "linux")]
pub(crate) fn clipboard_selection_unavailable() -> bool {
    linux::unavailable()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn clipboard_selection_unavailable() -> bool {
    true
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
            if count as usize > MAX_CLIPBOARD_FILES {
                return None;
            }
            let mut paths = Vec::with_capacity(count as usize);
            for index in 0..count {
                // Called twice per file by design: once for the length, once for
                // the characters. The first call must pass no buffer.
                let len = DragQueryFileW(drop_handle, index, None);
                if len == 0 {
                    return None;
                }
                // +1 for the null terminator the second call writes.
                let mut buffer = vec![0u16; len as usize + 1];
                let written = DragQueryFileW(drop_handle, index, Some(&mut buffer));
                if written != len {
                    return None;
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

/// Linux reads file-URI selections on the clipboard worker.
#[cfg(target_os = "linux")]
pub(crate) fn clipboard_paths() -> Option<Vec<PathBuf>> {
    linux::paths()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn clipboard_paths() -> Option<Vec<PathBuf>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn windows_virtual_file_formats_are_never_plain_text() {
        use ClipboardFileSelection::{LocalOrUnknown, NonFile, RemoteOwned};

        for (registered, hdrop, descriptor, contents, owned, expected) in [
            (true, false, false, false, false, NonFile),
            (false, false, false, false, false, LocalOrUnknown),
            (true, true, false, false, false, LocalOrUnknown),
            (true, false, true, false, false, LocalOrUnknown),
            (true, false, false, true, false, LocalOrUnknown),
            (true, false, true, true, false, LocalOrUnknown),
            (true, false, true, true, true, RemoteOwned),
            (true, false, true, false, true, RemoteOwned),
            (true, false, false, true, true, RemoteOwned),
            (true, true, true, true, true, LocalOrUnknown),
        ] {
            assert_eq!(
                classify_windows_file_selection(registered, hdrop, descriptor, contents, owned),
                expected
            );
        }
    }

    #[test]
    fn verified_snapshot_rechecks_formats_and_sequence_around_the_read() {
        use std::cell::Cell;
        let sequence = Cell::new(7);
        let selection = Cell::new(ClipboardFileSelection::NonFile);
        let sample = || {
            verified_non_file_snapshot(7, || sequence.get(), || selection.get(), || Some("text"))
        };
        assert_eq!(sample(), Some("text"));
        sequence.set(8);
        assert_eq!(sample(), None);
        sequence.set(7);
        selection.set(ClipboardFileSelection::LocalOrUnknown);
        assert_eq!(sample(), None);
        selection.set(ClipboardFileSelection::NonFile);
        assert_eq!(
            verified_non_file_snapshot(
                7,
                || sequence.get(),
                || selection.get(),
                || {
                    selection.set(ClipboardFileSelection::LocalOrUnknown);
                    Some("file representation")
                },
            ),
            None
        );
        selection.set(ClipboardFileSelection::NonFile);
        assert_eq!(
            verified_non_file_snapshot(
                7,
                || sequence.get(),
                || selection.get(),
                || {
                    sequence.set(8);
                    Some("stale text")
                },
            ),
            None
        );
        assert_eq!(
            verified_non_file_snapshot(0, || 0, || ClipboardFileSelection::NonFile, || Some("x")),
            None
        );
    }

    #[test]
    fn clipboard_change_seen_during_poll_still_requests_desktop_cleanup() {
        let mut tracker = OfferTracker::default();
        assert!(!tracker.observe_sequence(7));
        assert!(!tracker.take_desktop_cleanup());
        assert!(tracker.observe_sequence(8));
        assert!(!tracker.observe_sequence(8));
        assert!(tracker.take_desktop_cleanup());
        assert!(!tracker.take_desktop_cleanup());
    }

    #[test]
    fn disabled_policy_cancels_binding_and_reports_each_session_without_rebinding() {
        let enabled = local_file_send_enabled();
        assert!(!enabled);
        let mut tracker = OfferTracker {
            observed_sequence: Some(7),
            used_sequence: Some(7),
            ..OfferTracker::default()
        };
        let first = SessionId::new();
        let second = SessionId::new();
        let roots = vec![PathBuf::from("synthetic-selection")];
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        tracker.sessions.insert(
            first,
            TrackedOffer {
                sequence: 7,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: cancelled.clone(),
                    roots: roots.clone(),
                    result: rx,
                    completed: None,
                }),
            },
        );

        assert!(matches!(
            tracker.poll_with_policy(first, 4, 7, Some(roots.clone()), enabled),
            (OfferPoll::Disabled { session, epoch: 4 }, false) if session == first
        ));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(tx.send(Ok(Vec::new())).is_err());
        assert!(tracker.sessions.is_empty());
        assert_eq!(tracker.used_sequence, Some(7));
        assert!(matches!(
            tracker.poll_with_policy(first, 4, 7, Some(roots.clone()), enabled),
            (OfferPoll::Unchanged, false)
        ));
        assert!(matches!(
            tracker.poll_with_policy(second, 9, 7, Some(roots.clone()), enabled),
            (OfferPoll::Disabled { session, epoch: 9 }, false) if session == second
        ));
        assert_eq!(tracker.disabled_notices.len(), 2);

        // Lifting a policy cannot turn an already consumed generation back
        // into a file handle, even if the old selection still has the same path.
        assert!(matches!(
            tracker.poll_with_policy(first, 4, 7, Some(roots.clone()), true),
            (
                OfferPoll::Failed {
                    error: BindError::Unavailable,
                    ..
                },
                false
            )
        ));
        assert!(matches!(
            tracker.poll_with_policy(second, 9, 8, Some(roots.clone()), enabled),
            (OfferPoll::Disabled { session, epoch: 9 }, true) if session == second
        ));
        assert!(tracker.take_desktop_cleanup());
        assert_eq!(tracker.disabled_notices.len(), 1);
        assert_eq!(tracker.disabled_notices.get(&second), Some(&(8, 9)));
        assert!(matches!(
            tracker.poll_with_policy(second, 9, 8, Some(roots), enabled),
            (OfferPoll::Unchanged, false)
        ));
    }

    #[test]
    fn disabled_policy_only_reports_an_observed_file_selection() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(matches!(
            tracker.poll_with_policy(session, 1, 7, None, false),
            (OfferPoll::Started, false)
        ));
        assert!(tracker.disabled_notices.is_empty());
        assert!(matches!(
            tracker.poll_with_policy(session, 1, 7, Some(Vec::new()), false),
            (OfferPoll::Disabled { session: owner, epoch: 1 }, false) if owner == session
        ));
    }

    #[test]
    fn unavailable_clipboard_backend_sends_one_fail_closed_notice() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(matches!(
            tracker.poll_with_policy(session, 4, 0, Some(Vec::new()), false),
            (OfferPoll::Disabled { session: owner, epoch: 4 }, false) if owner == session
        ));
        tracker.record_file_selection_marker(session, 0, 5, &[session]);
        assert!(matches!(
            tracker.poll_with_policy(session, 5, 0, Some(Vec::new()), false),
            (OfferPoll::Unchanged, false)
        ));
    }

    #[test]
    fn file_marker_rearms_the_epoch_and_invalidates_text_per_session() {
        let mut tracker = OfferTracker::default();
        let first = SessionId::new();
        let second = SessionId::new();
        assert!(matches!(
            tracker.poll_with_policy(first, 4, 7, Some(Vec::new()), false),
            (OfferPoll::Disabled { session, epoch: 4 }, false) if session == first
        ));
        tracker.record_file_selection_marker(first, 7, 5, &[first, second]);
        assert!(matches!(
            tracker.poll_with_policy(first, 5, 7, Some(Vec::new()), false),
            (OfferPoll::Unchanged, false)
        ));
        assert!(tracker.text_offer_invalidations.remove(&first));
        assert!(!tracker.text_offer_invalidations.remove(&first));
        tracker.retain_live(|_| false);
        assert!(tracker.text_offer_invalidations.contains(&second));
        assert!(tracker.observe_sequence(8));
        assert!(tracker.take_desktop_cleanup());
        assert!(tracker.text_offer_invalidations.remove(&second));
        assert!(!tracker.text_offer_invalidations.remove(&second));
    }

    #[test]
    fn a_replaced_file_sample_does_not_keep_an_old_session_notice() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(matches!(
            tracker.poll_with_policy(session, 1, 7, Some(Vec::new()), false),
            (OfferPoll::Disabled { .. }, false)
        ));
        // The final sequence check raced a new copy before marker delivery.
        assert!(matches!(
            tracker.poll_with_policy(session, 1, 8, None, false),
            (OfferPoll::Pending, true)
        ));
        assert!(tracker.take_desktop_cleanup());
        assert!(tracker.disabled_notices.is_empty());
        assert!(tracker.text_offer_invalidations.is_empty());
        assert!(matches!(
            tracker.poll_with_policy(session, 1, 8, None, false),
            (OfferPoll::Started, false)
        ));
    }

    #[test]
    fn new_clipboard_offer_waits_for_desktop_cleanup_before_binding() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(!tracker.observe_sequence(7));
        let (result, changed) = tracker.poll_observing(session, 0, 8, None);
        assert!(changed);
        assert!(matches!(result, OfferPoll::Pending));
        assert!(tracker.sessions.is_empty());
        assert!(tracker.take_desktop_cleanup());
        let (result, changed) = tracker.poll_observing(session, 0, 8, None);
        assert!(!changed);
        assert!(matches!(result, OfferPoll::Started));
    }

    #[cfg(windows)]
    #[test]
    fn delayed_file_reply_requires_the_same_nonzero_clipboard_sequence() {
        assert!(clipboard_sequence_matches(Some(7), 7));
        assert!(!clipboard_sequence_matches(Some(7), 8));
        assert!(!clipboard_sequence_matches(Some(0), 0));
        assert!(!clipboard_sequence_matches(None, 7));
    }

    #[test]
    fn unknown_clipboard_sequence_retires_the_previous_offer() {
        let mut tracker = OfferTracker::default();
        assert!(!tracker.observe_sequence(7));
        assert!(tracker.observe_sequence(0));
        assert!(tracker.take_desktop_cleanup());
        assert!(!tracker.observe_sequence(0));
        assert!(tracker.observe_sequence(8));
        assert!(tracker.take_desktop_cleanup());
    }

    /// Build a small tree and hand back its root.
    fn tree() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "adit-clipfiles-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("new fixture root");
        std::fs::create_dir_all(root.join("work").join("sub")).expect("mkdir");
        std::fs::write(root.join("work").join("a.bin"), [0u8, 1, 2, 3, 4, 5, 6, 7]).expect("write");
        std::fs::write(root.join("work").join("sub").join("b.txt"), b"hello").expect("write");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let root = std::fs::canonicalize(root).expect("canonical fixture root");
        root
    }

    fn checked_root(root: &Path) -> PathBuf {
        let temp = std::fs::canonicalize(std::env::temp_dir()).expect("temp directory");
        let resolved = std::fs::canonicalize(root).expect("fixture root");
        assert_eq!(resolved.parent(), Some(temp.as_path()));
        assert!(resolved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("adit-clipfiles-"));
        resolved
    }

    fn cleanup(root: &Path) {
        let resolved = checked_root(root);
        for _ in 0..100 {
            if std::fs::remove_dir_all(&resolved).is_ok() || !resolved.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("could not remove fixture {resolved:?}");
    }

    fn bind(paths: &[PathBuf]) -> Vec<OfferedFile> {
        expand(paths, &AtomicBool::new(false)).expect("bind fixture")
    }

    fn synthetic_resources(limit: usize) -> (FileResources, mpsc::Receiver<CloseJob>) {
        let (closer, jobs) = mpsc::channel();
        (
            FileResources {
                budget: Arc::new(FileBudget {
                    used: AtomicUsize::new(0),
                    limit,
                }),
                closer,
            },
            jobs,
        )
    }

    fn close_queued(jobs: &mpsc::Receiver<CloseJob>) {
        while let Ok(job) = jobs.try_recv() {
            job.close();
        }
    }

    #[test]
    fn concurrent_openings_cannot_overbook_the_shared_budget() {
        let (resources, _jobs) = synthetic_resources(4);
        let start = Arc::new(std::sync::Barrier::new(17));
        let release = Arc::new(std::sync::Barrier::new(17));
        let (tx, rx) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..16 {
            let budget = resources.budget.clone();
            let start = start.clone();
            let release = release.clone();
            let tx = tx.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                let permit = budget.reserve().ok();
                tx.send(permit.is_some()).unwrap();
                release.wait();
                drop(permit);
            }));
        }
        start.wait();
        let accepted = (0..16)
            .filter(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .count();
        assert_eq!(accepted, 4);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 4);
        release.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn budget_spans_shared_offers_and_the_reaper_queue_until_close() {
        let root = tree();
        let first = root.join("work").join("a.bin");
        let second = root.join("work").join("sub").join("b.txt");
        let (resources, jobs) = synthetic_resources(1);
        let cancel = AtomicBool::new(false);

        let original = expand_with_resources(&[first], &cancel, &resources).unwrap();
        let read_reference = original.clone();
        drop(original);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 1);
        assert_eq!(read_range(&read_reference[0], 0, 2), Some(vec![0, 1]));
        assert!(matches!(
            expand_with_resources(std::slice::from_ref(&second), &cancel, &resources),
            Err(BindError::HandleBudget)
        ));

        drop(read_reference);
        let waiting_to_close = jobs.try_recv().expect("the old handle reached the reaper");
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 1);
        assert!(matches!(
            expand_with_resources(std::slice::from_ref(&second), &cancel, &resources),
            Err(BindError::HandleBudget)
        ));
        waiting_to_close.close();
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);

        let replacement = expand_with_resources(&[second], &cancel, &resources).unwrap();
        assert_eq!(read_range(&replacement[0], 0, 5), Some(b"hello".to_vec()));
        drop(replacement);
        close_queued(&jobs);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);
        cleanup(&root);
    }

    #[test]
    fn reservation_precedes_open_and_failed_open_releases_it() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let (resources, jobs) = synthetic_resources(1);
        let worker_resources = resources.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_resources.open(|| {
                started_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                std::fs::File::open(path).map_err(|_| ())
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 1);
        let called = AtomicBool::new(false);
        assert!(matches!(
            resources.open(|| {
                called.store(true, Ordering::Release);
                Err(())
            }),
            Err(BindError::HandleBudget)
        ));
        assert!(!called.load(Ordering::Acquire));
        resume_tx.send(()).unwrap();
        drop(worker.join().unwrap().unwrap());
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 1);
        close_queued(&jobs);
        assert!(matches!(
            resources.open(|| Err(())),
            Err(BindError::Unavailable)
        ));
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);
        cleanup(&root);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn rejected_partial_selection_keeps_budget_until_its_files_close() {
        let root = tree();
        let first = root.join("work").join("a.bin");
        let missing = root.join("missing-directory").join("missing.bin");
        let (resources, jobs) = synthetic_resources(1);
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            expand_with_resources(&[first.clone(), missing], &cancel, &resources),
            Err(BindError::Unavailable)
        ));
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 1);
        assert!(matches!(
            expand_with_resources(std::slice::from_ref(&first), &cancel, &resources),
            Err(BindError::HandleBudget)
        ));
        close_queued(&jobs);
        let files = expand_with_resources(&[first], &cancel, &resources).unwrap();
        drop(files);
        close_queued(&jobs);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);
        cleanup(&root);
    }

    #[test]
    fn disconnected_reaper_retains_the_leaked_handle_budget() {
        const CHILD_ROOT: &str = "ADIT_TEST_FILE_REAPER_DISCONNECTED_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let path = PathBuf::from(root).join("work").join("a.bin");
            let (resources, jobs) = synthetic_resources(1);
            drop(jobs);
            let file = resources
                .open(|| std::fs::File::open(path).map_err(|_| ()))
                .unwrap();
            drop(file);
            assert_eq!(resources.budget.used.load(Ordering::Acquire), 1);
            assert!(matches!(
                resources.open(|| Err(())),
                Err(BindError::HandleBudget)
            ));
            return;
        }
        let root = tree();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clipboard_files::tests::disconnected_reaper_retains_the_leaked_handle_budget",
                "--test-threads=1",
            ])
            .env(CHILD_ROOT, &root)
            .status()
            .unwrap();
        assert!(status.success(), "isolated reaper failure test: {status}");
        cleanup(&root);
    }

    #[test]
    fn limit_failures_are_attached_to_each_offer_without_system_clipboard() {
        for (error, limit) in [
            (BindError::SelectionLimit, MAX_BOUND_FILES),
            (BindError::HandleBudget, MAX_FILE_HANDLES),
        ] {
            assert!(error.message().contains(&limit.to_string()));
            let mut tracker = OfferTracker::default();
            let session = SessionId::new();
            let roots = vec![PathBuf::from("synthetic.bin")];
            let (tx, rx) = mpsc::channel();
            tx.send(Err(error)).unwrap();
            tracker.sessions.insert(
                session,
                TrackedOffer {
                    sequence: 7,
                    bound: None,
                    pending: Some(PendingOffer {
                        cancelled: Arc::new(AtomicBool::new(false)),
                        roots: roots.clone(),
                        result: rx,
                        completed: None,
                    }),
                },
            );
            assert!(matches!(tracker.poll(session, 4, 7, Some(roots.clone())),
                OfferPoll::Failed { session: owner, epoch: 4, error: actual }
                    if owner == session && actual == error));
            let second = SessionId::new();
            assert!(matches!(
                tracker.poll(second, 9, 7, Some(roots)),
                OfferPoll::Failed { session: owner, epoch: 9, error: actual }
                    if owner == second && actual == error
            ));
        }
    }

    #[test]
    fn competing_sessions_keep_their_own_completed_binding_errors() {
        let mut tracker = OfferTracker::default();
        let first = SessionId::new();
        let second = SessionId::new();
        let roots = vec![PathBuf::from("synthetic.bin")];
        for (session, error) in [
            (first, BindError::SelectionLimit),
            (second, BindError::HandleBudget),
        ] {
            let (tx, rx) = mpsc::channel();
            tx.send(Err(error)).unwrap();
            tracker.sessions.insert(
                session,
                TrackedOffer {
                    sequence: 7,
                    bound: None,
                    pending: Some(PendingOffer {
                        cancelled: Arc::new(AtomicBool::new(false)),
                        roots: roots.clone(),
                        result: rx,
                        completed: None,
                    }),
                },
            );
        }

        // Polling the second session harvests the first worker's result, but
        // must leave it available for the first session's own poll.
        assert!(matches!(
            tracker.poll(second, 11, 7, Some(roots.clone())),
            OfferPoll::Failed { session, epoch: 11, error: BindError::HandleBudget }
                if session == second
        ));
        assert!(matches!(
            tracker.poll(first, 14, 7, Some(roots.clone())),
            OfferPoll::Failed { session, epoch: 14, error: BindError::SelectionLimit }
                if session == first
        ));
        assert!(matches!(
            tracker.poll(first, 14, 7, Some(roots.clone())),
            OfferPoll::Unchanged
        ));
        assert!(matches!(
            tracker.poll(second, 11, 7, Some(roots)),
            OfferPoll::Unchanged
        ));
    }

    #[test]
    fn shared_failed_binding_remains_available_to_its_original_session() {
        let mut tracker = OfferTracker::default();
        let first = SessionId::new();
        let second = SessionId::new();
        let roots = vec![PathBuf::from("synthetic.bin")];
        let (tx, rx) = mpsc::channel();
        tracker.sessions.insert(
            first,
            TrackedOffer {
                sequence: 7,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    roots: roots.clone(),
                    result: rx,
                    completed: None,
                }),
            },
        );

        assert!(matches!(
            tracker.poll(second, 11, 7, Some(roots.clone())),
            OfferPoll::Started
        ));
        tx.send(Err(BindError::SelectionLimit)).unwrap();
        // The second session is itself trying the same clipboard generation.
        // Its failure must not consume the first session's pending result.
        assert!(matches!(
            tracker.poll(second, 11, 7, Some(roots.clone())),
            OfferPoll::Failed { session, epoch: 11, error: BindError::SelectionLimit }
                if session == second
        ));
        // Focus may have advanced this session's epoch. The same sequence and
        // roots are revalidated before delivering the retained result.
        assert!(matches!(
            tracker.poll(first, 14, 7, Some(roots)),
            OfferPoll::Failed { session, epoch: 14, error: BindError::SelectionLimit }
                if session == first
        ));
    }

    fn queued_read(
        enabled: bool,
        files: &[OfferedFile],
        index: u32,
        offset: u64,
        length: u32,
    ) -> Option<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        queue_offered_read(
            enabled,
            files,
            index,
            offset,
            length,
            Box::new(move |data| tx.send(data).unwrap()),
        );
        rx.recv_timeout(Duration::from_secs(5))
            .expect("worker answer")
    }

    /// The list is flat, relative to the selection's *parent*, and a directory
    /// comes before the things inside it — that ordering is what lets the
    /// receiver create the folder before writing into it.
    #[test]
    fn a_directory_expands_to_a_flat_relative_list_in_creation_order() {
        let root = tree();
        let files = bind(&[root.join("work")]);

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

        drop(files);
        cleanup(&root);
    }

    #[test]
    fn an_empty_directory_is_still_offered() {
        let root = tree();
        let directory = root.join("empty");
        std::fs::create_dir(&directory).unwrap();
        let files = bind(&[directory]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].meta.name, "empty");
        assert!(files[0].meta.is_dir);
        drop(files);
        cleanup(&root);
    }

    /// Binary is the whole point: bytes come back exactly as written, including
    /// a NUL, and a range that runs past the end returns the short tail rather
    /// than failing.
    #[test]
    fn a_byte_range_is_returned_verbatim_and_clamps_at_the_end() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let files = bind(&[path]);

        assert_eq!(
            read_range(&files[0], 0, 8).expect("full read"),
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(read_range(&files[0], 4, 2).expect("mid read"), [4, 5]);
        // Past the end: the tail, not an error.
        assert_eq!(read_range(&files[0], 6, 64).expect("tail read"), [6, 7]);
        // Entirely past the end is an empty chunk, which is how a transfer ends.
        assert!(read_range(&files[0], 99, 8)
            .expect("beyond read")
            .is_empty());

        drop(files);
        cleanup(&root);
    }

    #[test]
    fn truncation_after_binding_is_a_failed_read_not_a_short_success() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let files = bind(std::slice::from_ref(&path));
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(3)
            .unwrap();
        assert_eq!(queued_read(true, &files, 0, 0, 8), None);
        assert_eq!(queued_read(true, &files, 0, 3, 2), None);
        assert_eq!(queued_read(true, &files, 0, 0, 3), Some(vec![0, 1, 2]));
        drop(files);
        cleanup(&root);
    }

    #[test]
    fn directory_depth_limit_fails_the_entire_offer() {
        let root = tree();
        let mut directory = root.join("deep");
        std::fs::create_dir(&directory).unwrap();
        for _ in 0..=MAX_DEPTH {
            directory.push("d");
            std::fs::create_dir(&directory).unwrap();
        }
        std::fs::write(directory.join("last.bin"), b"not partial").unwrap();
        assert!(expand(&[root.join("deep")], &AtomicBool::new(false)).is_err());
        cleanup(&root);
    }

    #[test]
    fn directory_entry_errors_and_descriptor_limits_reject_the_whole_list() {
        let entries = vec![
            Ok(PathBuf::from("visible")),
            Err(std::io::Error::other("synthetic directory read failure")),
        ];
        assert!(collect_children(entries.into_iter(), 10).is_err());
        let entries = vec![Ok(PathBuf::from("one")), Ok(PathBuf::from("two"))];
        assert!(collect_children(entries.into_iter(), 1).is_err());
        let entries = vec![Ok(PathBuf::from("two")), Ok(PathBuf::from("one"))];
        assert_eq!(
            collect_children(entries.into_iter(), 2).unwrap(),
            vec![PathBuf::from("one"), PathBuf::from("two")]
        );
    }

    #[test]
    fn an_unreadable_later_root_rejects_the_entire_selection() {
        let root = tree();
        assert!(expand(
            &[root.join("work"), root.join("missing")],
            &AtomicBool::new(false)
        )
        .is_err());
        cleanup(&root);
    }

    #[cfg(windows)]
    #[test]
    fn exceeding_the_open_file_limit_rejects_the_entire_directory() {
        let root = tree();
        let directory = root.join("many");
        std::fs::create_dir(&directory).unwrap();
        for index in 0..MAX_BOUND_FILES {
            std::fs::write(directory.join(format!("{index:04}.bin")), []).unwrap();
        }
        let (resources, jobs) = synthetic_resources(MAX_FILE_HANDLES);
        let cancel = AtomicBool::new(false);
        let files = expand_with_resources(std::slice::from_ref(&directory), &cancel, &resources)
            .expect("exactly 1024 regular files are accepted");
        assert_eq!(
            files.iter().filter(|file| !file.meta.is_dir).count(),
            MAX_BOUND_FILES
        );
        drop(files);
        close_queued(&jobs);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);

        std::fs::write(directory.join(format!("{MAX_BOUND_FILES:04}.bin")), []).unwrap();
        assert!(matches!(
            expand_with_resources(&[directory], &cancel, &resources),
            Err(BindError::SelectionLimit)
        ));
        close_queued(&jobs);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);
        cleanup(&root);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn file_limit_accepts_1024_and_rejects_1025_without_requiring_1024_descriptors() {
        #[cfg(windows)]
        let expand_near_limit = windows_bound::expand_near_file_limit;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let expand_near_limit = unix_bound::expand_near_file_limit;

        // Seed the selection count at 1023 so this remains valid on hosts with
        // a lower process descriptor limit than the application's limit.
        let root = tree();
        let first = root.join("work").join("a.bin");
        let second = root.join("work").join("sub").join("b.txt");
        let (resources, jobs) = synthetic_resources(3);
        let cancel = AtomicBool::new(false);
        let files = expand_near_limit(std::slice::from_ref(&first), &cancel, &resources).unwrap();
        assert_eq!(files.len(), 1);
        drop(files);
        close_queued(&jobs);
        assert!(matches!(
            expand_near_limit(&[first, second], &cancel, &resources),
            Err(BindError::SelectionLimit)
        ));
        close_queued(&jobs);
        assert_eq!(resources.budget.used.load(Ordering::Acquire), 0);
        cleanup(&root);
    }

    #[test]
    fn disabled_sharing_refuses_reads_even_with_a_stale_local_file_mapping() {
        let root = tree();
        let files = bind(&[root.join("work").join("a.bin")]);
        assert_eq!(queued_read(true, &files, 0, 1, 3), Some(vec![1, 2, 3]));
        assert_eq!(queued_read(false, &files, 0, 1, 3), None);
        drop(files);
        cleanup(&root);
    }

    /// A read that cannot happen must be `None`, not an empty chunk: the caller
    /// turns `None` into the protocol error and an empty chunk into "end of
    /// file", and confusing the two writes a truncated file on the far side.
    #[test]
    fn a_missing_file_fails_rather_than_reading_as_empty() {
        let root = tree();
        assert!(expand(&[root.join("missing.bin")], &AtomicBool::new(false)).is_err());
        cleanup(&root);
    }

    /// A single file keeps its own name and nothing more — copying `a.bin` must
    /// paste as `a.bin`, not as `work\a.bin`.
    #[test]
    fn a_lone_file_is_offered_under_its_bare_name() {
        let root = tree();
        let files = bind(&[root.join("work").join("a.bin")]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].meta.name, "a.bin");
        assert!(!files[0].meta.is_dir);

        drop(files);
        cleanup(&root);
    }

    #[test]
    fn replacing_a_path_after_an_offer_keeps_reading_the_original_object() {
        let root = tree();
        let root_checked = checked_root(&root);
        let path = root_checked.join("work").join("a.bin");
        let files = bind(std::slice::from_ref(&path));
        let moved = root_checked.join("work").join("moved.bin");
        std::fs::rename(&path, &moved).expect("move only the synthetic file");
        std::fs::write(&path, b"intruder").expect("replacement fixture");
        assert_eq!(queued_read(true, &files, 0, 0, 8), Some((0..8).collect()));
        drop(files);
        cleanup(&root);
    }

    #[cfg(windows)]
    #[test]
    fn parent_replacement_during_binding_stays_on_the_open_directory() {
        use std::io::Read;
        use std::os::windows::process::CommandExt;

        let root = tree();
        let root_checked = checked_root(&root);
        let selected = root_checked.join("work");
        let original = root_checked.join("original");
        let outside = root_checked.join("other");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("a.bin"), b"intruder").unwrap();
        std::fs::write(outside.join("outside-only.bin"), b"intruder").unwrap();
        let junction = |target: &Path| {
            let result = std::process::Command::new("cmd.exe")
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .args(["/c", "mklink", "/J"])
                .arg(&selected)
                .arg(target)
                .output()
                .unwrap();
            assert!(result.status.success(), "junction fixture: {result:?}");
        };
        let (parent, name) = windows_bound::open_parent(&selected.join("a.bin"))
            .expect("open ordinary synthetic parent directories");
        std::fs::rename(&selected, &original).expect("move only the fixture directory");
        junction(&outside);
        let mut contents = Vec::new();
        windows_bound::open_child(&parent, &name)
            .expect("open relative to original directory")
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, (0..8).collect::<Vec<_>>());
        assert_eq!(
            windows_bound::children(&parent, 10).unwrap(),
            vec![PathBuf::from("a.bin"), PathBuf::from("sub")]
        );
        assert!(expand(&[selected.join("a.bin")], &AtomicBool::new(false)).is_err());
        std::fs::remove_dir(&selected).expect("remove only fixture junction");
        drop(parent);
        cleanup(&root);
    }

    #[cfg(windows)]
    #[test]
    fn setting_a_junction_on_an_open_parent_cannot_redirect_binding() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;

        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                handle: *mut std::ffi::c_void,
                code: u32,
                input: *const std::ffi::c_void,
                input_len: u32,
                output: *mut std::ffi::c_void,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }

        let root = tree();
        let selected = root.join("empty-parent");
        let outside = root.join("work");
        std::fs::create_dir(&selected).unwrap();
        let (parent, name) = windows_bound::open_parent(&selected.join("a.bin")).unwrap();
        let tagger = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(0x100) // FILE_WRITE_ATTRIBUTES
            .share_mode(7)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(&selected)
            .unwrap();
        let substitute: Vec<u16> = format!(r"\??\{}", outside.display())
            .encode_utf16()
            .collect();
        let display: Vec<u16> = outside.to_string_lossy().encode_utf16().collect();
        let mut path = substitute.clone();
        path.push(0);
        path.extend(&display);
        path.push(0);
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&0xa000_0003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
        buffer.extend_from_slice(&((8 + path.len() * 2) as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        buffer.extend_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
        buffer.extend_from_slice(&(((substitute.len() + 1) * 2) as u16).to_le_bytes());
        buffer.extend_from_slice(&((display.len() * 2) as u16).to_le_bytes());
        for unit in path {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }
        let mut returned = 0;
        // SAFETY: the fixture directory handle and input buffer are valid for
        // the synchronous FSCTL; no output buffer is required.
        let ok = unsafe {
            DeviceIoControl(
                tagger.as_raw_handle(),
                0x900a4, // FSCTL_SET_REPARSE_POINT
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            ok,
            0,
            "fixture junction: {}",
            std::io::Error::last_os_error()
        );
        assert!(windows_bound::open_child(&parent, &name).is_err());
        if let Ok(children) = windows_bound::children(&parent, 10) {
            assert!(children.is_empty());
        }
        drop(tagger);
        std::fs::remove_dir(&selected).expect("remove only fixture junction");
        drop(parent);
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    type BindingBarrier = (
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
        std::thread::JoinHandle<Result<Vec<OfferedFile>, BindError>>,
    );

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn bind_at_barrier(
        selected: PathBuf,
        paused_path: PathBuf,
        stage: unix_bound::Stage,
    ) -> BindingBarrier {
        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut hook = |observed, path: &Path| {
                if observed == stage && path == paused_path {
                    reached_tx.send(()).expect("barrier observer");
                    resume_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("barrier resume");
                }
            };
            unix_bound::expand_with_hook(&[selected], &AtomicBool::new(false), &mut hook)
        });
        (reached_rx, resume_tx, worker)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_symlink_in_a_selected_parent_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let selected = root.join("selection");
        symlink(root.join("work"), &selected).unwrap();
        assert!(expand(&[selected.join("a.bin")], &AtomicBool::new(false)).is_err());
        std::fs::remove_file(&selected).unwrap();
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn replaced_parent_cannot_redirect_a_file_during_binding() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let selected_parent = root.join("work");
        let selected = selected_parent.join("a.bin");
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("a.bin"), b"intruder").unwrap();
        let (reached, resume, worker) = bind_at_barrier(
            selected.clone(),
            selected.clone(),
            unix_bound::Stage::ParentOpened,
        );
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::rename(&selected_parent, root.join("original")).unwrap();
        symlink(&outside, &selected_parent).unwrap();
        resume.send(()).unwrap();
        let files = worker.join().unwrap().expect("bind anchored original");
        assert_eq!(queued_read(true, &files, 0, 0, 8), Some((0..8).collect()));
        assert!(expand(&[selected], &AtomicBool::new(false)).is_err());
        std::fs::remove_file(&selected_parent).unwrap();
        drop(files);
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn replaced_directory_cannot_redirect_enumeration_during_binding() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let selected = root.join("work");
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("a.bin"), b"intruder").unwrap();
        std::fs::write(outside.join("outside-only.bin"), b"outside").unwrap();
        let (reached, resume, worker) = bind_at_barrier(
            selected.clone(),
            selected.clone(),
            unix_bound::Stage::DirectoryOpened,
        );
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::rename(&selected, root.join("original")).unwrap();
        symlink(&outside, &selected).unwrap();
        resume.send(()).unwrap();
        let files = worker.join().unwrap().expect("enumerate anchored original");
        let names: Vec<_> = files.iter().map(|file| file.meta.name.as_str()).collect();
        assert_eq!(
            names,
            ["work", "work\\a.bin", "work\\sub", "work\\sub\\b.txt"]
        );
        assert_eq!(queued_read(true, &files, 1, 0, 8), Some((0..8).collect()));
        std::fs::remove_file(&selected).unwrap();
        drop(files);
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn symlink_substitution_after_entry_check_fails_the_offer() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let selected = root.join("work").join("a.bin");
        let outside = root.join("outside.bin");
        std::fs::write(&outside, b"intruder").unwrap();
        let (reached, resume, worker) = bind_at_barrier(
            selected.clone(),
            selected.clone(),
            unix_bound::Stage::EntryChecked,
        );
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::rename(&selected, root.join("work").join("original.bin")).unwrap();
        symlink(&outside, &selected).unwrap();
        resume.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        std::fs::remove_file(&selected).unwrap();
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn directory_symlink_substitution_after_entry_check_fails_the_offer() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let selected = root.join("work");
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("a.bin"), b"intruder").unwrap();
        let (reached, resume, worker) = bind_at_barrier(
            selected.clone(),
            selected.clone(),
            unix_bound::Stage::EntryChecked,
        );
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::rename(&selected, root.join("original")).unwrap();
        symlink(&outside, &selected).unwrap();
        resume.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        std::fs::remove_file(&selected).unwrap();
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn symlinks_inside_a_selected_directory_are_not_offered() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let selected = root.join("work");
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.bin"), b"outside").unwrap();
        symlink(&outside, selected.join("outside-dir")).unwrap();
        symlink(outside.join("secret.bin"), selected.join("outside-file")).unwrap();
        let files = bind(std::slice::from_ref(&selected));
        let names: Vec<_> = files.iter().map(|file| file.meta.name.as_str()).collect();
        assert_eq!(
            names,
            ["work", "work\\a.bin", "work\\sub", "work\\sub\\b.txt"]
        );
        drop(files);
        cleanup(&root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn relative_paths_and_parent_components_are_rejected() {
        let root = tree();
        let relative = PathBuf::from("work").join("a.bin");
        let parent_component = root.join("work").join("..").join("work").join("a.bin");
        assert!(expand(&[relative], &AtomicBool::new(false)).is_err());
        assert!(expand(&[parent_component], &AtomicBool::new(false)).is_err());
        cleanup(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_selected_file_is_not_offered() {
        use std::os::unix::fs::symlink;

        let root = tree();
        let root_checked = checked_root(&root);
        let selected = root_checked.join("link.bin");
        symlink(root_checked.join("work").join("a.bin"), &selected).unwrap();
        let files = bind(std::slice::from_ref(&selected));
        assert!(files.is_empty());
        std::fs::remove_file(&selected).unwrap();
        cleanup(&root);
    }

    #[test]
    fn concurrent_ranges_use_their_own_offsets_and_reject_oversized_requests() {
        let root = tree();
        let files = bind(&[root.join("work").join("a.bin")]);
        let (tx, rx) = mpsc::channel();
        for offset in 0..8 {
            let tx = tx.clone();
            queue_offered_read(
                true,
                &files,
                0,
                offset,
                1,
                Box::new(move |data| tx.send((offset, data)).unwrap()),
            );
        }
        for _ in 0..8 {
            let (offset, data) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(data, Some(vec![offset as u8]));
        }
        assert_eq!(
            queued_read(true, &files, 0, 0, MAX_FILE_READ_BYTES + 1),
            None
        );
        drop(files);
        cleanup(&root);
    }

    #[test]
    fn queueing_a_read_does_not_wait_for_the_file_lock() {
        let root = tree();
        let files = bind(&[root.join("work").join("a.bin")]);
        let locked = files[0].file.as_ref().unwrap().lock().unwrap();
        let (tx, rx) = mpsc::channel();
        queue_offered_read(
            true,
            &files,
            0,
            0,
            2,
            Box::new(move |data| tx.send(data).unwrap()),
        );
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(locked);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(vec![0, 1])
        );
        drop(files);
        cleanup(&root);
    }

    fn wait_offer(
        tracker: &mut OfferTracker,
        session: SessionId,
        epoch: u64,
        sequence: u32,
        path: &Path,
    ) -> OfferPoll {
        for _ in 0..500 {
            match tracker.poll(session, epoch, sequence, Some(vec![path.to_path_buf()])) {
                OfferPoll::Started | OfferPoll::Pending => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result,
            }
        }
        panic!("offer worker did not finish");
    }

    fn wait_observed_offer(
        tracker: &mut OfferTracker,
        session: SessionId,
        sequence: u32,
        path: &Path,
    ) -> OfferPoll {
        for _ in 0..500 {
            let (result, changed) =
                tracker.poll_observing(session, 1, sequence, Some(vec![path.to_path_buf()]));
            assert!(!changed);
            match result {
                OfferPoll::Started | OfferPoll::Pending => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result,
            }
        }
        panic!("offer worker did not finish");
    }

    #[test]
    fn unknown_sequence_does_not_allow_a_replacement_to_be_rebound() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let mut tracker = OfferTracker::default();
        let first = SessionId::new();
        let OfferPoll::Ready(original) = wait_observed_offer(&mut tracker, first, 7, &path) else {
            panic!("initial synthetic offer");
        };
        assert_eq!(tracker.used_sequence, Some(7));
        std::fs::rename(&path, root.join("work").join("moved.bin")).unwrap();
        std::fs::write(&path, b"intruder").unwrap();

        assert!(tracker.observe_sequence(0));
        assert!(tracker.take_desktop_cleanup());
        assert!(tracker.sessions.is_empty());
        assert!(matches!(
            tracker.poll_observing(first, 1, 0, Some(vec![path.clone()])),
            (OfferPoll::Started, false)
        ));
        assert!(tracker.sessions.is_empty());
        assert!(tracker.observe_sequence(7));
        assert!(tracker.take_desktop_cleanup());
        let second = SessionId::new();
        assert!(matches!(
            tracker.poll_observing(second, 1, 7, Some(vec![path.clone()])),
            (OfferPoll::Failed { .. }, false)
        ));
        assert!(matches!(
            tracker.poll_observing(SessionId::new(), 1, 7, Some(vec![path.clone()])),
            (OfferPoll::Failed { .. }, false)
        ));
        assert_eq!(
            queued_read(true, &original, 0, 0, 8),
            Some((0..8).collect())
        );

        assert!(tracker.observe_sequence(8));
        assert!(tracker.take_desktop_cleanup());
        let OfferPoll::Ready(recopied) = wait_observed_offer(&mut tracker, second, 8, &path) else {
            panic!("a different nonzero sequence can bind the new selection");
        };
        assert_eq!(
            queued_read(true, &recopied, 0, 0, 8),
            Some(b"intruder".to_vec())
        );
        drop((original, recopied, tracker));
        cleanup(&root);
    }

    #[test]
    fn last_session_retirement_blocks_rebinding_but_live_windows_share_handles() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let mut tracker = OfferTracker::default();
        let first = SessionId::new();
        let second = SessionId::new();
        let third = SessionId::new();
        let OfferPoll::Ready(original) = wait_observed_offer(&mut tracker, first, 7, &path) else {
            panic!("initial synthetic offer");
        };
        std::fs::rename(&path, root.join("work").join("moved.bin")).unwrap();
        std::fs::write(&path, b"intruder").unwrap();

        let OfferPoll::Ready(shared) = wait_observed_offer(&mut tracker, second, 7, &path) else {
            panic!("another window must reuse the original handles");
        };
        assert_eq!(shared, original);
        tracker.retain_live(|session| session == second);
        let OfferPoll::Ready(still_shared) = wait_observed_offer(&mut tracker, third, 7, &path)
        else {
            panic!("a surviving window must still share the original handles");
        };
        assert_eq!(still_shared, original);
        assert_eq!(
            queued_read(true, &still_shared, 0, 0, 8),
            Some((0..8).collect())
        );

        tracker.retain_live(|_| false);
        assert!(tracker.sessions.is_empty());
        let fourth = SessionId::new();
        assert!(matches!(
            tracker.poll_observing(fourth, 1, 7, Some(vec![path.clone()])),
            (OfferPoll::Failed { .. }, false)
        ));
        assert!(tracker.observe_sequence(8));
        assert!(tracker.take_desktop_cleanup());
        let OfferPoll::Ready(recopied) = wait_observed_offer(&mut tracker, fourth, 8, &path) else {
            panic!("a different nonzero sequence can bind the new selection");
        };
        assert_eq!(
            queued_read(true, &recopied, 0, 0, 8),
            Some(b"intruder".to_vec())
        );
        drop((original, shared, still_shared, recopied, tracker));
        cleanup(&root);
    }

    #[test]
    fn retiring_pending_work_keeps_its_sequence_consumed() {
        for cancel_all in [false, true] {
            let mut tracker = OfferTracker {
                observed_sequence: Some(7),
                used_sequence: Some(7),
                ..OfferTracker::default()
            };
            let session = SessionId::new();
            let roots = vec![PathBuf::from("synthetic.bin")];
            let cancelled = Arc::new(AtomicBool::new(false));
            let (tx, rx) = mpsc::channel();
            tracker.sessions.insert(
                session,
                TrackedOffer {
                    sequence: 7,
                    bound: None,
                    pending: Some(PendingOffer {
                        cancelled: cancelled.clone(),
                        roots: roots.clone(),
                        result: rx,
                        completed: None,
                    }),
                },
            );
            if cancel_all {
                tracker.cancel();
                assert_eq!(tracker.observed_sequence, None);
            } else {
                tracker.retain_live(|_| false);
            }
            assert!(cancelled.load(Ordering::Acquire));
            assert!(tx.send(Ok(Vec::new())).is_err());
            assert!(matches!(
                tracker.poll_observing(SessionId::new(), 1, 7, Some(roots)),
                (OfferPoll::Failed { .. }, false)
            ));
            assert_eq!(tracker.failed_binding, Some((7, BindError::Unavailable)));
            assert!(tracker.observe_sequence(0));
            assert!(tracker.take_desktop_cleanup());
            assert!(tracker.observe_sequence(7));
            assert!(tracker.take_desktop_cleanup());
            assert_eq!(tracker.used_sequence, Some(7));
            assert_eq!(tracker.failed_binding, Some((7, BindError::Unavailable)));
            assert!(tracker.observe_sequence(8));
            assert!(tracker.take_desktop_cleanup());
            assert_eq!(tracker.used_sequence, None);
            assert_eq!(tracker.failed_binding, None);
        }
    }

    #[test]
    fn cancelling_offers_preserves_pending_desktop_cleanup() {
        let mut tracker = OfferTracker::default();
        assert!(!tracker.observe_sequence(7));
        assert!(tracker.observe_sequence(8));
        tracker.cancel();
        assert!(tracker.take_desktop_cleanup());
    }

    #[test]
    fn unused_sequence_remains_retryable_after_an_unknown_sample() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(matches!(
            tracker.poll_observing(session, 1, 7, None),
            (OfferPoll::Started, false)
        ));
        assert_eq!(tracker.used_sequence, None);
        assert!(tracker.observe_sequence(0));
        assert!(tracker.take_desktop_cleanup());
        assert!(tracker.observe_sequence(7));
        assert!(tracker.take_desktop_cleanup());
        let OfferPoll::Ready(files) = wait_observed_offer(&mut tracker, session, 7, &path) else {
            panic!("a sequence with no binding attempt can still be offered");
        };
        assert_eq!(queued_read(true, &files, 0, 0, 2), Some(vec![0, 1]));
        drop((files, tracker));
        cleanup(&root);
    }

    #[test]
    fn unchanged_clipboard_sequence_does_not_rebind_a_replacement() {
        let root = tree();
        let root_checked = checked_root(&root);
        let path = root_checked.join("work").join("a.bin");
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        let OfferPoll::Ready(original) = wait_offer(&mut tracker, session, 1, 10, &path) else {
            panic!("initial offer");
        };
        std::fs::rename(&path, root_checked.join("work").join("moved.bin")).unwrap();
        std::fs::write(&path, b"intruder").unwrap();
        assert!(matches!(
            tracker.poll(session, 2, 10, Some(vec![path.clone()])),
            OfferPoll::Unchanged
        ));
        assert_eq!(
            queued_read(true, &original, 0, 0, 8),
            Some((0..8).collect())
        );
        let other_session = SessionId::new();
        let OfferPoll::Ready(shared) = wait_offer(&mut tracker, other_session, 2, 10, &path) else {
            panic!("another focused session should reuse the original binding");
        };
        assert_eq!(shared, original);
        assert_eq!(queued_read(true, &shared, 0, 0, 8), Some((0..8).collect()));
        let OfferPoll::Ready(recopied) = wait_offer(&mut tracker, session, 1, 11, &path) else {
            panic!("explicit new copy");
        };
        assert_ne!(
            original, recopied,
            "equal path and size do not identify an offer"
        );
        assert_eq!(
            queued_read(true, &recopied, 0, 0, 8),
            Some(b"intruder".to_vec())
        );
        drop((original, shared, recopied, tracker));
        cleanup(&root);
    }

    #[test]
    fn a_temporarily_unavailable_clipboard_is_retried_for_the_same_sequence() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(matches!(
            tracker.poll(session, 1, 10, None),
            OfferPoll::Started
        ));
        let OfferPoll::Ready(files) = wait_offer(&mut tracker, session, 1, 10, &path) else {
            panic!("offer after clipboard becomes available");
        };
        assert_eq!(queued_read(true, &files, 0, 0, 2), Some(vec![0, 1]));
        drop((files, tracker));
        cleanup(&root);
    }

    #[test]
    fn a_new_unreadable_sequence_retires_the_old_offer_then_retries() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        let OfferPoll::Ready(original) = wait_offer(&mut tracker, session, 1, 10, &path) else {
            panic!("initial offer");
        };
        assert!(matches!(
            tracker.poll(session, 1, 11, None),
            OfferPoll::Started
        ));
        let OfferPoll::Ready(retried) = wait_offer(&mut tracker, session, 1, 11, &path) else {
            panic!("new offer after clipboard becomes readable");
        };
        assert_eq!(queued_read(true, &retried, 0, 0, 2), Some(vec![0, 1]));
        drop((original, retried, tracker));
        cleanup(&root);
    }

    #[test]
    fn a_new_sequence_is_reported_once_even_while_clipboard_reads_fail() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        assert!(!tracker.observe_sequence(10));
        assert!(matches!(
            tracker.poll(session, 1, 10, None),
            OfferPoll::Started
        ));
        assert!(tracker.observe_sequence(11));
        assert!(matches!(
            tracker.poll(session, 1, 11, None),
            OfferPoll::Started
        ));
        assert!(!tracker.observe_sequence(11));
        assert!(matches!(
            tracker.poll(session, 2, 11, None),
            OfferPoll::Started
        ));
    }

    #[test]
    fn a_completed_binding_waits_until_the_clipboard_can_be_checked() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(Vec::new())).unwrap();
        tracker.sessions.insert(
            session,
            TrackedOffer {
                sequence: 10,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    roots: vec![PathBuf::from("synthetic.bin")],
                    result: rx,
                    completed: None,
                }),
            },
        );
        assert!(matches!(
            tracker.poll(session, 1, 10, None),
            OfferPoll::Pending
        ));
        assert!(matches!(
            tracker.poll(session, 1, 10, Some(vec![PathBuf::from("synthetic.bin")])),
            OfferPoll::Ready(_)
        ));
    }

    #[test]
    fn changed_paths_under_the_same_sequence_cancel_the_pending_binding() {
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        tracker.sessions.insert(
            session,
            TrackedOffer {
                sequence: 10,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: cancelled.clone(),
                    roots: vec![PathBuf::from("original.bin")],
                    result: rx,
                    completed: None,
                }),
            },
        );
        assert!(matches!(
            tracker.poll(session, 1, 10, Some(vec![PathBuf::from("replacement.bin")])),
            OfferPoll::Failed { .. }
        ));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(tx.send(Ok(Vec::new())).is_err());
        assert!(matches!(
            tracker.poll(session, 1, 10, Some(vec![PathBuf::from("replacement.bin")])),
            OfferPoll::Unchanged
        ));
    }

    #[test]
    fn completed_unfocused_bindings_do_not_starve_a_new_session() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let original = bind(std::slice::from_ref(&path));
        let mut tracker = OfferTracker::default();
        let mut cancelled = Vec::new();
        for _ in 0..MAX_PENDING_OFFERS {
            let flag = Arc::new(AtomicBool::new(false));
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(original.clone())).unwrap();
            tracker.sessions.insert(
                SessionId::new(),
                TrackedOffer {
                    sequence: 10,
                    bound: None,
                    pending: Some(PendingOffer {
                        cancelled: flag.clone(),
                        roots: vec![path.clone()],
                        result: rx,
                        completed: None,
                    }),
                },
            );
            cancelled.push(flag);
        }
        let session = SessionId::new();
        let OfferPoll::Ready(files) = wait_offer(&mut tracker, session, 1, 10, &path) else {
            panic!("a finished inactive selection must not block this session");
        };
        assert!(cancelled.iter().all(|flag| !flag.load(Ordering::Acquire)));
        assert_eq!(queued_read(true, &files, 0, 0, 2), Some(vec![0, 1]));
        drop((files, original, tracker));
        cleanup(&root);
    }

    #[test]
    fn an_unfocused_completed_binding_keeps_its_original_files() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let original = bind(std::slice::from_ref(&path));
        let mut tracker = OfferTracker::default();
        let session = SessionId::new();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(original.clone())).unwrap();
        tracker.sessions.insert(
            session,
            TrackedOffer {
                sequence: 10,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    roots: vec![path.clone()],
                    result: rx,
                    completed: None,
                }),
            },
        );
        assert!(matches!(
            tracker.poll(SessionId::new(), 1, 10, None),
            OfferPoll::Started
        ));
        std::fs::rename(&path, root.join("work").join("moved.bin")).unwrap();
        std::fs::write(&path, b"intruder").unwrap();
        let OfferPoll::Ready(files) = tracker.poll(session, 2, 10, Some(vec![path])) else {
            panic!("the original completed binding should survive the focus change");
        };
        assert_eq!(queued_read(true, &files, 0, 0, 8), Some((0..8).collect()));
        drop((original, files, tracker));
        cleanup(&root);
    }

    #[test]
    fn a_second_session_waits_for_the_existing_binding() {
        let root = tree();
        let path = root.join("work").join("a.bin");
        let original = bind(std::slice::from_ref(&path));
        let mut tracker = OfferTracker::default();
        let first = SessionId::new();
        let second = SessionId::new();
        let (tx, rx) = mpsc::channel();
        tracker.sessions.insert(
            first,
            TrackedOffer {
                sequence: 10,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    roots: vec![path.clone()],
                    result: rx,
                    completed: None,
                }),
            },
        );
        assert!(matches!(
            tracker.poll(second, 2, 10, Some(vec![path.clone()])),
            OfferPoll::Started
        ));
        assert!(!tracker.sessions.contains_key(&second));
        std::fs::rename(&path, root.join("work").join("moved.bin")).unwrap();
        std::fs::write(&path, b"intruder").unwrap();
        tx.send(Ok(original.clone())).unwrap();
        let OfferPoll::Ready(shared) = tracker.poll(second, 2, 10, Some(vec![path])) else {
            panic!("the second session must receive the existing binding");
        };
        assert_eq!(shared, original);
        assert_eq!(queued_read(true, &shared, 0, 0, 8), Some((0..8).collect()));
        drop((original, shared, tracker));
        cleanup(&root);
    }

    #[test]
    fn a_new_session_epoch_keeps_the_original_pending_binding() {
        let mut tracker = OfferTracker::default();
        let a = SessionId::new();
        let b = SessionId::new();
        let cancelled_a = Arc::new(AtomicBool::new(false));
        let cancelled_b = Arc::new(AtomicBool::new(false));
        let (tx_a, rx_a) = mpsc::channel();
        let (_tx_b, rx_b) = mpsc::channel();
        for (session, cancelled, result) in [
            (a, cancelled_a.clone(), rx_a),
            (b, cancelled_b.clone(), rx_b),
        ] {
            tracker.sessions.insert(
                session,
                TrackedOffer {
                    sequence: 1,
                    bound: None,
                    pending: Some(PendingOffer {
                        cancelled,
                        roots: vec![PathBuf::from("synthetic.bin")],
                        result,
                        completed: None,
                    }),
                },
            );
        }
        assert!(matches!(tracker.poll(a, 2, 1, None), OfferPoll::Pending));
        assert!(!cancelled_a.load(Ordering::Acquire));
        assert!(!cancelled_b.load(Ordering::Acquire));
        tx_a.send(Ok(Vec::new())).unwrap();
        assert!(matches!(
            tracker.poll(a, 2, 1, Some(vec![PathBuf::from("synthetic.bin")])),
            OfferPoll::Ready(_)
        ));
        assert!(cancelled_a.load(Ordering::Acquire));
        tracker.retain_live(|session| session == a);
        assert!(cancelled_b.load(Ordering::Acquire));
        assert!(tracker.sessions.contains_key(&a));
    }

    #[test]
    fn a_new_clipboard_sequence_cancels_unfocused_pending_work() {
        let mut tracker = OfferTracker::default();
        let old_session = SessionId::new();
        let current_session = SessionId::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        tracker.sessions.insert(
            old_session,
            TrackedOffer {
                sequence: 1,
                bound: None,
                pending: Some(PendingOffer {
                    cancelled: cancelled.clone(),
                    roots: vec![PathBuf::from("old.bin")],
                    result: rx,
                    completed: None,
                }),
            },
        );
        assert!(matches!(
            tracker.poll(current_session, 1, 2, None),
            OfferPoll::Started
        ));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(tx.send(Ok(Vec::new())).is_err());
    }
}
