use super::*;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn event(handle: &TunnelHandle) -> TunnelEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(event) = handle.try_recv() {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("tunnel event deadline")
}

fn request(port: u16, kind: TunnelKind) -> TunnelRequest {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let bind_port = listener.local_addr().unwrap().port();
    let mut request = TunnelRequest::new(
        "127.0.0.1",
        port,
        "fixture",
        "fixture",
        kind,
        "127.0.0.1",
        bind_port,
        "127.0.0.1",
        9,
    );
    request.auth = AuthOptions {
        try_password: true,
        try_agent: false,
        try_default_keys: false,
        ..Default::default()
    };
    request.connect_timeout_secs = 10;
    request.reconnect_delays = vec![Duration::from_millis(20); 2];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tunnel-fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    request.known_hosts_path = fixtures.join(format!(
        "known-hosts-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    request
}

#[derive(Clone)]
struct Server {
    accepts_password: Arc<AtomicBool>,
    forwards: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
}

impl russh::server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        _: &str,
        _: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        Ok(if self.accepts_password.load(Ordering::Relaxed) {
            russh::server::Auth::Accept
        } else {
            russh::server::Auth::reject()
        })
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        _: russh::Channel<russh::server::Msg>,
        _: &str,
        _: u32,
        _: &str,
        _: u32,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn data(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())
    }

    async fn tcpip_forward(
        &mut self,
        _: &str,
        _: &mut u32,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        self.forwards.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _: &str,
        _: u32,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        self.cancelled.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

struct Fixture {
    port: u16,
    connections: tokio_mpsc::UnboundedReceiver<russh::server::Handle>,
    task: tokio::task::JoinHandle<()>,
    accepts_password: Arc<AtomicBool>,
    seed: Arc<AtomicU8>,
    accepted: Arc<AtomicUsize>,
    forwards: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
}

impl Fixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, connections) = tokio_mpsc::unbounded_channel();
        let accepts_password = Arc::new(AtomicBool::new(true));
        let seed = Arc::new(AtomicU8::new(7));
        let accepted = Arc::new(AtomicUsize::new(0));
        let forwards = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let handler = Server {
            accepts_password: accepts_password.clone(),
            forwards: forwards.clone(),
            cancelled: cancelled.clone(),
        };
        let task_seed = seed.clone();
        let task_accepted = accepted.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                task_accepted.fetch_add(1, Ordering::Relaxed);
                let key = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
                    &[task_seed.load(Ordering::Relaxed); 32],
                );
                let config = Arc::new(russh::server::Config {
                    keys: vec![key.into()],
                    auth_rejection_time: Duration::ZERO,
                    auth_rejection_time_initial: Some(Duration::ZERO),
                    ..Default::default()
                });
                let tx = tx.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, socket, handler).await {
                        let _ = tx.send(session.handle());
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            port,
            connections,
            task,
            accepts_password,
            seed,
            accepted,
            forwards,
            cancelled,
        }
    }

    async fn connection(&mut self) -> russh::server::Handle {
        tokio::time::timeout(Duration::from_secs(3), self.connections.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn drop_connection(&mut self) {
        self.connection()
            .await
            .disconnect(
                Disconnect::ByApplication,
                "fixture drop".into(),
                "en".into(),
            )
            .await
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn listening(handle: &TunnelHandle) -> usize {
    let mut retries = 0;
    loop {
        match event(handle).await {
            TunnelEvent::Listening { .. } => return retries,
            TunnelEvent::Reconnecting {
                attempt,
                max_attempts,
                ..
            } => {
                assert!(attempt <= max_attempts);
                retries += 1;
            }
            TunnelEvent::Stopped => panic!("tunnel stopped before listening"),
            _ => {}
        }
    }
}

async fn stopped(handle: &TunnelHandle) -> (usize, String) {
    let mut retries = 0;
    let mut last_error = String::new();
    loop {
        match event(handle).await {
            TunnelEvent::Reconnecting { .. } => retries += 1,
            TunnelEvent::Error(error) => last_error = error,
            TunnelEvent::Stopped => return (retries, last_error),
            _ => {}
        }
    }
}

async fn echo(port: u16, dynamic: bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        if dynamic {
            socket.write_all(&[5, 1, 0]).await.unwrap();
            let mut method = [0; 2];
            socket.read_exact(&mut method).await.unwrap();
            assert_eq!(method, [5, 0]);
            socket
                .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, 0, 9])
                .await
                .unwrap();
            let mut reply = [0; 10];
            socket.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[1], 0);
        }
        socket.write_all(b"after-reconnect").await.unwrap();
        let mut echoed = [0; 15];
        socket.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"after-reconnect");
    })
    .await
    .unwrap();
}

