//! One atomic policy word shared by the app and its own helper.
//!
//! Clipboard notifications and control frames travel in opposite directions.
//! A shared word gives both processes the same policy boundary even when an
//! already-started IPC frame delays the control message.

use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

const SIZE: usize = std::mem::size_of::<AtomicU64>();
// One atomic snapshot prevents a policy change and an observed clipboard
// replacement from being combined into a state that never existed.
const MAX_EPOCH: u64 = (1 << 31) - 1;
const MAX_CONTENT_REVISION: u64 = u32::MAX as u64;

/// A mapping owned by exactly one app session and opened by exactly one helper.
/// The name contains no credential or clipboard data.
pub struct SharedClipboardPolicy {
    mapping: platform::Mapping,
    id: String,
    writable: bool,
}

// SAFETY: each mapping is page-aligned and remains alive for the entire value.
// Its only contents are accessed through AtomicU64; no mutable references or
// non-atomic accesses are exposed after initialization.
unsafe impl Send for SharedClipboardPolicy {}
// SAFETY: the same invariant allows concurrent atomic reads and writes.
unsafe impl Sync for SharedClipboardPolicy {}

impl SharedClipboardPolicy {
    pub fn create(epoch: u64, enabled: bool) -> io::Result<Self> {
        let word = encode(epoch, enabled)?;
        for _ in 0..32 {
            let id = new_id()?;
            match platform::Mapping::create(&id) {
                Ok(mapping) => {
                    // SAFETY: this process has just created the exclusive map,
                    // which is aligned and not yet published to the helper.
                    unsafe { mapping.ptr.as_ptr().write(AtomicU64::new(word)) };
                    let id = mapping.identifier(&id);
                    return Ok(Self {
                        mapping,
                        id,
                        writable: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot allocate clipboard policy mapping",
        ))
    }

    pub fn open(id: &str) -> io::Result<Self> {
        if !platform::valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid clipboard policy mapping id",
            ));
        }
        let mapping = platform::Mapping::open(id)?;
        Ok(Self {
            mapping,
            id: id.to_owned(),
            writable: false,
        })
    }

    /// Open in the child and release the inherited descriptor after mapping.
    pub fn open_in_child(id: &str) -> io::Result<Self> {
        let policy = Self::open(id)?;
        platform::close_inherited(id)?;
        Ok(policy)
    }

    /// Prepare only this child to inherit the read-only Unix capability.
    pub fn configure_child(&self, command: &mut std::process::Command) -> io::Result<()> {
        self.mapping.configure_child(command)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn load(&self) -> (u64, bool) {
        let (epoch, enabled, _) = self.load_full();
        (epoch, enabled)
    }

    pub fn load_full(&self) -> (u64, bool, u64) {
        // SAFETY: the mapping remains alive and contains an initialized atomic.
        let word = unsafe { self.mapping.ptr.as_ref() }.load(Ordering::Acquire);
        (
            word >> 33,
            word & 1 != 0,
            (word >> 1) & MAX_CONTENT_REVISION,
        )
    }

    pub fn store(&self, epoch: u64, enabled: bool) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "clipboard policy mapping is read-only",
            ));
        }
        let atom = unsafe { self.mapping.ptr.as_ref() };
        let mut previous = atom.load(Ordering::Acquire);
        loop {
            let revision = (previous >> 1) & MAX_CONTENT_REVISION;
            let word = encode_full(epoch, enabled, revision)?;
            match atom.compare_exchange_weak(previous, word, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(current) => previous = current,
            }
        }
        Ok(())
    }

    /// Publish a complete state under the app session's policy lock.
    pub fn store_full(&self, epoch: u64, enabled: bool, revision: u64) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "clipboard policy mapping is read-only",
            ));
        }
        let word = encode_full(epoch, enabled, revision)?;
        // SAFETY: only the creating app writes, and all accesses are atomic.
        unsafe { self.mapping.ptr.as_ref() }.store(word, Ordering::Release);
        Ok(())
    }
}

fn encode(epoch: u64, enabled: bool) -> io::Result<u64> {
    encode_full(epoch, enabled, 0)
}

fn encode_full(epoch: u64, enabled: bool, revision: u64) -> io::Result<u64> {
    if epoch > MAX_EPOCH || revision > MAX_CONTENT_REVISION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard policy or content revision exhausted",
        ));
    }
    Ok((epoch << 33) | (revision << 1) | u64::from(enabled))
}

