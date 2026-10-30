use super::*;
use iced::widget::column;

pub(crate) fn view(app: &AditApp) -> Element<'_, Message> {
    view_for(app, app.accessibility.primary)
}

/// Detached-window callers supply their own ID; no HWND or semantic action is
/// inferred from the foreground window or whichever session happens to be active.
pub(crate) fn view_for(app: &AditApp, window: Option<window::Id>) -> Element<'_, Message> {
    let a11y_namespace = accessibility::namespace(app);
    let _building = accessibility::building(window, &a11y_namespace);
    DARK_MODE.store(app.dark_mode, Ordering::Relaxed);

    // The rail is outside the switch on purpose: it is what stays put when a
    // host is opened, so getting back to the list never means closing a session.
    // Fullscreen drops the sidebar too, whatever the persisted setting says:
    // the point of the mode is that the remote desktop owns the glass.
    let main = if app.fullscreen {
        row![workspace(app)]
    } else if app.sidebar_visible {
        row![sidebar(app), sidebar_divider(), workspace(app)]
    } else {
        // Hidden, not gone: the strip is the only thing on screen that can bring
        // the panel back. Its width is part of `sidebar_offset`, which is what
        // the terminal's hit-testing measures from.
        row![sidebar_reveal_strip(), workspace(app)]
    }
    .height(Fill)
    .width(Fill);

    // The status bar survives fullscreen on purpose. It is one thin line, and
    // it is the only thing left telling the user how to get back out.
    // No toolbar row: every one of its buttons is a menu item, and the icon
    // strip cost a permanent 36px of desktop for shortcuts that were mostly
    // duplicates. Session Manager's own header keeps the three that belong to
    // it (host list, new session, hide panel).
    // The status bar survives fullscreen for every other session type because
    // it is the only thing left naming the way out. Over a remote desktop the
    // floating toolbar is that thing, so the bar goes as well and the glass is
    // wholly the desktop's.
    let rdp_fullscreen = app.fullscreen && app.manager.active_is_desktop();
    let layout = if rdp_fullscreen {
        column![main]
    } else if app.fullscreen {
        column![main].push(status_bar(app))
    } else {
        column![menu_bar(app)].push(main).push(status_bar(app))
    }
    .height(Fill)
    .width(Fill);

    let base: Element<'_, Message> = container(layout)
        .style(|_theme| app_background_style())
        .height(Fill)
        .width(Fill)
        .into();

    // The status bar is hidden over a fullscreen desktop. Keep its errors and
    // file-safety notice visible without changing the desktop's layout or input.
    let safety = app
        .manager
        .active_session()
        .and_then(|session| app.manager.rdp_local_file_safety_notice(session));
    let policy = app
        .manager
        .active_session()
        .filter(|_| native_windows::owns_selected_session(app))
        .and_then(clipboard_files::local_file_send_disabled_notice);
    let clipboard = rdp_clipboard_policy::for_selected_session(app);
    let clipboard_text = clipboard.map(|status| status.inline());
    let warning = safety.is_some()
        || policy.is_some()
        || app.last_error.is_some()
        || clipboard.is_some_and(|status| status.is_failed());
    let base: Element<'_, Message> = match desktop_overlay_notice(
        rdp_fullscreen,
        app.last_error.as_deref(),
        safety,
        policy,
        clipboard_text.as_deref(),
    ) {
        Some(notice) => fullscreen_notice_overlay(base, notice, warning),
        None => base,
    };

    // Menus and the connection dialog float above the chrome instead of
    // reserving layout space, so opening one never shifts the content.
    let mut layers: Vec<Element<'_, Message>> = vec![base];
    if let Some(menu) = app.active_menu {
        layers.push(menu_overlay(menu));
    }
    if let Some(profile_id) = app.profile_context_menu {
        layers.push(opaque(profile_context_overlay(app, profile_id)));
    }
    if let Some(session_id) = app.tab_context_menu {
        layers.push(opaque(tab_context_overlay(app, session_id)));
    }
    if let Some(group) = app.group_context_menu.clone() {
        let collapsed = app.collapsed_groups.contains(&group);
        layers.push(opaque(group_context_overlay(app, group, collapsed)));
    }
    if app.move_to_group_menu && !app.selected_profiles.is_empty() {
        layers.push(opaque(move_to_group_overlay(app)));
    }
    if app.terminal_context_menu {
        layers.push(opaque(terminal_context_overlay(app)));
    }
    if app.profile_editor.is_some() {
        layers.push(opaque(profile_editor_overlay(app)));
    }
    if app.connection_dialog.is_some() && native_windows::dialog_visible(app) {
        layers.push(opaque(connection_dialog_overlay(app)));
    }
    if let Some((session_id, prompt)) = app
        .manager
        .pending_host_key()
        .filter(|(sid, _)| native_windows::prompt_visible(app, *sid))
    {
        layers.push(opaque(host_key_dialog_overlay(session_id, &prompt)));
    }
    if let Some((session_id, prompt)) = app
        .auth_prompt
        .as_ref()
        .filter(|(sid, _)| native_windows::prompt_visible(app, *sid))
    {
        layers.push(opaque(auth_prompt_dialog_overlay(
            *session_id,
            prompt,
            &app.auth_prompt_answers,
        )));
    }
    if let Some(url) = &app.pending_hyperlink {
        layers.push(opaque(hyperlink_confirm_overlay(url)));
    }
    if app.manager.sftp_is_open() && native_windows::sftp_visible(app) {
        layers.push(opaque(sftp_panel_overlay(app)));
        if let Some((pane, name, is_dir)) = app.sftp_context_menu.clone() {
            layers.push(opaque(sftp_context_overlay(app, pane, name, is_dir)));
        }
    }
    if app.tunnels_open {
        layers.push(opaque(tunnels_panel_overlay(app)));
    }
    if app.about_open {
        layers.push(opaque(about_dialog_overlay()));
    }
    if app.update_dialog_open {
        layers.push(opaque(update_dialog_overlay(app)));
    }
    if app.paste_confirm_open {
        layers.push(opaque(paste_confirm_overlay(app)));
    }
    if app.renaming_session.is_some() {
        layers.push(opaque(session_rename_overlay(app)));
    }
    if app.snippets_open {
        layers.push(opaque(snippets_panel_overlay(app)));
    }
    if app.settings_open {
        layers.push(opaque(settings_dialog_overlay(app)));
    }
    if app.known_hosts_open {
        layers.push(opaque(known_hosts_overlay(app)));
    }

    let another_window_modal = app.native_window_id.is_some()
        && (app
            .manager
            .pending_host_key()
            .is_some_and(|(sid, _)| !native_windows::prompt_visible(app, sid))
            || app
                .auth_prompt
                .as_ref()
                .is_some_and(|(sid, _)| !native_windows::prompt_visible(app, *sid))
            || (app.connection_dialog.is_some() && !native_windows::dialog_visible(app))
            || (app.manager.sftp_is_open() && !native_windows::sftp_visible(app)));
    if another_window_modal {
        layers.push(opaque(
            container(text(t("请在另一窗口完成验证或文件操作")))
                .padding(12)
                .center_x(Fill)
                .center_y(Fill)
                .style(|_| dialog_scrim_style()),
        ));
    }
    if layers.len() > 1 {
        let top = layers.pop().unwrap();
        layers.push(accessibility::modal(top));
    }
    let content = if layers.len() == 1 {
        layers.pop().unwrap()
    } else {
        stack(layers).into()
    };
    accessibility::scope(window, a11y_namespace, content)
}

