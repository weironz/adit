pub use adit_domain::JumpHop;
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
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};
use russh_sftp::client::SftpSession;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;

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
    /// SSH keepalive interval in seconds (0 disables). Keeps idle sessions alive
    /// through NAT/firewall timeouts and detects dead connections.
    pub keepalive_secs: u64,
    /// A command sent to the shell once it opens (empty = none).
    pub startup_command: String,
    /// `TERM` requested for the PTY (empty ⇒ `xterm-256color`).
    pub term: String,
    /// Abort the TCP/handshake if it takes longer than this many seconds
    /// (0 disables the cap).
    pub connect_timeout_secs: u64,
    /// Trust a new (never-seen) host key automatically and record it, instead of
    /// prompting (trust-on-first-use). A *changed* key still prompts/errors.
    pub auto_accept_host_keys: bool,
    /// Jump hosts to chain through before `host` (OpenSSH `ProxyJump`). Empty ⇒
    /// connect directly.
    pub jumps: Vec<JumpHop>,
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
            keepalive_secs: 30,
            startup_command: String::new(),
            term: String::new(),
            connect_timeout_secs: 20,
            auto_accept_host_keys: false,
            jumps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthOptions {
    pub try_password: bool,
    pub try_agent: bool,
    pub try_default_keys: bool,
    pub identity_file: Option<PathBuf>,
    /// Passphrase for an encrypted `identity_file`, distinct from the login
    /// password. When `None`/empty the login password is tried as a fallback
    /// (so profiles that stored the passphrase in the password field still work).
    pub passphrase: Option<String>,
}

