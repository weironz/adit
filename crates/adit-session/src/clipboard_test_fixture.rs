//! In-memory RDP peer for cross-crate clipboard regressions.
//!
//! This module is enabled only by the `test-fixtures` feature. It attaches to
//! an existing mock session and sends events through the normal session pump;
//! it never starts a helper, connects to a host, or opens a clipboard.

use super::*;
use adit_rdp_proto::ClientMsg;
use std::sync::mpsc::{Receiver, RecvTimeoutError};

pub struct SyntheticRdp {
    session: SessionId,
    commands: Receiver<ClientMsg>,
    consume_raw_offer: Box<dyn Fn(&ClientMsg) + Send + Sync>,
}

impl SyntheticRdp {
    pub fn attach(manager: &mut SessionManager, session: SessionId) -> Self {
        let (handle, commands) = RdpClientHandle::test_fixture_with_commands();
        let consume_raw_offer = Box::new(handle.test_raw_command_observer());
        let record = manager.sessions.get_mut(&session).expect("mock session");
        assert!(record.rdp.is_none());
        record.rdp = Some(handle);
        Self {
            session,
            commands,
            consume_raw_offer,
        }
    }

    fn handle<'a>(&self, manager: &'a SessionManager) -> &'a RdpClientHandle {
        manager.sessions[&self.session]
            .rdp
            .as_ref()
            .expect("fixture attached")
    }

    pub fn remote_offer(&self, manager: &SessionManager, generation: u64) -> RdpFileRequester {
        self.handle(manager).test_remote_file_offer(generation)
    }

    pub fn send_event(&self, manager: &SessionManager, event: RdpClientEvent) {
        self.handle(manager).test_send_event(event);
    }

    pub fn send_local_offer(&self, manager: &SessionManager, files: Vec<ClipFile>) {
        self.handle(manager)
            .send_clipboard_files(files, Some(RdpClipboardSequence::new(1, || 1)));
    }

    /// Install a synthetic shared-policy boundary and return its current epoch.
    pub fn prepare_policy(&self, manager: &SessionManager) -> std::io::Result<u64> {
        self.handle(manager).test_prepare_policy()
    }

    pub fn acknowledge_policy(&self, manager: &SessionManager, epoch: u64, enabled: bool) {
        self.handle(manager).test_policy_ack(epoch, enabled);
    }

    pub fn policy_channel_ready(&self, manager: &SessionManager, channel_epoch: u64) {
        self.handle(manager).test_policy_ready(channel_epoch);
    }

    pub fn policy_channel_changed(&self, manager: &SessionManager, channel_epoch: u64) {
        self.handle(manager)
            .test_policy_channel_changed(channel_epoch);
    }

    pub fn policy_channel_unavailable(&self, manager: &SessionManager) {
        self.handle(manager).test_policy_channel_unavailable();
    }

    pub fn reject_policy(
        &self,
        manager: &SessionManager,
        epoch: u64,
        reason: RdpClipboardPolicyFailure,
    ) {
        self.handle(manager).test_policy_failed(epoch, reason);
    }

    pub fn offer_remote_files(
        &self,
        manager: &SessionManager,
        epoch: u64,
        generation: u64,
        files: Vec<ClipFile>,
    ) {
        self.handle(manager)
            .test_policy_offer(epoch, generation, files);
    }

    pub fn disconnect_policy(&self, manager: &SessionManager) {
        self.handle(manager).test_policy_disconnected();
    }

    pub fn timeout_policy(&self, manager: &SessionManager) {
        self.handle(manager).test_policy_timeout();
    }

    pub fn next_command(&self, timeout: Duration) -> Result<ClientMsg, RecvTimeoutError> {
        let command = self.commands.recv_timeout(timeout)?;
        (self.consume_raw_offer)(&command);
        Ok(command)
    }

    pub fn try_command(&self) -> Option<ClientMsg> {
        let command = self.commands.try_recv().ok()?;
        (self.consume_raw_offer)(&command);
        Some(command)
    }

    /// Take one queued command through the production first-write guard.
    /// `Ok(None)` means it was revoked before any synthetic IPC bytes were sent.
    pub fn write_next_command(
        &self,
        manager: &SessionManager,
        timeout: Duration,
    ) -> Result<Option<ClientMsg>, RecvTimeoutError> {
        let command = self.commands.recv_timeout(timeout)?;
        Ok(self.handle(manager).test_write_optional_command(command))
    }
}
