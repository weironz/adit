//! Isolated network-recovery acceptance. Password on stdin, never argv/env.
//! Run via tests/vnc/network-recovery.sh, not against a user's desktop.
use adit_domain::{ConnectionProfile, Protocol, SessionStatus, VncQuality};
use adit_session::SessionManager;
use std::{
    io::{self, BufRead},
    path::Path,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

fn solid(manager: &SessionManager, rgb: [u8; 3]) -> bool {
    manager.active_rdp_frame_if_newer(0).is_some_and(|frame| {
        frame.width == 800
            && frame.height == 600
            && frame.rgba.len() == 800 * 600 * 4
            && frame
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel == &[rgb[0], rgb[1], rgb[2], 255])
    })
}

fn marker(dir: &Path, name: &str, start: Instant) -> io::Result<()> {
    println!("phase={name} elapsed_ms={}", start.elapsed().as_millis());
    std::fs::write(dir.join(name), b"ready\n")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let synthetic_none = args.get(4).is_some_and(|value| value == "--synthetic-none");
    if args.len() != 4 && !(args.len() == 5 && synthetic_none) {
        return Err(
            "usage: vnc_recovery_probe private-ip port owned-marker-dir; password on stdin".into(),
        );
    }
    let ip: std::net::Ipv4Addr = args[1].parse()?;
    if !ip.is_private() && !ip.is_loopback() {
        return Err(
            "recovery acceptance is limited to an isolated private/loopback fixture".into(),
        );
    }
    let dir = Path::new(&args[3]);
    if !dir.is_dir() {
        return Err("the runner must create a unique marker directory first".into());
    }
    let mut profile =
        ConnectionProfile::new("isolated network recovery", &args[1], args[2].parse()?, "");
    profile.protocol = Protocol::Vnc;
    if synthetic_none {
        if !ip.is_loopback() {
            return Err("synthetic None acceptance requires loopback".into());
        }
        profile.vnc.security = adit_domain::VncSecurity::AllowNone;
    }
    profile.vnc.read_only = true;
    profile.vnc.clipboard_send = false;
    profile.vnc.clipboard_receive = false;
    profile.vnc.quality = VncQuality::Zrle;
    profile.vnc.auto_reconnect = true;
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let mut password = Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password)?;
    let session =
        manager.open_live_vnc_session(id, password.trim_end_matches(['\r', '\n']).into())?;
    drop(password);
    let start = Instant::now();
    let mut last_progress = start;
    let (mut ready, mut disconnected, mut recovered) = (false, false, false);
    let accepted = loop {
        manager.poll_events();
        let summary = manager
            .active_session_summary()
            .ok_or("lost active session")?;
        assert_eq!(summary.id, session, "recovery opened a different tab");
        assert_eq!(
            manager.sessions().len(),
            1,
            "recovery duplicated the session"
        );
        if summary.status == SessionStatus::Error {
            break Err(format!(
                "terminal connection error: {:?}",
                manager.active_desktop_error()
            ));
        }
        let connected = summary.status == SessionStatus::Connected;
        if last_progress.elapsed() >= Duration::from_secs(5) {
            println!(
                "progress elapsed_ms={} status={:?} diagnostics={:?}",
                start.elapsed().as_millis(),
                summary.status,
                manager.active_vnc_diagnostics()
            );
            last_progress = Instant::now();
        }
        if !ready
            && connected
            && solid(&manager, [255, 0, 0])
            && manager
                .active_vnc_diagnostics()
                .is_some_and(|diagnostics| diagnostics.fence_rtt_ms.is_some())
        {
            ready = true;
            marker(dir, "ready", start)?;
        } else if ready && !disconnected && !connected {
            disconnected = true;
            assert!(
                !solid(&manager, [255, 0, 0]),
                "disconnected session retained the old desktop"
            );
            marker(dir, "disconnected", start)?;
        } else if disconnected && !recovered && connected && solid(&manager, [0, 0, 255]) {
            recovered = true;
            assert!(
                manager.active_vnc_read_only(),
                "reconnect discarded read-only policy"
            );
            let diagnostics = manager
                .active_vnc_diagnostics()
                .ok_or("missing diagnostics")?;
            assert_eq!(
                diagnostics.security,
                if synthetic_none { "None" } else { "VncAuth" }
            );
            assert!(
                diagnostics.encoding.contains("(16,"),
                "ZRLE policy was not preserved"
            );
            assert!(diagnostics.pending_updates <= 1);
            println!(
                "recovered_policy={},ZRLE,readonly; session_count=1",
                diagnostics.security
            );
            marker(dir, "recovered", start)?;
        } else if recovered && connected && solid(&manager, [0, 255, 0]) {
            assert!(manager.active_vnc_read_only());
            marker(dir, "resumed", start)?;
            break Ok(());
        }
        if start.elapsed() > Duration::from_secs(140) {
            break Err(format!(
                "recovery deadline; phase=({ready},{disconnected},{recovered}); diagnostics={:?}",
                manager.active_vnc_diagnostics()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    manager.close(session);
    accepted.map_err(Into::into)
}
