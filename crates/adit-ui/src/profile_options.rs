use super::*;
use adit_domain::{
    DesktopQuality, LogPolicy, OutputTrigger, ReconnectPolicy, SessionOptions, TerminalEncoding,
    TriggerAction,
};
use iced::widget::column;

#[derive(Debug, Clone)]
pub enum ProfileOptionEdit {
    Encoding(TerminalEncoding),
    Quality(Option<DesktopQuality>),
    EnvAdd,
    EnvRemove(usize),
    EnvName(usize, String),
    EnvValue(usize, String),
    ReconnectOverride(bool),
    ReconnectEnabled(bool),
    Attempts(String),
    InitialDelay(String),
    MaxDelay(String),
    LogOverride(bool),
    LogAuto(bool),
    LogPlain(bool),
    LogInput(bool),
    LogDirectory(String),
    LogPattern(String),
    TriggerAdd,
    TriggerRemove(usize),
    TriggerEnabled(usize, bool),
    TriggerPattern(usize, String),
    TriggerSendMode(usize, bool),
    TriggerText(usize, String),
}

pub(crate) struct ProfileOptionsForm {
    options: SessionOptions,
    environment: Vec<(String, String)>,
    attempts: String,
    initial_delay: String,
    max_delay: String,
}

impl Default for ProfileOptionsForm {
    fn default() -> Self {
        Self::from_options(SessionOptions::default())
    }
}

impl ProfileOptionsForm {
    pub(crate) fn from_options(options: SessionOptions) -> Self {
        let policy = options.reconnect.clone().unwrap_or_default();
        Self {
            environment: options
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            options,
            attempts: policy.max_attempts.to_string(),
            initial_delay: policy.initial_delay_secs.to_string(),
            max_delay: policy.max_delay_secs.to_string(),
        }
    }

    pub(crate) fn value(&self) -> Result<SessionOptions, String> {
        let mut value = self.options.clone();
        value.environment.clear();
        for (key, item) in &self.environment {
            if key.trim().is_empty() && item.is_empty() {
                continue;
            }
            if value
                .environment
                .insert(key.trim().to_string(), item.clone())
                .is_some()
            {
                return Err(tf("环境变量重复：{}", &[&key.trim()]));
            }
        }
        if let Some(policy) = &mut value.reconnect {
            policy.max_attempts = self.attempts.parse().map_err(|_| t("重连次数必须为整数"))?;
            policy.initial_delay_secs = self
                .initial_delay
                .parse()
                .map_err(|_| t("重连间隔必须为整数"))?;
            policy.max_delay_secs = self.max_delay.parse().map_err(|_| t("最大间隔必须为整数"))?;
        }
        value.validate().map_err(|error| runtime(&error))?;
        Ok(value)
    }

    pub(crate) fn update_monitor_preferences(&mut self, options: &SessionOptions) {
        self.options.rdp_monitors = options.rdp_monitors.clone();
        self.options.rdp_selected_monitor = options.rdp_selected_monitor;
    }

    pub(crate) fn update(&mut self, edit: ProfileOptionEdit) {
        match edit {
            ProfileOptionEdit::Encoding(v) => self.options.encoding = v,
            ProfileOptionEdit::Quality(v) => self.options.rdp_quality = v,
            ProfileOptionEdit::EnvAdd => {
                if self.environment.len() < 64 {
                    self.environment.push((String::new(), String::new()));
                }
            }
            ProfileOptionEdit::EnvRemove(i) => {
                if i < self.environment.len() {
                    self.environment.remove(i);
                }
            }
            ProfileOptionEdit::EnvName(i, v) => {
                if let Some(entry) = self.environment.get_mut(i) {
                    entry.0 = v;
                }
            }
            ProfileOptionEdit::EnvValue(i, v) => {
                if let Some(entry) = self.environment.get_mut(i) {
                    entry.1 = v;
                }
            }
            ProfileOptionEdit::ReconnectOverride(v) => {
                self.options.reconnect = v.then(ReconnectPolicy::default);
            }
            ProfileOptionEdit::ReconnectEnabled(v) => {
                if let Some(policy) = &mut self.options.reconnect {
                    policy.enabled = v;
                }
            }
            ProfileOptionEdit::Attempts(v) => self.attempts = v,
            ProfileOptionEdit::InitialDelay(v) => self.initial_delay = v,
            ProfileOptionEdit::MaxDelay(v) => self.max_delay = v,
            ProfileOptionEdit::LogOverride(v) => self.options.logging = v.then(LogPolicy::default),
            ProfileOptionEdit::LogAuto(v) => {
                if let Some(policy) = &mut self.options.logging {
                    policy.auto_start = v;
                }
            }
            ProfileOptionEdit::LogPlain(v) => {
                if let Some(policy) = &mut self.options.logging {
                    policy.plaintext = v;
                }
            }
            ProfileOptionEdit::LogInput(v) => {
                if let Some(policy) = &mut self.options.logging {
                    policy.record_input = v;
                }
            }
            ProfileOptionEdit::LogDirectory(v) => {
                if let Some(policy) = &mut self.options.logging {
                    policy.directory = v;
                }
            }
            ProfileOptionEdit::LogPattern(v) => {
                if let Some(policy) = &mut self.options.logging {
                    policy.filename_pattern = v;
                }
            }
            ProfileOptionEdit::TriggerAdd => {
                if self.options.triggers.len() < 16 {
                    self.options.triggers.push(OutputTrigger::default());
                }
            }
            ProfileOptionEdit::TriggerRemove(index) => {
                if index < self.options.triggers.len() {
                    self.options.triggers.remove(index);
                }
            }
            ProfileOptionEdit::TriggerEnabled(index, enabled) => {
                if let Some(rule) = self.options.triggers.get_mut(index) {
                    rule.enabled = enabled;
                }
            }
            ProfileOptionEdit::TriggerPattern(index, pattern) => {
                if let Some(rule) = self.options.triggers.get_mut(index) {
                    rule.pattern = pattern;
                }
            }
            ProfileOptionEdit::TriggerSendMode(index, send) => {
                if let Some(rule) = self.options.triggers.get_mut(index) {
                    rule.action = if send {
                        TriggerAction::SendLine(String::new())
                    } else {
                        TriggerAction::Notify
                    };
                }
            }
            ProfileOptionEdit::TriggerText(index, text) => {
                if let Some(rule) = self.options.triggers.get_mut(index) {
                    rule.action = TriggerAction::SendLine(text);
                }
            }
        }
    }
}

