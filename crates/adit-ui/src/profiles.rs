use super::*;

pub(crate) fn run_menu_command(app: &mut AditApp, command: MenuCommand) {
    match command {
        MenuCommand::NewProfile => new_profile_draft(app),
        MenuCommand::NewGroup => new_group_draft(app),
        MenuCommand::SaveProfile => save_profile(app),
        MenuCommand::DeleteProfile => delete_selected_profile(app),
        MenuCommand::SortByName => sort_profiles(app, ProfileSortKey::Name),
        MenuCommand::SortByHost => sort_profiles(app, ProfileSortKey::Host),
        MenuCommand::Connect => connect_or_prompt(app),
        MenuCommand::Disconnect => disconnect_active(app),
        MenuCommand::OpenMockTab => open_selected_mock_tab(app),
        MenuCommand::CloseActiveTab => {
            if let Some(session_id) = app.manager.active_session() {
                app.manager.close(session_id);
                app.terminal_scroll_offset = 0;
                app.terminal_selection = None;
                app.terminal_context_menu = false;
                app.notice = String::from("当前标签已关闭");
            } else {
                app.last_error = Some(String::from("没有可关闭的标签"));
            }
        }
        MenuCommand::ClearTerminal => clear_active_terminal(app),
        MenuCommand::ResizeDefault => resize_active(app, 96, 28),
        MenuCommand::ResizeWide => resize_active(app, 120, 36),
        MenuCommand::Sftp => {
            if let Err(error) = app.manager.open_sftp_for_active() {
                app.last_error = Some(format!("打开 SFTP 失败: {error}"));
            }
        }
        MenuCommand::Tunnels => {
            if app.manager.active_session().is_none() {
                app.last_error = Some(String::from("请先连接一个会话再配置端口转发"));
            } else {
                app.tunnels_open = true;
            }
        }
        MenuCommand::Logging => toggle_active_logging(app),
        MenuCommand::ToggleAutoReconnect => {
            let enabled = !app.manager.auto_reconnect();
            app.manager.set_auto_reconnect(enabled);
            app.notice = if enabled {
                String::from("自动重连已开启")
            } else {
                String::from("自动重连已关闭")
            };
        }
        MenuCommand::KnownHosts => {
            app.known_hosts = list_known_hosts(&known_hosts_path());
            app.known_hosts_open = true;
        }
        // Both used to live on the toolbar; the menu is their only home now.
        MenuCommand::ToggleSidebar => {
            app.sidebar_visible = !app.sidebar_visible;
            sync_terminal_size(app);
        }
        MenuCommand::ToggleTheme => app.dark_mode = !app.dark_mode,
        MenuCommand::Appearance => {
            app.settings_open = true;
            app.settings_category = SettingsCategory::Appearance;
        }
        MenuCommand::Options => {
            app.settings_open = true;
            app.settings_category = SettingsCategory::App;
        }
        MenuCommand::SyncCloud => {
            app.settings_open = true;
            app.settings_category = SettingsCategory::Sync;
            app.sync_secret_draft.clear();
        }
        MenuCommand::ImportSshConfig => import_ssh_config(app),
        // Handled in the RunMenu message arm (opens an async folder picker).
        MenuCommand::ImportSecureCrt => {}
        MenuCommand::Snippets => app.snippets_open = true,
        // Handled in the RunMenu arm (needs to return an async Task).
        MenuCommand::CheckUpdate => {}
        MenuCommand::SplitPane => split_pane(app),
        MenuCommand::TileVertical => tile_all_sessions(app, TileMode::Columns),
        MenuCommand::TileHorizontal => tile_all_sessions(app, TileMode::Rows),
        MenuCommand::TileGrid => tile_all_sessions(app, TileMode::Grid),
        MenuCommand::Untile => untile_sessions(app),
        MenuCommand::ToggleBroadcast => {
            app.broadcast_input = !app.broadcast_input;
            app.notice = if app.broadcast_input {
                String::from("输入广播已开启：键盘输入将同时发往所有已连接会话")
            } else {
                String::from("输入广播已关闭")
            };
        }
        MenuCommand::ToggleCommandWindow => {
            app.command_window_open = !app.command_window_open;
            app.command_history_pos = None;
            app.notice = if app.command_window_open {
                String::from("命令窗口已打开")
            } else {
                String::from("命令窗口已关闭")
            };
        }
        MenuCommand::About => app.about_open = true,
    }
}

pub(crate) fn select_profile(app: &mut AditApp, profile_id: ProfileId) {
    app.terminal_focused = false;
    app.selected_profile = Some(profile_id);
    load_selected_profile(app);
    app.last_error = None;
}

pub(crate) fn close_profile_editor_if_other(app: &mut AditApp, profile_id: ProfileId) {
    if app
        .profile_editor
        .is_some_and(|editing| editing != profile_id)
    {
        app.profile_editor = None;
    }
}

/// One entry in the top level: an ungrouped session (keyed by its global
/// sort_order) or a folder (keyed by its slot). Used to compute drop positions.
pub(crate) enum TopKind {
    Session(ProfileId),
    Folder,
}

