//! Translate protocol-neutral [`adit_rdp_proto::InputEvent`]s into IronRDP input.

use adit_rdp_proto::{InputEvent, MouseButton};
use ironrdp_input::{
    Database, MouseButton as IronButton, MousePosition, Operation, Scancode, WheelRotations,
};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use smallvec::SmallVec;

fn to_iron_button(button: MouseButton) -> IronButton {
    match button {
        MouseButton::Left => IronButton::Left,
        MouseButton::Right => IronButton::Right,
        MouseButton::Middle => IronButton::Middle,
        MouseButton::X1 => IronButton::X1,
        MouseButton::X2 => IronButton::X2,
    }
}

/// Fold one input event into the session's [`Database`] and return the resulting
/// fast-path events. Non-input variants (resize/clipboard) are handled by the
/// caller and yield nothing here.
pub(crate) fn map_input(
    db: &mut Database,
    event: &InputEvent,
) -> SmallVec<[FastPathInputEvent; 2]> {
    let ops: Vec<Operation> = match event {
        InputEvent::MouseMove { x, y } => {
            vec![Operation::MouseMove(MousePosition { x: *x, y: *y })]
        }
        InputEvent::MouseButton { button, pressed } => {
            let b = to_iron_button(*button);
            vec![if *pressed {
                Operation::MouseButtonPressed(b)
            } else {
                Operation::MouseButtonReleased(b)
            }]
        }
        InputEvent::Wheel { vertical, delta } => vec![Operation::WheelRotations(WheelRotations {
            is_vertical: *vertical,
            rotation_units: *delta,
        })],
        InputEvent::Key {
            scancode,
            extended,
            pressed,
        } => {
            let code = Scancode::from_u8(*extended, *scancode);
            vec![if *pressed {
                Operation::KeyPressed(code)
            } else {
                Operation::KeyReleased(code)
            }]
        }
        InputEvent::Unicode { ch, pressed } => vec![if *pressed {
            Operation::UnicodeKeyPressed(*ch)
        } else {
            Operation::UnicodeKeyReleased(*ch)
        }],
        // Handled by the session loop, not as fast-path input.
        InputEvent::Resize { .. } | InputEvent::ClipboardText(_) => return SmallVec::new(),
    };
    db.apply(ops)
}
