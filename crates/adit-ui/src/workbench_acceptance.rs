//! Opt-in native workbench capture; never call this from the shipping boot path.
//!
//! Required: ADIT_CONFIG_DIR points to a new/empty absolute directory under the
//! OS temp directory. It is checked before constructing AditApp or any store.
//! Optional environment:
//! - ADIT_WORKBENCH_SCREENSHOT_DIR: empty absolute output directory (default:
//!   ADIT_CONFIG_DIR/screenshots). Existing artifacts are never overwritten.
//! - ADIT_WORKBENCH_VIEW: hosts | terminal | missions (default: hosts).
//! - ADIT_WORKBENCH_THEME: dark | light (default: dark).
//! - ADIT_WORKBENCH_SIZE: logical WIDTHxHEIGHT (default: 1360x860).
//! - ADIT_WORKBENCH_SCALE: iced application scale, e.g. 1, 1.5, 2 (default: 1).
//! - ADIT_WORKBENCH_SETTLE_MS: delay before each capture (default: 750).
//! - ADIT_WORKBENCH_TIMEOUT_MS: whole-run watchdog (default: 60000).
//! - ADIT_WORKBENCH_INTERACTIVE: 1 holds the window after successful captures
//!   for local OS pointer/focus smoke; close the window to exit (default: 0).
//!
//! All views use production chrome and production update messages. Fixture tasks
//! are created through ShowMainView/NewMission/AddMissionSession, then exercised
//! through PickMission/FocusMissionPane/ReturnToMission. Automation dispatches
//! messages; it does not inject OS pointer input or prove pointer hit testing.
//! Interactive mode forwards production runtime events after the report is
//! saved; subsequent GUI changes are outside its evidence and invariants.
//! Local navigation/focus actions are allowed; transport, credential and
//! clipboard operations remain gated. There is no production polling subscription,
//! update check, sync, keyring migration, or blinking cursor timer.

use super::*;
use image::ImageEncoder;
use serde_json::{json, Value};
use std::{
    error::Error,
    fs,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const FIXTURE_COUNT: usize = 12;
type Outcome = Arc<Mutex<Option<Result<(), String>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureView {
    Hosts,
    Terminal,
    Missions,
}

impl CaptureView {
    fn name(self) -> &'static str {
        match self {
            Self::Hosts => "hosts",
            Self::Terminal => "terminal",
            Self::Missions => "missions",
        }
    }

    fn main_view(self) -> MainView {
        match self {
            Self::Hosts => MainView::Hosts,
            Self::Terminal => MainView::Terminal,
            Self::Missions => MainView::Missions,
        }
    }
}

#[derive(Clone)]
struct Config {
    config_dir: PathBuf,
    output_dir: PathBuf,
    view: CaptureView,
    dark: bool,
    size: iced::Size,
    scale: f32,
    settle: Duration,
    timeout: Duration,
    interactive: bool,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn env_or(name: &str, fallback: &str) -> Result<String, std::env::VarError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(fallback.to_owned()),
        value => value,
    }
}

