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

/// VNC sends logical X11 keysyms. Keep the symbol chosen at key-down so a
/// modifier/layout change before key-up cannot leave the original symbol held.
pub(super) fn send_vnc_keyboard(app: &mut AditApp, event: &keyboard::Event) {
    let (key, physical, pressed) = match event {
        keyboard::Event::KeyPressed {
            modified_key, physical_key, ..
        } => (modified_key, *physical_key, true),
        keyboard::Event::KeyReleased {
            modified_key, physical_key, ..
        } => (modified_key, *physical_key, false),
        _ => return,
    };
    let tracked = app
        .vnc_pressed_keys
        .iter()
        .position(|(code, _)| *code == physical);
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

fn vnc_keysym(key: &Key, physical: keyboard::key::Physical) -> Option<u32> {
    use keyboard::key::{Code, Physical};
    // Distinguish right-hand modifiers even when the logical key is just Alt.
    let modifier = match physical {
        Physical::Code(Code::ShiftLeft) => Some(0xffe1),
        Physical::Code(Code::ShiftRight) => Some(0xffe2),
        Physical::Code(Code::ControlLeft) => Some(0xffe3),
        Physical::Code(Code::ControlRight) => Some(0xffe4),
        Physical::Code(Code::AltLeft) => Some(0xffe9),
        Physical::Code(Code::AltRight) => Some(0xffea),
        Physical::Code(Code::SuperLeft) => Some(0xffeb),
        Physical::Code(Code::SuperRight) => Some(0xffec),
        _ => None,
    };
    if modifier.is_some() {
        return modifier;
    }
    Some(match key {
        Key::Character(text) => adit_session::unicode_keysym(text.chars().next()?),
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
