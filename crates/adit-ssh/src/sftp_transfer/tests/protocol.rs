//! Exercise the real SFTP client over an in-memory duplex stream. No listener,
//! SSH authentication, known_hosts, real server, or installed state is involved.
use super::*;
use russh_sftp::{
    protocol::{Attrs, Data, FileAttributes, Handle, OpenFlags, Status, StatusCode},
    server::Handler,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct State {
    files: BTreeMap<String, Vec<u8>>,
    replace_at_close: Option<Vec<u8>>,
    cancel_at_close: Option<(CancelSet, u64)>,
    overwrite_rename: bool,
    target_after_backup: Option<Vec<u8>>,
    cancel_after_backup: Option<(CancelSet, u64)>,
    corrupt_publication: bool,
}
struct Server(Arc<Mutex<State>>);

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en".into(),
    }
}

impl Server {
    fn attrs(&self, id: u32, path: &str) -> Result<Attrs, StatusCode> {
        let state = self.0.lock().unwrap();
        let bytes = state.files.get(path).ok_or(StatusCode::NoSuchFile)?;
        let mut attrs = FileAttributes::empty();
        attrs.size = Some(bytes.len() as u64);
        attrs.permissions = Some(0o100644);
        attrs.mtime = Some(7);
        Ok(Attrs { id, attrs })
    }
}

impl Handler for Server {
    type Error = StatusCode;
    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let mut state = self.0.lock().unwrap();
        if flags.contains(OpenFlags::EXCLUDE) && state.files.contains_key(&filename) {
            return Err(StatusCode::Failure);
        }
        if flags.contains(OpenFlags::CREATE) {
            state.files.entry(filename.clone()).or_default();
        }
        if !state.files.contains_key(&filename) {
            return Err(StatusCode::NoSuchFile);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            state.files.get_mut(&filename).unwrap().clear();
        }
        Ok(Handle {
            id,
            handle: filename,
        })
    }
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        if handle.contains(".adit-part-") {
            let mut state = self.0.lock().unwrap();
            if let Some(replacement) = state.replace_at_close.take() {
                state.files.insert("/target".into(), replacement);
            }
            if let Some((cancel, id)) = state.cancel_at_close.take() {
                cancel.lock().unwrap().insert(id);
            }
        }
        Ok(ok(id))
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        let state = self.0.lock().unwrap();
        let bytes = state.files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        if offset >= bytes.len() {
            return Err(StatusCode::Eof);
        }
        let end = bytes.len().min(offset.saturating_add(len as usize));
        Ok(Data {
            id,
            data: bytes[offset..end].to_vec(),
        })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let mut state = self.0.lock().unwrap();
        let bytes = state.files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        let end = offset.checked_add(data.len()).ok_or(StatusCode::Failure)?;
        bytes.resize(bytes.len().max(end), 0);
        bytes[offset..end].copy_from_slice(&data);
        if handle == "/target" && state.corrupt_publication && !data.is_empty() {
            state.files.get_mut(&handle).unwrap()[offset] ^= 1;
        }
        Ok(ok(id))
    }
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        self.attrs(id, &path)
    }
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        self.attrs(id, &path)
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        self.attrs(id, &handle)
    }
    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, StatusCode> {
        let mut state = self.0.lock().unwrap();
        if !state.overwrite_rename && state.files.contains_key(&to) {
            return Err(StatusCode::Failure);
        }
        let bytes = state.files.remove(&from).ok_or(StatusCode::NoSuchFile)?;
        let is_backup = to.contains(".adit-original-");
        state.files.insert(to, bytes);
        if is_backup {
            if let Some(bytes) = state.target_after_backup.take() {
                state.files.insert("/target".into(), bytes);
            }
            if let Some((cancel, id)) = state.cancel_after_backup.take() {
                cancel.lock().unwrap().insert(id);
            }
        }
        Ok(ok(id))
    }
}

async fn client(state: Arc<Mutex<State>>) -> SftpSession {
    let (client, server) = tokio::io::duplex(1024 * 1024);
    russh_sftp::server::run(server, Server(state)).await;
    let client = SftpSession::new(client).await.unwrap();
    client.set_timeout(2);
    client
}

#[test]
fn upload_resume_uses_verified_offset_and_preserves_original_partial_backup() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"abcdefgh").unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        state
            .lock()
            .unwrap()
            .files
            .insert("/target".into(), b"abc".to_vec());
        let client = client(Arc::clone(&state)).await;
        let cancel = cancel_set();
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 20,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::Resume,
        };
        assert_eq!(
            upload(&client, &scratch.file(), "/target", &ctx, 0, 8)
                .await
                .unwrap(),
            (8, false)
        );
        let state = state.lock().unwrap();
        assert_eq!(state.files["/target"], b"abcdefgh");
        assert!(state
            .files
            .iter()
            .any(|(name, bytes)| name.contains(".adit-original-") && bytes == b"abc"));
    });
}

#[test]
fn a_same_size_wrong_remote_file_cannot_be_reported_as_resumed() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"right").unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        state
            .lock()
            .unwrap()
            .files
            .insert("/target".into(), b"wrong".to_vec());
        let client = client(Arc::clone(&state)).await;
        let cancel = cancel_set();
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 21,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::Resume,
        };
        assert!(upload(&client, &scratch.file(), "/target", &ctx, 0, 5)
            .await
            .is_err());
        assert_eq!(state.lock().unwrap().files["/target"], b"wrong");
    });
}

