use super::remote_tree::{Node, RemoteReader, RemoteTree, ROOT};
use super::{ChunkBridge, ClipFile};
use crate::i18n::tf;
use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, ReplyOpen, Request, SessionACL,
};
use std::{
    ffi::OsStr,
    io,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    time::{Duration, SystemTime},
};

const TTL: Duration = Duration::from_secs(1);

struct FileSystem {
    tree: RemoteTree,
    reader: RemoteReader,
    uid: u32,
    gid: u32,
    created: SystemTime,
}

impl FileSystem {
    fn attr(&self, node: &Node) -> FileAttr {
        FileAttr {
            ino: INodeNo(node.ino),
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: self.created,
            mtime: self.created,
            ctime: self.created,
            crtime: self.created,
            kind: if node.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if node.is_dir { 0o555 } else { 0o444 },
            nlink: if node.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

impl Filesystem for FileSystem {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match name
            .to_str()
            .and_then(|name| self.tree.lookup(parent.0, name))
        {
            Some(node) => reply.entry(&TTL, &self.attr(node), Generation(1)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.tree.get(ino.0) {
            Some(node) => reply.attr(&TTL, &self.attr(node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(Errno::EROFS);
        } else {
            match self.tree.get(ino.0) {
                Some(node) if !node.is_dir => {
                    reply.opened(FileHandle(ino.0), FopenFlags::FOPEN_DIRECT_IO)
                }
                Some(_) => reply.error(Errno::EISDIR),
                None => reply.error(Errno::ENOENT),
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(node) = self.tree.get(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.reader.read(node, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = self.tree.get(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if !node.is_dir {
            reply.error(Errno::ENOTDIR);
            return;
        }
        let entries = [
            (ino.0, FileType::Directory, "."),
            (node.parent, FileType::Directory, ".."),
        ]
        .into_iter()
        .chain(self.tree.children(ino.0).map(|child| {
            (
                child.ino,
                if child.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                child.name.as_str(),
            )
        }));
        for (index, (ino, kind, name)) in entries
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(INodeNo(ino), index as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

struct MountDirectory(PathBuf);
impl Drop for MountDirectory {
    fn drop(&mut self) {
        // Only this empty, newly-created mount directory. Never recurse through
        // a mount or delete content supplied by another application.
        let _ = std::fs::remove_dir(&self.0);
    }
}

pub(super) struct RemoteMount {
    session: Option<BackgroundSession>,
    directory: MountDirectory,
    bridge: ChunkBridge,
    pub paths: Vec<String>,
}

impl RemoteMount {
    pub fn new(
        files: Vec<ClipFile>,
        bridge: ChunkBridge,
        request: super::remote_tree::Requester,
        chunk: u32,
    ) -> Result<Self, String> {
        let tree = RemoteTree::new(&files).map_err(str::to_owned)?;
        let dir = std::env::temp_dir().join(format!(
            "adit-rdp-clipboard-{}-{}",
            std::process::id(),
            adit_domain::ProfileId::new()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|_| "无法创建私有 RDP 剪贴板挂载目录")?;
        let directory = MountDirectory(dir);
        let paths: Vec<_> = tree
            .children(ROOT)
            .map(|node| {
                url::Url::from_file_path(directory.0.join(&node.name))
                    .map(String::from)
                    .map_err(|_| "无法构造 RDP 剪贴板文件 URI".to_owned())
            })
            .collect::<Result<_, _>>()?;
        let fs = FileSystem {
            tree,
            reader: RemoteReader::new(bridge.clone(), request, chunk),
            // These are process credentials, not identities supplied by the peer.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            created: SystemTime::now(),
        };
        let mut options = Config::default();
        // Auto-unmount's helper must access the mount as root. Other users are
        // excluded, and the containing directory stays private (0700).
        options.acl = SessionACL::RootAndOwner;
        options.n_threads = Some(2);
        options.mount_options = vec![
            MountOption::RO,
            MountOption::NoExec,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::AutoUnmount,
            MountOption::FSName("adit-rdp-clipboard".into()),
        ];
        let session = fuser::spawn_mount(fs, &directory.0, &options).map_err(|error| {
            if error.kind() == io::ErrorKind::PermissionDenied {
                tf(
                    "无法挂载 RDP 文件剪贴板；请检查 FUSE 用户权限：{}",
                    &[&error],
                )
            } else {
                tf(
                    "无法挂载 RDP 文件剪贴板；需要可用的 /dev/fuse 和 fusermount3：{}",
                    &[&error],
                )
            }
        })?;
        Ok(Self {
            session: Some(session),
            directory,
            bridge,
            paths,
        })
    }

    #[cfg(test)]
    pub fn owns(&self, path: &std::path::Path) -> bool {
        path.starts_with(&self.directory.0)
    }
}

impl Drop for RemoteMount {
    fn drop(&mut self) {
        self.bridge.close();
        // The worker owns this object. Unmount and join never run on the GUI.
        if let Some(session) = self.session.take() {
            let _ = session.umount_and_join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipboard_rs::{Clipboard, ClipboardContext};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    #[ignore = "requires an isolated X11 display, /dev/fuse and fusermount3"]
    fn native_x11_offer_reads_files_through_the_kernel_only_when_pasted() {
        let expected = std::env::var("ADIT_CLIPBOARD_FIXTURE_DISPLAY")
            .expect("explicit private-display fixture required");
        assert_eq!(std::env::var("DISPLAY").unwrap(), expected);
        let bridge = ChunkBridge::new();
        let responder = bridge.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let calls = reads.clone();
        let bytes = [0u8, 255, 0, 7, 42, 9];
        let mount = RemoteMount::new(
            vec![ClipFile {
                name: "fixture directory\\binary.dat".into(),
                size: bytes.len() as u64,
                is_dir: false,
            }],
            bridge.clone(),
            Arc::new(move |stream, index, offset, length| {
                assert_eq!(index, 0);
                calls.fetch_add(1, Ordering::SeqCst);
                let from = offset as usize;
                let end = (from + length as usize).min(bytes.len());
                responder.deliver(stream, Some(bytes[from..end].to_vec()));
            }),
            2,
        )
        .unwrap();
        let directory = mount.directory.0.clone();
        let clipboard = ClipboardContext::new().unwrap();
        clipboard.set_files(mount.paths.clone()).unwrap();
        let mut peer = std::process::Command::new("/usr/bin/xclip")
            .args(["-selection", "clipboard", "-o", "-target", "text/uri-list"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while peer.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = peer.kill();
                let _ = peer.wait();
                panic!("private X11 clipboard peer timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = peer.wait_with_output().unwrap();
        assert!(output.status.success());
        let uris = String::from_utf8(output.stdout).unwrap();
        assert!(uris.lines().any(|line| line.trim() == mount.paths[0]));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "publishing metadata must not fetch file bytes"
        );
        let file = directory.join("fixture directory/binary.dat");
        if unsafe { libc::geteuid() } == 0 {
            let denied = std::process::Command::new("/usr/bin/setpriv")
                .args([
                    "--reuid=65534",
                    "--regid=65534",
                    "--clear-groups",
                    "--no-new-privs",
                    "/usr/bin/test",
                    "-r",
                ])
                .arg(&file)
                .output()
                .unwrap();
            assert_eq!(denied.status.code(), Some(1));
            assert!(
                denied.stderr.is_empty(),
                "permission test must execute without setup errors"
            );
        }
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        assert!(
            reads.load(Ordering::SeqCst) >= 3,
            "bounded reads must cross the captured requester"
        );
        assert!(std::fs::write(&file, b"must not write").is_err());
        bridge.close();
        assert!(
            std::fs::read(&file).is_err(),
            "disconnection must fail, not report an empty successful file"
        );
        drop(mount);
        assert!(
            !directory.exists(),
            "only the owned empty mount directory is removed"
        );
    }
}
