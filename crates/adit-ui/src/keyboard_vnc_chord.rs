//! Ownership of Windows shortcuts, before the local IME sees their members.
//! Physical identities survive layout changes; translation stays outside the hook.

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub right_alt: bool,
    pub caps: Option<bool>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Stroke {
    pub vk: u16,
    pub scan: u8,
    pub extended: bool,
    pub pressed: bool,
    pub repeat: bool,
    pub layout: isize,
    pub modifiers: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Local,
    System,
    Member { repeat: bool },
}

pub(crate) struct Ownership {
    down: [bool; 512],
    members: [bool; 512],
    wins: u8,
    tab: bool,
    caps: Option<bool>,
}

impl Ownership {
    pub fn new(down: [bool; 512], caps: Option<bool>) -> Self {
        Self {
            down,
            members: [false; 512],
            wins: 0,
            tab: false,
            caps,
        }
    }

    pub fn clear(&mut self) {
        self.members.fill(false);
        self.wins = 0;
        self.tab = false;
        // Existing presses now belong to another local input lease. Preserve
        // `down` so coming back during an auto-repeat cannot adopt them.
    }

    pub fn caps(&self) -> Option<bool> {
        self.caps
    }

    pub fn foreign_event(&mut self, vk: u16, scan: u8, extended: bool, pressed: bool) {
        self.clear();
        let index = usize::from(scan) + if extended { 256 } else { 0 };
        if vk == 0x14 && pressed && !self.down[index] {
            self.caps = self.caps.map(|value| !value);
        }
        self.down[index] = pressed;
    }

    pub fn member_down(&self, scan: u8, extended: bool) -> bool {
        self.members[usize::from(scan) + if extended { 256 } else { 0 }]
    }

    pub fn key_down(&self, scan: u8, extended: bool) -> bool {
        self.down[usize::from(scan) + if extended { 256 } else { 0 }]
    }

    pub fn event(&mut self, vk: u16, scan: u8, extended: bool, pressed: bool) -> Route {
        let index = usize::from(scan) + if extended { 256 } else { 0 };
        let was_down = self.down[index];
        self.down[index] = pressed;
        if vk == 0x14 && pressed && !was_down {
            // Caps itself is never swallowed. Its local toggle takes effect;
            // repeats must not toggle it a second time.
            self.caps = self.caps.map(|value| !value);
        }
        if vk == 0x0d
            && (self.down[0x1d] || self.down[0x1d + 256])
            && (self.down[0x38] || self.down[0x38 + 256])
            && (pressed || !self.members[index])
        {
            // Also retain the escape in this value policy; Ctrl/Alt were
            // deliberately left visible to the normal window dispatcher.
            return Route::Local;
        }
        let win = match vk {
            0x5b => 1,
            0x5c => 2,
            _ => 0,
        };
        if win != 0 {
            if pressed {
                if !was_down {
                    self.wins |= win;
                }
                return if self.wins & win != 0 {
                    Route::System
                } else {
                    Route::Local
                };
            }
            let owned = self.wins & win != 0;
            self.wins &= !win;
            return if owned { Route::System } else { Route::Local };
        }
        if vk == 0x09 {
            if pressed && !was_down {
                self.tab = true;
            }
            let owned = self.tab;
            if !pressed {
                self.tab = false;
            }
            return if owned { Route::System } else { Route::Local };
        }
        if self.members[index] {
            if !pressed {
                self.members[index] = false;
            }
            return Route::Member {
                repeat: pressed && was_down,
            };
        }
        if pressed && !was_down && self.wins != 0 && eligible(vk) {
            self.members[index] = true;
            return Route::Member { repeat: false };
        }
        Route::Local
    }
}

fn eligible(vk: u16) -> bool {
    // Ctrl/Alt keep the existing window modifier path, including AltGr and
    // Ctrl+Alt+Enter. Hiding them would hide the emergency escape from iced.
    matches!(vk, 0x08 | 0x0d | 0x10 | 0x1b | 0x20..=0x2e
        | 0x30..=0x39 | 0x41..=0x5a | 0x60..=0x6f | 0x70..=0x87 | 0xa0..=0xa1
        | 0xba..=0xc0 | 0xdb..=0xdf | 0xe2)
}

