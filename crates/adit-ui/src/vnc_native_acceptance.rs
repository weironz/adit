//! Opt-in native acceptance window. It reuses the shipping widgets/dispatcher,
//! but has isolated storage, synthetic localhost input and no clipboard access.
use super::*;
use std::io::Write;
use std::sync::atomic::AtomicU32;

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
    ime_snapshot: Option<serde_json::Value>,
    auto: Option<HookAutotest>,
    ui_stall_enabled: bool,
    ui_stalls: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AutoPhase {
    Await,
    PendingOn,
    Observe,
    PendingOff,
    OffQueued,
    Recover,
    Writing,
}

#[derive(Clone)]
struct HookAutotest {
    started: Instant,
    phase: AutoPhase,
    phase_started: Instant,
    report: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    accepted: std::sync::Arc<AtomicU32>,
    accepted_before: u32,
    probe_before: u32,
    swallowed_before: u32,
    current: Option<serde_json::Value>,
    cycles: Vec<serde_json::Value>,
    samples: Vec<serde_json::Value>,
    incomplete: bool,
}

fn auto_focus() -> serde_json::Value {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let foreground = GetForegroundWindow();
        let mut process = 0;
        let thread = GetWindowThreadProcessId(foreground, Some(&mut process));
        let mut gui = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let read = GetGUIThreadInfo(thread, &mut gui).is_ok();
        serde_json::json!({"foreground_owned":process == std::process::id(),
            "gui_thread_info_ok":read,"focus_owned":read && gui.hwndFocus == foreground,
            "active_owned":read && gui.hwndActive == foreground,"gui_flags":gui.flags.0})
    }
    #[cfg(not(windows))]
    serde_json::json!({"foreground_owned":false,"focus_owned":false,"active_owned":false})
}

fn auto_focused(focus: &serde_json::Value) -> bool {
    focus["foreground_owned"] == true
        && focus["focus_owned"] == true
        && focus["active_owned"] == true
        && focus["gui_flags"]
            .as_u64()
            .is_some_and(|flags| flags & 0x1e == 0)
}

fn raw_without_keyboard(snapshot: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    (snapshot["read"] == true).then_some(())?;
    Some(
        snapshot["devices"]
            .as_array()?
            .iter()
            .filter(|device| !(device["page"] == 1 && device["usage"] == 6))
            .cloned()
            .collect(),
    )
}

fn auto_after_toggle(state: &mut State) {
    let Some(auto) = state.auto.as_mut() else {
        return;
    };
    match auto.phase {
        AutoPhase::PendingOn => {
            let raw = native_hook_probe::raw_input_snapshot();
            let focus = auto_focus();
            let current = auto.current.as_mut().expect("pending cycle");
            current["focus_on"] = focus.clone();
            current["continuous_foreground"] = serde_json::json!(focus["foreground_owned"] == true);
            current["continuous_focus"] = serde_json::json!(auto_focused(&focus));
            if !auto_focused(&focus) {
                auto.incomplete = true;
            }
            current["raw_armed"] = raw.clone();
            current["grab_on"] = serde_json::json!(state.app.vnc_grab_keys);
            current["hook_armed"] = serde_json::json!(keyboard_hook::diagnostics().0);
            current["keyboard_removed"] = serde_json::json!(
                raw["read"] == true
                    && raw["devices"].as_array().is_some_and(|devices| devices
                        .iter()
                        .all(|device| !(device["page"] == 1 && device["usage"] == 6)))
            );
            current["other_devices_unchanged"] =
                serde_json::json!(raw_without_keyboard(&current["baseline_raw"])
                    .zip(raw_without_keyboard(&raw))
                    .is_some_and(|(before, after)| before == after));
            auto.phase = AutoPhase::Observe;
            auto.phase_started = Instant::now();
        }
        AutoPhase::OffQueued => {
            auto.phase = AutoPhase::Recover;
            auto.phase_started = Instant::now();
        }
        _ => {}
    }
}

