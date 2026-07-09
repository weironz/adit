use adit_domain::{
    AuthMethod, ConnectionProfile, ProfileId, Protocol, SessionId, SessionStatus, TunnelDef,
};
use adit_ssh::{
    AuthOptions, HostKeyPrompt, LiveShellCommand, LiveShellEvent, LiveShellHandle,
    LiveShellRequest, PasswordShellProbe, SftpCommand, SftpEvent, SftpHandle, SftpRequest, SshError,
    TunnelCommand, TunnelEvent, TunnelRequest,
};

pub use adit_ssh::HostKeyPrompt as HostKeyPromptInfo;
pub use adit_ssh::{SftpEntry, TunnelKind};
use adit_terminal::{TerminalCore, TerminalSize, TerminalSnapshot, Viewport, VtTerminal};
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

/// Give up auto-reconnect after this many consecutive failed attempts.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Cap on retained SFTP transfer-queue entries before old finished ones are dropped.
const MAX_TRANSFER_HISTORY: usize = 200;

/// Exponential backoff (1,2,4,8,16,30,30,…s) between reconnect attempts.
fn reconnect_delay(attempts: u32) -> Duration {
    Duration::from_secs((1u64 << attempts.min(5)).min(30))
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("profile was not found")]
    ProfileNotFound,
    #[error("session was not found")]
    SessionNotFound,
    #[error("profile name is required")]
    EmptyProfileName,
    #[error("profile host is required")]
    EmptyProfileHost,
    #[error("profile username is required")]
    EmptyProfileUsername,
    #[error("profile port must be between 1 and 65535")]
    InvalidProfilePort,
    #[error("ssh probe failed: {0}")]
    Ssh(#[from] SshError),
    #[error("session log failed: {0}")]
    Logging(String),
    #[error("no active SSH session for SFTP")]
    NoActiveSshSession,
    #[error("sftp failed: {0}")]
    Sftp(String),
    #[error("{0}")]
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: SessionId,
    pub profile_id: ProfileId,
    pub title: String,
    pub endpoint: String,
    pub status: SessionStatus,
}

#[derive(Debug, Clone)]
pub struct SshProbeSession {
    pub profile_id: ProfileId,
    pub title: String,
    pub endpoint: String,
    pub transcript: String,
}

struct SessionRecord {
    summary: SessionSummary,
    terminal: VtTerminal,
    live: Option<LiveShellHandle>,
    log: Option<SessionLog>,
    pending_host_key: Option<HostKeyPrompt>,
    reconnect: Option<ReconnectState>,
}

/// An open transcript log for a session: raw PTY output is appended here while
/// logging is enabled.
struct SessionLog {
    path: PathBuf,
    writer: BufWriter<fs::File>,
}

/// Auto-reconnect bookkeeping for a live SSH session.
struct ReconnectState {
    /// Credential to reuse when respawning (empty for key/agent auth).
    password: String,
    /// Consecutive failed attempts since the last successful connect.
    attempts: u32,
    /// When the next attempt is due (set after an unexpected drop).
    retry_at: Option<Instant>,
    /// The user disconnected or the shell exited; do not reconnect.
    manual: bool,
    /// Whether the session ever reached "connected" — only established sessions
    /// auto-reconnect, so a bad password / unreachable host does not loop.
    ever_connected: bool,
}

impl ReconnectState {
    fn new(password: String) -> Self {
        Self {
            password,
            attempts: 0,
            retry_at: None,
            manual: false,
            ever_connected: false,
        }
    }
}

/// A dual-pane SFTP file manager (local on one side, remote on the other),
/// tied to a connected session's profile.
pub struct SftpBrowser {
    handle: SftpHandle,
    pub profile_id: ProfileId,
    pub endpoint: String,
    // Remote pane.
    pub cwd: String,
    pub entries: Vec<SftpEntry>,
    // Local pane.
    pub local_cwd: PathBuf,
    pub local_entries: Vec<LocalEntry>,
    pub status: String,
    pub busy: bool,
    pub connected: bool,
    /// Transfer queue/history (most recent last).
    pub transfers: Vec<TransferItem>,
}

/// One local-filesystem entry shown in the local pane.
#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Last-modified time as seconds since the Unix epoch (UTC).
    pub mtime: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub direction: TransferDirection,
    pub name: String,
    /// Full source path.
    pub source: String,
    /// Full destination path (the key detail: where a download landed locally).
    pub dest: String,
    pub done: u64,
    pub total: u64,
    pub status: TransferStatus,
    /// Current transfer speed in bytes/sec.
    pub bps: u64,
    /// Failure reason, set when `status` becomes `Failed`.
    pub error: Option<String>,
    started_at: Option<Instant>,
}

impl TransferItem {
    fn new(direction: TransferDirection, name: String, source: String, dest: String) -> Self {
        Self {
            direction,
            name,
            source,
            dest,
            done: 0,
            total: 0,
            status: TransferStatus::Pending,
            bps: 0,
            error: None,
            started_at: Some(Instant::now()),
        }
    }

