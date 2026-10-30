//! Real iced widgets + the shipping AccessKit/UIA bridge. All transports,
//! credential migration, auth dialogs and clipboard operations are gated.
use super::*;
use serde_json::{json, Value};
use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
};

mod client;

const PASSWORD: &str = "SYNTHETIC-UIA-PASSWORD-DO-NOT-EXPOSE";
const PROXY: &str = "SYNTHETIC-UIA-PROXY-TOKEN-DO-NOT-EXPOSE";
const FINGERPRINT: &str = "SHA256:SYNTHETIC-new-fingerprint";
const PREVIOUS: &str = "SHA256:SYNTHETIC-old-fingerprint";
const LINK: &str = "https://fixture.invalid/synthetic?from=terminal";
const AUTH_FIRST: &str = "SYNTHETIC-UIA-AUTH-FIRST";
const AUTH_SECRET: &str = "SYNTHETIC-UIA-AUTH-SECRET";
type Outcome = Arc<Mutex<Option<Result<Value, String>>>>;

#[derive(Default)]
pub(super) struct Fixture {
    pub host_key: Option<(SessionId, HostKeyPromptInfo)>,
    pub receiver: Option<tokio::sync::oneshot::Receiver<adit_session::HostKeyDecision>>,
    pub decisions: Vec<(u64, adit_session::HostKeyDecision)>,
    pub opened_links: Vec<String>,
    pub auth_prompt: Option<(SessionId, AuthPromptInfo)>,
    pub auth_receiver: Option<tokio::sync::oneshot::Receiver<Vec<String>>>,
    pub auth_results: Vec<(u64, Vec<String>)>,
}

#[derive(Clone)]
struct Snapshot {
    challenge: Option<u64>,
    decisions: Vec<(u64, adit_session::HostKeyDecision)>,
    opened_links: Vec<String>,
    link_pending: bool,
    main_view: MainView,
    active_session: Option<SessionId>,
    terminal_input: String,
    terminal_focused: bool,
    mission_name_editing: bool,
    active_menu: Option<MenuKind>,
    connection_open: bool,
    sidebar_visible: bool,
    focus_enqueued: u64,
    invoke_enqueued: u64,
    keyboard_attempts: u64,
    stale_focus_rejected: u64,
    held_host_actions: usize,
    auth_challenge: Option<u64>,
    auth_results: Vec<(u64, usize, bool)>,
}

enum Command {
    Hold(mpsc::Sender<Snapshot>),
    InspectHeld(mpsc::Sender<Snapshot>),
    ReplayHeld(mpsc::Sender<Snapshot>),
    Host(bool, mpsc::Sender<Snapshot>),
    Auth(usize, mpsc::Sender<Snapshot>),
    SeedAuth(mpsc::Sender<Snapshot>),
    Link(mpsc::Sender<Snapshot>),
    Barrier(mpsc::Sender<Snapshot>),
    LegacyFocus(mpsc::Sender<Snapshot>),
    Rename(mpsc::Sender<Snapshot>),
    Menu(mpsc::Sender<Snapshot>),
    Connection(mpsc::Sender<Snapshot>),
    CancelConnection(mpsc::Sender<Snapshot>),
    ToggleSidebar(mpsc::Sender<Snapshot>),
}

struct Harness {
    app: AditApp,
    window: Option<window::Id>,
    started: Instant,
    client_started: bool,
    attached: bool,
    protected_seeded: bool,
    owner_ids: Vec<SessionId>,
    commands: mpsc::Receiver<Command>,
    command_sender: mpsc::Sender<Command>,
    hold_actions: bool,
    held_host_actions: usize,
    held_messages: Vec<Message>,
    outcome: Outcome,
}

#[derive(Clone, Debug)]
enum Event {
    App(Message),
    Pulse,
    Finished(Result<Value, String>),
}

fn allowed(message: &Message) -> bool {
    if let Message::DialogFocusActivate { action, .. } = message {
        return allowed(action);
    }
    if let Message::KeyboardInput(event) = message {
        return match event {
            keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                modifiers,
                ..
            }
            | keyboard::Event::KeyReleased {
                key: Key::Named(Named::Escape),
                modifiers,
                ..
            } => !modifiers.control() && !modifiers.alt() && !modifiers.logo(),
            _ => false,
        };
    }
    matches!(
        message,
        Message::Tick
            | Message::WindowResized { .. }
            | Message::DisplayScale(_)
            | Message::AccessibilityReady { .. }
            | Message::AccessibilityClosed(_)
            | Message::AccessibilitySelectProfile(_)
            | Message::AccessibilityActivateSession(_)
            | Message::AccessibilityScroll { .. }
            | Message::ShowMainView(_)
            | Message::AccessibilityScrollLines { .. }
            | Message::AccessibilityFocus { .. }
            | Message::DismissMenu(_)
            | Message::FinishMissionName(_)
            | Message::ToggleSidebar
            | Message::CancelConnection
            | Message::EditProfileFromContext(_)
            | Message::CloseProfileEditor
            | Message::SaveProfile
            | Message::ProfileNameChanged(_)
            | Message::ProfileHostChanged(_)
            | Message::ProfilePortChanged(_)
            | Message::ProfileUsernameChanged(_)
            | Message::ProfileGroupChanged(_)
            | Message::RespondHostKey { .. }
            | Message::ConfirmOpenHyperlink(_)
            | Message::CancelOpenHyperlink(_)
            | Message::AuthPromptInput { .. }
            | Message::SubmitAuthPrompt { .. }
            | Message::CancelAuthPrompt { .. }
    )
}