fn desktop_overlay_notice(
    rdp_fullscreen: bool,
    error: Option<&str>,
    safety: Option<&str>,
    policy: Option<&str>,
    clipboard: Option<&str>,
) -> Option<String> {
    if !rdp_fullscreen && policy.is_none() {
        return None;
    }
    let mut messages = Vec::new();
    for message in [clipboard.filter(|_| rdp_fullscreen), safety, policy, error]
        .into_iter()
        .flatten()
    {
        if !message.is_empty() {
            let message = runtime(message);
            if !messages.contains(&message) {
                messages.push(message);
            }
        }
    }
    (!messages.is_empty()).then(|| messages.join(" | "))
}

fn fullscreen_notice_overlay<'a, Renderer>(
    base: iced::Element<'a, Message, Theme, Renderer>,
    notice: String,
    warning: bool,
) -> iced::Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::text::Renderer + 'a,
{
    let banner = container(text(notice).size(12).color(primary_text()))
        .padding([8, 14])
        .width(Fill)
        .style(move |_| container::Style {
            background: Some(
                if warning {
                    danger_background()
                } else {
                    surface_alt()
                }
                .into(),
            ),
            ..container::Style::default()
        });
    // Text and containers do not capture events or claim pointer interaction;
    // the stack lays out the desktop as its base layer at its original size.
    iced::widget::stack![
        base,
        column![Space::new().height(Fill), banner]
            .width(Fill)
            .height(Fill)
    ]
    .into()
}

