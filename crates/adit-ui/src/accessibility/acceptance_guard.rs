//! Synthetic acceptance must never hand the system clipboard to an iced widget.
use iced::{
    advanced::{
        layout, overlay, renderer,
        widget::{self, Tree},
        Clipboard, Layout, Shell, Widget,
    },
    mouse, Element, Event, Length, Rectangle, Size, Theme, Vector,
};

pub(crate) fn validate_environment(
    temp: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for key in ["USERPROFILE", "HOME", "APPDATA", "LOCALAPPDATA"] {
        let directory = std::env::var_os(key).ok_or("missing synthetic environment directory")?;
        let directory = std::fs::canonicalize(directory)?;
        if directory == temp || !directory.starts_with(temp) {
            return Err(format!("{key} must be an isolated directory beneath OS temp").into());
        }
    }
    Ok(())
}

pub(crate) fn no_clipboard<'a, M: 'a>(child: Element<'a, M>) -> Element<'a, M> {
    Element::new(Guard { child })
}

struct Guard<'a, M> {
    child: Element<'a, M>,
}
struct GuardTag;

impl<M> Widget<M, Theme, iced::Renderer> for Guard<'_, M> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<GuardTag>()
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
        _: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            &mut iced::advanced::clipboard::Null,
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
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, M, Theme, iced::Renderer>> {
        self.child
            .as_widget_mut()
            .overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
            .map(|child| overlay::Element::new(Box::new(GuardOverlay { child })))
    }
}

struct GuardOverlay<'a, M> {
    child: overlay::Element<'a, M, Theme, iced::Renderer>,
}

impl<M> overlay::Overlay<M, Theme, iced::Renderer> for GuardOverlay<'_, M> {
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        self.child.as_overlay_mut().layout(renderer, bounds)
    }
    fn draw(
        &self,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.child
            .as_overlay()
            .draw(renderer, theme, style, layout, cursor);
    }
    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.child
            .as_overlay_mut()
            .operate(layout, renderer, operation);
    }
    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        _: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
    ) {
        self.child.as_overlay_mut().update(
            event,
            layout,
            cursor,
            renderer,
            &mut iced::advanced::clipboard::Null,
            shell,
        );
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.child
            .as_overlay()
            .mouse_interaction(layout, cursor, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
    ) -> Option<overlay::Element<'a, M, Theme, iced::Renderer>> {
        self.child
            .as_overlay_mut()
            .overlay(layout, renderer)
            .map(|child| overlay::Element::new(Box::new(GuardOverlay { child })))
    }
    fn index(&self) -> f32 {
        self.child.as_overlay().index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_runtime::user_interface::{Cache, UserInterface};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct Probe;
    impl Widget<(), Theme, iced::Renderer> for Probe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }
        fn layout(
            &mut self,
            _: &mut Tree,
            _: &iced::Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.max())
        }
        fn update(
            &mut self,
            _: &mut Tree,
            _: &Event,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &iced::Renderer,
            clipboard: &mut dyn Clipboard,
            _: &mut Shell<'_, ()>,
            _: &Rectangle,
        ) {
            assert!(clipboard
                .read(iced::advanced::clipboard::Kind::Standard)
                .is_none());
            clipboard.write(
                iced::advanced::clipboard::Kind::Standard,
                "synthetic".into(),
            );
        }
        fn draw(
            &self,
            _: &Tree,
            _: &mut iced::Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
    }
    struct Spy(Arc<AtomicUsize>);
    impl Clipboard for Spy {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some("must-not-be-read".into())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, _: String) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn fixture_widgets_never_receive_the_system_clipboard() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut clipboard = Spy(Arc::clone(&calls));
        let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
        ));
        let mut ui = UserInterface::build(
            no_clipboard(Element::new(Probe)),
            Size::new(20.0, 20.0),
            Cache::new(),
            &mut renderer,
        );
        let mut messages = Vec::new();
        ui.update(
            &[Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::empty(),
            ))],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
