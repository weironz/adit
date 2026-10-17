//! Manual protected-vault acceptance on an explicitly selected private RDP fixture.
//! argv: private-ip username [port]. Password on stdin; helper via ADIT_RDP_HOST.
//! Uses an isolated scratch vault and never reads the user's Adit configuration.
use adit_domain::{ConnectionProfile, ProfileId, Protocol, SessionStatus};
use adit_session::SessionManager;
use adit_storage::{CredentialError, CredentialProtection, CredentialStore, ProfileStore};
use std::{
    io::{self, BufRead},
    path::PathBuf,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        // Only the unique directory constructed by this probe is eligible.
        if self.0.parent() == Some(std::env::temp_dir().as_path())
            && self
                .0
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with("adit-vault-rdp-"))
        {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: vault_rdp_probe private-ip username [port]; password on stdin".into());
    }
    let host: std::net::Ipv4Addr = args[1].parse()?;
    if !host.is_private() && !host.is_loopback() {
        return Err("acceptance is restricted to a private/loopback fixture".into());
    }
    let mut password = Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password)?;
    let scratch =
        Scratch(std::env::temp_dir().join(format!("adit-vault-rdp-{}", ProfileId::new())));
    std::fs::create_dir(&scratch.0)?;
    let profile_store = ProfileStore::new(scratch.0.join("profiles.json"));
    let mut profile = ConnectionProfile::new(
        "isolated protected RDP",
        &args[1],
        args.get(3).map_or(Ok(3389), |s| s.parse())?,
        &args[2],
    );
    profile.protocol = Protocol::Rdp;
    let id = profile.id;
    profile_store.save_profiles(&[profile])?;
    let credentials = scratch.0.join("credentials.json");
    let master = Zeroizing::new(ProfileId::new().to_string());
    {
        let vault = CredentialStore::with_path("isolated-rdp-probe", &credentials);
        // The real fixture password is first written directly into v2. No
        // legacy-key backup containing that password is created by this probe.
        vault.enable_master_password(&master)?;
        vault.save_profile_password(id, password.trim_end_matches(['\r', '\n']))?;
    }
    drop(password);
    let vault = CredentialStore::with_path("isolated-rdp-probe", &credentials);
    assert_eq!(vault.protection_status()?, CredentialProtection::Locked);
    assert!(matches!(
        vault.load_profile_password(id),
        Err(CredentialError::Locked)
    ));
    let bytes = std::fs::read(&credentials)?;
    assert!(vault.unlock("incorrect isolated master").is_err());
    assert_eq!(std::fs::read(&credentials)?, bytes);
    vault.unlock(&master)?;
    let mut manager = SessionManager::with_profiles(profile_store.load_profiles()?);
    manager.set_auto_reconnect(false);
    manager.set_rdp_clipboard(false);
    let started = Instant::now();
    let session = manager.open_live_rdp_session(
        id,
        vault.load_profile_password(id)?.ok_or("password missing")?,
        1280,
        800,
    )?;
    let accepted: Result<(), String> = loop {
        manager.poll_events();
        if let Some(error) = manager.active_desktop_error() {
            break Err(error.to_owned());
        }
        if manager
            .active_session_summary()
            .is_some_and(|s| s.status == SessionStatus::Connected)
        {
            if let Some(frame) = manager.active_rdp_frame_if_newer(0) {
                if frame.width == 1280
                    && frame.height == 800
                    && frame.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
                {
                    println!("protected RDP cold-load: authenticated complete 1280x800 framebuffer; elapsed_ms={}", started.elapsed().as_millis());
                    break Ok(());
                }
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            break Err("RDP acceptance timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    vault.lock_vault()?;
    assert!(matches!(
        vault.load_profile_password(id),
        Err(CredentialError::Locked)
    ));
    manager.close(session);
    accepted.map_err(Into::into)
}
