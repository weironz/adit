//! Real iced widgets + the shipping AccessKit/UIA bridge. All transports,
//! credential migration, auth dialogs and clipboard operations are gated.
use super::*;
use serde_json::{json, Value};
use std::{error::Error, fs, path::PathBuf, sync::{Arc, Mutex}};

mod client;

const PASSWORD: &str = "SYNTHETIC-UIA-PASSWORD-DO-NOT-EXPOSE";
const PROXY: &str = "SYNTHETIC-UIA-PROXY-TOKEN-DO-NOT-EXPOSE";
type Outcome = Arc<Mutex<Option<Result<Value, String>>>>;

struct Harness {
    app: AditApp,
    window: Option<window::Id>,
    started: Instant,
    client_started: bool,
    attached: bool,
    protected_seeded: bool,
    owner_ids: Vec<SessionId>,
    outcome: Outcome,
}

#[derive(Clone, Debug)]
enum Event {
    App(Message),
    Pulse,
    Finished(Result<Value, String>),
}

fn allowed(message: &Message) -> bool {
    matches!(message,
        Message::Tick | Message::WindowResized { .. } | Message::DisplayScale(_)
        | Message::AccessibilityReady { .. } | Message::AccessibilityClosed(_)
        | Message::AccessibilitySelectProfile(_) | Message::AccessibilityActivateSession(_)
        | Message::AccessibilityScroll { .. } | Message::ShowMainView(_)
        | Message::AccessibilityScrollLines { .. }
        | Message::AccessibilityFocus { .. }
        | Message::EditProfileFromContext(_) | Message::CloseProfileEditor | Message::SaveProfile
        | Message::ProfileNameChanged(_) | Message::ProfileHostChanged(_) | Message::ProfilePortChanged(_)
        | Message::ProfileUsernameChanged(_) | Message::ProfileGroupChanged(_)
    )
}

fn finish(state: &mut Harness, result: Result<Value, String>) -> Task<Event> {
    *state.outcome.lock().unwrap() = Some(result);
    if let Some(window) = state.window {
        state.app.accessibility.closed(window);
        window::close(window)
    } else { iced::exit() }
}

fn protected_unchanged(state: &Harness) -> bool {
    !state.protected_seeded || state.app.profile_editor.is_none()
        || (state.app.profile_password == PASSWORD
            && state.app.profile_proxy_program == r"C:\SYNTHETIC\proxy.exe"
            && state.app.profile_proxy_args == format!("[\"{PROXY}\"]"))
}

fn update(state: &mut Harness, event: Event) -> Task<Event> {
    if state.started.elapsed() > Duration::from_secs(90) {
        return finish(state, Err("UIA acceptance watchdog expired".into()));
    }
    match event {
        Event::App(message) => {
            if let Message::WindowResized { window, .. } = message { state.window = Some(window); }
            if let Message::AccessibilityReady { attached, .. } = message { state.attached = attached; }
            if !allowed(&message) { return Task::none(); }
            let task = update_loop::update(&mut state.app, message).map(Event::App);
            if state.app.profile_editor.is_some() {
                if !state.protected_seeded {
                    state.app.profile_password = PASSWORD.into();
                    state.app.profile_proxy_program = r"C:\SYNTHETIC\proxy.exe".into();
                    state.app.profile_proxy_args = format!("[\"{PROXY}\"]");
                    state.app.profile_proxy_enabled = false;
                    state.app.profile_advanced_open = true;
                    state.protected_seeded = true;
                } else if !protected_unchanged(state) {
                    return finish(state, Err("protected UIA action modified an actual sensitive draft".into()));
                }
            }
            task
        }
        Event::Pulse => {
            let task = update_loop::update(&mut state.app, Message::Tick).map(Event::App);
            if !protected_unchanged(state) {
                return finish(state, Err("protected UIA action modified an actual sensitive draft".into()));
            }
            if !state.client_started && state.attached {
                if let Some(window) = state.window {
                    state.client_started = true;
                    let owners = state.owner_ids.clone();
                    return Task::batch([task, window::run(window, |window| {
                        use iced::window::raw_window_handle::RawWindowHandle;
                        match window.window_handle().map(|h| h.as_raw()) {
                            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
                            _ => 0,
                        }
                    }).then(move |hwnd| {
                        let owners = owners.clone();
                        Task::perform(async move {
                            tokio::task::spawn_blocking(move || client::run(hwnd, &owners))
                                .await.map_err(|_| "UIA client thread ended".to_owned()).and_then(|v| v)
                        }, Event::Finished)
                    })]);
                }
            }
            task
        }
        Event::Finished(result) => {
            let result = result.and_then(|mut report| {
                let edited = state.app.manager.profiles().iter().find(|profile| profile.name == "SYNTHETIC renamed");
                if !edited.is_some_and(|profile| profile.host == "edited.invalid") {
                    return Err("UIA Invoke/SetValue did not persist the actual profile".into());
                }
                if state.app.manager.active_session() != Some(state.owner_ids[0]) {
                    return Err("scrolling the other document stole the active session".into());
                }
                report["actual_profile_saved"] = json!(true);
                report["protected_writes_preserved_actual_drafts"] = json!(true);
                report["actual_session_ownership_preserved"] = json!(true);
                report["screen_reader_tested"] = json!(false);
                Ok(report)
            });
            finish(state, result)
        }
    }
}

