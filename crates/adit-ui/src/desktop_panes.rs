//! Independent desktop surfaces for the mixed mission grid.
//!
//! Frames and pointer leases belong to SessionId, so the keyboard owner can
//! change without painting or releasing input into a different host.
//! Focused/fullscreen presentation keeps the existing full desktop renderer.

use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct DesktopPanes {
    surfaces: HashMap<SessionId, DesktopPane>,
    sampled_at: Option<Instant>,
    // Input leases outlive textures: hiding/pruning a pane must not erase the
    // information needed to release its remote buttons.
    owned_buttons: Vec<(SessionId, mouse::Button)>,
}

#[derive(Default)]
struct DesktopPane {
    generation: u64,
    size: Option<(u16, u16)>,
    tiles: Vec<RdpTile>,
    error: Option<String>,
    // Body-local logical points, including the letterbox. Never remote pixels.
    pointer: Option<Point>,
}

impl DesktopPanes {
    fn take_button(&mut self, owner: Option<SessionId>, button: mouse::Button) -> Vec<SessionId> {
        let mut owners = Vec::new();
        self.owned_buttons.retain(|(id, held)| {
            if *held == button && owner.is_none_or(|owner| owner == *id) {
                owners.push(*id);
                false
            } else {
                true
            }
        });
        owners
    }

    fn retain(&mut self, visible: &[SessionId]) {
        self.surfaces.retain(|id, _| visible.contains(id));
        if visible.is_empty() {
            self.sampled_at = None;
        }
    }

    fn observe(&mut self, id: SessionId, frame: &adit_session::RdpFrame) {
        let pane = self.surfaces.entry(id).or_default();
        if pane.generation == frame.generation {
            return;
        }
        pane.generation = frame.generation;
        let size = (frame.width, frame.height);
        let expected = usize::from(frame.width)
            .checked_mul(usize::from(frame.height))
            .and_then(|pixels| pixels.checked_mul(4));
        if frame.width == 0 || frame.height == 0 || expected != Some(frame.rgba.len()) {
            pane.size = None;
            pane.tiles.clear();
            pane.pointer = None;
            pane.error = Some(format!(
                "远程桌面帧数据无效：{} × {}，收到 {} 字节",
                frame.width,
                frame.height,
                frame.rgba.len()
            ));
            return;
        }
        if pane.size != Some(size) {
            pane.pointer = None;
        }
        pane.size = Some(size);
        pane.error = None;
        // Use the existing synchronous-upload tile size. A VNC snapshot may
        // contain transparent, unreceived regions; do not fill them with pixels
        // from an older resize epoch or from a different session.
        pane.tiles = split_into_tiles(frame);
    }
}

fn split_visible(app: &AditApp) -> bool {
    app.main_view == MainView::Missions && app.panes.len() >= 2 && !app.fullscreen
}

fn connected_desktop(app: &AditApp, id: SessionId) -> bool {
    app.manager.session_is_desktop(id)
        && app
            .manager
            .session_summary(id)
            .is_some_and(|summary| summary.status == SessionStatus::Connected)
        && app.manager.session_desktop_error(id).is_none()
}

/// Include this in the window::frames subscription predicate: the keyboard
/// owner may be an SSH pane while a different visible pane is painting.
pub(crate) fn needs_frames(app: &AditApp) -> bool {
    split_visible(app)
        && app.panes.iter().any(|id| {
            app.manager.session_is_desktop(*id)
                && app.manager.session_desktop_error(*id).is_none()
                && app.manager.session_summary(*id).is_some_and(|summary| {
                    matches!(
                        summary.status,
                        SessionStatus::Connecting | SessionStatus::Connected
                    )
                })
        })
}

/// Prune before throttling: an Error must retire its pixels on the very next
/// update even when the previous upload was less than 33 ms ago.
pub(crate) fn refresh(app: &mut AditApp) {
    let visible: Vec<_> = if split_visible(app) {
        app.panes
            .iter()
            .copied()
            .filter(|id| connected_desktop(app, *id))
            .collect()
    } else {
        Vec::new()
    };
    if app.desktop_panes.owned_buttons.iter().any(|(id, _)| {
        !visible.contains(id)
            || app.manager.active_session() != Some(*id)
            || app.manager.session_desktop_read_only(*id)
            || local_input_blocked(app)
    }) {
        release_owned(app);
    }
    app.desktop_panes.retain(&visible);
    if visible.is_empty() {
        return;
    }
    let now = Instant::now();
    if app
        .desktop_panes
        .sampled_at
        .is_some_and(|last| now.duration_since(last) < RDP_FRAME_MIN_INTERVAL)
    {
        return;
    }
    app.desktop_panes.sampled_at = Some(now);
    for id in visible {
        let generation = app
            .desktop_panes
            .surfaces
            .get(&id)
            .map_or(0, |pane| pane.generation);
        if let Some(frame) = app.manager.session_desktop_frame_if_newer(id, generation) {
            app.desktop_panes.observe(id, &frame);
        }
    }
}