impl Default for AuthOptions {
    fn default() -> Self {
        Self {
            try_password: true,
            try_agent: true,
            try_default_keys: true,
            identity_file: None,
            passphrase: None,
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
    /// User's answer to a pending [`LiveShellEvent::HostKeyPrompt`].
    HostKeyDecision(bool),
    /// User's answers to a pending [`LiveShellEvent::AuthPrompt`], one per prompt
    /// in order. An empty vec cancels the authentication.
    AuthResponses(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum LiveShellEvent {
    Status(String),
    Output(Vec<u8>),
    Error(String),
    Closed,
    /// The handshake is paused awaiting the user's decision about the server's
    /// host key. Answer with [`LiveShellCommand::HostKeyDecision`].
    HostKeyPrompt(HostKeyPrompt),
    /// The server is asking for one or more interactive answers (keyboard-
    /// interactive, e.g. an MFA/OTP code). Answer with
    /// [`LiveShellCommand::AuthResponses`].
    AuthPrompt(AuthPromptRequest),
}

/// A keyboard-interactive challenge from the server that Adit can't auto-answer
/// with the stored password (e.g. a one-time code), surfaced for the user.
#[derive(Debug, Clone)]
pub struct AuthPromptRequest {
    /// Server-supplied heading (often empty).
    pub name: String,
    /// Server-supplied instructions (often empty).
    pub instructions: String,
    /// The individual fields to answer, in order.
    pub prompts: Vec<AuthPromptField>,
}

/// One field within an [`AuthPromptRequest`].
#[derive(Debug, Clone)]
pub struct AuthPromptField {
    /// The label shown to the user (e.g. `Verification code:`).
    pub prompt: String,
    /// Whether the typed answer should be echoed (`false` ⇒ mask it).
    pub echo: bool,
}

/// A server host key awaiting the user's trust decision during connect.
#[derive(Debug, Clone)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
    /// `Some` when the key differs from a previously stored one (a potential
    /// man-in-the-middle); `None` for a first-time unknown host.
    pub previous_fingerprint: Option<String>,
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
    #[error("authentication cancelled")]
    AuthenticationCancelled,
    #[error("could not load private key {0}; if it is passphrase-protected, set its passphrase in the profile")]
    KeyPassphraseRequired(String),
    #[error("could not load private key {0} with the given passphrase (wrong passphrase or unsupported key format)")]
    KeyPassphraseWrong(String),
    #[error("identity file was not found: {0}")]
    IdentityFileMissing(String),
    #[error("host key changed for {host}; expected {expected}, got {actual}. Check {known_hosts_path} before trusting this server.")]
    HostKeyChanged {
        host: String,
        expected: String,
        actual: String,
        known_hosts_path: String,
    },
    #[error("host key was rejected by the user")]
    HostKeyRejected,
    #[error("known hosts storage failed: {0}")]
    KnownHosts(String),
    #[error("sftp error: {0}")]
    Sftp(String),
    #[error("local file error: {0}")]
    LocalIo(String),
    #[error("port forwarding error: {0}")]
    Tunnel(String),
    #[error("no authentication method is available; enter a password or add a default SSH key under ~/.ssh")]
    NoAuthenticationMethod,
    #[error("ssh agent error: {0}")]
    Agent(String),
    #[error("tokio runtime failed: {0}")]
    Runtime(String),
    #[error("ssh command channel is closed")]
    CommandChannelClosed,
    #[error("connection timed out after {0}s")]
    Timeout(u64),
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

/// Spawn a local shell in a pseudo-terminal (ConPTY on Windows), presented
/// through the same [`LiveShellHandle`] transport as an SSH shell so the session
/// layer treats it uniformly. `program` overrides the default shell when set.
pub fn spawn_local_shell(
    cols: u16,
    rows: u16,
    program: Option<String>,
) -> Result<LiveShellHandle, SshError> {
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel();

    thread::Builder::new()
        .name(String::from("adit-local-shell"))
        .spawn(move || {
            if let Err(error) = run_local_shell(cols, rows, program, command_rx, &event_tx) {
                let _ = event_tx.send(LiveShellEvent::Error(error));
            }
            let _ = event_tx.send(LiveShellEvent::Closed);
        })
        .map_err(|error| SshError::Runtime(error.to_string()))?;

    Ok(LiveShellHandle {
        command_tx,
        event_rx,
    })
}

fn run_local_shell(
    cols: u16,
    rows: u16,
    program: Option<String>,
    mut commands: tokio_mpsc::UnboundedReceiver<LiveShellCommand>,
    events: &mpsc::Sender<LiveShellEvent>,
) -> Result<(), String> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|error| error.to_string())?;

    let shell = program
        .filter(|program| !program.trim().is_empty())
        .unwrap_or_else(default_shell);
    let mut builder = CommandBuilder::new(&shell);
    if let Some(home) = dirs_home() {
        builder.cwd(home);
    }

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| format!("failed to start {shell}: {error}"))?;
    // Close the slave in the parent so the reader sees EOF when the child exits.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| error.to_string())?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| error.to_string())?;
    let master = pair.master;

    let _ = events.send(LiveShellEvent::Status(format!("本地 Shell: {shell}")));

    // Reader thread: PTY output → Output events.
    let reader_events = events.clone();
    let reader_handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if reader_events
                        .send(LiveShellEvent::Output(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Command loop: input/resize/disconnect.
    while let Some(command) = commands.blocking_recv() {
        match command {
            LiveShellCommand::Input(bytes) => {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
            LiveShellCommand::Resize { cols, rows } => {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            LiveShellCommand::Disconnect => break,
            LiveShellCommand::HostKeyDecision(_) | LiveShellCommand::AuthResponses(_) => {}
        }
    }

    let _ = child.kill();
    drop(writer);
    drop(master);
    let _ = reader_handle.join();
    Ok(())
}

fn default_shell() -> String {
    if cfg!(windows) {
        env::var("COMSPEC").unwrap_or_else(|_| String::from("cmd.exe"))
    } else {
        env::var("SHELL").unwrap_or_else(|_| String::from("/bin/sh"))
    }
}

fn dirs_home() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Open a serial port (8N1, no flow control) and present it through the same
/// [`LiveShellHandle`] transport as an SSH/local shell.
pub fn spawn_serial(port_name: String, baud: u32) -> Result<LiveShellHandle, SshError> {
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel();

    thread::Builder::new()
        .name(format!("adit-serial-{port_name}"))
        .spawn(move || {
            if let Err(error) = run_serial(&port_name, baud, command_rx, &event_tx) {
                let _ = event_tx.send(LiveShellEvent::Error(error));
            }
            let _ = event_tx.send(LiveShellEvent::Closed);
        })
        .map_err(|error| SshError::Runtime(error.to_string()))?;

    Ok(LiveShellHandle {
        command_tx,
        event_rx,
    })
}

fn run_serial(
    port_name: &str,
    baud: u32,
    mut commands: tokio_mpsc::UnboundedReceiver<LiveShellCommand>,
    events: &mpsc::Sender<LiveShellEvent>,
) -> Result<(), String> {
    use serialport::{DataBits, FlowControl, Parity, StopBits};
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut writer = serialport::new(port_name, baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|error| format!("打开串口 {port_name} 失败: {error}"))?;
    let mut reader = writer.try_clone().map_err(|error| error.to_string())?;

    let _ = events.send(LiveShellEvent::Status(format!(
        "串口 {port_name} @ {baud} 8N1"
    )));

    let running = Arc::new(AtomicBool::new(true));
    let reader_running = Arc::clone(&running);
    let reader_events = events.clone();
    let reader_handle = thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while reader_running.load(Ordering::Relaxed) {
            match reader.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    if reader_events
                        .send(LiveShellEvent::Output(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                // A read timeout just means "no bytes yet"; keep polling.
                Err(ref error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    });

    while let Some(command) = commands.blocking_recv() {
        match command {
            LiveShellCommand::Input(bytes) => {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
            LiveShellCommand::Resize { .. }
            | LiveShellCommand::HostKeyDecision(_)
            | LiveShellCommand::AuthResponses(_) => {}
            LiveShellCommand::Disconnect => break,
        }
    }

    running.store(false, Ordering::Relaxed);
    let _ = reader_handle.join();
    Ok(())
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
        // No idle drop; liveness comes from keepalive instead. With a keepalive
        // every `keepalive_secs`, the connection is torn down only after
        // `keepalive_max` unanswered probes (i.e. a genuinely dead link).
        inactivity_timeout: None,
        keepalive_interval: (request.keepalive_secs > 0)
            .then(|| Duration::from_secs(request.keepalive_secs)),
        keepalive_max: 3,
        ..Default::default()
    });
    // Cap the connect/handshake so an unreachable host fails fast instead of
    // hanging on the OS TCP timeout. A very long sleep stands in for "disabled".
    let timeout_secs = effective_timeout_secs(request.connect_timeout_secs);

    // Connect to the target — directly, or chained through jump hosts. The chain
    // (jump handles) must stay alive for the whole session, so it is bound here.
    let (mut session, _jump_handles): (
        client::Handle<KnownHostsClient>,
        Vec<client::Handle<KnownHostsClient>>,
    ) = if request.jumps.is_empty() {
        // The host-key check may pause to ask the user. Drive `connect` while
        // concurrently forwarding the user's HostKeyDecision (and an early
        // Disconnect) into the handler's one-shot decision channel.
        let (decision_tx, decision_rx) = oneshot::channel::<bool>();
        let handler = KnownHostsClient::new(
            request.host.clone(),
            request.port,
            request.known_hosts_path.clone(),
            Some(events.clone()),
            Some(decision_rx),
        )
        .with_auto_accept(request.auto_accept_host_keys);

        let connect = client::connect(config.clone(), (request.host.as_str(), request.port), handler);
        tokio::pin!(connect);
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);
        let mut decision_tx = Some(decision_tx);
        let session = loop {
            tokio::select! {
                result = &mut connect => break result?,
                _ = &mut deadline => {
                    return Err(SshError::Timeout(request.connect_timeout_secs));
                }
                command = commands.recv() => match command {
                    Some(LiveShellCommand::HostKeyDecision(accept)) => {
                        if let Some(tx) = decision_tx.take() {
                            let _ = tx.send(accept);
                        }
                    }
                    Some(LiveShellCommand::Disconnect) | None => {
                        // Cancelled before the session opened: reject any pending
                        // host-key prompt so `connect` unwinds, then stop.
                        if let Some(tx) = decision_tx.take() {
                            let _ = tx.send(false);
                        }
                        return Ok(());
                    }
                    // Input/resize before the shell exists are dropped.
                    Some(_) => {}
                },
            }
        };
        (session, Vec::new())
    } else {
        // The bastions auto-record their keys, but the target gets the same
        // interactive first-use host-key check as a direct shell: its handler
        // carries the events + decision channel, and we drive the user's
        // HostKeyDecision into it while the chain connects (a changed key on any
        // hop or the target still aborts).
        let _ = events.send(LiveShellEvent::Status(String::from("connecting via jump host")));
        let (decision_tx, decision_rx) = oneshot::channel::<bool>();
        let final_handler = KnownHostsClient::new(
            request.host.clone(),
            request.port,
            request.known_hosts_path.clone(),
            Some(events.clone()),
            Some(decision_rx),
        )
        .with_auto_accept(request.auto_accept_host_keys);

        let connect = connect_through_jumps(
            &request.jumps,
            &request.auth,
            &request.password,
            &request.host,
            request.port,
            final_handler,
            &config,
            &request.known_hosts_path,
        );
        tokio::pin!(connect);
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);
        let mut decision_tx = Some(decision_tx);
        loop {
            tokio::select! {
                result = &mut connect => break result?,
                _ = &mut deadline => {
                    return Err(SshError::Timeout(request.connect_timeout_secs));
                }
                command = commands.recv() => match command {
                    Some(LiveShellCommand::HostKeyDecision(accept)) => {
                        if let Some(tx) = decision_tx.take() {
                            let _ = tx.send(accept);
                        }
                    }
                    Some(LiveShellCommand::Disconnect) | None => {
                        if let Some(tx) = decision_tx.take() {
                            let _ = tx.send(false);
                        }
                        return Ok(());
                    }
                    Some(_) => {}
                },
            }
        }
    };

    let _ = events.send(LiveShellEvent::Status(String::from("authenticating")));
    // Authenticate while concurrently pumping commands, so a keyboard-interactive
    // MFA prompt can be answered by the user mid-handshake (AuthResponses) and an
    // early Disconnect unwinds cleanly. The block scopes the auth future so its
    // borrow of `session` is released before the shell is opened below.
    {
        let (kbd_tx, kbd_rx) = tokio_mpsc::unbounded_channel::<Vec<String>>();
        let interactive = InteractiveKbd {
            events: &events,
            responses: kbd_rx,
        };
        let auth = authenticate_with_available_methods(
            &mut session,
            &request.username,
            &request.password,
            &request.auth,
            Some(&events),
            Some(interactive),
        );
        tokio::pin!(auth);
        loop {
            tokio::select! {
                result = &mut auth => {
                    result?;
                    break;
                }
                command = commands.recv() => match command {
                    Some(LiveShellCommand::AuthResponses(answers)) => {
                        let _ = kbd_tx.send(answers);
                    }
                    Some(LiveShellCommand::Disconnect) | None => {
                        // Cancel any pending prompt (empty answer ⇒ cancel), then stop.
                        let _ = kbd_tx.send(Vec::new());
                        return Ok(());
                    }
                    Some(_) => {}
                },
            }
        }
    }

    let _ = events.send(LiveShellEvent::Status(String::from("opening pty")));
    let term = if request.term.trim().is_empty() {
        "xterm-256color"
    } else {
        request.term.trim()
    };
    let mut channel = session.channel_open_session().await?;
    channel
        .request_pty(
            true,
            term,
            u32::from(request.cols),
            u32::from(request.rows),
            0,
            0,
            &[],
        )
        .await?;
    channel.request_shell(true).await?;

    let _ = events.send(LiveShellEvent::Status(String::from("connected")));

    // Run the profile's startup command once the shell is ready.
    if !request.startup_command.trim().is_empty() {
        let mut line = request.startup_command.clone();
        line.push('\r');
        channel.data_bytes(Bytes::from(line.into_bytes())).await?;
    }

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
                // Only meaningful during the handshake; ignore once connected.
                LiveShellCommand::HostKeyDecision(_) | LiveShellCommand::AuthResponses(_) => {}
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

/// Default handshake deadline (seconds) for a jump chain when a request doesn't
/// override it. The shell path uses the profile's connect timeout; SFTP/tunnel
/// requests default to this and adit-session overrides both with the profile's.
const DEFAULT_JUMP_CONNECT_TIMEOUT_SECS: u64 = 30;

/// Resolve a connect-timeout knob to an actual deadline: 0 means "disabled", so
/// stand in a very long sleep rather than a zero-length (instantly-firing) one.
fn effective_timeout_secs(configured: u64) -> u64 {
    if configured == 0 {
        u64::from(u32::MAX)
    } else {
        configured
    }
}

/// Connect + authenticate through `jumps` in order, then open a `direct-tcpip`
/// channel to (`final_host`, `final_port`) and run the target handshake over it.
/// Returns the target session (NOT yet authenticated to the target — the caller
/// does that with the profile credentials) plus the intermediate hop handles,
/// which the caller MUST keep alive for the whole session (dropping one closes
/// its tunnel).
///
/// Each hop authenticates with the same profile credentials as the target
/// (`hop_auth` + `hop_password`) — Adit stores one credential per profile, so a
/// bastion is expected to accept the same password / key / passphrase. `hop_auth`
/// carries the password's double duty as a key passphrase, so an encrypted
/// identity file decrypts on every hop. Jump hops auto-record their host keys
/// (no interactive prompt through the tunnel); a *changed* hop key is rejected.
/// The target key is checked by `final_handler`, which the caller can make
/// interactive.
#[allow(clippy::too_many_arguments)] // cohesive connect params; a struct adds noise
async fn connect_through_jumps(
    jumps: &[JumpHop],
    hop_auth: &AuthOptions,
    hop_password: &str,
    final_host: &str,
    final_port: u16,
    final_handler: KnownHostsClient,
    config: &Arc<client::Config>,
    known_hosts_path: &Path,
) -> Result<
    (
        client::Handle<KnownHostsClient>,
        Vec<client::Handle<KnownHostsClient>>,
    ),
    SshError,
> {
    let mut hops: Vec<client::Handle<KnownHostsClient>> = Vec::new();
    for (index, hop) in jumps.iter().enumerate() {
        if hop.host.trim().is_empty() {
            return Err(SshError::EmptyHost);
        }
        // Non-interactive handler: `decision`/`auto_accept` both None ⇒ record an
        // unknown key on first use and reject a changed one.
        let handler = KnownHostsClient::new(
            hop.host.clone(),
            hop.port,
            known_hosts_path.to_path_buf(),
            None,
            None,
        );
        let mut session = if index == 0 {
            client::connect(config.clone(), (hop.host.as_str(), hop.port), handler).await?
        } else {
            let prev = hops.last().expect("previous hop exists");
            let channel = prev
                .channel_open_direct_tcpip(
                    hop.host.clone(),
                    u32::from(hop.port),
                    "127.0.0.1".to_string(),
                    0,
                )
                .await?;
            client::connect_stream(config.clone(), channel.into_stream(), handler).await?
        };
        authenticate_with_available_methods(
            &mut session,
            &hop.username,
            hop_password,
            hop_auth,
            None,
            None,
        )
        .await?;
        hops.push(session);
    }

    let last = hops.last().expect("at least one jump host");
    let channel = last
        .channel_open_direct_tcpip(
            final_host.to_string(),
            u32::from(final_port),
            "127.0.0.1".to_string(),
            0,
        )
        .await?;
    let target =
        client::connect_stream(config.clone(), channel.into_stream(), final_handler).await?;
    Ok((target, hops))
}

/// Channel back to the UI for answering keyboard-interactive challenges that
/// can't be auto-filled from the stored password (an MFA/OTP code). Present only
/// on the interactive shell path; SFTP/tunnel connections pass `None`.
struct InteractiveKbd<'a> {
    events: &'a mpsc::Sender<LiveShellEvent>,
    responses: tokio_mpsc::UnboundedReceiver<Vec<String>>,
}

async fn authenticate_with_available_methods(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    password: &str,
    auth: &AuthOptions,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
    interactive: Option<InteractiveKbd<'_>>,
) -> Result<(), SshError> {
    let mut attempted = false;

    if auth.try_password && !password.is_empty() {
        attempted = true;
        if authenticate_password_or_keyboard_interactive(
            session,
            username,
            password,
            events,
            interactive,
        )
        .await?
        {
            return Ok(());
        }
    }

    // Prefer the distinct key passphrase; fall back to the login password so
    // profiles that stored the passphrase in the password field still work. Used
    // for both the explicit identity file and the ~/.ssh default-key scan.
    let key_secret = auth
        .passphrase
        .as_deref()
        .filter(|p| !p.is_empty())
        .unwrap_or(password);

    if let Some(identity_file) = &auth.identity_file {
        attempted = true;
        // Surface a clear passphrase error only when this key is the last resort
        // (no agent/default-key fallback left, e.g. explicit Key auth); in Auto a
        // load failure falls through to the remaining methods instead.
        let require = !(auth.try_agent || auth.try_default_keys);
        if authenticate_with_private_key(
            session,
            username,
            identity_file,
            key_secret,
            require,
            events,
        )
        .await?
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
            authenticate_with_default_private_keys(session, username, key_secret, events).await?;
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
    mut interactive: Option<InteractiveKbd<'_>>,
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
            client::KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                if rounds >= 16 {
                    return Ok(false);
                }
                rounds += 1;

                // A user cancel propagates as `AuthenticationCancelled`, which
                // aborts the whole connect rather than falling through to other
                // auth methods (keys/agent).
                let answers = keyboard_interactive_round_answers(
                    &name,
                    &instructions,
                    &prompts,
                    password,
                    events,
                    interactive.as_mut(),
                )
                .await?;

                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
}

/// Produce the answers for one keyboard-interactive round. The account
/// password / key passphrase is auto-filled from the stored password; any
/// remaining field (an MFA code, a *new* password, an echoed username) is asked
/// of the user when an interactive channel is available. A user cancel surfaces
/// as [`SshError::AuthenticationCancelled`] so the whole connect aborts. Without
/// an interactive channel (SFTP/tunnel) it falls back to the best-effort
/// heuristic.
async fn keyboard_interactive_round_answers(
    name: &str,
    instructions: &str,
    prompts: &[client::Prompt],
    password: &str,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
    interactive: Option<&mut InteractiveKbd<'_>>,
) -> Result<Vec<String>, SshError> {
    // Auto-fill account-password fields; leave the rest as `None` ("needs the user").
    let answers: Vec<Option<String>> = prompts
        .iter()
        .map(|p| {
            (should_autofill_password(&p.prompt, p.echo) && !password.is_empty())
                .then(|| password.to_owned())
        })
        .collect();

    if answers.iter().all(Option::is_some) {
        return Ok(fill_answers(answers, std::iter::empty()));
    }

    match interactive {
        Some(kbd) => {
            let fields: Vec<AuthPromptField> = prompts
                .iter()
                .zip(answers.iter())
                .filter(|(_, filled)| filled.is_none())
                .map(|(p, _)| AuthPromptField {
                    prompt: p.prompt.clone(),
                    echo: p.echo,
                })
                .collect();
            send_status(events, "waiting for interactive authentication (MFA)");
            let _ = kbd.events.send(LiveShellEvent::AuthPrompt(AuthPromptRequest {
                name: name.to_owned(),
                instructions: instructions.to_owned(),
                prompts: fields,
            }));
            // Block until the user answers; a closed channel or an empty vec is a
            // deliberate cancel that aborts authentication.
            let user = kbd
                .responses
                .recv()
                .await
                .ok_or(SshError::AuthenticationCancelled)?;
            if user.is_empty() {
                return Err(SshError::AuthenticationCancelled);
            }
            Ok(fill_answers(answers, user.into_iter()))
        }
        None => {
            // Non-interactive path: best effort with the stored password.
            Ok(prompts
                .iter()
                .map(|p| keyboard_interactive_answer(&p.prompt, p.echo, password))
                .collect())
        }
    }
}

/// Splice user-supplied answers into the `None` slots of `answers`, in order.
fn fill_answers(
    answers: Vec<Option<String>>,
    mut supplied: impl Iterator<Item = String>,
) -> Vec<String> {
    answers
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| supplied.next().unwrap_or_default()))
        .collect()
}

/// Whether Adit may auto-fill a keyboard-interactive prompt with the stored
/// account password, rather than asking the user. Mirrors the pre-MFA behaviour
/// (any masked field is the password) but carves out the two cases where reusing
/// the stored password is wrong: a *second factor* (OTP / verification code /
/// Duo / token) and *setting a new* password (new / retype / confirm / change).
fn should_autofill_password(prompt: &str, echo: bool) -> bool {
    let p = prompt.to_ascii_lowercase();

    // A one-time code / second factor — never the stored password.
    const SECOND_FACTOR: &[&str] = &[
        "otp",
        "one-time",
        "one time",
        "verification",
        "verify",
        "passcode",
        "token",
        "authenticator",
        "2fa",
        "two-factor",
        "duo",
        "code",
        "pin",
        "yubikey",
        "securid",
    ];
    if SECOND_FACTOR.iter().any(|needle| p.contains(needle)) {
        return false;
    }

    // Choosing / confirming a *new* password — don't reuse the old one.
    const NEW_PASSWORD: &[&str] = &["new ", "retype", "re-type", "again", "confirm", "change"];
    if NEW_PASSWORD.iter().any(|needle| p.contains(needle)) {
        return false;
    }

    // A masked (non-echoed) field is the account password / passphrase; an
    // explicitly password-labelled field qualifies even if oddly echoed.
    !echo || p.contains("password") || p.contains("passphrase")
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
        // Opportunistic ~/.ssh scan: a key that won't load (e.g. encrypted with a
        // different passphrase) is skipped, not fatal — `require = false`.
        if authenticate_with_private_key_and_hash(
            session, username, path, password, rsa_hash, events, false,
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
    passphrase: &str,
    require: bool,
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
    // When `require`, a key that won't load is a hard error (a clear passphrase
    // message); otherwise it's skipped so other auth methods can be tried.
    authenticate_with_private_key_and_hash(
        session, username, path, passphrase, rsa_hash, events, require,
    )
    .await
}

async fn authenticate_with_private_key_and_hash(
    session: &mut client::Handle<KnownHostsClient>,
    username: &str,
    path: &Path,
    passphrase: &str,
    rsa_hash: Option<HashAlg>,
    events: Option<&mpsc::Sender<LiveShellEvent>>,
    require: bool,
) -> Result<bool, SshError> {
    send_status(
        events,
        format!("trying private key {}", key_file_label(path)),
    );

    let secret = (!passphrase.is_empty()).then_some(passphrase);
    let key_pair = match load_secret_key(path, secret) {
        Ok(key_pair) => key_pair,
        Err(error) => {
            let label = key_file_label(path);
            send_status(events, format!("could not load private key {label}: {error}"));
            if require {
                // Distinguish "needs a passphrase" from "wrong passphrase" by
                // whether one was supplied; both messages are phrased to also
                // cover an unsupported/corrupt key.
                return Err(if passphrase.is_empty() {
                    SshError::KeyPassphraseRequired(label)
                } else {
                    SshError::KeyPassphraseWrong(label)
                });
            }
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

// --- SFTP ------------------------------------------------------------------

const SFTP_CHUNK: usize = 64 * 1024;
const SFTP_PROGRESS_STEP: u64 = 256 * 1024;

/// Connection details for an SFTP session (a separate SSH connection to the
/// same host, reusing the profile's credentials).
#[derive(Debug, Clone)]
pub struct SftpRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub auth: AuthOptions,
    pub known_hosts_path: PathBuf,
    /// Jump hosts to chain through before `host` (OpenSSH `ProxyJump`).
    pub jumps: Vec<JumpHop>,
    /// Handshake deadline for a jump chain, in seconds (0 ⇒ effectively none).
    pub connect_timeout_secs: u64,
}

impl SftpRequest {
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
            jumps: Vec::new(),
            connect_timeout_secs: DEFAULT_JUMP_CONNECT_TIMEOUT_SECS,
        }
    }
}

/// One remote directory entry.
#[derive(Debug, Clone)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum SftpCommand {
    ListDir(String),
    Download { remote: String, local: PathBuf },
    Upload { local: PathBuf, remote: String },
    Mkdir(String),
    Rename { from: String, to: String },
    Remove { path: String, is_dir: bool },
    Disconnect,
}

#[derive(Debug, Clone)]
pub enum SftpEvent {
    Status(String),
    Ready { home: String },
    Listing { path: String, entries: Vec<SftpEntry> },
    Progress { label: String, done: u64, total: u64 },
    Done { label: String, bytes: u64 },
    Error(String),
    Closed,
}

pub struct SftpHandle {
    command_tx: tokio_mpsc::UnboundedSender<SftpCommand>,
    event_rx: mpsc::Receiver<SftpEvent>,
}

impl SftpHandle {
    pub fn send(&self, command: SftpCommand) -> Result<(), SshError> {
        self.command_tx
            .send(command)
            .map_err(|_| SshError::CommandChannelClosed)
    }

    #[must_use]
    pub fn try_recv(&self) -> Option<SftpEvent> {
        self.event_rx.try_recv().ok()
    }
}

pub fn spawn_sftp_session(request: SftpRequest) -> Result<SftpHandle, SshError> {
    if request.host.trim().is_empty() {
        return Err(SshError::EmptyHost);
    }
    if request.username.trim().is_empty() {
        return Err(SshError::EmptyUsername);
    }
    if request.port == 0 {
        return Err(SshError::InvalidPort);
    }

    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel();

    thread::Builder::new()
        .name(format!("adit-sftp-{}:{}", request.host, request.port))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    if let Err(error) =
                        runtime.block_on(run_sftp_session(request, command_rx, event_tx.clone()))
                    {
                        let _ = event_tx.send(SftpEvent::Error(error.to_string()));
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(SftpEvent::Error(error.to_string()));
                }
            }
            let _ = event_tx.send(SftpEvent::Closed);
        })
        .map_err(|error| SshError::Runtime(error.to_string()))?;