pub(crate) fn named_symbol(vk: u16, extended: bool) -> Option<u32> {
    Some(match vk {
        0x08 => 0xff08,
        0x09 => 0xff09,
        0x0d => 0xff0d,
        0x10 | 0xa0 => 0xffe1,
        0xa1 => 0xffe2,
        0x11 | 0xa2 => {
            if extended {
                0xffe4
            } else {
                0xffe3
            }
        }
        0xa3 => 0xffe4,
        0x12 | 0xa4 => {
            if extended {
                0xffea
            } else {
                0xffe9
            }
        }
        0xa5 => 0xffea,
        0x1b => 0xff1b,
        0x20 => 0x20,
        0x21 => 0xff55,
        0x22 => 0xff56,
        0x23 => 0xff57,
        0x24 => 0xff50,
        0x25 => 0xff51,
        0x26 => 0xff52,
        0x27 => 0xff53,
        0x28 => 0xff54,
        0x2c => 0xff61,
        0x2d => 0xff63,
        0x2e => 0xffff,
        0x70..=0x87 => 0xffbe + u32::from(vk - 0x70),
        _ => return None,
    })
}

pub(crate) fn single_character(result: i32, text: &[u16]) -> Result<char, &'static str> {
    if result < 0 {
        return Err("dead-key");
    }
    if result == 0 {
        return Err("unmapped");
    }
    let text = text.get(..result as usize).ok_or("translation-too-long")?;
    let mut chars = char::decode_utf16(text.iter().copied());
    let first = chars
        .next()
        .ok_or("unmapped")?
        .map_err(|_| "invalid-utf16")?;
    if chars.next().is_some() {
        return Err("multiple-scalars");
    }
    if first.is_control() {
        return Err("control-character");
    }
    Ok(first)
}

