//! Content-verified resume. Size/mtime are change detectors, never proof of
//! equality. Open handles keep writes off a concurrently replaced local path.
use super::*;
use sha2::{Digest, Sha256};
use std::io::SeekFrom;
use tokio::io::{AsyncRead, AsyncSeek, AsyncSeekExt};

fn failure(message: impl Into<String>) -> SshError {
    SshError::Sftp(message.into())
}
fn io_error(error: std::io::Error) -> SshError {
    SshError::LocalIo(error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    identity: (u64, u64),
    size: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp(file: &fs::File) -> Result<Stamp, SshError> {
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(failure("仅支持普通文件，拒绝设备、目录及链接"));
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(windows)]
    let identity = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // The handle stays open for the transfer; a replacement has another ID.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    };
    #[cfg(not(any(unix, windows)))]
    return Err(failure("当前平台无法验证文件身份，已拒绝不安全的传输"));
    #[cfg(any(unix, windows))]
    Ok(Stamp {
        identity,
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn plain_path(path: &Path) -> Result<(), SshError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(failure("拒绝重解析点目标"));
        }
    }
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(failure("拒绝非普通文件或符号链接目标"));
    }
    Ok(())
}

pub(super) fn check_directory(path: &Path) -> Result<(), SshError> {
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_error(e)),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(failure("下载路径包含链接或非目录父路径"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(failure("下载父目录包含重解析点"));
            }
        }
    }
    Ok(())
}

struct LocalFile {
    file: tokio::fs::File,
    guard: fs::File,
    path: PathBuf,
    initial: Stamp,
}

