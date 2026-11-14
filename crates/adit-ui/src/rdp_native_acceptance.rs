//! Native GUI acceptance for #92 — the server's pointer shape.
//!
//! Three things have to be true at once for a remote desktop to look right:
//!
//! 1. the bitmap the server sent is the bitmap the app draws, at the hotspot
//!    the server announced;
//! 2. the local OS pointer is hidden while it is over the remote surface, so
//!    the two pointers do not sit on top of each other;
//! 3. the local OS pointer comes back when it leaves the remote surface.
//!
//! The compositor's screenshot carries the app's own widgets and nothing else
//! — the OS pointer is never in it — so (1) is measured against the PNG and
//! (2)/(3) have to be measured at the OS level.
//!
//! ## How the screenshot is compared
//!
//! The bitmap is drawn through iced's core image path (`Image::new(handle)`),
//! whose filter method defaults to **Linear**. The picture is therefore
//! *resampled*, not copied, and an exact pixel comparison would fail for the
//! wrong reason. The sampler is a wgpu linear sampler over an atlas that
//! replicates a one-texel border *outside* the sampled UV rectangle, so the
//! draw is equivalent to a bilinear resample of the source with edge
//! clamping. This harness resamples the source the same way and compares per
//! channel with a tolerance.
//!
//! The shape is identified by its **geometry** — the opaque-pixel count and
//! the opaque bounding box — and never by its colour: the same shape arrives
//! magenta on some dumps and white on others. The colour used for the
//! comparison is taken from the bitmap the app actually holds, so a colour
//! change on the server cannot fail the run.
//!
//! ## How the OS pointer is read
//!
//! iced_winit hides the pointer by publishing `mouse::Interaction::Hidden`
//! and calling winit's `set_cursor_visible(false)`, which is `ShowCursor(FALSE)`
//! on the window's own thread. `ShowCursor`'s counter is **per thread**, so it
//! cannot be read from another thread: the counter of a foreign thread says
//! nothing about this window's pointer. Worse, `ShowCursor` is not a query —
//! every call moves the counter — so reading it perturbs the very state under
//! test.
//!
//! The truth is `GetCursorInfo`'s flag byte, which is global. `CURSOR_SHOWING`
//! says the pointer is displayed at all: `ShowCursor(FALSE)` clears it and
//! `ShowCursor(TRUE)` sets it again, and the pointer's handle comes back NULL
//! while it is hidden. That is what this harness reads, and it never calls
//! `ShowCursor` itself.
//!
//! ## The fixture
//!
//! The shadow server only sends a pointer *bitmap* when the cursor image
//! changes: moving the pointer alone yields positions, never shapes. Each
//! shape step therefore asks the fixture for a different shape and waits for
//! the new bitmap to arrive before anything is measured. The fixture's
//! hotspots are facts rather than assumptions: the arrow's tip is at (2,2)
//! and the I-beam's stem is centred at (16,16); the xterm in the fixture owns
//! a 9×16 text cursor with its hotspot at (4,8).

use super::*;
use image::ImageEncoder;
use std::io::Write;

/// The fixture's X screen. The RDP viewport is requested at this size so the
/// picture is presented 1:1 and the pointer hotspot arithmetic runs at a
/// known scale.
const REMOTE_WIDTH: u16 = 1280;
const REMOTE_HEIGHT: u16 = 800;

/// The acceptance window's client area.
///
/// The chrome takes 36 (menu) + 34 (tab strip) + 28 (status bar) = 98
/// logical points and the hidden sidebar's reveal strip takes 16, so a pane
/// of exactly 1280×800 needs a client area of 1296×898. Anything else leaves
/// the picture resampled, and the hotspot arithmetic stops being an integer
/// number of pixels.
const WINDOW_WIDTH: f32 = 1296.0;
const WINDOW_HEIGHT: f32 = 898.0;

/// A smaller window, used for the scaled state.
///
/// The app renegotiates the remote desktop to its viewport, and the shadow
/// server refuses every size but its own 1280×800, so the picture stays at
/// 1280×800 and the app settles into its letterbox branch: the surface is
/// drawn smaller than 1:1 and the pointer bitmap is minified with it.
const SCALED_WINDOW_WIDTH: f32 = 800.0;
const SCALED_WINDOW_HEIGHT: f32 = 600.0;

/// How long the compositor gets to settle after the pointer has been moved
/// before the screenshot is taken.
const SETTLE_MS: u64 = 1500;

/// How long the scaled state gets: the app debounces a resize for 300 ms and
/// then treats the picture as "in transition" for two more seconds, and only
/// after that does it fall back to the letterbox branch.
const SCALED_SETTLE_MS: u64 = 5000;

/// How long a shape step waits for the server to send the new bitmap.
const SHAPE_TIMEOUT_MS: u64 = 10000;

/// How long the fixture is given to rebuild the cursor after a shape was
/// requested.
///
/// The fixture applies a shape within a tenth of a second; the pointer may
/// only move once the new cursor is in place, or the server sends the old
/// bitmap and the step measures the wrong shape.
const SHAPE_SETTLE_MS: u64 = 400;

/// How long a step waits for the server's bitmap before giving up on it.
///
/// A step that never sees a bitmap would otherwise hold the whole plan
/// forever, and the run would end on the harness's own timeout with no
/// verdict per step.
const STEP_TIMEOUT_MS: u64 = 30000;

/// How often the pointer is re-driven when the window is not under it.
///
/// Another application holding the foreground is a condition of the
/// machine, not of the product, and it may well persist; the retry limit
/// is what keeps a permanently covered window from looping forever.
const MAX_POINTER_RETRIES: u32 = 5;

/// How often the pointer is driven again because the server's cursor never
/// arrived.
///
/// The server sends a cursor only in response to a mouse move, so a step
/// that waits for one can only be rescued by moving again. The limit keeps
/// a server that never answers from holding the run open.
const MAX_SHAPE_RETRIES: u32 = 3;

/// The remote point that lands on the root window's background.
///
/// The fixture's xterm covers 100..704 x 100..504 and owns its own text
/// cursor, so the root background is the only place the fixture's own shape
/// is what the server reports.
///
/// Every step needs its own point, and no two of them may be the same:
/// `SetCursorPos` to the pointer's current position generates no
/// `WM_MOUSEMOVE`, so the app would never learn a position and the cursor
/// layer would have nothing to anchor the bitmap to. The two rounds of the
/// background sequence therefore need two different points each.
const BACKGROUND_POINT: (u16, u16) = (1000, 600);

/// The second round's background point.
const BACKGROUND_POINT_ALT: (u16, u16) = (1100, 680);

/// The first round's I-beam point.
const IBEAM_POINT: (u16, u16) = (900, 560);

/// The second round's I-beam point.
const IBEAM_POINT_ALT: (u16, u16) = (800, 680);

/// The xterm's top-left pixel, a known clickable control in the remote
/// picture. The pointer over it makes the server report the xterm's own
/// 9×16 text cursor, whose hotspot can be checked against a known place.
const XTERM_POINT: (u16, u16) = (100, 100);

/// The remote point the scaled step drives the pointer to.
///
/// It has to be clear of the xterm for the same reason the background points
/// are: over the xterm the server reports the xterm's own text cursor, not
/// the fixture's arrow.
const SCALED_POINT: (u16, u16) = (1050, 720);

/// The signature of a pointer bitmap, as measured on the wire.
///
/// The opaque-pixel count and the opaque bounding box identify a shape
/// without naming a colour: the arrow arrives magenta on some dumps and
/// white on others, but its geometry never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Signature {
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    /// The number of fully opaque pixels.
    opaque: usize,
    /// The opaque bounding box, inclusive.
    min_x: u16,
    min_y: u16,
    max_x: u16,
    max_y: u16,
}

impl Signature {
    /// Whether this bitmap is the shape that was asked for.
    ///
    /// The size and the hotspot have to match exactly; the opaque count and
    /// the bounding box are allowed a pixel of slack, because they are
    /// measurements of the server's own antialiasing rather than contract.
    fn is_shape(&self, expected: &ShapeKind) -> bool {
        let want = expected.signature();
        self.width == want.width
            && self.height == want.height
            && self.hotspot_x == want.hotspot_x
            && self.hotspot_y == want.hotspot_y
            && self.opaque.abs_diff(want.opaque) <= 4
            && self.min_x.abs_diff(want.min_x) <= 1
            && self.min_y.abs_diff(want.min_y) <= 1
            && self.max_x.abs_diff(want.max_x) <= 1
            && self.max_y.abs_diff(want.max_y) <= 1
    }
}

/// The pointer shapes the fixture's server is known to send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShapeKind {
    /// The fixture's own arrow.
    Arrow,
    /// The fixture's own I-beam.
    IBeam,
    /// The xterm's own text cursor, which the server reports when the pointer
    /// is over the xterm.
    Xterm,
}

