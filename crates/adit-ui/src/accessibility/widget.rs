//! Transparent widget wrapper: geometry comes from the actual draw layout.
//! It delegates input unchanged, including remote Tab/shortcut handling.
use super::{native, *};
use iced::{
    advanced::{
        layout, overlay, renderer,
        widget::{self, Tree},
        Clipboard, Layout, Shell, Widget,
    },
    Event, Rectangle, Size,
};
use std::cell::RefCell;

struct Frame {
    window: window::Id,
    namespace: String,
    bounds: Rectangle,
    nodes: Vec<native::Rendered>,
    parents: Vec<accesskit::NodeId>,
    layer: usize,
    top_layer: usize,
    occurrences: HashMap<String, usize>,
}

thread_local! {
    static FRAME: RefCell<Option<Frame>> = const { RefCell::new(None) };
}

struct Accessible<'a> {
    child: Element<'a, Message>,
    semantics: Option<Control>,
    scope: Option<(Option<window::Id>, String)>,
    modal: bool,
}

struct AccessibleTag;

pub(super) fn control<'a>(child: Element<'a, Message>, semantics: Control) -> Element<'a, Message> {
    Element::new(Accessible {
        child,
        semantics: Some(semantics),
        scope: None,
        modal: false,
    })
}

pub(super) fn scope<'a>(
    child: Element<'a, Message>,
    window: Option<window::Id>,
    namespace: String,
) -> Element<'a, Message> {
    Element::new(Accessible {
        child,
        semantics: None,
        scope: Some((window, namespace)),
        modal: false,
    })
}

pub(super) fn modal<'a>(child: Element<'a, Message>) -> Element<'a, Message> {
    Element::new(Accessible {
        child,
        semantics: None,
        scope: None,
        modal: true,
    })
}

impl Widget<Message, Theme, iced::Renderer> for Accessible<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<AccessibleTag>()
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
        if let Some((Some(window), namespace)) = &self.scope {
            FRAME.with(|frame| {
                *frame.borrow_mut() = Some(Frame {
                    window: *window,
                    namespace: namespace.clone(),
                    bounds: layout.bounds(),
                    nodes: Vec::new(),
                    parents: vec![accesskit::NodeId(1)],
                    layer: 0,
                    top_layer: 0,
                    occurrences: HashMap::new(),
                })
            });
        }
        let entered = FRAME.with(|frame| {
            let mut frame = frame.borrow_mut();
            let Some(frame) = frame.as_mut() else {
                return false;
            };
            if self.modal {
                frame.layer += 1;
                frame.top_layer = frame.top_layer.max(frame.layer);
            }
            let Some(control) = &self.semantics else {
                return false;
            };
            let Some(bounds) = layout.bounds().intersection(viewport) else {
                return false;
            };
            if bounds.width <= 0.0 || bounds.height <= 0.0 {
                return false;
            }
            let mut control = control.clone();
            let occurrence = frame.occurrences.entry(control.key.clone()).or_default();
            if *occurrence > 0 {
                control.key = format!("{}@{}", control.key, occurrence);
            }
            *occurrence += 1;
            let id = native::node_id(
                frame.window,
                &format!("{}/{}", frame.namespace, control.key),
            );
            type InputState = iced::widget::text_input::State<
                <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
            >;
            let focused = control.focus.is_some()
                && tree.children[0].tag == widget::tree::Tag::of::<InputState>()
                && tree.children[0]
                    .state
                    .downcast_ref::<InputState>()
                    .is_focused();
            frame.nodes.push(native::Rendered {
                id,
                parent: *frame.parents.last().unwrap(),
                layer: frame.layer,
                control,
                bounds,
                focused,
            });
            frame.parents.push(id);
            true
        });
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        FRAME.with(|frame| {
            if let Some(frame) = frame.borrow_mut().as_mut() {
                if entered {
                    frame.parents.pop();
                }
                if self.modal {
                    frame.layer = frame.layer.saturating_sub(1);
                }
            }
        });
        if self.scope.is_some() {
            let frame = FRAME.with(|frame| frame.borrow_mut().take());
            if let Some(frame) = frame {
                native::publish(
                    frame.window,
                    frame.namespace,
                    frame.nodes,
                    frame.top_layer,
                    frame.bounds,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_diff_initializes_the_real_iced_child_tree() {
        let element = control(
            iced::widget::button("SYNTHETIC").into(),
            Control::button("fixture", "SYNTHETIC", Message::CloseProfileEditor),
        );
        let mut tree = Tree::empty();
        element.as_widget().diff(&mut tree);
        assert_eq!(tree.children.len(), 1);
        element.as_widget().diff(&mut tree);
        assert_eq!(tree.children.len(), 1);
    }
}