fn auto_tick(state: &mut State) -> Task<NativeMessage> {
    let Some(mut auto) = state.auto.take() else {
        return Task::none();
    };
    if auto.phase == AutoPhase::Writing {
        state.auto = Some(auto);
        return Task::none();
    }
    let focus = auto_focus();
    auto.samples.push(
        serde_json::json!({"elapsed_ms":auto.started.elapsed().as_millis(),
        "focus":focus,"grab":state.app.vnc_grab_keys,"hook":native_hook_probe::snapshot()}),
    );
    let mut task = Task::none();
    // Leave the restore interval and asynchronous report write room inside
    // the twenty-second acceptance budget.
    let timed_out = auto.started.elapsed() >= std::time::Duration::from_millis(19_500);
    if timed_out
        && !matches!(
            auto.phase,
            AutoPhase::PendingOff | AutoPhase::OffQueued | AutoPhase::Recover
        )
    {
        auto.incomplete = true;
        auto.phase = AutoPhase::PendingOff;
    }
    match auto.phase {
        AutoPhase::Await => {
            if state.app.manager.active_rdp_live()
                && state.app.rdp_surface_size.is_some()
                && auto_focused(&focus)
            {
                auto.probe_before = keyboard_hook::probe_events();
                auto.swallowed_before = keyboard_hook::diagnostics().2[4];
                auto.accepted_before = auto.accepted.load(Ordering::Acquire);
                auto.current = Some(serde_json::json!({"cycle":auto.cycles.len()+1,
                    "baseline_raw":native_hook_probe::raw_input_snapshot(),
                    "grab_before":state.app.vnc_grab_keys,"continuous_foreground":true,
                    "continuous_focus":true,"continuous_grab":true}));
                auto.phase = AutoPhase::PendingOn;
                task = Task::done(NativeMessage::App(Message::ToggleVncGrab));
            }
        }
        AutoPhase::Observe => {
            let current = auto.current.as_mut().expect("observing cycle");
            current["continuous_foreground"] = serde_json::json!(
                current["continuous_foreground"] == true && focus["foreground_owned"] == true
            );
            current["continuous_focus"] =
                serde_json::json!(current["continuous_focus"] == true && auto_focused(&focus));
            current["continuous_grab"] =
                serde_json::json!(current["continuous_grab"] == true && state.app.vnc_grab_keys);
            if !auto_focused(&focus) {
                auto.incomplete = true;
            }
            if auto.phase_started.elapsed() >= std::time::Duration::from_secs(2) {
                auto.phase = AutoPhase::PendingOff;
            }
        }
        AutoPhase::Recover
            if auto.phase_started.elapsed() >= std::time::Duration::from_millis(300) =>
        {
            if let Some(mut current) = auto.current.take() {
                let raw = native_hook_probe::raw_input_snapshot();
                let probe_delta = keyboard_hook::probe_events().wrapping_sub(auto.probe_before);
                let swallowed_delta =
                    keyboard_hook::diagnostics().2[4].wrapping_sub(auto.swallowed_before);
                let accepted_delta = auto
                    .accepted
                    .load(Ordering::Acquire)
                    .wrapping_sub(auto.accepted_before);
                current["raw_off"] = raw.clone();
                current["raw_restored"] =
                    serde_json::json!(raw["read"] == true && raw == current["baseline_raw"]);
                current["grab_off"] = serde_json::json!(!state.app.vnc_grab_keys);
                current["hook_off"] = serde_json::json!(!keyboard_hook::diagnostics().0);
                current["own_probe_delta"] = serde_json::json!(probe_delta);
                current["swallowed_delta"] = serde_json::json!(swallowed_delta);
                current["injected_f24_accepted"] = serde_json::json!(accepted_delta);
                current["passed"] = serde_json::json!(
                    current["grab_before"] == false
                        && current["grab_on"] == true
                        && current["hook_armed"] == true
                        && current["keyboard_removed"] == true
                        && current["other_devices_unchanged"] == true
                        && current["continuous_foreground"] == true
                        && current["continuous_focus"] == true
                        && current["continuous_grab"] == true
                        && current["raw_restored"] == true
                        && current["grab_off"] == true
                        && current["hook_off"] == true
                        && probe_delta >= 2
                        && swallowed_delta == 0
                        && accepted_delta == 2
                );
                log(
                    state,
                    serde_json::json!({"event":"hook_auto_cycle","result":current}),
                );
                auto.cycles.push(current);
            }
            auto.phase = AutoPhase::Await;
            if auto.cycles.len() >= 3 || auto.incomplete || timed_out {
                release_rdp_modifiers(&mut state.app);
                keyboard_hook::disarm();
                let raw_final = native_hook_probe::raw_input_snapshot();
                let final_restored = auto.cycles.first().is_some_and(|first| {
                    raw_final["read"] == true && raw_final == first["baseline_raw"]
                });
                let outcome = if auto.incomplete || auto.cycles.len() != 3 {
                    "incomplete"
                } else if auto.cycles.iter().all(|cycle| cycle["passed"] == true)
                    && final_restored
                    && !state.app.vnc_grab_keys
                {
                    "passed"
                } else {
                    "failed"
                };
                let report = serde_json::json!({"scope":"full synthetic native GUI; production dispatcher and SessionManager; programmatic ToggleVncGrab, not toolbar click or physical Win/AltTab interception",
                        "outcome":outcome,"pid":std::process::id(),"cycles":auto.cycles,
                        "samples":auto.samples,"raw_final":raw_final,"final_restored":final_restored,
                        "grab_final":state.app.vnc_grab_keys,"hook_final":native_hook_probe::snapshot(),
                        "elapsed_ms":auto.started.elapsed().as_millis()});
                log(
                    state,
                    serde_json::json!({"event":"hook_auto_finished","outcome":outcome}),
                );
                auto.phase = AutoPhase::Writing;
                let file = std::sync::Arc::clone(&auto.report);
                task = Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || -> Result<(), String> {
                            let bytes = serde_json::to_vec_pretty(&report)
                                .map_err(|error| error.to_string())?;
                            let mut file = file.lock().map_err(|error| error.to_string())?;
                            file.write_all(&bytes)
                                .and_then(|()| file.flush())
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                    },
                    NativeMessage::AutoSaved,
                );
            }
        }
        _ => {}
    }
    if auto.phase == AutoPhase::PendingOff {
        if state.app.vnc_grab_keys {
            auto.phase = AutoPhase::OffQueued;
            task = Task::done(NativeMessage::App(Message::ToggleVncGrab));
        } else {
            release_rdp_modifiers(&mut state.app);
            keyboard_hook::disarm();
            auto.phase = AutoPhase::Recover;
            auto.phase_started = Instant::now();
        }
    }
    state.auto = Some(auto);
    task
}

