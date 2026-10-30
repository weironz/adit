//! Explicit OLE file drags. No clipboard polling, no synthetic paste, no writes
//! to local paths: the destination shell owns its copy/overwrite confirmation.
use crate::clipboard_files::ChunkBridge;
use crate::i18n::t;
use adit_domain::SessionId;
use adit_session::{ClipFile, RdpFileDragEvent};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};

#[cfg(windows)]
mod native;

type Request = Arc<dyn Fn(u32, u32, u64, u32) -> Result<(), ()> + Send + Sync>;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Notice {
    Opened = 1,
    InboundUnsupported = 2,
    Accepted = 3,
    Cancelled = 4,
}

impl Notice {
    fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Opened),
            2 => Some(Self::InboundUnsupported),
            3 => Some(Self::Accepted),
            4 => Some(Self::Cancelled),
            _ => None,
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Opened => "已打开文件拖出窗口；尚未读取文件内容",
            Self::InboundUnsupported => {
                "当前仅支持远端文件拖出；拖入远端尚不支持，未读取或传输文件"
            }
            Self::Accepted => "OLE 目标已接受文件复制；目标文件由资源管理器处理",
            Self::Cancelled => "文件拖放已取消",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum NoticeError {
    Open = 1,
    InvalidFiles = 2,
    Incomplete = 4,
}

impl NoticeError {
    fn from_bits(bits: u8) -> Option<Self> {
        if bits & Self::Incomplete as u8 != 0 {
            Some(Self::Incomplete)
        } else if bits & Self::InvalidFiles as u8 != 0 {
            Some(Self::InvalidFiles)
        } else if bits & Self::Open as u8 != 0 {
            Some(Self::Open)
        } else {
            None
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Open => "无法打开原生文件拖放窗口；未传输文件",
            Self::InvalidFiles => "文件描述无效，已拒绝拖放",
            Self::Incomplete => "文件拖放未完成；未请求移动或删除远端文件",
        }
    }
}

struct PendingNotice {
    text: Option<String>,
    errors: u8,
}

impl PendingNotice {
    fn merge(&mut self, next: Self) {
        self.errors |= next.errors;
        if self.errors != 0 {
            self.text = None;
        } else {
            self.text = next.text;
        }
    }

    fn take(&mut self) -> Option<String> {
        if let Some(error) = NoticeError::from_bits(self.errors) {
            self.errors &= !(error as u8);
            Some(error.text().into())
        } else {
            self.text.take()
        }
    }

    fn is_empty(&self) -> bool {
        self.errors == 0 && self.text.is_none()
    }
}

/// Each live session has one fixed-size slot, regardless of how long the UI
/// stops polling. OLE threads only perform atomic operations; errors have a
/// separate latch so a later success or cancellation cannot overwrite them.
#[derive(Default)]
struct NoticeSlot {
    status: AtomicU8,
    errors: AtomicU8,
    #[cfg(test)]
    test_message: std::sync::Mutex<Option<String>>,
}

impl NoticeSlot {
    #[cfg(any(windows, test))]
    fn publish(&self, notice: Notice) {
        self.status.store(notice as u8, Ordering::Release);
    }

    #[cfg(any(windows, test))]
    fn fail(&self, error: NoticeError) {
        self.errors.fetch_or(error as u8, Ordering::Release);
    }

    fn take(&self) -> Option<PendingNotice> {
        let status = self.status.swap(0, Ordering::AcqRel);
        let errors = self.errors.swap(0, Ordering::AcqRel);
        #[cfg(test)]
        let test_message = self
            .test_message
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if errors != 0 {
            // A status published while the error was being read must not be
            // delivered on the next tick as the outcome of the failed drag.
            self.status.swap(0, Ordering::AcqRel);
            Some(PendingNotice { text: None, errors })
        } else {
            #[cfg(test)]
            if let Some(text) = test_message {
                return Some(PendingNotice {
                    text: Some(text),
                    errors: 0,
                });
            }
            Notice::from_byte(status).map(|notice| PendingNotice {
                text: Some(notice.text().into()),
                errors: 0,
            })
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    offers: HashMap<SessionId, Offer>,
    active: Option<Lease>,
    notices: RefCell<HashMap<SessionId, Arc<NoticeSlot>>>,
    pending_notices: HashMap<SessionId, PendingNotice>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl State {
    #[cfg(any(windows, test))]
    fn notice_slot(&self, session: SessionId) -> Arc<NoticeSlot> {
        self.notices
            .borrow_mut()
            .entry(session)
            .or_default()
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn test_notice(&self, session: SessionId, message: &str) {
        *self
            .notice_slot(session)
            .test_message
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(message.into());
    }

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
    ) {
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
                            request: Arc::new(move |s, i, o, n| {
                                requester.try_request(s, i, o, n).map_err(|_| ())
                            }),
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
        // A tick from any window drains the slots. Keep each session's result
        // until its selected window can display it, with errors taking priority.
        self.pending_notices.retain(|session, _| live(*session));
        let pending = &mut self.pending_notices;
        self.notices.get_mut().retain(|session, slot| {
            if !live(*session) {
                return false;
            }
            if let Some(notice) = slot.take() {
                if let Some(previous) = pending.get_mut(session) {
                    previous.merge(notice);
                } else {
                    pending.insert(*session, notice);
                }
            }
            true
        });
    }

    pub(crate) fn take_notice(&mut self, session: SessionId) -> Option<String> {
        let pending = self.pending_notices.get_mut(&session)?;
        let notice = pending.take();
        if pending.is_empty() {
            self.pending_notices.remove(&session);
        }
        notice
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
            self.notice_slot(session),
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