/// Geometry is shared by painting and hit testing. The framebuffer is measured
/// in device pixels; pane bounds and mouse_area coordinates are logical points.
#[derive(Clone, Copy, Debug)]
struct Contain {
    remote: (u16, u16),
    width: f32,
    height: f32,
    left: f32,
    top: f32,
}

impl Contain {
    fn new(remote: (u16, u16), pane_w: f32, pane_h: f32, display_scale: f32) -> Option<Self> {
        if remote.0 == 0
            || remote.1 == 0
            || !pane_w.is_finite()
            || !pane_h.is_finite()
            || !display_scale.is_finite()
            || pane_w <= 0.0
            || pane_h <= 0.0
            || display_scale <= 0.0
        {
            return None;
        }
        let fit = ((pane_w * display_scale) / f32::from(remote.0))
            .min((pane_h * display_scale) / f32::from(remote.1));
        let logical_scale = fit / display_scale;
        // Shared rounded tile edges eliminate DPI seams. Use the same rounded
        // outer extent for the letterbox and the inverse pointer transform.
        let width = (f32::from(remote.0) * logical_scale).round().min(pane_w);
        let height = (f32::from(remote.1) * logical_scale).round().min(pane_h);
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return None;
        }
        Some(Self {
            remote,
            width,
            height,
            left: (pane_w - width) / 2.0,
            top: (pane_h - height) / 2.0,
        })
    }

    fn remote_point(self, local: Point) -> Option<(u16, u16)> {
        let x = local.x - self.left;
        let y = local.y - self.top;
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= self.width
            || y >= self.height
        {
            return None;
        }
        Some((
            ((x / self.width * f32::from(self.remote.0)) as u16).min(self.remote.0 - 1),
            ((y / self.height * f32::from(self.remote.1)) as u16).min(self.remote.1 - 1),
        ))
    }

    fn edge_x(self, x: u32) -> f32 {
        (x as f32 * self.width / f32::from(self.remote.0))
            .round()
            .min(self.width)
    }

    fn edge_y(self, y: u32) -> f32 {
        (y as f32 * self.height / f32::from(self.remote.1))
            .round()
            .min(self.height)
    }
}

fn transform(app: &AditApp, id: SessionId) -> Option<Contain> {
    let layout = pane_layout(app);
    Contain::new(
        app.desktop_panes.surfaces.get(&id)?.size?,
        layout.pane_w,
        layout.pane_h - layout.header,
        app.display_scale,
    )
}

fn tile_grid(tiles: &[RdpTile], fit: Contain) -> Element<'static, Message> {
    let mut rows: Vec<Element<'static, Message>> = Vec::new();
    let mut row: Vec<Element<'static, Message>> = Vec::new();
    let mut current_y = None;
    let mut x = 0_u32;
    for tile in tiles {
        if current_y != Some(tile.y) {
            if !row.is_empty() {
                rows.push(iced::widget::Row::with_children(std::mem::take(&mut row)).into());
            }
            current_y = Some(tile.y);
            x = 0;
        }
        let next_x = x + u32::from(tile.width);
        let y = u32::from(tile.y);
        row.push(
            iced::widget::image(tile.handle.clone())
                .width(Length::Fixed(fit.edge_x(next_x) - fit.edge_x(x)))
                .height(Length::Fixed(
                    fit.edge_y(y + u32::from(tile.height)) - fit.edge_y(y),
                ))
                .filter_method(iced::widget::image::FilterMethod::Nearest)
                .content_fit(iced::ContentFit::Fill)
                .into(),
        );
        x = next_x;
    }
    if !row.is_empty() {
        rows.push(iced::widget::Row::with_children(row).into());
    }
    iced::widget::Column::with_children(rows)
        .width(Length::Fixed(fit.width))
        .height(Length::Fixed(fit.height))
        .into()
}

