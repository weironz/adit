use super::*;
use iced::widget::column;

/// How wide the editor card is.
///
/// At 440 every field was full-width by default, so the form could only grow
/// downwards — host, user and password ended up at the top of a column tall
/// enough to touch both edges of the screen, with ProxyJump and TERM trailing
/// below them. Width is what lets related fields share a line instead.
const EDITOR_WIDTH: f32 = 660.0;

/// How tall the scrolling part of the form may grow, given the window it has to
/// fit inside.
///
/// Measured from the window rather than fixed, because a fixed number is wrong
/// at both ends: small enough for a laptop and the common fields scroll on a
/// 27-inch monitor that had room for all of them; large enough for the monitor
/// and 保存 goes off the bottom of the laptop. The subtracted margin is the
/// card's own chrome (padding, title, action row) plus a little breathing room
/// around it on the scrim.
fn editor_body_max_height(app: &AditApp) -> f32 {
    (app.window_height - 220.0).clamp(240.0, 720.0)
}

pub(crate) fn dialog_field<'a>(label: &'static str, input: Element<'a, Message>) -> Element<'a, Message> {
    column![text(label).size(11).color(muted_text()), input]
        .spacing(3)
        .into()
}

/// The shape every plain field in this dialog takes.
///
/// Written out at each call site it was twelve lines per field, which is why
/// the form stayed one field per row: moving two of them onto a shared line
/// meant moving two dozen lines of builder calls.
fn text_field<'a>(
    label: &'static str,
    placeholder: &'static str,
    value: &'a str,
    on_input: fn(String) -> Message,
) -> Element<'a, Message> {
    dialog_field(
        label,
        text_input(placeholder, value)
            .on_input(on_input)
            .padding([5, 8])
            .style(text_input_style)
            .width(Fill)
            .into(),
    )
}

/// Same, for the fields where Enter means "connect now" — the ones that name
/// what is being connected to, or the secret that gets it in.
fn dial_field<'a>(
    label: &'static str,
    placeholder: &'static str,
    value: &'a str,
    on_input: fn(String) -> Message,
) -> Element<'a, Message> {
    dialog_field(
        label,
        text_input(placeholder, value)
            .on_input(on_input)
            .on_submit(Message::ConnectSelectedProfile)
            .padding([5, 8])
            .style(text_input_style)
            .width(Fill)
            .into(),
    )
}

