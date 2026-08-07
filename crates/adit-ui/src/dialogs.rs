use super::*;
use iced::widget::column;

pub(crate) fn connection_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let Some(dialog) = app.connection_dialog.as_ref() else {
        return Space::new().width(Fill).height(Fill).into();
    };

    // The dialog is shared by SSH and RDP; label it by the profile's protocol.
    let protocol = app
        .manager
        .profile(dialog.profile_id)
        .map(|p| p.protocol)
        .unwrap_or(Protocol::Ssh);
    let is_rdp = protocol == Protocol::Rdp;

    let auth_hint = if is_rdp {
        "密码认证：请输入远程桌面 (RDP) 登录密码"
    } else {
        match dialog.auth_method {
            AuthMethod::Auto => "自动认证：密码可选，会先尝试密码、agent 和默认密钥",
            AuthMethod::Password => "密码认证：请输入 SSH 密码",
            AuthMethod::Key => "密钥认证：passphrase 建议在会话设置里保存；未保存时可在此临时输入",
            AuthMethod::Agent => "Agent 认证：通常不需要密码",
        }
    };
    let dialog_title = if is_rdp { "连接 RDP" } else { "连接 SSH" };

    let mut details = column![
        row![
            text(dialog_title).size(16).color(primary_text()),
            Space::new().width(Fill),
            button("×")
                .width(Length::Fixed(26.0))
                .height(Length::Fixed(24.0))
                .padding(0)
                .style(|_theme, status| close_button_style(status))
                .on_press(Message::CancelConnection),
        ]
        .align_y(Alignment::Center),
        text(dialog.title.clone()).size(13).color(primary_text()),
        text(dialog.endpoint.clone()).size(12).color(muted_text()),
        text(auth_hint).size(11).color(muted_text()),
    ]
    .spacing(6);

    if !dialog.identity_file.trim().is_empty() {
        details = details.push(
            text(format!("Identity: {}", dialog.identity_file))
                .size(11)
                .color(muted_text()),
        );
    }

    let panel = container(
        column![
            details,
            text_input("Password / passphrase", &app.password)
                .secure(true)
                .on_input(Message::ConnectionPasswordChanged)
                .on_submit(Message::ConfirmConnection)
                .padding([6, 8])
                .style(text_input_style),
            checkbox(app.remember_connection_password)
                .label(t("保存密码"))
                .on_toggle(Message::RememberConnectionPasswordChanged)
                .size(14)
                .text_size(12)
                .spacing(8),
            text(t("加密保存在配置目录，可随 Dropbox 等同步到其他电脑"))
                .size(10)
                .color(muted_text()),
            row![
                button("取消")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelConnection),
                button("连接")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmConnection),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(430.0))
    .padding(14)
    .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

pub(crate) fn host_key_dialog_overlay(
    session_id: SessionId,
    prompt: &HostKeyPromptInfo,
) -> Element<'static, Message> {
    let changed = prompt.previous_fingerprint.is_some();
    let title = if changed {
        "⚠ 主机密钥已变更"
    } else {
        "确认主机密钥"
    };
    let title_color = if changed { danger() } else { primary_text() };

    let mut details = column![
        text(title).size(16).color(title_color),
        text(format!("{}:{}", prompt.host, prompt.port))
            .size(13)
            .color(primary_text()),
        text(tf("密钥类型: {}", &[&prompt.key_type]))
            .size(11)
            .color(muted_text()),
        text(t("SHA256 指纹")).size(11).color(muted_text()),
        text(prompt.fingerprint.clone())
            .size(12)
            .font(Font::MONOSPACE)
            .color(primary_text()),
    ]
    .spacing(6);

    if let Some(previous) = &prompt.previous_fingerprint {
        details = details
            .push(text(t("此前记录的指纹")).size(11).color(muted_text()))
            .push(
                text(previous.clone())
                    .size(12)
                    .font(Font::MONOSPACE)
                    .color(danger()),
            )
            .push(
                text(t("密钥变更可能意味着中间人攻击。仅在你确知服务器更换过密钥时才接受。"))
                    .size(11)
                    .color(danger()),
            );
    } else {
        details = details.push(
            text(t("首次连接此主机。请通过其它可信渠道核对指纹后再信任。"))
                .size(11)
                .color(muted_text()),
        );
    }

    let accept_label = if changed {
        "更新并继续"
    } else {
        "信任并继续"
    };

    let panel = container(
        column![
            details,
            row![
                button("拒绝")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::RespondHostKey {
                        session_id,
                        accept: false
                    }),
                button(text(accept_label))
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::RespondHostKey {
                        session_id,
                        accept: true
                    }),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(460.0))
    .padding(14)
    .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// Mirror the session manager's pending interactive-auth prompt into UI state,
/// (re)sizing the answer buffer when a new prompt (or round) appears and clearing
/// it once the prompt is gone.
pub(crate) fn sync_auth_prompt(app: &mut AditApp) {
    match app.manager.pending_auth_prompt() {
        Some((session_id, prompt)) => {
            let is_new = app.auth_prompt.as_ref().map(|(id, _)| *id) != Some(session_id)
                || app.auth_prompt_answers.len() != prompt.prompts.len();
            if is_new {
                app.auth_prompt_answers = vec![String::new(); prompt.prompts.len()];
            }
            app.auth_prompt = Some((session_id, prompt));
        }
        None => {
            if app.auth_prompt.is_some() {
                app.auth_prompt = None;
                app.auth_prompt_answers.clear();
            }
        }
    }
}

/// Modal for keyboard-interactive / MFA challenges: one input per server field
/// (masked unless the server asks for echo), answered by the user at connect time.
pub(crate) fn auth_prompt_dialog_overlay<'a>(
    session_id: SessionId,
    prompt: &'a AuthPromptInfo,
    answers: &'a [String],
) -> Element<'a, Message> {
    let mut body = column![text(t("需要交互式验证")).size(16).color(primary_text())].spacing(8);

    if !prompt.name.trim().is_empty() {
        body = body.push(text(prompt.name.clone()).size(12).color(primary_text()));
    }
    if !prompt.instructions.trim().is_empty() {
        body = body.push(
            text(prompt.instructions.clone())
                .size(11)
                .color(muted_text()),
        );
    }

    let last = prompt.prompts.len().saturating_sub(1);
    for (index, field) in prompt.prompts.iter().enumerate() {
        let value = answers.get(index).map(String::as_str).unwrap_or("");
        let label = if field.prompt.trim().is_empty() {
            String::from(t("请输入"))
        } else {
            field.prompt.clone()
        };
        let mut input = text_input("", value)
            .on_input(move |value| Message::AuthPromptInput { index, value })
            .padding([6, 8])
            .style(text_input_style)
            .width(Fill);
        // Only the last field submits on Enter, so a multi-field round isn't sent
        // prematurely with later fields still blank.
        if index == last {
            input = input.on_submit(Message::SubmitAuthPrompt { session_id });
        }
        if !field.echo {
            input = input.secure(true);
        }
        body = body.push(column![text(label).size(11).color(muted_text()), input].spacing(4));
    }

    let panel = container(
        column![
            body,
            row![
                button("取消")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelAuthPrompt { session_id }),
                button("确定")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::SubmitAuthPrompt { session_id }),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(420.0))
    .padding(14)
    .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// Confirm-before-open for a terminal hyperlink: shows the **real** destination
