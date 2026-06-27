use bytes::Bytes;
use russh::{
    client,
    keys::{
        agent::{
            client::{AgentClient, AgentStream},
            AgentIdentity,
        },
        load_secret_key, HashAlg, PrivateKeyWithHashAlg,
    },
    ChannelMsg, Disconnect,
};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;

#[derive(Debug, Clone)]
pub struct PasswordShellProbe {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub auth: AuthOptions,
    pub known_hosts_path: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub read_window: Duration,
}

impl PasswordShellProbe {
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: password.into(),
            auth: AuthOptions::default(),
            known_hosts_path: default_known_hosts_path(),
            cols: 96,
            rows: 28,
            read_window: Duration::from_millis(900),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LiveShellRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub auth: AuthOptions,
    pub known_hosts_path: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

impl LiveShellRequest {
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: password.into(),
            auth: AuthOptions::default(),
            known_hosts_path: default_known_hosts_path(),
            cols: 96,
            rows: 28,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthOptions {
    pub try_password: bool,
    pub try_agent: bool,
    pub try_default_keys: bool,
    pub identity_file: Option<PathBuf>,
}

impl Default for AuthOptions {
    fn default() -> Self {
        Self {
            try_password: true,
            try_agent: true,
            try_default_keys: true,
            identity_file: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShellProbeOutput {
    pub transcript: String,
}

#[derive(Debug, Clone)]
pub enum LiveShellCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Disconnect,
}

#[derive(Debug, Clone)]
pub enum LiveShellEvent {
    Status(String),
    Output(Vec<u8>),
    Error(String),
    Closed,
}

pub struct LiveShellHandle {
    command_tx: tokio_mpsc::UnboundedSender<LiveShellCommand>,
    event_rx: mpsc::Receiver<LiveShellEvent>,
}

impl LiveShellHandle {
    pub fn send(&self, command: LiveShellCommand) -> Result<(), SshError> {
        self.command_tx
            .send(command)
            .map_err(|_| SshError::CommandChannelClosed)
    }

    #[must_use]
    pub fn try_recv(&self) -> Option<LiveShellEvent> {
        self.event_rx.try_recv().ok()
    }
}

#[derive(Debug, Error)]
pub enum SshError {
    #[error("host is required")]
    EmptyHost,
    #[error("username is required")]
    EmptyUsername,
    #[error("password is required")]
    EmptyPassword,
    #[error("port must be between 1 and 65535")]
    InvalidPort,
    #[error("authentication was rejected by the server")]
    AuthenticationRejected,
    #[error("identity file was not found: {0}")]
    IdentityFileMissing(String),
    #[error("host key changed for {host}; expected {expected}, got {actual}. Check {known_hosts_path} before trusting this server.")]
    HostKeyChanged {
        host: String,
        expected: String,
        actual: String,
        known_hosts_path: String,
    },
    #[error("known hosts storage failed: {0}")]
    KnownHosts(String),
    #[error("no authentication method is available; enter a password or add a default SSH key under ~/.ssh")]
    NoAuthenticationMethod,
    #[error("ssh agent error: {0}")]
    Agent(String),
    #[error("tokio runtime failed: {0}")]
    Runtime(String),
    #[error("ssh command channel is closed")]
    CommandChannelClosed,
    #[error("ssh error: {0}")]
    Russh(#[from] russh::Error),
}

pub fn spawn_password_shell(request: LiveShellRequest) -> Result<LiveShellHandle, SshError> {
    validate_live_request(&request)?;

    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel();

    thread::Builder::new()
        .name(format!("adit-ssh-{}:{}", request.host, request.port))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();

            match runtime {
                Ok(runtime) => {
                    if let Err(error) = runtime.block_on(run_live_password_shell(
                        request,
                        command_rx,
                        event_tx.clone(),
                    )) {
                        let _ = event_tx.send(LiveShellEvent::Error(error.to_string()));
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(LiveShellEvent::Error(error.to_string()));
                }
            }

            let _ = event_tx.send(LiveShellEvent::Closed);
        })
        .map_err(|error| SshError::Runtime(error.to_string()))?;

    Ok(LiveShellHandle {
        command_tx,
        event_rx,
    })
}

pub fn probe_password_shell_blocking(
    request: PasswordShellProbe,
) -> Result<ShellProbeOutput, SshError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| SshError::Runtime(error.to_string()))?;