fn pane_notice(app: &AditApp, id: SessionId) -> Option<String> {
    if let Some(error) = app.manager.session_desktop_error(id) {
        return Some(error.to_owned());
    }
    let Some(summary) = app.manager.session_summary(id) else {
        return Some(t("会话已关闭").to_owned());
    };
    let status = match summary.status {
        SessionStatus::Connecting => Some(t("正在连接远程桌面…")),
        SessionStatus::Disconnected => Some(t("连接已断开，按 Enter 重新连接")),
        SessionStatus::Error => Some(t("远程桌面连接失败，请重新连接")),
        SessionStatus::Connected => None,
    };
    if let Some(status) = status {
        return Some(status.to_owned());
    }
    let Some(pane) = app.desktop_panes.surfaces.get(&id) else {
        return Some(t("等待远程桌面画面…").to_owned());
    };
    if let Some(error) = &pane.error {
        return Some(error.clone());
    }
    (pane.tiles.is_empty() || transform(app, id).is_none())
        .then(|| t("等待远程桌面画面…").to_owned())
}

/// A complete headed pane; the caller selects this only for desktop sessions
/// in the split grid. No floating single-desktop toolbar is installed here.
pub(crate) fn pane_view(app: &AditApp, id: SessionId, index: usize) -> Element<'_, Message> {
    let focused = index == app.focused_pane;
    let summary = app.manager.session_summary(id);
    let title = summary
        .as_ref()
        .map_or_else(|| t("会话").to_owned(), |s| s.title.clone());
    let status = summary.map_or(SessionStatus::Disconnected, |s| s.status);
    let read_only = app.manager.session_desktop_read_only(id);
    let mut labels = row![
        text("●").size(9).color(status_color(status)),
        text(title).size(11).color(primary_text()).width(Fill),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if read_only {
        labels = labels.push(text(t("只读")).size(11).color(muted_text()));
    }
    let header = mouse_area(
        container(
            labels
                .push(
                    button(text(t("聚焦")).size(11))
                        .padding([0, 6])
                        .style(|_, status| secondary_button_style(status))
                        .on_press(Message::FocusMissionPane(index)),
                )
                .push(
                    button(text("×").size(13))
                        .padding([0, 6])
                        .style(|_, status| tab_close_button_style(status))
                        .on_press(Message::ClosePane(index)),
                ),
        )
        .padding([1, 6])
        .height(Length::Fixed(PANE_HEADER_HEIGHT))
        .width(Fill)
        .center_y(Length::Fixed(PANE_HEADER_HEIGHT))
        .style(move |_| pane_header_style(focused)),
    )
    .on_press(Message::FocusPane(index))
    .interaction(mouse::Interaction::Pointer);

    let body: Element<'_, Message> = if let Some(notice) = pane_notice(app, id) {
        // Evaluate current session state during view as well as refresh, so a
        // queued Error cannot leave stale pixels visible until the next Tick.
        mouse_area(
            container(text(notice).size(13).color(muted_text()))
                .padding(12)
                .center_x(Fill)
                .center_y(Fill)
                .style(|_| desktop_background()),
        )
        .on_press(Message::FocusPane(index))
        .into()
    } else if let (Some(pane), Some(fit)) =
        (app.desktop_panes.surfaces.get(&id), transform(app, id))
    {
        // Mouse coordinates include the black bars. Testing the inverse
        // transform rejects those bars instead of clicking the remote edge.
        mouse_area(
            container(tile_grid(&pane.tiles, fit))
                .center_x(Fill)
                .center_y(Fill)
                .style(|_| desktop_background()),
        )
        .on_move(move |point| Message::DesktopPaneMoved(id, point))
        .on_exit(Message::DesktopPaneMoved(
            id,
            Point::new(f32::NAN, f32::NAN),
        ))
        .on_press(Message::DesktopPanePressed(id, mouse::Button::Left))
        .on_release(Message::DesktopPaneReleased(id, mouse::Button::Left))
        .on_right_press(Message::DesktopPanePressed(id, mouse::Button::Right))
        .on_right_release(Message::DesktopPaneReleased(id, mouse::Button::Right))
        .on_middle_press(Message::DesktopPanePressed(id, mouse::Button::Middle))
        .on_middle_release(Message::DesktopPaneReleased(id, mouse::Button::Middle))
        .on_scroll(move |delta| Message::DesktopPaneScrolled(id, delta))
        .into()
    } else {
        container(text(t("等待远程桌面画面…")))
            .center_x(Fill)
            .center_y(Fill)
            .style(|_| desktop_background())
            .into()
    };
    iced::widget::column![header, body]
        .spacing(0)
        .width(Fill)
        .height(Fill)
        .into()
}