impl Config {
    fn read() -> Result<Self, Box<dyn Error>> {
        let config_dir = PathBuf::from(
            std::env::var_os("ADIT_CONFIG_DIR")
                .ok_or_else(|| invalid("ADIT_CONFIG_DIR must name an isolated temp directory"))?,
        );
        let output_dir = std::env::var_os("ADIT_WORKBENCH_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.join("screenshots"));
        let view = match env_or("ADIT_WORKBENCH_VIEW", "hosts")?.as_str() {
            "hosts" => CaptureView::Hosts,
            "terminal" => CaptureView::Terminal,
            "missions" => CaptureView::Missions,
            _ => return Err(invalid("ADIT_WORKBENCH_VIEW must be hosts/terminal/missions").into()),
        };
        let dark = match env_or("ADIT_WORKBENCH_THEME", "dark")?.as_str() {
            "dark" => true,
            "light" => false,
            _ => return Err(invalid("ADIT_WORKBENCH_THEME must be dark/light").into()),
        };
        let size = env_or("ADIT_WORKBENCH_SIZE", "1360x860")?;
        let (width, height) = size
            .split_once('x')
            .ok_or_else(|| invalid("ADIT_WORKBENCH_SIZE must be WIDTHxHEIGHT"))?;
        let size = iced::Size::new(width.parse::<f32>()?, height.parse::<f32>()?);
        let scale = env_or("ADIT_WORKBENCH_SCALE", "1")?.parse::<f32>()?;
        if !size.width.is_finite()
            || !size.height.is_finite()
            || !(320.0..=4096.0).contains(&size.width)
            || !(320.0..=4096.0).contains(&size.height)
            || !scale.is_finite()
            || !(0.5..=3.0).contains(&scale)
        {
            return Err(invalid("logical dimensions must be 320..4096 and scale 0.5..3").into());
        }
        let settle = env_or("ADIT_WORKBENCH_SETTLE_MS", "750")?.parse::<u64>()?;
        let timeout = env_or("ADIT_WORKBENCH_TIMEOUT_MS", "60000")?.parse::<u64>()?;
        if !(100..=10_000).contains(&settle) || !(1_000..=600_000).contains(&timeout) {
            return Err(invalid("settle must be 100..10000 ms; timeout 1000..600000 ms").into());
        }
        Ok(Self {
            config_dir,
            output_dir,
            view,
            dark,
            size,
            scale,
            settle: Duration::from_millis(settle),
            timeout: Duration::from_millis(timeout),
            interactive: env_or("ADIT_WORKBENCH_INTERACTIVE", "0")? == "1",
        })
    }

    fn prepare(&mut self) -> Result<(), Box<dyn Error>> {
        if !self.config_dir.is_absolute() || !self.output_dir.is_absolute() {
            return Err(invalid("config and screenshot paths must be absolute").into());
        }
        let temp = fs::canonicalize(std::env::temp_dir())?;
        if !self.config_dir.exists() {
            let parent = fs::canonicalize(
                self.config_dir
                    .parent()
                    .ok_or_else(|| invalid("config directory needs a parent"))?,
            )?;
            if !parent.starts_with(&temp) {
                return Err(invalid("config directory must be beneath OS temp").into());
            }
            fs::create_dir(&self.config_dir)?;
        }
        self.config_dir = fs::canonicalize(&self.config_dir)?;
        if self.config_dir == temp || !self.config_dir.starts_with(&temp) {
            return Err(invalid("config directory must be beneath OS temp").into());
        }
        claim_empty_dir(&self.config_dir)?;
        fs::create_dir_all(&self.output_dir)?;
        self.output_dir = fs::canonicalize(&self.output_dir)?;
        claim_empty_dir(&self.output_dir)?;
        Ok(())
    }
}

fn claim_empty_dir(path: &Path) -> io::Result<()> {
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        return Err(invalid(format!(
            "directory must be empty: {}",
            path.display()
        )));
    }
    // A second harness must not pass the empty check and share this fixture.
    fs::create_dir(path.join(".workbench-acceptance"))
}

