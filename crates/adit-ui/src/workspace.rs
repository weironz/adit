use super::*;
use iced::widget::column;

pub(crate) fn workspace(app: &AditApp) -> Element<'_, Message> {
    let tabs = app
        .manager
        .sessions()
        .into_iter()
        .fold(
            row![hosts_tab_button(app.main_view == MainView::Hosts)]
                .spacing(2)
                .height(TAB_BAR_HEIGHT),
            |tabs, session| {
            let accent = profile_accent(app, session.profile_id);
            let badge = profile_badge(app, session.profile_id);
            tabs.push(tab_button(
                session,
                app.manager.active_session(),
                app.dragged_tab,
                accent,
                badge,
                ))
            },
        );

    // Split panes: 2–4 tiled sessions. Otherwise the single-pane view, left
    // byte-for-byte as before (it is the well-tested selection/hit-test path).
    let body: Element<'_, Message> = if app.main_view == MainView::Hosts {
        hosts_view(app)
    } else if app.panes.len() >= 2 {
        tiled_workspace_body(app)
    } else if app.manager.active_is_rdp() {
        rdp_surface_view(app)
    } else {
        let snapshot = active_terminal_snapshot(app);
        let highlights = search_highlights_for(app, &snapshot);
        let links_clickable = link_open_modifier(app);
        mouse_area(terminal_view(
            snapshot,
            app.terminal_focused,
            app.terminal_selection,
            app.terminal_scroll_offset,
            highlights,
            links_clickable,
            app.terminal_focused && app.cursor_blink_on,
            // Single-pane view: the scrollbar always drives the one terminal.
            true,
        ))
        .on_press(Message::BeginTerminalSelection)
        .on_release(Message::EndTerminalSelection)
        .on_right_press(Message::ShowTerminalContextMenu)
        .on_move(Message::TerminalPointerMoved)
        .on_scroll(Message::TerminalScrolled)
        .interaction(mouse::Interaction::Text)
        .into()
    };

    // With no sessions open the strip holds one 主机 tab and the word 空闲,
    // and nothing else — a full row spent on a button the sidebar already has
    // (▦ 返回主机列表) and a status both the status bar and the title bar
    // already show. Hiding it then is safe by construction: no sessions means
    // the host view is what is on screen, so there is nothing to switch to.
    let has_sessions = !app.manager.sessions().is_empty();
    let mut layout = column![].height(Fill).width(Fill);
    if has_sessions {
        layout = layout.push(
            row![
                scrollable(tabs).direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new()
                )),
                active_session_action(app),
                container(text(app.manager.status_line()).size(12).color(muted_text()))
                    .padding([0, 8])
                    .center_y(TAB_BAR_HEIGHT),
                Space::new().width(Fill),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .height(TAB_BAR_HEIGHT)
            .width(Fill),
        );
    }
    if app.search_open {
        layout = layout.push(terminal_search_bar(app));
    }
    layout = layout.push(body);
    if app.command_window_open {
        layout = layout.push(command_window_bar(app));
    }

    container(layout)
        .padding(0)
        .style(|_theme| workspace_style())
        .height(Fill)
        .width(Fill)
        .into()
}