    runtime.block_on(probe_password_shell(request))
}

pub async fn probe_password_shell(
    request: PasswordShellProbe,
) -> Result<ShellProbeOutput, SshError> {
    validate_request(&request)?;

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(20)),
        ..Default::default()
    });
    let handler = KnownHostsClient::new(
        request.host.clone(),
        request.port,
        request.known_hosts_path.clone(),
        None,
    );
    let mut session =
        client::connect(config, (request.host.as_str(), request.port), handler).await?;

    authenticate_with_available_methods(
        &mut session,
        &request.username,
        &request.password,
        &request.auth,
        None,
    )
    .await?;

    let mut channel = session.channel_open_session().await?;
    channel
        .request_pty(
            true,
            "xterm-256color",
            u32::from(request.cols),
            u32::from(request.rows),
            0,
            0,
            &[],
        )
        .await?;
    channel.request_shell(true).await?;

    let mut transcript = Vec::new();
    let read_result = tokio::time::timeout(request.read_window, async {
        loop {
            let Some(message) = channel.wait().await else {
                break;
            };

            match message {
                ChannelMsg::Data { data } => transcript.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => transcript.extend_from_slice(&data),
                ChannelMsg::ExitStatus { .. } | ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
    })
    .await;

    if read_result.is_err() && transcript.is_empty() {
        transcript.extend_from_slice(
            b"SSH connected. No shell banner was received before the probe timeout.\r\n",
        );
    }

    let _ = channel.close().await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "probe complete", "en")
        .await;

    Ok(ShellProbeOutput {
        transcript: String::from_utf8_lossy(&transcript).to_string(),
    })
}

fn validate_request(request: &PasswordShellProbe) -> Result<(), SshError> {
    if request.host.trim().is_empty() {
        return Err(SshError::EmptyHost);
    }

    if request.username.trim().is_empty() {
        return Err(SshError::EmptyUsername);
    }

    if request.port == 0 {
        return Err(SshError::InvalidPort);
    }

    Ok(())
}

fn validate_live_request(request: &LiveShellRequest) -> Result<(), SshError> {
    if request.host.trim().is_empty() {
        return Err(SshError::EmptyHost);
    }

    if request.username.trim().is_empty() {
        return Err(SshError::EmptyUsername);
    }

    if request.port == 0 {
        return Err(SshError::InvalidPort);
    }

    Ok(())
}

async fn run_live_password_shell(
    request: LiveShellRequest,
    mut commands: tokio_mpsc::UnboundedReceiver<LiveShellCommand>,
    events: mpsc::Sender<LiveShellEvent>,
) -> Result<(), SshError> {
    let _ = events.send(LiveShellEvent::Status(String::from("connecting")));

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(20)),
        ..Default::default()
    });
    let handler = KnownHostsClient::new(
        request.host.clone(),
        request.port,
        request.known_hosts_path.clone(),
        Some(events.clone()),
    );
    let mut session =
        client::connect(config, (request.host.as_str(), request.port), handler).await?;

    let _ = events.send(LiveShellEvent::Status(String::from("authenticating")));
    authenticate_with_available_methods(
        &mut session,
        &request.username,
        &request.password,
        &request.auth,
        Some(&events),
    )
    .await?;

    let _ = events.send(LiveShellEvent::Status(String::from("opening pty")));
    let mut channel = session.channel_open_session().await?;
    channel
        .request_pty(
            true,
            "xterm-256color",
            u32::from(request.cols),
            u32::from(request.rows),
            0,
            0,
            &[],
        )
        .await?;
    channel.request_shell(true).await?;

    let _ = events.send(LiveShellEvent::Status(String::from("connected")));

    let mut should_close = false;
    while !should_close {
        while let Ok(command) = commands.try_recv() {
            match command {
                LiveShellCommand::Input(data) => {
                    if !data.is_empty() {
                        channel.data_bytes(Bytes::from(data)).await?;
                    }
                }
                LiveShellCommand::Resize { cols, rows } => {
                    channel
                        .window_change(u32::from(cols), u32::from(rows), 0, 0)
                        .await?;
                }
                LiveShellCommand::Disconnect => {
                    should_close = true;
                }
            }
        }

        if should_close || commands.is_closed() {
            break;
        }

        match tokio::time::timeout(Duration::from_millis(20), channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                let _ = events.send(LiveShellEvent::Output(data.to_vec()));
            }
            Ok(Some(ChannelMsg::ExtendedData { data, .. })) => {
                let _ = events.send(LiveShellEvent::Output(data.to_vec()));
            }
            Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                let _ = events.send(LiveShellEvent::Status(format!("exit status {exit_status}")));
                should_close = true;
            }
            Ok(Some(ChannelMsg::Eof | ChannelMsg::Close)) | Ok(None) => {
                should_close = true;
            }
            Ok(Some(_)) | Err(_) => {}
        }
    }

    let _ = channel.close().await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "session closed", "en")
        .await;

    Ok(())
}