/// The sorted top-level entries (ungrouped sessions + folders), excluding
/// `exclude`, with their interleave keys.
pub(crate) fn top_level_entries(app: &AditApp, exclude: ProfileId) -> Vec<(i32, TopKind)> {
    let profiles = app.manager.profiles();
    let mut entries: Vec<(i32, TopKind)> = profiles
        .iter()
        .filter(|profile| profile.group.trim().is_empty() && profile.id != exclude)
        .map(|profile| (profile.sort_order, TopKind::Session(profile.id)))
        .collect();
    for (index, _) in sidebar_group_names(app, profiles).iter().enumerate() {
        entries.push(((index as i32 + 1) * TOP_LEVEL_STEP, TopKind::Folder));
    }
    entries.sort_by_key(|(key, _)| *key);
    entries
}

/// Ungroup `source` and place it at top-level slot `index` (0..=len) by giving
/// it a sort_order midway between its new neighbours' keys.
pub(crate) fn place_ungrouped_at(
    app: &mut AditApp,
    source_id: ProfileId,
    index: usize,
) -> Result<(), SessionError> {
    let entries = top_level_entries(app, source_id);
    let prev = if index == 0 {
        entries.first().map(|(k, _)| k - TOP_LEVEL_STEP).unwrap_or(0)
    } else {
        entries[index - 1].0
    };
    let next = if index >= entries.len() {
        entries
            .last()
            .map(|(k, _)| k + TOP_LEVEL_STEP)
            .unwrap_or(TOP_LEVEL_STEP)
    } else {
        entries[index].0
    };
    let order = prev + (next - prev) / 2;
    app.manager.move_profile_to_group(source_id, "")?;
    app.manager.set_profile_sort_order(source_id, order);
    Ok(())
}

/// Commit the drag: move the held session to wherever the insertion line sits
/// (beside a row, into/around a folder, or out to the top level), then persist.
/// A plain click leaves `profile_drop` unset, so nothing moves.
pub(crate) fn finish_profile_drag(app: &mut AditApp) {
    app.profile_drag_origin = None;
    // Settle whatever is mid-flight into its final slot, after the commit below
    // has decided what that is.
    let settle = app.dragged_profile.is_some();
    let was_active = app.profile_drag_active;
    app.profile_drag_active = false;
    let Some(source_id) = app.dragged_profile.take() else {
        app.profile_drop = None;
        app.group_drop_target = None;
        return;
    };
    let drop = app.profile_drop.take();
    app.group_drop_target = None;
    // A press without a real drag (e.g. a click or double-click) never moves.
    if !was_active {
        if settle {
            retarget_card_slots(app);
        }
        return;
    }

    let result = match drop {
        Some(ProfileDrop::IntoGroup(group)) => app.manager.move_profile_to_group(source_id, group),
        Some(ProfileDrop::TopLevel) if !app.drag_from_grid => place_ungrouped_at(app, source_id, 0),
        Some(ProfileDrop::BottomLevel) if !app.drag_from_grid => {
            let len = top_level_entries(app, source_id).len();
            place_ungrouped_at(app, source_id, len)
        }
        // A grid drag edits the grid's order and stops there. Which group a
        // host is in stays shared — that is data — but where it sits in a view
        // is that view's business, and a casual drag here must not rearrange a
        // tree somebody curated.
        Some(ProfileDrop::Beside {
            profile_id,
            position,
        }) if app.drag_from_grid && profile_id != source_id => {
            reorder_grid(app, source_id, profile_id, position);
            app.selected_profile = Some(source_id);
            retarget_card_slots(app);
            persist_settings_if_changed(app);
            return;
        }
        Some(ProfileDrop::Beside {
            profile_id,
            position,
        }) if profile_id != source_id => {
            let target_group = app
                .manager
                .profile(profile_id)
                .map(|profile| profile.group.clone())
                .unwrap_or_default();
            if target_group.trim().is_empty() {
                // Interleave at the top level, beside another ungrouped session.
                let entries = top_level_entries(app, source_id);
                let at = entries
                    .iter()
                    .position(|(_, kind)| matches!(kind, TopKind::Session(id) if *id == profile_id));
                let index = match at {
                    Some(i) if position == ProfileDropPosition::After => i + 1,
                    Some(i) => i,
                    None => entries.len(),
                };
                place_ungrouped_at(app, source_id, index)
            } else {
                // Beside a session inside a folder: join that folder at that spot.
                app.manager.reorder_profile(source_id, profile_id, position)
            }
        }
        _ => return,
    };

    retarget_card_slots(app);
    match result {
        Ok(()) => {
            app.selected_profile = Some(source_id);
            load_selected_profile(app);
            if persist_profiles(app) {
                app.notice = String::from("会话已移动");
            }
        }
        Err(error) => app.last_error = Some(error.to_string()),
    }
}