#[test]
fn tunnel_local_and_socks_redial_and_carry_real_bytes_after_loss() {
    runtime().block_on(async {
        for kind in [TunnelKind::Local, TunnelKind::Dynamic] {
            let mut fixture = Fixture::start().await;
            let request = request(fixture.port, kind);
            let bind = request.bind_port;
            let handle = spawn_tunnel_session(request).unwrap();
            assert_eq!(listening(&handle).await, 0);
            echo(bind, kind == TunnelKind::Dynamic).await;
            fixture.drop_connection().await;
            assert_eq!(listening(&handle).await, 1);
            echo(bind, kind == TunnelKind::Dynamic).await;
            handle.send(TunnelCommand::Disconnect).unwrap();
            assert_eq!(stopped(&handle).await.0, 0);
            let _released = std::net::TcpListener::bind(("127.0.0.1", bind)).unwrap();
            assert_eq!(fixture.accepted.load(Ordering::Relaxed), 2);
        }
    });
}

#[test]
fn tunnel_remote_forward_is_requested_again_after_loss() {
    runtime().block_on(async {
        let mut fixture = Fixture::start().await;
        let target = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let mut request = request(fixture.port, TunnelKind::Remote);
        request.target_port = target.local_addr().unwrap().port();
        let bind = request.bind_port;
        let handle = spawn_tunnel_session(request).unwrap();
        let echo_target = tokio::spawn(async move {
            while let Ok((mut socket, _)) = target.accept().await {
                tokio::spawn(async move {
                    let mut data = [0; 7];
                    if socket.read_exact(&mut data).await.is_ok() {
                        let _ = socket.write_all(&data).await;
                    }
                });
            }
        });
        listening(&handle).await;
        let first = fixture.connection().await;
        remote_echo(&first, bind).await;
        first
            .disconnect(
                Disconnect::ByApplication,
                "fixture drop".into(),
                "en".into(),
            )
            .await
            .unwrap();
        assert_eq!(listening(&handle).await, 1);
        let second = fixture.connection().await;
        remote_echo(&second, bind).await;
        assert_eq!(fixture.forwards.load(Ordering::Relaxed), 2);
        handle.send(TunnelCommand::Disconnect).unwrap();
        stopped(&handle).await;
        assert_eq!(fixture.cancelled.load(Ordering::Relaxed), 1);
        echo_target.abort();
    });
}

async fn remote_echo(server: &russh::server::Handle, bind: u16) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let channel = server
            .channel_open_forwarded_tcpip("127.0.0.1", u32::from(bind), "127.0.0.1", 43210)
            .await
            .unwrap();
        let mut stream = channel.into_stream();
        stream.write_all(b"remote!").await.unwrap();
        let mut data = [0; 7];
        stream.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"remote!");
    })
    .await
    .unwrap();
}

#[test]
fn tunnel_duplicate_remote_bind_does_not_replace_the_existing_target() {
    let registry = ForwardRegistry::default();
    let (events, _) = mpsc::channel();
    let (_stop, cancelled) = tokio::sync::watch::channel(());
    registry
        .register(
            "127.0.0.1",
            1234,
            ForwardTarget {
                host: "original".into(),
                port: 1,
                events: events.clone(),
                cancelled: cancelled.clone(),
            },
        )
        .unwrap();
    assert!(registry
        .register(
            "127.0.0.1",
            1234,
            ForwardTarget {
                host: "replacement".into(),
                port: 2,
                events,
                cancelled
            }
        )
        .is_err());
    assert_eq!(
        registry.resolve("127.0.0.1", 1234).unwrap().host,
        "original"
    );
}

#[test]
fn tunnel_redial_budget_is_bounded_even_if_every_login_briefly_listens() {
    runtime().block_on(async {
        let mut fixture = Fixture::start().await;
        let handle = spawn_tunnel_session(request(fixture.port, TunnelKind::Local)).unwrap();
        listening(&handle).await;
        for _ in 0..2 {
            fixture.drop_connection().await;
            assert_eq!(listening(&handle).await, 1);
        }
        fixture.drop_connection().await;
        let mut exhausted = None;
        loop {
            match event(&handle).await {
                TunnelEvent::RetryExhausted { attempts } => exhausted = Some(attempts),
                TunnelEvent::Stopped => break,
                TunnelEvent::Reconnecting { .. } => {
                    panic!("budget restarted after a brief successful login")
                }
                _ => {}
            }
        }
        assert_eq!(exhausted, Some(2));
        assert_eq!(fixture.accepted.load(Ordering::Relaxed), 3);
    });
}