impl LocalFile {
    fn open(path: &Path, mode: TransferMode, writing: bool) -> Result<Self, SshError> {
        if let Some(parent) = path.parent() {
            check_directory(parent)?;
        }
        let before = match fs::symlink_metadata(path) {
            Ok(_) => {
                plain_path(path)?;
                Some(stamp(&fs::File::open(path).map_err(io_error)?)?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_error(e)),
        };
        if mode == TransferMode::Resume && before.is_none() {
            return Err(failure("续传目标不存在"));
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(writing);
        if writing && (mode == TransferMode::CreateNew || before.is_none()) {
            options.create_new(true);
        }
        #[cfg(windows)]
        if writing {
            use std::os::windows::fs::OpenOptionsExt;
            // Deny other writers and delete/rename for the lifetime of the handle.
            options.share_mode(1);
        }
        let guard = options.open(path).map_err(io_error)?;
        let initial = stamp(&guard)?;
        if let Some(before) = before {
            if before != initial {
                return Err(failure("目标在打开前已变化；未写入文件"));
            }
        }
        let file = tokio::fs::File::from_std(guard.try_clone().map_err(io_error)?);
        let result = Self {
            file,
            guard,
            path: path.to_path_buf(),
            initial,
        };
        result.check_path()?;
        Ok(result)
    }

    fn check_path(&self) -> Result<(), SshError> {
        if let Some(parent) = self.path.parent() {
            check_directory(parent)?;
        }
        plain_path(&self.path)?;
        let current = fs::File::open(&self.path).map_err(io_error)?;
        if stamp(&current)?.identity != self.initial.identity {
            return Err(failure("目标文件在传输期间被替换；已停止，未删除替换文件"));
        }
        Ok(())
    }

    fn check_source(&self) -> Result<(), SshError> {
        self.check_path()?;
        if stamp(&self.guard)? != self.initial {
            return Err(failure("本地源文件在传输期间变化"));
        }
        Ok(())
    }
}

/// Compare exactly `length` bytes, including the entire file when sizes match.
/// Returns false only for cancellation; a mismatch/short read is an error.
async fn compare_prefix<R: AsyncRead + Unpin, L: AsyncRead + Unpin>(
    remote: &mut R,
    local: &mut L,
    length: u64,
    ctx: &TransferCtx<'_>,
) -> Result<bool, SshError> {
    let mut remaining = length;
    let mut a = vec![0; SFTP_CHUNK];
    let mut b = vec![0; SFTP_CHUNK];
    while remaining > 0 {
        if ctx.cancelled() {
            return Ok(false);
        }
        let count = remaining.min(SFTP_CHUNK as u64) as usize;
        remote.read_exact(&mut a[..count]).await.map_err(io_error)?;
        local.read_exact(&mut b[..count]).await.map_err(io_error)?;
        if a[..count] != b[..count] {
            return Err(failure("已有内容与源文件前缀不一致，拒绝续传"));
        }
        remaining -= count as u64;
    }
    Ok(!ctx.cancelled())
}

fn resume_offset(existing: u64, source: u64) -> Result<u64, SshError> {
    if existing > source {
        return Err(failure("续传目标大于源文件，拒绝续传"));
    }
    Ok(existing)
}

async fn guarded_download_file<R: AsyncRead + AsyncSeek + Unpin>(
    reader: &mut R,
    target: &mut LocalFile,
    size: u64,
    ctx: &TransferCtx<'_>,
    base: u64,
    total: u64,
) -> Result<(u64, bool), SshError> {
    if ctx.cancelled() {
        return Ok((0, true));
    }
    let offset = if ctx.mode == TransferMode::Resume {
        resume_offset(target.initial.size, size)?
    } else {
        0
    };
    if offset > 0 {
        ctx.status("正在逐字节校验已有前缀…".into());
        if !compare_prefix(reader, &mut target.file, offset, ctx).await? {
            return Ok((0, true));
        }
    }
    target.check_path()?;
    if ctx.cancelled() {
        return Ok((0, true));
    }
    if ctx.mode == TransferMode::Replace {
        target.file.set_len(0).await.map_err(io_error)?;
    }
    target
        .file
        .seek(SeekFrom::Start(offset))
        .await
        .map_err(io_error)?;
    reader
        .seek(SeekFrom::Start(offset))
        .await
        .map_err(io_error)?;
    let mut copied = offset;
    let mut emitted = offset;
    let mut buffer = vec![0; SFTP_CHUNK];
    ctx.progress(base + copied, total);
    while copied < size {
        if ctx.cancelled() {
            target.file.flush().await.map_err(io_error)?;
            return Ok((copied, true));
        }
        let count = ((size - copied).min(SFTP_CHUNK as u64)) as usize;
        let read = reader.read(&mut buffer[..count]).await.map_err(io_error)?;
        if read == 0 {
            return Err(failure("源文件提前结束；部分文件已保留，未报告成功"));
        }
        if ctx.cancelled() {
            target.file.flush().await.map_err(io_error)?;
            return Ok((copied, true));
        }
        target.check_path()?;
        target
            .file
            .write_all(&buffer[..read])
            .await
            .map_err(io_error)?;
        copied += read as u64;
        if copied - emitted >= SFTP_PROGRESS_STEP {
            ctx.progress(base + copied, total);
            emitted = copied;
        }
    }
    target.file.flush().await.map_err(io_error)?;
    target.file.sync_all().await.map_err(io_error)?;
    target.check_path()?;
    // Read back both sides. Matching length is never a checksum shortcut.
    ctx.status("正在读回校验完整下载文件…".into());
    reader.seek(SeekFrom::Start(0)).await.map_err(io_error)?;
    target
        .file
        .seek(SeekFrom::Start(0))
        .await
        .map_err(io_error)?;
    if !compare_prefix(reader, &mut target.file, size, ctx).await? {
        return Ok((copied, true));
    }
    let mut extra = [0];
    if reader.read(&mut extra).await.map_err(io_error)? != 0
        || target.file.read(&mut extra).await.map_err(io_error)? != 0
    {
        return Err(failure("源或目标长度在传输期间变化"));
    }
    target.check_path()?;
    ctx.progress(base + copied, total);
    Ok((copied, ctx.cancelled()))
}

#[cfg(test)]
async fn guarded_download<R: AsyncRead + AsyncSeek + Unpin>(
    reader: &mut R,
    local: &Path,
    size: u64,
    ctx: &TransferCtx<'_>,
    base: u64,
    total: u64,
) -> Result<(u64, bool), SshError> {
    if ctx.cancelled() {
        return Ok((0, true));
    }
    let mut target = LocalFile::open(local, ctx.mode, true)?;
    guarded_download_file(reader, &mut target, size, ctx, base, total).await
}

pub(super) async fn download(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    ctx: &TransferCtx<'_>,
    base: u64,
    total: u64,
) -> Result<(u64, bool), SshError> {
    let mut input = sftp
        .open(remote)
        .await
        .map_err(|e| failure(e.to_string()))?;
    let before = input.metadata().await.map_err(|e| failure(e.to_string()))?;
    if !before.file_type().is_file() {
        return Err(failure("下载源必须是普通文件"));
    }
    let size = before
        .size
        .ok_or_else(|| failure("服务器未提供文件长度，无法安全传输"))?;
    if ctx.cancelled() {
        return Ok((0, true));
    }
    let mut target = LocalFile::open(local, ctx.mode, true)?;
    let result = guarded_download_file(&mut input, &mut target, size, ctx, base, total).await?;
    if result.1 {
        ctx.note(format!("已保留部分下载：{}", local.display()));
        return Ok(result);
    }
    let after = input.metadata().await.map_err(|e| failure(e.to_string()))?;
    let current = sftp
        .metadata(remote)
        .await
        .map_err(|e| failure(e.to_string()))?;
    if before.size != after.size
        || before.mtime != after.mtime
        || after.size != current.size
        || after.mtime != current.mtime
    {
        return Err(failure("远程源文件在传输期间变化，未报告成功"));
    }
    let mut named_source = sftp
        .open(remote)
        .await
        .map_err(|e| failure(e.to_string()))?;
    let mut named_target = LocalFile::open(local, TransferMode::Resume, false)?;
    if !compare_prefix(&mut named_source, &mut named_target.file, size, ctx).await? {
        return Ok((size, true));
    }
    let mut extra = [0];
    if named_source.read(&mut extra).await.map_err(io_error)? != 0 {
        return Err(failure("远程源路径已变化"));
    }
    named_target.check_source()?;
    target.check_path()?;
    Ok((result.0, ctx.cancelled()))
}

async fn hash<R: AsyncRead + Unpin>(
    reader: &mut R,
    ctx: &TransferCtx<'_>,
) -> Result<Option<[u8; 32]>, SshError> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0; SFTP_CHUNK];
    loop {
        if ctx.cancelled() {
            return Ok(None);
        }
        let read = reader.read(&mut buffer).await.map_err(io_error)?;
        if read == 0 {
            return Ok(Some(digest.finalize().into()));
        }
        digest.update(&buffer[..read]);
    }
}

