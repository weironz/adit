//! Port forwarding workers and their bounded reconnect lifecycle.
use super::*;

mod reconnect;
#[cfg(test)]
mod tests;

pub use adit_domain::TunnelKind;

#[derive(Clone)]
pub struct TunnelRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub auth: AuthOptions,
    pub known_hosts_path: PathBuf,
    pub kind: TunnelKind,
    pub bind_address: String,
    pub bind_port: u16,
    /// Forward target (Local only; ignored for Dynamic).
    pub target_host: String,
    pub target_port: u16,
    /// Jump hosts to chain through before `host` (OpenSSH `ProxyJump`).
    pub jumps: Vec<JumpHop>,
    pub proxy_command: adit_domain::ProxyCommand,
    pub jump_credentials: JumpCredentials,
    /// Handshake deadline for a jump chain, in seconds (0 ⇒ effectively none).
    pub connect_timeout_secs: u64,
    /// Keepalive probe interval in seconds (0 ⇒ disabled), as for a shell session.
    pub keepalive_secs: u64,
    /// Delays for a finite retry budget, supplied by the session's policy.
    /// Empty disables redial. Initial authentication failures never redial.
    pub reconnect_delays: Vec<Duration>,
}

impl TunnelRequest {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
        kind: TunnelKind,
        bind_address: impl Into<String>,
        bind_port: u16,
        target_host: impl Into<String>,
        target_port: u16,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: password.into(),
            auth: AuthOptions::default(),
            known_hosts_path: default_known_hosts_path(),
            kind,
            bind_address: bind_address.into(),
            bind_port,
            target_host: target_host.into(),
            target_port,
            jumps: Vec::new(),
            proxy_command: adit_domain::ProxyCommand::default(),
            jump_credentials: JumpCredentials::new(),
            connect_timeout_secs: DEFAULT_JUMP_CONNECT_TIMEOUT_SECS,
            keepalive_secs: DEFAULT_KEEPALIVE_SECS,
            reconnect_delays: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TunnelCommand {
    Disconnect,
}

#[derive(Debug, Clone)]
pub enum TunnelEvent {
    AuthPrompt(AuthPromptRequest),
    Status(String),
    Listening {
        bind: String,
    },
    Opened {
        peer: String,
    },
    Closed {
        peer: String,
    },
    Error(String),
    Reconnecting {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
    },
    RetryExhausted {
        attempts: u32,
    },
    Stopped,
}

pub struct TunnelHandle {
    command_tx: tokio_mpsc::UnboundedSender<TunnelCommand>,
    event_rx: mpsc::Receiver<TunnelEvent>,
    retry_enabled: tokio::sync::watch::Sender<bool>,
}

impl TunnelHandle {
    pub fn send(&self, command: TunnelCommand) -> Result<(), SshError> {
        self.command_tx
            .send(command)
            .map_err(|_| SshError::CommandChannelClosed)
    }

    #[must_use]
    pub fn try_recv(&self) -> Option<TunnelEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Disabling the policy wakes a pending backoff without closing a live
    /// listener. A later enable cannot restart a tunnel that already stopped.
    pub fn set_reconnect_enabled(&self, enabled: bool) {
        self.retry_enabled.send_replace(enabled);
    }
}

/// Prefer the already authenticated connection for the first attempt. After it
/// drops, each redial authenticates afresh, including a new MFA conversation.
pub fn spawn_tunnel_session_on(
    shared: SharedSession,
    request: TunnelRequest,
) -> Result<TunnelHandle, SshError> {
    spawn(request, Some(shared))
}

pub fn spawn_tunnel_session(request: TunnelRequest) -> Result<TunnelHandle, SshError> {
    spawn(request, None)
}

fn spawn(request: TunnelRequest, shared: Option<SharedSession>) -> Result<TunnelHandle, SshError> {
    if request.host.trim().is_empty() {
        return Err(SshError::EmptyHost);
    }
    if request.username.trim().is_empty() {
        return Err(SshError::EmptyUsername);
    }
    if request.port == 0 || request.bind_port == 0 {
        return Err(SshError::InvalidPort);
    }
    if matches!(request.kind, TunnelKind::Local) && request.target_host.trim().is_empty() {
        return Err(SshError::Tunnel(String::from("本地转发需要填写目标主机")));
    }
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel();
    let (retry_enabled, retry_changes) =
        tokio::sync::watch::channel(!request.reconnect_delays.is_empty());
    thread::Builder::new()
        .name(format!(
            "adit-tunnel-{}:{}",
            request.bind_address, request.bind_port
        ))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(reconnect::run(
                    request,
                    shared,
                    command_rx,
                    &event_tx,
                    retry_changes,
                )),
                Err(error) => {
                    let _ = event_tx.send(TunnelEvent::Error(error.to_string()));
                }
            }
            // Shared and standalone workers have the same terminal event. A
            // peer Closed event cannot describe the lifetime of the listener.
            let _ = event_tx.send(TunnelEvent::Stopped);
        })
        .map_err(|error| SshError::Runtime(error.to_string()))?;
    Ok(TunnelHandle {
        command_tx,
        event_rx,
        retry_enabled,
    })
}

