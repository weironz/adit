//! Cold-start protected-vault regression against disposable loopback fixtures.
//! See tests/vnc/README.md. The fixture passwords below are public, synthetic
//! test inputs; this test never reads the user's Adit directory or credentials.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use adit_domain::{AuthMethod, ConnectionProfile, ProfileId, Protocol, SessionStatus};
use adit_session::{HostKeyDecision, SessionManager};
use adit_storage::{CredentialError, CredentialProtection, CredentialStore, ProfileStore};
use adit_terminal::Viewport;

const MASTER: &str = "isolated vault regression master";
const SSH_PASSWORD: &str = "adit-test-isolated";
const VNC_PASSWORD: &str = "adit-ci";
const RDP_SYNTHETIC_PASSWORD: &str = "not-a-real-rdp-password";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "adit-protected-vault-live-{}-{}",
            std::process::id(),
            ProfileId::new()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn credentials(&self) -> CredentialStore {
        CredentialStore::with_path("adit-vault-live-isolated", self.0.join("credentials.json"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The absolute unique scratch root is constructed by this test. It can
        // never resolve to the user's selected configuration directory.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait(manager: &mut SessionManager, ready: impl Fn(&SessionManager) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        manager.poll_events();
        if let Some((id, _)) = manager.pending_host_key() {
            manager
                .respond_host_key(id, HostKeyDecision::TrustOnce)
                .unwrap();
        }
        if ready(manager) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "isolated connection did not reach its expected state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn terminal_text(manager: &SessionManager) -> String {
    manager
        .active_snapshot(Viewport::tail(40))
        .lines
        .iter()
        .map(|line| {
            line.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore = "needs isolated tests/vnc fixture SSH 12222 and TigerVNC 15903"]
fn protected_cold_start_credentials_authenticate_real_ssh_and_vnc() {
    let scratch = Scratch::new();
    let profiles = ProfileStore::new(scratch.0.join("profiles.json"));
    let mut ssh = ConnectionProfile::new("vault-SSH", "127.0.0.1", 12222, "adit-test");
    ssh.auth_method = AuthMethod::Password;
    let mut vnc = ConnectionProfile::new("vault-VNC", "127.0.0.1", 15903, "");
    vnc.protocol = Protocol::Vnc;
    vnc.vnc.read_only = true;
    vnc.vnc.auto_reconnect = false;
    let mut rdp = ConnectionProfile::new("vault-RDP-storage-only", "127.0.0.1", 13389, "synthetic");
    rdp.protocol = Protocol::Rdp;
    let ids = [ssh.id, vnc.id, rdp.id];
    profiles.save_profiles(&[ssh, vnc, rdp]).unwrap();

    {
        let vault = scratch.credentials();
        for (id, password) in
            ids.into_iter()
                .zip([SSH_PASSWORD, VNC_PASSWORD, RDP_SYNTHETIC_PASSWORD])
        {
            vault.save_profile_password(id, password).unwrap();
        }
        vault
            .save_profile_passphrase(ids[0], "synthetic unused private-key passphrase")
            .unwrap();
        let legacy = std::fs::read(vault.path()).unwrap();
        let backup = vault.enable_master_password(MASTER).unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), legacy);
    }

    // Reconstruct stores, profiles and connection manager from disk. No cached
    // key or decrypted connection password survives this boundary.
    let vault = scratch.credentials();
    assert_eq!(
        vault.protection_status().unwrap(),
        CredentialProtection::Locked
    );
    for id in ids {
        assert!(matches!(
            vault.load_profile_password(id),
            Err(CredentialError::Locked)
        ));
    }
    let original = std::fs::read(vault.path()).unwrap();
    assert!(vault.unlock("incorrect synthetic master").is_err());
    assert_eq!(std::fs::read(vault.path()).unwrap(), original);
    vault.unlock(MASTER).unwrap();
    assert_eq!(
        vault.load_profile_password(ids[2]).unwrap().as_deref(),
        Some(RDP_SYNTHETIC_PASSWORD)
    );

    let mut manager = SessionManager::with_profiles(profiles.load_profiles().unwrap());
    manager.set_auto_accept_host_keys(false);
    manager.set_auto_reconnect(false);
    manager.set_connect_timeout(10);
    let ssh_session = manager
        .open_live_ssh_session(
            ids[0],
            vault.load_profile_password(ids[0]).unwrap().unwrap(),
            vault.load_profile_passphrase(ids[0]).unwrap().unwrap(),
        )
        .unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary()
            .is_some_and(|s| s.status == SessionStatus::Connected)
    });
    // Splitting the marker in printf prevents terminal local echo of the input
    // from satisfying the expected remotely executed output assertion.
    manager
        .send_input_to_active("printf '__ADIT_VAULT_%s__\\n' 'SSH_OK'\r")
        .unwrap();
    wait(&mut manager, |m| {
        terminal_text(m).contains("__ADIT_VAULT_SSH_OK__")
    });
    manager.close(ssh_session);

    let vnc_session = manager
        .open_live_vnc_session(
            ids[1],
            vault.load_profile_password(ids[1]).unwrap().unwrap(),
        )
        .unwrap();
    wait(&mut manager, |m| {
        m.active_session_summary()
            .is_some_and(|s| s.status == SessionStatus::Connected)
            && m.active_vnc_diagnostics().is_some_and(|d| {
                d.framebuffer_pixels > 0 && d.covered_pixels == d.framebuffer_pixels
            })
    });
    let frame = manager.active_rdp_frame_if_newer(0).unwrap();
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!(
        frame.rgba.len(),
        usize::from(frame.width) * usize::from(frame.height) * 4
    );
    assert!(frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[3] == 255));

    // Locking disables subsequent retrieval; it does not revoke or disconnect
    // an already authenticated session, as documented in the settings UI.
    vault.lock_vault().unwrap();
    assert!(matches!(
        vault.load_profile_password(ids[1]),
        Err(CredentialError::Locked)
    ));
    manager.poll_events();
    assert_eq!(
        manager.active_session_summary().unwrap().status,
        SessionStatus::Connected
    );
    manager.close(vnc_session);
    assert!(manager.sessions().is_empty());
    println!("protected vault cold-load: SSH command round-trip PASS; VNC authenticated complete framebuffer PASS; RDP encrypted storage round-trip only");
}