    fn update_speed(&mut self, now: Instant) {
        if let Some(start) = self.started_at {
            let elapsed = now.duration_since(start).as_secs_f64();
            if elapsed > 0.05 {
                self.bps = (self.done as f64 / elapsed) as u64;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Pending,
    Active,
    Done,
    Failed,
}

pub struct SessionManager {
    profiles: Vec<ConnectionProfile>,
    sessions: HashMap<SessionId, SessionRecord>,
    active_session: Option<SessionId>,
    auto_reconnect: bool,
    sftp: Option<SftpBrowser>,
    tunnels: Vec<TunnelState>,
    next_tunnel_id: u64,
}

/// A live port-forwarding tunnel and its observable state.
pub struct TunnelState {
    handle: adit_ssh::TunnelHandle,
    pub id: u64,
    pub kind: adit_ssh::TunnelKind,
    pub bind: String,
    pub target: String,
    pub status: String,
    pub listening: bool,
    pub active: usize,
    pub total: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileMove {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSortKey {
    Name,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileDropPosition {
    Before,
    After,
}

impl SessionManager {
    #[must_use]
    pub fn with_demo_profiles() -> Self {
        Self::with_profiles(vec![
            ConnectionProfile::with_group("Local", "local-lab", "127.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Production", "prod-web-01", "10.0.0.12", 22, "deploy"),
            ConnectionProfile::with_group("Build", "mac-build", "build-mac.local", 22, "builder"),
        ])
    }

    #[must_use]
    pub fn with_profiles(profiles: Vec<ConnectionProfile>) -> Self {
        let mut profiles = profiles;
        normalize_profile_sort_orders(&mut profiles);

        Self {
            profiles,
            sessions: HashMap::new(),
            active_session: None,
            auto_reconnect: true,
            sftp: None,
            tunnels: Vec::new(),
            next_tunnel_id: 0,
        }
    }

    #[must_use]
    pub fn auto_reconnect(&self) -> bool {
        self.auto_reconnect
    }

    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
    }

    #[must_use]
    pub fn profiles(&self) -> &[ConnectionProfile] {
        &self.profiles
    }

    #[must_use]
    pub fn profile(&self, profile_id: ProfileId) -> Option<&ConnectionProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.id == profile_id)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the profile editor's field set
    pub fn create_profile(
        &mut self,
        group: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth_method: AuthMethod,
        identity_file: impl Into<String>,
    ) -> Result<ProfileId, SessionError> {
        let mut profile = build_profile(
            group,
            name,
            host,
            port,
            username,
            auth_method,
            identity_file,
        )?;
        profile.sort_order = next_sort_order_for_group(&self.profiles, &profile.group);
        let profile_id = profile.id;
        self.profiles.push(profile);
        Ok(profile_id)
    }

    /// Duplicate an existing profile (a full copy in the same group, placed
    /// right after the original). Returns the new profile's id.
    pub fn duplicate_profile(&mut self, profile_id: ProfileId) -> Option<ProfileId> {
        let index = self.profiles.iter().position(|p| p.id == profile_id)?;
        let mut clone = self.profiles[index].clone();
        clone.id = ProfileId::new();
        clone.name = format!("{} 副本", clone.name);
        let new_id = clone.id;
        // Same sort_order as the original; the stable sidebar sort keeps the
        // copy immediately after it (it is inserted right after in the vec).
        self.profiles.insert(index + 1, clone);
        Some(new_id)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the profile editor's field set
    pub fn update_profile(
        &mut self,
        profile_id: ProfileId,
        group: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth_method: AuthMethod,
        identity_file: impl Into<String>,
    ) -> Result<(), SessionError> {
        let updated = build_profile(
            group,
            name,
            host,
            port,
            username,
            auth_method,
            identity_file,
        )?;
        let Some(index) = self
            .profiles
            .iter()
            .position(|profile| profile.id == profile_id)
        else {
            return Err(SessionError::ProfileNotFound);
        };

        let sort_order = if self.profiles[index].group == updated.group {
            self.profiles[index].sort_order
        } else {
            next_sort_order_for_group(&self.profiles, &updated.group)
        };

        let profile = &mut self.profiles[index];

        profile.group = updated.group;
        profile.name = updated.name;
        profile.host = updated.host;
        profile.port = updated.port;
        profile.username = updated.username;
        profile.sort_order = sort_order;
        profile.auth_method = updated.auth_method;
        profile.identity_file = updated.identity_file;

        let endpoint = profile.endpoint();
        for record in self.sessions.values_mut() {
            if record.summary.profile_id == profile_id {
                record.summary.title = profile.name.clone();
                record.summary.endpoint = endpoint.clone();
            }
        }

        Ok(())
    }

    pub fn move_profile(
        &mut self,
        profile_id: ProfileId,
        direction: ProfileMove,
    ) -> Result<(), SessionError> {
        normalize_profile_sort_orders(&mut self.profiles);

        let index = self
            .profiles
            .iter()
            .position(|profile| profile.id == profile_id)
            .ok_or(SessionError::ProfileNotFound)?;
        let group = self.profiles[index].group.clone();

        let mut ordered = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| profile.group == group)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        ordered
            .sort_by(|&left, &right| compare_profiles(&self.profiles[left], &self.profiles[right]));

        let Some(position) = ordered.iter().position(|candidate| *candidate == index) else {
            return Err(SessionError::ProfileNotFound);
        };

        let swap_position = match direction {
            ProfileMove::Up if position > 0 => Some(position - 1),
            ProfileMove::Down if position + 1 < ordered.len() => Some(position + 1),
            _ => None,
        };

        if let Some(swap_position) = swap_position {
            let other_index = ordered[swap_position];
            let sort_order = self.profiles[index].sort_order;
            self.profiles[index].sort_order = self.profiles[other_index].sort_order;
            self.profiles[other_index].sort_order = sort_order;
        }

        Ok(())
    }

    pub fn sort_profiles(&mut self, key: ProfileSortKey) {
        self.profiles.sort_by(|left, right| {
            left.group.cmp(&right.group).then_with(|| match key {
                ProfileSortKey::Name => left
                    .name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
                    .then_with(|| left.host.cmp(&right.host)),
                ProfileSortKey::Host => left.host.cmp(&right.host).then_with(|| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                }),
            })
        });
        renumber_profile_sort_orders(&mut self.profiles);
    }

    pub fn reorder_profile(
        &mut self,
        source_id: ProfileId,
        target_id: ProfileId,
        position: ProfileDropPosition,
    ) -> Result<(), SessionError> {
        if source_id == target_id {
            return Ok(());
        }

        normalize_profile_sort_orders(&mut self.profiles);

        let mut ordered = self.profiles.clone();
        ordered.sort_by(compare_profiles);

        let source_index = ordered
            .iter()
            .position(|profile| profile.id == source_id)
            .ok_or(SessionError::ProfileNotFound)?;
        let mut source = ordered.remove(source_index);

        let target_index = ordered
            .iter()
            .position(|profile| profile.id == target_id)
            .ok_or(SessionError::ProfileNotFound)?;

        source.group = ordered[target_index].group.clone();
        let insert_index = match position {
            ProfileDropPosition::Before => target_index,
            ProfileDropPosition::After => target_index + 1,
        };

        ordered.insert(insert_index, source);
        renumber_profile_sort_orders(&mut ordered);
        self.profiles = ordered;

        Ok(())
    }

    pub fn move_profile_to_group(
        &mut self,
        profile_id: ProfileId,
        group: impl Into<String>,
    ) -> Result<(), SessionError> {
        let group = normalize_group(group);
        let Some(index) = self
            .profiles
            .iter()
            .position(|profile| profile.id == profile_id)
        else {
            return Err(SessionError::ProfileNotFound);
        };

        let sort_order = self
            .profiles
            .iter()
            .filter(|profile| profile.id != profile_id && profile.group == group)
            .map(|profile| profile.sort_order)
            .max()
            .unwrap_or(0)
            + 10;

        self.profiles[index].group = group;
        self.profiles[index].sort_order = sort_order;
        normalize_profile_sort_orders(&mut self.profiles);

        Ok(())
    }

    pub fn rename_group(
        &mut self,
        old_group: impl AsRef<str>,
        new_group: impl Into<String>,
    ) -> Result<(), SessionError> {
        let old_group = old_group.as_ref();
        let new_group = normalize_group(new_group);
        let mut renamed = false;

        for profile in &mut self.profiles {
            if profile.group == old_group {
                profile.group = new_group.clone();
                renamed = true;
            }
        }

        if renamed {
            normalize_profile_sort_orders(&mut self.profiles);
        }

        Ok(())
    }

    pub fn delete_profile(&mut self, profile_id: ProfileId) -> Result<(), SessionError> {
        let original_len = self.profiles.len();
        self.profiles.retain(|profile| profile.id != profile_id);

        if self.profiles.len() == original_len {
            return Err(SessionError::ProfileNotFound);
        }

        Ok(())
    }

    #[must_use]
    pub fn sessions(&self) -> Vec<SessionSummary> {
        self.sessions
            .values()
            .map(|record| record.summary.clone())
            .collect()
    }

    #[must_use]
    pub fn active_session(&self) -> Option<SessionId> {
        self.active_session
    }

    #[must_use]
    pub fn active_session_summary(&self) -> Option<SessionSummary> {
        self.active_session
            .and_then(|session_id| self.sessions.get(&session_id))
            .map(|record| record.summary.clone())
    }

    pub fn open_mock_session(&mut self, profile_id: ProfileId) -> Result<SessionId, SessionError> {
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .ok_or(SessionError::ProfileNotFound)?;

        let session_id = SessionId::new();
        let endpoint = profile.endpoint();
        let terminal = welcome_terminal(&profile.name, &endpoint);
        let summary = SessionSummary {
            id: session_id,
            profile_id,
            title: profile.name.clone(),
            endpoint,
            status: SessionStatus::Connected,
        };

        self.sessions.insert(
            session_id,
            SessionRecord {
                summary,
                terminal,
                live: None,
                log: None,
                pending_host_key: None,
                reconnect: None,
            },
        );
        self.active_session = Some(session_id);

        Ok(session_id)
    }

    pub fn open_live_ssh_session(
        &mut self,
        profile_id: ProfileId,
        password: String,
    ) -> Result<SessionId, SessionError> {
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .ok_or(SessionError::ProfileNotFound)?
            .clone();

        let (live, endpoint, terminal, reconnect) = match profile.protocol {
            Protocol::Ssh => {
                let live = spawn_live_shell(&profile, &password)?;
                let endpoint = profile.endpoint();
                let mut terminal = live_shell_terminal(&profile.name, &endpoint);
                terminal.append_status(format!("connecting to {endpoint}"));
                (
                    live,
                    endpoint,
                    terminal,
                    Some(ReconnectState::new(password)),
                )
            }
            Protocol::LocalShell => {
                // `identity_file` doubles as an optional shell-program override
                // (unused by local shells, and never polluted by quick-connect the
                // way `host` is). Empty → the system default shell.
                let program = (!profile.identity_file.trim().is_empty())
                    .then(|| profile.identity_file.trim().to_string());
                let live = adit_ssh::spawn_local_shell(96, 28, program)?;
                let endpoint = String::from("本地 Shell");
                let terminal = local_shell_terminal(&profile.name);
                (live, endpoint, terminal, None)
            }
            Protocol::Serial => {
                let port_name = profile.host.trim();
                if port_name.is_empty() {
                    return Err(SessionError::Unsupported(String::from(
                        "请填写串口号（如 COM3）",
                    )));
                }
                // `identity_file` doubles as the baud rate (empty → 115200).
                let baud = profile
                    .identity_file
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(115_200);
                let live = adit_ssh::spawn_serial(port_name.to_string(), baud)?;
                let endpoint = format!("{port_name} @ {baud}");
                let terminal = serial_terminal(&profile.name, &endpoint);
                (live, endpoint, terminal, None)
            }
            Protocol::Rdp => {
                // RDP is launched externally (see `launch_rdp`); it never opens a
                // terminal session.
                return Err(SessionError::Unsupported(String::from(
                    "RDP 通过系统远程桌面客户端连接",
                )));
            }
        };

        let session_id = SessionId::new();
        let summary = SessionSummary {
            id: session_id,
            profile_id,
            title: profile.name.clone(),
            endpoint,
            status: SessionStatus::Connecting,
        };

        self.sessions.insert(
            session_id,
            SessionRecord {
                summary,
                terminal,
                live: Some(live),
                log: None,
                pending_host_key: None,
                reconnect,
            },
        );
        self.active_session = Some(session_id);

        Ok(session_id)
    }

    /// Set a profile's connection protocol (edited separately from the core
    /// fields so `update_profile` leaves it untouched).
    pub fn set_profile_protocol(&mut self, profile_id: ProfileId, protocol: Protocol) {
        if let Some(profile) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            profile.protocol = protocol;
        }
    }

    /// Launch an RDP profile in the system Remote Desktop client (mstsc). RDP is
    /// graphical, so it opens externally rather than in a terminal tab. Returns
    /// the endpoint that was launched.
    pub fn launch_rdp(&self, profile_id: ProfileId) -> Result<String, SessionError> {
        let profile = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .ok_or(SessionError::ProfileNotFound)?;

        let host = profile.host.trim();
        if host.is_empty() {
            return Err(SessionError::Unsupported(String::from("请填写 RDP 主机")));
        }
        // Port 0/22 means "unset for RDP" → the default RDP port.
        let port = if profile.port == 0 || profile.port == 22 {
            3389
        } else {
            profile.port
        };
        let endpoint = format!("{host}:{port}");

        // A generated .rdp file lets us prefill the address and username;
        // mstsc still prompts for the password (it is never passed on the CLI).
        let mut contents = format!(
            "full address:s:{endpoint}\r\nprompt for credentials:i:1\r\n"
        );
        if !profile.username.trim().is_empty() {
            contents.push_str(&format!("username:s:{}\r\n", profile.username.trim()));
        }

        let file = std::env::temp_dir().join(format!("adit-rdp-{}.rdp", sanitize_file(host)));
        std::fs::write(&file, contents)
            .map_err(|error| SessionError::Unsupported(format!("写入 RDP 文件失败: {error}")))?;

        std::process::Command::new("mstsc.exe")
            .arg(&file)
            .spawn()
            .map_err(|error| SessionError::Unsupported(format!("启动 mstsc 失败: {error}")))?;

        Ok(endpoint)
    }

    pub fn build_ssh_probe_session(
        profile: ConnectionProfile,
        password: String,
    ) -> Result<SshProbeSession, SessionError> {
        let mut request = PasswordShellProbe::new(
            profile.host.clone(),
            profile.port,
            profile.username.clone(),
            password,
        );
        request.auth = auth_options_for_profile(&profile, &request.password);
        request.cols = 96;
        request.rows = 28;

        let output = adit_ssh::probe_password_shell_blocking(request)?;

        let endpoint = profile.endpoint();

        Ok(SshProbeSession {
            profile_id: profile.id,
            title: profile.name,
            endpoint,
            transcript: output.transcript,
        })
    }

    pub fn open_ssh_probe_session(&mut self, probe: SshProbeSession) -> SessionId {
        let session_id = SessionId::new();
        let terminal = probe_terminal(&probe.title, &probe.endpoint, &probe.transcript);
        let summary = SessionSummary {
            id: session_id,
            profile_id: probe.profile_id,
            title: probe.title,
            endpoint: probe.endpoint,
            status: SessionStatus::Disconnected,
        };

        self.sessions.insert(
            session_id,
            SessionRecord {
                summary,
                terminal,
                live: None,
                log: None,
                pending_host_key: None,
                reconnect: None,
            },
        );
        self.active_session = Some(session_id);

        session_id
    }

    pub fn activate(&mut self, session_id: SessionId) -> Result<(), SessionError> {
        if self.sessions.contains_key(&session_id) {
            self.active_session = Some(session_id);
            Ok(())
        } else {
            Err(SessionError::SessionNotFound)
        }
    }

    pub fn close(&mut self, session_id: SessionId) {
        if let Some(record) = self.sessions.get(&session_id) {
            if let Some(live) = &record.live {
                let _ = live.send(LiveShellCommand::Disconnect);
            }
        }

        self.sessions.remove(&session_id);

        if self.active_session == Some(session_id) {
            self.active_session = self.sessions.keys().next().copied();
        }
    }

    pub fn disconnect(&mut self, session_id: SessionId) -> Result<(), SessionError> {
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;

        // Stop any auto-reconnect: this is a deliberate disconnect.
        if let Some(reconnect) = &mut record.reconnect {
            reconnect.manual = true;
            reconnect.retry_at = None;
        }

        if let Some(live) = &record.live {
            live.send(LiveShellCommand::Disconnect)?;
            record.summary.status = SessionStatus::Disconnected;
            record.terminal.append_status("disconnect requested");
        } else {
            // Already dropped and possibly waiting to reconnect — cancel it.
            record.summary.status = SessionStatus::Disconnected;
            record.terminal.append_status("auto-reconnect cancelled");
        }

        Ok(())
    }

    pub fn send_input_to_active(&mut self, input: impl Into<String>) -> Result<(), SessionError> {
        let session_id = self.active_session.ok_or(SessionError::SessionNotFound)?;
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;
        let input = input.into();

        if let Some(live) = &record.live {
            live.send(LiveShellCommand::Input(input.into_bytes()))?;
        } else {
            record.terminal.feed(input.as_bytes());
            record.terminal.feed(b"\r\n");
        }

        Ok(())
    }

    pub fn send_input_bytes_to_active(&mut self, input: Vec<u8>) -> Result<(), SessionError> {
        if input.is_empty() {
            return Ok(());
        }

        let session_id = self.active_session.ok_or(SessionError::SessionNotFound)?;
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;

        if let Some(live) = &record.live {
            live.send(LiveShellCommand::Input(input))?;
        } else {
            record.terminal.feed(&input);
        }

        Ok(())
    }

    /// Send raw bytes to every connected (live) session at once. Returns how
    /// many sessions received the input. Powers the UI's input-broadcast mode
    /// (fan-out administration); sessions whose channel is already gone are
    /// skipped rather than aborting the whole broadcast.
    pub fn send_input_bytes_broadcast(&mut self, input: Vec<u8>) -> Result<usize, SessionError> {
        if input.is_empty() {
            return Ok(0);
        }

        let mut sent = 0usize;
        for record in self.sessions.values_mut() {
            if let Some(live) = &record.live {
                if live.send(LiveShellCommand::Input(input.clone())).is_ok() {
                    sent += 1;
                }
            }
        }

        Ok(sent)
    }

    /// Number of sessions with a live (connected) backend — used by the UI to
    /// label how many terminals an input broadcast will reach.
    #[must_use]
    pub fn live_session_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|record| record.live.is_some())
            .count()
    }