    Ok(SftpHandle {
        command_tx,
        event_rx,
    })
}

async fn run_sftp_session(
    request: SftpRequest,
    mut commands: tokio_mpsc::UnboundedReceiver<SftpCommand>,
    events: mpsc::Sender<SftpEvent>,
) -> Result<(), SshError> {
    let _ = events.send(SftpEvent::Status(String::from("connecting")));

    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    // Connect directly, or chain through jump hosts (their handles are kept
    // alive alongside the session). Host keys are verified non-interactively.
    let (mut session, _jump_handles) = if request.jumps.is_empty() {
        let handler = KnownHostsClient::new(
            request.host.clone(),
            request.port,
            request.known_hosts_path.clone(),
            None,
            None,
        );
        let session =
            client::connect(config, (request.host.as_str(), request.port), handler).await?;
        (session, Vec::new())
    } else {
        let final_handler = KnownHostsClient::new(
            request.host.clone(),
            request.port,
            request.known_hosts_path.clone(),
            None,
            None,
        );
        // Bound the whole chain's handshake so a hop that opens the socket but
        // never completes the SSH handshake can't hang the session forever.
        tokio::time::timeout(
            Duration::from_secs(effective_timeout_secs(request.connect_timeout_secs)),
            connect_through_jumps(
                &request.jumps,
                &request.auth,
                &request.password,
                &request.host,
                request.port,
                final_handler,
                &config,
                &request.known_hosts_path,
            ),
        )
        .await
        .map_err(|_| SshError::Timeout(request.connect_timeout_secs))??
    };

    let _ = events.send(SftpEvent::Status(String::from("authenticating")));
    authenticate_with_available_methods(
        &mut session,
        &request.username,
        &request.password,
        &request.auth,
        None,
        None,
    )
    .await?;

    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| SshError::Sftp(error.to_string()))?;

    let home = sftp.canonicalize(".").await.unwrap_or_else(|_| String::from("/"));
    let _ = events.send(SftpEvent::Ready { home: home.clone() });
    list_dir(&sftp, &home, &events).await;

    while let Some(command) = commands.recv().await {
        match command {
            SftpCommand::ListDir(path) => list_dir(&sftp, &path, &events).await,
            SftpCommand::Download { remote, local } => {
                if let Err(error) = sftp_download(&sftp, &remote, &local, &events).await {
                    let _ = events.send(SftpEvent::Error(error.to_string()));
                }
            }
            SftpCommand::Upload { local, remote } => {
                if let Err(error) = sftp_upload(&sftp, &local, &remote, &events).await {
                    let _ = events.send(SftpEvent::Error(error.to_string()));
                }
            }
            SftpCommand::Mkdir(path) => {
                if let Err(error) = sftp.create_dir(path.clone()).await {
                    let _ = events.send(SftpEvent::Error(format!("mkdir {path}: {error}")));
                }
            }
            SftpCommand::Rename { from, to } => {
                if let Err(error) = sftp.rename(from.clone(), to).await {
                    let _ = events.send(SftpEvent::Error(format!("rename {from}: {error}")));
                }
            }
            SftpCommand::Remove { path, is_dir } => {
                let result = if is_dir {
                    sftp.remove_dir(path.clone()).await
                } else {
                    sftp.remove_file(path.clone()).await
                };
                if let Err(error) = result {
                    let _ = events.send(SftpEvent::Error(format!("delete {path}: {error}")));
                }
            }
            SftpCommand::Disconnect => break,
        }
    }

    let _ = sftp.close().await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "sftp session closed", "en")
        .await;
    Ok(())
}

