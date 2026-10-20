//! Explicit Windows hook delivery probe. Only injected F24 is generated;
//! ordinary input is never recorded. Run on an authorized test desktop.
#[path = "../src/keyboard_hook.rs"]
mod keyboard_hook;

fn main() {
    let injection_second: u32 = std::env::args()
        .find_map(|value| value.strip_prefix("--inject-after=").map(str::to_owned))
        .map(|value| value.parse().expect("--inject-after must be 0..12 seconds"))
        .unwrap_or(1);
    assert!(injection_second <= 12);
    let rdp = std::env::args().any(|value| value == "--rdp");
    let observer = std::env::args().any(|value| value == "--observe-only");
    let installed = if observer {
        keyboard_hook::arm_observer()
    } else if rdp {
        keyboard_hook::arm()
    } else {
        keyboard_hook::arm_vnc()
    };
    println!(
        "installed={installed} pid={} probe_tag={}",
        std::process::id(),
        keyboard_hook::probe_input_tag()
    );
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::StationsAndDesktops::*;
        let object_name = |handle| {
            let mut text = [0u16; 256];
            let mut needed = 0;
            let result = GetUserObjectInformationW(
                handle,
                UOI_NAME,
                Some(text.as_mut_ptr().cast()),
                size_of_val(&text) as u32,
                Some(&mut needed),
            );
            format!(
                "{:?}:{}",
                result,
                String::from_utf16_lossy(&text[..text.iter().position(|v| *v == 0).unwrap_or(0)])
            )
        };
        println!(
            "station={}",
            object_name(HANDLE(GetProcessWindowStation().unwrap().0))
        );
        if let Some(id) = keyboard_hook::worker_thread_id() {
            println!(
                "worker={id} desktop={}",
                object_name(HANDLE(GetThreadDesktop(id).unwrap().0))
            );
        }
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(desktop) => {
                println!("input_desktop={}", object_name(HANDLE(desktop.0)));
                let _ = CloseDesktop(desktop);
            }
            Err(error) => println!("input_desktop_error={error}"),
        }
    }
    for second in 0..15 {
        #[cfg(windows)]
        if second == injection_second {
            use windows::Win32::UI::Input::KeyboardAndMouse::*;
            let mut foreground_pid = 0;
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                    windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow(),
                    Some(&mut foreground_pid),
                );
            }
            println!("selftest_second={second} selftest_foreground_pid={foreground_pid}");
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
            println!("injected_f24={}", unsafe {
                SendInput(&inputs, size_of::<INPUT>() as i32)
            });
        }
        let state = keyboard_hook::diagnostics();
        println!("worker_health={:?}", keyboard_hook::worker_health());
        println!("own_probe_events={}", keyboard_hook::probe_events());
        let drained: Vec<_> = keyboard_hook::drain()
            .into_iter()
            .map(|stroke| (stroke.scancode, stroke.extended, stroke.pressed))
            .collect();
        println!(
            "second={second} armed={} vnc={} counters={:?} queued={}",
            state.0,
            state.1,
            state.2,
            drained.len()
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    keyboard_hook::disarm();
}
