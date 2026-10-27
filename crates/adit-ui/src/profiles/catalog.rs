use super::*;
use std::{collections::HashMap, io::Write, path::PathBuf};

pub(crate) fn begin_putty_registry_import() -> Task<Message> {
    Task::perform(
        async {
            tokio::task::spawn_blocking(adit_storage::read_putty_sessions)
                .await
                .unwrap_or_else(|e| Err(runtime(&e)))
        },
        Message::CatalogImported,
    )
}

pub(crate) fn begin_catalog_transfer(command: MenuCommand) -> Task<Message> {
    let dialog = rfd::AsyncFileDialog::new();
    let dialog = match command {
        MenuCommand::ImportSshConfig => {
            let mut dialog = dialog.set_title(t("选择 SSH config（不执行其中的命令）"));
            if let Some(path) = adit_storage::ssh_config_path() {
                if let Some(parent) = path.parent() {
                    dialog = dialog.set_directory(parent);
                }
            }
            dialog
        }
        MenuCommand::ImportPutty => dialog
            .set_title(t("选择 PuTTY Sessions 注册表导出文件（.reg）"))
            .add_filter(t("PuTTY 注册表导出"), &["reg"]),
        MenuCommand::ImportCatalog => dialog
            .set_title(t("导入会话目录；启动命令、环境变量和触发器将移除，自动日志需重新启用"))
            .add_filter(t("Adit 会话目录"), &["json"]),
        MenuCommand::ExportCatalog => dialog
            .set_title(t("导出会话目录到新文件（不含凭据、启动命令、环境变量值和触发器）"))
            .set_file_name("adit-profiles.json")
            .add_filter(t("Adit 会话目录"), &["json"]),
        _ => return Task::none(),
    };
    Task::perform(
        async move {
            let handle = if command == MenuCommand::ExportCatalog {
                dialog.save_file().await
            } else {
                dialog.pick_file().await
            };
            handle.map(|h| h.path().to_path_buf())
        },
        move |path| Message::CatalogFilePicked(command, path),
    )
}

pub(crate) fn run_catalog_transfer(
    app: &mut AditApp,
    command: MenuCommand,
    path: PathBuf,
) -> Task<Message> {
    if command == MenuCommand::ExportCatalog {
        let catalog = ProfileCatalog::with_group_icons(
            app.groups.clone(),
            app.group_icons.clone(),
            app.manager.profiles().to_vec(),
        );
        return Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let text = adit_storage::export_catalog(&catalog)?;
                    // create_new also refuses aliases/symlinks to existing config or
                    // vault files. An export must never overwrite the active store.
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map_err(|e| tf("请选择不存在的新文件：{}", &[&e]))?;
                    let result = file
                        .write_all(text.as_bytes())
                        .and_then(|()| file.sync_all());
                    drop(file);
                    if let Err(error) = result {
                        return Err(tf("导出未完成：{}。已创建的文件可能不完整，请检查 {}", &[&runtime(&error), &path.display()]));
                    }
                    Ok(())
                })
                .await
                .unwrap_or_else(|e| Err(runtime(&e)))
            },
            Message::CatalogExported,
        );
    }
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                let text = adit_storage::read_import_file(&path)?;
                match command {
                    MenuCommand::ImportSshConfig => adit_storage::parse_ssh_config_report(&text),
                    MenuCommand::ImportPutty => adit_storage::parse_putty_reg(&text),
                    MenuCommand::ImportCatalog => adit_storage::import_catalog(&text),
                    _ => Err(t("无效的导入类型").into()),
                }
            })
            .await
            .unwrap_or_else(|e| Err(runtime(&e)))
        },
        Message::CatalogImported,
    )
}

pub(crate) fn finish_catalog_import(
    app: &mut AditApp,
    result: Result<adit_storage::ImportReport, String>,
) {
    let mut report = match result {
        Ok(report) => report,
        Err(error) => {
            app.last_error = Some(tf("导入失败：{}", &[&runtime(&error)]));
            return;
        }
    };
    let existing = ProfileCatalog::with_group_icons(
        app.groups.clone(),
        app.group_icons.clone(),
        app.manager.profiles().to_vec(),
    );
    let (catalog, added, skipped, merge_warnings) = merge_catalog(existing, report.catalog);
    report.warnings.extend(merge_warnings);
    let changed =
        added > 0 || app.groups != catalog.groups || app.group_icons != catalog.group_icons;
    app.groups = catalog.groups;
    app.group_icons = catalog.group_icons;
    app.manager.replace_profiles(catalog.profiles);
    if changed && !persist_profiles(app) {
        return;
    }
    app.notice = tf("已导入 {} 个会话，跳过 {} 个同分组同名会话；凭据未导入", &[&added, &skipped]);
    app.last_error = if report.warnings.is_empty() {
        None
    } else {
        let details = report
            .warnings
            .iter()
            .take(8)
            .map(runtime)
            .collect::<Vec<_>>()
            .join("\n");
        Some(tf("导入提示（共 {} 项）：\n{}", &[&report.warnings.len(), &details]))
    };
}

