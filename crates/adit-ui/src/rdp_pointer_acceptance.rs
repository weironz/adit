//! Native GUI acceptance for #233's pointer-coordinate item.
//!
//! The multi-monitor item that is easiest to get wrong is the pointer: a
//! coordinate that belongs on the second screen but is delivered as if it
//! were on the first still *draws* correctly, because the framebuffer is one
//! flat grid of pixels. The mistake is invisible in the picture and visible
//! only in the numbers, which is why it needs its own acceptance.
//!
//! ## What is measured
//!
//! The issue names three things that have to agree: the local coordinate, the
//! remote coordinate, and the scale factor. The chain in the app is
//!
//! ```text
//! OS pointer -> client point -> remote pixel -> wire coordinate
//! ```
//!
//! and each arrow is a place where a factor can be dropped. So the harness
//! drives the real OS pointer to a chosen client point, works out what the
//! app's own arithmetic says that should be on the wire, and then reads the
//! coordinate that *actually* reached the wire.
//!
//! The last arrow is the one that cannot be derived from the app's state, so
//! it is read from the helper's log: the helper logs every pointer move it
//! receives, and the log is the only place the wire coordinate exists.
//!
//! ## Why the server's own answer is not used
//!
//! A server that draws the pointer into its framebuffer would settle the
//! question end to end. GRD does not: it paints no cursor, it never sends a
//! pointer position back (its cursor update carries a shape and a hotspot but
//! no position), and it drives the host's pointer through libei on a virtual
//! monitor that lives outside the host's physical screen. So the server half
//! is not observable on that host, and what is left is the half the issue
//! actually names: the client's own arithmetic. This harness therefore
//! measures the client against itself — the coordinate the app claims to have
//! computed, against the coordinate that reached the wire — on both
//! monitors, which is exactly where a dropped offset would show up.
//!
//! ## What is deliberately not done
//!
//! The harness never *reads* the app's picture, and it never assumes the
//! picture's size: the surface size, the fit factors and the display scale
//! all come from the app's live state, because the app renegotiates the
//! remote desktop and a stale assumption would measure the pointer against
//! the wrong picture.
//!
//! It also never opens the session until the window's real client area is
//! known. The window's *outer* size is what gets requested, and the frame
//! around it is not ours to predict; a pane a few pixels narrower than the
//! remote desktop would make the app scale the picture to fit and scroll it
//! when it does not, which changes where the surface starts and would turn
//! the measurement into a guess.

use super::*;
use std::io::Write;

/// The remote desktop: two 1200×800 monitors side by side.
///
/// 1200 wide keeps a monitor's worth of pixels an exact number of client
/// points, and keeps the whole 2400-wide virtual desktop inside the local
/// screen, so the second monitor's points are reachable by the OS pointer.
const MONITOR_WIDTH: u16 = 1200;
const MONITOR_HEIGHT: u16 = 800;

/// The acceptance window's size.
///
/// iced's window size is the *inner* size, which is exactly the area the app
/// lays out in. The chrome takes 36 (menu) + 34 (tab strip) + 28 (status
/// bar) = 98 points of that, and the collapsed sidebar's reveal strip takes
/// 16 more, so a window of 2416×898 leaves a pane of exactly 2400×800 — the
/// virtual desktop's size.
///
/// That exactness is the point. The pane and the remote desktop are then the
/// same size, so the app asks the server for exactly the desktop's size, the
/// server answers with exactly that, the fit factors come out at 1.0, and the
/// picture is drawn 1:1 with no scaling anywhere in the chain. A harness that
/// scaled would be measuring a rounding exercise instead of a mapping.
const WINDOW_WIDTH: f32 = 2416.0;
const WINDOW_HEIGHT: f32 = 898.0;

/// Where the acceptance window is put on the screen.
///
/// The window is 2416 points wide and the local screen is 2560, so it
/// barely fits. Left to itself, Windows puts a new window wherever it
/// likes, and a window whose client origin sits at x=320 pushes the second
/// monitor's right-hand edge off the screen — where `SetCursorPos` silently
/// clamps the pointer to the last pixel instead of failing. The harness
/// would then measure the *clamped* position and blame the app's mapping for
/// a coordinate the harness itself had moved off-screen.
///
/// Pinning the window to the top-left corner puts the client origin at
/// x=0, which leaves the whole 2400-point virtual desktop inside the
/// screen. The position is in the window's *outer* coordinates, so the
/// frame's shadow and border offset the client area by a few points; the
/// harness does not rely on that offset, because it reads the pointer's
/// actual position back rather than assuming one.
const WINDOW_POSITION: (f32, f32) = (0.0, 0.0);

/// How long a step waits for the pointer to reach the wire.
const STEP_TIMEOUT_MS: u64 = 15000;

