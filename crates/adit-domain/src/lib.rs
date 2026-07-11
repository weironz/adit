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
    /// Jump hosts (bastions) to chain through before reaching `host`, in order
    /// (OpenSSH `ProxyJump`). Empty ⇒ connect directly.
    #[serde(default)]
    pub jumps: Vec<JumpHop>,
}

/// One bastion/jump host in a [`ConnectionProfile`]'s chain. Authenticates via
/// SSH agent / default keys / the profile's identity file (no per-hop password
/// yet).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JumpHop {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
}

fn default_ssh_port() -> u16 {
    22
}

/// Parse a TCP port, accepting only 1–65535 (0 is not a connectable port).
fn parse_port(text: &str) -> Option<u16> {
    match text.trim().parse::<u16>() {
        Ok(0) => None,
        Ok(port) => Some(port),
        Err(_) => None,
    }
}

impl JumpHop {
    /// Parse an OpenSSH-style `[user@]host[:port]` hop spec. Handles IPv6 the
    /// way ssh does: a bracketed literal (`[2001:db8::1]` or `[2001:db8::1]:22`)
    /// and a bare literal with no port (`2001:db8::1`, several colons ⇒ all host).
    #[must_use]
    pub fn parse(spec: &str) -> Option<JumpHop> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let (username, rest) = match spec.split_once('@') {
            Some((user, rest)) => (user.trim().to_string(), rest.trim()),
            None => (String::new(), spec),
        };
        let (host, port) = Self::split_host_port(rest)?;
        if host.is_empty() {
            return None;
        }
        Some(JumpHop {
            host,
            port,
            username,
        })
    }

    /// Split `host[:port]` into a host and a port (default 22), respecting IPv6
    /// brackets and bare IPv6 literals. An explicit port of 0 is rejected.
    fn split_host_port(rest: &str) -> Option<(String, u16)> {
        if let Some(stripped) = rest.strip_prefix('[') {
            // Bracketed IPv6: `[addr]` or `[addr]:port`.
            let (addr, after) = stripped.split_once(']')?;
            let port = match after.trim() {
                "" => 22,
                p => parse_port(p.strip_prefix(':')?)?,
            };
            return Some((addr.trim().to_string(), port));
        }
        // A bare literal with two or more colons is an unbracketed IPv6 address
        // with no port — `rsplit_once(':')` would corrupt it, so keep it whole.
        if rest.matches(':').count() >= 2 {
            return Some((rest.trim().to_string(), 22));
        }
        match rest.rsplit_once(':') {
            Some((host, port)) => Some((host.trim().to_string(), parse_port(port)?)),
            None => Some((rest.trim().to_string(), 22)),
        }
    }

    /// Render back to `[user@]host[:port]` (omitting default port 22, and
    /// bracketing an IPv6 literal so it round-trips through [`parse`]).
    #[must_use]
    pub fn to_spec(&self) -> String {
        let mut spec = String::new();
        if !self.username.is_empty() {
            spec.push_str(&self.username);
            spec.push('@');
        }
        let is_ipv6 = self.host.contains(':') && !self.host.starts_with('[');
        if is_ipv6 {
            spec.push('[');
            spec.push_str(&self.host);
            spec.push(']');
        } else {
            spec.push_str(&self.host);
        }
        if self.port != 22 {
            spec.push(':');
            spec.push_str(&self.port.to_string());
        }
        spec
    }
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
            jumps: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::JumpHop;

    #[test]
    fn jump_hop_parses_and_renders_specs() {
        let h = JumpHop::parse("bob@bastion.example.com:2222").unwrap();
        assert_eq!(h.username, "bob");
        assert_eq!(h.host, "bastion.example.com");
        assert_eq!(h.port, 2222);
        assert_eq!(h.to_spec(), "bob@bastion.example.com:2222");

        // Defaults: no user, default port 22 (omitted in the rendered spec).
        let h = JumpHop::parse("10.0.0.5").unwrap();
        assert_eq!(h.username, "");
        assert_eq!(h.host, "10.0.0.5");
        assert_eq!(h.port, 22);
        assert_eq!(h.to_spec(), "10.0.0.5");

        assert_eq!(JumpHop::parse("  ").map(|_| ()), None);
        assert_eq!(JumpHop::parse("").map(|_| ()), None);
    }

    #[test]
    fn jump_hop_parses_ipv6_literals() {
        // Bare IPv6, no port: kept whole (not split on the last colon).
        let h = JumpHop::parse("root@2001:db8::10").unwrap();
        assert_eq!(h.username, "root");
        assert_eq!(h.host, "2001:db8::10");
        assert_eq!(h.port, 22);
        // Round-trips with brackets so re-parsing recovers the same host.
        assert_eq!(h.to_spec(), "root@[2001:db8::10]");
        assert_eq!(JumpHop::parse(&h.to_spec()).unwrap(), h);

        // Bracketed IPv6 with a port.
        let h = JumpHop::parse("[2001:db8::10]:2222").unwrap();
        assert_eq!(h.host, "2001:db8::10");
        assert_eq!(h.port, 2222);
        assert_eq!(h.to_spec(), "[2001:db8::10]:2222");
        assert_eq!(JumpHop::parse(&h.to_spec()).unwrap(), h);

        // Bracketed IPv6 without a port.
        let h = JumpHop::parse("[::1]").unwrap();
        assert_eq!(h.host, "::1");
        assert_eq!(h.port, 22);

        // A normal host:port still splits as before.
        let h = JumpHop::parse("bastion:2200").unwrap();
        assert_eq!(h.host, "bastion");
        assert_eq!(h.port, 2200);

        // Port 0 and out-of-range ports are rejected (not a connectable port).
        assert_eq!(JumpHop::parse("bastion:0").map(|_| ()), None);
        assert_eq!(JumpHop::parse("[::1]:0").map(|_| ()), None);
        assert_eq!(JumpHop::parse("bastion:70000").map(|_| ()), None);
    }
}
