//! Manual multi-monitor acceptance for issue #233 against a real RDP server.
//!
//! argv: private-ip username [port]. Password on stdin; helper via ADIT_RDP_HOST.
//! Uses an isolated scratch config dir and never reads the user's Adit
//! configuration.
//!
//! The point of this probe is the *reconnect* half. Connecting with a
//! multi-screen layout and seeing the server honour it is the easy half, and
//! the protocol-level probe already covers it. What cannot be seen from the
//! wire is whether the layout comes back after the session is dropped and the
//! profile is loaded again from disk — that is a property of the app's
//! profile persistence, not of the protocol.
//!
//! So this probe drives the app's own connect path (`open_live_rdp_session`)
//! twice against the same on-disk profile: once with a two-monitor layout
//! persisted in the profile, then a second time after the profile has been
//! reloaded from the file. If the layout is honoured on the second connect,
//! the layout survived the round trip through the store.

use adit_domain::{
    AuthMethod, ConnectionProfile, DesktopMonitor, ProfileId, Protocol, SessionOptions, SessionStatus,
};
use adit_session::SessionManager;
use adit_storage::{CredentialStore, ProfileStore};
use std::{
    io::{self, BufRead},
    path::PathBuf,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

/// The layout persisted in the profile: two 800x600 screens side by side.
fn two_monitors() -> Vec<DesktopMonitor> {
    vec![
        DesktopMonitor {
            left: 0,
            top: 0,
            width: 800,
            height: 600,
            primary: true,
            scale_percent: 100,
        },
        DesktopMonitor {
            left: 800,
            top: 0,
            width: 800,
            height: 600,
            primary: false,
            scale_percent: 100,
        },
    ]
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        // Only the unique directory constructed by this probe is eligible.
        if self.0.parent() == Some(std::env::temp_dir().as_path())
            && self
                .0
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with("adit-multimon-"))
        {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: multimon_reconnect_probe private-ip username [port]; password on stdin".into());
    }
    let host: std::net::Ipv4Addr = args[1].parse()?;
    if !adit_domain::is_non_public_v4(host) {
        return Err("acceptance is restricted to a private/loopback host".into());
    }
    let mut password = Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']).to_owned();

    let scratch = Scratch(std::env::temp_dir().join(format!("adit-multimon-{}", ProfileId::new())));
    std::fs::create_dir(&scratch.0)?;
    let profiles_path = scratch.0.join("profiles.json");
    let profile_store = ProfileStore::new(profiles_path.clone());

    let mut profile = ConnectionProfile::new(
        "isolated multi-monitor acceptance",
        &args[1],
        args.get(3).map_or(Ok(3389), |s| s.parse())?,
        &args[2],
    );
    profile.protocol = Protocol::Rdp;
    profile.auth_method = AuthMethod::Password;
    // The layout lives in the profile's session options, which is where the
    // app keeps it between sessions. Writing it here is the whole point:
    // the second connect must reuse it without anything re-supplying it.
    profile.session_options = SessionOptions {
        rdp_monitors: Some(two_monitors()),
        ..profile.session_options.clone()
    };
    profile.session_options.validate()?;
    let id = profile.id;
    profile_store.save_profiles(&[profile])?;

    let credentials = scratch.0.join("credentials.json");
    {
        let vault = CredentialStore::with_path("isolated-multimon-probe", &credentials);
        vault.save_profile_password(id, &password)?;
    }

    // Two connects, both against the profile as it is on disk. The second
    // one runs after the profile has been read back from the file, so the
    // layout can only come from the store.
    for round in 1..=2 {
        // Reload the profile from disk every round. That is what makes the
        // second round a persistence test rather than a repeat of the first.
        let loaded = profile_store.load_profiles()?;
        let stored = loaded
            .iter()
            .find(|p| p.id == id)
            .ok_or("the profile vanished from the store")?;
        let monitors = stored.session_options.rdp_monitors.clone();
        println!(
            "round={round} persisted_monitors={}",
            monitors
                .as_ref()
                .map(|m| m
                    .iter()
                    .map(|s| format!("{}x{}+{}+{}", s.width, s.height, s.left, s.top))
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_else(|| "none".into())
        );

        let vault = CredentialStore::with_path("isolated-multimon-probe", &credentials);
        let stored_password = vault.load_profile_password(id)?.ok_or("password missing")?;
        let mut manager = SessionManager::with_profiles(loaded);
        manager.set_auto_reconnect(false);
        manager.set_rdp_clipboard(false);

        let started = Instant::now();
        let session = manager.open_live_rdp_session(id, stored_password, 800, 600)?;
        let observed: Result<Vec<(u16, u16, i32, i32)>, String> = loop {
            manager.poll_events();
            if let Some(error) = manager.active_desktop_error() {
                break Err(error.to_owned());
            }
            if manager
                .active_session_summary()
                .is_some_and(|s| s.status == SessionStatus::Connected)
            {
                // The DisplayControl capabilities are the gate for anything
                // layout-related, so wait for them before judging the layout.
                if let Some((state, _)) = manager.active_rdp_display_control() {
                    if state.max_monitors > 0 {
                        break Ok(state
                            .sent_layout
                            .iter()
                            .map(|m| (m.width, m.height, m.left, m.top))
                            .collect());
                    }
                }
            }
            if started.elapsed() > Duration::from_secs(60) {
                break Err("RDP acceptance timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        manager.close(session);

        let layout = observed.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        println!("round={round} negotiated_layout={layout:?}");
        if layout.len() != 2 {
            return Err(format!(
                "round {round}: expected a two-monitor layout, got {layout:?}"
            )
            .into());
        }
    }

    println!("multi-monitor layout survived a reconnect through the profile store");
    Ok(())
}
