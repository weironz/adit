//! Loopback-only SSH peers with synthetic host keys and passwords.
use super::*;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn password_auth() -> AuthOptions {
    AuthOptions {
        try_password: true,
        try_agent: false,
        try_default_keys: false,
        identity_file: None,
        ..Default::default()
    }
}

struct KnownHosts(PathBuf);

impl KnownHosts {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("adit-ssh-227-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for KnownHosts {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Forward {
    Bridge,
    Accept,
}

#[derive(Clone)]
struct Server {
    forward: Forward,
    auth_delay: Duration,
}

impl russh::server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        tokio::time::sleep(self.auth_delay).await;
        Ok(
            if username == "fixture-user" && password == "fixture-password" {
                russh::server::Auth::Accept
            } else {
                russh::server::Auth::reject()
            },
        )
    }

    async fn channel_open_session(
        &mut self,
        _: russh::Channel<russh::server::Msg>,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        _: &str,
        port: u32,
        _: &str,
        _: u32,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        if matches!(self.forward, Forward::Bridge) {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port as u16)).await?;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            });
        }
        Ok(true)
    }
}

struct Peer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Peer {
    async fn start(
        seed: u8,
        handshake_delay: Duration,
        auth_delay: Duration,
        forward: Forward,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let key = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
        let config = Arc::new(russh::server::Config {
            keys: vec![key.into()],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let server = Server {
            forward,
            auth_delay,
        };
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let config = config.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(handshake_delay).await;
                    if let Ok(session) = russh::server::run_stream(config, socket, server).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self { port, task }
    }

    async fn stalled_after_banner() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = socket.write_all(b"SSH-2.0-adit-test\r\n").await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                });
            }
        });
        Self { port, task }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn request(target: &Peer, known_hosts: &KnownHosts, jump: Option<&Peer>) -> LiveShellRequest {
    let mut request =
        LiveShellRequest::new("127.0.0.1", target.port, "fixture-user", "fixture-password");
    request.known_hosts_path = known_hosts.0.clone();
    request.connect_timeout_secs = 1;
    request.auth = password_auth();
    if let Some(jump) = jump {
        let owner = adit_domain::ProfileId::new();
        request.jump_credentials.insert(
            owner,
            JumpCredential {
                username: "fixture-user".into(),
                password: "fixture-password".into(),
                auth: password_auth(),
            },
        );
        request.jumps.push(JumpHop {
            host: "127.0.0.1".into(),
            port: jump.port,
            username: "fixture-user".into(),
            auth_profile: Some(owner),
        });
    }
    request
}

async fn next_event(events: &mpsc::Receiver<LiveShellEvent>) -> LiveShellEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.try_recv() {
                Ok(event) => return event,
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("SSH event channel closed"),
            }
        }
    })
    .await
    .expect("SSH event deadline")
}

async fn prompt(events: &mpsc::Receiver<LiveShellEvent>) -> HostKeyPrompt {
    loop {
        match next_event(events).await {
            LiveShellEvent::HostKeyPrompt(prompt) => return prompt,
            LiveShellEvent::Error(error) | LiveShellEvent::AuthRejected(error) => panic!("{error}"),
            _ => {}
        }
    }
}

