use super::*;
use iced::widget::column;

pub(crate) fn view(app: &AditApp) -> Element<'_, Message> {
    DARK_MODE.store(app.dark_mode, Ordering::Relaxed);
    TERM_FONT.store(font_preset_index(&app.font_family), Ordering::Relaxed);
    TERM_FONT_SIZE.store(
        (app.font_size as u32).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE),
        Ordering::Relaxed,
    );
    TERM_SCHEME.store(color_scheme_index(&app.color_scheme), Ordering::Relaxed);

    // The rail is outside the switch on purpose: it is what stays put when a
    // host is opened, so getting back to the list never means closing a session.
    // Fullscreen drops the sidebar too, whatever the persisted setting says:
    // the point of the mode is that the remote desktop owns the glass.
    let main = if app.sidebar_visible && !app.fullscreen {
        row![sidebar(app), sidebar_divider(), workspace(app)]
    } else {
        row![workspace(app)]
    }
    .height(Fill)
    .width(Fill);

    // The status bar survives fullscreen on purpose. It is one thin line, and
    // it is the only thing left telling the user how to get back out.
    // No toolbar row: every one of its buttons is a menu item, and the icon
    // strip cost a permanent 36px of desktop for shortcuts that were mostly
    // duplicates. Session Manager's own header keeps the three that belong to
    // it (host list, new session, hide panel).
    let layout = if app.fullscreen {
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
    if app.terminal_context_menu {
        layers.push(opaque(terminal_context_overlay(app)));
    }
    if app.profile_editor.is_some() {
        layers.push(opaque(profile_editor_overlay(app)));
    }
    if app.connection_dialog.is_some() {
        layers.push(opaque(connection_dialog_overlay(app)));
    }
    if let Some((session_id, prompt)) = app.manager.pending_host_key() {
        layers.push(opaque(host_key_dialog_overlay(session_id, &prompt)));
    }
    if let Some((session_id, prompt)) = &app.auth_prompt {
        layers.push(opaque(auth_prompt_dialog_overlay(
            *session_id,
            prompt,
            &app.auth_prompt_answers,
        )));
    }
    if let Some(url) = &app.pending_hyperlink {
        layers.push(opaque(hyperlink_confirm_overlay(url)));
    }
    if app.manager.sftp_is_open() {
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

    if layers.len() == 1 {
        layers.pop().unwrap()
    } else {
        stack(layers).into()
    }
}

pub(crate) fn menu_bar(app: &AditApp) -> Element<'_, Message> {
    container(
        row![
            text("▣").size(14).color(accent()),
            menu_button(app, MenuKind::File, t("文件")),
            menu_button(app, MenuKind::Session, t("会话")),
            menu_button(app, MenuKind::Edit, t("编辑")),
            menu_button(app, MenuKind::View, t("视图")),
            menu_button(app, MenuKind::Transfer, t("传输")),
            menu_button(app, MenuKind::Script, t("脚本")),
            menu_button(app, MenuKind::Help, t("帮助")),
            Space::new().width(Fill),
            text(app.manager.status_line()).size(11).color(muted_text()),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([3, 10])
    .height(MENU_BAR_HEIGHT)
    .width(Fill)
    .style(|_theme| top_bar_style())
    .into()
}

pub(crate) fn menu_button<'a>(app: &AditApp, kind: MenuKind, label: &'a str) -> Element<'a, Message> {
    let active = app.active_menu == Some(kind);

    button(text(label).size(13))
        .padding([6, 10])
        .style(move |_theme, status| menu_button_style(active, status))
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
            Space::new().width(Length::Fixed(menu_dropdown_offset(menu))),
            container(commands)
                .width(Length::Fixed(182.0))
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

pub(crate) fn menu_commands(menu: MenuKind) -> &'static [(&'static str, MenuCommand)] {
    match menu {
        MenuKind::File => &[
            ("新建会话", MenuCommand::NewProfile),
            ("新建分组", MenuCommand::NewGroup),
            ("保存会话", MenuCommand::SaveProfile),
            ("删除会话", MenuCommand::DeleteProfile),
            ("按名称排序", MenuCommand::SortByName),
            ("按主机排序", MenuCommand::SortByHost),
            ("导入 ~/.ssh/config", MenuCommand::ImportSshConfig),
            ("导入 SecureCRT 会话…", MenuCommand::ImportSecureCrt),
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

pub(crate) fn menu_dropdown_offset(menu: MenuKind) -> f32 {
    match menu {
        MenuKind::File => 28.0,
        MenuKind::Session => 72.0,
        MenuKind::Edit => 138.0,
        MenuKind::View => 184.0,
        MenuKind::Transfer => 236.0,
        MenuKind::Script => 318.0,
        MenuKind::Help => 436.0,
    }
}

pub(crate) fn menu_dropdown_button(label: &'static str, command: MenuCommand) -> Element<'static, Message> {
    button(text(label).size(12))
        .width(Fill)
        .padding([6, 12])
        .style(|_theme, status| menu_command_button_style(status))
        .on_press(Message::RunMenu(command))
        .into()
}



