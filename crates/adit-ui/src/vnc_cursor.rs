//! Client-side cursor overlay: the desktop pixels stay untouched and the hotspot
//! follows the same surface-to-logical transform as pointer input.
use super::*;
use iced::advanced::{layout, renderer, widget::Tree, Layout, Widget};
use iced::{Rectangle, Size};

pub(crate) struct CursorLayer {
    pub cursor: Option<adit_session::VncCursor>,
    pub ime_enabled: bool,
    pub session_id: SessionId,
    pub input_epoch: u64,
    pub preedit: Option<iced::advanced::input_method::Preedit>,
    pub handle: Option<iced::widget::image::Handle>,
    pub logical_size: Size,
    pub scale_x: f32,
    pub scale_y: f32,
}

impl Widget<Message, Theme, iced::Renderer> for CursorLayer {
    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.logical_size.width),
            Length::Fixed(self.logical_size.height),
        )
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Scrollables can supply infinite maxima; the overlay must retain the
        // framebuffer's finite geometry rather than expanding the scroll range.
        layout::Node::new(limits.resolve(
            Length::Fixed(self.logical_size.width),
            Length::Fixed(self.logical_size.height),
            self.logical_size,
        ))
    }
    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(shape) = self.cursor.as_ref() else {
            return;
        };
        let bounds = layout.bounds();
        let Some(point) = cursor.position_over(bounds) else {
            return;
        };
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let target = cursor_bounds(point, shape, self.scale_x, self.scale_y);
        if let Some(clip) = bounds.intersection(viewport) {
            iced::advanced::image::Renderer::draw_image(
                renderer,
                iced::advanced::image::Image::new(handle.clone()),
                target,
                clip,
            );
        }
    }
    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if self.ime_enabled {
            let point = cursor
                .position_over(layout.bounds())
                .unwrap_or(layout.bounds().position());
            shell.request_input_method(&iced::advanced::input_method::InputMethod::Enabled {
                cursor: Rectangle::new(point, Size::new(1.0, 20.0)),
                purpose: iced::advanced::input_method::Purpose::Terminal,
                preedit: self.preedit.clone(),
            });
            if let iced::Event::InputMethod(event) = event {
                shell.publish(Message::VncImeEvent(
                    self.session_id,
                    self.input_epoch,
                    event.clone(),
                ));
                shell.capture_event();
            }
        }
    }
    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if self.cursor.is_some() && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Hidden
        } else {
            mouse::Interaction::None
        }
    }
}

fn cursor_bounds(
    point: Point,
    shape: &adit_session::VncCursor,
    scale_x: f32,
    scale_y: f32,
) -> Rectangle {
    Rectangle {
        x: point.x - f32::from(shape.hotspot_x) / scale_x,
        y: point.y - f32::from(shape.hotspot_y) / scale_y,
        width: f32::from(shape.width) / scale_x,
        height: f32::from(shape.height) / scale_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonzero_hotspot_stays_at_pointer_across_dpi_and_fit() {
        let shape = adit_session::VncCursor {
            width: 32,
            height: 24,
            hotspot_x: 7,
            hotspot_y: 11,
            rgba: vec![255; 32 * 24 * 4],
            generation: 1,
        };
        for dpi in [1.0, 1.5, 2.0] {
            for fit in [0.5, 1.0, 1.7] {
                let scale = dpi / fit;
                let point = Point::new(230.0, 145.0);
                let b = cursor_bounds(point, &shape, scale, scale);
                assert!((b.x + 7.0 / scale - point.x).abs() < 0.001);
                assert!((b.y + 11.0 / scale - point.y).abs() < 0.001);
                assert_eq!(b.width, 32.0 / scale);
            }
        }
    }
}