async fn connected(events: &mpsc::Receiver<LiveShellEvent>) {
    loop {
        match next_event(events).await {
            LiveShellEvent::Status(status) if status == "connected" => return,
            LiveShellEvent::Error(error) | LiveShellEvent::AuthRejected(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[test]
fn direct_and_jump_target_trust_wait_does_not_consume_network_budget() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            let target = Peer::start(10, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
            let jump = Peer::start(11, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            // Exercise both unknown and changed target keys without a real file.
            if via_jump {
                let old = russh::keys::PrivateKey::from(
                    russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[12; 32]),
                );
                append_known_host(
                    &known_hosts.0,
                    &known_host_spec("127.0.0.1", target.port),
                    old.public_key(),
                )
                .unwrap();
            }
            let (commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let challenge = prompt(&event_rx).await;
            assert_eq!(challenge.previous_fingerprint.is_some(), via_jump);
            assert!(challenge.is_pending());
            tokio::time::sleep(Duration::from_millis(1300)).await;
            assert!(
                !task.is_finished(),
                "trust wait consumed the one-second budget"
            );
            if via_jump {
                // Existing command clients remain supported as well.
                commands
                    .send(LiveShellCommand::HostKeyDecision(
                        HostKeyDecision::TrustOnce,
                    ))
                    .unwrap();
            } else {
                assert!(challenge.respond(HostKeyDecision::TrustOnce));
            }
            connected(&event_rx).await;
            assert!(!challenge.is_pending());
            commands.send(LiveShellCommand::Disconnect).unwrap();
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if via_jump {
                let known = fs::read_to_string(&known_hosts.0).unwrap();
                assert_eq!(
                    known.lines().count(),
                    2,
                    "changed target must not be recorded"
                );
            } else {
                assert!(
                    !known_hosts.0.exists(),
                    "trust once must not record a target key"
                );
            }
        }
    });
}

#[test]
fn real_stalled_network_handshake_times_out_directly_and_through_jump() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            let target =
                Peer::start(20, Duration::from_secs(3), Duration::ZERO, Forward::Accept).await;
            let jump = Peer::start(21, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            let (_commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, Err(SshError::Timeout(1))));
            assert!(!event_rx
                .try_iter()
                .any(|event| matches!(event, LiveShellEvent::HostKeyPrompt(_))));
        }
    });
}

#[test]
fn stalled_key_exchange_after_banner_times_out_directly_and_through_jump() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            let target = Peer::stalled_after_banner().await;
            let jump = Peer::start(22, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            let (_commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, Err(SshError::Timeout(1))));
            assert!(!event_rx
                .try_iter()
                .any(|event| matches!(event, LiveShellEvent::HostKeyPrompt(_))));
        }
    });
}

#[test]
fn rejecting_target_key_after_wait_stops_direct_and_jump_connections() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            let target = Peer::start(25, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
            let jump = Peer::start(26, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            let (_commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let challenge = prompt(&event_rx).await;
            tokio::time::sleep(Duration::from_millis(1300)).await;
            assert!(challenge.respond(HostKeyDecision::Reject));
            let result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, Err(SshError::HostKeyRejected)));
            assert!(!challenge.is_pending());
            let content = fs::read_to_string(&known_hosts.0).unwrap_or_default();
            assert_eq!(content.lines().count(), usize::from(via_jump));
        }
    });
}

#[test]
fn cancelling_trust_invalidates_old_prompt_and_cannot_answer_new_handshake() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            let target = Peer::start(30, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
            let jump = Peer::start(31, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            let (commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let old = prompt(&event_rx).await;
            commands.send(LiveShellCommand::Disconnect).unwrap();
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!old.is_pending());
            assert!(!old.respond(HostKeyDecision::TrustAndRecord));

            let (commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let fresh = prompt(&event_rx).await;
            assert_ne!(old.id, fresh.id);
            assert!(!old.respond(HostKeyDecision::TrustOnce));
            assert!(fresh.is_pending());
            assert!(fresh.respond(HostKeyDecision::TrustOnce));
            connected(&event_rx).await;
            commands.send(LiveShellCommand::Disconnect).unwrap();
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let content = fs::read_to_string(&known_hosts.0).unwrap_or_default();
            assert_eq!(content.lines().count(), usize::from(via_jump));
        }
    });
}

