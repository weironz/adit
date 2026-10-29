//! Preserve the transport while russh's initial host-key callback is pending.
use super::{client, KnownHostsClient, SshError};
use std::{
    collections::VecDeque,
    future::poll_fn,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Wake, Waker},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// A peer may pipeline packets while the user is deciding. Do not let it grow
// memory without bound, and do not silently discard any of those packets.
const MAX_TRUST_BUFFER: usize = 256 * 1024;
const MAX_READ_POLLS_PER_CHECK: usize = 4096;

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

struct Inner {
    stream: Box<dyn Transport>,
    buffered: VecDeque<u8>,
}

struct TrustStream(Arc<Mutex<Inner>>);

struct ReadWake {
    notified: AtomicBool,
    parent: Waker,
}

impl Wake for ReadWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified.store(true, Ordering::Release);
        self.parent.wake_by_ref();
    }
}

#[derive(Clone)]
pub(super) struct TrustMonitor(Arc<Mutex<Inner>>);

impl TrustStream {
    fn new(stream: impl Transport + 'static) -> (Self, TrustMonitor) {
        let inner = Arc::new(Mutex::new(Inner {
            stream: Box::new(stream),
            buffered: VecDeque::new(),
        }));
        (Self(inner.clone()), TrustMonitor(inner))
    }
}

impl AsyncRead for TrustStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !inner.buffered.is_empty() {
            let count = buf.remaining().min(inner.buffered.len());
            let (first, second) = inner.buffered.as_slices();
            let first_count = count.min(first.len());
            buf.put_slice(&first[..first_count]);
            buf.put_slice(&second[..count - first_count]);
            inner.buffered.drain(..count);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut *inner.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TrustStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Pin::new(&mut *inner.stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Pin::new(&mut *inner.stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Pin::new(&mut *inner.stream).poll_shutdown(cx)
    }
}

impl TrustMonitor {
    /// Refresh Tokio's cooperative budget and poll the transport once before
    /// applying a decision. A ready answer must not win over a queued close
    /// merely because a channel read exhausted its current task budget.
    pub(super) async fn check_before_decision(&self) -> Option<io::Error> {
        tokio::task::yield_now().await;
        tokio::select! {
            biased;
            error = tokio::task::unconstrained(self.disconnected()) => Some(error),
            _ = std::future::ready(()) => None,
        }
    }

    /// Russh 0.61 calls `check_server_key` only after its current read future
    /// has completed and does not start another until the callback returns.
    /// Thus this is the sole reader while the prompt is pending. When the
    /// callback returns, russh consumes the buffered bytes before new ones.
    pub(super) async fn disconnected(&self) -> io::Error {
        poll_fn(|cx| {
            let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let wake = Arc::new(ReadWake {
                notified: AtomicBool::new(false),
                parent: cx.waker().clone(),
            });
            let waker = Waker::from(wake.clone());
            let mut read_cx = Context::from_waker(&waker);
            let mut self_wakes = 0;
            let mut read_polls = 0;
            loop {
                read_polls += 1;
                if read_polls > MAX_READ_POLLS_PER_CHECK {
                    return Poll::Ready(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "SSH transport exceeded the host-key confirmation read-work limit",
                    ));
                }
                let remaining = MAX_TRUST_BUFFER - inner.buffered.len();
                let mut chunk = [0; 8192];
                // At the limit, read one more byte to distinguish EOF from an
                // overflowing peer. Either result terminates this handshake.
                let size = remaining.clamp(1, chunk.len());
                let mut buf = ReadBuf::new(&mut chunk[..size]);
                wake.notified.store(false, Ordering::Release);
                match Pin::new(&mut *inner.stream).poll_read(&mut read_cx, &mut buf) {
                    Poll::Pending if wake.notified.swap(false, Ordering::AcqRel) => {
                        // ChannelRx may consume several control messages, waking
                        // after each, before a close becomes visible as EOF.
                        self_wakes += 1;
                        if self_wakes > 128 {
                            return Poll::Ready(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "SSH transport exceeded the host-key confirmation wake limit",
                            ));
                        }
                    }
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(error),
                    Poll::Ready(Ok(())) if buf.filled().is_empty() => {
                        return Poll::Ready(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "SSH transport closed during host-key confirmation",
                        ));
                    }
                    Poll::Ready(Ok(())) if remaining == 0 => {
                        return Poll::Ready(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SSH transport exceeded the host-key confirmation buffer",
                        ));
                    }
                    Poll::Ready(Ok(())) => inner.buffered.extend(buf.filled()),
                }
            }
        })
        .await
    }
}