async fn run_tunnel_session(
    request: TunnelRequest,
    shared: Option<SharedSession>,
    commands: &mut tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
) -> Result<(), SshError> {
    let _ = events.send(TunnelEvent::Status("connecting".to_owned()));
    if let Some(shared) = shared {
        // Never disconnect somebody else's authenticated connection.
        return run_tunnel_over(
            shared.handle(),
            shared.forwards(),
            request,
            commands,
            events,
        )
        .await;
    }
    let forwards = ForwardRegistry::default();
    let (session, connection_lifetime) = {
        let handshake = auxiliary::connect(
            auxiliary::tunnel_request(&request),
            AuthEventSink::Tunnel(&events),
            forwards.clone(),
        );
        tokio::pin!(handshake);
        tokio::select! {
            biased;
            _ = commands.recv() => return Ok(()),
            result = &mut handshake => result?,
        }
    };
    let session = Arc::new(session);
    let result = run_tunnel_over(Arc::clone(&session), forwards, request, commands, events).await;
    // A dead transport must not hold cancellation or the next retry hostage.
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        session.disconnect(Disconnect::ByApplication, "tunnel closed", "en"),
    )
    .await;
    drop(connection_lifetime);
    result
}

#[cfg(test)]
pub(super) async fn run_tunnel_session_for_test(
    request: TunnelRequest,
    shared: Option<SharedSession>,
    commands: &mut tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
) -> Result<(), SshError> {
    run_tunnel_session(request, shared, commands, events).await
}

/// Serve a tunnel over `session` until it is closed.
///
/// Shared by both entry points. What happens to the *connection* afterwards is
/// deliberately left to the caller, because that is the only thing that differs.
async fn run_tunnel_over(
    session: Arc<client::Handle<KnownHostsClient>>,
    forwards: ForwardRegistry,
    request: TunnelRequest,
    commands: &mut tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
) -> Result<(), SshError> {
    if session.is_closed() {
        return Err(SshError::Russh(russh::Error::Disconnect));
    }
    if matches!(request.kind, TunnelKind::Remote) {
        return run_remote_forward(session, forwards, request, commands, events).await;
    }

    let bind = format!("{}:{}", request.bind_address, request.bind_port);
    let listener = tokio::select! {
        biased;
        _ = commands.recv() => return Ok(()),
        result = tokio::net::TcpListener::bind(&bind) =>
            result.map_err(|error| SshError::Tunnel(format!("bind {bind}: {error}")))?,
    };
    let _ = events.send(TunnelEvent::Listening { bind });

    let kind = request.kind;
    let target_host = request.target_host.clone();
    let target_port = request.target_port;

    let mut clients = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = commands.recv() => break Ok(()),
            _ = disconnected(&session) => break Err(SshError::Russh(russh::Error::Disconnect)),
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        let _ = events.send(TunnelEvent::Error(format!("accept: {error}")));
                        continue;
                    }
                };
                let peer_label = peer.to_string();
                let _ = events.send(TunnelEvent::Opened { peer: peer_label.clone() });
                let session = Arc::clone(&session);
                let events = events.clone();
                let target_host = target_host.clone();
                clients.spawn(async move {
                    let result = match kind {
                        TunnelKind::Local => {
                            forward_local(socket, &session, &target_host, target_port, peer).await
                        }
                        TunnelKind::Dynamic => forward_dynamic(socket, &session, peer).await,
                        TunnelKind::Remote => unreachable!("remote forward handled separately"),
                    };
                    if let Err(error) = result {
                        let _ = events.send(TunnelEvent::Error(format!("{peer_label}: {error}")));
                    }
                    let _ = events.send(TunnelEvent::Closed { peer: peer_label });
                });
            }
        }
    };
    clients.abort_all();
    result
}