// Some v3 servers implement RENAME with a racy stat + overwriting rename.
// Only exclusive OPEN protects a destination which appears concurrently.
async fn publish_exclusively(
    sftp: &SftpSession,
    from: &str,
    to: &str,
    expected: [u8; 32],
    ctx: &TransferCtx<'_>,
) -> Result<bool, SshError> {
    use russh_sftp::protocol::OpenFlags;
    if ctx.cancelled() {
        return Ok(false);
    }
    let mut input = sftp.open(from).await.map_err(|e| failure(e.to_string()))?;
    let attrs = input.metadata().await.map_err(|e| failure(e.to_string()))?;
    if !attrs.file_type().is_file() {
        return Err(failure("发布源已变化为非普通文件"));
    }
    let size = attrs.size.ok_or_else(|| failure("发布源缺少长度"))?;
    if ctx.cancelled() {
        return Ok(false);
    }
    let mut output = sftp
        .open_with_flags(
            to,
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::READ | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| failure(format!("目标被占用或无法排他创建，未覆盖：{e}")))?;
    ctx.note(format!(
        "发布中；中断时部分目标保留于 {to}，完整源保留于 {from}"
    ));
    let mut copied = 0;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; SFTP_CHUNK];
    while copied < size {
        if ctx.cancelled() {
            output.shutdown().await.map_err(io_error)?;
            return Ok(false);
        }
        let count = (size - copied).min(SFTP_CHUNK as u64) as usize;
        let read = input.read(&mut buffer[..count]).await.map_err(io_error)?;
        if read == 0 {
            return Err(failure("发布源提前结束；源和部分目标均保留"));
        }
        if ctx.cancelled() {
            output.shutdown().await.map_err(io_error)?;
            return Ok(false);
        }
        output.write_all(&buffer[..read]).await.map_err(io_error)?;
        digest.update(&buffer[..read]);
        copied += read as u64;
    }
    output.flush().await.map_err(io_error)?;
    let mut extra = [0];
    if input.read(&mut extra).await.map_err(io_error)? != 0
        || <[u8; 32]>::from(digest.finalize()) != expected
    {
        return Err(failure("发布源在校验后变化；源和部分目标均保留"));
    }
    output.seek(SeekFrom::Start(0)).await.map_err(io_error)?;
    let Some(actual) = hash(&mut output, ctx).await? else {
        output.shutdown().await.map_err(io_error)?;
        return Ok(false);
    };
    output.shutdown().await.map_err(io_error)?;
    if actual != expected {
        return Err(failure("发布目标读回不一致；源和目标均保留"));
    }
    let mut named = sftp.open(to).await.map_err(|e| failure(e.to_string()))?;
    let Some(actual) = hash(&mut named, ctx).await? else {
        return Ok(false);
    };
    if actual != expected {
        return Err(failure("发布目标路径在传输期间变化；未删除任何文件"));
    }
    Ok(!ctx.cancelled())
}

