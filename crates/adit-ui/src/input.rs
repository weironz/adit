//! Turning iced input events into bytes the remote end understands, and
//! recognising the few chords Adit keeps for itself.
//!
//! Pure decoding: nothing here reads or writes app state. Anything a shortcut
//! matches is a key sequence the terminal never gets to see, so the predicates
//! deliberately stay narrow.

use super::*;

/// Encode a mouse event as an xterm report. `button`: 0=left, 1=middle,
/// 2=right, 3=none/release, 64=wheel-up, 65=wheel-down. `col`/`row` are 0-based
/// cells; `press` is the button-down (or wheel) edge; `motion` marks a drag.
pub(super) fn encode_mouse_event(sgr: bool, button: u8, col: usize, row: usize, press: bool, motion: bool) -> Vec<u8> {
    let mut cb = u32::from(button);
    if motion {
        cb += 32;
    }
    let cx = col + 1;
    let cy = row + 1;

    if sgr {
        let terminator = if press { 'M' } else { 'm' };
        format!("\x1b[<{cb};{cx};{cy}{terminator}").into_bytes()
    } else {
        // Legacy X10: ESC [ M  (Cb+32)  (Cx+32)  (Cy+32), coords capped at 223.
        let cb_byte = (cb + 32).min(255) as u8;
        let cx_byte = (32 + cx.min(223)) as u8;
        let cy_byte = (32 + cy.min(223)) as u8;
        vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
    }
}

pub(super) fn is_enter_key(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: Key::Named(Named::Enter),
            ..
        }
    )
}

pub(super) fn is_escape_key(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        }
    )
}

pub(super) fn is_terminal_copy_shortcut(event: &keyboard::Event) -> bool {
    terminal_shortcut(event, 'c')
}

pub(super) fn is_terminal_paste_shortcut(event: &keyboard::Event) -> bool {
    terminal_shortcut(event, 'v')
}

pub(super) fn terminal_shortcut(event: &keyboard::Event, key: char) -> bool {
    let keyboard::Event::KeyPressed {
        key: logical_key,
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return false;
    };

    modifiers.control()
        && modifiers.shift()
        && logical_key
            .to_latin(*physical_key)
            .is_some_and(|pressed| pressed.eq_ignore_ascii_case(&key))
}

/// A plain Alt+<key> app shortcut (SecureCRT-style, e.g. Alt+P opens SFTP).
/// Matched via the physical key so it works on non-Latin layouts. Alt normally
/// prefixes terminal input with ESC (Meta), so this deliberately claims the combo
/// before the terminal sees it.
/// Ctrl+<key> with no Shift and no Alt — the sidebar's copy/paste pairing.
///
/// Distinct from `terminal_shortcut`, which requires Shift: in a terminal
/// Ctrl+C is SIGINT and can never be taken, so copy there is Ctrl+Shift+C. On a
/// list of sessions there is no such conflict, and plain Ctrl+C/V is what every
/// file manager uses for the same gesture.
pub(super) fn plain_ctrl_shortcut(event: &keyboard::Event, key: char) -> bool {
    let keyboard::Event::KeyPressed {
        key: logical_key,
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return false;
    };

    modifiers.control()
        && !modifiers.shift()
        && !modifiers.alt()
        && logical_key
            .to_latin(*physical_key)
            .is_some_and(|ch| ch.eq_ignore_ascii_case(&key))
}
pub(super) fn alt_shortcut(event: &keyboard::Event, key: char) -> bool {
    let keyboard::Event::KeyPressed {
        key: logical_key,
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return false;
    };

    modifiers.alt()
        && !modifiers.control()
        && !modifiers.shift()
        && logical_key
            .to_latin(*physical_key)
            .is_some_and(|pressed| pressed.eq_ignore_ascii_case(&key))
}

pub(super) fn terminal_scroll_shortcut(
    event: &keyboard::Event,
    visible_rows: u16,
) -> Option<TerminalScrollAction> {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return None;
    };

    if !modifiers.shift() {
        return None;
    }

    let Key::Named(named) = key else {
        return None;
    };

    let page = i32::from(visible_rows.saturating_sub(1).max(1));
    match *named {
        Named::PageUp => Some(TerminalScrollAction::Lines(page)),
        Named::PageDown => Some(TerminalScrollAction::Lines(-page)),
        Named::Home if modifiers.control() => Some(TerminalScrollAction::Top),
        Named::End if modifiers.control() => Some(TerminalScrollAction::Bottom),
        _ => None,
    }
}