    pub fn resize_active(&mut self, cols: u16, rows: u16) -> Result<(), SessionError> {
        let session_id = self.active_session.ok_or(SessionError::SessionNotFound)?;
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;

        record
            .terminal
            .resize(adit_terminal::TerminalSize::new(cols, rows));

        if let Some(live) = &record.live {
            live.send(LiveShellCommand::Resize { cols, rows })?;
        }

        Ok(())
    }

    pub fn clear_active_terminal(&mut self) -> Result<(), SessionError> {
        let session_id = self.active_session.ok_or(SessionError::SessionNotFound)?;
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;

        record.terminal.clear();
        record.terminal.append_status("terminal cleared");

        Ok(())
    }

    /// Begin appending the active session's raw PTY output to a log file under
    /// `dir`, using `file_name` (empty ⇒ an auto-generated name). Returns the
    /// log path. No-op (returns the existing path) if already logging.
    pub fn start_active_logging(
        &mut self,
        dir: &Path,
        file_name: &str,
    ) -> Result<PathBuf, SessionError> {
        let session_id = self.active_session.ok_or(SessionError::SessionNotFound)?;
        self.start_logging(session_id, dir, file_name)
    }

    /// Begin logging a specific session (used by manual toggle for the active
    /// session and by auto-log-on-connect for any session).
    pub fn start_logging(
        &mut self,
        session_id: SessionId,
        dir: &Path,
        file_name: &str,
    ) -> Result<PathBuf, SessionError> {
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;

        if let Some(log) = &record.log {
            return Ok(log.path.clone());
        }

        fs::create_dir_all(dir).map_err(|error| SessionError::Logging(error.to_string()))?;
        let name = sanitize_log_file_name(file_name, &record.summary.title, session_id);
        let path = dir.join(name);
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| SessionError::Logging(error.to_string()))?;

        let mut writer = BufWriter::new(file);
        let header = format!(
            "\n# adit session log\n# endpoint: {}\n\n",
            record.summary.endpoint
        );
        writer
            .write_all(header.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|error| SessionError::Logging(error.to_string()))?;

        record.log = Some(SessionLog {
            path: path.clone(),
            writer,
        });
        record
            .terminal
            .append_status(format!("logging to {}", path.display()));