fn desktop_background() -> container::Style {
    container::Style {
        background: Some(Color::BLACK.into()),
        ..container::Style::default()
    }
}

/// Pointer input has the same local modal exclusions as desktop keyboard
/// input, including context menus whose mouse_area may ignore a wheel event.
pub(crate) fn local_input_blocked(app: &AditApp) -> bool {
    !app.desktop_window_focused
        || app.missions.picker
        || mission_name_editing(app)
        || app.active_menu.is_some()
        || app.profile_context_menu.is_some()
        || app.tab_context_menu.is_some()
        || app.group_context_menu.is_some()
        || app.move_to_group_menu
        || app.terminal_context_menu
        || app.settings_open
        || app.profile_editor.is_some()
        || app.connection_dialog.is_some()
        || app.manager.pending_host_key().is_some()
        || app.auth_prompt.is_some()
        || app.pending_hyperlink.is_some()
        || app.manager.sftp_is_open()
        || app.tunnels_open
        || app.about_open
        || app.update_dialog_open
        || app.paste_confirm_open
        || app.renaming_session.is_some()
        || app.snippets_open
        || app.known_hosts_open
        || app.command_window_open
        || app.search_open
        || app.editing_profile.is_some()
        || app.editing_group.is_some()
        || app.rdp_quality_menu_open
}

fn input_pane(app: &AditApp, id: SessionId) -> Option<usize> {
    if !split_visible(app)
        || !connected_desktop(app, id)
        || app.manager.session_desktop_read_only(id)
        || local_input_blocked(app)
    {
        return None;
    }
    app.panes.iter().position(|pane| *pane == id)
}

fn pointer_pixels(app: &AditApp, id: SessionId) -> Option<(u16, u16)> {
    transform(app, id)?.remote_point(app.desktop_panes.surfaces.get(&id)?.pointer?)
}

fn focus_target(app: &mut AditApp, id: SessionId, index: usize) -> bool {
    if app.manager.active_session() != Some(id) {
        // Release while the outgoing transport is still active. Clearing the
        // bookkeeping here would prevent the parent's cleanup from seeing it.
        release_owned(app);
    }
    session_ops::focus_pane(app, index);
    app.manager.active_session() == Some(id)
}

/// Hover records this pane's local position without stealing keyboard focus.
/// Only the active pane forwards motion; press/scroll focus explicitly.
pub(crate) fn moved(app: &mut AditApp, id: SessionId, point: Point) {
    let allowed = input_pane(app, id).is_some();
    if let Some(pane) = app.desktop_panes.surfaces.get_mut(&id) {
        pane.pointer = (allowed && point.x.is_finite() && point.y.is_finite()).then_some(point);
    }
    if !allowed || app.manager.active_session() != Some(id) {
        return;
    }
    if let Some((x, y)) = pointer_pixels(app, id) {
        app.manager
            .send_rdp_input_to_active(RdpInput::MouseMove { x, y });
    }
}

pub(crate) fn pressed(app: &mut AditApp, id: SessionId, button: mouse::Button) {
    let Some(index) = input_pane(app, id) else {
        return;
    };
    let Some(button_remote) = rdp_mouse_button(button) else {
        return;
    };
    let Some((x, y)) = pointer_pixels(app, id) else {
        return;
    };
    if !focus_target(app, id, index) {
        return;
    }
    // A newly focused VNC session still has its own old pointer position until
    // the first move. Deliver the position before its button mask changes.
    app.manager
        .send_rdp_input_to_active(RdpInput::MouseMove { x, y });
    app.manager.send_rdp_input_to_active(RdpInput::MouseButton {
        button: button_remote,
        pressed: true,
    });
    if !app.desktop_panes.owned_buttons.contains(&(id, button)) {
        app.desktop_panes.owned_buttons.push((id, button));
    }
}

fn send_owned_release(app: &AditApp, id: SessionId, button: mouse::Button) {
    // Never reactivate a hidden/outgoing session or release a different host's
    // button. The parent must call release_owned BEFORE changing activation.
    if app.manager.active_session() != Some(id) {
        return;
    }
    if let Some(button) = rdp_mouse_button(button) {
        app.manager.send_rdp_input_to_active(RdpInput::MouseButton {
            button,
            pressed: false,
        });
    }
}

/// Parent release_rdp_modifiers calls this before switching, hiding, opening a
/// modal, or losing window focus. Cleanup deliberately bypasses input_pane:
/// the pane may already be hidden or covered. No keys or activation are changed.
pub(crate) fn release_owned(app: &mut AditApp) {
    for (id, button) in std::mem::take(&mut app.desktop_panes.owned_buttons) {
        send_owned_release(app, id, button);
    }
}