pub(super) fn normalize_paste(contents: &str) -> Vec<u8> {
    contents
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r")
        .into_bytes()
}

/// Map an iced keyboard event to an RDP scancode press/release.
/// Returns `(scancode, extended, pressed)` using PC/AT set-1 make codes; the
/// `extended` flag marks the E0 set (right-side modifiers, nav cluster, arrows,
/// numpad Enter/Divide, Windows/Menu keys). Layout-independent: it keys off the
/// physical key, so the remote applies its own layout.
pub(super) fn encode_rdp_scancode(event: &keyboard::Event) -> Option<(u8, bool, bool)> {
    let (physical, pressed) = match event {
        keyboard::Event::KeyPressed { physical_key, .. } => (physical_key, true),
        keyboard::Event::KeyReleased { physical_key, .. } => (physical_key, false),
        _ => return None,
    };
    let keyboard::key::Physical::Code(code) = physical else {
        return None;
    };
    let (scancode, extended) = rdp_scancode_for_code(*code)?;
    Some((scancode, extended, pressed))
}

/// Local overlays and text fields own input even when their ignored keys bubble
/// to the runtime. Keep this VNC-only so the existing RDP focus contract stays intact.
pub(crate) fn vnc_accepts_text_input(app: &AditApp) -> bool {
    app.manager.active_is_vnc()
        && app.manager.active_session_summary().is_some_and(|s| s.status == SessionStatus::Connected)
        && !app.manager.active_vnc_read_only()
        && app.desktop_window_focused
        && app.terminal_focused
        && app.main_view == MainView::Terminal
        && app.active_menu.is_none()
        && !app.settings_open
        && app.profile_editor.is_none()
        && app.connection_dialog.is_none()
        && app.manager.pending_host_key().is_none()
        && app.auth_prompt.is_none()
        && app.pending_hyperlink.is_none()
        && !app.manager.sftp_is_open()
        && !app.tunnels_open
        && !app.about_open
        && !app.update_dialog_open
        && !app.paste_confirm_open
        && app.renaming_session.is_none()
        && !app.snippets_open
        && !app.known_hosts_open
        && !app.command_window_open
        && !app.search_open
        && app.editing_profile.is_none()
        && app.editing_group.is_none()
}

/// VNC sends logical X11 keysyms. Keep the symbol chosen at key-down so a
/// modifier/layout change before key-up cannot leave the original symbol held.
pub(super) fn send_vnc_keyboard(app: &mut AditApp, event: &keyboard::Event) {
    if app.manager.active_vnc_read_only() { return; }
    if let keyboard::Event::KeyPressed { text: Some(text), modifiers, .. } = event {
        // A composition commit is a string, not one key. Send all scalars,
        // including combining marks, without leaving a synthetic key held.
        if text.chars().count() > 1 && !modifiers.control() && !modifiers.alt() && !modifiers.logo() {
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                let symbol = adit_session::unicode_keysym(ch);
                app.manager.send_vnc_key_to_active(symbol, true);
                app.manager.send_vnc_key_to_active(symbol, false);
            }
            return;
        }
    }
    let (key, physical, pressed, modifiers) = match event {
        keyboard::Event::KeyPressed {
            modified_key, physical_key, modifiers, ..
        } => (modified_key, vnc_window_physical(*physical_key), true, modifiers),
        keyboard::Event::KeyReleased {
            modified_key, physical_key, modifiers, ..
        } => (modified_key, vnc_window_physical(*physical_key), false, modifiers),
        _ => return,
    };
    // Owned Win combinations now come entirely through the hook, before IME.
    // A window-delivered logo press belongs to the local OS (including a Win
    // held before this grab). Its leftover character must not leak remotely.
    // Owned releases below must still pass, even while a local shortcut is held.
    if pressed
        && (modifiers.logo() || matches!(physical, keyboard::key::Physical::Code(
            keyboard::key::Code::SuperLeft | keyboard::key::Code::SuperRight))) {
        return;
    }
    let tracked = app
        .vnc_pressed_keys
        .iter()
        .position(|(code, _)| *code == physical);
    // A local field may have owned the press, or focus cleanup / composition
    // may already have released it. Do not manufacture an unowned key-up on a
    // shared desktop from the current layout's logical symbol.
    if !pressed && tracked.is_none() {
        return;
    }
    let keysym = if let Some(index) = tracked {
        let keysym = app.vnc_pressed_keys[index].1;
        if !pressed {
            app.vnc_pressed_keys.remove(index);
        }
        Some(keysym)
    } else {
        vnc_keysym(key, physical)
    };
    if let Some(keysym) = keysym {
        if pressed && tracked.is_none() {
            app.vnc_pressed_keys.push((physical, keysym));
        }
        app.manager.send_vnc_key_to_active(keysym, pressed);
    } else if let keyboard::Event::KeyPressed {
        text: Some(text), ..
    } = event
    {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            let keysym = adit_session::unicode_keysym(ch);
            app.manager.send_vnc_key_to_active(keysym, true);
            app.manager.send_vnc_key_to_active(keysym, false);
        }
    }
}