impl ShapeKind {
    /// The name the fixture's control file uses, for the shapes it owns.
    ///
    /// The xterm's cursor needs no request: the server picks it by itself
    /// when the pointer is over the xterm.
    fn fixture_name(self) -> Option<&'static str> {
        match self {
            ShapeKind::Arrow => Some("arrow"),
            ShapeKind::IBeam => Some("ibeam"),
            ShapeKind::Xterm => None,
        }
    }

    /// The shape as measured on the wire.
    fn signature(self) -> Signature {
        match self {
            ShapeKind::Arrow => Signature {
                width: 32,
                height: 32,
                hotspot_x: 2,
                hotspot_y: 2,
                opaque: 272,
                min_x: 2,
                min_y: 3,
                max_x: 19,
                max_y: 28,
            },
            ShapeKind::IBeam => Signature {
                width: 32,
                height: 32,
                hotspot_x: 16,
                hotspot_y: 16,
                opaque: 128,
                min_x: 10,
                min_y: 8,
                max_x: 21,
                max_y: 23,
            },
            ShapeKind::Xterm => Signature {
                width: 9,
                height: 16,
                hotspot_x: 4,
                hotspot_y: 8,
                opaque: 86,
                min_x: 0,
                min_y: 0,
                max_x: 8,
                max_y: 15,
            },
        }
    }
}

/// One state the acceptance has to observe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// Over the remote surface, on the server's arrow.
    Arrow,
    /// Over the remote surface, on the server's I-beam.
    IBeam,
    /// Over the xterm's top-left pixel: the hotspot against a known control.
    XtermCursor,
    /// Over the collapsed toolbar's reveal zone, which sits over the remote
    /// surface.
    ToolbarZone,
    /// Outside the remote surface but still inside the app window.
    Outside,
    /// The arrow again, in a window small enough that the picture is scaled.
    ScaledArrow,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Arrow => "arrow",
            Step::IBeam => "ibeam",
            Step::XtermCursor => "xterm-cursor",
            Step::ToolbarZone => "toolbar-zone",
            Step::Outside => "outside",
            Step::ScaledArrow => "scaled-arrow",
        }
    }

    /// The shape the server is expected to be sending.
    fn expected_shape(self) -> ShapeKind {
        match self {
            Step::Arrow | Step::ToolbarZone | Step::Outside | Step::ScaledArrow => ShapeKind::Arrow,
            Step::IBeam => ShapeKind::IBeam,
            Step::XtermCursor => ShapeKind::Xterm,
        }
    }

    /// Whether the local OS pointer is expected to be visible here.
    fn expect_os_pointer_visible(self) -> bool {
        matches!(self, Step::Outside)
    }

    /// Whether the server's bitmap is expected to be visible in the
    /// screenshot.
    ///
    /// Over the collapsed toolbar's reveal zone the app's own chrome is
    /// painted on top of the picture, so the bitmap is drawn but hidden: what
    /// is checked there is that the zone does not swallow the pointer.
    fn expect_drawn(self) -> bool {
        matches!(
            self,
            Step::Arrow | Step::IBeam | Step::XtermCursor | Step::ScaledArrow
        )
    }

    /// Whether the step runs in the smaller window.
    fn scaled(self) -> bool {
        matches!(self, Step::ScaledArrow)
    }

    /// How long the step waits for the picture to settle.
    fn settle_ms(self) -> u64 {
        if self.scaled() {
            SCALED_SETTLE_MS
        } else {
            SETTLE_MS
        }
    }
}

/// The OS pointer's visibility, read on the window's own thread.
#[derive(Clone, Copy, Debug)]
struct PointerProbe {
    /// `GetCursorInfo`'s global `CURSOR_SHOWING` flag: the pointer is
    /// displayed at all.
    showing: bool,
    /// The global `CURSOR_SUPPRESSED` flag: the system is drawing the pointer
    /// itself instead of letting the application hide it. `ShowCursor(FALSE)`
    /// clears it, so a pointer the app hid is suppressed.
    suppressed: bool,
    /// Whether the pointer is over our own window, which is what decides
    /// whether the hidden flag is applied at all.
    over_window: bool,
}

impl PointerProbe {
    /// Whether the pointer is drawn for the user to see.
    ///
    /// `CURSOR_SHOWING` is the global "the pointer is displayed" flag, and
    /// `ShowCursor(FALSE)` — which is what winit calls to hide it — clears
    /// it.
    fn visible(&self) -> bool {
        self.showing
    }
}

/// Everything the pointer arithmetic needs, derived from the app's live
/// state rather than from assumed constants.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    /// The surface's top-left in window coordinates, in logical points.
    origin: (f32, f32),
    /// The surface's size in logical points.
    logical: (f32, f32),
    /// The per-axis scale from surface pixels to logical points.
    scale_x: f32,
    scale_y: f32,
    /// The fit factors the app settled on.
    fit: (f32, f32),
    /// The top of the workspace body, in window coordinates.
    body_top: f32,
    /// The width of the workspace's left edge offset.
    sidebar: f32,
    /// The pane's size in logical points.
    pane: (f32, f32),
}

impl Geometry {
    /// The scale the cursor layer uses for the bitmap: the desktop may
    /// stretch during a resize, but the cursor keeps its aspect.
    fn cursor_scale(&self) -> f32 {
        self.scale_x.max(self.scale_y)
    }
}

/// The app's own state, plus everything the harness needs to run its plan.
struct State {
    app: AditApp,
    events: std::sync::mpsc::Sender<String>,
    capture_dir: Option<std::path::PathBuf>,
    /// The window handle, needed to move the OS pointer to a client point.
    hwnd: Option<isize>,
    /// The window's own id, read at runtime: iced 0.14 has no `Id::MAIN`.
    window_id: Option<window::Id>,
    /// The window's scale factor, to turn logical client points into the
    /// physical pixels `SetCursorPos` takes.
    scale_factor: f32,
    /// The plan, in order.
    plan: Vec<(Step, (u16, u16))>,
    /// The step being served.
    step: Option<Step>,
    /// When the current step started.
    step_started: Option<Instant>,
    /// When the pointer was last driven for the current step.
    ///
    /// The settle clock runs from here, not from `step_started`: the settle
    /// window has to give the app time to *see* the move, and the move only
    /// happens when the pointer is driven.
    pointer_driven_at: Option<Instant>,
    /// How often the pointer has been re-driven for the current step.
    ///
    /// A re-drive is a retry of the harness's own precondition (the pointer
    /// was not over our window), not a second measurement of the step.
    pointer_retries: u32,
    /// How often the pointer has been driven again because the server's
    /// cursor never arrived.
    ///
    /// The server publishes a cursor only in response to a mouse move, so a
    /// missing cursor means the move did not reach it.
    shape_retries: u32,
    /// The client point the pointer was driven to, in logical points.
    pointer_client: Option<(f32, f32)>,
    /// The remote point the current step drives the pointer to.
    step_target: Option<(u16, u16)>,
    /// The screen point the pointer was last driven to.
    ///
    /// `SetCursorPos` to the pointer's *current* position is a no-op, and a
    /// no-op produces no `WM_MOUSEMOVE`: the app never learns a position, so
    /// it never forwards a move to the helper, and the server - which only
    /// re-sends a pointer bitmap when the cursor image changes - sends
    /// nothing. The first step of a run therefore has to move the pointer
    /// away from wherever the last run left it.
    pointer_screen: Option<(i32, i32)>,
    /// Whether the step has been given its own time slice.
    step_armed: bool,
    /// Whether the step's shape timeout has already been reported.
    ///
    /// The pump runs ten times a second, so an unreported timeout would be
    /// written thousands of times and the log would bury everything else.
    shape_timeout_logged: bool,
    /// The remote point the pointer was driven to, in surface pixels.
    expected_remote: Option<(u16, u16)>,
    /// The shape the fixture was last asked for, and when.
    shape_requested: Option<ShapeKind>,
    shape_requested_at: Option<Instant>,
    /// Whether the fixture has had time to apply the shape the step wants.
    ///
    /// The shape has to be applied before the pointer moves: the server
    /// sends a pointer bitmap when the cursor image changes, and only the
    /// client that moves the mouse gets the new one.
    shape_ready: bool,
    /// The OS pointer probe taken for the current step.
    probe: Option<PointerProbe>,
    /// The last sampled visibility of the OS pointer.
    ///
    /// The step's own probe is taken once, right before the screenshot; this
    /// one is sampled on every tick, so a pointer that appears and
    /// disappears between two steps is still visible in the log.
    visibility: Option<bool>,
    /// Whether the harness has asked for the foreground and been refused.
    ///
    /// A refused `SetForegroundWindow` means another application holds the
    /// foreground. The pointer cannot be measured against a window that is
    /// behind it, so the harness reports the condition instead of blaming
    /// the app for it.
    foreground_refused: bool,
    /// Whether a screenshot is in flight. More than one at a time would
    /// measure a step against a picture of a later state.
    capturing: bool,
    /// Whether the window has been shrunk for the scaled step.
    scaled_applied: bool,
    /// The measurements, one per step.
    results: Vec<serde_json::Value>,
    /// Set once the plan is finished.
    finished: bool,
}