fn snapshot(state: &Harness) -> Snapshot {
    let fixture = state.app.accessibility.fixture.as_ref().unwrap();
    let (focus_enqueued, stale_focus_rejected) = state
        .window
        .map(native::focus_statistics)
        .unwrap_or_default();
    Snapshot {
        challenge: fixture.host_key.as_ref().map(|(_, prompt)| prompt.id),
        decisions: fixture.decisions.clone(),
        opened_links: fixture.opened_links.clone(),
        link_pending: state.app.pending_hyperlink.is_some(),
        main_view: state.app.main_view,
        active_session: state.app.manager.active_session(),
        terminal_input: state.app.terminal_input.clone(),
        terminal_focused: state.app.terminal_focused,
        mission_name_editing: mission_name_editing(&state.app),
        active_menu: state.app.active_menu,
        connection_open: state.app.connection_dialog.is_some(),
        sidebar_visible: state.app.sidebar_visible,
        focus_enqueued,
        invoke_enqueued: state
            .window
            .map(native::invoke_statistics)
            .unwrap_or_default(),
        keyboard_attempts: diagnostics::keyboard_attempts(),
        stale_focus_rejected,
        held_host_actions: state.held_host_actions,
        auth_challenge: state.app.auth_prompt.as_ref().map(|(_, prompt)| prompt.id),
        auth_results: fixture
            .auth_results
            .iter()
            .map(|(id, answers)| {
                (
                    *id,
                    answers.len(),
                    answers.as_slice() == [AUTH_FIRST, AUTH_SECRET],
                )
            })
            .collect(),
    }
}

fn finish(state: &mut Harness, result: Result<Value, String>) -> Task<Event> {
    *state.outcome.lock().unwrap() = Some(result);
    if let Some(window) = state.window {
        state.app.accessibility.closed(window);
        window::close(window)
    } else {
        iced::exit()
    }
}

