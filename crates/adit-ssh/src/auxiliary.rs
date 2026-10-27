//! Standalone SFTP/tunnel authentication uses the same MFA engine as shells.
use super::*;

pub(super) fn sftp_request(request: &SftpRequest) -> LiveShellRequest {
    let mut out = LiveShellRequest::new(
        &request.host,
        request.port,
        &request.username,
        &request.password,
    );
    out.auth = request.auth.clone();
    out.jumps = request.jumps.clone();
    out.proxy_command = request.proxy_command.clone();
    out.jump_credentials = request.jump_credentials.clone();
    out.known_hosts_path = request.known_hosts_path.clone();
    out.connect_timeout_secs = request.connect_timeout_secs;
    out.keepalive_secs = request.keepalive_secs;
    out
}

pub(super) fn tunnel_request(request: &TunnelRequest) -> LiveShellRequest {
    let mut out = LiveShellRequest::new(
        &request.host,
        request.port,
        &request.username,
        &request.password,
    );
    out.auth = request.auth.clone();
    out.jumps = request.jumps.clone();
    out.proxy_command = request.proxy_command.clone();
    out.jump_credentials = request.jump_credentials.clone();
    out.known_hosts_path = request.known_hosts_path.clone();
    out.connect_timeout_secs = request.connect_timeout_secs;
    out.keepalive_secs = request.keepalive_secs;
    out
}

pub(super) struct ConnectionLifetime {
    _jumps: Vec<client::Handle<KnownHostsClient>>,
    _proxy: Option<proxy_command::Process>,
}

pub(super) async fn connect(
    request: LiveShellRequest,
    events: AuthEventSink<'_>,
    forwards: ForwardRegistry,
) -> Result<
    (
        client::Handle<KnownHostsClient>,
        ConnectionLifetime,
    ),
    SshError,
> {
    validate_live_request(&request)?;
    let mut proxy_process = None;
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: (request.keepalive_secs > 0)
            .then(|| Duration::from_secs(request.keepalive_secs)),
        keepalive_max: 3,
        ..Default::default()
    });
    let handler = KnownHostsClient::new(
        request.host.clone(),
        request.port,
        request.known_hosts_path.clone(),
        None,
        None,
    )
    .with_forwards(forwards);
    let mut interactive = InteractiveKbd {
        events,
        responses: None,
        endpoint: String::new(),
    };
    let (mut session, jumps) = if request.jumps.is_empty() {
        let session = tokio::time::timeout(
            Duration::from_secs(effective_timeout_secs(request.connect_timeout_secs)),
            proxy_command::connect_endpoint(&request, config.clone(), handler, &mut proxy_process),
        )
        .await
        .map_err(|_| SshError::Timeout(request.connect_timeout_secs))??;
        (session, Vec::new())
    } else {
        connect_through_jumps(
            &request.jumps,
            &request.jump_credentials,
            &request.username,
            &request.host,
            request.port,
            handler,
            &config,
            &request.known_hosts_path,
            request.connect_timeout_secs,
            Some(&mut interactive),
        )
        .await?
    };
    interactive.endpoint = format!(
        "目标：{}@{}:{}",
        request.username, request.host, request.port
    );
    authenticate_with_available_methods(
        &mut session,
        &request.username,
        &request.password,
        &request.auth,
        None,
        Some(&mut interactive),
    )
    .await?;
    Ok((session, ConnectionLifetime { _jumps: jumps, _proxy: proxy_process }))
}