async fn disconnected(session: &client::Handle<KnownHostsClient>) {
    while !session.is_closed() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct ForwardLease {
    registry: ForwardRegistry,
    address: String,
    port: u16,
    _stop: tokio::sync::watch::Sender<()>,
}

impl Drop for ForwardLease {
    fn drop(&mut self) {
        self.registry.deregister(&self.address, self.port);
    }
}

/// Remote forward (`-R`): ask the server to listen, then idle until closed —
/// incoming connections arrive as forwarded channels handled by the connection
/// handler (`server_channel_open_forwarded_tcpip`).
async fn run_remote_forward(
    session: Arc<client::Handle<KnownHostsClient>>,
    forwards: ForwardRegistry,
    request: TunnelRequest,
    commands: &mut tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
) -> Result<(), SshError> {
    // Register before asking the server to listen, not after: the first
    // forwarded connection can arrive the moment `tcpip_forward` returns, and a
    // channel that resolves to no target is dropped without a word.
    let (stop, cancelled) = tokio::sync::watch::channel(());
    forwards.register(
        &request.bind_address,
        request.bind_port,
        ForwardTarget {
            host: request.target_host.clone(),
            port: request.target_port,
            events: events.clone(),
            cancelled,
        },
    )?;
    let lease = ForwardLease {
        registry: forwards,
        address: request.bind_address.clone(),
        port: request.bind_port,
        _stop: stop,
    };
    let result = async {
        tokio::select! {
            biased;
            _ = commands.recv() => return Ok(()),
            _ = disconnected(&session) => return Err(SshError::Russh(russh::Error::Disconnect)),
            result = session.tcpip_forward(request.bind_address.clone(), u32::from(request.bind_port)) => {
                result.map_err(|error| {
                    if session.is_closed() { SshError::Russh(russh::Error::Disconnect) }
                    else { SshError::Tunnel(format!("remote forward: {error}")) }
                })?;
            }
        }
        let _ = events.send(TunnelEvent::Listening {
            bind: format!("远端 {}:{}", request.bind_address, request.bind_port),
        });
        tokio::select! {
            biased;
            _ = commands.recv() => Ok(()),
            _ = disconnected(&session) => Err(SshError::Russh(russh::Error::Disconnect)),
        }
    }.await;
    // The connection outlives this tunnel whenever the tunnel was riding on the
    // shell's, so the registration has to be withdrawn with it — otherwise a
    // later forward on the same port would resolve to a dead target.
    drop(lease);
    if !session.is_closed() {
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            session.cancel_tcpip_forward(request.bind_address, u32::from(request.bind_port)),
        )
        .await;
    }
    result
}

/// Pipe a server-opened `forwarded-tcpip` channel to a local target.
pub(super) async fn pipe_forwarded(
    channel: russh::Channel<client::Msg>,
    target_host: &str,
    target_port: u16,
) -> Result<(), SshError> {
    let mut target = tokio::net::TcpStream::connect((target_host, target_port))
        .await
        .map_err(|error| SshError::Tunnel(error.to_string()))?;
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut target, &mut stream)
        .await
        .map_err(|error| SshError::Tunnel(error.to_string()))?;
    Ok(())
}