fn fixture_profiles() -> Vec<ConnectionProfile> {
    let groups = ["SYNTHETIC Dev", "SYNTHETIC Ops", "SYNTHETIC Data"];
    let icons = ["ubuntu", "windows", "container", "database"];
    (0..FIXTURE_COUNT)
        .map(|index| {
            let mut profile = ConnectionProfile::with_group(
                groups[index / 4],
                format!("SYNTHETIC node-{:02}", index + 1),
                format!("198.51.100.{}", index + 1),
                22,
                "fixture",
            );
            profile.auth_method = AuthMethod::Password;
            profile.sort_order = index as i32;
            profile.icon = icons[index % icons.len()].into();
            profile.pinned = index % 4 == 0;
            profile.pinned_order = index as i32;
            profile.vnc.clipboard_send = false;
            profile.vnc.clipboard_receive = false;
            profile.vnc.auto_reconnect = false;
            profile
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Expected {
    view: MainView,
    active: SessionId,
    sidebar: bool,
    pane_start: usize,
    pane_count: usize,
    focused_pane: usize,
    mission: u64,
}

struct Step {
    name: &'static str,
    messages: Vec<Message>,
    expected: Expected,
}

fn plan(view: CaptureView, ids: &[SessionId]) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut expected = Expected {
        view: view.main_view(),
        active: ids[3],
        sidebar: true,
        pane_start: 0,
        pane_count: 0,
        focused_pane: 0,
        mission: 1,
    };
    // Missions::default starts its counter at zero. These two IDs are produced
    // by the real creation messages below; later picks verify their distinct
    // member sets instead of constructing private mission state.
    let initial = vec![
        Message::TabClicked(ids[0]),
        Message::TabReleased,
        Message::ShowMainView(MainView::Missions),
        Message::AddMissionSession(ids[1]),
        Message::AddMissionSession(ids[2]),
        Message::AddMissionSession(ids[3]),
        Message::ShowMainView(MainView::Terminal),
        Message::TabClicked(ids[4]),
        Message::TabReleased,
        Message::NewMission,
        Message::AddMissionSession(ids[5]),
        Message::AddMissionSession(ids[6]),
        Message::AddMissionSession(ids[7]),
        Message::PickMission(1),
        Message::ShowMainView(expected.view),
    ];
    if view == CaptureView::Missions {
        expected.pane_count = 4;
        expected.focused_pane = 3;
    }
    steps.push(Step {
        name: view.name(),
        messages: initial,
        expected,
    });
    expected.view = MainView::Hosts;
    expected.pane_count = 0;
    expected.focused_pane = 0;
    steps.push(Step {
        name: "d-hosts-preserve-sessions",
        messages: vec![Message::ShowMainView(MainView::Hosts)],
        expected,
    });
    expected.view = MainView::Terminal;
    steps.push(Step {
        name: "e-terminal-preserve-sessions",
        messages: vec![Message::ShowMainView(MainView::Terminal)],
        expected,
    });
    for (name, index) in [
        ("switch-last-session", ids.len() - 1),
        ("switch-first-session", 0),
    ] {
        expected.active = ids[index];
        expected.pane_count = 0;
        expected.focused_pane = 0;
        steps.push(Step {
            name,
            messages: vec![Message::TabClicked(ids[index]), Message::TabReleased],
            expected,
        });
    }
    for (name, sidebar) in [("sidebar-hidden", false), ("sidebar-restored", true)] {
        expected.sidebar = sidebar;
        steps.push(Step {
            name,
            messages: vec![Message::ToggleSidebar],
            expected,
        });
    }
    expected.active = ids[3];
    expected.view = MainView::Missions;
    expected.pane_count = 4;
    expected.focused_pane = 3;
    steps.push(Step {
        name: "f-first-task-four-session-panes",
        messages: vec![Message::ShowMainView(MainView::Missions)],
        expected,
    });
    expected.active = ids[0];
    expected.pane_count = 0;
    expected.focused_pane = 0;
    steps.push(Step {
        name: "f-first-task-focus",
        messages: vec![Message::FocusMissionPane(0)],
        expected,
    });
    expected.pane_count = 4;
    steps.push(Step {
        name: "f-first-task-return",
        messages: vec![Message::ReturnToMission],
        expected,
    });
    expected.active = ids[7];
    expected.pane_start = 4;
    expected.focused_pane = 3;
    expected.mission = 2;
    steps.push(Step {
        name: "f-second-task",
        messages: vec![Message::PickMission(2)],
        expected,
    });
    expected.active = ids[5];
    expected.pane_count = 0;
    expected.focused_pane = 0;
    steps.push(Step {
        name: "f-second-task-focus",
        messages: vec![Message::FocusMissionPane(1)],
        expected,
    });
    expected.pane_count = 4;
    expected.focused_pane = 1;
    steps.push(Step {
        name: "f-second-task-return",
        messages: vec![Message::ReturnToMission],
        expected,
    });
    expected.active = ids[0];
    expected.pane_start = 0;
    expected.focused_pane = 0;
    expected.mission = 1;
    steps.push(Step {
        name: "f-switch-back-first-task",
        messages: vec![Message::PickMission(1)],
        expected,
    });
    expected.view = MainView::Hosts;
    expected.pane_count = 0;
    steps.push(Step {
        name: "d-hosts-preserve-tasks",
        messages: vec![Message::ShowMainView(MainView::Hosts)],
        expected,
    });
    expected.view = MainView::Missions;
    expected.pane_count = 4;
    steps.push(Step {
        name: "f-restore-existing-task",
        messages: vec![Message::ShowMainView(MainView::Missions)],
        expected,
    });
    steps
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Settling,
    Capturing,
    Writing,
    Finishing,
    Held,
}

struct State {
    app: AditApp,
    config: Config,
    ids: Vec<SessionId>,
    steps: Vec<Step>,
    index: usize,
    phase: Phase,
    started: Instant,
    changed: Instant,
    window: Option<window::Id>,
    os_scale: Option<f32>,
    captures: Vec<Value>,
    executed_messages: Vec<String>,
    outcome: Outcome,
}

#[derive(Debug, Clone)]
enum NativeMessage {
    App(Message),
    Window(window::Id, window::Event),
    Tick,
    Advance,
    Captured(window::Screenshot),
    Saved(Result<Value, String>),
    Finished(Result<(), String>),
}

// Widgets are real, but arbitrary clicks must not start transports, read the
// clipboard, change global config pointers, or invoke credential operations.
fn dispatch(state: &mut State, mut message: Message) -> Task<NativeMessage> {
    let interactive = state.phase == Phase::Held && interactive_message_allowed(state, &message);
    let allowed = match &mut message {
        Message::ShowMainView(_)
        | Message::ToggleSidebar
        | Message::TabReleased
        | Message::NewMission
        | Message::ReturnToMission
        | Message::WindowFocused
        | Message::WindowUnfocused
        | Message::WindowResized { .. } => true,
        Message::TabClicked(id) => state.ids.contains(id),
        Message::AddMissionSession(id) => state.ids.contains(id),
        Message::PickMission(id) => state.phase == Phase::Held || matches!(*id, 1 | 2),
        Message::FocusMissionPane(index) => *index < state.app.panes.len(),
        Message::DisplayScale(scale) => {
            state.os_scale = Some(*scale);
            *scale *= state.config.scale;
            true
        }
        _ => interactive,
    };
    if allowed {
        // Preserve the completed capture evidence and avoid accumulating an
        // unbounded cursor-motion log during a manual smoke session.
        if state.phase != Phase::Held {
            state.executed_messages.push(format!("{message:?}"));
        }
        update_loop::update(&mut state.app, message).map(NativeMessage::App)
    } else {
        Task::none()
    }
}

fn interactive_message_allowed(state: &State, message: &Message) -> bool {
    match message {
        Message::MissionNameChanged(_)
        | Message::BeginMissionName
        | Message::FinishMissionName(_)
        | Message::GlobalPointerReleased(_, _)
        | Message::HostDetailsClosed
        | Message::HostsCursorMoved(_)
        | Message::MissionPickerToggled
        | Message::RemoveMission
        | Message::FindOpenSession
        | Message::SessionFilterChanged(_)
        | Message::GlobalCursorMoved(_)
        | Message::ModifiersChanged(_)
        | Message::CancelProfileDrag
        | Message::ToggleMenu(_)
        | Message::ToggleTheme
        | Message::ToggleFullscreen
        | Message::HostLayoutChanged(_)
        | Message::BeginSidebarDrag
        | Message::SidebarDragMove(_)
        | Message::EndSidebarDrag => true,
        Message::ActivateMissionSession(id) | Message::TabDragOver(id) => state.ids.contains(id),
        Message::GridProfilePressed(id, _) | Message::ProfilePressed(id)
        | Message::ProfileDropped(id) | Message::GridProfileHovered(id, _)
        | Message::ProfileHoverExited(id) | Message::OpenExistingProfile(id) =>
            state.app.manager.profile(*id).is_some(),
        Message::FocusPane(index)
        | Message::ClosePane(index)
        | Message::PaneMousePressed(index)
        | Message::PanePointerMoved(index, _) => *index < state.app.panes.len(),
        Message::EndTerminalSelection | Message::TerminalPointerMoved(_) => true,
        Message::RunMenu(
            MenuCommand::ToggleSidebar
            | MenuCommand::ToggleFullscreen
            | MenuCommand::TileGrid
            | MenuCommand::TileVertical
            | MenuCommand::TileHorizontal
            | MenuCommand::Untile,
        ) => true,
        // Text inputs consume their own typing. Only focus/escape keys from
        // the ignored-event path reach production keyboard routing here;
        // reconnect, SFTP, clipboard and other shortcut paths stay disabled.
        Message::KeyboardInput(
            keyboard::Event::KeyPressed { key, modifiers, .. }
            | keyboard::Event::KeyReleased { key, modifiers, .. },
        ) => {
            !modifiers.control()
                && !modifiers.alt()
                && matches!(key, Key::Named(Named::Tab | Named::Escape))
        }
        _ => false,
    }
}

fn verify(state: &State) -> Result<(), String> {
    let sessions = state.app.manager.sessions();
    let actual: Vec<_> = sessions.iter().map(|session| session.id).collect();
    if state.ids.len() != FIXTURE_COUNT
        || actual != state.ids
        || sessions
            .iter()
            .any(|session| session.status != SessionStatus::Connected)
    {
        return Err(
            "navigation changed fixture session identity, order, or connection status".into(),
        );
    }
    let expected = state.steps[state.index].expected;
    if state.app.main_view != expected.view
        || state.app.manager.active_session() != Some(expected.active)
        || state.app.sidebar_visible != expected.sidebar
        || state.app.panes.len() != expected.pane_count
        || state.app.focused_pane != expected.focused_pane
        || state.app.terminal_focused != (expected.view != MainView::Hosts)
    {
        return Err(format!(
            "unexpected navigation/focus state at {}",
            state.steps[state.index].name
        ));
    }
    if !state.app.panes.iter().all(|id| state.ids.contains(id))
        || state.app.panes.iter().collect::<HashSet<_>>().len() != state.app.panes.len()
        || state.app.panes
            != state.ids[expected.pane_start..expected.pane_start + expected.pane_count]
    {
        return Err("split changed fixture membership, uniqueness, or pane order".into());
    }
    if let Some(error) = &state.app.last_error {
        return Err(format!("application error: {error}"));
    }
    Ok(())
}

fn repaint_mock_terminals(state: &mut State) {
    // A real PTY redraws after SIGWINCH. The in-memory transport has no peer,
    // so repaint its explicitly synthetic text after each layout transition.
    let active = state.app.manager.active_session();
    for (index, id) in state.ids.iter().enumerate() {
        state.app.manager.activate(*id).expect("fixture session");
        state.app.manager.send_input_to_active(format!(
            "\x1b[2J\x1b[HSYNTHETIC node-{:02}\r\n\
             In-memory fixture / no network transport\r\n\r\n\
             $ hostname\r\nsynthetic-node-{:02}\r\n\
             $ echo native-workbench\r\nnative-workbench\r\n$ ",
            index + 1, index + 1,
        )).expect("fixture redraw");
    }
    if let Some(id) = active {
        state.app.manager.activate(id).expect("restore fixture focus");
    }
}

fn snapshot(state: &State) -> Value {
    json!({
        "main_view": format!("{:?}", state.app.main_view),
        "active_session": state.app.manager.active_session().map(|id| id.to_string()),
        "session_ids": state.app.manager.sessions().iter().map(|s| s.id.to_string()).collect::<Vec<_>>(),
        "session_count": state.app.manager.sessions().len(),
        "expected_session_count": FIXTURE_COUNT,
        "session_statuses": state.app.manager.sessions().iter()
            .map(|session| format!("{:?}", session.status)).collect::<Vec<_>>(),
        "panes": state.app.panes.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "focused_pane": state.app.focused_pane,
        "terminal_focused": state.app.terminal_focused,
        "sidebar_visible": state.app.sidebar_visible,
        "logical_size": [state.app.window_width, state.app.window_height],
        "os_scale": state.os_scale,
        "application_scale": state.config.scale,
        "elapsed_ms": state.started.elapsed().as_millis(),
        "executed_dispatcher_messages": state.executed_messages,
        "expected_mission": state.steps.get(state.index).map(|step| step.expected.mission),
        "expected_navigation": state.steps.get(state.index).map(|step| {
            let expected = step.expected;
            json!({
                "main_view": format!("{:?}", expected.view),
                "active_session": expected.active.to_string(),
                "sidebar_visible": expected.sidebar,
                "panes": state.ids[expected.pane_start..expected.pane_start + expected.pane_count]
                    .iter().map(ToString::to_string).collect::<Vec<_>>(),
                "focused_pane": expected.focused_pane,
            })
        }),
    })
}

fn finish(state: &mut State, result: Result<(), String>) -> Task<NativeMessage> {
    state.phase = Phase::Finishing;
    let report = json!({
        "synthetic": true,
        "status": if result.is_ok() { "passed" } else { "failed" },
        "result_scope": "native captures and navigation assertions; no pixel baseline comparison",
        "error": result.as_ref().err(),
        "current_step": state.steps.get(state.index).map(|step| step.name),
        "requested_view": state.config.view.name(),
        "theme": if state.config.dark { "dark" } else { "light" },
        "requested_logical_size": [state.config.size.width, state.config.size.height],
        "application_scale": state.config.scale,
        "scale_note": "application scale multiplies OS DPI; this does not change the OS DPI setting",
        "fixture_profiles": state.app.manager.profiles(),
        "fixture_session_ids": state.ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "planned_captures": state.steps.len(),
        "captures": state.captures,
        "input_evidence": {
            "scope": "automated screenshot sequence only; subsequent interactive smoke is not recorded",
            "automation": "production update_loop::update dispatcher messages",
            "real_os_window_and_compositor": true,
            "os_pointer_injected": false,
            "os_pointer_hit_testing_verified": false,
            "mission_identity": "restored active session and exact ordered pane membership",
        },
        "interactive_mode": {
            "requested": state.config.interactive,
            "hold_after_success": state.config.interactive && result.is_ok(),
            "recorded_in_this_report": false,
        },
        "final_state": snapshot(state),
        "not_covered": [
            "real OS pointer clicks and hit testing",
            "real SSH/SFTP/RDP/VNC/local-shell/serial/Telnet transports",
            "remote input, clipboard, IME, native OS DPI changes"
        ],
    });
    let path = state.config.output_dir.join("report.json");
    Task::perform(
        async move {
            let write = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(|error| error.to_string())?;
                let mut writer = BufWriter::new(file);
                serde_json::to_writer_pretty(&mut writer, &report)
                    .map_err(|error| error.to_string())?;
                writer.flush().map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            write.and(result)
        },
        NativeMessage::Finished,
    )
}

fn update_native(state: &mut State, message: NativeMessage) -> Task<NativeMessage> {
    if state.phase == Phase::Finishing {
        if let NativeMessage::Finished(result) = message {
            let hold = state.config.interactive && result.is_ok();
            *state.outcome.lock().expect("acceptance outcome") = Some(result);
            if hold {
                state.phase = Phase::Held;
                return Task::none();
            }
            return iced::exit();
        }
        return Task::none();
    }
    // Late capture/timer messages cannot restart the sequence, index past its
    // last step, or apply its old invariants to real GUI changes.
    if state.phase == Phase::Held {
        return match message {
            NativeMessage::App(message) => dispatch(state, message),
            NativeMessage::Window(id, window::Event::Rescaled(_)) => window::scale_factor(id)
                .map(|scale| NativeMessage::App(Message::DisplayScale(scale))),
            _ => Task::none(),
        };
    }
    match message {
        NativeMessage::App(message) => dispatch(state, message),
        NativeMessage::Window(id, event) => {
            state.window = Some(id);
            match event {
                window::Event::Opened { size, .. } | window::Event::Resized(size) => {
                    state.changed = Instant::now();
                    dispatch(
                        state,
                        Message::WindowResized {
                            width: size.width,
                            height: size.height,
                            window: id,
                        },
                    )
                }
                window::Event::Focused => dispatch(state, Message::WindowFocused),
                window::Event::Unfocused => dispatch(state, Message::WindowUnfocused),
                window::Event::Rescaled(_) => {
                    state.changed = Instant::now();
                    window::scale_factor(id)
                        .map(|scale| NativeMessage::App(Message::DisplayScale(scale)))
                }
                _ => Task::none(),
            }
        }
        NativeMessage::Tick => {
            if state.started.elapsed() >= state.config.timeout {
                return finish(state, Err("native screenshot run timed out".into()));
            }
            if state.phase == Phase::Waiting && state.window.is_some() {
                state.phase = Phase::Settling;
                return Task::done(NativeMessage::Advance);
            }
            // The first capture gets extra time for initial font/GPU setup.
            let delay = state.config.settle * if state.index == 0 { 2 } else { 1 };
            if state.phase == Phase::Settling && state.changed.elapsed() >= delay {
                if let Err(error) = verify(state) {
                    return finish(state, Err(error));
                }
                state.phase = Phase::Capturing;
                return window::screenshot(state.window.expect("opened acceptance window"))
                    .map(NativeMessage::Captured);
            }
            Task::none()
        }
        NativeMessage::Advance => {
            if state.index == state.steps.len() {
                return finish(state, Ok(()));
            }
            let messages = state.steps[state.index].messages.clone();
            state.executed_messages.clear();
            let tasks: Vec<_> = messages
                .into_iter()
                .map(|message| dispatch(state, message))
                .collect();
            repaint_mock_terminals(state);
            state.phase = Phase::Settling;
            state.changed = Instant::now();
            Task::batch(tasks)
        }
        NativeMessage::Captured(screenshot) if state.phase == Phase::Capturing => {
            if let Err(error) = verify(state) {
                return finish(state, Err(error));
            }
            if !screenshot.scale_factor.is_finite()
                || screenshot.scale_factor <= 0.0
                || screenshot.size.width == 0
                || screenshot.size.height == 0
            {
                return finish(state, Err("invalid native screenshot geometry".into()));
            }
            let name = format!("{:02}-{}.png", state.index, state.steps[state.index].name);
            let path = state.config.output_dir.join(&name);
            let mut record = snapshot(state);
            record["navigation_invariants_passed"] = json!(true);
            record["file"] = json!(name);
            record["actions"] = json!(state.steps[state.index]
                .messages
                .iter()
                .map(|message| format!("{message:?}"))
                .collect::<Vec<_>>());
            record["physical_size"] = json!([screenshot.size.width, screenshot.size.height]);
            record["compositor_scale"] = json!(screenshot.scale_factor);
            let logical_width = screenshot.size.width as f32 / screenshot.scale_factor;
            let logical_height = screenshot.size.height as f32 / screenshot.scale_factor;
            record["screenshot_logical_size"] = json!([logical_width, logical_height]);
            record["requested_size_matches"] = json!(
                (logical_width - state.config.size.width).abs() <= 2.0
                    && (logical_height - state.config.size.height).abs() <= 2.0
            );
            record["requested_scale_matches"] = json!(state.os_scale.is_some_and(|os_scale| {
                (screenshot.scale_factor - os_scale * state.config.scale).abs() <= 0.01
            }));
            state.phase = Phase::Writing;
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || -> Result<Value, String> {
                        let file = fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(path)
                            .map_err(|error| error.to_string())?;
                        let mut writer = BufWriter::new(file);
                        image::codecs::png::PngEncoder::new(&mut writer)
                            .write_image(
                                &screenshot.rgba,
                                screenshot.size.width,
                                screenshot.size.height,
                                image::ExtendedColorType::Rgba8,
                            )
                            .map_err(|error| error.to_string())?;
                        writer.flush().map_err(|error| error.to_string())?;
                        Ok(record)
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result)
                },
                NativeMessage::Saved,
            )
        }
        NativeMessage::Saved(result) if state.phase == Phase::Writing => match result {
            Ok(record) => {
                let geometry_matches = record["requested_size_matches"] == true
                    && record["requested_scale_matches"] == true;
                state.captures.push(record);
                if !geometry_matches {
                    return finish(
                        state,
                        Err("native size/scale differs from requested geometry; see capture metadata".into()),
                    );
                }
                state.index += 1;
                Task::done(NativeMessage::Advance)
            }
            Err(error) => finish(state, Err(error)),
        },
        _ => Task::none(),
    }
}

