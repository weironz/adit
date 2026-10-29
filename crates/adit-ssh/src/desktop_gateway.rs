//! A single SSH direct-tcpip stream with interactive trust and independent
//! credentials. No remote shell, loopback listener, or direct-connect fallback.
use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct GatewayStream {
    stream: russh::ChannelStream<client::Msg>,
    // Dropping the VNC stream also drops its claim on the SSH connection.
    _session: client::Handle<KnownHostsClient>,
    _proxy: Option<proxy_command::Process>,
}

impl AsyncRead for GatewayStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for GatewayStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub async fn connect(
    request: LiveShellRequest,
    target_host: String,
    target_port: u16,
    mut commands: tokio_mpsc::UnboundedReceiver<LiveShellCommand>,
    events: mpsc::Sender<LiveShellEvent>,
) -> Result<GatewayStream, SshError> {
    validate_live_request(&request)?;
    if target_host.trim().is_empty() {
        return Err(SshError::EmptyHost);
    }
    if target_port == 0 {
        return Err(SshError::InvalidPort);
    }
    // Chained gateways need a separate identity/trust conversation per hop.
    if !request.jumps.is_empty() {
        return Err(SshError::Tunnel(
            "VNC 网关目前仅支持单个独立 SSH 配置".into(),
        ));
    }
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(request.keepalive_secs.clamp(5, 60))),
        keepalive_max: 3,
        ..Default::default()
    });
    let (decision_tx, decision_rx) = oneshot::channel();
    let (kbd_tx, kbd_rx) = tokio_mpsc::unbounded_channel();
    let (budget, _guard) = HandshakeBudget::new(request.connect_timeout_secs);
    let handler = KnownHostsClient::new(
        request.host.clone(),
        request.port,
        request.known_hosts_path.clone(),
        Some(events.clone()),
        Some(decision_rx),
    )
    .with_handshake_budget(budget.clone());
    // Gateway trust is always explicit, including when terminal TOFU is enabled.
    let connect = async {
        let mut proxy_process = None;
        let mut session =
            proxy_command::connect_endpoint(&request, config, handler, &mut proxy_process).await?;
        let mut interactive = InteractiveKbd {
            events: AuthEventSink::Shell(&events),
            endpoint: format!("{}@{}:{}", request.username, request.host, request.port),
            responses: Some(kbd_rx),
        };
        authenticate_with_available_methods(
            &mut session,
            &request.username,
            &request.password,
            &request.auth,
            Some(&events),
            Some(&mut interactive),
        )
        .await?;
        let channel = session
            .channel_open_direct_tcpip(target_host, u32::from(target_port), "127.0.0.1", 0)
            .await?;
        Ok(GatewayStream {
            stream: channel.into_stream(),
            _session: session,
            _proxy: proxy_process,
        })
    };
    tokio::pin!(connect);
    let mut decision_tx = Some(decision_tx);
    loop {
        tokio::select! {
            result = &mut connect => return result,
            _ = budget.expired() => return Err(SshError::Timeout(request.connect_timeout_secs)),
            command = commands.recv() => match command {
                Some(LiveShellCommand::HostKeyDecision(decision)) => {
                    if let Some(tx) = decision_tx.take() { let _ = tx.send(decision); }
                }
                Some(LiveShellCommand::AuthResponses(answers)) => { let _ = kbd_tx.send(answers); }
                Some(LiveShellCommand::Disconnect) | None => return Err(SshError::AuthenticationCancelled),
                _ => {}
            },
        }
    }
}

/// Authentication and trust failures must never enter automatic network retry.
pub fn is_transport_error(error: &SshError) -> bool {
    matches!(
        error,
        SshError::Timeout(_)
            | SshError::Russh(russh::Error::IO(_) | russh::Error::Disconnect | russh::Error::HUP)
    )
}