/// Pipe one accepted local socket to a `direct-tcpip` channel to a fixed target.
async fn forward_local(
    mut socket: tokio::net::TcpStream,
    session: &client::Handle<KnownHostsClient>,
    target_host: &str,
    target_port: u16,
    peer: std::net::SocketAddr,
) -> Result<(), SshError> {
    let channel = session
        .channel_open_direct_tcpip(
            target_host.to_string(),
            u32::from(target_port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await?;
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut socket, &mut stream)
        .await
        .map_err(|error| SshError::Tunnel(error.to_string()))?;
    Ok(())
}

/// Serve one SOCKS5 client: negotiate, open a `direct-tcpip` channel to the
/// requested target, then pipe.
async fn forward_dynamic(
    mut socket: tokio::net::TcpStream,
    session: &client::Handle<KnownHostsClient>,
    peer: std::net::SocketAddr,
) -> Result<(), SshError> {
    let (host, port) = socks5_negotiate(&mut socket)
        .await
        .map_err(SshError::Tunnel)?;
    match session
        .channel_open_direct_tcpip(
            host,
            u32::from(port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
    {
        Ok(channel) => {
            socks5_reply(&mut socket, 0x00)
                .await
                .map_err(SshError::Tunnel)?;
            let mut stream = channel.into_stream();
            tokio::io::copy_bidirectional(&mut socket, &mut stream)
                .await
                .map_err(|error| SshError::Tunnel(error.to_string()))?;
            Ok(())
        }
        Err(error) => {
            let _ = socks5_reply(&mut socket, 0x05).await; // connection refused
            Err(SshError::from(error))
        }
    }
}

/// Minimal SOCKS5 negotiation (no-auth + CONNECT); returns the requested target.
pub(super) async fn socks5_negotiate<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    socket: &mut S,
) -> Result<(String, u16), String> {
    let mut greeting = [0u8; 2];
    socket
        .read_exact(&mut greeting)
        .await
        .map_err(|error| error.to_string())?;
    if greeting[0] != 0x05 {
        return Err(String::from("not a SOCKS5 client"));
    }
    let mut methods = vec![0u8; greeting[1] as usize];
    socket
        .read_exact(&mut methods)
        .await
        .map_err(|error| error.to_string())?;
    // Select "no authentication".
    socket
        .write_all(&[0x05, 0x00])
        .await
        .map_err(|error| error.to_string())?;

    let mut request = [0u8; 4];
    socket
        .read_exact(&mut request)
        .await
        .map_err(|error| error.to_string())?;
    if request[0] != 0x05 {
        return Err(String::from("bad SOCKS5 request"));
    }
    if request[1] != 0x01 {
        let _ = socks5_reply(socket, 0x07).await; // command not supported
        return Err(String::from("only CONNECT is supported"));
    }
    let host = match request[3] {
        0x01 => {
            let mut addr = [0u8; 4];
            socket
                .read_exact(&mut addr)
                .await
                .map_err(|error| error.to_string())?;
            std::net::Ipv4Addr::from(addr).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            socket
                .read_exact(&mut len)
                .await
                .map_err(|error| error.to_string())?;
            let mut name = vec![0u8; len[0] as usize];
            socket
                .read_exact(&mut name)
                .await
                .map_err(|error| error.to_string())?;
            String::from_utf8(name).map_err(|_| String::from("invalid hostname"))?
        }
        0x04 => {
            let mut addr = [0u8; 16];
            socket
                .read_exact(&mut addr)
                .await
                .map_err(|error| error.to_string())?;
            std::net::Ipv6Addr::from(addr).to_string()
        }
        _ => return Err(String::from("unsupported address type")),
    };
    let mut port = [0u8; 2];
    socket
        .read_exact(&mut port)
        .await
        .map_err(|error| error.to_string())?;
    Ok((host, u16::from_be_bytes(port)))
}

/// Send a SOCKS5 reply with the given status code and a zeroed bind address.
async fn socks5_reply<S: tokio::io::AsyncWrite + Unpin>(
    socket: &mut S,
    code: u8,
) -> Result<(), String> {
    socket
        .write_all(&[0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
        .map_err(|error| error.to_string())
}
