//! Isolated two-HWND regression using the shipping window router and UIA bridge.
use super::*;
use serde_json::{json, Value};
use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    },
    UI::{
        Accessibility::*,
        WindowsAndMessaging::{GetForegroundWindow, PostMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP},
    },
};

type Outcome = Arc<Mutex<Option<Result<Value, String>>>>;

#[derive(Clone)]
struct Snapshot {
    input: String,
    terminal: String,
    view: MainView,
    child_scroll: usize,
    child_terminal: String,
    child_terminal_focused: bool,
    primary_focused: bool,
    command_open: bool,
    immediate: bool,
    primary_active: bool,
    background_focusable_found: Option<bool>,
    child_focus_enqueued: u64,
    child_focus_rejected: u64,
    cross_window_lease: Option<bool>,
    foreground_attempts: u64,
    conflict: Option<(u64, u64)>,
}

enum Command {
    Focus(mpsc::Sender<Snapshot>),
    BackgroundFocus(mpsc::Sender<Snapshot>),
    HoldChild(bool, mpsc::Sender<Snapshot>),
    DispatchPrimaryThenModal(mpsc::Sender<Snapshot>),
    Switch(bool, mpsc::Sender<Snapshot>),
    ProbeBackground(mpsc::Sender<Snapshot>),
    Modal(mpsc::Sender<Snapshot>),
    CloseModal(mpsc::Sender<Snapshot>),
    InstallConflict(mpsc::Sender<Snapshot>),
    Ime(bool, mpsc::Sender<Snapshot>),
    Wheel(usize, mpsc::Sender<Snapshot>),
    Snapshot(mpsc::Sender<Snapshot>),
}

struct Harness {
    desktop: DesktopApplication,
    child: window::Id,
    child_session: SessionId,
    primary_session: SessionId,
    ready: HashSet<window::Id>,
    started: bool,
    since: Instant,
    commands: mpsc::Receiver<Command>,
    sender: mpsc::Sender<Command>,
    outcome: Outcome,
    conflict: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
enum Event {
    Native(NativeMessage),
    Pulse,
    ProbeResult(bool, mpsc::Sender<Snapshot>),
    Finished(Result<Value, String>),
}

// This fixture must remain unable to open links, connect, read credentials,
// or dispatch application clipboard tasks, even if an operator interacts
// with its visible windows while the synthetic UIA client is running.
fn allowed_message(message: &Message) -> bool {
    if let Message::DialogFocusActivate { action, .. } = message {
        return allowed_message(action);
    }
    if let Message::KeyboardInput(event) = message {
        return allowed_runtime(&iced::Event::Keyboard(event.clone()));
    }
    matches!(
        message,
        Message::Tick
            | Message::RdpTick
            | Message::CursorBlink
            | Message::WindowResized { .. }
            | Message::DisplayScale(_)
            | Message::AccessibilityReady { .. }
            | Message::AccessibilityClosed(_)
            | Message::AccessibilityFocus { .. }
            | Message::AccessibilityScroll { .. }
            | Message::AccessibilityScrollLines { .. }
            | Message::TerminalInputChanged(_)
            | Message::SendTerminalInput
            | Message::NativeImeEvent(..)
            | Message::ModifiersChanged(_)
            | Message::RespondHostKey { .. }
            | Message::SubmitAuthPrompt { .. }
            | Message::CancelAuthPrompt { .. }
    )
}

fn allowed_runtime(event: &iced::Event) -> bool {
    match event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
        | iced::Event::Keyboard(keyboard::Event::KeyReleased { key, modifiers, .. }) => {
            !modifiers.control()
                && !modifiers.alt()
                && !modifiers.logo()
                && (matches!(key, Key::Named(Named::Enter))
                    || matches!(key, Key::Character(value) if value.eq_ignore_ascii_case("a") || value.eq_ignore_ascii_case("b")))
        }
        iced::Event::Keyboard(_)
        | iced::Event::InputMethod(_)
        | iced::Event::Mouse(_)
        | iced::Event::Window(window::Event::FileDropped(_)) => false,
        _ => true,
    }
}