#[derive(Debug, Clone)]
enum NativeMessage {
    App(Message),
    Scale(f32),
    Fit(bool),
    RemoteResize,
    NativeDpi(Option<u32>),
    AutoTick,
    AutoSaved(Result<(), String>),
    ImeTick,
    NativeIme(serde_json::Value),
    StallUi,
}

fn native_ime(window: window::Id) -> Task<NativeMessage> {
    window::run(window, |native| {
        #[cfg(windows)]
        unsafe {
            use iced::window::raw_window_handle::RawWindowHandle;
            use windows::Win32::UI::Input::{Ime::*, KeyboardAndMouse::GetKeyboardLayout};
            use windows::Win32::UI::WindowsAndMessaging::*;
            let Ok(handle) = native.window_handle() else {
                return serde_json::json!({"available":false});
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return serde_json::json!({"available":false});
            };
            let hwnd = windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _);
            let thread = windows::Win32::System::Threading::GetCurrentThreadId();
            let owner = GetWindowThreadProcessId(hwnd, None);
            // IMM context access belongs on the owning native window thread.
            // Never inspect another foreground application's input context.
            if thread != owner {
                return serde_json::json!({"available":false,"same_thread":false});
            }
            let context = ImmGetContext(hwnd);
            let mut conversion = IME_CONVERSION_MODE::default();
            let mut sentence = IME_SENTENCE_MODE::default();
            let valid = !context.0.is_null();
            let open = valid && ImmGetOpenStatus(context).as_bool();
            let converted = valid
                && ImmGetConversionStatus(context, Some(&mut conversion), Some(&mut sentence))
                    .as_bool();
            if valid {
                let _ = ImmReleaseContext(hwnd, context);
            }
            serde_json::json!({"available":true,"same_thread":true,
                "foreground":GetForegroundWindow()==hwnd,"context":valid,"open":open,
                "conversion_read":converted,"conversion":conversion.0,"sentence":sentence.0,
                "layout":GetKeyboardLayout(0).0 as usize})
        }
        #[cfg(not(windows))]
        {
            let _ = native;
            serde_json::json!({"available":false})
        }
    })
    .map(NativeMessage::NativeIme)
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