/// How often the pointer is driven again while a step waits for the wire.
///
/// The app drops a pointer move whose frame lease has gone stale, so a single
/// move can be lost to a race between the harness's decision and the app's
/// handling of it. Repeating every 250 ms turns that race into a wait.
const REDRIVE_MS: u64 = 250;

/// How long the wire coordinate must stay unchanged before it is read as the
/// pointer's final position.
///
/// A long jump makes Windows emit a series of `WM_MOUSEMOVE` messages as the
/// pointer travels, and every one of them is a real move that reaches the
/// wire. Only the last one is the destination.
const SETTLE_MS: u64 = 400;

/// How long the app is given to settle after the session connects, before
/// the first step drives the pointer.
///
/// The picture arrives as a set of tiles that are uploaded asynchronously,
/// and the pointer is only forwarded once the frame lease is accepted, so
/// the harness waits for a real frame rather than for a fixed time.
const CONNECT_SETTLE_MS: u64 = 4000;

/// The remote points the pointer is driven to.
///
/// All of them sit at mid-height: the RDP toolbar is a floating bar centred
/// at the *top* of the picture, and a point underneath it would be answered
/// by the toolbar's own mouse area instead of by the surface.
///
/// Two of them are on the first monitor and two on the second, and each has
/// to be a *different* point: `SetCursorPos` to the pointer's current
/// position generates no `WM_MOUSEMOVE`, so the app would never learn a
/// position and nothing would reach the wire.
///
/// The second monitor's points are what make the item worth measuring. A
/// dropped monitor offset would put them at the first monitor's
/// coordinates, and nothing else in the app would notice: the framebuffer is
/// one flat grid of pixels, so a coordinate that belongs on the second screen
/// but is delivered as if it were on the first still draws correctly.
const POINTS: &[(&str, u16, u16)] = &[
    ("mon0-near-origin", 100, 400),
    ("mon0-far-edge", 1100, 400),
    ("mon1-near-origin", 1300, 400),
    ("mon1-far-edge", 2300, 400),
];

/// The harness's own messages.
#[derive(Clone, Debug)]
enum NativeMessage {
    App(Message),
    /// The window's own handle, read on the window's thread.
    Hwnd(Option<isize>),
    /// The window's own id and scale factor.
    WindowInfo(Option<(window::Id, f32)>),
    /// The OS pointer was moved to the requested client point.
    PointerMoved(Option<PointerLanding>),
    Tick,
}

/// The app's own state, plus everything the harness needs to run its plan.
struct State {
    app: AditApp,
    events: std::sync::mpsc::Sender<String>,
    /// The profile the session is opened for.
    profile_id: adit_domain::ProfileId,
    /// The password, held until the window's real size is known.
    password: String,
    /// Whether the session has been opened.
    opened: bool,
    /// The window handle, needed to move the OS pointer to a client point.
    hwnd: Option<isize>,
    /// The window's scale factor, to turn logical client points into the
    /// physical pixels `SetCursorPos` takes.
    scale_factor: f32,
    /// Whether the window has reported its size yet.
    sized: bool,
    /// The step being served, as an index into `POINTS`.
    step: Option<usize>,
    /// The step to serve next.
    ///
    /// Separate from `step` because a step that has been *measured* is done,
    /// and the next one starts at the following index — the index is not
    /// derived from `results.len()`, because a step that times out still
    /// counts as measured.
    next_step: usize,
    /// When the current step started.
    step_started: Option<Instant>,
    /// The client point the pointer was last driven to.
    pointer_client: Option<(f32, f32)>,
    /// When the pointer was last driven.
    last_redrive: Option<Instant>,
    /// The last wire coordinate read, used to tell a settled coordinate from
    /// one that is still travelling.
    settled_wire: Option<(u16, u16)>,
    /// When the wire coordinate was last seen to stop changing.
    settled_since: Option<Instant>,
    /// Where the pointer actually landed after the last drive.
    ///
    /// Not the point that was asked for: `SetCursorPos` clamps a point
    /// outside the screen, and a clamped move still reaches the app. The
    /// landing is what the app really saw, so comparing it against the
    /// request is how the harness tells a clamped move from a mapping
    /// error.
    pointer_landing: Option<PointerLanding>,
    /// The helper's log, located once the helper has started.
    helper_log: Option<HelperLog>,
    /// The log's length before the session was opened, so the harness reads
    /// only the lines its own helper wrote.
    log_baseline: u64,
    /// How many pointer lines the log held at the last measurement.
    seen_pointer_lines: usize,
    /// The measurements, one per step.
    results: Vec<serde_json::Value>,
    finished: bool,
}