fn snapshot(state: &Harness) -> Snapshot {
    let (child_focus_enqueued, child_focus_rejected) =
        accessibility::native_focus_statistics(state.child);
    Snapshot {
        input: state.desktop.app.terminal_input.clone(),
        terminal: snapshot_to_text(
            &state
                .desktop
                .app
                .manager
                .snapshot_for(state.primary_session, Viewport::visible(200)),
        ),
        view: state.desktop.app.main_view,
        child_scroll: state.desktop.windows[&state.child]
            .state
            .terminal_scroll_offset,
        child_terminal: snapshot_to_text(
            &state
                .desktop
                .app
                .manager
                .snapshot_for(state.child_session, Viewport::visible(300)),
        ),
        child_terminal_focused: state.desktop.windows[&state.child].state.terminal_focused,
        primary_focused: state.desktop.focused == Some(state.desktop.primary),
        command_open: state.desktop.app.command_window_open,
        immediate: state.desktop.app.command_send_immediately,
        primary_active: state.desktop.app.manager.active_session() == Some(state.primary_session),
        background_focusable_found: None,
        child_focus_enqueued,
        child_focus_rejected,
        cross_window_lease: None,
        foreground_attempts: accessibility::diagnostics::foreground_attempts(),
        conflict: state.conflict,
    }
}