/// (the URL came from remote output) before launching the browser.
pub(crate) fn hyperlink_confirm_overlay(url: &str) -> Element<'static, Message> {
    let panel = container(
        column![
            text(t("打开链接？")).size(16).color(primary_text()),
            text(t("此链接来自终端输出，请确认目标地址后再打开："))
                .size(11)
                .color(muted_text()),
            container(
                text(url.to_string())
                    .size(12)
                    .font(Font::MONOSPACE)
                    .color(primary_text()),
            )
            .padding([6, 8])
            .width(Fill)
            .style(|_theme| connection_dialog_style()),
            row![
                button("取消")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelOpenHyperlink),
                button(text(t("打开")).size(13))
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmOpenHyperlink),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(480.0))
    .padding(14)
    .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

pub(crate) fn add_tunnel(app: &mut AditApp) {
    let bind_port: u16 = match app.tunnel_bind_port.trim().parse() {
        Ok(port) if port > 0 => port,
        _ => {
            app.last_error = Some(String::from(t("请输入有效的本地端口")));
            return;
        }
    };
    let (target_host, target_port) = match app.tunnel_kind {
        TunnelKind::Local | TunnelKind::Remote => {
            let host = app.tunnel_target_host.trim().to_string();
            if host.is_empty() {
                app.last_error = Some(String::from(t("该转发需要填写目标主机")));
                return;
            }
            match app.tunnel_target_port.trim().parse::<u16>() {
                Ok(port) if port > 0 => (host, port),
                _ => {
                    app.last_error = Some(String::from(t("请输入有效的目标端口")));
                    return;
                }
            }
        }
        TunnelKind::Dynamic => (String::new(), 0),
    };

    let bind_address = {
        let trimmed = app.tunnel_bind_addr.trim();
        if trimmed.is_empty() {
            String::from("127.0.0.1")
        } else {
            trimmed.to_string()
        }
    };

    match app.manager.open_tunnel(
        app.tunnel_kind,
        bind_address.clone(),
        bind_port,
        target_host.clone(),
        target_port,
    ) {
        Ok(()) => {
            app.last_error = None;
            app.notice = String::from(t("已创建端口转发"));
            // Persist to the active profile so it auto-starts on the next connect.
            if app.tunnel_save {
                if let Some(profile_id) = app.manager.active_session_summary().map(|s| s.profile_id)
                {
                    app.manager.add_profile_tunnel(
                        profile_id,
                        TunnelDef {
                            kind: app.tunnel_kind,
                            bind_address,
                            bind_port,
                            target_host,
                            target_port,
                        },
                    );
                    persist_profiles(app);
                }
            }
            app.tunnel_bind_port.clear();
            app.tunnel_target_host.clear();
            app.tunnel_target_port.clear();
        }
        Err(error) => app.last_error = Some(tf("端口转发失败: {}", &[&error])),
    }
}

