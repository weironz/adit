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
    hook_diagnostics: Option<(bool, bool, [u32; 5])>,
    hook_health: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
enum NativeMessage {
    App(Message),
    Scale(f32),
    Fit(bool),
    RemoteResize,
    NativeDpi(Option<u32>),
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
            let toggling_grab = matches!(message, Message::ToggleVncGrab);
            #[cfg(windows)]
            if matches!(message, Message::ToggleVncGrab) {
                // The hook must be installed on a thread which pumps Win32
                // messages. Observe the real dispatcher rather than assuming
                // iced's update callback runs on its native window thread.
                let (dispatcher, foreground) = unsafe {
                    (
                        windows::Win32::System::Threading::GetCurrentThreadId(),
                        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                            windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow(),
                            None,
                        ),
                    )
                };
                log(state, serde_json::json!({"event":"hook_dispatcher", "dispatcher_thread":dispatcher,
                    "foreground_window_thread":foreground, "grab_before":state.app.vnc_grab_keys}));
            }
            match &mut message {
                Message::DisplayScale(scale) => {
                    state.os_scale = *scale;
                    *scale *= state.scale;
                }
                Message::RdpPointerMoved(point) => log(
                    state,
                    serde_json::json!({
                        "event":"pointer", "remote_x":point.x, "remote_y":point.y,
                        "app_scale":state.scale, "os_scale":state.os_scale, "fit":state.app.vnc_fit,
                        "surface":state.app.rdp_surface_size,
                    }),
                ),
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
            ]);
            if toggling_grab {
                log(state, serde_json::json!({"event":"grab_state", "enabled":state.app.vnc_grab_keys}));
                #[cfg(windows)]
                if state.app.vnc_grab_keys
                    && std::env::var("ADIT_NATIVE_HOOK_SELFTEST").as_deref() == Ok("1")
                {
                    let events = state.events.clone();
                    std::thread::spawn(move || {
                        use windows::Win32::UI::Input::KeyboardAndMouse::*;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let inputs = [false, true].map(|up| INPUT {
                            r#type: INPUT_KEYBOARD,
                            Anonymous: INPUT_0 { ki: KEYBDINPUT {
                                wVk: VK_F24,
                                dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                                ..Default::default()
                            } },
                        });
                        let accepted = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
                        let _ = events.send(serde_json::json!({
                            "event":"hook_selftest", "injected_f24_accepted":accepted,
                            "unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default().as_millis()
                        }).to_string());
                    });
                }
            }
            let hook = keyboard_hook::diagnostics();
            if state.hook_diagnostics != Some(hook) {
                state.hook_diagnostics = Some(hook);
                log(state, serde_json::json!({"event":"hook_diagnostics", "armed":hook.0,
                    "vnc":hook.1,"counters":hook.2,
                    "worker_thread":keyboard_hook::worker_thread_id()}));
            }
            let health = keyboard_hook::worker_health();
            let health_snapshot = (health.0 / 20, health.1);
            if state.hook_health != Some(health_snapshot) {
                state.hook_health = Some(health_snapshot);
                log(state, serde_json::json!({"event":"hook_pump", "turns":health.0,
                    "exit_reason":health.1,"worker_thread":keyboard_hook::worker_thread_id()}));
            }
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
    profile.vnc.resize_remote = std::env::var("ADIT_NATIVE_RESIZE").as_deref() == Ok("1");
    let scale: f32 = std::env::var("ADIT_NATIVE_SCALE")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .expect("acceptance scale");
    assert!(
        matches!(scale, 1.0 | 1.5 | 2.0),
        "acceptance scale must be 1/1.5/2"
    );
    let fit = std::env::var("ADIT_NATIVE_FIT").as_deref() != Ok("0");
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
            vnc_fit: fit,
            display_scale: scale,
            ..AditApp::default()
        };
        app.manager.set_rdp_clipboard(false);
        app.manager
            .open_live_vnc_session(profile.id, String::new())
            .expect("isolated VNC fixture");
        (
            State {
                app,
                scale,
                os_scale: 1.0,
                rescale_pending: false,
                events: events.clone(),
                geometry: None,
                native_dpi: None,
                hook_diagnostics: None,
                hook_health: None,
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