fn update(state: &mut Harness, event: Event) -> Task<Event> {
    if state.since.elapsed() > Duration::from_secs(120) {
        *state.outcome.lock().unwrap() = Some(Err("detached UIA watchdog expired".into()));
        return iced::exit();
    }
    match event {
        Event::Native(message) => {
            match &message {
                NativeMessage::Window(_, message) | NativeMessage::View(_, _, _, message)
                    if !allowed_message(message) =>
                {
                    return Task::none()
                }
                NativeMessage::Runtime(_, event, _) if !allowed_runtime(event) => {
                    return Task::none()
                }
                _ => {}
            }
            if let NativeMessage::Window(
                _,
                Message::AccessibilityReady {
                    window,
                    attached: true,
                },
            ) = &message
            {
                state.ready.insert(*window);
            }
            let task = state.desktop.update(message).map(Event::Native);
            if !state.started
                && state.ready.contains(&state.desktop.primary)
                && state.ready.contains(&state.child)
            {
                state.started = true;
                let primary = state.desktop.primary;
                let child = state.child;
                let session = state.child_session;
                let sender = state.sender.clone();
                return Task::batch([
                    task,
                    window::run(primary, hwnd).then(move |primary_hwnd| {
                        let sender = sender.clone();
                        window::run(child, hwnd).then(move |child_hwnd| {
                            let sender = sender.clone();
                            Task::perform(
                                async move {
                                    tokio::task::spawn_blocking(move || {
                                        client(primary_hwnd, child_hwnd, session, sender)
                                    })
                                    .await
                                    .map_err(|e| e.to_string())
                                    .and_then(|result| result)
                                },
                                Event::Finished,
                            )
                        })
                    }),
                ]);
            }
            task
        }
        Event::Pulse => {
            let Ok(command) = state.commands.try_recv() else {
                return Task::none();
            };
            let (ack, task) = match command {
                Command::Focus(ack) => {
                    let primary = state.desktop.primary;
                    (
                        ack,
                        Task::batch([
                            window::gain_focus(primary),
                            accessibility::focus_widget(Some(primary), command_input_id()),
                        ]),
                    )
                }
                Command::BackgroundFocus(ack) => (
                    ack,
                    accessibility::focus_widget(Some(state.desktop.primary), command_input_id()),
                ),
                Command::HoldChild(hold, ack) => {
                    accessibility::hold_native_actions(state.child, hold);
                    (ack, Task::none())
                }
                Command::DispatchPrimaryThenModal(ack) => {
                    let primary = state.desktop.primary;
                    // Prepare the production window task while the other real
                    // HWND is foreground, then install its modal before polling.
                    let task =
                        accessibility::synthetic_cross_window_focus(primary, command_input_id());
                    let prepared = task.is_some();
                    let profile = state
                        .desktop
                        .app
                        .manager
                        .session_summary(state.child_session)
                        .unwrap()
                        .profile_id;
                    state.desktop.app.connection_dialog = Some(ConnectionDialog {
                        profile_id: profile,
                        retry_session: Some(state.child_session),
                        title: "SYNTHETIC".into(),
                        endpoint: "fixture.invalid".into(),
                        auth_method: AuthMethod::Password,
                        identity_file: String::new(),
                    });
                    state.desktop.app.native_dialog_window = Some(state.child);
                    state.desktop.sync_modal_owner();
                    let mut result = snapshot(state);
                    result.cross_window_lease = Some(prepared);
                    let _ = ack.send(result);
                    return task.unwrap_or_else(Task::none).map(move |message| {
                        Event::Native(NativeMessage::Window(primary, message))
                    });
                }
                Command::Switch(child, ack) => (
                    ack,
                    window::gain_focus(if child {
                        state.child
                    } else {
                        state.desktop.primary
                    }),
                ),
                Command::ProbeBackground(ack) => {
                    let scope = iced::advanced::widget::Id::from(format!(
                        "adit-window:{:?}",
                        state.desktop.primary
                    ));
                    let operation = iced::advanced::widget::operation::scope(
                        scope,
                        FindFocusable {
                            target: command_input_id(),
                            found: false,
                        },
                    );
                    return iced::advanced::widget::operate(operation)
                        .map(move |found| Event::ProbeResult(found, ack.clone()));
                }
                Command::Modal(ack) => {
                    let profile = state
                        .desktop
                        .app
                        .manager
                        .session_summary(state.child_session)
                        .unwrap()
                        .profile_id;
                    state.desktop.app.connection_dialog = Some(ConnectionDialog {
                        profile_id: profile,
                        retry_session: Some(state.child_session),
                        title: "SYNTHETIC".into(),
                        endpoint: "fixture.invalid".into(),
                        auth_method: AuthMethod::Password,
                        identity_file: String::new(),
                    });
                    state.desktop.app.native_dialog_window = Some(state.child);
                    state.desktop.sync_modal_owner();
                    (ack, Task::none())
                }
                Command::CloseModal(ack) => {
                    state.desktop.app.connection_dialog = None;
                    state.desktop.sync_modal_owner();
                    (ack, Task::none())
                }
                Command::InstallConflict(ack) => {
                    state.conflict = Some(accessibility::synthetic_conflicting_prompts(
                        &mut state.desktop.app,
                        state.primary_session,
                        state.child_session,
                    ));
                    state.desktop.sync_modal_owner();
                    (ack, Task::none())
                }
                Command::Ime(stale, ack) => {
                    let epoch = state.desktop.windows[&state.child].state.vnc_input_epoch;
                    let epoch = if stale { epoch.wrapping_sub(1) } else { epoch };
                    let task = state.desktop.dispatch(
                        state.child,
                        Message::NativeImeEvent(
                            state.child_session,
                            epoch,
                            iced::advanced::input_method::Event::Commit(
                                "SYNTHETIC-FRESH-IME".into(),
                            ),
                        ),
                    );
                    let _ = ack.send(snapshot(state));
                    return task.map(Event::Native);
                }
                Command::Wheel(offset, ack) => {
                    state
                        .desktop
                        .windows
                        .get_mut(&state.child)
                        .unwrap()
                        .state
                        .terminal_scroll_offset = offset;
                    state.desktop.sync_presentations();
                    (ack, Task::none())
                }
                Command::Snapshot(ack) => (ack, Task::none()),
            };
            let _ = ack.send(snapshot(state));
            let primary = state.desktop.primary;
            task.map(move |message| Event::Native(NativeMessage::Window(primary, message)))
        }
        Event::ProbeResult(found, ack) => {
            let mut result = snapshot(state);
            result.background_focusable_found = Some(found);
            let _ = ack.send(result);
            Task::none()
        }
        Event::Finished(result) => {
            *state.outcome.lock().unwrap() = Some(result);
            iced::exit()
        }
    }
}

struct FindFocusable {
    target: iced::advanced::widget::Id,
    found: bool,
}

impl iced::advanced::widget::Operation<bool> for FindFocusable {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<bool>),
    ) {
        operate(self);
    }
    fn focusable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _: iced::Rectangle,
        _: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        self.found |= id == Some(&self.target);
    }
    fn finish(&self) -> iced::advanced::widget::operation::Outcome<bool> {
        iced::advanced::widget::operation::Outcome::Some(self.found)
    }
}