#[derive(Debug, Clone)]
enum NativeMessage {
    App(Message),
    /// The compositor produced a screenshot of the acceptance window.
    Captured(window::Screenshot),
    /// The screenshot was written, or failed to be written.
    CaptureSaved(Result<(), String>),
    /// The window's own handle, read on the window's thread.
    Hwnd(Option<isize>),
    /// The window's own id and scale factor.
    WindowInfo(Option<(window::Id, f32)>),
    /// The OS pointer's visibility, read on the window's thread.
    Pointer(Option<PointerProbe>),
    /// The fixture's cursor shape was changed.
    ShapeSet(bool),
    /// The window was resized.
    Resized,
    /// The harness asked for the foreground; the answer says whether it was
    /// granted.
    Raised(bool),
    /// A periodic reading of the OS pointer's visibility.
    ///
    /// Only the Windows probe produces one: the reader is `GetCursorInfo`,
    /// which has no counterpart elsewhere. The variant still exists on every
    /// platform so the message type has one shape, and so the handler below
    /// needs no cfg of its own.
    #[cfg_attr(not(windows), allow(dead_code))]
    Visibility(bool),
    /// The OS pointer was moved to the requested client point, or the move
    /// failed. The screen point is reported so a failure is visible.
    PointerMoved(Option<(i32, i32)>),
    Tick,
}

fn log(state: &State, mut event: serde_json::Value) {
    event["unix_ms"] = serde_json::json!(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis());
    let _ = state.events.send(event.to_string());
}

/// The window's own handle and scale factor, read on the window's thread.
fn harness_window_info() -> Task<NativeMessage> {
    window::latest()
        .and_then(|id| {
            window::scale_factor(id).map(move |scale| (id, scale)).map(Some)
        })
        .map(NativeMessage::WindowInfo)
}

/// The window handle, read on the window's own thread.
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

/// The OS pointer's visibility, read on the window's own thread.
///
/// `GetCursorInfo` reports the pointer's flags globally, so the reading does
/// not depend on which thread asks — but it is taken on the window's thread
/// anyway, because that is where the pointer is hidden from and the reading
/// then describes the window's own state.
///
/// `ShowCursor` is deliberately never called: its counter is per thread, so
/// it says nothing about a pointer another thread hid, and every call moves
/// the counter — reading it perturbs the state under test.
#[cfg(windows)]
fn probe_pointer() -> Task<NativeMessage> {
    window::latest()
        .and_then(|id| {
            window::run(id, |native| {
                #[cfg(windows)]
                unsafe {
                    use iced::window::raw_window_handle::RawWindowHandle;
                    use windows::Win32::Foundation::POINT;
                    use windows::Win32::UI::WindowsAndMessaging::{
                        GetCursorInfo, WindowFromPoint, CURSORINFO, CURSORINFO_FLAGS, CURSOR_SHOWING,
                        CURSOR_SUPPRESSED, HCURSOR,
                    };
                    let Ok(handle) = native.window_handle() else {
                        return None;
                    };
                    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                        return None;
                    };
                    let hwnd = windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _);
                    // The cursor has to be over our own window for the hidden
                    // flag to mean anything: winit only hides the pointer
                    // while it is inside the client area.
                    let mut info = CURSORINFO {
                        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                        flags: CURSORINFO_FLAGS(0),
                        hCursor: HCURSOR(std::ptr::null_mut()),
                        ptScreenPos: POINT::default(),
                    };
                    let ok = GetCursorInfo(&mut info).is_ok();
                    let over_window = ok && WindowFromPoint(info.ptScreenPos) == hwnd;
                    // `CURSOR_SHOWING` is the global "the pointer is displayed"
                    // flag; `CURSOR_SUPPRESSED` is set while the system draws
                    // the pointer itself, which is what `ShowCursor(FALSE)`
                    // clears. A pointer the app hid is suppressed.
                    let showing = ok && info.flags.0 & CURSOR_SHOWING.0 != 0;
                    let suppressed = ok && info.flags.0 & CURSOR_SUPPRESSED.0 != 0;
                    Some(PointerProbe {
                        showing,
                        suppressed,
                        over_window,
                    })
                }
                #[cfg(not(windows))]
                {
                    let _ = native;
                    None
                }
            })
        })
        .map(NativeMessage::Pointer)
}

#[cfg(not(windows))]
fn probe_pointer() -> Task<NativeMessage> {
    Task::done(NativeMessage::Pointer(None))
}

/// The OS pointer's visibility, sampled on every tick.
///
/// The step's own probe is a single reading taken just before the
/// screenshot; this one watches continuously, so a pointer that the window
/// shows again between two steps - which the step's probe would never
/// catch - still leaves a trace.
#[cfg(windows)]
fn probe_visibility() -> Task<NativeMessage> {
    probe_pointer().map(|message| match message {
        NativeMessage::Pointer(Some(probe)) => NativeMessage::Visibility(probe.visible()),
        other => other,
    })
}

#[cfg(not(windows))]
fn probe_visibility() -> Task<NativeMessage> {
    Task::none()
}

/// Raise our own window and bring it to the foreground.
///
/// The pointer's position only matters to the app while the app's window is
/// under it: with another window on top, `SetCursorPos` moves the pointer
/// onto that window, the app receives no `WM_MOUSEMOVE`, and the step
/// measures a picture in which the app never saw the pointer at all.
///
/// The harness is not the foreground application - it is a test running
/// beside whatever the user is doing - so it cannot rely on staying on top.
/// It asks, and checks the answer: `SetForegroundWindow` is refused when
/// the calling process is not the foreground process, so the result is
/// reported rather than assumed.
#[cfg(windows)]
fn raise_window(hwnd: isize) -> Task<NativeMessage> {
    window::latest()
        .and_then(move |id| {
            window::run(id, move |_native| {
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::{
                        BringWindowToTop, SetForegroundWindow, SetWindowPos, HWND_TOP,
                        SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
                    };
                    let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut _);
                    // `SetWindowPos` with `HWND_TOP` puts the window above
                    // every other non-topmost window without asking for the
                    // foreground, which `SetForegroundWindow` alone often
                    // refuses to grant.
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOP),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                    );
                    let _ = BringWindowToTop(hwnd);
                    let foreground = SetForegroundWindow(hwnd);
                    Some(foreground.as_bool())
                }
                #[cfg(not(windows))]
                {
                    let _ = hwnd;
                    None
                }
            })
        })
        .map(|granted| NativeMessage::Raised(granted.unwrap_or(false)))
}

#[cfg(not(windows))]
fn raise_window(_hwnd: isize) -> Task<NativeMessage> {
    Task::done(NativeMessage::Raised(false))
}

