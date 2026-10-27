use super::*;

#[derive(Clone)]
pub struct SecretAnswer(pub(crate) String);
impl std::fmt::Debug for SecretAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Choice {
    owner: Option<ProfileId>,
    label: String,
}
impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

pub(crate) fn view(app: &AditApp) -> Element<'_, Message> {
    if app.profile_jump_auth.is_empty() {
        return Space::new().into();
    }
    let legacy = Choice {
        owner: None,
        label: t("独立认证：使用 agent、默认密钥或交互输入").to_owned(),
    };
    let choices: Vec<_> = std::iter::once(legacy.clone())
        .chain(
            app.manager
                .profiles()
                .iter()
                .filter(|p| p.protocol.is_ssh_based())
                .map(|p| Choice {
                    owner: Some(p.id),
                    label: format!(
                        "{} · {} · {}",
                        p.name,
                        p.username,
                        t(match p.auth_method {
                            AuthMethod::Auto => "自动",
                            AuthMethod::Password => "密码",
                            AuthMethod::Key => "密钥",
                            AuthMethod::Agent => "agent",
                        })
                    ),
                }),
        )
        .collect();
    let mut fields = iced::widget::column![
        text(t("每跳可选择独立 SSH/SFTP 认证配置（密码、密钥或 agent）；秘密仍保存在凭据库中。"))
            .size(11)
            .color(muted_text()),
        text(t("需要独立凭据时先新建配置，再在这里选择；只引用认证参数，不递归其跳板链；user@host 中的用户名优先。"))
            .size(11)
            .color(muted_text()),
    ]
    .spacing(6);
    for (index, hop) in app.profile_jump_auth.iter().enumerate() {
        let selected = choices
            .iter()
            .find(|c| c.owner == hop.auth_profile)
            .cloned()
            .unwrap_or_else(|| Choice {
                owner: hop.auth_profile,
                label: t("认证配置已删除，请重新选择").to_owned(),
            });
        fields = fields.push(
            iced::widget::row![
                text(tf("第 {} 跳：{}", &[&(index + 1), &hop.to_spec()]))
                    .size(11)
                    .width(Fill),
                iced::widget::pick_list(choices.clone(), Some(selected), move |choice| {
                    Message::ProfileHopAuthChanged {
                        index,
                        owner: choice.owner,
                    }
                })
                .text_size(11)
                .padding([5, 8])
                .style(pick_list_style)
                .width(Length::Fixed(260.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    fields.into()
}

pub(crate) fn edit_chain(app: &mut AditApp, text: String) {
    if let Ok(mut parsed) = parse_jumps_checked(&text) {
        let original = app.profile_jump_auth.clone();
        let mut old = original.clone();
        let same_count = parsed.len() == original.len();
        for (position, hop) in parsed.iter_mut().enumerate() {
            if let Some(index) = old.iter().position(|h| {
                h.host == hop.host && h.port == hop.port && h.username == hop.username
            }) {
                hop.auth_profile = old.remove(index).auth_profile;
            } else if same_count {
                hop.auth_profile = original[position].auth_profile;
            }
        }
        app.profile_jump_auth = parsed;
    }
    // Retain draft ownership while the user is midway through an invalid spec.
    app.profile_jumps = text;
}

pub(crate) fn prepare(app: &mut AditApp, profile_id: ProfileId) -> bool {
    let result = (|| {
        let profile = app.manager.profile(profile_id).ok_or(t("会话配置已删除"))?;
        let owners: Vec<_> = profile
            .jumps
            .iter()
            .filter_map(|h| h.auth_profile)
            .collect();
        let mut resolved = Vec::new();
        for owner in owners {
            let p = app
                .manager
                .profile(owner)
                .filter(|p| p.protocol.is_ssh_based())
                .ok_or_else(|| tf("跳板机认证配置 {} 不存在或不是 SSH/SFTP 配置", &[&owner]))?;
            let password = if matches!(
                p.auth_method,
                AuthMethod::Password | AuthMethod::Auto | AuthMethod::Key
            ) {
                app.credential_store
                    .load_profile_password(owner)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let passphrase = if matches!(p.auth_method, AuthMethod::Key | AuthMethod::Auto) {
                app.credential_store
                    .load_profile_passphrase(owner)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
            } else {
                String::new()
            };
            resolved.push((
                owner,
                adit_session::jump_credential(p, password, passphrase),
            ));
        }
        Ok::<_, String>(resolved)
    })();
    match result {
        Ok(resolved) => {
            for (owner, credential) in resolved {
                app.manager.set_jump_credential(owner, credential);
            }
            true
        }
        Err(error) => {
            app.last_error = Some(runtime(&error));
            false
        }
    }
}

pub(crate) fn respond(app: &mut AditApp, session: SessionId, challenge_id: u64, cancel: bool) {
    if !app
        .auth_prompt
        .as_ref()
        .is_some_and(|(id, p)| *id == session && p.id == challenge_id)
    {
        return;
    }
    let Some((_, prompt)) = app.auth_prompt.take() else {
        return;
    };
    let answers = std::mem::take(&mut app.auth_prompt_answers);
    if !prompt.respond(if cancel { Vec::new() } else { answers }) {
        app.notice = t("该认证请求已结束，请等待新的验证请求").to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_profile_secrets_use_the_encrypted_store_and_debug_redacts_answers() {
        struct TempVault {
            dir: std::path::PathBuf,
            file: std::path::PathBuf,
        }
        impl Drop for TempVault {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.file);
                let _ = std::fs::remove_dir(&self.dir);
            }
        }
        let dir = std::env::temp_dir().join(format!("adit-ssh-vault-fixture-{}", ProfileId::new()));
        std::fs::create_dir(&dir).unwrap();
        let temp = TempVault {
            file: dir.join("credentials.json"),
            dir,
        };
        let store = CredentialStore::with_path("synthetic-hop-test", &temp.file);
        let owner = ConnectionProfile::new("fixture-owner", "unused.invalid", 22, "hop-user");
        let mut target =
            ConnectionProfile::new("fixture-target", "unused.invalid", 22, "target-user");
        let mut hop = JumpHop::parse("hop.invalid").unwrap();
        hop.auth_profile = Some(owner.id);
        target.jumps = vec![hop];
        store
            .save_profile_password(owner.id, "SYNTHETIC-HOP-PASSWORD")
            .unwrap();
        store
            .save_profile_password(target.id, "SYNTHETIC-TARGET-PASSWORD")
            .unwrap();
        store
            .save_profile_passphrase(owner.id, "SYNTHETIC-HOP-PASSPHRASE")
            .unwrap();
        let credential = adit_session::jump_credential(
            &owner,
            store.load_profile_password(owner.id).unwrap().unwrap(),
            store.load_profile_passphrase(owner.id).unwrap().unwrap(),
        );
        assert_eq!(credential.password, "SYNTHETIC-HOP-PASSWORD");
        let profile_json = serde_json::to_string(&target).unwrap();
        let vault_json = std::fs::read_to_string(&temp.file).unwrap();
        let message = Message::AuthPromptInput {
            challenge_id: 1,
            index: 0,
            value: SecretAnswer("SYNTHETIC-OTP-ANSWER".into()),
        };
        for secret in [
            "SYNTHETIC-HOP-PASSWORD",
            "SYNTHETIC-TARGET-PASSWORD",
            "SYNTHETIC-HOP-PASSPHRASE",
        ] {
            assert!(!profile_json.contains(secret));
            assert!(!vault_json.contains(secret));
            assert!(!format!("{credential:?}").contains(secret));
        }
        assert!(!format!("{message:?}").contains("SYNTHETIC-OTP-ANSWER"));
    }
}
