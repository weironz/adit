//! Bounded, observe-only Win32 control for the actual keyboard hook source.
//! Only this process's tagged F24 is counted; no desktop or key text is recorded.
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VK_F24, VK_LMENU, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    static WINDOW_F24: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
            && wparam.0 == usize::from(VK_F24.0)
            && GetMessageExtraInfo().0 as usize == keyboard_hook::probe_input_tag()
        {
            WINDOW_F24.fetch_add(1, Ordering::Relaxed);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    unsafe fn focus_snapshot(hwnd: HWND, window_thread: u32) -> Value {
        let foreground = GetForegroundWindow();
        let mut foreground_pid = 0;
        let foreground_thread = GetWindowThreadProcessId(foreground, Some(&mut foreground_pid));
        let mut gui = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let read = GetGUIThreadInfo(window_thread, &mut gui).is_ok();
        json!({
            "foreground_owned":foreground == hwnd && foreground_pid == std::process::id(),
            "foreground_thread_is_window_thread":foreground_thread == window_thread,
            "gui_thread_info_ok":read,
            "active_owned":read && gui.hwndActive == hwnd,
            "focus_owned":read && gui.hwndFocus == hwnd,
            "gui_flags":gui.flags.0,
        })
    }

    fn input_pair(
        vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
        tag: usize,
    ) -> [INPUT; 2] {
        [false, true].map(|up| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwExtraInfo: tag,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                },
            },
        })
    }

    fn owned_and_focused(snapshot: &Value) -> bool {
        snapshot["foreground_owned"] == true
            && snapshot["focus_owned"] == true
            && snapshot["active_owned"] == true
            && snapshot["gui_flags"]
                .as_u64()
                .is_some_and(|flags| flags & 0x1e == 0)
    }

    pub(super) fn run() {
        let args: Vec<_> = std::env::args_os().collect();
        let output =
            std::path::PathBuf::from(args.get(1).expect("absolute fresh JSON report path"));
        assert!(output.is_absolute() && output.parent().is_some_and(|p| p.is_dir()));
        let focus_alt = args.iter().skip(2).any(|arg| arg == "--focus-alt");
        assert!(
            args.iter().skip(2).all(|arg| arg == "--focus-alt"),
            "unknown option"
        );
        let mut report_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .expect("refusing to overwrite an earlier report");
        // Keep the complete shipping module checked without ever calling its
        // interception modes. This control only invokes arm_observer(target=0).
        let _shipping_modes: [fn() -> bool; 2] = [keyboard_hook::arm, keyboard_hook::arm_vnc];
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).expect("current module").0);
            let class = w!("AditHookWin32Probe");
            assert_ne!(
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: instance,
                    lpszClassName: class,
                    ..Default::default()
                }),
                0,
                "register owned window"
            );
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!("Adit 自动 Win32 键盘诊断（仅观察，完成自动关闭）"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                580,
                160,
                None,
                None,
                Some(instance),
                None,
            )
            .expect("create owned window");
            let window_thread = GetCurrentThreadId();
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let before_focus_request = focus_snapshot(hwnd, window_thread);
            let alt_accepted = if focus_alt {
                let mut input = input_pair(VK_LMENU, 0);
                for event in &mut input {
                    event.Anonymous.ki.wScan =
                        MapVirtualKeyW(u32::from(VK_MENU.0), MAPVK_VK_TO_VSC) as u16;
                    event.Anonymous.ki.dwFlags |= KEYEVENTF_EXTENDEDKEY;
                }
                SendInput(&input, size_of::<INPUT>() as i32)
            } else {
                0
            };
            let foreground_request_accepted = SetForegroundWindow(hwnd).as_bool();
            let after_focus_request = focus_snapshot(hwnd, window_thread);
            let started = Instant::now();
            let mut armed_at = None;
            let mut hook_installed = false;
            let mut continuous_foreground = true;
            let mut continuous_focus = true;
            let mut before_probe = 0;
            let mut next_sample = Duration::ZERO;
            let mut samples = Vec::new();
            let mut injection = None;
            let (tx, rx) = mpsc::channel();
            let mut closed = false;
            loop {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_QUIT {
                        closed = true;
                        break;
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if closed {
                    break;
                }
                let elapsed = started.elapsed();
                if elapsed >= next_sample {
                    let focus = focus_snapshot(hwnd, window_thread);
                    if armed_at.is_none()
                        && owned_and_focused(&focus)
                        && elapsed >= Duration::from_millis(150)
                    {
                        before_probe = keyboard_hook::probe_events();
                        hook_installed = keyboard_hook::arm_observer();
                        armed_at = Some(Instant::now());
                        let tx = tx.clone();
                        let target = hwnd.0 as isize;
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(100));
                            let hwnd = HWND(target as *mut _);
                            let before = focus_snapshot(hwnd, window_thread);
                            let accepted = if owned_and_focused(&before) {
                                let input = input_pair(VK_F24, keyboard_hook::probe_input_tag());
                                SendInput(&input, size_of::<INPUT>() as i32)
                            } else {
                                0
                            };
                            let after = focus_snapshot(hwnd, window_thread);
                            let _ =
                                tx.send(json!({"before":before,"after":after,"accepted":accepted}));
                        });
                    }
                    if armed_at.is_some() {
                        continuous_foreground &= focus["foreground_owned"] == true;
                        continuous_focus &= owned_and_focused(&focus);
                    }
                    let diagnostics = keyboard_hook::diagnostics();
                    samples.push(json!({"elapsed_ms":elapsed.as_millis(),"focus":focus,
                        "armed":diagnostics.0,"counters":diagnostics.2,
                        "own_probe_events":keyboard_hook::probe_events(),
                        "window_tagged_f24_events":WINDOW_F24.load(Ordering::Relaxed),
                        "worker_health":keyboard_hook::worker_health()}));
                    next_sample = elapsed + Duration::from_millis(50);
                }
                if let Ok(value) = rx.try_recv() {
                    injection = Some(value);
                }
                if elapsed >= Duration::from_secs(15)
                    || armed_at.is_some_and(|armed| armed.elapsed() >= Duration::from_secs(2))
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let final_focus = focus_snapshot(hwnd, window_thread);
            let diagnostics = keyboard_hook::diagnostics();
            let worker_health = keyboard_hook::worker_health();
            let worker_thread = keyboard_hook::worker_thread_id();
            let own_probe_delta = keyboard_hook::probe_events().wrapping_sub(before_probe);
            let window_f24 = WINDOW_F24.load(Ordering::Relaxed);
            let mut queued = 0;
            for stroke in keyboard_hook::drain() {
                let _ = (stroke.scancode, stroke.extended, stroke.pressed);
                queued += 1;
            }
            let valid = !closed
                && armed_at.is_some()
                && continuous_foreground
                && continuous_focus
                && owned_and_focused(&final_focus)
                && injection.as_ref().is_some_and(|event| {
                    event["accepted"] == 2
                        && owned_and_focused(&event["before"])
                        && owned_and_focused(&event["after"])
                });
            let outcome = if !valid {
                "incomplete"
            } else if hook_installed
                && own_probe_delta >= 2
                && window_f24 >= 2
                && diagnostics.2[4] == 0
                && queued == 0
            {
                "passed"
            } else {
                "failed"
            };
            keyboard_hook::disarm();
            if !closed {
                let _ = DestroyWindow(hwnd);
            }
            let _ = UnregisterClassW(class, Some(instance));
            let report = json!({"scope":"pure Win32 foreground window; actual source hook; observe only",
                "outcome":outcome,"pid":std::process::id(),"window_thread":window_thread,
                "worker_thread":worker_thread,"parameters":{"focus_alt":focus_alt},
                "before_focus_request":before_focus_request,"after_focus_request":after_focus_request,
                "foreground_request_accepted":foreground_request_accepted,"focus_alt_accepted":alt_accepted,
                "continuous_foreground":continuous_foreground,"continuous_focus":continuous_focus,
                "final_focus":final_focus,"hook_installed":hook_installed,"armed":diagnostics.0,
                "counters":diagnostics.2,"own_probe_events":keyboard_hook::probe_events(),
                "own_probe_delta":own_probe_delta,"window_tagged_f24_events":window_f24,
                "worker_health":worker_health,"queued":queued,"injection":injection,"samples":samples});
            report_file
                .write_all(&serde_json::to_vec_pretty(&report).expect("serialize report"))
                .expect("write report");
        }
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
