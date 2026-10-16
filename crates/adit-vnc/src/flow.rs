//! At most one application refresh request is outstanding, including initial paint.
use std::sync::atomic::{AtomicBool, Ordering};
#[derive(Debug, Default)]
pub(crate) struct Flow {
    outstanding: AtomicBool,
    full: AtomicBool,
    deferred: AtomicBool,
}
impl Flow {
    pub(crate) fn initial() -> Self {
        Self {
            outstanding: AtomicBool::new(true),
            full: AtomicBool::new(false),
            deferred: AtomicBool::new(false),
        }
    }
    pub(crate) fn request(&self, full: bool) -> Option<bool> {
        if full {
            self.full.store(true, Ordering::Release);
        }
        if self
            .outstanding
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        self.deferred.store(false, Ordering::Release);
        Some(self.full.swap(false, Ordering::AcqRel))
    }
    pub(crate) fn request_resize_refresh(&self) -> Option<bool> {
        self.deferred.store(true, Ordering::Release);
        self.request(false)
    }
    pub(crate) fn needs_followup(&self) -> bool {
        self.has_full() || self.deferred.load(Ordering::Acquire)
    }
    pub(crate) fn complete(&self) {
        self.outstanding.store(false, Ordering::Release);
    }
    pub(crate) fn has_full(&self) -> bool {
        self.full.load(Ordering::Acquire)
    }
    pub(crate) fn pending(&self) -> u8 {
        u8::from(self.outstanding.load(Ordering::Acquire))
    }
}

pub(crate) struct CountingRead<S> {
    pub inner: S,
    pub bytes: u64,
}
impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountingRead<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        self.bytes += (buf.filled().len() - before) as u64;
        result
    }
}
