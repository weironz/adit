//! Exercise the actual UI dispatcher against a loopback RFB peer. Observing the
//! bytes catches cross-tab text delivery that key-state-only tests cannot see.
use super::*;
use std::io::{Read, Write};

type ObservedKey = (usize, u32, bool);

fn wire_peers() -> (
    u16,
    std::sync::mpsc::Receiver<ObservedKey>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut peers = Vec::new();
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(8)))
                .unwrap();
            let tx = tx.clone();
            peers.push(std::thread::spawn(move || {
                stream.write_all(b"RFB 003.008\n").unwrap();
                let mut version = [0; 12];
                stream.read_exact(&mut version).unwrap();
                stream.write_all(&[1, 1]).unwrap();
                let mut choice = [0];
                stream.read_exact(&mut choice).unwrap();
                assert_eq!(choice, [1]);
                stream.write_all(&[0, 0, 0, 0]).unwrap();
                stream.read_exact(&mut choice).unwrap();
                let mut init = vec![
                    0, 1, 0, 1, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
                ];
                init.extend_from_slice(&4u32.to_be_bytes());
                init.extend_from_slice(b"wire");
                stream.write_all(&init).unwrap();
                while stream.read_exact(&mut choice).is_ok() {
                    match choice[0] {
                        0 => {
                            let mut body = [0; 19];
                            if stream.read_exact(&mut body).is_err() {
                                break;
                            }
                        }
                        2 => {
                            let mut header = [0; 3];
                            stream.read_exact(&mut header).unwrap();
                            let n = usize::from(u16::from_be_bytes([header[1], header[2]]));
                            let mut body = vec![0; n * 4];
                            stream.read_exact(&mut body).unwrap();
                        }
                        3 => {
                            let mut body = [0; 9];
                            if stream.read_exact(&mut body).is_err() {
                                break;
                            }
                            if stream.write_all(&[0, 0, 0, 0]).is_err() {
                                break;
                            }
                        }
                        4 => {
                            let mut body = [0; 7];
                            if stream.read_exact(&mut body).is_err() {
                                break;
                            }
                            let symbol = u32::from_be_bytes(body[3..7].try_into().unwrap());
                            tx.send((index, symbol, body[0] != 0)).unwrap();
                        }
                        5 => {
                            let mut body = [0; 5];
                            if stream.read_exact(&mut body).is_err() {
                                break;
                            }
                        }
                        6 => {
                            let mut header = [0; 7];
                            stream.read_exact(&mut header).unwrap();
                            let n = u32::from_be_bytes(header[3..7].try_into().unwrap()) as usize;
                            assert!(n < 1024 * 1024);
                            let mut body = vec![0; n];
                            stream.read_exact(&mut body).unwrap();
                            panic!("keyboard input must not silently fall back to clipboard");
                        }
                        other => panic!("unexpected RFB client message {other}"),
                    }
                }
            }));
        }
        for peer in peers {
            peer.join().unwrap();
        }
    });
    (port, rx, worker)
}

