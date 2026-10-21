//! Winit-only, observe-only control: tagged F24, no intercepted input or desktop recording.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "../src/keyboard_hook.rs"]
mod keyboard_hook;

#[cfg(windows)]
mod probe {
    use super::keyboard_hook;
    use serde_json::{json, Value};
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F24,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetMessageExtraInfo, GetWindowThreadProcessId,
        GUITHREADINFO, MSG, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
    use winit::keyboard::{KeyCode, PhysicalKey};
    use winit::platform::windows::EventLoopBuilderExtWindows;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    static WINDOW_TAGGED_F24: AtomicU32 = AtomicU32::new(0);

    fn window_hwnd(window: &Window) -> HWND {
        match window
            .window_handle()
            .expect("owned window handle")
            .as_raw()
        {
            RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut _),
            _ => unreachable!("Windows-only fixture"),
        }
    }

    fn focus_snapshot(hwnd: HWND, window_thread: u32) -> Value {
        unsafe {
            let foreground = GetForegroundWindow();
            let mut process = 0;
            let thread = GetWindowThreadProcessId(foreground, Some(&mut process));
            let mut gui = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let read = GetGUIThreadInfo(window_thread, &mut gui).is_ok();
            json!({"foreground_owned":foreground == hwnd && process == std::process::id(),
                "foreground_thread_is_window_thread":thread == window_thread,
                "gui_thread_info_ok":read,"active_owned":read && gui.hwndActive == hwnd,
                "focus_owned":read && gui.hwndFocus == hwnd,"gui_flags":gui.flags.0})
        }
    }

    fn focused(snapshot: &Value) -> bool {
        snapshot["foreground_owned"] == true
            && snapshot["focus_owned"] == true
            && snapshot["active_owned"] == true
            && snapshot["gui_flags"]
                .as_u64()
                .is_some_and(|flags| flags & 0x1e == 0)
    }

    struct App {
        window: Option<Window>,
        window_thread: u32,
        started: Instant,
        armed_at: Option<Instant>,
        next_sample: Instant,
        hook_installed: bool,
        before_probe: u32,
        continuous_foreground: bool,
        continuous_focus: bool,
        closed: bool,
        window_f24: u32,
        no_device_events: bool,
        receiver: Option<Receiver<Value>>,
        injection: Option<Value>,
        samples: Vec<Value>,
        before_focus: Option<Value>,
        after_focus: Option<Value>,
        report: Option<Value>,
    }

    impl App {
        fn finish(&mut self, event_loop: &ActiveEventLoop) {
            let final_focus = self
                .window
                .as_ref()
                .map(|window| focus_snapshot(window_hwnd(window), self.window_thread));
            let diagnostics = keyboard_hook::diagnostics();
            let delta = keyboard_hook::probe_events().wrapping_sub(self.before_probe);
            let native_events = WINDOW_TAGGED_F24.load(Ordering::Relaxed);
            let mut queued = 0;
            for stroke in keyboard_hook::drain() {
                let _ = (stroke.scancode, stroke.extended, stroke.pressed);
                queued += 1;
            }
            let valid = !self.closed
                && self.armed_at.is_some()
                && self.continuous_foreground
                && self.continuous_focus
                && final_focus.as_ref().is_some_and(focused)
                && self.injection.as_ref().is_some_and(|event| {
                    event["accepted"] == 2 && focused(&event["before"]) && focused(&event["after"])
                });
            let outcome = if !valid {
                "incomplete"
            } else if self.hook_installed
                && delta >= 2
                && native_events >= 2
                && self.window_f24 >= 2
                && diagnostics.2[4] == 0
                && queued == 0
            {
                "passed"
            } else {
                "failed"
            };
            self.report = Some(
                json!({"scope":"winit-only foreground window; actual source hook; observe only",
                "outcome":outcome,"pid":std::process::id(),"window_thread":self.window_thread,
                "worker_thread":keyboard_hook::worker_thread_id(),
                "parameters":{"no_device_events":self.no_device_events,"focus_request_once":true},
                "before_focus_request":self.before_focus,"after_focus_request":self.after_focus,
                "continuous_foreground":self.continuous_foreground,"continuous_focus":self.continuous_focus,
                "final_focus":final_focus,"hook_installed":self.hook_installed,
                "armed":diagnostics.0,"counters":diagnostics.2,
                "own_probe_events":keyboard_hook::probe_events(),"own_probe_delta":delta,
                "window_tagged_f24_events":native_events,"winit_f24_events":self.window_f24,
                "worker_health":keyboard_hook::worker_health(),"queued":queued,
                "injection":self.injection,"samples":self.samples}),
            );
            keyboard_hook::disarm();
            event_loop.exit();
        }

        fn tick(&mut self, event_loop: &ActiveEventLoop) {
            let now = Instant::now();
            let Some(window) = self.window.as_ref() else {
                return;
            };
            let hwnd = window_hwnd(window);
            if let Some(receiver) = &self.receiver {
                if let Ok(event) = receiver.try_recv() {
                    self.injection = Some(event);
                }
            }
            if now >= self.next_sample {
                let focus = focus_snapshot(hwnd, self.window_thread);
                if self.armed_at.is_none()
                    && focused(&focus)
                    && self.started.elapsed() >= Duration::from_millis(150)
                {
                    self.before_probe = keyboard_hook::probe_events();
                    self.hook_installed = keyboard_hook::arm_observer();
                    self.armed_at = Some(now);
                    let (tx, rx) = mpsc::channel();
                    self.receiver = Some(rx);
                    let target = hwnd.0 as isize;
                    let thread = self.window_thread;
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(100));
                        let hwnd = HWND(target as *mut _);
                        let before = focus_snapshot(hwnd, thread);
                        let accepted = if focused(&before) {
                            let input = [false, true].map(|up| INPUT {
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
                            unsafe { SendInput(&input, size_of::<INPUT>() as i32) }
                        } else {
                            0
                        };
                        let after = focus_snapshot(hwnd, thread);
                        let _ = tx.send(json!({"before":before,"after":after,"accepted":accepted}));
                    });
                }
                if self.armed_at.is_some() {
                    self.continuous_foreground &= focus["foreground_owned"] == true;
                    self.continuous_focus &= focused(&focus);
                }
                let diagnostics = keyboard_hook::diagnostics();
                self.samples.push(json!({"event":"sample","elapsed_ms":self.started.elapsed().as_millis(),
                    "focus":focus,"armed":diagnostics.0,"counters":diagnostics.2,
                    "own_probe_events":keyboard_hook::probe_events(),
                    "window_tagged_f24_events":WINDOW_TAGGED_F24.load(Ordering::Relaxed),
                    "winit_f24_events":self.window_f24,"worker_health":keyboard_hook::worker_health()}));
                self.next_sample = now + Duration::from_millis(50);
            }
            if self.started.elapsed() >= Duration::from_secs(15)
                || self
                    .armed_at
                    .is_some_and(|armed| armed.elapsed() >= Duration::from_secs(2))
            {
                self.finish(event_loop);
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_sample));
            }
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let window = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Adit 自动 winit 键盘诊断（仅观察，完成自动关闭）")
                        .with_inner_size(winit::dpi::LogicalSize::new(580.0, 160.0)),
                )
                .expect("create owned window");
            self.window_thread = unsafe { GetCurrentThreadId() };
            let hwnd = window_hwnd(&window);
            self.before_focus = Some(focus_snapshot(hwnd, self.window_thread));
            window.focus_window();
            self.after_focus = Some(focus_snapshot(hwnd, self.window_thread));
            self.window = Some(window);
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            window_id: WindowId,
            event: WindowEvent,
        ) {
            if self
                .window
                .as_ref()
                .is_none_or(|window| window.id() != window_id)
            {
                return;
            }
            match event {
                WindowEvent::CloseRequested => {
                    self.closed = true;
                    self.finish(event_loop);
                }
                WindowEvent::Focused(value) => {
                    self.samples
                        .push(json!({"event":"winit_focus","focused":value,
                        "elapsed_ms":self.started.elapsed().as_millis()}));
                    if self.armed_at.is_some() && !value {
                        self.continuous_focus = false;
                    }
                }
                WindowEvent::KeyboardInput { event, .. }
                    if event.physical_key == PhysicalKey::Code(KeyCode::F24) =>
                {
                    self.window_f24 += 1;
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.tick(event_loop);
        }
    }

    pub(super) fn run() {
        let args: Vec<_> = std::env::args_os().collect();
        let output =
            std::path::PathBuf::from(args.get(1).expect("absolute fresh JSON report path"));
        assert!(output.is_absolute() && output.parent().is_some_and(|p| p.is_dir()));
        assert!(
            args.iter().skip(2).all(|arg| arg == "--no-device-events"),
            "unknown option"
        );
        let no_device_events = args.iter().skip(2).any(|arg| arg == "--no-device-events");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .expect("refusing to overwrite an earlier report");
        let _shipping_modes: [fn() -> bool; 2] = [keyboard_hook::arm, keyboard_hook::arm_vnc];
        let mut builder = EventLoop::builder();
        builder.with_msg_hook(|message| {
            // Observe the exact tagged native event while retaining normal winit dispatch.
            let message = unsafe { &*message.cast::<MSG>() };
            if matches!(
                message.message,
                WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
            ) && message.wParam.0 == usize::from(VK_F24.0)
                && unsafe { GetMessageExtraInfo() }.0 as usize == keyboard_hook::probe_input_tag()
            {
                WINDOW_TAGGED_F24.fetch_add(1, Ordering::Relaxed);
            }
            false
        });
        let event_loop = builder.build().expect("create event loop");
        if no_device_events {
            event_loop.listen_device_events(DeviceEvents::Never);
        }
        let started = Instant::now();
        let mut app = App {
            window: None,
            window_thread: 0,
            started,
            armed_at: None,
            next_sample: started,
            hook_installed: false,
            before_probe: 0,
            continuous_foreground: true,
            continuous_focus: true,
            closed: false,
            window_f24: 0,
            no_device_events,
            receiver: None,
            injection: None,
            samples: Vec::new(),
            before_focus: None,
            after_focus: None,
            report: None,
        };
        event_loop
            .run_app(&mut app)
            .expect("run bounded event loop");
        keyboard_hook::disarm();
        file.write_all(
            &serde_json::to_vec_pretty(&app.report.expect("bounded run produced report"))
                .expect("serialize report"),
        )
        .expect("write report");
    }
}

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This observe-only probe requires a Windows test desktop.");
}
