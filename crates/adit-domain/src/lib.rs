use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(Uuid);

impl ProfileId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: ProfileId,
    #[serde(default = "default_profile_group", alias = "folder")]
    pub group: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub auth_method: AuthMethod,
    #[serde(default)]
    pub identity_file: String,
    #[serde(default)]
    pub tunnels: Vec<TunnelDef>,
    #[serde(default)]
    pub protocol: Protocol,
    /// A command sent to the shell right after it opens (e.g. `tmux attach`,
    /// `cd /srv`). Empty means nothing is sent.
    #[serde(default)]
    pub startup_command: String,
    /// `TERM` to request for the PTY (empty ⇒ `xterm-256color`).
    #[serde(default)]
    pub terminal_type: String,
}

impl ConnectionProfile {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
    ) -> Self {
        Self::with_group("Default", name, host, port, username)
    }

    #[must_use]
    pub fn with_group(
        group: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
    ) -> Self {
        Self {
            id: ProfileId::new(),
            group: group.into(),
            name: name.into(),
            host: host.into(),
            port,
            username: username.into(),
            sort_order: 0,
            auth_method: AuthMethod::Auto,
            identity_file: String::new(),
            tunnels: Vec::new(),
            protocol: Protocol::Ssh,
            startup_command: String::new(),
            terminal_type: String::new(),
        }
    }

    #[must_use]
    pub fn with_folder(
        folder: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
    ) -> Self {
        Self::with_group(folder, name, host, port, username)
    }

    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

fn default_profile_group() -> String {
    String::from("Default")
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Auto,
    Password,
    Key,
    Agent,
}

impl AuthMethod {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Password => "密码",
            Self::Key => "密钥",
            Self::Agent => "Agent",
        }
    }
}

/// The connection protocol a session profile uses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Ssh,
    /// A local shell process (ConPTY on Windows, a PTY elsewhere).
    LocalShell,
    /// A serial port (COM/tty).
    Serial,
    /// Remote Desktop (graphical).
    Rdp,
}

impl Protocol {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ssh => "SSH",
            Self::LocalShell => "本地 Shell",
            Self::Serial => "串口",
            Self::Rdp => "RDP",
        }
    }

    /// Whether this protocol drives the built-in VT terminal (byte-stream based).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ssh | Self::LocalShell | Self::Serial)
    }
}

/// What kind of SSH port forward a tunnel performs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelKind {
    /// Local forward (`-L`): listen locally, dial out from the SSH server.
    #[default]
    Local,
    /// Dynamic SOCKS5 proxy (`-D`): listen locally, route each request via SSH.
    Dynamic,
    /// Remote forward (`-R`): server listens, the client dials a local target.
    Remote,
}

impl TunnelKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "本地转发 -L",
            Self::Dynamic => "动态 SOCKS -D",
            Self::Remote => "远程转发 -R",
        }
    }
}

fn default_bind_address() -> String {
    String::from("127.0.0.1")
}

/// A saved port-forward definition, persisted with a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelDef {
    pub kind: TunnelKind,
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    pub bind_port: u16,
    #[serde(default)]
    pub target_host: String,
    #[serde(default)]
    pub target_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting,
    Connected,
    Disconnected,
    Error,
}

impl SessionStatus {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Disconnected => "Disconnected",
            Self::Error => "Error",
        }
    }
}
