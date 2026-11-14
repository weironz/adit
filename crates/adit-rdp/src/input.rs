//! Translate protocol-neutral [`adit_rdp_proto::InputEvent`]s into IronRDP input.

use adit_rdp_proto::{InputEvent, MouseButton};
use ironrdp_input::{
    Database, MouseButton as IronButton, MousePosition, Operation, Scancode, WheelRotations,
};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::input::mouse::PointerFlags;
use ironrdp_pdu::input::MousePdu;
use smallvec::SmallVec;

fn to_iron_button(button: MouseButton) -> IronButton {
    match button {
        MouseButton::Left => IronButton::Left,
        MouseButton::Middle => IronButton::Middle,
        MouseButton::Right => IronButton::Right,
        MouseButton::X1 => IronButton::X1,
        MouseButton::X2 => IronButton::X2,
    }
}

/// The session's input state: IronRDP's own state machine plus what the
/// server has actually been told about the pointer.
pub(crate) struct InputState {
    db: Database,
    /// The last pointer position sent to the server, or `None` before the
    /// first move.
    ///
    /// `Database` cannot answer this on its own: it starts out holding the
    /// origin and only emits a move when the position *differs* from the one
    /// it holds. So the first move of a session to (0,0) — the top-left
    /// corner of the remote desktop — looks to it like a repeat and is
    /// swallowed. The server then keeps whatever pointer position it had, the
    /// client's idea of the pointer and the server's disagree, and the button
    /// PDUs that follow carry the position the Database holds rather than the
    /// one the user aimed at.
    pointer_sent: Option<(u16, u16)>,
}

impl InputState {
    pub(crate) fn new() -> Self {
        Self {
            db: Database::new(),
            pointer_sent: None,
        }
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

/// Move the pointer to an absolute position.
///
/// The move is emitted from here rather than left to the Database's change
/// detection, so that the first move of a session is delivered even when it
/// lands on the origin. A mouse that has not moved produces nothing.
fn mouse_move(state: &mut InputState, x: u16, y: u16) -> SmallVec<[FastPathInputEvent; 2]> {
    if state.pointer_sent == Some((x, y)) {
        return SmallVec::new();
    }
    state.pointer_sent = Some((x, y));
    // Keep the Database's own position in step with what the server has been
    // told, so the button PDUs that follow carry this position.
    let events = state.db.apply([Operation::MouseMove(MousePosition { x, y })]);
    if !events.is_empty() {
        return events;
    }
    // The Database held this position already (it starts at the origin), so it
    // emitted nothing — but the server has not been told about this move yet,
    // so it still has to go out.
    let mut events: SmallVec<[FastPathInputEvent; 2]> = SmallVec::new();
    events.push(FastPathInputEvent::MouseEvent(MousePdu {
        flags: PointerFlags::MOVE,
        number_of_wheel_rotation_units: 0,
        x_position: x,
        y_position: y,
    }));
    events
}

/// Fold one input event into the session's input state and return the
/// resulting fast-path events. Non-input variants (resize/clipboard) are
/// handled by the caller and yield nothing here.
pub(crate) fn map_input(
    state: &mut InputState,
    event: &InputEvent,
) -> SmallVec<[FastPathInputEvent; 2]> {
    if let InputEvent::MouseMove { x, y } = event {
        return mouse_move(state, *x, *y);
    }

    let ops: Vec<Operation> = match event {
        // Handled above.
        InputEvent::MouseMove { .. } => return SmallVec::new(),
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
        InputEvent::Resize { .. }
        | InputEvent::ClipboardText(_)
        | InputEvent::MonitorLayout { .. }
        | InputEvent::CancelMonitorLayout { .. } => return SmallVec::new(),
    };
    state.db.apply(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_to(state: &mut InputState, x: u16, y: u16) -> Option<(u16, u16)> {
        let events = map_input(state, &InputEvent::MouseMove { x, y });
        let Some(FastPathInputEvent::MouseEvent(pdu)) = events.first() else {
            return None;
        };
        Some((pdu.x_position, pdu.y_position))
    }

    #[test]
    fn a_move_to_the_origin_is_delivered() {
        // A fresh Database holds the origin, so its change detection would
        // swallow the first move to (0,0) — the top-left corner of the remote
        // desktop — and the server would keep its old pointer position.
        let mut state = InputState::new();
        assert_eq!(move_to(&mut state, 0, 0), Some((0, 0)));
    }

    #[test]
    fn a_move_to_the_origin_is_delivered_after_a_move_elsewhere() {
        let mut state = InputState::new();
        assert_eq!(move_to(&mut state, 2000, 400), Some((2000, 400)));
        assert_eq!(move_to(&mut state, 0, 0), Some((0, 0)));
    }

    #[test]
    fn a_repeated_move_to_the_same_place_emits_nothing() {
        // A mouse that does not move must not turn into a stream of MOVE
        // PDUs; the server only needs to hear about actual movement.
        let mut state = InputState::new();
        assert_eq!(move_to(&mut state, 0, 0), Some((0, 0)));
        assert_eq!(move_to(&mut state, 0, 0), None);
        assert_eq!(move_to(&mut state, 300, 200), Some((300, 200)));
        assert_eq!(move_to(&mut state, 300, 200), None);
    }

    #[test]
    fn a_button_follows_the_position_of_the_move_before_it() {
        // The button PDU carries the position the Database holds, so a move
        // that never reached the Database would leave the click aimed at the
        // wrong spot.
        let mut state = InputState::new();
        map_input(&mut state, &InputEvent::MouseMove { x: 0, y: 0 });
        let events = map_input(
            &mut state,
            &InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        );
        let Some(FastPathInputEvent::MouseEvent(pdu)) = events.first() else {
            panic!("expected a mouse event, got {events:?}");
        };
        assert_eq!((pdu.x_position, pdu.y_position), (0, 0));
    }

    #[test]
    fn a_button_after_a_move_elsewhere_carries_that_position() {
        let mut state = InputState::new();
        map_input(&mut state, &InputEvent::MouseMove { x: 2000, y: 400 });
        let events = map_input(
            &mut state,
            &InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        );
        let Some(FastPathInputEvent::MouseEvent(pdu)) = events.first() else {
            panic!("expected a mouse event, got {events:?}");
        };
        assert_eq!((pdu.x_position, pdu.y_position), (2000, 400));
    }
}
