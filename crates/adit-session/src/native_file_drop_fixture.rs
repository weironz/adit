//! Synthetic Ready events for native queue unit tests. The feature is enabled
//! by adit-ui's dev-dependency and is absent from a normal application build.
use super::*;

impl SessionManager {
    #[doc(hidden)]
    pub fn native_file_drop_fixture_ready(&mut self) {
        for record in self.sessions.values_mut() {
            if let Some(shell) = &mut record.sftp_shell {
                shell.on_event(
                    &mut record.terminal,
                    SftpEvent::Ready {
                        home: "/fixture".into(),
                    },
                );
                shell.on_event(
                    &mut record.terminal,
                    SftpEvent::Listing {
                        path: "/fixture".into(),
                        entries: Vec::new(),
                    },
                );
                record.summary.status = SessionStatus::Connected;
            }
        }
        if let Some(browser) = &mut self.sftp {
            browser.drop_revision.revoke();
            browser.cwd = "/fixture".into();
            browser.pending_listings.clear();
            browser.connected = true;
        }
    }
}