/// Feed every physical window ButtonReleased here, including captured events
/// and releases outside the body. Returns whether a split-pane lease consumed
/// the up; fallback is only appropriate for false in the single-desktop view.
/// This and released are idempotent in either delivery order.
pub(crate) fn global_button_released(app: &mut AditApp, button: mouse::Button) -> bool {
    let owners = app.desktop_panes.take_button(None, button);
    for &id in &owners {
        send_owned_release(app, id, button);
    }
    !owners.is_empty()
}

pub(crate) fn released(app: &mut AditApp, id: SessionId, button: mouse::Button) {
    for owner in app.desktop_panes.take_button(Some(id), button) {
        send_owned_release(app, owner, button);
    }
}

fn wheel_units(delta: mouse::ScrollDelta) -> Option<(bool, i16)> {
    let (x, y) = match delta {
        mouse::ScrollDelta::Lines { x, y } => (x, y),
        mouse::ScrollDelta::Pixels { x, y } => (x / 20.0, y / 20.0),
    };
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let (vertical, amount) = if y.abs() >= x.abs() {
        (true, y)
    } else {
        (false, x)
    };
    let units = (amount * 120.0).clamp(-32768.0, 32767.0) as i16;
    (units != 0).then_some((vertical, units))
}

pub(crate) fn scrolled(app: &mut AditApp, id: SessionId, delta: mouse::ScrollDelta) {
    let Some(index) = input_pane(app, id) else {
        return;
    };
    let Some((x, y)) = pointer_pixels(app, id) else {
        return;
    };
    let Some((vertical, delta)) = wheel_units(delta) else {
        return;
    };
    if focus_target(app, id, index) {
        app.manager
            .send_rdp_input_to_active(RdpInput::MouseMove { x, y });
        app.manager
            .send_rdp_input_to_active(RdpInput::Wheel { vertical, delta });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(generation: u64, rgba: [u8; 4]) -> adit_session::RdpFrame {
        adit_session::RdpFrame {
            width: 2,
            height: 1,
            rgba: rgba.repeat(2),
            generation,
        }
    }

    fn pixels(pane: &DesktopPane) -> &[u8] {
        let iced::widget::image::Handle::Rgba { pixels, .. } = &pane.tiles[0].handle else {
            panic!("desktop tiles must own decoded RGBA");
        };
        pixels.as_ref()
    }

    #[test]
    fn contain_maps_letterboxed_body_points_at_each_display_scale() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let fit = Contain::new((1920, 1080), 800.0, 600.0, scale).unwrap();
            assert_eq!(
                (fit.width, fit.height, fit.left, fit.top),
                (800.0, 450.0, 0.0, 75.0)
            );
            assert_eq!(fit.remote_point(Point::new(400.0, 300.0)), Some((960, 540)));
            assert_eq!(fit.remote_point(Point::new(0.0, 75.0)), Some((0, 0)));
            assert_eq!(
                fit.remote_point(Point::new(799.99, 524.99)),
                Some((1919, 1079))
            );
            for outside in [
                Point::new(400.0, 74.99),
                Point::new(400.0, 525.0),
                Point::new(800.0, 300.0),
                Point::new(-0.1, 300.0),
                Point::new(f32::NAN, f32::NAN),
            ] {
                assert_eq!(fit.remote_point(outside), None);
            }
        }
    }

    #[test]
    fn portrait_frame_uses_side_bars_and_header_is_outside_the_body() {
        let fit = Contain::new((600, 1200), 900.0, 626.0 - PANE_HEADER_HEIGHT, 1.5).unwrap();
        assert_eq!(
            (fit.width, fit.height, fit.left, fit.top),
            (300.0, 600.0, 300.0, 0.0)
        );
        assert_eq!(fit.remote_point(Point::new(450.0, 300.0)), Some((300, 600)));
        assert_eq!(fit.remote_point(Point::new(299.9, 300.0)), None);
        assert_eq!(fit.remote_point(Point::new(600.0, 300.0)), None);
    }

    #[test]
    fn shared_tile_edges_cover_fractional_pane_extent_without_overflow() {
        let fit = Contain::new((1920, 1080), 639.5, 400.5, 1.25).unwrap();
        let edges = [0, 512, 1024, 1536, 1920].map(|x| fit.edge_x(x));
        let total: f32 = edges.windows(2).map(|pair| pair[1] - pair[0]).sum();
        assert_eq!(total, fit.width);
        assert_eq!(fit.edge_y(1080), fit.height);
        assert!(fit.width <= 639.5 && fit.height <= 400.5);
        assert_eq!(
            fit.remote_point(Point::new(fit.left + fit.width, fit.top)),
            None
        );
    }

    #[test]
    fn degenerate_contain_and_nonfinite_wheels_are_rejected() {
        assert!(Contain::new((0, 10), 100.0, 100.0, 1.0).is_none());
        assert!(Contain::new((10, 10), 100.0, -1.0, 1.0).is_none());
        assert!(Contain::new((10, 10), 100.0, 100.0, f32::NAN).is_none());
        assert_eq!(
            wheel_units(mouse::ScrollDelta::Lines { x: 0.0, y: 0.0 }),
            None
        );
        assert_eq!(
            wheel_units(mouse::ScrollDelta::Lines {
                x: f32::NAN,
                y: 1.0
            }),
            None
        );
        assert_eq!(
            wheel_units(mouse::ScrollDelta::Pixels { x: -40.0, y: 1.0 }),
            Some((false, -240))
        );
        assert_eq!(
            wheel_units(mouse::ScrollDelta::Lines { x: 0.0, y: 500.0 }),
            Some((true, i16::MAX))
        );
    }

    #[test]
    fn equal_generations_from_two_sessions_keep_distinct_pixels_and_handles() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut cache = DesktopPanes::default();
        cache.observe(a, &frame(7, [255, 0, 0, 255]));
        cache.observe(b, &frame(7, [0, 0, 255, 255]));
        assert_eq!(pixels(&cache.surfaces[&a]), &[255, 0, 0, 255].repeat(2));
        assert_eq!(pixels(&cache.surfaces[&b]), &[0, 0, 255, 255].repeat(2));
        let a_handle = cache.surfaces[&a].tiles[0].handle.id();
        assert_ne!(a_handle, cache.surfaces[&b].tiles[0].handle.id());
        cache.observe(a, &frame(7, [0, 255, 0, 255]));
        assert_eq!(cache.surfaces[&a].tiles[0].handle.id(), a_handle);
        cache.observe(b, &frame(8, [0, 255, 0, 255]));
        assert_eq!(cache.surfaces[&a].tiles[0].handle.id(), a_handle);
        assert_eq!(pixels(&cache.surfaces[&b]), &[0, 255, 0, 255].repeat(2));
    }

    #[test]
    fn invalid_frame_retires_old_pixels_and_a_new_frame_recovers() {
        let id = SessionId::new();
        let mut cache = DesktopPanes::default();
        cache.observe(id, &frame(1, [1, 2, 3, 255]));
        let pane = cache.surfaces.get_mut(&id).unwrap();
        pane.pointer = Some(Point::new(1.0, 1.0));
        cache.owned_buttons.push((id, mouse::Button::Left));
        let mut invalid = frame(2, [1, 2, 3, 255]);
        invalid.rgba.pop();
        cache.observe(id, &invalid);
        let pane = &cache.surfaces[&id];
        assert!(pane.error.as_ref().unwrap().contains("7"));
        assert!(pane.tiles.is_empty() && pane.size.is_none() && pane.pointer.is_none());
        assert_eq!(cache.owned_buttons, vec![(id, mouse::Button::Left)]);
        cache.observe(id, &frame(3, [4, 5, 6, 255]));
        assert!(cache.surfaces[&id].error.is_none());
        assert_eq!(pixels(&cache.surfaces[&id]), &[4, 5, 6, 255].repeat(2));
    }

    #[test]
    fn pruning_and_returning_session_reset_its_generation_and_input() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut cache = DesktopPanes::default();
        cache.observe(a, &frame(30, [1, 2, 3, 255]));
        cache.observe(b, &frame(1, [4, 5, 6, 255]));
        cache.sampled_at = Some(Instant::now());
        cache.retain(&[b]);
        assert!(!cache.surfaces.contains_key(&a));
        assert_eq!(cache.surfaces[&b].generation, 1);
        cache.observe(a, &frame(1, [7, 8, 9, 255]));
        assert_eq!(pixels(&cache.surfaces[&a]), &[7, 8, 9, 255].repeat(2));
        assert!(cache.surfaces[&a].pointer.is_none());
        cache.retain(&[]);
        assert!(cache.surfaces.is_empty() && cache.sampled_at.is_none());
    }

    #[test]
    fn button_leases_survive_pruning_and_each_up_consumes_only_its_owner() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut cache = DesktopPanes {
            owned_buttons: vec![
                (a, mouse::Button::Left),
                (b, mouse::Button::Left),
                (a, mouse::Button::Right),
            ],
            ..DesktopPanes::default()
        };
        cache.retain(&[]);
        assert_eq!(cache.take_button(Some(b), mouse::Button::Left), vec![b]);
        assert_eq!(cache.take_button(None, mouse::Button::Left), vec![a]);
        assert!(cache.take_button(Some(a), mouse::Button::Left).is_empty());
        assert_eq!(cache.owned_buttons, vec![(a, mouse::Button::Right)]);
    }

    // An actual RFB peer proves that sampling and input reach independent
    // session transports. Everything stays on loopback with explicit None auth.
    fn framebuffer_peer(
        color: [u8; 3],
        fail_after_presses: usize,
    ) -> (
        u16,
        std::sync::mpsc::Receiver<(u8, u16, u16)>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(b"RFB 003.008\n").unwrap();
            let mut version = [0; 12];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            stream.write_all(&[1, 1]).unwrap();
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            assert_eq!(byte, [1]);
            stream.write_all(&[0; 4]).unwrap();
            stream.read_exact(&mut byte).unwrap();
            assert_eq!(byte, [1]);
            let mut init = vec![
                0, 2, 0, 1, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0,
            ];
            init.extend(4_u32.to_be_bytes());
            init.extend(b"pane");
            stream.write_all(&init).unwrap();
            let mut painted = false;
            let mut presses = 0;
            while stream.read_exact(&mut byte).is_ok() {
                let mut body = vec![
                    0;
                    match byte[0] {
                        0 => 19,
                        2 => 3,
                        3 => 9,
                        4 => 7,
                        5 => 5,
                        other => panic!("unexpected loopback client message {other}"),
                    }
                ];
                if stream.read_exact(&mut body).is_err() {
                    break;
                }
                match byte[0] {
                    2 => {
                        let count = usize::from(u16::from_be_bytes([body[1], body[2]]));
                        stream.read_exact(&mut vec![0; count * 4]).unwrap();
                    }
                    3 if !painted => {
                        let mut update = vec![0, 0, 0, 1];
                        for value in [0_u16, 0, 2, 1] {
                            update.extend(value.to_be_bytes());
                        }
                        update.extend(0_i32.to_be_bytes());
                        update.extend([color[0], color[1], color[2], 0].repeat(2));
                        stream.write_all(&update).unwrap();
                        painted = true;
                    }
                    5 => {
                        tx.send((
                            body[0],
                            u16::from_be_bytes([body[1], body[2]]),
                            u16::from_be_bytes([body[3], body[4]]),
                        ))
                        .unwrap();
                        if body[0] != 0 {
                            presses += 1;
                            // A deterministic real transport error after the
                            // cleanup checks, while cached pixels exist.
                            if presses == fail_after_presses {
                                stream.write_all(&[255]).unwrap();
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        (port, rx, peer)
    }

    #[test]
    fn loopback_frames_sample_behind_terminal_and_click_error_retires_only_its_pane() {
        let (port_a, wire_a, peer_a) = framebuffer_peer([200, 10, 20], 3);
        let (port_b, wire_b, peer_b) = framebuffer_peer([30, 40, 210], 0);
        let profiles: Vec<_> = [port_a, port_b]
            .into_iter()
            .enumerate()
            .map(|(index, port)| {
                let mut profile = ConnectionProfile::new(
                    format!("synthetic desktop {index}"),
                    "127.0.0.1",
                    port,
                    "",
                );
                profile.protocol = Protocol::Vnc;
                profile.vnc.security = VncSecurity::AllowNone;
                profile.vnc.auto_reconnect = false;
                profile.vnc.read_only = index == 1;
                profile
            })
            .collect();
        let terminal = ConnectionProfile::new("synthetic terminal", "adit.invalid", 22, "test");
        let terminal_profile = terminal.id;
        let (profile_a, profile_b) = (profiles[0].id, profiles[1].id);
        let mut manager =
            SessionManager::with_profiles(profiles.into_iter().chain([terminal]).collect());
        let a = manager
            .open_live_vnc_session(profile_a, String::new())
            .unwrap();
        let b = manager
            .open_live_vnc_session(profile_b, String::new())
            .unwrap();
        let terminal = manager.open_mock_session(terminal_profile).unwrap();
        let mut app = AditApp {
            manager,
            desktop_window_focused: true,
            ..AditApp::default()
        };
        missions::tile_mission(&mut app, vec![a, b, terminal], TileMode::Grid);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.manager.poll_events();
            refresh(&mut app);
            assert_eq!(app.manager.active_session(), Some(terminal));
            assert_eq!(app.focused_pane, 2);
            let painted = [(a, [200, 10, 20, 255]), (b, [30, 40, 210, 255])]
                .into_iter()
                .all(|(id, color)| {
                    app.desktop_panes.surfaces.get(&id).is_some_and(|pane| {
                        !pane.tiles.is_empty() && pixels(pane) == color.repeat(2)
                    })
                });
            if painted {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "loopback desktop frames timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(needs_frames(&app));
        assert!(!app.manager.active_is_desktop());
        assert!(app.manager.session_desktop_read_only(b));
        assert_eq!(input_pane(&app, b), None);
        let b_handle = app.desktop_panes.surfaces[&b].tiles[0].handle.id();
        let layout = pane_layout(&app);
        let center = Point::new(layout.pane_w / 2.0, (layout.pane_h - layout.header) / 2.0);
        moved(&mut app, a, center);
        assert_eq!(
            app.manager.active_session(),
            Some(terminal),
            "hover stole keyboard focus"
        );
        app.settings_open = true;
        assert_eq!(input_pane(&app, a), None);
        pressed(&mut app, a, mouse::Button::Left);
        app.settings_open = false;
        pressed(&mut app, b, mouse::Button::Left);
        assert_eq!(
            app.manager.active_session(),
            Some(terminal),
            "modal/read-only input was accepted"
        );
        moved(&mut app, a, Point::new(-1.0, center.y));
        pressed(&mut app, a, mouse::Button::Left);
        assert_eq!(
            app.manager.active_session(),
            Some(terminal),
            "letterbox/outside input was accepted"
        );
        moved(&mut app, a, center);
        pressed(&mut app, a, mouse::Button::Left);
        assert_eq!(app.manager.active_session(), Some(a));
        assert_eq!(app.focused_pane, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (mask, x, y) = wire_a.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!((x, y), (1, 0));
            if mask == 1 {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        moved(&mut app, a, Point::new(f32::NAN, f32::NAN));
        assert!(global_button_released(&mut app, mouse::Button::Left));
        assert_eq!(
            wire_a.recv_timeout(Duration::from_secs(5)).unwrap(),
            (0, 1, 0)
        );
        assert!(!global_button_released(&mut app, mouse::Button::Left));
        released(&mut app, a, mouse::Button::Left);
        moved(&mut app, a, center);
        pressed(&mut app, a, mouse::Button::Left);
        app.settings_open = true;
        release_owned(&mut app);
        release_owned(&mut app);
        // Hover and press both send the position. The asynchronous input queue
        // may coalesce those zero-mask moves or deliver both; require the real
        // press/up order rather than a scheduling-dependent packet count.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_press = false;
        loop {
            let (mask, x, y) = wire_a.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!((x, y), (1, 0));
            if mask == 1 {
                saw_press = true;
            }
            if saw_press && mask == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "owned button was not released");
        }
        assert!(app.desktop_panes.owned_buttons.is_empty());
        app.settings_open = false;
        pressed(&mut app, a, mouse::Button::Left);
        assert!(
            wire_b.try_recv().is_err(),
            "input reached the read-only background pane"
        );
        release_owned(&mut app);
        missions::activate_mission_session(&mut app, terminal);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.manager.session_desktop_error(a).is_none() {
            app.manager.poll_events();
            assert!(
                Instant::now() < deadline,
                "loopback error did not reach the session"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let error = app.manager.session_desktop_error(a).unwrap().to_owned();
        assert_eq!(pane_notice(&app, a), Some(error));
        assert!(app.manager.session_desktop_frame_if_newer(a, 0).is_none());
        // Pruning must precede the 33 ms upload gate.
        app.desktop_panes.sampled_at = Some(Instant::now());
        refresh(&mut app);
        assert!(!app.desktop_panes.surfaces.contains_key(&a));
        assert_eq!(
            app.desktop_panes.surfaces[&b].tiles[0].handle.id(),
            b_handle
        );
        pressed(&mut app, a, mouse::Button::Left);
        assert_eq!(app.manager.active_session(), Some(terminal));
        app.manager.close(a);
        app.manager.close(b);
        peer_a.join().unwrap();
        peer_b.join().unwrap();
    }
}
