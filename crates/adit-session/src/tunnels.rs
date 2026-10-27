//! Tunnel presentation and policy adapter; no SFTP lifecycle changes.
use super::*;

/// A live port-forwarding tunnel and its observable state.
pub struct TunnelState {
    handle: adit_ssh::TunnelHandle,
    pub(super) owner: SessionId,
    pub(super) pending_auth_prompt: Option<AuthPromptRequest>,
    definition: TunnelDef,
    pub id: u64,
    pub kind: adit_ssh::TunnelKind,
    pub bind: String,
    pub target: String,
    pub status: String,
    pub listening: bool,
    pub active: usize,
    pub total: usize,
    pub error: Option<String>,
    pub retry: Option<TunnelRetry>,
    pub stopped: bool,
    pub retries_exhausted: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelRetry {
    pub attempt: u32,
    pub max_attempts: u32,
    /// None after the wait expires and the fresh handshake has begun.
    pub delay_secs: Option<u64>,
}

impl TunnelState {
    fn apply(&mut self, event: TunnelEvent) {
        match event {
            TunnelEvent::AuthPrompt(prompt) => self.pending_auth_prompt = Some(prompt),
            TunnelEvent::Status(status) => {
                if status == "connecting" {
                    if let Some(retry) = &mut self.retry {
                        retry.delay_secs = None;
                    }
                }
                self.status = status;
            }
            TunnelEvent::Listening { bind } => {
                self.listening = true;
                self.stopped = false;
                self.error = None;
                self.retry = None;
                self.pending_auth_prompt = None;
                self.retries_exhausted = None;
                self.status = format!("监听 {bind}");
            }
            TunnelEvent::Opened { .. } => {
                self.active += 1;
                self.total += 1;
            }
            TunnelEvent::Closed { .. } => self.active = self.active.saturating_sub(1),
            TunnelEvent::Error(error) => {
                self.status = format!("error: {error}");
                self.error = Some(error);
            }
            TunnelEvent::Reconnecting {
                attempt,
                max_attempts,
                delay_secs,
            } => {
                self.listening = false;
                self.active = 0;
                self.pending_auth_prompt = None;
                self.retry = Some(TunnelRetry {
                    attempt,
                    max_attempts,
                    delay_secs: Some(delay_secs),
                });
            }
            TunnelEvent::RetryExhausted { attempts } => self.retries_exhausted = Some(attempts),
            TunnelEvent::Stopped => {
                self.listening = false;
                self.active = 0;
                self.pending_auth_prompt = None;
                self.retry = None;
                self.stopped = true;
            }
        }
    }
}

impl SessionManager {
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
        let password = reconnect.password.clone();
        let passphrase = reconnect.passphrase.clone();
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
        let definition = TunnelDef {
            kind,
            bind_address: bind_address.clone(),
            bind_port,
            target_host: target_host.clone(),
            target_port,
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
        request.auth = auth_options_for_profile(&profile, &password, &passphrase);
        request.jumps = profile.jumps.clone();
        request.proxy_command = profile.proxy_command.clone();
        request.jump_credentials = self.resolve_jump_credentials(&profile)?;
        request.connect_timeout_secs = self.connect_timeout_secs;
        let (delays, enabled) = self.tunnel_reconnect_policy(record);
        request.reconnect_delays = delays;
        let handle = open_tunnel_for(record, request)?;
        handle.set_reconnect_enabled(enabled);
        // Re-adding a forward is an explicit retry; automatic restoration below
        // cannot clear this user's suppression.
        self.suppressed_tunnels
            .retain(|(owner, def)| *owner != session_id || *def != definition);

        let id = self.next_tunnel_id;
        self.next_tunnel_id += 1;
        self.tunnels.push(TunnelState {
            handle,
            owner: session_id,
            pending_auth_prompt: None,
            definition,
            id,
            kind,
            bind,
            target,
            status: String::from("connecting"),
            listening: false,
            active: 0,
            total: 0,
            error: None,
            retry: None,
            stopped: false,
            retries_exhausted: None,
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
        for mut def in defs {
            if def.bind_address.trim().is_empty() {
                def.bind_address = "127.0.0.1".into();
            }
            // Shell and tunnel recovery run independently. A shell becoming
            // ready must not create a duplicate listener or revive a tunnel
            // stopped on authentication/security failure.
            if self
                .suppressed_tunnels
                .iter()
                .any(|(owner, closed)| Some(*owner) == self.active_session && *closed == def)
                || self
                    .tunnels
                    .iter()
                    .any(|t| Some(t.owner) == self.active_session && t.definition == def)
            {
                continue;
            }
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
            if !self
                .suppressed_tunnels
                .contains(&(tunnel.owner, tunnel.definition.clone()))
            {
                self.suppressed_tunnels
                    .push((tunnel.owner, tunnel.definition));
            }
        }
    }

    /// The transport receives a plan, not separate retry constants. Keep this
    /// adapter next to the caller so SessionOptions can supply its per-session
    /// override when integrated; this SSH baseline uses the shell's policy.
    fn tunnel_reconnect_policy(&self, record: &SessionRecord) -> (Vec<Duration>, bool) {
        let policy = record.options.reconnect.clone().unwrap_or(ReconnectPolicy {
            enabled: self.auto_reconnect,
            ..Default::default()
        });
        let delays = (0..policy.max_attempts.min(100))
            .map(|attempt| Duration::from_secs(policy.delay_secs(attempt)))
            .collect();
        (delays, policy.enabled)
    }

    pub(super) fn cancel_tunnels_for_owner(&mut self, owner: SessionId) {
        self.suppressed_tunnels
            .retain(|(session, _)| *session != owner);
        self.tunnels.retain(|tunnel| {
            if tunnel.owner == owner {
                let _ = tunnel.handle.send(TunnelCommand::Disconnect);
                false
            } else {
                true
            }
        });
    }

    pub(super) fn update_tunnel_reconnect_enabled(&self) {
        for tunnel in &self.tunnels {
            if let Some(record) = self.sessions.get(&tunnel.owner) {
                let (_, enabled) = self.tunnel_reconnect_policy(record);
                tunnel.handle.set_reconnect_enabled(enabled);
            }
        }
    }

    /// Drain tunnel events into observable state; retain terminal errors.
    pub fn poll_tunnel_events(&mut self) {
        let mut stopped = Vec::new();
        for tunnel in &mut self.tunnels {
            while let Some(event) = tunnel.handle.try_recv() {
                tunnel.apply(event);
                if tunnel.stopped && tunnel.error.is_none() {
                    stopped.push(tunnel.id);
                }
            }
        }
        if !stopped.is_empty() {
            self.tunnels.retain(|t| !stopped.contains(&t.id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_policy_follows_its_owner_snapshot_and_inherits_only_when_unset() {
        let mut profile = ConnectionProfile::new("fixture", "fixture.invalid", 22, "fixture");
        profile.session_options.reconnect = Some(ReconnectPolicy {
            enabled: true,
            max_attempts: 3,
            initial_delay_secs: 2,
            max_delay_secs: 7,
        });
        let profile_id = profile.id;
        let mut manager = SessionManager::with_profiles(vec![profile]);
        let owner = manager.open_mock_session(profile_id).unwrap();
        manager.set_auto_reconnect(false);
        let (delays, enabled) = manager.tunnel_reconnect_policy(&manager.sessions[&owner]);
        assert!(enabled, "explicit profile policy overrides the global switch");
        assert_eq!(delays, [2, 4, 7].map(Duration::from_secs));
        manager.set_profile_session_options(profile_id, SessionOptions::default()).unwrap();
        let inherited = manager.open_mock_session(profile_id).unwrap();
        assert!(!manager.tunnel_reconnect_policy(&manager.sessions[&inherited]).1);
        manager.set_auto_reconnect(true);
        assert!(manager.tunnel_reconnect_policy(&manager.sessions[&inherited]).1);
        assert_eq!(manager.tunnel_reconnect_policy(&manager.sessions[&owner]).0, delays);
    }

    fn manager() -> (SessionManager, SessionId) {
        let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let profile = ConnectionProfile::with_group(
            "",
            "fixture",
            "127.0.0.1",
            socket.local_addr().unwrap().port(),
            "fixture",
        );
        let id = profile.id;
        drop(socket);
        let mut manager = SessionManager::with_profiles(vec![profile]);
        let owner = manager.open_mock_session(id).unwrap();
        manager.sessions.get_mut(&owner).unwrap().reconnect =
            Some(ReconnectState::new(String::new(), String::new()));
        manager.set_auto_reconnect(true);
        manager
            .open_tunnel(
                TunnelKind::Local,
                "127.0.0.1".into(),
                20000,
                "127.0.0.1".into(),
                9,
            )
            .unwrap();
        (manager, owner)
    }

    #[test]
    fn tunnel_state_clears_old_clients_and_errors_on_recovery() {
        let (mut manager, _) = manager();
        let tunnel = manager.tunnels.last_mut().unwrap();
        tunnel.apply(TunnelEvent::Opened {
            peer: "old-peer".into(),
        });
        tunnel.apply(TunnelEvent::Error("lost".into()));
        tunnel.apply(TunnelEvent::Reconnecting {
            attempt: 1,
            max_attempts: 3,
            delay_secs: 2,
        });
        assert_eq!(tunnel.active, 0);
        assert!(!tunnel.listening);
        assert_eq!(tunnel.retry.unwrap().delay_secs, Some(2));
        tunnel.apply(TunnelEvent::Status("connecting".into()));
        assert_eq!(tunnel.retry.unwrap().delay_secs, None);
        tunnel.apply(TunnelEvent::Listening {
            bind: "127.0.0.1:20000".into(),
        });
        assert!(tunnel.listening);
        assert!(tunnel.retry.is_none());
        assert!(tunnel.error.is_none());
        tunnel.apply(TunnelEvent::Error("transport lost".into()));
        tunnel.apply(TunnelEvent::RetryExhausted { attempts: 3 });
        tunnel.apply(TunnelEvent::Stopped);
        assert!(tunnel.stopped);
        assert!(!tunnel.listening);
        assert_eq!(tunnel.error.as_deref(), Some("transport lost"));
        assert_eq!(tunnel.retries_exhausted, Some(3));
    }

    #[test]
    fn tunnel_owner_close_and_disconnect_cancel_its_workers() {
        for close in [false, true] {
            let (mut manager, owner) = manager();
            if close {
                manager.close(owner);
            } else {
                manager.disconnect(owner).unwrap();
            }
            assert!(manager.tunnels().is_empty());
        }
    }

    #[test]
    fn shell_recovery_does_not_duplicate_or_revive_saved_tunnels() {
        let (mut manager, owner) = manager();
        let profile = manager.sessions[&owner].summary.profile_id;
        manager.add_profile_tunnel(
            profile,
            TunnelDef {
                kind: TunnelKind::Local,
                bind_address: "127.0.0.1".into(),
                bind_port: 20000,
                target_host: "127.0.0.1".into(),
                target_port: 9,
            },
        );
        manager.start_profile_tunnels(profile);
        assert_eq!(manager.tunnels().len(), 1);
        manager.tunnels[0].apply(TunnelEvent::Error("security refusal".into()));
        manager.tunnels[0].apply(TunnelEvent::Stopped);
        manager.start_profile_tunnels(profile);
        assert_eq!(manager.tunnels().len(), 1);
        assert!(manager.tunnels[0].stopped);
        let id = manager.tunnels[0].id;
        manager.close_tunnel(id);
        manager.start_profile_tunnels(profile);
        assert!(
            manager.tunnels.is_empty(),
            "explicitly closed forwards must stay closed"
        );
        manager
            .open_tunnel(
                TunnelKind::Local,
                "127.0.0.1".into(),
                20000,
                "127.0.0.1".into(),
                9,
            )
            .unwrap();
        assert_eq!(
            manager.tunnels.len(),
            1,
            "an explicit Add may retry the same definition"
        );
    }
}
