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