fn vnc_window_physical(physical: keyboard::key::Physical) -> keyboard::key::Physical {
    use keyboard::key::{Code, NativeCode, Physical};
    // winit leaves E0-Shift as NativeCode::Windows instead of a Shift keycode.
    // Normalize the window identity too: normalizing only the low-level hook
    // still stranded its fence and lost the ordinary Shift down/up on the wire.
    match physical {
        Physical::Unidentified(NativeCode::Windows(0xe02a)) => Physical::Code(Code::ShiftLeft),
        Physical::Unidentified(NativeCode::Windows(0xe036)) => Physical::Code(Code::ShiftRight),
        _ => physical,
    }
}

fn vnc_keysym(key: &Key, physical: keyboard::key::Physical) -> Option<u32> {
    use keyboard::key::{Code, Physical};
    let physical = vnc_window_physical(physical);
    // Distinguish right-hand modifiers even when the logical key is just Alt.
    let modifier = match physical {
        Physical::Code(Code::ShiftLeft) => Some(0xffe1),
        Physical::Code(Code::ShiftRight) => Some(0xffe2),
        Physical::Code(Code::ControlLeft) => Some(0xffe3),
        Physical::Code(Code::ControlRight) => Some(0xffe4),
        Physical::Code(Code::AltLeft) => Some(0xffe9),
        Physical::Code(Code::AltRight) => Some(if matches!(key, Key::Named(Named::AltGraph)) { 0xfe03 } else { 0xffea }),
        Physical::Code(Code::SuperLeft) => Some(0xffeb),
        Physical::Code(Code::SuperRight) => Some(0xffec),
        _ => None,
    };
    if modifier.is_some() {
        return modifier;
    }
    Some(match key {
        Key::Character(text) => {
            let mut chars = text.chars();
            let first = chars.next()?;
            if chars.next().is_some() { return None; }
            adit_session::unicode_keysym(first)
        },
        Key::Named(named) => match named {
            Named::Backspace => 0xff08,
            Named::Tab => 0xff09,
            Named::Enter => 0xff0d,
            Named::Escape => 0xff1b,
            Named::Delete => 0xffff,
            Named::Home => 0xff50,
            Named::ArrowLeft => 0xff51,
            Named::ArrowUp => 0xff52,
            Named::ArrowRight => 0xff53,
            Named::ArrowDown => 0xff54,
            Named::PageUp => 0xff55,
            Named::PageDown => 0xff56,
            Named::End => 0xff57,
            Named::Insert => 0xff63,
            Named::ContextMenu => 0xff67,
            Named::CapsLock => 0xffe5,
            Named::NumLock => 0xff7f,
            Named::ScrollLock => 0xff14,
            Named::Pause => 0xff13,
            Named::PrintScreen => 0xff61,
            Named::Space => 0x20,
            Named::F1 => 0xffbe,
            Named::F2 => 0xffbf,
            Named::F3 => 0xffc0,
            Named::F4 => 0xffc1,
            Named::F5 => 0xffc2,
            Named::F6 => 0xffc3,
            Named::F7 => 0xffc4,
            Named::F8 => 0xffc5,
            Named::F9 => 0xffc6,
            Named::F10 => 0xffc7,
            Named::F11 => 0xffc8,
            Named::F12 => 0xffc9,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod vnc_tests {
    use super::*;
    use keyboard::key::{Code, Physical};

    #[test]
    fn native_extended_shift_alias_matches_window_tracking_and_wire_symbol() {
        use keyboard::key::NativeCode;
        for (scan, code, symbol) in [(0xe02a, Code::ShiftLeft, 0xffe1), (0xe036, Code::ShiftRight, 0xffe2)] {
            let native = Physical::Unidentified(NativeCode::Windows(scan));
            assert_eq!(vnc_window_physical(native), Physical::Code(code));
            assert_eq!(vnc_keysym(&Key::Unidentified, native), Some(symbol));
        }
        let other = Physical::Unidentified(NativeCode::Windows(0xe01d));
        assert_eq!(vnc_window_physical(other), other);
    }

    #[test]
    fn vnc_uses_logical_text_and_distinguishes_right_modifiers() {
        assert_eq!(
            vnc_keysym(&Key::Character("A".into()), Physical::Code(Code::KeyA)),
            Some(0x41)
        );
        assert_eq!(
            vnc_keysym(&Key::Character("中".into()), Physical::Code(Code::KeyA)),
            Some(0x0100_4e2d)
        );
        assert_eq!(
            vnc_keysym(
                &Key::Named(Named::Control),
                Physical::Code(Code::ControlRight)
            ),
            Some(0xffe4)
        );
        assert_eq!(
            vnc_keysym(&Key::Named(Named::Delete), Physical::Code(Code::Delete)),
            Some(0xffff)
        );
        assert_eq!(
            vnc_keysym(&Key::Named(Named::F12), Physical::Code(Code::F12)),
            Some(0xffc9)
        );
    }
}

#[cfg(test)]
mod vnc_input_tests {
    use super::*;
    #[test]
    fn composition_is_never_truncated_to_the_first_character() {
        assert_eq!(vnc_keysym(&Key::Character("中文🙂".into()), keyboard::key::Physical::Code(keyboard::key::Code::KeyA)), None);
        assert_eq!(vnc_keysym(&Key::Character("e\u{301}".into()), keyboard::key::Physical::Code(keyboard::key::Code::KeyE)), None);
    }
    #[test]
    fn altgr_and_right_alt_keep_distinct_symbols() {
        let key = keyboard::key::Physical::Code(keyboard::key::Code::AltRight);
        assert_eq!(vnc_keysym(&Key::Named(Named::AltGraph), key), Some(0xfe03));
        assert_eq!(vnc_keysym(&Key::Named(Named::Alt), key), Some(0xffea));
    }
}

/// Merge selectively swallowed system keys before the next ordinary window key.
pub(crate) fn drain_vnc_system_keys(app: &mut AditApp) {
    drain_vnc_keys_after_modifier(app, None);
}

/// Acknowledge only after the ordinary window modifier has reached the wire.
pub(crate) fn finish_vnc_window_key(app: &mut AditApp, event: &keyboard::Event) {
    use keyboard::key::{Code, Physical};
    let (physical, pressed) = match event {
        keyboard::Event::KeyPressed { physical_key, .. } => (*physical_key, true),
        keyboard::Event::KeyReleased { physical_key, .. } => (*physical_key, false),
        _ => return,
    };
    let (scan, extended) = match vnc_window_physical(physical) {
        Physical::Code(Code::ShiftLeft) => (0x2a, false),
        Physical::Code(Code::ShiftRight) => (0x36, false),
        Physical::Code(Code::ControlLeft) => (0x1d, false),
        Physical::Code(Code::ControlRight) => (0x1d, true),
        Physical::Code(Code::AltLeft) => (0x38, false),
        Physical::Code(Code::AltRight) => (0x38, true),
        _ => return,
    };
    if app.vnc_grab_keys {
        drain_vnc_keys_after_modifier(app, Some(keyboard_hook::vnc_order::ModifierEdge {
            scan, extended, pressed,
        }));
    }
}

fn drain_vnc_keys_after_modifier(app: &mut AditApp, acknowledged: Option<keyboard_hook::vnc_order::ModifierEdge>) {
    let (strokes, timed_out) = keyboard_hook::drain_vnc(acknowledged);
    if timed_out {
        release_rdp_modifiers(app);
        app.notice = "VNC：窗口修饰键同步超时，已释放系统键抓取".into();
        return;
    }
    for stroke in strokes {
        if let Some(chord) = stroke.chord {
            route_vnc_chord(app, chord);
        } else {
            route_vnc_system_keys(
                app,
                std::iter::once(VncSystemStroke {
                    scancode: stroke.scancode,
                    extended: stroke.extended,
                    pressed: stroke.pressed,
                }),
            );
        }
    }
}

fn route_vnc_chord(app: &mut AditApp, stroke: keyboard_hook::vnc_chord::Stroke) {
    if !app.vnc_grab_keys || !vnc_accepts_text_input(app) {
        return;
    }
    // This identity describes the captured physical press, not a US letter.
    // A layout change while held must still release the original keysym.
    let physical = keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Windows(
        u16::from(stroke.scan) | if stroke.extended { 0xe000 } else { 0 },
    ));
    let held = app
        .vnc_pressed_keys
        .iter()
        .position(|(key, _)| *key == physical);
    let symbol = if let Some(index) = held {
        let symbol = app.vnc_pressed_keys[index].1;
        if !stroke.pressed {
            app.vnc_pressed_keys.remove(index);
        }
        symbol
    } else if !stroke.pressed || stroke.repeat {
        // Unsupported presses and pre-existing local presses own no remote up.
        return;
    } else {
        match keyboard_hook::vnc_chord::translate(&stroke) {
            Ok(symbol) => {
                app.vnc_pressed_keys.push((physical, symbol));
                symbol
            }
            Err(reason) => {
                app.notice = "VNC：当前布局的此系统组合字符不支持（未映射、死键或多字符），字符未发送；此前发送的 Win 键仍按原顺序释放".into();
                // One diagnostic per physical press, with no typed text. The
                // repeat/up branches above cannot flood logs or invent key-ups.
                eprintln!("VNC system chord translation unavailable: {reason}");
                return;
            }
        }
    };
    app.manager.send_vnc_key_to_active(symbol, stroke.pressed);
}

/// Value-only boundary so routing can be verified without a global OS hook.
pub(crate) struct VncSystemStroke {
    pub scancode: u8,
    pub extended: bool,
    pub pressed: bool,
}

pub(crate) fn route_vnc_system_keys(
    app: &mut AditApp,
    strokes: impl IntoIterator<Item = VncSystemStroke>,
) {
    use keyboard::key::{Code, Physical};
    for stroke in strokes {
        let key = match (stroke.scancode, stroke.extended) {
            (0x0f, _) => Some((0xff09, Physical::Code(Code::Tab))),
            (0x5b, true) => Some((0xffeb, Physical::Code(Code::SuperLeft))),
            (0x5c, true) => Some((0xffec, Physical::Code(Code::SuperRight))),
            _ => None,
        };
        if let Some((symbol, physical)) = key {
            if stroke.pressed {
                if !app
                    .vnc_pressed_keys
                    .iter()
                    .any(|(code, _)| *code == physical)
                {
                    app.vnc_pressed_keys.push((physical, symbol));
                }
            } else if let Some(index) = app
                .vnc_pressed_keys
                .iter()
                .position(|(code, _)| *code == physical)
            {
                app.vnc_pressed_keys.remove(index);
            } else {
                // The press may predate this grab and belong to a local app.
                // Shared VNC desktops must not receive its orphaned release.
                continue;
            }
            app.manager.send_vnc_key_to_active(symbol, stroke.pressed);
        }
    }
}

#[cfg(all(test, windows))]
#[path = "vnc_chord_tests.rs"]
mod vnc_chord_tests;