/// Where the helper's log is, and how far into it this session's helper
/// starts.
struct HelperLog {
    path: std::path::PathBuf,
    /// The byte offset this session's helper's banner is at.
    ///
    /// The log is shared by every helper on the machine, so the harness
    /// records where its *own* helper starts and reads only from there. The
    /// offset is found from a baseline taken before the session was opened:
    /// a helper that was already running has its banner before the baseline
    /// and must not be read.
    start: u64,
}

impl HelperLog {
    /// The pointer moves the helper logged since this session's banner.
    ///
    /// Returns the last one and how many lines there were in total, so the
    /// caller can tell a *new* move from the previous step's.
    fn pointer_moves(&self) -> Option<((u16, u16), usize)> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let body = text.get(self.start as usize..)?;
        let mut count = 0usize;
        let mut last = None;
        for line in body.lines() {
            // The helper's own line:
            //   INFO adit_rdp::session: pointer move reached the helper x=200 y=200
            let Some(rest) = line.split("pointer move reached the helper").nth(1) else {
                continue;
            };
            let mut x = None;
            let mut y = None;
            for field in rest.split_whitespace() {
                if let Some(value) = field.strip_prefix("x=") {
                    x = value.parse::<u16>().ok();
                } else if let Some(value) = field.strip_prefix("y=") {
                    y = value.parse::<u16>().ok();
                }
            }
            if let (Some(x), Some(y)) = (x, y) {
                count += 1;
                last = Some((x, y));
            }
        }
        last.map(|last| (last, count))
    }
}

/// The app's own surface geometry, derived from its live state.
///
/// This is the same arithmetic the app does when it draws the surface and
/// maps the mouse area, so what the harness computes here is what the app
/// *believes* the mapping is.
struct Geometry {
    /// The surface's top-left in window coordinates, in logical points.
    origin: (f32, f32),
    /// The per-axis scale from surface pixels to logical points.
    scale_x: f32,
    scale_y: f32,
    /// The fit factors the app settled on.
    fit: (f32, f32),
    /// The surface's size in device pixels.
    surface: (u16, u16),
}

fn surface_geometry(app: &AditApp) -> Option<Geometry> {
    let (sw, sh) = app.rdp_surface_size?;
    if sw == 0 || sh == 0 {
        return None;
    }
    let sidebar = sidebar::sidebar_offset(app);
    let (pane_w, pane_h) = session_ops::terminal_region_area(
        app.window_width,
        app.window_height,
        sidebar,
        session_ops::chrome_height(app),
    );
    let (fit_x, fit_y) = session_ops::rdp_fit_factors(app, (sw, sh));
    let scale_x = app.display_scale.max(0.1) / fit_x;
    let scale_y = app.display_scale.max(0.1) / fit_y;
    let logical_w = f32::from(sw) / scale_x;
    let logical_h = f32::from(sh) / scale_y;
    let body_top = MENU_BAR_HEIGHT + TAB_BAR_HEIGHT;
    // The surface is centred in the pane, unless it overflows: then it is
    // scrolled and starts at the pane's top-left instead.
    let overflows = logical_w > pane_w + 1.0 || logical_h > pane_h + 1.0;
    let origin = if overflows {
        (sidebar, body_top)
    } else {
        (
            sidebar + (pane_w - logical_w) / 2.0,
            body_top + (pane_h - logical_h) / 2.0,
        )
    };
    Some(Geometry {
        origin,
        scale_x,
        scale_y,
        fit: (fit_x, fit_y),
        surface: (sw, sh),
    })
}

/// The client point the pointer is driven to for a remote point.
fn client_point(app: &AditApp, target: (u16, u16)) -> Option<(f32, f32)> {
    let geometry = surface_geometry(app)?;
    Some((
        geometry.origin.0 + f32::from(target.0) / geometry.scale_x,
        geometry.origin.1 + f32::from(target.1) / geometry.scale_y,
    ))
}

/// The remote pixel the app's own mouse mapping produces for a client point.
///
/// This mirrors `workspace::rdp_surface_view`'s mouse area. Two details
/// matter and both are easy to get wrong:
///
/// * the mouse area's coordinate is **relative to the surface widget**, not
///   to the window, so the surface's origin has to come off before the
///   scale goes on;
/// * the result is clamped to the surface, because the app clamps it too.
///
/// The wire coordinate is this value plus the viewport's offset. The
/// viewport's offset is zero here: it is non-zero only when a single monitor
/// is selected out of a multi-monitor layout, and this profile selects none.
fn app_mapped_remote(geometry: &Geometry, client: (f32, f32)) -> (u16, u16) {
    let local_x = client.0 - geometry.origin.0;
    let local_y = client.1 - geometry.origin.1;
    let x = (local_x * geometry.scale_x).clamp(0.0, f32::from(geometry.surface.0) - 1.0);
    let y = (local_y * geometry.scale_y).clamp(0.0, f32::from(geometry.surface.1) - 1.0);
    (x as u16, y as u16)
}