pub(crate) fn translate(stroke: &Stroke) -> Result<u32, &'static str> {
    if stroke.vk == 0x10 && stroke.scan == 0x36 {
        return Ok(0xffe2);
    }
    if let Some(symbol) = named_symbol(stroke.vk, stroke.extended) {
        return Ok(symbol);
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::{ToUnicodeEx, HKL};
        if stroke.modifiers.caps.is_none() {
            return Err("caps-state-unavailable");
        }
        let mut state = [0u8; 256];
        if stroke.modifiers.shift {
            state[0x10] = 0x80;
        }
        if stroke.modifiers.caps == Some(true) {
            state[0x14] = 1;
        }
        // Ctrl shortcuts name the key, not its U+0001..U+001A control text.
        // A physical right Alt may select a real third-level layout character.
        let altgr = stroke.modifiers.right_alt && stroke.modifiers.control && stroke.modifiers.alt;
        if altgr {
            state[0x11] = 0x80;
            state[0xa2] = 0x80;
            state[0x12] = 0x80;
            state[0xa5] = 0x80;
        }
        let mut text = [0u16; 16];
        let mut result = ToUnicodeEx(
            u32::from(stroke.vk),
            u32::from(stroke.scan),
            &state,
            &mut text,
            4,
            Some(HKL(stroke.layout as *mut _)),
        );
        if altgr && (result == 0 || single_character(result, &text) == Err("control-character")) {
            state[0x11] = 0;
            state[0xa2] = 0;
            state[0x12] = 0;
            state[0xa5] = 0;
            result = ToUnicodeEx(
                u32::from(stroke.vk),
                u32::from(stroke.scan),
                &state,
                &mut text,
                4,
                Some(HKL(stroke.layout as *mut _)),
            );
        }
        let value = u32::from(single_character(result, &text)?);
        Ok(if value <= 0xff {
            value
        } else {
            0x0100_0000 | value
        })
    }
    #[cfg(not(windows))]
    Err("native-layout-unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn member_release_and_repeat_stay_owned_after_win_release() {
        let mut keys = Ownership::new([false; 512], Some(false));
        assert_eq!(keys.event(0x5b, 0x5b, true, true), Route::System);
        assert_eq!(
            keys.event(0x52, 0x13, false, true),
            Route::Member { repeat: false }
        );
        assert_eq!(keys.event(0x5b, 0x5b, true, false), Route::System);
        assert_eq!(
            keys.event(0x52, 0x13, false, true),
            Route::Member { repeat: true }
        );
        assert_eq!(
            keys.event(0x52, 0x13, false, false),
            Route::Member { repeat: false }
        );
        assert_eq!(keys.event(0x52, 0x13, false, false), Route::Local);
    }
    #[test]
    fn preheld_win_and_preheld_letters_are_not_adopted() {
        let mut down = [false; 512];
        down[0x5b + 256] = true;
        let mut keys = Ownership::new(down, Some(false));
        assert_eq!(keys.event(0x5b, 0x5b, true, true), Route::Local);
        assert_eq!(keys.event(0x52, 0x13, false, true), Route::Local);
        assert_eq!(keys.event(0x5b, 0x5b, true, false), Route::Local);
        assert_eq!(keys.event(0x5b, 0x5b, true, true), Route::System);
        assert_eq!(keys.event(0x52, 0x13, false, true), Route::Local);
        assert_eq!(keys.event(0x52, 0x13, false, false), Route::Local);
    }
    #[test]
    fn both_wins_and_focus_lease_are_independent() {
        let mut keys = Ownership::new([false; 512], Some(false));
        keys.event(0x5b, 0x5b, true, true);
        keys.event(0x5c, 0x5c, true, true);
        keys.event(0x5b, 0x5b, true, false);
        assert_eq!(
            keys.event(0x44, 0x20, false, true),
            Route::Member { repeat: false }
        );
        keys.clear();
        assert_eq!(keys.event(0x44, 0x20, false, false), Route::Local);
        assert_eq!(keys.event(0x52, 0x13, false, true), Route::Local);
        keys.foreign_event(0x5c, 0x5c, true, false);
        assert_eq!(keys.event(0x5c, 0x5c, true, true), Route::System);
        assert_eq!(keys.event(0xa2, 0x1d, false, true), Route::Local);
        assert_eq!(keys.event(0xa4, 0x38, false, true), Route::Local);
        assert_eq!(keys.event(0x0d, 0x1c, false, true), Route::Local);
        assert_eq!(keys.event(0x0d, 0x1c, false, false), Route::Local);
    }
    #[test]
    fn caps_repeats_and_unicode_results_are_explicit() {
        let mut keys = Ownership::new([false; 512], Some(false));
        keys.event(0x14, 0x3a, false, true);
        keys.event(0x14, 0x3a, false, true);
        assert_eq!(keys.caps(), Some(true));
        assert_eq!(single_character(2, &[0xd83d, 0xde0a]), Ok('😊'));
        assert_eq!(single_character(2, &[0x65, 0x301]), Err("multiple-scalars"));
        assert_eq!(single_character(-1, &[0xb4]), Err("dead-key"));
        assert_eq!(single_character(1, &[0x12]), Err("control-character"));
        assert_eq!(single_character(1, &[0xd83d]), Err("invalid-utf16"));
    }

    #[test]
    fn win_ctrl_alt_enter_keeps_escape_modifiers_in_window_path() {
        let mut keys = Ownership::new([false; 512], Some(false));
        assert_eq!(keys.event(0x5b, 0x5b, true, true), Route::System);
        assert_eq!(keys.event(0xa2, 0x1d, false, true), Route::Local);
        assert_eq!(keys.event(0xa4, 0x38, false, true), Route::Local);
        // The outer hook escape guard sees these real OS modifiers and passes
        // Enter before this policy. No swallowed Ctrl/Alt can hide it from iced.
        assert_eq!(keys.event(0x0d, 0x1c, false, true), Route::Local);
        assert_eq!(keys.event(0x0d, 0x1c, false, false), Route::Local);
        assert_eq!(keys.event(0xa2, 0x1d, false, false), Route::Local);
        assert_eq!(keys.event(0xa4, 0x38, false, false), Route::Local);
    }

    #[test]
    fn shift_membership_preserves_both_modifier_orders() {
        let mut first = Ownership::new([false; 512], Some(false));
        first.event(0xa0, 0x2a, false, true);
        first.event(0x5b, 0x5b, true, true);
        assert_eq!(
            first.event(0x52, 0x13, false, true),
            Route::Member { repeat: false }
        );
        let mut second = Ownership::new([false; 512], Some(false));
        second.event(0x5b, 0x5b, true, true);
        assert_eq!(
            second.event(0xa0, 0x2a, false, true),
            Route::Member { repeat: false }
        );
        assert!(second.member_down(0x2a, false));
        assert_eq!(
            second.event(0x52, 0x13, false, true),
            Route::Member { repeat: false }
        );
        second.event(0xa0, 0x2a, false, false);
        assert!(!second.member_down(0x2a, false));
        second.foreign_event(0x14, 0x3a, false, true);
        assert_eq!(second.caps(), Some(true));
        assert_eq!(second.event(0x52, 0x13, false, false), Route::Local);
    }

    #[test]
    fn owned_enter_up_survives_later_escape_modifiers_for_both_enter_keys() {
        for extended in [false, true] {
            let mut keys = Ownership::new([false; 512], Some(false));
            assert_eq!(keys.event(0x5b, 0x5b, true, true), Route::System);
            assert_eq!(
                keys.event(0x0d, 0x1c, extended, true),
                Route::Member { repeat: false }
            );
            assert_eq!(keys.event(0xa2, 0x1d, false, true), Route::Local);
            assert_eq!(keys.event(0xa4, 0x38, false, true), Route::Local);
            assert_eq!(
                keys.event(0x0d, 0x1c, extended, false),
                Route::Member { repeat: false }
            );
            assert_eq!(keys.event(0x0d, 0x1c, extended, false), Route::Local);
            assert_eq!(keys.event(0x0d, 0x1c, extended, true), Route::Local);
        }
    }
}