#[test]
fn gateway_keeps_only_the_remaining_budget_after_trust() {
    runtime().block_on(async {
        let target = Peer::start(
            40,
            Duration::from_millis(500),
            Duration::from_secs(2),
            Forward::Accept,
        )
        .await;
        let known_hosts = KnownHosts::new();
        let (commands, rx) = tokio_mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::channel();
        let task = tokio::spawn(desktop_gateway::connect(
            request(&target, &known_hosts, None),
            "127.0.0.1".into(),
            5900,
            rx,
            events,
        ));
        let challenge = prompt(&event_rx).await;
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!task.is_finished());
        assert!(challenge.respond(HostKeyDecision::TrustOnce));
        let result = tokio::time::timeout(Duration::from_millis(850), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(SshError::Timeout(1))));
        assert!(!challenge.is_pending());
        assert!(!known_hosts.0.exists());
        drop(commands);
    });
}

#[test]
fn gateway_disconnect_invalidates_pending_trust() {
    runtime().block_on(async {
        let target = Peer::start(50, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
        let known_hosts = KnownHosts::new();
        let (commands, rx) = tokio_mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::channel();
        let task = tokio::spawn(desktop_gateway::connect(
            request(&target, &known_hosts, None),
            "127.0.0.1".into(),
            5900,
            rx,
            events,
        ));
        let challenge = prompt(&event_rx).await;
        commands.send(LiveShellCommand::Disconnect).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(SshError::AuthenticationCancelled)));
        assert!(!challenge.is_pending());
        assert!(!challenge.respond(HostKeyDecision::TrustAndRecord));
        assert!(!known_hosts.0.exists());
    });
}