/// The active RDP session's framebuffer, scaled to fit (aspect-preserved), with
/// mouse and scroll captured and mapped to remote-desktop pixels. A single OS
/// cursor is shown (the server pointer isn't composited), so there's no
/// double-cursor; its shape is a plain arrow for now.
pub(crate) fn rdp_surface_view(app: &AditApp) -> Element<'_, Message> {
    let (sw, sh) = app.rdp_surface_size.unwrap_or((0, 0));
    // One scale for the whole function, folding the fit factor into the DPI
    // scale. Everything below — tile sizes and the mouse mapping — divides or
    // multiplies by this single number, which is what keeps the picture and the
    // pointer from disagreeing about where a pixel is.
    let display_scale = app.display_scale.max(0.1) / rdp_fit_factor(app, (sw, sh));

    if app.rdp_tiles.is_empty() || sw == 0 || sh == 0 {
        // The toolbar wraps this branch too: a connect that hangs in fullscreen
        // is exactly when someone needs the way out, and there is no menu bar.
        return with_rdp_toolbar(
            app,
            container(text(t("正在连接 RDP…")).size(14).color(muted_text()))
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill)
                .style(|_theme| container::Style {
                    background: Some(Color::BLACK.into()),
                    ..container::Style::default()
                })
                .into(),
        );
    }

    // The texture arrives as a grid of tiles, each small enough for the
    // renderer's synchronous upload path (see `split_into_tiles`). Lay them
    // back out edge to edge: rows of images, each sized in logical points so
    // the compositor's DPI scaling lands every device pixel on exactly one
    // frame pixel and Nearest never resamples.
    // Seams are computed from SHARED edges, never from each tile's own size.
    //
    // `tile_size / scale` is fractional at any non-integer display scale
    // (512 / 1.25 = 409.6), and laying fractional boxes end to end leaves the
    // rounding error between them — a hairline of container showing through
    // at every row and column boundary. Rounding each cumulative edge once and
    // taking differences means neighbours agree on their shared edge by
    // construction, so the seams close exactly and the sizes still sum to the
    // full surface.
    let edge = |device: u32| (device as f32 / display_scale).round();
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut current_y: Option<u16> = None;
    let mut row: Vec<Element<'_, Message>> = Vec::new();
    let mut device_x: u32 = 0;
    let mut device_y: u32 = 0;
    for tile in &app.rdp_tiles {
        if current_y != Some(tile.y) {
            if !row.is_empty() {
                rows.push(iced::widget::Row::with_children(std::mem::take(&mut row)).into());
                device_y = u32::from(tile.y);
            }
            current_y = Some(tile.y);
            device_x = 0;
        }
        let next_x = device_x + u32::from(tile.width);
        let next_y = device_y + u32::from(tile.height);
        row.push(
            iced::widget::image(tile.handle.clone())
                .width(Length::Fixed(edge(next_x) - edge(device_x)))
                .height(Length::Fixed(edge(next_y) - edge(device_y)))
                .filter_method(iced::widget::image::FilterMethod::Nearest)
                .content_fit(iced::ContentFit::Fill)
                .into(),
        );
        device_x = next_x;
    }
    if !row.is_empty() {
        rows.push(iced::widget::Row::with_children(row).into());
    }

    let surface = mouse_area(iced::widget::Column::with_children(rows))
        .on_move(move |p| {
            let x = (p.x * display_scale).clamp(0.0, f32::from(sw) - 1.0);
            let y = (p.y * display_scale).clamp(0.0, f32::from(sh) - 1.0);
            Message::RdpPointerMoved(Point::new(x, y))
        })
        .on_press(Message::RdpPressed(mouse::Button::Left))
        .on_release(Message::RdpReleased(mouse::Button::Left))
        .on_right_press(Message::RdpPressed(mouse::Button::Right))
        .on_right_release(Message::RdpReleased(mouse::Button::Right))
        .on_middle_press(Message::RdpPressed(mouse::Button::Middle))
        .on_middle_release(Message::RdpReleased(mouse::Button::Middle))
        .on_scroll(Message::RdpScrolled);

    // Centred, and scrollable when the desktop is momentarily larger than the
    // pane (a resize renegotiation in flight): clipping a fixed-size child
    // would otherwise hide part of it with no way to reach it.
    let desktop = container(scrollable(container(surface).center_x(Fill)).direction(
        iced::widget::scrollable::Direction::Both {
            vertical: iced::widget::scrollable::Scrollbar::new().width(0).scroller_width(0),
            horizontal: iced::widget::scrollable::Scrollbar::new().width(0).scroller_width(0),
        },
    ))
    .width(Fill)
    .height(Fill)
    .style(|_theme| container::Style {
        background: Some(Color::BLACK.into()),
        ..container::Style::default()
    })
    .into();

    with_rdp_toolbar(app, desktop)
}

/// Stack the floating toolbar over a remote desktop, in fullscreen only.
///
/// Windowed sessions already have the menu bar and the tab strip; a second row
/// of icons there would be the 36px of duplicated shortcuts that the previous
/// toolbar was deleted for (see `chrome::view`). Fullscreen is the mode that
/// genuinely has nothing, so it is the mode that gets this.
pub(crate) fn with_rdp_toolbar<'a>(
    app: &'a AditApp,
    desktop: Element<'a, Message>,
) -> Element<'a, Message> {
    if !app.fullscreen {
        return desktop;
    }

    let open = app.rdp_toolbar_pinned || app.rdp_toolbar_hovered || app.rdp_quality_menu_open;

    // The reveal strip: a thin band along the top edge that only has to notice
    // the pointer. It stays in the stack even when the bar is open, because it
    // is what keeps `rdp_toolbar_hovered` true in the gap above the bar.
    let strip = column![
        mouse_area(container(Space::new()).width(Fill).height(Length::Fixed(RDP_TOOLBAR_REVEAL_PX)))
            .on_enter(Message::RdpToolbarHovered(true))
            .on_exit(Message::RdpToolbarHovered(false)),
        Space::new().height(Fill),
    ]
    .width(Fill)
    .height(Fill);

    let mut layers: Vec<Element<'a, Message>> = vec![desktop, strip.into()];
    if open {
        layers.push(rdp_toolbar_overlay(app));
        if app.rdp_quality_menu_open {
            layers.push(rdp_quality_menu_overlay(app));
        }
    }
    stack(layers).into()
}

