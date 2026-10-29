//! Keyboard containment and explicit focus for the security confirmations.
use super::*;
use iced::{
    advanced::{
        layout, overlay, renderer,
        widget::{self, Tree},
        Clipboard, Layout, Renderer as _, Shell, Widget,
    },
    Event, Rectangle, Size,
};
use std::{cell::Cell, rc::Rc};

struct Shield<'a> {
    child: Element<'a, Message>,
    capture_input: bool,
    block_operations: bool,
    escape: Option<Message>,
    mission_input: bool,
    mission_scope: String,
}

#[derive(Default)]
struct ShieldState {
    mission_input: bool,
    escape_armed: bool,
    mission_scope: String,
}

pub(crate) fn shield(child: Element<'_, Message>, escape: Option<Message>) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: true,
        block_operations: false,
        escape,
        mission_input: false,
        mission_scope: String::new(),
    })
}

pub(crate) fn background(child: Element<'_, Message>, blocked: bool) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: false,
        block_operations: blocked,
        escape: None,
        mission_input: false,
        mission_scope: String::new(),
    })
}

pub(crate) fn mission_name_input(child: Element<'_, Message>) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: false,
        block_operations: false,
        escape: None,
        mission_input: true,
        mission_scope: accessibility::current_build_scope(),
    })
}

pub(crate) fn mission_escape_event(
    event: &Event,
    eligible: bool,
    armed: &mut bool,
    shell: &mut Shell<'_, Message>,
) -> bool {
    match event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        }) if eligible => {
            *armed = true;
            shell.capture_event();
            true
        }
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Named(Named::Escape),
            ..
        }) if *armed => {
            *armed = false;
            shell.publish(Message::FinishMissionName(false));
            shell.capture_event();
            true
        }
        _ => false,
    }
}

fn capture_modal_input(event: &Event, shell: &mut Shell<'_, Message>, escape: Option<&Message>) {
    if let (true, Some(message)) = (
        matches!(
            event,
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                ..
            })
        ),
        escape,
    ) {
        shell.publish(message.clone());
    }
    if matches!(event, Event::Keyboard(_) | Event::InputMethod(_)) {
        shell.capture_event();
    }
}

impl Widget<Message, Theme, iced::Renderer> for Shield<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<ShieldState>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(ShieldState {
            mission_input: self.mission_input,
            escape_armed: false,
            mission_scope: self.mission_scope.clone(),
        })
    }
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.child.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<ShieldState>();
        if state.mission_input != self.mission_input || state.mission_scope != self.mission_scope {
            state.mission_input = self.mission_input;
            state.escape_armed = false;
            state.mission_scope.clone_from(&self.mission_scope);
        }
        tree.diff_children(std::slice::from_ref(&self.child));
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        if !self.block_operations {
            self.child
                .as_widget_mut()
                .operate(&mut tree.children[0], layout, renderer, operation);
        }
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.mission_input
            && !shell.is_event_captured()
            && mission_escape_event(
                event,
                true,
                &mut tree.state.downcast_mut::<ShieldState>().escape_armed,
                shell,
            )
        {
            return;
        }
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        // iced::stack otherwise forwards uncaptured keyboard and IME events to
        // a focused input underneath the scrim, including in another HWND.
        if self.capture_input {
            capture_modal_input(event, shell, self.escape.as_ref());
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, iced::Renderer>> {
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
}

pub(crate) struct FocusState {
    scope: String,
    index: usize,
    group: FocusGroup,
}

#[derive(Clone)]
pub(crate) struct FocusGroup(Rc<Cell<Option<usize>>>);

impl FocusGroup {
    pub(crate) fn new() -> Self {
        Self(Rc::new(Cell::new(Some(0))))
    }
}

impl Default for FocusGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusState {
    pub(crate) fn focused(&self) -> bool {
        self.group.0.get() == Some(self.index)
    }

    pub(crate) fn focus(&mut self) {
        self.group.0.set(Some(self.index));
    }
}

pub(crate) struct ButtonFocus {
    pub scope: String,
    pub id: widget::Id,
    pub index: usize,
    pub count: usize,
    pub group: FocusGroup,
    pub action: Message,
}

struct FocusButton<'a> {
    child: Element<'a, Message>,
    focus: ButtonFocus,
}

pub(crate) fn button(child: Element<'_, Message>, focus: ButtonFocus) -> Element<'_, Message> {
    Element::new(FocusButton { child, focus })
}

fn confirmation_key(
    focus: &ButtonFocus,
    state: &FocusState,
    event: &Event,
    shell: &mut Shell<'_, Message>,
) -> bool {
    // Row visits later siblings even after one captured the event.
    if shell.is_event_captured() {
        return true;
    }
    let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
        return false;
    };
    if !state.focused() || modifiers.control() || modifiers.alt() || modifiers.logo() {
        return false;
    }
    match key {
        Key::Named(Named::Tab) => {
            let next = if modifiers.shift() {
                // Do not wrap the safe default backwards onto trust/open: an
                // immediately following Enter could otherwise accept before
                // the newly selected action has even been painted.
                focus.index.saturating_sub(1)
            } else {
                (focus.index + 1) % focus.count
            };
            // The next event in this same iced batch must see this selection.
            state.group.0.set(Some(next));
            shell.request_redraw();
            shell.capture_event();
            true
        }
        Key::Named(Named::Enter | Named::Space) => {
            shell.publish(focus.action.clone());
            shell.capture_event();
            true
        }
        Key::Character(value) if value.as_str() == " " => {
            shell.publish(focus.action.clone());
            shell.capture_event();
            true
        }
        _ => false,
    }
}

