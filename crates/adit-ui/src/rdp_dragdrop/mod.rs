//! Explicit OLE file drags. No clipboard polling, no synthetic paste, no writes
//! to local paths: the destination shell owns its copy/overwrite confirmation.
use crate::clipboard_files::ChunkBridge;
use crate::i18n::t;
use adit_domain::SessionId;
use adit_session::{ClipFile, RdpFileDragEvent};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

#[cfg(windows)]
mod native;

type Request = Arc<dyn Fn(u32, u32, u64, u32) + Send + Sync>;

struct Offer {
    generation: u64,
    files: Vec<ClipFile>,
    request: Request,
    current: Arc<dyn Fn() -> bool + Send + Sync>,
}

#[derive(Clone)]
pub(super) struct Lease {
    session: SessionId,
    generation: u64,
    current: Arc<dyn Fn() -> bool + Send + Sync>,
    bridge: ChunkBridge,
    cancelled: Arc<AtomicBool>,
}

impl Lease {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.bridge.close();
    }
}

pub(crate) struct State {
    offers: HashMap<SessionId, Offer>,
    active: Option<Lease>,
    notices: mpsc::Receiver<(SessionId, String)>,
    #[cfg_attr(not(windows), allow(dead_code))]
    notice_tx: mpsc::Sender<(SessionId, String)>,
}

impl Default for State {
    fn default() -> Self {
        let (notice_tx, notices) = mpsc::channel();
        Self {
            offers: HashMap::new(),
            active: None,
            notices,
            notice_tx,
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl State {
    pub(crate) fn cancel(&mut self) {
        if let Some(lease) = self.active.take() {
            lease.cancel();
        }
    }

    pub(crate) fn update(
        &mut self,
        events: Vec<RdpFileDragEvent>,
        enabled: bool,
        live: impl Fn(SessionId) -> bool,
    ) -> Option<(SessionId, String)> {
        // Revoke a snapshot before delivering any replies in the same batch.
        // A reply queued just before a replacement offer must not wake a stream
        // after the remote selection changed.
        if self.active.as_ref().is_some_and(|active| {
            events.iter().any(|event| match event {
                RdpFileDragEvent::Offered { session, .. }
                | RdpFileDragEvent::Closed { session } => *session == active.session,
                _ => false,
            })
        }) {
            self.cancel();
        }
        for event in events {
            match event {
                RdpFileDragEvent::Offered {
                    session,
                    generation,
                    files,
                    requester,
                } => {
                    if requester.generation() != generation {
                        self.offers.remove(&session);
                        continue;
                    }
                    if self.active.as_ref().is_some_and(|a| a.session == session) {
                        self.cancel();
                    }
                    let current = requester.clone();
                    self.offers.insert(
                        session,
                        Offer {
                            generation,
                            current: Arc::new(move || current.is_current()),
                            files,
                            request: Arc::new(move |s, i, o, n| requester.request(s, i, o, n)),
                        },
                    );
                }
                RdpFileDragEvent::Chunk {
                    session,
                    generation,
                    stream_id,
                    data,
                } => {
                    if let Some(active) = self
                        .active
                        .as_ref()
                        .filter(|a| a.session == session && a.generation == generation)
                    {
                        active.bridge.deliver(stream_id, data);
                    }
                }
                RdpFileDragEvent::Closed { session } => {
                    self.offers.remove(&session);
                    if self.active.as_ref().is_some_and(|a| a.session == session) {
                        self.cancel();
                    }
                }
            }
        }
        self.offers.retain(|id, _| enabled && live(*id));
        if !enabled
            || self
                .active
                .as_ref()
                .is_some_and(|a| !live(a.session) || !(a.current)())
        {
            self.cancel();
        }
        self.notices.try_iter().last()
    }

    /// `owner` is obtained from the specific iced window, never a foreground
    /// window lookup. Multi-window callers supply their own session binding.
    pub(crate) fn open(
        &mut self,
        session: SessionId,
        owner: isize,
        title: String,
    ) -> Result<(), &'static str> {
        let offer = self.offers.get(&session).ok_or(t(
            "请先在该远端会话中选择并复制普通文件，再打开文件拖出窗口",
        ))?;
        validate_files(&offer.files)?;
        if !(offer.current)() {
            return Err(t("该远端文件选择已失效，请重新复制后拖出"));
        }
        let generation = offer.generation;
        let current = offer.current.clone();
        let files = offer.files.clone();
        let request = offer.request.clone();
        self.cancel();
        #[cfg(windows)]
        let lease = Lease {
            session,
            generation,
            current,
            bridge: ChunkBridge::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        #[cfg(windows)]
        native::open(
            owner,
            title,
            files,
            request,
            lease.clone(),
            self.notice_tx.clone(),
        )?;
        #[cfg(not(windows))]
        {
            let _ = (owner, title, files, request, generation, current);
            Err(t("OLE 文件拖放仅支持 Windows"))
        }
        #[cfg(windows)]
        {
            self.active = Some(lease);
            Ok(())
        }
    }
}

pub(crate) fn owner_task(
    window: iced::window::Id,
    session: SessionId,
) -> iced::Task<crate::Message> {
    iced::window::run(window, move |native| {
        #[cfg(windows)]
        let owner = native.window_handle().ok().and_then(|h| match h.as_raw() {
            iced::window::raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        });
        #[cfg(not(windows))]
        let owner = {
            let _ = native;
            None
        };
        crate::Message::RdpFileDragOwner { session, owner }
    })
}

/// A conservative first slice: no directories, virtual hierarchies, device
/// names, duplicate destinations or truncated descriptors.
pub(crate) fn validate_files(files: &[ClipFile]) -> Result<(), &'static str> {
    if files.is_empty() {
        return Err(t("当前没有可安全拖出的文件；文件描述已拒绝或为空"));
    }
    if files.len() > 256 {
        return Err(t("一次拖放仅支持 1–256 个普通文件"));
    }
    adit_session::validate_clipboard_file_list(files)?;
    let mut total = 0u64;
    for file in files {
        // ClipFile represents an absent size as zero; do not mistake an unknown
        // virtual file for a successfully transferred empty file.
        if file.is_dir || file.size == 0 || file.name.contains(['/', '\\']) {
            return Err(t(
                "仅支持已知大小的非空普通文件；目录、未知大小和虚拟路径已拒绝",
            ));
        }
        total = total.checked_add(file.size).ok_or(t("文件描述长度溢出"))?;
        if file.size > 16 * 1024 * 1024 * 1024 || total > 64 * 1024 * 1024 * 1024 {
            return Err(t("拖放单文件上限 16 GiB，一次总量上限 64 GiB"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
