//! Actual layout translation and RFB bytes; no global hook or injected input.
use super::*;
use keyboard_hook::vnc_chord::{Modifiers, Ownership, Route, Stroke};
use std::io::{Read, Write};

fn stroke(vk: u16, scan: u8, pressed: bool) -> Stroke {
    Stroke {
        vk,
        scan,
        extended: false,
        pressed,
        repeat: false,
        layout: test_layout().0,
        modifiers: Modifiers {
            caps: Some(false),
            ..Default::default()
        },
    }
}

fn test_layout() -> (isize, bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, GetKeyboardLayoutList, HKL,
    };
    unsafe {
        let count = GetKeyboardLayoutList(None).max(0) as usize;
        let mut layouts = vec![HKL::default(); count];
        let got = GetKeyboardLayoutList(Some(&mut layouts)).max(0) as usize;
        layouts.truncate(got);
        if let Some(layout) = layouts.iter().find(|layout| {
            matches!(
                layout.0 as usize & 0xffff,
                0x0409 | 0x0809 | 0x0407 | 0x0807 | 0x0c07
            )
        }) {
            return (layout.0 as isize, true);
        }
        // Never load or activate a user's keyboard layout just for a unit test.
        (GetKeyboardLayout(0).0 as isize, false)
    }
}

#[test]
fn native_translation_keeps_ctrl_shortcuts_out_of_control_characters() {
    let mut key = stroke(0x52, 0x13, true);
    let base =
        keyboard_hook::vnc_chord::translate(&key).expect("current layout translates test key");
    key.modifiers.control = true;
    assert_eq!(keyboard_hook::vnc_chord::translate(&key), Ok(base));
    if test_layout().1 {
        assert_eq!(base, u32::from('r'));
        key.modifiers.shift = true;
        assert_eq!(
            keyboard_hook::vnc_chord::translate(&key),
            Ok(u32::from('R'))
        );
        key.modifiers.caps = Some(true);
        assert_eq!(
            keyboard_hook::vnc_chord::translate(&key),
            Ok(u32::from('r'))
        );
    }
    assert_eq!(
        keyboard_hook::vnc_chord::translate(&stroke(0x10, 0x36, true)),
        Ok(0xffe2)
    );
}