pub(crate) fn about_dialog_overlay() -> Element<'static, Message> {
    let version = env!("CARGO_PKG_VERSION");
    let card = container(
        column![
            row![
                text("Adit").size(20).color(primary_text()),
                Space::new().width(Fill),
                button("×")
                    .width(Length::Fixed(26.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseAbout),
            ]
            .align_y(Alignment::Center),
            text(tf("版本 v{}", &[&version])).size(13).color(accent()),
            text(t("原生 Rust 桌面 SSH 终端")).size(13).color(primary_text()),
            text(t("iced · russh · vte 终端核心 — 无 WebView，无 JavaScript"))
                .size(12)
                .color(muted_text()),
            text("github.com/weironz/adit").size(12).color(muted_text()),
            row![
                Space::new().width(Fill),
                button(text(t("确定")).size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::CloseAbout),
            ],
        ]
        .spacing(12),
    )
    .width(Length::Fixed(380.0))
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

/// A single font-family choice button (label rendered in that very font).
pub(crate) fn appearance_font_button(index: usize, current: u8) -> Element<'static, Message> {
    let (label, family) = FONT_PRESETS[index];
    let selected = index as u8 == current;
    let font = match family {
        Some(name) => Font::with_name(name),
        None => Font::MONOSPACE,
    };
    button(text(label).size(12).font(font))
        .padding([5, 10])
        .width(Length::Fixed(134.0))
        .style(move |_theme, status| {
            if selected {
                primary_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(Message::FontFamilyChanged(index as u8))
        .into()
}

/// A color-scheme choice button: a background swatch plus the scheme name.
pub(crate) fn appearance_scheme_button(index: usize, current: u8) -> Element<'static, Message> {
    let scheme = &COLOR_SCHEMES[index];
    let selected = index as u8 == current;
    let (br, bg, bb) = scheme.background;
    let (fr, fg, fb) = scheme.ansi[2];
    let swatch = container(Space::new())
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(14.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(br, bg, bb))),
            border: Border {
                color: Color::from_rgb8(fr, fg, fb),
                width: 1.5,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        });
    button(
        row![swatch, text(scheme.name).size(12)]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding([5, 10])
    .width(Length::Fixed(150.0))
    .style(move |_theme, status| {
        if selected {
            primary_button_style(status)
        } else {
            secondary_button_style(status)
        }
    })
    .on_press(Message::ColorSchemeChanged(index as u8))
    .into()
}

pub(crate) fn appearance_highlight_button(
    spec: &'static highlight::RuleSpec,
    on: bool,
) -> Element<'static, Message> {
    // The swatch is the rule's own colour resolved through the active scheme, so
    // the dialog shows what the rule will actually look like rather than a
    // stand-in.
    let swatch = container(Space::new())
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(14.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(palette_color(spec.ansi))),
            border: Border {
                radius: 3.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });
    button(
        row![
            text(if on { "✓" } else { " " }).size(12).width(Length::Fixed(12.0)),
            swatch,
            text(spec.label).size(12),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([5, 10])
    .width(Length::Fixed(150.0))
    .style(move |_theme, status| {
        if on {
            primary_button_style(status)
        } else {
            secondary_button_style(status)
        }
    })
    .on_press(Message::HighlightRuleToggled(spec.id))
    .into()
}

/// Chunk stretchable cards into rows of `per_row`, padding the last row.
///
/// Cards fill their share of the row, which is what removes the dead strip a
/// fixed width left down the right of a wide window. The padding matters for the
/// same reason in reverse: without it a final row of one card would stretch that
/// card across the whole pane.
pub(crate) fn wrap_cards(mut cards: Vec<Element<'static, Message>>, per_row: usize) -> Element<'static, Message> {
    let mut rows = column![].spacing(8);
    while !cards.is_empty() {
        let take = cards.len().min(per_row);
        let mut r = row![].spacing(8);
        for element in cards.drain(0..take) {
            r = r.push(element);
        }
        for _ in take..per_row {
            r = r.push(Space::new().width(Fill));
        }
        rows = rows.push(r);
    }
    rows.into()
}

/// Chunk a flat list of built widgets into rows of `per_row`.
pub(crate) fn wrap_rows(mut buttons: Vec<Element<'static, Message>>, per_row: usize) -> Element<'static, Message> {
    let mut rows = column![].spacing(8);
    while !buttons.is_empty() {
        let take = buttons.len().min(per_row);
        let mut r = row![].spacing(8);
        for element in buttons.drain(0..take) {
            r = r.push(element);
        }
        rows = rows.push(r);
    }
    rows.into()
}

pub(crate) fn appearance_section(app: &AditApp) -> Element<'_, Message> {
    let current_font = font_preset_index(&app.font_family);
    let current_scheme = color_scheme_index(&app.color_scheme);
    let size = app.font_size as i32;

    let font_buttons: Vec<Element<'static, Message>> = (0..FONT_PRESETS.len())
        .map(|i| appearance_font_button(i, current_font))
        .collect();
    let scheme_buttons: Vec<Element<'static, Message>> = (0..COLOR_SCHEMES.len())
        .map(|i| appearance_scheme_button(i, current_scheme))
        .collect();
    let highlight_buttons: Vec<Element<'static, Message>> = highlight::rules()
        .iter()
        .map(|spec| {
            let on = app
                .highlight_rules
                .get(spec.id)
                .copied()
                .unwrap_or(spec.enabled);
            appearance_highlight_button(spec, on)
        })
        .collect();

    let size_row = row![
        text(t("字号"))
            .size(12)
            .color(muted_text())
            .width(Length::Fixed(52.0)),
        button(text("−").size(15))
            .width(Length::Fixed(32.0))
            .padding([2, 0])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::FontSizeStep(-1)),
        container(text(format!("{size} px")).size(13).color(primary_text()))
            .width(Length::Fixed(56.0))
            .center_x(Length::Fixed(56.0)),
        button(text("＋").size(15))
            .width(Length::Fixed(32.0))
            .padding([2, 0])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::FontSizeStep(1)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // Live preview — the static appearance is already set for this frame, so the
    // sample renders in exactly the chosen font + palette.
    let swatches = (0..16).fold(row![].spacing(2), |r, i| {
        r.push(
            container(Space::new())
                .width(Length::Fixed(13.0))
                .height(Length::Fixed(13.0))
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(palette_color(i))),
                    border: Border {
                        radius: 2.0.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                }),
        )
    });
    let preview = container(
        column![
            text("adit@host:~/project$  ls -la  AaBbCc 0123")
                .size(term_font_size())
                .font(term_font())
                .color(default_foreground()),
            swatches,
        ]
        .spacing(8),
    )
    .width(Fill)
    .padding(12)
    .style(|_theme| container::Style {
        background: Some(Background::Color(terminal_background())),
        border: Border {
            color: border_color(),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    });

    column![
            text(t("字体")).size(12).color(muted_text()),
            wrap_rows(font_buttons, 3),
            size_row,
            text(t("配色方案")).size(12).color(muted_text()),
            wrap_rows(scheme_buttons, 3),
            text(t("输出高亮")).size(12).color(muted_text()),
            text(t("仅对服务端未着色的文本生效，全屏程序（vim、less 等）中不启用"))
                .size(11)
                .color(muted_text()),
            wrap_rows(highlight_buttons, 3),
            text(t("预览")).size(12).color(muted_text()),
            preview,
    ]
    .spacing(12)
    .into()
}

pub(crate) fn update_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let current = env!("CARGO_PKG_VERSION");

    let body: Element<'_, Message> = match &app.update_state {
        UpdateState::Idle | UpdateState::Checking => {
            column![text(t("正在检查更新…")).size(13).color(primary_text())].into()
        }
        UpdateState::UpToDate => column![
            text(tf("已是最新版本（v{}）", &[&current]))
                .size(13)
                .color(primary_text()),
        ]
        .into(),
        UpdateState::Available(info) => {
            let mut col = column![
                text(tf("发现新版本 {}", &[&info.tag]))
                    .size(15)
                    .color(accent()),
                text(tf("当前版本 v{}", &[&current]))
                    .size(12)
                    .color(muted_text()),
            ]
            .spacing(6);
            if !info.notes_url.is_empty() {
                col = col.push(
                    button(text(t("查看发布说明")).size(12))
                        .padding([3, 0])
                        .style(|_theme, _status| {
                            base_button_style(transparent(), accent(), transparent())
                        })
                        .on_press(Message::OpenReleaseNotes(info.notes_url.clone())),
                );
            }
            let action = if info.installer_url.is_empty() {
                text(t("该版本暂无 Windows 安装包"))
                    .size(12)
                    .color(muted_text())
                    .into()
            } else {
                let btn: Element<'_, Message> = button(text(t("下载并更新")).size(12))
                    .padding([6, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::StartUpdateDownload)
                    .into();
                btn
            };
            col.push(Space::new().height(Length::Fixed(4.0)))
                .push(action)
                .into()
        }
        UpdateState::Downloading => column![
            text(t("正在下载安装包…")).size(13).color(primary_text()),
            text(t("完成后会自动启动安装程序"))
                .size(11)
                .color(muted_text()),
        ]
        .spacing(6)
        .into(),
        UpdateState::Launched => column![
            text(t("正在后台安装更新…")).size(13).color(primary_text()),
            text(t("无需操作，安装完成后 Adit 会自动关闭并重启（可能需要确认一次 UAC）"))
                .size(11)
                .color(muted_text()),
        ]
        .spacing(6)
        .into(),
        UpdateState::Error(error) => column![
            text(t("检查/更新失败")).size(13).color(danger()),
            text(error.clone()).size(11).color(muted_text()),
            button(text(t("重试")).size(12))
                .padding([5, 16])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CheckForUpdates),
        ]
        .spacing(8)
        .into(),
    };

    let card = container(
        column![
            row![
                text(t("检查更新")).size(18).color(primary_text()),
                Space::new().width(Fill),
                button("×")
                    .width(Length::Fixed(26.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseUpdateDialog),
            ]
            .align_y(Alignment::Center),
            body,
            row![
                Space::new().width(Fill),
                button(text(t("关闭")).size(12))
                    .padding([5, 18])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CloseUpdateDialog),
            ],
        ]
        .spacing(16),
    )
    .width(Length::Fixed(420.0))
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

