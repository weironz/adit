//! The COM data object that makes remote files pasteable in Explorer.
//!
//! Three interfaces, which is what every implementation of this ends up with —
//! FreeRDP's `wf_cliprdr.c` and, through it, RustDesk's:
//!
//! * [`FileDataObject`] (`IDataObject`) advertises `CFSTR_FILEDESCRIPTORW` and
//!   `CFSTR_FILECONTENTS`, and hands out one stream per file;
//! * [`FormatEnumerator`] (`IEnumFORMATETC`) lists those two formats;
//! * [`FileStream`] (`IStream`) is where a file's bytes actually come from.
//!
//! ## Delayed rendering, and why it is the whole point
//!
//! Nothing is downloaded when the remote copies. The data object carries only
//! names and sizes; Explorer calls `IStream::Read` per file at paste time, and
//! that is the first moment any byte crosses the wire. Copying a 4 GB folder and
//! never pasting it costs nothing.
//!
//! ## Threading
//!
//! `Read` runs on **Explorer's** thread and blocks there via `ChunkBridge` while
//! the round trip happens. It must never run on Adit's UI thread — that shows up
//! as "Not Responding" and nothing else (see CLAUDE.md). Everything here is
//! consequently `Send + Sync` and touches no `AditApp` state.

use std::sync::Arc;

use adit_session::ClipFile;
use windows::core::{implement, Result as WinResult, BOOL, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA,
    ISequentialStream_Impl, IStream, IStream_Impl, FORMATETC, LOCKTYPE, STATFLAG, STATSTG,
    STGC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::UI::Shell::FILEDESCRIPTORW;

use super::bridge::{ChunkBridge, ChunkError};

/// Asks the RDP session for a byte range. Implemented by the session layer;
/// called from Explorer's threads, hence `Send + Sync`.
pub(crate) type ChunkRequester = Arc<dyn Fn(u32, u32, u64, u32) + Send + Sync>;

// Constants the `windows` crate's bindings do not export, spelled out from the
// SDK headers rather than guessed.
const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_FILESIZE: u32 = 0x0000_0040;
const FD_PROGRESSUI: u32 = 0x0000_4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const TYMED_HGLOBAL: u32 = 0x0000_0001;
const TYMED_ISTREAM: u32 = 0x0000_0004;
const DVASPECT_CONTENT: u32 = 0x0000_0001;
const DATADIR_GET: u32 = 1;
const STGTY_STREAM: u32 = 2;
/// `DV_E_FORMATETC` — "that format is not one I serve".
const DV_E_FORMATETC: HRESULT = HRESULT(0x8004_0064u32 as i32);
/// `OLE_E_ADVISENOTSUPPORTED`.
const OLE_E_ADVISENOTSUPPORTED: HRESULT = HRESULT(0x8004_0003u32 as i32);
/// `DATA_S_SAMEFORMATETC`.
const DATA_S_SAMEFORMATETC: HRESULT = HRESULT(0x0004_0130);
/// `STG_E_ACCESSDENIED`.
const STG_E_ACCESSDENIED: HRESULT = HRESULT(0x8003_0005u32 as i32);
const E_UNEXPECTED: HRESULT = HRESULT(0x8000_FFFFu32 as i32);
/// `HRESULT_FROM_WIN32(ERROR_TIMEOUT)`.
const HRESULT_TIMEOUT: HRESULT = HRESULT(0x8007_05B4u32 as i32);

/// The two shell formats, registered by name. Their numeric ids are assigned per
/// session by Windows, so they cannot be constants — MS-RDPECLIP says the same
/// about `FileGroupDescriptorW` on the wire, for the same reason.
fn descriptor_format() -> u16 {
    // SAFETY: a static null-terminated UTF-16 literal. Registering a format that
    // already exists returns the existing id, so repeat calls are free.
    unsafe { RegisterClipboardFormatW(windows::core::w!("FileGroupDescriptorW")) as u16 }
}

fn contents_format() -> u16 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("FileContents")) as u16 }
}