async fn list_dir(sftp: &SftpSession, path: &str, events: &mpsc::Sender<SftpEvent>) {
    match sftp.read_dir(path.to_string()).await {
        Ok(read_dir) => {
            let mut entries: Vec<SftpEntry> = read_dir
                .map(|entry| {
                    let metadata = entry.metadata();
                    SftpEntry {
                        name: entry.file_name(),
                        is_dir: metadata.is_dir(),
                        size: metadata.size.unwrap_or(0),
                        mtime: metadata.mtime,
                    }
                })
                .collect();
            // Directories first, then case-insensitive by name.
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            let _ = events.send(SftpEvent::Listing {
                path: path.to_string(),
                entries,
            });
        }
        Err(error) => {
            let _ = events.send(SftpEvent::Error(format!("list {path}: {error}")));
        }
    }
}

/// Join a remote (POSIX) directory path with a child name.
fn join_remote_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

/// Whether a directory-entry name is a safe single path component. A malicious
/// or buggy SFTP server can return `readdir` names containing separators or `..`
/// (or empty); joining those into the local path would let a download escape the
/// chosen folder (zip-slip) or loop forever, so such entries are skipped.
fn is_safe_child_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Download a remote path — a single file, or a whole directory tree — into
/// `local`, emitting one terminal `Done` for the transfer.
async fn sftp_download(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<(), SshError> {
    let label = remote.rsplit('/').next().unwrap_or(remote).to_string();
    let is_dir = sftp
        .metadata(remote.to_string())
        .await
        .ok()
        .is_some_and(|meta| meta.is_dir());

    if is_dir {
        let (bytes, skipped) = download_dir(sftp, remote, local, &label, events).await?;
        let _ = events.send(SftpEvent::Done {
            label: label.clone(),
            bytes,
        });
        if skipped > 0 {
            let _ = events.send(SftpEvent::Status(format!(
                "{label}: 下载完成，跳过 {skipped} 个无法访问的条目"
            )));
        }
    } else {
        // A lone file shows a percentage, so pass its size as the grand total.
        let size = sftp
            .metadata(remote.to_string())
            .await
            .ok()
            .and_then(|meta| meta.size)
            .unwrap_or(0);
        let bytes = download_file(sftp, remote, local, &label, 0, size, events).await?;
        let _ = events.send(SftpEvent::Done { label, bytes });
    }
    Ok(())
}

/// Recursively download a remote directory tree into `local`. Iterative (an
/// explicit stack) to avoid async recursion. Best-effort: a *deeper* entry that
/// can't be read (a permission error, a symlink whose target isn't a plain file,
/// an unsafe name, …) is skipped rather than aborting the whole tree — but a
/// failure at the transfer ROOT (destination can't be created, or the remote
/// root can't be listed) is fatal, so a wholly-failed transfer surfaces as an
/// error rather than a misleading "done". `top_label` names the folder for the
/// live progress row. Returns the total bytes copied and the number of skips.
async fn download_dir(
    sftp: &SftpSession,
    root_remote: &str,
    root_local: &Path,
    top_label: &str,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<(u64, usize), SshError> {
    tokio::fs::create_dir_all(root_local)
        .await
        .map_err(|error| SshError::LocalIo(error.to_string()))?;

    let mut total = 0u64;
    let mut skipped = 0usize;
    let mut stack = vec![(root_remote.to_string(), root_local.to_path_buf())];
    while let Some((remote_dir, local_dir)) = stack.pop() {
        let read_dir = match sftp.read_dir(remote_dir.clone()).await {
            Ok(read_dir) => read_dir,
            Err(error) => {
                if remote_dir.as_str() == root_remote {
                    // The root itself can't be listed — nothing can be downloaded.
                    return Err(SshError::Sftp(error.to_string()));
                }
                let _ = events.send(SftpEvent::Status(format!("跳过 {remote_dir}: {error}")));
                skipped += 1;
                continue;
            }
        };
        for entry in read_dir {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            if !is_safe_child_name(&name) {
                let _ = events.send(SftpEvent::Status(format!("跳过不安全的名称: {name}")));
                skipped += 1;
                continue;
            }
            let child_remote = join_remote_path(&remote_dir, &name);
            let child_local = local_dir.join(&name);
            if entry.metadata().is_dir() {
                match tokio::fs::create_dir_all(&child_local).await {
                    Ok(()) => stack.push((child_remote, child_local)),
                    Err(error) => {
                        let _ = events.send(SftpEvent::Status(format!("跳过 {name}: {error}")));
                        skipped += 1;
                    }
                }
            } else {
                // Report progress under the folder label with a running cumulative
                // total, so an inner filename never aliases another transfer.
                match download_file(sftp, &child_remote, &child_local, top_label, total, 0, events)
                    .await
                {
                    Ok(bytes) => total += bytes,
                    Err(error) => {
                        let _ = events.send(SftpEvent::Status(format!("跳过 {name}: {error}")));
                        skipped += 1;
                    }
                }
            }
        }
    }
    Ok((total, skipped))
}

/// Download one remote file, reporting progress under `label` with
/// `done = base + copied` and `grand_total` in the total field (see
/// [`stream_copy`]). Emits progress but not the terminal `Done`; returns bytes
/// copied.
async fn download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    label: &str,
    base: u64,
    grand_total: u64,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<u64, SshError> {
    let mut remote_file = sftp
        .open(remote.to_string())
        .await
        .map_err(|error| SshError::Sftp(error.to_string()))?;
    let mut local_file = tokio::fs::File::create(local)
        .await
        .map_err(|error| SshError::LocalIo(error.to_string()))?;

    let bytes =
        stream_copy(&mut remote_file, &mut local_file, label, base, grand_total, events).await?;
    local_file
        .flush()
        .await
        .map_err(|error| SshError::LocalIo(error.to_string()))?;
    Ok(bytes)
}

/// Upload a local path — a single file, or a whole directory tree — to `remote`,
/// emitting one terminal `Done` for the transfer.
async fn sftp_upload(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<(), SshError> {
    let label = remote.rsplit('/').next().unwrap_or(remote).to_string();
    let is_dir = tokio::fs::metadata(local)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false);

    if is_dir {
        let (bytes, skipped) = upload_dir(sftp, local, remote, &label, events).await?;
        let _ = events.send(SftpEvent::Done {
            label: label.clone(),
            bytes,
        });
        if skipped > 0 {
            let _ = events.send(SftpEvent::Status(format!(
                "{label}: 上传完成，跳过 {skipped} 个无法访问的条目"
            )));
        }
    } else {
        // A lone file shows a percentage, so pass its size as the grand total.
        let size = tokio::fs::metadata(local).await.map(|m| m.len()).unwrap_or(0);
        let bytes = upload_file(sftp, local, remote, &label, 0, size, events).await?;
        let _ = events.send(SftpEvent::Done { label, bytes });
    }
    Ok(())
}

/// Recursively upload a local directory tree to `remote`. Iterative (an explicit
/// stack). Best-effort: a *deeper* entry that can't be read/created is skipped
/// rather than aborting the whole tree — but a failure at the transfer ROOT (the
/// remote root dir can't be created, or the local root can't be read) is fatal,
/// so a wholly-failed transfer surfaces as an error rather than a misleading
/// "done". `top_label` names the folder for the live progress row. Returns the
/// total bytes copied and the number of skips.
async fn upload_dir(
    sftp: &SftpSession,
    root_local: &Path,
    root_remote: &str,
    top_label: &str,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<(u64, usize), SshError> {
    let mut total = 0u64;
    let mut skipped = 0usize;
    let mut stack = vec![(root_local.to_path_buf(), root_remote.to_string())];
    while let Some((local_dir, remote_dir)) = stack.pop() {
        let is_root = remote_dir.as_str() == root_remote;
        // Create the remote dir. Tolerate "already exists" (detected by a
        // follow-up stat), but treat a genuine failure as a skip so it is not
        // silently masked into a "success" — and as fatal at the root.
        if let Err(error) = sftp.create_dir(remote_dir.clone()).await {
            let exists_as_dir = sftp
                .metadata(remote_dir.clone())
                .await
                .ok()
                .is_some_and(|meta| meta.is_dir());
            if !exists_as_dir {
                if is_root {
                    return Err(SshError::Sftp(error.to_string()));
                }
                let _ = events.send(SftpEvent::Status(format!("跳过 {remote_dir}: {error}")));
                skipped += 1;
                continue;
            }
        }
        let mut read_dir = match tokio::fs::read_dir(&local_dir).await {
            Ok(read_dir) => read_dir,
            Err(error) => {
                if is_root {
                    return Err(SshError::LocalIo(error.to_string()));
                }
                let _ = events.send(SftpEvent::Status(format!(
                    "跳过 {}: {error}",
                    local_dir.display()
                )));
                skipped += 1;
                continue;
            }
        };
        loop {
            let entry = match read_dir.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    let _ = events.send(SftpEvent::Status(format!("跳过条目: {error}")));
                    skipped += 1;
                    break;
                }
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let child_local = entry.path();
            let child_remote = join_remote_path(&remote_dir, &name);
            let is_dir = entry
                .file_type()
                .await
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            if is_dir {
                stack.push((child_local, child_remote));
            } else {
                // Report progress under the folder label with a running cumulative
                // total, so an inner filename never aliases another transfer.
                match upload_file(sftp, &child_local, &child_remote, top_label, total, 0, events)
                    .await
                {
                    Ok(bytes) => total += bytes,
                    Err(error) => {
                        let _ = events.send(SftpEvent::Status(format!("跳过 {name}: {error}")));
                        skipped += 1;
                    }
                }
            }
        }
    }
    Ok((total, skipped))
}

/// Upload one local file, reporting progress under `label` with
/// `done = base + copied` and `grand_total` in the total field (see
/// [`stream_copy`]). Emits progress but not the terminal `Done`; returns bytes
/// copied.
async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    label: &str,
    base: u64,
    grand_total: u64,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<u64, SshError> {
    let mut local_file = tokio::fs::File::open(local)
        .await
        .map_err(|error| SshError::LocalIo(error.to_string()))?;
    let mut remote_file = sftp
        .create(remote.to_string())
        .await
        .map_err(|error| SshError::Sftp(error.to_string()))?;

    let bytes =
        stream_copy(&mut local_file, &mut remote_file, label, base, grand_total, events).await?;
    remote_file
        .shutdown()
        .await
        .map_err(|error| SshError::Sftp(error.to_string()))?;
    Ok(bytes)
}

/// Copy `reader` into `writer` in chunks, emitting throttled progress events.
/// Returns the number of bytes copied. Does NOT emit the terminal `Done` event —
/// the caller owns that, so a recursive folder transfer reports a single
/// completion for the whole tree rather than one per file.
///
/// Progress is always reported under `label` (for a folder, the top-level folder
/// name, never an inner filename — so it can't alias an unrelated queued
/// transfer of the same name) with `done = base + this_file_bytes`, so a folder
/// shows smooth cumulative progress. `grand_total` is the value put in the
/// progress event's `total` (a single file's size for a percentage, or 0 for a
/// folder where the grand total isn't known — the UI then shows bytes only).
async fn stream_copy<R, W>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    base: u64,
    grand_total: u64,
    events: &mpsc::Sender<SftpEvent>,
) -> Result<u64, SshError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buffer = vec![0u8; SFTP_CHUNK];
    let mut done = 0u64;
    let mut emitted = 0u64;
    let _ = events.send(SftpEvent::Progress {
        label: label.to_string(),
        done: base,
        total: grand_total,
    });
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| SshError::Sftp(error.to_string()))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(|error| SshError::Sftp(error.to_string()))?;
        done += read as u64;
        if done - emitted >= SFTP_PROGRESS_STEP {
            emitted = done;
            let _ = events.send(SftpEvent::Progress {
                label: label.to_string(),
                done: base + done,
                total: grand_total,
            });
        }
    }
    Ok(done)
}