pub(super) async fn upload(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    ctx: &TransferCtx<'_>,
    base: u64,
    total: u64,
) -> Result<(u64, bool), SshError> {
    use russh_sftp::protocol::OpenFlags;
    let mut source = LocalFile::open(local, TransferMode::Resume, false)?;
    let size = source.initial.size;
    let exists = sftp
        .try_exists(remote)
        .await
        .map_err(|e| failure(e.to_string()))?;
    if exists && ctx.mode == TransferMode::CreateNew {
        return Err(failure("远程目标已存在，请确认覆盖或选择续传"));
    }
    if !exists && ctx.mode == TransferMode::Resume {
        return Err(failure("远程续传目标不存在"));
    }
    let mut offset = 0;
    let old_hash = if exists {
        let metadata = sftp
            .symlink_metadata(remote)
            .await
            .map_err(|e| failure(e.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(failure("拒绝覆盖远程目录、链接或设备"));
        }
        let mut old = sftp
            .open(remote)
            .await
            .map_err(|e| failure(e.to_string()))?;
        if ctx.mode == TransferMode::Resume {
            let length = metadata
                .size
                .ok_or_else(|| failure("服务器未提供续传偏移"))?;
            resume_offset(length, size)?;
            if !compare_prefix(&mut old, &mut source.file, length, ctx).await? {
                return Ok((0, true));
            }
            offset = length;
            old.seek(SeekFrom::Start(0)).await.map_err(io_error)?;
        }
        let Some(digest) = hash(&mut old, ctx).await? else {
            return Ok((0, true));
        };
        Some(digest)
    } else {
        None
    };
    source
        .file
        .seek(SeekFrom::Start(offset))
        .await
        .map_err(io_error)?;
    source.check_source()?;
    let token = uuid::Uuid::new_v4();
    let staging = format!("{remote}.adit-part-{token}");
    ctx.note(format!("上传暂存文件：{staging}"));
    // Never truncate a path supplied by the user or found in an old listing.
    let mut output = sftp
        .open_with_flags(
            &staging,
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| failure(e.to_string()))?;
    if offset > 0 {
        let old = sftp
            .open(remote)
            .await
            .map_err(|e| failure(e.to_string()))?;
        let (prefix, cancelled) =
            stream_copy(&mut old.take(offset), &mut output, ctx, base, total).await?;
        if cancelled || prefix != offset {
            output.shutdown().await.map_err(io_error)?;
            ctx.note(format!("已有前缀变化或传输停止；暂存 {staging}"));
            if cancelled {
                return Ok((prefix, true));
            }
            return Err(failure("远程续传前缀在校验后缩短"));
        }
    }
    let (tail, cancelled) =
        stream_copy(&mut source.file, &mut output, ctx, base + offset, total).await?;
    let bytes = offset + tail;
    output.shutdown().await.map_err(io_error)?;
    source.check_source()?;
    if cancelled {
        ctx.note(format!("上传已停止；部分数据保留于 {staging}"));
        return Ok((bytes, true));
    }
    if bytes != size {
        return Err(failure(format!("上传源长度变化；暂存文件保留于 {staging}")));
    }
    source
        .file
        .seek(SeekFrom::Start(0))
        .await
        .map_err(io_error)?;
    let mut check = sftp
        .open(&staging)
        .await
        .map_err(|e| failure(e.to_string()))?;
    ctx.status("正在读回校验远程暂存文件…".into());
    if !compare_prefix(&mut check, &mut source.file, size, ctx).await? {
        return Ok((bytes, true));
    }
    let mut extra = [0];
    if check.read(&mut extra).await.map_err(io_error)? != 0 {
        return Err(failure("远程暂存长度不符"));
    }
    check.shutdown().await.map_err(io_error)?;
    source.check_source()?;
    source
        .file
        .seek(SeekFrom::Start(0))
        .await
        .map_err(io_error)?;
    let Some(staging_hash) = hash(&mut source.file, ctx).await? else {
        return Ok((bytes, true));
    };
    source.check_source()?;
    if ctx.cancelled() {
        return Ok((bytes, true));
    }
    if let Some(expected) = old_hash {
        let backup = format!("{remote}.adit-original-{token}");
        // The unpredictable backup name belongs to this transfer. Publication
        // and recovery to the shared target path must instead use exclusive OPEN.
        sftp.rename(remote, &backup)
            .await
            .map_err(|e| failure(e.to_string()))?;
        ctx.note(format!("原远程文件已移至备份 {backup}；暂存 {staging}"));
        let verification = async {
            let metadata = sftp
                .symlink_metadata(&backup)
                .await
                .map_err(|e| failure(e.to_string()))?;
            if !metadata.file_type().is_file() {
                return Err(failure("原目标被替换为链接或其他类型"));
            }
            let mut moved = sftp
                .open(&backup)
                .await
                .map_err(|e| failure(e.to_string()))?;
            hash(&mut moved, ctx).await
        }
        .await;
        if ctx.cancelled() || !matches!(&verification, Ok(Some(actual)) if *actual == expected) {
            ctx.note(format!("原数据保留于 {backup}；暂存 {staging}"));
            if ctx.cancelled() || matches!(verification, Ok(None)) {
                return Ok((bytes, true));
            }
            if let Ok(Some(actual)) = verification {
                match publish_exclusively(sftp, &backup, remote, actual, ctx).await {
                    Ok(true) => ctx.note(format!(
                        "变化后的原数据已恢复至 {remote}；备份仍保留于 {backup}"
                    )),
                    Ok(false) => return Ok((bytes, true)),
                    Err(error) => {
                        ctx.note(format!("未覆盖当前目标；恢复失败：{error}；备份 {backup}"))
                    }
                }
            }
            return Err(failure(format!(
                "远程目标在传输期间变化；拒绝覆盖，原数据保留于 {backup}"
            )));
        }
        match publish_exclusively(sftp, &staging, remote, staging_hash, ctx).await {
            Ok(true) => {}
            Ok(false) => return Ok((bytes, true)),
            Err(error) => {
                return Err(failure(format!(
                    "发布失败，未删除任何文件：{error}；备份 {backup}，暂存 {staging}"
                )))
            }
        }
        // Keep the backup: another process can still hold a writable remote
        // handle, and deleting it would discard a concurrent writer's data.
        ctx.note(format!("原远程文件保留于 {backup}"));
    } else {
        if !publish_exclusively(sftp, &staging, remote, staging_hash, ctx)
            .await
            .map_err(|e| failure(format!("发布失败：{e}；暂存 {staging}")))?
        {
            return Ok((bytes, true));
        }
        ctx.note(format!("已校验并发布到 {remote}"));
    }
    source.check_source()?;
    Ok((bytes, ctx.cancelled()))
}

#[cfg(test)]
mod tests {
    mod protocol;
    use super::*;
    use std::{
        io::Cursor,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncSeek, ReadBuf};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = env::temp_dir().join(format!("adit-sftp-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }
        fn file(&self) -> PathBuf {
            self.0.join("target")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            // Only this UUID-named fixture directory is owned by the test.
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn cancel_set() -> CancelSet {
        Arc::new(Mutex::new(HashSet::new()))
    }

    struct TriggerReader {
        bytes: Cursor<Vec<u8>>,
        at: u64,
        action: Option<Box<dyn FnOnce() + Send>>,
    }
    impl AsyncRead for TriggerReader {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.bytes.position() >= this.at {
                if let Some(action) = this.action.take() {
                    action();
                }
            }
            Pin::new(&mut this.bytes).poll_read(cx, buf)
        }
    }
    impl AsyncSeek for TriggerReader {
        fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
            Pin::new(&mut self.get_mut().bytes).start_seek(position)
        }
        fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
            Pin::new(&mut self.get_mut().bytes).poll_complete(cx)
        }
    }

    #[test]
    fn resume_offsets_are_bounded_and_same_length_is_not_a_proof() {
        assert_eq!(resume_offset(3, 10).unwrap(), 3);
        assert_eq!(resume_offset(10, 10).unwrap(), 10);
        assert!(resume_offset(11, 10).is_err());
        runtime().block_on(async {
            let scratch = Scratch::new();
            let path = scratch.file();
            fs::write(&path, b"wrong").unwrap();
            let cancel = cancel_set();
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 4,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::Resume,
            };
            assert!(
                guarded_download(&mut Cursor::new(b"right".to_vec()), &path, 5, &ctx, 0, 5)
                    .await
                    .is_err()
            );
            assert_eq!(fs::read(&path).unwrap(), b"wrong");
        });
    }

    #[test]
    fn resumed_bytes_progress_and_complete_content_are_verified() {
        runtime().block_on(async {
            let scratch = Scratch::new();
            let path = scratch.file();
            fs::write(&path, b"abc").unwrap();
            let cancel = cancel_set();
            let (tx, rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 7,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::Resume,
            };
            assert_eq!(
                guarded_download(
                    &mut Cursor::new(b"abcdefgh".to_vec()),
                    &path,
                    8,
                    &ctx,
                    10,
                    18
                )
                .await
                .unwrap(),
                (8, false)
            );
            assert_eq!(fs::read(&path).unwrap(), b"abcdefgh");
            assert!(rx.try_iter().any(|e| matches!(
                e,
                SftpEvent::Progress {
                    id: 7,
                    done: 13,
                    total: 18,
                    ..
                }
            )));
            assert_eq!(
                guarded_download(&mut Cursor::new(b"abcdefgh".to_vec()), &path, 8, &ctx, 0, 8)
                    .await
                    .unwrap(),
                (8, false)
            );
        });
    }

    #[test]
    fn cancellation_after_read_never_writes_the_next_chunk_and_preserves_resume_prefix() {
        runtime().block_on(async {
            let scratch = Scratch::new();
            let path = scratch.file();
            fs::write(&path, b"abc").unwrap();
            let cancel = cancel_set();
            let trigger = Arc::clone(&cancel);
            let mut reader = TriggerReader {
                bytes: Cursor::new(b"abcdefgh".to_vec()),
                at: 3,
                action: Some(Box::new(move || {
                    trigger.lock().unwrap().insert(9);
                })),
            };
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 9,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::Resume,
            };
            assert_eq!(
                guarded_download(&mut reader, &path, 8, &ctx, 0, 8)
                    .await
                    .unwrap(),
                (3, true)
            );
            assert_eq!(fs::read(&path).unwrap(), b"abc");
            forget_transfer(&cancel, 9);
            assert!(!is_cancelled(&cancel, 9));
        });
    }

    #[test]
    fn a_file_appearing_at_an_unapproved_target_is_never_truncated() {
        runtime().block_on(async {
            let scratch = Scratch::new();
            let path = scratch.file();
            fs::write(&path, b"keep me").unwrap();
            let cancel = cancel_set();
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 1,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::CreateNew,
            };
            assert!(
                guarded_download(&mut Cursor::new(b"replace".to_vec()), &path, 7, &ctx, 0, 7)
                    .await
                    .is_err()
            );
            assert_eq!(fs::read(&path).unwrap(), b"keep me");
        });
    }

    #[test]
    fn queued_cancellation_does_not_even_create_the_destination() {
        runtime().block_on(async {
            let scratch = Scratch::new();
            let path = scratch.file();
            let cancel = cancel_set();
            cancel.lock().unwrap().insert(10);
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 10,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::CreateNew,
            };
            assert_eq!(
                guarded_download(&mut Cursor::new(b"data".to_vec()), &path, 4, &ctx, 0, 4)
                    .await
                    .unwrap(),
                (0, true)
            );
            assert!(!path.exists());
        });
    }

    #[test]
    fn target_replacement_is_detected_or_denied_without_destroying_the_replacement() {
        runtime().block_on(async {
            let scratch = Scratch::new();
            let path = scratch.file();
            fs::write(&path, b"old").unwrap();
            let target = path.clone();
            let backup = scratch.0.join("moved");
            let replaced = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = Arc::clone(&replaced);
            let mut reader = TriggerReader {
                bytes: Cursor::new(b"new contents".to_vec()),
                at: 0,
                action: Some(Box::new(move || {
                    // Windows denies this rename while the writer handle is open.
                    // Unix permits it; the per-chunk identity check must catch it.
                    if fs::rename(&target, &backup).is_ok() {
                        fs::write(&target, b"replacement").unwrap();
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })),
            };
            let cancel = cancel_set();
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 3,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::Replace,
            };
            let result = guarded_download(&mut reader, &path, 12, &ctx, 0, 12).await;
            if replaced.load(std::sync::atomic::Ordering::SeqCst) {
                assert!(result.is_err());
                assert_eq!(fs::read(&path).unwrap(), b"replacement");
            } else {
                assert_eq!(result.unwrap(), (12, false));
                assert_eq!(fs::read(&path).unwrap(), b"new contents");
            }
        });
    }

    #[test]
    fn source_shortening_and_growth_are_errors_not_successful_transfers() {
        runtime().block_on(async {
            let scratch = Scratch::new();
            let cancel = cancel_set();
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 5,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::Replace,
            };
            for (data, size) in [(b"short".to_vec(), 10), (b"too long".to_vec(), 3)] {
                assert!(guarded_download(
                    &mut Cursor::new(data),
                    &scratch.file(),
                    size,
                    &ctx,
                    0,
                    size
                )
                .await
                .is_err());
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_and_parent_directories_are_rejected_without_touching_the_referent() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        let original = scratch.0.join("original");
        fs::write(&original, b"do not touch").unwrap();
        symlink(&original, scratch.file()).unwrap();
        assert!(LocalFile::open(&scratch.file(), TransferMode::Replace, true).is_err());
        assert_eq!(fs::read(&original).unwrap(), b"do not touch");
        let outside = scratch.0.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, scratch.0.join("link")).unwrap();
        assert!(LocalFile::open(
            &scratch.0.join("link").join("file"),
            TransferMode::CreateNew,
            true
        )
        .is_err());
        assert!(!outside.join("file").exists());
    }
}