/// The floating bar itself: centred at the top, over the desktop, reserving no
/// layout space.
fn rdp_toolbar_overlay(app: &AditApp) -> Element<'_, Message> {
    let live = app.manager.active_rdp_live();

    let buttons = row![
        // Pin first, matching every floating toolbar that has one: it is the
        // control that decides whether the others stay reachable.
        rdp_toolbar_button("📌", t("固定工具栏"), app.rdp_toolbar_pinned, Some(Message::ToggleRdpToolbarPin)),
        rdp_toolbar_button("⚡", t("画质"), app.rdp_quality_menu_open, Some(Message::ToggleRdpQualityMenu)),
        rdp_toolbar_button(
            "⤢",
            t("缩放适应窗口"),
            app.rdp_scale_fit,
            Some(Message::ToggleRdpScaleFit),
        ),
        // Ctrl+Alt+Del and the clipboard are meaningless without a live desktop
        // to receive them, so they grey out rather than firing into a dead pipe.
        rdp_toolbar_button(
            "⌨",
            t("发送 Ctrl+Alt+Del"),
            false,
            live.then_some(Message::RdpSendCtrlAltDel),
        ),
        rdp_toolbar_button(
            "📋",
            t("剪贴板同步"),
            app.rdp_clipboard,
            Some(Message::ToggleRdpClipboard(!app.rdp_clipboard)),
        ),
        rdp_toolbar_separator(),
        rdp_toolbar_button("⛶", t("退出全屏"), false, Some(Message::ToggleFullscreen)),
        rdp_toolbar_button(
            "✕",
            t("断开会话"),
            false,
            app.manager.active_session().map(Message::CloseSession),
        ),
    ]
    .spacing(2)
    .align_y(Alignment::Center);

    // `mouse_area` around the bar, not just the strip above it: without it the
    // bar would hide the moment the pointer left the strip to reach a button,
    // which is to say it could never be clicked.
    let bar = mouse_area(
        container(buttons)
            .padding([4, 6])
            .style(|_theme| rdp_toolbar_style()),
    )
    .on_enter(Message::RdpToolbarHovered(true))
    .on_exit(Message::RdpToolbarHovered(false));

    column![
        row![Space::new().width(Fill), bar, Space::new().width(Fill)],
        Space::new().height(Fill),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// The ⚡ dropdown. Picking an entry reconnects the session, so each one says so.
fn rdp_quality_menu_overlay(app: &AditApp) -> Element<'_, Message> {
    let mut entries = column![].spacing(0).width(Fill);
    for (quality, label, hint) in [
        (RdpQuality::High, "高清", "局域网：桌面特效全开"),
        (RdpQuality::Balanced, "均衡", "默认：保留壁纸与主题"),
        (RdpQuality::Speed, "流畅", "慢速链路：关闭壁纸与主题"),
    ] {
        let chosen = app.rdp_quality == quality;
        entries = entries.push(
            button(
                column![
                    text(format!("{} {}", if chosen { "●" } else { "○" }, t(label))).size(12),
                    text(t(hint)).size(10).color(muted_text()),
                ]
                .spacing(1),
            )
            .width(Fill)
            .padding([6, 12])
            .style(|_theme, status| menu_command_button_style(status))
            .on_press(Message::RdpQualityChosen(quality)),
        );
    }

    let card = container(
        column![
            entries,
            container(
                text(t("切换后会重新连接（RDP 只在握手时协商画质）"))
                    .size(10)
                    .color(muted_text())
            )
            .padding([4, 12]),
        ]
        .spacing(2),
    )
    .width(Length::Fixed(212.0))
    .padding([5, 0])
    .style(|_theme| menu_dropdown_style());

    // A full-screen click-catcher behind the card so clicking anywhere else
    // closes the dropdown instead of landing on the remote desktop.
    let backdrop = mouse_area(container(Space::new()).width(Fill).height(Fill))
        .on_press(Message::ToggleRdpQualityMenu);

    let positioned = column![
        Space::new().height(Length::Fixed(RDP_TOOLBAR_HEIGHT)),
        row![
            Space::new().width(Fill),
            card,
            // Nudges the card under the ⚡ button rather than under the centre
            // of the bar: ⚡ is the second of eight, left of centre.
            Space::new().width(Length::Fixed(RDP_QUALITY_MENU_RIGHT_PAD)),
        ],
        Space::new().height(Fill),
    ]
    .width(Fill)
    .height(Fill);

    stack(vec![backdrop.into(), positioned.into()]).into()
}

fn rdp_toolbar_button(
    glyph: &'static str,
    tip: &'static str,
    active: bool,
    on_press: Option<Message>,
) -> Element<'static, Message> {
    let enabled = on_press.is_some();
    let control = button(
        container(text(glyph).size(15))
            .center_x(Length::Fixed(30.0))
            .center_y(Length::Fixed(26.0)),
    )
    .padding(0)
    .style(move |_theme, status| rdp_toolbar_button_style(active, enabled, status));
    let control = match on_press {
        Some(message) => control.on_press(message),
        None => control,
    };
    // The glyphs carry no words, so the label has to come from somewhere; a
    // tooltip is the only place it fits without making the bar three times wider.
    tooltip(
        control,
        container(text(tip).size(11))
            .padding([3, 7])
            .style(|_theme| tooltip_style()),
        tooltip::Position::Bottom,
    )
    .into()
}

fn rdp_toolbar_separator() -> Element<'static, Message> {
    container(Space::new().width(Length::Fixed(1.0)).height(Length::Fixed(18.0)))
        .padding([0, 4])
        .style(|_theme| container::Style {
            background: Some(muted_text().scale_alpha(0.35).into()),
            ..container::Style::default()
        })
        .into()
}