fn stall_ui(state: &mut State, trigger: &'static str) -> Task<NativeMessage> {
    if !state.ui_stall_enabled {
        return Task::none();
    }
    state.ui_stalls += 1;
    let sequence = state.ui_stalls;
    #[cfg(windows)]
    let threads = unsafe {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let dispatcher = GetCurrentThreadId();
        let foreground = GetWindowThreadProcessId(GetForegroundWindow(), None);
        serde_json::json!({"dispatcher":dispatcher,"foreground":foreground,"same":dispatcher == foreground})
    };
    #[cfg(not(windows))]
    let threads = serde_json::json!({"same":false});
    log(
        state,
        serde_json::json!({"event":"ui_stall_start","sequence":sequence,"trigger":trigger,
        "duration_ms":1500,"threads":threads,"grab":state.app.vnc_grab_keys,
        "input_epoch":state.app.vnc_input_epoch,"focus":auto_focus()}),
    );
    let started = Instant::now();
    // Explicit fixture-only fault injection. The independently pumping hook
    // stays live while the native UI thread accumulates real WM backlog.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    log(
        state,
        serde_json::json!({"event":"ui_stall_end","sequence":sequence,
        "elapsed_ms":started.elapsed().as_millis(),"grab":state.app.vnc_grab_keys,
        "input_epoch":state.app.vnc_input_epoch,"focus":auto_focus()}),
    );
    Task::none()
}