/// A cell in a multi-field row, sized by share of the line.
fn cell(field: Element<'_, Message>, portion: u16) -> Element<'_, Message> {
    container(field).width(Length::FillPortion(portion)).into()
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
        text_field("名称", "会话名称", &app.profile_name, Message::ProfileNameChanged),
    ]
    .spacing(12);

    match app.profile_protocol {
        // SFTP dials over SSH, so it wants the same host, user, port and
        // authentication fields — only what happens after the handshake differs.
        Protocol::Ssh | Protocol::Sftp => {
            // The three that name the machine, on one line: they are read and
            // typed together, and each is short enough that a full-width field
            // was mostly empty space.
            form = form.push(
                row![
                    cell(
                        dial_field("主机", "10.0.0.5", &app.profile_host, Message::ProfileHostChanged),
                        3,
                    ),
                    cell(text_field("端口", "22", &app.profile_port, Message::ProfilePortChanged), 1),
                    cell(
                        text_field("用户名", "root", &app.profile_username, Message::ProfileUsernameChanged),
                        3,
                    ),
                ]
                .spacing(10),
            );

            // The method and the secret it needs share a line: exactly one of
            // password / passphrase can ever apply, so the column below the
            // buttons was always one live field and one gap.
            //
            // Password auth: a masked field, saved (encrypted) to credentials.json
            // in the config dir on Save — never written to profiles.json. The key
            // passphrase is a different secret in the same store, and only means
            // anything to the key-bearing methods.
            let secret: Element<'_, Message> = match app.profile_auth_method {
                AuthMethod::Password => dialog_field(
                    "密码（加密保存，可随配置目录同步）",
                    text_input("连接密码", &app.profile_password)
                        .secure(true)
                        .on_input(Message::ProfilePasswordChanged)
                        .on_submit(Message::ConnectSelectedProfile)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ),
                AuthMethod::Key | AuthMethod::Auto => dialog_field(
                    "密钥 passphrase（可选，加密保存；私钥加密时填写）",
                    text_input("私钥 passphrase", &app.profile_passphrase)
                        .secure(true)
                        .on_input(Message::ProfilePassphraseChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ),
                // ssh-agent holds the secret; there is nothing to ask for.
                AuthMethod::Agent => Space::new().into(),
            };
            form = form.push(
                row![
                    cell(
                        dialog_field(
                            "认证方式",
                            row![
                                auth_method_button(app, AuthMethod::Password),
                                auth_method_button(app, AuthMethod::Auto),
                                auth_method_button(app, AuthMethod::Key),
                                auth_method_button(app, AuthMethod::Agent),
                            ]
                            .spacing(6)
                            .wrap()
                            .into(),
                        ),
                        1,
                    ),
                    cell(secret, 1),
                ]
                .spacing(10),
            );

            form = form
                .push(dialog_field(
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
                ))
                .push(advanced_section(app));
        }
        Protocol::LocalShell => {
            form = form.push(dial_field(
                "Shell 程序（可选，留空用系统默认）",
                "powershell.exe / cmd.exe / bash",
                &app.profile_identity_file,
                Message::ProfileIdentityFileChanged,
            ));
        }
        Protocol::Serial => {
            form = form.push(
                row![
                    cell(
                        dial_field("串口号", "COM3", &app.profile_host, Message::ProfileHostChanged),
                        1,
                    ),
                    cell(
                        dial_field(
                            "波特率（8N1，无流控）",
                            "115200",
                            &app.profile_identity_file,
                            Message::ProfileIdentityFileChanged,
                        ),
                        1,
                    ),
                ]
                .spacing(10),
            );
        }
        Protocol::Telnet => {
            form = form
                .push(
                    row![
                        cell(
                            dial_field("主机", "10.0.0.5", &app.profile_host, Message::ProfileHostChanged),
                            3,
                        ),
                        cell(text_field("端口", "23", &app.profile_port, Message::ProfilePortChanged), 1),
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
                        cell(
                            dial_field("主机", "10.0.0.5", &app.profile_host, Message::ProfileHostChanged),
                            3,
                        ),
                        cell(text_field("端口", "3389", &app.profile_port, Message::ProfilePortChanged), 1),
                        cell(
                            text_field(
                                "用户名",
                                "Administrator",
                                &app.profile_username,
                                Message::ProfileUsernameChanged,
                            ),
                            3,
                        ),
                    ]
                    .spacing(10),
                )
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

    let actions = row![
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
    .align_y(Alignment::Center);

    // Title and actions stay put; only the fields between them scroll. The old
    // dialog grew until it ran off both edges of the screen, which put 保存
    // somewhere the mouse could not reach.
    let card = container(
        column![
            header,
            container(scrollable(container(form).padding([0, 10])).width(Fill))
                .max_height(editor_body_max_height(app)),
            actions,
        ]
        .spacing(12),
    )
    .width(Length::Fixed(EDITOR_WIDTH))
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

/// How many folders the picker offers at once.
///
/// Past a dozen the chips stop being a list you scan and become a wall you
/// hunt through, and hunting is slower than typing the name — which is what
/// narrows this list. The cap is what keeps 50 folders the same interaction as
/// 5 rather than a taller dialog.
const GROUP_SUGGESTION_LIMIT: usize = 12;

/// The folder picker: one field whose text *is* the folder, over the existing
/// folders it still matches.
///
/// It was a bare text field once, which meant retyping a folder name on every
/// edit — and one wrong character silently invents a folder, because a
/// profile's folder *is* its name string and nothing warns about a near-miss.
/// Chips fixed that and brought their own problem: a scrolling colour wall once
/// there were twenty of them, plus a separate 新建分组 mode, so filing a session
/// was two different interactions depending on whether the folder existed yet.
///
/// Typing is the one interaction that scales. The field filters the chips as it
/// goes, and a name matching nothing is simply the new folder — no mode, no
/// second button. A drop-down would be the conventional shape and the wrong one
/// here: this editor is itself a modal floating on a scrim, and a second
/// overlay stacked inside it is where iced's layering has the least to be
/// trusted with.
fn group_picker(app: &AditApp) -> Element<'_, Message> {
    let typed = app.profile_group.trim();
    // A name that exactly matches a folder is a finished choice, not a
    // half-typed filter. Narrowing on it would hide every other folder the
    // instant one was picked, leaving no way back to them but clearing the
    // field.
    let settled = typed.is_empty() || app.groups.iter().any(|group| group == typed);
    let needle = if settled { String::new() } else { typed.to_lowercase() };

    let mut chips = row![].spacing(6);
    let mut shown = 0usize;
    let mut hidden = 0usize;
    // Ungrouped first (the empty string = top level), then the folders in the
    // order the sidebar tree shows them, so the two surfaces read the same way.
    // It only appears while nothing is being filtered on, because "no folder"
    // is what an empty field already means.
    if needle.is_empty() {
        chips = chips.push(group_chip(app, String::new(), String::from(t("未分组")), None));
        shown += 1;
    }
    for group in &app.groups {
        if !needle.is_empty() && !group.to_lowercase().contains(&needle) {
            continue;
        }
        if shown >= GROUP_SUGGESTION_LIMIT {
            hidden += 1;
            continue;
        }
        let icon = app
            .group_icons
            .get(group)
            .and_then(|key| HOST_ICONS.iter().find(|icon| icon.key == key));
        chips = chips.push(group_chip(app, group.clone(), group.clone(), icon));
        shown += 1;
    }

    let mut picker = column![text_input(t("筛选分组，或直接输入新分组名"), &app.profile_group)
        .on_input(Message::ProfileGroupChanged)
        .on_submit(Message::SaveProfile)
        .padding([5, 8])
        .style(text_input_style)
        .width(Fill)]
    .spacing(6);

    if shown > 0 {
        picker = picker.push(chips.wrap());
    }
    if hidden > 0 {
        picker = picker.push(
            text(tf("还有 {} 个分组未显示，继续输入以筛选", &[&hidden]))
                .size(10)
                .color(muted_text()),
        );
    }
    // Say so out loud. Inventing a folder by typo is the failure this picker
    // exists to prevent, and without the old 新建分组 button there is nothing
    // else to distinguish "creating one" from "picking one".
    if !settled {
        picker = picker.push(
            text(tf("保存后将新建分组：{}", &[&typed]))
                .size(10)
                .color(accent()),
        );
    }

    picker.into()
}

/// The fields nine edits out of ten never touch, behind one disclosure.
///
/// Collapsed state is read from `app.profile_advanced_open` rather than
/// inferred from whether any of these happens to be set: a section that opens
/// itself would make the dialog a different height for every session, and
/// leave the user no state to click back to. It is sticky for the run but
/// never persisted, so a fresh start is always collapsed.
fn advanced_section(app: &AditApp) -> Element<'_, Message> {
    let open = app.profile_advanced_open;
    let mut head = row![
        text(if open { "▾" } else { "▸" }).size(11).color(muted_text()),
        text(t("高级")).size(12),
        // Name what is inside while it is shut: ProxyJump used to be findable
        // by scrolling, and a nameless disclosure would just have lost it.
        text(t("跳板机、启动命令、TERM、环境色标、标签徽标、连接保留"))
            .size(10)
            .color(muted_text()),
        Space::new().width(Fill),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    // A session that already carries advanced settings has to say so, or
    // collapsing them hides the fact that this profile is unusual.
    let set = advanced_field_count(app);
    if set > 0 {
        head = head.push(text(tf("已设置 {} 项", &[&set])).size(10).color(accent()));
    }

    let mut section = column![button(head)
        .width(Fill)
        .padding([4, 6])
        .style(|_theme, status| menu_button_style(false, status))
        .on_press(Message::ToggleProfileAdvanced)]
    .spacing(12);

    if !open {
        return section.into();
    }

    section = section
        .push(text_field(
            "跳板机 ProxyJump（可选，user@bastion:22，多个用逗号按顺序；各跳板机复用本会话的密码/密钥）",
            "jump@bastion.example.com:22",
            &app.profile_jumps,
            Message::ProfileJumpsChanged,
        ))
        .push(
            row![
                cell(
                    text_field(
                        "启动命令（可选，连接后自动执行，如 tmux attach）",
                        "tmux new -A -s main",
                        &app.profile_startup_command,
                        Message::ProfileStartupCommandChanged,
                    ),
                    1,
                ),
                cell(
                    text_field(
                        "终端类型 TERM（可选，默认 xterm-256color）",
                        "xterm-256color",
                        &app.profile_terminal_type,
                        Message::ProfileTerminalTypeChanged,
                    ),
                    1,
                ),
            ]
            .spacing(10),
        )
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
            .wrap()
            .into(),
        ))
        // Worded around what it costs, not what it enables. "Keep the
        // connection" sounds free; what is being agreed to is an authenticated
        // SSH connection to a production host left open after someone typed
        // `exit`, and MFA is the reason anyone would want that.
        .push(
            checkbox(app.profile_keep_connection)
                .label(t(
                    "退出 shell 后保留 SSH 连接，供 SFTP/隧道复用（MFA 主机需要；连接会一直挂着）",
                ))
                .on_toggle(Message::ProfileKeepConnectionToggled)
                .size(16)
                .text_size(12),
        );

    let label_field = text_field(
        "标签徽标（可选，如 PROD；留空用环境名）",
        "PROD",
        &app.profile_label,
        Message::ProfileLabelChanged,
    );
    section = if app.profile_environment == Environment::Custom {
        section.push(
            row![
                cell(
                    text_field(
                        "自定义颜色（#RRGGBB）",
                        "#3f7fd1",
                        &app.profile_accent_color,
                        Message::ProfileAccentColorChanged,
                    ),
                    1,
                ),
                cell(label_field, 1),
            ]
            .spacing(10),
        )
    } else {
        section.push(label_field)
    };

    section.into()
}

/// How many of the advanced fields this profile actually uses — the badge on
/// the collapsed header. Display only; the section's open state is its own.
fn advanced_field_count(app: &AditApp) -> usize {
    usize::from(!app.profile_jumps.trim().is_empty())
        + usize::from(!app.profile_startup_command.trim().is_empty())
        + usize::from(!app.profile_terminal_type.trim().is_empty())
        + usize::from(app.profile_environment != Environment::None)
        + usize::from(!app.profile_label.trim().is_empty())
        + usize::from(app.profile_keep_connection)
}

/// One folder in the picker — the same shape and the same selected style as
/// `protocol_button`, because it is the same kind of choice.
fn group_chip(
    app: &AditApp,
    group: String,
    label: String,
    icon: Option<&HostIcon>,
) -> Element<'static, Message> {
    let selected = app.profile_group.trim() == group;

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