/// Map an iced mouse button to the RDP button set (`Other` is ignored).
pub(crate) fn rdp_mouse_button(button: mouse::Button) -> Option<RdpMouseButton> {
    match button {
        mouse::Button::Left => Some(RdpMouseButton::Left),
        mouse::Button::Right => Some(RdpMouseButton::Right),
        mouse::Button::Middle => Some(RdpMouseButton::Middle),
        mouse::Button::Back => Some(RdpMouseButton::X1),
        mouse::Button::Forward => Some(RdpMouseButton::X2),
        mouse::Button::Other(_) => None,
    }
}

/// The scrollback-search bar shown above the terminal (Ctrl+Shift+F).
pub(crate) fn terminal_search_bar(app: &AditApp) -> Element<'_, Message> {
    let count = app.search_matches.len();
    let status = if app.search_query.is_empty() {
        String::new()
    } else if count == 0 {
        String::from(t("无匹配"))
    } else {
        format!("{}/{}", app.search_index.map(|i| i + 1).unwrap_or(0), count)
    };

    container(
        row![
            text(t("查找")).size(12).color(muted_text()),
            text_input("搜索终端历史…", &app.search_query)
                .id(search_input_id())
                .on_input(Message::SearchQueryChanged)
                .on_submit(Message::SearchNext)
                .padding([4, 8])
                .style(text_input_style)
                .width(Length::Fixed(280.0)),
            container(text(status).size(11).color(muted_text()))
                .width(Length::Fixed(64.0)),
            button(text("↑").size(13))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SearchPrev),
            button(text("↓").size(13))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SearchNext),
            Space::new().width(Fill),
            button(text("×").size(14))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CloseSearch),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .width(Fill)
    .style(|_theme| toolbar_style())
    .into()
}

/// The bottom command window: type a line and send it to the active session or
/// broadcast it to every session, SecureCRT-style. The text lives in
/// `terminal_input`; sending / history / send-immediately are handled here.
pub(crate) fn command_window_bar(app: &AditApp) -> Element<'_, Message> {
    let target = app.command_target;
    let broadcasting = target == CommandTarget::AllSessions;
    let target_label = if broadcasting {
        tf("→ 所有会话 ({})", &[&app.manager.live_session_count()])
    } else {
        format!("→ {}", target.label())
    };

    let placeholder = if app.command_send_immediately {
        "逐字符即时发送到目标…（回车提交整行）"
    } else if broadcasting {
        "输入命令，回车广播到所有会话"
    } else {
        "输入命令，回车发送到当前会话"
    };

    let immediate = app.command_send_immediately;

    container(
        row![
            button(text(target_label).size(12))
                .padding([4, 10])
                .style(move |_theme, status| if broadcasting {
                    base_button_style(accent(), Color::from_rgb8(245, 249, 255), transparent())
                } else {
                    secondary_button_style(status)
                })
                .on_press(Message::CommandTargetToggled),
            text_input(placeholder, &app.terminal_input)
                .id(command_input_id())
                .on_input(Message::TerminalInputChanged)
                .on_submit(Message::SendTerminalInput)
                .padding([4, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("▲").size(11))
                .padding([3, 8])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CommandHistoryPrev),
            button(text("▼").size(11))
                .padding([3, 8])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CommandHistoryNext),
            button(text(t("即时")).size(12))
                .padding([4, 10])
                .style(move |_theme, status| if immediate {
                    base_button_style(accent(), Color::from_rgb8(245, 249, 255), transparent())
                } else {
                    secondary_button_style(status)
                })
                .on_press(Message::ToggleCommandSendImmediately),
            button(text(t("发送")).size(12))
                .padding([4, 14])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SendTerminalInput),
            button(text("×").size(14))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::ToggleCommandWindow),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .width(Fill)
    .style(|_theme| toolbar_style())
    .into()
}

/// Tile the current `panes` into a row/grid, each a headed terminal pane.
pub(crate) fn tiled_workspace_body(app: &AditApp) -> Element<'_, Message> {
    let layout = pane_layout(app);
    let mut grid = column![].spacing(PANE_GAP).width(Fill).height(Fill);
    let mut idx = 0usize;

    while idx < app.panes.len() {
        let mut r = row![].spacing(PANE_GAP).width(Fill).height(Fill);
        for _ in 0..layout.cols {
            if idx >= app.panes.len() {
                break;
            }
            let session_id = app.panes[idx];
            r = r.push(
                container(terminal_pane(app, session_id, idx))
                    .width(Length::FillPortion(1))
                    .height(Fill),
            );
            idx += 1;
        }
        grid = grid.push(r);
    }

    grid.into()
}