/// Write one event to the harness's own log.
///
/// The log is a JSONL file, so it can be read with `jq` and diffed against
/// another run.
fn log(state: &State, mut event: serde_json::Value) {
    event["unix_ms"] = serde_json::json!(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis());
    let _ = state.events.send(event.to_string());
}

/// The window's own handle, read on the window's thread.
fn harness_hwnd() -> Task<NativeMessage> {
    window::latest()
        .and_then(|id| {
            window::run(id, |native| {
                #[cfg(windows)]
                {
                    use iced::window::raw_window_handle::RawWindowHandle;
                    let Ok(handle) = native.window_handle() else {
                        return None;
                    };
                    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                        return None;
                    };
                    Some(handle.hwnd.get())
                }
                #[cfg(not(windows))]
                {
                    let _ = native;
                    None
                }
            })
        })
        .map(NativeMessage::Hwnd)
}

/// The window's own id and scale factor, read on the window's thread.
fn harness_window_info() -> Task<NativeMessage> {
    window::latest()
        .and_then(|id| window::scale_factor(id).map(move |scale| (id, scale)).map(Some))
        .map(NativeMessage::WindowInfo)
}

/// Where the OS pointer actually ended up, relative to the window's client
/// area.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PointerLanding {
    /// The client point the pointer is on, in physical pixels.
    client: (i32, i32),
    /// The screen point the pointer is on, in physical pixels.
    screen: (i32, i32),
}

/// Move the OS pointer to a client point of our own window, and report where
/// it really landed.
///
/// The client point is in logical points; `ClientToScreen` turns it into a
/// screen point, which is what `SetCursorPos` takes. The conversion to
/// physical pixels happens here, because the window's scale factor is the
/// only thing that knows how many device pixels a logical point is.
///
/// The landing position is read back rather than assumed. `SetCursorPos`
/// **clamps** a point that lies outside the screen to the nearest edge
/// instead of failing, and the harness's far-edge point on the second
/// monitor can land outside it: the window is 2416 points wide and the
/// screen is 2560, so the window's own origin decides whether the point
/// fits. A clamped move still produces a `WM_MOUSEMOVE`, so the app
/// forwards it and the harness measures a coordinate that the harness — not
/// the app — got wrong. Reading the pointer back turns that into a
/// diagnosable condition instead of a failed assertion.
#[cfg(windows)]
fn drive_pointer(hwnd: isize, client: (f32, f32), scale_factor: f32) -> Task<NativeMessage> {
    window::latest()
        .and_then(move |id| {
            window::run(id, move |_native| {
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::Foundation::{HWND, POINT};
                    use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
                    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

                    let hwnd = HWND(hwnd as *mut _);
                    let mut point = POINT {
                        x: (client.0 * scale_factor).round() as i32,
                        y: (client.1 * scale_factor).round() as i32,
                    };
                    if !ClientToScreen(hwnd, &mut point).as_bool() {
                        return None;
                    }
                    let target = (point.x, point.y);
                    // A move to the pointer's current position generates no
                    // `WM_MOUSEMOVE`, so the app would never learn a position
                    // and nothing would reach the wire. Step aside first, so
                    // that moving to the target is a real change.
                    let mut current = POINT { x: 0, y: 0 };
                    if GetCursorPos(&mut current).is_ok() && (current.x, current.y) == target {
                        let _ = SetCursorPos(current.x + 1, current.y + 1);
                    }
                    let _ = SetCursorPos(target.0, target.1);
                    // Read the pointer back. The screen point is where the
                    // pointer really is, which may not be where it was asked
                    // to go; the client point is that position expressed in
                    // the window's own coordinates, which is what the app
                    // saw.
                    let mut landed = POINT { x: 0, y: 0 };
                    if !GetCursorPos(&mut landed).is_ok() {
                        return None;
                    }
                    let screen = (landed.x, landed.y);
                    let mut client_point = landed;
                    if !ScreenToClient(hwnd, &mut client_point).as_bool() {
                        return None;
                    }
                    Some(PointerLanding {
                        client: (client_point.x, client_point.y),
                        screen,
                    })
                }
                #[cfg(not(windows))]
                {
                    let _ = (hwnd, client, scale_factor);
                    None
                }
            })
        })
        .map(NativeMessage::PointerMoved)
}

#[cfg(not(windows))]
fn drive_pointer(
    _hwnd: isize,
    _client: (f32, f32),
    _scale_factor: f32,
) -> Task<NativeMessage> {
    Task::done(NativeMessage::PointerMoved(None))
}

