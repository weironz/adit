//! Opt-in native acceptance window. It reuses the shipping widgets/dispatcher,
//! but has isolated storage, synthetic localhost input and no clipboard access.
use super::*;
use std::io::Write;

struct State {
    app: AditApp,
    scale: f32,
    os_scale: f32,
    rescale_pending: bool,
    events: std::sync::mpsc::Sender<String>,
    geometry: Option<(u16, u16, u64)>,
    native_dpi: Option<u32>,
    pointer_sequence: u64,
}

#[derive(Debug, Clone)]
enum NativeMessage {
    App(Message),
    Scale(f32),
    Fit(bool),
    RemoteResize,
    NativeDpi(Option<u32>),
    NativePointerSnapshot(serde_json::Value),
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(windows)]
fn native_pointer_snapshot(
    window: window::Id,
    sequence: u64,
    remote: Point,
    requested: u128,
) -> Task<NativeMessage> {
    window::run(window, move |native| {
        use iced::window::raw_window_handle::RawWindowHandle;
        use windows::Win32::Foundation::{HWND, POINT};
        use windows::Win32::UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::{GetCursorPos, GetPhysicalCursorPos, GetWindowInfo, WINDOWINFO}};
        let mut observation = serde_json::json!({
            "event":"native_pointer_snapshot", "sequence":sequence,
            "remote_x":remote.x, "remote_y":remote.y,
            "requested_unix_ms":requested, "capture_started_unix_ms":unix_ms(),
            "scope":"asynchronous current cursor and own-window state; not the original OS event or physical-input proof",
            "atomic":false, "sampling":"sequential API reads; the pointer may move between reads",
            "apis":["GetCursorPos","GetPhysicalCursorPos","GetWindowInfo","GetDpiForWindow"],
        });
        let handle = native.window_handle().ok().map(|handle| handle.as_raw());
        if let Some(RawWindowHandle::Win32(handle)) = handle {
            // Use only the live handle supplied by iced; never retain a HWND.
            let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
            let mut cursor = POINT::default();
            let mut physical_cursor = POINT::default();
            let mut info = WINDOWINFO { cbSize: std::mem::size_of::<WINDOWINFO>() as u32, ..Default::default() };
            // These APIs observe current position/geometry and do not inject
            // input, change focus, or alter process/system DPI awareness.
            let cursor_result = unsafe { GetCursorPos(&mut cursor) };
            let physical_cursor_result = unsafe { GetPhysicalCursorPos(&mut physical_cursor) };
            let info_result = unsafe { GetWindowInfo(hwnd, &mut info) };
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            observation["cursor_screen"] = if cursor_result.is_ok() { serde_json::json!([cursor.x,cursor.y]) } else { serde_json::Value::Null };
            observation["physical_cursor_screen"] = if physical_cursor_result.is_ok() { serde_json::json!([physical_cursor.x,physical_cursor.y]) } else { serde_json::Value::Null };
            if info_result.is_ok() {
                observation["client_rect_screen"] = serde_json::json!([info.rcClient.left,info.rcClient.top,info.rcClient.right,info.rcClient.bottom]);
                observation["window_rect_screen"] = serde_json::json!([info.rcWindow.left,info.rcWindow.top,info.rcWindow.right,info.rcWindow.bottom]);
            }
            observation["cursor_api_ok"] = serde_json::json!(cursor_result.is_ok());
            observation["physical_cursor_api_ok"] = serde_json::json!(physical_cursor_result.is_ok());
            observation["window_info_api_ok"] = serde_json::json!(info_result.is_ok());
            observation["dpi"] = if dpi == 0 { serde_json::Value::Null } else { serde_json::json!(dpi) };
        } else {
            observation["unavailable"] = serde_json::json!("no live Win32 window handle");
        }
        observation["captured_unix_ms"] = serde_json::json!(unix_ms());
        observation
    }).map(NativeMessage::NativePointerSnapshot)
}

#[cfg(not(windows))]
fn native_pointer_snapshot(
    _window: window::Id,
    sequence: u64,
    remote: Point,
    requested: u128,
) -> Task<NativeMessage> {
    Task::done(NativeMessage::NativePointerSnapshot(serde_json::json!({
        "event":"native_pointer_snapshot", "sequence":sequence,
        "remote_x":remote.x, "remote_y":remote.y, "requested_unix_ms":requested,
        "scope":"asynchronous observation unavailable on this platform; not physical-input proof",
        "unavailable":"Win32 observation only",
    })))
}