/// Small dialog to rename the active session's tab.
pub(crate) fn session_rename_overlay(app: &AditApp) -> Element<'_, Message> {
    let card = container(
        column![
            text(t("重命名标签")).size(16).color(primary_text()),
            text_input("标签名称", &app.session_rename_draft)
                .on_input(Message::SessionRenameChanged)
                .on_submit(Message::ConfirmRenameSession)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
            row![
                Space::new().width(Fill),
                button(text(t("取消")).size(12))
                    .padding([5, 16])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelRenameSession),
                button(text(t("确定")).size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmRenameSession),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(380.0))
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

/// Command-snippets panel: list saved commands (send / delete) + an add form.
pub(crate) fn snippets_panel_overlay(app: &AditApp) -> Element<'_, Message> {
    let header = row![
        text(t("命令片段")).size(16).color(primary_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseSnippets),
    ]
    .align_y(Alignment::Center);

    let mut list = column![].spacing(6);
    if app.snippets.is_empty() {
        list = list.push(
            text(t("还没有片段。在下方添加常用命令，一键发送到当前终端。"))
                .size(11)
                .color(muted_text()),
        );
    }
    for (index, snippet) in app.snippets.iter().enumerate() {
        list = list.push(
            container(
                row![
                    column![
                        text(snippet.name.clone()).size(12).color(primary_text()),
                        text(snippet.command.clone()).size(11).color(muted_text()),
                    ]
                    .spacing(1)
                    .width(Fill),
                    button(text(t("发送")).size(11))
                        .padding([4, 12])
                        .style(|_theme, status| primary_button_style(status))
                        .on_press(Message::SendSnippet(index)),
                    button(text(t("删除")).size(11))
                        .padding([4, 10])
                        .style(|_theme, status| secondary_button_style(status))
                        .on_press(Message::DeleteSnippet(index)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([4, 6])
            .style(|_theme| sftp_pane_style()),
        );
    }

    let form = column![
        text(t("新增片段")).size(12).color(muted_text()),
        text_input("名称（可选）", &app.snippet_name_draft)
            .on_input(Message::SnippetNameChanged)
            .padding([5, 8])
            .style(text_input_style)
            .width(Fill),
        row![
            text_input("命令，如 tail -f /var/log/syslog", &app.snippet_command_draft)
                .on_input(Message::SnippetCommandChanged)
                .on_submit(Message::AddSnippet)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
            button(text(t("添加")).size(12))
                .padding([5, 16])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::AddSnippet),
        ]
        .spacing(8),
    ]
    .spacing(6);

    let card = container(
        column![
            header,
            scrollable(list).height(Length::Fixed(240.0)),
            form,
        ]
        .spacing(14),
    )
    .width(Length::Fixed(560.0))
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

/// Confirmation dialog shown before pasting multi-line clipboard text.
pub(crate) fn paste_confirm_overlay(app: &AditApp) -> Element<'_, Message> {
    let contents = app.pending_paste.as_deref().unwrap_or_default();
    let line_count = contents.lines().count().max(1);
    let preview: String = contents.lines().take(8).collect::<Vec<_>>().join("\n");
    let preview = if preview.chars().count() > 400 {
        format!("{}…", preview.chars().take(400).collect::<String>())
    } else {
        preview
    };

    let card = container(
        column![
            text(t("确认粘贴")).size(16).color(primary_text()),
            text(tf("将向当前终端粘贴 {} 行内容：", &[&line_count]))
                .size(12)
                .color(muted_text()),
            container(
                scrollable(text(preview).size(12).font(Font::MONOSPACE).color(primary_text()))
                    .height(Length::Fixed(140.0))
            )
            .width(Fill)
            .padding(10)
            .style(|_theme| container::Style {
                background: Some(Background::Color(terminal_background())),
                border: border(RADIUS_SM, 1.0, border_color()),
                ..container::Style::default()
            }),
            row![
                Space::new().width(Fill),
                button(text(t("取消")).size(12))
                    .padding([5, 16])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelPaste),
                button(text(t("粘贴")).size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmPaste),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(480.0))
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

/// A read-only path row: label + monospace path + an 打开 button.
pub(crate) fn options_path_row<'a>(
    label: &'a str,
    path: String,
    open: Option<Message>,
) -> Element<'a, Message> {
    let mut row = row![
        text(label)
            .size(11)
            .color(muted_text())
            .width(Length::Fixed(96.0)),
        container(text(path).size(12).font(Font::MONOSPACE).color(primary_text()))
            .width(Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if let Some(message) = open {
        row = row.push(
            button(text(t("打开")).size(11))
                .padding([3, 12])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(message),
        );
    }
    row.into()
}

/// The trusted-host-keys (known_hosts) management dialog: list each pinned
/// `host → key type · SHA256 fingerprint` and forget individual entries.
pub(crate) fn known_hosts_overlay(app: &AditApp) -> Element<'_, Message> {
    let header = row![
        text(t("受信主机密钥")).size(15).color(primary_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseKnownHosts),
    ]
    .align_y(Alignment::Center);

    let mut list = column![].spacing(4).width(Fill);
    if app.known_hosts.is_empty() {
        list = list.push(
            text(t("尚无受信主机密钥（首次连接会自动信任并记录）"))
                .size(12)
                .color(muted_text()),
        );
    } else {
        for entry in &app.known_hosts {
            let host = entry.host.clone();
            let fingerprint = entry.fingerprint.clone();
            list = list.push(
                container(
                    row![
                        column![
                            text(entry.host.clone()).size(12).color(primary_text()),
                            text(format!("{} · {}", entry.key_type, entry.fingerprint))
                                .size(10)
                                .font(Font::MONOSPACE)
                                .color(muted_text()),
                        ]
                        .spacing(1)
                        .width(Fill),
                        button(text(t("删除")).size(11))
                            .padding([3, 10])
                            .style(|_theme, status| close_button_style(status))
                            .on_press(Message::RemoveKnownHost(host, fingerprint)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([4, 6])
                .style(|_theme| sftp_row_highlight(false)),
            );
        }
    }

    let body = column![
        header,
        text(known_hosts_path().display().to_string())
            .size(10)
            .font(Font::MONOSPACE)
            .color(muted_text()),
        text(t("删除某台主机后，下次连接会重新记录其密钥；密钥被更改（可能的中间人）时仍会拦截。"))
            .size(11)
            .color(muted_text()),
        scrollable(list).height(Length::Fixed(360.0)),
    ]
    .spacing(10);

    let card = container(body)
        .width(Length::Fixed(560.0))
        .padding(18)
        .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// The three category bodies the 设置 dialog draws for 应用 / 日志 / 终端.
///
/// Built together because they share the same locals; the caller picks one.
pub(crate) fn options_sections(app: &AditApp) -> [Element<'_, Message>; 3] {
    let config_dir = &app.config_dir;
    // The env override, if set, wins over the UI, so hide the change controls.
    let overridden = std::env::var_os("ADIT_CONFIG_DIR")
        .is_some_and(|value| !value.is_empty());
    let is_custom = app.config_dir_custom;

    // The config-folder row: the current path, an "open" button, and (unless the
    // env override is in force) "change" / "reset to default" buttons.
    let mut config_dir_row = row![
        text(t("配置目录"))
            .size(11)
            .color(muted_text())
            .width(Length::Fixed(96.0)),
        container(
            text(config_dir.display().to_string())
                .size(12)
                .font(Font::MONOSPACE)
                .color(primary_text())
        )
        .width(Fill),
        button(text(t("打开")).size(11))
            .padding([3, 12])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::OpenConfigFolder),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if !overridden {
        config_dir_row = config_dir_row.push(
            button(text(t("更改…")).size(11))
                .padding([3, 12])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::PickConfigDir),
        );
        if is_custom {
            config_dir_row = config_dir_row.push(
                button(text(t("恢复默认")).size(11))
                    .padding([3, 12])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::ResetConfigDir),
            );
        }
    }

    let config_note = if overridden {
        "由环境变量 ADIT_CONFIG_DIR 指定（重启生效）"
    } else {
        "指向 Dropbox 等同步盘可在多台机器间同步会话配置（密码仍保存在各机器本地凭据库）。更改后重启 Adit 生效。"
    };

    let mut config_section = column![
        text(t("配置目录")).size(13).color(primary_text()),
        setting_card(config_dir_row),
        setting_card(options_path_row(
            t("会话配置"),
            config_dir.join("profiles.json").display().to_string(),
            None,
        )),
        setting_card(options_path_row(
            t("应用设置"),
            config_dir.join("settings.json").display().to_string(),
            None,
        )),
        setting_card(text(t(config_note)).size(11).color(muted_text()))]
    .spacing(10);

    if let Some(pending) = &app.pending_config_dir {
        config_section = config_section.push(
            text(tf("重启后生效: {}", &[&pending.display()]))
                .size(11)
                .color(accent()),
        );
    }

    config_section = config_section
        .push(
            row![
                text(t("连接超时（秒，0 = 不限）"))
                    .size(12)
                    .color(muted_text())
                    .width(Length::Fixed(180.0)),
                text_input("20", &app.connect_timeout_secs.to_string())
                    .on_input(Message::ConnectTimeoutChanged)
                    .padding([4, 8])
                    .style(text_input_style)
                    .width(Length::Fixed(80.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .push(
            row![
                text(t("滚动历史行数"))
                    .size(12)
                    .color(muted_text())
                    .width(Length::Fixed(180.0)),
                text_input("5000", &app.scrollback_lines.to_string())
                    .on_input(Message::ScrollbackLinesChanged)
                    .padding([4, 8])
                    .style(text_input_style)
                    .width(Length::Fixed(80.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .push(
            checkbox(app.auto_check_updates)
                .label(t("启动时自动检查更新"))
                .on_toggle(Message::ToggleAutoCheckUpdates)
                .size(16)
                .text_size(12),
        )
        .push(
            checkbox(app.auto_accept_host_keys)
                .label(t("自动信任新主机密钥（不逐个弹窗确认）"))
                .on_toggle(Message::ToggleAutoAcceptHostKeys)
                .size(16)
                .text_size(12),
        )
        .push(
            checkbox(app.rdp_clipboard)
                .label(t("RDP 会话共享剪贴板（仅文本，下次连接生效）"))
                .on_toggle(Message::ToggleRdpClipboard)
                .size(16)
                .text_size(12),
        )
        // Also on the toolbar's ⚡ dropdown; duplicated here because the toolbar
        // collapses to a tab, and a setting should not hide inside it.
        .push(
            row![
                text(t("RDP 画质")).size(12).color(primary_text()),
                rdp_quality_choice(app, RdpQuality::High),
                rdp_quality_choice(app, RdpQuality::Balanced),
                rdp_quality_choice(app, RdpQuality::Speed),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );

    // Live preview of the rendered log filename for the active (or a sample)
    // session.
    let sample = app
        .manager
        .active_session_summary()
        .map(|summary| (summary.title, summary.endpoint))
        .unwrap_or_else(|| (String::from("web01"), String::from("root@10.0.0.5:22")));
    let preview_name = render_log_name(&effective_log_pattern(app), &sample.0, &sample.1);
    let preview_path = effective_log_dir(app).join(&preview_name);

    let log_section = column![
        text(t("会话日志")).size(13).color(primary_text()),
        setting_card(column![
            text(t("日志目录（留空 = 配置目录下的 logs）"))
                .size(11)
                .color(muted_text()),
            row![
                text_input(
                    &app.config_dir.join("logs").display().to_string(),
                    &app.log_dir,
                )
                .on_input(Message::LogDirChanged)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
                button(text(t("浏览…")).size(11))
                    .padding([5, 12])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::PickLogDir),
                button(text(t("打开")).size(11))
                    .padding([5, 12])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::OpenLogFolder),
            ]
            .spacing(8),
        ]
        .spacing(3)),
        setting_card(column![
            text(t("日志文件名（留空 = 默认）")).size(11).color(muted_text()),
            text_input(DEFAULT_LOG_PATTERN, &app.log_name_pattern)
                .on_input(Message::LogNamePatternChanged)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
        ]
        .spacing(3)),
        setting_card(text(t("可用变量：%N 会话名  %H 主机  %Y 年 %M 月 %D 日  %h 时 %m 分 %s 秒"))
            .size(11)
            .color(muted_text())),
        setting_card(options_path_row(t("预览"), preview_path.display().to_string(), None)),
        setting_card(checkbox(app.auto_log_on_connect)
            .label(t("连接后自动开始记录日志"))
            .on_toggle(Message::ToggleAutoLog)
            .size(16)
            .text_size(12)),
        setting_card(checkbox(app.log_plaintext)
            .label(t("记录为纯文本（去除颜色/转义码，便于阅读和 grep）"))
            .on_toggle(Message::ToggleLogPlaintext)
            .size(16)
            .text_size(12))]
    .spacing(10);

    let mouse_section = column![
        text(t("终端复制 / 粘贴（PuTTY 风格）"))
            .size(13)
            .color(primary_text()),
        setting_card(checkbox(app.copy_on_select)
            .label(t("选中内容即复制到剪贴板"))
            .on_toggle(Message::ToggleCopyOnSelect)
            .size(16)
            .text_size(12)),
        setting_card(checkbox(app.right_click_paste)
            .label(t("右键直接粘贴（不弹出菜单）"))
            .on_toggle(Message::ToggleRightClickPaste)
            .size(16)
            .text_size(12)),
        setting_card(checkbox(app.confirm_multiline_paste)
            .label(t("粘贴多行内容前先确认"))
            .on_toggle(Message::ToggleConfirmMultilinePaste)
            .size(16)
            .text_size(12)),
        setting_card(text(t("提示：右键粘贴开启后，清屏 / 回到底部可用工具栏或 Edit 菜单。程序也支持 bracketed paste（应用开启后粘贴不会被自动执行）。"))
            .size(11)
            .color(muted_text()))]
    .spacing(10);


    [config_section.into(), log_section.into(), mouse_section.into()]
}

/// One segment of the settings dialog's RDP-quality picker. A row of small
/// buttons rather than a radio group, so it matches the toolbar's dropdown and
/// stays on one line.
fn rdp_quality_choice(app: &AditApp, quality: RdpQuality) -> Element<'_, Message> {
    let chosen = app.rdp_quality == quality;
    button(text(t(rdp_quality_label(quality))).size(12))
        .padding([4, 10])
        .style(move |_theme, status| {
            if chosen {
                primary_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(Message::RdpQualityChosen(quality))
        .into()
}

pub(crate) fn tunnels_panel_overlay(app: &AditApp) -> Element<'_, Message> {
    let endpoint = app
        .manager
        .active_session_summary()
        .map(|summary| summary.endpoint)
        .unwrap_or_default();

    let header = row![
        text(t("端口转发")).size(15).color(primary_text()),
        text(endpoint).size(11).color(muted_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseTunnels),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let kind_row = row![
        text(t("类型")).size(12).color(muted_text()).width(Length::Fixed(52.0)),
        tunnel_kind_button("本地转发 -L", TunnelKind::Local, app.tunnel_kind),
        tunnel_kind_button("动态 SOCKS -D", TunnelKind::Dynamic, app.tunnel_kind),
        tunnel_kind_button("远程转发 -R", TunnelKind::Remote, app.tunnel_kind),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let hint = match app.tunnel_kind {
        TunnelKind::Local => "本机端口 → 经 SSH 服务器 → 目标地址（访问服务器能到达的内网服务）",
        TunnelKind::Dynamic => "本机启动 SOCKS5 代理，应用挂上后所有流量走服务器出口",
        TunnelKind::Remote => "服务器监听端口 → 经 SSH 隧道 → 本机目标地址（把本地服务暴露给远端网络）",
    };

    let bind_label = if app.tunnel_kind == TunnelKind::Remote {
        "远端"
    } else {
        "本地"
    };
    let bind_placeholder = if app.tunnel_kind == TunnelKind::Remote {
        "127.0.0.1（远端绑定，0.0.0.0 对外）"
    } else {
        "127.0.0.1"
    };

    let bind_row = row![
        text(bind_label).size(12).color(muted_text()).width(Length::Fixed(52.0)),
        text_input(bind_placeholder, &app.tunnel_bind_addr)
            .on_input(Message::TunnelBindAddrChanged)
            .padding([4, 8])
            .style(text_input_style)
            .width(Length::Fixed(150.0)),
        text(":").size(12).color(muted_text()),
        text_input("端口", &app.tunnel_bind_port)
            .on_input(Message::TunnelBindPortChanged)
            .on_submit(Message::AddTunnel)
            .padding([4, 8])
            .style(text_input_style)
            .width(Length::Fixed(90.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut form = column![kind_row, text(hint).size(10).color(muted_text()), bind_row].spacing(8);

    if app.tunnel_kind != TunnelKind::Dynamic {
        let target_label = if app.tunnel_kind == TunnelKind::Remote {
            "本地"
        } else {
            "目标"
        };
        form = form.push(
            row![
                text(target_label).size(12).color(muted_text()).width(Length::Fixed(52.0)),
                text_input("目标主机（如 10.0.0.5）", &app.tunnel_target_host)
                    .on_input(Message::TunnelTargetHostChanged)
                    .padding([4, 8])
                    .style(text_input_style)
                    .width(Fill),
                text(":").size(12).color(muted_text()),
                text_input("端口", &app.tunnel_target_port)
                    .on_input(Message::TunnelTargetPortChanged)
                    .on_submit(Message::AddTunnel)
                    .padding([4, 8])
                    .style(text_input_style)
                    .width(Length::Fixed(90.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }

    form = form.push(
        row![
            checkbox(app.tunnel_save)
                .label(t("保存到会话配置（连接时自动开启）"))
                .on_toggle(Message::ToggleTunnelSave)
                .size(15)
                .text_size(11),
            Space::new().width(Fill),
            button(text(t("添加转发")).size(12))
                .padding([5, 16])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::AddTunnel),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );

    let tunnels = app.manager.tunnels();
    let mut list = column![].spacing(2);
    if tunnels.is_empty() {
        list = list.push(text(t("（暂无转发）")).size(11).color(muted_text()));
    } else {
        for tunnel in tunnels {
            list = list.push(tunnel_row(tunnel));
        }
    }

    // Saved (auto-start) definitions for the active profile.
    let saved: Vec<TunnelDef> = app
        .manager
        .active_session_summary()
        .and_then(|summary| {
            app.manager
                .profile(summary.profile_id)
                .map(|profile| profile.tunnels.clone())
        })
        .unwrap_or_default();
    let mut saved_list = column![].spacing(2);
    if saved.is_empty() {
        saved_list = saved_list.push(text(t("（无）")).size(11).color(muted_text()));
    } else {
        for (index, def) in saved.iter().enumerate() {
            saved_list = saved_list.push(saved_tunnel_row(index, def));
        }
    }

    let content = column![
        header,
        container(form)
            .padding(12)
            .width(Fill)
            .style(|_theme| sftp_pane_style()),
        text(t("已保存（连接时自动开启）")).size(12).color(primary_text()),
        container(saved_list)
            .padding(8)
            .width(Fill)
            .style(|_theme| sftp_list_inner_style()),
        text(t("活动转发")).size(12).color(primary_text()),
        container(scrollable(list).height(Fill))
            .height(Fill)
            .padding(6)
            .style(|_theme| sftp_list_inner_style()),
    ]
    .spacing(10);

    let panel = container(content)
        .width(Fill)
        .height(Fill)
        .padding(16)
        .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .padding(48)
        .style(|_theme| dialog_scrim_style())
        .into()
}

pub(crate) fn tunnel_kind_button(
    label: &'static str,
    kind: TunnelKind,
    current: TunnelKind,
) -> Element<'static, Message> {
    let selected = kind == current;
    button(text(label).size(12))
        .padding([5, 14])
        .style(move |_theme, status| {
            if selected {
                primary_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(Message::TunnelKindChanged(kind))
        .into()
}

pub(crate) fn tunnel_row(tunnel: &TunnelState) -> Element<'static, Message> {
    let kind = match tunnel.kind {
        TunnelKind::Local => "L",
        TunnelKind::Dynamic => "D",
        TunnelKind::Remote => "R",
    };
    let route = match tunnel.kind {
        TunnelKind::Local => format!("{} → {}", tunnel.bind, tunnel.target),
        TunnelKind::Dynamic => format!("{}  (SOCKS5)", tunnel.bind),
        TunnelKind::Remote => tf("远端 {} → 本地 {}", &[&tunnel.bind, &tunnel.target]),
    };
    let status_color = if tunnel.error.is_some() {
        danger()
    } else if tunnel.listening {
        Color::from_rgb8(34, 197, 94)
    } else {
        muted_text()
    };

    container(
        row![
            text(kind).size(11).color(accent()).width(Length::Fixed(18.0)),
            text(route).size(12).color(primary_text()).width(Fill),
            text(tf("活动 {}", &[&tunnel.active]))
                .size(10)
                .color(muted_text())
                .width(Length::Fixed(60.0)),
            text(tunnel.status.clone())
                .size(10)
                .color(status_color)
                .width(Length::Fixed(190.0)),
            button(text(t("关闭")).size(11))
                .padding([3, 10])
                .style(|_theme, status| close_button_style(status))
                .on_press(Message::CloseTunnel(tunnel.id)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .into()
}

pub(crate) fn saved_tunnel_row(index: usize, def: &TunnelDef) -> Element<'static, Message> {
    let label = match def.kind {
        TunnelKind::Local => format!(
            "L  {}:{} → {}:{}",
            def.bind_address, def.bind_port, def.target_host, def.target_port
        ),
        TunnelKind::Dynamic => format!("D  {}:{}  (SOCKS5)", def.bind_address, def.bind_port),
        TunnelKind::Remote => format!(
            "R  远端 {}:{} → 本地 {}:{}",
            def.bind_address, def.bind_port, def.target_host, def.target_port
        ),
    };
    row![
        text(label).size(11).color(primary_text()).width(Fill),
        button(text(t("删除")).size(11))
            .padding([3, 10])
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::RemoveSavedTunnel(index)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .padding([2, 8])
    .into()
}

/// The 同步与云 panel.
///
/// One provider at a time rather than a list of independently connectable
/// services: syncing the same catalog to two places would need a merge between
/// them as well, and every question that answers ("which one wins?") is one the
/// user should not have to think about.
pub(crate) fn sync_section(app: &AditApp) -> Element<'_, Message> {
    use adit_storage::SyncProvider;

    let sync = &app.sync;

    // One card per provider, the way a settings page usually lists accounts:
    // name, what state it is in, and the control that changes it. A row of
    // radio buttons said which was ticked but never whether it worked.
    let provider_card = |provider: SyncProvider| {
        let selected = sync.provider == provider;
        let state = if provider == SyncProvider::None {
            "不同步，配置只留在本机"
        } else if !selected {
            "未使用"
        } else if app.sync_secret_saved {
            "已连接"
        } else if provider.is_oauth() {
            "尚未授权"
        } else {
            "尚未填写凭据"
        };

        let mut pick = button(text(t(if selected { "使用中" } else { "使用" })).size(11))
            .padding([4, 14])
            .style(move |_theme, status| {
                if selected {
                    primary_button_style(status)
                } else {
                    secondary_button_style(status)
                }
            });
        // The active one is not a button to press again; leaving it live would
        // invite a click that does nothing.
        if !selected {
            pick = pick.on_press(Message::SyncProviderChanged(provider));
        }

        container(
            row![
                column![
                    text(t(provider.label())).size(12).color(primary_text()),
                    text(t(state)).size(10).color(muted_text()),
                ]
                .spacing(2),
                Space::new().width(Fill),
                pick,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([8, 12])
        .style(move |_theme| settings_card_style(selected))
    };

    let provider_cards = column![
        provider_card(SyncProvider::None),
        provider_card(SyncProvider::Gist),
        provider_card(SyncProvider::WebDav),
        provider_card(SyncProvider::S3),
        provider_card(SyncProvider::GoogleDrive),
        provider_card(SyncProvider::OneDrive),
        provider_card(SyncProvider::Dropbox),
    ]
    .spacing(6);

    let field = |label: &'static str, value: &str, placeholder: &'static str, which: SyncField| {
        row![
            text(label)
                .size(11)
                .color(muted_text())
                .width(Length::Fixed(96.0)),
            text_input(placeholder, value)
                .on_input(move |text| Message::SyncFieldChanged(which, text))
                .padding([5, 8])
                .size(12)
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
    };

    // A stored secret is never read back into the box — the panel cannot show
    // what it does not need to know. An empty box means "keep what is saved",
    // which is why the placeholder says so.
    let secret_field = |label: &'static str, saved: bool| {
        let placeholder = if saved {
            "已保存（留空则不修改）"
        } else {
            "必填"
        };
        row![
            text(label)
                .size(11)
                .color(muted_text())
                .width(Length::Fixed(96.0)),
            text_input(placeholder, &app.sync_secret_draft)
                .secure(true)
                .on_input(Message::SyncSecretChanged)
                .padding([5, 8])
                .size(12)
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
    };

    let mut body = column![provider_cards].spacing(10);

    match sync.provider {
        SyncProvider::None => {
            body = body.push(
                text(t("选择一个云服务后，会话、分组与设置会在多台机器间合并同步。"))
                    .size(11)
                    .color(muted_text()),
            );
        }
        SyncProvider::Gist => {
            let connected = app.sync_secret_saved;
            // The device flow needs a registered client id like any other
            // OAuth app; without one only the manual token path is available,
            // and the panel says so rather than offering a dead button.
            let unconfigured = sync_client_id(app, SyncProvider::Gist).trim().is_empty();
            let status_line = if app.sync_device_prompt.is_some() {
                "正在等待你在浏览器中确认…"
            } else if app.sync_connecting {
                "正在向 GitHub 申请设备码…"
            } else if unconfigured {
                "此构建未内置 GitHub 的 client id — 可填写自己的，或直接粘贴令牌"
            } else if connected {
                "已连接"
            } else {
                "尚未连接"
            };

            let mut connect =
                button(text(t(if connected { "重新连接账号" } else { "连接账号" })).size(12))
                    .padding([5, 14])
                    .style(|_theme, status| primary_button_style(status));
            if !app.sync_connecting && !unconfigured {
                connect = connect.on_press(Message::SyncConnectAccount);
            }

            body = body.push(
                row![
                    text(t(status_line)).size(11).color(muted_text()),
                    Space::new().width(Fill),
                    connect,
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );

            // The user code, while one is live. This is the entire interaction
            // point of a device flow — there is no redirect back — so it is
            // shown large, in monospace (GitHub's codes mix letters and digits,
            // where 0/O and 1/I have to be told apart), and next to a copy
            // button so it need not be transcribed at all.
            if let Some(prompt) = &app.sync_device_prompt {
                body = body.push(
                    container(
                        column![
                            text(t("用户码")).size(10).color(muted_text()),
                            row![
                                text(prompt.user_code.clone())
                                    .size(24)
                                    .font(Font::MONOSPACE)
                                    .color(primary_text()),
                                Space::new().width(Fill),
                                button(text(t("复制")).size(11))
                                    .padding([4, 12])
                                    .style(|_theme, status| secondary_button_style(status))
                                    .on_press(Message::SyncCopyUserCode),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                            text(tf(
                                "在浏览器中打开 {} 并输入以上用户码。完成后本页会自动继续。",
                                &[&prompt.verification_uri]
                            ))
                            .size(10)
                            .color(muted_text()),
                        ]
                        .spacing(6),
                    )
                    .width(Fill)
                    .padding([10, 12])
                    .style(|_theme| settings_card_style(true)),
                );
            }

            body = body
                .push(field(
                    "Gist ID",
                    &sync.gist_id,
                    "留空则首次同步时自动创建",
                    SyncField::GistId,
                ))
                .push(
                    text(t("授权只申请 gist 权限，看不到你的仓库。GitHub 自带版本历史，可在网页端回滚。"))
                        .size(10)
                        .color(muted_text()),
                )
                // The manual path, kept deliberately. A corporate network can
                // block github.com/login outright, and someone who already
                // holds a fine-grained token should not have to authorise an
                // OAuth app to use it.
                .push(
                    text(t("浏览器不可用时，也可以直接粘贴一个带 gist 权限的个人访问令牌："))
                        .size(10)
                        .color(muted_text()),
                )
                .push(secret_field("访问令牌", app.sync_secret_saved))
                .push(field(
                    "client id",
                    &sync.github_client_id,
                    "留空则用本应用内置的",
                    SyncField::GitHubClientId,
                ));
        }
        SyncProvider::WebDav => {
            body = body
                .push(field(
                    "文件 URL",
                    &sync.webdav_url,
                    "https://dav.example.com/.../adit-sync.json",
                    SyncField::WebDavUrl,
                ))
                .push(field(
                    "用户名",
                    &sync.webdav_username,
                    "alice",
                    SyncField::WebDavUsername,
                ))
                .push(secret_field("密码", app.sync_secret_saved))
                .push(
                    text(t("填到文件而不是目录。Nextcloud、坚果云、群晖均可；该方式支持并发写检测，最安全。"))
                        .size(10)
                        .color(muted_text()),
                );
        }
        SyncProvider::S3 => {
            body = body
                .push(field(
                    "Endpoint",
                    &sync.s3_endpoint,
                    "s3.amazonaws.com / play.min.io",
                    SyncField::S3Endpoint,
                ))
                .push(field("区域", &sync.s3_region, "us-east-1", SyncField::S3Region))
                .push(field("存储桶", &sync.s3_bucket, "adit", SyncField::S3Bucket))
                .push(field(
                    "对象键",
                    &sync.s3_key,
                    "adit/adit-sync.json",
                    SyncField::S3Key,
                ))
                .push(field(
                    "Access Key",
                    &sync.s3_access_key,
                    "AKIA...",
                    SyncField::S3AccessKey,
                ))
                .push(secret_field("Secret Key", app.sync_secret_saved))
                .push(
                    text(t("兼容 AWS S3、MinIO、Cloudflare R2、阿里云 OSS。MinIO 等自建网关需要路径风格寻址。"))
                        .size(10)
                        .color(muted_text()),
                );
        }
        // The three that authorise in a browser. Same shape for all of them,
        // so it is built once rather than copied three times.
        SyncProvider::GoogleDrive | SyncProvider::OneDrive | SyncProvider::Dropbox => {
            let (override_value, override_field, note) = match sync.provider {
                SyncProvider::GoogleDrive => (
                    &sync.google_client_id,
                    SyncField::GoogleClientId,
                    "仅访问本应用创建的文件，看不到你云端硬盘里的其他内容。",
                ),
                SyncProvider::OneDrive => (
                    &sync.onedrive_client_id,
                    SyncField::OneDriveClientId,
                    "仅访问 Adit 自己的应用文件夹，碰不到其他文件。",
                ),
                _ => (
                    &sync.dropbox_client_id,
                    SyncField::DropboxClientId,
                    "仅访问 Apps/Adit 文件夹，碰不到其他文件。",
                ),
            };

            let connected = app.sync_secret_saved;
            // Derived rather than stored: it depends only on the build's
            // baked-in id and the override the user just typed, both of which
            // the view already has.
            let unconfigured = sync_client_id(app, sync.provider).trim().is_empty();
            let status_line = if app.sync_connecting {
                "正在等待浏览器授权…"
            } else if unconfigured {
                "此构建未内置该云服务的 client id — 请在下方填写自己的"
            } else if connected {
                "已连接"
            } else {
                "尚未连接"
            };

            let mut connect =
                button(text(t(if connected { "重新连接账号" } else { "连接账号" })).size(12))
                    .padding([5, 14])
                    .style(|_theme, status| primary_button_style(status));
            if !app.sync_connecting && !unconfigured {
                connect = connect.on_press(Message::SyncConnectAccount);
            }

            body = body
                .push(
                    row![
                        text(t(status_line)).size(11).color(muted_text()),
                        Space::new().width(Fill),
                        connect,
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .push(text(t(note)).size(10).color(muted_text()))
                .push(field(
                    "client id",
                    override_value,
                    "留空则用本应用内置的",
                    override_field,
                ))
                .push(
                    text(t("填写自己的 client id 可避开共享配额；本地或自行编译的版本也需要它。"))
                        .size(10)
                        .color(muted_text()),
                );

            // Only Google refuses the token exchange without one. Its own docs
            // call it optional; its server does not.
            if matches!(sync.provider, SyncProvider::GoogleDrive) {
                body = body.push(field(
                    "client secret",
                    &sync.google_client_secret,
                    "Google 桌面客户端必需，留空则用内置的",
                    SyncField::GoogleClientSecret,
                ));
            }
        }
    }

    let mut status_tab = column![].spacing(10);

    if sync.provider != SyncProvider::None {
        status_tab = status_tab.push(setting_card(
            checkbox(sync.include_credentials)
                .label(t("同时同步已保存的密码（加密后上传，主密码不出本机）"))
                .on_toggle(Message::SyncIncludeCredentialsToggled)
                .size(14)
                .text_size(11)
                .spacing(8),
        ));
    }

    // Status: what the last attempt did, then any sessions it could not settle.
    if !app.sync_status.is_empty() {
        status_tab = status_tab.push(setting_card(
            text(app.sync_status.clone())
                .size(11)
                .color(primary_text()),
        ));
    }
    for conflict in &app.sync_conflicts {
        status_tab = status_tab.push(text(conflict.clone()).size(10).color(muted_text()));
    }

    let sync_label = if app.sync_busy { "同步中…" } else { "立即同步" };
    let mut sync_button = button(text(t(sync_label)).size(12))
        .padding([6, 16])
        .style(|_theme, status| primary_button_style(status));
    if !app.sync_busy && sync.provider != SyncProvider::None {
        sync_button = sync_button.on_press(Message::SyncNow);
    }

    let status_tab = status_tab.push(
        row![Space::new().width(Fill), sync_button]
            .spacing(8)
            .align_y(Alignment::Center),
    );

    // Two tabs rather than one long scroll: where the data goes is decided once,
    // whether it arrived is checked over and over, and stacking them buried the
    // second under the first.
    let tab_button = |tab: SyncTab, label: &'static str| {
        let selected = app.sync_tab == tab;
        button(text(t(label)).size(12))
            .padding([5, 16])
            .width(Fill)
            .style(move |_theme, status| {
                if selected {
                    primary_button_style(status)
                } else {
                    secondary_button_style(status)
                }
            })
            .on_press(Message::SyncTabPicked(tab))
    };
    let tabs = row![
        tab_button(SyncTab::Services, "云服务"),
        tab_button(SyncTab::Status, "同步状态"),
    ]
    .spacing(6);

    let shown: Element<'_, Message> = match app.sync_tab {
        SyncTab::Services => body.into(),
        SyncTab::Status => status_tab.into(),
    };

    column![tabs, shown].spacing(14).into()
}

/// The one settings page: a category rail on the left, the chosen section on
/// the right.
///
/// It replaces three separate dialogs (应用 / 外观 / 同步与云), which between
/// them meant three places to look for one setting and three different ways to
/// close what you opened.
/// Wrap one setting in the card the 设置 page uses.
///
/// A column of these reads as a list of separate choices; the same widgets
/// stacked bare read as one dense block, which is what the old dialogs looked
/// like and why nobody could find anything in them.
fn setting_card<'a>(inner: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(inner)
        .width(Fill)
        .padding([14, 16])
        .style(|_theme| settings_card_style(false))
        .into()
}

pub(crate) fn settings_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let [config_section, log_section, mouse_section] = options_sections(app);

    let rail_item = |category: SettingsCategory| {
        let selected = app.settings_category == category;
        button(text(t(category.label())).size(12))
            .padding([9, 14])
            .width(Fill)
            .style(move |_theme, status| {
                if selected {
                    primary_button_style(status)
                } else {
                    secondary_button_style(status)
                }
            })
            .on_press(Message::SettingsCategoryPicked(category))
    };

    let rail = container(
        column![
            rail_item(SettingsCategory::App),
            rail_item(SettingsCategory::Appearance),
            rail_item(SettingsCategory::Terminal),
            rail_item(SettingsCategory::Logging),
            rail_item(SettingsCategory::Sync),
        ]
        .spacing(6),
    )
    .width(Length::Fixed(168.0))
    .padding(10)
    .style(|_theme| settings_rail_style());

    // Endonyms, deliberately: a reader who cannot read the current language
    // still recognises the name of their own.
    let language_pick = |language: adit_storage::Language| {
        let selected = app.language == language;
        button(text(language.label()).size(12))
            .padding([5, 16])
            .style(move |_theme, status| {
                if selected {
                    primary_button_style(status)
                } else {
                    secondary_button_style(status)
                }
            })
            .on_press(Message::LanguageChanged(language))
    };
    let language_card = setting_card(
        row![
            column![
                text(t("界面语言")).size(12).color(primary_text()),
                text(t("切换后立即生效")).size(10).color(muted_text()),
            ]
            .spacing(2),
            Space::new().width(Fill),
            language_pick(adit_storage::Language::Zh),
            language_pick(adit_storage::Language::En),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    );

    let body: Element<'_, Message> = match app.settings_category {
        SettingsCategory::App => column![language_card, config_section].spacing(10).into(),
        SettingsCategory::Appearance => appearance_section(app),
        SettingsCategory::Terminal => mouse_section,
        SettingsCategory::Logging => log_section,
        SettingsCategory::Sync => sync_section(app),
    };

    let card = container(
        column![
            row![
                text(t("设置")).size(15).color(primary_text()),
                Space::new().width(Fill),
                button(text("×").size(16))
                    .width(Length::Fixed(24.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseSettings),
            ]
            .align_y(Alignment::Center),
            row![
                rail,
                // Fixed height so switching category does not resize the dialog
                // under the cursor — a rail whose items move as you use them is
                // worse than a slightly tall panel.
                container(scrollable(container(body).padding([0, 20])))
                    .height(Length::Fixed(560.0))
                    .width(Fill),
            ]
            .spacing(18),
        ]
        .spacing(16),
    )
    .width(Length::Fixed(900.0))
    .padding(24)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}