pub(crate) fn menu_bar(app: &AditApp) -> Element<'_, Message> {
    container(
        row![
            global_view_button(app, MainView::Hosts, "资源库"),
            global_view_button(app, MainView::Terminal, "专注工作区"),
            global_view_button(app, MainView::Missions, "任务分屏"),
            menu_button(app, MenuKind::File, t("菜单")),
            Space::new().width(Fill),
            text(tf("{} 个已打开会话", &[&app.manager.sessions().len()]))
                .size(TEXT_META)
                .color(Color::from_rgb8(158, 171, 190)),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([0, 12])
    .center_y(MENU_BAR_HEIGHT)
    .width(Fill)
    .style(|_theme| top_bar_style())
    .into()
}

pub(crate) fn global_view_button(
    app: &AditApp,
    view: MainView,
    label: &'static str,
) -> Element<'static, Message> {
    let active = app.main_view == view;
    let icon = match view {
        MainView::Hosts => UiIcon::Hosts,
        MainView::Terminal => UiIcon::Terminal,
        MainView::Missions => UiIcon::Tasks,
    };
    let control: Element<'static, Message> = button(
        row![
            ui_icon(icon, 14.0, Color::from_rgb8(202, 214, 229)),
            text(t_in("navigation", label)).size(12),
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    )
    .padding([5, 10])
    .style(move |_, status| global_navigation_style(active, status))
    .on_press(Message::ShowMainView(view))
    .into();
    accessibility::control(
        control,
        accessibility::Control::button(
            format!("navigation:{view:?}"),
            t_in("navigation", label),
            Message::ShowMainView(view),
        ),
    )
}

pub(crate) fn menu_button<'a>(
    app: &AditApp,
    kind: MenuKind,
    label: &'a str,
) -> Element<'a, Message> {
    let active = app.active_menu == Some(kind);

    button(text(t_in("navigation", label)).size(12))
        .padding([5, 10])
        .style(move |_theme, status| global_navigation_style(active, status))
        .on_press(Message::ToggleMenu(kind))
        .into()
}

pub(crate) fn menu_overlay(menu: MenuKind) -> Element<'static, Message> {
    let mut commands = column![].spacing(0).width(Fill);
    for (label, command) in menu_commands(menu) {
        let label = t(label);
        commands = commands.push(menu_dropdown_button(label, *command));
    }

    // The dropdown card, positioned under its menu-bar button.
    let positioned = column![
        Space::new().height(Length::Fixed(MENU_BAR_HEIGHT)),
        row![
            container(row![
                column![
                    menu_category_button(menu, MenuKind::File, "文件"),
                    menu_category_button(menu, MenuKind::Session, "会话"),
                    menu_category_button(menu, MenuKind::Edit, "编辑"),
                    menu_category_button(menu, MenuKind::View, "视图"),
                    menu_category_button(menu, MenuKind::Transfer, "传输"),
                    menu_category_button(menu, MenuKind::Script, "脚本"),
                    menu_category_button(menu, MenuKind::Help, "帮助"),
                ]
                .width(96),
                scrollable(commands).height(Length::Shrink).width(210),
            ])
            .width(Length::Fixed(306.0))
            .padding([5, 0])
            .style(|_theme| menu_dropdown_style()),
            Space::new().width(Fill),
        ],
        Space::new().height(Fill),
    ]
    .width(Fill)
    .height(Fill);

    // A click-catcher below the menu bar that dismisses the menu. It starts
    // below the bar so the other menu buttons stay clickable underneath.
    let backdrop = column![
        Space::new().height(Length::Fixed(MENU_BAR_HEIGHT)),
        mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleMenu(menu)),
    ]
    .width(Fill)
    .height(Fill);

    stack(vec![backdrop.into(), positioned.into()]).into()
}