#[test]
fn vnc_gateway_forwards_rfb_after_scoped_or_legacy_trust() {
    runtime().block_on(async {
        for legacy in [false, true] {
            let vnc = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let vnc_port = vnc.local_addr().unwrap().port();
            let vnc_task = tokio::spawn(async move {
                let (mut socket, _) = vnc.accept().await.unwrap();
                socket.write_all(b"RFB 003.008\n").await.unwrap();
                let mut response = [0; 12];
                socket.read_exact(&mut response).await.unwrap();
                assert_eq!(&response, b"RFB 003.008\n");
                socket.write_all(b"fixture-vnc-ready").await.unwrap();
            });
            let gateway = Peer::start(45, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            let mut request = request(&gateway, &known_hosts, None);
            // VNC gateway trust stays explicit even when terminal TOFU is on.
            request.auto_accept_host_keys = true;
            let (commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(desktop_gateway::connect(
                request,
                "127.0.0.1".into(),
                vnc_port,
                rx,
                events,
            ));
            let challenge = prompt(&event_rx).await;
            if legacy {
                // Older callers discard the event before sending their command.
                drop(challenge);
                commands
                    .send(LiveShellCommand::HostKeyDecision(
                        HostKeyDecision::TrustOnce,
                    ))
                    .unwrap();
            } else {
                assert!(challenge.respond(HostKeyDecision::TrustOnce));
                assert!(!challenge.is_pending());
            }
            let mut stream = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let mut banner = [0; 12];
            stream.read_exact(&mut banner).await.unwrap();
            assert_eq!(&banner, b"RFB 003.008\n");
            stream.write_all(&banner).await.unwrap();
            let mut ready = [0; 17];
            stream.read_exact(&mut ready).await.unwrap();
            assert_eq!(&ready, b"fixture-vnc-ready");
            vnc_task.await.unwrap();
            assert!(!known_hosts.0.exists());
        }
    });
}

#[test]
fn vnc_gateway_rejects_untrusted_key_and_wrong_synthetic_password() {
    runtime().block_on(async {
        for reject_key in [false, true] {
            let gateway = Peer::start(48, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
            let known_hosts = KnownHosts::new();
            let mut request = request(&gateway, &known_hosts, None);
            if !reject_key {
                request.password = "incorrect-fixture-password".into();
            }
            let (_commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(desktop_gateway::connect(
                request,
                "127.0.0.1".into(),
                5900,
                rx,
                events,
            ));
            let challenge = prompt(&event_rx).await;
            assert!(challenge.respond(if reject_key {
                HostKeyDecision::Reject
            } else {
                HostKeyDecision::TrustOnce
            }));
            let result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert!(if reject_key {
                matches!(result, Err(SshError::HostKeyRejected))
            } else {
                matches!(result, Err(SshError::AuthenticationRejected))
            });
            assert!(!known_hosts.0.exists());
        }
    });
}

#[test]
fn standalone_sftp_and_tunnel_authenticate_with_direct_and_jump_trust() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            for sftp in [false, true] {
                let target = Peer::start(46, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
                let jump = Peer::start(47, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
                let known_hosts = KnownHosts::new();
                let source = request(&target, &known_hosts, via_jump.then_some(&jump));
                let (sftp_events, _sftp_rx) = mpsc::channel();
                let (tunnel_events, _tunnel_rx) = mpsc::channel();
                let (request, sink) = if sftp {
                    let mut request = SftpRequest::new(
                        &source.host,
                        source.port,
                        &source.username,
                        &source.password,
                    );
                    request.auth = source.auth.clone();
                    request.known_hosts_path = source.known_hosts_path.clone();
                    request.connect_timeout_secs = source.connect_timeout_secs;
                    request.jumps = source.jumps.clone();
                    request.jump_credentials = source.jump_credentials.clone();
                    (
                        auxiliary::sftp_request(&request),
                        AuthEventSink::Sftp(&sftp_events),
                    )
                } else {
                    let mut request = TunnelRequest::new(
                        &source.host,
                        source.port,
                        &source.username,
                        &source.password,
                        TunnelKind::Local,
                        "127.0.0.1",
                        12345,
                        "127.0.0.1",
                        12346,
                    );
                    request.auth = source.auth.clone();
                    request.known_hosts_path = source.known_hosts_path.clone();
                    request.connect_timeout_secs = source.connect_timeout_secs;
                    request.jumps = source.jumps.clone();
                    request.jump_credentials = source.jump_credentials.clone();
                    (
                        auxiliary::tunnel_request(&request),
                        AuthEventSink::Tunnel(&tunnel_events),
                    )
                };
                let (session, lifetime) = tokio::time::timeout(
                    Duration::from_secs(3),
                    auxiliary::connect(request, sink, ForwardRegistry::default()),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(!session.is_closed());
                let known = fs::read_to_string(&known_hosts.0).unwrap();
                assert_eq!(known.lines().count(), if via_jump { 2 } else { 1 });
                let _ = session
                    .disconnect(Disconnect::ByApplication, "fixture done", "en")
                    .await;
                drop(lifetime);
            }
        }
    });
}

#[test]
fn dropping_handshake_cancels_detached_key_exchange() {
    runtime().block_on(async {
        for via_jump in [false, true] {
            let target = Peer::start(60, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
            let jump = Peer::start(61, Duration::ZERO, Duration::ZERO, Forward::Bridge).await;
            let known_hosts = KnownHosts::new();
            let (_commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(
                request(&target, &known_hosts, via_jump.then_some(&jump)),
                rx,
                events,
            ));
            let challenge = prompt(&event_rx).await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(!challenge.is_pending());
            assert!(!challenge.respond(HostKeyDecision::TrustAndRecord));
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!event_rx.try_iter().any(|event| matches!(
                event,
                LiveShellEvent::Status(status) if status.contains("trusted for this session")
                    || status.contains("recorded new host key")
            )));
            let content = fs::read_to_string(&known_hosts.0).unwrap_or_default();
            assert_eq!(content.lines().count(), usize::from(via_jump));
        }
    });
}

#[test]
fn disconnected_prompt_listener_does_not_leave_handshake_paused() {
    runtime().block_on(async {
        let target = Peer::start(70, Duration::ZERO, Duration::ZERO, Forward::Accept).await;
        let known_hosts = KnownHosts::new();
        let (_commands, rx) = tokio_mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::channel();
        drop(event_rx);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run_live_password_shell(request(&target, &known_hosts, None), rx, events),
        )
        .await
        .expect("a lost prompt listener must not suspend the handshake forever");
        assert!(matches!(result, Err(SshError::AuthenticationCancelled)));
        assert!(!known_hosts.0.exists());
    });
}