/// One split pane: a clickable header (session title + close-pane ×) over a
/// terminal body wired to pane-scoped input/selection messages.
pub(crate) fn terminal_pane(app: &AditApp, session_id: SessionId, index: usize) -> Element<'static, Message> {
    let is_focused = index == app.focused_pane;
    let summary = app.manager.session_summary(session_id);
    let title = summary
        .as_ref()
        .map(|summary| summary.title.clone())
        .unwrap_or_else(|| String::from(t("会话")));
    let status = summary
        .map(|summary| summary.status)
        .unwrap_or(SessionStatus::Disconnected);

    let header = mouse_area(
        container(
            row![
                text("●").size(9).color(status_color(status)),
                text(title).size(11).color(primary_text()).width(Fill),
                button(text("×").size(13))
                    .padding([0, 6])
                    .style(|_theme, status| tab_close_button_style(status))
                    .on_press(Message::ClosePane(index)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([1, 6])
        .height(Length::Fixed(PANE_HEADER_HEIGHT))
        .width(Fill)
        .style(move |_theme| pane_header_style(is_focused)),
    )
    .on_press(Message::FocusPane(index))
    .interaction(mouse::Interaction::Pointer);

    let snapshot = pane_snapshot(app, session_id, is_focused);
    let selection = if is_focused {
        app.terminal_selection
    } else {
        None
    };
    let highlights = if is_focused {
        search_highlights_for(app, &snapshot)
    } else {
        Vec::new()
    };
    let body = mouse_area(terminal_view(
        snapshot,
        is_focused,
        selection,
        app.terminal_scroll_offset,
        highlights,
        link_open_modifier(app),
        // Only the focused pane shows a cursor — that's what marks it as the one
        // taking keystrokes.
        is_focused && app.terminal_focused && app.cursor_blink_on,
        // Only the focused pane's scrollbar drives scrolling (the offset is shared).
        is_focused,
    ))
    .on_press(Message::PaneMousePressed(index))
    .on_release(Message::EndTerminalSelection)
    .on_right_press(Message::PaneRightPressed(index))
    .on_move(move |point| Message::PanePointerMoved(index, point))
    .on_scroll(Message::TerminalScrolled)
    .interaction(mouse::Interaction::Text);

    column![header, body]
        .spacing(0)
        .width(Fill)
        .height(Fill)
        .into()
}

/// Snapshot for a pane; only the focused pane honors the scroll-back offset.
pub(crate) fn pane_snapshot(app: &AditApp, session_id: SessionId, is_focused: bool) -> TerminalSnapshot {
    let rows = terminal_view_rows(app);
    let tail = app.manager.snapshot_for(session_id, Viewport::tail(rows));

    if !is_focused || app.terminal_scroll_offset == 0 {
        return tail;
    }

    let offset = app
        .terminal_scroll_offset
        .min(max_scroll_offset_for(&tail, rows));
    let first_row = tail.total_rows.saturating_sub(rows).saturating_sub(offset);
    app.manager.snapshot_for(
        session_id,
        Viewport {
            first_row,
            height: rows,
        },
    )
}

pub(crate) fn active_session_action(app: &AditApp) -> Element<'_, Message> {
    if app.manager.active_session_summary().is_some_and(|summary| {
        matches!(
            summary.status,
            SessionStatus::Error | SessionStatus::Disconnected
        )
    }) {
        return button(text(t("重连")).size(12))
            .padding([4, 10])
            .style(|_theme, status| primary_button_style(status))
            .on_press(Message::RetryActiveSession)
            .into();
    }

    Space::new().width(Length::Shrink).into()
}

pub(crate) fn tab_button(
    session: SessionSummary,
    active_session: Option<SessionId>,
    dragged: Option<SessionId>,
    accent: Option<Color>,
    badge: Option<String>,
) -> Element<'static, Message> {
    let id = session.id;
    let active = Some(id) == active_session;
    // The tab currently being dragged gets a "lifted" accent so its live
    // reordering is easy to follow.
    let is_dragging = dragged == Some(id);

    // The whole pill is a mouse_area (click = activate, drag = reorder); only the
    // close × stays a button so it can consume its own click.
    let mut inner = row![text("●").size(10).color(status_color(session.status))]
        .spacing(6)
        .align_y(Alignment::Center);
    // Environment badge (e.g. PROD) so an operator can tell prod from staging.
    if let Some(badge_text) = badge {
        let badge_bg = accent.unwrap_or_else(muted_text);
        inner = inner.push(
            container(text(badge_text).size(9).color(Color::WHITE))
                .padding([1, 5])
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(badge_bg)),
                    border: iced::border::rounded(4),
                    ..container::Style::default()
                }),
        );
    }
    let inner = inner
        .push(text(session.title).size(12).color(primary_text()))
        .push(
            button(text("×").size(15))
                .padding([2, 7])
                .style(|_theme, status| tab_close_button_style(status))
                .on_press(Message::CloseSession(id)),
        );

    mouse_area(
        container(inner)
            .padding([2, 6])
            .style(move |_theme| tab_container_style_dnd(active, is_dragging)),
    )
    .on_press(Message::TabPressed(id))
    .on_release(Message::TabReleased)
    .on_enter(Message::TabDragOver(id))
    .on_right_press(Message::ShowTabContextMenu(id))
    .interaction(mouse::Interaction::Pointer)
    .into()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn terminal_view(
    snapshot: TerminalSnapshot,
    focused: bool,
    selection: Option<TerminalSelection>,
    scroll_offset: usize,
    search_highlights: Vec<Vec<(usize, usize, bool)>>,
    links_clickable: bool,
    show_cursor: bool,
    scrollbar_interactive: bool,
) -> Element<'static, Message> {
    // Capture the counts the scrollbar needs before `snapshot.lines` is consumed.
    let total_rows = snapshot.total_rows;
    let viewport = snapshot.lines.len();
    // The selection is anchored in absolute scrollback rows; map it into this
    // snapshot's viewport rows (clipped to the visible window) to render it.
    let selection =
        selection.and_then(|sel| selection_for_viewport(sel, snapshot.first_row, viewport));
    let alt_screen = snapshot.alt_screen;
    let lines = if snapshot.lines.is_empty() {
        column![text("not connected")
            .size(13)
            .font(Font::MONOSPACE)
            .color(default_foreground())]
    } else {
        snapshot
            .lines
            .into_iter()
            .enumerate()
            .fold(column![].spacing(0), |column, (row_index, line)| {
                let highlights = search_highlights
                    .get(row_index)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                // Suppressed wholesale on the alternate screen: vim, less, htop
                // and tmux paint every cell themselves, so a rule firing inside
                // one of their status lines is noise at best.
                let keywords = if alt_screen {
                    Vec::new()
                } else {
                    highlight::spans_for(&line)
                };
                column.push(terminal_line(
                    line,
                    row_index,
                    selection,
                    highlights,
                    &keywords,
                    links_clickable,
                    show_cursor,
                ))
            })
    };

    // The context menu now floats (see the layers stack in `view`), so the
    // terminal body no longer reserves a strip for it. A scrollbar rides the right
    // edge, inside the panel padding, showing (and driving) the scrollback position.
    let body = row![
        container(lines).height(Fill).width(Fill),
        terminal_scrollbar(total_rows, viewport, scroll_offset, scrollbar_interactive),
    ];
    container(body)
        .padding(TERMINAL_PANEL_PADDING as u16)
        .height(Fill)
        .width(Fill)
        .style(move |_theme| terminal_panel_style(focused))
        .into()
}