#[test]
fn captured_win_r_repeats_and_release_order_reach_real_rfb_wire() {
    // AditApp::default loads settings. Isolate this one wire test in its own
    // process rather than mutating a global config override during other tests.
    if std::env::var("ADIT_VNC_CHORD_WIRE_CHILD").as_deref() != Ok("1") {
        let scratch = std::env::temp_dir().join(format!("adit-chord-wire-{}", SessionId::new()));
        std::fs::create_dir(&scratch).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","input::vnc_chord_tests::captured_win_r_repeats_and_release_order_reach_real_rfb_wire","--nocapture"])
            .env("ADIT_CONFIG_DIR",&scratch).env("ADIT_VNC_CHORD_WIRE_CHILD","1")
            .output().unwrap();
        let _ = std::fs::remove_dir(&scratch);
        assert!(
            output.status.success(),
            "isolated wire test failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, wire) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        stream.write_all(b"RFB 003.008\n").unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).unwrap();
        stream.write_all(&[1, 1]).unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        stream.write_all(&[0, 0, 0, 0]).unwrap();
        stream.read_exact(&mut byte).unwrap();
        let mut init = vec![
            0, 1, 0, 1, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
        ];
        init.extend_from_slice(&4u32.to_be_bytes());
        init.extend_from_slice(b"chrd");
        stream.write_all(&init).unwrap();
        while stream.read_exact(&mut byte).is_ok() {
            match byte[0] {
                0 => {
                    let mut data = [0; 19];
                    if stream.read_exact(&mut data).is_err() {
                        break;
                    }
                }
                2 => {
                    let mut data = [0; 3];
                    stream.read_exact(&mut data).unwrap();
                    let n = usize::from(u16::from_be_bytes([data[1], data[2]]));
                    stream.read_exact(&mut vec![0; n * 4]).unwrap();
                }
                3 => {
                    let mut data = [0; 9];
                    if stream.read_exact(&mut data).is_err() {
                        break;
                    }
                    if stream.write_all(&[0, 0, 0, 0]).is_err() {
                        break;
                    }
                }
                4 => {
                    let mut data = [0; 7];
                    if stream.read_exact(&mut data).is_err() {
                        break;
                    }
                    tx.send((
                        u32::from_be_bytes(data[3..7].try_into().unwrap()),
                        data[0] != 0,
                    ))
                    .unwrap();
                }
                5 => {
                    let mut data = [0; 5];
                    if stream.read_exact(&mut data).is_err() {
                        break;
                    }
                }
                other => panic!("unexpected client message {other}"),
            }
        }
    });
    let mut profile = ConnectionProfile::new("synthetic chord wire", "127.0.0.1", port, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.security = VncSecurity::AllowNone;
    profile.vnc.auto_reconnect = false;
    let id = profile.id;
    let mut app = AditApp {
        manager: SessionManager::with_profiles(vec![profile]),
        main_view: MainView::Terminal,
        terminal_focused: true,
        desktop_window_focused: true,
        ..AditApp::default()
    };
    let session = app
        .manager
        .open_live_vnc_session(id, String::new())
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while app.manager.active_session_summary().unwrap().status != SessionStatus::Connected {
        app.manager.poll_events();
        assert!(Instant::now() < deadline, "synthetic RFB authentication");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    app.vnc_grab_keys = true;
    let mut keys = Ownership::new([false; 512], Some(false));
    let events = [
        (0x5b, 0x5b, true, true),
        (0x52, 0x13, false, true),
        (0x52, 0x13, false, true),
        (0x5b, 0x5b, true, false),
        (0x52, 0x13, false, false),
        (0x52, 0x13, false, false),
    ];
    for (vk, scan, extended, pressed) in events {
        match keys.event(vk, scan, extended, pressed) {
            Route::System => route_vnc_system_keys(
                &mut app,
                [VncSystemStroke {
                    scancode: scan,
                    extended,
                    pressed,
                }],
            ),
            Route::Member { repeat } => {
                let mut event = stroke(vk, scan, pressed);
                event.extended = extended;
                event.repeat = repeat;
                // A release must not consult this changed/invalid layout, or
                // the first down's keysym would remain held on the real peer.
                if repeat || !pressed {
                    event.layout = 0;
                    event.vk = 0x59;
                    event.modifiers.shift = true;
                }
                route_vnc_chord(&mut app, event);
            }
            Route::Local => {}
        }
    }
    let selected = keyboard_hook::vnc_chord::translate(&stroke(0x52, 0x13, true)).unwrap();
    if test_layout().1 {
        assert_eq!(selected, u32::from('r'));
    }
    let expected = [
        (0xffeb, true),
        (selected, true),
        (selected, true),
        (0xffeb, false),
        (selected, false),
    ];
    for event in expected {
        assert_eq!(
            wire.recv_timeout(std::time::Duration::from_secs(3))
                .unwrap(),
            event
        );
    }
    assert!(
        wire.recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "orphan up leaked"
    );
    assert!(app.vnc_pressed_keys.is_empty());
    let mut preheld = [false; 512];
    preheld[0x5b + 256] = true;
    let mut keys = Ownership::new(preheld, Some(false));
    assert_eq!(keys.event(0x5b, 0x5b, true, true), Route::Local);
    assert_eq!(keys.event(0x52, 0x13, false, true), Route::Local);
    assert_eq!(keys.event(0x52, 0x13, false, false), Route::Local);
    assert!(
        wire.recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "local preheld Win adopted"
    );
    send_vnc_keyboard(
        &mut app,
        &keyboard::Event::KeyPressed {
            key: Key::Character("r".into()),
            modified_key: Key::Character("r".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyR),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::LOGO,
            text: Some("r".into()),
            repeat: false,
        },
    );
    assert!(
        wire.recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "unowned window logo press leaked through grab fallback"
    );
    for enter_extended in [false, true] {
        let mut keys = Ownership::new([false; 512], Some(false));
        let sequence = [
            (0x5b, 0x5b, true, true),
            (0x0d, 0x1c, enter_extended, true),
            (0xa2, 0x1d, false, true),
            (0xa4, 0x38, false, true),
            (0x0d, 0x1c, enter_extended, false),
            (0xa4, 0x38, false, false),
            (0xa2, 0x1d, false, false),
            (0x5b, 0x5b, true, false),
        ];
        for (vk, scan, extended, pressed) in sequence {
            match keys.event(vk, scan, extended, pressed) {
                Route::System => route_vnc_system_keys(
                    &mut app,
                    [VncSystemStroke {
                        scancode: scan,
                        extended,
                        pressed,
                    }],
                ),
                Route::Member { repeat } => {
                    let mut event = stroke(vk, scan, pressed);
                    event.extended = extended;
                    event.repeat = repeat;
                    route_vnc_chord(&mut app, event);
                }
                Route::Local => {
                    let (key, physical) = match vk {
                        0xa2 => (Key::Named(Named::Control), keyboard::key::Code::ControlLeft),
                        0xa4 => (Key::Named(Named::Alt), keyboard::key::Code::AltLeft),
                        _ => panic!("owned Enter unexpectedly reached the local window"),
                    };
                    let event = if pressed {
                        keyboard::Event::KeyPressed {
                            key: key.clone(),
                            modified_key: key,
                            physical_key: keyboard::key::Physical::Code(physical),
                            location: keyboard::Location::Standard,
                            modifiers: keyboard::Modifiers::empty(),
                            text: None,
                            repeat: false,
                        }
                    } else {
                        keyboard::Event::KeyReleased {
                            key: key.clone(),
                            modified_key: key,
                            physical_key: keyboard::key::Physical::Code(physical),
                            location: keyboard::Location::Standard,
                            modifiers: keyboard::Modifiers::empty(),
                        }
                    };
                    send_vnc_keyboard(&mut app, &event);
                }
            }
        }
        for expected in [
            (0xffeb, true),
            (0xff0d, true),
            (0xffe3, true),
            (0xffe9, true),
            (0xff0d, false),
            (0xffe9, false),
            (0xffe3, false),
            (0xffeb, false),
        ] {
            assert_eq!(
                wire.recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap(),
                expected,
                "owned Enter release was lost after Ctrl/Alt appeared"
            );
        }
        assert!(app.vnc_pressed_keys.is_empty(), "owned Enter remained held");
        assert!(
            wire.recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "extra Enter release"
        );
    }
    app.vnc_grab_keys = false;
    app.manager.close(session);
    drop(app);
    peer.join().unwrap();
}