fn update_native(state: &mut State, message: NativeMessage) -> Task<NativeMessage> {
    match message {
        NativeMessage::StallUi => stall_ui(state, "button"),
        NativeMessage::ImeTick => window::latest().and_then(native_ime),
        NativeMessage::NativeIme(mut snapshot) => {
            snapshot["grab"] = serde_json::json!(state.app.vnc_grab_keys);
            snapshot["accepts_text"] = serde_json::json!(vnc_accepts_text_input(&state.app));
            snapshot["window_focused"] = serde_json::json!(state.app.desktop_window_focused);
            snapshot["desktop_focused"] = serde_json::json!(state.app.terminal_focused);
            snapshot["input_epoch"] = serde_json::json!(state.app.vnc_input_epoch);
            if state.ime_snapshot.as_ref() != Some(&snapshot) {
                state.ime_snapshot = Some(snapshot.clone());
                log(
                    state,
                    serde_json::json!({"event":"native_ime_state","state":snapshot}),
                );
            }
            Task::none()
        }
        NativeMessage::AutoTick => auto_tick(state),
        NativeMessage::AutoSaved(result) => {
            log(
                state,
                serde_json::json!({"event":"hook_auto_report_saved","result":result}),
            );
            iced::exit()
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
            if state.ui_stall_enabled {
                let stall_key = match &message {
                    Message::KeyboardInput(keyboard::Event::KeyPressed {
                        physical_key,
                        repeat,
                        ..
                    }) if *physical_key
                        == keyboard::key::Physical::Code(keyboard::key::Code::F8) =>
                    {
                        Some(!*repeat)
                    }
                    Message::KeyboardInput(keyboard::Event::KeyReleased {
                        physical_key, ..
                    }) if *physical_key
                        == keyboard::key::Physical::Code(keyboard::key::Code::F8) =>
                    {
                        Some(false)
                    }
                    _ => None,
                };
                if let Some(trigger) = stall_key {
                    return if trigger {
                        stall_ui(state, "F8")
                    } else {
                        Task::none()
                    };
                }
            }
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
                log(
                    state,
                    serde_json::json!({"event":"hook_dispatcher", "dispatcher_thread":dispatcher,
                    "foreground_window_thread":foreground, "grab_before":state.app.vnc_grab_keys}),
                );
            }
            match &mut message {
                Message::VncImeEvent(_, epoch, event) => {
                    use iced::advanced::input_method::Event;
                    let (kind, scalars) = match event {
                        Event::Opened => ("opened", 0),
                        Event::Closed => ("closed", 0),
                        Event::Preedit(text, _) => ("preedit", text.chars().count()),
                        Event::Commit(text) => ("commit", text.chars().count()),
                    };
                    log(
                        state,
                        serde_json::json!({"event":"native_ime_event","kind":kind,
                        "scalars":scalars,"epoch":epoch,"current_epoch":state.app.vnc_input_epoch,
                        "accepts_text":vnc_accepts_text_input(&state.app),"grab":state.app.vnc_grab_keys}),
                    );
                }
                Message::KeyboardInput(event) => {
                    let (physical, pressed) = match event {
                        keyboard::Event::KeyPressed { physical_key, .. } => {
                            (Some(*physical_key), true)
                        }
                        keyboard::Event::KeyReleased { physical_key, .. } => {
                            (Some(*physical_key), false)
                        }
                        _ => (None, false),
                    };
                    if let Some(keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Windows(scan @ (0xe02a | 0xe036)),
                    )) = physical
                    {
                        // Public fixture metadata only; never record typed text.
                        log(
                            state,
                            serde_json::json!({"event":"window_shift_alias", "scan":scan, "pressed":pressed}),
                        );
                    }
                }
                Message::DisplayScale(scale) => {
                    state.os_scale = *scale;
                    *scale *= state.scale;
                }
                Message::RdpPointerMoved(point, _) => log(
                    state,
                    serde_json::json!({
                        "event":"pointer", "remote_x":point.x, "remote_y":point.y,
                        "app_scale":state.scale, "os_scale":state.os_scale, "fit":state.app.vnc_fit,
                        "surface":state.app.rdp_surface_size,
                    }),
                ),
                Message::RdpPressed(button, _) => log(
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
                auto_after_toggle(state);
                log(
                    state,
                    serde_json::json!({"event":"grab_state", "enabled":state.app.vnc_grab_keys}),
                );
                #[cfg(windows)]
                if state.app.vnc_grab_keys
                    && (std::env::var("ADIT_NATIVE_HOOK_SELFTEST").as_deref() == Ok("1")
                        || state.auto.is_some())
                {
                    let events = state.events.clone();
                    let accepted_counter = state
                        .auto
                        .as_ref()
                        .map(|auto| std::sync::Arc::clone(&auto.accepted));
                    std::thread::spawn(move || {
                        use windows::Win32::UI::Input::KeyboardAndMouse::*;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let inputs = [false, true].map(|up| INPUT {
                            r#type: INPUT_KEYBOARD,
                            Anonymous: INPUT_0 {
                                ki: KEYBDINPUT {
                                    wVk: VK_F24,
                                    dwExtraInfo: keyboard_hook::probe_input_tag(),
                                    dwFlags: if up {
                                        KEYEVENTF_KEYUP
                                    } else {
                                        Default::default()
                                    },
                                    ..Default::default()
                                },
                            },
                        });
                        let accepted = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
                        if let Some(counter) = accepted_counter {
                            counter.fetch_add(accepted, Ordering::Release);
                        }
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
                log(
                    state,
                    serde_json::json!({"event":"hook_diagnostics", "armed":hook.0,
                    "vnc":hook.1,"counters":hook.2,
                    "worker_thread":keyboard_hook::worker_thread_id()}),
                );
            }
            let health = keyboard_hook::worker_health();
            let health_snapshot = (health.0 / 20, health.1);
            if state.hook_health != Some(health_snapshot) {
                state.hook_health = Some(health_snapshot);
                log(
                    state,
                    serde_json::json!({"event":"hook_pump", "turns":health.0,
                    "exit_reason":health.1,"worker_thread":keyboard_hook::worker_thread_id()}),
                );
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
    let mut controls = row![
        button("100%").on_press(NativeMessage::Scale(1.0)),
        button("150%").on_press(NativeMessage::Scale(1.5)),
        button("200%").on_press(NativeMessage::Scale(2.0)),
        button("Fit").on_press(NativeMessage::Fit(true)),
        button("1:1").on_press(NativeMessage::Fit(false)),
        button("Remote resize").on_press(NativeMessage::RemoteResize),
    ];
    if state.ui_stall_enabled {
        controls = controls.push(button("UI backlog 1.5s (F8)").on_press(NativeMessage::StallUi));
    }
    let controls = controls.spacing(5).align_y(Alignment::Center);
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
    let auto = (std::env::var("ADIT_NATIVE_HOOK_AUTOTEST").as_deref() == Ok("1")).then(|| {
        let path = std::path::PathBuf::from(
            std::env::var_os("ADIT_NATIVE_HOOK_REPORT")
                .expect("autotest requires ADIT_NATIVE_HOOK_REPORT"),
        );
        assert!(path.is_absolute() && path.parent().is_some_and(|parent| parent.is_dir()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("fresh absolute autotest report path");
        HookAutotest {
            started: Instant::now(),
            phase: AutoPhase::Await,
            phase_started: Instant::now(),
            report: std::sync::Arc::new(std::sync::Mutex::new(file)),
            accepted: std::sync::Arc::new(AtomicU32::new(0)),
            accepted_before: 0,
            probe_before: 0,
            swallowed_before: 0,
            current: None,
            cycles: Vec::new(),
            samples: Vec::new(),
            incomplete: false,
        }
    });
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
                ime_snapshot: None,
                auto: auto.clone(),
                ui_stall_enabled: std::env::var("ADIT_NATIVE_UI_STALL").as_deref() == Ok("1"),
                ui_stalls: 0,
            },
            Task::batch([
                window::latest().and_then(native_dpi),
                if auto.is_some() {
                    window::latest().and_then(window::gain_focus)
                } else {
                    Task::none()
                },
            ]),
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
        .subscription(|state: &State| {
            let normal = Subscription::batch([
                subscription(&state.app).map(NativeMessage::App),
                iced::time::every(std::time::Duration::from_millis(500))
                    .map(|_| NativeMessage::ImeTick),
            ]);
            if state.auto.is_some() {
                Subscription::batch([
                    normal,
                    iced::time::every(std::time::Duration::from_millis(50))
                        .map(|_| NativeMessage::AutoTick),
                ])
            } else {
                normal
            }
        })
        .scale_factor(|state: &State| state.scale)
        .window(window::Settings {
            size: iced::Size::new(1100.0, 760.0),
            ..Default::default()
        })
        .run()
}