fn new_id() -> io::Result<String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    // macOS limits POSIX shared-memory names to 31 bytes including '/'.
    // Base32 keeps all 128 random bits in 26 characters.
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut id = String::from("ap-");
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in random {
        accumulator = (accumulator << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            id.push(ALPHABET[((accumulator >> bits) & 31) as usize] as char);
        }
        accumulator &= (1 << bits) - 1;
    }
    if bits != 0 {
        id.push(ALPHABET[((accumulator << (5 - bits)) & 31) as usize] as char);
    }
    Ok(id)
}

#[cfg(windows)]
fn valid_name(id: &str) -> bool {
    id.len() <= 128
        && id.starts_with("ap-")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::process::CommandExt;

    pub(super) struct Mapping {
        pub(super) ptr: NonNull<AtomicU64>,
        inherited_fd: Option<libc::c_int>,
    }

    impl Mapping {
        pub(super) fn create(id: &str) -> io::Result<Self> {
            #[cfg(target_os = "linux")]
            match Self::create_memfd(id) {
                Ok(mapping) => return Ok(mapping),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOSYS | libc::EPERM | libc::ENOENT)
                    ) => {}
                Err(error) => return Err(error),
            }
            let name = CString::new(format!("/{id}")).map_err(io::Error::other)?;
            // SAFETY: name is NUL terminated; O_EXCL ensures this is our object.
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // Open a separate read-only capability and remove the name before
            // any mapping or initialization work can delay cleanup.
            let inherited_fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
            if inherited_fd < 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                    libc::shm_unlink(name.as_ptr());
                }
                return Err(error);
            }
            if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(inherited_fd);
                    libc::shm_unlink(name.as_ptr());
                    libc::close(fd);
                }
                return Err(error);
            }
            if unsafe { libc::fcntl(inherited_fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0
                || unsafe { libc::ftruncate(fd, SIZE as libc::off_t) } != 0
            {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(inherited_fd);
                    libc::close(fd);
                }
                return Err(error);
            }
            let mut mapping = match Self::map(fd, true) {
                Ok(mapping) => mapping,
                Err(error) => {
                    unsafe {
                        libc::close(inherited_fd);
                        libc::close(fd);
                    }
                    return Err(error);
                }
            };
            unsafe {
                libc::close(fd);
            }
            mapping.inherited_fd = Some(inherited_fd);
            Ok(mapping)
        }

        #[cfg(target_os = "linux")]
        fn create_memfd(id: &str) -> io::Result<Self> {
            struct Fd(libc::c_int);
            impl Drop for Fd {
                fn drop(&mut self) {
                    unsafe {
                        libc::close(self.0);
                    }
                }
            }
            let name = CString::new(id).map_err(io::Error::other)?;
            // SAFETY: the name is terminated and flags request a private,
            // close-on-exec anonymous kernel object.
            let fd = unsafe {
                libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = Fd(fd);
            if unsafe { libc::fchmod(fd.0, 0o600) } != 0
                || unsafe { libc::ftruncate(fd.0, SIZE as libc::off_t) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // Prevent a peer from shrinking the object and causing SIGBUS.
            if unsafe {
                libc::fcntl(
                    fd.0,
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut mapping = Self::map(fd.0, true)?;
            let path = CString::new(format!("/proc/self/fd/{}", fd.0)).map_err(io::Error::other)?;
            // SAFETY: procfs resolves this process's already-open anonymous fd;
            // the helper receives a separately opened read-only capability.
            let inherited_fd =
                unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            if inherited_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            mapping.inherited_fd = Some(inherited_fd);
            Ok(mapping)
        }

        pub(super) fn open(id: &str) -> io::Result<Self> {
            let fd = parse_fd(id)?;
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if flags & libc::O_ACCMODE != libc::O_RDONLY {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "clipboard policy descriptor is not read-only",
                ));
            }
            Self::map(fd, false)
        }

        fn map(fd: libc::c_int, owner: bool) -> io::Result<Self> {
            let protection = libc::PROT_READ | if owner { libc::PROT_WRITE } else { 0 };
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: fd is open and metadata points to writable storage.
            if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful fstat initialized the complete structure.
            let metadata = unsafe { metadata.assume_init() };
            if metadata.st_size < SIZE as libc::off_t {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "short clipboard policy mapping",
                ));
            }
            // SAFETY: geteuid has no arguments and only reads the process identity.
            if metadata.st_uid != unsafe { libc::geteuid() } || metadata.st_mode & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "clipboard policy mapping permissions are invalid",
                ));
            }
            // SAFETY: fd is open, SIZE bytes were allocated by the creator.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    SIZE,
                    protection,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let Some(ptr) = NonNull::new(ptr.cast::<AtomicU64>()) else {
                unsafe {
                    libc::munmap(ptr, SIZE);
                }
                return Err(io::Error::other("null clipboard policy mapping"));
            };
            Ok(Self {
                ptr,
                inherited_fd: None,
            })
        }

        pub(super) fn identifier(&self, _: &str) -> String {
            format!("fd-{}", self.inherited_fd.expect("owner descriptor"))
        }

        pub(super) fn configure_child(
            &self,
            command: &mut std::process::Command,
        ) -> io::Result<()> {
            let fd = self
                .inherited_fd
                .ok_or_else(|| io::Error::other("missing policy descriptor"))?;
            // SAFETY: pre_exec changes only this child's descriptor flags using
            // async-signal-safe fcntl; the parent keeps FD_CLOEXEC set.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(())
        }
    }

    fn parse_fd(id: &str) -> io::Result<libc::c_int> {
        id.strip_prefix("fd-")
            .and_then(|value| value.parse::<libc::c_int>().ok())
            .filter(|fd| *fd >= 3)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid policy descriptor"))
    }

    pub(super) fn valid_id(id: &str) -> bool {
        parse_fd(id).is_ok()
    }

    pub(super) fn close_inherited(id: &str) -> io::Result<()> {
        let fd = parse_fd(id)?;
        if unsafe { libc::close(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: ptr is the original live mapping and SIZE is unchanged.
            unsafe {
                libc::munmap(self.ptr.as_ptr().cast(), SIZE);
            }
            if let Some(fd) = self.inherited_fd {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn shared_mapping_is_random_read_only_and_tracks_updates() {
        let writer = SharedClipboardPolicy::create(0, false).unwrap();
        let another = SharedClipboardPolicy::create(0, false).unwrap();
        assert_ne!(writer.id(), another.id());
        assert_ne!(new_id().unwrap(), new_id().unwrap());
        #[cfg(windows)]
        assert_eq!(writer.id().len(), "ap-".len() + 26);
        let reader = SharedClipboardPolicy::open(writer.id()).unwrap();
        assert_eq!(reader.load(), (0, false));
        assert_eq!(
            reader.store(1, true).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        writer.store(1, true).unwrap();
        assert_eq!(reader.load_full(), (1, true, 0));
        writer.store(2, false).unwrap();
        assert_eq!(reader.load_full(), (2, false, 0));
        writer.store_full(3, true, 42).unwrap();
        assert_eq!(reader.load_full(), (3, true, 42));
        writer.store(4, false).unwrap();
        assert_eq!(reader.load_full(), (4, false, 42));
        assert_eq!(
            reader.store_full(4, true, 43).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(writer.store_full(MAX_EPOCH + 1, false, 42).is_err());
        assert!(writer
            .store_full(4, false, MAX_CONTENT_REVISION + 1)
            .is_err());
        assert!(SharedClipboardPolicy::open("../invalid").is_err());
        #[cfg(unix)]
        assert!(writer.id().starts_with("fd-"));
    }

    #[test]
    #[ignore = "synthetic subprocess entry point"]
    fn policy_child_process() {
        let id = std::env::var("ADIT_SYNTHETIC_POLICY_ID").expect("synthetic mapping id");
        let reader = SharedClipboardPolicy::open_in_child(&id).unwrap();
        eprintln!("POLICY_READY");
        let mut command = [0u8; 1];
        io::stdin().read_exact(&mut command).unwrap();
        assert_eq!(reader.load_full(), (1, true, 41));
        eprintln!("POLICY_UPDATED");
        io::stdin().read_exact(&mut command).unwrap();
        assert_eq!(reader.load_full(), (2, false, 42));
    }

    #[test]
    #[ignore = "synthetic subprocess entry point"]
    fn policy_owner_before_bind_child() {
        let owner = SharedClipboardPolicy::create(0, false).unwrap();
        eprintln!("{}", owner.id());
        let mut command = [0u8; 1];
        let _ = io::stdin().read_exact(&mut command);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "synthetic subprocess entry point"]
    fn policy_unrelated_child_process() {
        let id = std::env::var("ADIT_SYNTHETIC_POLICY_ID").unwrap();
        assert!(SharedClipboardPolicy::open(&id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn policy_descriptor_is_not_inherited_by_an_unrelated_child() {
        let owner = SharedClipboardPolicy::create(0, false).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clipboard_policy::tests::policy_unrelated_child_process",
                "--ignored",
            ])
            .env("ADIT_SYNTHETIC_POLICY_ID", owner.id())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn policy_mapping_propagates_across_real_processes() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let writer = SharedClipboardPolicy::create(0, false).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "clipboard_policy::tests::policy_child_process",
                "--ignored",
                "--nocapture",
            ])
            .env("ADIT_SYNTHETIC_POLICY_ID", writer.id())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        writer.configure_child(&mut command).unwrap();
        let mut child = ChildGuard(command.spawn().unwrap());
        let stderr = child.0.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(stderr).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "POLICY_READY"
        );
        writer.store_full(1, true, 41).unwrap();
        child.0.stdin.as_mut().unwrap().write_all(b"1").unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "POLICY_UPDATED"
        );
        writer.store_full(2, false, 42).unwrap();
        child.0.stdin.as_mut().unwrap().write_all(b"2").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "synthetic child timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn owner_crash_before_bind_leaves_no_reopenable_object() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "clipboard_policy::tests::policy_owner_before_bind_child",
                    "--ignored",
                    "--nocapture",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = child.0.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut id = String::new();
            let result = io::BufReader::new(stderr).read_line(&mut id);
            let _ = tx.send(result.map(|_| id));
        });
        let id = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .trim()
            .to_owned();
        #[cfg(target_os = "linux")]
        {
            let fd = id.strip_prefix("fd-").unwrap().parse::<i32>().unwrap();
            let path = format!("/proc/{}/fd/{fd}", child.0.id());
            assert!(std::fs::read_link(path)
                .unwrap()
                .to_string_lossy()
                .ends_with(" (deleted)"));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        #[cfg(windows)]
        assert!(SharedClipboardPolicy::open(&id).is_err());
    }

    #[test]
    fn helper_crash_after_binding_preserves_owner_then_releases_resources() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let owner = SharedClipboardPolicy::create(0, false).unwrap();
        let id = owner.id().to_owned();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "clipboard_policy::tests::policy_child_process",
                "--ignored",
                "--nocapture",
            ])
            .env("ADIT_SYNTHETIC_POLICY_ID", &id)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        owner.configure_child(&mut command).unwrap();
        let mut child = ChildGuard(command.spawn().unwrap());
        let stderr = child.0.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = io::BufReader::new(stderr).read_line(&mut line);
            let _ = tx.send(result.map(|_| line));
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .trim(),
            "POLICY_READY"
        );
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        owner.store(4, true).unwrap();
        assert_eq!(owner.load(), (4, true));
        #[cfg(unix)]
        let fd = id
            .strip_prefix("fd-")
            .unwrap()
            .parse::<libc::c_int>()
            .unwrap();
        drop(owner);
        #[cfg(unix)]
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        #[cfg(windows)]
        assert!(SharedClipboardPolicy::open(&id).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires passwordless sudo for a synthetic nobody principal"]
    fn linux_other_user_cannot_open_the_anonymous_policy_descriptor() {
        let owner = SharedClipboardPolicy::create(0, false).unwrap();
        let fd = owner
            .id()
            .strip_prefix("fd-")
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(unsafe { libc::fstat(fd, metadata.as_mut_ptr()) }, 0);
        let metadata = unsafe { metadata.assume_init() };
        assert_eq!(metadata.st_mode & 0o777, 0o600);
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
        assert_eq!(
            unsafe { libc::ftruncate(fd, (SIZE * 2) as libc::off_t) },
            -1
        );
        let target = format!("/proc/{}/fd/{fd}", std::process::id());
        let link = std::fs::read_link(&target).unwrap();
        assert!(link.to_string_lossy().ends_with(" (deleted)"));
        assert!(Command::new("sudo")
            .args(["-n", "-u", "nobody", "--", "true"])
            .status()
            .unwrap()
            .success());
        let status = Command::new("sudo")
            .args(["-n", "-u", "nobody", "--", "cat", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "another Unix user opened the private policy descriptor"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_mapping_denies_reopening_with_write_access() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{OpenFileMappingW, FILE_MAP_WRITE};
        let owner = SharedClipboardPolicy::create(0, false).unwrap();
        let name: Vec<u16> = format!("Local\\{}", owner.id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: name is a terminated mapping name; the returned handle is closed if present.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_WRITE, 0, name.as_ptr()) };
        if !handle.is_null() {
            unsafe {
                CloseHandle(handle);
            }
            panic!("policy mapping reopened with write access");
        }
        assert_eq!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(windows)]
    #[test]
    fn anonymous_windows_principal_cannot_open_the_mapping() {
        use windows_sys::Win32::Security::{ImpersonateAnonymousToken, RevertToSelf};
        use windows_sys::Win32::System::Threading::GetCurrentThread;
        let owner = SharedClipboardPolicy::create(0, false).unwrap();
        let id = owner.id().to_owned();
        let result = std::thread::spawn(move || {
            struct Revert;
            impl Drop for Revert {
                fn drop(&mut self) {
                    unsafe {
                        RevertToSelf();
                    }
                }
            }
            // SAFETY: impersonation is limited to this dedicated synthetic thread.
            if unsafe { ImpersonateAnonymousToken(GetCurrentThread()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let _revert = Revert;
            match SharedClipboardPolicy::open(&id) {
                Ok(_) => Err(io::Error::other(
                    "anonymous principal opened the policy mapping",
                )),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
                Err(error) => Err(error),
            }
        })
        .join()
        .unwrap();
        result.expect("anonymous access must be denied");
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
        FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(super) struct Mapping {
        pub(super) ptr: NonNull<AtomicU64>,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        handle: HANDLE,
    }

    impl Mapping {
        pub(super) fn create(id: &str) -> io::Result<Self> {
            let name = wide(id);
            let descriptor = current_user_descriptor()?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            // SAFETY: unnamed paging-file backing, bounded size and valid UTF-16 name.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    &attributes,
                    PAGE_READWRITE,
                    0,
                    SIZE as u32,
                    name.as_ptr(),
                )
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            Self::map(handle, FILE_MAP_READ | FILE_MAP_WRITE)
        }

        pub(super) fn open(id: &str) -> io::Result<Self> {
            let name = wide(id);
            // SAFETY: name is a NUL-terminated mapping name; no handle inheritance.
            let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Self::map(handle, FILE_MAP_READ)
        }

        fn map(handle: HANDLE, access: u32) -> io::Result<Self> {
            // SAFETY: handle refers to the policy map and SIZE is its full length.
            let view = unsafe { MapViewOfFile(handle, access, 0, 0, SIZE) };
            let Some(ptr) = NonNull::new(view.Value.cast::<AtomicU64>()) else {
                let error = io::Error::last_os_error();
                unsafe {
                    CloseHandle(handle);
                }
                return Err(error);
            };
            Ok(Self { ptr, view, handle })
        }

        pub(super) fn identifier(&self, name: &str) -> String {
            name.to_owned()
        }

        pub(super) fn configure_child(&self, _: &mut std::process::Command) -> io::Result<()> {
            Ok(())
        }
    }

    pub(super) fn valid_id(id: &str) -> bool {
        valid_name(id)
    }

    pub(super) fn close_inherited(_: &str) -> io::Result<()> {
        Ok(())
    }

    fn wide(id: &str) -> Vec<u16> {
        format!("Local\\{id}")
            .encode_utf16()
            .chain(Some(0))
            .collect()
    }

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct LocalAllocation(*mut std::ffi::c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn current_user_descriptor() -> io::Result<LocalAllocation> {
        let mut token = std::ptr::null_mut();
        // SAFETY: current process pseudo-handle is valid and token points to storage.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Token(token);
        let mut bytes = 0u32;
        // SAFETY: the first call only asks for the required length.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        // SAFETY: buffer is aligned and has at least the requested size.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful TokenUser query initialized the TOKEN_USER header.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = std::ptr::null_mut();
        // SAFETY: SID is supplied by the token and output storage is valid.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid_allocation = LocalAllocation(sid.cast());
        let mut length = 0usize;
        // SAFETY: the system returns a NUL-terminated SID string.
        while unsafe { *sid.add(length) } != 0 {
            length += 1;
        }
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) })
            .map_err(io::Error::other)?;
        let sddl: Vec<u16> = format!("D:P(A;;GR;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        drop(sid_allocation);
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: SDDL is terminated and output points to writable storage.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalAllocation(descriptor))
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: both values belong to this mapping and are released once.
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub(super) struct Mapping {
        pub(super) ptr: NonNull<AtomicU64>,
    }
    impl Mapping {
        pub(super) fn create(_: &str) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "clipboard policy mapping is unsupported",
            ))
        }
        pub(super) fn open(_: &str) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "clipboard policy mapping is unsupported",
            ))
        }
        pub(super) fn identifier(&self, name: &str) -> String {
            name.to_owned()
        }
        pub(super) fn configure_child(&self, _: &mut std::process::Command) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "clipboard policy mapping is unsupported",
            ))
        }
    }
    pub(super) fn valid_id(_: &str) -> bool {
        false
    }
    pub(super) fn close_inherited(_: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clipboard policy mapping is unsupported",
        ))
    }
}