async fn authenticate_with_available_methods(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    password: &str,
    auth: &AuthOptions,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<(), SshError> {
    let mut attempted = false;

    if auth.try_password && !password.is_empty() {
        attempted = true;
        if authenticate_password_or_keyboard_interactive(session, username, password, events)
            .await?
        {
            return Ok(());
        }
    }

    if let Some(identity_file) = &auth.identity_file {
        attempted = true;
        if authenticate_with_private_key(session, username, identity_file, password, events).await?
        {
            return Ok(());
        }
    }

    if auth.try_agent {
        let (authenticated, agent_attempted) =
            authenticate_with_ssh_agent(session, username, events).await?;
        attempted |= agent_attempted;
        if authenticated {
            return Ok(());
        }
    }

    if auth.try_default_keys {
        let (authenticated, key_attempted) =
            authenticate_with_default_private_keys(session, username, password, events).await?;
        attempted |= key_attempted;
        if authenticated {
            return Ok(());
        }
    }

    if attempted {
        Err(SshError::AuthenticationRejected)
    } else {
        Err(SshError::NoAuthenticationMethod)
    }
}

async fn authenticate_with_ssh_agent(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<(bool, bool), SshError> {
    #[cfg(windows)]
    {
        match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            Ok(agent) => {
                send_status(events, "trying Windows OpenSSH agent");
                let attempt =
                    authenticate_agent_identities(session, username, agent, events).await?;
                if attempt.0 {
                    return Ok(attempt);
                }
            }
            Err(error) => {
                send_status(
                    events,
                    format!("Windows OpenSSH agent unavailable: {error}"),
                );
            }
        }

        match AgentClient::connect_pageant().await {
            Ok(agent) => {
                send_status(events, "trying Pageant agent");
                return authenticate_agent_identities(session, username, agent, events).await;
            }
            Err(error) => {
                send_status(events, format!("Pageant agent unavailable: {error}"));
            }
        }
    }

    #[cfg(unix)]
    {
        match AgentClient::connect_env().await {
            Ok(agent) => {
                send_status(events, "trying SSH_AUTH_SOCK agent");
                return authenticate_agent_identities(session, username, agent, events).await;
            }
            Err(error) => {
                send_status(events, format!("SSH_AUTH_SOCK agent unavailable: {error}"));
            }
        }
    }

    Ok((false, false))
}

async fn authenticate_agent_identities<S>(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    mut agent: AgentClient<S>,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<(bool, bool), SshError>
where
    S: AgentStream + Send + Unpin,
{
    let identities = match agent.request_identities().await {
        Ok(identities) => identities,
        Err(error) => {
            send_status(events, format!("ssh agent identities unavailable: {error}"));
            return Ok((false, false));
        }
    };

    if identities.is_empty() {
        send_status(events, "ssh agent has no identities");
        return Ok((false, false));
    }

    let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
    for identity in identities {
        send_status(
            events,
            format!("trying agent identity {}", agent_identity_label(&identity)),
        );

        let auth = match identity {
            AgentIdentity::PublicKey { key, .. } => session
                .authenticate_publickey_with(username.to_owned(), key, rsa_hash, &mut agent)
                .await
                .map_err(|error| SshError::Agent(error.to_string()))?,
            AgentIdentity::Certificate { certificate, .. } => session
                .authenticate_certificate_with(
                    username.to_owned(),
                    certificate,
                    rsa_hash,
                    &mut agent,
                )
                .await
                .map_err(|error| SshError::Agent(error.to_string()))?,
        };

        if auth.success() {
            return Ok((true, true));
        }
    }

    Ok((false, true))
}

async fn authenticate_password_or_keyboard_interactive(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    password: &str,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<bool, SshError> {
    let auth = session
        .authenticate_password(username.to_owned(), password.to_owned())
        .await?;

    if auth.success() {
        return Ok(true);
    }

    send_status(
        events,
        "password auth rejected; trying keyboard-interactive",
    );

    let mut response = session
        .authenticate_keyboard_interactive_start(username.to_owned(), Option::<String>::None)
        .await?;
    let mut rounds = 0_u8;

    loop {
        match response {
            client::KeyboardInteractiveAuthResponse::Success => return Ok(true),
            client::KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            client::KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                if rounds >= 8 {
                    return Ok(false);
                }
                rounds += 1;

                let responses = prompts
                    .iter()
                    .map(|prompt| {
                        keyboard_interactive_answer(&prompt.prompt, prompt.echo, password)
                    })
                    .collect();

                response = session
                    .authenticate_keyboard_interactive_respond(responses)
                    .await?;
            }
        }
    }
}

async fn authenticate_with_default_private_keys(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    password: &str,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<(bool, bool), SshError> {
    let key_paths = default_private_key_paths();
    let existing_keys = key_paths
        .iter()
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();

    if existing_keys.is_empty() {
        send_status(events, "no default private keys found");
        return Ok((false, false));
    }

    let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
    for path in existing_keys {
        if authenticate_with_private_key_and_hash(
            session, username, path, password, rsa_hash, events,
        )
        .await?
        {
            return Ok((true, true));
        }
    }

    Ok((false, true))
}

async fn authenticate_with_private_key(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    path: &Path,
    password: &str,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<bool, SshError> {
    if !path.is_file() {
        send_status(
            events,
            format!("configured private key not found: {}", path.display()),
        );
        return Err(SshError::IdentityFileMissing(path.display().to_string()));
    }

    let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
    authenticate_with_private_key_and_hash(session, username, path, password, rsa_hash, events)
        .await
}

async fn authenticate_with_private_key_and_hash(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    path: &Path,
    password: &str,
    rsa_hash: Option<HashAlg>,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
) -> Result<bool, SshError> {
    send_status(
        events,
        format!("trying private key {}", key_file_label(path)),
    );

    let passphrase = (!password.is_empty()).then_some(password);
    let key_pair = match load_secret_key(path, passphrase) {
        Ok(key_pair) => key_pair,
        Err(error) => {
            send_status(
                events,
                format!("skipping private key {}: {error}", key_file_label(path)),
            );
            return Ok(false);
        }
    };

    let auth = session
        .authenticate_publickey(
            username.to_owned(),
            PrivateKeyWithHashAlg::new(Arc::new(key_pair), rsa_hash),
        )
        .await?;

    Ok(auth.success())
}

fn default_private_key_paths() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };

    ["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"]
        .into_iter()
        .map(|file_name| home.join(".ssh").join(file_name))
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            Some(PathBuf::from(format!(
                "{}{}",
                drive.to_string_lossy(),
                path.to_string_lossy()
            )))
        })
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

fn key_file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("unknown key")
        .to_owned()
}

