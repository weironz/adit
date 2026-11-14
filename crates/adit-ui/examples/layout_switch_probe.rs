//! Manual acceptance for #233's "the remote desktop follows a layout change"
//! item, against a real RDP server.
//!
//! argv: host username [port]. Password on stdin; helper via ADIT_RDP_HOST.
//! Uses an isolated scratch config dir and never reads the user's Adit
//! configuration.
//!
//! ## Why this probe exists
//!
//! #233 asks for four things. The second is "after switching the layout in the
//! toolbar, the remote desktop follows". Everything that reaches the wire
//! before that point carries the layout the client advertised at connect time:
//! `SessionOptions.rdp_monitors` is used twice — once in the GCC Client
//! Monitor Data block, once in a post-connect DisplayControl request — and
//! both times with the *same* geometry.
//!
//! So the DisplayControl channel is exercised, the server rebuilds its
//! surfaces, and nothing ever actually *changes*. Every acceptance run so far
//! has therefore proven that the channel works, not that a change is followed.
//!
//! The path that does change the layout is
//! `SessionManager::set_active_rdp_monitor_count` — the same call the toolbar's
//! single/dual-screen button makes. Going from two monitors to one shrinks
//! the virtual desktop from 2400x800 to 1200x800, which is a layout the server
//! has never seen in this session.
//!
//! ## What is measured, and against what
//!
//! The client side is easy to read: `active_rdp_display_control()` reports the
//! layout that was last encoded onto the DisplayControl channel. That is the
//! client's own account of what it sent.
//!
//! The server side is what makes it an acceptance rather than a self-report,
//! and DisplayControl has no layout acknowledgement, so it cannot be read off
//! the wire. It has to be read off the server's own log. GRD's journal is the
//! oracle: it records the monitor geometry it built for every layout it
//! receives. The probe prints the exact command to read it, so the two
//! accounts can be compared by hand.
//!
//! ## Why the probe does not assert on the client's surface list
//!
//! It is tempting to conclude "the remote followed" from the client's own
//! view of the server's surfaces — the server deleting the old ones and
//! creating new ones. That conclusion does not hold, and this host shows
//! why: when it applies a two-monitor -> one-monitor request it disposes
//! only the *second* virtual monitor and keeps the first one alive, so the
//! surviving surface is the same surface as before — no deletion, no new
//! id, no geometry change. A probe asserting "one surface left" fails on a
//! switch the server applied perfectly.
//!
//! So the client-side truth is what the client sent, the server-side truth
//! is what the server logged, and the acceptance compares the two.

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

/// The layout persisted in the profile: two 1200x800 screens side by side.
///
/// 1200x800 is the size the GRD acceptance used, so the numbers in the
/// server's journal can be compared with the ones already recorded.
fn two_monitors() -> Vec<DesktopMonitor> {
    vec![
        DesktopMonitor {
            left: 0,
            top: 0,
            width: 1200,
            height: 800,
            primary: true,
            scale_percent: 100,
        },
        DesktopMonitor {
            left: 1200,
            top: 0,
            width: 1200,
            height: 800,
            primary: false,
            scale_percent: 100,
        },
    ]
}