// ===== Port forwarding (SSH tunnels) =====

pub use adit_domain::TunnelKind;

#[derive(Debug, Clone)]
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
    /// Handshake deadline for a jump chain, in seconds (0 ⇒ effectively none).
    pub connect_timeout_secs: u64,
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
            connect_timeout_secs: DEFAULT_JUMP_CONNECT_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TunnelCommand {
    Disconnect,
}

#[derive(Debug, Clone)]
pub enum TunnelEvent {
    Status(String),
    Listening { bind: String },
    Opened { peer: String },
    Closed { peer: String },
    Error(String),
    Stopped,
}

pub struct TunnelHandle {
    command_tx: tokio_mpsc::UnboundedSender<TunnelCommand>,
    event_rx: mpsc::Receiver<TunnelEvent>,
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
}

pub fn spawn_tunnel_session(request: TunnelRequest) -> Result<TunnelHandle, SshError> {
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
        return Err(SshError::Tunnel(String::from(
            "本地转发需要填写目标主机",
        )));
    }

    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel();

    thread::Builder::new()
        .name(format!(
            "adit-tunnel-{}:{}",
            request.bind_address, request.bind_port
        ))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    if let Err(error) =
                        runtime.block_on(run_tunnel_session(request, command_rx, event_tx.clone()))
                    {
                        let _ = event_tx.send(TunnelEvent::Error(error.to_string()));
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(TunnelEvent::Error(error.to_string()));
                }
            }
            let _ = event_tx.send(TunnelEvent::Stopped);
        })
        .map_err(|error| SshError::Runtime(error.to_string()))?;

    Ok(TunnelHandle {
        command_tx,
        event_rx,
    })
}