#[test]
fn unicode_compatibility_rejects_whole_commits_without_truncation_or_owned_keys() {
    use iced::advanced::input_method::Event as Ime;
    use keyboard::key::Code;
    let (port, wire, peer) = wire_peers();
    let profiles: Vec<_> = (0..2).map(|index| {
        let mut profile = ConnectionProfile::new(format!("unicode {index}"), "127.0.0.1", port, "");
        profile.protocol = Protocol::Vnc;
        profile.vnc.security = VncSecurity::AllowNone;
        profile.vnc.auto_reconnect = false;
        if index == 0 { profile.vnc.unicode_bmp_only = true; }
        profile
    }).collect();
    assert!(!profiles[1].vnc.unicode_bmp_only, "full Unicode remains the default");
    let ids: Vec<_> = profiles.iter().map(|p| p.id).collect();
    let scratch = std::env::temp_dir().join(format!("adit-unicode-wire-{}", SessionId::new()));
    let mut app = AditApp {
        manager: SessionManager::with_profiles(profiles.clone()),
        profile_store: ProfileStore::new(scratch.join("profiles.json")),
        settings_store: SettingsStore::new(scratch.join("settings.json")),
        credential_store: CredentialStore::with_path("unicode-wire", scratch.join("credentials.json")),
        ..AditApp::default()
    };
    let mut sessions = Vec::new();
    for id in ids {
        sessions.push(app.manager.open_live_vnc_session(id, String::new()).unwrap());
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.manager.active_session_summary().unwrap().status != SessionStatus::Connected {
            app.manager.poll_events();
            assert!(Instant::now() < deadline, "Unicode peer must authenticate");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    app.main_view = MainView::Terminal;
    app.terminal_focused = true;
    // Catalogue edits cannot silently change an established transport's policy.
    let mut edited = profiles;
    edited[0].vnc.unicode_bmp_only = false;
    edited[1].vnc.unicode_bmp_only = true;
    app.manager.replace_profiles(edited);
    app.manager.activate(sessions[0]).unwrap();
    let epoch = app.vnc_input_epoch;
    let _ = update(&mut app, Message::VncImeEvent(sessions[0], epoch, Ime::Commit("前🙂后".into())));
    assert!(app.notice.contains("整段未发送"));
    let mut composed = press(Key::Character("a".into()), Code::KeyA, keyboard::Modifiers::empty());
    if let Message::KeyboardInput(keyboard::Event::KeyPressed { text, .. }) = &mut composed {
        *text = Some("prefix𠀀suffix".into());
    }
    let _ = update(&mut app, composed);
    let _ = update(&mut app, release(Key::Character("a".into()), Code::KeyA));
    let _ = update(&mut app, press(Key::Character("🙂".into()), Code::KeyB, keyboard::Modifiers::empty()));
    assert!(app.vnc_pressed_keys.is_empty(), "rejected scalar must not own a remote up");
    let _ = update(&mut app, release(Key::Character("🙂".into()), Code::KeyB));
    // Cover unmapped physical text fallback separately from the string branch.
    let mut fallback = press(Key::Unidentified, Code::KeyC, keyboard::Modifiers::empty());
    if let Message::KeyboardInput(keyboard::Event::KeyPressed { text, .. }) = &mut fallback {
        *text = Some("🙂".into());
    }
    let _ = update(&mut app, fallback);
    assert!(app.vnc_pressed_keys.is_empty());
    // Direct API callers cannot bypass the per-connection snapshot either.
    let emoji = adit_session::unicode_keysym('🙂');
    app.manager.send_vnc_key_to_active(emoji, true);
    app.manager.send_vnc_key_to_active(emoji, false);
    app.manager.send_rdp_input_to_active(RdpInput::Unicode { ch: '🙂', pressed: true });
    app.manager.send_rdp_input_to_active(RdpInput::Unicode { ch: '🙂', pressed: false });
    assert!(app.manager.send_vnc_text_to_active("before🙂after").is_err());
    app.manager.poll_events();
    assert!(app.manager.take_desktop_notice().is_some_and(|notice| notice.contains("整段未发送")),
        "direct Unicode callers must produce a visible nonfatal notice");
    // A one-scalar TEXT payload must be checked even if modified_key differs.
    let mut scalar_text = press(Key::Character("d".into()), Code::KeyD, keyboard::Modifiers::empty());
    if let Message::KeyboardInput(keyboard::Event::KeyPressed { text, .. }) = &mut scalar_text {
        *text = Some("🙂".into());
    }
    let _ = update(&mut app, scalar_text);
    let _ = update(&mut app, release(Key::Character("d".into()), Code::KeyD));
    assert!(app.vnc_pressed_keys.is_empty());
    // The byte cap applies before queueing any valid prefix, independently of
    // Unicode compatibility. The scalar count fits; the UTF-8 byte count does not.
    let oversized = "中".repeat(1366);
    assert_eq!(oversized.len(), 4098);
    let _ = update(&mut app, Message::VncImeEvent(sessions[0], epoch, Ime::Commit(oversized.clone())));
    assert!(app.notice.contains("4 KiB") && app.notice.contains("整段未发送"));
    assert!(app.vnc_pressed_keys.is_empty());
    let _ = update(&mut app, press(Key::Character("é".into()), Code::KeyE, keyboard::Modifiers::empty()));
    let _ = update(&mut app, release(Key::Character("é".into()), Code::KeyE));
    let _ = update(&mut app, press(Key::Named(Named::Tab), Code::Tab, keyboard::Modifiers::empty()));
    let _ = update(&mut app, release(Key::Named(Named::Tab), Code::Tab));
    let bmp = "中文e\u{301}\u{ffff}☃";
    let _ = update(&mut app, Message::VncImeEvent(sessions[0], epoch, Ime::Commit(bmp.into())));
    app.manager.activate(sessions[1]).unwrap();
    let _ = update(&mut app, Message::VncImeEvent(sessions[1], epoch, Ime::Commit(oversized)));
    assert!(app.notice.contains("4 KiB") && app.notice.contains("整段未发送"));
    let full = "🙂\u{10000}𠀀\u{10ffff}☃";
    let _ = update(&mut app, Message::VncImeEvent(sessions[1], epoch, Ime::Commit(full.into())));
    let mut observed = [Vec::new(), Vec::new()];
    let mut barriers = [false; 2];
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while barriers != [true, true] {
        let (index, symbol, pressed) = wire.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("per-peer Unicode barrier");
        observed[index].push((symbol, pressed));
        if symbol == adit_session::unicode_keysym('☃') && !pressed { barriers[index] = true; }
    }
    for (index, text) in [bmp, full].into_iter().enumerate() {
        let mut expected = if index == 0 {
            vec![(0xe9, true), (0xe9, false), (0xff09, true), (0xff09, false)]
        } else { Vec::new() };
        expected.extend(text.chars().flat_map(|ch| {
            let symbol = adit_session::unicode_keysym(ch);
            [(symbol, true), (symbol, false)]
        }));
        assert_eq!(observed[index], expected, "no rejected prefix, truncation, surrogate replacement or orphan up");
    }
    for session in sessions { app.manager.close(session); }
    peer.join().unwrap();
}

fn press(key: Key, physical: keyboard::key::Code, modifiers: keyboard::Modifiers) -> Message {
    Message::KeyboardInput(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(physical),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    })
}

fn release(key: Key, physical: keyboard::key::Code) -> Message {
    Message::KeyboardInput(keyboard::Event::KeyReleased {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(physical),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
    })
}

#[test]
fn ime_and_local_focus_changes_preserve_remote_input_ownership() {
    use iced::advanced::input_method::Event as Ime;
    use keyboard::key::Code;
    let (port, wire, peer) = wire_peers();
    let profiles: Vec<_> = (0..2)
        .map(|index| {
            let mut profile =
                ConnectionProfile::new(format!("input {index}"), "127.0.0.1", port, "");
            profile.protocol = Protocol::Vnc;
            profile.vnc.security = VncSecurity::AllowNone;
            profile.vnc.auto_reconnect = false;
            profile
        })
        .collect();
    let ids: Vec<_> = profiles.iter().map(|p| p.id).collect();
    let scratch = std::env::temp_dir().join(format!("adit-input-wire-{}", SessionId::new()));
    let mut app = AditApp {
        manager: SessionManager::with_profiles(profiles),
        profile_store: ProfileStore::new(scratch.join("profiles.json")),
        settings_store: SettingsStore::new(scratch.join("settings.json")),
        credential_store: CredentialStore::with_path(
            "input-wire",
            scratch.join("credentials.json"),
        ),
        ..AditApp::default()
    };
    let first = app
        .manager
        .open_live_vnc_session(ids[0], String::new())
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while app.manager.active_session_summary().unwrap().status != SessionStatus::Connected {
        app.manager.poll_events();
        assert!(
            Instant::now() < deadline,
            "first RFB peer must authenticate"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let second = app
        .manager
        .open_live_vnc_session(ids[1], String::new())
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while app
        .manager
        .sessions()
        .iter()
        .any(|session| session.status != SessionStatus::Connected)
    {
        app.manager.poll_events();
        assert!(
            Instant::now() < deadline,
            "both RFB peers must authenticate"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    app.main_view = MainView::Terminal;
    app.terminal_focused = true;
    // The symbol at press time owns release even after Shift changes. Exercise
    // the real transport: a mock session must never acquire remote ownership.
    let shifted = press(Key::Character("A".into()), Code::KeyA, keyboard::Modifiers::SHIFT);
    let _ = update(&mut app, shifted.clone());
    assert_eq!(app.vnc_pressed_keys, vec![(keyboard::key::Physical::Code(Code::KeyA), 0x41)]);
    let _ = update(&mut app, release(Key::Character("a".into()), Code::KeyA));
    assert!(app.vnc_pressed_keys.is_empty());
    let _ = update(&mut app, shifted);
    assert_eq!(app.vnc_pressed_keys, vec![(keyboard::key::Physical::Code(Code::KeyA), 0x41)]);
    let _ = update(&mut app, Message::WindowUnfocused);
    assert!(app.vnc_pressed_keys.is_empty());
    let _ = update(&mut app, Message::WindowFocused);
    // Local layout/OS shortcuts must not leak a bare Super into the peer,
    // even if Windows delivers a constituent letter to this window.
    for physical in [Code::SuperLeft, Code::SuperRight] {
        let _ = update(
            &mut app,
            press(
                Key::Named(Named::Super),
                physical,
                keyboard::Modifiers::LOGO,
            ),
        );
        let _ = update(
            &mut app,
            press(
                Key::Character("r".into()),
                Code::KeyR,
                keyboard::Modifiers::LOGO,
            ),
        );
        let _ = update(&mut app, release(Key::Character("r".into()), Code::KeyR));
        let _ = update(&mut app, release(Key::Named(Named::Super), physical));
    }
    let _ = update(&mut app, release(Key::Character("q".into()), Code::KeyQ));
    // Windows dead-key composition can emit several scalars in one key event,
    // then a normal physical release. Its transient pairs already own key-up.
    let mut composed = press(
        Key::Character("e".into()),
        Code::KeyE,
        keyboard::Modifiers::empty(),
    );
    if let Message::KeyboardInput(keyboard::Event::KeyPressed { text, .. }) = &mut composed {
        *text = Some("e\u{301}".into());
    }
    let _ = update(&mut app, composed);
    let _ = update(&mut app, release(Key::Character("e".into()), Code::KeyE));
    // A real owned release must still reach the peer while a local Win shortcut
    // is held; filtering the whole event by modifiers would leave Q stuck.
    let _ = update(
        &mut app,
        press(
            Key::Character("q".into()),
            Code::KeyQ,
            keyboard::Modifiers::empty(),
        ),
    );
    let mut owned_release = release(Key::Character("q".into()), Code::KeyQ);
    if let Message::KeyboardInput(keyboard::Event::KeyReleased { modifiers, .. }) =
        &mut owned_release
    {
        *modifiers = keyboard::Modifiers::LOGO;
    }
    let _ = update(&mut app, owned_release);
    let epoch = app.vnc_input_epoch;
    let _ = update(
        &mut app,
        Message::VncImeEvent(first, epoch, Ime::Commit("错".into())),
    );
    let text = "中文🙂e\u{301}";
    let _ = update(
        &mut app,
        Message::VncImeEvent(second, epoch, Ime::Commit(text.into())),
    );
    let _ = update(&mut app, Message::ToggleMenu(MenuKind::File));
    let _ = update(&mut app, release(Key::Named(Named::Shift), Code::ShiftLeft));
    let _ = update(&mut app, Message::ToggleMenu(MenuKind::File));
    let _ = update(
        &mut app,
        Message::VncImeEvent(second, epoch, Ime::Commit("无预编辑的旧提交".into())),
    );
    let epoch = app.vnc_input_epoch;
    let _ = update(
        &mut app,
        press(
            Key::Named(Named::Shift),
            Code::ShiftLeft,
            keyboard::Modifiers::SHIFT,
        ),
    );
    let _ = update(&mut app, Message::ToggleMenu(MenuKind::File));
    assert!(
        app.vnc_pressed_keys.is_empty(),
        "a local menu must release remote input"
    );
    assert_ne!(app.vnc_input_epoch, epoch);
    let _ = update(
        &mut app,
        press(
            Key::Character("a".into()),
            Code::KeyA,
            keyboard::Modifiers::empty(),
        ),
    );
    let _ = update(&mut app, Message::ToggleMenu(MenuKind::File));
    let _ = update(
        &mut app,
        Message::VncImeEvent(second, epoch, Ime::Commit("旧".into())),
    );
    let _ = update(&mut app, Message::WindowUnfocused);
    let _ = update(
        &mut app,
        press(
            Key::Character("a".into()),
            Code::KeyA,
            keyboard::Modifiers::empty(),
        ),
    );
    let background_epoch = app.vnc_input_epoch;
    let _ = update(
        &mut app,
        Message::VncImeEvent(second, background_epoch, Ime::Commit("后台".into())),
    );
    let _ = update(&mut app, Message::WindowFocused);
    let _ = update(&mut app, release(Key::Named(Named::Shift), Code::ShiftLeft));
    // Model a proven LL repair witness while a Win held before capture remains
    // local. Repaired Ctrl must use the RightAlt event's same logo eligibility.
    for extended in [false, true] {
        forward_filtered_vnc_control(
            &mut app,
            keyboard_hook::vnc_order::ModifierEdge {
                scan: 0x1d,
                extended,
                pressed: true,
            },
            keyboard::Modifiers::LOGO,
        );
    }
    assert!(
        app.vnc_pressed_keys.is_empty(),
        "local preheld Win must suppress repaired Ctrl downs"
    );
    // A capture-mode auxiliary Ctrl must neither add a duplicate down nor
    // release an already remote-owned real Ctrl. No current hook/fence exists
    // in this fixture, so both exposed auxiliary window edges are unowned.
    let _ = update(
        &mut app,
        press(
            Key::Named(Named::Control),
            Code::ControlLeft,
            keyboard::Modifiers::CTRL,
        ),
    );
    app.vnc_grab_keys = true;
    let _ = update(
        &mut app,
        press(
            Key::Named(Named::Control),
            Code::ControlLeft,
            keyboard::Modifiers::CTRL,
        ),
    );
    let _ = update(
        &mut app,
        release(Key::Named(Named::Control), Code::ControlLeft),
    );
    // RightAlt alone cannot supply a missing real Ctrl witness. In particular,
    // an unarmed/focus-retired hook must never repair an invented Ctrl edge.
    let _ = update(
        &mut app,
        press(
            Key::Named(Named::AltGraph),
            Code::AltRight,
            keyboard::Modifiers::ALT,
        ),
    );
    let _ = update(
        &mut app,
        release(Key::Named(Named::AltGraph), Code::AltRight),
    );
    assert!(
        app.vnc_pressed_keys
            .iter()
            .any(|(physical, symbol)| *physical
                == keyboard::key::Physical::Code(Code::ControlLeft)
                && *symbol == 0xffe3)
    );
    forward_filtered_vnc_control(
        &mut app,
        keyboard_hook::vnc_order::ModifierEdge {
            scan: 0x1d,
            extended: false,
            pressed: false,
        },
        keyboard::Modifiers::LOGO,
    );
    assert!(
        app.vnc_pressed_keys.is_empty(),
        "logo must not suppress a real owned Ctrl release"
    );
    app.vnc_grab_keys = false;
    let _ = update(
        &mut app,
        release(Key::Named(Named::Control), Code::ControlLeft),
    );
    // A toolbar fullscreen switch shares the shipping dispatcher. Build the
    // captured-state precondition without installing a global hook or injecting
    // any Windows key; the peer must observe the release before the next key.
    app.vnc_grab_keys = true;
    let _ = update(
        &mut app,
        press(
            Key::Named(Named::Shift),
            Code::ShiftLeft,
            keyboard::Modifiers::SHIFT,
        ),
    );
    let _ = update(&mut app, Message::ToggleFullscreen);
    assert!(app.fullscreen);
    assert!(
        !app.vnc_grab_keys,
        "fullscreen must not retain a disarmed VNC grab"
    );
    assert!(app.vnc_pressed_keys.is_empty());
    assert!(keyboard_hook::drain().is_empty());
    app.vnc_grab_keys = true;
    let _ = update(
        &mut app,
        press(
            Key::Named(Named::Control),
            Code::ControlRight,
            keyboard::Modifiers::CTRL,
        ),
    );
    let _ = update(&mut app, Message::ToggleFullscreen);
    assert!(!app.fullscreen);
    assert!(!app.vnc_grab_keys);
    assert!(app.vnc_pressed_keys.is_empty());
    assert!(keyboard_hook::drain().is_empty());
    let stroke = |scancode, pressed| VncSystemStroke {
        scancode,
        extended: scancode != 0x0f,
        pressed,
    };
    route_vnc_system_keys(
        &mut app,
        [
            stroke(0x0f, false),
            stroke(0x5b, false),
            stroke(0x5c, false),
        ],
    );
    // Repeat key-downs stay on the wire, while one physical ownership record
    // remains responsible for the eventual release.
    route_vnc_system_keys(
        &mut app,
        [stroke(0x0f, true), stroke(0x0f, true), stroke(0x0f, false)],
    );
    assert!(app.vnc_pressed_keys.is_empty());
    route_vnc_system_keys(&mut app, [stroke(0x5c, true), stroke(0x5c, false)]);
    assert!(app.vnc_pressed_keys.is_empty());
    app.vnc_grab_keys = true;
    route_vnc_system_keys(&mut app, [stroke(0x0f, true), stroke(0x5b, true)]);
    assert_eq!(app.vnc_pressed_keys.len(), 2);
    let _ = update(&mut app, Message::ToggleFullscreen);
    assert!(app.fullscreen);
    assert!(!app.vnc_grab_keys);
    assert!(app.vnc_pressed_keys.is_empty());
    // A release captured at the old grab boundary cannot release anyone else's
    // key after the fullscreen path already released our own tracked downs.
    route_vnc_system_keys(&mut app, [stroke(0x0f, false), stroke(0x5b, false)]);
    let current_epoch = app.vnc_input_epoch;
    let _ = update(
        &mut app,
        Message::VncImeEvent(second, current_epoch, Ime::Commit("☃".into())),
    );
    let mut observed = Vec::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let key = wire
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("wire barrier");
        observed.push(key);
        if key.1 == adit_session::unicode_keysym('☃') && !key.2 {
            break;
        }
    }
    let mut expected: Vec<_> = "AAe\u{301}q"
        .chars()
        .chain(text.chars())
        .flat_map(|ch| {
            let symbol = adit_session::unicode_keysym(ch);
            [(1, symbol, true), (1, symbol, false)]
        })
        .collect();
    expected.extend([
        (1, 0xffe1, true),
        (1, 0xffe1, false),
        (1, 0xffe3, true),
        (1, 0xfe03, true),
        (1, 0xfe03, false),
        (1, 0xffe3, false),
        (1, 0xffe1, true),
        (1, 0xffe1, false),
        (1, 0xffe4, true),
        (1, 0xffe4, false),
        (1, 0xff09, true),
        (1, 0xff09, true),
        (1, 0xff09, false),
        (1, 0xffec, true),
        (1, 0xffec, false),
        (1, 0xff09, true),
        (1, 0xffeb, true),
        (1, 0xff09, false),
        (1, 0xffeb, false),
        (1, adit_session::unicode_keysym('☃'), true),
        (1, adit_session::unicode_keysym('☃'), false),
    ]);
    assert_eq!(
        observed, expected,
        "old-tab, stale composition, menu and background keys must not reach either server"
    );
    app.manager.close(first);
    app.manager.close(second);
    peer.join().unwrap();
}