/// Width of the terminal scrollbar gutter, in pixels.
pub(crate) const SCROLLBAR_WIDTH: f32 = 12.0;

/// The vertical scrollback scrollbar for the terminal body.
///
/// The terminal is not an iced `scrollable` (its content is a fixed viewport-sized
/// grid; scrollback is served by re-snapshotting at a different offset), so this is
/// a hand-built thumb. Thumb height ≈ viewport/total; its position runs bottom =
/// newest (offset 0) to top = oldest (max offset). Interactive only on the pane
/// that owns the scroll — dragging is wired through `scrollbar_drag_to`.
pub(crate) fn terminal_scrollbar(
    total: usize,
    viewport: usize,
    offset: usize,
    interactive: bool,
) -> Element<'static, Message> {
    // Nothing to scroll: keep an empty gutter so the text width doesn't jump when
    // scrollback first appears.
    if total <= viewport || viewport == 0 {
        return container(Space::new())
            .width(Length::Fixed(SCROLLBAR_WIDTH))
            .height(Fill)
            .into();
    }

    let max_offset = total - viewport;
    // Per-mille weights for FillPortion so the thumb tracks size+position without
    // needing pixel heights here (the drag handler does the pixel math).
    let thumb = (((viewport as f32 / total as f32) * 1000.0).round() as u16).clamp(48, 1000);
    let travel = 1000u16.saturating_sub(thumb);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let below = ((offset as f32 / max_offset as f32) * f32::from(travel)).round() as u16;
    let above = travel.saturating_sub(below);

    let thumb_bar = container(Space::new())
        .width(Fill)
        .height(Length::FillPortion(thumb.max(1)))
        .style(|_theme| container::Style {
            background: Some(Background::Color(scrollbar_thumb_color())),
            border: iced::border::rounded(SCROLLBAR_WIDTH / 2.0 - 2.0),
            ..container::Style::default()
        });

    let track = column![
        Space::new().height(Length::FillPortion(above)),
        thumb_bar,
        Space::new().height(Length::FillPortion(below)),
    ]
    .width(Length::Fixed(SCROLLBAR_WIDTH - 4.0));

    let gutter = container(track)
        .width(Length::Fixed(SCROLLBAR_WIDTH))
        .height(Fill)
        .align_x(Alignment::Center)
        .style(|_theme| container::Style {
            background: Some(Background::Color(scrollbar_track_color())),
            border: iced::border::rounded(SCROLLBAR_WIDTH / 2.0),
            ..container::Style::default()
        });

    if !interactive {
        return gutter.into();
    }
    mouse_area(gutter)
        .on_press(Message::BeginScrollbarDrag)
        .interaction(mouse::Interaction::Pointer)
        .into()
}