/// Find the helper's log and where this session's helper starts in it.
///
/// The baseline is the log's length before the session was opened: a helper
/// that was already running has its banner before that, and its lines must
/// not be read as this session's.
fn locate_helper_log(baseline: u64) -> Option<HelperLog> {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join("Adit").join("rdp-helper.log");
    let text = std::fs::read_to_string(&path).ok()?;
    // The first banner at or after the baseline is this session's helper.
    // The log rolls once it grows past 4 MiB, which resets every offset —
    // then the newest banner is the only usable one.
    let rolled = (text.len() as u64) < baseline;
    let start = if rolled {
        text.rfind("=== adit-rdp-host")
    } else {
        text.match_indices("=== adit-rdp-host")
            .map(|(index, _)| index)
            .find(|index| *index as u64 >= baseline)
    };
    // No banner yet: this session's helper has not started, so there is
    // nothing to read. Falling back to offset 0 would read the whole log,
    // and a pointer move from an *earlier* session would be mistaken for a
    // fresh one — which is exactly what happened the first time this ran.
    let start = start? as u64;
    Some(HelperLog { path, start })
}

/// The fixture's password, read from stdin.
///
/// The password never goes into argv or the environment: argv is readable
/// through the process list by anything on the machine.
fn read_password() -> String {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("password on stdin");
    line.trim_end_matches(['\r', '\n']).to_string()
}

/// The two monitors the harness advertises.
fn two_monitors() -> Vec<adit_domain::DesktopMonitor> {
    vec![
        adit_domain::DesktopMonitor {
            left: 0,
            top: 0,
            width: MONITOR_WIDTH,
            height: MONITOR_HEIGHT,
            primary: true,
            scale_percent: 100,
        },
        adit_domain::DesktopMonitor {
            left: i32::from(MONITOR_WIDTH),
            top: 0,
            width: MONITOR_WIDTH,
            height: MONITOR_HEIGHT,
            primary: false,
            scale_percent: 100,
        },
    ]
}

pub fn run() -> iced::Result {
    // This entry point is available only to an explicit development feature.
    // The configuration directory is replaced before anything reads settings
    // or the credential store, so the real one is never touched.
    let scratch = std::env::temp_dir()
        .join(format!("adit-rdp-pointer-acceptance-{}", std::process::id()));
    std::fs::create_dir(&scratch).expect("fresh isolated acceptance directory");
    std::env::set_var("ADIT_CONFIG_DIR", &scratch);

    // The helper is named explicitly: the root workspace's `target` holds a
    // stale copy from before the pointer work, and the helper is a separate
    // workspace that has to be built on its own.
    let helper = std::env::var("ADIT_RDP_HELPER")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/adit-rdp/target/release/adit-rdp-host.exe")
        });
    if helper.exists() {
        std::env::set_var("ADIT_RDP_HOST", &helper);
    }
    // The helper's pointer probe is an `info` line, and the helper's default
    // filter is `warn`. Without this the harness would find nothing in the
    // log and every step would fail for a reason that has nothing to do
    // with the mapping.
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "warn,adit_rdp=info");
    }

    let host = std::env::var("ADIT_RDP_FIXTURE_HOST").unwrap_or_else(|_| "127.0.0.1:13394".into());
    let (host_name, port) = host.rsplit_once(':').unwrap_or((host.as_str(), "3389"));
    let port: u16 = port.parse().expect("fixture port");
    let username = std::env::var("ADIT_RDP_FIXTURE_USER").unwrap_or_else(|_| "ADIT\\fixture".into());
    let password = read_password();

    let mut profile = ConnectionProfile::new("RDP pointer acceptance", host_name, port, username);
    profile.protocol = Protocol::Rdp;
    profile.auth_method = AuthMethod::Password;
    // The layout lives in the profile, because that is where the app keeps it
    // between sessions — and the harness wants the same layout the app would
    // use for a real multi-screen profile.
    profile.session_options.rdp_monitors = Some(two_monitors());
    ProfileStore::default()
        .save_profiles(&[profile.clone()])
        .expect("acceptance profile");

    // The log's length before anything starts: the helper this session
    // launches appends its banner after this point, and the harness reads
    // only from there.
    let log_baseline = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Adit")
        .join("rdp-helper.log")
        .metadata()
        .map(|meta| meta.len())
        .unwrap_or(0);

    let (events, rx) = std::sync::mpsc::channel::<String>();
    let event_path = scratch.join("events.jsonl");
    std::thread::spawn(move || {
        let mut file = std::fs::File::create(event_path).expect("acceptance event log");
        for line in rx {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    });

    let boot = move || {
        let mut app = AditApp {
            auto_check_updates: false,
            keyring_migrated: true,
            auto_log_on_connect: false,
            rdp_clipboard: false,
            startup_maximize_pending: false,
            main_view: MainView::Terminal,
            sidebar_visible: false,
            terminal_focused: true,
            window_width: WINDOW_WIDTH,
            window_height: WINDOW_HEIGHT,
            display_scale: 1.0,
            ..AditApp::default()
        };
        app.manager.set_rdp_clipboard(false);
        (
            State {
                app,
                events: events.clone(),
                profile_id: profile.id,
                // The password is cloned rather than moved: the boot closure is
                // called again if the window is recreated, and the session is
                // opened from the update loop rather than from here.
                password: password.clone(),
                opened: false,
                hwnd: None,
                scale_factor: 1.0,
                sized: false,
                step: None,
                next_step: 0,
                step_started: None,
                pointer_client: None,
                last_redrive: None,
                settled_wire: None,
                settled_since: None,
                pointer_landing: None,
                helper_log: None,
                log_baseline,
                seen_pointer_lines: 0,
                results: Vec::new(),
                finished: false,
            },
            Task::batch([
                harness_hwnd(),
                harness_window_info(),
                window::latest().and_then(window::gain_focus),
            ]),
        )
    };

    iced::application(boot, update_pointer, view_pointer)
        .title(|_: &State| String::from("Adit RDP pointer acceptance — #233"))
        .theme(|state: &State| app_theme(&state.app))
        .subscription(|state: &State| {
            Subscription::batch([
                subscription(&state.app).map(NativeMessage::App),
                iced::time::every(std::time::Duration::from_millis(100)).map(|_| NativeMessage::Tick),
            ])
        })
        .window(window::Settings {
            size: iced::Size::new(WINDOW_WIDTH, WINDOW_HEIGHT),
            position: window::Position::Specific(iced::Point::new(
                WINDOW_POSITION.0,
                WINDOW_POSITION.1,
            )),
            ..Default::default()
        })
        .run()
}

