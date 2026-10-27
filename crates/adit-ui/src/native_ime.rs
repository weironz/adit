//! Composition leases include the owning window (outer daemon message), session,
//! and input epoch. A late commit after a focus transfer cannot type elsewhere.
use super::*;
use iced::advanced::input_method::{Event as ImeEvent, Preedit};
use iced::advanced::{layout, renderer, widget::Tree, Layout, Widget};

pub(crate) fn update(app: &mut AditApp, session: SessionId, epoch: u64, event: ImeEvent) {
    if app.manager.active_session() != Some(session)
        || app.vnc_input_epoch != epoch
        || !remote_workspace_accepts_input(app)
        || app.manager.active_is_vnc()
    {
        return;
    }
    match event {
        ImeEvent::Commit(text) => {
            app.native_preedit = None;
            if app.manager.active_is_desktop() {
                if app.manager.session_desktop_read_only(session) {
                    return;
                }
                for ch in text.chars() {
                    app.manager
                        .send_rdp_input_to_active(RdpInput::Unicode { ch, pressed: true });
                    app.manager
                        .send_rdp_input_to_active(RdpInput::Unicode { ch, pressed: false });
                }
            } else {
                send_terminal_bytes(app, text.into_bytes());
            }
        }
        ImeEvent::Preedit(content, selection) => {
            app.native_preedit = Some(Preedit {
                content,
                selection,
                text_size: None,
            });
        }
        ImeEvent::Closed => app.native_preedit = None,
        ImeEvent::Opened => {}
    }
}

pub(crate) fn layer(
    session: SessionId,
    epoch: u64,
    enabled: bool,
    preedit: Option<Preedit>,
) -> Element<'static, Message> {
    Element::new(ImeLayer {
        extent: None,
        session,
        epoch,
        enabled,
        preedit,
    })
}

pub(crate) fn surface_layer(
    session: SessionId,
    epoch: u64,
    enabled: bool,
    preedit: Option<Preedit>,
    extent: iced::Size,
) -> Element<'static, Message> {
    Element::new(ImeLayer {
        session,
        epoch,
        enabled,
        preedit,
        extent: Some(extent),
    })
}

struct ImeLayer {
    extent: Option<iced::Size>,
    session: SessionId,
    epoch: u64,
    enabled: bool,
    preedit: Option<Preedit>,
}
impl Widget<Message, Theme, iced::Renderer> for ImeLayer {
    fn size(&self) -> iced::Size<Length> {
        self.extent
            .map(|s| iced::Size::new(Length::Fixed(s.width), Length::Fixed(s.height)))
            .unwrap_or_else(|| iced::Size::new(Fill, Fill))
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = self.size();
        layout::Node::new(limits.resolve(
            size.width,
            size.height,
            self.extent.unwrap_or(iced::Size::ZERO),
        ))
    }
    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &iced::Rectangle,
    ) {
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
        _viewport: &iced::Rectangle,
    ) {
        if !self.enabled {
            return;
        }
        let point = cursor
            .position_over(layout.bounds())
            .unwrap_or(layout.bounds().position());
        shell.request_input_method(&iced::advanced::input_method::InputMethod::Enabled {
            cursor: iced::Rectangle::new(point, iced::Size::new(1.0, 20.0)),
            purpose: iced::advanced::input_method::Purpose::Terminal,
            preedit: self.preedit.clone(),
        });
        if let iced::Event::InputMethod(event) = event {
            shell.publish(Message::NativeImeEvent(
                self.session,
                self.epoch,
                event.clone(),
            ));
            shell.capture_event();
        }
    }
}
