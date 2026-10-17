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
}

#[derive(Debug, Clone)]
enum NativeMessage {
    App(Message),
    Scale(f32),
    Fit(bool),
    RemoteResize,
}

fn log(state: &State, event: serde_json::Value) {
    let _ = state.events.send(event.to_string());
}

fn update_native(state: &mut State, message: NativeMessage) -> Task<NativeMessage> {
    match message {
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
            update(&mut state.app, Message::ToggleVncResize).map(NativeMessage::App)
        }
        NativeMessage::App(mut message) => {
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
            let task = update(&mut state.app, message).map(NativeMessage::App);
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
    std::fs::create_dir_all(&scratch).expect("isolated acceptance directory");
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
            },
            Task::none(),
        )
    };
    iced::application(boot, update_native, view_native)
        .title(|state: &State| {
            format!(
                "Adit SYNTHETIC acceptance — app {}% / OS {:.2}",
                state.scale * 100.0,
                state.os_scale
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