#[test]
fn a_remote_target_replaced_during_upload_is_restored_and_never_overwritten() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"new").unwrap();
        let state = Arc::new(Mutex::new(State {
            files: BTreeMap::from([("/target".into(), b"old".to_vec())]),
            replace_at_close: Some(b"XYZ".to_vec()),
            ..State::default()
        }));
        let client = client(Arc::clone(&state)).await;
        let cancel = cancel_set();
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 22,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::Replace,
        };
        assert!(upload(&client, &scratch.file(), "/target", &ctx, 0, 3)
            .await
            .is_err());
        assert_eq!(state.lock().unwrap().files["/target"], b"XYZ");
    });
}

#[test]
fn a_remote_target_appearing_after_preflight_blocks_exclusive_publication() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"new").unwrap();
        let state = Arc::new(Mutex::new(State {
            replace_at_close: Some(b"appeared".to_vec()),
            ..State::default()
        }));
        let client = client(Arc::clone(&state)).await;
        let cancel = cancel_set();
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 23,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::CreateNew,
        };
        assert!(upload(&client, &scratch.file(), "/target", &ctx, 0, 3)
            .await
            .is_err());
        assert_eq!(state.lock().unwrap().files["/target"], b"appeared");
    });
}

#[test]
fn cancellation_during_upload_readback_never_publishes_an_unverified_file() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"new contents").unwrap();
        let cancel = cancel_set();
        let state = Arc::new(Mutex::new(State {
            cancel_at_close: Some((Arc::clone(&cancel), 24)),
            ..State::default()
        }));
        let client = client(Arc::clone(&state)).await;
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 24,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::CreateNew,
        };
        assert_eq!(
            upload(&client, &scratch.file(), "/target", &ctx, 0, 12)
                .await
                .unwrap(),
            (12, true)
        );
        let state = state.lock().unwrap();
        assert!(!state.files.contains_key("/target"));
        assert!(state
            .files
            .iter()
            .any(|(name, contents)| name.contains(".adit-part-") && contents == b"new contents"));
    });
}

#[test]
fn overwriting_rename_server_cannot_clobber_a_target_created_after_backup() {
    runtime().block_on(async {
        for replacement in [None, Some(b"changed before backup".to_vec())] {
            let scratch = Scratch::new();
            fs::write(scratch.file(), b"new").unwrap();
            let original = replacement.as_deref().unwrap_or(b"old").to_vec();
            let state = Arc::new(Mutex::new(State {
                files: BTreeMap::from([("/target".into(), b"old".to_vec())]),
                replace_at_close: replacement,
                overwrite_rename: true,
                target_after_backup: Some(b"third party".to_vec()),
                ..State::default()
            }));
            let client = client(Arc::clone(&state)).await;
            let cancel = cancel_set();
            let (tx, _rx) = mpsc::channel();
            let ctx = TransferCtx {
                id: 25,
                label: "fixture",
                cancel: &cancel,
                events: &tx,
                mode: TransferMode::Replace,
            };
            assert!(upload(&client, &scratch.file(), "/target", &ctx, 0, 3)
                .await
                .is_err());
            let state = state.lock().unwrap();
            assert_eq!(state.files["/target"], b"third party");
            assert!(state
                .files
                .iter()
                .any(|(path, bytes)| path.contains(".adit-original-") && bytes == &original));
            assert!(state
                .files
                .iter()
                .any(|(path, bytes)| path.contains(".adit-part-") && bytes == b"new"));
        }
    });
}

#[test]
fn cancellation_after_backup_preserves_it_without_starting_recovery_or_publication() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"new").unwrap();
        let cancel = cancel_set();
        let state = Arc::new(Mutex::new(State {
            files: BTreeMap::from([("/target".into(), b"old".to_vec())]),
            cancel_after_backup: Some((Arc::clone(&cancel), 26)),
            ..State::default()
        }));
        let client = client(Arc::clone(&state)).await;
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 26,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::Replace,
        };
        assert_eq!(
            upload(&client, &scratch.file(), "/target", &ctx, 0, 3)
                .await
                .unwrap(),
            (3, true)
        );
        let state = state.lock().unwrap();
        assert!(!state.files.contains_key("/target"));
        assert!(state
            .files
            .iter()
            .any(|(path, bytes)| path.contains(".adit-original-") && bytes == b"old"));
    });
}

#[test]
fn publication_readback_corruption_is_an_error_with_sources_preserved() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        fs::write(scratch.file(), b"new").unwrap();
        let state = Arc::new(Mutex::new(State {
            corrupt_publication: true,
            ..State::default()
        }));
        let client = client(Arc::clone(&state)).await;
        let cancel = cancel_set();
        let (tx, _rx) = mpsc::channel();
        let ctx = TransferCtx {
            id: 27,
            label: "fixture",
            cancel: &cancel,
            events: &tx,
            mode: TransferMode::CreateNew,
        };
        assert!(upload(&client, &scratch.file(), "/target", &ctx, 0, 3)
            .await
            .is_err());
        let state = state.lock().unwrap();
        assert_ne!(state.files["/target"], b"new");
        assert!(state
            .files
            .iter()
            .any(|(path, bytes)| path.contains(".adit-part-") && bytes == b"new"));
    });
}