/// The terminal context-menu card (used inside the floating overlay).
pub(crate) fn terminal_context_menu() -> Element<'static, Message> {
    container(
        column![
            profile_menu_item("复制", Message::CopyTerminalSelection, false),
            profile_menu_item("粘贴", Message::PasteIntoTerminal, false),
            profile_menu_divider(),
            profile_menu_item("清屏", Message::ClearActiveTerminal, false),
            profile_menu_item("回到底部", Message::TerminalJumpToBottom, false),
        ]
        .spacing(1),
    )
    .padding(4)
    .width(Length::Fixed(PROFILE_MENU_WIDTH))
    .style(|_theme| profile_context_menu_style())
    .into()
}

pub(crate) fn terminal_line(
    line: TerminalLine,
    row_index: usize,
    selection: Option<TerminalSelection>,
    search: &[(usize, usize, bool)],
    // Keyword-highlight spans for this row, as `(start_col, end_col, ansi)`.
    // Already restricted to cells the server left uncoloured — see
    // `highlight::Highlighter::spans`.
    keywords: &[(usize, usize, TermColor)],
    links_clickable: bool,
    show_cursor: bool,
) -> Element<'static, Message> {
    let font_size = term_font_size();
    let base_font = term_font();
    let cell_w = cell_width();
    let cell_h = cell_height();

    if line.cells.is_empty() {
        // Preserve the exact row height of a visually blank terminal line.
        return container(text(" ").size(font_size).font(base_font))
            .height(Length::Fixed(cell_h))
            .into();
    }

    let selected_range =
        selection.and_then(|selection| selection_range_for_row(selection, row_index));
    let selected_fg = selection_foreground();
    let mut col = 0_usize;
    let mut row_widget = row![].spacing(0);

    for cell in line.cells {
        let mut fg = term_color(cell.fg, default_foreground());
        if cell.dim {
            fg = dim_color(fg);
        }
        // OSC 8 hyperlink: only present openable http(s) links as links (a
        // non-openable scheme stays plain text, not a dead blue click target).
        // The glyph is wrapped in a click target only while the open modifier
        // (Ctrl/Cmd) is held, so a plain click always falls through to selection
        // and mouse-report passthrough rather than being swallowed.
        let link_url = cell
            .hyperlink
            .filter(|url| is_openable_http_url(url));
        let is_link = link_url.is_some();
        let link_click = if links_clickable { link_url.clone() } else { None };
        let font = Font {
            weight: if cell.bold {
                Weight::Bold
            } else {
                Weight::Normal
            },
            style: if cell.italic {
                iced::font::Style::Italic
            } else {
                iced::font::Style::Normal
            },
            ..base_font
        };

        for ch in cell.text.chars() {
            // A CJK/wide glyph occupies two grid columns; size its cell to two so
            // it doesn't overflow into (and garble) the next column, and advance
            // the column counter by two to keep selection/hit-testing aligned.
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            let selected = selected_range.is_some_and(|range| col >= range.0 && col < range.1);
            let search_hit = search
                .iter()
                .find_map(|(start, end, current)| (col >= *start && col < *end).then_some(*current));
            let keyword_hit = keywords
                .iter()
                .find_map(|(start, end, color)| (col >= *start && col < *end).then_some(*color));

            // The text cursor is a reverse-video block on its own cell: the glyph
            // takes the background colour and vice-versa, so it reads correctly in
            // any theme and stays legible over whatever it sits on.
            let cursor_here = cell.cursor && show_cursor;
            let glyph_color = if cursor_here {
                // Under the block, the glyph takes the cell's background — which
                // for a default cell is the terminal's own background colour.
                term_color(cell.bg, terminal_background())
            } else if selected {
                selected_fg
            } else if let Some(current) = search_hit {
                if current {
                    Color::from_rgb8(24, 24, 24)
                } else {
                    Color::from_rgb8(245, 236, 210)
                }
            } else if is_link {
                hyperlink_color()
            } else if let Some(color) = keyword_hit {
                // Last before the cell's own colour, so selection, search and
                // OSC 8 links all outrank a local rule. `spans` has already
                // guaranteed this cell had no colour of the server's to lose.
                //
                // Resolved through the scheme like every other colour on screen,
                // rather than being a fixed RGB — that is what keeps a highlight
                // looking like part of the terminal instead of shouting over it.
                term_color(color, fg)
            } else {
                fg
            };
            let label = text(ch.to_string())
                .size(font_size)
                .font(font)
                .color(glyph_color);

            let background = if cursor_here {
                Some(fg)
            } else if selected {
                Some(selection_background())
            } else if let Some(current) = search_hit {
                Some(if current {
                    Color::from_rgb8(240, 180, 60)
                } else {
                    Color::from_rgb8(96, 82, 44)
                })
            } else {
                match cell.bg {
                    TermColor::Default => None,
                    other => Some(term_color(other, default_foreground())),
                }
            };

            // Fixed-size cell so the rendered grid exactly matches the
            // pixel→cell hit-testing used for selection (no drift). Wide glyphs
            // span two columns.
            let cell_box = container(label)
                .width(Length::Fixed(cell_w * ch_width as f32))
                .height(Length::Fixed(cell_h))
                .style(move |_theme| container::Style {
                    background: background.map(Background::Color),
                    ..container::Style::default()
                });
            // SGR 9: iced's text has no strikethrough, so draw a rule across the
            // glyph's middle. Stacked so it doesn't disturb the cell's fixed size.
            let glyph: Element<'static, Message> = if cell.strike {
                stack![
                    cell_box,
                    container(
                        container(Space::new().width(Fill).height(Length::Fixed(1.0)))
                            .width(Fill)
                            .style(move |_theme| container::Style {
                                background: Some(Background::Color(glyph_color)),
                                ..container::Style::default()
                            })
                    )
                    .width(Length::Fixed(cell_w * ch_width as f32))
                    .height(Length::Fixed(cell_h))
                    .center_y(Fill),
                ]
                .into()
            } else {
                cell_box.into()
            };
            let glyph: Element<'static, Message> = match &link_click {
                Some(url) => mouse_area(glyph)
                    .on_press(Message::OpenHyperlink(url.clone()))
                    .interaction(mouse::Interaction::Pointer)
                    .into(),
                None => glyph,
            };
            row_widget = row_widget.push(glyph);

            col += ch_width;
        }
    }

    row_widget.into()
}