/// Open the session, now that the window's real size is known.
///
/// The session is opened with the *viewport* size, exactly as the app does
/// when a user connects: the app asks for the pane's size in device pixels
/// and lets the server answer with whatever it wants.
///
/// The two-monitor layout is advertised at the same time, so the server
/// sizes its virtual desktop from the layout's bounding box (2400×800)
/// rather than from the viewport. That is the case under test: the pointer
/// has to land on the right monitor of a desktop whose size did not come
/// from the window.
fn open_session(state: &mut State) {
    let (rw, rh) = session_ops::rdp_viewport_size(&state.app);
    state
        .app
        .manager
        .open_live_rdp_session(
            state.profile_id,
            state.password.clone(),
            rw,
            rh,
        )
        .expect("acceptance RDP session");
    // The app records the size it asked for. That record is what the fit
    // factors compare the server's answer against, so it has to be set here
    // too — otherwise the app believes it is still transitioning and scales
    // the picture to fill the pane instead of drawing it as it is.
    state.app.rdp_target_size = (rw > 0).then_some((rw, rh));
    state.opened = true;
    state.step_started = Some(Instant::now());
}

/// Serve one step: drive the pointer, wait for the wire, measure.
fn step_pump(state: &mut State) -> Task<NativeMessage> {
    if state.finished || !state.opened {
        return Task::none();
    }
    if state.step.is_none() {
        // The first step waits for the picture: the pointer is only forwarded
        // once a frame's lease is accepted.
        if state
            .step_started
            .is_some_and(|started| started.elapsed() < Duration::from_millis(CONNECT_SETTLE_MS))
        {
            return Task::none();
        }
        state.step = Some(state.next_step);
        state.step_started = Some(Instant::now());
        // The landing belongs to the step that asked for it: a landing from
        // the previous step would be compared against this step's point and
        // make it look clamped.
        state.pointer_landing = None;
    }
    let Some(index) = state.step else {
        return Task::none();
    };
    let Some(&(label, target_x, target_y)) = POINTS.get(index) else {
        return Task::none();
    };
    let target = (target_x, target_y);
    let Some(geometry) = surface_geometry(&state.app) else {
        // No picture yet: the session is still settling.
        if state
            .step_started
            .is_some_and(|started| started.elapsed() > Duration::from_secs(60))
        {
            return finish(state, "no surface appeared");
        }
        return Task::none();
    };
    let Some(client) = client_point(&state.app, target) else {
        return Task::none();
    };
    // The wire coordinate the app *should* produce, for the point the
    // harness asked for.
    let expected = app_mapped_remote(&geometry, client);
    // The app forwards a pointer move only when the frame it belongs to is
    // still the live one: the mouse area carries the frame's lease, and a
    // lease of `None` means the picture is mid-swap between two sizes —
    // in which case `send_pointer_input` drops the move outright.
    //
    // So the harness waits for a lease to exist rather than assuming one.
    // The lease is also what makes a second, deliberate move safe: without
    // it the move is discarded and the step would time out for a reason
    // that has nothing to do with the mapping.
    let lease = state.app.rdp_input_epoch.is_some() && state.app.rdp_tiles_prev.is_empty();
    if !lease {
        return Task::none();
    }
    // The pointer is driven repeatedly while the step waits for the *first*
    // wire line.
    //
    // One move is not enough: the app drops a move whose frame lease is
    // stale, and the lease can go stale between the harness's decision to
    // drive and the app's handling of the move. Repeating turns a race into
    // a wait — and the repeat is harmless, because a repeated move to the
    // same point produces the same wire coordinate.
    //
    // Once a line *has* arrived the driving stops: more moves would keep
    // the log changing and the coordinate would never look settled. The
    // travel messages that a long jump produces all arrive first, so by
    // then the last of them is the destination.
    let arrived = state
        .helper_log
        .as_ref()
        .and_then(HelperLog::pointer_moves)
        .is_some_and(|(_, count)| count > state.seen_pointer_lines);
    let redrive = !arrived
        && (state.pointer_client.is_none_or(|previous| previous != client)
            || state
                .last_redrive
                .is_none_or(|at| at.elapsed() >= Duration::from_millis(REDRIVE_MS)));
    if redrive {
        let Some(hwnd) = state.hwnd else {
            return Task::none();
        };
        state.pointer_client = Some(client);
        state.last_redrive = Some(Instant::now());
        return drive_pointer(hwnd, client, state.scale_factor);
    }
    // The pointer is where it was asked to be. Now wait for the wire.
    //
    // The wire coordinate is only read once it has *settled*. Moving the
    // pointer a long way makes Windows emit a series of `WM_MOUSEMOVE`
    // messages as the pointer travels, and each one is a real move that
    // reaches the wire — so the first line in the log after a 1000-pixel
    // jump is an intermediate position, not the destination. Reading it early
    // would report a wrong coordinate for a mapping that is in fact exact.
    //
    // Settling means the coordinate has stopped changing: two reads a
    // moment apart agree. The line count is what says a *new* move
    // happened; the coordinate is what has to stop changing.
    let wire_read = state.helper_log.as_ref().and_then(HelperLog::pointer_moves);
    if let Some((wire, count)) = wire_read {
        if count > state.seen_pointer_lines {
            // A new move reached the wire. It may still be travelling.
            state.seen_pointer_lines = count;
            state.settled_wire = Some(wire);
            state.settled_since = None;
            return Task::none();
        }
        // No new line since the last read: if the coordinate has not moved
        // since the last read either, it has settled.
        if state.settled_wire == Some(wire) {
            let settled = state.settled_since.is_some_and(|at| {
                at.elapsed() >= Duration::from_millis(SETTLE_MS)
            });
            if settled {
                // What the app should have produced, for the point the
                // pointer *actually* reached.
                //
                // The landing is in physical pixels and the geometry is in
                // logical points, so it is converted through the window's
                // scale factor. When the landing matches the request this is
                // the same number as `expected`; when the pointer was
                // clamped it is different, and it is the *right* number to
                // compare against, because it is the point the app really
                // saw.
                let landing_client = state.pointer_landing.map(|landing| {
                    (
                        landing.client.0 as f32 / state.scale_factor,
                        landing.client.1 as f32 / state.scale_factor,
                    )
                });
                let expected_at_landing = landing_client
                    .map(|landed| app_mapped_remote(&geometry, landed))
                    .unwrap_or(expected);
                // The landing must also *be* the requested point, or the
                // step did not measure what it claims to measure.
                let landed_as_asked = landing_client.is_some_and(|landed| {
                    (landed.0 - client.0).abs() < 1.0 && (landed.1 - client.1).abs() < 1.0
                });
                let passes = wire == expected_at_landing && landed_as_asked;
                log(
                    state,
                    serde_json::json!({
                        "event": "step_measured",
                        "label": label,
                        "target": [target.0, target.1],
                        "client_point": [client.0, client.1],
                        "expected_wire": [expected.0, expected.1],
                        "expected_at_landing": [expected_at_landing.0, expected_at_landing.1],
                        "actual_wire": [wire.0, wire.1],
                        "landing": state.pointer_landing.map(|landing| serde_json::json!({
                            "client": [landing.client.0, landing.client.1],
                            "screen": [landing.screen.0, landing.screen.1],
                        })),
                        "landed_as_asked": landed_as_asked,
                        "surface": [geometry.surface.0, geometry.surface.1],
                        "scale": [geometry.scale_x, geometry.scale_y],
                        "fit": [geometry.fit.0, geometry.fit.1],
                        "window": [state.app.window_width, state.app.window_height],
                        "pass": passes,
                    }),
                );
                state.results.push(serde_json::json!({
                    "label": label,
                    "target": [target.0, target.1],
                    "expected": [expected.0, expected.1],
                    "expected_at_landing": [expected_at_landing.0, expected_at_landing.1],
                    "wire": [wire.0, wire.1],
                    "landed_as_asked": landed_as_asked,
                    "pass": passes,
                }));
                state.step = None;
                state.step_started = None;
                state.pointer_client = None;
                state.last_redrive = None;
                state.settled_wire = None;
                state.settled_since = None;
                state.pointer_landing = None;
                state.next_step += 1;
                return Task::none();
            }
            // The coordinate is steady but not yet for long enough.
            state.settled_since.get_or_insert_with(Instant::now);
            return Task::none();
        }
        // The coordinate changed without a new line — the log was read
        // mid-write. Read it again next tick.
        state.settled_wire = Some(wire);
        state.settled_since = None;
    }
    if state
        .step_started
        .is_some_and(|started| started.elapsed() > Duration::from_millis(STEP_TIMEOUT_MS))
    {
        let passes = false;
        log(
            state,
            serde_json::json!({
                "event": "step_measured",
                "label": label,
                "target": [target.0, target.1],
                "client_point": [client.0, client.1],
                "expected_wire": [expected.0, expected.1],
                "actual_wire": serde_json::Value::Null,
                "surface": [geometry.surface.0, geometry.surface.1],
                "scale": [geometry.scale_x, geometry.scale_y],
                "pass": passes,
                "note": "no pointer move reached the wire",
            }),
        );
        state.results.push(serde_json::json!({
            "label": label,
            "target": [target.0, target.1],
            "expected": [expected.0, expected.1],
            "wire": serde_json::Value::Null,
            "pass": passes,
        }));
        state.step = None;
        state.step_started = None;
        state.pointer_client = None;
        state.last_redrive = None;
        state.settled_wire = None;
        state.next_step += 1;
    }
    Task::none()
}