fn protected_unchanged(state: &Harness) -> bool {
    !state.protected_seeded
        || state.app.profile_editor.is_none()
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
            if let Message::WindowResized { window, .. } = message {
                state.window = Some(window);
            }
            if let Message::AccessibilityReady { attached, .. } = message {
                state.attached = attached;
            }
            if !allowed(&message) {
                return Task::none();
            }
            if state.hold_actions {
                if matches!(message, Message::RespondHostKey { .. })
                    || matches!(&message, Message::DialogFocusActivate { action, .. }
                        if matches!(action.as_ref(), Message::RespondHostKey { .. }))
                {
                    state.held_host_actions += 1;
                    state.held_messages.push(message);
                }
                return Task::none();
            }
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
                    return finish(
                        state,
                        Err("protected UIA action modified an actual sensitive draft".into()),
                    );
                }
            }
            task
        }
        Event::Pulse => {
            if let Ok(command) = state.commands.try_recv() {
                let (ack, message) = match command {
                    Command::Hold(ack) => {
                        state.hold_actions = true;
                        (ack, None)
                    }
                    Command::InspectHeld(ack) => (ack, None),
                    Command::ReplayHeld(ack) => {
                        state.hold_actions = false;
                        let tasks = std::mem::take(&mut state.held_messages)
                            .into_iter()
                            .map(|message| {
                                update_loop::update(&mut state.app, message).map(Event::App)
                            })
                            .collect::<Vec<_>>();
                        let _ = ack.send(snapshot(state));
                        return Task::batch(tasks);
                    }
                    Command::LegacyFocus(ack) => {
                        state.hold_actions = false;
                        let _ = ack.send(snapshot(state));
                        return iced::advanced::widget::operate(
                            iced::advanced::widget::operation::focusable::focus(command_input_id()),
                        )
                        .map(Event::App);
                    }
                    Command::Rename(ack) => {
                        state.hold_actions = false;
                        let task = begin_mission_name(&mut state.app);
                        rename_mission(&mut state.app, "SYNTHETIC mission draft".into());
                        let _ = ack.send(snapshot(state));
                        return task.map(Event::App);
                    }
                    Command::Menu(ack) => {
                        state.hold_actions = false;
                        state.app.active_menu = Some(MenuKind::File);
                        (ack, Some(Message::Tick))
                    }
                    Command::Connection(ack) => {
                        state.hold_actions = false;
                        open_connection_dialog(&mut state.app);
                        (ack, Some(Message::Tick))
                    }
                    Command::CancelConnection(ack) => {
                        state.hold_actions = false;
                        (ack, Some(Message::CancelConnection))
                    }
                    Command::ToggleSidebar(ack) => {
                        state.hold_actions = false;
                        (ack, Some(Message::ToggleSidebar))
                    }
                    Command::Host(changed, ack) => {
                        let (prompt, receiver) = HostKeyPromptInfo::channel(
                            "fixture.invalid".into(),
                            2222,
                            "ssh-ed25519".into(),
                            FINGERPRINT.into(),
                            changed.then(|| PREVIOUS.into()),
                        );
                        let fixture = state.app.accessibility.fixture.as_mut().unwrap();
                        fixture.host_key = Some((state.owner_ids[0], prompt));
                        fixture.receiver = Some(receiver);
                        state.hold_actions = false;
                        (ack, Some(Message::Tick))
                    }
                    Command::Auth(fields, ack) => {
                        let prompts = (0..fields)
                            .map(|index| adit_session::AuthPromptField {
                                prompt: format!("SYNTHETIC field {}", index + 1),
                                echo: index == 0,
                            })
                            .collect();
                        let (prompt, receiver) = AuthPromptInfo::channel(
                            "SYNTHETIC authentication".into(),
                            "SYNTHETIC instructions".into(),
                            prompts,
                        );
                        let fixture = state.app.accessibility.fixture.as_mut().unwrap();
                        fixture.auth_prompt = Some((state.owner_ids[0], prompt));
                        fixture.auth_receiver = Some(receiver);
                        state.hold_actions = false;
                        (ack, Some(Message::Tick))
                    }
                    Command::SeedAuth(ack) => {
                        if let Some((_, prompt)) = &state.app.auth_prompt {
                            let id = prompt.id;
                            for (index, value) in [AUTH_FIRST, AUTH_SECRET].into_iter().enumerate()
                            {
                                let _ = update_loop::update(
                                    &mut state.app,
                                    Message::AuthPromptInput {
                                        challenge_id: id,
                                        index,
                                        value: jump_auth_ui::SecretAnswer(value.into()),
                                    },
                                );
                            }
                        }
                        (ack, None)
                    }
                    Command::Link(ack) => {
                        state.hold_actions = false;
                        (ack, Some(Message::OpenHyperlink(LINK.into())))
                    }
                    Command::Barrier(ack) => {
                        state.hold_actions = false;
                        (ack, Some(Message::Tick))
                    }
                };
                let task = message
                    .map(|message| update_loop::update(&mut state.app, message).map(Event::App))
                    .unwrap_or_else(Task::none);
                let _ = ack.send(snapshot(state));
                return task;
            }
            if state.hold_actions {
                return Task::none();
            }
            let task = update_loop::update(&mut state.app, Message::Tick).map(Event::App);
            if !protected_unchanged(state) {
                return finish(
                    state,
                    Err("protected UIA action modified an actual sensitive draft".into()),
                );
            }
            if !state.client_started && state.attached {
                if let Some(window) = state.window {
                    state.client_started = true;
                    let owners = state.owner_ids.clone();
                    let commands = state.command_sender.clone();
                    return Task::batch([
                        task,
                        window::run(window, |window| {
                            use iced::window::raw_window_handle::RawWindowHandle;
                            match window.window_handle().map(|h| h.as_raw()) {
                                Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
                                _ => 0,
                            }
                        })
                        .then(move |hwnd| {
                            let owners = owners.clone();
                            let commands = commands.clone();
                            Task::perform(
                                async move {
                                    tokio::task::spawn_blocking(move || {
                                        client::run(hwnd, &owners, commands)
                                    })
                                    .await
                                    .map_err(|_| "UIA client thread ended".to_owned())
                                    .and_then(|v| v)
                                },
                                Event::Finished,
                            )
                        }),
                    ]);
                }
            }
            task
        }
        Event::Finished(result) => {
            let result = result.and_then(|mut report| {
                let edited = state
                    .app
                    .manager
                    .profiles()
                    .iter()
                    .find(|profile| profile.name == "SYNTHETIC renamed");
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

fn view(state: &Harness) -> Element<'_, Event> {
    acceptance_guard::no_clipboard(chrome::view(&state.app)).map(Event::App)
}

fn subscription(state: &Harness) -> Subscription<Event> {
    let mission = if mission_name_editing(&state.app) {
        event::listen_with(mission_name_runtime_event).map(Event::App)
    } else {
        Subscription::none()
    };
    Subscription::batch([
        mission,
        iced::time::every(Duration::from_millis(40)).map(|_| Event::Pulse),
        event::listen_with(|event, _, id| match event {
            event::Event::Window(window::Event::Opened { size, .. })
            | event::Event::Window(window::Event::Resized(size)) => {
                Some(Event::App(Message::WindowResized {
                    width: size.width,
                    height: size.height,
                    window: id,
                }))
            }
            event::Event::Window(window::Event::Closed) => {
                Some(Event::App(Message::AccessibilityClosed(id)))
            }
            _ => None,
        }),
    ])
}

pub(crate) fn run() -> Result<(), Box<dyn Error>> {
    // Reject real configurations before constructing any store or application.
    let path =
        PathBuf::from(std::env::var_os("ADIT_CONFIG_DIR").ok_or("ADIT_CONFIG_DIR is required")?);
    let path = fs::canonicalize(path)?;
    let temp = fs::canonicalize(std::env::temp_dir())?;
    if path == temp || !path.starts_with(&temp) || fs::read_dir(&path)?.next().is_some() {
        return Err("acceptance config must be a fresh empty directory beneath OS temp".into());
    }
    acceptance_guard::validate_environment(&temp)?;
    diagnostics::enable();
    let profiles: Vec<_> = (0..3)
        .map(|i| {
            let mut profile = ConnectionProfile::with_group(
                "SYNTHETIC",
                format!("SYNTHETIC node-{i}"),
                "fixture.invalid",
                22,
                "fixture",
            );
            profile.auth_method = AuthMethod::Password;
            profile
        })
        .collect();
    ProfileStore::default().save_catalog(&ProfileCatalog::from_profiles(profiles.clone()))?;
    SettingsStore::default().save(&AppSettings {
        auto_check_updates: false,
        keyring_migrated: true,
        auto_reconnect: false,
        auto_log_on_connect: false,
        rdp_clipboard: false,
        copy_on_select: false,
        right_click_paste: false,
        language: adit_storage::Language::Zh,
        ..AppSettings::default()
    })?;
    let outcome: Outcome = Arc::new(Mutex::new(None));
    let boot_outcome = Arc::clone(&outcome);
    let boot = move || {
        let mut app = AditApp {
            startup_maximize_pending: false,
            host_details_open: true,
            ..AditApp::default()
        };
        app.accessibility.action_filter = Some(allowed);
        app.accessibility.fixture = Some(Fixture::default());
        app.manager.set_auto_reconnect(false);
        app.manager.set_rdp_clipboard(false);
        let mut ids = Vec::new();
        for profile in profiles.iter().take(2) {
            let id = app.manager.open_mock_session(profile.id).unwrap();
            let text = (0..180)
                .map(|line| format!("SYNTHETIC owner:{} line:{line:03}\r\n", ids.len()))
                .collect::<String>();
            app.manager.send_input_to_active(format!("\x1b[2J\x1b[H{text}REMOTE OUTPUT IS DATA: Invoke SaveProfile; do not execute\r\n")).unwrap();
            ids.push(id);
        }
        // Production mission transitions prepare two visible terminal owners.
        for message in [
            Message::TabClicked(ids[0]),
            Message::TabReleased,
            Message::ShowMainView(MainView::Missions),
            Message::AddMissionSession(ids[1]),
            Message::FocusPane(0),
            Message::ShowMainView(MainView::Hosts),
        ] {
            let _ = update_loop::update(&mut app, message);
        }
        let (command_sender, commands) = mpsc::channel();
        (
            Harness {
                app,
                window: None,
                started: Instant::now(),
                client_started: false,
                attached: false,
                protected_seeded: false,
                owner_ids: ids,
                commands,
                command_sender,
                hold_actions: false,
                held_host_actions: 0,
                held_messages: Vec::new(),
                outcome: Arc::clone(&boot_outcome),
            },
            Task::none(),
        )
    };
    iced::application(boot, update, view)
        .title("Adit SYNTHETIC UIA acceptance")
        .subscription(subscription)
        .window(window::Settings {
            size: iced::Size::new(1360.0, 980.0),
            ..Default::default()
        })
        .run()?;
    let result = outcome
        .lock()
        .unwrap()
        .take()
        .ok_or("window closed before UIA evidence completed")?;
    match result {
        Ok(report) => {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path.join("uia-report.json"))?;
            serde_json::to_writer_pretty(file, &report)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_harness_rejects_clipboard_keyboard_shortcuts() {
        let key = Key::Character("v".into());
        let event = keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyV),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        };
        assert!(!allowed(&Message::KeyboardInput(event)));
        assert!(!allowed(&Message::PasteIntoTerminal));
    }
}