#[cfg(windows)]
fn native_dpi(window: window::Id) -> Task<NativeMessage> {
    window::run(window, |native| {
        use iced::window::raw_window_handle::RawWindowHandle;
        let handle = native.window_handle().ok()?.as_raw();
        let RawWindowHandle::Win32(handle) = handle else {
            return None;
        };
        // Observe only our own live HWND; never change process or system DPI.
        let dpi = unsafe {
            windows::Win32::UI::HiDpi::GetDpiForWindow(windows::Win32::Foundation::HWND(
                handle.hwnd.get() as *mut std::ffi::c_void,
            ))
        };
        (dpi != 0).then_some(dpi)
    })
    .map(NativeMessage::NativeDpi)
}

#[cfg(not(windows))]
fn native_dpi(_window: window::Id) -> Task<NativeMessage> {
    Task::done(NativeMessage::NativeDpi(None))
}

fn log(state: &State, mut event: serde_json::Value) {
    event["unix_ms"] = serde_json::json!(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis());
    let _ = state.events.send(event.to_string());
}

fn update_native(state: &mut State, message: NativeMessage) -> Task<NativeMessage> {
    match message {
        NativeMessage::NativePointerSnapshot(mut observation) => {
            observation["completed_unix_ms"] = serde_json::json!(unix_ms());
            log(state, observation);
            Task::none()
        }
        NativeMessage::NativeDpi(dpi) => {
            state.native_dpi = dpi;
            log(
                state,
                serde_json::json!({"event":"native_dpi", "dpi":dpi,
                "scale":dpi.map(|v| v as f32 / 96.0), "api":"GetDpiForWindow"}),
            );
            Task::none()
        }
        NativeMessage::Scale(scale) => {
            state.scale = scale;
            state.rescale_pending = true;
            state.app.display_scale = state.os_scale * scale;
            state.app.rdp_target_size = None;
            log(
                state,
                serde_json::json!({"event":"app_scale", "app_scale":scale, "os_scale":state.os_scale}),
            );
            Task::none()
        }
        NativeMessage::Fit(value) => {
            state.app.vnc_fit = value;
            log(state, serde_json::json!({"event":"fit", "fit":value}));
            Task::none()
        }
        NativeMessage::RemoteResize => {
            let task = update(&mut state.app, Message::ToggleVncResize).map(NativeMessage::App);
            log(
                state,
                serde_json::json!({"event":"remote_resize_enabled",
                "enabled":state.app.manager.active_vnc_resize_remote()}),
            );
            task
        }
        NativeMessage::App(mut message) => {
            let mut pointer_probe = Task::none();
            match &mut message {
                Message::DisplayScale(scale) => {
                    state.os_scale = *scale;
                    *scale *= state.scale;
                }
                Message::RdpPointerMoved(point) => {
                    state.pointer_sequence = state.pointer_sequence.wrapping_add(1);
                    let sequence = state.pointer_sequence;
                    let remote = *point;
                    let requested = unix_ms();
                    pointer_probe = window::latest().and_then(move |id| {
                        native_pointer_snapshot(id, sequence, remote, requested)
                    });
                    log(
                        state,
                        serde_json::json!({
                            "event":"pointer", "remote_x":point.x, "remote_y":point.y,
                            "sequence":sequence, "snapshot_requested_unix_ms":requested,
                            "app_scale":state.scale, "os_scale":state.os_scale, "fit":state.app.vnc_fit,
                            "surface":state.app.rdp_surface_size,
                        }),
                    );
                }
                Message::RdpPressed(button) => log(
                    state,
                    serde_json::json!({"event":"button", "button":format!("{button:?}"), "app_scale":state.scale}),
                ),
                Message::WindowResized { width, height, .. } => log(
                    state,
                    serde_json::json!({"event":"window", "width":width, "height":height, "app_scale":state.scale}),
                ),
                _ => {}
            }
            let rescale = state.rescale_pending && matches!(message, Message::RdpTick);
            let dpi_probe = if let Message::WindowResized { window, .. } = &message {
                native_dpi(*window)
            } else {
                Task::none()
            };
            let task = Task::batch([
                update(&mut state.app, message).map(NativeMessage::App),
                dpi_probe,
                pointer_probe,
            ]);
            if let Some((width, height)) = state.app.rdp_surface_size {
                let generation = state
                    .app
                    .vnc_cursor
                    .as_ref()
                    .map_or(0, |cursor| cursor.generation);
                if state.geometry != Some((width, height, generation)) {
                    state.geometry = Some((width, height, generation));
                    let cursor = state.app.vnc_cursor.as_ref().map(|cursor| {
                        serde_json::json!({
                            "width":cursor.width,"height":cursor.height,
                            "hotspot_x":cursor.hotspot_x,"hotspot_y":cursor.hotspot_y,
                            "generation":cursor.generation,
                        })
                    });
                    log(
                        state,
                        serde_json::json!({"event":"geometry", "surface":[width,height],
                        "cursor":cursor, "app_scale":state.scale,"os_scale":state.os_scale,
                        "display_scale":state.app.display_scale}),
                    );
                }
            }
            if rescale {
                state.rescale_pending = false;
                Task::batch([
                    task,
                    window::latest().and_then(|id| {
                        window::size(id).map(move |size| {
                            NativeMessage::App(Message::WindowResized {
                                width: size.width,
                                height: size.height,
                                window: id,
                            })
                        })
                    }),
                ])
            } else {
                task
            }
        }
    }
}