fn hwnd(window: &dyn iced::window::Window) -> isize {
    use iced::window::raw_window_handle::RawWindowHandle;
    match window.window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

fn view(state: &Harness, id: window::Id) -> Element<'_, Event> {
    accessibility::acceptance_guard::no_clipboard(state.desktop.view(id)).map(Event::Native)
}

fn subscription(state: &Harness) -> Subscription<Event> {
    Subscription::batch([
        state.desktop.subscription().map(Event::Native),
        iced::time::every(Duration::from_millis(40)).map(|_| Event::Pulse),
    ])
}

fn command(
    sender: &mpsc::Sender<Command>,
    make: impl FnOnce(mpsc::Sender<Snapshot>) -> Command,
) -> Result<Snapshot, String> {
    let (tx, rx) = mpsc::channel();
    sender.send(make(tx)).map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())
}

fn wait<T>(label: &str, mut f: impl FnMut() -> Option<T>) -> Result<T, String> {
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(result) = f() {
            return Ok(result);
        }
        if Instant::now() >= until {
            return Err(format!("detached UIA timed out: {label}"));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn element(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    fragment: &str,
) -> Option<IUIAutomationElement> {
    unsafe {
        let condition = automation.CreateTrueCondition().ok()?;
        let elements = root.FindAll(TreeScope_Descendants, &condition).ok()?;
        (0..elements.Length().ok()?)
            .filter_map(|i| elements.GetElement(i).ok())
            .find(|e| {
                e.CurrentAutomationId()
                    .is_ok_and(|id| id.to_string().contains(fragment))
            })
    }
}

fn post_key(hwnd: isize, key: u16, scan: isize) -> Result<(), String> {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let down = 1 | (scan << 16);
    unsafe {
        PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(key as usize), LPARAM(down))
            .map_err(|e| e.to_string())?;
        // winit assembles text from KEYDOWN followed by CHAR. Posting KEYUP
        // first can put TranslateMessage's CHAR behind the release.
        if matches!(key, 0x41..=0x42) {
            PostMessageW(
                Some(hwnd),
                WM_CHAR,
                WPARAM((key + 32) as usize),
                LPARAM(down),
            )
            .map_err(|e| e.to_string())?;
        }
        PostMessageW(
            Some(hwnd),
            WM_KEYUP,
            WPARAM(key as usize),
            LPARAM(down | (3isize << 30)),
        )
        .map_err(|e| e.to_string())
    }
}

fn client(
    primary: isize,
    child: isize,
    session: SessionId,
    sender: mpsc::Sender<Command>,
) -> Result<Value, String> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.map_err(|e| e.to_string())?;
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() }
        }
    }
    let _com = Com;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| e.to_string())?;
    let root = unsafe { automation.ElementFromHandle(HWND(child as *mut std::ffi::c_void)) }
        .map_err(|e| e.to_string())?;

    let initial = command(&sender, Command::Focus)?;
    std::thread::sleep(Duration::from_millis(300));
    if command(&sender, Command::ProbeBackground)?.background_focusable_found != Some(true) {
        return Err("fixture cannot inspect the visible command input".into());
    }
    post_key(primary, 0x41, 0x1e)?;
    let before = wait("command input receives a real key before the modal", || {
        let state = command(&sender, Command::Snapshot).ok()?;
        (state.input != "seed").then_some(state)
    })
    .map_err(|error| {
        let state = command(&sender, Command::Snapshot).ok();
        format!(
            "{error}; fixture state: focused={}, command_open={}, view={:?}",
            state.as_ref().is_some_and(|s| s.primary_focused),
            state.as_ref().is_some_and(|s| s.command_open),
            state.map(|s| s.view)
        )
    })?;
    if before.terminal == initial.terminal {
        return Err(format!(
            "baseline immediate input did not reach the synthetic session; input={:?}, immediate={}, primary_active={}",
            before.input, before.immediate, before.primary_active
        ));
    }

    // This schedules the shipping focus task with a real cross-HWND lease.
    // The remaining scenarios exercise UIA SetFocus and Invoke themselves.
    command(&sender, |ack| Command::Switch(true, ack))?;
    wait("child foreground before scheduled primary focus", || {
        (unsafe { GetForegroundWindow() }.0 as isize == child).then_some(())
    })?;
    let scheduled = command(&sender, Command::DispatchPrimaryThenModal)?;
    if scheduled.cross_window_lease != Some(true) {
        return Err("could not prepare a focus task while the other HWND was foreground".into());
    }
    wait(
        "scheduled foreground attempt completed after the child modal",
        || {
            (command(&sender, Command::Snapshot)
                .ok()?
                .foreground_attempts
                > scheduled.foreground_attempts)
                .then_some(())
        },
    )?;
    if unsafe { GetForegroundWindow() }.0 as isize != child {
        return Err("already-scheduled primary focus stole foreground after child modal".into());
    }
    command(&sender, Command::CloseModal)?;
    command(&sender, |ack| Command::Switch(false, ack))?;
    wait(
        "primary foreground restored before the next scenario",
        || (unsafe { GetForegroundWindow() }.0 as isize == primary).then_some(()),
    )?;

    command(&sender, Command::Modal)?;
    let password = wait("detached connection input", || {
        element(&automation, &root, "protected:Password / passphrase")
    })?;
    if !unsafe { password.CurrentIsPassword() }
        .map_err(|e| e.to_string())?
        .as_bool()
    {
        return Err("detached password is not protected".into());
    }
    post_key(primary, 0x42, 0x30)?;
    post_key(primary, 0x0d, 0x1c)?;
    std::thread::sleep(Duration::from_millis(300));
    let blocked = command(&sender, Command::Snapshot)?;
    if blocked.input != before.input
        || blocked.terminal != before.terminal
        || blocked.view != before.view
    {
        return Err("keyboard changed or sent the covered primary command input".into());
    }
    command(&sender, |ack| Command::Switch(true, ack))?;
    wait("inspect child focus before queuing", || {
        (unsafe { GetForegroundWindow() }.0 as isize == child).then_some(())
    })?;
    if unsafe { password.CurrentHasKeyboardFocus() }
        .map_err(|e| e.to_string())?
        .as_bool()
    {
        return Err("switch fixture password was already focused".into());
    }
    command(&sender, |ack| Command::Switch(false, ack))?;
    wait("return to primary before queuing", || {
        (unsafe { GetForegroundWindow() }.0 as isize == primary).then_some(())
    })?;
    let before_switch = command(&sender, |ack| Command::HoldChild(true, ack))?;
    unsafe { password.SetFocus() }.map_err(|e| format!("queued child focus failed: {e}"))?;
    if command(&sender, Command::Snapshot)?.child_focus_enqueued
        <= before_switch.child_focus_enqueued
    {
        return Err("child focus request was not enqueued".into());
    }
    command(&sender, |ack| Command::Switch(true, ack))?;
    wait("switch to child before dispatch", || {
        (unsafe { GetForegroundWindow() }.0 as isize == child).then_some(())
    })?;
    command(&sender, |ack| Command::Switch(false, ack))?;
    wait("switch back to primary before dispatch", || {
        (unsafe { GetForegroundWindow() }.0 as isize == primary).then_some(())
    })?;
    command(&sender, |ack| Command::HoldChild(false, ack))?;
    wait("old child request rejected after switch", || {
        (command(&sender, Command::Snapshot)
            .ok()?
            .child_focus_rejected
            > before_switch.child_focus_rejected)
            .then_some(())
    })?;
    if unsafe { GetForegroundWindow() }.0 as isize != primary {
        return Err("stale child request stole foreground from primary".into());
    }
    command(&sender, |ack| Command::Switch(true, ack))?;
    wait("return to child to inspect its focus", || {
        (unsafe { GetForegroundWindow() }.0 as isize == child).then_some(())
    })?;
    if unsafe { password.CurrentHasKeyboardFocus() }
        .map_err(|e| e.to_string())?
        .as_bool()
    {
        return Err("stale request changed the background child's selection".into());
    }
    command(&sender, |ack| Command::Switch(false, ack))?;
    wait("return to primary for fresh UIA focus", || {
        (unsafe { GetForegroundWindow() }.0 as isize == primary).then_some(())
    })?;
    unsafe { password.SetFocus() }.map_err(|e| format!("detached UIA SetFocus failed: {e}"))?;
    wait("detached UIA SetFocus reached iced", || {
        unsafe { password.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    if command(&sender, Command::Snapshot)?.child_terminal_focused {
        return Err("UIA focus did not suspend detached remote input".into());
    }
    command(&sender, Command::BackgroundFocus)?;
    if command(&sender, Command::ProbeBackground)?.background_focusable_found != Some(false) {
        return Err("covered command input remained reachable by widget operations".into());
    }
    if !unsafe { password.CurrentHasKeyboardFocus() }
        .map_err(|e| e.to_string())?
        .as_bool()
    {
        return Err("queued focus on the covered window displaced modal focus".into());
    }
    command(&sender, Command::CloseModal)?;
    let restored = wait("detached session focus restored", || {
        let state = command(&sender, Command::Snapshot).ok()?;
        state.child_terminal_focused.then_some(state)
    })?;
    post_key(child, 0x41, 0x1e)?;
    let after_key = wait("detached keyboard works after confirmation", || {
        let state = command(&sender, Command::Snapshot).ok()?;
        (state.child_terminal != restored.child_terminal).then_some(state)
    })?;
    let stale = command(&sender, |ack| Command::Ime(true, ack))?;
    if stale.child_terminal != after_key.child_terminal {
        return Err("stale detached IME commit was accepted".into());
    }
    let fresh = command(&sender, |ack| Command::Ime(false, ack))?;
    if !fresh.child_terminal.contains("SYNTHETIC-FRESH-IME") {
        return Err("fresh detached IME commit did not reach its mock session".into());
    }
    std::thread::sleep(Duration::from_millis(200));
    if command(&sender, Command::ProbeBackground)?.background_focusable_found != Some(true) {
        return Err("command input did not return after the modal closed".into());
    }

    let range = wait("detached terminal range", || {
        element(&automation, &root, &format!("terminal-scroll:{session}"))
    })?;
    let range: IUIAutomationRangeValuePattern =
        unsafe { range.GetCurrentPatternAs(UIA_RangeValuePatternId) }.map_err(|e| e.to_string())?;
    unsafe { range.SetValue(20.0) }.map_err(|e| e.to_string())?;
    wait("detached range applied", || {
        (command(&sender, Command::Snapshot).ok()?.child_scroll == 20).then_some(())
    })?;
    command(&sender, |ack| Command::Wheel(7, ack))?;
    let up = wait("detached scroll up", || {
        element(&automation, &root, &format!("terminal-up:{session}"))
    })?;
    let invoke: IUIAutomationInvokePattern =
        unsafe { up.GetCurrentPatternAs(UIA_InvokePatternId) }.map_err(|e| e.to_string())?;
    unsafe { invoke.Invoke() }.map_err(|e| e.to_string())?;
    wait("UIA scroll follows wheel position", || {
        (command(&sender, Command::Snapshot).ok()?.child_scroll == 10).then_some(())
    })?;

    command(&sender, |ack| Command::Switch(false, ack))?;
    wait("primary foreground for conflicting authentication", || {
        (unsafe { GetForegroundWindow() }.0 as isize == primary).then_some(())
    })?;
    let (host_id, auth_id) = command(&sender, Command::InstallConflict)?
        .conflict
        .ok_or("conflicting prompt fixture missing")?;
    let primary_root =
        unsafe { automation.ElementFromHandle(HWND(primary as *mut std::ffi::c_void)) }
            .map_err(|e| e.to_string())?;
    let reject = wait("host-key owner remains actionable", || {
        element(
            &automation,
            &primary_root,
            &format!(":{host_id}/host-key:reject"),
        )
    })?;
    wait(
        "child UIA tree blocked while host-key owner is active",
        || {
            (element(&automation, &root, &format!("terminal-scroll:{session}")).is_none()
                && element(
                    &automation,
                    &root,
                    &format!("challenge:{auth_id}/auth:dialog"),
                )
                .is_none())
            .then_some(())
        },
    )?;
    let invoke: IUIAutomationInvokePattern =
        unsafe { reject.GetCurrentPatternAs(UIA_InvokePatternId) }.map_err(|e| e.to_string())?;
    unsafe { invoke.Invoke() }.map_err(|e| e.to_string())?;
    let cancel = wait("MFA owner becomes actionable after host decision", || {
        element(
            &automation,
            &root,
            &format!("challenge:{auth_id}/auth:cancel"),
        )
    })?;
    command(&sender, |ack| Command::Switch(true, ack))?;
    wait("MFA window foreground", || {
        (unsafe { GetForegroundWindow() }.0 as isize == child).then_some(())
    })?;
    let invoke: IUIAutomationInvokePattern =
        unsafe { cancel.GetCurrentPatternAs(UIA_InvokePatternId) }.map_err(|e| e.to_string())?;
    unsafe { invoke.Invoke() }.map_err(|e| e.to_string())?;
    wait("cancelled MFA prompt closes", || {
        element(&automation, &root, "auth:dialog")
            .is_none()
            .then_some(())
    })?;
    Ok(
        json!({"native_uia": true, "production_detached_window_router": true,
        "cross_window_command_keyboard_blocked": true, "detached_uia_focus": true,
        "background_focus_operation_blocked": true,
        "stale_uia_request_cannot_steal_foreground_or_change_background_focus": true,
        "scheduled_focus_revoked_before_other_window_modal": true,
        "host_key_then_mfa_across_windows_actionable": true,
        "detached_focus_keyboard_and_fresh_ime_restored": true,
        "detached_scroll_after_wheel": true, "screen_reader_tested": false, "remote_server_tested": false}),
    )
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let path =
        PathBuf::from(std::env::var_os("ADIT_CONFIG_DIR").ok_or("ADIT_CONFIG_DIR is required")?);
    let path = fs::canonicalize(path)?;
    let temp = fs::canonicalize(std::env::temp_dir())?;
    if path == temp || !path.starts_with(&temp) || fs::read_dir(&path)?.next().is_some() {
        return Err("acceptance config must be a fresh empty directory beneath OS temp".into());
    }
    accessibility::acceptance_guard::validate_environment(&temp)?;
    accessibility::diagnostics::enable();
    let profiles: Vec<_> = (0..2)
        .map(|i| ConnectionProfile::new(format!("SYNTHETIC-{i}"), "fixture.invalid", 22, "fixture"))
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
        ..AppSettings::default()
    })?;
    let outcome: Outcome = Arc::new(Mutex::new(None));
    let boot_outcome = Arc::clone(&outcome);
    iced::daemon(
        move || {
            let mut app = AditApp {
                startup_maximize_pending: false,
                ..AditApp::default()
            };
            accessibility::use_synthetic_fixture(&mut app);
            app.accessibility.action_filter = Some(allowed_message);
            app.manager.set_auto_reconnect(false);
            app.manager.set_rdp_clipboard(false);
            let child_session = app.manager.open_mock_session(profiles[0].id).unwrap();
            app.manager
                .send_input_to_active(
                    (0..200)
                        .map(|n| format!("synthetic line {n}\r\n"))
                        .collect::<String>(),
                )
                .unwrap();
            let primary_session = app.manager.open_mock_session(profiles[1].id).unwrap();
            app.main_view = MainView::Terminal;
            app.command_window_open = true;
            app.command_send_immediately = true;
            app.terminal_input = "seed".into();
            let (primary, opened) = window::open(window::Settings {
                size: iced::Size::new(960.0, 700.0),
                exit_on_close_request: false,
                ..Default::default()
            });
            app.native_main_window = Some(primary);
            app.native_window_id = Some(primary);
            let mut desktop = DesktopApplication {
                app,
                primary,
                focused: None,
                windows: HashMap::new(),
                clipboard_leases: HashMap::new(),
                modal_owner: None,
            };
            let detached = desktop.detach(child_session);
            let child = desktop.owner(child_session);
            let (sender, commands) = mpsc::channel();
            (
                Harness {
                    desktop,
                    child,
                    child_session,
                    primary_session,
                    ready: HashSet::new(),
                    started: false,
                    since: Instant::now(),
                    commands,
                    sender,
                    outcome: Arc::clone(&boot_outcome),
                    conflict: None,
                },
                Task::batch([
                    opened.map(|id| Event::Native(NativeMessage::Opened(id))),
                    detached.map(Event::Native),
                ]),
            )
        },
        update,
        view,
    )
    .title(|_: &Harness, _: window::Id| "Adit SYNTHETIC detached UIA acceptance".to_owned())
    .subscription(subscription)
    .run()?;
    let result = outcome
        .lock()
        .unwrap()
        .take()
        .ok_or("detached windows closed before evidence completed")?;
    let report = result.map_err(std::io::Error::other)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join("uia-detached-report.json"))?;
    serde_json::to_writer_pretty(file, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