fn agent_identity_label(identity: &AgentIdentity) -> String {
    let comment = identity.comment().trim();
    if comment.is_empty() {
        String::from("agent key")
    } else {
        comment.to_owned()
    }
}

fn keyboard_interactive_answer(prompt: &str, echo: bool, password: &str) -> String {
    let prompt = prompt.to_ascii_lowercase();
    if !echo
        || prompt.contains("password")
        || prompt.contains("passphrase")
        || prompt.contains("passcode")
        || prompt.contains("otp")
    {
        password.to_owned()
    } else {
        String::new()
    }
}

fn send_status(events: Option<&mpsc::Sender<LiveShellEvent>>, message: impl Into<String>) {
    if let Some(events) = events {
        let _ = events.send(LiveShellEvent::Status(message.into()));
    }
}

struct KnownHostsClient {
    host: String,
    port: u16,
    known_hosts_path: PathBuf,
    events: Option<mpsc::Sender<LiveShellEvent>>,
}

impl KnownHostsClient {
    fn new(
        host: String,
        port: u16,
        known_hosts_path: PathBuf,
        events: Option<mpsc::Sender<LiveShellEvent>>,
    ) -> Self {
        Self {
            host,
            port,
            known_hosts_path,
            events,
        }
    }
}

impl client::Handler for KnownHostsClient {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match verify_known_host(
            &self.host,
            self.port,
            &self.known_hosts_path,
            server_public_key,
        )? {
            KnownHostCheck::Trusted { fingerprint } => {
                send_status(
                    self.events.as_ref(),
                    format!("host key verified: {fingerprint}"),
                );
            }
            KnownHostCheck::Recorded {
                host_spec,
                fingerprint,
            } => {
                send_status(
                    self.events.as_ref(),
                    format!("recorded new host key for {host_spec}: {fingerprint}"),
                );
            }
        }