fn edit(message: ProfileOptionEdit) -> Message {
    Message::ProfileOptionChanged(message)
}

fn field<'a>(
    label: &'static str,
    value: &'a str,
    changed: fn(String) -> ProfileOptionEdit,
) -> Element<'a, Message> {
    dialog_field(
        label,
        text_input("", value)
            .on_input(move |v| edit(changed(v)))
            .padding([5, 8])
            .style(text_input_style)
            .into(),
    )
}

pub(crate) fn profile_options_editor(app: &AditApp) -> Element<'_, Message> {
    let form = &app.profile_options;
    let mut body = column![].spacing(10);
    if app.profile_protocol == Protocol::Rdp {
        let mut quality = row![].spacing(5);
        for (choice, label) in [
            (None, "继承全局画质"),
            (Some(DesktopQuality::High), "高清"),
            (Some(DesktopQuality::Balanced), "均衡"),
            (Some(DesktopQuality::Speed), "流畅"),
        ] {
            let selected = form.options.rdp_quality == choice;
            quality = quality.push(
                button(text(t(label)).size(11))
                    .style(move |_, status| method_button_style(selected, status))
                    .on_press(edit(ProfileOptionEdit::Quality(choice))),
            );
        }
        return dialog_field("此会话的 RDP 画质", quality.wrap().into());
    }
    if !matches!(
        app.profile_protocol,
        Protocol::Ssh | Protocol::LocalShell | Protocol::Serial | Protocol::Telnet
    ) {
        return body.into();
    }
    if app.profile_protocol != Protocol::Sftp {
        body = body.push(dialog_field(
            "字符编码",
            iced::widget::pick_list(TerminalEncoding::ALL, Some(form.options.encoding), |v| {
                edit(ProfileOptionEdit::Encoding(v))
            })
            .into(),
        ));
    }
    if app.profile_protocol == Protocol::Ssh {
        body = body.push(
            row![
                text(t("环境变量")).size(11),
                Space::new().width(Fill),
                button(text(t("添加变量")).size(11)).on_press(edit(ProfileOptionEdit::EnvAdd))
            ]
            .align_y(Alignment::Center),
        );
        for (index, (key, value)) in form.environment.iter().enumerate() {
            body = body.push(
                row![
                    text_input(t("变量名"), key)
                        .on_input(move |v| edit(ProfileOptionEdit::EnvName(index, v)))
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Length::FillPortion(2)),
                    text_input(t("值"), value)
                        .on_input(move |v| edit(ProfileOptionEdit::EnvValue(index, v)))
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Length::FillPortion(3)),
                    button(t("移除")).on_press(edit(ProfileOptionEdit::EnvRemove(index))),
                ]
                .spacing(6),
            );
        }
        body = body.push(
            text(t("SSH 环境变量由服务端 AcceptEnv 策略决定是否接收。"))
                .size(10)
                .color(muted_text()),
        );
        body = body.push(
            text(t("变量值会明文保存在会话配置中，请勿填写密码或令牌。"))
                .size(10)
                .color(muted_text()),
        );
    }
    if app.profile_protocol == Protocol::Ssh {
        body = body.push(
            checkbox(form.options.reconnect.is_some())
                .label(t("为此会话单独配置重连"))
                .on_toggle(|v| edit(ProfileOptionEdit::ReconnectOverride(v))),
        );
        if let Some(policy) = &form.options.reconnect {
            body = body
                .push(
                    checkbox(policy.enabled)
                        .label(t("意外断开后自动重连"))
                        .on_toggle(|v| edit(ProfileOptionEdit::ReconnectEnabled(v))),
                )
                .push(
                    row![
                        field("最多尝试次数", &form.attempts, ProfileOptionEdit::Attempts),
                        field(
                            "初始间隔（秒）",
                            &form.initial_delay,
                            ProfileOptionEdit::InitialDelay
                        ),
                        field(
                            "最大间隔（秒）",
                            &form.max_delay,
                            ProfileOptionEdit::MaxDelay
                        ),
                    ]
                    .spacing(8),
                );
        }
    }
    body = body.push(
        checkbox(form.options.logging.is_some())
            .label(t("为此会话单独配置日志"))
            .on_toggle(|v| edit(ProfileOptionEdit::LogOverride(v))),
    );
    if let Some(policy) = &form.options.logging {
        body = body
            .push(
                checkbox(policy.auto_start)
                    .label(t("连接后自动记录输出"))
                    .on_toggle(|v| edit(ProfileOptionEdit::LogAuto(v))),
            )
            .push(field(
                "日志目录（留空继承全局）",
                &policy.directory,
                ProfileOptionEdit::LogDirectory,
            ))
            .push(field(
                "文件名模式（留空继承全局）",
                &policy.filename_pattern,
                ProfileOptionEdit::LogPattern,
            ))
            .push(
                checkbox(policy.plaintext)
                    .label(t("日志去除终端控制序列"))
                    .on_toggle(|v| edit(ProfileOptionEdit::LogPlain(v))),
            )
            .push(
                checkbox(policy.record_input)
                    .label(t("记录键盘输入（可能包含敏感信息）"))
                    .on_toggle(|v| edit(ProfileOptionEdit::LogInput(v))),
            );
    }
    body = body.push(
        row![
            text(t("输出触发器")).size(11),
            Space::new().width(Fill),
            button(text(t("添加触发器")).size(11)).on_press(edit(ProfileOptionEdit::TriggerAdd)),
        ]
        .align_y(Alignment::Center),
    );
    for (index, rule) in form.options.triggers.iter().enumerate() {
        let send = matches!(rule.action, TriggerAction::SendLine(_));
        let mut entry = column![
            row![
                checkbox(rule.enabled)
                    .label(tf("启用规则 {}", &[&(index + 1)]))
                    .on_toggle(move |v| edit(ProfileOptionEdit::TriggerEnabled(index, v))),
                Space::new().width(Fill),
                button(t("移除")).on_press(edit(ProfileOptionEdit::TriggerRemove(index))),
            ]
            .align_y(Alignment::Center),
            text_input(t("匹配表达式（正则）"), &rule.pattern)
                .padding([5, 8])
                .style(text_input_style)
                .on_input(move |v| edit(ProfileOptionEdit::TriggerPattern(index, v))),
            row![
                button(t("仅提示"))
                    .style(move |_, status| method_button_style(!send, status))
                    .on_press(edit(ProfileOptionEdit::TriggerSendMode(index, false))),
                button(t("发送一行文本"))
                    .style(move |_, status| method_button_style(send, status))
                    .on_press(edit(ProfileOptionEdit::TriggerSendMode(index, true))),
            ]
            .spacing(6),
        ]
        .spacing(6);
        if let TriggerAction::SendLine(text_value) = &rule.action {
            entry = entry
                .push(
                    text_input(t("发送后自动回车"), text_value)
                        .padding([5, 8])
                        .style(text_input_style)
                        .on_input(move |v| edit(ProfileOptionEdit::TriggerText(index, v))),
                )
                .push(
                    text(t("发送文本会明文保存；请勿填写密码或令牌。"))
                        .size(10)
                        .color(muted_text()),
                );
        }
        body = body.push(container(entry).padding(8).width(Fill));
    }
    if !form.options.triggers.is_empty() {
        body = body.push(
            text(t("匹配可见输出；每条规则每秒最多执行一次。"))
                .size(10)
                .color(muted_text()),
        );
    }
    body.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_drafts_are_not_silently_saved_as_defaults() {
        let mut form = ProfileOptionsForm::default();
        form.update(ProfileOptionEdit::ReconnectOverride(true));
        form.update(ProfileOptionEdit::Attempts("invalid".into()));
        assert!(form.value().is_err());
        form.update(ProfileOptionEdit::Attempts("8".into()));
        form.update(ProfileOptionEdit::EnvAdd);
        form.update(ProfileOptionEdit::EnvName(0, "LANG".into()));
        form.update(ProfileOptionEdit::EnvValue(0, "zh_CN.UTF-8".into()));
        form.update(ProfileOptionEdit::EnvAdd);
        form.update(ProfileOptionEdit::EnvName(1, "LANG".into()));
        assert!(form.value().is_err());
    }
}