/// Dim (SGR 2) foreground: scale the glyph color toward black so faint text
/// reads as fainter than normal.
pub(crate) fn dim_color(color: Color) -> Color {
    Color {
        r: color.r * 0.6,
        g: color.g * 0.6,
        b: color.b * 0.6,
        a: color.a,
    }
}

/// Text color for selected cells: dark on a light selection highlight, light on
/// a dark one, so selected glyphs stay legible across every scheme.
pub(crate) fn selection_foreground() -> Color {
    let (r, g, b) = active_scheme().selection;
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance > 140.0 {
        Color::from_rgb8(20, 22, 28)
    } else {
        Color::from_rgb8(245, 249, 255)
    }
}

pub(crate) fn default_foreground() -> Color {
    let (r, g, b) = active_scheme().foreground;
    Color::from_rgb8(r, g, b)
}

/// Scrollbar thumb colour: the scheme's foreground, softened so it reads as chrome
/// rather than text (light-on-dark or dark-on-light both work out).
pub(crate) fn scrollbar_thumb_color() -> Color {
    let fg = default_foreground();
    Color { a: 0.35, ..fg }
}

/// Scrollbar track colour: a barely-there tint over the terminal background.
pub(crate) fn scrollbar_track_color() -> Color {
    let fg = default_foreground();
    Color { a: 0.08, ..fg }
}

/// Resolve an Adit terminal color into a concrete iced color, using `fallback`
/// for the theme default and the xterm 256-color palette for indexed colors.
pub(crate) fn term_color(color: TermColor, fallback: Color) -> Color {
    match color {
        TermColor::Default => fallback,
        TermColor::Rgb(r, g, b) => Color::from_rgb8(r, g, b),
        TermColor::Indexed(index) => palette_color(index),
    }
}

/// The standard xterm 256-color palette: 16 named colors, a 6x6x6 RGB cube, and
/// a 24-step grayscale ramp.
pub(crate) fn palette_color(index: u8) -> Color {
    match index {
        0..=15 => {
            let (r, g, b) = active_scheme().ansi[index as usize];
            Color::from_rgb8(r, g, b)
        }
        16..=231 => {
            let c = index - 16;
            let level = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            Color::from_rgb8(level(c / 36), level((c / 6) % 6), level(c % 6))
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            Color::from_rgb8(value, value, value)
        }
    }
}

pub(crate) fn status_bar(app: &AditApp) -> Element<'_, Message> {
    let status = if let Some(error) = &app.last_error {
        format!("Error: {error}")
    } else {
        app.notice.clone()
    };

    // Left cluster: a red REC badge while the active session is logging,
    // followed by the current status/notice text.
    let mut left = row![].spacing(7).align_y(Alignment::Center);
    if app.manager.active_is_logging() {
        left = left
            .push(text("●").size(11).color(danger()))
            .push(text("REC").size(11).color(danger()));
    }
    if app.broadcast_input {
        // Always-visible warning that keystrokes fan out to every session.
        let reach = app.manager.live_session_count();
        left = left
            .push(text("⇶").size(12).color(accent()))
            .push(text(tf("广播 ×{}", &[&reach])).size(11).color(accent()));
    }
    left = left.push(text(status).size(12).color(muted_text()));

    container(
        row![
            left,
            Space::new().width(Fill),
            text(app.manager.status_line()).size(12).color(muted_text()),
            Space::new().width(Length::Fixed(18.0)),
            text(format!("Profiles: {}", app.manager.profiles().len()))
                .size(12)
                .color(muted_text()),
            Space::new().width(Length::Fixed(18.0)),
            text(format!(
                "{}x{}",
                app.terminal_size.cols, app.terminal_size.rows
            ))
            .size(12)
            .color(muted_text()),
            Space::new().width(Length::Fixed(18.0)),
            text("Adit Native").size(12).color(muted_text()),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([6, 14])
    .height(STATUS_BAR_HEIGHT)
    .width(Fill)
    .style(|_theme| status_bar_style())
    .into()
}