        Ok(path)
    }

    /// Stop logging the active session, flushing the file. Returns its path.
    pub fn stop_active_logging(&mut self) -> Option<PathBuf> {
        let session_id = self.active_session?;
        let record = self.sessions.get_mut(&session_id)?;
        let mut log = record.log.take()?;
        let _ = log.writer.flush();
        record
            .terminal
            .append_status(format!("logging stopped: {}", log.path.display()));
        Some(log.path)
    }

    #[must_use]
    pub fn active_is_logging(&self) -> bool {
        self.active_session
            .and_then(|session_id| self.sessions.get(&session_id))
            .is_some_and(|record| record.log.is_some())
    }

    #[must_use]
    pub fn active_log_path(&self) -> Option<PathBuf> {
        let session_id = self.active_session?;
        self.sessions
            .get(&session_id)?
            .log
            .as_ref()
            .map(|log| log.path.clone())
    }

    /// Whether a specific session is currently logging (for auto-log-on-connect,
    /// which must not restart an already-logging session).
    #[must_use]
    pub fn session_is_logging(&self, session_id: SessionId) -> bool {
        self.sessions
            .get(&session_id)
            .is_some_and(|record| record.log.is_some())
    }

    // --- SFTP ---------------------------------------------------------------

    #[must_use]
    pub fn sftp_browser(&self) -> Option<&SftpBrowser> {
        self.sftp.as_ref()
    }

    #[must_use]
    pub fn sftp_is_open(&self) -> bool {
        self.sftp.is_some()
    }

    /// Open an SFTP browser for the active live session, reusing its
    /// credentials over a separate SSH connection.
    /// Live port-forwarding tunnels (most recently opened last).
    #[must_use]
    pub fn tunnels(&self) -> &[TunnelState] {
        &self.tunnels
    }

    /// Open a new tunnel reusing the active session's profile and credentials.
    pub fn open_tunnel(
        &mut self,
        kind: TunnelKind,
        bind_address: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    ) -> Result<(), SessionError> {
        let session_id = self.active_session.ok_or(SessionError::NoActiveSshSession)?;
        let record = self
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NoActiveSshSession)?;
        let reconnect = record
            .reconnect
            .as_ref()
            .ok_or(SessionError::NoActiveSshSession)?;
        let profile_id = record.summary.profile_id;
        let password = reconnect.password.clone();
        let profile = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .ok_or(SessionError::ProfileNotFound)?;

        let bind_address = if bind_address.trim().is_empty() {
            String::from("127.0.0.1")
        } else {
            bind_address
        };
        let bind = format!("{bind_address}:{bind_port}");
        let target = match kind {
            TunnelKind::Local => format!("{target_host}:{target_port}"),
            TunnelKind::Dynamic => String::from("SOCKS5 代理"),
            TunnelKind::Remote => format!("{target_host}:{target_port}"),
        };

        let mut request = TunnelRequest::new(
            profile.host.clone(),
            profile.port,
            profile.username.clone(),
            password.clone(),
            kind,
            bind_address,
            bind_port,
            target_host,
            target_port,
        );
        request.auth = auth_options_for_profile(&profile, &password);
        let handle = adit_ssh::spawn_tunnel_session(request)?;

        let id = self.next_tunnel_id;
        self.next_tunnel_id += 1;
        self.tunnels.push(TunnelState {
            handle,
            id,
            kind,
            bind,
            target,
            status: String::from("connecting"),
            listening: false,
            active: 0,
            total: 0,
            error: None,
        });
        Ok(())
    }

    /// Append a saved tunnel definition to a profile (deduplicated).
    pub fn add_profile_tunnel(&mut self, profile_id: ProfileId, def: TunnelDef) {
        if let Some(profile) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            if !profile.tunnels.contains(&def) {
                profile.tunnels.push(def);
            }
        }
    }

    /// Remove the saved tunnel definition at `index` from a profile.
    pub fn remove_profile_tunnel(&mut self, profile_id: ProfileId, index: usize) {
        if let Some(profile) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            if index < profile.tunnels.len() {
                profile.tunnels.remove(index);
            }
        }
    }

    /// Open every saved tunnel of a profile (called after it connects).
    pub fn start_profile_tunnels(&mut self, profile_id: ProfileId) {
        let defs = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .map(|p| p.tunnels.clone())
            .unwrap_or_default();
        for def in defs {
            let _ = self.open_tunnel(
                def.kind,
                def.bind_address,
                def.bind_port,
                def.target_host,
                def.target_port,
            );
        }
    }

    /// Close a tunnel by id (stops its listener and SSH connection).
    pub fn close_tunnel(&mut self, id: u64) {
        if let Some(index) = self.tunnels.iter().position(|t| t.id == id) {
            let tunnel = self.tunnels.remove(index);
            let _ = tunnel.handle.send(TunnelCommand::Disconnect);
        }
    }

    /// Drain tunnel events into observable state; drop stopped tunnels.
    pub fn poll_tunnel_events(&mut self) {
        let mut stopped = Vec::new();
        for tunnel in &mut self.tunnels {
            while let Some(event) = tunnel.handle.try_recv() {
                match event {
                    TunnelEvent::Status(status) => tunnel.status = status,
                    TunnelEvent::Listening { bind } => {
                        tunnel.listening = true;
                        tunnel.status = format!("监听 {bind}");
                    }
                    TunnelEvent::Opened { .. } => {
                        tunnel.active += 1;
                        tunnel.total += 1;
                    }
                    TunnelEvent::Closed { .. } => {
                        tunnel.active = tunnel.active.saturating_sub(1);
                    }
                    TunnelEvent::Error(error) => {
                        tunnel.error = Some(error.clone());
                        tunnel.status = format!("error: {error}");
                    }
                    TunnelEvent::Stopped => {
                        tunnel.listening = false;
                        stopped.push(tunnel.id);
                    }
                }
            }
        }
        if !stopped.is_empty() {
            self.tunnels.retain(|t| !stopped.contains(&t.id));
        }
    }

    pub fn open_sftp_for_active(&mut self) -> Result<(), SessionError> {
        if self.sftp.is_some() {
            return Ok(());
        }
        let session_id = self
            .active_session
            .ok_or(SessionError::NoActiveSshSession)?;
        let record = self
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NoActiveSshSession)?;
        let reconnect = record
            .reconnect
            .as_ref()
            .ok_or(SessionError::NoActiveSshSession)?;
        let profile_id = record.summary.profile_id;
        let endpoint = record.summary.endpoint.clone();
        let password = reconnect.password.clone();

        let profile = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .ok_or(SessionError::ProfileNotFound)?;

        let mut request = SftpRequest::new(
            profile.host.clone(),
            profile.port,
            profile.username.clone(),
            password.clone(),
        );
        request.auth = auth_options_for_profile(&profile, &password);
        let handle = adit_ssh::spawn_sftp_session(request)?;

        let local_cwd = default_local_dir();
        let local_entries = read_local_dir(&local_cwd);
        self.sftp = Some(SftpBrowser {
            handle,
            profile_id,
            endpoint,
            cwd: String::from("/"),
            entries: Vec::new(),
            local_cwd,
            local_entries,
            status: String::from("connecting"),
            busy: false,
            connected: false,
            transfers: Vec::new(),
        });
        Ok(())
    }

    // Local pane navigation.
    pub fn sftp_local_navigate(&mut self, name: &str) {
        if let Some(browser) = &mut self.sftp {
            let target = browser.local_cwd.join(name);
            if target.is_dir() {
                browser.local_cwd = target;
                browser.local_entries = read_local_dir(&browser.local_cwd);
            }
        }
    }

    pub fn sftp_local_up(&mut self) {
        if let Some(browser) = &mut self.sftp {
            if let Some(parent) = browser.local_cwd.parent() {
                browser.local_cwd = parent.to_path_buf();
                browser.local_entries = read_local_dir(&browser.local_cwd);
            }
        }
    }

    pub fn sftp_local_refresh(&mut self) {
        if let Some(browser) = &mut self.sftp {
            browser.local_entries = read_local_dir(&browser.local_cwd);
        }
    }

    /// Jump the local pane to a typed absolute path.
    pub fn sftp_local_goto(&mut self, path: &Path) {
        if let Some(browser) = &mut self.sftp {
            if path.is_dir() {
                browser.local_cwd = path.to_path_buf();
                browser.local_entries = read_local_dir(&browser.local_cwd);
            } else {
                browser.status = format!("本地目录不存在: {}", path.display());
            }
        }
    }

    /// Jump the remote pane to a typed path.
    pub fn sftp_goto(&mut self, path: &str) {
        if let Some(browser) = &mut self.sftp {
            let path = path.trim();
            if !path.is_empty() {
                browser.status = format!("opening {path}");
                let _ = browser.handle.send(SftpCommand::ListDir(path.to_string()));
            }
        }
    }

    pub fn sftp_mkdir(&mut self, name: &str) {
        if let Some(browser) = &mut self.sftp {
            let path = join_remote(&browser.cwd, name);
            let cwd = browser.cwd.clone();
            browser.status = format!("mkdir {name}…");
            let _ = browser.handle.send(SftpCommand::Mkdir(path));
            let _ = browser.handle.send(SftpCommand::ListDir(cwd));
        }
    }

    pub fn sftp_rename(&mut self, from: &str, to: &str) {
        if let Some(browser) = &mut self.sftp {
            let from_path = join_remote(&browser.cwd, from);
            let to_path = join_remote(&browser.cwd, to);
            let cwd = browser.cwd.clone();
            browser.status = format!("rename {from} → {to}…");
            let _ = browser.handle.send(SftpCommand::Rename {
                from: from_path,
                to: to_path,
            });
            let _ = browser.handle.send(SftpCommand::ListDir(cwd));
        }
    }

    pub fn sftp_delete(&mut self, name: &str, is_dir: bool) {
        if let Some(browser) = &mut self.sftp {
            let path = join_remote(&browser.cwd, name);
            let cwd = browser.cwd.clone();
            browser.status = format!("delete {name}…");
            let _ = browser.handle.send(SftpCommand::Remove { path, is_dir });
            let _ = browser.handle.send(SftpCommand::ListDir(cwd));
        }
    }

    /// Rename a file/folder in the local pane's current directory.
    pub fn sftp_local_rename(&mut self, from: &str, to: &str) {
        if let Some(browser) = &mut self.sftp {
            let from_path = browser.local_cwd.join(from);
            let to_path = browser.local_cwd.join(to);
            match fs::rename(&from_path, &to_path) {
                Ok(()) => {
                    browser.local_entries = read_local_dir(&browser.local_cwd);
                    browser.status = format!("renamed {from} → {to}");
                }
                Err(error) => browser.status = format!("本地重命名失败: {error}"),
            }
        }
    }

    /// Delete a file/folder in the local pane's current directory.
    pub fn sftp_local_delete(&mut self, name: &str, is_dir: bool) {
        if let Some(browser) = &mut self.sftp {
            let path = browser.local_cwd.join(name);
            let result = if is_dir {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match result {
                Ok(()) => {
                    browser.local_entries = read_local_dir(&browser.local_cwd);
                    browser.status = format!("deleted {name}");
                }
                Err(error) => browser.status = format!("本地删除失败: {error}"),
            }
        }
    }

    /// Drop finished (done/failed) transfers from the queue, keeping any that
    /// are still pending or active.
    pub fn sftp_clear_finished(&mut self) {
        if let Some(browser) = &mut self.sftp {
            browser
                .transfers
                .retain(|item| matches!(item.status, TransferStatus::Pending | TransferStatus::Active));
        }
    }

    pub fn close_sftp(&mut self) {
        if let Some(browser) = self.sftp.take() {
            let _ = browser.handle.send(SftpCommand::Disconnect);
        }
    }

    pub fn sftp_navigate(&mut self, name: &str) {
        if let Some(browser) = &mut self.sftp {
            let target = join_remote(&browser.cwd, name);
            browser.status = format!("opening {target}");
            let _ = browser.handle.send(SftpCommand::ListDir(target));
        }
    }

    pub fn sftp_up(&mut self) {
        if let Some(browser) = &mut self.sftp {
            let parent = parent_remote(&browser.cwd);
            browser.status = format!("opening {parent}");
            let _ = browser.handle.send(SftpCommand::ListDir(parent));
        }
    }

    pub fn sftp_refresh(&mut self) {
        if let Some(browser) = &mut self.sftp {
            let cwd = browser.cwd.clone();
            let _ = browser.handle.send(SftpCommand::ListDir(cwd));
        }
    }

    /// Download a remote file into the current local pane directory.
    pub fn sftp_download(&mut self, name: &str) {
        if let Some(browser) = &mut self.sftp {
            let remote = join_remote(&browser.cwd, name);
            let local = browser.local_cwd.join(name);
            browser.transfers.push(TransferItem::new(
                TransferDirection::Download,
                name.to_string(),
                remote.clone(),
                local.display().to_string(),
            ));
            browser.busy = true;
            browser.status = format!("downloading {name}…");
            let _ = browser.handle.send(SftpCommand::Download { remote, local });
        }
    }

    /// Upload a file from the current local pane directory to the remote pane.
    pub fn sftp_upload_local(&mut self, name: &str) {
        if let Some(browser) = &mut self.sftp {
            let local = browser.local_cwd.join(name);
            let remote = join_remote(&browser.cwd, name);
            browser.transfers.push(TransferItem::new(
                TransferDirection::Upload,
                name.to_string(),
                local.display().to_string(),
                remote.clone(),
            ));
            browser.busy = true;
            browser.status = format!("uploading {name}…");
            let _ = browser.handle.send(SftpCommand::Upload { local, remote });
        }
    }

    /// Upload an arbitrary local file (e.g. from a file picker or a dropped
    /// file) to the remote pane's current directory.
    pub fn sftp_upload(&mut self, local: &Path) -> Result<(), SessionError> {
        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| SessionError::Sftp(String::from("invalid local file path")))?
            .to_string();
        let browser = self.sftp.as_mut().ok_or(SessionError::NoActiveSshSession)?;
        let remote = join_remote(&browser.cwd, &name);
        browser.transfers.push(TransferItem::new(
            TransferDirection::Upload,
            name.clone(),
            local.display().to_string(),
            remote.clone(),
        ));
        browser.busy = true;
        browser.status = format!("uploading {name}…");
        browser.handle.send(SftpCommand::Upload {
            local: local.to_path_buf(),
            remote,
        })?;
        Ok(())
    }

    fn poll_sftp_events(&mut self) {
        let mut closed = false;
        if let Some(browser) = &mut self.sftp {
            while let Some(event) = browser.handle.try_recv() {
                match event {
                    SftpEvent::Status(status) => browser.status = status,
                    SftpEvent::Ready { home } => {
                        browser.connected = true;
                        browser.cwd = home.clone();
                        browser.status = format!("connected: {home}");
                    }
                    SftpEvent::Listing { path, entries } => {
                        browser.cwd = path;
                        browser.entries = entries;
                        browser.busy = false;
                        browser.status = format!("{} item(s)", browser.entries.len());
                    }
                    SftpEvent::Progress { label, done, total } => {
                        browser.busy = true;
                        browser.status = match done.saturating_mul(100).checked_div(total) {
                            Some(percent) => format!("{label}: {percent}%"),
                            None => format!("{label}: {done} bytes"),
                        };
                        if let Some(item) = browser.transfers.iter_mut().find(|t| {
                            t.name == label
                                && matches!(t.status, TransferStatus::Pending | TransferStatus::Active)
                        }) {
                            item.status = TransferStatus::Active;
                            item.done = done;
                            item.total = total;
                            item.update_speed(Instant::now());
                        }
                    }
                    SftpEvent::Done { label, bytes } => {
                        browser.busy = false;
                        browser.status = format!("{label} done ({bytes} bytes)");
                        if let Some(item) = browser.transfers.iter_mut().find(|t| {
                            t.name == label
                                && matches!(t.status, TransferStatus::Pending | TransferStatus::Active)
                        }) {
                            item.status = TransferStatus::Done;
                            item.done = bytes;
                            if item.total == 0 {
                                item.total = bytes;
                            }
                            item.update_speed(Instant::now());
                        }
                        // Refresh both panes — a transferred file appears on the
                        // other side.
                        let cwd = browser.cwd.clone();
                        let _ = browser.handle.send(SftpCommand::ListDir(cwd));
                        browser.local_entries = read_local_dir(&browser.local_cwd);
                    }
                    SftpEvent::Error(error) => {
                        browser.busy = false;
                        if let Some(item) = browser
                            .transfers
                            .iter_mut()
                            .find(|t| matches!(t.status, TransferStatus::Pending | TransferStatus::Active))
                        {
                            item.status = TransferStatus::Failed;
                            item.error = Some(error.clone());
                        }
                        browser.status = format!("error: {error}");
                    }
                    SftpEvent::Closed => {
                        closed = true;
                        break;
                    }
                }
            }
            // Bound queue growth over long sessions: drop the oldest finished
            // transfers once the history gets large (active ones are kept).
            while browser.transfers.len() > MAX_TRANSFER_HISTORY {
                match browser
                    .transfers
                    .iter()
                    .position(|item| matches!(item.status, TransferStatus::Done | TransferStatus::Failed))
                {
                    Some(index) => {
                        browser.transfers.remove(index);
                    }
                    None => break,
                }
            }
        }
        if closed {
            self.sftp = None;
        }
    }

    /// The first session whose connect is paused awaiting a host-key decision.
    #[must_use]
    pub fn pending_host_key(&self) -> Option<(SessionId, HostKeyPrompt)> {
        self.sessions
            .iter()
            .find_map(|(id, record)| record.pending_host_key.clone().map(|prompt| (*id, prompt)))
    }

    /// Answer a pending host-key prompt: accept records/replaces the key and lets
    /// the handshake continue; reject aborts the connection.
    pub fn respond_host_key(
        &mut self,
        session_id: SessionId,
        accept: bool,
    ) -> Result<(), SessionError> {
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;
        record.pending_host_key = None;
        if let Some(live) = &record.live {
            live.send(LiveShellCommand::HostKeyDecision(accept))?;
        }
        record.terminal.append_status(if accept {
            "host key trusted"
        } else {
            "host key rejected"
        });
        Ok(())
    }

    pub fn poll_events(&mut self) -> usize {
        let mut handled = 0;
        let auto_reconnect = self.auto_reconnect;

        for record in self.sessions.values_mut() {
            let mut closed = false;

            while let Some(event) = record.live.as_ref().and_then(LiveShellHandle::try_recv) {
                handled += 1;

                match event {
                    LiveShellEvent::Status(status) => {
                        record.terminal.append_status(&status);
                        if status == "connected" {
                            record.summary.status = SessionStatus::Connected;
                            if let Some(reconnect) = &mut record.reconnect {
                                reconnect.ever_connected = true;
                                reconnect.attempts = 0;
                                reconnect.retry_at = None;
                            }
                        } else if status.starts_with("exit status") {
                            // Remote shell ended on purpose; do not reconnect.
                            record.summary.status = SessionStatus::Disconnected;
                            if let Some(reconnect) = &mut record.reconnect {
                                reconnect.manual = true;
                            }
                        } else {
                            record.summary.status = SessionStatus::Connecting;
                        }
                    }
                    LiveShellEvent::Output(bytes) => {
                        record.terminal.feed(&bytes);
                        record.summary.status = SessionStatus::Connected;
                        let mut log_failed = false;
                        if let Some(log) = &mut record.log {
                            log_failed = log.writer.write_all(&bytes).is_err();
                        }
                        if log_failed {
                            record.log = None;
                            record.terminal.append_status("session log write failed; logging off");
                        }
                    }
                    LiveShellEvent::Error(error) => {
                        record.terminal.append_status(format!(
                            "error while connecting to {}: {error}",
                            record.summary.endpoint
                        ));
                        record.summary.status = SessionStatus::Error;
                    }
                    LiveShellEvent::Closed => {
                        closed = true;
                        let can_retry = auto_reconnect
                            && record
                                .reconnect
                                .as_ref()
                                .is_some_and(|rc| rc.ever_connected && !rc.manual);
                        let attempts = record.reconnect.as_ref().map_or(0, |rc| rc.attempts);

                        if can_retry && attempts < MAX_RECONNECT_ATTEMPTS {
                            let delay = reconnect_delay(attempts);
                            if let Some(reconnect) = &mut record.reconnect {
                                reconnect.retry_at = Some(Instant::now() + delay);
                            }
                            record.summary.status = SessionStatus::Connecting;
                            record.terminal.append_status(format!(
                                "connection lost; reconnecting in {}s",
                                delay.as_secs()
                            ));
                        } else {
                            if can_retry {
                                record.terminal.append_status(
                                    "auto-reconnect gave up after repeated failures",
                                );
                            } else {
                                record.terminal.append_status("disconnected");
                            }
                            record.summary.status = status_after_closed(record.summary.status);
                        }
                    }
                    LiveShellEvent::HostKeyPrompt(prompt) => {
                        record.summary.status = SessionStatus::Connecting;
                        record.terminal.append_status(if prompt.previous_fingerprint.is_some() {
                            "host key CHANGED — awaiting confirmation"
                        } else {
                            "unknown host key — awaiting confirmation"
                        });
                        record.pending_host_key = Some(prompt);
                    }
                }
            }

            // Forward any terminal-generated replies (cursor position reports,
            // device attributes) back to the remote PTY.
            if record.live.is_some() {
                let responses = record.terminal.take_responses();
                if !responses.is_empty() {
                    if let Some(live) = &record.live {
                        let _ = live.send(LiveShellCommand::Input(responses));
                    }
                }
            }

            // Keep the on-disk log current within a tick.
            if let Some(log) = &mut record.log {
                let _ = log.writer.flush();
            }

            if closed {
                record.live = None;
            }
        }

        self.drive_reconnects();
        self.poll_sftp_events();
        self.poll_tunnel_events();
        handled
    }

    /// Respawn any dropped session whose backoff timer has elapsed.
    fn drive_reconnects(&mut self) {
        let now = Instant::now();
        let due: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, record)| record.live.is_none())
            .filter(|(_, record)| {
                record
                    .reconnect
                    .as_ref()
                    .and_then(|rc| rc.retry_at)
                    .is_some_and(|at| at <= now)
            })
            .map(|(id, _)| *id)
            .collect();

        for session_id in due {
            self.attempt_reconnect(session_id);
        }
    }

    fn attempt_reconnect(&mut self, session_id: SessionId) {
        let Some(record) = self.sessions.get(&session_id) else {
            return;
        };
        let profile_id = record.summary.profile_id;
        let password = record
            .reconnect
            .as_ref()
            .map_or_else(String::new, |rc| rc.password.clone());
        let attempt_no = record.reconnect.as_ref().map_or(1, |rc| rc.attempts + 1);

        let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id).cloned() else {
            if let Some(record) = self.sessions.get_mut(&session_id) {
                record
                    .terminal
                    .append_status("reconnect aborted: profile no longer exists");
                record.summary.status = SessionStatus::Error;
                if let Some(reconnect) = &mut record.reconnect {
                    reconnect.retry_at = None;
                }
            }
            return;
        };

        match spawn_live_shell(&profile, &password) {
            Ok(live) => {
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    record
                        .terminal
                        .append_status(format!("reconnecting (attempt {attempt_no})…"));
                    record.live = Some(live);
                    record.summary.status = SessionStatus::Connecting;
                    if let Some(reconnect) = &mut record.reconnect {
                        reconnect.attempts = attempt_no;
                        reconnect.retry_at = None;
                    }
                }
            }
            Err(error) => {
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    if let Some(reconnect) = &mut record.reconnect {
                        reconnect.attempts = attempt_no;
                        if attempt_no >= MAX_RECONNECT_ATTEMPTS {
                            reconnect.retry_at = None;
                            record.summary.status = SessionStatus::Error;
                            record
                                .terminal
                                .append_status(format!("reconnect failed: {error}"));
                        } else {
                            reconnect.retry_at = Some(Instant::now() + reconnect_delay(attempt_no));
                        }
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn active_snapshot(&self, viewport: Viewport) -> TerminalSnapshot {
        self.active_session
            .and_then(|session_id| self.sessions.get(&session_id))
            .map(|record| record.terminal.snapshot(viewport))
            .unwrap_or_else(|| TerminalSnapshot::empty(Default::default()))
    }

    /// Snapshot of a specific session (used to render split-pane views, which
    /// show several sessions at once rather than only the active one).
    #[must_use]
    pub fn snapshot_for(&self, session_id: SessionId, viewport: Viewport) -> TerminalSnapshot {
        self.sessions
            .get(&session_id)
            .map(|record| record.terminal.snapshot(viewport))
            .unwrap_or_else(|| TerminalSnapshot::empty(Default::default()))
    }

    /// Summary (title/status/endpoint) for a specific session, if it exists.
    #[must_use]
    pub fn session_summary(&self, session_id: SessionId) -> Option<SessionSummary> {
        self.sessions
            .get(&session_id)
            .map(|record| record.summary.clone())
    }

    /// Resize a specific session's terminal + backend. Like [`Self::resize_active`]
    /// but targets any session, so each split pane can be fitted independently.
    pub fn resize_session(
        &mut self,
        session_id: SessionId,
        cols: u16,
        rows: u16,
    ) -> Result<(), SessionError> {
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound)?;

        record
            .terminal
            .resize(adit_terminal::TerminalSize::new(cols, rows));

        if let Some(live) = &record.live {
            live.send(LiveShellCommand::Resize { cols, rows })?;
        }

        Ok(())
    }

    #[must_use]
    pub fn status_line(&self) -> String {
        match self
            .active_session
            .and_then(|session_id| self.sessions.get(&session_id))
        {
            Some(record) => format!(
                "{} - {}",
                record.summary.status.label(),
                record.summary.endpoint
            ),
            None => String::from("Idle"),
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::with_demo_profiles()
    }
}

fn build_profile(
    group: impl Into<String>,
    name: impl Into<String>,
    host: impl Into<String>,
    port: u16,
    username: impl Into<String>,
    auth_method: AuthMethod,
    identity_file: impl Into<String>,
) -> Result<ConnectionProfile, SessionError> {
    let group = normalize_group(group);
    let name = name.into().trim().to_string();
    let host = host.into().trim().to_string();
    let username = username.into().trim().to_string();
    let identity_file = identity_file.into().trim().to_string();

    if name.is_empty() {
        return Err(SessionError::EmptyProfileName);
    }

    if host.is_empty() {
        return Err(SessionError::EmptyProfileHost);
    }

    if username.is_empty() {
        return Err(SessionError::EmptyProfileUsername);
    }

    if port == 0 {
        return Err(SessionError::InvalidProfilePort);
    }

    let mut profile = ConnectionProfile::with_group(group, name, host, port, username);
    profile.auth_method = auth_method;
    profile.identity_file = identity_file;

    Ok(profile)
}

/// Join a POSIX remote path component onto the current directory.
/// Keep only filename-safe characters (for the generated .rdp temp file name).
fn sanitize_file(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn join_remote(cwd: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_string();
    }
    let base = cwd.trim_end_matches('/');
    if base.is_empty() {
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The parent of a POSIX remote path (root stays root).
fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        None | Some(0) => String::from("/"),
        Some(index) => trimmed[..index].to_string(),
    }
}

/// Read a local directory into sorted entries (directories first).
fn read_local_dir(dir: &Path) -> Vec<LocalEntry> {
    let mut entries = Vec::new();
    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
            let size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
            let mtime = metadata.as_ref().and_then(|m| {
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
            });
            entries.push(LocalEntry {
                name,
                is_dir,
                size,
                mtime,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// The directory the local pane opens in: the user's home, else the CWD.
fn default_local_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Spawn a live SSH shell actor for `profile`, reusing `password` for password
/// auth. Shared by the initial connect and auto-reconnect.
fn spawn_live_shell(
    profile: &ConnectionProfile,
    password: &str,
) -> Result<LiveShellHandle, SessionError> {
    let mut request = LiveShellRequest::new(
        profile.host.clone(),
        profile.port,
        profile.username.clone(),
        password.to_string(),
    );
    request.auth = auth_options_for_profile(profile, password);
    request.cols = 96;
    request.rows = 28;
    Ok(adit_ssh::spawn_password_shell(request)?)
}

fn auth_options_for_profile(profile: &ConnectionProfile, password: &str) -> AuthOptions {
    let identity_file = (!profile.identity_file.trim().is_empty())
        .then(|| std::path::PathBuf::from(profile.identity_file.trim()));

    match profile.auth_method {
        AuthMethod::Auto => AuthOptions {
            try_password: !password.is_empty(),
            try_agent: true,
            try_default_keys: true,
            identity_file,
        },
        AuthMethod::Password => AuthOptions {
            try_password: true,
            try_agent: false,
            try_default_keys: false,
            identity_file: None,
        },
        AuthMethod::Key => AuthOptions {
            try_password: false,
            try_agent: false,
            try_default_keys: identity_file.is_none(),
            identity_file,
        },
        AuthMethod::Agent => AuthOptions {
            try_password: false,
            try_agent: true,
            try_default_keys: false,
            identity_file: None,
        },
    }
}

fn normalize_group(group: impl Into<String>) -> String {
    let group = group.into().trim().to_string();

    if group.is_empty() {
        String::from("Default")
    } else {
        group
    }
}

/// Build a filesystem-safe log file name from a session title and id, e.g.
/// `prod-web-01_a1b2c3d4.log`.
/// Filesystem-safe log filename. If `requested` is non-empty it is sanitized and
/// used (the UI renders its pattern into this); otherwise an auto name is built
/// from the session title + a short id.
fn sanitize_log_file_name(requested: &str, title: &str, session_id: SessionId) -> String {
    if !requested.trim().is_empty() {
        let safe = sanitize_component(requested);
        if !safe.is_empty() {
            return safe;
        }
    }
    log_file_name(title, session_id)
}

fn sanitize_component(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    safe.trim_matches('_').to_string()
}

fn log_file_name(title: &str, session_id: SessionId) -> String {
    let safe = sanitize_component(title);
    let stem = if safe.is_empty() { "session" } else { &safe };
    let id = session_id.to_string();
    let short = &id[..id.len().min(8)];
    format!("{stem}_{short}.log")
}

fn normalize_profile_sort_orders(profiles: &mut [ConnectionProfile]) {
    profiles.sort_by(compare_profiles);
    renumber_profile_sort_orders(profiles);
}

fn renumber_profile_sort_orders(profiles: &mut [ConnectionProfile]) {
    let mut current_group = String::new();
    let mut order = 0_i32;

    for profile in profiles {
        if profile.group != current_group {
            current_group = profile.group.clone();
            order = 10;
        } else {
            order += 10;
        }
        profile.sort_order = order;
    }
}

fn next_sort_order_for_group(profiles: &[ConnectionProfile], group: &str) -> i32 {
    profiles
        .iter()
        .filter(|profile| profile.group == group)
        .map(|profile| profile.sort_order)
        .max()
        .unwrap_or(0)
        + 10
}

fn compare_profiles(left: &ConnectionProfile, right: &ConnectionProfile) -> std::cmp::Ordering {
    left.group
        .cmp(&right.group)
        .then_with(|| left.sort_order.cmp(&right.sort_order))
        .then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
        .then_with(|| left.host.cmp(&right.host))
        .then_with(|| left.port.cmp(&right.port))
        .then_with(|| left.username.cmp(&right.username))
}

fn status_after_closed(status: SessionStatus) -> SessionStatus {
    if status == SessionStatus::Error {
        SessionStatus::Error
    } else {
        SessionStatus::Disconnected
    }
}

/// Build a banner terminal for a mock (no-SSH) demo tab.
fn welcome_terminal(profile_name: &str, endpoint: &str) -> VtTerminal {
    let mut terminal = VtTerminal::with_title(TerminalSize::default(), profile_name);
    terminal.feed_str(&format!(
        "\x1b[1;36madit\x1b[0m native rust terminal\r\n\r\n\
         profile  : {profile_name}\r\n\
         endpoint : {endpoint}\r\n\
         status   : mock session, SSH transport not connected yet\r\n\r\n\
         \x1b[90m$\x1b[0m cargo run -p adit-app\r\n\
         opening native iced workspace... \x1b[32mok\x1b[0m\r\n\
         terminal core: full ANSI/VT emulation is now live\r\n"
    ));
    terminal
}

/// Build a banner terminal for a live SSH tab while it connects.
fn live_shell_terminal(profile_name: &str, endpoint: &str) -> VtTerminal {
    let mut terminal = VtTerminal::with_title(TerminalSize::default(), profile_name);
    terminal.feed_str(&format!(
        "\x1b[1;32mssh\x1b[0m live shell starting\r\n\r\n\
         profile  : {profile_name}\r\n\
         endpoint : {endpoint}\r\n\
         status   : creating SSH session actor\r\n\r\n"
    ));
    terminal
}

fn local_shell_terminal(profile_name: &str) -> VtTerminal {
    let mut terminal = VtTerminal::with_title(TerminalSize::default(), profile_name);
    terminal.feed_str(&format!(
        "\x1b[1;36mlocal shell\x1b[0m starting\r\n\r\n\
         profile  : {profile_name}\r\n\
         status   : spawning local pseudo-terminal\r\n\r\n"
    ));
    terminal
}

fn serial_terminal(profile_name: &str, endpoint: &str) -> VtTerminal {
    let mut terminal = VtTerminal::with_title(TerminalSize::default(), profile_name);
    terminal.feed_str(&format!(
        "\x1b[1;36mserial\x1b[0m starting\r\n\r\n\
         profile  : {profile_name}\r\n\
         endpoint : {endpoint}\r\n\
         status   : opening serial port\r\n\r\n"
    ));
    terminal
}

/// Build a terminal that replays a one-shot SSH password probe transcript.
fn probe_terminal(profile_name: &str, endpoint: &str, transcript: &str) -> VtTerminal {
    let mut terminal = VtTerminal::with_title(TerminalSize::default(), profile_name);
    terminal.feed_str(&format!(
        "\x1b[1;36mssh\x1b[0m password probe completed\r\n\r\n\
         profile  : {profile_name}\r\n\
         endpoint : {endpoint}\r\n\
         status   : PTY shell opened, initial output captured, connection closed\r\n\r\n"
    ));

    if transcript.trim().is_empty() {
        terminal.feed_str("\x1b[90m(no shell output captured)\x1b[0m\r\n");
    } else {
        terminal.feed_str(transcript);
        if !transcript.ends_with('\n') {
            terminal.feed_str("\r\n");
        }
    }

    terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_terminal::Color as TermColor;

    #[test]
    fn remote_path_join_and_parent() {
        assert_eq!(join_remote("/home/me", "docs"), "/home/me/docs");
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/home/", "a"), "/home/a");
        assert_eq!(join_remote("/home", "/abs/path"), "/abs/path");

        assert_eq!(parent_remote("/home/me/docs"), "/home/me");
        assert_eq!(parent_remote("/home"), "/");
        assert_eq!(parent_remote("/"), "/");
        assert_eq!(parent_remote("/home/me/"), "/home");
    }

    #[test]
    fn reconnect_backoff_is_exponential_and_capped() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(2), Duration::from_secs(4));
        assert_eq!(reconnect_delay(3), Duration::from_secs(8));
        assert_eq!(reconnect_delay(4), Duration::from_secs(16));
        assert_eq!(reconnect_delay(5), Duration::from_secs(30));
        assert_eq!(reconnect_delay(20), Duration::from_secs(30));
    }

    #[test]
    fn log_file_name_sanitizes_title() {
        let id = SessionId::new();
        let name = log_file_name("prod/web 01", id);
        assert!(name.starts_with("prod_web_01_"), "got {name}");
        assert!(name.ends_with(".log"));

        let blank = log_file_name("///", id);
        assert!(blank.starts_with("session_"), "got {blank}");
    }

    #[test]
    fn session_logging_lifecycle_writes_file() {
        let dir = std::env::temp_dir().join(format!("adit-log-test-{}", SessionId::new()));
        let mut manager = SessionManager::with_demo_profiles();
        let profile_id = manager.profiles()[0].id;
        manager
            .open_mock_session(profile_id)
            .expect("mock session should open");

        assert!(!manager.active_is_logging());
        let path = manager
            .start_active_logging(&dir, "")
            .expect("logging should start");
        assert!(manager.active_is_logging());
        assert_eq!(manager.active_log_path().as_deref(), Some(path.as_path()));
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).expect("log should be readable");
        assert!(content.contains("adit session log"));

        let stopped = manager.stop_active_logging();
        assert_eq!(stopped.as_deref(), Some(path.as_path()));
        assert!(!manager.active_is_logging());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn logging_uses_and_sanitizes_requested_filename() {
        let dir = std::env::temp_dir().join(format!("adit-logname-test-{}", SessionId::new()));
        let mut manager = SessionManager::with_demo_profiles();
        let profile_id = manager.profiles()[0].id;
        let session_id = manager
            .open_mock_session(profile_id)
            .expect("mock session should open");

        // A rendered pattern with an unsafe char is sanitized but otherwise used.
        let path = manager
            .start_active_logging(&dir, "web01@2026-07-08.log")
            .expect("logging should start");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("web01_2026-07-08.log")
        );
        assert!(path.exists());
        assert!(manager.session_is_logging(session_id));

        manager.stop_active_logging();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn mock_session_renders_colored_banner() {
        let mut manager = SessionManager::with_demo_profiles();
        let profile_id = manager.profiles()[0].id;
        manager
            .open_mock_session(profile_id)
            .expect("mock session should open");

        let snapshot = manager.active_snapshot(Viewport::tail(28));
        let first = &snapshot.lines[0].cells[0];

        // The banner opens with SGR 1;36 ("\x1b[1;36madit"): bold cyan. Bold
        // brightens indexed color 6 to 14 at snapshot time.
        assert_eq!(first.text, "adit");
        assert!(first.bold);
        assert_eq!(first.fg, TermColor::Indexed(14));
    }

    #[test]
    fn live_input_without_channel_echoes_locally() {
        let mut manager = SessionManager::with_demo_profiles();
        let profile_id = manager.profiles()[0].id;
        manager
            .open_mock_session(profile_id)
            .expect("mock session should open");

        manager
            .send_input_to_active("whoami")
            .expect("local echo should succeed");

        let snapshot = manager.active_snapshot(Viewport::tail(28));
        let rendered: String = snapshot
            .lines
            .iter()
            .flat_map(|line| line.cells.iter().map(|cell| cell.text.as_str()))
            .collect();
        assert!(rendered.contains("whoami"));
    }

    #[test]
    fn active_session_summary_tracks_active_tab() {
        let mut manager = SessionManager::with_demo_profiles();
        let first_profile = manager.profiles()[0].id;
        let second_profile = manager.profiles()[1].id;
        let first_session = manager
            .open_mock_session(first_profile)
            .expect("first session should open");
        let second_session = manager
            .open_mock_session(second_profile)
            .expect("second session should open");

        assert_eq!(
            manager
                .active_session_summary()
                .expect("active summary should exist")
                .id,
            second_session
        );

        manager
            .activate(first_session)
            .expect("first session should activate");
        let summary = manager
            .active_session_summary()
            .expect("active summary should exist");

        assert_eq!(summary.id, first_session);
        assert_eq!(summary.profile_id, first_profile);
    }

    #[test]
    fn closed_event_does_not_hide_error_status() {
        assert_eq!(
            status_after_closed(SessionStatus::Error),
            SessionStatus::Error
        );
        assert_eq!(
            status_after_closed(SessionStatus::Connecting),
            SessionStatus::Disconnected
        );
        assert_eq!(
            status_after_closed(SessionStatus::Connected),
            SessionStatus::Disconnected
        );
    }

    #[test]
    fn move_profile_reorders_within_group() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "bravo", "10.0.0.2", 22, "root"),
            ConnectionProfile::with_group("Lab", "charlie", "10.0.0.3", 22, "root"),
        ]);
        let bravo = manager.profiles()[1].id;

        manager
            .move_profile(bravo, ProfileMove::Up)
            .expect("profile should move up");

        let mut profiles = manager.profiles().to_vec();
        profiles.sort_by(super::compare_profiles);
        assert_eq!(profiles[0].name, "bravo");
        assert_eq!(profiles[1].name, "alpha");
        assert_eq!(profiles[2].name, "charlie");
    }

    #[test]
    fn sort_profiles_by_host_persists_order_values() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Lab", "b", "10.0.0.20", 22, "root"),
            ConnectionProfile::with_group("Lab", "a", "10.0.0.10", 22, "root"),
        ]);

        manager.sort_profiles(ProfileSortKey::Host);

        let mut profiles = manager.profiles().to_vec();
        profiles.sort_by(super::compare_profiles);
        assert_eq!(profiles[0].host, "10.0.0.10");
        assert_eq!(profiles[0].sort_order, 10);
        assert_eq!(profiles[1].host, "10.0.0.20");
        assert_eq!(profiles[1].sort_order, 20);
    }

    #[test]
    fn reorder_profile_drops_source_after_target() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "bravo", "10.0.0.2", 22, "root"),
            ConnectionProfile::with_group("Lab", "charlie", "10.0.0.3", 22, "root"),
        ]);
        let alpha = manager.profiles()[0].id;
        let charlie = manager.profiles()[2].id;

        manager
            .reorder_profile(alpha, charlie, ProfileDropPosition::After)
            .expect("profile should reorder");

        let mut profiles = manager.profiles().to_vec();
        profiles.sort_by(super::compare_profiles);
        assert_eq!(profiles[0].name, "bravo");
        assert_eq!(profiles[1].name, "charlie");
        assert_eq!(profiles[2].name, "alpha");
        assert_eq!(profiles[2].sort_order, 30);
    }

    #[test]
    fn reorder_profile_across_groups_moves_into_target_group() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Build", "builder", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.2", 22, "root"),
            ConnectionProfile::with_group("Lab", "bravo", "10.0.0.3", 22, "root"),
        ]);
        let builder = manager.profiles()[0].id;
        let bravo = manager.profiles()[2].id;

        manager
            .reorder_profile(builder, bravo, ProfileDropPosition::Before)
            .expect("profile should move into target group");

        let mut profiles = manager.profiles().to_vec();
        profiles.sort_by(super::compare_profiles);
        assert_eq!(
            profiles
                .iter()
                .map(|profile| (profile.group.as_str(), profile.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("Lab", "alpha"), ("Lab", "builder"), ("Lab", "bravo")]
        );
    }

    #[test]
    fn move_profile_to_group_allows_empty_target_group() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Build", "builder", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.2", 22, "root"),
        ]);
        let alpha = manager.profiles()[1].id;

        manager
            .move_profile_to_group(alpha, "Empty")
            .expect("profile should move to empty group");

        let profile = manager.profile(alpha).expect("profile should exist");
        assert_eq!(profile.group, "Empty");
        assert_eq!(profile.sort_order, 10);
    }

    #[test]
    fn rename_group_updates_profiles() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "bravo", "10.0.0.2", 22, "root"),
            ConnectionProfile::with_group("Prod", "web", "10.0.0.3", 22, "root"),
        ]);

        manager
            .rename_group("Lab", "Workspace")
            .expect("group should rename");

        assert_eq!(
            manager
                .profiles()
                .iter()
                .filter(|profile| profile.group == "Workspace")
                .count(),
            2
        );
        assert_eq!(
            manager
                .profiles()
                .iter()
                .filter(|profile| profile.group == "Prod")
                .count(),
            1
        );
    }

    #[test]
    fn duplicate_profile_creates_a_copy_after_the_original() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "bravo", "10.0.0.2", 22, "root"),
        ]);
        let original_id = manager.profiles()[0].id;

        let new_id = manager.duplicate_profile(original_id).expect("clone");
        assert_ne!(new_id, original_id);
        assert_eq!(manager.profiles().len(), 3);

        // The copy sits immediately after the original, a full field copy but
        // with a fresh id and a suffixed name.
        let clone = &manager.profiles()[1];
        assert_eq!(clone.id, new_id);
        assert_eq!(clone.name, "alpha 副本");
        assert_eq!(clone.host, "10.0.0.1");
        assert_eq!(clone.group, "Lab");
    }

    #[test]
    fn editing_profile_keeps_sort_order() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_group("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Lab", "bravo", "10.0.0.2", 22, "root"),
        ]);
        let profile_id = manager.profiles()[1].id;
        let sort_order = manager.profiles()[1].sort_order;

        manager
            .update_profile(
                profile_id,
                "Lab",
                "bravo-renamed",
                "10.0.0.22",
                22,
                "admin",
                AuthMethod::Auto,
                "",
            )
            .expect("profile should update");

        let profile = manager.profile(profile_id).expect("profile should exist");
        assert_eq!(profile.name, "bravo-renamed");
        assert_eq!(profile.sort_order, sort_order);
    }
}