async fn run_tunnel_session(
    request: TunnelRequest,
    mut commands: tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
) -> Result<(), SshError> {
    let _ = events.send(TunnelEvent::Status(String::from("connecting")));

    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    // Remote forwards need a handler that pipes server-opened channels to a
    // local target; local/dynamic forwards use the plain non-interactive handler.
    let handler = if matches!(request.kind, TunnelKind::Remote) {
        KnownHostsClient::new(
            request.host.clone(),
            request.port,
            request.known_hosts_path.clone(),
            None,
            None,
        )
        .with_forward(
            request.target_host.clone(),
            request.target_port,
            events.clone(),
        )
    } else {
        KnownHostsClient::new(
            request.host.clone(),
            request.port,
            request.known_hosts_path.clone(),
            None,
            None,
        )
    };
    // Connect directly, or chain through jump hosts (their handles are kept
    // alive alongside the session). The tunnel's forward handler rides the
    // final target session either way.
    let (mut session, _jump_handles) = if request.jumps.is_empty() {
        let session =
            client::connect(config.clone(), (request.host.as_str(), request.port), handler).await?;
        (session, Vec::new())
    } else {
        // Same whole-chain handshake bound as the SFTP path (see above).
        tokio::time::timeout(
            Duration::from_secs(effective_timeout_secs(request.connect_timeout_secs)),
            connect_through_jumps(
                &request.jumps,
                &request.auth,
                &request.password,
                &request.host,
                request.port,
                handler,
                &config,
                &request.known_hosts_path,
            ),
        )
        .await
        .map_err(|_| SshError::Timeout(request.connect_timeout_secs))??
    };

    let _ = events.send(TunnelEvent::Status(String::from("authenticating")));
    authenticate_with_available_methods(
        &mut session,
        &request.username,
        &request.password,
        &request.auth,
        None,
        None,
    )
    .await?;

    if matches!(request.kind, TunnelKind::Remote) {
        return run_remote_forward(session, request, commands, events).await;
    }

    let bind = format!("{}:{}", request.bind_address, request.bind_port);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|error| SshError::Tunnel(format!("bind {bind}: {error}")))?;
    let _ = events.send(TunnelEvent::Listening { bind });

    let session = Arc::new(session);
    let kind = request.kind;
    let target_host = request.target_host.clone();
    let target_port = request.target_port;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(TunnelCommand::Disconnect) | None => break,
            },
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
                tokio::spawn(async move {
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
    }

    let _ = session
        .disconnect(Disconnect::ByApplication, "tunnel closed", "en")
        .await;
    Ok(())
}

/// Remote forward (`-R`): ask the server to listen, then idle until closed —
/// incoming connections arrive as forwarded channels handled by the connection
/// handler (`server_channel_open_forwarded_tcpip`).
async fn run_remote_forward(
    session: client::Handle<KnownHostsClient>,
    request: TunnelRequest,
    mut commands: tokio_mpsc::UnboundedReceiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
) -> Result<(), SshError> {
    session
        .tcpip_forward(request.bind_address.clone(), u32::from(request.bind_port))
        .await
        .map_err(|error| SshError::Tunnel(format!("remote forward: {error}")))?;
    let _ = events.send(TunnelEvent::Listening {
        bind: format!("远端 {}:{}", request.bind_address, request.bind_port),
    });

    // Idle until a Disconnect command arrives (or the command channel closes);
    // forwarded connections are served by the handler in the meantime.
    let _ = commands.recv().await;

    let _ = session
        .cancel_tcpip_forward(request.bind_address.clone(), u32::from(request.bind_port))
        .await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "tunnel closed", "en")
        .await;
    Ok(())
}

