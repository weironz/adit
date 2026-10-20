//! Bounded minimal iced window: observe hook delivery, never intercept input.
#![cfg_attr(windows, windows_subsystem = "windows")]

use adit_ui::native_hook_probe;
use iced::{Element, Subscription, Task};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

struct State {
    started: Instant,
    armed_at: Option<Instant>,
    continuous_foreground: bool,
    sent: Arc<AtomicU32>,
    samples: Vec<serde_json::Value>,
    output: std::path::PathBuf,
    finishing: bool,
    background: bool,
    prearm: bool,
    requested_cycles: u32,
    cycles: Vec<serde_json::Value>,
    injected_before: u64,
}

#[derive(Clone, Debug)]
enum Message {
    Tick,
    Saved,
}

fn foreground_owned() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        pid == std::process::id()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn inject_f24(sent: Arc<AtomicU32>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        #[cfg(windows)]
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::*;
            let inputs = [false, true].map(|up| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_F24,
                        dwExtraInfo: native_hook_probe::input_tag(),
                        dwFlags: if up {
                            KEYEVENTF_KEYUP
                        } else {
                            Default::default()
                        },
                        ..Default::default()
                    },
                },
            });
            sent.fetch_add(
                SendInput(&inputs, size_of::<INPUT>() as i32),
                Ordering::Release,
            );
        }
        #[cfg(not(windows))]
        sent.store(0, Ordering::Release);
    });
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    if matches!(message, Message::Saved) {
        return iced::exit();
    }
    if state.finishing {
        return Task::none();
    }
    let foreground = foreground_owned();
    if state.armed_at.is_none() {
        state
            .samples
            .push(serde_json::json!({"event":"await_foreground",
            "foreground_owned":foreground,"elapsed_ms":state.started.elapsed().as_millis()}));
    }
    if state.armed_at.is_none() && (foreground || state.background) {
        state.injected_before = native_hook_probe::snapshot()["probe_events"]
            .as_u64()
            .unwrap_or(0);
        let installed = if state.prearm && state.cycles.is_empty() {
            native_hook_probe::snapshot()["armed"]
                .as_bool()
                .unwrap_or(false)
        } else {
            native_hook_probe::arm()
        };
        state
            .samples
            .push(serde_json::json!({"event":"arm","installed":installed,
            "foreground_owned":foreground,"hook":native_hook_probe::snapshot()}));
        state.armed_at = Some(Instant::now());
        inject_f24(Arc::clone(&state.sent));
    }
    if let Some(armed) = state.armed_at {
        state.continuous_foreground &= foreground;
        state.samples.push(
            serde_json::json!({"event":"sample","elapsed_ms":armed.elapsed().as_millis(),
            "foreground_owned":foreground,"hook":native_hook_probe::snapshot()}),
        );
    }
    if state
        .armed_at
        .is_some_and(|v| v.elapsed() >= Duration::from_secs(2))
    {
        let hook = native_hook_probe::snapshot();
        let injected = hook["probe_events"].as_u64().unwrap_or(0);
        let expected_sent = (state.cycles.len() as u32 + 1) * 2;
        let passed = state.sent.load(Ordering::Acquire) == expected_sent
            && injected.saturating_sub(state.injected_before) >= 2
            && hook["counters"][4].as_u64() == Some(0);
        state
            .cycles
            .push(serde_json::json!({"passed":passed,"hook":hook,
            "injected_delta":injected.saturating_sub(state.injected_before)}));
        native_hook_probe::disarm();
        state.armed_at = None;
    }
    if state.started.elapsed() >= Duration::from_secs(15)
        || state.cycles.len() as u32 >= state.requested_cycles
    {
        state.finishing = true;
        let snapshot = native_hook_probe::snapshot();
        native_hook_probe::disarm();
        let accepted = state.sent.load(Ordering::Acquire);
        let outcome = if state.cycles.len() as u32 != state.requested_cycles
            || (!state.background && !state.continuous_foreground)
        {
            "incomplete"
        } else if state
            .cycles
            .iter()
            .all(|cycle| cycle["passed"].as_bool() == Some(true))
        {
            "passed"
        } else {
            "failed"
        };
        let report = serde_json::json!({"scope":"minimal iced GUI, actual library hook, observe only",
            "outcome":outcome,"pid":std::process::id(),"injected_f24_accepted":accepted,
            "continuous_foreground":state.continuous_foreground,
            "foreground_required":!state.background,
            "installed_before_gui":state.prearm,
            "requested_cycles":state.requested_cycles,"cycles":state.cycles,
            "final_hook":snapshot,"samples":state.samples});
        let path = state.output.clone();
        return Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    std::fs::write(
                        path,
                        serde_json::to_vec_pretty(&report).expect("serialize synthetic report"),
                    )
                })
                .await
                .expect("report writer")
                .expect("write synthetic report");
            },
            |_| Message::Saved,
        );
    }
    Task::none()
}

fn view(_state: &State) -> Element<'_, Message> {
    iced::widget::container(iced::widget::text(
        "键盘诊断中，完成后自动关闭。\n此窗口只观察回调，不拦截按键。",
    ))
    .padding(24)
    .into()
}

fn main() -> iced::Result {
    let output = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("absolute synthetic report path"),
    );
    assert!(output.is_absolute() && output.parent().is_some_and(|p| p.is_dir()));
    assert!(!output.exists(), "refusing to overwrite an earlier result");
    let background = std::env::args().any(|arg| arg == "--background");
    let prearm = std::env::args().any(|arg| arg == "--prearm");
    let requested_cycles = if std::env::args().any(|arg| arg == "--rearm") {
        3
    } else {
        1
    };
    if prearm {
        let _ = native_hook_probe::arm();
    }
    let result = iced::application(
        move || {
            (
                State {
                    started: Instant::now(),
                    armed_at: None,
                    continuous_foreground: true,
                    sent: Arc::new(AtomicU32::new(0)),
                    samples: Vec::new(),
                    output: output.clone(),
                    finishing: false,
                    background,
                    prearm,
                    requested_cycles,
                    cycles: Vec::new(),
                    injected_before: 0,
                },
                if background {
                    Task::none()
                } else {
                    iced::window::latest().and_then(iced::window::gain_focus)
                },
            )
        },
        update,
        view,
    )
    .title("Adit 自动键盘诊断")
    .subscription(|_| -> Subscription<Message> {
        iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick)
    })
    .window_size((500.0, 160.0))
    .run();
    native_hook_probe::disarm();
    result
}