/// Everything the streams need, shared by every stream the object hands out.
struct Source {
    files: Vec<ClipFile>,
    bridge: ChunkBridge,
    request: ChunkRequester,
    /// Largest range to ask for in one round trip. Mirrors the helper's own
    /// clamp; asking for more just gets clamped there and wastes a message.
    chunk: u32,
}

/// `IDataObject` over a remote file list.
#[implement(IDataObject)]
pub(crate) struct FileDataObject {
    source: Arc<Source>,
}

impl FileDataObject {
    pub(crate) fn new(
        files: Vec<ClipFile>,
        bridge: ChunkBridge,
        request: ChunkRequester,
        chunk: u32,
    ) -> Self {
        Self {
            source: Arc::new(Source {
                files,
                bridge,
                request,
                chunk,
            }),
        }
    }
}

/// The `FILEGROUPDESCRIPTORW` block: a count followed by one descriptor per
/// entry, in a movable global allocation because that is what the shell takes
/// ownership of.
fn descriptor_block(files: &[ClipFile]) -> WinResult<STGMEDIUM> {
    let bytes = size_of::<u32>() + files.len() * size_of::<FILEDESCRIPTORW>();

    // SAFETY: GHND is zero-initialised and movable. The handle leaves inside an
    // STGMEDIUM, which the shell frees — freeing it here would double-free.
    unsafe {
        let handle = GlobalAlloc(GHND, bytes).map_err(|_| windows::core::Error::from(E_OUTOFMEMORY))?;
        let base = GlobalLock(handle);
        if base.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        // Count first, then the descriptors packed straight after it:
        // FILEGROUPDESCRIPTORW is a header-plus-tail struct, so the tail is
        // written by hand rather than through the binding's one-element array.
        base.cast::<u32>().write_unaligned(files.len() as u32);
        let entries = base
            .cast::<u8>()
            .add(size_of::<u32>())
            .cast::<FILEDESCRIPTORW>();
        for (index, file) in files.iter().enumerate() {
            entries.add(index).write_unaligned(descriptor_for(file));
        }
        let _ = GlobalUnlock(handle);

        // `pUnkForRelease: None` means "the receiver frees this with the routine
        // implied by tymed" — GlobalFree here. Handing over an owner instead
        // would make the shell call back into an object that no longer exists.
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL,
            u: STGMEDIUM_0 { hGlobal: handle },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }
}

/// One `FILEDESCRIPTORW`.
///
/// `FD_PROGRESSUI` is always set, matching mstsc and FreeRDP: without it the
/// shell shows no progress dialog and a multi-gigabyte paste looks like a hang.
/// `FD_FILESIZE` matters as much — the shell pre-allocates from it, and lacking
/// it reads until a stream claims to be finished.
fn descriptor_for(file: &ClipFile) -> FILEDESCRIPTORW {
    let mut descriptor = FILEDESCRIPTORW {
        dwFlags: FD_ATTRIBUTES | FD_FILESIZE | FD_PROGRESSUI,
        dwFileAttributes: if file.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        },
        nFileSizeHigh: (file.size >> 32) as u32,
        nFileSizeLow: (file.size & 0xFFFF_FFFF) as u32,
        ..Default::default()
    };
    // cFileName is a fixed 260-wide array including the terminator. Truncated
    // rather than refused: the wire already caps names at 259, so anything
    // longer means the remote sent something out of spec.
    // Built as a whole array and assigned in one go: FILEDESCRIPTORW is packed,
    // so `descriptor.cFileName[..n]` would borrow a possibly-unaligned field,
    // which is not allowed. Assigning the field copies and sidesteps that.
    let mut name = [0u16; 260];
    for (slot, unit) in name.iter_mut().zip(file.name.encode_utf16().take(259)) {
        *slot = unit;
    }
    descriptor.cFileName = name;
    descriptor
}

impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> WinResult<STGMEDIUM> {
        // SAFETY: the shell always passes a valid FORMATETC.
        let format =
            unsafe { format.as_ref() }.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;

        if format.cfFormat == descriptor_format() {
            return descriptor_block(&self.source.files);
        }
        if format.cfFormat == contents_format() {
            // `lindex` selects the file. -1 means "all of them", which only
            // applies to the storage medium we do not offer.
            let index = u32::try_from(format.lindex)
                .map_err(|_| windows::core::Error::from(E_INVALIDARG))?;
            let file = self
                .source
                .files
                .get(index as usize)
                .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            if file.is_dir {
                // A directory has no contents to stream; the shell creates it
                // from the descriptor alone.
                return Err(E_INVALIDARG.into());
            }
            let stream: IStream =
                FileStream::new(Arc::clone(&self.source), index, file.size).into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM,
                u: STGMEDIUM_0 {
                    pstm: std::mem::ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }
        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _format: *const FORMATETC, _medium: *mut STGMEDIUM) -> WinResult<()> {
        // Only meaningful for caller-allocated storage, which neither format uses.
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return E_INVALIDARG;
        };
        if format.cfFormat == descriptor_format() || format.cfFormat == contents_format() {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn GetCanonicalFormatEtc(&self, _input: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        // No canonical form: every FORMATETC accepted here is already canonical.
        if !out.is_null() {
            unsafe { (*out).ptd = std::ptr::null_mut() };
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, _f: *const FORMATETC, _m: *const STGMEDIUM, _release: BOOL) -> WinResult<()> {
        // Read-only: the object publishes what the remote copied.
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> WinResult<IEnumFORMATETC> {
        // DATADIR_GET only. Enumerating what could be *set* on a read-only object
        // is meaningless, and an empty enumerator makes the shell retry instead.
        if direction != DATADIR_GET {
            return Err(E_NOTIMPL.into());
        }
        Ok(FormatEnumerator::new(0).into())
    }

    // Advisory connections are for data that changes. A clipboard snapshot does
    // not, and OLE_E_ADVISENOTSUPPORTED is the documented way to say so.
    fn DAdvise(
        &self,
        _f: *const FORMATETC,
        _advf: u32,
        _sink: windows::core::Ref<'_, IAdviseSink>,
    ) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _connection: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// `IEnumFORMATETC` over the two formats the object serves.
#[implement(IEnumFORMATETC)]
pub(crate) struct FormatEnumerator {
    index: std::sync::atomic::AtomicUsize,
}

impl FormatEnumerator {
    fn new(start: usize) -> Self {
        Self {
            index: std::sync::atomic::AtomicUsize::new(start),
        }
    }

    fn formats() -> [FORMATETC; 2] {
        let entry = |cf_format: u16, tymed: u32| FORMATETC {
            cfFormat: cf_format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed,
        };
        [
            entry(descriptor_format(), TYMED_HGLOBAL),
            entry(contents_format(), TYMED_ISTREAM),
        ]
    }
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, count: u32, out: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
        use std::sync::atomic::Ordering;

        let all = FormatEnumerator::formats();
        let start = self.index.load(Ordering::Relaxed);
        let available = all.len().saturating_sub(start).min(count as usize);

        // SAFETY: the caller guarantees `out` holds `count` entries.
        for offset in 0..available {
            unsafe { out.add(offset).write(all[start + offset]) };
        }
        self.index.store(start + available, Ordering::Relaxed);
        if !fetched.is_null() {
            unsafe { *fetched = available as u32 };
        }
        // S_FALSE means "fewer than asked for", which is not an error.
        if available == count as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> WinResult<()> {
        use std::sync::atomic::Ordering;
        let all = FormatEnumerator::formats().len();
        let next = self
            .index
            .load(Ordering::Relaxed)
            .saturating_add(count as usize);
        self.index.store(next.min(all), Ordering::Relaxed);
        // Skipping past the end is not an error, but the caller is told it did
        // not get everything it asked for.
        if next <= all {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> WinResult<()> {
        self.index.store(0, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn Clone(&self) -> WinResult<IEnumFORMATETC> {
        // A clone continues from where this one is, per the interface contract.
        let at = self.index.load(std::sync::atomic::Ordering::Relaxed);
        Ok(FormatEnumerator::new(at).into())
    }
}

/// `IStream` over one remote file.
///
/// Read-only and forward-biased: the shell reads sequentially, and every read is
/// a wire round trip, so nothing is cached. `Seek` is honoured because the shell
/// rewinds after a `Stat`.
#[implement(IStream)]
pub(crate) struct FileStream {
    source: Arc<Source>,
    index: u32,
    size: u64,
    position: std::sync::Mutex<u64>,
}

impl FileStream {
    fn new(source: Arc<Source>, index: u32, size: u64) -> Self {
        Self {
            source,
            index,
            size,
            position: std::sync::Mutex::new(0),
        }
    }
}

impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, buffer: *mut core::ffi::c_void, len: u32, read: *mut u32) -> HRESULT {
        let mut position = self
            .position
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // At or past the end: zero bytes read is how a stream says "done".
        if *position >= self.size || len == 0 || buffer.is_null() {
            if !read.is_null() {
                unsafe { *read = 0 };
            }
            return S_OK;
        }

        let want = u64::from(len).min(self.size - *position) as u32;
        let want = want.min(self.source.chunk);

        // Registered before the request goes out, so a response that beats us
        // back still has a slot waiting. See `ChunkBridge::begin`.
        let Ok(stream_id) = self.source.bridge.begin() else {
            return E_UNEXPECTED;
        };
        (self.source.request)(stream_id, self.index, *position, want);

        // Blocks on *Explorer's* thread, never Adit's UI thread.
        match self.source.bridge.wait(stream_id) {
            Ok(bytes) => {
                let copied = bytes.len().min(len as usize);
                // SAFETY: the caller guarantees `buffer` holds `len` bytes, and
                // `copied` is clamped to it.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copied);
                }
                *position += copied as u64;
                if !read.is_null() {
                    unsafe { *read = copied as u32 };
                }
                S_OK
            }
            // A failed read must not look like end-of-file, or the shell writes a
            // truncated file and reports success.
            Err(ChunkError::TimedOut) => HRESULT_TIMEOUT,
            Err(_) => E_UNEXPECTED,
        }
    }

    fn Write(&self, _buffer: *const core::ffi::c_void, _len: u32, _written: *mut u32) -> HRESULT {
        // The stream publishes remote bytes; it does not take them.
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, new_position: *mut u64) -> WinResult<()> {
        let mut position = self
            .position
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = match origin.0 {
            0 => 0i64,             // STREAM_SEEK_SET
            1 => *position as i64, // STREAM_SEEK_CUR
            2 => self.size as i64, // STREAM_SEEK_END
            _ => return Err(E_INVALIDARG.into()),
        };
        let target = base
            .checked_add(offset)
            .filter(|t| *t >= 0)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        // Seeking past the end is legal and simply reads as end-of-file.
        *position = target as u64;
        if !new_position.is_null() {
            unsafe { *new_position = *position };
        }
        Ok(())
    }

    fn Stat(&self, out: *mut STATSTG, _flag: &STATFLAG) -> WinResult<()> {
        // The shell asks for this to size its progress dialog. The size is the
        // only field it needs; pwcsName stays null (STATFLAG_NONAME).
        let Some(out) = (unsafe { out.as_mut() }) else {
            return Err(E_INVALIDARG.into());
        };
        *out = STATSTG {
            cbSize: self.size,
            r#type: STGTY_STREAM,
            ..Default::default()
        };
        Ok(())
    }

    fn Clone(&self) -> WinResult<IStream> {
        let at = *self
            .position
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let clone = FileStream::new(Arc::clone(&self.source), self.index, self.size);
        *clone
            .position
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = at;
        Ok(clone.into())
    }

    // The rest belong to writable or transacted streams. A read-only clipboard
    // stream has no meaningful implementation of any of them, and E_NOTIMPL is
    // what the shell expects.
    fn SetSize(&self, _size: u64) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(
        &self,
        _to: windows::core::Ref<'_, IStream>,
        _len: u64,
        _read: *mut u64,
        _written: *mut u64,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _flags: &STGC) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn Revert(&self) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(&self, _offset: u64, _len: u64, _kind: &LOCKTYPE) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _offset: u64, _len: u64, _kind: u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64, is_dir: bool) -> ClipFile {
        ClipFile {
            name: name.to_owned(),
            size,
            is_dir,
        }
    }

    /// `FD_PROGRESSUI` is not decorative: without it a multi-gigabyte paste shows
    /// no progress dialog and reads as a hang. `FD_FILESIZE` is what the shell
    /// pre-allocates from.
    #[test]
    fn a_descriptor_advertises_size_and_progress() {
        let descriptor = descriptor_for(&file("a.bin", 8, false));
        // Copied out one field at a time: FILEDESCRIPTORW is packed, and
        // assert_eq! takes references, which a packed field cannot give.
        let (flags, low, high, attrs) = (
            descriptor.dwFlags,
            descriptor.nFileSizeLow,
            descriptor.nFileSizeHigh,
            descriptor.dwFileAttributes,
        );

        assert_eq!(flags & FD_FILESIZE, FD_FILESIZE);
        assert_eq!(flags & FD_PROGRESSUI, FD_PROGRESSUI);
        assert_eq!(low, 8);
        assert_eq!(high, 0);
        assert_eq!(attrs, FILE_ATTRIBUTE_NORMAL);
    }

    /// A size over 4 GiB has to survive the split into two 32-bit halves — get
    /// this wrong and a large file silently truncates to its low word.
    #[test]
    fn a_size_above_four_gigabytes_survives_the_split() {
        let size = 5_000_000_000u64;
        let descriptor = descriptor_for(&file("big.iso", size, false));

        let (high, low) = (descriptor.nFileSizeHigh, descriptor.nFileSizeLow);
        let recombined = (u64::from(high) << 32) | u64::from(low);
        assert_eq!(recombined, size);
    }

    /// A directory is marked as one and carries no size, so the shell creates it
    /// rather than trying to stream bytes out of it.
    #[test]
    fn a_directory_is_marked_and_carries_no_size() {
        let descriptor = descriptor_for(&file("docs", 0, true));

        let (attrs, low) = (descriptor.dwFileAttributes, descriptor.nFileSizeLow);
        assert_eq!(attrs & FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(low, 0);
    }

    /// The name is UTF-16 and null-terminated within a fixed 260-wide field.
    /// Non-ASCII is the normal case here, not an edge one.
    #[test]
    fn a_name_is_written_as_null_terminated_utf16() {
        let descriptor = descriptor_for(&file("报告\\第一章.docx", 1, false));

        let name = descriptor.cFileName;
        let expected: Vec<u16> = "报告\\第一章.docx".encode_utf16().collect();
        assert_eq!(&name[..expected.len()], expected.as_slice());
        assert_eq!(name[expected.len()], 0);
    }

    /// An over-long name is truncated to what the field holds rather than
    /// overflowing it. 259 is the cap the wire enforces too.
    #[test]
    fn an_overlong_name_is_truncated_not_overflowed() {
        let descriptor = descriptor_for(&file(&"x".repeat(500), 1, false));

        let name = descriptor.cFileName;
        assert_eq!(name[258], u16::from(b'x'));
        // The last slot stays null: the field is 260 wide including terminator.
        assert_eq!(name[259], 0);
    }
}
