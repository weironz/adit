//! Optional master-password operations run on a worker, never the iced thread.
use super::*;
use adit_storage::CredentialProtection;
use zeroize::Zeroizing;

#[derive(Default)]
pub(crate) struct State {
    current: Zeroizing<String>,
    next: Zeroizing<String>,
    confirm: Zeroizing<String>,
    pub busy: bool,
    status: Option<CredentialProtection>,
    detail: String,
    acknowledged: bool,
}
impl State {
    pub fn clear_secrets(&mut self) {
        self.current = Zeroizing::new(String::new());
        self.next = Zeroizing::new(String::new());
        self.confirm = Zeroizing::new(String::new());
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Refresh,
    Enable,
    Unlock,
    Lock,
    Change,
    Disable,
    Export,
    Import,
}
#[derive(Clone)]
pub enum Message {
    Current(String),
    Next(String),
    Confirm(String),
    Acknowledge(bool),
    Run(Action),
    Finished(Result<(CredentialProtection, String), String>),
}
// Input messages deliberately redact secrets even if a debug subscriber is enabled.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Current(_) | Self::Next(_) | Self::Confirm(_) => "VaultSecret(redacted)",
            Self::Acknowledge(_) => "VaultAcknowledgement",
            Self::Run(_) => "VaultAction",
            Self::Finished(_) => "VaultFinished",
        })
    }
}
fn message(action: Action) -> super::Message {
    super::Message::Vault(Message::Run(action))
}

pub(crate) fn view(app: &AditApp) -> Element<'_, super::Message> {
    let state = &app.vault_ui;
    let status = match state.status {
        None => "尚未读取",
        Some(CredentialProtection::Legacy) => "兼容模式（无主密码）",
        Some(CredentialProtection::Locked) => "主密码保护：已锁定",
        Some(CredentialProtection::Unlocked) => "主密码保护：已解锁",
    };
    let action = |label: &'static str, op: Action, enabled: bool| {
        let b = button(text(label).size(11))
            .padding([5, 8])
            .style(|_, s| secondary_button_style(s));
        if enabled && !state.busy {
            b.on_press(message(op))
        } else {
            b
        }
    };
    let legacy = state.status == Some(CredentialProtection::Legacy);
    let protected = matches!(
        state.status,
        Some(CredentialProtection::Locked | CredentialProtection::Unlocked)
    );
    let secret = |hint: &str, value: &str, on_input: fn(String) -> Message| {
        text_input(hint, value)
            .secure(true)
            .size(12)
            .on_input(move |s| super::Message::Vault(on_input(s)))
    };
    let body = iced::widget::column![
        text("凭据库主密码").size(13),
        text(status).size(11),
        text("启用后需解锁才能读取保存的连接密码。忘记主密码无法恢复；现有连接继续运行。").size(11),
        text("迁移会保留旧模式的加密备份；旧兼容模式备份仍可用程序内置密钥解密，请妥善保管。")
            .size(10),
        secret(
            "当前库主密码（解锁/修改/关闭保护）",
            &state.current,
            Message::Current
        ),
        secret(
            "新主密码或导入文件的主密码（至少 12 字符）",
            &state.next,
            Message::Next
        ),
        secret(
            "确认新主密码（启用/修改时）",
            &state.confirm,
            Message::Confirm
        ),
        checkbox(state.acknowledged)
            .label("我理解备份保护级别及忘记主密码无法恢复的后果")
            .on_toggle(|v| super::Message::Vault(Message::Acknowledge(v))),
        row![
            action("读取状态", Action::Refresh, true),
            action("启用保护", Action::Enable, legacy && state.acknowledged),
            action("解锁", Action::Unlock, protected),
            action("锁定", Action::Lock, protected)
        ]
        .spacing(5),
        row![
            action(
                "修改主密码",
                Action::Change,
                protected && state.acknowledged
            ),
            action("关闭保护", Action::Disable, protected && state.acknowledged),
            action("导出加密备份", Action::Export, true),
            action("导入加密凭据", Action::Import, state.acknowledged)
        ]
        .spacing(5),
        text(if state.busy {
            "正在处理，请稍候…"
        } else {
            &state.detail
        })
        .size(10),
    ]
    .spacing(7);
    container(body).padding(12).width(Fill).into()
}

