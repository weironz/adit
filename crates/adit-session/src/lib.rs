use adit_domain::{AuthMethod, ConnectionProfile, ProfileId, SessionId, SessionStatus};
use adit_ssh::{
    AuthOptions, LiveShellCommand, LiveShellEvent, LiveShellHandle, LiveShellRequest,
    PasswordShellProbe, SshError,
};
use adit_terminal::{TerminalCore, TerminalSize, TerminalSnapshot, Viewport, VtTerminal};
use std::collections::HashMap;
use thiserror::Error;

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
}

pub struct SessionManager {
    profiles: Vec<ConnectionProfile>,
    sessions: HashMap<SessionId, SessionRecord>,
    active_session: Option<SessionId>,
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

impl SessionManager {
    #[must_use]
    pub fn with_demo_profiles() -> Self {
        Self::with_profiles(vec![
            ConnectionProfile::with_folder("Local", "local-lab", "127.0.0.1", 22, "root"),
            ConnectionProfile::with_folder("Production", "prod-web-01", "10.0.0.12", 22, "deploy"),
            ConnectionProfile::with_folder("Build", "mac-build", "build-mac.local", 22, "builder"),
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
        }
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

    pub fn create_profile(
        &mut self,
        folder: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth_method: AuthMethod,
        identity_file: impl Into<String>,
    ) -> Result<ProfileId, SessionError> {
        let mut profile = build_profile(
            folder,
            name,
            host,
            port,
            username,
            auth_method,
            identity_file,
        )?;
        profile.sort_order = next_sort_order_for_folder(&self.profiles, &profile.folder);
        let profile_id = profile.id;
        self.profiles.push(profile);
        Ok(profile_id)
    }

    pub fn update_profile(
        &mut self,
        profile_id: ProfileId,
        folder: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth_method: AuthMethod,
        identity_file: impl Into<String>,
    ) -> Result<(), SessionError> {
        let updated = build_profile(
            folder,
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

        let sort_order = if self.profiles[index].folder == updated.folder {
            self.profiles[index].sort_order
        } else {
            next_sort_order_for_folder(&self.profiles, &updated.folder)
        };

        let profile = &mut self.profiles[index];

        profile.folder = updated.folder;
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
        let folder = self.profiles[index].folder.clone();

        let mut ordered = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| profile.folder == folder)
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
            left.folder.cmp(&right.folder).then_with(|| match key {
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
            .ok_or(SessionError::ProfileNotFound)?;

        let mut request = LiveShellRequest::new(
            profile.host.clone(),
            profile.port,
            profile.username.clone(),
            password,
        );
        request.auth = auth_options_for_profile(profile, &request.password);
        request.cols = 96;
        request.rows = 28;

        let live = adit_ssh::spawn_password_shell(request)?;
        let session_id = SessionId::new();
        let endpoint = profile.endpoint();
        let mut terminal = live_shell_terminal(&profile.name, &endpoint);
        terminal.append_status(format!("connecting to {endpoint}"));
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
            },
        );
        self.active_session = Some(session_id);

        Ok(session_id)
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

        if let Some(live) = &record.live {
            live.send(LiveShellCommand::Disconnect)?;
            record.summary.status = SessionStatus::Disconnected;
            record.terminal.append_status("disconnect requested");
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

    pub fn poll_events(&mut self) -> usize {
        let mut handled = 0;

        for record in self.sessions.values_mut() {
            let mut closed = false;

            while let Some(event) = record.live.as_ref().and_then(LiveShellHandle::try_recv) {
                handled += 1;

                match event {
                    LiveShellEvent::Status(status) => {
                        record.terminal.append_status(&status);
                        record.summary.status = match status.as_str() {
                            "connected" => SessionStatus::Connected,
                            "exit status 0" => SessionStatus::Disconnected,
                            _ => SessionStatus::Connecting,
                        };
                    }
                    LiveShellEvent::Output(bytes) => {
                        record.terminal.feed(&bytes);
                        record.summary.status = SessionStatus::Connected;
                    }
                    LiveShellEvent::Error(error) => {
                        record.terminal.append_status(format!(
                            "error while connecting to {}: {error}",
                            record.summary.endpoint
                        ));
                        record.summary.status = SessionStatus::Error;
                    }
                    LiveShellEvent::Closed => {
                        record.terminal.append_status("disconnected");
                        record.summary.status = status_after_closed(record.summary.status);
                        closed = true;
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

            if closed {
                record.live = None;
            }
        }

        handled
    }

    #[must_use]
    pub fn active_snapshot(&self, viewport: Viewport) -> TerminalSnapshot {
        self.active_session
            .and_then(|session_id| self.sessions.get(&session_id))
            .map(|record| record.terminal.snapshot(viewport))
            .unwrap_or_else(|| TerminalSnapshot::empty(Default::default()))
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
    folder: impl Into<String>,
    name: impl Into<String>,
    host: impl Into<String>,
    port: u16,
    username: impl Into<String>,
    auth_method: AuthMethod,
    identity_file: impl Into<String>,
) -> Result<ConnectionProfile, SessionError> {
    let folder = normalize_folder(folder);
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

    let mut profile = ConnectionProfile::with_folder(folder, name, host, port, username);
    profile.auth_method = auth_method;
    profile.identity_file = identity_file;

    Ok(profile)
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

fn normalize_folder(folder: impl Into<String>) -> String {
    let folder = folder.into().trim().to_string();

    if folder.is_empty() {
        String::from("Default")
    } else {
        folder
    }
}

fn normalize_profile_sort_orders(profiles: &mut Vec<ConnectionProfile>) {
    profiles.sort_by(compare_profiles);
    renumber_profile_sort_orders(profiles);
}

fn renumber_profile_sort_orders(profiles: &mut [ConnectionProfile]) {
    let mut current_folder = String::new();
    let mut order = 0_i32;

    for profile in profiles {
        if profile.folder != current_folder {
            current_folder = profile.folder.clone();
            order = 10;
        } else {
            order += 10;
        }
        profile.sort_order = order;
    }
}

fn next_sort_order_for_folder(profiles: &[ConnectionProfile], folder: &str) -> i32 {
    profiles
        .iter()
        .filter(|profile| profile.folder == folder)
        .map(|profile| profile.sort_order)
        .max()
        .unwrap_or(0)
        + 10
}

fn compare_profiles(left: &ConnectionProfile, right: &ConnectionProfile) -> std::cmp::Ordering {
    left.folder
        .cmp(&right.folder)
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
    fn move_profile_reorders_within_folder() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_folder("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_folder("Lab", "bravo", "10.0.0.2", 22, "root"),
            ConnectionProfile::with_folder("Lab", "charlie", "10.0.0.3", 22, "root"),
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
            ConnectionProfile::with_folder("Lab", "b", "10.0.0.20", 22, "root"),
            ConnectionProfile::with_folder("Lab", "a", "10.0.0.10", 22, "root"),
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
    fn editing_profile_keeps_sort_order() {
        let mut manager = SessionManager::with_profiles(vec![
            ConnectionProfile::with_folder("Lab", "alpha", "10.0.0.1", 22, "root"),
            ConnectionProfile::with_folder("Lab", "bravo", "10.0.0.2", 22, "root"),
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