/// How long a single wait may take before it is called a failure.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        // Only the unique directory constructed by this probe is eligible.
        if self.0.parent() == Some(std::env::temp_dir().as_path())
            && self
                .0
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with("adit-layout-switch-"))
        {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// The layout, in the shape the wire carries it.
fn render(monitors: &[(u16, u16, i32, i32)]) -> String {
    if monitors.is_empty() {
        return "none".into();
    }
    monitors
        .iter()
        .map(|(w, h, left, top)| format!("{w}x{h}+{left}+{top}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The helper's log, from the point this session's helper started.
///
/// The helper writes every surface it is told about to one shared file, and
/// the file also holds every *earlier* session's lines. Reading from the
/// start would mix a previous session's geometry into this one's, so the
/// baseline offset is recorded before the session opens and the first banner
/// at or after it marks this helper's first line.
struct HelperLog {
    path: PathBuf,
    start: u64,
}

fn locate_helper_log(baseline: u64) -> Option<HelperLog> {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join("Adit").join("rdp-helper.log");
    let text = std::fs::read_to_string(&path).ok()?;
    // The log rolls once it grows past 4 MiB, which resets every offset —
    // then the newest banner is the only usable one.
    let rolled = (text.len() as u64) < baseline;
    let start = if rolled {
        text.rfind("=== adit-rdp-host")
    } else {
        text.match_indices("=== adit-rdp-host")
            .map(|(index, _)| index)
            .find(|index| *index as u64 >= baseline)
    }?;
    Some(HelperLog {
        path,
        start: start as u64,
    })
}

impl HelperLog {
    /// The surface sets this session's helper was told about, one set per
    /// generation.
    ///
    /// `surface mapped` is the line that carries a geometry, and the server
    /// sends it for each surface it creates. The server tears its old
    /// surfaces down before it creates the new ones, so a deletion closes a
    /// generation.
    ///
    /// The generations are the server's *surface* history, not its layout
    /// history — see the note at the call site for why the two are not the
    /// same thing.
    fn surface_generations(&self) -> Vec<Vec<(u16, u16, i32, i32)>> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let Some(rest) = text.get(self.start as usize..) else {
            return Vec::new();
        };
        let mut sizes: Vec<(u16, u16)> = Vec::new();
        let mut generations: Vec<Vec<(u16, u16, i32, i32)>> = Vec::new();
        let mut current: Vec<(u16, u16, i32, i32)> = Vec::new();
        for line in rest.lines() {
            let Some(rest_of_line) = line.split("adit_rdp::egfx: surface ").nth(1) else {
                continue;
            };
            let Some(id) = rest_of_line
                .split_whitespace()
                .find_map(|word| word.strip_prefix("id="))
                .and_then(|id| id.parse::<usize>().ok())
            else {
                continue;
            };
            let number = |key: &str| {
                rest_of_line
                    .split_whitespace()
                    .find_map(|word| word.strip_prefix(key))
                    .and_then(|value| value.parse::<i64>().ok())
            };
            if rest_of_line.starts_with("created") {
                let size = (
                    number("w=").unwrap_or(0) as u16,
                    number("h=").unwrap_or(0) as u16,
                );
                while sizes.len() <= id {
                    sizes.push((0, 0));
                }
                sizes[id] = size;
            } else if rest_of_line.starts_with("deleted") {
                // The server tears the old surfaces down before it creates the
                // new ones, so a deletion closes the current generation.
                if !current.is_empty() {
                    generations.push(std::mem::take(&mut current));
                }
            } else if rest_of_line.starts_with("mapped") {
                let size = sizes.get(id).copied().unwrap_or((0, 0));
                current.push((
                    size.0,
                    size.1,
                    number("origin_x=").unwrap_or(0) as i32,
                    number("origin_y=").unwrap_or(0) as i32,
                ));
            }
        }
        if !current.is_empty() {
            generations.push(current);
        }
        generations
    }
}

/// Wait until the session's DisplayControl state reports the wanted layout.
///
/// `sent_layout` is the layout that was last encoded onto the channel, so it
/// is the client's own record of what the server was asked for. It only
/// changes once the helper has confirmed the encoding, which is the point at
/// which the request has actually gone out.
fn wait_for_layout(
    manager: &mut SessionManager,
    wanted: usize,
) -> Result<Vec<(u16, u16, i32, i32)>, String> {
    let started = Instant::now();
    loop {
        manager.poll_events();
        if let Some(error) = manager.active_desktop_error() {
            return Err(error.to_owned());
        }
        if manager
            .active_session_summary()
            .is_some_and(|s| s.status == SessionStatus::Connected)
        {
            if let Some((state, _)) = manager.active_rdp_display_control() {
                if state.max_monitors > 0 && state.sent_layout.len() == wanted {
                    return Ok(state
                        .sent_layout
                        .iter()
                        .map(|m| (m.width, m.height, m.left, m.top))
                        .collect());
                }
            }
        }
        if started.elapsed() > STEP_TIMEOUT {
            return Err("timed out waiting for the layout to be sent".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: layout_switch_probe host username [port]; password on stdin".into());
    }
    // The guard is the point of the check: a stray command line must not be
    // able to aim a real password at a third party's machine. A tailnet
    // address counts as "not the public internet" — see `adit_domain`.
    let host: std::net::IpAddr = args[1].parse()?;
    if !adit_domain::is_non_public(host) {
        return Err("acceptance is restricted to a non-public host".into());
    }
    let mut password = Zeroizing::new(String::new());
    io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']).to_owned();

    let scratch = Scratch(std::env::temp_dir().join(format!("adit-layout-switch-{}", ProfileId::new())));
    std::fs::create_dir(&scratch.0)?;
    let profiles_path = scratch.0.join("profiles.json");
    let profile_store = ProfileStore::new(profiles_path.clone());

    let mut profile = ConnectionProfile::new(
        "isolated layout-switch acceptance",
        &args[1],
        args.get(3).map_or(Ok(3389), |s| s.parse())?,
        &args[2],
    );
    profile.protocol = Protocol::Rdp;
    profile.auth_method = AuthMethod::Password;
    // The connect-time layout. Two monitors, so that the switch to one is a
    // real change rather than the first layout the server ever sees.
    profile.session_options = SessionOptions {
        rdp_monitors: Some(two_monitors()),
        ..profile.session_options.clone()
    };
    profile.session_options.validate()?;
    let id = profile.id;
    profile_store.save_profiles(&[profile])?;

    let credentials = scratch.0.join("credentials.json");
    {
        let vault = CredentialStore::with_path("isolated-layout-switch-probe", &credentials);
        vault.save_profile_password(id, &password)?;
    }

    let loaded = profile_store.load_profiles()?;
    let vault = CredentialStore::with_path("isolated-layout-switch-probe", &credentials);
    let stored_password = vault.load_profile_password(id)?.ok_or("password missing")?;
    let mut manager = SessionManager::with_profiles(loaded);
    manager.set_auto_reconnect(false);
    manager.set_rdp_clipboard(false);

    // The helper's log is where the *server's* answer shows up on the client
    // side: the surfaces it creates are the ones the server told it about.
    // The baseline is the log's length before the session opens, so a helper
    // that is already running does not contribute its lines.
    let log_baseline = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Adit")
        .join("rdp-helper.log")
        .metadata()
        .map(|meta| meta.len())
        .unwrap_or(0);

    // The desktop has to be big enough for the *widest* layout the session
    // will ask for, or the outer monitor would be clipped.
    let session = manager.open_live_rdp_session(id, stored_password, 2400, 800)?;

    let connect_layout = wait_for_layout(&mut manager, 2)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    println!("connect_time_layout={}", render(&connect_layout));

    // The actual switch: two monitors -> one. This is the call the toolbar's
    // single-screen button makes, and the only one that sends a layout the
    // server has not already seen.
    manager.set_active_rdp_monitor_count(1)?;

    let switched = wait_for_layout(&mut manager, 1).map_err(|e| -> Box<dyn std::error::Error> {
        e.into()
    })?;
    println!("after_switch_layout={}", render(&switched));

    if switched.len() != 1 {
        return Err(format!("expected a single monitor after the switch, got {switched:?}").into());
    }
    if switched == connect_layout {
        return Err("the switch did not change the layout".into());
    }
    // The single monitor must be the one that was primary, not an arbitrary
    // one: the switch shrinks the desktop, it does not move the pointer's
    // world around.
    if switched[0] != connect_layout[0] {
        return Err(format!(
            "the surviving monitor is not the primary one: {switched:?} vs {connect_layout:?}"
        )
        .into());
    }

    // Give the helper a moment to write the surfaces the server created for
    // the new layout before the log is read.
    std::thread::sleep(Duration::from_millis(500));
    if let Some(log) = locate_helper_log(log_baseline) {
        let generations = log.surface_generations();
        for (index, generation) in generations.iter().enumerate() {
            println!("helper_generation_{index}={}", render(generation));
        }
        // The surface count is deliberately NOT asserted here, and the reason
        // is worth writing down.
        //
        // A layout change reaches the server as a DisplayControl request, and
        // DisplayControl has no layout acknowledgement and no monitor-layout
        // echo. What the client can see instead is the server's *surface*
        // churn — and that is not a proxy for the layout. When this host
        // applies a two-monitor -> one-monitor request it disposes only the
        // *second* virtual monitor and keeps the first one alive, so the
        // surviving surface is byte-for-byte the same surface as before: no
        // deletion, no new id, no geometry change. A probe that asserted
        // "one surface left" would fail on a switch that the server applied
        // correctly.
        //
        // So the client-side truth is what the client sent, and the
        // server-side truth is what the server logged. The two are compared
        // by hand, using the command the probe prints.
        println!(
            "client_side_surface_count_is_not_a_layout_observable=true (last generation has {} surface(s))",
            generations.last().map_or(0, Vec::len)
        );
    }

    manager.close(session);

    println!("client_sent_layout_change=yes");
    println!("client_layouts={} -> {}", render(&connect_layout), render(&switched));
    println!();
    println!("now check the server's own account of the same session:");
    println!("  journalctl --user -u gnome-remote-desktop-headless.service | grep -E 'DISP|Layout manager'");
    println!("the server must report a layout with ONE monitor after the switch");
    Ok(())
}