        Ok(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KnownHostCheck {
    Trusted {
        fingerprint: String,
    },
    Recorded {
        host_spec: String,
        fingerprint: String,
    },
}

fn verify_known_host(
    host: &str,
    port: u16,
    known_hosts_path: &Path,
    server_public_key: &russh::keys::ssh_key::PublicKey,
) -> Result<KnownHostCheck, SshError> {
    let host_spec = known_host_spec(host, port);
    let actual = host_key_fingerprint(server_public_key);

    let content = match fs::read_to_string(known_hosts_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(SshError::KnownHosts(error.to_string())),
    };

    let mut expected = Vec::new();
    for line in content.lines() {
        let Some((hosts, public_key)) = parse_known_host_line(line) else {
            continue;
        };

        if !known_host_matches(&hosts, &host_spec) {
            continue;
        }

        let fingerprint = host_key_fingerprint(&public_key);
        if fingerprint == actual {
            return Ok(KnownHostCheck::Trusted {
                fingerprint: actual,
            });
        }

        expected.push(fingerprint);
    }

    if !expected.is_empty() {
        return Err(SshError::HostKeyChanged {
            host: host_spec,
            expected: expected.join(", "),
            actual,
            known_hosts_path: known_hosts_path.display().to_string(),
        });
    }

    append_known_host(known_hosts_path, &host_spec, server_public_key)?;

    Ok(KnownHostCheck::Recorded {
        host_spec,
        fingerprint: actual,
    })
}

fn append_known_host(
    known_hosts_path: &Path,
    host_spec: &str,
    public_key: &russh::keys::ssh_key::PublicKey,
) -> Result<(), SshError> {
    if let Some(parent) = known_hosts_path.parent() {
        fs::create_dir_all(parent).map_err(|error| SshError::KnownHosts(error.to_string()))?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(known_hosts_path)
        .map_err(|error| SshError::KnownHosts(error.to_string()))?;

    let encoded_key = public_key
        .to_openssh()
        .map_err(|error| SshError::KnownHosts(error.to_string()))?;

    writeln!(file, "{host_spec} {encoded_key}")
        .map_err(|error| SshError::KnownHosts(error.to_string()))
}

fn parse_known_host_line(line: &str) -> Option<(String, russh::keys::ssh_key::PublicKey)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let mut tokens = line.split_whitespace();
    let first = tokens.next()?;
    let hosts = if first.starts_with('@') {
        tokens.next()?
    } else {
        first
    };
    let key_type = tokens.next()?;
    let key_body = tokens.next()?;
    let public_key = format!("{key_type} {key_body}").parse().ok()?;

    Some((hosts.to_string(), public_key))
}

fn known_host_matches(hosts: &str, host_spec: &str) -> bool {
    hosts.split(',').any(|host| host == host_spec)
}

fn known_host_spec(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

fn host_key_fingerprint(public_key: &russh::keys::ssh_key::PublicKey) -> String {
    public_key.fingerprint(HashAlg::Sha256).to_string()
}

fn default_known_hosts_path() -> PathBuf {
    platform_config_dir().join("known_hosts")
}

fn platform_config_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        if let Some(app_data) = env::var_os("APPDATA") {
            return PathBuf::from(app_data).join("Adit");
        }
    }

    if cfg!(target_os = "macos") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Adit");
        }
    }

    if let Some(xdg_config_home) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg_config_home).join("adit");
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("adit");
    }

    PathBuf::from(".").join(".adit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const ED25519_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
    const ECDSA_KEY: &str =
        "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBHwf2HMM5TRXvo2SQJjsNkiDD5KqiiNjrGVv3UUh+mMT5RHxiRtOnlqvjhQtBq0VpmpCV/PwUdhOig4vkbqAcEc=";

    #[test]
    fn unknown_host_is_recorded_and_then_trusted() {
        let path = temp_known_hosts_path("record");
        let key = public_key(ED25519_KEY);

        let first = verify_known_host("192.168.1.20", 22, &path, &key)
            .expect("unknown host should be recorded");
        assert!(matches!(
            first,
            KnownHostCheck::Recorded { ref host_spec, .. } if host_spec == "192.168.1.20"
        ));

        let second = verify_known_host("192.168.1.20", 22, &path, &key)
            .expect("known host should be trusted");
        assert!(matches!(second, KnownHostCheck::Trusted { .. }));

        let content = fs::read_to_string(&path).expect("known_hosts should exist");
        assert!(content.contains("192.168.1.20 ssh-ed25519"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_default_port_uses_bracketed_host_spec() {
        let path = temp_known_hosts_path("port");
        let key = public_key(ED25519_KEY);

        let decision = verify_known_host("example.com", 2222, &path, &key)
            .expect("unknown non-default port should be recorded");

        assert!(matches!(
            decision,
            KnownHostCheck::Recorded { ref host_spec, .. } if host_spec == "[example.com]:2222"
        ));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn changed_host_key_is_rejected() {
        let path = temp_known_hosts_path("changed");
        let original = public_key(ED25519_KEY);
        let changed = public_key(ECDSA_KEY);

        verify_known_host("node5", 22, &path, &original).expect("initial key should be recorded");
        let error = verify_known_host("node5", 22, &path, &changed)
            .expect_err("changed key should be rejected");

        match error {
            SshError::HostKeyChanged {
                host,
                expected,
                actual,
                ..
            } => {
                assert_eq!(host, "node5");
                assert!(expected.starts_with("SHA256:"));
                assert!(actual.starts_with("SHA256:"));
                assert_ne!(expected, actual);
            }
            other => panic!("expected HostKeyChanged, got {other:?}"),
        }

        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_marker_known_host_lines() {
        let line = format!("@cert-authority example.com {ED25519_KEY}");
        let (hosts, key) = parse_known_host_line(&line).expect("line should parse");

        assert_eq!(hosts, "example.com");
        assert_eq!(
            host_key_fingerprint(&key),
            host_key_fingerprint(&public_key(ED25519_KEY))
        );
    }

    fn public_key(encoded: &str) -> russh::keys::ssh_key::PublicKey {
        encoded.parse().expect("test key should parse")
    }

    fn temp_known_hosts_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        env::temp_dir()
            .join(format!("adit-ssh-known-hosts-{label}-{unique}"))
            .join("known_hosts")
    }
}
