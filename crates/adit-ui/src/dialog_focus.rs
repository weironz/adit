//! Keyboard containment and explicit focus for the security confirmations.
use super::*;
use iced::{
    advanced::{
        layout, overlay, renderer,
        widget::{self, operation::Focusable, Tree},
        Clipboard, Layout, Renderer as _, Shell, Widget,
    },
    Event, Rectangle, Size,
};

struct Shield<'a> {
    child: Element<'a, Message>,
    capture_input: bool,
    block_operations: bool,
}

pub(crate) fn shield(child: Element<'_, Message>) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: true,
        block_operations: false,
    })
}

pub(crate) fn background(child: Element<'_, Message>, blocked: bool) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: false,
        block_operations: blocked,
    })
}

fn capture_modal_input(event: &Event, shell: &mut Shell<'_, Message>) {
    if matches!(event, Event::Keyboard(_) | Event::InputMethod(_)) {
        shell.capture_event();
    }
}

impl Widget<Message, Theme, iced::Renderer> for Shield<'_> {
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
            capture_modal_input(event, shell);
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
    focused: bool,
}

impl Focusable for FocusState {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
    }
    fn unfocus(&mut self) {
        self.focused = false;
    }
}

impl FocusState {
    #[cfg(windows)]
    pub(crate) fn focused(&self) -> bool {
        self.focused
    }
}

pub(crate) struct ButtonFocus {
    pub window: Option<window::Id>,
    pub scope: String,
    pub id: widget::Id,
    pub previous: widget::Id,
    pub next: widget::Id,
    pub default: bool,
    pub action: Message,
}

struct FocusButton<'a> {
    child: Element<'a, Message>,
    focus: ButtonFocus,
}

pub(crate) fn button(child: Element<'_, Message>, focus: ButtonFocus) -> Element<'_, Message> {
    Element::new(FocusButton { child, focus })
}

impl Widget<Message, Theme, iced::Renderer> for FocusButton<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<FocusState>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(FocusState {
            scope: self.focus.scope.clone(),
            focused: self.focus.default,
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.child.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<FocusState>();
        if state.scope != self.focus.scope {
            state.scope = self.focus.scope.clone();
            state.focused = self.focus.default;
        }
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
        operation.focusable(
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
        let focused = tree.state.downcast_ref::<FocusState>().focused;
        let focus = |id| Message::DialogFocus {
            window: self.focus.window,
            scope: self.focus.scope.clone(),
            id,
        };
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if focused && !modifiers.control() && !modifiers.alt() && !modifiers.logo() =>
            {
                match key {
                    Key::Named(Named::Tab) => {
                        shell.publish(focus(if modifiers.shift() {
                            self.focus.previous.clone()
                        } else {
                            self.focus.next.clone()
                        }));
                        shell.capture_event();
                        return;
                    }
                    Key::Named(Named::Enter | Named::Space) => {
                        shell.publish(self.focus.action.clone());
                        shell.capture_event();
                        return;
                    }
                    Key::Character(value) if value.as_str() == " " => {
                        shell.publish(self.focus.action.clone());
                        shell.capture_event();
                        return;
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(layout.bounds()) =>
            {
                shell.publish(focus(self.focus.id.clone()));
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
        if tree.state.downcast_ref::<FocusState>().focused {
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

    fn button_for(scope: &str, default: bool) -> Element<'static, Message> {
        let id = widget::Id::from("synthetic-button");
        button(
            iced::widget::button("synthetic").into(),
            ButtonFocus {
                window: None,
                scope: scope.to_owned(),
                id: id.clone(),
                previous: id.clone(),
                next: id,
                default,
                action: Message::ClearError,
            },
        )
    }

    #[test]
    fn replacement_confirmation_resets_to_its_safe_default() {
        let old = button_for("challenge:old", true);
        let mut tree = Tree::new(old.as_widget());
        assert!(tree.state.downcast_ref::<FocusState>().focused);
        tree.state.downcast_mut::<FocusState>().unfocus();
        old.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<FocusState>().focused);
        let replacement = button_for("challenge:new", true);
        replacement.as_widget().diff(&mut tree);
        assert!(tree.state.downcast_ref::<FocusState>().focused);

        let trust = button_for("challenge:old", false);
        let mut tree = Tree::new(trust.as_widget());
        tree.state.downcast_mut::<FocusState>().focus();
        let replacement = button_for("challenge:new", false);
        replacement.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<FocusState>().focused);
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
            capture_modal_input(&event, &mut shell);
            assert!(shell.is_event_captured());
        }
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        capture_modal_input(&Event::Mouse(mouse::Event::CursorLeft), &mut shell);
        assert!(!shell.is_event_captured());
    }
}
