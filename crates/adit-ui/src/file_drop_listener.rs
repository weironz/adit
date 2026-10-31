//! Receive file events in the window's widget tree, before the bounded runtime
//! subscription can discard some of a multi-file drag.
use super::*;
use iced::advanced::{
    layout, overlay, renderer,
    widget::{self, Tree},
    Clipboard, Layout, Shell, Widget,
};
use iced::{Event, Rectangle, Size};
use native_windows::NativeMessage;

struct Listener<'a> {
    child: Element<'a, NativeMessage>,
    window: window::Id,
}

pub(crate) fn wrap<'a>(
    child: Element<'a, NativeMessage>,
    window: window::Id,
) -> Element<'a, NativeMessage> {
    Element::new(Listener { child, window })
}

impl Widget<NativeMessage, Theme, iced::Renderer> for Listener<'_> {
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
        shell: &mut Shell<'_, NativeMessage>,
        viewport: &Rectangle,
    ) {
        if matches!(
            event,
            Event::Window(
                window::Event::FileHovered(_)
                    | window::Event::FileDropped(_)
                    | window::Event::FilesHoveredLeft
            )
        ) {
            shell.publish(NativeMessage::Runtime(
                self.window,
                event.clone(),
                event::Status::Ignored,
            ));
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
    ) -> Option<overlay::Element<'a, NativeMessage, Theme, iced::Renderer>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use iced_runtime::user_interface::{Cache, UserInterface};

    #[test]
    fn a_large_file_batch_reaches_the_correct_window_in_order() {
        let window = window::Id::unique();
        let child: Element<'_, NativeMessage> = Space::new().width(Fill).height(Fill).into();
        let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
        ));
        let mut ui = UserInterface::build(
            wrap(child, window),
            Size::new(640.0, 480.0),
            Cache::new(),
            &mut renderer,
        );
        let mut events = Vec::new();
        for index in 0..150 {
            events.push(Event::Window(window::Event::FileHovered(
                format!("{index}.txt").into(),
            )));
        }
        for index in 0..150 {
            events.push(Event::Window(window::Event::FileDropped(
                format!("{index}.txt").into(),
            )));
        }
        let mut messages = Vec::new();
        ui.update(
            &events,
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages.len(), 300);
        for (message, expected) in messages.iter().zip(events) {
            match message {
                NativeMessage::Runtime(id, event, _) => {
                    assert_eq!(*id, window);
                    assert_eq!(*event, expected);
                }
                _ => panic!("unexpected file event route"),
            }
        }
    }
}