pub(crate) fn update(app: &mut AditApp, message: Message) -> Task<super::Message> {
    match message {
        Message::Current(value) => app.vault_ui.current = Zeroizing::new(value),
        Message::Next(value) => app.vault_ui.next = Zeroizing::new(value),
        Message::Confirm(value) => app.vault_ui.confirm = Zeroizing::new(value),
        Message::Acknowledge(value) => app.vault_ui.acknowledged = value,
        Message::Finished(result) => {
            app.vault_ui.busy = false;
            app.vault_ui.clear_secrets();
            match result {
                Ok((status, detail)) => {
                    app.vault_ui.status = Some(status);
                    app.vault_ui.detail = detail;
                    app.last_error = None;
                }
                Err(error) => {
                    app.vault_ui.detail = error.clone();
                    app.last_error = Some(error);
                }
            }
        }
        Message::Run(action) => {
            if matches!(action, Action::Lock | Action::Import) { app.manager.clear_jump_credentials(); }
            if app.vault_ui.busy {
                return Task::none();
            }
            if matches!(action, Action::Enable | Action::Change)
                && app.vault_ui.next.as_str() != app.vault_ui.confirm.as_str()
            {
                app.vault_ui.detail = "两次新主密码不一致".into();
                return Task::none();
            }
            if matches!(
                action,
                Action::Enable | Action::Change | Action::Disable | Action::Import
            ) && !app.vault_ui.acknowledged
            {
                app.vault_ui.detail = "请先确认备份与恢复说明".into();
                return Task::none();
            }
            let store = app.credential_store.clone();
            let current = std::mem::take(&mut app.vault_ui.current);
            let next = std::mem::take(&mut app.vault_ui.next);
            app.vault_ui.clear_secrets();
            app.vault_ui.busy = true;
            return Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || execute(store, action, &current, &next))
                        .await
                        .unwrap_or_else(|_| Err("凭据操作线程结束，请重新读取状态".into()))
                },
                |r| super::Message::Vault(Message::Finished(r)),
            );
        }
    }
    Task::none()
}
fn execute(
    store: CredentialStore,
    action: Action,
    current: &str,
    next: &str,
) -> Result<(CredentialProtection, String), String> {
    let result = (|| -> Result<String, adit_storage::CredentialError> {
        let status = store.protection_status()?;
        Ok(match action {
            Action::Refresh => "状态已读取".into(),
            Action::Enable => format!(
                "已启用。旧模式备份：{}",
                store.enable_master_password(next)?.display()
            ),
            Action::Unlock => {
                store.unlock(current)?;
                "已解锁".into()
            }
            Action::Lock => {
                store.lock_vault()?;
                "已锁定；已有会话仍可继续使用".into()
            }
            Action::Change => format!(
                "主密码已修改。旧保护备份：{}",
                store.change_master_password(current, next)?.display()
            ),
            Action::Disable => format!(
                "已恢复兼容模式。原保护备份：{}",
                store.disable_master_password(current)?.display()
            ),
            Action::Export => {
                let path = rfd::FileDialog::new()
                    .set_title("导出加密凭据（不含主密码）")
                    .set_file_name("adit-credentials-backup.json")
                    .save_file();
                if let Some(path) = path {
                    store.export_encrypted(&path)?;
                    format!("加密备份已导出：{}", path.display())
                } else {
                    "已取消，凭据未修改".into()
                }
            }
            Action::Import => {
                let path = rfd::FileDialog::new()
                    .set_title("导入加密凭据（将备份并替换当前库）")
                    .add_filter("加密凭据", &["json"])
                    .pick_file();
                if let Some(path) = path {
                    if status == CredentialProtection::Locked {
                        store.unlock(current)?;
                    }
                    let backup =
                        store.import_encrypted(&path, (!next.is_empty()).then_some(next))?;
                    format!(
                        "已导入，原库备份：{}；请用导入文件的主密码解锁",
                        backup.display()
                    )
                } else {
                    "已取消，凭据未修改".into()
                }
            }
        })
    })()
    .map_err(|e| e.to_string())?;
    Ok((
        store.protection_status().map_err(|e| e.to_string())?,
        result,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_debug_redacts_secret_messages() {
        assert!(
            !format!("{:?}", Message::Current("synthetic-hidden".into()))
                .contains("synthetic-hidden")
        );
    }
}