impl Widget<Message, Theme, iced::Renderer> for FocusButton<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<FocusState>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(FocusState {
            scope: self.focus.scope.clone(),
            index: self.focus.index,
            group: self.focus.group.clone(),
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.child.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<FocusState>();
        if state.scope == self.focus.scope {
            self.focus.group.0.set(state.group.0.get());
        }
        state.scope = self.focus.scope.clone();
        state.index = self.focus.index;
        state.group = self.focus.group.clone();
        tree.diff_children(std::slice::from_ref(&self.child));
    }
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        // Global iced focus operations unfocus every focusable they encounter.
        // Confirmation focus is custom, so an old unrelated task cannot clear
        // the safe default in a newly opened confirmation.
        operation.custom(
            Some(&self.focus.id),
            layout.bounds(),
            tree.state.downcast_mut::<FocusState>(),
        );
        self.child
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if confirmation_key(
            &self.focus,
            tree.state.downcast_ref::<FocusState>(),
            event,
            shell,
        ) {
            return;
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(layout.bounds()) =>
            {
                self.focus.group.0.set(Some(self.focus.index));
                shell.request_redraw();
            }
            _ => {}
        }
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, iced::Renderer>> {
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if tree.state.downcast_ref::<FocusState>().focused() {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: iced::Border {
                        color: accent(),
                        width: 2.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                },
                Color::TRANSPARENT,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressed(named: Named, modifiers: keyboard::Modifiers) -> Event {
        let key = Key::Named(named);
        let code = match named {
            Named::Tab => keyboard::key::Code::Tab,
            Named::Enter => keyboard::key::Code::Enter,
            Named::Space => keyboard::key::Code::Space,
            _ => keyboard::key::Code::Escape,
        };
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn batched_tabs_and_activation_use_the_synchronously_selected_action() {
        use adit_session::HostKeyDecision;
        let group = FocusGroup::new();
        let decisions = [
            HostKeyDecision::Reject,
            HostKeyDecision::RejectAndRecord,
            HostKeyDecision::TrustOnce,
            HostKeyDecision::TrustAndRecord,
        ];
        let buttons: Vec<_> = decisions
            .iter()
            .enumerate()
            .map(|(index, decision)| ButtonFocus {
                scope: "challenge:synthetic".into(),
                id: widget::Id::from(format!("button-{index}")),
                index,
                count: decisions.len(),
                group: group.clone(),
                action: Message::RespondHostKey {
                    session_id: SessionId::new(),
                    challenge_id: 1,
                    decision: *decision,
                },
            })
            .collect();
        let states: Vec<_> = (0..decisions.len())
            .map(|index| FocusState {
                scope: "challenge:synthetic".into(),
                index,
                group: group.clone(),
            })
            .collect();
        let mut messages = Vec::new();
        let mut batch = |events: &[Event]| {
            for event in events {
                let mut shell = Shell::new(&mut messages);
                for (button, state) in buttons.iter().zip(&states) {
                    confirmation_key(button, state, event, &mut shell);
                }
                assert!(shell.is_event_captured());
            }
        };
        batch(&[
            pressed(Named::Tab, keyboard::Modifiers::empty()),
            pressed(Named::Space, keyboard::Modifiers::empty()),
        ]);
        assert!(matches!(
            messages.last(),
            Some(Message::RespondHostKey {
                decision: HostKeyDecision::RejectAndRecord,
                ..
            })
        ));
        messages.clear();
        group.0.set(Some(3));
        for event in [
            pressed(Named::Tab, keyboard::Modifiers::SHIFT),
            pressed(Named::Enter, keyboard::Modifiers::empty()),
        ] {
            let mut shell = Shell::new(&mut messages);
            for (button, state) in buttons.iter().zip(&states) {
                confirmation_key(button, state, &event, &mut shell);
            }
            assert!(shell.is_event_captured());
        }
        assert!(matches!(
            messages.last(),
            Some(Message::RespondHostKey {
                decision: HostKeyDecision::TrustOnce,
                ..
            })
        ));
        assert_eq!(messages.len(), 1);
        messages.clear();
        group.0.set(Some(0));
        for event in [
            pressed(Named::Tab, keyboard::Modifiers::SHIFT),
            pressed(Named::Enter, keyboard::Modifiers::empty()),
        ] {
            let mut shell = Shell::new(&mut messages);
            for (button, state) in buttons.iter().zip(&states) {
                confirmation_key(button, state, &event, &mut shell);
            }
            assert!(shell.is_event_captured());
        }
        assert!(matches!(
            messages.as_slice(),
            [Message::RespondHostKey {
                decision: HostKeyDecision::Reject,
                ..
            }]
        ));
    }

    #[test]
    fn legacy_global_focus_operation_cannot_clear_confirmation_default() {
        use iced::advanced::widget::Operation;
        let group = FocusGroup::new();
        let mut state = FocusState {
            scope: "challenge".into(),
            index: 0,
            group,
        };
        let mut old =
            widget::operation::focusable::focus::<()>(widget::Id::from("stale-background"));
        old.custom(
            Some(&widget::Id::from("reject")),
            Rectangle::default(),
            &mut state,
        );
        assert!(state.focused());
    }

    #[test]
    fn mission_escape_arm_is_revoked_by_a_covering_modal() {
        let build = |scope| {
            let _building = accessibility::building(None, scope);
            mission_name_input(iced::widget::text_input("synthetic", "draft").into())
        };
        let rename = build("main");
        let mut tree = Tree::new(rename.as_widget());
        tree.state.downcast_mut::<ShieldState>().escape_armed = true;
        let covered = build("hyperlink:1");
        covered.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<ShieldState>().escape_armed);
        let restored = build("main");
        restored.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<ShieldState>().escape_armed);
    }

    #[test]
    fn menu_escape_is_captured_and_targets_only_the_visible_menu() {
        let event = pressed(Named::Escape, keyboard::Modifiers::empty());
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        capture_modal_input(
            &event,
            &mut shell,
            Some(&Message::DismissMenu(MenuEscapeTarget::Active(
                MenuKind::File,
            ))),
        );
        assert!(shell.is_event_captured());
        assert!(matches!(
            messages.as_slice(),
            [Message::DismissMenu(MenuEscapeTarget::Active(
                MenuKind::File
            ))]
        ));
    }

    fn button_for(scope: &str, index: usize, group: &FocusGroup) -> Element<'static, Message> {
        let id = widget::Id::from("synthetic-button");
        button(
            iced::widget::button("synthetic").into(),
            ButtonFocus {
                scope: scope.to_owned(),
                id,
                index,
                count: 4,
                group: group.clone(),
                action: Message::ClearError,
            },
        )
    }

    #[test]
    fn replacement_confirmation_resets_to_its_safe_default() {
        let group = FocusGroup::new();
        let old = button_for("challenge:old", 0, &group);
        let mut tree = Tree::new(old.as_widget());
        assert!(tree.state.downcast_ref::<FocusState>().focused());
        group.0.set(Some(3));
        old.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<FocusState>().focused());
        let replacement_group = FocusGroup::new();
        let replacement = button_for("challenge:new", 0, &replacement_group);
        replacement.as_widget().diff(&mut tree);
        assert!(tree.state.downcast_ref::<FocusState>().focused());

        let trust = button_for("challenge:old", 3, &group);
        let mut tree = Tree::new(trust.as_widget());
        tree.state.downcast_mut::<FocusState>().focus();
        let replacement = button_for("challenge:new", 3, &replacement_group);
        replacement.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<FocusState>().focused());
    }

    #[test]
    fn rebuilt_buttons_keep_one_shared_selection_for_same_challenge() {
        let old_group = FocusGroup::new();
        let old: Vec<_> = (0..4)
            .map(|i| button_for("challenge:same", i, &old_group))
            .collect();
        let mut trees: Vec<_> = old
            .iter()
            .map(|button| Tree::new(button.as_widget()))
            .collect();
        old_group.0.set(Some(2));
        let new_group = FocusGroup::new();
        let rebuilt: Vec<_> = (0..4)
            .map(|i| button_for("challenge:same", i, &new_group))
            .collect();
        for (button, tree) in rebuilt.iter().zip(&mut trees) {
            button.as_widget().diff(tree);
        }
        let focused: Vec<_> = trees
            .iter()
            .enumerate()
            .filter_map(|(i, tree)| {
                tree.state
                    .downcast_ref::<FocusState>()
                    .focused()
                    .then_some(i)
            })
            .collect();
        assert_eq!(focused, vec![2]);
    }

    #[test]
    fn modal_captures_keyboard_and_all_ime_phases_before_the_background() {
        use iced::advanced::input_method;
        for event in [
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::default(),
            )),
            Event::InputMethod(input_method::Event::Opened),
            Event::InputMethod(input_method::Event::Preedit("SYNTHETIC".into(), Some(0..1))),
            Event::InputMethod(input_method::Event::Commit("SYNTHETIC".into())),
            Event::InputMethod(input_method::Event::Closed),
        ] {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            capture_modal_input(&event, &mut shell, None);
            assert!(shell.is_event_captured());
        }
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        capture_modal_input(&Event::Mouse(mouse::Event::CursorLeft), &mut shell, None);
        assert!(!shell.is_event_captured());
    }
}