/// Pipe a server-opened `forwarded-tcpip` channel to a local target.
async fn pipe_forwarded(
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
            socks5_reply(&mut socket, 0x00).await.map_err(SshError::Tunnel)?;
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
async fn socks5_negotiate<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
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

struct KnownHostsClient {
    host: String,
    port: u16,
    known_hosts_path: PathBuf,
    events: Option<mpsc::Sender<LiveShellEvent>>,
    /// Present for interactive (UI) connections so an unknown/changed key can be
    /// confirmed by the user; absent for the one-shot probe, which keeps
    /// non-interactive trust-on-first-use.
    decision: Option<oneshot::Receiver<bool>>,
    /// Trust a never-seen host key automatically (record it, no prompt).
    auto_accept: bool,
    /// For remote forwards (`-R`): the local target to pipe forwarded channels to.
    forward_target: Option<(String, u16)>,
    /// For remote forwards: the tunnel actor's event channel.
    tunnel_events: Option<mpsc::Sender<TunnelEvent>>,
}

impl KnownHostsClient {
    fn new(
        host: String,
        port: u16,
        known_hosts_path: PathBuf,
        events: Option<mpsc::Sender<LiveShellEvent>>,
        decision: Option<oneshot::Receiver<bool>>,
    ) -> Self {
        Self {
            host,
            port,
            known_hosts_path,
            events,
            decision,
            auto_accept: false,
            forward_target: None,
            tunnel_events: None,
        }
    }

    /// Trust a never-seen host key automatically (record it, no prompt).
    fn with_auto_accept(mut self, auto_accept: bool) -> Self {
        self.auto_accept = auto_accept;
        self
    }

    /// Configure this handler to pipe server-opened forwarded channels to a
    /// local target (remote forward, `-R`).
    fn with_forward(
        mut self,
        target_host: String,
        target_port: u16,
        tunnel_events: mpsc::Sender<TunnelEvent>,
    ) -> Self {
        self.forward_target = Some((target_host, target_port));
        self.tunnel_events = Some(tunnel_events);
        self
    }

    fn emit_prompt(
        &self,
        key: &russh::keys::ssh_key::PublicKey,
        fingerprint: &str,
        previous_fingerprint: Option<String>,
    ) {
        if let Some(events) = &self.events {
            let _ = events.send(LiveShellEvent::HostKeyPrompt(HostKeyPrompt {
                host: self.host.clone(),
                port: self.port,
                key_type: key.algorithm().to_string(),
                fingerprint: fingerprint.to_string(),
                previous_fingerprint,
            }));
        }
    }
}

impl client::Handler for KnownHostsClient {
    type Error = SshError;

    /// A remote-forwarded connection arrived: pipe it to the configured local
    /// target on a detached task so the session loop is not blocked.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        _connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if let (Some((host, port)), Some(events)) =
            (self.forward_target.clone(), self.tunnel_events.clone())
        {
            let origin = format!("{originator_address}:{originator_port}");
            let _ = events.send(TunnelEvent::Opened {
                peer: origin.clone(),
            });
            tokio::spawn(async move {
                if let Err(error) = pipe_forwarded(channel, &host, port).await {
                    let _ = events.send(TunnelEvent::Error(format!("{origin}: {error}")));
                }
                let _ = events.send(TunnelEvent::Closed { peer: origin });
            });
        }
        Ok(())
    }

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let host_spec = known_host_spec(&self.host, self.port);
        match host_key_status(
            &self.host,
            self.port,
            &self.known_hosts_path,
            server_public_key,
        )? {
            HostKeyStatus::Trusted { fingerprint } => {
                send_status(
                    self.events.as_ref(),
                    format!("host key verified: {fingerprint}"),
                );
                Ok(true)
            }
            HostKeyStatus::Unknown { fingerprint } => {
                // Non-interactive probe, or auto-accept enabled: trust-on-first-use
                // silently — record the new key and connect without prompting.
                if self.auto_accept || self.decision.is_none() {
                    self.decision.take();
                    append_known_host(&self.known_hosts_path, &host_spec, server_public_key)?;
                    send_status(
                        self.events.as_ref(),
                        format!("auto-trusted new host key for {host_spec}: {fingerprint}"),
                    );
                    return Ok(true);
                }
                let decision = self.decision.take().expect("decision present");
                self.emit_prompt(server_public_key, &fingerprint, None);
                if decision.await.unwrap_or(false) {
                    append_known_host(&self.known_hosts_path, &host_spec, server_public_key)?;
                    send_status(
                        self.events.as_ref(),
                        format!("recorded new host key for {host_spec}: {fingerprint}"),
                    );
                    Ok(true)
                } else {
                    Err(SshError::HostKeyRejected)
                }
            }
            HostKeyStatus::Changed {
                fingerprint,
                previous,
            } => {
                let Some(decision) = self.decision.take() else {
                    return Err(SshError::HostKeyChanged {
                        host: host_spec,
                        expected: previous.join(", "),
                        actual: fingerprint,
                        known_hosts_path: self.known_hosts_path.display().to_string(),
                    });
                };
                self.emit_prompt(server_public_key, &fingerprint, Some(previous.join(", ")));
                if decision.await.unwrap_or(false) {
                    replace_known_host(&self.known_hosts_path, &host_spec, server_public_key)?;
                    send_status(
                        self.events.as_ref(),
                        format!("updated host key for {host_spec}: {fingerprint}"),
                    );
                    Ok(true)
                } else {
                    Err(SshError::HostKeyRejected)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKeyStatus {
    Trusted {
        fingerprint: String,
    },
    Unknown {
        fingerprint: String,
    },
    Changed {
        fingerprint: String,
        previous: Vec<String>,
    },
}

/// Classify a server key against `known_hosts` without modifying the file.
fn host_key_status(
    host: &str,
    port: u16,
    known_hosts_path: &Path,
    server_public_key: &russh::keys::ssh_key::PublicKey,
) -> Result<HostKeyStatus, SshError> {
    let host_spec = known_host_spec(host, port);
    let actual = host_key_fingerprint(server_public_key);

    let content = match fs::read_to_string(known_hosts_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(SshError::KnownHosts(error.to_string())),
    };

    let mut previous = Vec::new();
    for line in content.lines() {
        let Some((hosts, public_key)) = parse_known_host_line(line) else {
            continue;
        };
        if !known_host_matches(&hosts, &host_spec) {
            continue;
        }
        let fingerprint = host_key_fingerprint(&public_key);
        if fingerprint == actual {
            return Ok(HostKeyStatus::Trusted { fingerprint: actual });
        }
        previous.push(fingerprint);
    }

    if previous.is_empty() {
        Ok(HostKeyStatus::Unknown { fingerprint: actual })
    } else {
        Ok(HostKeyStatus::Changed {
            fingerprint: actual,
            previous,
        })
    }
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

/// Replace every stored key for `host_spec` with `public_key` (used when the
/// user accepts a changed host key).
fn replace_known_host(
    known_hosts_path: &Path,
    host_spec: &str,
    public_key: &russh::keys::ssh_key::PublicKey,
) -> Result<(), SshError> {
    let content = match fs::read_to_string(known_hosts_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(SshError::KnownHosts(error.to_string())),
    };

    let mut kept = String::new();
    for line in content.lines() {
        let drop_line = parse_known_host_line(line)
            .is_some_and(|(hosts, _)| known_host_matches(&hosts, host_spec));
        if !drop_line {
            kept.push_str(line);
            kept.push('\n');
        }
    }

    let encoded_key = public_key
        .to_openssh()
        .map_err(|error| SshError::KnownHosts(error.to_string()))?;
    kept.push_str(&format!("{host_spec} {encoded_key}\n"));

    if let Some(parent) = known_hosts_path.parent() {
        fs::create_dir_all(parent).map_err(|error| SshError::KnownHosts(error.to_string()))?;
    }
    fs::write(known_hosts_path, kept).map_err(|error| SshError::KnownHosts(error.to_string()))
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

/// One trusted host key recorded in a `known_hosts` file (for the management UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHostEntry {
    /// `host` or `[host]:port`.
    pub host: String,
    /// The key algorithm, e.g. `ssh-ed25519`.
    pub key_type: String,
    /// The OpenSSH `SHA256:…` fingerprint.
    pub fingerprint: String,
}

/// List the trusted host keys in a `known_hosts` file (one row per host). A
/// missing/unreadable file yields an empty list. Tolerant of OpenSSH markers
/// (`@revoked`/`@cert-authority`), comment lines, and `[host]:port` specs.
#[must_use]
pub fn list_known_hosts(path: &Path) -> Vec<KnownHostEntry> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let Some(first) = tokens.next() else { continue };
        let hosts = if first.starts_with('@') {
            match tokens.next() {
                Some(host) => host,
                None => continue,
            }
        } else {
            first
        };
        let (Some(key_type), Some(key_body)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        let Ok(public_key) =
            format!("{key_type} {key_body}").parse::<russh::keys::ssh_key::PublicKey>()
        else {
            continue;
        };
        let fingerprint = host_key_fingerprint(&public_key);
        // A line may pin several hosts (comma-separated) — emit one row each.
        for host in hosts.split(',').filter(|host| !host.is_empty()) {
            entries.push(KnownHostEntry {
                host: host.to_string(),
                key_type: key_type.to_string(),
                fingerprint: fingerprint.clone(),
            });
        }
    }
    entries
}

/// Remove the trusted key matching `host` + `fingerprint` from a `known_hosts`
/// file (the "forget this host" action — the host reverts to first-connect).
pub fn remove_known_host(path: &Path, host: &str, fingerprint: &str) -> Result<(), SshError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(SshError::KnownHosts(error.to_string())),
    };
    let mut kept = String::new();
    for line in content.lines() {
        let drop_line = parse_known_host_line(line).is_some_and(|(hosts, key)| {
            known_host_matches(&hosts, host) && host_key_fingerprint(&key) == fingerprint
        });
        if !drop_line {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    fs::write(path, kept).map_err(|error| SshError::KnownHosts(error.to_string()))
}

/// The `known_hosts` file Adit records trusted host keys in.
#[must_use]
pub fn known_hosts_path() -> PathBuf {
    default_known_hosts_path()
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
    use russh::client::Handler as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn current_thread_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn join_remote_path_handles_root_and_nested() {
        assert_eq!(join_remote_path("/home/user", "docs"), "/home/user/docs");
        // A trailing slash on the parent is not doubled.
        assert_eq!(join_remote_path("/home/user/", "docs"), "/home/user/docs");
        // Root (or empty) yields a single leading slash.
        assert_eq!(join_remote_path("/", "docs"), "/docs");
        assert_eq!(join_remote_path("", "docs"), "/docs");
    }

    #[test]
    fn should_autofill_password_distinguishes_password_from_mfa_and_new_password() {
        // The account password / key passphrase (masked): auto-fillable.
        assert!(should_autofill_password("Password: ", false));
        assert!(should_autofill_password("Password for user:", false));
        assert!(should_autofill_password(
            "Enter passphrase for key /home/x/.ssh/id_ed25519:",
            false
        ));
        // A masked field with a localized or empty label is still the password
        // (mirrors the pre-MFA behaviour — no regression).
        assert!(should_autofill_password("Passwort: ", false));
        assert!(should_autofill_password("", false));
        // Second factors / one-time codes must NOT be auto-filled.
        assert!(!should_autofill_password("Verification code: ", false));
        assert!(!should_autofill_password("One-time password (OTP): ", false));
        assert!(!should_autofill_password("OTP: ", false));
        assert!(!should_autofill_password("Passcode or option (1-3): ", false));
        assert!(!should_autofill_password("Duo two-factor login", false));
        // Hardware tokens (RSA SecurID PIN, Yubikey) are second factors too.
        assert!(!should_autofill_password("Enter PIN: ", false));
        assert!(!should_autofill_password("YubiKey: ", false));
        // Setting a NEW password must NOT reuse the stored (old) one.
        assert!(!should_autofill_password("New password: ", false));
        assert!(!should_autofill_password("Retype new password: ", false));
        assert!(!should_autofill_password("Confirm password: ", false));
        // Echoed fields (e.g. a username) are asked, not auto-filled.
        assert!(!should_autofill_password("Username: ", true));
    }

    #[test]
    fn fill_answers_splices_user_input_into_empty_slots() {
        let answers = vec![
            Some("pw".to_string()),
            None,
            Some("x".to_string()),
            None,
        ];
        let got = fill_answers(answers, ["code".to_string(), "more".to_string()].into_iter());
        assert_eq!(got, vec!["pw", "code", "x", "more"]);

        // Fewer user answers than empty slots ⇒ the remainder become empty strings.
        let answers = vec![None, None];
        let got = fill_answers(answers, ["only".to_string()].into_iter());
        assert_eq!(got, vec!["only", ""]);
    }

    #[test]
    fn kbd_round_auto_fills_password_but_prompts_for_a_code() {
        let rt = current_thread_rt();
        rt.block_on(async {
            let (events_tx, events_rx) = mpsc::channel::<LiveShellEvent>();
            let (resp_tx, resp_rx) = tokio_mpsc::unbounded_channel::<Vec<String>>();
            let mut kbd = InteractiveKbd {
                events: &events_tx,
                responses: resp_rx,
            };

            // A password-only round auto-fills silently — no AuthPrompt emitted.
            let pw = [client::Prompt {
                prompt: "Password: ".into(),
                echo: false,
            }];
            let answers = keyboard_interactive_round_answers(
                "",
                "",
                &pw,
                "hunter2",
                Some(&events_tx),
                Some(&mut kbd),
            )
            .await
            .unwrap();
            assert_eq!(answers, vec!["hunter2".to_string()]);
            assert!(events_rx.try_recv().is_err(), "no event for a silent round");

            // A verification-code round emits an AuthPrompt and returns the user's code.
            resp_tx.send(vec!["123456".to_string()]).unwrap();
            let code = [client::Prompt {
                prompt: "Verification code: ".into(),
                echo: false,
            }];
            let answers = keyboard_interactive_round_answers(
                "MFA",
                "Enter code",
                &code,
                "hunter2",
                Some(&events_tx),
                Some(&mut kbd),
            )
            .await
            .unwrap();
            assert_eq!(answers, vec!["123456".to_string()]);
            let mut saw_prompt = false;
            while let Ok(event) = events_rx.try_recv() {
                if let LiveShellEvent::AuthPrompt(request) = event {
                    assert_eq!(request.prompts.len(), 1);
                    assert_eq!(request.prompts[0].prompt, "Verification code: ");
                    saw_prompt = true;
                }
            }
            assert!(saw_prompt, "a code round must surface an AuthPrompt");

            // A mixed round: password auto-filled, only the code asked of the user.
            resp_tx.send(vec!["999".to_string()]).unwrap();
            let mixed = [
                client::Prompt {
                    prompt: "Password: ".into(),
                    echo: false,
                },
                client::Prompt {
                    prompt: "Verification code: ".into(),
                    echo: false,
                },
            ];
            let answers = keyboard_interactive_round_answers(
                "",
                "",
                &mixed,
                "hunter2",
                Some(&events_tx),
                Some(&mut kbd),
            )
            .await
            .unwrap();
            assert_eq!(answers, vec!["hunter2".to_string(), "999".to_string()]);

            // Cancelling (empty answers) aborts auth with a distinct error, so the
            // connect does not silently fall through to key/agent methods.
            resp_tx.send(Vec::new()).unwrap();
            let result = keyboard_interactive_round_answers(
                "",
                "",
                &code,
                "hunter2",
                Some(&events_tx),
                Some(&mut kbd),
            )
            .await;
            assert!(matches!(result, Err(SshError::AuthenticationCancelled)));
        });
    }

    #[test]
    fn kbd_round_without_interactive_channel_falls_back_to_heuristic() {
        let rt = current_thread_rt();
        rt.block_on(async {
            // SFTP/tunnel path: no user to ask, so best-effort with the password.
            let code = [client::Prompt {
                prompt: "Verification code: ".into(),
                echo: false,
            }];
            let answers = keyboard_interactive_round_answers("", "", &code, "pw", None, None)
                .await
                .unwrap();
            assert_eq!(answers, vec!["pw".to_string()]);
        });
    }

    #[test]
    fn is_safe_child_name_rejects_traversal() {
        assert!(is_safe_child_name("report.txt"));
        assert!(is_safe_child_name("my folder"));
        assert!(is_safe_child_name(".hidden"));
        // Server-controlled names that would escape the target or loop forever.
        assert!(!is_safe_child_name(""));
        assert!(!is_safe_child_name("."));
        assert!(!is_safe_child_name(".."));
        assert!(!is_safe_child_name("../evil"));
        assert!(!is_safe_child_name("..\\evil.exe"));
        assert!(!is_safe_child_name("a/b"));
        assert!(!is_safe_child_name("/abs"));
    }

    #[test]
    fn socks5_negotiate_parses_domain_connect() {
        current_thread_rt().block_on(async {
            let (mut client, mut server) = tokio::io::duplex(256);
            // Greeting (1 method: no-auth) + CONNECT example.com:443.
            let mut request = vec![0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x03, 0x0B];
            request.extend_from_slice(b"example.com");
            request.extend_from_slice(&[0x01, 0xBB]);
            client.write_all(&request).await.unwrap();

            let (host, port) = socks5_negotiate(&mut server).await.unwrap();
            assert_eq!(host, "example.com");
            assert_eq!(port, 443);

            // The negotiator must have selected the no-auth method.
            let mut reply = [0u8; 2];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [0x05, 0x00]);
        });
    }

    #[test]
    fn socks5_negotiate_parses_ipv4_connect() {
        current_thread_rt().block_on(async {
            let (mut client, mut server) = tokio::io::duplex(256);
            // Greeting + CONNECT 127.0.0.1:8080.
            client
                .write_all(&[
                    0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0x1F, 0x90,
                ])
                .await
                .unwrap();
            let (host, port) = socks5_negotiate(&mut server).await.unwrap();
            assert_eq!(host, "127.0.0.1");
            assert_eq!(port, 8080);
        });
    }

    const ED25519_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
    const ECDSA_KEY: &str =
        "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBHwf2HMM5TRXvo2SQJjsNkiDD5KqiiNjrGVv3UUh+mMT5RHxiRtOnlqvjhQtBq0VpmpCV/PwUdhOig4vkbqAcEc=";

    #[test]
    fn unknown_host_recorded_then_trusted() {
        let path = temp_known_hosts_path("record");
        let key = public_key(ED25519_KEY);

        let first = host_key_status("192.168.1.20", 22, &path, &key).expect("status");
        assert!(matches!(first, HostKeyStatus::Unknown { .. }));

        append_known_host(&path, &known_host_spec("192.168.1.20", 22), &key).expect("record");

        let second = host_key_status("192.168.1.20", 22, &path, &key).expect("status");
        assert!(matches!(second, HostKeyStatus::Trusted { .. }));

        let content = fs::read_to_string(&path).expect("known_hosts should exist");
        assert!(content.contains("192.168.1.20 ssh-ed25519"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_default_port_uses_bracketed_host_spec() {
        let path = temp_known_hosts_path("port");
        let key = public_key(ED25519_KEY);

        assert!(matches!(
            host_key_status("example.com", 2222, &path, &key).expect("status"),
            HostKeyStatus::Unknown { .. }
        ));

        let spec = known_host_spec("example.com", 2222);
        assert_eq!(spec, "[example.com]:2222");
        append_known_host(&path, &spec, &key).expect("record");
        let content = fs::read_to_string(&path).expect("known_hosts");
        assert!(content.contains("[example.com]:2222 ssh-ed25519"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn lists_and_removes_trusted_host_keys() {
        let path = temp_known_hosts_path("manage");
        let ed = public_key(ED25519_KEY);
        let ec = public_key(ECDSA_KEY);
        append_known_host(&path, &known_host_spec("host-a", 22), &ed).unwrap();
        append_known_host(&path, &known_host_spec("host-b", 2222), &ec).unwrap();

        let entries = list_known_hosts(&path);
        assert_eq!(entries.len(), 2);
        let a = entries.iter().find(|e| e.host == "host-a").expect("host-a listed");
        assert_eq!(a.key_type, "ssh-ed25519");
        assert!(a.fingerprint.starts_with("SHA256:"));
        assert!(entries.iter().any(|e| e.host == "[host-b]:2222"
            && e.key_type == "ecdsa-sha2-nistp256"));

        // Removing by (host, fingerprint) drops only that entry.
        remove_known_host(&path, "host-a", &a.fingerprint).unwrap();
        let after = list_known_hosts(&path);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].host, "[host-b]:2222");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn changed_host_key_is_detected_and_replaceable() {
        let path = temp_known_hosts_path("changed");
        let original = public_key(ED25519_KEY);
        let changed = public_key(ECDSA_KEY);
        let spec = known_host_spec("node5", 22);

        append_known_host(&path, &spec, &original).expect("record original");

        match host_key_status("node5", 22, &path, &changed).expect("status") {
            HostKeyStatus::Changed {
                fingerprint,
                previous,
            } => {
                assert!(fingerprint.starts_with("SHA256:"));
                assert_eq!(previous.len(), 1);
                assert!(previous[0].starts_with("SHA256:"));
                assert_ne!(previous[0], fingerprint);
            }
            other => panic!("expected Changed, got {other:?}"),
        }

        // Accepting the change replaces the stored key; it then verifies trusted
        // and the old entry is gone.
        replace_known_host(&path, &spec, &changed).expect("replace");
        assert!(matches!(
            host_key_status("node5", 22, &path, &changed).expect("status"),
            HostKeyStatus::Trusted { .. }
        ));
        let content = fs::read_to_string(&path).expect("known_hosts");
        assert_eq!(content.matches("node5").count(), 1, "old key should be gone");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn interactive_unknown_host_prompts_then_records_on_accept() {
        let path = temp_known_hosts_path("interactive-accept");
        let key = public_key(ED25519_KEY);
        let (events_tx, events_rx) = mpsc::channel();
        let (decision_tx, decision_rx) = oneshot::channel();
        let mut handler = KnownHostsClient::new(
            "node-x".into(),
            22,
            path.clone(),
            Some(events_tx),
            Some(decision_rx),
        );

        decision_tx.send(true).expect("send decision");
        let trusted = current_thread_rt()
            .block_on(handler.check_server_key(&key))
            .expect("accept should yield Ok");
        assert!(trusted);

        let event = events_rx.try_recv().expect("a prompt event");
        assert!(matches!(
            event,
            LiveShellEvent::HostKeyPrompt(prompt) if prompt.previous_fingerprint.is_none()
        ));
        assert!(fs::read_to_string(&path)
            .expect("known_hosts")
            .contains("node-x ssh-ed25519"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn auto_accept_records_unknown_host_without_prompting() {
        let path = temp_known_hosts_path("auto-accept");
        let key = public_key(ED25519_KEY);
        let (events_tx, events_rx) = mpsc::channel();
        let (_decision_tx, decision_rx) = oneshot::channel();
        let mut handler = KnownHostsClient::new(
            "node-z".into(),
            22,
            path.clone(),
            Some(events_tx),
            Some(decision_rx),
        )
        .with_auto_accept(true);

        let trusted = current_thread_rt()
            .block_on(handler.check_server_key(&key))
            .expect("auto-accept should yield Ok");
        assert!(trusted);
        // No confirmation prompt was emitted, and the key was recorded.
        assert!(
            !matches!(events_rx.try_recv(), Ok(LiveShellEvent::HostKeyPrompt(_))),
            "auto-accept must not prompt"
        );
        assert!(fs::read_to_string(&path)
            .expect("known_hosts")
            .contains("node-z ssh-ed25519"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn interactive_unknown_host_aborts_on_reject() {
        let path = temp_known_hosts_path("interactive-reject");
        let key = public_key(ED25519_KEY);
        let (events_tx, _events_rx) = mpsc::channel();
        let (decision_tx, decision_rx) = oneshot::channel();
        let mut handler = KnownHostsClient::new(
            "node-y".into(),
            22,
            path.clone(),
            Some(events_tx),
            Some(decision_rx),
        );

        decision_tx.send(false).expect("send decision");
        let error = current_thread_rt()
            .block_on(handler.check_server_key(&key))
            .expect_err("reject should abort");
        assert!(matches!(error, SshError::HostKeyRejected));
        assert!(!path.exists(), "rejected key must not be recorded");

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
