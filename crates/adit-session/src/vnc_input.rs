use crate::{RdpInput, RdpMouseButton};
use vnc::X11Event;

#[derive(Default)]
pub(super) struct Pointer {
    x: u16,
    y: u16,
    pub(super) buttons: u8,
}

impl Pointer {
    pub(super) fn event(&self, buttons: u8) -> X11Event {
        X11Event::PointerEvent((self.x, self.y, buttons).into())
    }

    pub(super) fn input(&mut self, input: RdpInput) -> Vec<X11Event> {
        match input {
            RdpInput::MouseMove { x, y } => {
                self.x = x;
                self.y = y;
                vec![self.event(self.buttons)]
            }
            RdpInput::MouseButton { button, pressed } => {
                let bit = match button {
                    RdpMouseButton::Left => 1,
                    RdpMouseButton::Middle => 2,
                    RdpMouseButton::Right => 4,
                    _ => return vec![],
                };
                if pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                vec![self.event(self.buttons)]
            }
            RdpInput::Wheel { vertical, delta } => {
                if delta == 0 {
                    return vec![];
                }
                let bit = match (vertical, delta > 0) {
                    (true, true) => 8,
                    (true, false) => 16,
                    (false, true) => 64,
                    (false, false) => 32,
                };
                (0..(delta.unsigned_abs() / 120).clamp(1, 32))
                    .flat_map(|_| [self.event(self.buttons | bit), self.event(self.buttons)])
                    .collect()
            }
            // Used by the toolbar's secure-attention sequence and by the
            // Windows fullscreen hook. Ordinary keys arrive as X11 keysyms.
            RdpInput::Key {
                scancode,
                extended,
                pressed,
            } => scancode_keysym(scancode, extended)
                .map(|key| vec![X11Event::KeyEvent((key, pressed).into())])
                .unwrap_or_default(),
            RdpInput::Unicode { ch, pressed } => {
                vec![X11Event::KeyEvent((unicode_keysym(ch), pressed).into())]
            }
            RdpInput::ClipboardText(text) if text.len() <= 1_048_576 => {
                vec![X11Event::CopyText(text)]
            }
            // VNC desktop size is server-controlled. Window resizes use local
            // fit/1:1 scaling rather than changing the remote user's desktop.
            _ => vec![],
        }
    }
}

pub fn unicode_keysym(ch: char) -> u32 {
    let code = u32::from(ch);
    if code <= 0xff {
        code
    } else {
        0x0100_0000 | code
    }
}

fn scancode_keysym(code: u8, extended: bool) -> Option<u32> {
    Some(match (code, extended) {
        (0x1d, false) => 0xffe3,
        (0x1d, true) => 0xffe4,
        (0x2a, _) => 0xffe1,
        (0x36, _) => 0xffe2,
        (0x38, false) => 0xffe9,
        (0x38, true) => 0xffea,
        (0x5b, true) => 0xffeb,
        (0x5c, true) => 0xffec,
        (0x0f, _) => 0xff09,
        (0x01, _) => 0xff1b,
        (0x53, true) => 0xffff,
        _ => return None,
    })
}