pub(crate) fn drop_profile_on_group(app: &mut AditApp, group: String) {
    app.profile_drop = None;
    let Some(source_id) = app.dragged_profile.take() else {
        app.group_drop_target = None;
        return;
    };

    app.group_drop_target = None;

    match app.manager.move_profile_to_group(source_id, group.clone()) {
        Ok(()) => {
            add_group(&mut app.groups, &group);
            app.collapsed_groups.remove(&group);
            app.selected_profile = Some(source_id);
            load_selected_profile(app);
            if persist_profiles(app) {
                app.notice = format!("会话已移动到分组: {group}");
            }
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

/// Toggle a folder's collapsed state (shared by the header click and the folder
/// context menu's collapse/expand item).
pub(crate) fn toggle_group_collapsed(app: &mut AditApp, group: &str) {
    if !app.collapsed_groups.remove(group) {
        app.collapsed_groups.insert(group.to_string());
    }
    app.profile_context_menu = None;
    app.group_context_menu = None;
    app.profile_editor = None;
}

/// A folder drag released directly on the `target` folder header.
pub(crate) fn finish_group_drag_on(app: &mut AditApp, target: String) {
    let Some(source) = app.dragged_group.take() else {
        return;
    };
    let active = app.group_drag_active;
    app.group_drag_active = false;
    app.group_drag_origin = None;
    app.group_drop = None;
    if !active {
        // No real movement — treat the press+release as a click on the header.
        toggle_group_collapsed(app, &source);
        return;
    }
    commit_group_reorder(app, source, target);
}

/// A folder drag released off any header (empty space or a session row). Commits
/// from the last-hovered target, if any; a press that never moved just toggles.
pub(crate) fn cancel_group_drag(app: &mut AditApp) {
    let Some(source) = app.dragged_group.take() else {
        return;
    };
    let active = app.group_drag_active;
    let target = app.group_drop.take();
    app.group_drag_active = false;
    app.group_drag_origin = None;
    if !active {
        toggle_group_collapsed(app, &source);
        return;
    }
    if let Some(target) = target {
        commit_group_reorder(app, source, target);
    }
}

/// Move folder `source` next to `target` in the folder order and persist. The
/// drag direction picks the side: dragging down lands after the target, dragging
/// up lands before it (mirroring the session/tab reorder).
pub(crate) fn commit_group_reorder(app: &mut AditApp, source: String, target: String) {
    // Materialize the full displayed folder order — app.groups may omit folders
    // that exist only via profiles — so reordering the Vec fully controls order.
    let order = sidebar_group_names(app, app.manager.profiles());
    app.groups = reordered_folders(order, &source, &target);
    if persist_profiles(app) {
        app.notice = String::from("分组顺序已更新");
    }
}

/// Pure folder-reorder: return `order` with `source` moved next to `target`.
/// Direction-aware — if `source` sits above `target` (dragging down) it lands
/// after `target`, otherwise (dragging up) before it. A no-op if either name is
/// missing or they are the same.
pub(crate) fn reordered_folders(mut order: Vec<String>, source: &str, target: &str) -> Vec<String> {
    if source == target {
        return order;
    }
    let (Some(si), Some(ti)) = (
        order.iter().position(|g| g == source),
        order.iter().position(|g| g == target),
    ) else {
        return order;
    };
    let after = si < ti;
    order.retain(|g| g != source);
    let mut idx = order.iter().position(|g| g == target).unwrap_or(order.len());
    if after {
        idx += 1;
    }
    order.insert(idx, source.to_string());
    order
}

/// Whether to draw a folder-reorder insertion line above/below `group`'s header
/// while a folder is being dragged. The side follows the drag direction.
pub(crate) fn folder_reorder_lines(app: &AditApp, folders: &[String], group: &str) -> (bool, bool) {
    if !app.group_drag_active
        || app.group_drop.as_deref() != Some(group)
        || app.dragged_group.as_deref() == Some(group)
    {
        return (false, false);
    }
    let src_idx = app
        .dragged_group
        .as_ref()
        .and_then(|source| folders.iter().position(|g| g == source));
    let tgt_idx = folders.iter().position(|g| g == group);
    match (src_idx, tgt_idx) {
        (Some(s), Some(t)) if s < t => (false, true), // dragging down → after
        _ => (true, false),                           // dragging up → before
    }
}

pub(crate) fn load_selected_profile(app: &mut AditApp) {
    let profile = app
        .selected_profile
        .and_then(|profile_id| app.manager.profile(profile_id).cloned());

    if let Some(profile) = profile {
        app.profile_group = profile.group;
        let group = app.profile_group.clone();
        add_group(&mut app.groups, &group);
        app.profile_name = profile.name;
        app.profile_host = profile.host;
        app.profile_port = profile.port.to_string();
        app.profile_username = profile.username;
        app.profile_auth_method = profile.auth_method;
        app.profile_identity_file = profile.identity_file;
        app.profile_protocol = profile.protocol;
        app.profile_icon = profile.icon.clone();
        app.profile_startup_command = profile.startup_command;
        app.profile_jumps = jumps_to_spec(&profile.jumps);
        app.profile_terminal_type = profile.terminal_type;
        app.profile_environment = profile.environment;
        app.profile_accent_color = profile.accent_color.clone().unwrap_or_default();
        app.profile_label = profile.label.clone().unwrap_or_default();
        // Password-auth password comes from the OS credential vault, not the
        // profile record.
        app.profile_password = if profile.auth_method == AuthMethod::Password {
            app.credential_store
                .load_profile_password(profile.id)
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            String::new()
        };
        // The key passphrase is likewise vault-stored, distinct from the password,
        // and only relevant to key-bearing auth methods.
        app.profile_passphrase =
            if matches!(profile.auth_method, AuthMethod::Key | AuthMethod::Auto) {
                app.credential_store
                    .load_profile_passphrase(profile.id)
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            } else {
                String::new()
            };
    }
}

pub(crate) fn new_profile_draft(app: &mut AditApp) {
    // Starting a new session saves any in-place rename in progress (click-away).
    commit_inline_rename(app);
    let name = next_profile_name(app);
    let group = active_profile_group(app);
    match app.manager.create_profile(
        group.clone(),
        name,
        "127.0.0.1",
        22,
        "root",
        // New sessions default to SSH, which defaults to password auth.
        AuthMethod::Password,
        "",
    ) {
        Ok(profile_id) => {
            app.selected_profile = Some(profile_id);
            app.profile_editor = Some(profile_id);
            // Empty group ⇒ ungrouped; don't register a phantom empty folder.
            if !group.is_empty() {
                add_group(&mut app.groups, &group);
                app.collapsed_groups.remove(&group);
            }
            load_selected_profile(app);
            app.last_error = None;
            if persist_profiles(app) {
                app.notice = String::from("新建会话已加入左侧列表，编辑后点击保存");
            }
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

pub(crate) fn new_group_draft(app: &mut AditApp) {
    // Starting a new folder saves any in-place rename in progress (click-away).
    commit_inline_rename(app);
    let group = next_group_name(app);
    add_group(&mut app.groups, &group);
    app.collapsed_groups.remove(&group);
    app.profile_group = group.clone();
    app.profile_context_menu = None;
    app.group_context_menu = None;
    app.profile_editor = None;
    app.last_error = None;

    if persist_profiles(app) {
        app.notice = format!("分组已创建: {group}");
    }
}

/// Drop any in-place rename in progress (folder or session) without saving.
/// Used by Escape and by deleting the row being edited.
pub(crate) fn cancel_inline_rename(app: &mut AditApp) {
    app.editing_profile = None;
    app.profile_name_draft.clear();
    app.editing_group = None;
    app.group_name_draft.clear();
}

/// Save any in-place rename in progress, then exit edit mode. Invalid edits
/// (empty / duplicate folder name) silently revert rather than trap the row.
/// Used when the user clicks away — there are no confirm/cancel buttons.
pub(crate) fn commit_inline_rename(app: &mut AditApp) {
    let mut resolved = false;
    if let Some(profile_id) = app.editing_profile.take() {
        resolved = true;
        let name = app.profile_name_draft.trim().to_string();
        app.profile_name_draft.clear();
        let unchanged = app
            .manager
            .profile(profile_id)
            .is_some_and(|profile| profile.name == name);
        if !name.is_empty() && !unchanged {
            apply_profile_rename(app, profile_id, &name);
        }
    }
    if let Some(old_group) = app.editing_group.take() {
        resolved = true;
        let new_group = app.group_name_draft.trim().to_string();
        app.group_name_draft.clear();
        if !new_group.is_empty() && new_group != old_group && !app.groups.contains(&new_group) {
            let _ = apply_group_rename(app, &old_group, &new_group);
        }
    }
    // Resolving the rename clears any lingering validation error it produced.
    if resolved {
        app.last_error = None;
    }
}

/// Apply a session rename (manager + editor-field sync + persist). Returns false
/// only if the manager rejects it. Does not touch `editing_profile`.
pub(crate) fn apply_profile_rename(app: &mut AditApp, profile_id: ProfileId, name: &str) -> bool {
    if app.manager.rename_profile(profile_id, name.to_string()).is_err() {
        return false;
    }
    // Keep the editor form's name field in sync if it is open on this row.
    if app.selected_profile == Some(profile_id) {
        app.profile_name = name.to_string();
    }
    if persist_profiles(app) {
        app.notice = String::from("会话已重命名");
    }
    true
}

/// Apply a validated folder rename (manager + app bookkeeping + persist). Returns
/// an error message if the manager rejects it. Does not touch `editing_group`.
pub(crate) fn apply_group_rename(app: &mut AditApp, old_group: &str, new_group: &str) -> Result<(), String> {
    app.manager
        .rename_group(old_group, new_group.to_string())
        .map_err(|error| error.to_string())?;
    // Replace in place so the folder keeps its position.
    if let Some(pos) = app.groups.iter().position(|group| group == old_group) {
        app.groups[pos] = new_group.to_string();
    } else {
        add_group(&mut app.groups, new_group);
    }
    if app.collapsed_groups.remove(old_group) {
        app.collapsed_groups.insert(new_group.to_string());
    }
    if app.profile_group == old_group {
        app.profile_group = new_group.to_string();
    }
    if persist_profiles(app) {
        app.notice = format!("分组已重命名: {old_group} -> {new_group}");
    }
    Ok(())
}

pub(crate) fn save_profile_rename(app: &mut AditApp) {
    let Some(profile_id) = app.editing_profile else {
        return;
    };
    let name = app.profile_name_draft.trim().to_string();
    if name.is_empty() {
        app.last_error = Some(String::from("会话名称不能为空"));
        return;
    }
    // Unchanged name — just close the editor (no rewrite, no "renamed" toast).
    let unchanged = app
        .manager
        .profile(profile_id)
        .is_some_and(|profile| profile.name == name);
    if unchanged {
        app.editing_profile = None;
        app.profile_name_draft.clear();
        return;
    }
    if apply_profile_rename(app, profile_id, &name) {
        app.editing_profile = None;
        app.profile_name_draft.clear();
        app.last_error = None;
    }
}

pub(crate) fn save_group_rename(app: &mut AditApp) {
    let Some(old_group) = app.editing_group.clone() else {
        return;
    };
    let new_group = app.group_name_draft.trim().to_string();
    if new_group.is_empty() {
        app.last_error = Some(String::from("分组名称不能为空"));
        return;
    }
    if new_group == old_group {
        // No change — just close the editor.
        app.editing_group = None;
        app.group_name_draft.clear();
        return;
    }
    if app.groups.contains(&new_group) {
        app.last_error = Some(format!("分组已存在: {new_group}"));
        return;
    }
    match apply_group_rename(app, &old_group, &new_group) {
        Ok(()) => {
            app.editing_group = None;
            app.group_name_draft.clear();
            app.last_error = None;
        }
        Err(error) => {
            app.last_error = Some(error);
        }
    }
}

/// Delete a folder and every session config inside it (and each one's saved
/// password). Open tabs are left running, matching single-session delete.
pub(crate) fn delete_group(app: &mut AditApp, group: String) {
    app.group_context_menu = None;
    app.editing_group = None;
    app.group_name_draft.clear();

    let ids: Vec<ProfileId> = app
        .manager
        .profiles()
        .iter()
        .filter(|profile| profile.group == group)
        .map(|profile| profile.id)
        .collect();
    let count = ids.len();

    for id in &ids {
        let _ = app.manager.delete_profile(*id);
        let _ = app.credential_store.delete_profile_password(*id);
        let _ = app.credential_store.delete_profile_passphrase(*id);
    }
    remove_group(&mut app.groups, &group);
    app.collapsed_groups.remove(&group);
    if app.profile_group == group {
        app.profile_group = String::new();
    }

    app.selected_profile = app.manager.profiles().first().map(|profile| profile.id);
    if app.selected_profile.is_some() {
        load_selected_profile(app);
    }
    app.last_error = None;
    if persist_profiles(app) {
        app.notice = if count > 0 {
            format!("已删除分组「{group}」及其 {count} 个会话配置（已打开标签不受影响）")
        } else {
            format!("已删除空分组「{group}」")
        };
    }
}

pub(crate) fn next_profile_name(app: &AditApp) -> String {
    let mut index = app.manager.profiles().len() + 1;
    loop {
        let name = format!("new-session-{index}");
        if app
            .manager
            .profiles()
            .iter()
            .all(|profile| profile.name != name)
        {
            return name;
        }
        index += 1;
    }
}

pub(crate) fn next_group_name(app: &AditApp) -> String {
    let mut index = app.groups.len() + 1;
    loop {
        let name = format!("group-{index}");
        if !app.groups.contains(&name) {
            return name;
        }
        index += 1;
    }
}

/// The group a new/saved profile lands in: the editor's group field, trimmed.
/// Empty means ungrouped (top level), so a new session need not be in a folder.
pub(crate) fn active_profile_group(app: &AditApp) -> String {
    app.profile_group.trim().to_string()
}

pub(crate) fn save_profile(app: &mut AditApp) {
    let _ = save_profile_from_form(app, true);
}

/// Render a jump-host chain as a comma-separated OpenSSH-style spec (for the
/// editor field).
pub(crate) fn jumps_to_spec(jumps: &[JumpHop]) -> String {
    jumps
        .iter()
        .map(JumpHop::to_spec)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse the editor's jump-host field — comma / newline / semicolon separated
/// `user@host:port` hops — into an ordered chain, reporting the first non-empty
/// hop that fails to parse. Saving a bad spec (e.g. a typo'd port) must be
/// surfaced, not silently drop a bastion and downgrade to a direct connection.
pub(crate) fn parse_jumps_checked(spec: &str) -> Result<Vec<JumpHop>, String> {
    let mut hops = Vec::new();
    for token in spec.split([',', '\n', ';']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match JumpHop::parse(token) {
            Some(hop) => hops.push(hop),
            None => {
                return Err(format!(
                    "跳板机格式无效：“{token}”（应为 user@host:port，端口 1-65535）"
                ))
            }
        }
    }
    Ok(hops)
}

pub(crate) fn save_profile_from_form(app: &mut AditApp, show_notice: bool) -> Option<ProfileId> {
    let Some(port) = parse_port(&app.profile_port) else {
        app.last_error = Some(String::from("端口必须是 1-65535 的数字"));
        return None;
    };
    // Validate the jump chain up front: a bad hop must block the save (and be
    // reported) rather than be dropped, which would silently bypass the bastion.
    let jumps = match parse_jumps_checked(&app.profile_jumps) {
        Ok(jumps) => jumps,
        Err(error) => {
            app.last_error = Some(error);
            return None;
        }
    };

    let result = if let Some(profile_id) = app.selected_profile {
        app.manager.update_profile(
            profile_id,
            app.profile_group.clone(),
            app.profile_name.clone(),
            app.profile_host.clone(),
            port,
            app.profile_username.clone(),
            app.profile_auth_method,
            app.profile_identity_file.clone(),
        )
    } else {
        match app.manager.create_profile(
            app.profile_group.clone(),
            app.profile_name.clone(),
            app.profile_host.clone(),
            port,
            app.profile_username.clone(),
            app.profile_auth_method,
            app.profile_identity_file.clone(),
        ) {
            Ok(profile_id) => {
                app.selected_profile = Some(profile_id);
                Ok(())
            }
            Err(error) => Err(error),
        }
    };

    match result {
        Ok(()) => {
            // Protocol is edited separately from the core fields, so apply it here
            // before persisting.
            if let Some(profile_id) = app.selected_profile {
                app.manager
                    .set_profile_protocol(profile_id, app.profile_protocol);
                app.manager
                    .set_profile_icon(profile_id, app.profile_icon.clone());
                app.manager.set_profile_startup_command(
                    profile_id,
                    app.profile_startup_command.clone(),
                );
                app.manager.set_profile_jumps(profile_id, jumps.clone());
                app.manager
                    .set_profile_terminal_type(profile_id, app.profile_terminal_type.clone());
                let accent_color = (!app.profile_accent_color.trim().is_empty())
                    .then(|| app.profile_accent_color.trim().to_string());
                let label = (!app.profile_label.trim().is_empty())
                    .then(|| app.profile_label.trim().to_string());
                app.manager.set_profile_appearance(
                    profile_id,
                    app.profile_environment,
                    accent_color,
                    label,
                );
                // Persist the password-auth password to the OS credential vault
                // (never to profiles.json). An empty field clears any saved one.
                if app.profile_auth_method == AuthMethod::Password {
                    let _ = if app.profile_password.is_empty() {
                        app.credential_store.delete_profile_password(profile_id)
                    } else {
                        app.credential_store
                            .save_profile_password(profile_id, &app.profile_password)
                    };
                }
                // Persist the key passphrase to the vault (distinct entry) only
                // for key-bearing auth with a non-empty value; otherwise clear any
                // saved one so a Password/Agent profile never keeps a stale secret.
                let keep_passphrase =
                    matches!(app.profile_auth_method, AuthMethod::Key | AuthMethod::Auto)
                        && !app.profile_passphrase.is_empty();
                let _ = if keep_passphrase {
                    app.credential_store
                        .save_profile_passphrase(profile_id, &app.profile_passphrase)
                } else {
                    app.credential_store.delete_profile_passphrase(profile_id)
                };
            }
            load_selected_profile(app);
            app.collapsed_groups.remove(app.profile_group.trim());
            if persist_profiles(app) {
                app.last_error = None;
                if show_notice {
                    app.notice = format!("会话配置已保存: {}", app.profile_store.path().display());
                }
                app.selected_profile
            } else {
                None
            }
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
            None
        }
    }
}

pub(crate) fn delete_selected_profile(app: &mut AditApp) {
    let Some(profile_id) = app.selected_profile else {
        app.last_error = Some(String::from("请选择要删除的会话配置"));
        return;
    };

    match app.manager.delete_profile(profile_id) {
        Ok(()) => {
            app.profile_context_menu = None;
            app.profile_editor = None;
            // The deleted row can't cancel its own in-place rename, so do it here.
            cancel_inline_rename(app);
            app.selected_profile = app.manager.profiles().first().map(|profile| profile.id);
            app.last_error = None;
            let credential_cleanup = app
                .credential_store
                .delete_profile_password(profile_id)
                .err();
            let _ = app.credential_store.delete_profile_passphrase(profile_id);
            if let Some(error) = credential_cleanup {
                app.last_error = Some(format!("删除系统凭据失败: {error}"));
            }
            if persist_profiles(app) {
                app.notice = format!(
                    "会话配置已删除；已打开标签不受影响。已写入 {}",
                    app.profile_store.path().display()
                );
            }
            if app.selected_profile.is_some() {
                load_selected_profile(app);
            } else {
                new_profile_draft(app);
            }
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

pub(crate) fn sort_profiles(app: &mut AditApp, key: ProfileSortKey) {
    app.manager.sort_profiles(key);
    if persist_profiles(app) {
        app.last_error = None;
        app.notice = match key {
            ProfileSortKey::Name => String::from("会话已按名称排序"),
            ProfileSortKey::Host => String::from("会话已按主机排序"),
        };
    }
}

/// Import hosts from `~/.ssh/config` into the profile list (group "Imported"),
/// skipping any whose name already exists.
/// Import SecureCRT sessions from a `.ini` session tree (folders → groups).
/// Passwords are not imported (SecureCRT stores them encrypted).
pub(crate) fn import_securecrt(app: &mut AditApp, root: &std::path::Path) {
    let sessions = adit_storage::parse_securecrt_sessions(root);
    if sessions.is_empty() {
        app.last_error =
            Some(String::from("该文件夹下没有找到 SecureCRT 会话（.ini），请选择 Config/Sessions 文件夹"));
        return;
    }

    let existing: BTreeSet<(String, String)> = app
        .manager
        .profiles()
        .iter()
        .map(|profile| (profile.group.clone(), profile.name.clone()))
        .collect();
    let fallback_user = adit_storage::current_username().unwrap_or_default();
    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut touched_groups: Vec<String> = Vec::new();

    for session in sessions {
        if existing.contains(&(session.group.clone(), session.name.clone())) {
            skipped += 1;
            continue;
        }
        let username = if session.username.is_empty() {
            fallback_user.clone()
        } else {
            session.username.clone()
        };
        match app.manager.create_profile(
            &session.group,
            &session.name,
            &session.hostname,
            session.port,
            username,
            AuthMethod::Auto,
            "",
        ) {
            Ok(id) => {
                // Everything imports as SSH unless SecureCRT marked it otherwise.
                let protocol = match session.protocol.as_str() {
                    "RDP" => Some(Protocol::Rdp),
                    "Serial" => Some(Protocol::Serial),
                    _ => None,
                };
                if let Some(protocol) = protocol {
                    app.manager.set_profile_protocol(id, protocol);
                }
                if !session.group.is_empty() && !touched_groups.contains(&session.group) {
                    touched_groups.push(session.group.clone());
                }
                added += 1;
            }
            Err(_) => skipped += 1,
        }
    }

    for group in &touched_groups {
        add_group(&mut app.groups, group);
    }

    if added > 0 {
        app.selected_profile = app.manager.profiles().first().map(|profile| profile.id);
        load_selected_profile(app);
        persist_profiles(app);
        app.last_error = None;
        app.notice = if skipped > 0 {
            format!("已从 SecureCRT 导入 {added} 个会话（跳过 {skipped} 个已存在/无效）；密码未导入，请重新设置")
        } else {
            format!("已从 SecureCRT 导入 {added} 个会话；密码未导入，请重新设置")
        };
    } else {
        app.notice = String::from("没有新的 SecureCRT 会话需要导入（可能都已存在）");
    }
}

pub(crate) fn import_ssh_config(app: &mut AditApp) {
    let Some(path) = adit_storage::ssh_config_path() else {
        app.last_error = Some(String::from("找不到用户主目录"));
        return;
    };
    if !path.exists() {
        app.last_error = Some(format!("未找到 {}", path.display()));
        return;
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            app.last_error = Some(format!("读取 ssh config 失败: {error}"));
            return;
        }
    };

    let hosts = adit_storage::parse_ssh_config(&text);
    if hosts.is_empty() {
        app.notice = String::from("~/.ssh/config 中没有可导入的主机");
        return;
    }

    let existing: BTreeSet<String> = app
        .manager
        .profiles()
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let fallback_user = adit_storage::current_username().unwrap_or_default();
    let group = "Imported";
    let mut added = 0usize;
    let mut skipped = 0usize;

    for host in hosts {
        if existing.contains(&host.alias) {
            skipped += 1;
            continue;
        }
        let username = if host.user.is_empty() {
            fallback_user.clone()
        } else {
            host.user
        };
        let auth = if host.identity_file.is_empty() {
            AuthMethod::Auto
        } else {
            AuthMethod::Key
        };
        if app
            .manager
            .create_profile(
                group,
                &host.alias,
                &host.hostname,
                host.port,
                username,
                auth,
                host.identity_file,
            )
            .is_ok()
        {
            added += 1;
        }
    }

    if added > 0 {
        add_group(&mut app.groups, group);
        persist_profiles(app);
        app.last_error = None;
        app.notice = if skipped > 0 {
            format!("已从 ~/.ssh/config 导入 {added} 个会话（跳过 {skipped} 个已存在）")
        } else {
            format!("已从 ~/.ssh/config 导入 {added} 个会话")
        };
    } else {
        app.notice = String::from("没有新的主机需要导入（可能都已存在）");
    }
}

pub(crate) fn persist_profiles(app: &mut AditApp) -> bool {
    let catalog = ProfileCatalog::new(app.groups.to_vec(), app.manager.profiles().to_vec());

    // Write on a background thread: the profiles.json disk write can block for
    // seconds (antivirus scan / synced folder / lock), and this runs on the UI
    // thread — a synchronous write froze the whole app on RDP connect
    // (`connect_profile` → `save_profile_from_form` → here). Only serialization
    // (fast, in-memory) stays on the caller and can still surface an error.
    match app.profile_store.save_catalog_async(&catalog) {
        Ok(()) => true,
        Err(error) => {
            app.last_error = Some(format!("保存会话配置失败: {error}"));
            false
        }
    }
}

/// Write (or, for the default location, clear) the bootstrap pointer so the next
/// launch resolves the config folder to `target`.
pub(crate) fn write_config_pointer(target: &std::path::Path) -> std::io::Result<()> {
    if target == adit_storage::default_config_dir() {
        adit_storage::set_custom_config_dir(None)
    } else {
        adit_storage::set_custom_config_dir(Some(target))
    }
}

/// Carry the current config into `target` (copying, never deleting the source, so
/// a shared/synced folder is safe) and switch the running app over to it live.
/// Returns whether the switch succeeded.
pub(crate) fn carry_config_to(app: &mut AditApp, target: std::path::PathBuf) -> bool {
    if let Err(error) = adit_storage::copy_config_files(&app.config_dir, &target) {
        app.last_error = Some(format!("复制配置到 {} 失败: {error}", target.display()));
        return false;
    }
    if let Err(error) = write_config_pointer(&target) {
        app.last_error = Some(format!("设置配置文件夹失败: {error}"));
        return false;
    }
    app.profile_store = ProfileStore::new(target.join("profiles.json"));
    app.settings_store = SettingsStore::new(target.join("settings.json"));
    app.config_dir_custom = target != adit_storage::default_config_dir();
    app.config_dir = target;
    app.pending_config_dir = None;
    app.last_error = None;
    // Re-persist the in-memory state into the new stores so they hold the latest.
    persist_profiles(app);
    let _ = app.settings_store.save(&current_settings(app));
    true
}

/// Point the config folder at `target` (e.g. a Dropbox path). The running app
/// never calls the live `config_dir()` for its paths — it uses `app.config_dir`
/// — so nothing is half-applied. Rules:
/// - target == current: no move, just re-assert the pointer.
/// - target already has config (synced from another machine): adopt it on the
///   next launch (do NOT overwrite it); the current run keeps its folder.
/// - target is empty: copy the current config in and switch over live.
pub(crate) fn relocate_config_dir(app: &mut AditApp, target: std::path::PathBuf) {
    let default = adit_storage::default_config_dir();
    // Flush the latest in-memory state to the current folder so any copy is fresh.
    let _ = app.settings_store.save(&current_settings(app));
    persist_profiles(app);

    if target == app.config_dir {
        // No move — just make sure the on-disk pointer matches the live folder
        // (repairs state if a prior reset cleared it).
        if let Err(error) = write_config_pointer(&target) {
            app.last_error = Some(format!("设置配置文件夹失败: {error}"));
            return;
        }
        app.config_dir_custom = target != default;
        app.pending_config_dir = None;
        app.last_error = None;
        app.notice = String::from("配置文件夹未改变");
        return;
    }

    // Adopt only a *different* populated folder — another machine's synced config.
    // The default folder's contents are this machine's own (stale) snapshot, so
    // going there always carries the current config in (overwriting it) instead of
    // adopting the old copy, matching the "恢复默认" button.
    if target != default && adit_storage::config_dir_has_config(&target) {
        // Adopt it on the next launch rather than overwriting it.
        if let Err(error) = write_config_pointer(&target) {
            app.last_error = Some(format!("设置配置文件夹失败: {error}"));
            return;
        }
        app.config_dir_custom = true;
        app.pending_config_dir = Some(target.clone());
        app.last_error = None;
        app.notice = format!(
            "将在重启后加载 {} 中的现有配置（不会覆盖该文件夹）；请尽快重启，重启前的修改不会保留",
            target.display()
        );
        return;
    }

    // Empty target (or the default folder) — carry the current config in and
    // switch over live.
    if carry_config_to(app, target.clone()) {
        app.notice = if target == default {
            String::from("已恢复到默认配置文件夹（已生效）")
        } else {
            format!("配置文件夹已切换到 {}（已生效）", target.display())
        };
    }
}

/// The ordered folder list: the persisted order first (user-arrangeable), then
/// any folder seen only on a profile, deduped, first occurrence wins.
pub(crate) fn groups_from_catalog(groups: Vec<String>, profiles: &[ConnectionProfile]) -> Vec<String> {
    let mut result = Vec::new();
    for group in groups.into_iter().chain(profiles.iter().map(|p| p.group.clone())) {
        let group = group.trim().to_string();
        if !group.is_empty() && !result.contains(&group) {
            result.push(group);
        }
    }
    result
}

pub(crate) fn groups_from_profiles(profiles: &[ConnectionProfile]) -> Vec<String> {
    groups_from_catalog(Vec::new(), profiles)
}

/// Append a folder to the ordered list if it isn't already present (and not
/// blank). Order is preserved so folders stay where the user put them.
pub(crate) fn add_group(groups: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if !name.is_empty() && !groups.iter().any(|group| group == name) {
        groups.push(name.to_string());
    }
}

/// Remove a folder from the ordered list; returns whether it was present.
pub(crate) fn remove_group(groups: &mut Vec<String>, name: &str) -> bool {
    let before = groups.len();
    groups.retain(|group| group != name);
    groups.len() != before
}

pub(crate) fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|port| *port > 0)
}