fn view_native(state: &State) -> Element<'_, NativeMessage> {
    chrome::view(&state.app).map(NativeMessage::App)
}

/// Boot the actual iced workbench with synthetic, isolated session fixtures.
pub fn run() -> Result<(), Box<dyn Error>> {
    let mut config = Config::read()?;
    config.prepare()?;
    let profiles = fixture_profiles();
    ProfileStore::default().save_catalog(&ProfileCatalog::from_profiles(profiles.clone()))?;
    SettingsStore::default().save(&AppSettings {
        dark_mode: config.dark,
        theme_mode: Some(if config.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        }),
        window_width: config.size.width,
        window_height: config.size.height,
        auto_check_updates: false,
        auto_reconnect: false,
        auto_log_on_connect: false,
        copy_on_select: false,
        right_click_paste: false,
        keyring_migrated: true,
        rdp_clipboard: false,
        recent_hosts: profiles
            .iter()
            .rev()
            .take(4)
            .map(|profile| profile.id)
            .collect(),
        ..AppSettings::default()
    })?;
    let outcome: Outcome = Arc::new(Mutex::new(None));
    let boot_outcome = Arc::clone(&outcome);
    let boot_config = config.clone();
    let boot = move || {
        let mut app = AditApp {
            startup_maximize_pending: false,
            selected_profile: Some(profiles[0].id),
            host_details_open: true,
            display_scale: boot_config.scale,
            notice: "\u{5408}\u{6210}\u{9a8c}\u{6536}\u{4f1a}\u{8bdd} - SYNTHETIC / NO TRANSPORT"
                .into(),
            ..AditApp::default()
        };
        app.manager.set_auto_reconnect(false);
        app.manager.set_rdp_clipboard(false);
        let ids: Vec<_> = profiles
            .iter()
            .map(|profile| {
                let id = app
                    .manager
                    .open_mock_session(profile.id)
                    .expect("synthetic fixture profile");
                app.manager
                    .send_input_to_active(format!(
                        "\x1b[2J\x1b[H\x1b[1;36mSYNTHETIC / NO TRANSPORT\x1b[0m\r\n\
                 {}\r\n\r\nIsolated native workbench regression fixture.\r\n\
                 This session has no network connection.\r\n",
                        profile.name,
                    ))
                    .expect("in-memory fixture terminal");
                id
            })
            .collect();
        let steps = plan(boot_config.view, &ids);
        (
            State {
                app,
                config: boot_config.clone(),
                ids,
                steps,
                index: 0,
                phase: Phase::Waiting,
                started: Instant::now(),
                changed: Instant::now(),
                window: None,
                os_scale: None,
                captures: Vec::new(),
                executed_messages: Vec::new(),
                outcome: Arc::clone(&boot_outcome),
            },
            // Querying also covers an Opened event delivered before subscription.
            window::latest().and_then(|id| {
                window::size(id)
                    .map(move |size| NativeMessage::Window(id, window::Event::Resized(size)))
            }),
        )
    };
    iced::application(boot, update_native, view_native)
        .title("Adit - SYNTHETIC workbench acceptance")
        .theme(|state: &State| app_theme(&state.app))
        .scale_factor(|state: &State| state.config.scale)
        .subscription(|state: &State| {
            let windows = window::events().map(|(id, event)| NativeMessage::Window(id, event));
            if state.phase == Phase::Held {
                Subscription::batch([
                    windows,
                    event::listen_with(runtime_event).map(NativeMessage::App),
                ])
            } else {
                Subscription::batch([
                    windows,
                    iced::time::every(Duration::from_millis(50)).map(|_| NativeMessage::Tick),
                ])
            }
        })
        .window(window::Settings {
            size: config.size,
            maximized: false,
            ..Default::default()
        })
        .run()?;
    let result = outcome
        .lock()
        .expect("acceptance outcome")
        .take()
        .unwrap_or_else(|| Err("acceptance window closed before the report completed".into()));
    result.map_err(io::Error::other)?;
    Ok(())
}