/// Only interactive initial handshakes need this wrapper. It remains owned by
/// russh afterwards, without a worker or a separate connection lifetime.
pub(super) async fn connect_stream<S>(
    config: Arc<client::Config>,
    stream: S,
    mut handler: KnownHostsClient,
) -> Result<client::Handle<KnownHostsClient>, SshError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if handler.decision.is_some() && handler.budget.is_some() {
        let (stream, monitor) = TrustStream::new(stream);
        handler.trust_monitor = Some(monitor);
        client::connect_stream(config, stream, handler).await
    } else {
        client::connect_stream(config, stream, handler).await
    }
}

pub(super) async fn connect_tcp(
    config: Arc<client::Config>,
    host: &str,
    port: u16,
    handler: KnownHostsClient,
) -> Result<client::Handle<KnownHostsClient>, SshError> {
    if handler.decision.is_none() || handler.budget.is_none() {
        return client::connect(config, (host, port), handler).await;
    }
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(russh::Error::from)?;
    if config.nodelay {
        let _ = stream.set_nodelay(true);
    }
    connect_stream(config, stream, handler).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HandshakeBudget, HostKeyDecision, HostKeyPrompt, LiveShellEvent};
    use russh::client::Handler;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct WakeBeforeEof(usize);

    struct ByteStorm;

    impl AsyncRead for ByteStorm {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            buf.put_slice(&[1]);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for ByteStorm {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct ChannelClose(tokio::sync::mpsc::UnboundedReceiver<()>);

    impl AsyncRead for ChannelClose {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.0.poll_recv(cx) {
                Poll::Ready(Some(())) => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Ready(None) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl AsyncWrite for ChannelClose {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for WakeBeforeEof {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.0 > 0 {
                self.0 -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    impl AsyncWrite for WakeBeforeEof {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn synchronous_close_wake_is_observed_before_a_ready_decision() {
        let (_stream, monitor) = TrustStream::new(WakeBeforeEof(9));
        let closed = tokio::select! {
            biased;
            _ = monitor.disconnected() => true,
            _ = std::future::ready(()) => false,
        };
        assert!(
            closed,
            "a synchronous close wake must be polled before a ready answer"
        );
    }

    #[tokio::test]
    async fn excessive_synchronous_wakes_fail_closed_without_spinning() {
        let (_stream, monitor) = TrustStream::new(WakeBeforeEof(1000));
        let error = monitor.disconnected().await;
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn continuously_ready_small_reads_are_bounded() {
        let (_stream, monitor) = TrustStream::new(ByteStorm);
        let error = monitor.disconnected().await;
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("read-work limit"));
    }

    #[tokio::test]
    async fn decision_recheck_observes_channel_close_after_cooperative_budget_exhaustion() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        sender.send(()).unwrap();
        drop(sender);
        let (_stream, monitor) = TrustStream::new(ChannelClose(receiver));
        for _ in 0..127 {
            tokio::task::consume_budget().await;
        }
        let decision_ready = tokio::select! {
            biased;
            _ = monitor.disconnected() => false,
            _ = std::future::ready(()) => true,
        };
        assert!(
            decision_ready,
            "fixture must exhaust budget before EOF becomes visible"
        );
        let error = monitor
            .check_before_decision()
            .await
            .expect("closed channel");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn decision_recheck_drains_128_channel_control_messages_before_eof() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..128 {
            sender.send(()).unwrap();
        }
        drop(sender);
        let (_stream, monitor) = TrustStream::new(ChannelClose(receiver));
        let error = monitor
            .check_before_decision()
            .await
            .expect("closed channel");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn monitored_bytes_are_replayed_in_order_before_later_input() {
        let (stream, mut peer) = tokio::io::duplex(MAX_TRUST_BUFFER + 1024);
        let (mut stream, monitor) = TrustStream::new(stream);
        let early: Vec<u8> = (0..MAX_TRUST_BUFFER).map(|i| (i % 251) as u8).collect();
        peer.write_all(&early).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), monitor.disconnected())
                .await
                .is_err()
        );
        let later = b"\x00later\xffbytes";
        peer.write_all(later).await.unwrap();
        let mut actual = vec![0; early.len() + later.len()];
        for chunk in actual.chunks_mut(137) {
            stream.read_exact(chunk).await.unwrap();
        }
        assert_eq!(&actual[..early.len()], early);
        assert_eq!(&actual[early.len()..], later);
    }

    #[tokio::test]
    async fn eof_after_buffered_bytes_and_overflow_both_fail_closed() {
        let (stream, mut peer) = tokio::io::duplex(128);
        let (_stream, monitor) = TrustStream::new(stream);
        peer.write_all(b"before EOF").await.unwrap();
        drop(peer);
        assert_eq!(
            monitor.disconnected().await.kind(),
            io::ErrorKind::UnexpectedEof
        );

        let (stream, mut peer) = tokio::io::duplex(MAX_TRUST_BUFFER + 1);
        let (_stream, monitor) = TrustStream::new(stream);
        peer.write_all(&vec![7; MAX_TRUST_BUFFER + 1])
            .await
            .unwrap();
        let error = monitor.disconnected().await;
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(monitor.0.lock().unwrap().buffered.len(), MAX_TRUST_BUFFER);
        let (budget, _guard) = HandshakeBudget::new(30);
        let _wait = budget.pause_for_trust().unwrap();
        let (mut prompt, _receiver) = HostKeyPrompt::channel(
            "overflow.example.test".into(),
            22,
            "test".into(),
            "test".into(),
            None,
        );
        prompt.budget = Some(budget.clone());
        budget.fail_transport(&error);
        assert!(!prompt.is_pending());
        assert!(!prompt.respond(HostKeyDecision::TrustAndRecord));
        assert!(matches!(
            budget.failed().await,
            SshError::Russh(russh::Error::IO(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    struct FailingStream;
    impl AsyncRead for FailingStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "test reset",
            )))
        }
    }
    impl AsyncWrite for FailingStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn monitor_preserves_transport_errors() {
        let (_stream, monitor) = TrustStream::new(FailingStream);
        let error = monitor.disconnected().await;
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(error.to_string(), "test reset");

        let (budget, _guard) = HandshakeBudget::new(30);
        let _wait = budget.pause_for_trust().unwrap();
        let error = io::Error::from_raw_os_error(10054);
        budget.fail_transport(&error);
        assert!(matches!(
            budget.failed().await,
            SshError::Russh(russh::Error::IO(error))
                if error.raw_os_error() == Some(10054)
        ));
    }

    #[test]
    fn cancellation_before_decision_claim_prevents_the_write() {
        let (budget, guard) = HandshakeBudget::new(30);
        let handler = KnownHostsClient::new(
            "cancel.example.test".into(),
            22,
            std::env::temp_dir().join("unused-known-hosts"),
            None,
            None,
        )
        .with_handshake_budget(budget);
        drop(guard);
        let mut wrote = false;
        let result = handler.apply_trust_decision(|| {
            wrote = true;
            Ok(true)
        });
        assert!(matches!(result, Err(SshError::AuthenticationCancelled)));
        assert!(!wrote);
    }

    #[tokio::test]
    async fn ready_eof_wins_over_a_simultaneously_ready_trust_reply() {
        let path = std::env::temp_dir().join(format!("adit-trust-race-{}", uuid::Uuid::new_v4()));
        let (stream, peer) = tokio::io::duplex(1024);
        let (_stream, monitor) = TrustStream::new(stream);
        let (events, event_rx) = std::sync::mpsc::channel();
        let (_legacy_tx, legacy_rx) = tokio::sync::oneshot::channel();
        let (budget, _guard) = HandshakeBudget::new(30);
        let mut handler = KnownHostsClient::new(
            "race.example.test".into(),
            22,
            path.clone(),
            Some(events),
            Some(legacy_rx),
        )
        .with_handshake_budget(budget.clone());
        handler.trust_monitor = Some(monitor);
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[85; 32]),
        );
        let task = tokio::spawn(async move { handler.check_server_key(key.public_key()).await });
        let prompt = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match event_rx.try_recv() {
                    Ok(LiveShellEvent::HostKeyPrompt(prompt)) => break prompt,
                    Ok(_) | Err(std::sync::mpsc::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("event channel closed")
                    }
                }
            }
        })
        .await
        .unwrap();
        drop(peer);
        assert!(prompt.respond(HostKeyDecision::TrustAndRecord));
        assert!(matches!(
            task.await.unwrap(),
            Err(SshError::Russh(russh::Error::IO(error)))
                if error.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(!prompt.is_pending());
        assert!(!path.exists());
        assert!(matches!(
            budget.failed().await,
            SshError::Russh(russh::Error::IO(error))
                if error.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), budget.expired())
                .await
                .is_err()
        );
    }
}