/// End the run and write the verdict.
fn finish(state: &mut State, reason: &str) -> Task<NativeMessage> {
    state.finished = true;
    let passed = state
        .results
        .iter()
        .filter(|result| result["pass"].as_bool() == Some(true))
        .count();
    let total = state.results.len();
    log(
        state,
        serde_json::json!({
            "event": "finished",
            "reason": reason,
            "passed": passed,
            "total": total,
            "results": state.results.clone(),
        }),
    );
    state
        .results
        .iter()
        .for_each(|result| println!("{result}"));
    println!("PASSED {passed}/{total}");
    // The window is closed once the run is over: the harness is a probe, and
    // a window left open would keep the session (and the helper) alive.
    window::latest().and_then(window::close)
}

fn update_pointer(state: &mut State, message: NativeMessage) -> Task<NativeMessage> {
    match message {
        NativeMessage::App(message) => {
            let resized = matches!(message, Message::WindowResized { .. });
            let task = update_loop::update(&mut state.app, message);
            if resized {
                // The window's real client area is now known, and the app's
                // own size is up to date with it.
                state.sized = true;
            }
            Task::batch([task.map(NativeMessage::App), step_pump(state)])
        }
        NativeMessage::Hwnd(hwnd) => {
            state.hwnd = hwnd;
            Task::none()
        }
        NativeMessage::WindowInfo(info) => {
            if let Some((_, scale)) = info {
                if scale.is_finite() && scale > 0.0 {
                    state.scale_factor = scale;
                }
            }
            Task::none()
        }
        NativeMessage::PointerMoved(landing) => {
            state.pointer_landing = landing;
            Task::none()
        }
        NativeMessage::Tick => {
            if state.finished {
                return Task::none();
            }
            // The session waits for the window's real size: the window frame
            // is not ours to predict, and a pane a few points narrower than
            // the remote desktop changes where the surface starts.
            if !state.opened {
                if state.sized && state.hwnd.is_some() {
                    // The app's display scale has to be the window's own
                    // before the viewport is computed, because the viewport
                    // is in device pixels.
                    state.app.display_scale = state.scale_factor;
                    open_session(state);
                }
                return Task::none();
            }
            // The helper's log is located once the helper has started, so
            // the harness reads only its own lines.
            if state.helper_log.is_none() {
                state.helper_log = locate_helper_log(state.log_baseline);
            }
            // The last step's wire coordinate has to be read after the step
            // has been measured, so the run ends one tick after the last
            // step.
            if state.results.len() == POINTS.len() {
                return finish(state, "all points measured");
            }
            step_pump(state)
        }
    }
}

fn view_pointer(state: &State) -> Element<'_, NativeMessage> {
    // Nothing is stacked on top of the app's own view: a widget of ours would
    // publish its own mouse interaction and would mask the very thing under
    // test.
    chrome::view(&state.app).map(NativeMessage::App)
}