fn menu_category_button(
    current: MenuKind,
    kind: MenuKind,
    label: &'static str,
) -> Element<'static, Message> {
    button(text(t(label)).size(TEXT_BODY))
        .padding([6, 10])
        .width(Fill)
        .style(move |_, status| menu_button_style(current == kind, status))
        .on_press(Message::ToggleMenu(kind))
        .into()
}
pub(crate) fn menu_commands(menu: MenuKind) -> &'static [(&'static str, MenuCommand)] {
    match menu {
        MenuKind::File => &[
            ("新建会话", MenuCommand::NewProfile),
            ("新建分组", MenuCommand::NewGroup),
            ("保存会话", MenuCommand::SaveProfile),
            ("删除会话", MenuCommand::DeleteProfile),
            ("按名称排序", MenuCommand::SortByName),
            ("按主机排序", MenuCommand::SortByHost),
            ("导入 SSH config…", MenuCommand::ImportSshConfig),
            ("导入 SecureCRT 会话…", MenuCommand::ImportSecureCrt),
            ("导入 PuTTY 会话（.reg）…", MenuCommand::ImportPutty),
            #[cfg(windows)]
            ("导入 PuTTY 注册表会话", MenuCommand::ImportPuttyRegistry),
            ("导入会话目录（JSON）…", MenuCommand::ImportCatalog),
            ("导出会话目录（不含秘密）…", MenuCommand::ExportCatalog),
            ("设置…", MenuCommand::Options),
            ("关闭标签", MenuCommand::CloseActiveTab),
        ],
        MenuKind::Session => &[
            ("连接", MenuCommand::Connect),
            ("断开", MenuCommand::Disconnect),
            ("自动重连开关", MenuCommand::ToggleAutoReconnect),
            ("主机密钥…", MenuCommand::KnownHosts),
            ("打开演示标签", MenuCommand::OpenMockTab),
            ("关闭标签", MenuCommand::CloseActiveTab),
        ],
        MenuKind::Edit => &[("清屏", MenuCommand::ClearTerminal)],
        MenuKind::View => &[
            // First, and first for a reason: fullscreen was reachable only by
            // Ctrl+Alt+Enter, which is undiscoverable — nothing on screen named
            // it. The floating tab over a remote desktop is the other half of
            // the same fix.
            ("全屏", MenuCommand::ToggleFullscreen),
            ("侧边栏开关", MenuCommand::ToggleSidebar),
            ("深色模式开关", MenuCommand::ToggleTheme),
            ("分屏（添加窗格）", MenuCommand::SplitPane),
            ("垂直平铺（并排）", MenuCommand::TileVertical),
            ("水平平铺（上下）", MenuCommand::TileHorizontal),
            ("网格平铺", MenuCommand::TileGrid),
            ("合并为标签", MenuCommand::Untile),
            ("命令窗口", MenuCommand::ToggleCommandWindow),
            ("输入广播开关", MenuCommand::ToggleBroadcast),
            ("终端 96x28", MenuCommand::ResizeDefault),
            ("终端 120x36", MenuCommand::ResizeWide),
        ],
        MenuKind::Transfer => &[
            ("SFTP", MenuCommand::Sftp),
            ("端口转发", MenuCommand::Tunnels),
        ],
        MenuKind::Script => &[
            ("命令片段", MenuCommand::Snippets),
            ("记录日志开关", MenuCommand::Logging),
        ],
        MenuKind::Help => &[
            ("检查更新…", MenuCommand::CheckUpdate),
            ("关于", MenuCommand::About),
        ],
    }
}

pub(crate) fn menu_dropdown_button(
    label: &'static str,
    command: MenuCommand,
) -> Element<'static, Message> {
    button(text(t_in("navigation", label)).size(12))
        .width(Fill)
        .padding([6, 12])
        .style(|_theme, status| menu_command_button_style(status))
        .on_press(Message::RunMenu(command))
        .into()
}

