use super::*;
use iced::widget::column;

pub(crate) fn dialog_field<'a>(label: &'static str, input: Element<'a, Message>) -> Element<'a, Message> {
    column![text(label).size(11).color(muted_text()), input]
        .spacing(3)
        .into()
}

/// The session editor as a centered modal dialog (over a scrim), instead of an
/// inline editor embedded in the sidebar list.
pub(crate) fn profile_editor_overlay(app: &AditApp) -> Element<'_, Message> {
    let status = if form_matches_selected_profile(app) {
        "已保存"
    } else {
        "未保存"
    };

    let header = row![
        text("编辑会话").size(15).color(primary_text()),
        text(status).size(11).color(muted_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseProfileEditor),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut form = column![
        header,
        dialog_field(
            "协议",
            row![
                protocol_button(app, Protocol::Ssh),
                protocol_button(app, Protocol::LocalShell),
                protocol_button(app, Protocol::Serial),
                protocol_button(app, Protocol::Rdp),
            ]
            .spacing(6)
            .into(),
        ),
        dialog_field(
            "图标",
            HOST_ICONS
                .iter()
                .fold(
                    row![icon_button(app, "", "自动", "\u{25cb}", muted_text())].spacing(6),
                    |buttons, icon| {
                        let (r, g, b) = icon.rgb;
                        buttons.push(icon_button(
                            app,
                            icon.key,
                            icon.label,
                            icon.glyph,
                            Color::from_rgb8(r, g, b),
                        ))
                    },
                )
                .wrap()
                .into(),
        ),
        row![
            dialog_field(
                "分组",
                text_input("默认", &app.profile_group)
                    .on_input(Message::ProfileGroupChanged)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ),
            dialog_field(
                "名称",
                text_input("会话名称", &app.profile_name)
                    .on_input(Message::ProfileNameChanged)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ),
        ]
        .spacing(10),
    ]
    .spacing(12);

    match app.profile_protocol {
        Protocol::Ssh => {
            form = form
                .push(
                    row![
                        container(dialog_field(
                            "主机",
                            text_input("10.0.0.5", &app.profile_host)
                                .on_input(Message::ProfileHostChanged)
                                .on_submit(Message::ConnectSelectedProfile)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(2)),
                        container(dialog_field(
                            "端口",
                            text_input("22", &app.profile_port)
                                .on_input(Message::ProfilePortChanged)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(1)),
                    ]
                    .spacing(10),
                )
                .push(dialog_field(
                    "用户名",
                    text_input("root", &app.profile_username)
                        .on_input(Message::ProfileUsernameChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "认证方式",
                    row![
                        auth_method_button(app, AuthMethod::Password),
                        auth_method_button(app, AuthMethod::Auto),
                        auth_method_button(app, AuthMethod::Key),
                        auth_method_button(app, AuthMethod::Agent),
                    ]
                    .spacing(6)
                    .into(),
                ));
            // Password auth: a masked field, saved (encrypted) to credentials.json
            // in the config dir on Save — never written to profiles.json.
            if app.profile_auth_method == AuthMethod::Password {
                form = form.push(dialog_field(
                    "密码（加密保存，可随配置目录同步）",
                    text_input("连接密码", &app.profile_password)
                        .secure(true)
                        .on_input(Message::ProfilePasswordChanged)
                        .on_submit(Message::ConnectSelectedProfile)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ));
            }
            form = form.push(dialog_field(
                "密钥文件（可选，支持 OpenSSH 与 PuTTY .ppk）",
                row![
                    text_input("~/.ssh/id_ed25519", &app.profile_identity_file)
                        .on_input(Message::ProfileIdentityFileChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill),
                    button(text("浏览…").size(12))
                        .padding([5, 12])
                        .style(|_theme, status| secondary_button_style(status))
                        .on_press(Message::PickIdentityFile),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
            ));
            // Key passphrase (masked, encrypted in the credential store, distinct
            // from the login password). Relevant only to key-bearing auth methods.
            if matches!(app.profile_auth_method, AuthMethod::Key | AuthMethod::Auto) {
                form = form.push(dialog_field(
                    "密钥 passphrase（可选，加密保存；私钥加密时填写）",
                    text_input("私钥 passphrase", &app.profile_passphrase)
                        .secure(true)
                        .on_input(Message::ProfilePassphraseChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ));
            }
            form = form
                .push(dialog_field(
                    "跳板机 ProxyJump（可选，user@bastion:22，多个用逗号按顺序；各跳板机复用本会话的密码/密钥）",
                    text_input("jump@bastion.example.com:22", &app.profile_jumps)
                        .on_input(Message::ProfileJumpsChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "启动命令（可选，连接后自动执行，如 tmux attach）",
                    text_input("tmux new -A -s main", &app.profile_startup_command)
                        .on_input(Message::ProfileStartupCommandChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "终端类型 TERM（可选，默认 xterm-256color）",
                    text_input("xterm-256color", &app.profile_terminal_type)
                        .on_input(Message::ProfileTerminalTypeChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "环境色标（标签页配色，避免连错服务器）",
                    row![
                        environment_button(app, Environment::None),
                        environment_button(app, Environment::Development),
                        environment_button(app, Environment::Staging),
                        environment_button(app, Environment::Production),
                        environment_button(app, Environment::Custom),
                    ]
                    .spacing(6)
                    .into(),
                ));
            if app.profile_environment == Environment::Custom {
                form = form.push(dialog_field(
                    "自定义颜色（#RRGGBB）",
                    text_input("#3f7fd1", &app.profile_accent_color)
                        .on_input(Message::ProfileAccentColorChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ));
            }
            form = form.push(dialog_field(
                "标签徽标（可选，如 PROD；留空用环境名）",
                text_input("PROD", &app.profile_label)
                    .on_input(Message::ProfileLabelChanged)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ));
        }
        Protocol::LocalShell => {
            form = form.push(dialog_field(
                "Shell 程序（可选，留空用系统默认）",
                text_input("powershell.exe / cmd.exe / bash", &app.profile_identity_file)
                    .on_input(Message::ProfileIdentityFileChanged)
                    .on_submit(Message::ConnectSelectedProfile)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ));
        }
        Protocol::Serial => {
            form = form
                .push(dialog_field(
                    "串口号",
                    text_input("COM3", &app.profile_host)
                        .on_input(Message::ProfileHostChanged)
                        .on_submit(Message::ConnectSelectedProfile)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "波特率（8N1，无流控）",
                    text_input("115200", &app.profile_identity_file)
                        .on_input(Message::ProfileIdentityFileChanged)
                        .on_submit(Message::ConnectSelectedProfile)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ));
        }
        Protocol::Rdp => {
            form = form
                .push(
                    row![
                        container(dialog_field(
                            "主机",
                            text_input("10.0.0.5", &app.profile_host)
                                .on_input(Message::ProfileHostChanged)
                                .on_submit(Message::ConnectSelectedProfile)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(2)),
                        container(dialog_field(
                            "端口",
                            text_input("3389", &app.profile_port)
                                .on_input(Message::ProfilePortChanged)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(1)),
                    ]
                    .spacing(10),
                )
                .push(dialog_field(
                    "用户名",
                    text_input("Administrator", &app.profile_username)
                        .on_input(Message::ProfileUsernameChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(
                    text("原生 RDP（NLA/CredSSP）。用户名可用 域\\用户 形式指定域；\
                          连接时在弹出的密码框输入密码（可勾选记住，存入系统凭据）。")
                        .size(11)
                        .color(muted_text()),
                );
        }
    }

    form = form.push(
        row![
            Space::new().width(Fill),
            button(text("取消").size(12))
                .padding([6, 16])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CloseProfileEditor),
            button(text("连接").size(12))
                .padding([6, 16])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::ConnectSelectedProfile),
            button(text("保存").size(12))
                .padding([6, 18])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SaveProfile),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );

    let card = container(form)
        .width(Length::Fixed(440.0))
        .padding(20)
        .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

pub(crate) fn auth_method_button(app: &AditApp, auth_method: AuthMethod) -> Element<'static, Message> {
    let selected = app.profile_auth_method == auth_method;

    button(text(auth_method.label()).size(11))
        .padding([4, 6])
        .style(move |_theme, status| method_button_style(selected, status))
        .on_press(Message::ProfileAuthMethodChanged(auth_method))
        .into()
}

pub(crate) fn environment_button(app: &AditApp, environment: Environment) -> Element<'static, Message> {
    let selected = app.profile_environment == environment;
    // Tint the selected chip with the environment's own colour so the picker is
    // itself a legend (green=dev, amber=staging, red=prod).
    let env_accent = environment.preset_hex().and_then(parse_hex_color);
    button(text(environment.label()).size(11))
        .padding([4, 8])
        .style(move |_theme, status| {
            if selected {
                let fill = env_accent.unwrap_or_else(accent);
                base_button_style(fill, Color::from_rgb8(245, 249, 255), transparent())
            } else {
                method_button_style(false, status)
            }
        })
        .on_press(Message::ProfileEnvironmentChanged(environment))
        .into()
}

/// One choice in the icon picker: the glyph in its own colour, so the row shows
/// what each will look like rather than naming it.
pub(crate) fn icon_button(
    app: &AditApp,
    key: &'static str,
    label: &'static str,
    glyph: &'static str,
    color: Color,
) -> Element<'static, Message> {
    let selected = app.profile_icon == key;
    button(
        row![text(glyph).size(12).color(color), text(label).size(11)]
            .spacing(5)
            .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .style(move |_theme, status| method_button_style(selected, status))
    .on_press(Message::ProfileIconChanged(key))
    .into()
}

pub(crate) fn protocol_button(app: &AditApp, protocol: Protocol) -> Element<'static, Message> {
    let selected = app.profile_protocol == protocol;

    button(text(protocol.label()).size(11))
        .padding([4, 10])
        .style(move |_theme, status| method_button_style(selected, status))
        .on_press(Message::ProfileProtocolChanged(protocol))
        .into()
}

/// The pinned first tab. The host list is a tab rather than a view behind its
/// own rail: tabs already exist, everyone knows what they do, and a second
/// navigation surface for one more destination was never worth it.
pub(crate) fn hosts_tab_button(active: bool) -> Element<'static, Message> {
    button(
        row![text("\u{25a6}").size(12), text("主机").size(12)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .height(TAB_BAR_HEIGHT)
    .padding([0, 12])
    .style(move |_theme, status| {
        if active {
            primary_button_style(status)
        } else {
            secondary_button_style(status)
        }
    })
    .on_press(Message::ShowMainView(MainView::Hosts))
    .into()
}