#[test]
fn tunnel_auth_or_changed_key_on_redial_requires_explicit_retry() {
    runtime().block_on(async {
        for change_key in [false, true] {
            let mut fixture = Fixture::start().await;
            let request = request(fixture.port, TunnelKind::Local);
            let known_hosts = request.known_hosts_path.clone();
            let handle = spawn_tunnel_session(request).unwrap();
            listening(&handle).await;
            let original = fs::read(&known_hosts).unwrap();
            if change_key {
                fixture.seed.store(8, Ordering::Relaxed);
            } else {
                fixture.accepts_password.store(false, Ordering::Relaxed);
            }
            fixture.drop_connection().await;
            let (retries, error) = stopped(&handle).await;
            assert_eq!(retries, 1);
            assert!(!error.is_empty());
            assert_eq!(fixture.accepted.load(Ordering::Relaxed), 2);
            assert_eq!(fs::read(&known_hosts).unwrap(), original);
        }
    });
}

#[test]
fn tunnel_close_or_policy_disable_wakes_a_real_hour_long_backoff() {
    runtime().block_on(async {
        for disable in [false, true] {
            let mut fixture = Fixture::start().await;
            let mut request = request(fixture.port, TunnelKind::Local);
            request.reconnect_delays = vec![Duration::from_secs(3600)];
            let handle = spawn_tunnel_session(request).unwrap();
            listening(&handle).await;
            fixture.drop_connection().await;
            while !matches!(event(&handle).await, TunnelEvent::Reconnecting { .. }) {}
            if disable {
                handle.set_reconnect_enabled(false);
            } else {
                handle.send(TunnelCommand::Disconnect).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(1), stopped(&handle))
                .await
                .unwrap();
            assert_eq!(fixture.accepted.load(Ordering::Relaxed), 1);
        }
    });
}

#[test]
fn tunnel_shared_close_does_not_disconnect_owner_and_loss_redials_independently() {
    runtime().block_on(async {
        for kind in [TunnelKind::Local, TunnelKind::Remote] {
            let mut fixture = Fixture::start().await;
            let request = request(fixture.port, kind);
            let registry = ForwardRegistry::default();
            let (tx, _rx) = mpsc::channel();
            let (session, _jumps) = auxiliary::connect(
                auxiliary::tunnel_request(&request),
                AuthEventSink::Tunnel(&tx),
                registry.clone(),
            )
            .await
            .unwrap();
            let (alive, _held) = tokio_mpsc::unbounded_channel();
            let shared = SharedSession {
                handle: Arc::new(session),
                _alive: alive,
                forwards: registry,
            };
            let handle = spawn_tunnel_session_on(shared.clone(), request.clone()).unwrap();
            listening(&handle).await;
            handle.send(TunnelCommand::Disconnect).unwrap();
            stopped(&handle).await;
            assert!(
                !shared.is_closed(),
                "closing a shared tunnel must preserve its owner"
            );
            let handle = spawn_tunnel_session_on(shared.clone(), request).unwrap();
            listening(&handle).await;
            fixture.drop_connection().await;
            assert_eq!(listening(&handle).await, 1);
            assert!(shared.is_closed());
            assert_eq!(fixture.accepted.load(Ordering::Relaxed), 2);
            handle.send(TunnelCommand::Disconnect).unwrap();
            stopped(&handle).await;
        }
    });
}

#[test]
fn tunnel_disconnect_interrupts_a_stalled_handshake_and_never_retries() {
    runtime().block_on(async {
        let server = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let handle = spawn_tunnel_session(request(
            server.local_addr().unwrap().port(),
            TunnelKind::Local,
        ))
        .unwrap();
        let (_socket, _) = tokio::time::timeout(Duration::from_secs(2), server.accept())
            .await
            .unwrap()
            .unwrap();
        handle.send(TunnelCommand::Disconnect).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match event(&handle).await {
                    TunnelEvent::Stopped => break,
                    TunnelEvent::Reconnecting { .. } => panic!("user cancellation redialled"),
                    _ => {}
                }
            }
        })
        .await
        .expect("cancel must not wait for handshake timeout");
    });
}

#[test]
fn tunnel_initial_connection_failure_is_terminal_even_with_a_retry_plan() {
    runtime().block_on(async {
        let closed = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let handle = spawn_tunnel_session(request(port, TunnelKind::Local)).unwrap();
        let mut error = false;
        loop {
            match event(&handle).await {
                TunnelEvent::Error(_) => error = true,
                TunnelEvent::Stopped => break,
                TunnelEvent::Reconnecting { .. } => panic!("initial failure must not loop"),
                _ => {}
            }
        }
        assert!(error);
    });
}
