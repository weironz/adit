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
                protocol_button(app, Protocol::Sftp),
                protocol_button(app, Protocol::Rdp),
                protocol_button(app, Protocol::Telnet),
                protocol_button(app, Protocol::LocalShell),
                protocol_button(app, Protocol::Serial),
            ]
            .spacing(6)
            .wrap()
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
        dialog_field("分组", group_picker(app)),
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
    .spacing(12);

    match app.profile_protocol {
        // SFTP dials over SSH, so it wants the same host, user, port and
        // authentication fields — only what happens after the handshake differs.
        Protocol::Ssh | Protocol::Sftp => {
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
        Protocol::Telnet => {
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
                            text_input("23", &app.profile_port)
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
                // No username, no password, no key: telnet authenticates in-band,
                // so the device's own login prompt is what asks. Offering the SSH
                // credential fields here would collect secrets nothing sends.
                .push(
                    text(t(
                        "Telnet 明文传输，用户名和密码都在终端里按提示输入，不经过凭据库。仅建议用于交换机、IPMI、串口服务器等只支持 telnet 的设备。",
                    ))
                    .size(11)
                    .color(muted_text()),
                );
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
                    // The Microsoft-account note is not trivia: NLA accepts the
                    // email and reports success, then Windows refuses to unlock
                    // with it and shows "用户名或密码不正确" — a connected
                    // session that looks like a wrong password. Only the local
                    // account name works for the logon.
                    text("原生 RDP（NLA/CredSSP）。用户名可用 域\\用户 形式指定域。\
                          微软账户请填本机账户名（远端 whoami 反斜杠后的那段，如 willz），\
                          不要填邮箱——邮箱能通过认证但无法登录桌面。\
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

/// How tall the folder chips may grow before they start scrolling — about three
/// lines on this dialog's width.
const GROUP_PICKER_MAX_HEIGHT: f32 = 104.0;

/// The folder picker: every folder that exists, as chips, plus one that opens a
/// field for a name that doesn't exist yet.
///
/// This was a bare text field, which meant retyping a folder name on every edit
/// — and one wrong character silently invents a folder, because a profile's
/// folder *is* its name string and nothing warns about a near-miss.
///
/// A drop-down is the obvious replacement and the wrong one here: the editor is
/// itself a modal floating on a scrim, and a second overlay stacked inside it is
/// where iced's layering has the least to be trusted with. Chips sit in the
/// dialog's own layout, exactly like the protocol and icon rows above them.
fn group_picker(app: &AditApp) -> Element<'_, Message> {
    // Ungrouped first, then the folders in the order the sidebar tree shows
    // them, so the two surfaces read the same way.
    let mut chips = row![group_chip(app, String::new(), String::from(t("未分组")), None)].spacing(6);
    for group in &app.groups {
        let icon = app
            .group_icons
            .get(group)
            .and_then(|key| HOST_ICONS.iter().find(|icon| icon.key == key));
        chips = chips.push(group_chip(app, group.clone(), group.clone(), icon));
    }
    // The escape hatch. A picker that can only pick would be a downgrade: new
    // folders have to be reachable from the same place sessions are filed.
    let creating = app.profile_group_new;
    chips = chips.push(
        button(
            row![text("\u{ff0b}").size(11), text(t("新建分组")).size(11)]
                .spacing(4)
                .align_y(Alignment::Center),
        )
        .padding([4, 8])
        .style(move |_theme, status| method_button_style(creating, status))
        .on_press(Message::ProfileGroupNewRequested),
    );

    // Five folders fit on one line today; twenty would push the dialog's own
    // buttons off the bottom of the screen. Wrapping alone only trades that for
    // a very tall dialog, so the row gets a ceiling and scrolls past it.
    let mut picker = column![container(scrollable(chips.wrap()))
        .width(Fill)
        .max_height(GROUP_PICKER_MAX_HEIGHT)]
    .spacing(6);

    if creating {
        picker = picker.push(
            text_input(t("新分组名称"), &app.profile_group)
                .id(group_input_id())
                .on_input(Message::ProfileGroupChanged)
                .on_submit(Message::SaveProfile)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
        );
    }

    picker.into()
}

/// One folder in the picker — the same shape and the same selected style as
/// `protocol_button`, because it is the same kind of choice.
fn group_chip(
    app: &AditApp,
    group: String,
    label: String,
    icon: Option<&HostIcon>,
) -> Element<'static, Message> {
    // Nothing existing is selected while a new name is being typed: an empty
    // field would otherwise light up 未分组 and claim a folder had been chosen.
    let selected = !app.profile_group_new && app.profile_group.trim() == group;

    let mut content = row![].spacing(5).align_y(Alignment::Center);
    if let Some(icon) = icon {
        let (r, g, b) = icon.rgb;
        // A selected chip is filled with the accent, where the icon's own colour
        // stops being legible.
        content = content.push(text(icon.glyph).size(12).color(if selected {
            Color::WHITE
        } else {
            Color::from_rgb8(r, g, b)
        }));
    }
    content = content.push(text(label).size(11));

    button(content)
        .padding([4, 8])
        .style(move |_theme, status| method_button_style(selected, status))
        .on_press(Message::ProfileGroupPicked(group))
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