/// Move the OS pointer to a client point of our own window.
///
/// The client point is in logical points; `ClientToScreen` turns it into a
/// screen point, which is what `SetCursorPos` takes. The conversion to
/// physical pixels happens here, because the window's scale factor is the
/// only thing that knows how many device pixels a logical point is.
#[cfg(windows)]
fn drive_pointer(
    hwnd: isize,
    client: (f32, f32),
    scale_factor: f32,
    from: Option<(i32, i32)>,
) -> Task<NativeMessage> {
    window::latest()
        .and_then(move |id| {
            // `window::run` runs the closure on the window's own thread.
            // The harness already knows the window handle, so it is passed
            // in rather than looked up again: the lookup inside the closure
            // would need the raw-window-handle dance again, and the handle
            // cannot change while the window is alive.
            window::run(id, move |_native| {
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::Foundation::{HWND, POINT};
                    use windows::Win32::Graphics::Gdi::ClientToScreen;
                    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

                    let hwnd = HWND(hwnd as *mut _);
                    let mut point = POINT {
                        x: (client.0 * scale_factor).round() as i32,
                        y: (client.1 * scale_factor).round() as i32,
                    };
                    if !ClientToScreen(hwnd, &mut point).as_bool() {
                        return None;
                    }
                    // The screen point comes back so a failed move shows up in
                    // the log. `SetCursorPos` fails when the pointer is locked
                    // to another display, and a step that then measured the
                    // wrong picture would look like a defect in the app.
                    let target = (point.x, point.y);
                    // Where the pointer is *now*, read rather than
                    // remembered: a previous run may have left it exactly
                    // where this step wants it, and a move to the pointer's
                    // current position is a no-op that produces no
                    // mouse-move event, so the app would never learn a
                    // position and the server would send no bitmap at all.
                    let mut current = POINT { x: 0, y: 0 };
                    let now = GetCursorPos(&mut current).is_ok();
                    let from = from.or(if now { Some((current.x, current.y)) } else { None });
                    if let Some((from_x, from_y)) = from {
                        if (from_x, from_y) == target {
                            // Step aside first, so that moving to the target
                            // is a real change.
                            let _ = SetCursorPos(from_x + 1, from_y + 1);
                        }
                    }
                    let _ = SetCursorPos(target.0, target.1);
                    Some(target)
                }
                #[cfg(not(windows))]
                {
                    let _ = (hwnd, client, scale_factor, from);
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
    _from: Option<(i32, i32)>,
) -> Task<NativeMessage> {
    Task::done(NativeMessage::PointerMoved(None))
}

/// Ask the fixture to show a different cursor shape.
///
/// The shadow server only sends a pointer *bitmap* when the cursor image
/// changes, so a step that wants a different shape has to make the server's
/// image change. The fixture reads its shape from a control file and applies
/// it within a tenth of a second.
///
/// The file lives in the fixture container, so the request is a `docker exec`
/// rather than a write: the harness must not depend on the fixture's
/// filesystem being reachable from here, and the container name is
/// configurable for the same reason.
///
/// The line carries a nonce. The fixture applies a shape when the whole line
/// differs from the one it applied last, so a repeated request for the same
/// shape would otherwise be ignored — and the request has to rebuild the
/// cursor, because it is the rebuild that makes the server send a new bitmap.
fn request_fixture_shape(shape: ShapeKind) -> Task<NativeMessage> {
    let container = std::env::var("ADIT_RDP_FIXTURE_CONTAINER")
        .unwrap_or_else(|_| "adit-shadow-live".into());
    let Some(name) = shape.fixture_name() else {
        return Task::done(NativeMessage::ShapeSet(false));
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Task::perform(
        async move {
            #[cfg(windows)]
            {
                let mut command = std::process::Command::new("docker");
                // Git Bash rewrites anything that looks like a path; the
                // control file's path has to reach the container unchanged.
                command.env("MSYS_NO_PATHCONV", "1");
                // CREATE_NO_WINDOW: `docker` is a console program, and a
                // console window flashing under the pointer makes winit
                // report the pointer as having left the app window. iced
                // then has no cursor position, so the app stops drawing the
                // server's bitmap and the local pointer comes back - a
                // defect of the harness, measured as a defect of the app.
                {
                    use std::os::windows::process::CommandExt;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    command.creation_flags(CREATE_NO_WINDOW);
                }
                command.args(["exec", container.as_str(), "sh", "-c"]);
                command.arg(format!("printf '%s %s\\n' {name} {nonce} > /tmp/cursor.cmd"));
                match command.output() {
                    Ok(result) if result.status.success() => true,
                    Ok(result) => {
                        let stderr = String::from_utf8_lossy(&result.stderr);
                        eprintln!("fixture shape {name} failed: {stderr}");
                        false
                    }
                    Err(error) => {
                        eprintln!("fixture shape {name} could not be requested: {error}");
                        false
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = (container, name, nonce);
                false
            }
        },
        NativeMessage::ShapeSet,
    )
}

/// The surface's geometry, read from the app's live state.
///
/// Nothing here is assumed: the surface size, the fit factors and the display
/// scale all come from the app, because the app renegotiates the remote
/// desktop to the viewport and a stale assumption would measure the pointer
/// against the wrong picture.
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
        logical: (logical_w, logical_h),
        scale_x,
        scale_y,
        fit: (fit_x, fit_y),
        body_top,
        sidebar,
        pane: (pane_w, pane_h),
    })
}

/// The client point the pointer is driven to for a step.
///
/// Inside the surface, the remote point is mapped through the surface's
/// per-axis scale; outside it, the status bar is used, which carries text
/// only and therefore publishes no mouse interaction of its own.
fn client_point(app: &AditApp, step: Step, target: (u16, u16)) -> Option<(f32, f32)> {
    let geometry = surface_geometry(app)?;
    if let Step::Outside = step {
        // The status bar: the bottom row of the window, past the surface
        // and clear of the sidebar's reveal strip.
        return Some((
            geometry.sidebar + 40.0,
            app.window_height - STATUS_BAR_HEIGHT / 2.0,
        ));
    }
    let (rx, ry) = target;
    Some((
        geometry.origin.0 + f32::from(rx) / geometry.scale_x,
        geometry.origin.1 + f32::from(ry) / geometry.scale_y,
    ))
}

/// The client point over the collapsed toolbar's reveal zone.
///
/// The zone is a fixed 96×20 box centred at the top of the workspace body,
/// so it sits over the remote surface. It is the one place where the app's
/// own chrome covers the picture, and it used to swallow the pointer there.
fn toolbar_zone_point(app: &AditApp) -> Option<(f32, f32)> {
    let geometry = surface_geometry(app)?;
    Some((
        geometry.sidebar + geometry.pane.0 / 2.0,
        geometry.body_top + RDP_TOOLBAR_REVEAL_HEIGHT / 2.0,
    ))
}

/// The signature of the bitmap the app is holding, from its pixels.
fn signature_of(width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, rgba: &[u8]) -> Signature {
    let mut opaque = 0usize;
    let mut min_x = u16::MAX;
    let mut min_y = u16::MAX;
    let mut max_x = 0u16;
    let mut max_y = 0u16;
    for y in 0..height {
        for x in 0..width {
            let offset = (usize::from(y) * usize::from(width) + usize::from(x)) * 4;
            if offset + 4 > rgba.len() {
                continue;
            }
            if rgba[offset + 3] == 0 {
                continue;
            }
            opaque += 1;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if opaque == 0 {
        min_x = 0;
        min_y = 0;
        max_x = 0;
        max_y = 0;
    }
    Signature {
        width,
        height,
        hotspot_x,
        hotspot_y,
        opaque,
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

/// The bitmap the app is about to draw, with its signature.
fn current_bitmap(app: &AditApp) -> Option<(Signature, Vec<u8>)> {
    let id = app.manager.active_session()?;
    let (width, height, hotspot_x, hotspot_y, _generation) =
        desktop_panes::cursor_visual_summary(app, id)?;
    let (_w, _h, rgba) = desktop_panes::cursor_visual_rgba(app, id)?;
    Some((
        signature_of(width, height, hotspot_x, hotspot_y, &rgba),
        rgba,
    ))
}

/// The server's pointer position, in viewport pixels.
///
/// This is the server's own report, not the app's echo of the local pointer:
/// the two agree only while nothing is warped.
fn server_position(app: &AditApp) -> Option<(u16, u16)> {
    let id = app.manager.active_session()?;
    app.manager.session_rdp_cursor(id)?.position
}

/// One bilinear sample of the source bitmap, with edge clamping.
///
/// The texel coordinate is in the same units as the atlas's UV rectangle:
/// texel `i` spans `[i, i+1)` and its centre is at `i + 0.5`. The wgpu
/// sampler is linear and the atlas replicates the edge texels outside the
/// sampled rectangle, which together make this the sampling the compositor
/// actually performs.
fn sample_bilinear(src: &[u8], sw: usize, sh: usize, tx: f32, ty: f32) -> [f32; 4] {
    let texel = |x: usize, y: usize| -> [f32; 4] {
        let offset = (y * sw + x) * 4;
        [
            src[offset] as f32,
            src[offset + 1] as f32,
            src[offset + 2] as f32,
            src[offset + 3] as f32,
        ]
    };
    let fx = tx - 0.5;
    let fy = ty - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let ax = fx - x0;
    let ay = fy - y0;
    let clamp_x = |v: f32| (v as i32).clamp(0, sw as i32 - 1) as usize;
    let clamp_y = |v: f32| (v as i32).clamp(0, sh as i32 - 1) as usize;
    let (x0i, x1i) = (clamp_x(x0), clamp_x(x0 + 1.0));
    let (y0i, y1i) = (clamp_y(y0), clamp_y(y0 + 1.0));
    let p00 = texel(x0i, y0i);
    let p10 = texel(x1i, y0i);
    let p01 = texel(x0i, y1i);
    let p11 = texel(x1i, y1i);
    let mut out = [0.0f32; 4];
    for channel in 0..4 {
        let top = p00[channel] + (p10[channel] - p00[channel]) * ax;
        let bottom = p01[channel] + (p11[channel] - p01[channel]) * ax;
        out[channel] = top + (bottom - top) * ay;
    }
    out
}

/// How well a bitmap matches the screenshot at one position.
#[derive(Clone, Copy, Debug, Default)]
struct MatchReport {
    /// The fraction of the bitmap's opaque pixels the screenshot agrees with.
    ratio: f32,
    /// The mean per-channel error over those pixels.
    mean_error: f32,
    /// The largest per-channel error over those pixels.
    max_error: u8,
    /// How many device pixels the bitmap covers with fully opaque colour.
    opaque_pixels: usize,
    /// How many of them the screenshot agrees with.
    matched_pixels: usize,
}

/// The frame the bitmap has to be found in: the compositor's output.
struct Frame<'a> {
    screenshot: &'a [u8],
    shot_width: u32,
    shot_height: u32,
}

/// The bitmap being looked for, and how the compositor draws it.
struct Bitmap<'a> {
    width: u16,
    height: u16,
    rgba: &'a [u8],
    /// The server's hotspot, in source pixels.
    hotspot_x: u16,
    hotspot_y: u16,
    /// The cursor layer's scale: the bitmap is drawn at `source / cursor_scale`
    /// logical points.
    cursor_scale: f32,
    /// Device pixels per logical point.
    compositor_scale: f32,
    /// How far a channel may differ before a pixel counts as a mismatch.
    tolerance: u8,
}

/// Write the server's raw pointer bitmap next to the capture.
///
/// The file is the bitmap's RGBA pixels verbatim, so the archived audit can
/// recompute the opaque-pixel count, the bounding box and the match against
/// the screenshot without knowing anything about Adit.
fn write_bitmap(state: &State, label: &str, signature: &Signature, rgba: &[u8]) {
    let Some(dir) = state.capture_dir.clone() else {
        return;
    };
    // The capture that goes with this bitmap is written after the step's
    // result is pushed, so its index is one higher than the count here.
    let index = state.results.len() + 1;
    let name = format!(
        "bitmap-{index:02}-{label}-{}x{}.rgba",
        signature.width, signature.height,
    );
    let path = dir.join(name);
    if let Err(error) = std::fs::write(&path, rgba) {
        log(
            state,
            serde_json::json!({"event":"bitmap_write_failed","error":error.to_string()}),
        );
    }
}

/// Where the bitmap was drawn, and how well it matches there.
struct BitmapMatch {
    /// The corner it matched at, when it matched where it was expected.
    found_at: Option<(i64, i64)>,
    /// How the expected corner compares with the screenshot.
    report: MatchReport,
    /// The best corner in the neighbourhood, if any.
    best_corner: Option<(i64, i64)>,
    /// How well the bitmap matches at `best_corner`.
    best_ratio: f32,
}

/// Compare the bitmap against the screenshot at a device-pixel corner.
///
/// `corner_x`/`corner_y` are the bitmap's top-left in device pixels. The
/// bitmap is resampled the way the compositor resamples it, and only the
/// fully opaque device pixels are compared: at the shape's edge the
/// compositor blends the shape with the picture behind it, and that picture
/// is not known here.
fn match_at(
    frame: &Frame<'_>,
    bitmap: &Bitmap<'_>,
    corner_x: f32,
    corner_y: f32,
) -> MatchReport {
    let mut report = MatchReport::default();
    if bitmap.rgba.is_empty() || bitmap.width == 0 || bitmap.height == 0 {
        return report;
    }
    let (sw, sh) = (usize::from(bitmap.width), usize::from(bitmap.height));
    // Device pixels per source texel.
    //
    // The cursor layer draws the bitmap at `source / cursor_scale` logical
    // points (`rdp_cursor_bounds`), and a logical point is `compositor_scale`
    // device pixels. One source texel therefore covers
    // `compositor_scale / cursor_scale` device pixels, and the drawn picture
    // is `source * compositor_scale / cursor_scale` device pixels across.
    //
    // The cursor keeps its aspect while the desktop stretches, so the drawn
    // size follows the same `scale = max(sx, sy)` the layer uses.
    let per_texel = bitmap.compositor_scale / bitmap.cursor_scale;
    let width_dev = f32::from(bitmap.width) * per_texel;
    let height_dev = f32::from(bitmap.height) * per_texel;
    let x_start = corner_x.floor() as i64;
    let y_start = corner_y.floor() as i64;
    let x_end = (corner_x + width_dev).ceil() as i64;
    let y_end = (corner_y + height_dev).ceil() as i64;
    let mut error_sum = 0.0f32;
    for y in y_start..y_end {
        for x in x_start..x_end {
            if x < 0 || y < 0 || x >= i64::from(frame.shot_width) || y >= i64::from(frame.shot_height) {
                continue;
            }
            let tx = (x as f32 + 0.5 - corner_x) / per_texel;
            let ty = (y as f32 + 0.5 - corner_y) / per_texel;
            let sampled = sample_bilinear(bitmap.rgba, sw, sh, tx, ty);
            if sampled[3] < 250.0 {
                // Not fully opaque: the compositor blends it with the picture
                // behind it, which this harness does not know.
                continue;
            }
            report.opaque_pixels += 1;
            let offset = (y as usize * frame.shot_width as usize + x as usize) * 4;
            if offset + 4 > frame.screenshot.len() {
                continue;
            }
            let mut worst = 0u8;
            for (channel, sample) in sampled.iter().enumerate().take(3) {
                let expected = sample.round().clamp(0.0, 255.0) as i32;
                let actual = i32::from(frame.screenshot[offset + channel]);
                worst = worst.max(expected.abs_diff(actual) as u8);
            }
            error_sum += f32::from(worst);
            report.max_error = report.max_error.max(worst);
            if worst <= bitmap.tolerance {
                report.matched_pixels += 1;
            }
        }
    }
    if report.opaque_pixels > 0 {
        report.ratio = report.matched_pixels as f32 / report.opaque_pixels as f32;
        report.mean_error = error_sum / report.opaque_pixels as f32;
    }
    report
}

/// Look for the bitmap in the screenshot.
///
/// The expected corner is derived from the pointer's client point and the
/// hotspot the server announced. When the bitmap does not match there, the
/// neighbourhood is searched, so a misplaced bitmap is reported as misplaced
/// rather than as absent.
fn find_bitmap(
    frame: &Frame<'_>,
    bitmap: &Bitmap<'_>,
    pointer_client: (f32, f32),
) -> BitmapMatch {
    // The cursor layer draws the bitmap so that the hotspot lands on the
    // pointer: the corner is the pointer minus the hotspot, divided by the
    // cursor's scale.
    let corner_logical = (
        pointer_client.0 - f32::from(bitmap.hotspot_x) / bitmap.cursor_scale,
        pointer_client.1 - f32::from(bitmap.hotspot_y) / bitmap.cursor_scale,
    );
    let corner = (
        corner_logical.0 * bitmap.compositor_scale,
        corner_logical.1 * bitmap.compositor_scale,
    );
    let at_expected = match_at(frame, bitmap, corner.0, corner.1);
    if at_expected.ratio >= 0.9 {
        return BitmapMatch {
            found_at: Some((corner.0.round() as i64, corner.1.round() as i64)),
            report: at_expected,
            best_corner: None,
            best_ratio: at_expected.ratio,
        };
    }
    // The bitmap is somewhere else, or nowhere: look around the expected
    // corner. A coarse stride keeps this cheap enough to run on every step.
    let mut best: Option<(i64, i64)> = None;
    let mut best_ratio = 0.0f32;
    for dy in -45i64..=45 {
        for dx in -45i64..=45 {
            if dx % 3 != 0 || dy % 3 != 0 {
                continue;
            }
            let report = match_at(
                frame,
                bitmap,
                corner.0 + dx as f32,
                corner.1 + dy as f32,
            );
            if report.ratio > best_ratio {
                best_ratio = report.ratio;
                best = Some((dx, dy));
            }
        }
    }
    let best_corner = best.map(|(dx, dy)| {
        (
            (corner.0 + dx as f32).round() as i64,
            (corner.1 + dy as f32).round() as i64,
        )
    });
    BitmapMatch {
        found_at: None,
        report: at_expected,
        best_corner,
        best_ratio,
    }
}

/// Measure one step against the screenshot and the OS pointer probe.
fn measure(
    state: &State,
    screenshot: &window::Screenshot,
    probe: Option<PointerProbe>,
) -> serde_json::Value {
    let step = state.step.unwrap_or(Step::Arrow);
    let shot_width = screenshot.size.width;
    let shot_height = screenshot.size.height;
    let pixels = &screenshot.rgba;
    let compositor_scale = screenshot.scale_factor;

    let geometry = surface_geometry(&state.app);
    let surface = state.app.rdp_surface_size.unwrap_or((0, 0));

    let (signature, rgba) = current_bitmap(&state.app).unwrap_or_else(|| {
        (
            Signature {
                width: 0,
                height: 0,
                hotspot_x: 0,
                hotspot_y: 0,
                opaque: 0,
                min_x: 0,
                min_y: 0,
                max_x: 0,
                max_y: 0,
            },
            Vec::new(),
        )
    });

    let mut drawn_at = serde_json::Value::Null;
    let mut misplaced = None;
    if let (Some(geometry), Some(pointer_client)) = (geometry, state.pointer_client) {
        // A minified bitmap is aliased by the sampler, so the comparison has
        // to be more forgiving there than at 1:1.
        let scaled = geometry.cursor_scale() > 1.05;
        let tolerance = if scaled { 96 } else { 24 };
        let ratio_floor = if scaled { 0.45 } else { 0.9 };
        let frame = Frame {
            screenshot: pixels,
            shot_width,
            shot_height,
        };
        let bitmap = Bitmap {
            width: signature.width,
            height: signature.height,
            rgba: &rgba,
            hotspot_x: signature.hotspot_x,
            hotspot_y: signature.hotspot_y,
            cursor_scale: geometry.cursor_scale(),
            compositor_scale,
            tolerance,
        };
        let found = find_bitmap(&frame, &bitmap, pointer_client);
        misplaced = found.best_corner;
        write_bitmap(state, step.label(), &signature, &rgba);
        if found.found_at.is_some() || found.best_ratio >= ratio_floor {
            drawn_at = serde_json::json!({
                "expected_corner": [
                    ((pointer_client.0 - f32::from(signature.hotspot_x) / geometry.cursor_scale())
                        * compositor_scale).round() as i64,
                    ((pointer_client.1 - f32::from(signature.hotspot_y) / geometry.cursor_scale())
                        * compositor_scale).round() as i64,
                ],
                "found_at": found.found_at,
                "best_corner": found.best_corner,
                "ratio": found.report.ratio,
                "best_ratio": found.best_ratio,
                "opaque_pixels": found.report.opaque_pixels,
                "matched_pixels": found.report.matched_pixels,
                "mean_error": found.report.mean_error,
                "max_error": found.report.max_error,
                "tolerance": tolerance,
            });
        }
    }

    let probe_ok = probe.is_some();
    let os_visible = probe.map(|p| p.visible()).unwrap_or(false);
    let over_window = probe.map(|p| p.over_window).unwrap_or(false);
    let expect_visible = step.expect_os_pointer_visible();
    let expect_drawn = step.expect_drawn();

    // Over the remote surface the server owns the pointer: the app must draw
    // the server's bitmap and the local pointer must be hidden. Outside it,
    // the local pointer must be back.
    //
    // The toolbar zone is over the surface but the app's own chrome is painted
    // on top of it, so the bitmap cannot be seen there — what is checked is
    // that the zone does not swallow the pointer: the local pointer stays
    // hidden and the app still holds the server's bitmap.
    let shape_ok = signature.is_shape(&step.expected_shape());
    let drawn_ok = if expect_drawn {
        drawn_at
            .get("ratio")
            .and_then(|r| r.as_f64())
            .map(|r| r >= if geometry.map(|g| g.cursor_scale() > 1.05).unwrap_or(false) {
                0.45
            } else {
                0.9
            })
            .unwrap_or(false)
    } else {
        true
    };
    // The pointer has to be where it was driven to, and that position has to
    // map onto the remote point the step asked for.
    //
    // The server's own report is only corroboration: the shadow server skips
    // `lastMouseClient` in its pointer-position update, so the client that
    // moved the mouse never receives a position back. Requiring the report
    // would make the check unsatisfiable by the fixture's design rather than
    // by the product's behaviour.
    let position_ok = match (state.expected_remote, state.pointer_client, geometry) {
        (Some((rx, ry)), Some((cx, cy)), Some(geometry)) => {
            let mapped = (
                ((cx - geometry.origin.0) * geometry.scale_x).round(),
                ((cy - geometry.origin.1) * geometry.scale_y).round(),
            );
            (mapped.0 - f32::from(rx)).abs() <= 1.0 && (mapped.1 - f32::from(ry)).abs() <= 1.0
        }
        // The outside step is not over the remote surface, so there is no
        // remote position to agree with.
        (None, _, _) => true,
        _ => false,
    };

    // The app must have learned the pointer's new position. The cursor layer
    // anchors the bitmap to the position the app knows about, so a move the
    // app never saw leaves the local pointer on screen and the bitmap
    // undrawn - which is exactly what a step that repeats the previous
    // point would produce.
    let app_saw_move = match state.pointer_client {
        Some((cx, cy)) => {
            (state.app.cursor_pos.x - cx).abs() <= 4.0
                && (state.app.cursor_pos.y - cy).abs() <= 4.0
        }
        None => false,
    };

    let pass = probe_ok
        && over_window
        && os_visible == expect_visible
        && shape_ok
        && position_ok
        && app_saw_move
        && drawn_ok;

    let status = state
        .app
        .manager
        .active_session()
        .and_then(|id| state.app.manager.session_summary(id))
        .map(|summary| format!("{:?}", summary.status));

    serde_json::json!({
        "step": step.label(),
        "session_status": status,
        "surface": [surface.0, surface.1],
        "surface_origin": geometry.map(|g| [g.origin.0, g.origin.1]),
        "surface_logical_size": geometry.map(|g| [g.logical.0, g.logical.1]),
        "pane": geometry.map(|g| [g.pane.0, g.pane.1]),
        "fit_factors": geometry.map(|g| [g.fit.0, g.fit.1]),
        "scale": geometry.map(|g| [g.scale_x, g.scale_y]),
        "cursor_scale": geometry.map(|g| g.cursor_scale()),
        "compositor_scale": compositor_scale,
        "screenshot_pixels": [shot_width, shot_height],
        "window": [state.app.window_width, state.app.window_height],
        "window_scale_factor": state.scale_factor,
        "display_scale": state.app.display_scale,
        "rdp_target_size": state.app.rdp_target_size.map(|(w, h)| [w, h]),
        "pointer_client": state.pointer_client.map(|(x, y)| [x, y]),
        "app_cursor_pos": [state.app.cursor_pos.x, state.app.cursor_pos.y],
        "toolbar_collapsed": state.app.rdp_toolbar_collapsed,
        "toolbar_hovered": state.app.rdp_toolbar_hovered,
        "expected_remote": state.expected_remote.map(|(x, y)| [x, y]),
        "server_position": server_position(&state.app).map(|(x, y)| [x, y]),
        "server_position_corrobates": server_position(&state.app)
            .zip(state.expected_remote)
            .map(|((sx, sy), (rx, ry))| {
                i32::from(sx) == i32::from(rx) && i32::from(sy) == i32::from(ry)
            }),
        "position_matches": position_ok,
        "app_saw_pointer_move": app_saw_move,
        "os_pointer_probe_ok": probe_ok,
        "os_pointer_over_window": over_window,
        "os_pointer_showing": probe.map(|p| p.showing),
        "os_pointer_suppressed": probe.map(|p| p.suppressed),
        "os_pointer_visible": os_visible,
        "expect_os_pointer_visible": expect_visible,
        "expect_server_bitmap_drawn": expect_drawn,
        "expected_shape": step.expected_shape().fixture_name().unwrap_or("xterm"),
        "server_bitmap": {
            "width": signature.width,
            "height": signature.height,
            "hotspot": [signature.hotspot_x, signature.hotspot_y],
            "opaque_pixels": signature.opaque,
            "bbox": [signature.min_x, signature.min_y, signature.max_x, signature.max_y],
            "is_expected_shape": shape_ok,
        },
        "server_bitmap_match": drawn_at,
        "server_bitmap_misplaced_at": misplaced,
        "pass": pass,
    })
}

/// Write a compositor screenshot as a PNG.
fn write_capture(
    state: &mut State,
    screenshot: window::Screenshot,
    label: &str,
) -> Task<NativeMessage> {
    let Some(dir) = state.capture_dir.clone() else {
        return Task::none();
    };
    if !screenshot.scale_factor.is_finite()
        || screenshot.scale_factor <= 0.0
        || screenshot.size.width == 0
        || screenshot.size.height == 0
    {
        return Task::done(NativeMessage::CaptureSaved(Err(
            "invalid native screenshot geometry".into(),
        )));
    }
    let index = state.results.len();
    let name = format!("capture-{index:02}-{label}.png");
    let path = dir.join(&name);
    let rgba = screenshot.rgba.clone();
    let (width, height) = (screenshot.size.width, screenshot.size.height);
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|error| error.to_string())?;
                let mut writer = std::io::BufWriter::new(file);
                image::codecs::png::PngEncoder::new(&mut writer)
                    .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
                    .map_err(|error| error.to_string())?;
                writer.flush().map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
        },
        NativeMessage::CaptureSaved,
    )
}

/// Advance the current step.
///
/// The phases are deliberately separate: the pointer has to be in place
/// before the shape is asked for, because the server reports the *xterm's*
/// cursor over the xterm and the fixture's own shape everywhere else. The
/// shape then has to have arrived before anything is measured, and the
/// picture has to have settled before the screenshot is taken.
fn step_pump(state: &mut State) -> Task<NativeMessage> {
    let Some(step) = state.step else {
        // No step is active: start the next one.
        if state.plan.is_empty() {
            if !state.finished {
                state.finished = true;
                let passed = state
                    .results
                    .iter()
                    .filter(|r| r.get("pass").and_then(|p| p.as_bool()) == Some(true))
                    .count();
                log(
                    state,
                    serde_json::json!({"event":"acceptance_finished",
                        "steps":state.results.len(),"passed":passed,
                        "results":state.results}),
                );
            }
            return iced::exit();
        }
        let (step, remote_target) = state.plan.remove(0);
        state.step = Some(step);
        state.step_started = Some(Instant::now());
        state.pointer_driven_at = None;
        state.pointer_retries = 0;
        state.foreground_refused = false;
        state.pointer_client = None;
        state.step_target = Some(remote_target);
        state.expected_remote = None;
        state.step_armed = false;
        state.shape_timeout_logged = false;
        state.probe = None;
        state.shape_requested = None;
        state.shape_requested_at = None;
        state.shape_ready = false;
        log(
            state,
            serde_json::json!({"event":"step_start","step":step.label(),
                "target":[remote_target.0, remote_target.1]}),
        );
        if step.scaled() {
            // Shrink the window: the app renegotiates the remote desktop to
            // its viewport, the shadow server refuses, and the app settles
            // into its letterbox branch.
            state.scaled_applied = true;
            log(
                state,
                serde_json::json!({"event":"window_resize_requested",
                    "width":SCALED_WINDOW_WIDTH,"height":SCALED_WINDOW_HEIGHT}),
            );
            return match state.window_id {
                Some(id) => window::resize(id, iced::Size::new(SCALED_WINDOW_WIDTH, SCALED_WINDOW_HEIGHT))
                    .map(|_: ()| NativeMessage::Resized),
                None => Task::none(),
            };
        }
        return Task::none();
    };

    // The session has to be up, and the picture has to be on screen.
    let status = state
        .app
        .manager
        .active_session()
        .and_then(|id| state.app.manager.session_summary(id))
        .map(|summary| summary.status);
    let connected = matches!(status, Some(SessionStatus::Connected));
    if !connected {
        return Task::none();
    }
    let ready = state.app.rdp_surface_size.is_some() && !state.app.rdp_tiles.is_empty();
    if !ready || state.app.rdp_resize_pending.is_some() {
        return Task::none();
    }

    // The app forwards a pointer move only when the frame it was built from
    // carries a valid lease, and the view withholds that lease while the
    // outgoing tiles are still on screen.
    //
    // The server publishes a cursor only in response to a mouse move, so a
    // move made without a lease never reaches it: the server sends no
    // cursor, the app keeps the cursor it got at connect time, and the
    // step waits for a picture that is not coming. The lease is the
    // harness's own precondition, so it waits for it rather than measuring
    // a step that could not pass.
    let leased = state.app.rdp_input_epoch.is_some() && state.app.rdp_tiles_prev.is_empty();
    if !leased {
        return Task::none();
    }

    // The scaled state needs the renegotiation to have been sent and to have
    // settled: the app treats the picture as "in transition" for two seconds
    // after the request goes out, and only then falls back to the letterbox.
    if step.scaled() {
        let settled = state
            .app
            .rdp_resize_requested_at
            .is_some_and(|at| at.elapsed() >= std::time::Duration::from_millis(2500));
        if !settled {
            return Task::none();
        }
    }

    // Where the pointer has to be for this step.
    let Some(remote_target) = state.step_target else {
        return Task::none();
    };

    // A step gets one time slice. A step that never sees the server's
    // bitmap has to be abandoned, or it holds the whole plan and the run
    // ends without a verdict for any step.
    if !state.step_armed
        && state.step_started.is_some_and(|started| {
            started.elapsed() >= std::time::Duration::from_millis(STEP_TIMEOUT_MS)
        })
    {
        log(
            state,
            serde_json::json!({"event":"step_timeout","step":step.label(),
                "target":[remote_target.0, remote_target.1]}),
        );
        state.step = None;
        state.step_started = None;
        state.pointer_driven_at = None;
        state.pointer_retries = 0;
        state.pointer_client = None;
        state.step_target = None;
        state.expected_remote = None;
        state.step_armed = true;
        state.shape_requested = None;
        state.shape_requested_at = None;
        state.shape_ready = false;
        return Task::none();
    }
    let target = match step {
        Step::ToolbarZone => toolbar_zone_point(&state.app),
        other => client_point(&state.app, other, remote_target),
    };
    let Some(target) = target else {
        return Task::none();
    };

    // The server's shape has to be the one this step asks for, and it has
    // to be in place BEFORE the pointer moves.
    //
    // The shadow server sends a pointer bitmap when the cursor image
    // changes, and the client that moved the mouse is the one that gets
    // the new bitmap. Asking for the shape after the pointer has already
    // been parked means no further mouse move follows the shape change, so
    // the server never sends the new bitmap at all - and the step waits for
    // a picture that is never going to arrive.
    let expected = step.expected_shape();
    if !state.shape_ready {
        if let Some(name) = expected.fixture_name() {
            let stale = state
                .shape_requested_at
                .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1));
            if state.shape_requested != Some(expected) || stale {
                state.shape_requested = Some(expected);
                state.shape_requested_at = Some(Instant::now());
                log(
                    state,
                    serde_json::json!({"event":"shape_requested","step":step.label(),
                        "shape":name}),
                );
                return request_fixture_shape(expected);
            }
            // The fixture rebuilds the cursor within a tenth of a second;
            // asking for it again before it had a chance would be a second
            // rebuild, not a second chance.
            let applied = state
                .shape_requested_at
                .is_some_and(|at| at.elapsed() >= std::time::Duration::from_millis(SHAPE_SETTLE_MS));
            if !applied {
                return Task::none();
            }
        }
        // The shape is in place. Now the pointer may move.
        state.shape_ready = true;
    }

    if state.pointer_client != Some(target) {
        state.pointer_client = Some(target);
        log(
            state,
            serde_json::json!({"event":"step_lease","step":step.label(),
                "lease":state.app.rdp_input_epoch,
                "surface":state.app.rdp_surface_size,
                "tiles":state.app.rdp_tiles.len(),
                "prev_tiles":state.app.rdp_tiles_prev.len()}),
        );
        state.expected_remote = match step {
            // The zone is over the surface, so it has a remote point too.
            Step::ToolbarZone => surface_geometry(&state.app).map(|geometry| {
                (
                    ((target.0 - geometry.origin.0) * geometry.scale_x).round() as u16,
                    ((target.1 - geometry.origin.1) * geometry.scale_y).round() as u16,
                )
            }),
            // Outside the surface there is no remote point at all.
            Step::Outside => None,
            _ => Some(remote_target),
        };
        let Some(hwnd) = state.hwnd else {
            return Task::none();
        };
        log(
            state,
            serde_json::json!({"event":"pointer_moved","step":step.label(),
                "client":[target.0, target.1]}),
        );
        // The window has to be up before the pointer is moved onto it: with
        // another window on top the app never sees the move at all.
        let scale_factor = state.scale_factor;
        let pointer_screen = state.pointer_screen;
        return raise_window(hwnd)
            .then(move |_| drive_pointer(hwnd, target, scale_factor, pointer_screen));
    }

    // The app has to be holding the shape this step asks for. The bitmap
    // only arrives after the pointer has moved with the new shape in place,
    // so this waits for the app to have caught up.
    let signature = current_bitmap(&state.app).map(|(signature, _)| signature);
    if !signature.as_ref().is_some_and(|s| s.is_shape(&expected)) {
        let waited = state.step_started.is_some_and(|started| {
            started.elapsed() >= std::time::Duration::from_millis(SHAPE_TIMEOUT_MS)
        });
        if waited && !state.shape_timeout_logged {
            state.shape_timeout_logged = true;
            log(
                state,
                serde_json::json!({"event":"shape_timeout","step":step.label(),
                    "shape":expected.fixture_name().unwrap_or("xterm"),
                    "have":signature.map(|s| serde_json::json!({
                        "width":s.width,"height":s.height,
                        "hotspot":[s.hotspot_x,s.hotspot_y],
                        "opaque_pixels":s.opaque,
                        "bbox":[s.min_x,s.min_y,s.max_x,s.max_y]}))}),
            );
            // The server's cursor comes with a mouse move and with nothing
            // else, so a cursor that never arrived means the move never
            // reached it. The pointer is driven again - from a different
            // point, so the move is a real one - and the server answers a
            // second time.
            if state.shape_retries < MAX_SHAPE_RETRIES {
                state.shape_retries += 1;
                state.shape_timeout_logged = false;
                state.pointer_client = None;
                state.pointer_driven_at = None;
                log(
                    state,
                    serde_json::json!({"event":"shape_retry","step":step.label(),
                        "retry":state.shape_retries}),
                );
            }
        }
        return Task::none();
    }

    // The settle clock runs from the moment the pointer was driven, not
    // from the start of the step: the wait exists to give the app time to
    // see the move and the server time to send the bitmap, and neither can
    // happen before the pointer is there.
    let settled = state.pointer_driven_at.is_some_and(|driven| {
        driven.elapsed() >= std::time::Duration::from_millis(step.settle_ms())
    });
    if !settled {
        return Task::none();
    }

    if state.probe.is_none() {
        // The probe comes first and is waited for: it is the reading of the
        // OS pointer's visibility, and the screenshot that follows has to
        // describe the same instant.
        return probe_pointer();
    }

    // The pointer has to be over our own window for anything the app does
    // with it to mean anything. If it is not, another window is on top, the
    // app was never told where the pointer is, and the step cannot say
    // anything about the product - so the harness puts its window back up
    // and drives the pointer again instead of measuring a failure that is
    // its own.
    if !state.probe.as_ref().is_some_and(|p| p.over_window) {
        if state.pointer_retries >= MAX_POINTER_RETRIES {
            log(
                state,
                serde_json::json!({"event":"pointer_not_over_window",
                    "step":step.label(),
                    "retries":state.pointer_retries,
                    "foreground_refused":state.foreground_refused}),
            );
            state.step = None;
            state.step_started = None;
            state.pointer_client = None;
            state.pointer_driven_at = None;
            state.step_target = None;
            state.expected_remote = None;
            state.probe = None;
            state.shape_requested = None;
            state.shape_requested_at = None;
            state.shape_ready = false;
            return Task::none();
        }
        state.pointer_retries += 1;
        log(
            state,
            serde_json::json!({"event":"pointer_not_over_window","step":step.label(),
                "retry":state.pointer_retries,
                "foreground_refused":state.foreground_refused}),
        );
        // The pointer is somewhere else on the screen, so the drive has to
        // start from there and step aside from the target.
        state.pointer_client = None;
        state.pointer_screen = None;
        state.probe = None;
        let Some(hwnd) = state.hwnd else {
            return Task::none();
        };
        return raise_window(hwnd);
    }

    if state.capturing {
        return Task::none();
    }
    state.capturing = true;
    window::latest().and_then(|id| window::screenshot(id).map(NativeMessage::Captured))
}

fn update_native(state: &mut State, message: NativeMessage) -> Task<NativeMessage> {
    match message {
        NativeMessage::App(message) => {
            let task = update_loop::update(&mut state.app, message);
            Task::batch([task.map(NativeMessage::App), step_pump(state)])
        }
        NativeMessage::Captured(screenshot) => {
            state.capturing = false;
            let probe = state.probe;
            let measurement = measure(state, &screenshot, probe);
            let label = state.step.map(|s| s.label()).unwrap_or("step");
            log(
                state,
                serde_json::json!({"event":"step_measured","measurement":measurement}),
            );
            state.results.push(measurement);
            // The step is measured; retire it and arm the next one.
            state.step = None;
            state.step_started = None;
            state.pointer_driven_at = None;
            state.pointer_retries = 0;
            state.pointer_client = None;
            state.step_target = None;
            state.expected_remote = None;
            state.probe = None;
            state.shape_requested = None;
            state.shape_requested_at = None;
            state.shape_ready = false;
            write_capture(state, screenshot, label)
        }
        NativeMessage::CaptureSaved(result) => {
            match result {
                Ok(()) => log(state, serde_json::json!({"event":"capture_saved"})),
                Err(error) => log(
                    state,
                    serde_json::json!({"event":"capture_failed","error":error}),
                ),
            }
            Task::none()
        }
        NativeMessage::Hwnd(hwnd) => {
            state.hwnd = hwnd;
            log(state, serde_json::json!({"event":"hwnd","hwnd":hwnd}));
            Task::none()
        }
        NativeMessage::WindowInfo(info) => {
            if let Some((id, scale)) = info {
                state.window_id = Some(id);
                state.scale_factor = if scale.is_finite() && scale > 0.0 {
                    scale
                } else {
                    1.0
                };
            }
            log(
                state,
                serde_json::json!({"event":"window_info",
                    "scale_factor":state.scale_factor}),
            );
            Task::none()
        }
        NativeMessage::Pointer(probe) => {
            state.probe = probe;
            Task::none()
        }
        NativeMessage::ShapeSet(ok) => {
            log(state, serde_json::json!({"event":"shape_set","ok":ok}));
            Task::none()
        }
        NativeMessage::Raised(granted) => {
            if !granted {
                // Another application holds the foreground. The window is
                // still raised above the others, but the pointer's input goes
                // to the foreground window, so the app cannot be measured
                // until the foreground is ours.
                state.foreground_refused = true;
                log(state, serde_json::json!({"event":"foreground_refused"}));
            }
            Task::none()
        }
        NativeMessage::Resized => {
            log(state, serde_json::json!({"event":"window_resized"}));
            Task::none()
        }
        NativeMessage::PointerMoved(screen) => {
            log(
                state,
                serde_json::json!({"event":"pointer_moved_os","screen":screen}),
            );
            if let Some((x, y)) = screen {
                state.pointer_screen = Some((x, y));
            }
            // The pointer is where it was asked to be; the settle clock
            // starts now.
            state.pointer_driven_at = Some(Instant::now());
            Task::none()
        }
        NativeMessage::Visibility(visible) => {
            if state.visibility != Some(visible) {
                log(
                    state,
                    serde_json::json!({"event":"os_pointer_visibility",
                        "visible":visible,"step":state.step.map(|s| s.label())}),
                );
                state.visibility = Some(visible);
            }
            Task::none()
        }
        NativeMessage::Tick => Task::batch([probe_visibility(), step_pump(state)]),
    }
}

fn view_native(state: &State) -> Element<'_, NativeMessage> {
    // Nothing is stacked on top of the app's own view: a widget of ours would
    // publish its own mouse interaction, and `Pointer` outranks `Hidden`, so
    // the very thing under test would be masked by the test.
    chrome::view(&state.app).map(NativeMessage::App)
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

pub fn run() -> iced::Result {
    // This entry point is available only to an explicit development feature.
    // The configuration directory is replaced before anything reads settings
    // or the credential store, so the real one is never touched.
    let scratch =
        std::env::temp_dir().join(format!("adit-rdp-native-acceptance-{}", std::process::id()));
    std::fs::create_dir(&scratch).expect("fresh isolated acceptance directory");
    std::env::set_var("ADIT_CONFIG_DIR", &scratch);

    // The helper is named explicitly: the root workspace's `target` holds a
    // stale copy from before the pointer work, and the helper is a separate
    // workspace that has to be built on its own.
    let helper = std::env::var("ADIT_RDP_HELPER").ok().map(std::path::PathBuf::from).unwrap_or_else(
        || {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/adit-rdp/target/release/adit-rdp-host.exe")
        },
    );
    if helper.exists() {
        std::env::set_var("ADIT_RDP_HOST", &helper);
    }

    let capture_dir = match std::env::var("ADIT_NATIVE_CAPTURE_DIR") {
        Ok(path) if !path.is_empty() => {
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).expect("capture directory");
            Some(path)
        }
        _ => None,
    };
    let host = std::env::var("ADIT_RDP_FIXTURE_HOST").unwrap_or_else(|_| "127.0.0.1:13394".into());
    let (host_name, port) = host.rsplit_once(':').unwrap_or((host.as_str(), "3389"));
    let port: u16 = port.parse().expect("fixture port");
    let username = std::env::var("ADIT_RDP_FIXTURE_USER").unwrap_or_else(|_| "ADIT\\fixture".into());
    let password = read_password();

    let mut profile = ConnectionProfile::new("RDP native acceptance", host_name, port, username);
    profile.protocol = Protocol::Rdp;
    profile.auth_method = AuthMethod::Password;
    // The profile has to exist before the app is built, or the session layer
    // will not find it.
    ProfileStore::default()
        .save_profiles(&[profile.clone()])
        .expect("fixture profile");

    let (events, rx) = std::sync::mpsc::channel::<String>();
    let event_path = capture_dir
        .clone()
        .unwrap_or_else(|| scratch.clone())
        .join("events.jsonl");
    std::thread::spawn(move || {
        let mut file = std::fs::File::create(event_path).expect("acceptance event log");
        for line in rx {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    });

    // Two rounds over the surface, so a pointer that only works the first
    // time is caught, and the shape the server sends has to change between
    // them — the fixture switches it while the app is running. The xterm
    // step checks the hotspot against a known place in the picture, the
    // toolbar zone and the outside step are the two edges of the surface,
    // and the last step repeats the arrow in a window small enough that the
    // picture is scaled.
    let plan = vec![
        (Step::Arrow, BACKGROUND_POINT),
        (Step::IBeam, IBEAM_POINT),
        (Step::Arrow, BACKGROUND_POINT_ALT),
        (Step::IBeam, IBEAM_POINT_ALT),
        (Step::XtermCursor, XTERM_POINT),
        // The pointer is driven to the zone's own place, which is over the
        // picture; the remote point here is only a placeholder.
        (Step::ToolbarZone, XTERM_POINT),
        (Step::Outside, BACKGROUND_POINT),
        (Step::ScaledArrow, SCALED_POINT),
    ];

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
        app.manager
            .open_live_rdp_session(profile.id, password.clone(), REMOTE_WIDTH, REMOTE_HEIGHT)
            .expect("isolated RDP fixture");
        (
            State {
                app,
                events: events.clone(),
                capture_dir: capture_dir.clone(),
                hwnd: None,
                window_id: None,
                scale_factor: 1.0,
                plan: plan.clone(),
                step: None,
                step_started: None,
                pointer_driven_at: None,
                pointer_retries: 0,
                shape_retries: 0,
                shape_ready: false,
                foreground_refused: false,
                pointer_client: None,
                step_target: None,
                pointer_screen: None,
                step_armed: false,
                shape_timeout_logged: false,
                expected_remote: None,
                shape_requested: None,
                shape_requested_at: None,
                probe: None,
                visibility: None,
                capturing: false,
                scaled_applied: false,
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

    iced::application(boot, update_native, view_native)
        .title(|_: &State| String::from("Adit RDP native acceptance — #92"))
        .theme(|state: &State| app_theme(&state.app))
        .subscription(|state: &State| {
            Subscription::batch([
                subscription(&state.app).map(NativeMessage::App),
                iced::time::every(std::time::Duration::from_millis(100)).map(|_| NativeMessage::Tick),
            ])
        })
        .window(window::Settings {
            size: iced::Size::new(WINDOW_WIDTH, WINDOW_HEIGHT),
            ..Default::default()
        })
        .run()
}