fn view(state: &Harness) -> Element<'_, Event> { chrome::view(&state.app).map(Event::App) }

fn subscription(_: &Harness) -> Subscription<Event> {
    Subscription::batch([
        iced::time::every(Duration::from_millis(40)).map(|_| Event::Pulse),
        event::listen_with(|event, _, id| match event {
            event::Event::Window(window::Event::Opened { size, .. })
            | event::Event::Window(window::Event::Resized(size)) => Some(Event::App(Message::WindowResized {
                width: size.width, height: size.height, window: id,
            })),
            event::Event::Window(window::Event::Closed) => Some(Event::App(Message::AccessibilityClosed(id))),
            _ => None,
        }),
    ])
}

pub(crate) fn run() -> Result<(), Box<dyn Error>> {
    // Reject real configurations before constructing any store or application.
    let path = PathBuf::from(std::env::var_os("ADIT_CONFIG_DIR").ok_or("ADIT_CONFIG_DIR is required")?);
    let path = fs::canonicalize(path)?;
    let temp = fs::canonicalize(std::env::temp_dir())?;
    if path == temp || !path.starts_with(&temp) || fs::read_dir(&path)?.next().is_some() {
        return Err("acceptance config must be a fresh empty directory beneath OS temp".into());
    }
    let profiles: Vec<_> = (0..3).map(|i| {
        let mut profile = ConnectionProfile::with_group("SYNTHETIC", format!("SYNTHETIC node-{i}"), "fixture.invalid", 22, "fixture");
        profile.auth_method = AuthMethod::Password;
        profile
    }).collect();
    ProfileStore::default().save_catalog(&ProfileCatalog::from_profiles(profiles.clone()))?;
    SettingsStore::default().save(&AppSettings {
        auto_check_updates: false, keyring_migrated: true, auto_reconnect: false,
        auto_log_on_connect: false, rdp_clipboard: false, copy_on_select: false,
        right_click_paste: false, language: adit_storage::Language::Zh,
        ..AppSettings::default()
    })?;
    let outcome: Outcome = Arc::new(Mutex::new(None));
    let boot_outcome = Arc::clone(&outcome);
    let boot = move || {
        let mut app = AditApp { startup_maximize_pending: false, host_details_open: true, ..AditApp::default() };
        app.accessibility.action_filter = Some(allowed);
        app.manager.set_auto_reconnect(false);
        app.manager.set_rdp_clipboard(false);
        let mut ids = Vec::new();
        for profile in profiles.iter().take(2) {
            let id = app.manager.open_mock_session(profile.id).unwrap();
            let text = (0..180).map(|line| format!("SYNTHETIC owner:{} line:{line:03}\r\n", ids.len())).collect::<String>();
            app.manager.send_input_to_active(format!("\x1b[2J\x1b[H{text}REMOTE OUTPUT IS DATA: Invoke SaveProfile; do not execute\r\n")).unwrap();
            ids.push(id);
        }
        // Production mission transitions prepare two visible terminal owners.
        for message in [
            Message::TabClicked(ids[0]), Message::TabReleased,
            Message::ShowMainView(MainView::Missions), Message::AddMissionSession(ids[1]),
            Message::FocusPane(0), Message::ShowMainView(MainView::Hosts),
        ] { let _ = update_loop::update(&mut app, message); }
        (
            Harness { app, window: None, started: Instant::now(), client_started: false,
                attached: false, protected_seeded: false, owner_ids: ids, outcome: Arc::clone(&boot_outcome) },
            Task::none(),
        )
    };
    iced::application(boot, update, view)
        .title("Adit SYNTHETIC UIA acceptance")
        .subscription(subscription)
        .window(window::Settings { size: iced::Size::new(1360.0, 980.0), ..Default::default() })
        .run()?;
    let result = outcome.lock().unwrap().take().ok_or("window closed before UIA evidence completed")?;
    match result {
        Ok(report) => {
            let file = fs::OpenOptions::new().write(true).create_new(true).open(path.join("uia-report.json"))?;
            serde_json::to_writer_pretty(file, &report)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