#[cfg(test)]
mod fullscreen_notice_tests {
    use super::*;
    use iced::advanced::{
        clipboard, layout, renderer, widget::Tree, Clipboard, Layout, Shell, Widget,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    type Observations = Rc<RefCell<Vec<(iced::Event, iced::Rectangle, Option<Point>)>>>;

    struct DesktopProbe(Observations);

    impl Widget<Message, Theme, ()> for DesktopProbe {
        fn size(&self) -> iced::Size<Length> {
            iced::Size::new(Fill, Fill)
        }

        fn layout(&mut self, _: &mut Tree, _: &(), limits: &layout::Limits) -> layout::Node {
            layout::Node::new(limits.resolve(Fill, Fill, iced::Size::ZERO))
        }

        fn draw(
            &self,
            _: &Tree,
            _: &mut (),
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &iced::Rectangle,
        ) {
        }

        fn update(
            &mut self,
            _: &mut Tree,
            event: &iced::Event,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            _: &(),
            _: &mut dyn Clipboard,
            _: &mut Shell<'_, Message>,
            _: &iced::Rectangle,
        ) {
            self.0
                .borrow_mut()
                .push((event.clone(), layout.bounds(), cursor.position()));
        }
    }

    fn observe_desktop(banner: Option<bool>) -> Vec<(iced::Event, iced::Rectangle, Option<Point>)> {
        let observations = Observations::default();
        let mut view: iced::Element<'_, Message, Theme, ()> =
            iced::Element::new(DesktopProbe(observations.clone()));
        if let Some(warning) = banner {
            view = fullscreen_notice_overlay(view, "synthetic clipboard status".into(), warning);
        }
        let mut tree = Tree::new(view.as_widget());
        let size = iced::Size::new(960.0, 640.0);
        let node = view
            .as_widget_mut()
            .layout(&mut tree, &(), &layout::Limits::new(size, size));
        let point = Point::new(40.0, 635.0);
        if banner.is_some() {
            let overlay = Layout::new(&node).children().nth(1).unwrap();
            let banner = overlay.children().nth(1).unwrap();
            assert!(
                banner.bounds().contains(point),
                "exercise input over the banner itself"
            );
        }
        let viewport = iced::Rectangle {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        };
        let events = [
            iced::Event::Mouse(mouse::Event::CursorMoved { position: point }),
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("x".into()),
                modified_key: keyboard::Key::Character("x".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::default(),
                text: Some("x".into()),
                repeat: false,
            }),
        ];
        for event in events {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            view.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &(),
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert!(!shell.is_event_captured());
        }
        let result = observations.borrow().clone();
        result
    }

    #[test]
    fn fullscreen_error_and_safety_are_visible_together() {
        assert_eq!(
            desktop_overlay_notice(false, Some("error"), Some("safety"), None, None),
            None
        );
        assert_eq!(desktop_overlay_notice(true, None, None, None, None), None);
        assert_eq!(
            desktop_overlay_notice(true, Some("error"), None, None, None).as_deref(),
            Some("error")
        );
        assert_eq!(
            desktop_overlay_notice(true, Some("error"), Some("safety"), None, None).as_deref(),
            Some("safety | error")
        );
        assert_eq!(
            desktop_overlay_notice(false, None, Some("disabled"), Some("disabled"), None)
                .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            desktop_overlay_notice(false, None, None, Some("disabled"), None).as_deref(),
            Some("disabled")
        );
        assert_eq!(
            desktop_overlay_notice(true, Some("error"), Some("safety"), None, Some("pending"))
                .as_deref(),
            Some("pending | safety | error")
        );
        assert_eq!(
            desktop_overlay_notice(false, None, None, None, Some("pending")),
            None
        );
    }

    #[test]
    fn fullscreen_banner_does_not_move_desktop_or_capture_input() {
        let normal = observe_desktop(None);
        let with_banner = observe_desktop(Some(true));
        assert_eq!(normal.len(), 4);
        assert_eq!(with_banner, normal);
        assert_eq!(observe_desktop(Some(false)), normal);
        assert_eq!(
            normal[0].1,
            iced::Rectangle {
                x: 0.0,
                y: 0.0,
                width: 960.0,
                height: 640.0
            }
        );
        assert_eq!(normal[0].2, Some(Point::new(40.0, 635.0)));
    }
}