fn view_native(state: &State) -> Element<'_, NativeMessage> {
    let controls = row![
        button("100%").on_press(NativeMessage::Scale(1.0)),
        button("150%").on_press(NativeMessage::Scale(1.5)),
        button("200%").on_press(NativeMessage::Scale(2.0)),
        button("Fit").on_press(NativeMessage::Fit(true)),
        button("1:1").on_press(NativeMessage::Fit(false)),
        button("Remote resize").on_press(NativeMessage::RemoteResize),
    ]
    .spacing(5)
    .align_y(Alignment::Center);
    stack(vec![
        view(&state.app).map(NativeMessage::App),
        iced::widget::column![Space::new().height(Fill), container(
            iced::widget::column![
                text("TEST app scale (OS DPI unchanged)").size(11),
                controls,
            ].spacing(3)
        ).padding(5).style(container::rounded_box)]
        .width(Fill)
        .height(Fill)
        .into(),
    ])
    .into()
}

pub fn run() -> iced::Result {
    // This entry point is available only to an explicit development feature.
    // Change the process override before the app reads any settings or vault.
    let scratch =
        std::env::temp_dir().join(format!("adit-vnc-native-acceptance-{}", std::process::id()));
    // A reused PID directory must not silently import any previous run's state.
    std::fs::create_dir(&scratch).expect("fresh isolated acceptance directory");
    std::env::set_var("ADIT_CONFIG_DIR", &scratch);
    let mut profile = ConnectionProfile::new("SYNTHETIC native acceptance", "127.0.0.1", 15915, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.security = VncSecurity::AllowNone;
    profile.vnc.clipboard_send = false;
    profile.vnc.clipboard_receive = false;
    profile.vnc.auto_reconnect = false;
    ProfileStore::default()
        .save_profiles(&[profile.clone()])
        .expect("synthetic profile");
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
            window_width: 1100.0,
            window_height: 760.0,
            ..AditApp::default()
        };
        app.manager.set_rdp_clipboard(false);
        app.manager
            .open_live_vnc_session(profile.id, String::new())
            .expect("isolated VNC fixture");
        (
            State {
                app,
                scale: 1.0,
                os_scale: 1.0,
                rescale_pending: false,
                events: events.clone(),
                geometry: None,
                native_dpi: None,
                pointer_sequence: 0,
            },
            window::latest().and_then(native_dpi),
        )
    };
    iced::application(boot, update_native, view_native)
        .title(|state: &State| {
            format!(
                "Adit SYNTHETIC acceptance — app {}% / OS {:.2} / native DPI {:?}",
                state.scale * 100.0,
                state.os_scale,
                state.native_dpi,
            )
        })
        .theme(|state: &State| app_theme(&state.app))
        .subscription(|state: &State| subscription(&state.app).map(NativeMessage::App))
        .scale_factor(|state: &State| state.scale)
        .window(window::Settings {
            size: iced::Size::new(1100.0, 760.0),
            ..Default::default()
        })
        .run()
}