fn merge_catalog(
    mut existing: ProfileCatalog,
    incoming: ProfileCatalog,
) -> (ProfileCatalog, usize, usize, Vec<String>) {
    let existing_ids: std::collections::HashSet<_> = existing.profiles.iter().map(|p| p.id).collect();
    let mut names: HashMap<(String, String), ProfileId> = existing
        .profiles
        .iter()
        .map(|p| ((p.group.clone(), p.name.clone()), p.id))
        .collect();
    let mut remap = HashMap::new();
    let mut added = Vec::new();
    let mut skipped = 0;
    let mut warnings = Vec::new();
    for profile in incoming.profiles {
        let key = (profile.group.clone(), profile.name.clone());
        if let Some(id) = names.get(&key) {
            remap.insert(profile.id, *id);
            skipped += 1;
        } else {
            names.insert(key, profile.id);
            added.push(profile);
        }
    }
    for profile in &mut added {
        for hop in &mut profile.jumps {
            if let Some(replacement) = hop.auth_profile.and_then(|id| remap.get(&id)) {
                // A matching name must never authorize sending this machine's
                // stored credentials to a host chosen by an imported catalog.
                if existing_ids.contains(replacement) {
                    hop.auth_profile = None;
                    warnings.push(tf("{}：同名认证配置已跳过，跳板恢复独立认证；请检查后重新选择凭据", &[&profile.name]));
                } else {
                    hop.auth_profile = Some(*replacement);
                }
            }
        }
        if let Some(id) = profile.vnc.gateway_profile.as_mut() {
            if let Some(replacement) = remap.get(id) {
                *id = *replacement;
            }
        }
    }
    let count = added.len();
    existing.profiles.extend(added);
    for group in incoming.groups {
        if !existing.groups.contains(&group) {
            existing.groups.push(group);
        }
    }
    for (group, icon) in incoming.group_icons {
        existing.group_icons.entry(group).or_insert(icon);
    }
    (
        ProfileCatalog::with_group_icons(existing.groups, existing.group_icons, existing.profiles),
        count,
        skipped,
        warnings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_jump_owner_name_collision_never_inherits_existing_vault_credentials() {
        let owner = ConnectionProfile::new("credential owner", "trusted.invalid", 22, "ops");
        let imported_owner = ConnectionProfile::new("credential owner", "imported.invalid", 22, "ops");
        let mut target = ConnectionProfile::new("new target", "target.invalid", 22, "ops");
        let mut hop = JumpHop::parse("imported-bastion.invalid").unwrap();
        hop.auth_profile = Some(imported_owner.id);
        target.jumps.push(hop);
        let (catalog, added, skipped, warnings) = merge_catalog(
            ProfileCatalog::from_profiles(vec![owner.clone()]),
            ProfileCatalog::from_profiles(vec![imported_owner, target]),
        );
        assert_eq!((added, skipped), (1, 1));
        assert_eq!(catalog.profiles[0].id, owner.id);
        assert_eq!(catalog.profiles[1].jumps[0].auth_profile, None);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn import_merges_without_replacing_existing_profiles_or_group_icons() {
        let first = ConnectionProfile::with_group("prod", "web", "old.example", 22, "ops");
        let id = first.id;
        let mut existing = ProfileCatalog::from_profiles(vec![first]);
        existing.group_icons.insert("prod".into(), "server".into());
        let mut incoming = ProfileCatalog::new(
            vec!["empty".into()],
            vec![
                ConnectionProfile::with_group("prod", "web", "new.example", 2222, "other"),
                ConnectionProfile::with_group("prod", "db", "db.example", 22, "ops"),
                ConnectionProfile::with_group("prod", "db", "duplicate.example", 22, "ops"),
            ],
        );
        incoming.group_icons.insert("prod".into(), "cloud".into());
        let (merged, added, skipped, warnings) = merge_catalog(existing, incoming);
        assert!(warnings.is_empty());
        assert_eq!((added, skipped), (1, 2));
        assert_eq!(merged.profiles[0].id, id);
        assert_eq!(merged.profiles[0].host, "old.example");
        assert_eq!(merged.group_icons["prod"], "server");
        assert!(merged.groups.contains(&"empty".into()));
    }

    #[test]
    fn menu_has_all_catalog_transfer_entries() {
        let commands = menu_commands(MenuKind::File);
        for command in [
            MenuCommand::ImportSshConfig,
            MenuCommand::ImportPutty,
            MenuCommand::ImportCatalog,
            MenuCommand::ExportCatalog,
        ] {
            assert!(commands.iter().any(|(_, c)| *c == command));
        }
        #[cfg(windows)]
        assert!(commands
            .iter()
            .any(|(_, c)| *c == MenuCommand::ImportPuttyRegistry));
    }

    #[test]
    fn a_skipped_duplicate_gateway_retargets_the_new_desktop_to_the_existing_profile() {
        let gateway = ConnectionProfile::with_group("prod", "gateway", "jump.test", 22, "ops");
        let existing_id = gateway.id;
        let imported_gateway =
            ConnectionProfile::with_group("prod", "gateway", "jump.test", 22, "ops");
        let mut desktop =
            ConnectionProfile::with_group("prod", "desktop", "desktop.test", 5900, "");
        desktop.protocol = Protocol::Vnc;
        desktop.vnc.gateway_profile = Some(imported_gateway.id);
        let (merged, added, skipped, warnings) = merge_catalog(
            ProfileCatalog::from_profiles(vec![gateway]),
            ProfileCatalog::from_profiles(vec![imported_gateway, desktop]),
        );
        assert!(warnings.is_empty());
        assert_eq!((added, skipped), (1, 1));
        assert_eq!(merged.profiles[1].vnc.gateway_profile, Some(existing_id));
    }
}
